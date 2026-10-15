#!/bin/sh
# Renders the app's standard views offscreen with already built binaries,
# for the visual check on pull requests (.github/workflows/visual.yml):
#
#   scripts/visual-shots.sh BIN_DIR OUT_DIR
#
# BIN_DIR holds isocast, isocastd and isocast-ui (one build, base or head);
# OUT_DIR gets VIEW-WxH.png for every view at every size in SIZES, plus
# logs/ and gpu.txt (the adapter each render used). Like
# scripts/ui-screenshots.sh it runs a throwaway engine with the synthetic
# sources in scripts/screenshots/offscreen.toml, so it needs no permissions
# and touches no one's setup. The offscreen window has its own clock and
# bundled fonts, so the same build draws the same pixels.
#
# A view that fails to draw is reported and skipped (a state the other
# build doesn't know shows as added or removed); the script fails only if
# no view at all was drawn, or the GPU is a software or unknown one.
#
# For a quicker local check, VIEWS and SIZES pick a subset (VIEWS="main
# settings" SIZES=1440x900@2), and JOBS renders that many views at once
# (default 4; the pictures are the same as one at a time). CI renders
# every view at every size, one at a time (JOBS=1 in visual.yml).
set -eu

# Window size and scale (WxH@SCALE). Adding a size is one line here.
SIZES="${SIZES:-
1440x900@2
1920x1080@1
900x600@2
}"
VIEWS="${VIEWS:-main studio properties settings mixer}"
JOBS="${JOBS:-4}"

here="$(cd "$(dirname "$0")" && pwd)"
bin="$(cd "${1:?usage: visual-shots.sh BIN_DIR OUT_DIR}" && pwd)"
out="${2:?usage: visual-shots.sh BIN_DIR OUT_DIR}"
mkdir -p "$out/logs"
out="$(cd "$out" && pwd)"
# Only this run's logs count towards "drawn" (OUT_DIR may be reused).
rm -f "${out:?}"/logs/*.log
isocast="$bin/isocast"
ui="$bin/isocast-ui"

# A fixed folder, not mktemp: paths can show in the views, and base and
# head must draw the same ones.
work="${ISOCAST_SHOTS_WORK:-/tmp/isocast-visual-shots}"
rm -rf "${work:?}"
mkdir -p "$work"
export ISOCAST_SOCKET="$work/engine.sock"
# A dev app that doesn't exist, so the CLI starts the isocastd next to it.
export ISOCAST_DEV_APP="$work/none.app"
export TZ=UTC LANG=en_US.UTF-8 LC_ALL=en_US.UTF-8
# No CPU reading or audio levels in the pictures (they differ every run).
export ISOCAST_SNAPSHOT_STILL=1
cleanup() {
    # Renders still running (an interrupted run) go first.
    # shellcheck disable=SC2046
    kill $(jobs -p) 2>/dev/null || true
    "$isocast" engine stop >/dev/null 2>&1 || true
    pkill -f "$work" 2>/dev/null || true
    rm -rf "${work:?}"
}
trap cleanup EXIT INT TERM

cp "$here/screenshots/offscreen.toml" "$here/screenshots/landscape.jpg" \
    "$here/screenshots/brb.jpg" "$work/"
"$isocast" apply --revision 0 "$work/offscreen.toml" >/dev/null
"$isocast" engine start >/dev/null
"$isocast" scene main --cut >/dev/null
"$isocast" preview intro >/dev/null

running=0
shot() { # shot NAME WxH@SCALE [STATE...]
    case " $VIEWS " in *" $1 "*) ;; *) return 0 ;; esac
    wh="${2%@*}"
    scale="${2#*@}"
    name="$1-$wh"
    shift 2
    set -- --snapshot "$out/$name.png" --size "$wh" --scale "$scale" --wait-ms 800 \
        $(for state in "$@"; do printf -- '--state %s ' "$state"; done)
    (
        if "$ui" "$@" >"$out/logs/$name.log" 2>&1; then
            echo "$out/$name.png"
        else
            echo "visual-shots: $name failed:" >&2
            tail -5 "$out/logs/$name.log" >&2
            rm -f "$out/$name.png"
        fi
    ) &
    running=$((running + 1))
    if [ "$running" -ge "$JOBS" ]; then
        wait
        running=0
    fi
}

for size in $SIZES; do
    shot main "$size"
    shot studio "$size" studio
    # The tone's dialog: a video source's has a live preview (the test
    # pattern's frame counter, an image that loads at its own pace).
    shot properties "$size" dialog:properties:mic
    shot settings "$size" settings:output
    shot mixer "$size" mixer-menu:mic
done
wait
drawn=0
for log in "$out"/logs/*.log; do
    [ -f "$out/$(basename "$log" .log).png" ] && drawn=$((drawn + 1))
done

# isocast-ui prints the adapter it drew with ("gpu: NAME (BACKEND,
# TYPE, ...)"). Metal has no software device, but a VM can expose a broken
# or CPU one; refuse those rather than compare garbage.
grep -h '^gpu: ' "$out"/logs/*.log | sort -u >"$out/gpu.txt" || true
if [ -s "$out/gpu.txt" ]; then
    cat "$out/gpu.txt"
    if grep -Eiq 'cpu|llvmpipe|swiftshader|software|basic render' "$out/gpu.txt"; then
        echo "visual-shots: rendering fell back to a software GPU; screenshots would not match a real Mac" >&2
        exit 2
    fi
else
    echo "visual-shots: this build doesn't report its GPU adapter" >&2
fi
if [ "$drawn" -eq 0 ]; then
    echo "visual-shots: no view could be drawn" >&2
    exit 1
fi
