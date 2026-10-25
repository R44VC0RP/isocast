#!/usr/bin/env python3
"""Pull requests for lanes: open them, list them, land them.

    scripts/pr.sh [--draft | --ready] [--report FILE] [--title T] [--hw H]
                  [--area A]… [--session ID] [--allow-workflow-changes]
    scripts/prs.sh [--json]
    scripts/merge-pr.sh [--dry-run] [--hw N=EVIDENCE]… [--notes REF] N [N…]

Implementers commit on their agent/<lane> branch and run pr.sh: it publishes
the branch merged with main as one scanned snapshot on ci/<lane> (ci.sh
--merge-main --no-wait; the public repo never sees private history) and
opens or updates the pull request ci/<lane> -> main, its description built
from the lane's REPORT.md. CI runs from that push. The description carries
a hidden marker with the lane's session id and branch, which prs.sh shows
so a coordinator can route review comments back to the implementer.

merge-pr.sh lands pull requests into the local main (main stays
local-first): every PR's latest snapshot must be green on CI, Engine E2E
and Windows, built from its branch's current tip, and contain the current
main, or it is re-published with --merge-main and waited on. Several PRs
are tested as ONE candidate (one CI run); if it is red, it is split in
halves and each half is landed the same way. Landing merges each branch
with --no-ff and a message built from its PR, checks that the result is
exactly the tree CI tested, runs `cargo check --workspace --all-targets
--locked` on a build host, fast-forwards main, publishes it
(publish-snapshot.sh --push), comments with the published commit, closes
the PR and deletes its ci/ branch. The merger writes the PRs' STATE notes
as one docs-only commit and passes it as --notes: it rides in the same
candidate and lands in the same publish, so main moves once per batch
(a standalone commit on main would make every open PR stale).

REPORT.md (default /Volumes/IsocastBuild/<lane>/scratch/REPORT.md):

    # Title of the pull request
    ## What and why
    ## How it was verified
    ## Hardware needed        none | windows-gpu | mac | linux-gpu, then detail
    ## STATE notes            optional: lines for docs/STATE.md (the merger applies them)
    ## Area                   optional: engine, ui, plugins, streaming, ci, release
    ## Linked issue           optional
    ## Screenshots            optional (required for UI changes by PR checks)
"""

import argparse
import datetime
import fcntl
import json
import os
import re
import subprocess
import sys
import tempfile
import time

REPO = "R44VC0RP/isocast"
REMOTE = f"https://github.com/{REPO}.git"
WORKFLOWS = ("CI", "Engine E2E", "Windows")
HARDWARE = ("none", "windows-gpu", "mac", "linux-gpu")
AREAS = ("engine", "ui", "plugins", "streaming", "ci", "release")
MARKER = re.compile(r"<!--\s*isocast-pr\s+(\{.*?\})\s*-->", re.DOTALL)
QA_VERDICT = re.compile(r"^\s*\**QA:\s*(approved|changes requested)\b", re.IGNORECASE)
LANE_RE = re.compile(r"^(?![.-])[A-Za-z0-9._-]+(?<![.-])$")
SCRIPTS = os.path.dirname(os.path.abspath(__file__))
CI_DIR = os.environ.get("ISOCAST_CI_DIR", "/Volumes/IsocastBuild/ci")
MERGE_DIR = os.environ.get("ISOCAST_MERGE_DIR", "/Volumes/IsocastBuild/merge")
PUBLIC_DIR = os.environ.get("ISOCAST_PUBLIC_DIR", os.path.expanduser("~/dev/isocast-public"))
# A neutral directory for gh, so it never reads a local checkout's remotes.
GH_CWD = tempfile.gettempdir()


class Refused(Exception):
    pass


def log(msg):
    print(msg, file=sys.stderr, flush=True)


def run(cmd, cwd=None, check=True, env=None):
    p = subprocess.run(cmd, cwd=cwd, env=env, text=True, capture_output=True)
    if check and p.returncode != 0:
        raise Refused(f"{' '.join(cmd[:4])}… failed ({p.returncode}): {(p.stderr or p.stdout).strip()[-800:]}")
    return p.stdout.strip()


def git(*args, cwd=None, check=True):
    return run(["git", *args], cwd=cwd or SCRIPTS, check=check)


def rev(ref, cwd=None):
    return git("rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}", cwd=cwd, check=False) or None


def gh(*args, check=True):
    return run(["gh", *args], cwd=GH_CWD, check=check)


def gh_json(*args):
    out = gh(*args)
    return json.loads(out) if out else None


# ---------------------------------------------------------------- text ----

def strip_comments(text):
    return re.sub(r"<!--.*?-->", "", text or "", flags=re.DOTALL).strip()


def sections(text):
    """`## heading` -> body, lower-cased headings, plus the `# title`."""
    out, title, current = {}, None, None
    for line in text.splitlines():
        m = re.match(r"^\s{0,3}(#{1,6})\s+(.*?)\s*#*\s*$", line)
        if m and len(m.group(1)) == 1 and title is None and current is None:
            title = m.group(2).strip()
            continue
        if m and len(m.group(1)) == 2:
            current = m.group(2).strip().lower()
            out[current] = []
            continue
        if current is not None:
            out[current].append(line)
    return title, {k: "\n".join(v).strip() for k, v in out.items()}


def section(secs, name):
    for key, body in secs.items():
        if key.startswith(name):
            return body
    return None


# Template leftovers: a line that is only TODO/TBD/…, "<what changed>"-style
# multi-word brackets (single words like ci/<lane> are notation), {{…}}.
PLACEHOLDER = re.compile(r"^\s*[-*]?\s*(TODO|TBD|FIXME|XXX|\.\.\.|…)\s*$|<[a-z]+ [a-z ,.'/-]+>|\{\{.*?\}\}",
                         re.MULTILINE | re.IGNORECASE)


def real_text(text, what):
    """Text that is there and isn't a placeholder, or Refused."""
    text = strip_comments(text)
    if not text:
        raise Refused(f"{what} is empty")
    m = PLACEHOLDER.search(text)
    if m:
        raise Refused(f"{what} has a placeholder ({m.group(0)!r}); write the real text")
    return text


