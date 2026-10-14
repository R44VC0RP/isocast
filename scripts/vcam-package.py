#!/usr/bin/env python3
"""Installer checks: immutable Isocast identities, Xcode inputs, signed profiles.

No account updates or installation here. Profile contents and certificate data
are decoded in memory and never printed or persisted; only checks are reported.
"""
import argparse
import datetime
import fnmatch
import fcntl
import json
import os
from pathlib import Path
import plistlib
import shutil
import socket
import subprocess
import sys
import time

TEAM = "9G68SMNHEU"
HOST = "dev.isocast.isocastd"
EXTENSION = HOST + ".camera"
GROUP = TEAM + "." + HOST
APP = Path("/Applications/Isocast Camera Host.app")


def require(condition, message):
    if not condition:
        raise ValueError(message)


def command(*args):
    result = subprocess.run(args, capture_output=True, check=False)
    require(result.returncode == 0,
            f"{args[0]} {args[1]} failed (exit {result.returncode}): "
            + result.stderr.decode(errors="replace").strip())
    return result


def plist(path):
    with path.open("rb") as file:
        return plistlib.load(file)


def next_version(folder):
    """Monotonic timestamp build number, shared by the host and extension."""
    folder.mkdir(parents=True, exist_ok=True)
    with (folder / ".vcam-version").open("a+") as file:
        fcntl.flock(file, fcntl.LOCK_EX)
        file.seek(0)
        previous = file.read().strip()
        version = str(max(int(time.time()), int(previous or 0) + 1))
        file.seek(0); file.truncate(); file.write(version); file.flush()
    return version


def clean_extension(path):
    require(path.name == EXTENSION + ".systemextension" and not path.is_symlink(), "refusing unrelated/symlink extension")
    require(not str(path.resolve()).startswith("/Applications/"), "scratch extension cleanup cannot touch /Applications")
    if path.exists():
        require(plist(path / "Contents/Info.plist").get("CFBundleIdentifier") == EXTENSION,
                "refusing to remove an unrelated extension")
        shutil.rmtree(path)


def replace_host(source, destination_path):
    """Called only after the installer's copy confirmation, never by a build."""
    destination(destination_path)
    verify(source, False)
    temporary = destination_path.with_name(".Isocast Camera Host.installing.app")
    backup = destination_path.with_name(".Isocast Camera Host.previous.app")
    require(not temporary.exists() and not backup.exists(), "a prior installation staging/backup exists; inspect it before retrying")
    command("/usr/bin/ditto", str(source), str(temporary))
    verify(temporary, False)
    existed = destination_path.exists()
    if existed: destination_path.rename(backup)
    try:
        temporary.rename(destination_path)
        verify(destination_path, False)
    except Exception:
        if destination_path.exists(): destination_path.rename(temporary)
        if existed: backup.rename(destination_path)
        raise
    if existed: shutil.rmtree(backup)


def bundles(app):
    return [(app, HOST), (app / "Contents/Library/SystemExtensions" /
                         (EXTENSION + ".systemextension"), EXTENSION)]


def identities(app, complete=True):
    require(not app.is_symlink(), f"refusing symlink app: {app}")
    if not app.exists():
        require(not complete, f"missing app: {app}")
        return
    for bundle, expected in bundles(app):
        if not bundle.exists() and not complete:
            continue
        require(not bundle.is_symlink(), f"refusing symlink bundle: {bundle}")
        actual = plist(bundle / "Contents/Info.plist").get("CFBundleIdentifier")
        require(actual == expected,
                f"refusing bundle-id collision at {bundle}: expected {expected}, found {actual!r}; "
                "Studio NDI and other apps must never be changed")
    folder = app / "Contents/Library/SystemExtensions"
    if folder.exists():
        require(not folder.is_symlink(), "refusing symlink SystemExtensions folder")
        require(all(p.name == EXTENSION + ".systemextension" for p in folder.iterdir()),
                "refusing host containing an unrelated system extension")


def destination(app):
    require(app == APP, f"the only install/removal destination is {APP}")
    require(app.resolve() == APP, "refusing a redirected /Applications destination")
    identities(app, complete=False)
    # Never overwrite or remove a host running someone's recording. Only this
    # exact app's executables count; unrelated Isocast/Studio NDI are untouched.
    processes = command("/bin/ps", "-axo", "pid=,comm=").stdout.decode()
    active = [line.split(None, 1)[0] for line in processes.splitlines()
              if len(line.split(None, 1)) == 2
              and line.split(None, 1)[1].startswith(str(app / "Contents/MacOS") + "/")]
    require(not active, f"stop the Isocast Camera Host processes before replacing/removing it (pids {', '.join(active)})")
    print(f"Destination checked: {app}; no identity collision or running host")


