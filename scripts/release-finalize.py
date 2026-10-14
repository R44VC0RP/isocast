#!/usr/bin/env python3
"""Resume a prepared release without compiling or re-signing its executables.

Keep the tested originals; finalize a copy under notarized/. Persist submission
ids and hashes before waiting, so retries cannot submit the same archive twice.
Only a verified final DMG may reach a draft, and an existing tag is never replaced.
"""

import argparse
import hashlib
import fcntl
import json
import os
from pathlib import Path
import plistlib
import re
import runpy
import shlex
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
    daemon = app / "Contents/Helpers/Isocast Engine.app/Contents/MacOS/isocastd"
    # Signing/notary/GitHub credentials never enter the recorded application.
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(("ISOCAST_", "APPLE_", "DEVELOPER_"))
           and k not in ("GH_TOKEN", "GITHUB_TOKEN")}
    env.update(ISOCAST_SOCKET=str(sock), ISOCAST_SETUP=str(root / "setup.toml"),
               ISOCAST_TIMELINES=str(root / "timelines"), ISOCAST_PLUGINS=str(root / "plugins"),
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


def draft_release(output, metadata, files, replace=False, generated=False, verify_tag=False):
    tag = "v" + metadata["version"]
    if verify_tag:
        command("gh", "api", f"repos/{REPO}/git/ref/tags/{tag}", capture=True)
    notes = None
    if generated:
        notes = json.loads(command("gh", "api", "--method", "POST", f"repos/{REPO}/releases/generate-notes",
                                   "-f", f"tag_name={tag}", "-f", "target_commitish=main", capture=True))["body"]

    def lookup():
        pages = json.loads(command("gh", "api", "--paginate", "--slurp",
                                   f"repos/{REPO}/releases?per_page=100", capture=True))
        found = [r for page in pages for r in page if r["tag_name"] == tag]
        require(len(found) <= 1, f"multiple releases already use {tag}; inspect them")
        return found[0] if found else None

    release = lookup()  # A failed/auth-denied read aborts; never assume absence.
    existed = release is not None
    if release is None:
        flags = ["--verify-tag"] if verify_tag else []
        if generated:
            flags += ["--generate-notes"]
        command("gh", "release", "create", tag, *files, "--draft", "--repo", REPO,
                "--target", "main", "--title", f"Isocast {metadata['version']} Alpha", *flags,
                *([] if generated else ["--notes", "Isocast's first alpha for Apple-silicon Macs (macOS 14+).\n\n"
                "- Native macOS app with scenes, sources and Studio Mode.\n"
                "- Program and per-source ISO recording, streaming, and pause/resume.\n"
                "- Virtual camera (requires the one-time installation) and customizable hotkeys.\n"
                "- Create view for recording, retaking and arranging takes.\n"
                "- Report a Bug with diagnostic previews and a shareable report.\n\n"
                "Open the DMG and drag Isocast to Applications. The CLI is bundled inside the app. "
                "See the [virtual-camera installation guide](https://github.com/R44VC0RP/isocast/blob/main/docs/virtual-camera.md) "
                "for its one-time setup."]))
        release = lookup()
    require(release is not None and release["draft"], f"{tag} is absent or published; refusing to change it")
    if replace and existed:
        # The workflow serializes releases. Never replace a published release,
        # and never create a second draft when a tag already has one.
        command("gh", "release", "upload", tag, *files, "--repo", REPO, "--clobber")
        for asset in release["assets"]:
            if asset["name"] not in {p.name for p in files}:
                command("gh", "api", "--method", "DELETE", f"repos/{REPO}/releases/assets/{asset['id']}")
        if notes is not None:
            command("gh", "release", "edit", tag, "--repo", REPO, "--draft", "--notes", notes)
        release = lookup()
        require(release is not None and release["draft"], "release is no longer a draft")
    assets = {a["name"]: a for a in release["assets"]}
    require(set(assets) == {p.name for p in files}, "existing draft assets differ; inspect without creating another")
    for path in files:
        asset = assets[path.name]
        require(asset.get("digest") == "sha256:" + sha(path) and asset["size"] == path.stat().st_size,
                f"draft asset mismatch: {path.name}; existing release retained")
    save(output / "draft.json", {"id": release["id"], "url": release["html_url"],
                                "tag": tag, "draft": True})
    print("Verified draft:", release["html_url"], flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--notarize", action="store_true")
    parser.add_argument("--draft-release", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--verify-runtime", action="store_true")
    parser.add_argument("--hosted", action="store_true")
    parser.add_argument("--replace-draft", action="store_true")
    parser.add_argument("--generate-notes", action="store_true")
    parser.add_argument("--verify-tag", action="store_true")
    args = parser.parse_args()
    require(re.fullmatch(r"[A-Z0-9]{10}", TEAM), "APPLE_TEAM_ID must be a 10-character Apple team id")
    require(not args.hosted or args.verify_runtime, "--hosted needs --verify-runtime")
    require(not (args.replace_draft or args.generate_notes or args.verify_tag) or args.draft_release,
            "draft options need --draft-release")
    output = args.output.resolve()
    metadata = json.loads((output / "release.json").read_text())
    require(metadata.get("schema") == 1 and metadata.get("signing") == "developer-id",
            "requires a prepared Developer ID release, not an old/ad-hoc artifact")
    require(re.fullmatch(r"\d+\.\d+\.\d+", metadata["version"]), "invalid saved version")
    require(metadata["app_target"] in ("isocast-ui", "isocast-app"), "invalid saved UI target")
    require(args.notarize or args.draft_release or args.verify_runtime, "choose a finalization operation")
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
            if args.generate_notes:
                print("Generate GitHub release notes for this tag.")
            if args.replace_draft:
                print("Replace assets/notes on an existing DRAFT; never alter a published release.")
            print(f"gh api repos/{REPO}/releases: find v{metadata['version']}; reuse only a matching draft.")
            notes_flag = "--generate-notes" if args.generate_notes else "--notes <alpha summary>"
            print(f"If absent: gh release create v{metadata['version']} <notarized DMG> <checksum> --draft --repo {REPO} --target main {notes_flag}")
            print("Verify draft=true and both uploaded asset SHA-256 digests; save draft.json URL. Never publish.")
        return
    lock = output / ".release-lock"
    lock.mkdir()  # Serialize finalization/draft operations for these artifacts.
    try:
        if args.notarize:
            finalize(output, metadata)
        files = verify_final(output, metadata, args.verify_runtime, args.hosted)
        if args.draft_release:
            draft_release(output, metadata, files, args.replace_draft, args.generate_notes, args.verify_tag)
    finally:
        lock.rmdir()


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, KeyError, subprocess.SubprocessError, TimeoutError) as error:
        sys.exit(f"release: {error}")
