#!/usr/bin/env python3
"""Bounded weak-machine prototype (#110); run only inside hw-batch cmd.

Run existing e2e/bench at normal priority, with optional CPU competition, an
offscreen Metal helper and a bounded touched-memory competitor. --background
also applies taskpolicy to the workload; that is a scheduler stress case, not
normal user behavior. No mode simulates a different chip, a true core limit or
a smaller physical RAM: --memory-mb holds incompressible pages resident in one
isolated child, and the host's real RAM, bandwidth, compressor and swap still
decide what happens. The report records requested vs resident bytes and
system-wide vm_stat compressor/swap counters so that difference stays visible.

Example: python3 scripts/weak-machine.py --seconds 180 --cpu-workers 4 \
    --gpu-helper /scratch/weak-gpu --memory-mb 2048 --report /scratch/weak.json -- \
    python3 scripts/bench.py --scenario program-1080 --runs 1 --seconds 20
"""

import argparse
import json
import os
from pathlib import Path
import select
import signal
import socket
import subprocess
import sys
import time

MEMORY_MB_MAX = 4096
MEMORY_FRACTION_MAX = 0.25  # of physical RAM
MEMORY_READY_SECONDS = 60
# Keep at least this much reclaimable memory free after the competitor fills.
RESERVE_MIN_BYTES = 2 << 30
RESERVE_FRACTION = 0.15
# Exit code when a competitor (or its preflight) failed: never a PASS.
COMPETITOR_FAILED = 125

# Fills its pages with fresh random bytes (incompressible, never shared), says
# it is ready only once every page is touched, then re-reads one byte per page
# each second so the pages stay recently used. It exits by itself at its own
# deadline or when its parent dies, so a SIGKILLed harness cannot leave it.
MEMORY_CHILD = r"""
import json, mmap, os, resource, sys, time
mb, seconds, parent = int(sys.argv[1]), float(sys.argv[2]), int(sys.argv[3])
until = time.monotonic() + seconds
size, block, page = mb << 20, 1 << 20, mmap.PAGESIZE
started = time.monotonic()
memory = mmap.mmap(-1, size)
for offset in range(0, size, block):
    memory[offset:offset + block] = os.urandom(block)
sys.stdout.write(json.dumps({
    "requested_bytes": size, "touched_bytes": size, "page_size": page,
    "fill_seconds": round(time.monotonic() - started, 3),
    "maxrss_bytes": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss}) + "\n")
sys.stdout.flush()
while time.monotonic() < until and os.getppid() == parent:
    total = 0
    for offset in range(0, size, page):
        total ^= memory[offset]
    time.sleep(1)
"""


class CompetitorError(RuntimeError):
    pass


def sysctl(name):
    return subprocess.run(["/usr/sbin/sysctl", "-n", name], capture_output=True,
                          text=True, check=True).stdout.strip()


def vm_stat():
    """System-wide vm_stat counters, raw (pages or events), plus page_size.

    Read-only: it injects no pressure or warnings.
    """
    out = subprocess.run(["/usr/bin/vm_stat"], capture_output=True, text=True,
                         check=True).stdout.splitlines()
    stats = {"page_size": int(out[0].split("page size of")[1].split()[0])}
    for line in out[1:]:
        key, _, value = line.partition(":")
        value = value.strip().rstrip(".")
        if value.isdigit():
            stats[key.strip().strip('"').lower().replace(" ", "_")] = int(value)
    return stats


def resident_bytes(pid):
    try:
        out = subprocess.run(["/bin/ps", "-o", "rss=", "-p", str(pid)],
                             capture_output=True, text=True).stdout.strip()
        return int(out) * 1024 if out else None
    except (OSError, ValueError):
        return None


def process_table():
    """Live (non-zombie) pid -> (ppid, pgid, start time); empty if ps fails."""
    try:
        out = subprocess.run(["/bin/ps", "-axo", "pid=,ppid=,pgid=,stat=,lstart="],
                             capture_output=True, text=True,
                             env={**os.environ, "LC_ALL": "C"}, timeout=10).stdout
    except (OSError, subprocess.TimeoutExpired):
        return {}
    table = {}
    for line in out.splitlines():
        fields = line.split(None, 4)
        if len(fields) == 5 and not fields[3].startswith("Z"):
            table[int(fields[0])] = (int(fields[1]), int(fields[2]), fields[4].strip())
    return table


