#!/usr/bin/env python3
"""Bounded weak-machine prototype (#110); run only inside hw-batch cmd.

Run existing e2e/bench at normal priority, with optional CPU competition and
an offscreen Metal helper. --background also applies taskpolicy to the
workload; that is a scheduler stress case, not normal user behavior. Neither
mode simulates a different chip or a true core limit.

Example: python3 scripts/weak-machine.py --seconds 180 --cpu-workers 4 \
    --gpu-helper /scratch/weak-gpu --report /scratch/weak.json -- \
    python3 scripts/bench.py --scenario program-1080 --runs 1 --seconds 20
"""

import argparse
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seconds", type=int, default=180)
    parser.add_argument("--cpu-workers", type=int, default=0)
    parser.add_argument("--background", action="store_true",
                        help="also deprioritize Isocast itself (not the realistic baseline)")
    parser.add_argument("--gpu-helper", type=Path)
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
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("give the existing e2e/bench command after --")
    # The caller owns the queue lock. Do not take it twice: bench/e2e also
    # acquire the remote per-machine benchmark lock themselves.
    processes = []
    started = time.monotonic()
    report = {"host": socket.gethostname(), "taskpolicy": "background" if args.background else "normal",
              "cpu_workers": args.cpu_workers, "gpu_contention": bool(args.gpu_helper),
              "core_limit": None, "command": command, "exit_code": None,
              "note": "Scheduling and CPU/GPU competition, not a simulated chip or core count"}

    def stop(_signum, _frame):
        raise InterruptedError("interrupted")

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    try:
        for _ in range(args.cpu_workers):
            processes.append(subprocess.Popen(
                [sys.executable, "-c", "x=1\nwhile True: x=(x*1664525+1013904223)&0xffffffff"],
                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL, start_new_session=True))
        if args.gpu_helper:
            processes.append(subprocess.Popen(
                [str(args.gpu_helper), str(args.seconds)], start_new_session=True))
            time.sleep(0.5)
            if processes[-1].poll() is not None:
                raise RuntimeError("GPU helper failed before the workload started")
        prefix = ["/usr/sbin/taskpolicy", "-b"] if args.background else []
        workload = subprocess.Popen([*prefix, *command],
                                    start_new_session=True)
        processes.append(workload)
        report["exit_code"] = workload.wait(timeout=args.seconds)
    except (subprocess.TimeoutExpired, InterruptedError, RuntimeError, OSError) as error:
        report["error"] = str(error)
        report["exit_code"] = 124
    finally:
        # Stop only process groups this invocation created. A suspended
        # child must be resumed before TERM can run its cleanup handler.
        for process in reversed(processes):
            if process.poll() is None:
                try:
                    os.killpg(process.pid, signal.SIGCONT)
                    os.killpg(process.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
        for process in reversed(processes):
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
        report["seconds"] = time.monotonic() - started
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, indent=2) + "\n")
    return report["exit_code"]


if __name__ == "__main__":
    sys.exit(main())
