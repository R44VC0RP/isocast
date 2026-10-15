#!/bin/sh
# The offscreen snapshot loop against a throwaway engine with synthetic
# sources (scripts/screenshots/offscreen.toml), never your own:
#
#   scripts/snapshot.sh [isocast-ui arguments…]
#   scripts/snapshot.sh --snapshot out.png --size 1440x900 --scale 2 --state record
#   ISOCAST_BIN=$CARGO_TARGET_DIR/ui-dev scripts/snapshot.sh --snapshot-watch DIR --crop controls
#
# ISOCAST_BIN holds isocast, isocastd and isocast-ui (default
# target/release). isocast-ui refuses offscreen runs on your own engine,
# because states such as `record` and `go-live` act for real; this script
# gives it an engine of its own in a scratch folder, recordings included,
# and removes both when it ends.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
bin="${ISOCAST_BIN:-$here/../target/release}"
work="$(mktemp -d "${TMPDIR:-/tmp}/isocast-snapshot.XXXXXX")"
export ISOCAST_SOCKET="$work/engine.sock"
# A dev app that doesn't exist, so the CLI starts the isocastd beside it.
export ISOCAST_DEV_APP="$work/none.app"
engine_pid=""
cleanup() {
    "$bin/isocast" engine stop >/dev/null 2>&1 || true
    # The engine's command line doesn't name the scratch folder, so stop it
    # by the pid it reported, in case `engine stop` didn't.
    [ -n "$engine_pid" ] && kill "$engine_pid" 2>/dev/null || true
    rm -rf "${work:?}"
}
trap cleanup EXIT INT TERM
cp "$here/screenshots/offscreen.toml" "$here/screenshots/landscape.jpg" "$here/screenshots/brb.jpg" "$work/"
# Recordings (from the `record` state) stay in the scratch folder.
sed -i '' "s|^folder = \"recordings\"|folder = \"$work/recordings\"|" "$work/offscreen.toml"
"$bin/isocast" apply --revision 0 "$work/offscreen.toml" >/dev/null
"$bin/isocast" engine start >/dev/null
engine_pid="$("$bin/isocast" status --json | /usr/bin/python3 -c 'import json,sys; print((json.load(sys.stdin).get("daemon") or {}).get("pid") or "")')"
"$bin/isocast" scene main --cut >/dev/null
"$bin/isocast" preview intro >/dev/null
"$bin/isocast-ui" "$@"
