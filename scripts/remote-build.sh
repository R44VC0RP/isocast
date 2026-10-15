#!/bin/sh
# Build, lint and test on exonbook (Ryan's M4 Max, 14 cores) instead of
# Ryan's Mac, with a warm target per lane, and copy binaries back.
#
#   scripts/remote-build.sh cargo <args>…      sync this checkout, run
#                                              `cargo <args>` there
#   scripts/remote-build.sh build [--to DIR] [BIN…]
#                                              cargo build --release of BINs
#                                              (default isocastd isocast
#                                              isocast-ui), then fetch them
#   scripts/remote-build.sh gates              the full workspace gates: fmt
#                                              --check, clippy --release
#                                              --all-targets -D warnings,
#                                              test --release
#   scripts/remote-build.sh fetch [--to DIR] BIN…
#                                              copy release binaries back
#   scripts/remote-build.sh run '<command>'    sync, run a shell command in
#                                              the lane's source folder there
#   scripts/remote-build.sh status             free space, lanes, sizes, last
#                                              use, which lanes are building
#   scripts/remote-build.sh prune [--days N | LANE…]
#                                              delete lanes unused for N days
#                                              (default 3), or the named ones
#
# The lane is $ISOCAST_LANE, default this checkout's folder name ("main" for
# the main checkout); it lives in ~/isocast-builds/<lane>/{src,target} on
# $ISOCAST_BUILD_HOST (default exonbook). Sync sends tracked and untracked,
# non-ignored files (uncommitted edits included, never target/ or .git),
# keeps their mtimes so cargo stays incremental, and deletes files that are
# gone here. A new lane's target starts as an APFS clone of the most
# recently used lane's (third-party crates stay warm).
#
# Fetched binaries go to DIR, default ${CARGO_TARGET_DIR:-target}/release.
# They are the same arch and macOS target as local builds (rustc's default
# minimum macOS 11, our Objective-C at 14.0): run them, or push them to a
# test Mac with scripts/testmac.sh push. scripts/sign-dev.sh still wraps
# isocastd in a dev app wherever it runs (e2e.py and bench.py sign their own).
#
# exonbook stays usable: builds run at nice 15, one at a time per lane (a
# second call waits up to 30 min for the lane's lock), and nothing starts
# below $ISOCAST_BUILD_MIN_FREE_GB (default 40) GB free there. Ctrl-C or
# SIGTERM stops the remote build too. The exit status is cargo's (255:
# exonbook unreachable; 3: disk guard; 75: lane lock timeout).
set -eu

host="${ISOCAST_BUILD_HOST:-exonbook}"
min_free_gb="${ISOCAST_BUILD_MIN_FREE_GB:-40}"
remote_script='isocast-builds/.remote-build.sh'

# ---- the remote half (runs on exonbook as `sh .remote-build.sh __<op>`) ----
remote_main() {
    op="$1"
    shift
    root="$HOME/isocast-builds"
    export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"
    case "$op" in
    __prepare)
        # __prepare LANE: make the lane's folders and seed its target.
        lane="$1"
        mkdir -p "$root/$lane/src"
        if [ ! -d "$root/$lane/target" ]; then
            seed="$(ls -t "$root"/*/last-used 2>/dev/null | while read -r f; do
                d="$(dirname "$f")"
                [ "$d" != "$root/$lane" ] && [ -d "$d/target" ] && { echo "$d/target"; break; }
            done)"
            if [ -n "$seed" ]; then
                echo "remote-build: seeding $lane's target from $seed" >&2
                cp -c -R "$seed" "$root/$lane/target.tmp" && mv "$root/$lane/target.tmp" "$root/$lane/target"
            fi
        fi
        touch "$root/$lane/last-used"
        ;;
    __clean)
        # __clean LANE < NUL-separated file list: delete files that are no
        # longer in the checkout, then empty folders.
        lane="$1"
        /usr/bin/python3 -c '
import os, sys
root = sys.argv[1]
keep = set(p for p in sys.stdin.buffer.read().decode().split("\0") if p)
for d, dirs, files in os.walk(root, topdown=False):
    for f in files:
        rel = os.path.relpath(os.path.join(d, f), root)
        if rel not in keep:
            os.remove(os.path.join(d, f))
    if d != root and not os.listdir(d):
        os.rmdir(d)
