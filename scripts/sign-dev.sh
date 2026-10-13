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
app="${ISOCAST_DEV_APP:-$HOME/Library/Application Support/Isocast/Isocast Dev.app}"
identity="${ISOCAST_SIGN_IDENTITY:-$(security find-identity -v -p codesigning |
    sed -n 's/.*"\(Apple Development[^"]*\)".*/\1/p' | head -n 1)}"
if [ -z "$identity" ]; then
    echo "no code signing identity found; set ISOCAST_SIGN_IDENTITY" >&2
    exit 1
fi

mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp crates/isocast-daemon/Info.plist "$app/Contents/Info.plist"
cp "$binary" "$app/Contents/MacOS/isocastd"
# The app's icon, which macOS shows in permission prompts and in Privacy &
# Security, when Xcode's actool is here to compile it.
if xcrun --find actool >/dev/null 2>&1; then
    xcrun actool "$PWD/crates/isocast-app/bundle/Isocast.icon" --compile "$app/Contents/Resources" \
        --platform macosx --minimum-deployment-target 14.0 --app-icon Isocast \
        --output-partial-info-plist /dev/null >/dev/null
fi
codesign --force --sign "$identity" "$app"
codesign --verify "$app"
echo "signed $app with \"$identity\""
