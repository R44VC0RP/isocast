#!/usr/bin/env python3
"""Resume a prepared release without compiling or re-signing its executables.

Keep the tested originals; finalize a copy under notarized/. Persist submission
ids and hashes before waiting, so retries cannot submit the same archive twice.
Only a verified final DMG may reach a draft, and an existing tag is never replaced.
"""

import argparse
import base64
import hashlib
import fcntl
import json
import os
from pathlib import Path
import plistlib
import re
import runpy
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time

IDENTITY = os.environ.get("ISOCAST_SIGN_IDENTITY", "Developer ID Application: Ryan Vogel (9G68SMNHEU)")
TEAM = os.environ.get("APPLE_TEAM_ID", "9G68SMNHEU")
REQUIREMENT = f'=anchor apple generic and certificate leaf[subject.OU] = "{TEAM}"'
PROFILE = "isocast-notary"
KEYCHAIN = os.environ.get("ISOCAST_SIGN_KEYCHAIN", str(Path.home() / "Library/Keychains/login.keychain-db"))
REPO = "R44VC0RP/isocast"
ROOT = Path(__file__).resolve().parent.parent
RELEASES = "docs/releases"  # X.Y.Z.md: each version's notes, committed before tagging (#106)
CHANGELOG = "**Full Changelog**"
SEMVER = r"[0-9]+\.[0-9]+\.[0-9]+"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def sha(path):
    with path.open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


def save(path, data):
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(data, indent=2) + "\n")
    temporary.replace(path)


def redact(text):
    for name in ("APPLE_API_KEY_PATH", "APPLE_API_KEY_ID", "APPLE_API_ISSUER_ID", "APPLE_TEAM_ID"):
        value = os.environ.get(name)
        if value:
            text = text.replace(value, "<redacted>")
    return text


def notary_auth():
    values = [os.environ.get(k) for k in ("APPLE_API_KEY_PATH", "APPLE_API_KEY_ID", "APPLE_API_ISSUER_ID")]
    if any(values):
        require(all(values), "API notarization needs APPLE_API_KEY_PATH, APPLE_API_KEY_ID and APPLE_API_ISSUER_ID")
        require(Path(values[0]).is_file(), "APPLE_API_KEY_PATH must name the protected .p8 file")
        return ["--key", values[0], "--key-id", values[1], "--issuer", values[2]]
    return ["--keychain-profile", PROFILE, "--keychain", KEYCHAIN]


def command(*args, capture=False, check=True, timeout=180):
    args = [str(a) for a in args]
    print("+", redact(shlex.join(args)), flush=True)
    process = subprocess.Popen(args, stdout=subprocess.PIPE if capture else None,
                               stderr=subprocess.PIPE, text=True)
    start = time.monotonic()
    while True:
        try:
            stdout, stderr = process.communicate(timeout=min(20, timeout))
            break
        except subprocess.TimeoutExpired:
            print(f"  {args[0]}: waiting ({int(time.monotonic() - start)}s)", flush=True)
            if time.monotonic() - start >= timeout:
                process.kill()
                process.communicate()
                raise TimeoutError(f"{args[0]} exceeded {timeout}s; retry the same release command")
    if stderr:
        print(redact(stderr), end="", file=sys.stderr)
    if check and process.returncode:
        raise subprocess.CalledProcessError(process.returncode, [redact(a) for a in args])
    return stdout or ""


def verify_app(app, metadata):
    command("codesign", "--verify", "--deep", "--strict", "--verbose=2",
            "--test-requirement", REQUIREMENT, app)
    for name, digest in metadata["app_files"].items():
        require(sha(app / name) == digest, f"prepared app bytes changed: {name}")


def verify_dmg(dmg):
    command("hdiutil", "verify", dmg)
    command("codesign", "--verify", "--strict", "--verbose=2",
            "--test-requirement", REQUIREMENT, dmg)


def assess(app, dmg):
    command("xcrun", "stapler", "validate", app)
    command("spctl", "--assess", "--type", "execute", "--verbose=2", app)
    command("xcrun", "stapler", "validate", dmg)
    command("spctl", "--assess", "--type", "open", "--context",
            "context:primary-signature", "--verbose=2", dmg)


def notarize(archive, receipt):
    digest = sha(archive)
    auth = notary_auth()
    if receipt.exists():
        result = json.loads(receipt.read_text())
        require(result["sha256"] == digest, f"archive changed since submission: {archive}")
    else:
        submitting = receipt.with_suffix(".submitting")
        require(not submitting.exists(),
                f"previous submission returned no id; inspect notarytool history before retrying ({submitting})")
        submitting.write_text(digest + "\n")
        # Get/persist the id immediately, then use bounded notarytool wait calls.
        raw = command("xcrun", "notarytool", "submit", archive, *auth,
                      "--no-wait", "--output-format", "json", capture=True)
        response = json.loads(raw)
        result = {k: response[k] for k in ("id", "status") if k in response}
        require(bool(result.get("id")), "notarytool returned no submission id")
        result.update(sha256=digest)
        save(receipt, result)
        submitting.unlink()
    for _ in range(30):
        status = result.get("status", "In Progress")
        print(f"Notarization {result['id']}: {status}", flush=True)
        if status == "Accepted":
            return
        require(status == "In Progress", f"notarization {status}; retrieve notarytool log {result['id']}")
        raw = command("xcrun", "notarytool", "wait", result["id"], *auth,
                      "--timeout", "60s", "--output-format", "json",
                      capture=True, check=False, timeout=90)
        update = json.loads(raw)
        require(update.get("id") == result["id"] and "status" in update,
                f"unexpected notary response; saved id remains in {receipt}")
        result["status"] = update["status"]
        save(receipt, result)
    raise TimeoutError(f"notarization still processing; retry the same command (saved id in {receipt})")


