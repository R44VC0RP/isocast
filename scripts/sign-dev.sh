#!/bin/sh
# Package the development isocastd as a signed, background-only app bundle,
# so macOS keeps its privacy permissions (camera, microphone, screen
# recording) across rebuilds: grants belong to the bundle id and signing
# identity, not to one build. Run it after every build of isocastd; the CLI
# launches the daemon from this bundle when it exists.
#
# Usage: scripts/sign-dev.sh [path/to/isocastd]
# Signs with $ISOCAST_SIGN_IDENTITY, or the first "Apple Development"
# identity in the keychain. The bundle goes to $ISOCAST_DEV_APP, or
# ~/Library/Application Support/Isocast/Isocast Dev.app (the internal disk).
set -eu

binary="${1:-target/debug/isocastd}"
if [ "${ISOCAST_CAMERA_EXTENSION:-0}" = 1 ]; then
    : "${ISOCAST_DEV_APP:?camera bundles require an explicit scratch ISOCAST_DEV_APP}"
    case "$ISOCAST_DEV_APP" in
        /Applications/*|"$HOME/Library/Application Support/Isocast/Isocast Dev.app") echo 'camera development bundles must not replace an installed/daily app' >&2; exit 2;;
    esac
    [ "${ISOCAST_SIGN_IDENTITY:-}" = - ] || { echo 'use vcam-install.sh for real signing and provisioning' >&2; exit 2; }
fi
app="${ISOCAST_DEV_APP:-$HOME/Library/Application Support/Isocast/Isocast Dev.app}"
identity="${ISOCAST_SIGN_IDENTITY:-$(security find-identity -v -p codesigning |
    sed -n 's/.*"\(Apple Development[^"]*\)".*/\1/p' | head -n 1)}"
if [ -z "$identity" ]; then
    echo "no code signing identity found; set ISOCAST_SIGN_IDENTITY" >&2
    exit 1
fi
if [ "${ISOCAST_CAMERA_EXTENSION:-0}" = 1 ] && [ "$identity" != - ]; then
    echo 'camera installation needs Xcode-managed profiles: use scripts/vcam-install.sh' >&2
    echo 'the sign-dev.sh camera option is only for ad-hoc (ISOCAST_SIGN_IDENTITY=-) scratch builds' >&2
    exit 2
fi

mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp crates/isocast-daemon/Info.plist "$app/Contents/Info.plist"
# The same workspace version as the embedded plist and production bundle.
version="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\([^"]*\)"/\1/p' Cargo.toml)"
/usr/libexec/PlistBuddy -c "Add :CFBundleShortVersionString string $version" "$app/Contents/Info.plist"
python3 scripts/vcam-package.py stamp "$app/Contents/Info.plist"
cp "$binary" "$app/Contents/MacOS/isocastd"
# The app's icon, which macOS shows in permission prompts and in Privacy &
# Security, when Xcode's actool is here to compile it.
if xcrun --find actool >/dev/null 2>&1; then
    xcrun actool "$PWD/crates/isocast-app/bundle/Isocast.icon" --compile "$app/Contents/Resources" \
        --platform macosx --minimum-deployment-target 14.0 --app-icon Isocast \
        --output-partial-info-plist /dev/null >/dev/null
fi
if [ "${ISOCAST_CAMERA_EXTENSION:-0}" = 1 ]; then
    : "${ISOCAST_TEAM_ID:?Set the Apple Developer team id to embed the camera extension}"
    camera_build="$(dirname "$binary")/camera-extension"
    sh scripts/build-camera-extension.sh "$camera_build"
    extension=dev.isocast.isocastd.camera.systemextension
    mkdir -p "$app/Contents/Library/SystemExtensions"
    python3 scripts/vcam-package.py clean-extension "$app/Contents/Library/SystemExtensions/$extension"
    ditto "$camera_build/$extension" "$app/Contents/Library/SystemExtensions/$extension"
    cp "$camera_build/Host-Info.plist" "$app/Contents/Info.plist"
    codesign --force --options runtime --sign "$identity" \
        --entitlements "$camera_build/Camera.entitlements" \
        "$app/Contents/Library/SystemExtensions/$extension"
    codesign --force --options runtime --sign "$identity" \
        --entitlements "$camera_build/Host.entitlements" "$app"
else
    codesign --force --sign "$identity" "$app"
fi
codesign --verify "$app"
echo "signed $app with \"$identity\""
