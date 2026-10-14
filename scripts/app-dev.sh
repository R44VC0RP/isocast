#!/bin/sh
# Build the desktop app and assemble it as a regular app bundle, Isocast.app
# (dev.isocast.app), with its icon, signed for this Mac. The app is a client
# of isocastd, which keeps its own signed bundle (scripts/sign-dev.sh); the
# app itself needs no camera or screen permissions.
#
# Since N.3 the app is the Slint one (isocast-ui). The retired web app
# (isocast-app) still builds with ISOCAST_APP_TARGET=isocast-app until Ryan
# has used the Slint app for the alpha week.
#
# Usage: scripts/app-dev.sh [--release]
# The bundle goes to $ISOCAST_APP, or ~/Applications/Isocast.app. Signs with
# $ISOCAST_SIGN_IDENTITY, else the first "Apple Development" identity, else
# ad hoc. Builds into $CARGO_TARGET_DIR (or target/); --release builds an
# optimized app (pair it with `scripts/sign-dev.sh target/release/isocastd`).
set -eu

app_target="${ISOCAST_APP_TARGET:-isocast-ui}" # N.3: the Slint app; isocast-app is the web app.

cd "$(dirname "$0")/.."
target="${CARGO_TARGET_DIR:-target}"
app="${ISOCAST_APP:-$HOME/Applications/Isocast.app}"
case "$app_target" in
isocast-ui) plist=crates/isocast-ui/Info.plist executable=isocast-ui ;;
# Not "Isocast" for the web app's binary on disk: on a case-insensitive
# disk it would overwrite the CLI's `isocast`; inside the bundle it is.
isocast-app) plist=crates/isocast-app/bundle/Info.plist executable=Isocast ;;
*) echo "ISOCAST_APP_TARGET: choose isocast-ui or isocast-app, not $app_target" >&2; exit 2 ;;
esac
case "$app" in
*.app) ;;
*) echo "ISOCAST_APP must end in .app: $app" >&2; exit 2 ;;
esac
profile=debug
if [ "${1:-}" = "--release" ]; then
    profile=release
    cargo build --release -p "$app_target"
else
    cargo build -p "$app_target"
fi

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$plist" "$app/Contents/Info.plist"
cp "$target/$profile/$app_target" "$app/Contents/MacOS/$executable"
# The icon: Isocast.icon (Icon Composer's Liquid Glass format), compiled by
# Xcode's actool into Assets.car plus a flat Isocast.icns for older macOS.
if xcrun --find actool >/dev/null 2>&1; then
    xcrun actool "$PWD/crates/isocast-app/bundle/Isocast.icon" --compile "$app/Contents/Resources" \
        --platform macosx --minimum-deployment-target 14.0 --app-icon Isocast \
        --output-partial-info-plist /dev/null >/dev/null
else
    echo "no actool (Xcode): $app has no icon" >&2
fi

identity="${ISOCAST_SIGN_IDENTITY:-$(security find-identity -v -p codesigning 2>/dev/null |
    sed -n 's/.*"\(Apple Development[^"]*\)".*/\1/p' | head -n 1)}"
codesign --force --sign "${identity:--}" "$app"
codesign --verify "$app"
echo "built $app from $app_target (signed with ${identity:-an ad hoc signature})"
