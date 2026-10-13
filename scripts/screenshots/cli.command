#!/bin/sh
# What the Terminal shows in the screenshots: the CLI driving the engine the
# app is showing. remote.sh writes demo.env (ISOCAST, ISOCAST_SOCKET,
# DEMO_GO, DEMO_DONE, DEMO_DIR) beside it, since Terminal doesn't pass the caller's environment.
. "$(dirname "$0")/demo.env"
export ISOCAST_SOCKET
printf '\033]0;isocast\007\033]7;file://localhost%s\007' "$DEMO_DIR"
cd "$DEMO_DIR"
clear
isocast() { "$ISOCAST" "$@"; }
run() {
    printf '\033[1;33m❯\033[0m %s\n' "$*"
    sleep 0.6
    "$@"
    echo
}
# Wait until remote.sh has started the engine (it captures this window).
while [ ! -e "$DEMO_GO" ]; do sleep 0.5; done
run isocast scene screen --cut
run isocast start
sleep 3
run isocast marker "intro done"
run isocast status
printf '\033[1;33m❯\033[0m '
touch "$DEMO_DONE"
read -r _