def scan_text(text, what):
    """publish-snapshot.sh's secret and attribution scan, on text bound for GitHub."""
    with tempfile.TemporaryDirectory() as d:
        with open(os.path.join(d, "text.md"), "w") as f:
            f.write(text)
        p = subprocess.run([os.path.join(SCRIPTS, "publish-snapshot.sh"), "--scan-only", d],
                           text=True, capture_output=True)
        if p.returncode != 0:
            raise Refused(f"{what} fails the secret/attribution scan: {p.stderr.strip()}")


def marker_of(body):
    m = MARKER.search(body or "")
    if not m:
        return None
    try:
        return json.loads(m.group(1))
    except json.JSONDecodeError:
        return None


def trailers_of(message):
    out = {"include": []}
    for line in (message or "").splitlines():
        m = re.match(r"^Isocast-(Source|Include|Main|Tree|Dirty):\s*(\S+)\s*$", line)
        if m:
            key = m.group(1).lower()
            if key == "include":
                out["include"].append(m.group(2))
            else:
                out[key] = m.group(2)
    return out


def age(stamp):
    t = datetime.datetime.fromisoformat(stamp.replace("Z", "+00:00"))
    s = (datetime.datetime.now(datetime.timezone.utc) - t).total_seconds()
    return f"{int(s // 60)}m" if s < 3600 else f"{s / 3600:.1f}h" if s < 172800 else f"{int(s // 86400)}d"


# ------------------------------------------------------------ CI state ----

def snapshot_runs(sha):
    """The newest push run of each workflow on a snapshot: name -> run dict."""
    runs = gh_json("run", "list", "--repo", REPO, "--commit", sha, "--event", "push", "--limit", "50",
                   "--json", "databaseId,workflowName,status,conclusion,headBranch,url,createdAt") or []
    newest = {}
    for r in sorted(runs, key=lambda r: r["createdAt"]):
        newest[r["workflowName"]] = r
    return newest


def ci_state(runs):
    """'green', 'red', 'pending', 'missing' or 'cancelled' over WORKFLOWS, plus a
    per-workflow summary. A cancelled run is no verdict (a newer snapshot or a
    batch candidate superseded it), never red."""
    marks, states = [], []
    for w in WORKFLOWS:
        r = runs.get(w)
        if r is None:
            marks.append("·"); states.append("missing")
        elif r["status"] != "completed":
            marks.append("●"); states.append("pending")
        elif r["conclusion"] == "success":
            marks.append("✓"); states.append("green")
        elif r["conclusion"] == "cancelled":
            marks.append("○"); states.append("cancelled")
        else:
            marks.append("✗"); states.append("red")
    for s in ("red", "pending", "missing", "cancelled"):
        if s in states:
            return s, "".join(marks)
    return "green", "".join(marks)


# ------------------------------------------------------------------ pr ----

def area_from_paths(paths):
    found = set()
    for p in paths:
        if p.startswith(("crates/isocast-ui", "crates/isocast-app")):
            found.add("ui")
        elif p.startswith(("crates/isocast-plugin", "examples/plugins")):
            found.add("plugins")
        elif p.startswith(("scripts/release", "scripts/sparkle", "scripts/dmg", "scripts/package-")):
            found.add("release")
        elif p.startswith((".github/", "scripts/")):
            found.add("ci")
        elif p.startswith("crates/") or p.startswith("Cargo."):
            found.add("engine")
    return sorted(found) or (["documentation"] if all(p.endswith(".md") for p in paths) else ["engine"])


def ensure_labels(names):
    have = {l["name"] for l in gh_json("label", "list", "--repo", REPO, "--limit", "200", "--json", "name")}
    for name in names:
        if name not in have:
            color = "ededed" if name == "needs:none" else "c5def5"
            desc = "No hardware qualification needed" if name == "needs:none" else \
                f"QA qualifies it on {name[6:]} hardware (hw-batch) before merge"
            gh("label", "create", name, "--repo", REPO, "--color", color, "--description", desc)


