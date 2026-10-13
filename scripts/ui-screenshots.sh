#!/bin/sh
# Draws the app's standard views offscreen, for before/after screenshots in
# pull requests that change the interface (see CONTRIBUTING.md):
#
#   scripts/ui-screenshots.sh before/     # on main
#   scripts/ui-screenshots.sh after/      # on your branch
#
# It builds the app, starts a throwaway engine (its own socket and setup in
# a temporary folder, synthetic sources from scripts/screenshots/
# offscreen.toml, no permissions needed) and writes 1440x900 windows at 2x
# (2880x1800 PNGs): main, studio, properties, settings and mixer. Nothing
# opens on screen and your own setup isn't touched. The video area shows a
# stand-in test picture offscreen; judge the interface around it.
#
# For the README's real-window screenshots, see scripts/screenshots/take.sh.
set -eu

cd "$(dirname "$0")/.."
out="${1:-target/ui-screenshots}"
target="${CARGO_TARGET_DIR:-target}"
size="${ISOCAST_SHOTS_SIZE:-1440x900}"
scale="${ISOCAST_SHOTS_SCALE:-2}"

cargo build --release -p isocast-ui -p isocast-daemon -p isocast-cli
isocast="$target/release/isocast"
ui="$target/release/isocast-ui"

work="$(mktemp -d "${TMPDIR:-/tmp}/isocast-ui-shots.XXXXXX")"
export ISOCAST_SOCKET="$work/engine.sock"
# A dev app that doesn't exist, so the CLI starts this build's isocastd.
export ISOCAST_DEV_APP="$work/none.app"
cleanup() {
    "$isocast" engine stop >/dev/null 2>&1 || true
    pkill -f "$work" 2>/dev/null || true
    rm -rf "${work:?}"
}
trap cleanup EXIT INT TERM

cp scripts/screenshots/offscreen.toml scripts/screenshots/landscape.jpg \
    scripts/screenshots/brb.jpg "$work/"
"$isocast" apply --revision 0 "$work/offscreen.toml" >/dev/null
"$isocast" engine start >/dev/null
"$isocast" scene main --cut >/dev/null
"$isocast" preview intro >/dev/null
mkdir -p "$out"

shot() { # shot NAME [STATE...]
    name="$1"
    shift
    set -- --snapshot "$out/$name.png" --size "$size" --scale "$scale" --wait-ms 800 \
        $(for state in "$@"; do printf -- '--state %s ' "$state"; done)
    if "$ui" "$@" >"$work/$name.log" 2>&1; then
        echo "$out/$name.png"
    else
        echo "ui-screenshots: $name failed:" >&2
        tail -5 "$work/$name.log" >&2
        return 1
    fi
}

failed=0
shot main || failed=1
shot studio studio || failed=1
shot properties select:camera-inset dialog:properties:camera || failed=1
shot settings settings:output || failed=1
shot mixer mixer-menu:mic || failed=1
exit "$failed"
