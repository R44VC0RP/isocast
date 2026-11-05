#!/usr/bin/env bash
# Private X servers, a dead Wayland socket and no ScreenCast backend. Never
# interact with the caller's desktop, engine, setup or recordings.
set -euo pipefail
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
export XDG_RUNTIME_DIR="$work/runtime" XDG_CONFIG_HOME="$work/config" XDG_DATA_HOME="$work/data"
export ISOCAST_SOCKET="$work/engine.sock" ISOCAST_SETUP="$work/setup.toml" ISOCAST_CALIBRATION=off
export WAYLAND_DISPLAY=wayland-dead DBUS_SESSION_BUS_ADDRESS="unix:path=$work/no-bus"
unset WAYLAND_SOCKET
mkdir -m 700 "$XDG_RUNTIME_DIR"
oracle=(cargo test --release --locked -p isocast-platform-linux x11_pixels_and_shm_lifetime -- --ignored --nocapture)
for shm in yes no; do
    args='-screen 0 128x96x24 -nolisten tcp'
    if [[ $shm == no ]]; then args+=' -extension MIT-SHM'; fi
    ISOCAST_TEST_X11_SHM=$shm xvfb-run -a -s "$args" "${oracle[@]}"
done

bin=$(realpath "${CARGO_TARGET_DIR:-target}/release")
export work bin
xvfb-run -a -s '-screen 0 800x600x24 -nolisten tcp' bash -euo pipefail <<'X11'
cat > "$work/fixture.toml" <<EOF
version = 1
name = "X11 screen"
[canvas]
width = 320
height = 240
fps = 10
[recording]
folder = "$work/takes"
program = { encoder = { codec = "h264" } }
iso = { enabled = true, sources = ["screen"] }
[[sources]]
id = "screen"
kind = "display"
display = "main"
cursor = false
[[scenes]]
id = "scene"
name = "Screen"
[[scenes.items]]
source = "screen"
rect = { x = 0, y = 0, width = 320, height = 240 }
EOF
wait_for_picker() {
    local pid=$1 title=$2 deadline=$((SECONDS + 60)) windows
    while kill -0 "$pid" 2>/dev/null && (( SECONDS < deadline )); do
        if windows=$(xdotool search --onlyvisible --name "$title"); then
            printf '%s\n' "${windows%%$'\n'*}"
            return 0
        fi
        sleep .1
    done
    echo "No visible picker: $title" >&2
    # A panic in the UI child is logged alongside the private engine socket.
    find "$work" -name '*isocast-ui-*.crash.json' -exec jq -r .panic '{}' \; >&2
    return 1
}
TMPDIR="$work" "$bin/isocast" record "$work/fixture.toml" --seconds 2 --json > "$work/report.json" & rec=$!
trap 'kill "$rec" 2>/dev/null || true' EXIT
w=$(wait_for_picker "$rec" 'Isocast.*Choose a screen to record')
if [[ -n ${ISOCAST_X11_PICKER_SHOT:-} ]]; then
    ffmpeg -nostdin -v error -f x11grab -window_id "$w" -i "$DISPLAY" -vframes 1 -y "$ISOCAST_X11_PICKER_SHOT"
fi
xdotool mousemove --window "$w" 365 118 click 1
wait "$rec"
trap - EXIT
mapfile -d '' files < <(find "$work/takes" \( -name '*.mp4' -o -name '*.mov' \) -print0)
test "${#files[@]}" -eq 2
for f in "${files[@]}"; do
    frames=$(ffprobe -v error -count_frames -select_streams v:0 -show_entries stream=nb_read_frames -of csv=p=0 "$f")
    echo "$(basename "$f"): $frames frames"
    test "$frames" = 20
