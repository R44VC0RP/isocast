#!/bin/sh
# Install Isocast for this user from the unpacked tarball: the app, engine
# and CLI in ~/.local/lib/isocast (linked from ~/.local/bin), its menu
# entry and icon. Nothing system-wide, no root. `./install.sh --uninstall`
# removes them again (recordings and settings stay).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
prefix=${XDG_DATA_HOME:-$HOME/.local/share}
lib=$HOME/.local/lib/isocast
bin=$HOME/.local/bin
apps=$prefix/applications
icons=$prefix/icons/hicolor/512x512/apps

if [ "${1:-}" = --uninstall ]; then
  rm -f "$bin/isocast" "$bin/isocast-ui" "$apps/dev.isocast.Isocast.desktop" "$icons/dev.isocast.Isocast.png"
  rm -rf "$lib"
  echo "Isocast removed. Recordings (~/Videos/Isocast unless you chose another folder) and settings ($prefix/isocast) are kept."
  exit 0
fi

mkdir -p "$lib" "$bin" "$apps" "$icons"
# The engine is found beside the app and the CLI, so all three stay together.
for name in isocast isocast-ui isocastd; do
  install -m 755 "$here/$name" "$lib/$name"
done
ln -sf "$lib/isocast" "$bin/isocast"
ln -sf "$lib/isocast-ui" "$bin/isocast-ui"
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
echo "Isocast installed: Isocast in your app menu, or isocast-ui and isocast in $bin."
case ":$PATH:" in *":$bin:"*) ;; *) echo "($bin isn't on your PATH yet; add it to use the commands.)" ;; esac
