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
#                                              or queued, the queue, the
#                                              compiler cache (every host
#                                              unless --host)
#   scripts/remote-build.sh [--host H] prune [--days N | LANE…]
#                                              delete lanes unused for N days
#                                              (default 3), or the named ones
#   scripts/remote-build.sh [--host H] tidy [--dry-run] [--days N]
#                                              free space (every host unless
#                                              --host): lane targets unused
#                                              for N days (default 2; never
#                                              main's or a building lane's);
#                                              incremental caches (dev ones
#                                              of any idle lane, others
#                                              unused for a day); in
#                                              ~/isocast-scratch and the
#                                              test-Mac checkouts
#                                              ~/dev/isocast-worktrees/*
#                                              cargo target/ folders unused
#                                              for 3 days; in scratch
#                                              recordings/traces (*.mov *.mp4
#                                              *.flv *.mkv *.h264 *.trace)
#                                              over 20 MB older than 7 days
#                                              (logs, JSON and reports stay);
#                                              this script's versions unused
#                                              for 7 days; and last, if still
#                                              below the minimum with nothing
#                                              building or queued, the
#                                              compiler cache. Nothing is
#                                              tidied while a hardware job or
#                                              benchmark holds the host.
#                                              Never ~/.lima. --dry-run lists
#                                              what and how much.
#
# Hosts (--host, or $ISOCAST_BUILD_HOST): m2-b, m2-a, or auto (default).
# Auto builds on m2-b. It falls back to the hardware-test Mac, m2-a, if
# m2-b is unavailable or its slots are taken and m2-a has a free slot.
# If both queues are full, it queues on m2-b. Slots include lanes picked
# in the last 3 minutes that haven't reached their queue yet. A host
# is not free while it's unreachable, below the disk minimum, running a
# hardware-queue job or its receiver (hw-batch: /Volumes/IsocastBuild/
# .bench.lock held, its running/*.json; on the host ~/isocast-builds/.hw-job
# or ~/isocast-scratch/.bench.lock), or, from 45 min before the nightly
# batch ($ISOCAST_HW_NIGHTLY, default 01:30), the target of a queued job.
# An explicit --host moves the lane there for this command. The lane's
# host is remembered in $ISOCAST_BUILD_STATE/<lane> (default
# ~/.cache/isocast-remote-build), so fetch finds the binaries.
#
# The lane is $ISOCAST_LANE, default this checkout's folder name ("main" for
# the main checkout); it lives in ~/isocast-builds/<lane>/{src,target} on
# its host. Sync sends tracked and untracked, non-ignored files
# (uncommitted edits included, never target/ or .git), keeps their mtimes
# so cargo stays incremental, and deletes files that are gone here. A new
# lane's target on a host starts as an APFS clone of the most recently used
# lane's there, without its incremental caches (third-party crates stay
# warm).
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
# second call waits up to 60 min for the lane's lock), and nothing starts
# below $ISOCAST_BUILD_MIN_FREE_GB (default 40) GB free there (a build
# refused for space tidies the host once, as above, and retries). Each host
# runs two commands at once by default (override in ~/isocast-builds/.slots
# there); the rest wait in arrival
# order and print their place ("queued 2nd on m2-a"); a `run` that compiles
# nothing can skip the queue with ISOCAST_BUILD_QUEUE=0. A queued command whose
# client is gone (killed, Ctrl-C, SIGTERM) leaves the queue, and a running
# one whose client is gone is stopped. A hardware job that reserves the host
# meanwhile turns queued commands away (76). Every rustc call goes through
# the host's shared compiler cache (sccache from Homebrew, in
# ~/isocast-builds/.sccache, capped at ~/isocast-builds/.sccache-size, else
# 60G; ISOCAST_BUILD_SCCACHE=0 builds without it, as does a call whose
# daemon neither answers nor starts), and dev and test builds
# aren't incremental (CARGO_PROFILE_DEV_INCREMENTAL=true in the command
# opts back in; never CARGO_INCREMENTAL=1, which sccache refuses). The exit
# status is cargo's (255: host unreachable; 3: disk guard; 75: lane lock
# timeout; 76: the host is reserved by a hardware job; 69: auto found no
# free host).
set -eu

host_arg="${ISOCAST_BUILD_HOST:-auto}"
min_free_gb="${ISOCAST_BUILD_MIN_FREE_GB:-40}"
# The builds' folder on every host, relative to the home folder
# ($ISOCAST_BUILD_ROOT only for testing this script in isolation).
root_name="${ISOCAST_BUILD_ROOT:-isocast-builds}"