def remember_descendants(known, roots, table):
    """Add every live descendant of roots to known, keyed by pid -> start time.

    Called during the run as well, so a daemon that later reparents to launchd
    (double fork, setsid) is still remembered and stopped at cleanup.
    """
    children = {}
    for pid, (ppid, _pgid, _start) in table.items():
        children.setdefault(ppid, []).append(pid)
    pending = list(roots) + [pid for pid in known if pid in table and table[pid][2] == known[pid]]
    while pending:
        for child in children.get(pending.pop(), []):
            if known.get(child) != table[child][2] and child not in roots:
                known[child] = table[child][2]
                pending.append(child)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seconds", type=int, default=180)
    parser.add_argument("--cpu-workers", type=int, default=0)
    parser.add_argument("--background", action="store_true",
                        help="also deprioritize Isocast itself (not the realistic baseline)")
    parser.add_argument("--gpu-helper", type=Path)
    parser.add_argument("--memory-mb", type=int, default=0,
                        help=f"hold this many MiB of touched, incompressible memory in one child "
                             f"(0..{MEMORY_MB_MAX}, at most 25%% of physical RAM)")
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("this prototype requires macOS taskpolicy")
    # hw-batch alone issues this reservation on its approved host. AWS
    # LocalHostName is often just "Mac-mini", so a hostname allowlist is
    # neither a reliable host identity nor a substitute for the queue lock.
    reservation = Path.home() / "isocast-builds/.hw-job"
    try:
        _job, expires = reservation.read_text().split()
        reserved = time.time() < float(expires)
    except (OSError, ValueError):
        reserved = False
    if not reserved:
        parser.error("run under hw-batch: no active hardware reservation")
    if not 1 <= args.seconds <= 600 or not 0 <= args.cpu_workers <= 8:
        parser.error("seconds: 1..600; cpu-workers: 0..8")
    if not 0 <= args.memory_mb <= MEMORY_MB_MAX:
        parser.error(f"memory-mb: 0..{MEMORY_MB_MAX}")
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("give the existing e2e/bench command after --")
    # The caller owns the queue lock. Do not take it twice: bench/e2e also
    # acquire the remote per-machine benchmark lock themselves.
    processes = []
    competitors = []  # (name, process, natural end in workload seconds or None)
    known = {}  # workload descendants: pid -> start time
    started = time.monotonic()
    report = {"host": socket.gethostname(), "taskpolicy": "background" if args.background else "normal",
              "cpu_workers": args.cpu_workers, "gpu_contention": bool(args.gpu_helper),
              "memory_mb": args.memory_mb, "core_limit": None, "physical_memory_limit": None,
              "command": command, "exit_code": None, "workload_exit_code": None,
              "note": "Scheduling, CPU/GPU competition and touched memory held by one child; "
                      "not a simulated chip, core count or smaller RAM"}

    def stop(_signum, _frame):
        # InterruptedError is an OSError: the best-effort ps/vm_stat
        # readers would swallow it and turn an interrupted run into PASS.
        raise KeyboardInterrupt("interrupted")

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    memory_child = None
    try:
        if args.memory_mb:
            requested = args.memory_mb << 20
            physical = int(sysctl("hw.memsize"))
            before = vm_stat()
            available = before["page_size"] * sum(before.get(key, 0) for key in (
                "pages_free", "pages_inactive", "pages_speculative", "pages_purgeable"))
            reserve = max(RESERVE_MIN_BYTES, int(physical * RESERVE_FRACTION))
            report["memory"] = memory = {
                "requested_bytes": requested, "physical_bytes": physical,
                "available_bytes_preflight": available, "reserve_bytes": reserve,
                "method": "random-filled anonymous mmap in one child; re-reads one byte per page "
                          "each second (a few % of one core); host compressor/swap unchanged",
                "vm_stat": {"preflight": before}}
            try:
                memory["memorystatus_level"] = {"preflight": int(sysctl("kern.memorystatus_level"))}
            except (OSError, ValueError, subprocess.CalledProcessError):
                pass
            if requested > physical * MEMORY_FRACTION_MAX:
                raise CompetitorError(f"memory-mb {args.memory_mb} exceeds 25% of physical RAM "
                                      f"({physical >> 20} MiB)")
            if available - requested < reserve:
                raise CompetitorError(f"refusing memory competitor: {available >> 20} MiB reclaimable, "
                                      f"needs {(requested + reserve) >> 20} MiB "
                                      f"({args.memory_mb} + {reserve >> 20} reserve)")
            child = subprocess.Popen(
                [sys.executable, "-c", MEMORY_CHILD, str(args.memory_mb),
                 str(args.seconds + MEMORY_READY_SECONDS + 30), str(os.getpid())],
                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, start_new_session=True)
            processes.append(child)
            memory_child = child
            competitors.append(("memory", child, None))
            ready, _, _ = select.select([child.stdout], [], [], MEMORY_READY_SECONDS)
            line = child.stdout.readline() if ready else b""
            try:
                memory["child"] = json.loads(line)
            except ValueError:
                raise CompetitorError(f"memory competitor not ready within {MEMORY_READY_SECONDS} s "
                                      f"(exit {child.poll()})") from None
            memory["resident_bytes_ready"] = resident = resident_bytes(child.pid)
            memory["vm_stat"]["ready"] = vm_stat()
            # Random pages cannot compress, so a shortfall means the host is
            # already compressing/swapping: the requested pressure is not real.
            if not resident or resident < 0.9 * requested:
                raise CompetitorError(f"memory competitor only {(resident or 0) >> 20} MiB resident "
                                      f"of {args.memory_mb} MiB requested")
        for index in range(args.cpu_workers):
            processes.append(subprocess.Popen(
                [sys.executable, "-c", "x=1\nwhile True: x=(x*1664525+1013904223)&0xffffffff"],
                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL, start_new_session=True))
            competitors.append((f"cpu-{index}", processes[-1], None))
        if args.gpu_helper:
            gpu_seconds = min(600, args.seconds + 5)
            processes.append(subprocess.Popen(
                [str(args.gpu_helper), str(gpu_seconds)], start_new_session=True))
            time.sleep(0.5)
            if processes[-1].poll() is not None:
                raise CompetitorError("GPU helper failed before the workload started")
            # The helper stops by itself after gpu_seconds; started 0.5 s earlier.
            competitors.append(("gpu", processes[-1], gpu_seconds - 2.5))
        prefix = ["/usr/sbin/taskpolicy", "-b"] if args.background else []
        workload = subprocess.Popen([*prefix, *command],
                                    start_new_session=True)
        processes.append(workload)
        workload_started = time.monotonic()
        while True:
            try:
                report["workload_exit_code"] = workload.wait(timeout=1)
            except subprocess.TimeoutExpired:
                pass
            elapsed = time.monotonic() - workload_started
            for name, process, natural_end in competitors:
                code = process.poll()
                if code is not None and not (code == 0 and natural_end is not None
                                             and elapsed >= natural_end):
                    raise CompetitorError(f"{name} competitor exited ({code}) after {elapsed:.1f} s "
                                          "of the workload")
            if report["workload_exit_code"] is not None:
                report["exit_code"] = report["workload_exit_code"]
                break
            if elapsed >= args.seconds:
                raise subprocess.TimeoutExpired(command, args.seconds)
            remember_descendants(known, [workload.pid], process_table())
    except (subprocess.TimeoutExpired, KeyboardInterrupt, RuntimeError, OSError,
            subprocess.CalledProcessError, ValueError, IndexError) as error:
        report["error"] = str(error)
        report["exit_code"] = COMPETITOR_FAILED if isinstance(error, CompetitorError) else 124
    finally:
        # A second Ctrl-C or TERM must not abort cleanup half way.
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        memory = report.get("memory")
        if memory and memory_child and memory_child.poll() is None:
            memory["resident_bytes_end"] = resident_bytes(memory_child.pid)
        if memory:
            try:
                memory["vm_stat"]["end"] = vm_stat()
            except (OSError, ValueError, IndexError, subprocess.CalledProcessError):
                pass
        report["leftover_pids"] = stop_all(processes, known)
        if report["leftover_pids"]:
            report["error"] = (report.get("error", "") + "; processes survived SIGKILL").lstrip("; ")
            report["exit_code"] = report["exit_code"] or 124
        report["seconds"] = time.monotonic() - started
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, indent=2) + "\n")
    return report["exit_code"]


