#!/usr/bin/env python3
"""The hardware batch: lanes queue test-Mac jobs, a nightly runner runs them.

Lanes don't wait for benchmarks, soaks or strict e2e on the M2s any more:
they queue a job and finish their turn. The runner (cronctl, nightly on
Ryan's Mac) runs queued jobs one at a time, in order, on isocast-m2-a or
isocast-m2-b over SSH, and writes a summary per job plus one morning
summary. One job runs on a test Mac at a time: a job for one Mac (--mac)
holds /Volumes/IsocastBuild/.bench.lock.<mac>, so `now` on m2-b runs while
m2-a soaks; --mac any holds .bench.lock while it builds and picks.

    scripts/hw-batch.py submit KIND [options]    queue a job (lanes)
    scripts/hw-batch.py now KIND [options]       queue it and run it now
                                                 (the delegator; waits for
                                                 the lock)
    scripts/hw-batch.py now --detach KIND [...]  the same in a runner of its
                                                 own that outlives this
                                                 shell: long soaks (never
                                                 start one by hand, outside
                                                 the per-Mac lock)
    scripts/hw-batch.py list                     queued, running, recent
    scripts/hw-batch.py cancel ID                drop a queued job
    scripts/hw-batch.py reclaim [--dry-run]      move running/ jobs whose
                                                 runner died (and whose test
                                                 Mac job is gone) to done/
                                                 as LOST; run-queue and now
                                                 do it first by themselves
    scripts/hw-batch.py reap [--hours N] [--kill]
                                                 list (or stop, with --kill)
                                                 lane engines on the test
                                                 Macs older than N hours
                                                 (default 12) that no
                                                 running job owns
    scripts/hw-batch.py run-queue [--max-hours H]
                                                 run every queued job (the
                                                 nightly runner)
    scripts/hw-batch.py schedule [--at HH:MM] [--enable] [--script PATH]
                                                 register (or update) the
                                                 nightly cronctl job, paused
                                                 unless --enable

Kinds (all strict: the test Macs' zero-drop bar):
  e2e       scripts/e2e.py, all scenarios or --only NAME (repeatable)
  bench     scripts/bench.py A/B, head against the base build, warmed up and
            interleaved on one test Mac; --gate fails the job when head is
            worse beyond noise (#124; --args '--warmup 2' adds bench options)
            (--scenario flagship-1080, --runs 3, --seconds 20)
  smoke     the tier-1 merge smoke (about 15 min): e2e --only record, iso,
            split, pause, shutdown, then bench flagship-1080 3 x 20 s A/B
  motion    scripts/motion.sh --window [--scenario NAME] [--variant V]
            (desktop session)
  drag-lag  scripts/drag-lag.py with --args '...' (desktop session)
  soak      scripts/soak.py --hours H [--scale S] [--profile hosted|hardware]
            [--receiver local|m2-a|m2-b]: the stream goes to an ffmpeg
            receiver on the other test Mac (the runner relays it over ssh:
            the test Macs can't reach each other directly)
  stress    scripts/stress.py, the extreme stress suite (about 30 min):
            every scenario, or --scenario NAME[,NAME…]; --args '…' adds
            stress.py options (--profile quick, --keep-failures…)
  cmd       --command '<shell>' [--gui]: anything else; $HW_OUT is the
            folder whose contents come back

What to test (pick one; the default is the current HEAD, built):
  --bin-dir DIR      release binaries already built (isocastd, isocast,
                     isocast-ui); copied into the job when you submit, so
                     later builds don't change them
  --commit REV       built with scripts/remote-build.sh (a free host) when the
                     job runs; default HEAD of this checkout
The base for bench and smoke: --base-bin-dir DIR or --base-commit REV
(default: the merge base of the commit and main). Scripts (e2e.py,
bench.py...) come from --commit (or HEAD); testmac.sh and remote-build.sh
from this runner.

Other options: --mac m2-a|m2-b|any (default m2-a; any picks the first reachable
Mac whose benchmark lock is free; for any the runner holds /Volumes/IsocastBuild/
.bench.lock while it builds and picks), --out DIR (results; default
/Volumes/IsocastBuild/hw-queue/results/DATE/ID), --timeout MIN, --note TEXT,
--lane NAME (default this checkout's folder name).

Results: OUT/summary.md (status, host, commit, the result lines), OUT/log.txt
and whatever the job wrote (e2e-report.json, bench.txt and results.jsonl,
motion/, lag/, soak/). The morning summary of a run-queue is
/Volumes/IsocastBuild/hw-queue/nightly/DATE.md (and latest.md). Exit
status: 0 if every job run passed, 1 otherwise.
"""

import argparse
import contextlib
import fcntl
import json
import os
import random
import re
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
RUNNER_REPO = HERE.parent
BUILD = Path(os.environ.get("ISOCAST_HW_ROOT", "/Volumes/IsocastBuild"))
ROOT = BUILD / "hw-queue"
LOCK = BUILD / ".bench.lock"
MACS = {"m2-a": "isocast-m2-a", "m2-b": "isocast-m2-b"}
KINDS = ("e2e", "bench", "smoke", "motion", "drag-lag", "soak", "stress", "cmd")
SMOKE_E2E = ("record", "iso", "split", "pause", "shutdown")
# Remote checkout name: testmac.sh puts it in ~/dev/isocast-worktrees/hw-batch.
REMOTE_NAME = "hw-batch"
TIMEOUT_MIN = {"e2e": 45, "bench": 30, "smoke": 30, "motion": 45, "drag-lag": 30, "stress": 120, "cmd": 60}
MEASURES = ("bench", "smoke", "motion", "drag-lag")
ID_RE = re.compile(r"^[0-9]{8}-[0-9]{6}-[A-Za-z0-9._-]+$")
# A running/ entry younger than this is never reclaimed: its runner may be
# between moving it there and recording its pid.
RECLAIM_GRACE_S = 120


def interrupted(signum, _frame):
    raise KeyboardInterrupt(f"signal {signum}")


def log(msg):
    print(f"hw-batch {time.strftime('%H:%M:%S')}: {msg}", file=sys.stderr, flush=True)


def this_host():
    """This Mac's name as runners record it: stable across network changes
    (socket.gethostname() turns into an IP-derived name on some networks,
    which made entries unreclaimable, #100)."""
    try:
        name = subprocess.run(["scutil", "--get", "LocalHostName"], capture_output=True, text=True,
                              timeout=10).stdout.strip()
    except (OSError, subprocess.TimeoutExpired):
        name = ""
    return host_key(name or socket.gethostname())


def host_key(name):
    """A host name compared without its domain or case (entries from older
    runners carry socket.gethostname())."""
    return (name or "").split(".")[0].lower()


def mac_lock(mac):
    """One job per test Mac: a job for one Mac waits only for that Mac."""
    return BUILD / f".bench.lock.{mac}"


def lock_held(path):
    """Whether another process holds `path` (a moment's try, never kept)."""
    try:
        with open(path, "a") as f:
            fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
            fcntl.flock(f, fcntl.LOCK_UN)
        return False
    except BlockingIOError:
        return True
    except OSError:
        return False


