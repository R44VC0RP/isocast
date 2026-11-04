#!/bin/bash
# Run in a fresh CI container with the native package from the build job.
set -euo pipefail
case $1 in
archlinux)
  pacman -Syu --noconfirm vulkan-swrast
  pacman -U --noconfirm dist/isocast-git-*.pkg.tar.zst
  ;;
fedora)
  dnf --disablerepo='*cisco*' install -y ./dist/*/isocast-[0-9]*.rpm noopenh264 mesa-vulkan-drivers
  ;;
*) exit 2 ;;
esac

# Check the installed payload before adding the smoke harness's tools.
for exe in /usr/bin/isocast{,d,-ui,-browser}; do
  test -x "$exe"
  libs=$(ldd "$exe")
  if grep -F 'not found' <<< "$libs"; then exit 1; fi
done
grep -Fx 'Exec=isocast-ui' /usr/share/applications/dev.isocast.Isocast.desktop
test -s /usr/share/icons/hicolor/scalable/apps/dev.isocast.Isocast.svg
command -v gtk4-broadwayd
export XDG_RUNTIME_DIR=/tmp/isocast-runtime LP_NUM_THREADS=2
mkdir -m 700 "$XDG_RUNTIME_DIR"
if [[ $1 == archlinux ]]; then
  pacman -S --needed --noconfirm python ffmpeg
else
  dnf --disablerepo='*cisco*' install -y python3 ffmpeg-free
fi
python3 scripts/linux-distro-smoke.py "$1"
if [[ $1 == fedora ]]; then
  dnf --enablerepo=fedora-cisco-openh264 swap -y noopenh264 openh264
  python3 scripts/linux-distro-smoke.py fedora-cisco
fi