def cmd_pr(a):
    top = git("rev-parse", "--show-toplevel", cwd=os.getcwd())
    branch = git("branch", "--show-current", cwd=top)
    if not branch or branch == "main":
        raise Refused(f"pr: run from a lane's branch, not {branch or 'a detached HEAD'}")
    lane = branch.rsplit("/", 1)[-1]
    if not LANE_RE.match(lane) or lane.endswith(".lock"):
        raise Refused(f"pr: branch {branch} doesn't make a valid ci/<lane> name")
    if git("status", "--porcelain", cwd=top):
        raise Refused("pr: the worktree has uncommitted changes; commit them (pr.sh publishes commits only)")
    source = rev("HEAD", cwd=top)
    commits = git("log", "--no-merges", "--format=- %h %s", f"main..{source}", cwd=top)
    if not commits:
        raise Refused(f"pr: {branch} has no commits that main doesn't have")
    report = a.report or f"/Volumes/IsocastBuild/{lane}/scratch/REPORT.md"
    if not os.path.isfile(report):
        raise Refused(f"pr: no REPORT at {report} (pass --report FILE); its format is in scripts/pr_flow.py")
    title, secs = sections(open(report).read())
    title = a.title or title
    if not title:
        raise Refused(f"pr: {report} needs a '# Title' line (or pass --title)")
    real_text(title, "the title")
    what = real_text(section(secs, "what and why") or "", f"{report} '## What and why'")
    verified = real_text(section(secs, "how it was verified") or "", f"{report} '## How it was verified'")
    hw_text = a.hw or strip_comments(section(secs, "hardware needed") or "")
    hw = hw_text.split()[0].strip("`*").lower() if hw_text else ""
    if hw not in HARDWARE:
        raise Refused(f"pr: '## Hardware needed' (or --hw) must start with one of {', '.join(HARDWARE)}; got {hw!r}")
    hw_detail = hw_text.split(None, 1)[1].strip(" -—:") if len(hw_text.split(None, 1)) > 1 else ""
    state = strip_comments(section(secs, "state notes") or "")
    if state:
        real_text(state, f"{report} '## STATE notes'")
    areas = a.area or [x.strip().lower().removeprefix("area:") for x in
                       re.split(r"[,\s]+", strip_comments(section(secs, "area") or "")) if x.strip()]
    if not areas:
        paths = git("diff", "--name-only", f"main...{source}", cwd=top).splitlines()
        areas = area_from_paths(paths)
    for x in areas:
        if x not in AREAS and x != "documentation":
            raise Refused(f"pr: unknown area {x!r}; valid: {', '.join(AREAS)}")
    session = a.session or os.environ.get("OPENCODE_SESSION_ID") or os.environ.get("CLAUDE_SESSION_ID")
    if not session:
        raise Refused("pr: no session id (OPENCODE_SESSION_ID isn't set); pass --session ID")
    labels = [x if x == "documentation" else f"area:{x}" for x in areas] + [f"needs:{hw}"]

    def body(snapshot, trail):
        parts = []
        if strip_comments(section(secs, "linked issue") or ""):
            parts += ["## Linked issue", "", strip_comments(section(secs, "linked issue")), ""]
        parts += ["## What and why", "", what, "", "## How it was verified", "", verified, ""]
        if strip_comments(section(secs, "screenshots") or ""):
            parts += ["## Screenshots", "", strip_comments(section(secs, "screenshots")), ""]
        parts += ["## Hardware needed", "", f"`{hw}`" + (f": {hw_detail}" if hw_detail else ""), "",
                  "## STATE notes", "", state or "None.", "",
                  "## Commits", "", commits, "",
                  "## Snapshot", "",
                  f"`{snapshot[:12]}` on `ci/{lane}`: `{branch}` at `{source[:12]}`, containing main "
                  f"`{(trail.get('main') or '')[:12]}`. CI, Engine E2E and Windows run from the push.", "",
                  "<!-- isocast-pr " + json.dumps({"session": session, "branch": branch, "lane": lane}) + " -->"]
        return "\n".join(parts) + "\n"

    scan_text(title + "\n" + body("0" * 40, {}), "the pull request")
    if a.dry_run:
        print(body("0" * 40, {}))
        print(f"pr: dry run: title {title!r}, labels {labels}; nothing published")
        return 0

    cmd = [os.path.join(SCRIPTS, "ci.sh"), "--source", source, "--merge-main", "--no-wait", "--lane", lane]
    if a.allow_workflow_changes:
        cmd.append("--allow-workflow-changes")
    p = subprocess.run(cmd, cwd=top)
    if p.returncode != 0:
        raise Refused(f"pr: ci.sh couldn't publish the candidate (exit {p.returncode})")
    snapshot = open(os.path.join(CI_DIR, lane, "pushed")).read().strip()
    msg = gh_json("api", f"repos/{REPO}/commits/{snapshot}", "--jq", "{m: .commit.message}")["m"]
    text = body(snapshot, trailers_of(msg))
    scan_text(text, "the pull request")
    ensure_labels(labels)
    with tempfile.NamedTemporaryFile("w", suffix=".md", delete=False) as f:
        f.write(text)
    try:
        open_prs = gh_json("pr", "list", "--repo", REPO, "--head", f"ci/{lane}", "--state", "open",
                           "--json", "number,url,isDraft,labels")
        if open_prs:
            pr = open_prs[0]
            stale = [l["name"] for l in pr["labels"]
                     if l["name"].startswith(("needs:", "area:")) and l["name"] not in labels]
            args = ["pr", "edit", str(pr["number"]), "--repo", REPO, "--title", title, "--body-file", f.name]
            for l in labels:
                args += ["--add-label", l]
            for l in stale:
                args += ["--remove-label", l]
            gh(*args)
            if a.ready and pr["isDraft"]:
                gh("pr", "ready", str(pr["number"]), "--repo", REPO)
            elif a.draft and not pr["isDraft"]:
                gh("pr", "ready", str(pr["number"]), "--repo", REPO, "--undo")
            url = pr["url"]
            log(f"pr: updated #{pr['number']} with snapshot {snapshot[:12]}")
        else:
            args = ["pr", "create", "--repo", REPO, "--base", "main", "--head", f"ci/{lane}",
                    "--title", title, "--body-file", f.name]
            for l in labels:
                args += ["--label", l]
            if not a.ready:
                args.append("--draft")
            url = gh(*args).splitlines()[-1]
            log(f"pr: opened {'' if a.ready else 'draft '}pull request with snapshot {snapshot[:12]}")
    finally:
        os.unlink(f.name)
    print(url)
    return 0


# ----------------------------------------------------------------- prs ----

PRS_QUERY = """
query($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) {
    pullRequests(states: OPEN, baseRefName: "main", first: 100, orderBy: {field: CREATED_AT, direction: ASC}) {
      nodes {
        number title url isDraft body createdAt headRefName headRefOid reviewDecision
        labels(first: 20) { nodes { name } }
        comments(last: 30) { nodes { body createdAt } }
        reviews(last: 30) { nodes { body submittedAt } }
        commits(last: 1) { nodes { commit { oid message } } }
      }
    }
  }
}"""


