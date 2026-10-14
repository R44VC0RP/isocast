#!/usr/bin/env python3
"""The trusted half of the visual check (.github/workflows/visual-report.yml):
it never runs pull request code, only reads the screenshot artifact and the
GitHub API.

    visual_report.py validate ARTIFACT_DIR
        Accept only the artifact's known layout: manifest.json and PNGs
        named VIEW-WxH.png under base/, head/, diff/ and small/*/, each a
        real PNG within size caps. Exits 1 (and says why) otherwise.

    visual_report.py pr --repo OWNER/NAME --pr N --head-sha SHA
            --diff OUT.diff --sources OUT_DIR
        Check that pull request N's head is SHA (the run is current), then
        write its unified diff limited to the interface files and copies of
        those files at the base and head commits, for the judge. Needs
        GH_TOKEN.

    visual_report.py comment ARTIFACT_DIR --repo OWNER/NAME --pr N
            --head-sha SHA --images-commit SHA --run-url URL
            [--judge judge.json] [--judge-skipped REASON]
            --markdown OUT.md --status OUT.json
        Write the pull request comment (marker <!-- isocast-visual -->) and
        the commit status (state and description) that follow from the
        manifest and the judge's verdicts. Everything the judge wrote is
        escaped and length-capped before it goes into Markdown.

Python standard library only.
"""

import argparse
import json
import os
import re
import struct
import sys
import urllib.error
import urllib.request

MARKER = "<!-- isocast-visual -->"
CONTEXT = "isocast/visual-check"
NAME = re.compile(r"^[a-z0-9]+(?:-[a-z0-9]+)*-\d{3,4}x\d{3,4}\.png$")
DIRS = ("base", "head", "diff", "small/base", "small/head", "small/diff")
MAX_FILE = 16 * 1024 * 1024
MAX_TOTAL = 256 * 1024 * 1024
MAX_FILES = 240
MAX_MANIFEST = 512 * 1024
MAX_SIDE = 4096
UI_PATHS = ("crates/isocast-ui/", "crates/isocast-app/ui/")
MAX_SOURCE_FILES = 40
MAX_SOURCE_BYTES = 200_000
SKIP_NAMES = {"AGENTS.md", "opencode.json", "opencode.jsonc"}


# ---------------------------------------------------------------- validate


def validate(root):
    problems = []
    total = 0
    count = 0
    for dirpath, dirnames, filenames in os.walk(root):
        rel = os.path.relpath(dirpath, root).replace(os.sep, "/")
        for d in dirnames:
            full = os.path.join(dirpath, d)
            sub = d if rel == "." else f"{rel}/{d}"
            if os.path.islink(full) or (sub not in DIRS and sub != "small"):
                problems.append(f"unexpected folder {sub}")
        dirnames[:] = [d for d in dirnames if not os.path.islink(os.path.join(dirpath, d))]
        for name in filenames:
            full = os.path.join(dirpath, name)
            path = name if rel == "." else f"{rel}/{name}"
            count += 1
            if os.path.islink(full) or not os.path.isfile(full):
                problems.append(f"{path}: not a regular file")
                continue
            size = os.path.getsize(full)
            total += size
            if path == "manifest.json":
                if size > MAX_MANIFEST:
                    problems.append("manifest.json is too big")
                continue
            if rel not in DIRS or not NAME.match(name):
                problems.append(f"unexpected file {path}")
                continue
            if size > MAX_FILE:
                problems.append(f"{path}: {size} bytes is over the cap")
                continue
            with open(full, "rb") as f:
                head = f.read(24)
            if head[:8] != b"\x89PNG\r\n\x1a\n" or head[12:16] != b"IHDR":
                problems.append(f"{path}: not a PNG")
                continue
            w, h = struct.unpack(">II", head[16:24])
            if not (0 < w <= MAX_SIDE and 0 < h <= MAX_SIDE):
                problems.append(f"{path}: {w}x{h} is out of range")
    if count > MAX_FILES:
        problems.append(f"{count} files is over the cap of {MAX_FILES}")
    if total > MAX_TOTAL:
        problems.append(f"{total} bytes in all is over the cap")
    manifest = None
    try:
        with open(os.path.join(root, "manifest.json")) as f:
            manifest = json.load(f)
    except (OSError, ValueError) as e:
        problems.append(f"manifest.json: {e}")
    if isinstance(manifest, dict):
        problems += check_manifest(manifest, root)
    elif manifest is not None:
        problems.append("manifest.json is not an object")
    return manifest, problems


