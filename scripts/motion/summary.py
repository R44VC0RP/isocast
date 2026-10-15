#!/usr/bin/env python3
"""Sums up a motion run's folder: OUT/summary.json and OUT/summary.md
(a table per run), and exits 0 if every run passed (or, for a control
with `expect`, came out as expected), 3 if any failed, 1 if one couldn't
run or has no result of its own.

    scripts/motion/summary.py OUT

Only the runs in OUT/runs.tsv (written by scripts/motion.sh: name,
variant, exit status) count, and only results marked with this
invocation (MOTION_INVOCATION): a result left from an earlier run in the
same folder never stands in for a run that failed to run.
"""

import json
import os
import sys
import tempfile
from pathlib import Path


def row(r):
    s = r.get("on_screen") or {}
    w = r.get("wall") or {}
    m = r.get("in_state") or {}
    g = r.get("glitches") or {}
    e = r.get("engine") or {}
    return {
        "id": r["id"],
        "pass": r["pass"],
        "expect": r.get("expect"),
        "ok": r["as_expected"] if r.get("as_expected") is not None else r["pass"],
        "gap_p95": s.get("gap_pt_p95"),
        "gap_max": s.get("gap_pt_max"),
        "gain": s.get("gain"),
        "lag": s.get("lag_frames"),
        "back": s.get("jump_back_frames"),
        "stall": s.get("stall_frames"),
        "state_gap_p95": m.get("gap_pt_p95"),
        "tracked": f"{r.get('tracked_frames')}/{r.get('gesture_frames')}",
        "sync_timeouts": r.get("engine_sync_timeouts"),
        "black": g.get("black_picture_frames", 0) + g.get("blank_window_frames", 0),
        "flicker": g.get("flicker_frames"),
        "frame_ms_p95": w.get("frame_ms_p95"),
        "frame_ms_max": w.get("frame_ms_max"),
        "calls": e.get("calls_during_gesture"),
        "failed": ", ".join(f"{f['check']} {f['value']} > {f['limit']}" for f in r.get("failures", [])),
        "sheet": (r.get("sheet") or {}).get("path"),
    }


def write(path, text):
    """Write PATH without following a link: refuse a symlink or anything
    but a plain file there, write a new temporary file beside it, then
    rename it over PATH (a rename replaces the name, not a link's target)."""
    if path.is_symlink() or (path.exists() and not path.is_file()):
        sys.exit(f"summary.py: {path} is a link or not a plain file; not writing it")
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=".motion-tmp.")
    try:
        os.fchmod(fd, 0o644)
        with os.fdopen(fd, "w") as f:
            f.write(text)
        os.replace(tmp, path)
    except BaseException:
        os.unlink(tmp)
        raise


def main():
    out = Path(sys.argv[1])
    invocation = os.environ.get("MOTION_INVOCATION")
    results, errors = [], []
    for line in (out / "runs.tsv").read_text().splitlines():
        name, variant, code = line.split("\t")
        run = f"{name}@{variant}"
        path = out / run / "result.json"
        if code not in ("0", "3"):
            errors.append(f"{run}: couldn't run (exit {code}; see {run}.log)")
            continue
        if not path.exists():
            errors.append(f"{run}: no result (see {run}.log)")
            continue
        result = json.loads(path.read_text())
        if invocation and result.get("invocation") != invocation:
            errors.append(f"{run}: result.json is from another invocation")
            continue
        results.append(result)
    rows = [row(r) for r in results]
    write(out / "summary.json", json.dumps({"runs": rows, "errors": errors}, indent=2))
    cols = ["id", "ok", "pass", "expect", "gap_p95", "gap_max", "gain", "lag", "back", "stall", "state_gap_p95",
            "tracked", "sync_timeouts", "black", "flicker", "frame_ms_p95", "frame_ms_max", "calls", "failed"]
    lines = ["| " + " | ".join(cols) + " |", "|" + "---|" * len(cols)]
    for r in rows:
        lines.append("| " + " | ".join("" if r[c] is None else str(r[c]) for c in cols) + " |")
    lines += [f"ERROR {e}" for e in errors]
    write(out / "summary.md", "\n".join(lines) + "\n")
    print("\n".join(lines))
    if errors:
        sys.exit(1)
    if not all(r["ok"] for r in rows):
        sys.exit(3)


if __name__ == "__main__":
    main()
