#!/bin/sh
# The test Mac's desktop-session bridge (used by scripts/testmac.sh).
#
# SSH sessions on macOS can't start apps in the logged-in desktop session
# (`open` fails with "Domain does not support specified action"), so windows,
# privacy prompts and cua-driver need a helper that already runs there: a
# launch agent that runs the jobs queued in ~/.config/isocast-testmac/queue.
# It must be ProcessType Interactive: launchd's default policy throttled its
# jobs' timers (a 10 ms sleep took 75-85 ms), which skewed every
# benchmark run through it.
#
#   testmac-bridge.sh install   copy this script to ~/.config/isocast-testmac
#                               and write the launch agent (over SSH; Ryan
#                               loads it once in the desktop session)
#   testmac-bridge.sh queue     read a job (shell script) from stdin, queue
#                               it, wait, print its output and exit with its
#                               status (over SSH)
#   testmac-bridge.sh           run the queued jobs (launchd, in the desktop
#                               session, whenever the queue changes)
set -u

dir="$HOME/.config/isocast-testmac"
queue="$dir/queue"
label=dev.isocast.testmac.bridge
plist="$HOME/Library/LaunchAgents/$label.plist"

case "${1:-}" in
install)
    set -e
    mkdir -p "$queue" "$HOME/Library/LaunchAgents"
    cp "$0" "$dir/bridge.sh"
    chmod 755 "$dir/bridge.sh"
    cat >"$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>$label</string>
    <key>ProgramArguments</key>
    <array><string>/bin/sh</string><string>$dir/bridge.sh</string></array>
    <key>WatchPaths</key><array><string>$queue</string></array>
    <key>RunAtLoad</key><true/>
    <key>ProcessType</key><string>Interactive</string>
    <key>AbandonProcessGroup</key><true/>
    <key>LimitLoadToSessionType</key><string>Aqua</string>
</dict>
</plist>
EOF
    echo "installed $plist"
    echo "load it once from Terminal in the desktop session (it then loads at every login):"
    echo "  launchctl bootstrap gui/\$(id -u) \"$plist\""
    ;;
queue)
    job="$queue/$(date +%Y%m%d-%H%M%S)-$$"
    cat >"$job.tmp" && mv "$job.tmp" "$job.sh"
    waited=0
    while [ ! -e "$job.status" ]; do
        waited=$((waited + 1))
        if [ $waited -gt 3000 ]; then
            echo "testmac: no answer from the desktop-session bridge after 10 minutes" >&2
            echo "  is it loaded? scripts/testmac.sh install-bridge" >&2
            exit 1
        fi
        sleep 0.2
    done
    cat "$job.out"
    status=$(cat "$job.status")
    rm -f "${job:?}.out" "${job:?}.status"
    exit "$status"
    ;;
"")
    # Keep going until the queue stays empty: jobs queued while one runs.
    while :; do
        found=
        for job in "$queue"/*.sh; do
            [ -e "$job" ] || continue
            found=1
            base="${job%.sh}"
            mv "$job" "$base.running"
            /bin/sh "$base.running" >"$base.out" 2>&1
            echo $? >"$base.status.tmp" && mv "$base.status.tmp" "$base.status"
            rm -f "${base:?}.running"
        done
        [ -n "$found" ] || exit 0
    done
    ;;
*)
    sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
