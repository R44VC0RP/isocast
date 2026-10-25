#!/bin/sh
# Check an installed isocast .deb (CI, on a clean machine): the engine
# starts without OpenH264 and doctor says how to get it, the per-user
# command installs it, a 5 s recording of test sources completes through
# the CLI, and purging the package leaves the user's data alone.
#
#   packaging/deb/test-installed.sh SETUP_TOML DEB
#
# The Ubuntu 24.04 package has the browser helper: its AppArmor profile is
# a conffile, AppArmor (where it runs) has it loaded after install and
# after reinstalling the .deb over itself (the upgrade path of the
# maintainer scripts), and it is unloaded and gone after purge. The Debian
# 12 package has none (GTK 4.8; it needs 4.10), and doctor says so.
set -eu
setup=${1:?usage: packaging/deb/test-installed.sh SETUP_TOML DEB}
deb=${2:?usage: packaging/deb/test-installed.sh SETUP_TOML DEB}
sudo=$(command -v sudo || true)
step() { printf '\n== %s\n' "$*"; }
# AppArmor can load profiles here (not in a container without it).
apparmor() {
  [ "$(cat /sys/module/apparmor/parameters/enabled 2>/dev/null)" = Y ] &&
    command -v apparmor_parser >/dev/null 2>&1 &&
    $sudo test -r /sys/kernel/security/apparmor/profiles
}
loaded() { $sudo cat /sys/kernel/security/apparmor/profiles | grep -q '^isocast-browser '; }
helper=
[ -x /usr/bin/isocast-browser ] && helper=1
case $(dpkg-query -W -f='${Version}' isocast) in
*~ubuntu*) [ -n "$helper" ] || { echo "the Ubuntu package has no /usr/bin/isocast-browser" >&2; exit 1; } ;;
*~debian12) [ -z "$helper" ] || { echo "the Debian 12 package has isocast-browser (it needs GTK 4.10)" >&2; exit 1; } ;;
esac

step "installed files"
dpkg -L isocast | grep -E '^/usr/(bin|share/(applications|icons|doc))/' | grep -v '/$' | sort
for exe in isocast isocastd isocast-ui isocast-openh264-install; do
  command -v "$exe" >/dev/null || { echo "missing $exe on PATH" >&2; exit 1; }
done
# Only the per-user command's name may mention it (Cisco's licence is
# CISCO_H264_LICENSE.txt).
if dpkg -L isocast | grep -i openh264 | grep -v -x /usr/bin/isocast-openh264-install; then
  echo "the package contains OpenH264" >&2
  exit 1
fi

if [ -n "$helper" ]; then
  step "browser helper's AppArmor profile"
  test -f /etc/apparmor.d/isocast-browser
  dpkg-query -W -f='${Conffiles}' isocast | grep -q /etc/apparmor.d/isocast-browser ||
    { echo "the profile isn't a conffile" >&2; exit 1; }
  if apparmor; then
    loaded || { echo "AppArmor hasn't loaded isocast-browser" >&2; exit 1; }
    echo "AppArmor: isocast-browser loaded"
  else
    echo "AppArmor isn't running here: the profile is installed, not loaded"
  fi
fi

step "upgrade path: the package installed over itself"
$sudo dpkg -i "$deb" >/dev/null
dpkg-query -W -f='${Status}\n' isocast
if [ -n "$helper" ] && apparmor; then
  loaded || { echo "AppArmor lost isocast-browser on reinstall" >&2; exit 1; }
  echo "AppArmor: isocast-browser still loaded"
fi

step "doctor without OpenH264"
rm -f "$HOME"/.local/lib/isocast/libopenh264-*.so
doctor=$(isocast doctor 2>&1 || true)
printf '%s\n' "$doctor"
printf '%s\n' "$doctor" | grep -q 'codec: OpenH264' || { echo "doctor didn't report the missing OpenH264" >&2; exit 1; }
printf '%s\n' "$doctor" | grep -q 'isocast-openh264-install' || { echo "doctor didn't name isocast-openh264-install" >&2; exit 1; }
if [ -z "$helper" ]; then
  printf '%s\n' "$doctor" | grep -q 'GTK 4.10' || { echo "doctor didn't say browser sources need GTK 4.10" >&2; exit 1; }