def prepare(repo, stage):
    if str(stage).startswith("/Volumes/IsocastBuild/"):
        require(Path("/Volumes/IsocastBuild").is_mount(), "mount IsocastBuild before building; no internal-disk fallback")
    stage.mkdir(parents=True, exist_ok=True)
    require(shutil.disk_usage(stage).free >= 20 * 1024**3,
            "build volume has less than 20 GiB free; free space before building")
    version = next_version(stage)
    inputs = [(repo / "crates/isocast-daemon/Info.plist", "Host-Info.plist"),
              (repo / "camera-extension/Info.plist", "Extension-Info.plist"),
              (repo / "camera-extension/Host.entitlements", "Host.entitlements"),
              (repo / "camera-extension/Camera.entitlements", "Camera.entitlements")]
    for source, filename in inputs:
        data = plistlib.loads(source.read_bytes().replace(b"TEAMID", TEAM.encode()))
        if filename == "Host-Info.plist":
            data["CFBundleName"] = data["CFBundleDisplayName"] = "Isocast Camera Host"
            # Older standalone dev hosts do not understand uninstall/probe
            # switches: do not accidentally launch their default daemon.
            data["IsocastCameraInstallerVersion"] = 1
        if "CFBundleIdentifier" in data:
            data["CFBundleVersion"] = version
            expected = HOST if filename.startswith("Host") else EXTENSION
            require(data["CFBundleIdentifier"] == expected,
                    f"refusing changed bundle id in {source}; never reuse Studio NDI ids")
        (stage / filename).write_bytes(plistlib.dumps(data))
    print(f"Prepared Xcode inputs in {stage} (team {TEAM}, Isocast-only ids, build {version})")


def project_check(project, repo, stage, binary):
    result = command("/usr/bin/xcodebuild", "-project", str(project), "-alltargets",
                     "-configuration", "Release", "-showBuildSettings", "-json",
                     "CODE_SIGNING_ALLOWED=NO", "CODE_SIGNING_REQUIRED=NO",
                     f"ISOCAST_REPO_ROOT={repo}", f"ISOCAST_VCAM_STAGE={stage}",
                     f"ISOCAST_RUST_BINARY={binary}")
    targets = json.loads(result.stdout)
    require({t["target"] for t in targets} == {"IsocastCameraHost", "IsocastCameraExtension"},
            "refusing unexpected Xcode targets before any account operation")
    for target in targets:
        settings = target["buildSettings"]
        expected = HOST if target["target"] == "IsocastCameraHost" else EXTENSION
        require(settings.get("PRODUCT_BUNDLE_IDENTIFIER") == expected,
                "refusing Xcode bundle-id collision; only the two Isocast ids are allowed")
        require(settings.get("DEVELOPMENT_TEAM") == TEAM, "unexpected Xcode signing team")
        require(settings.get("CODE_SIGN_STYLE") == "Automatic", "automatic signing is required")
        entitlements = plist(Path(settings["CODE_SIGN_ENTITLEMENTS"]))
        require(entitlements.get("com.apple.security.application-groups") == [GROUP],
                "unexpected Xcode app group; Studio NDI groups must not be used")
        print(f"Xcode target checked: {expected}, Automatic signing, team {TEAM}")


def verify(app, unsigned):
    identities(app)
    require(plist(app / "Contents/Info.plist").get("IsocastCameraInstallerVersion") == 1,
            "host predates installer maintenance commands; rebuild with scripts/vcam-install.sh")
    command("/usr/bin/codesign", "--verify", "--deep", "--strict", str(app))
    for bundle, expected in bundles(app):
        signed = command("/usr/bin/codesign", "-d", "--entitlements", ":-", str(bundle))
        entitlements = plistlib.loads(signed.stdout)
        require(entitlements.get("com.apple.security.application-groups") == [GROUP],
                f"{expected}: missing/wrong signed application group")
        capability = ("com.apple.developer.system-extension.install" if expected == HOST
                      else "com.apple.security.app-sandbox")
        require(entitlements.get(capability) is True, f"{expected}: missing {capability}")
        if expected == HOST:
            for key in ["com.apple.security.device.camera", "com.apple.security.device.audio-input",
                        "com.apple.security.cs.allow-jit", "com.apple.security.cs.allow-unsigned-executable-memory"]:
                require(entitlements.get(key) is True, f"host must preserve capture/JIT entitlement {key}")
            info = plist(bundle / "Contents/Info.plist")
            for key in ["NSCameraUsageDescription", "NSMicrophoneUsageDescription", "NSScreenCaptureUsageDescription", "NSAudioCaptureUsageDescription"]:
                require(bool(info.get(key)), f"host is missing {key}")
            require(info["CFBundleVersion"] == plist(bundles(app)[1][0] / "Contents/Info.plist")["CFBundleVersion"], "host/extension build versions differ")
        if unsigned:
            print(f"Ad-hoc build checked: {expected}; NOT eligible for installation")
            continue
        details = command("/usr/bin/codesign", "-dv", "--verbose=4", str(bundle)).stderr.decode()
        require(f"TeamIdentifier={TEAM}" in details and "Signature=adhoc" not in details,
                f"{expected}: not signed by the required Apple team")
        profile_path = bundle / "Contents/embedded.provisionprofile"
        require(profile_path.is_file(), f"{expected}: missing embedded provisioning profile; use Xcode automatic signing")
        profile = plistlib.loads(command("/usr/bin/security", "cms", "-D", "-i", str(profile_path)).stdout)
        require(TEAM in profile.get("TeamIdentifier", []), f"{expected}: wrong profile team")
        expiration = profile.get("ExpirationDate")
        require(isinstance(expiration, datetime.datetime)
                and expiration.replace(tzinfo=datetime.timezone.utc) > datetime.datetime.now(datetime.timezone.utc),
                f"{expected}: expired/invalid provisioning profile")
        allowed = profile.get("Entitlements", {})
        app_id = allowed.get("com.apple.application-identifier", allowed.get("application-identifier"))
        require(app_id == f"{TEAM}.{expected}", f"{expected}: profile is not for this exact bundle id")
        signed_id = entitlements.get("com.apple.application-identifier", entitlements.get("application-identifier"))
        require(signed_id == app_id, f"{expected}: signed app identifier disagrees with profile")
        groups = allowed.get("com.apple.security.application-groups", [])
        require(any(fnmatch.fnmatchcase(GROUP, value) for value in groups),
                f"{expected}: profile does not authorize the app group")
        if expected == HOST:
            require(allowed.get(capability) is True, "host profile lacks system-extension.install")
        print(f"Signed entitlements and exact-id embedded profile checked: {expected}")
        if expected == HOST and "Authority=Developer ID Application:" in details:
            assessment = subprocess.run(["/usr/sbin/spctl", "--assess", "--type", "execute", "--verbose=2", str(app)],
                                        capture_output=True, text=True)
            require(assessment.returncode == 0,
                    "Developer ID Gatekeeper assessment failed; notarize that distribution build before installing: " + assessment.stderr.strip())
        elif expected == HOST:
            print("Local Apple Development build: Gatekeeper distribution/notarization assessment is not applicable")


