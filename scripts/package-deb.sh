#!/bin/sh
# Build the Debian package of a Linux build: dist/isocast_<version>-1~<distro>_<arch>.deb
# with isocast, isocastd and isocast-ui in /usr/bin (from BIN_DIR), the
# per-user isocast-openh264-install command (packaging/linux/install.sh),
# the desktop entry and icon in /usr/share, and the docs, notices and
# Cisco's licence in /usr/share/doc/isocast. No OpenH264, and no
# maintainer scripts: nothing runs as root, nothing is downloaded, and
# removing or purging leaves every user's data alone.
#
#   scripts/package-deb.sh BIN_DIR DISTRO     DISTRO: ubuntu24.04 or debian12
#
# Runs on the distribution it packages for (dpkg-shlibdeps reads its
# libraries to compute Depends).
set -eu
bin=${1:?usage: scripts/package-deb.sh BIN_DIR DISTRO}
distro=${2:?usage: scripts/package-deb.sh BIN_DIR DISTRO}
case $distro in
ubuntu24.04 | debian12) ;;
*) echo "package-deb: DISTRO must be ubuntu24.04 or debian12, not $distro" >&2; exit 2 ;;
esac
for tool in dpkg-deb dpkg-shlibdeps dpkg-architecture gzip md5sum strip nm strings readelf; do
  command -v "$tool" >/dev/null || { echo "package-deb: $tool is needed (dpkg-dev, binutils)" >&2; exit 1; }
done
version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
test -n "$version" || { echo "package-deb: no [workspace.package] version in Cargo.toml" >&2; exit 1; }
arch=$(dpkg-architecture -qDEB_HOST_ARCH)
debversion=$version-1~$distro
name=isocast_${debversion}_$arch
work=dist/deb/$name
root=$work/root
maintainer=${DEB_MAINTAINER:-Ryan Vogel <ryan@anoma.ly>}
rm -rf "$work" "dist/$name.deb"
mkdir -p "$root/DEBIAN" "$root/usr/bin" "$root/usr/share/doc/isocast" \
  "$root/usr/share/applications" "$root/usr/share/icons/hicolor/512x512/apps"

for exe in isocast isocastd isocast-ui; do
  test -x "$bin/$exe" || { echo "package-deb: no $bin/$exe" >&2; exit 1; }
  # Before stripping: OpenH264's entry points defined in a binary mean it
  # was compiled in. (The tarball's binaries arrive stripped, checked the
  # same way by package-linux.sh; nm finds nothing in them here.)
  symbols=$(nm "$bin/$exe" 2>/dev/null || true)
  if printf '%s\n' "$symbols" | grep -E -q ' [TtWw] _?(WelsCreateSVCEncoder|WelsCreateDecoder)$'; then
    echo "package-deb: $bin/$exe has OpenH264 compiled in; build without openh264's source feature" >&2
    exit 1
  fi
  install -m 755 "$bin/$exe" "$root/usr/bin/$exe"
  strip --strip-unneeded "$root/usr/bin/$exe"
  # No OpenH264 in the package (Cisco's licence covers only its own
  # binary, which each user downloads): OpenH264's own log strings are in
  # a binary only if it was compiled in.
  text=$(strings "$root/usr/bin/$exe")
  if printf '%s\n' "$text" | grep -E -q 'openh264 codec version|CWelsDecoder::'; then
    echo "package-deb: $bin/$exe contains OpenH264 code" >&2
    exit 1
  fi
done
install -m 755 packaging/linux/install.sh "$root/usr/bin/isocast-openh264-install"
{
  grep -v '^Exec=' packaging/linux/dev.isocast.Isocast.desktop
  echo 'Exec=isocast-ui'
} >"$root/usr/share/applications/dev.isocast.Isocast.desktop"
chmod 644 "$root/usr/share/applications/dev.isocast.Isocast.desktop"
install -m 644 packaging/linux/dev.isocast.Isocast.png "$root/usr/share/icons/hicolor/512x512/apps/"
doc=$root/usr/share/doc/isocast
install -m 644 packaging/deb/copyright packaging/deb/README.Debian packaging/linux/CISCO_H264_LICENSE.txt "$doc/"
gzip -9n -c THIRD_PARTY_NOTICES >"$doc/THIRD_PARTY_NOTICES.gz"
date=$(date -R -u -d "@${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct 2>/dev/null || date +%s)}")
cat <<CHANGELOG | gzip -9n >"$doc/changelog.Debian.gz"
isocast ($debversion) unstable; urgency=medium

  * Isocast $version for $distro, built from the same sources as the
    isocast-linux tarball.

 -- $maintainer  $date