# ---- the remote half (runs on the build host as `sh .remote-build-<hash>.sh __<op>`) ----
# hw-batch writes "JOB_ID EXPIRY_EPOCH" to ~/isocast-builds/.hw-job on the
# test Macs a job uses; prints the job id while it holds.
hw_reserved() {
    f="$root/.hw-job"
    [ -f "$f" ] || return 1
    read -r id exp <"$f" || true
    [ "$(date +%s)" -lt "${exp:-0}" ] 2>/dev/null || return 1
    echo "$id"
}

# lane_group PGID LANE [CALL]: the process group PGID is still a
# remote-build command of LANE (of its call CALL if given). A pgid file can
# outlive its group, and a pid can be reused, so nothing is signalled or
# counted unless this holds.
lane_group() {
    case "$1" in '' | *[!0-9]*) return 1 ;; esac
    ps -ax -o pgid=,command= | awk -v g="$1" -v want="__run $2${3:+ $3}" '
        $1 == g && index($0, ".remote-build") && index($0 " ", " " want " ") { f = 1 }
        END { exit !f }'
}

# How many cargo jobs run at once on this host: ~/isocast-builds/.slots,
# else 2 on the M2s.
host_slots() {
    s="${ISOCAST_BUILD_SLOTS:-$(cat "$root/.slots" 2>/dev/null || true)}"
    case "$s" in '' | *[!0-9]* | 0) s=2 ;; esac
    echo "$s"
}

# The shared compiler cache (sccache, Homebrew): every lane's rustc calls go
# through it unless the lane sets its own RUSTC_WRAPPER or
# ISOCAST_BUILD_SCCACHE=0. Keys hold the compile's folder (cwd and
# CARGO_MANIFEST_DIR), so a lane's own crates are only ever reused by that
# lane; registry crates are shared by all. Incremental compiles (ui-dev) are
# passed through uncached. Client-side mode: rustc runs in the build's own
# process (nice 15, the build's environment); the daemon only stores.
# One cache and daemon per host, in ~/isocast-builds whatever the root, on
# its own socket: a daemon keeps the settings it started with, so another
# sccache user's (the default port 4226) must never be ours.
# sccache keys hold every CARGO_* variable rustc gets, so rustc goes
# through a wrapper without the lane's CARGO_TARGET_DIR (cargo's, never
# rustc's; build scripts and the lane's commands still have it): with it,
# no lane would ever share a crate. It is named sccache so the cc crate
# also caches build scripts' C through it.
sccache_env() {
    [ "${ISOCAST_BUILD_SCCACHE:-1}" != 0 ] && [ -z "${RUSTC_WRAPPER:-}" ] || return 0
    real="$(command -v sccache 2>/dev/null)" || return 0
    # Its folder: ~/isocast-builds ($ISOCAST_BUILD_SCCACHE_HOME only to test
    # this script without the host's daemon).
    base="${ISOCAST_BUILD_SCCACHE_HOME:-$HOME/isocast-builds}"
    wrapper="$base/.sccache-bin/sccache"
    if ! grep -qs "exec $real " "$wrapper"; then
        mkdir -p "${wrapper%/*}" &&
            printf '#!/bin/sh\nunset CARGO_TARGET_DIR\nexec %s "$@"\n' "$real" >"$wrapper.$$" &&
            chmod +x "$wrapper.$$" && mv "$wrapper.$$" "$wrapper"
    fi
    export RUSTC_WRAPPER="$wrapper" SCCACHE_DIR="$base/.sccache" SCCACHE_IDLE_TIMEOUT=0 \
        SCCACHE_CLIENT_SIDE=1 SCCACHE_SERVER_UDS="$base/.sccache.sock"
    SCCACHE_CACHE_SIZE="$(cat "$base/.sccache-size" 2>/dev/null || echo 60G)"
    export SCCACHE_CACHE_SIZE
}

# The daemon listens on its socket. (sccache 0.18's --show-stats is no test:
# with no daemon it exits 0 with stats computed locally, and starts none.)
sccache_up() {
    /usr/bin/python3 -c 'import socket, sys; socket.socket(socket.AF_UNIX).connect(sys.argv[1])' \
        "$SCCACHE_SERVER_UDS" 2>/dev/null
}