def check_setup(folder):
    # Only synthetic video. It neither records files nor opens real devices.
    folder.mkdir(parents=True, exist_ok=True)
    require(len(os.fsencode(folder / "daemon.sock")) < 104,
            "check socket path is too long; use a shorter --build-dir")
    text = f'''version = 1
name = "Isocast camera installation check"
[canvas]
width = 1920
height = 1080
fps = 60
[[sources]]
id = "pattern"
kind = "test_pattern"
width = 1920
height = 1080
fps = 60
[[scenes]]
id = "check"
items = [{{ source = "pattern", rect = {{ x = 0, y = 0, width = 1920, height = 1080 }} }}]
[recording]
folder = {json.dumps(str(folder / "unused-recordings"))}
[recording.program]
enabled = false
encoder = {{ codec = "hevc" }}
'''
    (folder / "input.toml").write_text(text)


def control(path, method):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(5)
        connection.connect(str(path))
        connection.sendall((json.dumps({"jsonrpc": "2.0", "id": 1, "method": method}) + "\n").encode())
        response = json.loads(connection.makefile().readline())
        require("result" in response, f"isolated daemon refused {method}: {response.get('error')}")


def wait_for_daemon(path):
    until = time.monotonic() + 15
    while True:
        try:
            control(path, "status")
            print("Isolated installation-check daemon is ready")
            return
        except (ConnectionRefusedError, FileNotFoundError):
            if time.monotonic() >= until:
                raise ValueError("isolated daemon did not start; see check folder's daemon.log")
            time.sleep(0.1)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="action", required=True)
    p = sub.add_parser("prepare"); p.add_argument("repo", type=Path); p.add_argument("stage", type=Path)
    p = sub.add_parser("project-check")
    for name in ("project", "repo", "stage", "binary"): p.add_argument(name, type=Path)
    p = sub.add_parser("destination"); p.add_argument("app", type=Path)
    p = sub.add_parser("verify"); p.add_argument("app", type=Path); p.add_argument("--unsigned", action="store_true")
    p = sub.add_parser("check-setup"); p.add_argument("folder", type=Path)
    p = sub.add_parser("clean-extension"); p.add_argument("path", type=Path)
    p = sub.add_parser("replace-host"); p.add_argument("source", type=Path); p.add_argument("destination", type=Path)
    p = sub.add_parser("stamp"); p.add_argument("path", type=Path)
    for action in ("wait", "shutdown"):
        p = sub.add_parser(action); p.add_argument("socket", type=Path)
    args = parser.parse_args()
    if args.action == "prepare": prepare(args.repo, args.stage)
    elif args.action == "project-check": project_check(args.project, args.repo, args.stage, args.binary)
    elif args.action == "destination": destination(args.app)
    elif args.action == "verify": verify(args.app, args.unsigned)
    elif args.action == "check-setup": check_setup(args.folder)
    elif args.action == "clean-extension": clean_extension(args.path)
    elif args.action == "replace-host": replace_host(args.source, args.destination)
    elif args.action == "stamp":
        data = plist(args.path); data["CFBundleVersion"] = next_version(args.path.parent.parent.parent)
        args.path.write_bytes(plistlib.dumps(data))
    elif args.action == "wait": wait_for_daemon(args.socket)
    elif args.action == "shutdown": control(args.socket, "shutdown")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, plistlib.InvalidFileException) as error:
        sys.exit(f"vcam: {error}")
