#!/usr/bin/env python3
"""Run the existing benchmark interleaved, or render a strictly data-only report.

The workflow_run publisher uses the default branch's copy of this file. It
accepts a bounded JSON artifact, never Python, Markdown, paths or commands from
the PR. Run metadata is checked against GitHub separately before commenting.
"""

import argparse
import json
import math
import re
import statistics
import subprocess
import sys
from pathlib import Path

SCENARIOS = ("program-1080", "stream", "flagship")
REASONS = {"hardware": "no hardware encoder on this runner",
           "capacity": "flagship not qualified on this runner (requires Max/Ultra chip)",
           "failed": "benchmark failed; inspect the run logs"}
METRICS = ("cpu", "instructions", "rss", "video_drops", "audio_drops")


def number(value, optional=False):
    if value is None and optional:
        return None
    if type(value) not in (float, int) or not math.isfinite(value) or not 0 <= value < 1e12:
        raise ValueError("invalid numeric measurement")
    return value


def validate(data):
    if data.get("version") != 1 or type(data.get("pr")) is not int or not 0 < data["pr"] < 10**9:
        raise ValueError("invalid benchmark report version or PR")
    for field in ("head_sha", "base_sha"):
        if not re.fullmatch(r"[a-f0-9]{40}", data.get(field, "")):
            raise ValueError(f"invalid {field}")
    cases = data["scenarios"]
    if type(cases) is not list or [c["name"] for c in cases] != list(SCENARIOS):
        raise ValueError("missing, duplicate or unknown benchmark scenarios")
    for case in cases:
        if case["status"] in ("skip", "fail"):
            if case.get("reason") not in REASONS:
                raise ValueError("unknown benchmark outcome")
            continue
        if case["status"] != "pass":
            raise ValueError("invalid status")
        runs = case["runs"]
        if type(runs) is not list or len(runs) != 6:
            raise ValueError("a comparison requires exactly three base/head pairs")
        if [(r["label"], r["run"]) for r in runs] != [(label, i) for i in range(3) for label in ("base", "new")]:
            raise ValueError("benchmark order is not three interleaved base/head pairs")
        for row in runs:
            for metric in METRICS:
                number(row[metric], optional=metric == "instructions")
            if type(row["complete"]) is not bool:
                raise ValueError("invalid integrity result")
    return data


def summary(data):
    validate(data)
    lines = ["<!-- isocast-bench -->", "## Engine benchmark", "",
             f"Base `{data['base_sha'][:12]}` → head `{data['head_sha'][:12]}`; three interleaved A/B pairs.", "",
             "GitHub VMs are noisy. Flags compare the medians with the full run-to-run spread; "
             "exact performance numbers come from the test Macs. These measurements are informational, not a merge gate.", "",
             "| Scenario | Metric | Base median [min–max] | Head median [min–max] | Change |",
             "|---|---|---:|---:|---|"]
    for case in data["scenarios"]:
        if case["status"] != "pass":
            lines.append(f"| {case['name']} | **{case['status']}** | — | — | {REASONS[case['reason']]} |")
            continue
        runs = case["runs"]
        for metric, label in (("cpu", "daemon CPU %"), ("instructions", "M instructions/s"), ("rss", "RSS MiB")):
            a = [r[metric] for r in runs if r["label"] == "base"]
            b = [r[metric] for r in runs if r["label"] == "new"]
            if None in a + b:
                lines.append(f"| {case['name']} | {label} | n/a | n/a | unavailable counter |")
                continue
            ma, mb = statistics.median(a), statistics.median(b)
            delta = (mb / ma - 1) * 100 if ma else None
            spread = max(max(a) - min(a), max(b) - min(b))
            separated = min(a) > max(b) or min(b) > max(a)
            flag = "within spread"
            if separated and abs(mb - ma) > spread:
                flag = "increase beyond spread" if mb > ma else "decrease beyond spread"
                if metric == "cpu":
                    flag = "REGRESSION >15%" if delta is not None and delta > 15 else (
                        "slower beyond spread" if mb > ma else "speed-up beyond spread")
            change = f"{delta:+.1f}%" if delta is not None else "n/a"
            lines.append(f"| {case['name']} | {label} | {ma:.1f} [{min(a):.1f}–{max(a):.1f}] | "
                         f"{mb:.1f} [{min(b):.1f}–{max(b):.1f}] | {change}; {flag} |")
        for metric, label in (("video_drops", "video drops (sum)"), ("audio_drops", "audio dropouts (sum)")):
            a, b = (sum(r[metric] for r in runs if r["label"] == build) for build in ("base", "new"))
            lines.append(f"| {case['name']} | {label} | {a:g} | {b:g} | {'NEW DROPS' if b > a else 'no new drops'} |")
        if any(not r["complete"] for r in runs):
            lines.append(f"| {case['name']} | **file integrity** | — | — | incomplete files; comparison not qualified |")
    return "\n".join(lines) + "\n"