# The per-host build queue: at most host_slots cargo jobs, the rest wait in
# FIFO order. A ticket is a file in $root/.queue named by its arrival time,
# held (flock) by the waiting or building process for its whole life, so a
# ticket nobody holds is a dead client's and is dropped. A waiter whose ssh
# client is gone (its stderr's reader closed) gives up its place; a build
# whose client is gone is stopped. A build that starts beside others gets
# CARGO_BUILD_JOBS = cores / slots (alone: all cores) unless it sets its own.
#   python3 -c "$queue_py" wait LANE CALL HOST SLOTS MIN_FREE_GB CMD  (in the lane's src)
#   python3 -c "$queue_py" list SLOTS        prints "SLOTS LIVE LANE,LANE…" then tickets
queue_py=$(
    cat <<'PY'
import fcntl, json, os, select, shutil, signal, subprocess, sys, time

root = os.environ["ISOCAST_ROOT"]
qdir = os.path.join(root, ".queue")


def tickets():
    """Live tickets, oldest first: (name, info). Unheld ones are deleted."""
    live = []
    try:
        names = sorted(n for n in os.listdir(qdir) if not n.startswith("."))
    except OSError:
        return live
    for name in names:
        path = os.path.join(qdir, name)
        if name in own:
            live.append((name, own[name]))
            continue
        try:
            fd = os.open(path, os.O_RDONLY)
        except OSError:
            continue
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            try:
                info = json.loads(os.read(fd, 4096) or b"{}")
            except ValueError:
                info = {}
            live.append((name, info))
        else:
            # Nobody holds it: its client died. Delete it while holding it.
            try:
                os.unlink(path)
            except OSError:
                pass
        finally:
            os.close(fd)
    return live


def nth(n):
    return f"{n}{'tsnrhtdd'[(n // 10 % 10 != 1) * (n % 10 < 4) * n % 10::4]}"


def hw_job():
    try:
        job, exp = open(os.path.join(root, ".hw-job")).read().split()[:2]
        return job if time.time() < float(exp) else None
    except (OSError, ValueError):
        return None


own = {}
if sys.argv[1] == "list":
    slots = int(sys.argv[2])
    live = tickets()
    lanes = ",".join(i.get("lane", "?") for _, i in live) or "-"
    print(slots, len(live), lanes)
    for k, (name, i) in enumerate(live):
        if i.get("state") == "building":
            state = "building"
        else:
            state = f"queued {nth(k - slots + 1)}" if k >= slots else "starting"
        since = time.time() - i.get("since", time.time())
        print(f"  {i.get('lane', '?'):<28} {state:<12} {int(since // 60)} min")
    sys.exit(0)

_, lane, call, host, slots, min_free, cmd = sys.argv[1:8]
slots, min_free = int(slots), int(min_free)
# Slots 0: a command that compiles nothing (ISOCAST_BUILD_QUEUE=0) runs at
# once, without a ticket.
queued = slots > 0
os.makedirs(qdir, exist_ok=True)
name = f"{time.time_ns():020d}-{lane}"
info = {"lane": lane, "state": "queued", "since": time.time(), "pid": os.getpid()}
fd = None


def write_info():
    if fd is not None:
        os.ftruncate(fd, 0)
        os.pwrite(fd, json.dumps(info).encode(), 0)


if queued:
    # Create, lock, then rename into place: a ticket is never seen unheld.
    tmp = os.path.join(qdir, "." + name)
    fd = os.open(tmp, os.O_RDWR | os.O_CREAT | os.O_TRUNC, 0o644)
    fcntl.flock(fd, fcntl.LOCK_EX)
    write_info()
    os.rename(tmp, os.path.join(qdir, name))
    own[name] = info
child = None
lanedir = os.path.join(root, lane)


def cleanup():
    for f in ("run.pgid", f"call-{call}.pgid"):
        try:
            os.unlink(os.path.join(lanedir, f))
        except OSError:
            pass
    try:
        os.unlink(os.path.join(qdir, name))
    except OSError:
        pass


def on_term(signum, frame):
    if child and child.poll() is None:
        child.terminate()
    cleanup()
    os._exit(128 + signum)


signal.signal(signal.SIGTERM, on_term)
signal.signal(signal.SIGHUP, on_term)
kq = select.kqueue()
watch = [select.kevent(f, filter=select.KQ_FILTER_WRITE, flags=select.KQ_EV_ADD) for f in (1, 2)]


def client_gone():
    try:
        return any(e.flags & select.KQ_EV_EOF for e in kq.control(watch, 2, 0))
    except OSError:
        return True


def say(msg):
    try:
        print(f"remote-build: {msg}", file=sys.stderr, flush=True)
    except OSError:
        pass


start, last = time.time(), None
while queued:
    live = tickets()
    pos = next((k for k, (n, _) in enumerate(live) if n == name), None)
    if pos is None:
        say(f"lost its place in the queue on {host} (its ticket is gone); retry")
        cleanup()
        sys.exit(1)
    if pos < slots:
        break
    job = hw_job()
    if job:
        say(f"refusing: {host} is now reserved by hardware job {job}; use another host (--host auto).")
        cleanup()
        sys.exit(76)
    if client_gone():
        cleanup()
        sys.exit(130)
    ahead = [i.get("lane", "?") for _, i in live[:pos]]
    building = [i.get("lane", "?") for _, i in live if i.get("state") == "building"]
    msg = (f"queued {nth(pos - slots + 1)} on {host} ({slots} at a time; building: "
           f"{', '.join(building) or '-'}; waiting ahead: {', '.join(ahead[len(building):]) or '-'})")
    if msg != last:
        say(msg)
        last = msg
    time.sleep(1)

# Admitted: what was true when it queued may not be any more. run.pgid
# first, then the reservation: hw-batch writes .hw-job, then waits for the
# builds run.pgid names, so one of the two always sees the other.
pgid = str(os.getpgrp())
open(os.path.join(lanedir, "run.pgid"), "w").write(pgid + "\n")
job = hw_job()
if job:
    say(f"refusing: {host} is reserved by hardware job {job}; use another host (--host auto).")
    cleanup()
    sys.exit(76)
free_gb = shutil.disk_usage(os.path.expanduser("~")).free // 2**30
if free_gb < min_free:
    say(f"refusing: {free_gb} GB free on {host}, below {min_free} GB.")
    cleanup()
    sys.exit(3)
waited = time.time() - start
if waited >= 2:
    say(f"building on {host} after {int(waited // 60)} min {int(waited % 60)} s in the queue")
info["state"], info["since"] = "building", time.time()
write_info()
env = dict(os.environ)
if queued and "CARGO_BUILD_JOBS" not in env:
    # Cargo's jobs per build: all cores for a build that starts alone,
    # cores / slots beside others (two -j8 builds on an M2 peaked at 14
    # rustc and load 83; with this, 11 and 67, in the same time, 2026-10-24).
    others = sum(1 for n, i in tickets() if n != name and i.get("state") == "building")
    cores = os.cpu_count() or 8
    env["CARGO_BUILD_JOBS"] = str(cores if not others else max(1, cores // slots))
child = subprocess.Popen(["nice", "-n", "15", "sh", "-c", cmd], stdin=subprocess.DEVNULL, env=env)
while True:
    try:
        status = child.wait(timeout=2)
        break
    except subprocess.TimeoutExpired:
        pass
    if client_gone():
        # Nobody will read the result: stop the build, free the slot.
        os.killpg(os.getpgrp(), signal.SIGTERM)
cleanup()
sys.exit(status if status >= 0 else 128 - status)
PY
)

remote_main() {
    op="$1"
    shift
    root="$HOME/$root_name"
    self="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
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
            done || true)"
            if [ -n "$seed" ]; then
                echo "remote-build: seeding $lane's target from $seed" >&2
                # Not the seed's incremental caches (dev and ui-dev builds):
                # only that lane's sources match them.
                cp -c -R "$seed" "$root/$lane/target.tmp" &&
                    rm -rf "$root/$lane/target.tmp"/*/incremental &&
                    mv "$root/$lane/target.tmp" "$root/$lane/target"
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
        # __run LANE CALL CMD: disk guard, lane lock, the host's queue, nice,
        # then CMD (a shell string) in the lane's source folder. CALL names
        # this invocation (its client's __kill stops only it).
        lane="$1"
        call="$2"
        shift 2
        case "$call" in '' | *[!A-Za-z0-9-]*) echo "remote-build: bad call id '$call'" >&2; exit 2 ;; esac
        # A host a hardware job holds is neither built on nor tidied.
        if hw="$(hw_reserved)"; then
            echo "remote-build: refusing: ${ISOCAST_BUILD_HOSTNAME:-$(hostname -s)} is reserved by hardware job $hw; use another host (--host auto)." >&2
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
        # The compiler cache's daemon starts here, outside every lock and
        # with no terminal, so it never holds a lock or the ssh session.
        # Only when none answers, one caller at a time: on a socket a second
        # daemon would take the socket over and orphan the first.
        # If it can't be reached, this call builds without it.
        if ! (
            sccache_env
            [ -n "${SCCACHE_DIR:-}" ] || exit 0
            mkdir -p "$SCCACHE_DIR"
            lockf -k -t 60 "$SCCACHE_DIR.lock" sh -c \
                "/usr/bin/python3 -c 'import socket, sys; socket.socket(socket.AF_UNIX).connect(sys.argv[1])' \"\$SCCACHE_SERVER_UDS\" 2>/dev/null ||
                    sccache --start-server" </dev/null >/dev/null 2>&1
            sccache_up
        ); then
            echo "remote-build: the compiler cache isn't answering on $(hostname -s); building without it" >&2
            export ISOCAST_BUILD_SCCACHE=0
        fi
        # The lane lock first, then the queue: a lane's second call waits
        # for its first without taking one of the host's slots.
        # This call's process group (sshd starts a new session for it),
        # from here on: waiting for the lane lock, queued or building.
        callf="$root/$lane/call-$call.pgid"
        ps -o pgid= -p $$ | tr -d ' ' >"$callf"
        lock="$root/$lane/lock"
        set +e
        lockf -s -k -t 0 "$lock" sh "$self" __locked "$lane" "$call" "$@"
        status=$?
        if [ "$status" = 75 ]; then
            echo "remote-build: lane $lane is busy (another build); waiting up to 60 min" >&2
            lockf -k -t 3600 "$lock" sh "$self" __locked "$lane" "$call" "$@"
            status=$?
        fi
        rm -f "$callf"
        exit "$status"
        ;;
    __locked)
        lane="$1"
        call="$2"
        shift 2
        # Building, the group is also in run.pgid (hw-batch reads it).
        touch "$root/$lane/last-used"
        export CARGO_TARGET_DIR="$root/$lane/target" CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-never}"
        # Dev and test builds aren't incremental here: incremental caches
        # were most of a lane's disk (13-15 GB) and can't be shared, while
        # non-incremental compiles go through the shared cache. The lane's
        # own command can still set CARGO_PROFILE_DEV_INCREMENTAL=true.
        export CARGO_PROFILE_DEV_INCREMENTAL="${CARGO_PROFILE_DEV_INCREMENTAL:-false}"
        sccache_env
        cd "$root/$lane/src"
        slots="$(host_slots)"
        [ "${ISOCAST_BUILD_QUEUE:-1}" != 0 ] || slots=0
        ISOCAST_ROOT="$root" exec /usr/bin/python3 -c "$queue_py" wait "$lane" "$call" \
            "${ISOCAST_BUILD_HOSTNAME:-$(hostname -s)}" "$slots" "$min_free_gb" "$*"
        ;;
    __kill)
        # __kill LANE CALL: stop that call only (waiting for the lane lock,
        # queued or building), if its group is still that call's.
        lane="$1"
        f="$root/$lane/call-$2.pgid"
        pgid="$(cat "$f" 2>/dev/null || true)"
        if lane_group "$pgid" "$lane" "$2"; then
            kill -TERM -- "-$pgid" 2>/dev/null && echo "remote-build: stopped $lane's command" >&2
        fi
        rm -f "$f"
        ;;
    __probe)
        # One line for auto: cores load1 free_gb builds hw_job bench_lock
        # slots tickets lanes (tickets: building and queued; lanes: theirs,
        # oldest first, comma-separated, or -).
        cores="$(sysctl -n hw.ncpu)"
        load="$(sysctl -n vm.loadavg | awk '{ print $2 }')"
        free=$(($(df -k "$HOME" | awk 'NR == 2 { print $4 }') / 1048576))
        builds=0
        for f in "$root"/*/run.pgid; do
            [ -f "$f" ] && lane_group "$(cat "$f")" "$(basename "$(dirname "$f")")" && builds=$((builds + 1))
        done
        hw="$(hw_reserved || echo -)"
        bench=free
        if [ -f "$HOME/isocast-scratch/.bench.lock" ] &&
            ! lockf -s -k -t 0 "$HOME/isocast-scratch/.bench.lock" true 2>/dev/null; then
            bench=held
        fi
        queue="$(ISOCAST_ROOT="$root" /usr/bin/python3 -c "$queue_py" list "$(host_slots)" | head -1)"
        echo "$cores $load $free $builds $hw $bench $queue"
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
            for f in "$d"call-*.pgid; do
                [ -f "$f" ] || continue
                c="${f##*/call-}"
                lane_group "$(cat "$f")" "$lane" "${c%.pgid}" && busy=waiting
            done
            [ -f "$d/run.pgid" ] && lane_group "$(cat "$d/run.pgid")" "$lane" && busy=building
            printf '%-28s %6s  last used %s  %s\n' "$lane" "$size" "$used" "$busy"
        done
        ISOCAST_ROOT="$root" /usr/bin/python3 -c "$queue_py" list "$(host_slots)" |
            awk 'NR == 1 { slots = $1 } NR > 1 { line[NR] = $0; if ($2 == "building") b++; else w++ }
                END { printf "queue: %d at a time; %d building, %d waiting\n", slots, b, w; for (k = 2; k <= NR; k++) print line[k] }'
        (
            sccache_env
            [ -n "${SCCACHE_DIR:-}" ] && [ -d "$SCCACHE_DIR" ] || exit 0
            printf 'compiler cache: %s' "$(du -sh "$SCCACHE_DIR" 2>/dev/null | cut -f1)"
            # The daemon's cap and counters since it started (it runs until
            # stopped and keeps the cap it started with).
            if sccache_up; then sccache --show-stats 2>/dev/null; fi | awk '
                /^Compile requests  / { req = $NF } /^Cache hits  / { hit = $NF } /^Cache misses  / { miss = $NF }
                /^Max cache size/ { cap = $(NF - 1) " " $NF }
                END { if (cap != "") printf " of %s (the daemon cap)", cap; else printf " (daemon not running)"
                      if (req != "") printf ", %s hits and %s misses since it started", hit, miss }'
            echo
        )
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
        /usr/bin/python3 - "$root" "$HOME/isocast-scratch" "$days" "$dry" "$min_free_gb" <<'PY'
import fcntl, os, shutil, subprocess, sys, time

root, scratch, days, dry = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4] == "1"
min_free = int(sys.argv[5])
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


def cargo_held(target):
    """Lock a cargo target's build locks (target/<profile>/.cargo-lock,
    flock like cargo's own) without waiting: the open, locked fds while
    they're free, so a build starting meanwhile waits for the deletion,
    or None while a build holds one."""
    fds = []
    try:
        names = os.listdir(target)
    except OSError:
        return fds
    for name in names:
        lock = os.path.join(target, name, ".cargo-lock")
        if os.path.islink(lock) or not os.path.isfile(lock):
            continue
        try:
            fd = os.open(lock, os.O_RDONLY)
        except OSError:
            continue
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            os.close(fd)
            for f in fds:
                os.close(f)
            return None
        fds.append(fd)
    return fds


def remove_target(p, inside, why, n):
    """remove() a cargo target unless a build holds it (then 0)."""
    fds = cargo_held(p)
    if fds is None:
        print(f"remote-build: {p} kept: a cargo build holds it")
        return 0
    try:
        return remove(p, inside, why, n)
    finally:
        for f in fds:
            os.close(f)


def held(lock):
    """A build holds the lane's lock. A dry run creates no lock file: with
    none, nothing can hold it."""
    if not os.path.exists(lock):
        return False
    return subprocess.run(["lockf", "-k", "-t", "0", lock, "true"], capture_output=True).returncode != 0


freed = 0
gone = set()
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
    gone.add(lane)

# 1b. Incremental caches (target/<profile>/incremental/<crate>), in every
#     lane, main's too, while it isn't building: dev and test builds here
#     aren't incremental (a lane that opts back in loses only a cache), and
#     other profiles' (ui-dev) when unused for a day. Each is only ever its
#     lane's; seeded targets used to carry clones of them.
for lane in sorted(os.listdir(root)) if os.path.isdir(root) else []:
    target = os.path.join(root, lane, "target")
    if lane in gone or lane.startswith(".") or not os.path.isdir(target) or os.path.islink(target):
        continue
    lock = os.path.join(root, lane, "lock")
    for profile in sorted(os.listdir(target)):
        inc = os.path.join(target, profile, "incremental")
        if not os.path.isdir(inc) or os.path.islink(inc):
            continue
        for crate in sorted(os.listdir(inc)):
            p = os.path.join(inc, crate)
            n, newest = stats(p)
            age = (now - newest) / DAY
            if (age <= 1 and profile != "debug") or not n:
                continue
            if held(lock):
                print(f"remote-build: {lane} is building; its incremental caches kept")
                break
            why = "dev builds aren't incremental" if profile == "debug" else f"unused for {age:.1f} days"
            freed += remove(p, root, f"{lane}'s incremental cache: {why}", n, lock)

# 1c. This script's old versions (each client installs its own).
for name in sorted(os.listdir(root)) if os.path.isdir(root) else []:
    p = os.path.join(root, name)
    if name.startswith(".remote-build-") and name.endswith(".sh") and now - os.path.getmtime(p) > 7 * DAY:
        freed += remove(p, root, "an old version of remote-build.sh", os.lstat(p).st_blocks * 512)

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
            freed += remove_target(p, scratch, f"cargo target unused for {age:.1f} days", n)
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
# 2b. Test-Mac checkouts (scripts/testmac.sh, ~/dev/isocast-worktrees/<name>):
#     their cargo target/ when unused for 3 days, as in scratch.
worktrees = os.path.join(home, "dev", "isocast-worktrees")
for name in sorted(os.listdir(worktrees)) if os.path.isdir(worktrees) else []:
    p = os.path.join(worktrees, name, "target")
    if os.path.islink(os.path.join(worktrees, name)) or os.path.islink(p) or not os.path.isfile(os.path.join(p, "CACHEDIR.TAG")):
        continue
    n, newest = stats(p)
    age = (now - newest) / DAY
    if age > 3:
        freed += remove_target(p, worktrees, f"test-Mac checkout {name}'s target unused for {age:.1f} days", n)

# 3. Last: still below the build minimum, empty the compiler cache (it is
#    capped and refills) if no build is running or queued.
cache = os.path.join(home, "isocast-builds", ".sccache")
if os.path.isdir(cache) and (shutil.disk_usage(home).free - (freed if dry else 0)) // 2**30 < min_free:
    live = []
    qdir = os.path.join(root, ".queue")
    for name in os.listdir(qdir) if os.path.isdir(qdir) else []:
        try:
            fd = os.open(os.path.join(qdir, name), os.O_RDONLY)
        except OSError:
            continue
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            live.append(name)
        finally:
            os.close(fd)
    if live:
        print(f"remote-build: compiler cache kept: {len(live)} build(s) running or queued")
    else:
        if not dry:
            env = dict(os.environ, SCCACHE_SERVER_UDS=cache + ".sock")
            subprocess.run(["sccache", "--stop-server"], capture_output=True, env=env)
        freed += remove(cache, os.path.join(home, "isocast-builds"), f"still below {min_free} GB free; the compiler cache refills", stats(cache)[0])

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
    [ $# -ge 2 ] || { echo "remote-build: --host takes m2-a, m2-b or auto" >&2; exit 2; }
    host_arg="$2"
    shift 2
fi
build_hosts="${ISOCAST_BUILD_HOSTS:-isocast-m2-b isocast-m2-a}"
state_dir="${ISOCAST_BUILD_STATE:-$HOME/.cache/isocast-remote-build}"
hw_root="${ISOCAST_HW_ROOT:-/Volumes/IsocastBuild}"
case "$host_arg" in
m2-a | m2-b) host_arg="isocast-$host_arg" ;;
auto | isocast-m2-a | isocast-m2-b) ;;
*)
    case " $build_hosts " in
    *" $host_arg "*) ;;
    *) echo "remote-build: unknown host '$host_arg' (m2-a, m2-b, auto)" >&2; exit 2 ;;
    esac
    ;;