fi

step "isocast-openh264-install"
isocast-openh264-install
doctor=$(isocast doctor 2>&1 || true)
if printf '%s\n' "$doctor" | grep 'codec: OpenH264'; then
  echo "doctor still reports OpenH264 after installing it" >&2
  exit 1
fi

step "apply the test setup"
isocast apply --revision 0 "$setup"

step "app snapshot (offscreen, no display server)"
# The app's own runtime libraries (Vulkan, Skia, fonts) from the
# package's Depends alone.
# Offscreen runs refuse the user's own engine (their states could start
# recordings there): an isolated engine of its own, as the linux-app job.
rm -rf /tmp/isocast-snapshot /tmp/isocast-ui.png
mkdir -p /tmp/isocast-snapshot
(
  export ISOCAST_SOCKET=/tmp/isocast-snapshot/engine.sock ISOCAST_SETUP=/tmp/isocast-snapshot/setup.toml ISOCAST_CALIBRATION=off
  isocast apply --revision 0 "$setup" >/dev/null
  isocast-ui --snapshot /tmp/isocast-ui.png --size 640x400
  isocast engine shutdown >/dev/null 2>&1 || true
)
python3 - <<'PY'
import struct
data = open("/tmp/isocast-ui.png", "rb").read()
assert data[:8] == b"\x89PNG\r\n\x1a\n", "not a PNG"
width, height = struct.unpack(">II", data[16:24])
print(f"snapshot: {width}x{height}, {len(data)} bytes")
assert (width, height) == (640, 400), (width, height)
PY

step "record 5 s of test sources"
isocast start --seconds 5 --json >/tmp/isocast-start.json
# The take's session manifest sits beside its files.
manifest=$(python3 -c '
import glob, json, os
files = json.load(open("/tmp/isocast-start.json"))["files"]
folder = os.path.dirname(files[0])
print(max(glob.glob(os.path.join(folder, "*.session.json")) or [os.path.join(folder, "missing.session.json")], key=os.path.getmtime))')
for _ in $(seq 60); do
  status=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("status",""))' "$manifest" 2>/dev/null || true)
  [ "$status" = complete ] && break
  sleep 1
done
python3 - "$manifest" <<'PY'
import json, os, sys
m = json.load(open(sys.argv[1]))
files = [(f["source"], f["frames"], f["dropped_frames"], f["audio_dropouts"]) for f in m["files"]]
print("take:", m["status"], m["duration_seconds"], "s", files)
assert m["status"] == "complete", m["status"]
assert m["files"] and all(f["frames"] > 0 for f in m["files"]), files
for f in m["files"]:
    path = os.path.join(os.path.dirname(sys.argv[1]), f["path"]) if not os.path.isabs(f["path"]) else f["path"]
    assert os.path.getsize(path) > 0, path
PY
isocast engine shutdown >/dev/null 2>&1 || true

step "purge leaves user data"
$sudo apt-get purge -y isocast >/dev/null
if dpkg-query -W -f='${Status}' isocast 2>/dev/null | grep -q installed || [ -e /usr/bin/isocast ]; then
  echo "isocast still installed" >&2
  exit 1
fi
test -f "$manifest" || { echo "purge removed the recording" >&2; exit 1; }
test -d "${XDG_DATA_HOME:-$HOME/.local/share}/isocast" || { echo "purge removed the settings" >&2; exit 1; }
ls "$HOME"/.local/lib/isocast/libopenh264-*.so >/dev/null || { echo "purge removed the user's OpenH264" >&2; exit 1; }
if [ -n "$helper" ]; then
  test ! -e /etc/apparmor.d/isocast-browser || { echo "purge left /etc/apparmor.d/isocast-browser" >&2; exit 1; }
  if apparmor && loaded; then
    echo "purge left isocast-browser loaded in AppArmor" >&2
    exit 1
  fi
  echo "purged: the AppArmor profile is unloaded and gone"
fi
echo "purged: the program is gone; recordings, settings and the user's OpenH264 remain"