def hold(locks, path, job_id):
    f = locks.enter_context(open(path, "a"))
    log(f"{job_id}: waiting for {path}")
    fcntl.flock(f, fcntl.LOCK_EX)


def git(repo, *args):
    return subprocess.run(["git", "-C", str(repo), *args], check=True, capture_output=True,
                          text=True).stdout.strip()


def dirs():
    for d in ("queue", "running", "done", "jobs", "results", "nightly", "src", "runners"):
        (ROOT / d).mkdir(parents=True, exist_ok=True)


# ---------- submitting ----------

def clone_bins(src, dest, names):
    src = Path(src).expanduser().resolve()
    if not (src / "isocastd").is_file():
        sys.exit(f"hw-batch: no isocastd in {src}")
    dest.mkdir(parents=True)
    for name in names:
        if (src / name).is_file():
            # APFS clone: instant, and later builds into src don't touch it.
            subprocess.run(["cp", "-c", "-p", str(src / name), str(dest / name)], check=True)


def worktree_snapshot(repo):
    """This checkout's files as they are (tracked and untracked, not
    ignored; uncommitted edits and an unfinished merge included) as a commit
    no branch points at, so a job's scripts are the ones in the worktree."""
    with tempfile.TemporaryDirectory() as tmp:
        env = dict(os.environ, GIT_INDEX_FILE=str(Path(tmp) / "index"))
        run = lambda *a: subprocess.run(["git", "-C", str(repo), *a], check=True, capture_output=True,  # noqa: E731
                                        text=True, env=env).stdout.strip()
        run("read-tree", "HEAD")
        run("add", "-A")
        tree = run("write-tree")
        return run("commit-tree", tree, "-p", "HEAD", "-m", "hw-batch: worktree snapshot for a job's scripts")


def submit(args):
    dirs()
    repo = Path(git(Path.cwd(), "rev-parse", "--show-toplevel"))
    lane = args.lane or (repo.name if repo.name != "isocast" else "main")
    lane = re.sub(r"[^A-Za-z0-9._-]", "-", lane)
    job_id = f"{time.strftime('%Y%m%d-%H%M%S')}-{lane}-{args.kind}"
    while (ROOT / "queue" / f"{job_id}.json").exists() or (ROOT / "jobs" / job_id).exists():
        time.sleep(1)
        job_id = f"{time.strftime('%Y%m%d-%H%M%S')}-{lane}-{args.kind}"
    if args.scripts == "worktree":
        if args.commit or not args.bin_dir:
            sys.exit("hw-batch: --scripts worktree takes the binaries from --bin-dir (not --commit)")
        commit = worktree_snapshot(repo)
    else:
        commit = git(repo, "rev-parse", args.commit or "HEAD")
    job = {
        "id": job_id, "kind": args.kind, "lane": lane, "repo": str(repo),
        "branch": git(repo, "rev-parse", "--abbrev-ref", "HEAD"), "commit": commit,
        "mac": args.mac, "note": args.note or "", "submitted": time.strftime("%Y-%m-%d %H:%M"),
        "timeout_min": args.timeout or TIMEOUT_MIN.get(args.kind) or 60,
        "out": str(Path(args.out).expanduser().resolve()) if args.out else
        (str(ROOT / "results" / time.strftime("%Y-%m-%d") / job_id) if getattr(args, "detach", False) else None),
        "only": args.only or [], "scenario": args.scenario, "variant": args.variant,
        "runs": args.runs, "seconds": args.seconds, "hours": args.hours, "scale": args.scale,
        "args": args.args or "", "command": args.command or "", "gui": args.gui,
        "scripts": (f"worktree snapshot {commit[:12]} of {git(repo, 'rev-parse', 'HEAD')[:12]} + uncommitted files"
                    if args.scripts == "worktree" else f"commit {commit[:12]}"),
    }
    if args.kind == "soak":
        hours = args.hours or 1.0
        job["hours"] = hours
        job["profile"] = args.profile
        job["receiver"] = None if args.receiver in (None, "local") else args.receiver
        if job["receiver"] and args.mac == job["receiver"]:
            sys.exit("hw-batch: --receiver must be the other test Mac")
        if job["receiver"] and args.mac == "any":
            job["mac"] = next(m for m in MACS if m != job["receiver"])
        # The hardware profile adds its crash tests and a slower last decode.
        extra = 45 if args.profile == "hardware" else 30
        job["timeout_min"] = args.timeout or int(hours * 60 * (args.scale or 1.0)) + extra
    elif args.profile or args.receiver:
        sys.exit("hw-batch: --profile and --receiver are for soak")
    if args.kind == "cmd" and not args.command:
        sys.exit("hw-batch: cmd needs --command")
    for d in (args.bin_dir, args.base_bin_dir):
        if d and not (Path(d).expanduser() / "isocastd").is_file():
            sys.exit(f"hw-batch: no isocastd in {d}")
    jobdir = ROOT / "jobs" / job_id
    if args.bin_dir:
        clone_bins(args.bin_dir, jobdir / "bin", ("isocastd", "isocast", "isocast-ui"))
        job["bin_dir"] = str(jobdir / "bin")
        job["bin_from"] = str(Path(args.bin_dir).expanduser().resolve())
        if git(repo, "status", "--porcelain", "--untracked-files=no"):
            log("note: uncommitted changes; the scripts come from the commit, the binaries from --bin-dir")
    if args.kind in ("bench", "smoke"):
        if args.base_bin_dir:
            clone_bins(args.base_bin_dir, jobdir / "base-bin", ("isocastd", "isocast"))
            job["base_bin_dir"] = str(jobdir / "base-bin")
        else:
            job["base_commit"] = git(repo, "rev-parse", args.base_commit) if args.base_commit else \
                git(repo, "merge-base", commit, "main")
    jobdir.mkdir(parents=True, exist_ok=True)
    tmp = ROOT / "queue" / f".{job_id}.json"
    tmp.write_text(json.dumps(job, indent=2) + "\n")
    tmp.rename(ROOT / "queue" / f"{job_id}.json")
    print(job_id)
    return job_id


# ---------- running ----------

def ssh(host, command, timeout=60):
    return subprocess.run(["ssh", "-o", "ConnectTimeout=15", "-o", "BatchMode=yes", host, command],
                          capture_output=True, text=True, timeout=timeout)


def stop_remote(host, job_id):
    ssh(host, 'k() { for c in $(pgrep -P "$1"); do k "$c"; done; kill -TERM "$1" 2>/dev/null; }; '
              f'p="$(cat "$HOME/isocast-scratch/{REMOTE_NAME}/{job_id}/pid" 2>/dev/null)"; [ -n "$p" ] && k "$p"')


def pick_mac(want):
    hosts = [MACS[want]] if want in MACS else list(MACS.values())
    reachable = []
    for host in hosts:
        if want not in MACS and lock_held(mac_lock(next(m for m, h in MACS.items() if h == host))):
            continue  # another runner's job holds it
        try:
            r = ssh(host, "mkdir -p isocast-scratch && lockf -s -k -t 0 isocast-scratch/.bench.lock true "
                          "&& echo free || echo busy",
                    timeout=40)
        except subprocess.TimeoutExpired:
            continue
        if r.returncode == 0:
            if r.stdout.strip() == "free":
                return host
            reachable.append(host)
    return reachable[0] if reachable else None


