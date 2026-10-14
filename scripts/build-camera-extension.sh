#!/bin/sh
# Compile only. Does not install, activate, or sign with a developer identity.
# ISOCAST_TEAM_ID resolves the app-group/Mach-service prefix for signing later.
set -eu
out="${1:?usage: scripts/build-camera-extension.sh OUTPUT-DIRECTORY}"
team="${ISOCAST_TEAM_ID:-9G68SMNHEU}"
case "$team" in *[!A-Z0-9]*) echo 'ISOCAST_TEAM_ID must contain uppercase letters/digits' >&2; exit 2;; esac
[ "$team" = 9G68SMNHEU ] || { echo 'this extension authorizes team 9G68SMNHEU only' >&2; exit 2; }
bundle="$out/dev.isocast.isocastd.camera.systemextension"
python3 scripts/vcam-package.py prepare "$PWD" "$out"
python3 scripts/vcam-package.py clean-extension "$bundle"
mkdir -p "$bundle/Contents/MacOS" "$out/module-cache"
xcrun swiftc -O -swift-version 5 -target arm64-apple-macos14.0 \
    -module-cache-path "$out/module-cache" \
    camera-extension/Camera.swift camera-extension/main.swift \
    -o "$bundle/Contents/MacOS/IsocastCamera"
cp "$out/Extension-Info.plist" "$bundle/Contents/Info.plist"
sed "s/TEAMID/$team/g" camera-extension/Camera.entitlements > "$out/Camera.entitlements"
sed "s/TEAMID/$team/g" camera-extension/Host.entitlements > "$out/Host.entitlements"
echo "$bundle"
