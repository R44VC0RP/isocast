#!/bin/sh
# Runs on the test Mac, in its desktop session (scripts/testmac.sh gui), from
# the checkout whose app is photographed; take.sh copies this folder to
# $ISOCAST_SCRATCH/docs-shots and calls it. Writes PNGs to $S/out.
# Needs CuaDriver (Accessibility and Screen Recording) on the test Mac.
#
#   sh remote.sh start      HiDPI display mode, isolated engine, the app
#   sh remote.sh shoot      every screenshot (after start)
#   sh remote.sh stop       quit the app and engine, restore the display
#   sh remote.sh all        start, shoot, stop
set -u

checkout="$(pwd)"
S="$ISOCAST_SCRATCH/docs-shots"
out="$S/out"
# The engine and recordings live in /Users/Shared, so the paths the CLI
# prints in the screenshots name no user.
E=/Users/Shared/isocast-shots
export ISOCAST_SOCKET="$E/engine.sock"
export ISOCAST_BACKGROUND=1
isocast="$checkout/target/release/isocast"
app="$S/Isocast.app"
cua=/Applications/CuaDriver.app/Contents/MacOS/cua-driver
# cua-driver ends idle sessions, so every call gets a fresh session label.
sid() { echo "docs-shots-$$-$(date +%s)-$RANDOM"; }
# The app window's size in points (2x pixels). It goes on a virtual HiDPI
# display made for it (virtual-display.m), so it can be bigger than the
# test Mac's own screen.
window_w="${SHOTS_WINDOW_W:-1440}"
window_h="${SHOTS_WINDOW_H:-900}"

log() { echo "shots: $*"; }
cua() { "$cua" call "$1" "$2"; }

# The window id and pid of an app's largest window, by app name (and a
# part of its title, if given).
window_of() {
    cua list_windows '{"on_screen_only":true}' | python3 -c '
import json, sys
name, title = sys.argv[1], (sys.argv[2] if len(sys.argv) > 2 else "")
wins = [w for w in json.load(sys.stdin)["windows"]
        if w.get("app_name") == name and title in (w.get("title") or "")]
wins.sort(key=lambda w: -(w["bounds"]["width"] * w["bounds"]["height"]))
print("%s %s" % (wins[0]["window_id"], wins[0]["pid"]) if wins else "")
' "$@"
}

close_window() { # close_window APP [TITLE]
    w="$(window_of "$@")"
    [ -n "$w" ] && cua hotkey "{\"pid\":${w#* },\"window_id\":${w% *},\"keys\":[\"cmd\",\"w\"],\"session\":\"$(sid)\"}" >/dev/null
}

grab() { # grab NAME APP [TITLE]
    name="$1"; shift
    set -- "$name" "$(window_of "$@")"
    [ -n "$2" ] || { log "no window for $1"; return 1; }
    wid="${2% *}"; pid="${2#* }"
    cua get_window_state "{\"pid\":$pid,\"window_id\":$wid,\"include_accessibility_tree\":false,\"max_image_dimension\":0,\"screenshot_out_file\":\"$out/$1.png\",\"session\":\"$(sid)\"}" >/dev/null
    log "$1: $(sips -g pixelWidth -g pixelHeight "$out/$1.png" | awk '/pixel/ {printf "%s ", $2}')"
}

frame() { # frame APP X Y W H [TITLE]
    set -- "$(window_of "$1" "${6:-}")" "$2" "$3" "$4" "$5"
    wid="${1% *}"; pid="${1#* }"
    cua set_window_frame "{\"pid\":$pid,\"window_id\":$wid,\"x\":$2,\"y\":$3,\"width\":$4,\"height\":$5,\"session\":\"$(sid)\"}" >/dev/null
}

menu() { # menu APP "Menu" "Item"
    w="$(window_of "$1")"; shift
    wid="${w% *}"; pid="${w#* }"
    path="$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1:]))' "$@")"
    cua invoke_menu "{\"pid\":$pid,\"window_id\":$wid,\"path\":$path,\"session\":\"$(sid)\"}" >/dev/null
}