# The remote builds on a host (scripts/remote-build.sh's run.pgid files):
# `count` prints how many, `kill` also stops them. A pgid counts only while
# its group still runs that lane's remote-build command (a stale file and a
# reused pid never do); stale files are deleted.
BUILDS_SH = r"""
n=0
for f in "$HOME"/isocast-builds/*/run.pgid; do
    [ -f "$f" ] || continue
    lane="$(basename "$(dirname "$f")")"
    g="$(cat "$f")"
    case "$g" in '' | *[!0-9]*) rm -f "$f"; continue ;; esac
    if ps -ax -o pgid=,command= | awk -v g="$g" -v w="__run $lane" '$1 == g && index($0, ".remote-build") && index($0 " ", " " w " ") { f = 1 } END { exit !f }'; then
        [ "$1" = kill ] && kill -TERM -- "-$g" 2>/dev/null
        n=$((n + 1))
    else
        rm -f "$f"
    fi
done
echo "$n"
"""


def reserve_builds(hosts, job_id, minutes, logf):
    """Keep scripts/remote-build.sh off the job's test Macs: mark them
    (~/isocast-builds/.hw-job, honored until it expires), then wait up to 20
    min for builds already running there and stop any still going."""
    expiry = int(time.time()) + (minutes + 60) * 60
    live = "sh -c " + shlex.quote(BUILDS_SH) + " builds count"
    for host in hosts:
        ssh(host, f'mkdir -p isocast-builds && echo "{job_id} {expiry}" >isocast-builds/.hw-job')
    for host in hosts:
        for _ in range(40):
            r = ssh(host, live)
            if r.returncode != 0 or r.stdout.strip() in ("", "0"):
                break
            log(f"{job_id}: {r.stdout.strip()} remote build(s) running on {host}; waiting")
            time.sleep(30)
        else:
            log(f"{job_id}: stopping remote builds still running on {host}")
            print(f"stopped remote builds on {host}", file=logf, flush=True)
            ssh(host, "sh -c " + shlex.quote(BUILDS_SH) + " builds kill")


def release_builds(hosts, job_id):
    for host in hosts:
        try:
            ssh(host, f'f=isocast-builds/.hw-job; grep -q "^{job_id} " "$f" 2>/dev/null && rm -f "$f"; true')
        except subprocess.TimeoutExpired:
            log(f"{job_id}: couldn't clear the build reservation on {host} (it expires by itself)")


def export_src(commit, dest):
    """The commit's source as a git index (testmac.sh and remote-build.sh sync
    `git ls-files`), with this runner's own testmac.sh and remote-build.sh."""
    if dest.exists():
        shutil.rmtree(dest)
    dest.mkdir(parents=True)
    archive = subprocess.Popen(["git", "-C", str(RUNNER_REPO), "archive", commit], stdout=subprocess.PIPE)
    subprocess.run(["tar", "-x", "-C", str(dest)], stdin=archive.stdout, check=True)
    if archive.wait() != 0:
        raise RuntimeError(f"git archive {commit} failed")
    for name in ("testmac.sh", "testmac-bridge.sh", "remote-build.sh"):
        shutil.copy2(HERE / name, dest / "scripts" / name)
    subprocess.run(["git", "init", "-q"], cwd=dest, check=True)
    subprocess.run(["git", "add", "-A"], cwd=dest, check=True)


def build_remote(src, lane, bins, dest, out):
    # remote-build.sh picks a free build host: not this job's test Macs
    # (the batch lock and running/*.json say which).
    log(f"building {' '.join(bins)} with scripts/remote-build.sh (lane {lane})")
    # This runner holds the batch lock while it builds, before any Mac is
    # picked; without ISOCAST_HW_BUILDING remote-build.sh would count both
    # test Macs busy and find no build host.
    env = dict(os.environ, ISOCAST_LANE=lane, ISOCAST_HW_BUILDING="1")
    with open(out / "build.log", "a") as f:
        r = subprocess.run(["sh", "scripts/remote-build.sh", "build", "--to", str(dest), *bins],
                           cwd=src, env=env, stdout=f, stderr=subprocess.STDOUT, timeout=3500)
    if r.returncode != 0:
        raise RuntimeError(f"remote build failed (exit {r.returncode}); see build.log")


def job_command(job, r, relay=None):
    """The shell command run in the test Mac's checkout; r is its results
    folder ($HW_OUT's parent); relay the ssh config for --receiver."""
    kind = job["kind"]
    out = f"{r}/out"
    e2e = (f"python3 scripts/e2e.py --daemon target/release/isocastd --work {r} "
           f"--report {out}/e2e-report.json")
    # The gate (#124) only where the job's own bench.py has it: a job queued
    # on an older commit measures as before and says it wasn't gated.
    bench = (f"G=; grep -q -- '\"--gate\"' scripts/bench.py && G=--gate; "
             f"python3 scripts/bench.py --work {r}/bench --daemon target/release/isocastd "
             f"--cli target/release/isocast --compare target/base/isocastd "
             f"--scenario {job.get('scenario') or 'flagship-1080'} --runs {job.get('runs') or 3} "
             f"--seconds {job.get('seconds') or 20} $G{' ' + job['args'] if job.get('args') and kind == 'bench' else ''}"
             f" >{out}/bench.txt 2>&1; s=$?; "
             f"[ -z \"$G\" ] && echo 'bench: NOT GATED: the bench.py of this job predates --gate (#124)' >>{out}/bench.txt; "
             f"cat {out}/bench.txt; cp {r}/bench/results.jsonl {r}/bench/gate.json {out}/ 2>/dev/null; exit $s")
    if kind == "e2e":
        return e2e + "".join(f" --only {o}" for o in job["only"]) + (f" {job['args']}" if job.get("args") else "")
    if kind == "bench":
        return f"({bench})"
    if kind == "smoke":
        only = "".join(f" --only {o}" for o in SMOKE_E2E)
        return f"{e2e}{only}; e=$?; ({bench}); b=$?; echo \"smoke: e2e exit $e, bench exit $b\"; [ $e = 0 ] && [ $b = 0 ]"
    if kind == "soak":
        scale = f" --scale {job['scale']}" if job.get("scale") else ""
        scale += f" --profile {job['profile']}" if job.get("profile") else ""
        scale += f" --receiver {Relay.ALIAS} --receiver-ssh-config {relay}" if relay else ""
        scale += f" {job['args']}" if job.get("args") else ""
        return (f"python3 scripts/soak.py --hours {job['hours']}{scale} --daemon target/release/isocastd "
                f"--work {r}/soak --out {out}/soak")
    if kind == "motion":
        extra = f" --scenario {job['scenario']}" if job.get("scenario") else ""
        extra += f" --variant {job['variant']}" if job.get("variant") else ""
        return f"scripts/motion.sh --window --bin target/release --out {out}/motion{extra}"
    if kind == "stress":
        names = [n for n in (job.get("scenario") or "").split(",") if n] + job["only"]
        return (f"python3 scripts/stress.py --daemon target/release/isocastd --work {r}/w "
                f"--out {out}/stress" + "".join(f" --scenario {n}" for n in names)
                + (f" {job['args']}" if job.get("args") else ""))
    if kind == "drag-lag":
        return f"python3 scripts/drag-lag.py --bin-dir target/release --work {out}/lag {job['args']}"
    return job["command"]


