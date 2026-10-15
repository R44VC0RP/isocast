#!/bin/sh
# The motion harness: replays scripted pointer input into the app and
# checks that what moves follows the pointer, every frame, without
# glitches (docs/motion-testing.md).
#
#   scripts/motion.sh [--headless|--window] [--suite FILE] [--scenario NAME]
#                     [--variant idle|recording] [--clock lockstep|realtime]
#                     [--out DIR] [--bin DIR] [--frames] [--no-timing]
#                     [--dump-frames N,M,…]
#
# --headless (the default) draws the real interface offscreen on a virtual
# display, against a throwaway engine with the synthetic sources in
# scripts/motion/setup.toml: no window, no permissions, any Mac. Each
# scenario × variant gets a fresh engine. Results go to OUT/<scenario>@<variant>/
# (result.json, frames.jsonl, sheet.png) and OUT/summary.json; the exit
# status is 0 if all passed, 3 if any failed, 1 if one couldn't run.
#
# --window runs the same suite in a real window on a test Mac (macOS
# events, the window recorded at 60 fps): scripts/motion/window.py, which
# this script hands its arguments to.
set -eu

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/.." && pwd)"
mode=headless
suite="$here/motion/suite.toml"
bin="${CARGO_TARGET_DIR:-$repo/target}/release"
out=""
scenario=""
variant=""
extra=""
while [ $# -gt 0 ]; do
    case "$1" in
    --headless) mode=headless ;;
    --window) mode=window ;;
    --suite) suite="$2"; shift ;;
    --scenario) scenario="$2"; shift ;;
    --variant) variant="$2"; shift ;;
    --clock) extra="$extra --clock $2"; shift ;;
    --out) out="$2"; shift ;;
    --bin) bin="$2"; shift ;;
    --frames) extra="$extra --frames" ;;
    --no-timing) extra="$extra --no-timing" ;;
    --dump-frames) extra="$extra --dump-frames $2"; shift ;;
    -h | --help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "motion.sh: unknown option $1 (see --help)" >&2; exit 1 ;;
    esac
    shift
done

if [ "$mode" = window ]; then
    set -- --suite "$suite" --bin "$bin"
    [ -n "$scenario" ] && set -- "$@" --scenario "$scenario"
    [ -n "$variant" ] && set -- "$@" --variant "$variant"
    [ -n "$out" ] && set -- "$@" --out "$out"
    # tomllib: Python 3.11 or newer (the test Macs' Homebrew one).
    py=python3
    [ -x /opt/homebrew/bin/python3 ] && py=/opt/homebrew/bin/python3
    exec "$py" "$here/motion/window.py" "$@"
fi

isocast="$bin/isocast"
ui="$bin/isocast-ui"
for b in "$isocast" "$bin/isocastd" "$ui"; do
    [ -x "$b" ] || { echo "motion.sh: no $b; cargo build --release -p isocast-ui -p isocast-daemon -p isocast-cli" >&2; exit 1; }
done
out="${out:-$repo/target/scratch/motion/$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"

# A throwaway engine of its own: never the default socket or setup. A
# fixed folder, not mktemp: its path shows in the window (the status bar's
# recordings folder), and lockstep runs must draw the same pixels.
work="${ISOCAST_MOTION_WORK:-${TMPDIR:-/tmp}/isocast-motion}"
work="${work%/}"
case "$(basename -- "$work")" in
isocast-motion*) ;;
*)
    echo "motion.sh: the work folder's name must start with isocast-motion (it is emptied): $work" >&2
    exit 1
    ;;
esac
parent="$(cd "$(dirname -- "$work")" 2>/dev/null && pwd -P)" || {
    echo "motion.sh: no folder $(dirname -- "$work")" >&2
    exit 1
}
work="$parent/$(basename -- "$work")"
if [ -L "$work" ] || [ -L "$work.lock" ]; then
    echo "motion.sh: $work or its lock is a symlink; refusing" >&2
    exit 1
fi

# One run at a time, held by the OS: an flock on $work.lock through fd 9,
# which this shell keeps open (children don't get it: 9>&- on each), so
# the lock goes when this run's process does, crash or not; nothing to
# publish, nothing to go stale.
if [ -d "$work.lock" ]; then
    # The old lock folder (mkdir + pid file).
    holder="$(cat "$work.lock/pid" 2>/dev/null || true)"
    if [ -n "$holder" ] && kill -0 "$holder" 2>/dev/null; then
        echo "motion.sh: an older motion.sh (pid $holder) holds $work.lock" >&2
        exit 1
    fi
    rm -f "$work.lock/pid"
    rmdir "$work.lock" || exit 1