start() {
    mkdir -p "$out"
    # Hold the benchmark lock (an hour at most) so no measurement runs
    # while the app and engine are up; stop releases it.
    rm -f "$S/locked"
    python3 -c '
import fcntl, sys, time
lock = open(sys.argv[1], "a")
fcntl.flock(lock, fcntl.LOCK_EX)
open(sys.argv[2], "w").close()
time.sleep(3600)
' "$ISOCAST_SCRATCH/.bench.lock" "$S/locked" >/dev/null 2>&1 &
    echo $! >"$S/lock.pid"
    while [ ! -e "$S/locked" ]; do sleep 1; done
    log "holding the benchmark lock"
    pgrep -q -f "CuaDriver.app/Contents/MacOS/cua-driver serve" || open -g -n -a CuaDriver --args serve
    sleep 2
    swift "$S/display-mode.swift" --hidpi
    # A HiDPI display big enough for the app window (the test Mac's own
    # screen is smaller); stop removes it.
    clang -fobjc-arc -framework Foundation -framework CoreGraphics \
        "$S/virtual-display.m" -o "$S/virtual-display" || return 1
    "$S/virtual-display" "$window_w" "$((window_h + 60))" >"$S/vd.log" 2>&1 &
    echo $! >"$S/vd.pid"
    while ! grep -q '^display' "$S/vd.log"; do sleep 1; done
    log "$(cat "$S/vd.log")"
    # Terminal first: the Screen scene captures its window, and the CLI in
    # it waits for $S/demo.go before driving the engine.
    "$isocast" engine stop >/dev/null 2>&1 || true
    rm -rf "${E:?}" /Users/Shared/Recordings && mkdir -p "$E"
    cp "$S/setup.toml" "$S/landscape.jpg" "$S/brb.jpg" "$S/cli.command" "$E/"
    chmod +x "$E/cli.command"
    rm -f "$S/demo.go" "$S/demo.done"
    {
        echo "ISOCAST=\"$isocast\""
        echo "ISOCAST_SOCKET=\"$ISOCAST_SOCKET\""
        echo "ISOCAST_DEV_APP=\"${ISOCAST_DEV_APP:-}\""
        echo "DEMO_GO=\"$S/demo.go\""
        echo "DEMO_DONE=\"$S/demo.done\""
        echo "DEMO_DIR=\"$E\""
    } >"$E/demo.env"
    # Terminal's dark "Pro" profile for these shots (stop restores it).
    [ -e "$S/terminal-default" ] ||
        defaults read com.apple.Terminal "Default Window Settings" >"$S/terminal-default" 2>/dev/null ||
        echo Basic >"$S/terminal-default"
    defaults write com.apple.Terminal "Default Window Settings" Pro
    defaults write com.apple.Terminal "Startup Window Settings" Pro
    open -a Terminal "$E/cli.command"
    sleep 3
    frame Terminal 32 60 896 504 cli.command
    # The isolated engine with the screenshot setup.
    "$isocast" apply --revision 0 "$E/setup.toml" >/dev/null || return 1
    "$isocast" engine start >/dev/null || return 1
    touch "$S/demo.go"
    # The app, as a bundle so macOS calls it Isocast.
    ISOCAST_UI_APP="$app" sh scripts/ui-dev.sh --release >/dev/null 2>&1 || {
        log "ui-dev.sh failed"; return 1; }
    env ISOCAST_SOCKET="$ISOCAST_SOCKET" "$app/Contents/MacOS/isocast-ui" >"$S/app.log" 2>&1 &
    sleep 6
    set -- $(cat "$S/vd.log")
    frame Isocast "${3%.*}" 30 "$window_w" "$window_h"
    sleep 2
    log "app window: $(window_of Isocast) $(cua list_windows '{"on_screen_only":true}' | python3 -c '
import json, sys
for w in json.load(sys.stdin)["windows"]:
    if w.get("app_name") == "Isocast":
        b = w["bounds"]; print("%gx%g at %g,%g" % (b["width"], b["height"], b["x"], b["y"]))
')"
}

front() { # front APP [TITLE]: bring it to the front, the pointer out of the way
    w="$(window_of "$@")"
    cua bring_to_front "{\"pid\":${w#* },\"session\":\"$(sid)\"}" >/dev/null
    swift "$S/cursor.swift"
    sleep 1
}

stop() {
    pkill -f "$app/Contents/MacOS/isocast-ui" 2>/dev/null
    "$isocast" stop >/dev/null 2>&1
    "$isocast" engine stop >/dev/null 2>&1
    pkill -f "$E/engine" 2>/dev/null
    pkill -f "$E/cli.command" 2>/dev/null
    sleep 1
    # Close our Terminal windows (titled with the engine folder's name).
    while w="$(window_of Terminal isocast-shots)" && [ -n "$w" ]; do
        cua hotkey "{\"pid\":${w#* },\"window_id\":${w% *},\"keys\":[\"cmd\",\"w\"],\"session\":\"$(sid)\"}" >/dev/null || break
        sleep 1
        [ "$w" = "$(window_of Terminal isocast-shots)" ] && break
    done
    close_window Finder Recordings
    close_window Finder Untitled
    if [ -e "$S/terminal-default" ]; then
        defaults write com.apple.Terminal "Default Window Settings" "$(cat "$S/terminal-default")"
        defaults write com.apple.Terminal "Startup Window Settings" "$(cat "$S/terminal-default")"
        rm -f "$S/terminal-default"
    fi
    [ -e "$S/vd.pid" ] && kill "$(cat "$S/vd.pid")" 2>/dev/null
    rm -f "$S/vd.pid" "$S/vd.log"
    swift "$S/display-mode.swift" 3 >/dev/null 2>&1 || true
    [ -e "$S/lock.pid" ] && kill "$(cat "$S/lock.pid")" 2>/dev/null
    rm -f "$S/lock.pid" "$S/locked"
    rm -rf "${E:?}" /Users/Shared/Recordings
    log "stopped, lock released"
}

case "${1:-all}" in
start) start ;;
stop) stop ;;
shoot) . "$S/shoot.sh" ;;
all) start && . "$S/shoot.sh"; stop ;;
*) sed -n 2,10p "$0"; exit 2 ;;
esac