esac
host=

ssh_remote() {
    ssh -o ConnectTimeout=10 -o ServerAliveInterval=30 "$host" "$@"
}

# This script there, named by its content: lanes on other versions of it
# each run their own (installed atomically: lanes call it concurrently).
remote_script="$root_name/.remote-build-$(shasum -a 256 "$0" | cut -c1-12).sh"
install_remote() {
    ssh_remote "mkdir -p $root_name && { [ -f $remote_script ] || { cat >$remote_script.\$\$ && mv $remote_script.\$\$ $remote_script; }; } && touch $remote_script" <"$0"
}

# The settings the remote half reads (each one word).
remote_env() {
    printf 'ISOCAST_BUILD_MIN_FREE_GB=%s ISOCAST_BUILD_ROOT=%s ISOCAST_BUILD_HOSTNAME=%s' \
        "$min_free_gb" "$root_name" "${host#isocast-}"
    [ -z "${ISOCAST_BUILD_SCCACHE:-}" ] || printf ' ISOCAST_BUILD_SCCACHE=%s' "$ISOCAST_BUILD_SCCACHE"
    [ -z "${ISOCAST_BUILD_SLOTS:-}" ] || printf ' ISOCAST_BUILD_SLOTS=%s' "$ISOCAST_BUILD_SLOTS"
    [ -z "${ISOCAST_BUILD_QUEUE:-}" ] || printf ' ISOCAST_BUILD_QUEUE=%s' "$ISOCAST_BUILD_QUEUE"
    [ -z "${ISOCAST_BUILD_SCCACHE_HOME:-}" ] || printf ' ISOCAST_BUILD_SCCACHE_HOME=%s' "$ISOCAST_BUILD_SCCACHE_HOME"
}