done
# A live take, not just a pixmap test: this window is green until it is
# mapped a second time. The recording must include the hidden interval and
# then the red pixels, in the same program and ISO files.
cc -x c - -lX11 -o "$work/window" <<'C'
#include <stdio.h>
#include <X11/Xlib.h>
int main(void) {
    Display *d = XOpenDisplay(NULL);
    if (!d) return 1;
    Window w = XCreateSimpleWindow(d, DefaultRootWindow(d), 0, 0, 320, 240, 0, 0, 0x00ff00);
    XStoreName(d, w, "Recording fixture");
    XMapWindow(d, w);
    XSync(d, False);
    puts("mapped"); fflush(stdout);
    if (getchar() != 'u') return 1;
    XUnmapWindow(d, w);
    XSetWindowBackground(d, w, 0xff0000);
    XSync(d, False);
    if (getchar() != 'm') return 1;
    XMapWindow(d, w);
    XClearWindow(d, w);
    XSync(d, False);
    getchar();
    XCloseDisplay(d);
}
C
coproc PAINT { "$work/window"; }
read -ru "${PAINT[0]}" mapped
test "$mapped" = mapped
sed 's/id = "screen"/id = "window"/;s/kind = "display"/kind = "window"/;s/display = "main"/app = "fixture"/;s/"screen"/"window"/g' "$work/fixture.toml" > "$work/window.toml"
cli="$bin/isocast"
trap '"$cli" stop --wait >/dev/null 2>&1 || true; "$cli" engine shutdown >/dev/null 2>&1 || true' EXIT
"$cli" apply --revision 0 "$work/window.toml" >/dev/null
printf '%s\n' '{"window:window":{"token":"fixture-prior-grant","width":320,"height":240}}' > "$work/setup.linux-capture.json"
"$cli" engine start >/dev/null
"$cli" screen pick window >/dev/null & pick=$!
w=$(wait_for_picker "$pick" 'Isocast.*Choose a window to record')
xdotool mousemove --window "$w" 365 118 click 1
wait "$pick"
"$cli" start --folder "$work/window-take" --json > "$work/started.json"
wait_state() {
    for _ in {1..50}; do
        "$cli" status --json > "$work/status.json"
        jq -e --arg state "$1" '.engine.sources[] | select(.source == "window" and .state == $state)' "$work/status.json" >/dev/null && return 0
        sleep .1
    done
    cat "$work/status.json" >&2; return 1
}
sleep .5
printf u >&"${PAINT[1]}"
wait_state paused
sleep 1.2
printf m >&"${PAINT[1]}"
wait_state running
sleep .6
"$cli" stop --wait --json > "$work/stopped.json"
printf q >&"${PAINT[1]}"
python3 - "$work/stopped.json" <<'PY'
import json, subprocess, sys
report = json.load(open(sys.argv[1]))
assert not report['errors'] and len(report['files']) == 2, report
for file in report['files']:
    rgb = subprocess.check_output(['ffmpeg', '-nostdin', '-v', 'error', '-i', file['path'], '-vf', 'crop=2:2,scale=1:1', '-fps_mode', 'passthrough', '-f', 'rawvideo', '-pix_fmt', 'rgb24', '-'])
    frames = list(zip(*[iter(rgb)] * 3))
    greens = sum(g > 180 and r < 50 for r,g,b in frames)
    reds = sum(r > 180 and g < 50 for r,g,b in frames)
    assert greens >= 14 and reds >= 3 and greens + reds == len(frames), (file, frames)
    assert all(g > r for r,g,b in frames[:greens]) and all(r > g for r,g,b in frames[greens:]), frames
    print(f"{file['source']}: {greens} green frames including hold, {reds} red after remap")
PY
"$cli" engine shutdown >/dev/null
jq -e '.["window:window"].token == "fixture-prior-grant"' "$work/setup.linux-capture.json" >/dev/null
"$cli" engine start >/dev/null
wait_state waiting_for_permission
jq -e '.engine.sources[] | select(.source == "window" and .needs_pick and (.error | contains("isocast screen pick window")))' "$work/status.json" >/dev/null
echo 'Reopen needs a new X11 pick; prior portal grant retained'
"$cli" engine shutdown >/dev/null
"$bin/isocast-ui" >"$work/ui.log" 2>&1 & ui=$!
trap 'kill "$ui" 2>/dev/null || true; "$bin/isocast" engine shutdown >/dev/null 2>&1 || true' EXIT
timeout 60 xdotool search --sync --onlyvisible --pid "$ui" --name 'Isocast' >/dev/null
echo 'Isocast main window mapped with dead WAYLAND_DISPLAY'
X11