fi
exec 9>>"$work.lock"
if ! python3 -c 'import fcntl; fcntl.flock(9, fcntl.LOCK_EX | fcntl.LOCK_NB)' 2>/dev/null; then
    echo "motion.sh: another motion run holds $work.lock; wait for it, or set ISOCAST_MOTION_WORK to another isocast-motion* folder" >&2
    exit 1
fi

# Every isocastd with a file open in the work folder is ours: the engine
# (its socket, lock and logs) and its log relay. The CLI launches the
# engine detached (parent launchd), so neither a process group nor
# `engine stop` (which only lets the engine idle) ends it: ask it to shut
# down, wait, then TERM and KILL what is left.
owned() {
    # isocastd processes only: this shell has a file open there too.
    # Pid lines only: if stdout was a closed pipe, a failed echo's buffer
    # can come out of a command substitution's subshell too.
    lsof -a -c isocastd -t +D "$work" 2>/dev/null | awk '/^[0-9]+$/' | sort -u | tr '\n' ' '
}
# A line on stdout with an external printf: the shell's own echo, when
# stdout is a closed pipe (motion.sh | head), keeps the unwritten text and
# leaks it into later command substitutions.
say() {
    /usr/bin/printf '%s\n' "$*" 2>/dev/null || true
}
stop_engine() {
    if [ -S "$work/engine.sock" ]; then
        printf '{"jsonrpc":"2.0","id":1,"method":"shutdown"}\n' |
            nc -U -w 5 "$work/engine.sock" >/dev/null 2>&1 9>&- || true
    fi
    for signal in "" TERM KILL; do
        i=0
        while [ -n "$(owned)" ] && [ "$i" -lt 25 ]; do
            sleep 0.2
            i=$((i + 1))
        done
        pids="$(owned)"
        [ -z "$pids" ] && return 0
        if [ -n "$signal" ]; then
            kill "-$signal" $pids 2>/dev/null || true
        fi
    done
    if [ -n "$(owned)" ]; then
        echo "motion.sh: couldn't end $(owned)" >&2
        return 1
    fi
}
# The UI processes a run started, from the marker ("ui PID" lines), that
# are still isocast-ui --motion (not a reused pid): a killed run's UI
# outlives it (it doesn't hold the lock) and would relaunch the engine.
stop_uis() {
    [ -f "$work/.isocast-motion" ] || return 0
    pids=""
    for pid in $(sed -n 's/^ui \([0-9][0-9]*\)$/\1/p' "$work/.isocast-motion"); do
        case "$(ps -o command= -p "$pid" 2>/dev/null)" in
        *isocast-ui\ --motion*) pids="$pids $pid" ;;
        esac
    done
    [ -n "$pids" ] || return 0
    for signal in TERM KILL; do
        kill "-$signal" $pids 2>/dev/null || true
        i=0
        while [ "$i" -lt 25 ] && kill -0 $pids 2>/dev/null; do
            sleep 0.2
            i=$((i + 1))
        done
        kill -0 $pids 2>/dev/null || return 0
    done
    echo "motion.sh: couldn't end UI process(es)$pids" >&2
    return 1
}
# Empty the work folder: only one this script made (its marker inside),
# and only once none of its processes is left: the UIs first (they would
# start the engine again), then the engine and its relay.
clear_work() {
    [ -e "$work" ] || return 0
    if [ ! -e "$work/.isocast-motion" ] && [ -n "$(ls -A "$work" 2>/dev/null)" ]; then
        echo "motion.sh: $work isn't a work folder this script made (no .isocast-motion); not emptying it" >&2
        return 1
    fi
    stop_uis || return 1
    stop_engine || return 1
    rm -rf "${work:?}"
}

# Put SRC at DEST in --out without following a link: refuse a DEST that is
# a symlink or not a plain file, copy to a new temporary file in --out,
# then rename it over DEST (a rename replaces the name, never a link's
# target).
publish() {
    if [ -L "$2" ] || { [ -e "$2" ] && [ ! -f "$2" ]; }; then
        echo "motion.sh: $2 is a link or not a plain file; not writing it" >&2
        return 1
    fi
    tmp="$(mktemp "$out/.motion-tmp.XXXXXX")" || return 1
    chmod 644 "$tmp"
    if ! cat "$1" >"$tmp" || ! mv -f "$tmp" "$2"; then
        rm -f "$tmp"
        return 1
    fi
}