def normalize(row, scenario):
    if row["scenario"] != scenario or row.get("real"):
        raise ValueError("wrong benchmark scenario/input")
    files = row["files"]
    stream = row.get("stream", {})
    if scenario == "stream":
        if stream.get("state") != "live" or not stream.get("frames_sent"):
            raise ValueError("stream never became live")
    elif len(files) != (3 if scenario == "flagship" else 1):
        raise ValueError("missing benchmark output files")
    return {"label": row["label"], "run": row["run"], "cpu": row["isocastd_cpu"],
            "instructions": row.get("isocastd_million_instructions_s"), "rss": row["rss_mb"],
            "video_drops": sum(number(f["dropped"]) for f in files) + number(stream.get("dropped_frames", 0)),
            "audio_drops": sum(number(f["audio_dropouts"]) for f in files) + number(stream.get("audio_dropouts", 0)),
            "complete": all(f["complete"] for f in files)}


def run(args):
    args.work.mkdir(parents=True, exist_ok=True)
    data = {"version": 1, "pr": args.pr, "head_sha": args.head_sha, "base_sha": args.base_sha, "scenarios": []}
    caps = json.loads(args.capabilities.read_text())["capabilities"]
    chip = subprocess.run(["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True, text=True, check=True).stdout
    failed = False
    try:
        for scenario in SCENARIOS:
            case = {"name": scenario, "status": "skip", "reason": "hardware"}
            data["scenarios"].append(case)
            if not caps["hevc"] or not caps["h264"]:
                continue
            if scenario == "flagship" and not re.search(r"\b(Max|Ultra)\b", chip):
                case["reason"] = "capacity"
                continue
            folder = args.work / scenario
            folder.mkdir(exist_ok=False)
            cmd = [sys.executable, "scripts/bench.py", "--work", str(folder), "--daemon", str(args.head),
                   "--compare", str(args.base), "--cli", str(args.cli), "--scenario", scenario,
                   "--runs", "3", "--seconds", "12"]
            try:
                with open(folder / "bench.log", "wb") as log:
                    subprocess.run(cmd, check=True, stdout=log, stderr=log, timeout=1200)
                rows = [json.loads(line) for line in (folder / "results.jsonl").read_text().splitlines()]
                case.update(status="pass", runs=[normalize(r, scenario) for r in rows])
                case.pop("reason")
                failed |= any(not r["complete"] for r in case["runs"])
            except (subprocess.SubprocessError, ValueError, KeyError, OSError) as exc:
                print(f"{scenario}: {exc}", file=sys.stderr)
                case.update(status="fail", reason="failed")
                failed = True
            args.output.write_text(json.dumps(data, indent=2) + "\n")
    finally:
        args.output.write_text(json.dumps(data, indent=2) + "\n")
    print(summary(data))
    return int(failed)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest="command", required=True)
    r = sub.add_parser("run")
    for name in ("work", "head", "base", "cli", "capabilities", "output"):
        r.add_argument("--" + name, type=Path, required=True)
    r.add_argument("--pr", type=int, required=True)
    r.add_argument("--head-sha", required=True)
    r.add_argument("--base-sha", required=True)
    s = sub.add_parser("render")
    s.add_argument("report", type=Path)
    args = p.parse_args()
    if args.command == "run":
        return run(args)
    if args.report.stat().st_size > 256 * 1024:
        raise ValueError("oversize benchmark artifact")
    print(summary(json.loads(args.report.read_text())), end="")
    return 0


if __name__ == "__main__":
    sys.exit(main())
