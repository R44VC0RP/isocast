#!/bin/sh
# Build the Slint app (isocast-ui) and wrap it as an app bundle, so macOS
# names it Isocast: the menu bar's app menu, About/Hide/Quit Isocast and
# the Dock. A bare binary (cargo run) is named after its file, isocast-ui,
# whatever its embedded Info.plist says. Until N.3 makes it Isocast.app,
# the bundle is "Isocast (Slint).app" so it sits beside the web app's.
#
# Usage: scripts/ui-dev.sh [--release]
# The bundle goes to $ISOCAST_UI_APP, or ~/Applications/Isocast (Slint).app.
# Run it as `open -g` would, or straight from a terminal (keeping stdio and
# flags): "$ISOCAST_UI_APP/Contents/MacOS/isocast-ui" --bench 30.
# Signs with $ISOCAST_SIGN_IDENTITY, else the first "Apple Development"
# identity, else ad hoc. Builds into $CARGO_TARGET_DIR (or target/).
set -eu

cd "$(dirname "$0")/.."
target="${CARGO_TARGET_DIR:-target}"
app="${ISOCAST_UI_APP:-$HOME/Applications/Isocast (Slint).app}"
profile=debug
if [ "${1:-}" = "--release" ]; then
    profile=release
    cargo build --release -p isocast-ui
else
    cargo build -p isocast-ui
fi

case "$app" in
*.app) ;;
*) echo "ISOCAST_UI_APP must end in .app: $app" >&2; exit 2 ;;
esac
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp crates/isocast-ui/Info.plist "$app/Contents/Info.plist"
cp "$target/$profile/isocast-ui" "$app/Contents/MacOS/isocast-ui"
# The app's icon, when Xcode's actool is here to compile it.
if xcrun --find actool >/dev/null 2>&1; then
    xcrun actool "$PWD/crates/isocast-app/bundle/Isocast.icon" --compile "$app/Contents/Resources" \
        --platform macosx --minimum-deployment-target 14.0 --app-icon Isocast \
        --output-partial-info-plist /dev/null >/dev/null
fi

identity="${ISOCAST_SIGN_IDENTITY:-$(security find-identity -v -p codesigning 2>/dev/null |
    sed -n 's/.*"\(Apple Development[^"]*\)".*/\1/p' | head -n 1)}"
codesign --force --sign "${identity:--}" "$app"
codesign --verify "$app"
echo "built $app (signed with ${identity:-an ad hoc signature})"