class Relay:
    """An ssh path from the job's test Mac to the receiver Mac for a soak's
    --receiver. The test Macs' security group lets ssh in only from Ryan's
    IP, so the runner forwards a port on the job's Mac to the receiver's
    sshd (`ssh -R`), and a throwaway key for this job, allowed on the
    receiver as `restrict,port-forwarding`, logs in through it. soak.py then
    starts its receiver there and tunnels the stream with `ssh -L`. stop()
    removes the key and checks nothing of ours still listens there."""
    ALIAS = "isocast-receiver"

    def __init__(self, host, receiver, job_id, jobdir, logf):
        self.host, self.receiver, self.job_id, self.logf = host, receiver, job_id, logf
        self.dir = jobdir / "relay"
        self.tag = f"isocast-hw-batch-{job_id}"
        self.forward = None

    def keys(self, add=None):
        """Drop every hw-batch key on the receiver (stale ones from a killed
        runner too) and add `add`; refuses to write a file that lost any
        other key."""
        script = ('set -e; cd ~/.ssh; touch authorized_keys; n=$(grep -vc isocast-hw-batch- authorized_keys || true); '
                  'grep -v isocast-hw-batch- authorized_keys >ak.hw-batch || true; cat >>ak.hw-batch; '
                  '[ "$(grep -vc isocast-hw-batch- ak.hw-batch || true)" = "$n" ] || { rm -f ak.hw-batch; exit 3; }; '
                  'chmod 600 ak.hw-batch; mv ak.hw-batch authorized_keys')
        r = subprocess.run(["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15", self.receiver, script],
                           input=add or "", capture_output=True, text=True, timeout=60)
        if r.returncode:
            raise RuntimeError(f"updating authorized_keys on {self.receiver} failed: {r.stderr[-300:]}")

    def start(self):
        if self.receiver == self.host:
            raise RuntimeError("the receiver must be the other test Mac")
        cfg = dict(l.split(" ", 1) for l in subprocess.run(["ssh", "-G", self.receiver], capture_output=True,
                                                             text=True, check=True).stdout.splitlines() if " " in l)
        hostname, user = cfg["hostname"], cfg["user"]
        known = subprocess.run(["ssh-keygen", "-F", hostname], capture_output=True, text=True).stdout
        known = "".join(f"{self.ALIAS} {l.split(' ', 1)[1]}\n" for l in known.splitlines()
                        if l and not l.startswith("#"))
        if not known:
            raise RuntimeError(f"no host key for {self.receiver} ({hostname}) in known_hosts")
        self.dir.mkdir(parents=True, exist_ok=True)
        key = self.dir / "key"
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", self.tag, "-f", str(key)],
                       check=True)
        self.keys(f"restrict,port-forwarding {(self.dir / 'key.pub').read_text().strip()}\n")
        home = ssh(self.host, "echo $HOME").stdout.strip()
        remote = f"{home}/isocast-scratch/{REMOTE_NAME}/{self.job_id}/relay"
        for _ in range(3):
            port = random.randrange(20000, 40000)
            self.forward = subprocess.Popen(
                ["ssh", "-N", "-o", "BatchMode=yes", "-o", "ExitOnForwardFailure=yes", "-o", "ServerAliveInterval=30",
                 "-o", "ServerAliveCountMax=4", "-R", f"127.0.0.1:{port}:{hostname}:22", self.host],
                stdin=subprocess.DEVNULL, stdout=self.logf, stderr=self.logf, start_new_session=True)
            time.sleep(5)
            if self.forward.poll() is None:
                break
        else:
            raise RuntimeError(f"could not forward a port on {self.host} to {self.receiver}")
        config = (f"Host {self.ALIAS}\n  HostName 127.0.0.1\n  Port {port}\n  User {user}\n"
                  f"  IdentityFile {remote}/key\n  IdentitiesOnly yes\n  HostKeyAlias {self.ALIAS}\n"
                  f"  UserKnownHostsFile {remote}/known_hosts\n  StrictHostKeyChecking yes\n")
        files = {"key": key.read_text(), "known_hosts": known, "config": config}
        for name, text in files.items():
            r = subprocess.run(["ssh", "-o", "BatchMode=yes", self.host,
                                f"umask 077; mkdir -p '{remote}' && cat >'{remote}/{name}'"],
                               input=text, capture_output=True, text=True, timeout=60)
            if r.returncode:
                raise RuntimeError(f"copying the relay {name} to {self.host} failed: {r.stderr[-300:]}")
        check = ssh(self.host, f"ssh -F '{remote}/config' -o BatchMode=yes {self.ALIAS} hostname")
        if check.returncode:
            raise RuntimeError(f"{self.host} can't reach {self.receiver} through the relay: {check.stderr[-300:]}")
        log(f"relay: {self.host} -> {self.receiver} ({check.stdout.strip()}) through 127.0.0.1:{port}")
        return f"{remote}/config"

    def stop(self):
        """Returns a warning if anything was left on the receiver."""
        warning = None
        if self.forward and self.forward.poll() is None:
            self.forward.terminate()
            try:
                self.forward.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.forward.kill()
        try:
            self.keys()
        except (RuntimeError, subprocess.TimeoutExpired) as e:
            warning = f"relay key not removed from {self.receiver}: {e}"
        # soak.py cleans up after itself; this is the backstop.
        r = ssh(self.receiver, "pgrep -fl 'listen 1 -i rtmp://127.0.0.1:' || true")
        if r.stdout.strip():
            ssh(self.receiver, "pkill -f 'listen 1 -i rtmp://127.0.0.1:' || true")
            warning = f"a receiver was left on {self.receiver}; killed: {r.stdout.strip()[:200]}"
        if warning:
            log(warning)
        return warning


