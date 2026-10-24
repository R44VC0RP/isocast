#!/bin/sh
# Build, lint and test on a build host instead of Ryan's Mac, with a warm
# target per lane, and copy binaries back.
# Build hosts need CMake on PATH for bundled libopus and aws-lc-sys.
#
#   scripts/remote-build.sh [--host H] cargo <args>…
#                                              sync this checkout, run
#                                              `cargo <args>` there
#   scripts/remote-build.sh [--host H] build [--to DIR] [BIN…]
#                                              cargo build --release of BINs
#                                              (default isocastd isocast
#                                              isocast-ui), then fetch them
#   scripts/remote-build.sh [--host H] gates   the full workspace gates: fmt
#                                              --check, clippy --release
#                                              --locked --all-targets -D
#                                              warnings, test --release
#                                              --locked
#   scripts/remote-build.sh [--host H] fetch [--to DIR] BIN…
#                                              copy release binaries back
#                                              (from the lane's host)
#   scripts/remote-build.sh [--host H] run '<command>'
#                                              sync, run a shell command in
#                                              the lane's source folder there
#   scripts/remote-build.sh [--host H] hosts   what auto sees on each host
#   scripts/remote-build.sh [--host H] status  free space, lanes, sizes, last
#                                              use, which lanes are building
#                                              (every host unless --host)
#   scripts/remote-build.sh [--host H] prune [--days N | LANE…]
#                                              delete lanes unused for N days
#                                              (default 3), or the named ones
#   scripts/remote-build.sh [--host H] tidy [--dry-run] [--days N]
#                                              free space (every host unless
#                                              --host): lane targets unused
#                                              for N days (default 2; never
#                                              main's or a building lane's),
#                                              and in ~/isocast-scratch cargo
#                                              target/ folders unused for 3
#                                              days and recordings/traces
#                                              (*.mov *.mp4 *.flv *.mkv *.h264
#                                              *.trace) over 20 MB older than
#                                              7 days; logs, JSON and reports
#                                              stay. Nothing is tidied while
#                                              a hardware job or benchmark
#                                              holds the host.
#                                              Never ~/.lima. --dry-run lists
#                                              what and how much.
#
# Hosts (--host, or $ISOCAST_BUILD_HOST): exonbook (M4 Max, 14 cores),
# m2-a and m2-b (isocast-m2-a/-b, the AWS M2 test Macs, 8 cores), or auto
# (the default). Auto is sticky per lane: a lane stays on the host it last
# built on in the last 12 h (its warm target is there) while that host is
# free, however loaded; otherwise it
# takes the free host with the lowest load (1-minute load per core plus
# builds running there, ties in the order of $ISOCAST_BUILD_HOSTS). A host
# is not free while it's unreachable, below the disk minimum, running a
# hardware-queue job or its receiver (hw-batch: /Volumes/IsocastBuild/
# .bench.lock held, its running/*.json; on the host ~/isocast-builds/.hw-job
# or ~/isocast-scratch/.bench.lock), or, from 45 min before the nightly
# batch ($ISOCAST_HW_NIGHTLY, default 01:30), the target of a queued job.
# An explicit --host moves the lane there (sticky from then on). The lane's
# host is remembered in $ISOCAST_BUILD_STATE/<lane> (default
# ~/.cache/isocast-remote-build), so fetch finds the binaries.
#
# The lane is $ISOCAST_LANE, default this checkout's folder name ("main" for
# the main checkout); it lives in ~/isocast-builds/<lane>/{src,target} on
# its host. Sync sends tracked and untracked, non-ignored files
# (uncommitted edits included, never target/ or .git), keeps their mtimes
# so cargo stays incremental, and deletes files that are gone here. A new
# lane's target on a host starts as an APFS clone of the most recently used
# lane's there (third-party crates stay warm).
#
# Fetched binaries go to DIR, default ${CARGO_TARGET_DIR:-target}/release.
# They are the same arch (arm64) and macOS target as local builds on any
# host (rustc's default minimum macOS 11, our Objective-C at 14.0; the same
# Rust and SDK everywhere): run them, or push them to a test Mac with
# scripts/testmac.sh push. The linker signs them ad hoc; scripts/sign-dev.sh
# still wraps isocastd in a dev app wherever it runs (testmac.sh push,
# e2e.py and bench.py do it themselves).
#
# Build hosts stay usable: builds run at nice 15, one at a time per lane (a
# second call waits up to 30 min for the lane's lock), and nothing starts
# below $ISOCAST_BUILD_MIN_FREE_GB (default 40) GB free there (a build
# refused for space tidies the host once, as above, and retries). Ctrl-C or
# SIGTERM stops the remote build too. The exit status is cargo's (255: host
# unreachable; 3: disk guard; 75: lane lock timeout; 76: the host is
# reserved by a hardware job; 69: auto found no free host).
set -eu

