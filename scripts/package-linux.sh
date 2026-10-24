#!/bin/sh
# Package a Linux build: dist/isocast-linux-<arch>.tar.gz holding isocast,
# isocastd, isocast-ui (from BIN_DIR, default target/release), the desktop
# entry, icon, install.sh and README.txt (packaging/linux).
#
#   scripts/package-linux.sh [BIN_DIR]
set -eu
bin=${1:-target/release}
arch=$(uname -m)
name=isocast-linux-$arch
out=dist/$name
rm -rf "$out" "dist/$name.tar.gz"
mkdir -p "$out"
for exe in isocast isocastd isocast-ui; do
  test -x "$bin/$exe" || { echo "package-linux: no $bin/$exe" >&2; exit 1; }
  cp "$bin/$exe" "$out/"
  # Symbols off the copies: about half the size (crash logs keep their
  # addresses; the build keeps its own symbols).
  strip "$out/$exe"
done
cp packaging/linux/dev.isocast.Isocast.desktop packaging/linux/dev.isocast.Isocast.png \
  packaging/linux/install.sh packaging/linux/README.txt "$out/"
cp LICENSE-MIT LICENSE-APACHE "$out/" 2>/dev/null || true
test -f THIRD_PARTY_NOTICES && cp THIRD_PARTY_NOTICES "$out/"
chmod 755 "$out/install.sh"
tar -C dist -czf "dist/$name.tar.gz" "$name"
echo "dist/$name.tar.gz"
