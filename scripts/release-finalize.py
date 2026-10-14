#!/usr/bin/env python3
"""Resume a prepared release without compiling or re-signing its executables.

Keep the tested originals; finalize a copy under notarized/. Persist submission
ids and hashes before waiting, so retries cannot submit the same archive twice.
Only a verified final DMG may reach a draft, and an existing tag is never replaced.
"""

import argparse
import hashlib
import json
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile
import time

IDENTITY = "Developer ID Application: Ryan Vogel (9G68SMNHEU)"
REQUIREMENT = '=anchor apple generic and certificate leaf[subject.OU] = "9G68SMNHEU"'
PROFILE = "isocast-notary"
KEYCHAIN = str(Path.home() / "Library/Keychains/login.keychain-db")
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


def command(*args, capture=False, check=True, timeout=180):
    args = [str(a) for a in args]
    print("+", shlex.join(args), flush=True)
    process = subprocess.Popen(args, stdout=subprocess.PIPE if capture else None,
                               text=True)
    start = time.monotonic()
    while True:
        try:
            stdout, _ = process.communicate(timeout=min(20, timeout))
            break
        except subprocess.TimeoutExpired:
            print(f"  {args[0]}: waiting ({int(time.monotonic() - start)}s)", flush=True)
            if time.monotonic() - start >= timeout:
                process.kill()
                process.communicate()
                raise TimeoutError(f"{args[0]} exceeded {timeout}s; retry the same release command")
    if check and process.returncode:
        raise subprocess.CalledProcessError(process.returncode, args, stdout)
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
    auth = ["--keychain-profile", PROFILE, "--keychain", KEYCHAIN]
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
        result = json.loads(raw)
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
        result.update(update)
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
    stage = final / "work" / "dmg"
    stage.mkdir(parents=True, exist_ok=True)
    command("ditto", final_app, stage / "Isocast.app")
    if not (stage / "Applications").is_symlink():
        (stage / "Applications").symlink_to("/Applications")
    command("cp", app / "Contents/Resources/INSTALL.md", stage / "Install.txt")
    dmg = final / dmg_name
    if not dmg.exists():
        command("hdiutil", "create", "-volname", f"Isocast {metadata['version']}",
                "-srcfolder", stage, "-format", "UDZO", "-fs", "HFS+", dmg)
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


def verify_final(output, metadata):
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
    with tempfile.TemporaryDirectory(prefix="verify-", dir=final) as mount:
        command("hdiutil", "attach", "-readonly", "-nobrowse", "-mountpoint", mount, dmg)
        try:
            mounted = Path(mount)
            verify_app(mounted / "Isocast.app", metadata)
            command("xcrun", "stapler", "validate", mounted / "Isocast.app")
            require((mounted / "Applications").is_symlink()
                    and (mounted / "Applications").readlink() == Path("/Applications"),
                    "DMG Applications link is missing")
        finally:
            command("hdiutil", "detach", mount)
    checksum = dmg.with_suffix(".dmg.sha256")
    require(checksum.read_text() == f"{sha(dmg)}  {dmg.name}\n", "final checksum mismatch")
    return dmg, checksum


def draft_release(output, metadata, files):
    tag = "v" + metadata["version"]

    def lookup():
        pages = json.loads(command("gh", "api", "--paginate", "--slurp",
                                   f"repos/{REPO}/releases?per_page=100", capture=True))
        found = [r for page in pages for r in page if r["tag_name"] == tag]
        require(len(found) <= 1, f"multiple releases already use {tag}; inspect them")
        return found[0] if found else None

    release = lookup()  # A failed/auth-denied read aborts; never assume absence.
    if release is None:
        command("gh", "release", "create", tag, *files, "--draft", "--repo", REPO,
                "--target", "main", "--title", f"Isocast {metadata['version']}", "--generate-notes",
                "--notes", "Apple silicon, macOS 14+. Open the DMG and drag Isocast to Applications. "
                "The CLI and installation guide are inside the app bundle. This is an alpha build.")
        release = lookup()
    require(release is not None and release["draft"], f"{tag} is absent or published; refusing to change it")
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
    args = parser.parse_args()
    output = args.output.resolve()
    metadata = json.loads((output / "release.json").read_text())
    require(metadata.get("schema") == 1 and metadata.get("signing") == "developer-id",
            "requires a prepared Developer ID release, not an old/ad-hoc artifact")
    require(re.fullmatch(r"\d+\.\d+\.\d+", metadata["version"]), "invalid saved version")
    require(metadata["app_target"] in ("isocast-ui", "isocast-app"), "invalid saved UI target")
    require(args.notarize or args.draft_release, "choose --notarize or --draft-release")
    if args.dry_run:
        print(f"Prepared {metadata['app_target']} {metadata['version']}, build {metadata['build_version']}: {output}")
        print("Verify prepared Developer ID signatures and recorded hashes; acquire output operation lock.")
        if args.notarize:
            print("Copy app to notarized/; ditto ZIP; notarytool submit --no-wait with isocast-notary/login keychain.")
            print("Save submission id/hash; notarytool wait --timeout 60s (20s progress); require Accepted.")
            print("Staple app; validate ticket/signature; repack DMG with stapled app and Applications link.")
            print("Developer ID-sign DMG; submit/wait/require Accepted; staple DMG; hdiutil/codesign/stapler/spctl checks.")
            print("Verify executable/plist hashes unchanged; write notarized DMG SHA-256 and completion metadata.")
        print("Re-verify final signatures, tickets, accepted receipts and checksum.")
        if args.draft_release:
            print(f"gh api repos/{REPO}/releases: find v{metadata['version']}; reuse only a matching draft.")
            print(f"If absent: gh release create v{metadata['version']} <notarized DMG> <checksum> --draft --repo {REPO} --target main --generate-notes")
            print("Verify draft=true and both uploaded asset SHA-256 digests; save draft.json URL. Never publish.")
        return
    lock = output / ".release-lock"
    lock.mkdir()  # Serialize finalization/draft operations for these artifacts.
    try:
        if args.notarize:
            finalize(output, metadata)
        files = verify_final(output, metadata)
        if args.draft_release:
            draft_release(output, metadata, files)
    finally:
        lock.rmdir()


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, KeyError, subprocess.SubprocessError, TimeoutError) as error:
        sys.exit(f"release: {error}")
