#!/bin/sh
# Package a Linux build: dist/isocast-linux-<arch>.tar.gz holding isocast,
# isocastd, isocast-ui (from BIN_DIR, default target/release), the desktop
# entry, icon, install.sh and README.txt (packaging/linux). No OpenH264:
# install.sh downloads Cisco's binary on the user's computer.
#
#   scripts/package-linux.sh [BIN_DIR]
set -eu
bin=${1:-target/release}
for tool in nm strings strip; do
  command -v "$tool" >/dev/null || { echo "package-linux: $tool is needed (binutils)" >&2; exit 1; }
done
arch=$(uname -m)
name=isocast-linux-$arch
out=dist/$name
rm -rf "$out" "dist/$name.tar.gz"
mkdir -p "$out"
for exe in isocast isocastd isocast-ui; do
  test -x "$bin/$exe" || { echo "package-linux: no $bin/$exe" >&2; exit 1; }
  # No OpenH264 compiled in: Cisco's licence covers only its own binary,
  # which install.sh downloads (crates/isocast-openh264 loads it).
  symbols=$(nm "$bin/$exe")
  if printf '%s\n' "$symbols" | grep -E -q ' [TtWw] _?(WelsCreateSVCEncoder|WelsCreateDecoder)$'; then
    echo "package-linux: $bin/$exe has OpenH264 compiled in; build without openh264's source feature" >&2
    exit 1
  fi
  cp "$bin/$exe" "$out/"
  # Symbols off the copies: about half the size (crash logs keep their
  # addresses; the build keeps its own symbols).
  strip "$out/$exe"
  # OpenH264's own log strings, there only if it was compiled in (the
  # loader names the entry points it looks up, so those prove nothing).
  text=$(strings "$out/$exe")
  if printf '%s\n' "$text" | grep -E -q 'openh264 codec version|CWelsDecoder::'; then
    echo "package-linux: $out/$exe contains OpenH264 code" >&2
    exit 1
  fi
done
cp packaging/linux/dev.isocast.Isocast.desktop packaging/linux/dev.isocast.Isocast.png \
  packaging/linux/install.sh packaging/linux/README.txt packaging/linux/CISCO_H264_LICENSE.txt "$out/"
cp LICENSE-MIT LICENSE-APACHE "$out/" 2>/dev/null || true
test -f THIRD_PARTY_NOTICES && cp THIRD_PARTY_NOTICES "$out/"
chmod 755 "$out/install.sh"
if find "$out" -iname '*openh264*' | grep -q .; then
  echo "package-linux: the package must not contain OpenH264" >&2
  exit 1
fi
tar -C dist -czf "dist/$name.tar.gz" "$name"
echo "dist/$name.tar.gz"