def finalize(output, metadata):
    app = output / "Isocast.app"
    dmg_name = f"Isocast-{metadata['version']}-arm64.dmg"
    verify_app(app, metadata)
    verify_dmg(output / dmg_name)
    require(sha(output / dmg_name) == metadata["dmg_sha256"], "prepared DMG hash changed")
    final = output / "notarized"
    final.mkdir(exist_ok=True)
    finished = final / "release.json"
    if finished.exists():
        require(json.loads(finished.read_text()) == metadata, "finalized metadata mismatch")
        return
    make_dmg = Path(__file__).with_name("dmg") / "make-dmg.sh"
    if not (final / dmg_name).exists():
        command(make_dmg, "--check")  # Fail before any notary submission.
    final_app = final / "Isocast.app"
    if not final_app.exists():
        command("ditto", app, final_app)
    archive = final / "Isocast.zip"
    if not archive.exists():
        command("ditto", "-c", "-k", "--keepParent", final_app, archive)
    verify_app(final_app, metadata)
    notarize(archive, final / "notary-app.json")
    command("xcrun", "stapler", "staple", final_app)
    command("xcrun", "stapler", "validate", final_app)
    verify_app(final_app, metadata)
    dmg = final / dmg_name
    if not dmg.exists():
        # Same designed layout as release.sh (scripts/dmg/settings.py).
        command(make_dmg, final_app,
                f"Isocast {metadata['version']}", dmg, timeout=600)
    signed = final / "dmg-signed.json"
    if not signed.exists():
        command("codesign", "--force", "--sign", IDENTITY, "--keychain", KEYCHAIN,
                "--timestamp", dmg, timeout=90)
        save(signed, {"sha256": sha(dmg)})
    verify_dmg(dmg)
    # Stapling changes the DMG hash. Keep the submitted copy so resume can
    # verify the notary receipt even after the distributable has been stapled.
    submitted = final / "submitted.dmg"
    if not submitted.exists():
        require(sha(dmg) == json.loads(signed.read_text())["sha256"], "signed DMG changed before submission")
        command("cp", dmg, submitted)
    require(sha(submitted) == json.loads(signed.read_text())["sha256"], "submitted DMG hash mismatch")
    notarize(submitted, final / "notary-dmg.json")
    command("xcrun", "stapler", "staple", dmg)
    verify_dmg(dmg)
    assess(final_app, dmg)
    (final / (dmg_name + ".sha256")).write_text(f"{sha(dmg)}  {dmg_name}\n")
    save(finished, metadata)


def runtime_check(app, output, hosted):
    """Exercise the shipped executable, without copying or ad-hoc re-signing it.

    Reuse E2E's capability, duration/count/decode/sync and loss validators. A
    hosted qualification allows at most its existing 25% accounted video loss;
    two zero-drop takes tighten the final take to zero. Audio is always lossless.
    """
    e2e = runpy.run_path(str(Path(__file__).with_name("e2e.py")))
    scratch = Path(os.environ.get("ISOCAST_SCRATCH", output.parent))
    scratch.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="release-", dir=scratch))
    sock = root / "d.sock"
    require(len(os.fsencode(sock)) < 104, "runtime scratch path is too long for a Unix socket")
    cli = app / "Contents/MacOS/isocast"
    # "Isocast Engine" (#83), or isocastd in builds prepared before #83.
    engine = app / "Contents/Helpers/Isocast Engine.app/Contents"
    name = plistlib.loads((engine / "Info.plist").read_bytes())["CFBundleExecutable"]
    daemon = engine / "MacOS" / name
    # Signing/notary/GitHub credentials never enter the recorded application.
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(("ISOCAST_", "APPLE_", "DEVELOPER_", "SPARKLE_"))
           and k not in ("GH_TOKEN", "GITHUB_TOKEN")}
    env.update(ISOCAST_SOCKET=str(sock), ISOCAST_SETUP=str(root / "setup.toml"),
               ISOCAST_PLUGINS=str(root / "plugins"),
               ISOCAST_LOG_DIR=str(root / "logs"), ISOCAST_DEV_APP=str(root / "absent.app"))

    def cli_call(*args):
        p = subprocess.run([str(cli), *map(str, args)], env=env, capture_output=True,
                           text=True, timeout=45)
        require(p.returncode == 0, f"bundled CLI {args[0]} failed: {redact(p.stderr)}")
        return p.stdout

    report = {"app": str(app), "work": str(root), "takes": [], "hosted": hosted}
    pid = None
    try:
        print("Bundled CLI:", cli_call("--version").strip(), flush=True)
        cli_call("get", "--json")
        pid = int(sock.with_suffix(".lock").read_text())
        executable = subprocess.check_output(["ps", "-p", str(pid), "-o", "comm="], text=True).strip()
        require(Path(executable).resolve() == daemon.resolve(), "CLI did not launch its bundled engine")
        report["daemon_sha256"] = sha(daemon)
        with (scratch / ".bench.lock").open("a") as lock:
            deadline = time.monotonic() + 120
            while True:
                try:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    break
                except BlockingIOError:
                    require(time.monotonic() < deadline, "recording lock busy for 120s")
                    print("Runtime check: waiting for recording lock (5s)", flush=True)
                    time.sleep(5)
            require(subprocess.run(["pgrep", "screensharingd"], stdout=subprocess.DEVNULL).returncode == 1,
                    "close screen sharing before runtime qualification")
            caps = e2e["capabilities"]()
            caps.update(hosted=hosted, max_drop_ratio=e2e["HOSTED_DROP_LIMIT"] if hosted else 0)
            if hosted and caps["h264"]:
                caps["record_codec"] = "h264"
            width, height, fps = (1280, 720, 30) if hosted else (1920, 1080, 60)
            caps["profile"] = dict(width=width, height=height, fps=fps)
            report["capabilities"] = caps
            setup = e2e["setup"](root, caps["record_codec"], True, width, height, fps)
            save(root / "input.json", setup)
            cli_call("apply", root / "input.json", "--revision", "0", "--json")
            evidence = {"capabilities": caps}
            for i in range(3 if hosted else 1):
                if i == 2 and all(r["dropped"] == 0 for r in evidence.get("drops", {}).values()):
                    caps["max_drop_ratio"] = 0
                print(f"Bundled runtime take {i + 1}: {width}x{height}/{fps}, video loss limit {caps['max_drop_ratio']:.0%}", flush=True)
                cli_call("start", "--seconds", "5", "--json")
                deadline = time.monotonic() + 45
                while True:
                    state = json.loads(cli_call("status", "--json"))
                    if state["state"] == "idle":
                        break
                    require(time.monotonic() < deadline, "recording finalization timed out")
                    time.sleep(1)
                require(not state.get("last_error") and state.get("last"), "recording has no successful report")
                manifest_path = Path(state["last"]["manifest"])
                manifest = json.loads(manifest_path.read_text())
                rows = e2e["check_take"](manifest_path, manifest, 5, sync=True,
                                          expected_sources=["program", "bars", "bars2"], evidence=evidence)
                report["takes"].append(rows)
                print("Verified:", json.dumps(rows), flush=True)
            report["drops"] = evidence["drops"]
            report["status"] = "PASS"
    except Exception as error:
        report.update(status="FAIL", error=redact(str(error)))
        raise
    finally:
        # A timed-out CLI may have spawned before its reply; this scratch lock
        # belongs only to this run. Never touch a default daemon.
        if pid is None and sock.with_suffix(".lock").exists():
            pid = int(sock.with_suffix(".lock").read_text())
        if pid:
            try:
                with socket.socket(socket.AF_UNIX) as connection:
                    connection.settimeout(5)
                    connection.connect(str(sock))
                    connection.sendall(b'{"jsonrpc":"2.0","id":1,"method":"shutdown"}\n')
                    connection.recv(65536)
            except OSError:
                pass
            for attempt in range(50):
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    break
                if attempt == 40:
                    try:
                        os.kill(pid, signal.SIGTERM)
                    except ProcessLookupError:
                        break
                time.sleep(0.2)
            else:
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        save(output / "runtime.json", report)