host_arg="${ISOCAST_BUILD_HOST:-auto}"
min_free_gb="${ISOCAST_BUILD_MIN_FREE_GB:-40}"
remote_script='isocast-builds/.remote-build.sh'

# ---- the remote half (runs on the build host as `sh .remote-build.sh __<op>`) ----
# hw-batch writes "JOB_ID EXPIRY_EPOCH" to ~/isocast-builds/.hw-job on the
# test Macs a job uses; prints the job id while it holds.
hw_reserved() {
    f="$HOME/isocast-builds/.hw-job"
    [ -f "$f" ] || return 1
    read -r id exp <"$f" || true
    [ "$(date +%s)" -lt "${exp:-0}" ] 2>/dev/null || return 1
    echo "$id"
}

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
        # A host a hardware job holds is neither built on nor tidied.
        if hw="$(hw_reserved)"; then
            echo "remote-build: refusing: $(hostname -s) is reserved by hardware job $hw; use another host (--host auto)." >&2
            exit 76
        fi
        avail_gb=$(($(df -k "$HOME" | awk 'NR == 2 { print $4 }') / 1048576))
        if [ "$avail_gb" -lt "$min_free_gb" ]; then
            # Once: tidy the host (never this lane: its last-used is fresh),
            # then measure again.
            echo "remote-build: $avail_gb GB free on $(hostname -s), below $min_free_gb GB; tidying" >&2
            remote_main __tidy 2 0 >&2 || true
            before_gb="$avail_gb"
            avail_gb=$(($(df -k "$HOME" | awk 'NR == 2 { print $4 }') / 1048576))
            echo "remote-build: tidying freed $((avail_gb - before_gb)) GB; $avail_gb GB free now" >&2
        fi
        if [ "$avail_gb" -lt "$min_free_gb" ]; then
            echo "remote-build: refusing: $avail_gb GB free on $(hostname -s), below $min_free_gb GB." >&2
            echo "remote-build: free space with scripts/remote-build.sh tidy or prune (status lists lanes)." >&2
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
    __probe)
        # One line for auto: cores load1 free_gb builds hw_job bench_lock.
        cores="$(sysctl -n hw.ncpu)"
        load="$(sysctl -n vm.loadavg | awk '{ print $2 }')"
        free=$(($(df -k "$HOME" | awk 'NR == 2 { print $4 }') / 1048576))
        builds=0
        for f in "$root"/*/run.pgid; do
            [ -f "$f" ] && kill -0 "-$(cat "$f")" 2>/dev/null && builds=$((builds + 1))
        done
        hw="$(hw_reserved || echo -)"
        bench=free
        if [ -f "$HOME/isocast-scratch/.bench.lock" ] &&
            ! lockf -s -k -t 0 "$HOME/isocast-scratch/.bench.lock" true 2>/dev/null; then
            bench=held
        fi
        echo "$cores $load $free $builds $hw $bench"
        ;;
    __status)
        df -h "$HOME" | awk 'NR == 2 { print "free: " $4 " of " $2 }'
        echo "minimum to build: $min_free_gb GB"
        hw="$(hw_reserved)" && echo "reserved by hardware job $hw"
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
    __tidy)
        # __tidy DAYS DRY: see `tidy` in the header. DRY 1 lists only.
        days="$1"
        dry="$2"
        busy=
        if hw="$(hw_reserved)"; then
            busy="hardware job $hw holds this host"
        elif [ -f "$HOME/isocast-scratch/.bench.lock" ] &&
            ! lockf -s -k -t 0 "$HOME/isocast-scratch/.bench.lock" true 2>/dev/null; then
            busy="a benchmark holds ~/isocast-scratch/.bench.lock"
        fi
        if [ -n "$busy" ]; then
            echo "remote-build: nothing tidied: $busy"
            # return, not exit: __run calls this in-process and must go on to
            # its disk check (exit would end the build as a false success).
            return 0
        fi
        /usr/bin/python3 - "$root" "$HOME/isocast-scratch" "$days" "$dry" <<'PY'
import os, shutil, subprocess, sys, time

root, scratch, days, dry = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4] == "1"
now = time.time()
DAY = 86400
MEDIA = (".mov", ".mp4", ".flv", ".mkv", ".h264", ".trace")
BIG = 20 * 1024 * 1024
home = os.path.expanduser("~")
never = (os.path.realpath(os.path.join(home, ".lima")),)


def stats(path):
    """(bytes, newest modification time) of a file or a folder's tree,
    folders included; files that vanish meanwhile are skipped. APFS clones
    shared between lanes count in each."""
    try:
        st = os.lstat(path)
    except OSError:
        return 0, 0
    if not os.path.isdir(path) or os.path.islink(path):
        return st.st_blocks * 512, st.st_mtime
    total, newest = 0, st.st_mtime
    for d, dirs, files in os.walk(path):
        for name in dirs + files:
            try:
                st = os.lstat(os.path.join(d, name))
            except OSError:
                continue
            newest = max(newest, st.st_mtime)
            if name in files:
                total += st.st_blocks * 512
    return total, newest


def gb(n):  # GiB, as df -k / 1048576 (the disk guard's GB); MiB below one
    return f"{n / 2**30:.1f} GB" if n >= 2**30 else f"{n / 2**20:.0f} MB"


def safe(path, inside):
    """`path` is no symlink, really inside `inside`, and not under ~/.lima."""
    real = os.path.realpath(path)
    return (not os.path.islink(path) and real.startswith(os.path.realpath(inside) + os.sep)
            and not any(real == n or real.startswith(n + os.sep) for n in never))


def remove(path, inside, why, n, lock=None):
    """Delete `path` (under `lock` if given); returns the bytes freed."""
    if not safe(path, inside):
        print(f"remote-build: kept {path} (a symlink, outside {inside}, or never touched)")
        return 0
    if dry:
        print(f"remote-build: would delete {path} ({gb(n)}): {why}")
        return n
    if lock:
        # Under the lane's lock, so a build that starts meanwhile waits
        # for it (or this gives way: 75 is lockf's "held").
        status = subprocess.run(["lockf", "-k", "-t", "0", lock, "/bin/rm", "-rf", path]).returncode
        if status == 75:
            print(f"remote-build: {path} kept: its lane started building")
            return 0
    else:
        status = 0
        try:
            if os.path.isdir(path) and not os.path.islink(path):
                shutil.rmtree(path, ignore_errors=True)
            else:
                os.remove(path)
        except OSError as error:
            status = error.errno or 1
    left = stats(path)[0] if os.path.lexists(path) else 0
    if status or left:
        print(f"remote-build: could not delete all of {path} (status {status}; {gb(left)} left): {why}")
    else:
        print(f"remote-build: deleted {path} ({gb(n)}): {why}")
    return max(0, n - left)


def held(lock):
    """A build holds the lane's lock. A dry run creates no lock file: with
    none, nothing can hold it."""
    if not os.path.exists(lock):
        return False
    return subprocess.run(["lockf", "-k", "-t", "0", lock, "true"], capture_output=True).returncode != 0


freed = 0
# 1. Lane targets unused for DAYS (main's always stays: every new lane is
#    seeded from the most recent target, and main's is the usual one).
for lane in sorted(os.listdir(root)) if os.path.isdir(root) else []:
    d = os.path.join(root, lane)
    target, used = os.path.join(d, "target"), os.path.join(d, "last-used")
    try:
        if lane == "main" or os.path.islink(d) or not os.path.isdir(target) or not os.path.isfile(used):
            continue
        age = (now - os.path.getmtime(used)) / DAY
    except OSError:
        continue
    if age <= days:
        continue
    lock = os.path.join(d, "lock")
    if held(lock):
        print(f"remote-build: {lane} is building; kept")
        continue
    freed += remove(target, root, f"lane {lane} unused for {age:.1f} days", stats(target)[0], lock)

# 2. ~/isocast-scratch: stale cargo targets and old recordings and traces
#    (Instruments' .trace is a folder, other tools' a file).
for d, dirs, files in os.walk(scratch) if os.path.isdir(scratch) else []:
    for name in list(dirs):
        p = os.path.join(d, name)
        if os.path.islink(p):
            dirs.remove(name)
            continue
        cargo = name == "target" and any(os.path.isfile(os.path.join(p, f))
                                         for f in (".rustc_info.json", "CACHEDIR.TAG"))
        trace = name.lower().endswith(".trace")
        if not (cargo or trace):
            continue
        dirs.remove(name)
        # Last use: the newest file or folder in it (a compile writes some).
        n, newest = stats(p)
        age = (now - newest) / DAY
        if cargo and age > 3:
            freed += remove(p, scratch, f"cargo target unused for {age:.1f} days", n)
        elif trace and n > BIG and age > 7:
            freed += remove(p, scratch, f"{n / 2**20:.0f} MB trace, {age:.0f} days old", n)
    for name in files:
        p = os.path.join(d, name)
        if not name.lower().endswith(MEDIA):
            continue
        try:
            st = os.lstat(p)
        except OSError:
            continue
        age = (now - st.st_mtime) / DAY
        if os.path.islink(p) or st.st_size <= BIG or age <= 7:
            continue
        freed += remove(p, scratch, f"{st.st_size / 2**20:.0f} MB, {age:.0f} days old", st.st_blocks * 512)
free = shutil.disk_usage(home).free
print(f"remote-build: {'would free' if dry else 'freed'} about {gb(freed)} "
      f"(clones shared between lanes count once each); {gb(free)} free now")
PY
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

if [ "${1:-}" = --host ]; then
    [ $# -ge 2 ] || { echo "remote-build: --host takes exonbook, m2-a, m2-b or auto" >&2; exit 2; }
    host_arg="$2"
    shift 2
fi
build_hosts="${ISOCAST_BUILD_HOSTS:-exonbook isocast-m2-a isocast-m2-b}"
state_dir="${ISOCAST_BUILD_STATE:-$HOME/.cache/isocast-remote-build}"
hw_root="${ISOCAST_HW_ROOT:-/Volumes/IsocastBuild}"
case "$host_arg" in
m2-a | m2-b) host_arg="isocast-$host_arg" ;;
auto | exonbook | isocast-m2-a | isocast-m2-b) ;;
*)
    case " $build_hosts " in
    *" $host_arg "*) ;;
    *) echo "remote-build: unknown host '$host_arg' (exonbook, m2-a, m2-b, auto)" >&2; exit 2 ;;
    esac
    ;;
esac
host=

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

# The test Macs a hardware job holds (isocast-m2-a/-b, one per line): the
# running job's Mac and receiver while the batch lock is held, and queued
# jobs' from 45 min before the nightly run until it starts.
hw_busy() {
    [ -d "$hw_root/hw-queue" ] || return 0
    jobs=
    # A job for one test Mac holds that Mac's lock (.bench.lock.m2-b),
    # not the batch lock: its running/*.json still says which Mac.
    for l in "$hw_root"/.bench.lock.m2-*; do
        if [ -f "$l" ] && ! lockf -s -k -t 0 "$l" true 2>/dev/null; then
            jobs="$(ls "$hw_root"/hw-queue/running/*.json 2>/dev/null || true)"
        fi
    done
    if ! lockf -s -k -t 0 "$hw_root/.bench.lock" true 2>/dev/null; then
        jobs="$(ls "$hw_root"/hw-queue/running/*.json 2>/dev/null || true)"
        # Held without a running job (a bench or `now` starting): both,
        # unless the holder is the hardware batch building its own job's
        # binaries before it picks a Mac (ISOCAST_HW_BUILDING).
        if [ -z "$jobs" ] && [ -z "${ISOCAST_HW_BUILDING:-}" ]; then
            printf 'isocast-m2-a\nisocast-m2-b\n'
            return 0
        fi
    fi
    nightly="${ISOCAST_HW_NIGHTLY:-01:30}"
    hh="$(date +%H)" mm="$(date +%M)" nh="${nightly%:*}" nm="${nightly#*:}"
    now=$((${hh#0} * 60 + ${mm#0}))
    start=$((${nh#0} * 60 + ${nm#0}))
    ahead=$(((start - now + 1440) % 1440))
    if [ "$ahead" -gt 0 ] && [ "$ahead" -le 45 ]; then
        jobs="$jobs $(ls "$hw_root"/hw-queue/queue/*.json 2>/dev/null || true)"
    fi
    [ -n "$(printf '%s' "$jobs" | tr -d ' \n')" ] || return 0
    # shellcheck disable=SC2086 # job files are named by id: no spaces
    /usr/bin/python3 -c '
import json, sys
for p in sys.argv[1:]:
    try:
        j = json.load(open(p))
    except (OSError, ValueError):
        continue
    # Not the receiver: a soak receiver only copies packets to disk.
    for m in (j.get("mac"), j.get("host")):
        if m == "any":
            print("isocast-m2-a\nisocast-m2-b")
        elif m:
            print(m if m.startswith("isocast-") else "isocast-" + m)
' $jobs
}

# Probe every build host into $probes and pick one into $host (auto; see
# the header). Uses $busy, $sticky and $recent.
pick_host() {
    for h in $build_hosts; do
        case " $busy " in *" $h "*) echo "hw-job" >"$probes/$h"; continue ;; esac
        (
            host="$h"
            if install_remote 2>/dev/null; then
                remote __probe >"$probes/$h" 2>/dev/null || echo unreachable >"$probes/$h"
            else
                echo unreachable >"$probes/$h"
            fi
        ) &
    done
    wait
    host="$(
        i=0
        for h in $build_hosts; do
            i=$((i + 1))
            printf '%s %s %s\n' "$i" "$h" "$(cat "$probes/$h" 2>/dev/null || echo unreachable)"
        done | awk -v min="$min_free_gb" -v sticky="$sticky" -v recent="$recent" '
            BEGIN { n = split(recent, r, " "); for (k = 1; k <= n; k++) picked[r[k]]++ }
            NF == 8 && $7 == "-" && $8 == "free" && $5 >= min {
                score = $4 / $3 + $6 + picked[$2]
                if ($2 == sticky) { pick = $2; found = 1 }
                if (!found && (best == "" || score < best)) { best = score; pick = $2 }
                printf "remote-build: %s load %s/%s cores, %s GB free, %s building, %d just picked\n", $2, $4, $3, $5, $6, picked[$2] > "/dev/stderr"
                next
            }
            NF == 8 { printf "remote-build: %s not free (%s GB free, hw job %s, bench lock %s)\n", $2, $5, $7, $8 > "/dev/stderr"; next }
            { printf "remote-build: %s not free (%s)\n", $2, $3 > "/dev/stderr" }
            END { print pick }'
    )"
}

# Pick the lane's host (sets $host): explicit, or auto (see the header).
choose_host() {
    mkdir -p "$state_dir"
    sticky="$(cat "$state_dir/$lane" 2>/dev/null || true)"
    if [ "$host_arg" != auto ]; then
        host="$host_arg"
        [ -n "$sticky" ] && [ "$sticky" != "$host" ] && echo "remote-build: moving lane $lane from $sticky to $host" >&2
    else
        # One pick at a time, and lanes picked in the last 3 min count as
        # building (their build may not have started yet), so lanes starting
        # together spread out.
        n=0
        until mkdir "$state_dir/.pick" 2>/dev/null; do
            n=$((n + 1))
            [ "$n" -lt 60 ] || { rmdir "$state_dir/.pick" 2>/dev/null || true; n=0; }
            sleep 0.5
        done
        trap 'rmdir "$state_dir/.pick" 2>/dev/null' EXIT
        recent="$(find "$state_dir" -maxdepth 1 -type f -mmin -3 ! -name "$lane" ! -name '*.*' -exec cat {} + 2>/dev/null | tr '\n' ' ')"
        # A lane idle for 12 h has finished its task: pick afresh.
        [ -n "$(find "$state_dir/$lane" -mmin -720 2>/dev/null)" ] || sticky=
        busy="$(hw_busy | sort -u | tr '\n' ' ')"
        probes="$(mktemp -d "${TMPDIR:-/tmp}/remote-build.XXXXXX")"
        pick_host
        if [ -z "$host" ]; then
            # Hosts that are free but for disk space: tidy each once (see
            # tidy) and look again.
            low="$(for h in $build_hosts; do
                # shellcheck disable=SC2046 # split the probe line into words
                set -- $(cat "$probes/$h" 2>/dev/null || echo unreachable)
                [ $# = 6 ] && [ "$5" = - ] && [ "$6" = free ] && [ "$3" -lt "$min_free_gb" ] && echo "$h"
            done || true)"
            if [ -n "${dry_run:-}" ]; then
                # `hosts` is a query: say what a build would do.
                for h in $low; do echo "remote-build: would tidy $h (low on disk) and look again" >&2; done
                low=
            fi
            for h in $low; do
                echo "remote-build: $h is low on disk; tidying it" >&2
                host="$h"
                remote __tidy 2 0 >&2 || true
                host=
            done
            [ -z "$low" ] || pick_host
        fi
        rm -rf "${probes:?}"
        if [ -z "$host" ]; then
            echo "remote-build: no free build host (all busy, unreachable or low on disk); retry later or pass --host" >&2
            exit 69
        fi
        if [ -n "$sticky" ] && [ "$sticky" != "$host" ]; then
            echo "remote-build: lane $lane moves from $sticky (not free) to $host; its target there starts from that host's warmest lane" >&2
        fi
    fi
    if [ "${dry_run:-}" ]; then
        echo "remote-build: lane $lane would build on $host"
    else
        echo "remote-build: lane $lane on $host" >&2
        printf '%s\n' "$host" >"$state_dir/$lane.$$" && mv "$state_dir/$lane.$$" "$state_dir/$lane"
    fi
    [ "$host_arg" != auto ] || { rmdir "$state_dir/.pick" 2>/dev/null || true; trap - EXIT; }
}

# The host that has the lane's binaries (fetch): explicit or the sticky one.
lane_host() {
    if [ "$host_arg" != auto ]; then
        host="$host_arg"
    else
        host="$(cat "$state_dir/$lane" 2>/dev/null || echo exonbook)"
    fi
}

files() {
    git -C "$here" ls-files -z --cached --others --exclude-standard |
        perl -0 -ne 'chomp; print "$_\0" if -f "'"$here"'/$_"'
}

sync() {
    choose_host
    install_remote
    remote __prepare "$lane"
    # Compare by content and give changed files the current time (-c, no
    # -t): a lane's target may be seeded from another lane's newer build,
    # and cargo trusts mtimes, so a copied-over older source mtime could
    # leave a changed crate uncompiled (seen 2026-10-16).
    files | rsync -rlpc --from0 --files-from=- -e "ssh -o ConnectTimeout=10" "$here/" "$host:isocast-builds/$lane/src/"
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
    run_remote 'cargo fmt --check && cargo clippy --release --locked --all-targets -- -D warnings && cargo test --release --locked'
    ;;
fetch)
    shift
    dir="$default_dir"
    if [ "${1:-}" = --to ]; then dir="$2"; shift 2; fi
    [ $# -ge 1 ] || { echo "usage: scripts/remote-build.sh fetch [--to DIR] BIN…" >&2; exit 2; }
    lane_host
    fetch "$dir" "$@"
    ;;
run)
    shift
    [ $# -ge 1 ] || { echo "usage: scripts/remote-build.sh run '<command>'" >&2; exit 2; }
    sync
    run_remote "$*"
    ;;
hosts)
    dry_run=1
    choose_host
    ;;
status)
    [ "$host_arg" = auto ] && hosts="$build_hosts" || hosts="$host_arg"
    for host in $hosts; do
        echo "== $host"
        { install_remote && remote __status; } || echo "unreachable"
    done
    ;;
prune)
    shift
    days=3
    if [ "${1:-}" = --days ]; then days="$2"; shift 2; fi
    case "$days" in *[!0-9]* | '') echo "remote-build: --days takes a number" >&2; exit 2 ;; esac
    for l in "$@"; do case "$l" in */* | .* | '') echo "remote-build: bad lane name '$l'" >&2; exit 2 ;; esac; done
    [ "$host_arg" = auto ] && hosts="$build_hosts" || hosts="$host_arg"
    for host in $hosts; do
        echo "== $host"
        { install_remote && remote __prune "$days" "$@"; } || echo "unreachable"
    done
    ;;
tidy)
    shift
    days=2
    dry=0
    while [ $# -gt 0 ]; do
        case "$1" in
        --dry-run) dry=1; shift ;;
        --days) [ $# -ge 2 ] || { echo "remote-build: --days takes a number" >&2; exit 2; }; days="$2"; shift 2 ;;
        *) echo "remote-build: tidy takes --dry-run and --days N, not '$1'" >&2; exit 2 ;;
        esac
    done
    case "$days" in *[!0-9]* | '' | 0) echo "remote-build: --days takes a number of days, at least 1" >&2; exit 2 ;; esac
    [ "$host_arg" = auto ] && hosts="$build_hosts" || hosts="$host_arg"
    for host in $hosts; do
        echo "== $host"
        { install_remote && remote __tidy "$days" "$dry"; } || echo "unreachable"
    done
    ;;
*)
    sed -n '2,86p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
