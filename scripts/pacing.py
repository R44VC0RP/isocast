#!/usr/bin/env python3
"""Diagnose hosted pacing without changing the E2E loss/integrity policy.

Records native timer slop/process policy/Metal identity, then a ten-second
720p30 H.264 program + two ISOs using the actual E2E setup and file checks.
Keep this separate from qualification: even a failing VM leaves measurements.
"""
import argparse
import fcntl
import hashlib
import json
import os
import signal
import tempfile
import time
from pathlib import Path

import e2e


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--daemon", type=Path, default=Path("target/release/isocastd"))
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    args.work.mkdir(parents=True, exist_ok=True)
    args.report.parent.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="p-", dir=args.work.resolve()))
    report = {"work": str(root), "daemon_sha256": hashlib.sha256(args.daemon.read_bytes()).hexdigest()}
    engine = None
    lock = None

    def expired(*_):
        raise TimeoutError("pacing diagnostic exceeded 600 seconds")

    signal.signal(signal.SIGALRM, expired)
    signal.signal(signal.SIGTERM, expired)
    signal.alarm(600)
    try:
        scratch = Path(os.environ.get("ISOCAST_SCRATCH", "/Volumes/IsocastBuild"))
        lock = open(scratch / ".bench.lock", "a")
        # Keep the lock through engine shutdown, including failures.
        fcntl.flock(lock, fcntl.LOCK_EX)
        probe = root / "probe"
        e2e.command(["xcrun", "clang", "-O2", "-Wall", "-Wextra", "-Werror",
                     "-framework", "Foundation", "-framework", "Metal",
                     e2e.REPO / "scripts/pacing-probe.m", "-o", probe])
        report["native"] = json.loads(e2e.command([probe], timeout=480).stdout)
        report["processes"] = e2e.command(["ps", "-o", "pid,ppid,pri,nice,state,comm", "-p",
                                          f"{os.getpid()},{os.getppid()}"], check=False).stdout.decode()
        e2e.write_json(args.report, report)
        engine = e2e.Engine(root, args.daemon.resolve())
        report["daemon_process"] = e2e.command(["ps", "-o", "pid,ppid,pri,nice,state,comm",
                                                "-p", str(engine.process.pid)]).stdout.decode()
        engine.apply(e2e.setup(root, "h264", True, 1280, 720, 30))
        started = engine.rpc("start", {"seconds": 10})
        report["samples"] = []
        deadline = time.monotonic() + 50
        while not e2e.finished(started["manifest"]):
            if time.monotonic() > deadline:
                raise TimeoutError("diagnostic recording did not finish")
            status = engine.rpc("status")
            report["samples"].append({"engine": status.get("engine"), "state": status["state"]})
            time.sleep(.5)
        path = Path(started["manifest"])
        manifest = e2e.read_json(path)
        report["manifest"] = manifest
        report["drops"] = e2e.drop_metrics(manifest)
        report["files"] = []
        for file in manifest["files"]:
            try:
                report["files"].append(e2e.check_file(path.parent / file["path"], 10, 30,
                    dimensions=(1280, 720), dropped=file["dropped_frames"],
                    repeated=file.get("repeated_frames", 0)))
            except Exception as exc:
                report["files"].append({"file": file["path"], "integrity_error": str(exc)})
    except Exception as exc:
        report["error"] = str(exc)
    finally:
        # Clean up before releasing the invocation; preserve partial evidence.
        signal.alarm(0)
        try:
            if engine is not None:
                engine.close()
        except Exception as exc:
            report["cleanup_error"] = str(exc)
        if lock is not None:
            lock.close()
        e2e.write_json(args.report, report)
    print(json.dumps(report, indent=2))
    return int("error" in report or "cleanup_error" in report)


if __name__ == "__main__":
    raise SystemExit(main())