CHANGELOG
chmod 644 "$doc"/*
if find "$root" -iname '*openh264*' ! -name isocast-openh264-install | grep -q .; then
  echo "package-deb: the package must not contain OpenH264" >&2
  exit 1
fi

# Depends: the libraries the binaries link (dpkg-shlibdeps), plus those
# they load at run time, which it can't see: Vulkan (with Mesa's drivers,
# lavapipe without a GPU), Wayland, X11 and xkbcommon for the window, and
# curl/bzip2/certificates for isocast-openh264-install.
mkdir -p "$work/debian"
printf 'Source: isocast\n\nPackage: isocast\nArchitecture: any\n' >"$work/debian/control"
shlibs=$(cd "$work" && dpkg-shlibdeps -O root/usr/bin/isocast root/usr/bin/isocastd root/usr/bin/isocast-ui |
  sed -n 's/^shlibs:Depends=//p')
test -n "$shlibs" || { echo "package-deb: dpkg-shlibdeps found no dependencies" >&2; exit 1; }
# The binaries load with any libpipewire 0.3.50 or newer, but capture needs
# 1.0.5 (per-buffer graph timestamps): ask for that, so a system without it
# (Debian 12 without bookworm-backports) says so at install, not at capture.
case $shlibs in
*libpipewire-0.3-0*) ;;
*) echo "package-deb: expected libpipewire in Depends: $shlibs" >&2; exit 1 ;;
esac
shlibs=$(printf '%s' "$shlibs" | sed -E 's/(libpipewire-0\.3-0[a-z0-9]*) \(>= [^)]*\)/\1 (>= 1.0.5)/')
case $shlibs in
*"libpipewire-0.3-0 (>= 1.0.5)"* | *"libpipewire-0.3-0t64 (>= 1.0.5)"*) ;;
*) echo "package-deb: libpipewire's minimum wasn't raised to 1.0.5: $shlibs" >&2; exit 1 ;;
esac
# Media files and images decode with the system's GStreamer once the engine
# links it (agent/linux-media): then Depends must name it, and the plugins
# it loads at run time are recommended.
needed=$(for exe in isocast isocastd isocast-ui; do readelf -d "$root/usr/bin/$exe"; done)
if printf '%s\n' "$needed" | grep -q 'libgstreamer-1\.0\.so'; then
  case $shlibs in
  *libgstreamer1.0-0*) ;;
  *) echo "package-deb: the binaries link GStreamer but Depends lacks it: $shlibs" >&2; exit 1 ;;
  esac
fi
runtime="libvulkan1, mesa-vulkan-drivers, libwayland-client0, libwayland-cursor0, libwayland-egl1, libxkbcommon0, libxkbcommon-x11-0, libx11-6, libx11-xcb1, libxcursor1, libxrandr2, libxi6, curl, bzip2, ca-certificates"
size=$(du -sk --exclude=DEBIAN "$root" | cut -f1)
cat >"$root/DEBIAN/control" <<CONTROL
Package: isocast
Version: $debversion
Architecture: $arch
Maintainer: $maintainer
Installed-Size: $size
Depends: $shlibs, $runtime
Recommends: pipewire, wireplumber, xdg-desktop-portal, xdg-utils, gstreamer1.0-plugins-base, gstreamer1.0-plugins-good, gstreamer1.0-libav
Section: video
Priority: optional
Homepage: https://isocast.com
Description: screen and camera recorder that records every source as its own file
 Isocast records your scenes, and every source as its own file (ISO), with
 a desktop app laid out like OBS Studio, a CLI (isocast) and an MCP server
 for agents, all controlling one engine (isocastd).
 .
 H.264 needs Cisco's OpenH264, which each user downloads with
 isocast-openh264-install (not included in this package).
CONTROL
(cd "$root" && find usr -type f | LC_ALL=C sort | xargs md5sum) >"$root/DEBIAN/md5sums"
chmod 644 "$root/DEBIAN/control" "$root/DEBIAN/md5sums"
find "$root" -type d -exec chmod 755 {} +
dpkg-deb --root-owner-group -Zxz --build "$root" "dist/$name.deb" >/dev/null
echo "dist/$name.deb"
