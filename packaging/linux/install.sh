#!/bin/sh
# Install Isocast for this user from the unpacked tarball: the app, engine
# and CLI in ~/.local/lib/isocast (linked from ~/.local/bin), its menu
# entry and icon, and Cisco's OpenH264 (the H.264 codec), downloaded from
# Cisco. Nothing system-wide, no root.
#
#   ./install.sh                     install (and download OpenH264)
#   ./install.sh --no-openh264       install without downloading OpenH264
#   ./install.sh --openh264          (re)install only Cisco's OpenH264
#   ./install.sh --remove-openh264   remove only Cisco's OpenH264 (H.264
#                                    off until --openh264)
#   ./install.sh --uninstall         remove Isocast and OpenH264
#                                    (recordings and settings stay)
#
# The installed copy (~/.local/lib/isocast/install.sh) takes the same
# options. Installed as `isocast-openh264-install` (the Debian package's
# /usr/bin), it manages only this user's OpenH264:
#
#   isocast-openh264-install            download Cisco's OpenH264 for this user
#   isocast-openh264-install --remove   remove it (H.264 off until run again)
set -eu
here=$(cd "$(dirname "$0")" && pwd -P)
prefix=${XDG_DATA_HOME:-$HOME/.local/share}
lib=$HOME/.local/lib/isocast
bin=$HOME/.local/bin
apps=$prefix/applications
icons=$prefix/icons/hicolor/512x512/apps
license=CISCO_H264_LICENSE.txt
attribution="OpenH264 Video Codec provided by Cisco Systems, Inc."
# The commands that turn OpenH264 on and off, as this copy is run.
if [ "$(basename "$0")" = isocast-openh264-install ]; then
  packaged=1
  on_cmd=isocast-openh264-install
  off_cmd="isocast-openh264-install --remove"
else
  packaged=
  on_cmd="$lib/install.sh --openh264"
  off_cmd="$lib/install.sh --remove-openh264"
fi

# Cisco's OpenH264 2.6.0 (the version Isocast's codec bindings expect), as
# Cisco serves it, with the SHA-256 of the download and of the library.
# crates/isocast-openh264 pins the same files and hashes.
openh264_version=2.6.0
case $(uname -m) in
x86_64)
  openh264_file=libopenh264-2.6.0-linux64.8.so
  openh264_bz2_sha256=27ab53323c110b76214c1c72222f459d17febbcd1e252136cadc292b0308d75b
  openh264_sha256=2f0cde7c6a6abcf5cae76942894ea42897fa677bce4ed6c91a24dd1b041d5f04
  ;;
aarch64 | arm64)
  openh264_file=libopenh264-2.6.0-linux-arm64.8.so
  openh264_bz2_sha256=a78aea7970150f46bcd3bb7994c9e6dd90bd7a9ea785920f5a73f6964e3fcda7
  openh264_sha256=12e7b33623667cdab0e575170c147b1b36eadb77d0d2aa7ceb5afd3e58902140
  ;;
*) openh264_file= ;;
esac
openh264_url=https://ciscobinary.openh264.org/$openh264_file.bz2

tmp=
staged=
cleanup() {
  [ -n "$tmp" ] && rm -rf "$tmp"
  [ -n "$staged" ] && rm -f "$staged"
  return 0
}
trap cleanup EXIT
trap 'exit 130' INT TERM

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

# Cisco's licence for its binary, shipped beside this installer (or in
# the Debian package's documentation).
license_text() {
  if [ -f "$here/$license" ]; then
    echo "$here/$license"
  elif [ -f "/usr/share/doc/isocast/$license" ]; then
    echo "/usr/share/doc/isocast/$license"
  else
    echo "https://www.openh264.org/BINARY_LICENSE.txt"
  fi
}

# Download Cisco's OpenH264 into $lib, check it, and say what happened.
# Returns 2 where Cisco has no build, 1 when it couldn't be installed.
install_openh264() {
  if [ -z "$openh264_file" ]; then
    echo "Cisco doesn't build OpenH264 for $(uname -m), so H.264 recording, streaming and previews are off on this computer." >&2
    return 2
  fi
  dest=$lib/$openh264_file
  if [ -f "$dest" ] && [ "$(sha256 "$dest")" = "$openh264_sha256" ]; then
    echo "Cisco's OpenH264 $openh264_version is already installed ($dest)."
    echo "$attribution Cisco's licence: $(license_text)"
    echo "To turn it off: $off_cmd"
    return 0
  fi
  echo "Downloading Cisco's OpenH264 $openh264_version, the H.264 codec Isocast records and streams with,"
  echo "from $openh264_url"
  echo "$attribution It is Cisco's binary, under Cisco's licence: $(license_text)"
  for tool in curl bzip2; do
    if ! command -v "$tool" >/dev/null 2>&1; then
      echo "install.sh: $tool is needed to download OpenH264; install it, then run: $on_cmd" >&2
      return 1
    fi
  done
  tmp=$(mktemp -d)
  if ! curl -fsSL --proto '=https' --tlsv1.2 --connect-timeout 10 --max-time 60 --retry 2 -o "$tmp/download.bz2" "$openh264_url"; then
    echo "install.sh: couldn't download $openh264_url; check the connection, then run: $on_cmd" >&2
    return 1
  fi
  got=$(sha256 "$tmp/download.bz2")
  if [ "$got" != "$openh264_bz2_sha256" ]; then
    echo "install.sh: the download's SHA-256 is $got, not Cisco's $openh264_bz2_sha256; nothing installed" >&2
    return 1
  fi
  # Staged beside the destination, checked, then moved over it in one
  # step: Isocast never sees a partly written library.
  mkdir -p "$lib"
  # A run killed between staging and moving leaves its staged file.
  rm -f "$lib"/.openh264.*
  staged=$(mktemp "$lib/.openh264.XXXXXX")
  if ! bzip2 -dc "$tmp/download.bz2" >"$staged"; then
    echo "install.sh: couldn't decompress the download; nothing installed" >&2
    return 1
  fi
  got=$(sha256 "$staged")
  if [ "$got" != "$openh264_sha256" ]; then
    echo "install.sh: the library's SHA-256 is $got, not Cisco's $openh264_sha256; nothing installed" >&2
    return 1
  fi
  chmod 644 "$staged"
  mv -f "$staged" "$dest"
  staged=
  echo "Installed Cisco's OpenH264 $openh264_version: $dest"
  echo "To turn it off: $off_cmd"
}