def summarize(job, out, status, seconds, host):
    lines = [f"# {job['id']}: {status}", "",
             f"- kind: {job['kind']}  lane: {job['lane']}  branch: {job['branch']}",
             f"- commit: {job['commit'][:12]}" + (f"  scripts: {job['scripts']}" if job.get("scripts") else "") + (f"  base: {job['base_commit'][:12]}" if job.get("base_commit") else ""),
             f"- binaries: {('copied from ' + job['bin_from']) if job.get('bin_from') else 'built from the commit (scripts/remote-build.sh)'}",
             f"- host: {host or '-'}  took: {seconds / 60:.1f} min  exit: {job.get('exit')}  "
             f"finished: {time.strftime('%Y-%m-%d %H:%M')}"]
    if job.get("note"):
        lines.append(f"- note: {job['note']}")
    if job.get("warning"):
        lines.append(f"- warning: {job['warning']}")
    report = out / "e2e-report.json"
    if report.exists():
        try:
            data = json.loads(report.read_text())
            lines += ["", "| e2e scenario | status | s | reason |", "|---|---|---|---|"]
            for s in data.get("scenarios", []):
                reason = str(s.get("reason") or "").replace("|", "/")[:120]
                lines.append(f"| {s.get('name')} | {s.get('status')} | {s.get('seconds', 0):.0f} | {reason} |")
        except (ValueError, KeyError) as e:
            lines.append(f"\n(e2e report unreadable: {e})")
    stress = out / "stress" / "summary.md"
    if stress.exists():
        text = stress.read_text(errors="replace").splitlines()
        lines += ["", *[l for l in text if l.startswith("|")],
                  "", *[l for l in text if l.startswith("- PROBLEM")][:20]]
    bench = out / "bench.txt"
    if bench.exists():
        table = [l for l in bench.read_text().splitlines() if l.startswith("|")]
        if table:
            lines += ["", *table]
    logfile = out / "log.txt"
    if logfile.exists():
        tail = logfile.read_text(errors="replace").splitlines()[-25:]
        lines += ["", "Last lines of log.txt:", "", "```", *tail, "```"]
    (out / "summary.md").write_text("\n".join(lines) + "\n")
    return lines[0]


def run_job(path):
    # Taken first, then read: a detached runner or `cancel` may take the
    # file between a read and the rename (#100).
    running = ROOT / "running" / path.name
    try:
        path.rename(running)
    except FileNotFoundError:
        log(f"{path.stem}: no longer queued (another runner took it, or it was cancelled)")
        return None
    job = json.loads(running.read_text())
    job_id = job["id"]
    assert ID_RE.match(job_id), job_id
    # Who runs it, so `reclaim` can tell a live runner from a dead one.
    job["runner"] = {"pid": os.getpid(), "host": this_host(), "started": time.time()}
    running.write_text(json.dumps(job, indent=2) + "\n")
    out = Path(job["out"]) if job.get("out") else ROOT / "results" / time.strftime("%Y-%m-%d") / job_id
    out.mkdir(parents=True, exist_ok=True)
    job["out"] = str(out)
    jobdir = ROOT / "jobs" / job_id
    start = time.time()
    host = None
    status = "FAIL"
    logf = open(out / "log.txt", "a")
    proc = None
    relay = None
    reserved = []
    try:
        with contextlib.ExitStack() as locks:
            # A job for one Mac (and its soak receiver) holds those Macs'
            # locks, so a job on m2-b runs while m2-a soaks. --mac any holds
            # the batch lock while it builds and picks, then the Mac's lock.
            pinned = sorted({m for m in (job.get("mac"), job.get("receiver")) if m in MACS})
            batch = None
            for mac in pinned:
                hold(locks, mac_lock(mac), job_id)
            if not pinned:
                batch = locks.enter_context(open(LOCK, "a"))
                log(f"{job_id}: waiting for {LOCK}")
                fcntl.flock(batch, fcntl.LOCK_EX)
            log(f"{job_id}: running")
            # Per job (two Macs can run at once), named as before: testmac.sh
            # names the test Mac's checkout and scratch after this folder.
            src = ROOT / "src" / job_id / REMOTE_NAME
            export_src(job["commit"], src)
            bin_dir = Path(job["bin_dir"]) if job.get("bin_dir") else jobdir / "bin"
            if not job.get("bin_dir"):
                build_remote(src, "hw-batch", ["isocastd", "isocast", "isocast-ui"], bin_dir, out)
            base_dir = None
            if job["kind"] in ("bench", "smoke"):
                base_dir = Path(job["base_bin_dir"]) if job.get("base_bin_dir") else jobdir / "base-bin"
                if not job.get("base_bin_dir"):
                    base_src = ROOT / "src" / job_id / "hw-batch-base"
                    export_src(job["base_commit"], base_src)
                    build_remote(base_src, "hw-batch-base", ["isocastd"], base_dir, out)
            host = pick_mac(job.get("mac") or "any")
            if not host:
                raise RuntimeError("no test Mac reachable")
            job["host"] = host
            if not pinned:
                hold(locks, mac_lock(next(m for m, h in MACS.items() if h == host)), job_id)
                # Only while it builds and picks (#100): the Mac's lock
                # covers the job from here.
                fcntl.flock(batch, fcntl.LOCK_UN)
            running.write_text(json.dumps(job, indent=2) + "\n")
            # Only the job's own Mac: a soak's stream receiver just copies
            # packets to disk, so builds (nice 15) may share it.
            reserved = [host]
            reserve_builds(reserved, job_id, job["timeout_min"], logf)
            log(f"{job_id}: pushing to {host}")
            env = dict(os.environ, ISOCAST_TESTMAC=host)
            subprocess.run(["sh", "scripts/testmac.sh", "push", str(bin_dir)], cwd=src, env=env,
                           stdout=logf, stderr=subprocess.STDOUT, check=True, timeout=900)
            remote_dir = f"dev/isocast-worktrees/{REMOTE_NAME}"
            if base_dir:
                ssh(host, f"mkdir -p {remote_dir}/target/base")
                subprocess.run(["rsync", "-az", "-e", "ssh", str(base_dir / "isocastd"),
                                f"{host}:{remote_dir}/target/base/"], check=True, timeout=600)
            if job["kind"] in MEASURES:
                for _ in range(20):
                    if ssh(host, "pgrep -x screensharingd").returncode != 0:
                        break
                    log(f"{job_id}: screen sharing open on {host}; waiting")
                    time.sleep(30)
                else:
                    job["warning"] = "a screen sharing session was open: numbers may be noisy"
            r = f'"$ISOCAST_SCRATCH/{REMOTE_NAME}/{job_id}"'
            relay_config = None
            if job.get("receiver"):
                relay = Relay(host, MACS[job["receiver"]], job_id, jobdir, logf)
                relay_config = relay.start()
                # For `reclaim`, if this runner dies (#100).
                job["relay"] = {"pid": relay.forward.pid, "receiver": relay.receiver}
                running.write_text(json.dumps(job, indent=2) + "\n")
            # $R/pid lets a timeout stop the job's whole process tree (in the
            # desktop session too, where the bridge's process group is shared).
            command = (f"mkdir -p {r}/out && echo $$ >{r}/pid && export HW_OUT={r}/out && "
                       f"{{ {job_command(job, r, relay_config)}\n}}")
            gui = job["kind"] in ("motion", "drag-lag") or job.get("gui")
            log(f"{job_id}: {job['kind']} on {host} ({'desktop session' if gui else 'ssh'}), "
                f"timeout {job['timeout_min']} min")
            proc = subprocess.Popen(["sh", "scripts/testmac.sh", "gui" if gui else "run", command],
                                    cwd=src, env=env, stdout=logf, stderr=subprocess.STDOUT,
                                    start_new_session=True)
            try:
                code = proc.wait(timeout=job["timeout_min"] * 60)
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid, 15)
                stop_remote(host, job_id)
                code = None
                job["warning"] = f"timed out after {job['timeout_min']} min; stopped"
            proc = None
            if relay:
                job["warning"] = relay.stop() or job.get("warning")
                relay = None
            subprocess.run(["rsync", "-a", "-e", "ssh", f"{host}:isocast-scratch/{REMOTE_NAME}/{job_id}/out/",
                            f"{out}/"], stdout=logf, stderr=subprocess.STDOUT, timeout=1800)
            ssh(host, f"rm -rf \"$HOME/isocast-scratch/{REMOTE_NAME}/{job_id}\"")
            job["exit"] = code
            status = "PASS" if code == 0 else "FAIL"
    except KeyboardInterrupt:
        job["warning"] = "interrupted"
        raise
    except Exception as e:  # noqa: BLE001 - every failure ends up in the summary
        job["warning"] = f"runner error: {e}"
        log(f"{job_id}: {e}")
    finally:
        # A second TERM (or HUP) must not cut the cleanup short (#100).
        handlers = {sig: signal.signal(sig, signal.SIG_IGN) for sig in (signal.SIGTERM, signal.SIGHUP)}
        if proc is not None:
            os.killpg(proc.pid, 15)
            if host:
                stop_remote(host, job_id)
        if relay:
            relay.stop()
        release_builds(reserved, job_id)
        logf.close()
        seconds = time.time() - start
        job["status"] = status
        job["seconds"] = round(seconds)
        headline = summarize(job, out, status, seconds, host)
        done = ROOT / "done" / path.name
        done.write_text(json.dumps(job, indent=2) + "\n")
        running.unlink(missing_ok=True)
        if jobdir.exists():
            shutil.rmtree(jobdir)
        shutil.rmtree(ROOT / "src" / job_id, ignore_errors=True)
        log(f"{headline} -> {out}/summary.md")
        for sig, handler in handlers.items():
            signal.signal(sig, handler)
    return job


