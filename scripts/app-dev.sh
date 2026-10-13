#!/bin/sh
# Build the desktop app and assemble it as a regular app bundle, Isocast.app,
# with its icon, signed for this Mac. The app is a client of isocastd, which
# keeps its own signed bundle (scripts/sign-dev.sh); the app itself needs no
# camera or screen permissions.
#
# Usage: scripts/app-dev.sh [--release]
# The bundle goes to $ISOCAST_APP, or ~/Applications/Isocast.app. Signs with
# $ISOCAST_SIGN_IDENTITY, else the first "Apple Development" identity, else
# ad hoc. Builds into $CARGO_TARGET_DIR (or target/); --release builds an
# optimized app (pair it with `scripts/sign-dev.sh target/release/isocastd`).
set -eu

cd "$(dirname "$0")/.."
target="${CARGO_TARGET_DIR:-target}"
app="${ISOCAST_APP:-$HOME/Applications/Isocast.app}"
profile=debug
if [ "${1:-}" = "--release" ]; then
    profile=release
    cargo build --release -p isocast-app
else
    cargo build -p isocast-app
fi
binary="$target/$profile/isocast-app"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# The icon: Isocast.icon (Icon Composer's Liquid Glass format), compiled by
# Xcode's actool into Assets.car plus a flat Isocast.icns for older macOS.
# Without Xcode, the interface draws a flat one (`--route icon`).
mkdir -p "$work/icon"
if xcrun --find actool >/dev/null 2>&1; then
    xcrun actool "$PWD/crates/isocast-app/bundle/Isocast.icon" --compile "$work/icon" \
        --platform macosx --minimum-deployment-target 14.0 --app-icon Isocast \
        --output-partial-info-plist "$work/icon.plist" >/dev/null
else
    "$binary" --snapshot "$work/icon.png" --route icon --size 1024x1024 --scale 1 --wait-ms 200 >/dev/null
    iconset="$work/Isocast.iconset"
    mkdir -p "$iconset"
    for size in 16 32 128 256 512; do
        sips -z "$size" "$size" "$work/icon.png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
        double=$((size * 2))
        sips -z "$double" "$double" "$work/icon.png" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
    done
    iconutil -c icns "$iconset" -o "$work/icon/Isocast.icns"
fi

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp crates/isocast-app/bundle/Info.plist "$app/Contents/Info.plist"
cp "$binary" "$app/Contents/MacOS/Isocast"
cp "$work"/icon/* "$app/Contents/Resources/"

identity="${ISOCAST_SIGN_IDENTITY:-$(security find-identity -v -p codesigning 2>/dev/null |
    sed -n 's/.*"\(Apple Development[^"]*\)".*/\1/p' | head -n 1)}"
codesign --force --sign "${identity:--}" "$app"
codesign --verify "$app"
echo "built $app (signed with ${identity:-an ad hoc signature})"
