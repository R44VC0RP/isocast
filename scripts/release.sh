#!/bin/bash
# Prepare a signed app/DMG; --notarize resumes those bytes without rebuilding.
# No checkout history is pushed. --draft-release creates at most one draft.
set -euo pipefail

cd "$(dirname "$0")/.."
root="$PWD"
app_target="${ISOCAST_APP_TARGET:-isocast-ui}" # Native default; one-line UI switch.
identity="${ISOCAST_SIGN_IDENTITY:-Developer ID Application: Ryan Vogel (9G68SMNHEU)}"
keychain="${ISOCAST_SIGN_KEYCHAIN:-$HOME/Library/Keychains/login.keychain-db}"
target="${CARGO_TARGET_DIR:-$root/target}"
dry_run=0
ad_hoc=0
draft=0
notarize=0
output=
runtime=0
update_test=
extra=()
# Sparkle (#40): the released app's update feed; its EdDSA public key is in
# scripts/sparkle/public-ed-key.txt and the private key never touches a build.
feed_url=https://github.com/R44VC0RP/isocast/releases/latest/download/appcast.xml
# Virtual camera (#67): Developer ID provisioning profiles for the engine
# helper (System Extension capability) and its embedded camera extension.
engine_profile="${ISOCAST_ENGINE_PROFILE:-$HOME/.config/isocast-release-secrets/isocast-engine.provisionprofile}"
camera_profile="${ISOCAST_CAMERA_PROFILE:-$HOME/.config/isocast-release-secrets/isocast-camera.provisionprofile}"