' "$root/$lane/src"
        ;;
    __run)
        # __run LANE CMD: disk guard, lane lock, nice, then CMD (a shell
        # string) in the lane's source folder.
        lane="$1"
        shift
        avail_gb=$(($(df -k "$HOME" | awk 'NR == 2 { print $4 }') / 1048576))
        if [ "$avail_gb" -lt "$min_free_gb" ]; then
            echo "remote-build: refusing: $avail_gb GB free on $(hostname -s), below $min_free_gb GB." >&2
            echo "remote-build: free space with scripts/remote-build.sh prune (status lists lanes)." >&2
            exit 3
        fi
        lock="$root/$lane/lock"
        set +e
        lockf -s -k -t 0 "$lock" sh "$root/.remote-build.sh" __locked "$lane" "$@"
        status=$?
        if [ "$status" = 75 ]; then
            echo "remote-build: lane $lane is busy (another build); waiting up to 30 min" >&2
            lockf -k -t 1800 "$lock" sh "$root/.remote-build.sh" __locked "$lane" "$@"
            status=$?
        fi
        exit "$status"
        ;;
    __locked)
        lane="$1"
        shift
        # The whole remote command is one process group (sshd starts a new
        # session); __kill signals it.
        ps -o pgid= -p $$ | tr -d ' ' >"$root/$lane/run.pgid"
        touch "$root/$lane/last-used"
        export CARGO_TARGET_DIR="$root/$lane/target" CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-never}"
        cd "$root/$lane/src"
        set +e
        nice -n 15 sh -c "$*"
        status=$?
        rm -f "$root/$lane/run.pgid"
        exit "$status"
        ;;
    __kill)
        lane="$1"
        pgid="$(cat "$root/$lane/run.pgid" 2>/dev/null || true)"
        [ -n "$pgid" ] && kill -TERM -- "-$pgid" 2>/dev/null && echo "remote-build: stopped $lane's build" >&2
        rm -f "$root/$lane/run.pgid"
        ;;
    __status)
        df -h "$HOME" | awk 'NR == 2 { print "free: " $4 " of " $2 }'
        echo "minimum to build: $min_free_gb GB"
        for d in "$root"/*/; do
            [ -f "$d/last-used" ] || continue
            lane="$(basename "$d")"
            size="$(du -sh "$d" 2>/dev/null | cut -f1)"
            used="$(stat -f %Sm -t '%Y-%m-%d %H:%M' "$d/last-used")"
            busy=idle
            [ -f "$d/run.pgid" ] && kill -0 "-$(cat "$d/run.pgid")" 2>/dev/null && busy=building
            printf '%-28s %6s  last used %s  %s\n' "$lane" "$size" "$used" "$busy"
        done
        ;;
    __prune)
        # __prune DAYS [LANE…]: named lanes, or every lane unused for DAYS.
        days="$1"
        shift
        if [ $# -gt 0 ]; then
            lanes="$*"
        else
            lanes="$(find "$root" -mindepth 2 -maxdepth 2 -name last-used -mtime "+$days" |
                while read -r f; do basename "$(dirname "$f")"; done)"
        fi
        for lane in $lanes; do
            case "$lane" in */* | . | .. | '') echo "remote-build: bad lane name '$lane'" >&2; continue ;; esac
            dir="$root/$lane"
            [ -d "$dir" ] || { echo "remote-build: no lane $lane" >&2; continue; }
            # Skip a lane that is building (its lock is held).
            if ! lockf -k -t 0 "$dir/lock" true 2>/dev/null; then
                echo "remote-build: $lane is building; kept" >&2
                continue
            fi
            echo "remote-build: deleting $lane ($(du -sh "$dir" | cut -f1))"
            rm -rf "${dir:?}"
        done
        df -h "$HOME" | awk 'NR == 2 { print "free: " $4 }'
        ;;
    esac
}

case "${1:-}" in
__*)
    remote_main "$@"
    exit
    ;;
esac