def run_queue(args):
    dirs()
    deadline = time.time() + args.max_hours * 3600
    ran = []
    while time.time() < deadline:
        # Submission order (ids share a second when a lane queues several).
        queued = sorted((ROOT / "queue").glob("*.json"), key=lambda p: (p.stat().st_mtime_ns, p.name))
        if not queued:
            break
        reclaim()
        job = run_job(queued[0])
        if job:
            ran.append(job)
    left = sorted(p.stem for p in (ROOT / "queue").glob("*.json"))
    date = time.strftime("%Y-%m-%d")
    lines = [f"# Hardware batch {date}", "",
             f"{len(ran)} job(s) run, {sum(j['status'] == 'PASS' for j in ran)} passed; {len(left)} left in the queue.", ""]
    if ran:
        lines += ["| job | kind | lane | host | status | min | summary |", "|---|---|---|---|---|---|---|"]
        for j in ran:
            lines.append(f"| {j['id']} | {j['kind']} | {j['lane']} | {j.get('host') or '-'} | {j['status']}"
                         f"{' (' + j['warning'] + ')' if j.get('warning') else ''} | {j['seconds'] / 60:.0f} | "
                         f"{j['out']}/summary.md |")
    if left:
        lines += ["", "Still queued (time ran out): " + ", ".join(left)]
    if ran or left:
        text = "\n".join(lines) + "\n"
        night = ROOT / "nightly" / f"{date}.md"
        with open(night, "a") as f:
            f.write(text)
        (ROOT / "nightly" / "latest.md").write_text(night.read_text())
        print(text)
    else:
        print("hw-batch: queue empty")
    return 0 if all(j["status"] == "PASS" for j in ran) else 1


def list_jobs(_args):
    dirs()
    for state in ("running", "queue"):
        for p in sorted((ROOT / state).glob("*.json")):
            try:
                j = json.loads(p.read_text())
            except ValueError:
                continue  # being rewritten
            gone = state == "running" and time.time() - p.stat().st_mtime >= RECLAIM_GRACE_S \
                and runner_alive(j, p) is False
            print(f"{state:8} {j['id']}  {j.get('note', '')}"
                  + ("  (runner gone: `hw-batch.py reclaim`)" if gone else ""))
    for p in sorted((ROOT / "done").glob("*.json"))[-10:]:
        j = json.loads(p.read_text())
        print(f"done     {j['id']}  {j.get('status')}  {j.get('out')}/summary.md")
    return 0


def cancel(args):
    if not ID_RE.match(args.id):
        sys.exit("hw-batch: bad job id")
    p = ROOT / "queue" / f"{args.id}.json"
    if not p.exists():
        sys.exit(f"hw-batch: {args.id} is not queued")
    p.unlink()
    jobdir = ROOT / "jobs" / args.id
    if jobdir.exists():
        shutil.rmtree(jobdir)
    print(f"cancelled {args.id}")
    return 0


# ---------- reclaiming and reaping ----------

def runner_alive(job, entry):
    """True if the job's runner still runs, False if it's gone, None if this
    Mac can't tell (another host's runner)."""
    runner = job.get("runner")
    if runner:
        if host_key(runner.get("host")) not in (this_host(), host_key(socket.gethostname())):
            return None
        r = subprocess.run(["ps", "-o", "command=", "-p", str(runner["pid"])], capture_output=True, text=True)
        return r.returncode == 0 and "hw-batch" in r.stdout
    # Runners from before runner pids were recorded: the runner keeps the
    # job's log.txt open from the moment it takes the job to the end.
    logs = [Path(job["out"]) / "log.txt"] if job.get("out") else []
    logs += list((ROOT / "results").glob(f"*/{job['id']}/log.txt"))
    logs = [str(l) for l in logs if l.exists()]
    if not logs:
        return False
    r = subprocess.run(["lsof", "-t", *logs], capture_output=True, text=True)
    if r.stdout.strip():
        return True
    if r.returncode not in (0, 1) or r.stderr.strip():
        return None  # lsof couldn't tell: never call it dead
    # A false "dead" would delete a live job's binaries, so an old runner
    # also has to be past its job's timeout before it counts as gone.
    age = time.time() - entry.stat().st_mtime
    limit = 60 * float(job.get("timeout_min") or 360) + 1800
    return False if age > limit else None


def remote_job_alive(host, job_id):
    """True if the job's command still runs on its test Mac, False if not,
    None if the Mac can't be reached."""
    # The pid file is written by the job's shell once it runs, so a process
    # younger than the file is another one that got the pid (after a
    # reboot, #100).
    f = f'"$HOME/isocast-scratch/{REMOTE_NAME}/{job_id}/pid"'
    try:
        r = ssh(host, f'p="$(cat {f} 2>/dev/null)"; [ -n "$p" ] && kill -0 "$p" 2>/dev/null || {{ echo gone; exit 0; }}; '
                      f'echo "live $(ps -o etime= -p "$p" | tr -d " ") $(( $(date +%s) - $(stat -f %m {f}) ))"',
                timeout=40)
    except subprocess.TimeoutExpired:
        return None
    if r.returncode != 0:
        return None
    words = r.stdout.split()
    if not words or words[0] != "live":
        return False
    if len(words) != 3:
        return True  # it ended just now, or can't tell: never call a live job dead
    try:
        return etime_seconds(words[1]) + 5 >= int(words[2])
    except (IndexError, ValueError):
        return True  # can't tell: never call a live job dead