def check_manifest(m, root):
    problems = []
    if m.get("version") != 1:
        problems.append("manifest version is not 1")
    if not isinstance(m.get("pr"), int) or m["pr"] <= 0:
        problems.append("manifest pr is not a positive number")
    for key in ("head_sha", "base_sha"):
        if not re.fullmatch(r"[0-9a-f]{40}", str(m.get(key, ""))):
            problems.append(f"manifest {key} is not a commit SHA")
    views = m.get("views")
    if not isinstance(views, list) or len(views) > 60:
        return problems + ["manifest views is not a list of at most 60"]
    for i, v in enumerate(views):
        if not isinstance(v, dict) or not NAME.match(str(v.get("file", ""))):
            problems.append(f"views[{i}]: bad file name")
            continue
        if v.get("status") not in ("changed", "unchanged", "resized", "added", "removed"):
            problems.append(f"views[{i}]: bad status")
        for key in ("base", "head", "diff"):
            if key in v and v[key] != f"{key}/{v['file']}":
                problems.append(f"views[{i}].{key}: unexpected path")
            elif key in v and not os.path.isfile(os.path.join(root, v[key])):
                problems.append(f"views[{i}].{key}: missing file")
        for key in ("changed_percent",):
            if key in v and not isinstance(v[key], (int, float)):
                problems.append(f"views[{i}].{key}: not a number")
    return problems


# ---------------------------------------------------------------------- pr