# ---- the local half ----
here="$(cd "$(dirname "$0")/.." && pwd)"
lane="${ISOCAST_LANE:-$(basename "$here")}"
[ "$lane" = isocast ] && lane=main
case "$lane" in */* | .* | '') echo "remote-build: bad lane name '$lane'" >&2; exit 2 ;; esac

ssh_remote() {
    ssh -o ConnectTimeout=10 -o ServerAliveInterval=30 "$host" "$@"
}

# Install this script there (atomically: lanes call it concurrently).
install_remote() {
    ssh_remote "mkdir -p isocast-builds && cat >$remote_script.\$\$ && mv $remote_script.\$\$ $remote_script" <"$0"
}

remote() { # op args… (each arg one word; no shell metacharacters)
    ssh_remote "ISOCAST_BUILD_MIN_FREE_GB=$min_free_gb sh $remote_script $*"
}

files() {
    git -C "$here" ls-files -z --cached --others --exclude-standard |
        perl -0 -ne 'chomp; print "$_\0" if -f "'"$here"'/$_"'
}

sync() {
    install_remote
    remote __prepare "$lane"
    files | rsync -a --from0 --files-from=- -e "ssh -o ConnectTimeout=10" "$here/" "$host:isocast-builds/$lane/src/"
    files | ssh_remote "sh $remote_script __clean $lane"
}

# Run a shell string under the lane lock there; Ctrl-C/TERM stop it there.
run_remote() {
    quoted="$(printf '%s' "$*" | sed "s/'/'\\\\''/g")"
    trap 'kill "$pid" 2>/dev/null; remote __kill "$lane" || true; exit 130' INT TERM
    ssh_remote "ISOCAST_BUILD_MIN_FREE_GB=$min_free_gb sh $remote_script __run $lane '$quoted'" &
    pid=$!
    set +e
    wait "$pid"
    status=$?
    set -e
    trap - INT TERM
    return "$status"
}

fetch() { # dir bin…
    dir="$1"
    shift
    mkdir -p "$dir"
    for bin in "$@"; do
        case "$bin" in */* | '') echo "remote-build: bad binary name '$bin'" >&2; exit 2 ;; esac
        set -- "$@" "$host:isocast-builds/$lane/target/release/$bin"
        shift
    done
    rsync -a -e "ssh -o ConnectTimeout=10" "$@" "$dir/"
    echo "remote-build: fetched to $dir"
}

default_dir="${CARGO_TARGET_DIR:-$here/target}/release"

case "${1:-}" in
cargo)
    shift
    [ $# -ge 1 ] || { echo "usage: scripts/remote-build.sh cargo <args>…" >&2; exit 2; }
    sync
    args=""
    for a in "$@"; do args="$args '$(printf '%s' "$a" | sed "s/'/'\\\\''/g")'"; done
    run_remote "cargo$args"
    ;;
build)
    shift
    dir="$default_dir"
    if [ "${1:-}" = --to ]; then dir="$2"; shift 2; fi
    [ $# -ge 1 ] || set -- isocastd isocast isocast-ui
    bins=""
    for b in "$@"; do bins="$bins --bin $b"; done
    sync
    run_remote "cargo build --release$bins"
    fetch "$dir" "$@"
    ;;
gates)
    sync
    run_remote 'cargo fmt --check && cargo clippy --release --all-targets -- -D warnings && cargo test --release'
    ;;
fetch)
    shift
    dir="$default_dir"
    if [ "${1:-}" = --to ]; then dir="$2"; shift 2; fi
    [ $# -ge 1 ] || { echo "usage: scripts/remote-build.sh fetch [--to DIR] BIN…" >&2; exit 2; }
    fetch "$dir" "$@"
    ;;
run)
    shift
    [ $# -ge 1 ] || { echo "usage: scripts/remote-build.sh run '<command>'" >&2; exit 2; }
    sync
    run_remote "$*"
    ;;
status)
    install_remote
    remote __status
    ;;
prune)
    shift
    days=3
    if [ "${1:-}" = --days ]; then days="$2"; shift 2; fi
    case "$days" in *[!0-9]* | '') echo "remote-build: --days takes a number" >&2; exit 2 ;; esac
    for l in "$@"; do case "$l" in */* | .* | '') echo "remote-build: bad lane name '$l'" >&2; exit 2 ;; esac; done
    install_remote
    remote __prune "$days" "$@"
    ;;
*)
    sed -n '2,46p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
