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
import re
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
    shared = str(stage).startswith("/Volumes/IsocastBuild/")
    if shared:
        require(Path("/Volumes/IsocastBuild").is_mount(), "mount IsocastBuild before building; no internal-disk fallback")
    stage.mkdir(parents=True, exist_ok=True)
    # The shared build disk's 20 GiB floor (agent rules). Elsewhere (a CI
    # runner after the Rust build, for example) the extension needs only MBs.
    require(not shared or shutil.disk_usage(stage).free >= 20 * 1024**3,
            "build volume has less than 20 GiB free; free space before building")
    version = next_version(stage)
    package = re.search(r"(?ms)^\[workspace\.package\]\n(.*?)(?=^\[|\Z)",
                        (repo / "Cargo.toml").read_text()).group(1)
    product_version = re.search(r'^version\s*=\s*"([^"]+)"', package, re.M).group(1)
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
            data["CFBundleShortVersionString"] = product_version
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


# ---- Release builds (scripts/release.sh): the extension inside Isocast.app's
# engine helper, Developer ID signing with Developer ID provisioning profiles.

RESTRICTED_PREFIXES = ("com.apple.developer.", "com.apple.application-identifier")
MIN_PROFILE_DAYS = 30


def decode_profile(path):
    require(path.is_file() and not path.is_symlink(), f"missing provisioning profile: {path}")
    return plistlib.loads(command("/usr/bin/security", "cms", "-D", "-i", str(path)).stdout)


def certificate_digests(profile):
    import hashlib
    return {hashlib.sha1(bytes(der)).hexdigest().upper() for der in profile.get("DeveloperCertificates", [])}


def check_release_profile(path, expected, certificate=None):
    """A Developer ID profile for exactly this bundle id, team and certificate."""
    profile = decode_profile(path)
    name = f"{path.name} ({expected})"
    require(profile.get("TeamIdentifier") == [TEAM], f"{name}: profile is for team {profile.get('TeamIdentifier')}, not {TEAM}")
    allowed = profile.get("Entitlements", {})
    app_id = allowed.get("com.apple.application-identifier", allowed.get("application-identifier"))
    require(app_id == f"{TEAM}.{expected}", f"{name}: profile is for app id {app_id!r}, not {TEAM}.{expected}")
    require(allowed.get("com.apple.developer.team-identifier") == TEAM, f"{name}: profile's team-identifier is not {TEAM}")
    require("OSX" in profile.get("Platform", []), f"{name}: not a macOS profile")
    # Developer ID profiles run on any Mac; development/ad-hoc ones list devices.
    require(profile.get("ProvisionsAllDevices") is True and not profile.get("ProvisionedDevices"),
            f"{name}: not a Developer ID profile (it lists devices); make it under Distribution > Developer ID")
    expiration = profile.get("ExpirationDate")
    require(isinstance(expiration, datetime.datetime), f"{name}: profile has no expiration date")
    left = expiration.replace(tzinfo=datetime.timezone.utc) - datetime.datetime.now(datetime.timezone.utc)
    require(left.days >= MIN_PROFILE_DAYS,
            f"{name}: profile expires {expiration:%Y-%m-%d} ({left.days} days); renew it (at least {MIN_PROFILE_DAYS} days needed)")
    if expected == HOST:
        require(allowed.get("com.apple.developer.system-extension.install") is True,
                f"{name}: profile lacks the System Extension capability (com.apple.developer.system-extension.install)")
    if certificate:
        require(certificate.upper() in certificate_digests(profile),
                f"{name}: profile doesn't include the signing certificate {certificate[:8]}…; regenerate it for that Developer ID Application certificate")
    print(f"Provisioning profile checked: {name}, Developer ID, team {TEAM}, expires {expiration:%Y-%m-%d}")
    return profile