remote() { # op args… (each arg one word; no shell metacharacters)
    ssh_remote "$(remote_env) sh $remote_script $*"
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
# the header). Uses $busy and $recent.
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
        done | awk -v min="$min_free_gb" -v recent="$recent" '
            # recent: "host:lane" picks in the last 3 min, which may not
            # have reached the queue on their host yet.
            BEGIN { n = split(recent, r, " "); for (k = 1; k <= n; k++) { split(r[k], hl, ":"); rh[k] = hl[1]; rl[k] = hl[2] } }
            NF == 11 && $7 == "-" && $8 == "free" && $5 >= min {
                q = $10; lanes = "," $11 ","; just = 0
                for (k = 1; k <= n; k++) if (rh[k] == $2 && index(lanes, "," rl[k] ",") == 0) just++
                q += just
                wait = q + 1 - $9; if (wait < 0) wait = 0
                # First prefer a free slot, then the build Mac over the test Mac.
                score = (wait > 0 ? 2 : 0) + ($2 == "isocast-m2-b" ? 0 : 1)
                if (!pick || score < best) { best = score; pick = $2 }
                printf "remote-build: %s %d of %d slots taken (%s%s), %s GB free, load %s/%s cores%s\n", $2, q, $9, $11, (just ? ", " just " just picked" : ""), $5, $4, $3, (wait ? ", would queue " wait : "") > "/dev/stderr"
                next
            }
            NF == 11 { printf "remote-build: %s not free (%s GB free, hw job %s, bench lock %s)\n", $2, $5, $7, $8 > "/dev/stderr"; next }
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
        recent="$(find "$state_dir" -maxdepth 1 -type f -mmin -3 ! -name "$lane" ! -name '*.*' 2>/dev/null |
            while read -r f; do printf '%s:%s ' "$(cat "$f")" "$(basename "$f")"; done)"
        busy="$(hw_busy | sort -u | tr '\n' ' ')"
        probes="$(mktemp -d "${TMPDIR:-/tmp}/remote-build.XXXXXX")"
        pick_host
        if [ -z "$host" ]; then
            # Hosts that are free but for disk space: tidy each once (see
            # tidy) and look again.
            low="$(for h in $build_hosts; do
                # shellcheck disable=SC2046 # split the probe line into words
                set -- $(cat "$probes/$h" 2>/dev/null || echo unreachable)
                [ $# = 9 ] && [ "$5" = - ] && [ "$6" = free ] && [ "$3" -lt "$min_free_gb" ] && echo "$h"
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
            echo "remote-build: lane $lane moves from $sticky to $host; its target there starts from that host's warmest lane" >&2
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
        host="$(cat "$state_dir/$lane" 2>/dev/null || echo isocast-m2-b)"
        case " $build_hosts " in
        *" $host "*) ;;
        *) echo "remote-build: remembered host '$host' is no longer configured; rebuild the lane before fetching" >&2; exit 69 ;;
        esac
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
    files | rsync -rlpc --from0 --files-from=- -e "ssh -o ConnectTimeout=10" "$here/" "$host:$root_name/$lane/src/"
    files | ssh_remote "sh $remote_script __clean $lane"
}