def reclaim(dry_run=False):
    """Move running/ entries whose runner died to done/ as LOST, with a
    summary saying so. Not while the job's command still runs on its test
    Mac (that is reported: stop it, then reclaim)."""
    dirs()
    reclaimed = []
    for entry in sorted((ROOT / "running").glob("*.json")):
        try:
            job = json.loads(entry.read_text())
            age = time.time() - entry.stat().st_mtime
        except (OSError, ValueError):
            continue  # gone, or being rewritten by its runner
        job_id = job.get("id", entry.stem)
        if not ID_RE.match(job_id) or age < RECLAIM_GRACE_S:
            continue
        alive = runner_alive(job, entry)
        if alive is not False:
            continue
        host = job.get("host")
        remote = remote_job_alive(host, job_id) if host else False
        if remote is None:
            log(f"{job_id}: its runner is gone, but {host} can't be reached to check the job there; left in running/")
            continue
        if remote:
            log(f"{job_id}: its runner is gone but the job still runs on {host}; left in running/ "
                f"(it holds no lock now: stop it there, then run `hw-batch.py reclaim`)")
            continue
        runner = job.get("runner") or {}
        why = (f"runner pid {runner['pid']} on {runner.get('host')} is gone" if runner.get("pid")
               else "no process holds its log.txt (a runner from before runner pids were recorded)")
        why += f"; no job on {host}" if host else "; it had no test Mac yet"
        if dry_run:
            print(f"would reclaim {job_id}: {why}")
            reclaimed.append(job_id)
            continue
        done = ROOT / "done" / entry.name
        try:
            entry.rename(done)  # atomic: only one reclaimer (or runner) wins
        except FileNotFoundError:
            continue
        out = Path(job["out"]) if job.get("out") else ROOT / "results" / time.strftime("%Y-%m-%d") / job_id
        out.mkdir(parents=True, exist_ok=True)
        job["out"] = str(out)
        job["status"] = "LOST"
        if host:
            # Whatever the job wrote before its runner died, then its scratch.
            # A Mac that stops answering leaves them there (said in the log).
            try:
                subprocess.run(["rsync", "-a", "-e", "ssh", f"{host}:isocast-scratch/{REMOTE_NAME}/{job_id}/out/",
                                f"{out}/"], capture_output=True, timeout=1800)
                ssh(host, f"rm -rf \"$HOME/isocast-scratch/{REMOTE_NAME}/{job_id}\"")
            except subprocess.TimeoutExpired:
                why += f"; {host} timed out while its results were copied (left there)"
            release_builds([host], job_id)
        if job.get("relay"):
            why += drop_relay(job_id, job["relay"])
        job["warning"] = f"reclaimed {time.strftime('%Y-%m-%d %H:%M')}: {why}"
        with open(out / "log.txt", "a") as f:
            print(f"hw-batch {time.strftime('%H:%M:%S')}: {job_id}: reclaimed as LOST: {why}", file=f)
        started = (job.get("runner") or {}).get("started")
        job["seconds"] = round(time.time() - started) if started else 0
        done.write_text(json.dumps(job, indent=2) + "\n")
        summarize(job, out, "LOST", job["seconds"], host)
        shutil.rmtree(ROOT / "jobs" / job_id, ignore_errors=True)
        shutil.rmtree(ROOT / "src" / job_id, ignore_errors=True)
        log(f"reclaimed {job_id} as LOST: {why} -> {out}/summary.md")
        reclaimed.append(job_id)
    return reclaimed


def drop_relay(job_id, relay):
    """A dead runner's soak relay: its `ssh -R` (if it is still that
    process) and its key on the receiver. Returns what was done, for the
    summary."""
    done = ""
    pid = relay.get("pid")
    if pid:
        r = subprocess.run(["ps", "-o", "command=", "-p", str(pid)], capture_output=True, text=True)
        if r.returncode == 0 and r.stdout.startswith("ssh ") and " -R " in r.stdout:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(pid, signal.SIGTERM)
            done += f"; stopped its relay (ssh pid {pid})"
    receiver = relay.get("receiver")
    if receiver:
        tag = f"isocast-hw-batch-{job_id}"
        try:
            r = ssh(receiver, f"cd ~/.ssh && grep -q '{tag}' authorized_keys 2>/dev/null || exit 0; "
                              f"grep -v '{tag}' authorized_keys >ak.hw-batch && chmod 600 ak.hw-batch && "
                              "mv ak.hw-batch authorized_keys && echo removed")
            if "removed" in r.stdout:
                done += f"; removed its relay key on {receiver}"
        except subprocess.TimeoutExpired:
            done += f"; its relay key may remain on {receiver} ({receiver} timed out)"
    return done


def reclaim_cmd(args):
    found = reclaim(args.dry_run)
    if not found:
        print("hw-batch: nothing to reclaim")
    return 0


def etime_seconds(text):
    days, _, rest = text.rpartition("-")
    parts = [int(x) for x in rest.split(":")]
    while len(parts) < 3:
        parts.insert(0, 0)
    return int(days or 0) * 86400 + parts[0] * 3600 + parts[1] * 60 + parts[2]