def verify_final(output, metadata, runtime=False, hosted=False):
    final = output / "notarized"
    require((final / "release.json").is_file(),
            "run --notarize --output on this prepared build before --draft-release")
    require(json.loads((final / "release.json").read_text()) == metadata,
            "run --notarize on this prepared build first")
    app, dmg = final / "Isocast.app", final / f"Isocast-{metadata['version']}-arm64.dmg"
    for receipt, archive in [("notary-app.json", "Isocast.zip"), ("notary-dmg.json", "submitted.dmg")]:
        result = json.loads((final / receipt).read_text())
        require(result.get("status") == "Accepted" and result["sha256"] == sha(final / archive),
                f"missing/mismatched accepted receipt: {receipt}")
    verify_app(app, metadata)
    verify_dmg(dmg)
    assess(app, dmg)
    # Check the app actually inside the shipped DMG, not just its staging copy.
    # macOS may refuse a custom mount point inside another mounted volume.
    # Let Disk Arbitration choose it and use its exact reported path.
    attached = plistlib.loads(command("hdiutil", "attach", "-readonly", "-nobrowse",
                                     "-plist", dmg, capture=True).encode())
    mounts = [Path(e["mount-point"]) for e in attached["system-entities"] if "mount-point" in e]
    try:
        require(len(mounts) == 1, "expected one mounted DMG volume")
        mounted = mounts[0]
        verify_app(mounted / "Isocast.app", metadata)
        command("xcrun", "stapler", "validate", mounted / "Isocast.app")
        require((mounted / "Applications").is_symlink()
                and (mounted / "Applications").readlink() == Path("/Applications"),
                "DMG Applications link is missing")
        if runtime:
            runtime_check(mounted / "Isocast.app", output, hosted)
    finally:
        for mount in mounts:
            command("hdiutil", "detach", mount)
    checksum = dmg.with_suffix(".dmg.sha256")
    require(checksum.read_text() == f"{sha(dmg)}  {dmg.name}\n", "final checksum mismatch")
    return dmg, checksum


# Ed25519 verification (RFC 8032), to check the appcast's archive signature
# against the SUPublicEDKey the shipped app actually carries.
_P = 2 ** 255 - 19
_Q = 2 ** 252 + 27742317777372353535851937790883648493
_D = -121665 * pow(121666, _P - 2, _P) % _P