# Run a shell string under the lane lock there; Ctrl-C/TERM stop it there.
run_remote() {
    quoted="$(printf '%s' "$*" | sed "s/'/'\\\\''/g")"
    # This call's id: Ctrl-C stops this call there, never the lane's other.
    call="$$-$(date +%s)-${call_n:=0}"
    call_n=$((call_n + 1))
    trap 'kill "$pid" 2>/dev/null; remote __kill "$lane" "$call" || true; exit 130' INT TERM
    ssh_remote "$(remote_env) sh $remote_script __run $lane $call '$quoted'" &
    pid=$!
    set +e
    wait "$pid"
    status=$?
    set -e
    trap - INT TERM
    return "$status"
}

# Sync, then run a shell string there; under auto, once more elsewhere if
# a hardware job reserved the host meanwhile (76).
sync_run() {
    sync
    set +e
    run_remote "$1"
    status=$?
    if [ "$status" = 76 ] && [ "$host_arg" = auto ]; then
        echo "remote-build: a hardware job took $host; picking another host" >&2
        set -e
        sync
        set +e
        run_remote "$1"
        status=$?
    fi
    set -e
    return "$status"
}

fetch() { # dir bin…
    dir="$1"
    shift
    mkdir -p "$dir"
    for bin in "$@"; do
        case "$bin" in */* | '') echo "remote-build: bad binary name '$bin'" >&2; exit 2 ;; esac
        set -- "$@" "$host:$root_name/$lane/target/release/$bin"
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
    args=""
    for a in "$@"; do args="$args '$(printf '%s' "$a" | sed "s/'/'\\\\''/g")'"; done
    sync_run "cargo$args"
    ;;
build)
    shift
    dir="$default_dir"
    if [ "${1:-}" = --to ]; then dir="$2"; shift 2; fi
    [ $# -ge 1 ] || set -- isocastd isocast isocast-ui
    bins=""
    for b in "$@"; do bins="$bins --bin $b"; done
    sync_run "cargo build --release$bins"
    fetch "$dir" "$@"
    ;;
gates)
    sync_run 'cargo fmt --check && cargo clippy --release --locked --all-targets -- -D warnings && cargo test --release --locked'
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
    sync_run "$*"
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
    sed -n '2,113p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