def cmd_prs(a):
    owner, name = REPO.split("/")
    data = gh_json("api", "graphql", "-F", f"owner={owner}", "-F", f"name={name}", "-f", "query=" + PRS_QUERY)
    prs = data["data"]["repository"]["pullRequests"]["nodes"]
    runs = gh_json("run", "list", "--repo", REPO, "--event", "push", "--limit", "400",
                   "--json", "headSha,workflowName,status,conclusion,createdAt,url,databaseId,headBranch") or []
    main = rev("main")
    rows = []
    for pr in prs:
        pr["labels"] = pr["labels"]["nodes"]
        pr["comments"] = pr["comments"]["nodes"]
        pr["reviews"] = pr["reviews"]["nodes"]
        mk = marker_of(pr["body"]) or {}
        trail = {}
        last = (pr["commits"]["nodes"] or [{}])[-1].get("commit") or {}
        if last.get("oid") == pr["headRefOid"]:
            trail = trailers_of(last.get("message", ""))
        newest = {}
        for r in sorted((r for r in runs if r["headSha"] == pr["headRefOid"]), key=lambda r: r["createdAt"]):
            newest[r["workflowName"]] = r
        state, marks = ci_state(newest)
        review = {"APPROVED": "approved", "CHANGES_REQUESTED": "changes"}.get(pr["reviewDecision"] or "")
        if not review:
            notes = [(c["createdAt"], c["body"]) for c in pr["comments"]] + \
                    [(r["submittedAt"], r["body"]) for r in pr["reviews"] if r.get("submittedAt")]
            for _, b in sorted(notes, reverse=True):
                m = QA_VERDICT.match(b or "")
                if m:
                    review = "approved" if m.group(1).lower() == "approved" else "changes"
                    break
        hw = ",".join(l["name"][6:] for l in pr["labels"] if l["name"].startswith("needs:")) or "?"
        branch_tip = rev(mk["branch"]) if mk.get("branch") else None
        fresh = "-" if not trail else (
            "stale" if trail.get("main") != main else
            "moved" if branch_tip and trail.get("source") != branch_tip else "ok")
        rows.append({"number": pr["number"], "title": pr["title"], "draft": pr["isDraft"], "ci": state,
                     "ci_marks": marks, "main": fresh, "review": review or "-", "hardware": hw,
                     "age": age(pr["createdAt"]), "session": mk.get("session"), "branch": mk.get("branch"),
                     "url": pr["url"]})
    if a.json:
        print(json.dumps(rows, indent=2))
        return 0
    if not rows:
        print("no open pull requests")
        return 0
    print(f"{'#':>4}  {'CEW':3}  {'ci':7}  {'main':5}  {'review':8}  {'hw':11}  {'age':5}  "
          f"{'session':30}  {'branch':24}  title")
    for r in rows:
        print(f"{r['number']:>4}  {r['ci_marks']:3}  {r['ci']:7}  {r['main']:5}  {r['review']:8}  "
              f"{r['hardware']:11}  {r['age']:5}  {(r['session'] or '-'):30}  {(r['branch'] or '-'):24}  "
              f"{'[draft] ' if r['draft'] else ''}{r['title']}")
    print("CEW: CI, Engine E2E, Windows (✓ green, ✗ red, ● running, ○ cancelled, · none). main: ok, stale (older main), "
          "moved (branch has commits since its snapshot).")
    return 0


# --------------------------------------------------------------- merge ----

def load(number):
    pr = gh_json("pr", "view", str(number), "--repo", REPO, "--json",
                 "number,title,body,url,state,isDraft,baseRefName,headRefName,headRefOid,labels")
    n = f"#{number}"
    if pr["state"] != "OPEN":
        raise Refused(f"{n} is {pr['state'].lower()}")
    if pr["isDraft"]:
        raise Refused(f"{n} is a draft; the implementer marks it ready (pr.sh --ready)")
    if pr["baseRefName"] != "main" or not pr["headRefName"].startswith("ci/"):
        raise Refused(f"{n} must be ci/<lane> -> main (it is {pr['headRefName']} -> {pr['baseRefName']})")
    mk = marker_of(pr["body"])
    if not mk or not mk.get("branch"):
        raise Refused(f"{n} has no isocast-pr marker; it wasn't opened by pr.sh")
    pr["marker"] = mk
    pr["lane"] = pr["headRefName"][3:]
    _, secs = sections(pr["body"])
    pr["what"] = real_text(section(secs, "what and why") or "", f"{n}'s 'What and why'")
    pr["verified"] = real_text(section(secs, "how it was verified") or "", f"{n}'s 'How it was verified'")
    real_text(pr["title"], f"{n}'s title")
    state = strip_comments(section(secs, "state notes") or "")
    pr["state_notes"] = "" if state.rstrip(".").lower() == "none" else state
    pr["hw"] = [l["name"][6:] for l in pr["labels"] if l["name"].startswith("needs:")]
    if len(pr["hw"]) != 1 or pr["hw"][0] not in HARDWARE:
        raise Refused(f"{n} needs exactly one needs:<{'|'.join(HARDWARE)}> label (has {pr['hw']})")
    pr["hw"] = pr["hw"][0]
    pr["tip"] = rev(mk["branch"])
    if not pr["tip"]:
        raise Refused(f"{n}'s branch {mk['branch']} isn't in this repo")
    refresh(pr)
    if pr["trail"].get("source") != pr["tip"]:
        raise Refused(f"{n}: {mk['branch']} ({pr['tip'][:12]}) has moved since its snapshot "
                      f"(built from {(pr['trail'].get('source') or 'unknown')[:12]}); the implementer runs pr.sh")
    return pr


def refresh(pr, snapshot=None):
    """Read the PR's latest snapshot (or SNAPSHOT) and its provenance trailers."""
    snapshot = snapshot or gh_json("pr", "view", str(pr["number"]), "--repo", REPO,
                                   "--json", "headRefOid")["headRefOid"]
    msg = gh_json("api", f"repos/{REPO}/commits/{snapshot}", "--jq", "{m: .commit.message}")["m"]
    pr["snapshot"], pr["trail"] = snapshot, trailers_of(msg)
    if pr["trail"].get("dirty"):
        raise Refused(f"#{pr['number']}'s snapshot has uncommitted edits (Isocast-Dirty); run pr.sh")
    if not pr["trail"].get("tree") or not pr["trail"].get("source"):
        raise Refused(f"#{pr['number']}'s snapshot {snapshot[:12]} has no provenance trailers; run pr.sh again")


def ci_wait(lane, snapshot):
    """ci.sh --wait: True when every workflow passed on SNAPSHOT."""
    log(f"merge: waiting for CI on {snapshot[:12]} (ci/{lane})")
    return subprocess.run([os.path.join(SCRIPTS, "ci.sh"), "--wait", snapshot, "--lane", lane]).returncode == 0


