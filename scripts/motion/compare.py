#!/usr/bin/env python3
"""Compares motion runs that must be identical: lockstep runs of the same
build and suite (scripts/motion.sh --clock lockstep). Every frame's pixels
(frame_hash), positions and engine facts, and every result number, must
match; only wall-clock numbers may differ (each frame's cpu/frame/gpu/
engine-wait times, `late`, engine frames received, redraws Slint asked
for (one per engine frame that arrives); result.json's "wall",
"invocation" and the sheet's path).

    scripts/motion/compare.py OUT_A OUT_B [OUT_C ...]

Prints the differences and exits 1 if there are any.
"""

import json
import sys
from pathlib import Path

WALL_FRAME = {"cpu_ms", "frame_ms", "gpu_ms", "engine_wait_ms", "late", "engine_frames", "redraws_requested"}
WALL_RESULT = {"wall", "invocation", "sheet"}


def load(out):
    runs = {}
    for line in (out / "runs.tsv").read_text().splitlines():
        name, variant, code = line.split("\t")
        run = f"{name}@{variant}"
        result = out / run / "result.json"
        frames = out / run / "frames.jsonl"
        runs[run] = {
            "code": code,
            "result": json.loads(result.read_text()) if result.exists() else None,
            "frames": [json.loads(l) for l in frames.read_text().splitlines()] if frames.exists() else None,
        }
    return runs


def strip(value, drop):
    return {k: v for k, v in value.items() if k not in drop} if isinstance(value, dict) else value


def diff(a, b, path=""):
    if isinstance(a, dict) and isinstance(b, dict):
        for k in sorted(set(a) | set(b)):
            yield from diff(a.get(k), b.get(k), f"{path}.{k}" if path else k)
    elif isinstance(a, list) and isinstance(b, list) and len(a) == len(b):
        for i, (x, y) in enumerate(zip(a, b)):
            yield from diff(x, y, f"{path}[{i}]")
    elif a != b:
        yield f"{path}: {a!r} != {b!r}"


def main():
    outs = [Path(p) for p in sys.argv[1:]]
    if len(outs) < 2:
        sys.exit(__doc__)
    base = load(outs[0])
    problems = 0
    for other_path in outs[1:]:
        other = load(other_path)
        for run in sorted(set(base) | set(other)):
            a, b = base.get(run), other.get(run)
            if a is None or b is None:
                print(f"{other_path.name} {run}: only in one")
                problems += 1
                continue
            found = list(diff({"code": a["code"]}, {"code": b["code"]}))
            found += diff(strip(a["result"], WALL_RESULT), strip(b["result"], WALL_RESULT), "result")
            fa, fb = a["frames"] or [], b["frames"] or []
            if len(fa) != len(fb):
                found.append(f"frames: {len(fa)} != {len(fb)}")
            pixels = sum(1 for x, y in zip(fa, fb) if x.get("frame_hash") != y.get("frame_hash"))
            for x, y in zip(fa, fb):
                found += diff(strip(x, WALL_FRAME), strip(y, WALL_FRAME), f"frame[{x['index']}]")
            status = "identical" if not found else f"{len(found)} differences ({pixels} pixel frames)"
            print(f"{outs[0].name} vs {other_path.name} {run}: {len(fa)} frames, {status}")
            for line in found[:8]:
                print(f"    {line}")
            problems += bool(found)
    sys.exit(1 if problems else 0)


if __name__ == "__main__":
    main()