def _x(y, sign):
    if y >= _P:
        return None
    x2 = (y * y - 1) * pow(_D * y * y + 1, _P - 2, _P) % _P
    if x2 == 0:
        return None if sign else 0
    x = pow(x2, (_P + 3) // 8, _P)
    if (x * x - x2) % _P:
        x = x * pow(2, (_P - 1) // 4, _P) % _P
    if (x * x - x2) % _P:
        return None
    return _P - x if (x & 1) != sign else x


def _add(a, b):
    e, f = (a[1] - a[0]) * (b[1] - b[0]) % _P, (a[1] + a[0]) * (b[1] + b[0]) % _P
    g, h = 2 * a[3] * b[3] * _D % _P, 2 * a[2] * b[2] % _P
    e, f, g, h = f - e, h - g, h + g, f + e
    return e * f % _P, g * h % _P, f * g % _P, e * h % _P


def _mul(scalar, point):
    result = (0, 1, 1, 0)
    while scalar:
        if scalar & 1:
            result = _add(result, point)
        point, scalar = _add(point, point), scalar >> 1
    return result


def _point(data):
    y = int.from_bytes(data, "little")
    x = _x(y & ((1 << 255) - 1), y >> 255)
    return None if x is None else (x, y & ((1 << 255) - 1), 1, x * (y & ((1 << 255) - 1)) % _P)


def ed25519_verify(public, path, signature):
    gy = 4 * pow(5, _P - 2, _P) % _P
    base = (_x(gy, 0), gy, 1, _x(gy, 0) * gy % _P)
    a, r, s = _point(public), _point(signature[:32]), int.from_bytes(signature[32:], "little")
    if len(public) != 32 or len(signature) != 64 or a is None or r is None or s >= _Q:
        return False
    digest = hashlib.sha512(signature[:32] + public)
    with path.open("rb") as file:
        for block in iter(lambda: file.read(1 << 20), b""):
            digest.update(block)
    h = int.from_bytes(digest.digest(), "little") % _Q
    left, right = _mul(s, base), _add(r, _mul(h, a))
    return all((left[i] * right[2] - right[i] * left[2]) % _P == 0 for i in (0, 1))


def sparkle_tool(name):
    dist = command(Path(__file__).with_name("sparkle") / "sparkle.sh", "path", capture=True).strip()
    return Path(dist) / "bin" / name


def sparkle_run(*args):
    """Run a Sparkle tool with the private key: SPARKLE_ED_PRIVATE_KEY on its
    stdin (CI), else the login keychain's (generate_keys). Never printed."""
    key = os.environ.get("SPARKLE_ED_PRIVATE_KEY")
    args = [str(a) for a in args]
    if key:
        args[1:1] = ["--ed-key-file", "-"]
    print("+", shlex.join(args), "(EdDSA key from", "SPARKLE_ED_PRIVATE_KEY)" if key else "login keychain)", flush=True)
    process = subprocess.run(args, input=(key.strip() + "\n") if key else None,
                             capture_output=True, text=True, timeout=600)
    output = (process.stdout + process.stderr).replace(key.strip(), "<redacted>") if key else process.stdout + process.stderr
    if process.returncode:
        print(output, end="", file=sys.stderr)  # Redacted above.
        raise subprocess.CalledProcessError(process.returncode, args, output)
    return output


def appcast(final, metadata, dmg, notes, prefix):
    """Write and verify final/appcast.xml: one EdDSA-signed item for this DMG,
    release notes embedded, the feed itself signed (SURequireSignedFeed).
    Reused while the DMG and notes are unchanged, so reruns upload the same bytes."""
    sparkle = metadata.get("sparkle")
    require(sparkle and sparkle.get("public_key"), "prepared build has no Sparkle key; rebuild with release.sh")
    out, state = final / "appcast.xml", final / "appcast.json"
    key = {"dmg_sha256": sha(dmg), "notes_sha256": hashlib.sha256(notes.encode()).hexdigest(), "prefix": prefix}
    if not (out.exists() and state.exists() and json.loads(state.read_text()) == key):
        write_appcast(final / "appcast-work", dmg, notes, prefix, out)
        save(state, key)
    verify_appcast(out, metadata, dmg, prefix)
    return out


def write_appcast(work, dmg, notes, prefix, out):
    """generate_appcast on a hard link to DMG, notes (markdown) beside it."""
    if work.exists():
        command("rm", "-rf", work)
    work.mkdir()
    os.link(dmg, work / dmg.name)
    (work / dmg.name).with_suffix(".md").write_text(notes + "\n", encoding="utf-8")
    sparkle_run(sparkle_tool("generate_appcast"), "--download-url-prefix", prefix,
                "--embed-release-notes", "--link", f"https://github.com/{REPO}", work)
    (work / "appcast.xml").replace(out)
    command("rm", "-rf", work)


def verify_appcast(out, metadata, dmg, prefix):
    import xml.etree.ElementTree as ET
    ns = {"sparkle": "http://www.andymatuschak.org/xml-namespaces/sparkle"}
    items = ET.parse(out).getroot().findall("./channel/item")
    require(len(items) == 1, f"appcast should have exactly one item, has {len(items)}")
    item = items[0]
    enclosure = item.find("enclosure")
    sp = "{" + ns["sparkle"] + "}"
    require(item.findtext("sparkle:version", namespaces=ns) == metadata["build_version"], "appcast build differs from the app's CFBundleVersion")
    require(item.findtext("sparkle:shortVersionString", namespaces=ns) == metadata["version"], "appcast version differs")
    require(enclosure is not None and enclosure.get("url") == prefix + dmg.name, "appcast URL is not this release's DMG")
    require(int(enclosure.get("length", -1)) == dmg.stat().st_size, "appcast length differs from the DMG")
    signature = base64.b64decode(enclosure.get(sp + "edSignature", ""))
    public = base64.b64decode(metadata["sparkle"]["public_key"])
    require(ed25519_verify(public, dmg, signature),
            "the DMG's EdDSA signature doesn't match the app's SUPublicEDKey: wrong SPARKLE_ED_PRIVATE_KEY or public key")
    require("sparkle-signatures" in out.read_text(), "appcast is not signed (SURequireSignedFeed)")
    sparkle_run(sparkle_tool("sign_update"), "--verify", out)
    print(f"Verified appcast: build {metadata['build_version']}, {dmg.name}, EdDSA signature matches SUPublicEDKey", flush=True)


def release_notes(version, root=ROOT):
    """The version's release notes (markdown): docs/releases/X.Y.Z.md, written
    and committed before tagging (#106). Sparkle's update window shows them,
    and the GitHub release body is them plus the Full Changelog link."""
    require(re.fullmatch(SEMVER, version), f"version must be X.Y.Z, not {version!r}")
    name = f"{RELEASES}/{version}.md"
    path = root / name
    require(path.is_file(), f"no release notes for {version}: write {name} and commit it before tagging "
            "(docs/releasing.md, 'Cut a release')")
    notes = path.read_text(encoding="utf-8").strip()
    require(notes, f"{name} is empty: write {version}'s release notes before tagging")
    require(CHANGELOG not in notes, f"{name}: leave out the {CHANGELOG} line; the GitHub release adds it")
    return notes


def release_body(notes, tag, releases):
    """The GitHub release body: the notes, then GitHub's usual Full Changelog
    link from the newest published release before this one."""
    version = tuple(map(int, tag[1:].split(".")))
    earlier = [r["tag_name"] for r in releases if not r["draft"] and re.fullmatch("v" + SEMVER, r["tag_name"])
               and tuple(map(int, r["tag_name"][1:].split("."))) < version]
    previous = max(earlier, key=lambda t: tuple(map(int, t[1:].split("."))), default=None)
    link = (f"https://github.com/{REPO}/compare/{previous}...{tag}" if previous
            else f"https://github.com/{REPO}/commits/{tag}")
    return f"{notes}\n\n{CHANGELOG}: {link}"


def all_releases():
    pages = json.loads(command("gh", "api", "--paginate", "--slurp",
                               f"repos/{REPO}/releases?per_page=100", capture=True))
    return [r for page in pages for r in page]


def linux_packages(version):
    """Release asset name -> exact path in this run's Actions artifacts.
    Avoid '~' in upload names (GitHub normalizes special characters). The
    package's internal dpkg version still distinguishes distribution builds.
    """
    packages = {}
    for machine, arch in [('x86_64', 'amd64'), ('aarch64', 'arm64')]:
        tar = f'isocast-linux-{machine}'
        packages[f'{tar}.tar.gz'] = f'{tar}/{tar}.tar.gz'
        for distro in ('debian12', 'ubuntu24.04'):
            packages[f'isocast-{version}-{distro}-{arch}.deb'] = (
                f'isocast-deb-{distro}-{machine}/isocast_{version}-1~{distro}_{arch}.deb')
    return packages


def stage_linux(output, version):
    src = os.environ.get('ISOCAST_LINUX_ARTIFACTS')
    require(src, 'Linux attachment requires ISOCAST_LINUX_ARTIFACTS')
    packages = linux_packages(version)
    root = Path(src)
    actual = {str(p.relative_to(root)) for p in root.rglob('*') if not p.is_dir()}
    require(actual == set(packages.values()), f'incomplete or unexpected Linux artifacts for {version}: {sorted(actual)}')
    dest = output / 'linux'
    dest.mkdir(parents=True, exist_ok=True)
    files = []
    for name, source in sorted(packages.items()):
        path = root / source
        require(path.is_file() and not path.is_symlink() and path.stat().st_size, f'empty or invalid {path}')
        files.append(Path(shutil.copyfile(path, dest / name)))
    sums = dest / 'SHA256SUMS'
    sums.write_text(''.join(f'{sha(p)}  {p.name}\n' for p in files))
    return [*files, sums]


def find_release(tag):
    found = [r for r in all_releases() if r['tag_name'] == tag]
    require(len(found) <= 1, f'multiple releases already use {tag}; inspect them')
    return found[0] if found else None


def linux_body(body, tag):
    heading = '**Linux install and update**'
    if heading in body:
        return body
    notes = (f'{heading}\n\nChoose the `.deb` for your distro and architecture (`dpkg --print-architecture`) '
             'and download `SHA256SUMS` beside it. In an empty download folder run '
             '`sha256sum --ignore-missing -c SHA256SUMS`, check your package says OK, then '
             '`sudo apt install ./isocast-*.deb`. Debian 12 needs PipeWire ≥1.0.5 from bookworm-backports first; '
             f'[install instructions](https://github.com/{REPO}/blob/{tag}/packaging/deb/README.Debian). '
             'To update, finish all outputs, quit the app, stop the idle engine and install the newer package.')
    before, marker, after = body.partition(CHANGELOG)
    return (before.rstrip() + '\n\n' + notes + '\n\n' + marker + after).strip()


def fingerprints(release, names):
    return {a['name']: (a['id'], a['size'], a['digest']) for a in release['assets'] if a['name'] in names}


def attach_linux(output, version, dry_run=False):
    require(re.fullmatch(SEMVER, version), 'version must be X.Y.Z')
    files = stage_linux(output, version)
    tag = 'v' + version
    if dry_run:
        print(f'Would attach to an existing, unpublished {tag}; no Mac or Windows assets are written.')
        for p in files:
            print(f'{sha(p)}  {p.name}')
        return
    release = find_release(tag)
    require(release and release['draft'], f'{tag} is missing or published; refusing Linux attachment')
    rid = release['id']
    mac = f'Isocast-{version}-arm64.dmg'
    owned = {p.name for p in files}
    assets = {a['name']: a for a in release['assets']}
    adjacent = set(assets) - owned
    mac_set = {mac, mac + '.sha256', 'appcast.xml'}
    require(adjacent in (mac_set, mac_set | windows_asset_names(version)),
            f'{tag} needs a complete Mac draft with no unknown assets; found {sorted(adjacent)}')
    untouched = fingerprints(release, adjacent)
    expected = {p.name: (p.stat().st_size, 'sha256:' + sha(p)) for p in files}

    def matches(name, inventory):
        asset = inventory.get(name, {})
        return (asset.get('size'), asset.get('digest')) == expected[name]

    def check_draft():
        current = json.loads(command('gh', 'api', f'repos/{REPO}/releases/{rid}', capture=True))
        require(current['draft'] and current['tag_name'] == tag, f'{tag} is no longer this draft')
        inventory = {a['name']: a for a in current['assets']}
        require(set(inventory) - owned == adjacent and fingerprints(current, adjacent) == untouched,
                'non-Linux assets changed during attachment; inspect the draft')
        return current, inventory

    # Only missing/changed Linux files; publish their checksum last. An
    # interrupted upload is resumable without replacing matching assets.
    for p in files:
        if not matches(p.name, assets):
            check_draft()
            command('gh', 'release', 'upload', tag, p, '--repo', REPO, '--clobber')
    current, uploaded = check_draft()
    require(all(matches(n, uploaded) for n in expected), 'Linux asset digest or size differs; do not publish')
    body = linux_body(current.get('body') or '', tag)
    if body != current.get('body'):
        command('gh', 'api', '--method', 'PATCH', f'repos/{REPO}/releases/{rid}', '-f', 'body=' + body, capture=True)
    print(f'{tag}: verified all seven Linux assets; other asset IDs, sizes and digests unchanged. Draft only.')


def draft_release(output, metadata, files, replace=False, verify_tag=False):
    tag = "v" + metadata["version"]
    notes = release_notes(metadata["version"])
    if verify_tag:
        command("gh", "api", f"repos/{REPO}/git/ref/tags/{tag}", capture=True)

    release = find_release(tag)  # A failed read aborts; never assume absence.
    existed = release is not None
    # The Mac-only replacement stage cannot verify or replace Windows assets.
    # In particular, its obsolete-asset cleanup must never delete them. Finish
    # this stage before the release operator attaches the verified Windows set.
    require(not release or not any(
        "windows" in asset["name"].lower() for asset in release["assets"]
    ), "Windows assets are already attached; the Mac-only finalizer refuses this draft. "
       "Finish the Mac draft before attaching verified Windows assets.")
    require(not release or release['draft'], f'{tag} is published; refusing to change it')
    linux = {'SHA256SUMS', *linux_packages(metadata['version'])}
    keep = fingerprints(release, linux) if release else {}
    body = release_body(notes, tag, all_releases())
    if set(keep) == linux:
        body = linux_body(body, tag)
    # Sparkle's feed beside the DMG: what the published release's
    # releases/latest/download/appcast.xml serves to installed apps.
    files = [*files, appcast(output / "notarized", metadata, files[0], notes,
                             f"https://github.com/{REPO}/releases/download/{tag}/")]
    if release is None:
        flags = ["--verify-tag"] if verify_tag else []
        command("gh", "release", "create", tag, *files, "--draft", "--repo", REPO,
                "--target", "main", "--title", f"Isocast {metadata['version']} Alpha", *flags,
                "--notes", body)
        release = find_release(tag)
    require(release is not None and release["draft"], f"{tag} is absent or published; refusing to change it")
    if replace and existed:
        # The workflow serializes releases. Never replace a published release,
        # and never create a second draft when a tag already has one.
        require({a['name'] for a in release['assets']} <= {p.name for p in files} | set(keep),
                'unknown draft assets; inspect before replacing the Mac files')
        command("gh", "release", "upload", tag, *files, "--repo", REPO, "--clobber")
        command("gh", "release", "edit", tag, "--repo", REPO, "--draft", "--notes", body)
        release = find_release(tag)
        require(release is not None and release["draft"], "release is no longer a draft")
    assets = {a["name"]: a for a in release["assets"]}
    require(set(assets) == {p.name for p in files} | set(keep) and fingerprints(release, linux) == keep,
            'draft assets differ; Linux assets must survive a Mac refresh unchanged')
    for path in files:
        asset = assets[path.name]
        require(asset.get("digest") == "sha256:" + sha(path) and asset["size"] == path.stat().st_size,
                f"draft asset mismatch: {path.name}; existing release retained")
    save(output / "draft.json", {"id": release["id"], "url": release["html_url"],
                                "tag": tag, "draft": True})
    print("Verified draft:", release["html_url"], flush=True)


def windows_asset_names(version):
    installer = f"Isocast-{version}-windows-x64-setup.exe"
    return {installer, installer + ".sha256", installer + ".ed25519", "appcast-windows.xml"}


def published_release(tag, names, allow_windows=False):
    """The PUBLISHED release for exactly this tag, with exactly these assets.
    releases/tags/TAG serves published releases only: a draft is a 404."""
    try:
        release = json.loads(command("gh", "api", f"repos/{REPO}/releases/tags/{tag}", capture=True))
    except subprocess.CalledProcessError:
        raise ValueError(f"no published release for {tag} (missing, a draft, or the API failed; see above); refusing")
    require(release.get("tag_name") == tag and release.get("draft") is False,
            f"{tag} is not a published release; refusing (drafts are refreshed by the Release workflow)")
    assets = {a["name"]: a for a in release["assets"]}
    allowed = [names]
    if allow_windows:
        allowed.append(names | windows_asset_names(tag.removeprefix("v")))
        linux = {'SHA256SUMS', *linux_packages(tag.removeprefix('v'))}
        allowed += [s | linux for s in allowed]
    require(set(assets) in allowed, f"{tag} assets are {sorted(assets)}, expected {sorted(names)} "
            "(optionally the complete Windows and/or Linux sets); refusing")
    return release, assets


def appcast_item(path):
    """The appcast's one item, as the fields that must survive a refresh."""
    import xml.etree.ElementTree as ET
    ns = "{http://www.andymatuschak.org/xml-namespaces/sparkle}"
    items = ET.parse(path).getroot().findall("./channel/item")
    require(len(items) == 1, f"{path.name} should have exactly one item, has {len(items)}")
    item, fields = items[0], {}
    for child in item:
        if child.tag == "enclosure":
            fields.update({"enclosure." + k.replace(ns, "sparkle:"): v for k, v in child.attrib.items()})
        elif child.tag == "description":
            fields["description"] = (child.text or "").strip()
            fields["description.format"] = child.get(ns + "format")
        else:
            fields[child.tag.replace(ns, "sparkle:")] = (child.text or "").strip()
    return fields


def mounted_app_info(dmg):
    """Isocast.app's Info.plist inside the DMG, mounted read-only."""
    attached = plistlib.loads(command("hdiutil", "attach", "-readonly", "-nobrowse",
                                     "-plist", dmg, capture=True).encode())
    mounts = [Path(e["mount-point"]) for e in attached["system-entities"] if "mount-point" in e]
    try:
        require(len(mounts) == 1, "expected one mounted DMG volume")
        return plistlib.loads((mounts[0] / "Isocast.app/Contents/Info.plist").read_bytes())
    finally:
        for mount in mounts:
            command("hdiutil", "detach", mount)


def refresh_appcast(output, version, dry_run=False):
    """Replace a PUBLISHED release's appcast.xml with one that embeds
    docs/releases/X.Y.Z.md (#106). The DMG is downloaded and checked, never
    rebuilt or re-signed: the new feed must carry the same item and the same
    enclosure (URL, length, EdDSA signature), signed with the same key."""
    notes = release_notes(version)
    tag, dmg_name = "v" + version, f"Isocast-{version}-arm64.dmg"
    names = {dmg_name, dmg_name + ".sha256", "appcast.xml"}
    prefix = f"https://github.com/{REPO}/releases/download/{tag}/"
    release, assets = published_release(tag, names, allow_windows=True)
    # Preserve and verify the digest of every asset, including Linux and Windows. Later
    # reads require this exact set so an addition/removal during refresh fails.
    names = set(assets)
    print(f"{tag}: published {release.get('published_at')}, assets {sorted(names)}; "
          f"notes {RELEASES}/{version}.md ({len(notes)} characters)", flush=True)
    if dry_run:
        print("Download the assets; check every GitHub digest and the DMG's .sha256. Preserve other platforms' assets unchanged.")
        print("Mount the DMG read-only: its app's version, CFBundleVersion and SUPublicEDKey.")
        print("Verify the published appcast: one item for this DMG, EdDSA signature against SUPublicEDKey, feed signed.")
        print("generate_appcast on the same DMG with the notes embedded (key: SPARKLE_ED_PRIVATE_KEY on stdin only).")
        print("Require the same item and enclosure (URL, length, signature), the notes embedded, the feed signed.")
        print(f"gh release upload {tag} appcast.xml --clobber; re-check every asset digest. The DMG is never rebuilt or re-signed.")
        return
    # The release environment's key only: never fall back to a login keychain.
    require(os.environ.get("SPARKLE_ED_PRIVATE_KEY", "").strip(), "refresh needs SPARKLE_ED_PRIVATE_KEY (the release environment)")
    require(not output.exists(), f"output exists: {output}; choose a new one")
    original = output / "original"
    original.mkdir(parents=True)
    patterns = [arg for name in sorted(names) for arg in ("--pattern", name)]
    command("gh", "release", "download", tag, "--repo", REPO, "--dir", original, *patterns, timeout=900)
    for name, asset in assets.items():
        require(asset.get("digest") == "sha256:" + sha(original / name) and asset["size"] == (original / name).stat().st_size,
                f"downloaded {name} differs from the release's digest")
    dmg = original / dmg_name
    require((original / (dmg_name + ".sha256")).read_text() == f"{sha(dmg)}  {dmg_name}\n",
            f"{dmg_name} doesn't match its published .sha256")
    info = mounted_app_info(dmg)
    require(info.get("CFBundleShortVersionString") == version,
            f"the DMG's app is {info.get('CFBundleShortVersionString')}, not {version}")
    require(info.get("SUPublicEDKey"), "the DMG's app has no SUPublicEDKey")
    metadata = {"version": version, "build_version": info["CFBundleVersion"],
                "sparkle": {"public_key": info["SUPublicEDKey"]}}
    print("Published appcast:", flush=True)
    verify_appcast(original / "appcast.xml", metadata, dmg, prefix)
    out = output / "appcast.xml"
    refresh_feed(output, dmg, notes, prefix, metadata, original / "appcast.xml", out)
    # Check again right before the upload, and that only appcast.xml changes.
    again, now = published_release(tag, names)
    require(again["id"] == release["id"], f"{tag} changed while refreshing; refusing")
    for name in names - {"appcast.xml"}:
        require(now[name]["id"] == assets[name]["id"] and now[name].get("digest") == assets[name].get("digest"),
                f"{name} changed while refreshing; refusing")
    command("gh", "release", "upload", tag, out, "--repo", REPO, "--clobber")
    _, after = published_release(tag, names)
    require(after["appcast.xml"].get("digest") == "sha256:" + sha(out), "uploaded appcast.xml digest differs")
    for name in names - {"appcast.xml"}:
        require(after[name].get("digest") == assets[name].get("digest"), f"{name} changed; inspect {tag}")
    print(f"Refreshed {tag}'s appcast.xml (sha256 {sha(out)}); DMG and checksum unchanged", flush=True)


def refresh_feed(output, dmg, notes, prefix, metadata, published, out):
    """Regenerate PUBLISHED's feed for DMG with NOTES; require everything
    but the notes (and the feed signature) to stay the same."""
    from email.utils import parsedate_to_datetime
    old = appcast_item(published)
    # generate_appcast dates the item from the archive's creation date. Moving
    # the mtime back moves the creation date with it (APFS), so the published
    # pubDate is kept (as the same instant; it prints it in local time).
    stamp = parsedate_to_datetime(old["pubDate"]).timestamp()
    os.utime(dmg, (stamp, stamp))
    write_appcast(output / "appcast-work", dmg, notes, prefix, out)
    print("Refreshed appcast:", flush=True)
    verify_appcast(out, metadata, dmg, prefix)
    new = appcast_item(out)
    require(new.get("description") == notes and new.get("description.format") == "markdown",
            "the refreshed appcast doesn't embed the release notes as markdown")
    for item in (old, new):
        item["pubDate"] = parsedate_to_datetime(item["pubDate"]).timestamp()
    changed = sorted(k for k in old.keys() | new.keys() if k not in ("description", "description.format")
                     and old.get(k) != new.get(k))
    require(not changed, f"refreshing changed more than the notes: {changed}")
    print("Same item and enclosure (URL, length, EdDSA signature); notes embedded", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--notarize", action="store_true")
    parser.add_argument("--draft-release", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--verify-runtime", action="store_true")
    parser.add_argument("--hosted", action="store_true")
    parser.add_argument("--replace-draft", action="store_true")
    parser.add_argument("--verify-tag", action="store_true")
    parser.add_argument("--check-notes", metavar="X.Y.Z",
                        help=f"only check that {RELEASES}/X.Y.Z.md has this version's release notes")
    parser.add_argument("--refresh-appcast", metavar="X.Y.Z",
                        help="replace a published release's appcast.xml with one embedding its notes (CI)")
    parser.add_argument('--attach-linux', metavar='X.Y.Z', help='attach verified Linux packages to the existing draft')
    args = parser.parse_args()
    if args.check_notes:
        notes = release_notes(args.check_notes)
        print(f"{RELEASES}/{args.check_notes}.md: {len(notes.splitlines())} lines of release notes")
        return
    require(args.output, "--output is required")
    if args.attach_linux:
        require(not any((args.refresh_appcast, args.notarize, args.draft_release, args.verify_runtime,
                         args.hosted, args.replace_draft, args.verify_tag)), '--attach-linux takes --output and --dry-run only')
        attach_linux(args.output.resolve(), args.attach_linux, args.dry_run)
        return
    if args.refresh_appcast:
        require(not (args.notarize or args.draft_release or args.verify_runtime or args.hosted
                     or args.replace_draft or args.verify_tag), "--refresh-appcast takes only --output and --dry-run")
        refresh_appcast(args.output.resolve(), args.refresh_appcast, args.dry_run)
        return
    require(re.fullmatch(r"[A-Z0-9]{10}", TEAM), "APPLE_TEAM_ID must be a 10-character Apple team id")
    require(not args.hosted or args.verify_runtime, "--hosted needs --verify-runtime")
    require(not (args.replace_draft or args.verify_tag) or args.draft_release,
            "draft options need --draft-release")
    output = args.output.resolve()
    metadata = json.loads((output / "release.json").read_text())
    require(metadata.get("schema") == 1 and metadata.get("signing") == "developer-id",
            "requires a prepared Developer ID release, not an old/ad-hoc artifact")
    require(re.fullmatch(r"\d+\.\d+\.\d+", metadata["version"]), "invalid saved version")
    require(metadata["app_target"] in ("isocast-ui", "isocast-app"), "invalid saved UI target")
    require(args.notarize or args.draft_release or args.verify_runtime, "choose a finalization operation")
    if args.draft_release:
        notes = release_notes(metadata["version"])  # Before notarizing: a draft needs them.
    if args.dry_run:
        print(f"Prepared {metadata['app_target']} {metadata['version']}, build {metadata['build_version']}: {output}")
        print("Verify prepared Developer ID signatures and recorded hashes; acquire output operation lock.")
        if args.notarize:
            mode = "API-key env (path/id/issuer)" if any(os.environ.get(k) for k in ("APPLE_API_KEY_PATH", "APPLE_API_KEY_ID", "APPLE_API_ISSUER_ID")) else "isocast-notary/keychain profile"
            print(f"Copy app to notarized/; ditto ZIP; notarytool submit --no-wait with {mode}.")
            print("Save submission id/hash; notarytool wait --timeout 60s (20s progress); require Accepted.")
            print("Staple app; validate ticket/signature; repack designed DMG (scripts/dmg) with stapled app and Applications link.")
            print("Developer ID-sign DMG; submit/wait/require Accepted; staple DMG; hdiutil/codesign/stapler/spctl checks.")
            print("Verify executable/plist hashes unchanged; write notarized DMG SHA-256 and completion metadata.")
        print("Re-verify final signatures, tickets, accepted receipts and checksum.")
        if args.verify_runtime:
            print("Run mounted CLI/engine, isolated synthetic program+2-ISO takes; " +
                  ("qualify E2E 720p30 hosted loss tolerance, zero audio loss and full integrity." if args.hosted else "strict E2E 1080p60 checks."))
        if args.draft_release:
            if args.verify_tag:
                print("Require the version tag to exist in the remote repository.")
            if args.replace_draft:
                print("Replace assets/notes on an existing DRAFT; never alter a published release.")
            print(f"gh api repos/{REPO}/releases: find v{metadata['version']}; reuse only a matching draft.")
            print(f"Release notes: {RELEASES}/{metadata['version']}.md ({len(notes.splitlines())} lines).")
            print("Sparkle generate_appcast (key: SPARKLE_ED_PRIVATE_KEY on stdin, else login keychain): one item, "
                  f"those notes embedded (markdown), URL https://github.com/{REPO}/releases/download/v{metadata['version']}/<DMG>; feed signed.")
            print("Verify appcast build/version/URL/length, the DMG's EdDSA signature against the app's SUPublicEDKey, and the feed signature.")
            print(f"If absent: gh release create v{metadata['version']} <notarized DMG> <checksum> <appcast.xml> --draft --repo {REPO} --target main --notes <notes + Full Changelog link>")
            print('Create/refresh only the three Mac assets. Preserve any existing Linux assets.')
            print("Verify draft=true and every uploaded asset's SHA-256 digest; save draft.json URL. Never publish.")
        return
    lock = output / ".release-lock"
    lock.mkdir()  # Serialize finalization/draft operations for these artifacts.
    try:
        if args.notarize:
            finalize(output, metadata)
        files = verify_final(output, metadata, args.verify_runtime, args.hosted)
        if args.draft_release:
            draft_release(output, metadata, files, args.replace_draft, args.verify_tag)
    finally:
        lock.rmdir()


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, KeyError, subprocess.SubprocessError, TimeoutError) as error:
        sys.exit(f"release: {error}")