# Set by merge-pr --allow-workflow-changes: QA reviewed the PRs' .github/ changes.
ALLOW_WORKFLOWS = []


def ci_publish(lane, source, includes=(), superseded=()):
    """ci.sh --merge-main, then wait: (green, snapshot). Once the candidate is
    pushed, the macOS runs still going on SUPERSEDED snapshots (PRs whose own
    CI the candidate replaces) are cancelled: macOS runners are the budget."""
    cmd = [os.path.join(SCRIPTS, "ci.sh"), "--source", source, "--merge-main", "--lane", lane,
           "--no-wait"] + ALLOW_WORKFLOWS
    for inc in includes:
        cmd += ["--include", inc]
    log(f"merge: publishing ci/{lane}: {' '.join(cmd[1:])}")
    pushed = os.path.join(CI_DIR, lane, "pushed")
    if os.path.exists(pushed):
        os.unlink(pushed)
    code = subprocess.run(cmd).returncode
    snapshot = open(pushed).read().strip() if os.path.exists(pushed) else None
    if code != 0 or not snapshot:
        return False, snapshot
    for sha in superseded:
        for name, r in snapshot_runs(sha).items():
            if name in MACOS_WORKFLOWS and r["status"] != "completed":
                log(f"merge: cancelling {name} on superseded snapshot {sha[:12]} ({r['url']})")
                gh("run", "cancel", str(r["databaseId"]), "--repo", REPO, check=False)
    return ci_wait(lane, snapshot), snapshot


# Snapshot -> what failed first and passed on a re-run, for merge messages.
RERUN_NOTES = {}

# --known-flake: hosted jobs whose failure QA may accept when the PR's paths
# can't affect them. job -> (workflow, job name prefix, ci_gate workflow, ci_gate job).
FLAKE_JOBS = {
    "e2e": ("Engine E2E", "e2e", "e2e", "e2e"),
    "motion": ("CI", "motion suite", "ci", "motion"),
}
# PR number -> (job, exact failure line), from --known-flake.
KNOWN_FLAKES = {}
# Snapshot -> the accepted flakes, for land_tested and the merge message.
FLAKE_NOTES = {}