remove_openh264() {
  rm -f "$lib"/libopenh264-*.so
  echo "Cisco's OpenH264 removed: H.264 recording, streaming and previews are off until $on_cmd."
  echo "$attribution"
}

if [ -n "$packaged" ]; then
  # Per user: under sudo it would install into root's home, where the
  # user's Isocast never looks. Root itself (a container, a kiosk) may opt
  # in with ISOCAST_OPENH264_AS_ROOT=1.
  if [ -n "${SUDO_USER:-}" ] || { [ "$(id -u)" = 0 ] && [ -z "${ISOCAST_OPENH264_AS_ROOT:-}" ]; }; then
    echo "isocast-openh264-install installs Cisco's OpenH264 for one user; run it as yourself, without sudo" >&2
    exit 1
  fi
  case "${1:-}" in
  "") install_openh264 ;;
  --remove) remove_openh264 ;;
  *)
    echo "isocast-openh264-install: unknown option $1; run it with no option to install Cisco's OpenH264 for this user, or --remove" >&2
    exit 2
    ;;
  esac
  exit
fi

case "${1:-}" in
--uninstall)
  rm -f "$bin/isocast" "$bin/isocast-ui" "$apps/dev.isocast.Isocast.desktop" "$icons/dev.isocast.Isocast.png"
  rm -rf "$lib"
  echo "Isocast and Cisco's OpenH264 removed. Recordings (~/Videos/Isocast unless you chose another folder) and settings ($prefix/isocast) are kept."
  exit 0
  ;;
--openh264)
  install_openh264
  exit
  ;;
--remove-openh264)
  remove_openh264
  exit 0
  ;;
"" | --no-openh264) ;;
*)
  echo "install.sh: unknown option $1; valid options are --openh264, --remove-openh264, --no-openh264, --uninstall" >&2
  exit 2
  ;;
esac

mkdir -p "$lib" "$bin" "$apps" "$icons"
# The installed copy, run again, has nothing to copy (and copying a file
# onto itself fails): it refreshes the links and OpenH264.
if [ "$here" != "$(cd "$lib" && pwd -P)" ]; then
  # The engine is found beside the app and the CLI, and the browser helper
  # beside the engine, so all four stay together; this installer and
  # Cisco's licence too, for later.
  for name in isocast isocast-ui isocastd isocast-browser install.sh "$license"; do
    install -m 755 "$here/$name" "$lib/$name"
  done
  chmod 644 "$lib/$license"
  install -m 644 "$here/dev.isocast.Isocast.png" "$icons/dev.isocast.Isocast.png"
  # Exec's path quoted (a home folder with spaces), with the characters the
  # desktop entry spec reserves inside quotes escaped (\\ in the file's
  # string value is one backslash).
  exec_path=$(printf '%s' "$lib/isocast-ui" | sed 's/[\\"`$]/\\\\&/g')
  {
    grep -v '^Exec=' "$here/dev.isocast.Isocast.desktop"
    printf 'Exec="%s"\n' "$exec_path"
  } >"$apps/dev.isocast.Isocast.desktop"
  command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$apps" >/dev/null 2>&1 || true
  command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q "$prefix/icons/hicolor" >/dev/null 2>&1 || true
fi
ln -sf "$lib/isocast" "$bin/isocast"
ln -sf "$lib/isocast-ui" "$bin/isocast-ui"
echo "Isocast installed: Isocast in your app menu, or isocast-ui and isocast in $bin."
case ":$PATH:" in *":$bin:"*) ;; *) echo "($bin isn't on your PATH yet; add it to use the commands.)" ;; esac
if [ "${1:-}" = --no-openh264 ]; then
  echo "OpenH264 not downloaded: H.264 recording, streaming and previews stay off until you run: $lib/install.sh --openh264"
  echo "$attribution"
else
  status=0
  install_openh264 || status=$?
  if [ "$status" = 1 ]; then
    echo "Isocast is installed, but H.264 recording, streaming and previews stay off until OpenH264 is: $lib/install.sh --openh264"
  fi
fi