# A run that crashed (or was SIGKILLed) left its lock to the OS, but maybe
# its engine: end that first (while its files still say which it is),
# then empty the folder.
clear_work || exit 1
mkdir "$work"
: >"$work/.isocast-motion"
export ISOCAST_SOCKET="$work/engine.sock"
export ISOCAST_SETUP="$work/live.toml"
export ISOCAST_TIMELINES="$work/timelines"
# A dev app that doesn't exist, so the CLI starts the isocastd next to it.
export ISOCAST_DEV_APP="$work/none.app"
export ISOCAST_MONITOR_SILENT=1
export TZ=UTC LANG=en_US.UTF-8 LC_ALL=en_US.UTF-8
# Marks this invocation's results: summary.py takes no others.
MOTION_INVOCATION="$(date +%s)-$$"
export MOTION_INVOCATION
child=""

cleanup() {
    code=$?
    set +e
    trap - EXIT INT TERM
    if [ -n "$child" ]; then
        kill "$child" 2>/dev/null
        wait "$child" 2>/dev/null
    fi
    clear_work
    exit "$code"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
# The recordings go in the work folder (a relative folder would be the
# engine's working directory's).
sed "s|^folder = \"recordings\"|folder = \"$work/recordings\"|" "$here/motion/setup.toml" >"$work/setup.toml"

# A fresh engine for every run: a new process, the same setup.
fresh() {
    stop_engine
    rm -rf "${work:?}/recordings"
    # The new engine may start from the saved live setup: replace it (at
    # the revision it is at; a new engine may still be loading, so again
    # if the revision moved).
    tries=0
    until {
        rev="$("$isocast" get --json 2>/dev/null 9>&- | python3 -c 'import json, sys; print(json.load(sys.stdin).get("revision", 0))' 2>/dev/null || echo 0)"
        "$isocast" apply --revision "$rev" "$work/setup.toml" >/dev/null 2>"$work/apply.err" 9>&-
    }; do
        tries=$((tries + 1))
        if [ "$tries" -ge 5 ]; then
            cat "$work/apply.err" >&2
            return 1
        fi
        sleep 0.5
    done
    "$isocast" engine start >/dev/null 9>&-
    "$isocast" scene main --cut >/dev/null 9>&-
}

set -- --motion "$suite"
[ -n "$scenario" ] && set -- "$@" --scenario "$scenario"
[ -n "$variant" ] && set -- "$@" --variant "$variant"
"$ui" "$@" --list >"$work/runs.txt" 9>&-
# What this invocation ran and how each run ended (summary.py reads only
# these runs, and only results with this invocation's mark).
: >"$work/runs.tsv"
mkdir "$work/logs"
while read -r name var; do
    [ -n "$name" ] || continue
    fresh
    # Written in the work folder, then published into --out (publish:
    # never through a link).
    log="$work/logs/$name@$var.log"
    # shellcheck disable=SC2086 # extra is a list of options
    "$ui" --motion "$suite" --scenario "$name" --variant "$var" --out "$out" $extra \
        >"$work/logs/$name@$var.json" 2>"$log" </dev/null 9>&- &
    child=$!
    # So a later run can end it if this one is killed (see clear_work).
    echo "ui $child" >>"$work/.isocast-motion"
    code=0
    wait "$child" || code=$?
    child=""
    printf '%s\t%s\t%s\n' "$name" "$var" "$code" >>"$work/runs.tsv"
    publish "$work/logs/$name@$var.json" "$out/$name@$var.json"
    publish "$log" "$out/$name@$var.log"
    publish "$work/runs.tsv" "$out/runs.tsv"
    case "$code" in
    0) say "pass  $name@$var" ;;
    3) say "FAIL  $name@$var" ;;
    *) say "ERROR $name@$var (exit $code): $(tail -3 "$log")" ;;
    esac
done <"$work/runs.txt"
publish "$work/runs.tsv" "$out/runs.tsv"
stop_engine
status=0
python3 "$here/motion/summary.py" "$out" || status=$?
say "results: $out"
exit "$status"
