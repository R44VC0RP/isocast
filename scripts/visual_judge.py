#!/usr/bin/env python3
"""The visual check's judge: a restricted OpenCode agent looks at each changed
view (before, after and diff images, plus the pull request's interface code
changes as read-only text) and answers only through its `deliver_verdict`
tool. Run by .github/workflows/visual-report.yml (trusted, no PR code runs):

    scripts/visual_judge.py ARTIFACT_DIR --judge .github/visual-judge \\
        --work DIR --out judge.json [--diff ui.diff] [--sources DIR] \\
        [--model opencode/claude-sonnet-5-5] [--max-views 15] [--dry-run]

ARTIFACT_DIR is the validated artifact of visual.yml (manifest.json and
small/{base,head,diff}/). For each view whose manifest status is changed,
resized or added (unchanged views cost nothing), it builds a plain input
folder, runs `opencode run --standalone --agent judge` from a fresh copy of
the trusted judge config (outside every folder the PR's files are in), and
reads the verdict file the tool wrote. No verdict, or one that fails
validation, after one retry is recorded as "check failed", never guessed.

Isolation: OpenCode reads config, agents, plugins and AGENTS.md from the
directory it runs in and its parents, and the global config from
XDG_CONFIG_HOME. The run directory is a copy of .github/visual-judge from
the base branch, under --work, and XDG_CONFIG_HOME points at an empty
folder there, so nothing from the pull request (or the machine) configures
the judge. The input folder is a sibling, reachable only through the
agent's read/glob/grep permissions.

Needs OPENCODE_API_KEY (the OpenCode console key) unless --dry-run, and the
`opencode` CLI (--opencode to point at it). Python standard library only.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import time

VERDICTS = ("no_change", "intended_change", "possible_regression", "broken")
KINDS = (
    "clipped_text",
    "overlap",
    "missing_control",
    "misplaced_control",
    "misalignment",
    "colour",
    "layout",
    "other",
)
SEVERITIES = ("low", "medium", "high")
JUDGED = ("changed", "resized", "added")
# Never copied into the input folder (defence in depth: OpenCode doesn't load
# them from there anyway; see "Visual check" in docs/archive/2026-10-22/notes.md).
SKIP_NAMES = {"AGENTS.md", "opencode.json", "opencode.jsonc", ".opencode"}
MAX_DIFF_BYTES = 60_000


def validate(v, view):
    """The verdict, checked as strictly as the tool checks it, or an error."""
    if not isinstance(v, dict):
        return None, "not an object"
    allowed = {"view", "verdict", "issues", "summary", "ready_to_merge", "ready_reason"}
    if set(v) != allowed:
        return None, f"fields {sorted(v)} (expected {sorted(allowed)})"
    if v["view"] != view:
        return None, f"view {v['view']!r}, expected {view!r}"
    if v["verdict"] not in VERDICTS:
        return None, "bad verdict"
    if not isinstance(v["ready_to_merge"], bool):
        return None, "ready_to_merge is not a boolean"
    if v["ready_to_merge"] and v["verdict"] in ("possible_regression", "broken"):
        return None, "ready_to_merge with a regression"
    for field, cap in (("summary", 600), ("ready_reason", 200)):
        if not isinstance(v[field], str) or not v[field].strip() or len(v[field]) > cap:
            return None, f"bad {field}"
    issues = v["issues"]
    if not isinstance(issues, list) or len(issues) > 8:
        return None, "bad issues"
    for i in issues:
        if not isinstance(i, dict) or set(i) != {"kind", "where", "description", "severity"}:
            return None, "bad issue"
        if i["kind"] not in KINDS or i["severity"] not in SEVERITIES:
            return None, "bad issue kind or severity"
        for field, cap in (("where", 120), ("description", 400)):
            if not isinstance(i[field], str) or not i[field].strip() or len(i[field]) > cap:
                return None, f"bad issue {field}"
    if v["verdict"] in ("possible_regression", "broken") and not issues:
        return None, "a regression without issues"
    return v, None


def copy_tree(src, dst):
    """Copy regular files only (no links, no config-looking names)."""
    for root, dirs, files in os.walk(src):
        dirs[:] = [d for d in dirs if d not in SKIP_NAMES and not os.path.islink(os.path.join(root, d))]
        for name in files:
            path = os.path.join(root, name)
            if name in SKIP_NAMES or os.path.islink(path) or not os.path.isfile(path):
                continue
            target = os.path.join(dst, os.path.relpath(path, src))
            os.makedirs(os.path.dirname(target), exist_ok=True)
            shutil.copyfile(path, target)


def usage_of(jsonl):
    """Tokens and cost from `opencode run --format json` output."""
    total = {"input": 0, "output": 0, "reasoning": 0, "cache_read": 0, "cache_write": 0, "cost": 0.0, "steps": 0}
    tools = []
    for line in jsonl.splitlines():
        try:
            event = json.loads(line)
        except ValueError:
            continue
        part = event.get("part") or {}
        if part.get("type") == "step-finish":
            t = part.get("tokens") or {}
            total["input"] += t.get("input", 0)
            total["output"] += t.get("output", 0)
            total["reasoning"] += t.get("reasoning", 0)
            total["cache_read"] += (t.get("cache") or {}).get("read", 0)
            total["cache_write"] += (t.get("cache") or {}).get("write", 0)
            total["cost"] += part.get("cost") or 0.0
            total["steps"] += 1
        elif part.get("type") == "tool":
            tools.append(part.get("tool"))
    total["cost"] = round(total["cost"], 5)
    return total, tools


def prompt_for(view, size, scale, inp):
    return (
        f"View: {view} (a {size} window at {scale}x). The three attached images are, in order, "
        f"BEFORE (base branch), AFTER (this pull request) and DIFF (AFTER dimmed, changed pixels "
        f"in magenta). Input folder (read-only): {inp} with ui.diff, base/ and head/. "
        f"Judge this view and call deliver_verdict once with view \"{view}\"."
    )


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("artifact")
    p.add_argument("--judge", required=True, help="trusted .github/visual-judge folder")
    p.add_argument("--work", required=True, help="empty scratch folder")
    p.add_argument("--out", required=True)
    p.add_argument("--diff", help="the PR's unified diff of interface files")
    p.add_argument("--sources", help="folder with base/ and head/ copies of the changed interface files")
    p.add_argument("--model", default=os.environ.get("VISION_MODEL") or "opencode/claude-sonnet-5-5")
    p.add_argument("--max-views", type=int, default=15)
    p.add_argument("--timeout", type=int, default=300, help="seconds per run")
    p.add_argument("--opencode", default="opencode")
    p.add_argument("--keep-data", action="store_true", help="use this machine's OpenCode data (local tests)")
    p.add_argument("--only", action="append", default=[], help="judge only this view (VIEW-WxH); repeatable")
    p.add_argument("--dry-run", action="store_true")
    args = p.parse_args()

    with open(os.path.join(args.artifact, "manifest.json")) as f:
        manifest = json.load(f)
    work = os.path.realpath(args.work)
    os.makedirs(work, exist_ok=True)

    diff_text = ""
    if args.diff and os.path.exists(args.diff):
        with open(args.diff, "rb") as f:
            raw = f.read()
        diff_text = raw[:MAX_DIFF_BYTES].decode("utf-8", "replace")
        if len(raw) > MAX_DIFF_BYTES:
            diff_text += f"\n\n[truncated: the interface diff is {len(raw)} bytes; only the first {MAX_DIFF_BYTES} are shown]\n"
    if not diff_text.strip():
        diff_text = "(no interface code changed in this pull request)\n"

    candidates = [v for v in manifest["views"] if v.get("status") in JUDGED]
    if args.only:
        candidates = [v for v in candidates if f"{v['name']}-{v['size']}" in args.only]
    chosen, skipped = candidates[: args.max_views], candidates[args.max_views :]
    result = {
        "model": args.model,
        "views": [],
        "skipped_over_cap": [f"{v['name']}-{v['size']}" for v in skipped],
        "unchanged": sum(1 for v in manifest["views"] if v.get("status") == "unchanged"),
    }

    # The trusted run directory and an empty global config.
    run = os.path.join(work, "run")
    xdg = os.path.join(work, "xdg")
    for d in (run, xdg):
        shutil.rmtree(d, ignore_errors=True)
    shutil.copytree(args.judge, run, symlinks=False)
    os.makedirs(os.path.join(xdg, "config", "opencode"))
    with open(os.path.join(xdg, "config", "opencode", "opencode.json"), "w") as f:
        json.dump({"$schema": "https://opencode.ai/config.json", "update": "disable"}, f)
    agent_path = os.path.join(run, ".opencode", "agents", "judge.md")
    with open(agent_path) as f:
        agent_template = f.read()

    for v in chosen:
        view = f"{v['name']}-{v['size']}"
        inp = os.path.join(work, "input", view)
        shutil.rmtree(inp, ignore_errors=True)
        os.makedirs(inp)
        attach = []
        for side in ("base", "head", "diff"):
            src = os.path.join(args.artifact, "small", side, v["file"])
            if os.path.exists(src):
                shutil.copyfile(src, os.path.join(inp, f"{side}.png"))
                attach.append(os.path.join(inp, f"{side}.png"))
        with open(os.path.join(inp, "ui.diff"), "w") as f:
            f.write(diff_text)
        for side in ("base", "head"):
            os.makedirs(os.path.join(inp, side), exist_ok=True)
            if args.sources and os.path.isdir(os.path.join(args.sources, side)):
                copy_tree(os.path.join(args.sources, side), os.path.join(inp, side))

        # The agent may read only this view's input folder.
        with open(agent_path, "w") as f:
            f.write(agent_template.replace("@@INPUT@@", inp))
        out = os.path.join(work, f"verdict-{view}.json")
        tools_log = os.path.join(work, f"tools-{view}.log")
        env = {
            "PATH": os.environ.get("PATH", ""),
            "HOME": os.environ.get("HOME", work),
            "LANG": "en_US.UTF-8",
            "XDG_CONFIG_HOME": os.path.join(xdg, "config"),
            "VISUAL_JUDGE_OUT": out,
            "VISUAL_JUDGE_VIEW": view,
            "VISUAL_JUDGE_TOOLS_LOG": tools_log,
        }
        if not args.keep_data:
            for kind in ("DATA", "STATE", "CACHE"):
                env[f"XDG_{kind}_HOME"] = os.path.join(xdg, kind.lower())
        if os.environ.get("OPENCODE_API_KEY"):
            env["OPENCODE_API_KEY"] = os.environ["OPENCODE_API_KEY"]
        cmd = [args.opencode, "run", "--standalone", "--agent", "judge", "-m", args.model, "--format", "json"]
        for path in attach:
            cmd += ["--file", path]
        cmd.append(prompt_for(view, v["size"], v.get("scale", 2), inp))

        if args.dry_run:
            print(json.dumps({"view": view, "cwd": run, "cmd": cmd, "env": sorted(env)}, indent=2))
            result["views"].append({"view": view, "status": "dry-run"})
            continue
        if not os.environ.get("OPENCODE_API_KEY") and not args.keep_data:
            result["views"].append({"view": view, "status": "check failed", "error": "OPENCODE_API_KEY is not set"})
            continue

        entry = {"view": view, "status": "check failed", "attempts": 0, "usage": None}
        usage_total = None
        for attempt in (1, 2):
            entry["attempts"] = attempt
            if os.path.exists(out):
                os.remove(out)
            started = time.time()
            try:
                proc = subprocess.run(cmd, cwd=run, env=env, capture_output=True, text=True, timeout=args.timeout)
                stdout, code = proc.stdout, proc.returncode
            except subprocess.TimeoutExpired as e:
                stdout = e.stdout.decode() if isinstance(e.stdout, bytes) else (e.stdout or "")
                code = "timeout"
            usage, tools = usage_of(stdout)
            if usage_total is None:
                usage_total = usage
            else:
                for k in usage_total:
                    usage_total[k] = round(usage_total[k] + usage[k], 5)
            entry["seconds"] = round(time.time() - started, 1)
            entry["tools_called"] = tools
            verdict, why = None, f"no deliver_verdict call (opencode exit {code})"
            if os.path.exists(out):
                try:
                    with open(out) as f:
                        verdict, why = validate(json.load(f), view)
                except ValueError:
                    verdict, why = None, "the verdict file is not JSON"
            if verdict:
                entry.update(status="ok", verdict=verdict)
                entry.pop("error", None)
                break
            entry["error"] = why
        entry["usage"] = usage_total
        try:
            with open(tools_log) as f:
                entry["tools_offered"] = sorted({t for line in f for t in json.loads(line)})
        except (OSError, ValueError):
            entry["tools_offered"] = []
        result["views"].append(entry)
        print(f"{view}: {entry['status']} {entry.get('verdict', {}).get('verdict', entry.get('error'))} "
              f"(${(usage_total or {}).get('cost', 0)})")

    keys = ("input", "output", "reasoning", "cache_read", "cache_write", "cost")
    result["usage"] = {
        k: round(sum((v.get("usage") or {}).get(k, 0) for v in result["views"]), 5) for k in keys
    }
    with open(args.out, "w") as f:
        json.dump(result, f, indent=2)
    return 0


if __name__ == "__main__":
    sys.exit(main())
