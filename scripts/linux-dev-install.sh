#!/bin/sh
# Install the desktop entry for a development build, so the desktop's
# portals know Isocast by its app id (dev.isocast.Isocast): GlobalShortcuts
# refuses host apps whose id has no desktop entry. Installed builds get it
# from packaging/linux/install.sh or the .deb instead.
#
#   scripts/linux-dev-install.sh [debug|release]   (default: debug)
#
# Removing the file undoes it:
#   rm ~/.local/share/applications/dev.isocast.Isocast.desktop
set -eu
repo=$(cd "$(dirname "$0")/.." && pwd -P)
profile=${1:-debug}
case "$profile" in
debug | release) ;;
*)
  echo "linux-dev-install.sh: unknown build $profile; valid builds are debug, release" >&2
  exit 2
  ;;
esac
apps=${XDG_DATA_HOME:-$HOME/.local/share}/applications
mkdir -p "$apps"
# Exec quoted and escaped as packaging/linux/install.sh does.
exec_path=$(printf '%s' "$repo/target/$profile/isocast-ui" | sed 's/[\\"`$]/\\\\&/g')
{
  grep -v '^Exec=' "$repo/packaging/linux/dev.isocast.Isocast.desktop"
  printf 'Exec="%s"\n' "$exec_path"
} >"$apps/dev.isocast.Isocast.desktop"
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$apps" >/dev/null 2>&1 || true
echo "Installed $apps/dev.isocast.Isocast.desktop"