def ci_gate_module():
    import importlib.util
    path = os.path.join(SCRIPTS, "..", ".github", "scripts", "ci_gate.py")
    spec = importlib.util.spec_from_file_location("ci_gate", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def paths_affecting(pr, job):
    """The PR's changed paths that the CI gate says run JOB (empty: it can't affect it)."""
    gate = ci_gate_module()
    _, _, workflow, key = FLAKE_JOBS[job]
    wants = gate.JOB_RULES[workflow][key][0]
    base = git("merge-base", "main", pr["tip"])
    paths = git("diff", "--name-only", "--no-renames", base, pr["tip"]).splitlines()
    return [p for p in paths if gate.tags_of(p)[0] & wants]


def parse_known_flakes(items, numbers):
    for item in items:
        n, _, rest = item.partition("=")
        job, _, line = rest.partition(":")
        job, line = job.strip(), line.strip()
        if not n.isdigit() or int(n) not in numbers or job not in FLAKE_JOBS or not line:
            raise Refused(f"merge: --known-flake takes N=JOB: EXACT FAILURE LINE with N among the PRs and JOB one of "
                          f"{', '.join(FLAKE_JOBS)}, not {item!r}")
        KNOWN_FLAKES[int(n)] = (job, line)


def accepted_flake(snapshot, runs, prs):
    """A note if every red workflow on SNAPSHOT is a known flake of one of PRS: only
    its JOB failed, the failed log holds the given line, and no PR's paths can affect
    the job. Otherwise None."""
    flakes = [(pr, KNOWN_FLAKES[pr["number"]]) for pr in prs if pr["number"] in KNOWN_FLAKES]
    red = {name: r for name, r in runs.items()
           if r["status"] == "completed" and r["conclusion"] not in ("success", "cancelled", "skipped")}
    if not red or not flakes:
        return None
    notes = []
    for name, r in red.items():
        match = None
        for pr, (job, line) in flakes:
            workflow, prefix, _, _ = FLAKE_JOBS[job]
            if workflow != name:
                continue
            jobs = gh_json("api", f"repos/{REPO}/actions/runs/{r['databaseId']}/jobs?per_page=100")["jobs"]
            failed = [j["name"] for j in jobs if j["conclusion"] not in ("success", "skipped", None)]
            if not failed or any(not f.startswith(prefix) for f in failed):
                continue
            logs = run(["gh", "run", "view", str(r["databaseId"]), "--repo", REPO, "--log-failed"], check=False)
            if line not in logs:
                continue
            affecting = [p for other in prs for p in paths_affecting(other, job)]
            if affecting:
                raise Refused(f"--known-flake {pr['number']}={job}: the batch's paths can affect {job} "
                              f"({', '.join(affecting[:5])})")
            match = f"{job}: \"{line}\" ({r['url']}); no merged path can affect {job}"
            break
        if not match:
            return None
        notes.append(match)
    return "; ".join(notes)


def rerun_failed_once(lane, snapshot):
    """A red candidate's failed jobs, re-run once (far cheaper than bisecting
    into fresh candidates): True if every workflow then passes. The first
    failure goes into the merge message."""
    runs = snapshot_runs(snapshot)
    failed = [(name, r) for name, r in runs.items()
              if r["status"] == "completed" and r["conclusion"] not in ("success", "cancelled", "skipped")]
    if not failed:
        return False
    for name, r in failed:
        log(f"merge: {name} failed on {snapshot[:12]} ({r['url']}); re-running its failed jobs once")
        gh("run", "rerun", str(r["databaseId"]), "--failed", "--repo", REPO, check=False)
    # Wait until the re-runs show as running, so the wait below can't read the old verdict.
    deadline = time.time() + 120
    while time.time() < deadline:
        now = snapshot_runs(snapshot)
        if all(now.get(name, {}).get("status") != "completed" for name, _ in failed):
            break
        time.sleep(10)
    if not ci_wait(lane, snapshot):
        log(f"merge: {snapshot[:12]} failed again after the re-run")
        return False
    RERUN_NOTES[snapshot] = "; ".join(f"{name} failed first ({r['url']}), passed when its failed jobs were re-run"
                                      for name, r in failed)
    return True


# The workflows that run on macOS runners (the scarce ones).
MACOS_WORKFLOWS = ("CI", "Engine E2E")


def green(pr, batch=False):
    """The PR's own latest snapshot: True when it may merge, False red. Alone,
    every workflow must pass (waits while running). In a batch, whose
    candidate is tested anyway, only the Windows workflow (no macOS runners)
    must pass; CI and Engine E2E may still be running, never red.
    Sets pr["own_green"] to whether all three passed."""
    runs = snapshot_runs(pr["snapshot"])
    state, marks = ci_state(runs)
    log(f"merge: #{pr['number']} snapshot {pr['snapshot'][:12]}: {state} {marks}")
    pr["own_green"] = state == "green"
    if state == "red" and pr["number"] in KNOWN_FLAKES:
        note = accepted_flake(pr["snapshot"], runs, [pr])
        if note:
            log(f"merge: #{pr['number']}: its own red run is a known flake ({note}); a candidate tests it")
            return True
    if state in ("green", "red"):
        return state == "green"
    if state == "cancelled":
        # Superseded, never judged: it may merge, through a fresh candidate.
        log(f"merge: #{pr['number']}: its own runs were cancelled; a new candidate tests it")
        return True
    if not batch:
        pr["own_green"] = ci_wait(pr["lane"], pr["snapshot"])
        return pr["own_green"]
    windows = runs.get("Windows")
    if not windows or windows["status"] != "completed":
        log(f"merge: #{pr['number']}: waiting for its Windows run (CI and Engine E2E are left to the candidate)")
        ok = subprocess.run([os.path.join(SCRIPTS, "ci.sh"), "--wait", pr["snapshot"], "--lane", pr["lane"],
                             "--workflow", "Windows"]).returncode == 0
        if not ok:
            return False
        runs = snapshot_runs(pr["snapshot"])
    return ci_state(runs)[0] != "red"


def main_worktree():
    out = git("worktree", "list", "--porcelain")
    path = None
    for line in out.splitlines():
        if line.startswith("worktree "):
            path = line[9:]
        elif line == "branch refs/heads/main":
            return path
    return None


def merge_message(pr, snapshot, runs, hw_evidence):
    lines = [f"Merge {pr['marker']['branch']}: {pr['title']} (#{pr['number']})", "",
             "What and why:", pr["what"], "", "Verified:", pr["verified"]]
    lines += ["", f"CI green on snapshot {snapshot[:12]}: " +
              ", ".join(f"{w} {runs[w]['url']}" for w in WORKFLOWS)]
    if snapshot in RERUN_NOTES:
        lines += [f"Re-run: {RERUN_NOTES[snapshot]}"]
    if snapshot in FLAKE_NOTES:
        lines += [f"Known flake accepted by QA: {FLAKE_NOTES[snapshot]}"]
    if pr["hw"] != "none":
        lines += [f"Hardware ({pr['hw']}): {hw_evidence[pr['number']]}"]
    lines += ["", f"PR: {pr['url']}"]
    text = "\n".join(lines) + "\n"
    real_text(text, f"the merge message for #{pr['number']}")
    return text


class MergeLock:
    def __enter__(self):
        path = os.path.join(git("rev-parse", "--git-common-dir", cwd=SCRIPTS), "isocast-merge.lock")
        if not os.path.isabs(path):
            path = os.path.join(SCRIPTS, path)
        self.f = open(path, "a")
        try:
            fcntl.flock(self.f, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            log(f"merge: waiting for another merge-pr.sh ({path})")
            fcntl.flock(self.f, fcntl.LOCK_EX)
        return self

    def __exit__(self, *exc):
        self.f.close()


def workspace_check(wt):
    """cargo check of the merged tree on a build host; waits while none is free."""
    env = dict(os.environ, ISOCAST_LANE="merge-check")
    cmd = [os.path.join(wt, "scripts", "remote-build.sh"), "cargo", "check", "--workspace", "--all-targets", "--locked"]
    deadline = time.time() + 3600
    while True:
        log("merge: workspace check: " + " ".join(cmd[1:]))
        code = subprocess.run(cmd, cwd=wt, env=env).returncode
        if code == 0:
            return
        if code in (69, 75, 76) and time.time() < deadline:
            log(f"merge: no build host free (exit {code}); retrying in a minute")
            time.sleep(60)
            continue
        raise Refused(f"the workspace check failed on the merged tree (remote-build exit {code})")


def land_tested(group, snapshot, trail, hw_evidence, dry_run, notes=None):
    """Merge GROUP (green on SNAPSHOT, whose trailers are TRAIL) into main and publish.
    NOTES: {"ref": commit or None}; a pending STATE-notes commit the snapshot
    includes is merged last, in the same publish, and then cleared.
    Returns False when main moved since the snapshot (the caller re-publishes)."""
    notes_ref = (notes or {}).get("ref")
    if notes_ref and notes_ref not in trail.get("include", []) and notes_ref != trail.get("source"):
        raise Refused(f"snapshot {snapshot[:12]} doesn't include the STATE notes {notes_ref[:12]}")
    runs = snapshot_runs(snapshot)
    if ci_state(runs)[0] != "green":
        if snapshot not in FLAKE_NOTES:
            raise Refused(f"snapshot {snapshot[:12]} isn't green on {', '.join(WORKFLOWS)}")
    messages = [merge_message(pr, snapshot, runs, hw_evidence) for pr in group]
    if dry_run:
        for m in messages:
            log("merge: dry run, would merge with:\n" + m)
        return True
    with MergeLock():
        base = rev("main")
        if trail.get("main") != base:
            log(f"merge: main moved to {base[:12]} since snapshot {snapshot[:12]}; re-publishing")
            return False
        wt = os.path.join(MERGE_DIR, "wt")
        os.makedirs(MERGE_DIR, exist_ok=True)
        if os.path.exists(wt):
            git("worktree", "remove", "--force", wt)
        git("worktree", "add", "--detach", wt, base)
        try:
            for pr, msg in zip(group, messages):
                sha = pr["tip"]
                with tempfile.NamedTemporaryFile("w", delete=False) as f:
                    f.write(msg)
                try:
                    git("merge", "--no-ff", "-F", f.name, sha, cwd=wt)
                finally:
                    os.unlink(f.name)
            if notes_ref:
                numbers = ", ".join(f"#{pr['number']}" for pr in group)
                git("merge", "--no-ff", "-m", f"STATE notes for {numbers}",
                    "-m", f"Applied by the merger in the same publish (tested in snapshot {snapshot[:12]}).",
                    notes_ref, cwd=wt)
            merged = rev("HEAD", cwd=wt)
            tree = git("rev-parse", f"{merged}^{{tree}}")
            if tree != trail["tree"]:
                raise Refused(f"the merge's tree {tree[:12]} isn't the tree CI tested ({trail['tree'][:12]})")
            workspace_check(wt)
            mwt = main_worktree()
            if mwt:
                if git("status", "--porcelain", "--untracked-files=no", cwd=mwt):
                    raise Refused(f"the main checkout {mwt} has uncommitted changes; main is left at {base[:12]}")
                git("merge", "--ff-only", merged, cwd=mwt)
            else:
                git("update-ref", "refs/heads/main", merged, base)
            log(f"merge: main is now {merged[:12]}")
            numbers = ", ".join(f"#{pr['number']}" for pr in group)
            if len(group) == 1:
                public_msg = f"{group[0]['title']} (#{group[0]['number']})\n\n{group[0]['what']}\n"
            else:
                public_msg = f"Merge {numbers}\n\n" + "".join(f"- {pr['title']} (#{pr['number']})\n" for pr in group)
            if notes_ref:
                public_msg += "\nWith their STATE notes.\n"
            scan_text(public_msg, "the public commit message")
            p = subprocess.run([os.path.join(wt, "scripts", "publish-snapshot.sh"), "--push", public_msg.strip()])
            if p.returncode != 0:
                raise Refused(f"publish-snapshot.sh failed (exit {p.returncode}); main is {merged[:12]}, unpublished")
            published = git("rev-parse", "HEAD", cwd=PUBLIC_DIR)
            remote_main = run(["git", "ls-remote", REMOTE, "refs/heads/main"]).split()[0]
            if remote_main != published:
                raise Refused(f"public main is {remote_main[:12]}, not the published {published[:12]}")
        finally:
            git("worktree", "remove", "--force", wt, check=False)
    url = f"https://github.com/{REPO}/commit/{published}"
    for pr in group:
        others = [f"#{o['number']}" for o in group if o is not pr]
        comment = (f"Merged into main as `{merged[:12]}`" + (f" with {', '.join(others)}" if others else "") +
                   f" and published as {url}.\n\nCI on snapshot `{snapshot[:12]}`: " +
                   ", ".join(f"[{w}]({runs[w]['url']})" for w in WORKFLOWS) + ".")
        gh("pr", "close", str(pr["number"]), "--repo", REPO, "--comment", comment, "--delete-branch")
        log(f"merge: closed #{pr['number']}")
        if pr["state_notes"] and not notes_ref:
            print(f"STATE notes from #{pr['number']} (apply to docs/STATE.md with the next batch's --notes):\n"
                  f"{pr['state_notes']}\n")
    if notes_ref:
        notes["ref"] = None
    print(f"merged {numbers} -> {url}")
    return True


def land(group, hw_evidence, dry_run, refused, notes=None):
    """Land GROUP (each green on its own snapshot): one candidate, bisected when red.
    A pending STATE-notes commit (NOTES["ref"]) rides in the candidate of
    whichever group lands first."""
    notes = notes if notes is not None else {"ref": None}
    for _attempt in range(3):
        if len(group) == 1 and not notes["ref"]:
            pr = group[0]
            if pr["trail"].get("main") != rev("main") or not pr.get("own_green", True):
                if dry_run:
                    log(f"merge: dry run: #{pr['number']} is behind main; would re-publish it with --merge-main")
                    return
                ok, snapshot = ci_publish(pr["lane"], pr["tip"])
                if not ok and snapshot:
                    ok = rerun_failed_once(pr["lane"], snapshot)
                if not ok and snapshot:
                    note = accepted_flake(snapshot, snapshot_runs(snapshot), [pr])
                    if note:
                        log(f"merge: {snapshot[:12]}: accepting a known flake: {note}")
                        FLAKE_NOTES[snapshot] = note
                        ok = True
                if not snapshot:
                    refused.append(f"#{pr['number']}: conflicts with main, or ci.sh couldn't publish it (see above); "
                                   "the implementer merges main and runs pr.sh")
                    return
                refresh(pr, snapshot)
                if not ok:
                    refused.append(f"#{pr['number']}: red when merged with current main ({pr['snapshot'][:12]})")
                    return
            if land_tested(group, pr["snapshot"], pr["trail"], hw_evidence, dry_run):
                return
            continue
        if dry_run:
            log(f"merge: dry run: would test {', '.join('#%d' % p['number'] for p in group)} as one candidate"
                + (f" with STATE notes {notes['ref'][:12]}" if notes["ref"] else ""))
            return
        lane = f"batch-{group[0]['number']}"
        includes = [p["tip"] for p in group[1:]] + ([notes["ref"]] if notes["ref"] else [])
        ok, snapshot = ci_publish(lane, group[0]["tip"], includes,
                                  superseded=[p["snapshot"] for p in group if not p.get("own_green", True)])
        if not ok and snapshot:
            ok = rerun_failed_once(lane, snapshot)
        if not ok and snapshot:
            note = accepted_flake(snapshot, snapshot_runs(snapshot), group)
            if note:
                log(f"merge: {snapshot[:12]}: accepting a known flake: {note}")
                FLAKE_NOTES[snapshot] = note
                ok = True
        if not ok:
            if not snapshot:
                refused.append(f"{[p['number'] for p in group]}: ci.sh couldn't publish the candidate (a conflict, "
                               "or .github/ changes without --allow-workflow-changes; see ci.sh above)")
                return
            gh("api", "-X", "DELETE", f"repos/{REPO}/git/refs/heads/ci/{lane}", check=False)
            if len(group) == 1:
                refused.append(f"#{group[0]['number']}: red when merged with current main"
                               + (" and the STATE notes" if notes["ref"] else ""))
                return
            half = len(group) // 2
            log(f"merge: the batch is red; bisecting {[p['number'] for p in group[:half]]} "
                f"and {[p['number'] for p in group[half:]]}")
            land(group[:half], hw_evidence, dry_run, refused, notes)
            land(group[half:], hw_evidence, dry_run, refused, notes)
            return
        msg = gh_json("api", f"repos/{REPO}/commits/{snapshot}", "--jq", "{m: .commit.message}")["m"]
        landed = land_tested(group, snapshot, trailers_of(msg), hw_evidence, dry_run, notes)
        gh("api", "-X", "DELETE", f"repos/{REPO}/git/refs/heads/ci/{lane}", check=False)
        if landed:
            return
    refused.append(f"{[p['number'] for p in group]}: main kept moving; try again")


def state_notes_ref(ref):
    """The merger's STATE-notes commit: docs only, merging cleanly with main."""
    sha = rev(ref)
    if not sha:
        raise Refused(f"merge: --notes {ref} is not a commit here")
    base = git("merge-base", "main", sha)
    if base == sha:
        raise Refused(f"merge: --notes {ref} is already in main")
    paths = git("diff", "--name-only", base, sha).splitlines()
    if not paths or any(not p.startswith("docs/") for p in paths):
        raise Refused(f"merge: --notes {ref} must change docs/ only (it changes {', '.join(paths) or 'nothing'})")
    if subprocess.run(["git", "merge-tree", "--write-tree", "main", sha], cwd=SCRIPTS,
                      capture_output=True).returncode != 0:
        raise Refused(f"merge: --notes {ref} conflicts with main; rebase it onto main")
    return sha


def cmd_merge(a):
    hw_evidence = {}
    for item in a.hw:
        n, _, text = item.partition("=")
        if not n.isdigit() or not text.strip():
            raise Refused(f"merge: --hw takes N=EVIDENCE (what hw-batch ran and found), not {item!r}")
        hw_evidence[int(n)] = text.strip()
    notes = {"ref": state_notes_ref(a.notes) if a.notes else None}
    parse_known_flakes(a.known_flake, set(a.numbers))
    if a.allow_workflow_changes:
        ALLOW_WORKFLOWS.append("--allow-workflow-changes")
    numbers = list(dict.fromkeys(a.numbers))
    prs, refused = [], []
    for n in numbers:
        try:
            pr = load(n)
            if pr["hw"] != "none" and n not in hw_evidence:
                raise Refused(f"#{n} needs {pr['hw']} hardware: qualify it with hw-batch, then pass "
                              f"--hw {n}=\"what ran and the numbers\"")
            prs.append(pr)
        except Refused as e:
            refused.append(str(e))
    for pr in prs:
        if not green(pr, batch=len(prs) > 1):
            refused.append(f"#{pr['number']}: its snapshot {pr['snapshot'][:12]} is red; "
                           "send it back to the implementer")
    if refused:
        for r in refused:
            log(f"merge: refused: {r}")
        return 1
    land(prs, hw_evidence, a.dry_run, refused, notes)
    if notes["ref"] and not a.dry_run:
        refused.append(f"the STATE notes {notes['ref'][:12]} didn't land (no group landed); pass them again")
    for r in refused:
        log(f"merge: refused: {r}")
    return 1 if refused else 0


def main():
    p = argparse.ArgumentParser(prog="pr_flow.py", description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    pr = sub.add_parser("pr", help="open or update this lane's pull request")
    g = pr.add_mutually_exclusive_group()
    g.add_argument("--draft", action="store_true", help="open as, or turn it back into, a draft")
    g.add_argument("--ready", action="store_true", help="open it, or mark it, ready for review")
    pr.add_argument("--report", help="the lane's REPORT.md (default /Volumes/IsocastBuild/<lane>/scratch/REPORT.md)")
    pr.add_argument("--title")
    pr.add_argument("--hw", help=f"hardware needed: {', '.join(HARDWARE)} (overrides the REPORT)")
    pr.add_argument("--area", action="append", choices=AREAS + ("documentation",))
    pr.add_argument("--session", help="the lane's session id (default $OPENCODE_SESSION_ID)")
    pr.add_argument("--allow-workflow-changes", action="store_true")
    pr.add_argument("--dry-run", action="store_true", help="print the description; publish nothing")
    ls = sub.add_parser("prs", help="list open pull requests")
    ls.add_argument("--json", action="store_true")
    m = sub.add_parser("merge", help="land pull requests into main and publish")
    m.add_argument("numbers", nargs="+", type=int)
    m.add_argument("--hw", action="append", default=[], metavar="N=EVIDENCE",
                   help="hardware qualification evidence for a needs:<hw> PR")
    m.add_argument("--known-flake", action="append", default=[], metavar="N=JOB: LINE",
                   help="accept PR N's failure of a hosted JOB (e2e or motion) when the exact LINE is in the failed log "
                        "and no merged path can affect JOB; written into the merge message")
    m.add_argument("--notes", metavar="REF",
                   help="a docs-only commit (the merger's STATE notes for these PRs), tested in the same candidate "
                        "and merged in the same publish, so main moves once per batch")
    m.add_argument("--allow-workflow-changes", action="store_true",
                   help="the PRs change .github/ and QA reviewed it (passed to ci.sh when re-publishing)")
    m.add_argument("--dry-run", action="store_true", help="check and say what would happen; change nothing")
    a = p.parse_args()
    try:
        return {"pr": cmd_pr, "prs": cmd_prs, "merge": cmd_merge}[a.cmd](a)
    except Refused as e:
        log(str(e))
        return 1


if __name__ == "__main__":
    sys.exit(main())