def stop_all(processes, known):
    """Stop every process group this invocation created and every remembered
    workload descendant (even ones that left the group), TERM then KILL.

    A suspended child must be resumed before TERM can run its cleanup handler.
    Groups are signalled only while a live member still has that group id, and
    descendants only while their start time matches, so a reused pid is never hit.
    """
    roots = [process.pid for process in processes]
    remember_descendants(known, roots, process_table())

    def signal_all(signum):
        table = process_table()
        groups = {pgid for _ppid, pgid, _start in table.values()}
        for pgid in roots:
            if pgid in groups:
                try:
                    os.killpg(pgid, signum)
                except (ProcessLookupError, PermissionError):
                    pass
        alive = [pid for pid, start in known.items() if pid in table and table[pid][2] == start]
        for pid in alive:
            try:
                os.kill(pid, signum)
            except (ProcessLookupError, PermissionError):
                pass
        return sorted(set(alive) | {pid for pid, (_ppid, pgid, _start) in table.items()
                                    if pgid in roots})

    signal_all(signal.SIGCONT)
    signal_all(signal.SIGTERM)
    deadline = time.monotonic() + 10
    for process in processes:
        try:
            process.wait(timeout=max(0, deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            pass
    while time.monotonic() < deadline and signal_all(0):
        time.sleep(0.2)
    signal_all(signal.SIGKILL)
    for process in processes:
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass
        if process.stdout:
            process.stdout.close()
    time.sleep(0.2)
    return signal_all(0)


if __name__ == "__main__":
    sys.exit(main())