def api(path, accept="application/vnd.github+json"):
    req = urllib.request.Request(
        f"https://api.github.com/{path}",
        headers={
            "Accept": accept,
            "Authorization": f"Bearer {os.environ['GH_TOKEN']}",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    with urllib.request.urlopen(req, timeout=30) as r:
        return r.read()


def fetch_pr(args):
    pr = json.loads(api(f"repos/{args.repo}/pulls/{args.pr}"))
    if pr["head"]["sha"] != args.head_sha:
        print(f"pull request {args.pr} has moved on to {pr['head']['sha']}; not reporting on {args.head_sha}")
        return 3
    base_sha = pr["base"]["sha"]
    files = []
    page = 1
    while page <= 30:
        batch = json.loads(api(f"repos/{args.repo}/pulls/{args.pr}/files?per_page=100&page={page}"))
        files += batch
        if len(batch) < 100:
            break
        page += 1
    ui = [f for f in files if f["filename"].startswith(UI_PATHS)]
    parts = []
    for f in ui:
        old = f.get("previous_filename", f["filename"])
        patch = f.get("patch")
        parts.append(f"diff --git a/{old} b/{f['filename']}\n--- a/{old}\n+++ b/{f['filename']}\n")
        parts.append((patch + "\n") if patch else "(binary or too large to show)\n")
    with open(args.diff, "w") as out:
        out.write("".join(parts))
    for f in ui[:MAX_SOURCE_FILES]:
        name = f["filename"]
        if os.path.basename(name) in SKIP_NAMES or ".opencode/" in name or ".." in name.split("/"):
            continue
        for side, ref, path in (
            ("base", base_sha, f.get("previous_filename", name)),
            ("head", args.head_sha, name),
        ):
            if (side == "base" and f["status"] == "added") or (side == "head" and f["status"] == "removed"):
                continue
            try:
                body = api(
                    f"repos/{args.repo}/contents/{urllib.request.quote(path)}?ref={ref}",
                    accept="application/vnd.github.raw",
                )
            except urllib.error.HTTPError:
                continue
            if len(body) > MAX_SOURCE_BYTES:
                continue
            target = os.path.join(args.sources, side, path)
            os.makedirs(os.path.dirname(target), exist_ok=True)
            with open(target, "wb") as out:
                out.write(body)
    print(f"{len(ui)} interface files changed of {len(files)}")
    return 0


# ----------------------------------------------------------------- comment


def esc(text, cap):
    """Untrusted text as inert Markdown: one line, capped, nothing that
    renders as a link, mention, image, HTML or table syntax."""
    text = re.sub(r"\s+", " ", str(text)).strip()
    if len(text) > cap:
        text = text[: cap - 1].rstrip() + "…"
    text = text.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
    text = re.sub(r"([\\`*_{}\[\]()#+!|~])", r"\\\1", text)
    text = text.replace("@", "&#64;").replace("://", ":&#8203;//")
    return text


VERDICT_LABEL = {
    "no_change": "no change",
    "intended_change": "intended change",
    "possible_regression": "**possible regression**",
    "broken": "**broken**",
}


def decide(manifest, judge, judge_skipped):
    """(state, description) for the commit status."""
    judged = [v for v in manifest["views"] if v["status"] in ("changed", "resized", "added")]
    if manifest.get("broken"):
        return "error", "Some screenshots look broken (blank render)"
    if not judged:
        return "success", "No visual changes"
    if judge is None:
        return "error", f"Visual changes not judged: {judge_skipped or 'no judge result'}"[:140]
    results = {v["view"]: v for v in judge.get("views", [])}
    failed, bad, not_ready = [], [], []
    for v in judged:
        r = results.get(f"{v['name']}-{v['size']}")
        if not r or r.get("status") != "ok":
            failed.append(v)
            continue
        verdict = r["verdict"]
        if verdict["verdict"] in ("possible_regression", "broken"):
            bad.append(v)
        elif not verdict["ready_to_merge"]:
            not_ready.append(v)
    if bad:
        return "failure", f"{len(bad)} view(s) look like a regression"
    if failed:
        return "error", f"The judge gave no valid verdict for {len(failed)} view(s)"
    if not_ready:
        return "failure", f"The judge says {len(not_ready)} view(s) aren't ready to merge"
    return "success", f"{len(judged)} changed view(s), all judged fine"


def comment(args):
    with open(os.path.join(args.artifact, "manifest.json")) as f:
        manifest = json.load(f)
    judge = None
    if args.judge and os.path.exists(args.judge):
        with open(args.judge) as f:
            judge = json.load(f)
    state, description = decide(manifest, judge, args.judge_skipped)
    raw = f"https://raw.githubusercontent.com/{args.repo}/{args.images_commit}/pr/{args.pr}/{args.head_sha}"
    tree = f"https://github.com/{args.repo}/tree/{args.images_commit}/pr/{args.pr}/{args.head_sha}"
    results = {v["view"]: v for v in (judge or {}).get("views", [])}

    def img(view, side):
        if side not in view:
            return "—"
        return f'<a href="{raw}/{view[side]}"><img src="{raw}/small/{view[side]}" width="260"></a>'

    def row(view):
        key = f"{view['name']}-{view['size']}"
        pct = view.get("changed_percent")
        changed = f"{pct:.3f}%" if isinstance(pct, (int, float)) else view["status"]
        r = results.get(key)
        if r and r.get("status") == "ok":
            verdict = VERDICT_LABEL[r["verdict"]["verdict"]]
        elif r:
            verdict = "check failed"
        else:
            verdict = "—"
        return f"| `{key}` | {img(view, 'base')} | {img(view, 'head')} | {img(view, 'diff')} | {changed} | {verdict} |"

    head = "| View | Before | After | Diff | Changed | Judge |\n|---|---|---|---|---|---|"
    changed = [v for v in manifest["views"] if v["status"] != "unchanged"]
    same = [v for v in manifest["views"] if v["status"] == "unchanged"]
    icon = {"success": "✅", "failure": "❌", "error": "⚠️"}[state]
    lines = [
        MARKER,
        "## Visual check",
        "",
        f"{icon} {description} · head `{args.head_sha[:10]}` vs base `{manifest['base_sha'][:10]}` · "
        f"[run]({args.run_url}) · [all images]({tree})",
        "",
    ]
    if changed:
        lines += [head] + [row(v) for v in changed] + [""]
    if same:
        names = ", ".join(f"`{v['name']}-{v['size']}`" for v in same)
        lines += [
            f"<details><summary>{len(same)} unchanged view(s)</summary>",
            "",
            names,
            "",
            "</details>",
            "",
        ]
    if manifest.get("broken"):
        lines += ["**Broken renders:** " + esc("; ".join(manifest["broken"]), 600), ""]

    if judge is not None:
        lines += ["### Judge", ""]
        for v in changed:
            r = results.get(f"{v['name']}-{v['size']}")
            if not r:
                continue
            key = f"{v['name']}-{v['size']}"
            if r.get("status") != "ok":
                lines.append(f"- `{key}`: check failed ({esc(r.get('error', 'no verdict'), 160)})")
                continue
            vd = r["verdict"]
            ready = "ready" if vd["ready_to_merge"] else "not ready"
            lines.append(
                f"- `{key}`: {VERDICT_LABEL[vd['verdict']]}, {ready}. {esc(vd['summary'], 400)} "
                f"*({esc(vd['ready_reason'], 200)})*"
            )
            for issue in vd["issues"][:8]:
                lines.append(
                    f"  - {esc(issue['severity'], 10)} · {esc(issue['kind'], 30)} · "
                    f"{esc(issue['where'], 120)}: {esc(issue['description'], 300)}"
                )
        if judge.get("skipped_over_cap"):
            lines.append(f"- Not judged (over the per-run cap): {esc(', '.join(judge['skipped_over_cap']), 400)}")
        u = judge.get("usage", {})
        lines += [
            "",
            "<details><summary>Judge usage</summary>",
            "",
            f"Model `{esc(judge.get('model', '?'), 60)}`; {len(judge.get('views', []))} run(s); "
            f"input {u.get('input', 0)}, output {u.get('output', 0)}, reasoning {u.get('reasoning', 0)}, "
            f"cache read {u.get('cache_read', 0)}, cache write {u.get('cache_write', 0)} tokens; "
            f"about ${u.get('cost', 0):.4f}.",
            "",
            "</details>",
            "",
        ]
    elif args.judge_skipped:
        lines += [f"*Judge not run: {esc(args.judge_skipped, 200)}.*", ""]
    lines.append(
        "<sub>Offscreen renders with synthetic sources; a pixel counts as changed above "
        f"{manifest.get('tolerance', 8)}/255 per channel. The judge is advisory and sees only these images "
        "and the interface diff.</sub>"
    )
    with open(args.markdown, "w") as f:
        f.write("\n".join(lines) + "\n")
    with open(args.status, "w") as f:
        json.dump({"context": CONTEXT, "state": state, "description": description[:140]}, f)
    print(f"{state}: {description}")
    return 0


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = p.add_subparsers(dest="cmd", required=True)
    v = sub.add_parser("validate")
    v.add_argument("artifact")
    r = sub.add_parser("pr")
    r.add_argument("--repo", required=True)
    r.add_argument("--pr", type=int, required=True)
    r.add_argument("--head-sha", required=True)
    r.add_argument("--diff", required=True)
    r.add_argument("--sources", required=True)
    c = sub.add_parser("comment")
    c.add_argument("artifact")
    c.add_argument("--repo", required=True)
    c.add_argument("--pr", type=int, required=True)
    c.add_argument("--head-sha", required=True)
    c.add_argument("--images-commit", required=True)
    c.add_argument("--run-url", required=True)
    c.add_argument("--judge")
    c.add_argument("--judge-skipped")
    c.add_argument("--markdown", required=True)
    c.add_argument("--status", required=True)
    args = p.parse_args()
    if args.cmd == "validate":
        manifest, problems = validate(args.artifact)
        if problems:
            print("visual_report: the artifact was rejected:", *problems[:30], sep="\n  ", file=sys.stderr)
            return 1
        print(f"artifact ok: {len(manifest['views'])} views, PR {manifest['pr']}")
        return 0
    if args.cmd == "pr":
        return fetch_pr(args)
    return comment(args)


if __name__ == "__main__":
    sys.exit(main())