usage() {
    cat <<'EOF'
Usage: scripts/release.sh [--dry-run] [--ad-hoc] [--output DIRECTORY]
       scripts/release.sh --notarize [--draft-release] [--dry-run] --output DIRECTORY
       scripts/release.sh --draft-release [--dry-run] --output DIRECTORY

Default: build arm64 release binaries, package Isocast.app, sign inside out with
Developer ID, make/sign a DMG and write its SHA-256 file. No notarization or
upload. Output must be new; default: $CARGO_TARGET_DIR/dist/VERSION.

--dry-run        Print every step without building, signing, writing or uploading.
--ad-hoc         Local testing only; no Developer ID, notarization or Gatekeeper claim.
--notarize       Resume prepared Developer ID output: notarize/staple app, repack
                 its DMG, sign/notarize/staple/re-verify. No Cargo build or binary
                 re-signing. Final artifacts go into OUTPUT/notarized/.
--draft-release  Upload verified notarized DMG/checksum as ONE GitHub draft;
                 reuse a matching existing draft, refuse mismatched/published ones.
                 Combine with --notarize for the single morning command.
--output DIR     Build into, or resume from, this directory. Test before drafting.
--verify-runtime Run the mounted bundled CLI/engine and short synthetic takes.
--hosted         With --verify-runtime: qualify the existing E2E 720p30 loss policy.
--generate-notes With --draft-release: use GitHub-generated release notes.
--replace-draft  With --draft-release: replace assets on an existing draft only.
--verify-tag     With --draft-release: require the version tag to exist remotely.
--update-test FEED  Local update proofs only: a separate dev.isocast.update-test
                 app whose Sparkle feed is FEED and public key is
                 ISOCAST_SPARKLE_PUBLIC_KEY. Finalization refuses it.
                 ISOCAST_UPDATE_TEST_ENV (KEY=VALUE lines, e.g. ISOCAST_SOCKET)
                 becomes its LSEnvironment, so relaunches stay isolated.

Requirements: Apple-silicon Mac, Rust, Xcode (actool/notarytool/stapler), Python 3,
dmgbuild (scripts/dmg/requirements.txt; or uv runs the pin);
gh for drafts. Developer ID builds embed Sparkle (scripts/sparkle/sparkle.sh: pinned,
hash-verified download) and need the real key in scripts/sparkle/public-ed-key.txt;
--ad-hoc builds have no updater. Drafts sign the appcast with SPARKLE_ED_PRIVATE_KEY
(CI) or the login keychain's Sparkle key (local). Credentials stay in the login keychain's isocast-notary profile.
Developer ID builds embed the virtual camera extension and need its two Developer ID
provisioning profiles: ISOCAST_ENGINE_PROFILE (dev.isocast.isocastd, System Extension
capability) and ISOCAST_CAMERA_PROFILE (dev.isocast.isocastd.camera); defaults
~/.config/isocast-release-secrets/isocast-{engine,camera}.provisionprofile.
ISOCAST_APP_TARGET selects isocast-ui (default) or isocast-app (retired web app).
Notarization uses the login keychain profile isocast-notary. Waits report progress
every 20s; submission ids are saved for safe resume. Never publishes or pushes git.
CI: ISOCAST_SIGN_KEYCHAIN/ISOCAST_SIGN_IDENTITY select the temporary signing key;
APPLE_API_KEY_PATH + APPLE_API_KEY_ID + APPLE_API_ISSUER_ID select API-key
notarization instead of the local profile. APPLE_TEAM_ID defaults to 9G68SMNHEU.
EOF
}
die() { echo "release: $*" >&2; exit 1; }
run() {
    printf '+'
    printf ' %q' "$@"
    printf '\n'
    if [ "$dry_run" = 0 ]; then "$@"; fi
}
while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) dry_run=1; shift ;;
        --ad-hoc) ad_hoc=1; shift ;;
        --draft-release) draft=1; shift ;;
        --notarize) notarize=1; shift ;;
        --verify-runtime) runtime=1; extra+=(--verify-runtime); shift ;;
        --hosted|--generate-notes|--replace-draft|--verify-tag) extra+=("$1"); shift ;;
        --output) [ $# -ge 2 ] || die '--output needs a directory'; output="$2"; shift 2 ;;
        --update-test) [ $# -ge 2 ] || die '--update-test needs a feed URL'; update_test="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown option: $1 (see --help)" ;;
    esac
done
case "$app_target" in isocast-app|isocast-ui) ;; *) die 'ISOCAST_APP_TARGET: choose isocast-app or isocast-ui' ;; esac
[ "$ad_hoc" = 0 ] || { [ "$draft:$notarize:$runtime" = 0:0:0 ] || die 'finalization refuses ad-hoc artifacts'; }
[ "$(uname -s)/$(uname -m)" = Darwin/arm64 ] || die 'build on an Apple-silicon Mac'
if [ -n "$update_test" ]; then
    [ "$ad_hoc:$draft:$notarize:$runtime" = 0:0:0:0 ] || die '--update-test builds are Developer ID signed and never finalized'
    [[ "$update_test" =~ ^https?://[^[:space:]]+$ ]] || die '--update-test needs an http(s) feed URL'
fi

# Resume from the saved version/target, even if this checkout changes overnight.
if [ "$notarize:$draft:$runtime" != 0:0:0 ]; then
    [ -n "$output" ] || die '--notarize/--draft-release need --output with a prepared build'
    args=(--output "$output")
    if [ "$notarize" = 1 ]; then args+=(--notarize); fi
    if [ "$draft" = 1 ]; then args+=(--draft-release); fi
    if [ "$dry_run" = 1 ]; then args+=(--dry-run); fi
    if [ "${#extra[@]}" -gt 0 ]; then args+=("${extra[@]}"); fi
    exec python3 scripts/release-finalize.py "${args[@]}"
fi
[ "${#extra[@]}" = 0 ] || die 'finalization options need --notarize, --draft-release or --verify-runtime'

# One version source; reading it does not build or resolve dependencies in dry runs.
version="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\([^"]*\)"/\1/p' Cargo.toml)"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die 'workspace.package.version must be X.Y.Z'
case "$target" in /*) ;; *) target="$root/$target" ;; esac
output="${output:-$target/dist/$version}"
case "$output" in /*) ;; *) output="$root/$output" ;; esac
app="$output/Isocast.app"
engine="$app/Contents/Helpers/Isocast Engine.app"
# The engine's executable (#83): macOS names it in prompts such as the camera
# extension's approval alert. Cargo still builds it as isocastd.
engine_executable="Isocast Engine"
dmg="$output/Isocast-$version-arm64.dmg"
entitlements="$root/scripts/release-entitlements.plist"
app_entitlements="$root/scripts/release-app-entitlements.plist"
camera_id=dev.isocast.isocastd.camera
extension="$engine/Contents/Library/SystemExtensions/$camera_id.systemextension"
camera_work="$output/work/camera"

verify_app() {
    run codesign --verify --deep --strict --verbose=2 "$app"
    # The app and its engine must both be allowed to ask for the camera and
    # microphone, or macOS denies them silently with no prompt (#50).
    if [ "$dry_run" = 1 ]; then
        printf '+ python3 scripts/vcam-package.py verify-release %q %s\n' "$engine" "$ad_hoc_flag"
        return 0
    fi
    local bundle key
    for bundle in "$app" "$engine"; do
        for key in com.apple.security.device.camera com.apple.security.device.audio-input; do
            codesign -d --entitlements - --xml "$bundle" 2>/dev/null | grep -q "<key>$key</key>" ||
                die "$bundle is missing the $key entitlement; camera/microphone would be denied without a prompt"
        done
        # Without it macOS denies the local network silently (#57).
        /usr/libexec/PlistBuddy -c 'Print :NSLocalNetworkUsageDescription' "$bundle/Contents/Info.plist" 2>/dev/null | grep -q . ||
            die "$bundle has no NSLocalNetworkUsageDescription; streaming to a local-network server would fail without a prompt"
    done
    # The virtual camera (#67): extension present, signed, and (Developer ID)
    # each bundle's profile matching its team, app id, expiry, certificate
    # and signed entitlements. Ad-hoc builds carry it without profiles.
    python3 scripts/vcam-package.py verify-release "$engine" ${ad_hoc_flag:+"$ad_hoc_flag"} ||
        die 'the virtual camera extension or its provisioning profiles are missing or wrong (see above)'
}
verify_dmg() {
    run hdiutil verify "$dmg"
    run codesign --verify --strict --verbose=2 "$dmg"
}

if [ "$dry_run" = 0 ]; then
    [ ! -e "$output" ] || die "output exists: $output; choose a new --output (existing artifacts are never overwritten)"
    for tool in cargo python3 codesign security xcrun hdiutil ditto shasum; do
        command -v "$tool" >/dev/null || die "missing tool: $tool"
    done
    xcrun --find actool >/dev/null || die 'Xcode actool is required (no app is launched to render an icon)'
    scripts/dmg/make-dmg.sh --check >/dev/null || die 'pinned dmgbuild is required for the installer DMG (see scripts/dmg/requirements.txt)'
    if [ "$ad_hoc" = 0 ]; then
        security find-identity -v -p codesigning "$keychain" | grep -Fq "$identity" || die "missing signing identity: $identity"
    fi
fi
ad_hoc_flag=
if [ "$ad_hoc" = 1 ]; then ad_hoc_flag=--ad-hoc; fi
# The camera extension's profiles before anything is built (#67).
if [ "$ad_hoc" = 0 ]; then
    if [ -f "$engine_profile" ] && [ -f "$camera_profile" ]; then
        if [ "$dry_run" = 1 ]; then
            python3 scripts/vcam-package.py release-profiles "$engine_profile" "$camera_profile" ||
                die 'the virtual camera provisioning profiles are wrong (see above; docs/releasing.md)'
        else
            python3 scripts/vcam-package.py release-profiles "$engine_profile" "$camera_profile" \
                --identity "$identity" --keychain "$keychain" ||
                die 'the virtual camera provisioning profiles are wrong (see above; docs/releasing.md)'
        fi
    elif [ "$dry_run" = 1 ]; then
        echo "release: dry run: skipping the virtual camera profile check (no $engine_profile or $camera_profile; a real build stops here)"
    else
        die "missing the virtual camera's Developer ID provisioning profiles: $engine_profile and $camera_profile (set ISOCAST_ENGINE_PROFILE/ISOCAST_CAMERA_PROFILE; docs/releasing.md)"
    fi
fi
# The update key before anything is built: a release without it can't ship.
bundle_id=dev.isocast.release
sparkle_key=
if [ -n "$update_test" ]; then
    bundle_id=dev.isocast.update-test
    feed_url="$update_test"
    sparkle_key="${ISOCAST_SPARKLE_PUBLIC_KEY:?--update-test needs ISOCAST_SPARKLE_PUBLIC_KEY (a throwaway test key)}"
elif [ "$ad_hoc" = 0 ]; then
    if ! sparkle_key="$(scripts/sparkle/sparkle.sh public-key 2>/dev/null)"; then
        [ "$dry_run" = 1 ] || { scripts/sparkle/sparkle.sh public-key >/dev/null || true; die 'no Sparkle public key; see docs/releasing.md'; }
        echo "release: dry run: skipping the Sparkle public key check (scripts/sparkle/public-ed-key.txt is still the placeholder; a real build stops here)"
        sparkle_key=DRY-RUN-PLACEHOLDER-KEY
    fi
fi
if [ -n "$sparkle_key" ]; then
    echo "release: Sparkle feed $feed_url"
    if [ "$dry_run" = 0 ]; then scripts/sparkle/sparkle.sh path >/dev/null; fi
else
    echo "release: ad-hoc build without an updater (Sparkle needs Developer ID signing)"
fi
build_version="${ISOCAST_BUILD_VERSION:-$(date +%s)}"
[[ "$build_version" =~ ^[0-9]+$ ]] || die 'ISOCAST_BUILD_VERSION must be numeric'
run mkdir -p "$output/work" "$app/Contents/MacOS" "$app/Contents/Resources" \
    "$engine/Contents/MacOS" "$engine/Contents/Resources" "$engine/Contents/Library/SystemExtensions"
run env CARGO_TARGET_DIR="$target" MACOSX_DEPLOYMENT_TARGET=14.0 ISOCAST_BUILD_VERSION="$build_version" \
    nice -n 10 cargo build --locked --release --target aarch64-apple-darwin \
    -p "$app_target" -p isocast-daemon -p isocast-cli
binary_dir="$target/aarch64-apple-darwin/release"
run cp "$binary_dir/$app_target" "$app/Contents/MacOS/$app_target"
run cp "$binary_dir/isocast" "$app/Contents/MacOS/isocast"
run cp "$binary_dir/isocastd" "$engine/Contents/MacOS/$engine_executable"
# The CMIO camera extension (camera-extension/), embedded in the engine helper
# that activates it (OSSystemExtensionRequest on the first Start).
run env MACOSX_DEPLOYMENT_TARGET=14.0 sh scripts/build-camera-extension.sh "$camera_work"
run ditto "$camera_work/$camera_id.systemextension" "$extension"
run python3 -c 'import plistlib,sys
from pathlib import Path
app,engine,version,executable,build_version,bundle_id,feed,key,engine_executable,lsenv=sys.argv[1:]
usage={"NSCameraUsageDescription":"Isocast records cameras and capture cards into your scenes and recordings.",
       "NSMicrophoneUsageDescription":"Isocast records microphones and audio interfaces into your scenes and recordings.",
       "NSScreenCaptureUsageDescription":"Isocast captures the displays, windows and desktop audio you select for scenes and recordings.",
       # System Audio Recording (process taps, macOS 14.2+; #33): asked for
       # under the outer app, which macOS attributes the engine to.
       "NSAudioCaptureUsageDescription":"Isocast records the system or app audio you choose for your scenes and recordings.",
       # Without it macOS keeps the engine off the local network with no
       # prompt: a LAN RTMP server fails with "No route to host" (#57).
       "NSLocalNetworkUsageDescription":"Isocast streams to servers on your local network, such as an RTMP server on another computer."}
for source,dest,identifier,name,binary in [
    ("crates/isocast-ui/Info.plist" if executable=="isocast-ui" else "crates/isocast-app/bundle/Info.plist",app,bundle_id,"Isocast",executable),
    ("crates/isocast-daemon/Info.plist",engine,"dev.isocast.isocastd","Isocast Engine",engine_executable)]:
    p=plistlib.loads(Path(source).read_bytes())
    p.update(usage)
    p.update(CFBundleIdentifier=identifier,CFBundleName=name,CFBundleDisplayName=name,
             CFBundleExecutable=binary,CFBundleShortVersionString=version,CFBundleVersion=build_version,
             LSMinimumSystemVersion="14.0",LSArchitecturePriority=["arm64"])
    if dest==app and key:
        # Sparkle (src/update.rs): a daily check, asking before every install
        # (no silent install-on-quit), EdDSA-verified archive and feed.
        p.update(SUFeedURL=feed,SUPublicEDKey=key,SUEnableAutomaticChecks=True,
                 SUScheduledCheckInterval=86400,SUAllowsAutomaticUpdates=False,
                 SUAutomaticallyUpdate=False,SUVerifyUpdateBeforeExtraction=True,
                 SURequireSignedFeed=True)
    if dest==app and lsenv:
        p["LSEnvironment"]=dict(line.split("=",1) for line in lsenv.splitlines() if "=" in line)
    Path(dest,"Contents/Info.plist").write_bytes(plistlib.dumps(p))
# The camera extension shares the engine'"'"'s versions (sysextd replaces an
# activated extension when a newer build asks).
ext=Path(engine,"Contents/Library/SystemExtensions/dev.isocast.isocastd.camera.systemextension/Contents/Info.plist")
p=plistlib.loads(ext.read_bytes())
p.update(CFBundleShortVersionString=version,CFBundleVersion=build_version)
ext.write_bytes(plistlib.dumps(p))' "$app" "$engine" "$version" "$app_target" "$build_version" "$bundle_id" "$feed_url" "$sparkle_key" \
    "$engine_executable" "${update_test:+${ISOCAST_UPDATE_TEST_ENV:-}}"
run xcrun actool "$root/crates/isocast-app/bundle/Isocast.icon" --compile "$app/Contents/Resources" \
    --platform macosx --minimum-deployment-target 14.0 --app-icon Isocast \
    --output-partial-info-plist "$output/work/icon.plist"
run ditto "$app/Contents/Resources" "$engine/Contents/Resources"
run cp docs/install.md "$app/Contents/Resources/INSTALL.md"
run cp LICENSE-MIT LICENSE-APACHE THIRD_PARTY_NOTICES "$app/Contents/Resources/"
if [ -n "$sparkle_key" ]; then
    run scripts/sparkle/sparkle.sh embed "$app"
fi

# Check every shipped Mach-O: arm64 only, and no Homebrew/build-machine dylibs.
run python3 -c 'import subprocess,sys
from pathlib import Path
for p in Path(sys.argv[1]).rglob("*"):
    if not p.is_file() or p.read_bytes()[:4] != b"\xcf\xfa\xed\xfe": continue
    assert subprocess.check_output(["lipo","-archs",str(p)],text=True).strip()=="arm64",p
    lines=subprocess.check_output(["otool","-L",str(p)],text=True).splitlines()[1:]
    own=subprocess.check_output(["otool","-D",str(p)],text=True).splitlines()[1:]
    for line in lines:
        dep=line.strip().split(" (",1)[0]
        assert dep in own or dep.startswith(("/System/Library/", "/usr/lib/")), (p,dep)
    print("verified arm64 and system-only dependencies:",p)' "$app"

sign() {
    # A first Developer ID use can ask Ryan to unlock/allow the key. Do not
    # hang a build indefinitely or attempt to answer that security prompt.
    run python3 -c 'import subprocess,sys
try: subprocess.run(sys.argv[1:],check=True,timeout=90)
except subprocess.TimeoutExpired:
    sys.exit("codesign waited 90s: approve the login-keychain prompt, then retry the printed signing command. No password was read or stored.")' codesign "$@"
}
if [ "$ad_hoc" = 1 ]; then
    sign_args=(--force --sign - --timestamp=none --options runtime)
else
    sign_args=(--force --sign "$identity" --keychain "$keychain" --timestamp --options runtime)
fi
# Inside out. CLI record runs the engine in-process, so it needs the same
# three as isocastd. The app gets camera and audio-input only: macOS
# attributes the bundled engine's device access to Isocast.app, and denies it
# silently (no prompt) if the app lacks them (0.0.2, GitHub #50).
# Sparkle's helpers as its docs sign them: hardened runtime, no entitlements,
# no --deep (the app's signature then seals the framework).
if [ -n "$sparkle_key" ]; then
    sparkle="$app/Contents/Frameworks/Sparkle.framework"
    sign "${sign_args[@]}" "$sparkle/Versions/B/Autoupdate"
    sign "${sign_args[@]}" "$sparkle/Versions/B/Updater.app"
    sign "${sign_args[@]}" "$sparkle"
fi
# The camera extension, then the engine that contains it. Developer ID: each
# gets its Developer ID profile and the id/team entitlements it grants;
# ad-hoc: no profiles and no restricted entitlements (never activatable).
run python3 scripts/vcam-package.py release-entitlements "$camera_work/Camera.entitlements" \
    "$output/work/camera.entitlements" "$camera_id" ${ad_hoc_flag:+"$ad_hoc_flag"}
run python3 scripts/vcam-package.py release-entitlements "$entitlements" \
    "$output/work/engine.entitlements" dev.isocast.isocastd ${ad_hoc_flag:+"$ad_hoc_flag"}
if [ "$ad_hoc" = 0 ]; then
    run cp "$camera_profile" "$extension/Contents/embedded.provisionprofile"
    run cp "$engine_profile" "$engine/Contents/embedded.provisionprofile"
fi
sign "${sign_args[@]}" --entitlements "$output/work/camera.entitlements" "$extension"
sign "${sign_args[@]}" --entitlements "$output/work/engine.entitlements" "$engine"
sign "${sign_args[@]}" --entitlements "$entitlements" "$app/Contents/MacOS/isocast"
sign "${sign_args[@]}" --entitlements "$app_entitlements" "$app"
run codesign --verify --deep --strict --verbose=2 "$app"

verify_app
# Designed window (scripts/dmg/settings.py): the app beside an Applications link.
run scripts/dmg/make-dmg.sh "$app" "Isocast $version" "$dmg"
if [ "$ad_hoc" = 1 ]; then
    sign --force --sign - --timestamp=none "$dmg"
else
    sign --force --sign "$identity" --keychain "$keychain" --timestamp "$dmg"
fi
verify_dmg
# This is the prepared-file hash; finalization writes a new one after stapling.
run bash -c 'cd "$1" && shasum -a 256 "$2" > "$2.sha256" && shasum -a 256 -c "$2.sha256"' \
    bash "$output" "$(basename "$dmg")"
mode=developer-id
if [ "$ad_hoc" = 1 ]; then mode=ad-hoc; fi
if [ -n "$update_test" ]; then mode=update-test; fi
run python3 -c 'import hashlib,json,subprocess,sys
from pathlib import Path
w=Path(sys.argv[1]); app=w/"Isocast.app"; target=sys.argv[4]
engine="Contents/Helpers/Isocast Engine.app/Contents"
camera=f"{engine}/Library/SystemExtensions/dev.isocast.isocastd.camera.systemextension/Contents"
paths=[f"Contents/MacOS/{target}","Contents/MacOS/isocast","Contents/Info.plist",
       f"{engine}/MacOS/{sys.argv[9]}",f"{engine}/Info.plist",
       f"{camera}/MacOS/IsocastCamera",f"{camera}/Info.plist"]
if sys.argv[3]!="ad-hoc": paths+=[f"{engine}/embedded.provisionprofile",f"{camera}/embedded.provisionprofile"]
hashes={p:hashlib.sha256((app/p).read_bytes()).hexdigest() for p in paths}
r=dict(schema=1,version=sys.argv[2],signing=sys.argv[3],app_target=target,
       build_version=sys.argv[5],source_commit=subprocess.check_output(["git","rev-parse","HEAD"],text=True).strip(),
       app_files=hashes,dmg_sha256=hashlib.sha256(Path(sys.argv[6]).read_bytes()).hexdigest())
if sys.argv[7]: r["sparkle"]=dict(feed=sys.argv[7],public_key=sys.argv[8])
(w/"release.json").write_text(json.dumps(r,indent=2)+"\n")' \
    "$output" "$version" "$mode" "$app_target" "$build_version" "$dmg" "${sparkle_key:+$feed_url}" "$sparkle_key" \
    "$engine_executable"
if [ "$ad_hoc" = 0 ]; then
    run python3 -c 'import subprocess,sys
for args in [["--type","execute",sys.argv[1]],["--type","open","--context","context:primary-signature",sys.argv[2]]]:
    p=subprocess.run(["spctl","--assess","--verbose=2",*args],capture_output=True,text=True,timeout=60)
    print(p.stdout+p.stderr,end="")
    if p.returncode and not (p.returncode==3 and "source=Unnotarized Developer ID" in p.stderr):
        sys.exit("Unexpected Gatekeeper assessment failure")
    print("Prepared Developer ID artifact; notarization is a separate step.")' "$app" "$dmg"
fi
echo "release: prepared $mode artifacts: $output"
echo "Next, after testing: scripts/release.sh --notarize --draft-release --output '$output'"