def identity_digest(identity, keychain):
    """SHA-1 of the signing identity's certificate (identity: name or SHA-1)."""
    listing = command("/usr/bin/security", "find-identity", "-v", "-p", "codesigning",
                      *([keychain] if keychain else [])).stdout.decode()
    found = {digest for digest, label in re.findall(r'([0-9A-F]{40}) "([^"]+)"', listing)
             if identity in (digest, label)}
    require(len(found) == 1, "signing identity not found (or ambiguous) in the keychain")
    return found.pop()


def release_profiles(engine_profile, camera_profile, identity, keychain):
    certificate = identity_digest(identity, keychain) if identity else None
    check_release_profile(engine_profile, HOST, certificate)
    check_release_profile(camera_profile, EXTENSION, certificate)


def release_entitlements(base, output, expected, team_ids=True):
    """Signed entitlements: base (+ Xcode's id/team keys that the profile grants)."""
    data = plistlib.loads(base.read_bytes().replace(b"TEAMID", TEAM.encode()))
    if expected == HOST:
        data["com.apple.developer.system-extension.install"] = True
        data["com.apple.security.application-groups"] = [GROUP]
    if team_ids:
        data["com.apple.application-identifier"] = f"{TEAM}.{expected}"
        data["com.apple.developer.team-identifier"] = TEAM
    output.write_bytes(plistlib.dumps(data))


def leaf_digest(bundle):
    import hashlib
    import tempfile
    with tempfile.TemporaryDirectory() as folder:
        command("/usr/bin/codesign", "-d", f"--extract-certificates={folder}/cert", str(bundle))
        return hashlib.sha1(Path(folder, "cert0").read_bytes()).hexdigest().upper()