def reap(args):
    """Lane engines left on the test Macs: isocastd processes older than
    --hours that no running job owns (a job owns what started on its Mac
    after it did). Never the Mac's own engine (~/Library/Application
    Support/Isocast/Isocast Dev.app) or an installed app's. Lists them;
    --kill stops them (TERM)."""
    dirs()
    now = time.time()
    jobs = []
    for entry in (ROOT / "running").glob("*.json"):
        try:
            job = json.loads(entry.read_text())
            started = (job.get("runner") or {}).get("started") or entry.stat().st_ctime
        except (OSError, ValueError):
            # Unreadable mid-rewrite: assume it owns everything on both Macs.
            job, started = {"id": entry.stem, "host": "*"}, 0
        jobs.append((job.get("host"), started, job.get("id", entry.stem)))
    status = 0
    macs = list(MACS) if args.mac == "all" else [args.mac]
    for mac in macs:
        host = MACS[mac]
        r = ssh(host, 'echo "HOME=$HOME"; ps -axo pid=,ppid=,user=,etime=,command= -U "$(id -un)"', timeout=60)
        if r.returncode != 0:
            log(f"{host}: unreachable ({r.stderr.strip()[-200:]})")
            status = 1
            continue
        lines = r.stdout.splitlines()
        home = lines[0].split("=", 1)[1]
        procs = []
        for line in lines[1:]:
            fields = line.split(None, 4)
            if len(fields) < 5:
                continue
            pid, ppid, _user, etime, command = fields
            if Path(command.split(" --")[0]).name != "isocastd":
                continue
            procs.append((int(pid), int(ppid), etime_seconds(etime), command))
        engines = {p[0] for p in procs if "--native-log-relay" not in p[3]}
        doomed, kept = [], []
        for pid, _ppid, age, command in procs:
            if pid not in engines:
                continue
            own = command.startswith(f"{home}/Library/Application Support/Isocast/Isocast Dev.app/") \
                or command.startswith("/Applications/") or command.startswith(f"{home}/Applications/")
            owner = next((j for h, started, j in jobs if h in (host, "*") and now - age >= started - 60), None)
            if own or age < args.hours * 3600 or owner:
                why = "the Mac's own engine" if own else f"owned by {owner}" if owner else "young"
                kept.append((pid, age, command, why))
                continue
            doomed.append((pid, age, command))
        print(f"== {host}: {len(doomed)} lane engine(s) older than {args.hours:g} h no running job owns"
              f"{'; stopping them' if args.kill else ' (dry run; --kill stops them)'}")
        for pid, age, command in doomed:
            print(f"  {pid:>6}  {age / 3600:6.1f} h  {command[:160]}")
        for pid, age, command, why in kept:
            if why != "young":
                print(f"  kept {pid} ({age / 3600:.1f} h, {why}): {command[:120]}")
        if args.kill and doomed:
            pids = " ".join(str(p[0]) for p in doomed)
            # Each engine and its log relay; TERM lets it shut down cleanly.
            k = ssh(host, f'for p in {pids}; do kill -TERM $(pgrep -P "$p") "$p" 2>/dev/null; done; sleep 10; '
                          f'for p in {pids}; do kill -0 "$p" 2>/dev/null && echo "$p"; done; true', timeout=120)
            left = k.stdout.split()
            log(f"{host}: stopped {len(doomed) - len(left)} engine(s)" + (f"; still running: {' '.join(left)}"
                                                                         if left else ""))
            if left:
                status = 1
    return status


def run_one(args):
    """`now --detach`'s runner: run one queued job, in a session of its own."""
    if not ID_RE.match(args.id):
        sys.exit("hw-batch: bad job id")
    job = run_job(ROOT / "queue" / f"{args.id}.json")
    return 0 if job and job["status"] == "PASS" else 1


def schedule(args):
    hour, minute = (int(x) for x in args.at.split(":"))
    argv = ["cronctl", "add", "isocast-hw-nightly", "--cron", f"{minute} {hour} * * *",
            "--timezone", "America/New_York", "--missed", "once", "--timeout", "12h",
            "--inherit-path", "--cwd", str(Path(args.script).parent.parent), "--replace", "--json", "--",
            "/usr/bin/python3", args.script, "run-queue", "--max-hours", "11"]
    subprocess.run(argv, check=True)
    if not args.enable:
        subprocess.run(["cronctl", "pause", "isocast-hw-nightly", "--json"], check=True)
    return 0


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="op", required=True)
    for name in ("submit", "now"):
        s = sub.add_parser(name)
        s.add_argument("kind", choices=KINDS)
        s.add_argument("--bin-dir")
        s.add_argument("--commit")
        s.add_argument("--scripts", choices=("commit", "worktree"), default="commit",
                       help="where the job's scripts (e2e.py, bench.py...) come from: --commit (or HEAD), or "
                            "this worktree as it is, uncommitted merge included (with --bin-dir)")
        s.add_argument("--base-bin-dir")
        s.add_argument("--base-commit")
        s.add_argument("--mac", choices=("any", *MACS), default="m2-a")
        s.add_argument("--out")
        s.add_argument("--timeout", type=int, help="minutes")
        s.add_argument("--note")
        s.add_argument("--lane")
        s.add_argument("--only", action="append")
        s.add_argument("--scenario")
        s.add_argument("--variant")
        s.add_argument("--runs", type=int)
        s.add_argument("--seconds", type=int)
        s.add_argument("--hours", type=float)
        s.add_argument("--scale", type=float)
        s.add_argument("--profile", choices=("hosted", "hardware"), help="soak: scripts/soak.py --profile")
        s.add_argument("--receiver", choices=("local", *MACS), help="soak: stream to a receiver on this Mac")
        s.add_argument("--args", help="extra arguments for drag-lag.py, soak.py, stress.py, bench.py or e2e.py")
        s.add_argument("--command", help="cmd: a shell command")
        s.add_argument("--gui", action="store_true", help="cmd: run in the desktop session")
        if name == "now":
            s.add_argument("--detach", action="store_true",
                           help="run it in a runner of its own that outlives this shell (long soaks); "
                                "prints the job id and where its summary will be")
    q = sub.add_parser("run-queue")
    q.add_argument("--max-hours", type=float, default=11,
                   help="start no job after this many hours (a 10 h soak takes about 10.75 h)")
    sub.add_parser("list")
    rc = sub.add_parser("reclaim")
    rc.add_argument("--dry-run", action="store_true")
    rp = sub.add_parser("reap")
    rp.add_argument("--hours", type=float, default=12, help="only engines older than this (default 12)")
    rp.add_argument("--mac", choices=("all", *MACS), default="all")
    rp.add_argument("--kill", action="store_true", help="stop them (default: list only)")
    r1 = sub.add_parser("run-job", help=argparse.SUPPRESS)
    r1.add_argument("id")
    c = sub.add_parser("cancel")
    c.add_argument("id")
    sc = sub.add_parser("schedule")
    sc.add_argument("--at", default="01:30")
    sc.add_argument("--enable", action="store_true")
    sc.add_argument("--script", default=str(Path(__file__).resolve()),
                    help="the runner to schedule (default this file; use the main checkout's)")
    args = p.parse_args()
    # A runner stopped with TERM (`timeout`, a lane's cleanup) still stops
    # its test Mac job and writes its summary, instead of leaving it running
    # there with no lock and a dead entry in running/.
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGHUP, interrupted)
    if args.op == "submit":
        submit(args)
        return 0
    if args.op == "now":
        dirs()
        reclaim()
        job_id = submit(args)
        if args.detach:
            runner_log = ROOT / "runners" / f"{job_id}.log"
            out = json.loads((ROOT / "queue" / f"{job_id}.json").read_text())["out"]
            with open(runner_log, "a") as f:
                # Its own session: closing this shell or ending the lane's
                # turn doesn't stop it.
                subprocess.Popen([sys.executable, str(Path(__file__).resolve()), "run-job", job_id],
                                 cwd=Path.cwd(), stdin=subprocess.DEVNULL, stdout=f, stderr=subprocess.STDOUT,
                                 start_new_session=True)
            print(f"detached: runner log {runner_log}; summary {out}/summary.md "
                  f"when it ends; `hw-batch.py list` shows it")
            return 0
        job = run_job(ROOT / "queue" / f"{job_id}.json")
        if not job:
            return 1
        print((Path(job["out"]) / "summary.md").read_text())
        return 0 if job["status"] == "PASS" else 1
    return {"run-queue": run_queue, "list": list_jobs, "cancel": cancel, "schedule": schedule,
            "reclaim": reclaim_cmd, "reap": reap, "run-job": run_one}[args.op](args)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt as e:
        log(f"stopped ({e or 'interrupted'})")
        sys.exit(130)