def verify_release(engine, ad_hoc):
    """The release app's engine helper and its embedded camera extension."""
    extension = engine / "Contents/Library/SystemExtensions" / (EXTENSION + ".systemextension")
    require(extension.is_dir() and not extension.is_symlink(),
            f"the release app has no camera extension at {extension}")
    folder = extension.parent
    require(sorted(p.name for p in folder.iterdir()) == [extension.name], "unexpected items in the engine's SystemExtensions folder")
    host_info, ext_info = plist(engine / "Contents/Info.plist"), plist(extension / "Contents/Info.plist")
    require(host_info.get("CFBundleIdentifier") == HOST, f"engine bundle id is {host_info.get('CFBundleIdentifier')!r}, not {HOST}")
    # The camera extension's producer path pin (camera-extension/Camera.swift)
    # and the clients' engine launch expect this executable (#83).
    require(host_info.get("CFBundleExecutable") == "Isocast Engine"
            and (engine / "Contents/MacOS/Isocast Engine").is_file(),
            f"engine executable is {host_info.get('CFBundleExecutable')!r}, not Contents/MacOS/Isocast Engine")
    require(ext_info.get("CFBundleIdentifier") == EXTENSION, f"extension bundle id is {ext_info.get('CFBundleIdentifier')!r}")
    require(ext_info.get("CFBundlePackageType") == "SYSX", "extension is not a system extension bundle (SYSX)")
    require((extension / "Contents/MacOS" / ext_info.get("CFBundleExecutable", "?")).is_file(), "extension executable missing")
    service = ext_info.get("CMIOExtension", {}).get("CMIOExtensionMachServiceName", "")
    require(service == GROUP + ".camera", f"CMIOExtensionMachServiceName {service!r} must be {GROUP}.camera (app-group prefixed)")
    require(host_info.get("CFBundleVersion") == ext_info.get("CFBundleVersion"), "engine and extension build versions differ")
    require(host_info.get("CFBundleShortVersionString") == ext_info.get("CFBundleShortVersionString"), "engine and extension versions differ")
    for bundle, expected in [(engine, HOST), (extension, EXTENSION)]:
        command("/usr/bin/codesign", "--verify", "--strict", str(bundle))
        signed = plistlib.loads(command("/usr/bin/codesign", "-d", "--entitlements", ":-", str(bundle)).stdout or plistlib.dumps({}))
        require(signed.get("com.apple.security.application-groups") == [GROUP], f"{expected}: signed app group must be [{GROUP}]")
        if expected == EXTENSION:
            require(signed.get("com.apple.security.app-sandbox") is True, f"{expected}: missing com.apple.security.app-sandbox")
        profile_path = bundle / "Contents/embedded.provisionprofile"
        details = command("/usr/bin/codesign", "-dv", "--verbose=4", str(bundle)).stderr.decode()
        if ad_hoc:
            require("Signature=adhoc" in details, f"{expected}: ad-hoc build is not ad-hoc signed")
            require(not profile_path.exists(), f"{expected}: ad-hoc build must not embed a provisioning profile")
            restricted = [k for k in signed if k.startswith(RESTRICTED_PREFIXES)]
            require(not restricted, f"{expected}: ad-hoc build carries restricted entitlements {restricted} (macOS would kill it)")
            print(f"Ad-hoc camera bundle checked: {expected} (not activatable: no Developer ID profile)")
            continue
        require(f"TeamIdentifier={TEAM}" in details and "Authority=Developer ID Application:" in details,
                f"{expected}: not signed with team {TEAM}'s Developer ID Application certificate")
        require("flags=0x10000(runtime)" in details, f"{expected}: hardened runtime is off")
        profile = check_release_profile(profile_path, expected, leaf_digest(bundle))
        allowed = profile["Entitlements"]
        require(signed.get("com.apple.application-identifier") == f"{TEAM}.{expected}",
                f"{expected}: signed com.apple.application-identifier is missing or wrong")
        require(signed.get("com.apple.developer.team-identifier") == TEAM, f"{expected}: signed team-identifier is missing")
        if expected == HOST:
            require(signed.get("com.apple.developer.system-extension.install") is True,
                    f"{expected}: missing com.apple.developer.system-extension.install")
        for key, value in signed.items():
            if key.startswith(RESTRICTED_PREFIXES):
                grant = allowed.get(key)
                ok = grant == value or (isinstance(grant, list) and isinstance(value, list)
                                        and all(any(fnmatch.fnmatchcase(v, g) for g in grant) for v in value))
                require(ok, f"{expected}: signed entitlement {key} isn't granted by its profile")
        print(f"Developer ID camera bundle checked: {expected}, embedded profile matches its signature")


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
    p = sub.add_parser("release-profiles", help="check both Developer ID profiles before a release build")
    p.add_argument("engine_profile", type=Path); p.add_argument("camera_profile", type=Path)
    p.add_argument("--identity"); p.add_argument("--keychain")
    p = sub.add_parser("release-entitlements", help="write a release bundle's signed entitlements")
    p.add_argument("base", type=Path); p.add_argument("output", type=Path)
    p.add_argument("bundle", choices=[HOST, EXTENSION]); p.add_argument("--ad-hoc", action="store_true")
    p = sub.add_parser("verify-release", help="check the release engine helper and its camera extension")
    p.add_argument("engine", type=Path); p.add_argument("--ad-hoc", action="store_true")
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
    elif args.action == "release-profiles":
        release_profiles(args.engine_profile, args.camera_profile, args.identity, args.keychain)
    elif args.action == "release-entitlements":
        if args.ad_hoc:
            # Restricted entitlements without a profile make macOS kill the
            # process; an ad-hoc engine keeps only the ordinary ones.
            data = plistlib.loads(args.base.read_bytes().replace(b"TEAMID", TEAM.encode()))
            if args.bundle == HOST: data["com.apple.security.application-groups"] = [GROUP]
            args.output.write_bytes(plistlib.dumps(data))
        else:
            release_entitlements(args.base, args.output, args.bundle)
    elif args.action == "verify-release": verify_release(args.engine, args.ad_hoc)
    elif args.action == "wait": wait_for_daemon(args.socket)
    elif args.action == "shutdown": control(args.socket, "shutdown")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, plistlib.InvalidFileException) as error:
        sys.exit(f"vcam: {error}")
