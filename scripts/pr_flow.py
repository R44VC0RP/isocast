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
local-first). Alone, a green snapshot of the current tip and main can land
directly. A batch admits QA-approved PRs without waiting on their own CI;
a relevant red snapshot or merge conflict skips only that PR. The candidate
replaces their pending macOS runs; Windows stays running to identify bad PRs
before a red candidate is split in
halves and each half is landed the same way. Landing merges each branch
with --no-ff and a message built from its PR, checks that the result is
exactly the tree CI tested, runs `cargo check --workspace --all-targets
--locked` on a build host, fast-forwards main, publishes it
(publish-snapshot.sh --push), comments with the published commit, closes
the PR and deletes its ci/ branch. The merger writes the PRs' STATE notes
as one docs-only commit and passes it as --notes: it rides in the same
candidate and lands in the same publish, so main moves once per batch
(a standalone commit on main would make every open PR stale).

After interruption, repeat the same merge-pr.sh command. Under $ISOCAST_CI_DIR
(default /Volumes/IsocastBuild/ci), merge-<command hash>.json checkpoints the
remaining groups, tips, candidate, run IDs and landing receipt. A restart checks
live provenance and CI; the journal is not a green verdict. After main advances
it retries only publication/PR closure, not the merge. Every command first
finishes (or refuses) interrupted landings, even from other commands. A dry
run reports such a journal without changing it. Pass skipped PRs again once fixed.

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
import contextlib
import datetime
import fcntl
import glob
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
import time
import uuid

REPO = "R44VC0RP/isocast"
REMOTE = f"https://github.com/{REPO}.git"
WORKFLOWS = ("CI", "Engine E2E", "Windows")
MACOS_WORKFLOWS = ("CI", "Engine E2E")
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
        m = re.match(r"^Isocast-(Source|Include|Main|Tree|Dirty|CI):\s*(\S+)\s*$", line)
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

def snapshot_runs(sha, branch):
    """The newest push run of each workflow on a snapshot: name -> run dict."""
    runs = gh_json("run", "list", "--repo", REPO, "--commit", sha, "--branch", branch, "--event", "push", "--limit", "50",
                   "--json", "databaseId,workflowName,status,conclusion,headBranch,headSha,url,createdAt") or []
    newest = {}
    for r in sorted(runs, key=lambda r: (r["createdAt"], r["databaseId"])):
        if r["headBranch"] == branch and r["headSha"] == sha:
            newest[r["workflowName"]] = r
    return newest


def ci_state(runs, plan=None):
    """Aggregate exact push runs. Skipped (including intentionally untriggered)
    and cancelled workflows are no verdict. A plan never supplies a pass."""
    marks, states = [], []
    for w in WORKFLOWS:
        r = runs.get(w)
        if (r is None and plan == 'windows' and w in MACOS_WORKFLOWS) or (
                r and r['status'] == 'completed' and r['conclusion'] == 'skipped'):
            marks.append("–"); states.append("skipped")
        elif r is None:
            marks.append("·"); states.append("missing")
        elif r["status"] != "completed":
            marks.append("●"); states.append("pending")
        elif r["conclusion"] == "success":
            marks.append("✓"); states.append("green")
        elif r["conclusion"] == "cancelled":
            marks.append("○"); states.append("cancelled")
        else:
            marks.append("✗"); states.append("red")
    for s in ("red", "pending", "missing", "cancelled", "skipped"):
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
                   f"`{(trail.get('main') or '')[:12]}`. Windows runs on lane pushes; the train tests all three workflows.", "",
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

QA_FIELDS = {
    'comments': 'body createdAt authorAssociation isMinimized',
    'reviews': 'body submittedAt authorAssociation isMinimized state',
}
PR_FIELDS = """
    number title url isDraft body state createdAt baseRefName headRefName headRefOid
    labels(first: 20) { nodes { name } }
    commits(last: 1) { nodes { commit { oid message committedDate } } }
""" + '\n'.join(name + '(last: 100) { pageInfo { hasPreviousPage startCursor } nodes { ' + fields + ' } }'
                 for name, fields in QA_FIELDS.items())

PRS_QUERY = """
query($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) {
    pullRequests(states: OPEN, baseRefName: "main", first: 100, orderBy: {field: CREATED_AT, direction: ASC}) {
      nodes { """ + PR_FIELDS + """ }
    }
  }
}"""


def query_pr(number, fields, before=None):
    owner, name = REPO.split('/')
    args = ['-F', f'owner={owner}', '-F', f'name={name}', '-F', f'n={number}']
    cursor = ''
    if before is not None:
        cursor = ', $before: String!'
        args += ['-f', f'before={before}']
    query = ('query($owner: String!, $name: String!, $n: Int!' + cursor + ') { repository(owner:$owner, name:$name) '
             '{ pullRequest(number:$n) { ' + fields + ' } } }')
    return gh_json('api', 'graphql', *args, '-f', 'query=' + query)['data']['repository']['pullRequest']


def head_commit(pr):
    last = (pr['commits']['nodes'] or [{}])[-1].get('commit') or {}
    return last if last.get('oid') == pr['headRefOid'] else {}


def qa_verdict(pr):
    """Same head, cutoff, trusted authors and latest-verdict rule for listing and landing."""
    committed = head_commit(pr).get('committedDate')
    if not committed:
        return None
    cutoff = datetime.datetime.fromisoformat(committed.replace('Z', '+00:00'))
    verdicts = []
    for name, fields in QA_FIELDS.items():
        page = pr[name]
        while True:
            for c in page['nodes']:
                m = QA_VERDICT.match(c.get('body') or '')
                stamp = c.get('submittedAt') if name == 'reviews' else c.get('createdAt')
                if (not m or not stamp or c['authorAssociation'] not in ('OWNER', 'MEMBER', 'COLLABORATOR') or
                        c['isMinimized'] or c.get('state') in ('DISMISSED', 'PENDING')):
                    continue
                when = datetime.datetime.fromisoformat(stamp.replace('Z', '+00:00'))
                if when > cutoff:
                    # On a same-second tie, changes requested wins, not approval.
                    verdicts.append((when, 'approved' if m[1].lower() == 'approved' else 'changes'))
            if not page['pageInfo']['hasPreviousPage']:
                break
            more = query_pr(pr['number'], 'headRefOid ' + name + '(last:100, before:$before) '
                            '{ pageInfo { hasPreviousPage startCursor } nodes { ' + fields + ' } }',
                            page['pageInfo']['startCursor'])
            if more['headRefOid'] != pr['headRefOid']:
                return None
            page = more[name]
    return max(verdicts)[1] if verdicts else None


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
        mk = marker_of(pr["body"]) or {}
        trail = trailers_of(head_commit(pr).get('message', '')) if head_commit(pr) else {}
        newest = {}
        for r in sorted((r for r in runs if r["headSha"] == pr["headRefOid"] and r['headBranch'] == pr['headRefName']),
                        key=lambda r: (r['createdAt'], r['databaseId'])):
            newest[r["workflowName"]] = r
        state, marks = ci_state(newest, trail.get('ci'))
        review = qa_verdict(pr)
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
    print("CEW: CI, Engine E2E, Windows (✓ green, ✗ red, ● running, ○ cancelled, – skipped, · none). main: ok, stale (older main), "
          "moved (branch has commits since its snapshot).")
    return 0


# --------------------------------------------------------------- merge ----

def load(number, batch=False):
    if batch:
        pr = query_pr(number, PR_FIELDS)
        if not pr:
            raise Refused(f'no such PR: #{number}')
        pr['labels'] = pr['labels']['nodes']
        pr['approved'] = qa_verdict(pr) == 'approved'
    else:
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
    refresh(pr, pr['headRefOid'])
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


def ci_wait_cmd(lane, snapshot, train):
    required = [arg for name in WORKFLOWS for arg in ('--workflow', name)]
    return [os.path.join(SCRIPTS, 'ci.sh'), '--wait', snapshot, '--train-lane' if train else '--lane', lane, *required]


def ci_wait(lane, snapshot, lock=(), *, train=False):
    """ci.sh --wait: True when every workflow passed on SNAPSHOT."""
    log(f"merge: waiting for CI on {snapshot[:12]} (ci/{lane})")
    return subprocess.run(ci_wait_cmd(lane, snapshot, train), pass_fds=lock).returncode == 0


# Set by merge-pr --allow-workflow-changes: QA reviewed the PRs' .github/ changes.
ALLOW_WORKFLOWS = []


class Train(dict):
    """Atomic, command-scoped journal. The merger lock serializes all writers."""
    def __init__(self, path, lock, numbers=()):
        self.path = path
        self.lock = (lock.f.fileno(),) if lock else ()
        super().__init__(phase="new", numbers=list(dict.fromkeys(numbers)), refused=[])
        if os.path.isfile(path):
            try:
                with open(path) as f:
                    data = json.load(f)
                if 'phase' not in data:
                    raise ValueError('missing phase')
                self.update(data)
            except (OSError, ValueError, TypeError) as e:
                raise Refused(f"merge: cannot inspect journal {path}: {e}") from e

    def save(self, **changes):
        self.update(changes)
        if self.lock:
            os.makedirs(CI_DIR, exist_ok=True)
            with tempfile.NamedTemporaryFile('w', dir=CI_DIR, delete=False) as f:
                json.dump(self, f)
            os.replace(f.name, self.path)


def remote_head(branch):
    out = run(["git", "ls-remote", REMOTE, f"refs/heads/{branch}"])
    return out.split()[0] if out else None


def ci_publish(train, group, notes):
    """Recover a push (even if killed before ci.sh wrote 'pushed'), or start one."""
    tips = {str(pr['number']): pr['tip'] for pr in group}
    source, *includes = tips.values()
    if notes:
        includes.append(notes)
    base = rev('main')
    inputs = [base, tips, notes]
    lane = train.get('lane') or f"batch-{group[0]['number']}-{uuid.uuid4().hex[:12]}"
    head = remote_head(f'ci/{lane}')
    if train.get('inputs') != inputs or train['phase'] not in ('pushing', 'waiting'):
        # Reuse one bare snapshot repo per train, including all its bisections.
        train.save(phase='pushing', inputs=inputs, lane=lane, previous=head, candidate=None, run_ids={}, rerun=None)
    snapshot = head if head != train['previous'] else None
    if snapshot:
        # The push can finish before the journal write. Accept only a commit
        # minted in this train's local ci.sh repo, not arbitrary remote trailers.
        local = os.path.join(CI_DIR, lane, 'public.git')
        if train['candidate'] not in (None, snapshot) or not os.path.isdir(local) or rev(snapshot, cwd=local) != snapshot:
            raise Refused(f"merge: ci/{lane} moved outside this train; refusing to reuse it")
    cmd = [os.path.join(SCRIPTS, "ci.sh"), "--source", source, "--merge-main", "--train-lane", lane,
           "--no-wait"] + ALLOW_WORKFLOWS
    for inc in includes:
        cmd += ["--include", inc]
    if not snapshot:
        log(f"merge: publishing ci/{lane}: {' '.join(cmd[1:])}")
        if subprocess.run(cmd, pass_fds=train.lock).returncode:
            return False, None
        with open(os.path.join(CI_DIR, lane, 'pushed')) as f:
            snapshot = f.read().strip()
    msg = gh_json('api', f'repos/{REPO}/commits/{snapshot}', '--jq', '{m: .commit.message}')['m']
    trail = trailers_of(msg)
    if (trail.get('source') != source or trail['include'] != includes or trail.get('dirty') or
            not trail.get('tree') or not trail.get('main')):
        raise Refused(f"merge: {snapshot} does not match the candidate inputs")
    train.save(phase='waiting', candidate=snapshot, inputs=[trail['main'], tips, notes])
    superseded = group if len(train['numbers']) > 1 else []
    for pr in superseded:
        sha, branch = pr['snapshot'], pr['headRefName']
        for name, r in snapshot_runs(sha, branch).items():
            if name in MACOS_WORKFLOWS and r['headBranch'] == branch and r["status"] != "completed":
                log(f"merge: cancelling {name} on superseded snapshot {sha[:12]} ({r['url']})")
                gh("run", "cancel", str(r["databaseId"]), "--repo", REPO, check=False)
    # ci.sh saves the exact run IDs once GitHub registers all three workflows.
    # Keep its timeout, evidence collection and SHA/branch checks on resume.
    ids_path = os.path.join(CI_DIR, lane, 'runs', snapshot, 'run-ids.txt')
    with subprocess.Popen(ci_wait_cmd(lane, snapshot, train=True),
                          pass_fds=train.lock) as waiter:
        while True:
            try:
                code = waiter.wait(timeout=5)
            except subprocess.TimeoutExpired:
                code = None
            if os.path.isfile(ids_path):
                with open(ids_path) as f:
                    ids = dict(zip(WORKFLOWS, f.read().split()))
                if len(ids) == len(WORKFLOWS) and ids != train['run_ids']:
                    train.save(run_ids=ids)
            if code is not None:
                return code == 0, snapshot


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


def accepted_flake(snapshot, runs, prs, own=False):
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
                if own:
                    return None
                raise Refused(f"--known-flake {pr['number']}={job}: the batch's paths can affect {job} "
                              f"({', '.join(affecting[:5])})")
            match = f"{job}: \"{line}\" ({r['url']}); no merged path can affect {job}"
            break
        if not match:
            return None
        notes.append(match)
    return "; ".join(notes)


def rerun_failed_once(train, snapshot):
    """A red candidate's failed jobs, re-run once (far cheaper than bisecting
    into fresh candidates): True if every workflow then passes. The first
    failure goes into the merge message."""
    lane = train['lane']
    if train.get('rerun'):
        return False
    runs = snapshot_runs(snapshot, f'ci/{lane}')
    failed = [(name, r) for name, r in runs.items()
              if r["status"] == "completed" and r["conclusion"] not in ("success", "cancelled", "skipped")]
    if not failed:
        return False
    train.save(rerun="; ".join(f"{name} failed first ({r['url']}), passed when its failed jobs were re-run"
                               for name, r in failed))
    for name, r in failed:
        log(f"merge: {name} failed on {snapshot[:12]} ({r['url']}); re-running its failed jobs once")
        gh("run", "rerun", str(r["databaseId"]), "--failed", "--repo", REPO, check=False)
    # Wait until the re-runs show as running, so the wait below can't read the old verdict.
    deadline = time.time() + 120
    while time.time() < deadline:
        now = snapshot_runs(snapshot, f'ci/{lane}')
        if all(now.get(name, {}).get("status") != "completed" for name, _ in failed):
            break
        time.sleep(10)
    if not ci_wait(lane, snapshot, train.lock, train=True):
        log(f"merge: {snapshot[:12]} failed again after the re-run")
        return False
    RERUN_NOTES[snapshot] = train['rerun']
    return True


def green(pr, batch=False, lock=()):
    """Is the snapshot admissible? Only an exact three-workflow pass also sets
    own_green (eligible to land directly). No evidence requires a candidate."""
    runs = snapshot_runs(pr["snapshot"], pr['headRefName'])
    # A single PR with a full set of runs in flight can still use its own CI.
    # Don't wait on absent Mac workflows. Re-read after waiting: a pending run
    # can finish skipped, which isn't a failure (or proof for the fast path).
    if not batch and ci_state(runs)[0] == 'pending' and all(
            w in runs and runs[w]['conclusion'] not in ('cancelled', 'skipped') for w in WORKFLOWS):
        ci_wait(pr['lane'], pr['snapshot'], lock)
        runs = snapshot_runs(pr['snapshot'], pr['headRefName'])
    state, marks = ci_state(runs, pr['trail'].get('ci'))
    log(f"merge: #{pr['number']} snapshot {pr['snapshot'][:12]}: {state} {marks}")
    pr["own_green"] = state == "green"
    if state == 'red' and batch:
        pr['red_reason'] = red_reason({w: runs[w] for w in WORKFLOWS if w in runs and ci_state({w: runs[w]})[0] == 'red'})
    if state == "red" and pr["number"] in KNOWN_FLAKES:
        note = accepted_flake(pr["snapshot"], runs, [pr], own=batch)
        if note:
            log(f"merge: #{pr['number']}: its own red run is a known flake ({note}); a candidate tests it")
            return True
    return state != 'red'


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


@contextlib.contextmanager
def staging_tree(base):
    wt = os.path.join(MERGE_DIR, 'wt')
    os.makedirs(MERGE_DIR, exist_ok=True)
    if os.path.exists(wt):
        git('worktree', 'remove', '--force', wt)
    git('worktree', 'add', '--detach', wt, base)
    try:
        yield wt
    finally:
        git('worktree', 'remove', '--force', wt, check=False)


def ancestor(old, new, cwd=None):
    code = subprocess.run(['git', 'merge-base', '--is-ancestor', old, new], cwd=cwd or SCRIPTS).returncode
    if code not in (0, 1):
        raise Refused(f'merge: could not check ancestry of {old} and {new}')
    return code == 0


def published_landing(train, main):
    """Recovery proof across two unrelated histories; never push to obtain it."""
    merged = train['receipt']['merged']
    if not ancestor(merged, main):
        raise Refused(f'merge: {merged} is not on main; inspect {train.path}')
    if train['phase'] == 'closing':
        # This receipt was written only after verifying the push.
        published = train['published']
    else:
        # publish-snapshot records the private ref BEFORE pushing. Neither that
        # marker nor local public HEAD alone proves anything reached GitHub.
        try:
            with open(os.path.join(PUBLIC_DIR, '.git', 'isocast-published')) as f:
                private = f.read().strip()
        except FileNotFoundError:
            log('merge: no private-to-public publication marker')
            return None
        if not re.fullmatch('[0-9a-f]{40}', private) or not rev(private) or not ancestor(merged, private):
            log(f'merge: last publication marker does not include {merged[:12]}')
            return None
        published = rev('HEAD', cwd=PUBLIC_DIR)
        if not published:
            return None
        # Bind the marker to an actual public tree, including export-ignore;
        # this also works when a later publication included the private merge.
        with tempfile.TemporaryDirectory(dir=CI_DIR) as d:
            top = git('rev-parse', '--show-toplevel')  # git archive from scripts/ would export only that subtree
            with subprocess.Popen(['git', 'archive', private], cwd=top, stdout=subprocess.PIPE) as archive:
                unpack = subprocess.run(['tar', '-xf', '-'], cwd=d, stdin=archive.stdout)
                archive.stdout.close()
                if archive.wait() or unpack.returncode:
                    raise Refused(f'merge: could not verify public export of {private}')
            git('init', '-q', cwd=d)
            git('add', '--force', '-A', cwd=d)
            expected = git('write-tree', cwd=d)
            actual = git('rev-parse', f'{published}^{{tree}}', cwd=PUBLIC_DIR)
            if expected != actual:
                log(f'merge: marker exports tree {expected[:12]}, public checkout has {actual[:12]}')
                return None
    remote = remote_head('main')
    if not remote:
        return None
    if not rev(remote, cwd=PUBLIC_DIR):
        git('fetch', '--quiet', '--no-tags', REMOTE, remote, cwd=PUBLIC_DIR)
    if not ancestor(published, remote, PUBLIC_DIR):
        log(f'merge: {published[:12]} is not on public main {remote[:12]}')
        return None
    return published


def finish_landing(train, wt=None):
    """After the checked ff, publishing the same tree is idempotent. Never merge here."""
    receipt = train['receipt']
    merged, snapshot = receipt['merged'], receipt['snapshot']
    if wt is None:
        main = rev('main')
        published = published_landing(train, main)
        if published:
            train.save(phase='closing', published=published)
        elif train['phase'] == 'closing' or main != merged:
            raise Refused(f'merge: cannot prove {merged} was published; inspect {train.path} before another train')
    if train['phase'] == 'landing':
        if rev('main') != merged:
            raise Refused(f"merge: cannot recover publication of {merged}: main moved; inspect {train.path}")
        with (contextlib.nullcontext(wt) if wt else staging_tree(merged)) as wt:
            cmd = [os.path.join(wt, 'scripts', 'publish-snapshot.sh'), '--push', receipt['message']]
            if subprocess.run(cmd, env=dict(os.environ, ISOCAST_PUBLIC_REF=merged), pass_fds=train.lock).returncode:
                raise Refused(f"merge: publication incomplete; main is {merged}; repeat this command")
        published = git('rev-parse', 'HEAD', cwd=PUBLIC_DIR)
        if remote_head('main') != published:
            raise Refused(f"merge: public main is not the published {published}")
        train.save(phase='closing', published=published)
    url = f"https://github.com/{REPO}/commit/{train['published']}"
    for pr in receipt['prs']:
        n = str(pr['number'])
        actual = gh_json('pr', 'view', n, '--repo', REPO, '--json', 'state,headRefOid')
        if actual['state'] == 'OPEN':
            others = ', '.join(f"#{p['number']}" for p in receipt['prs'] if p != pr)
            comment = (f"Merged into main as `{merged[:12]}`" + (f" with {others}" if others else '') +
                       f" and published as {url}.\n\nCI on snapshot `{snapshot[:12]}`: {receipt['links']}.")
            # Close for the proven, published merge but preserve a newer branch.
            delete = ['--delete-branch'] if actual['headRefOid'] == pr['snapshot'] else []
            gh('pr', 'close', n, '--repo', REPO, '--comment', comment, *delete)
            log(f"merge: closed #{n}")
        if pr['state_notes'] and not receipt['notes']:
            print(f"STATE notes from #{n} (apply to docs/STATE.md with the next batch's --notes):\n{pr['state_notes']}\n")
    print(f"merged {', '.join('#' + str(p['number']) for p in receipt['prs'])} -> {url}")
    if snapshot == train.get('candidate'):
        drop_candidate(train, snapshot)
    train.save(phase='ready', queue=train['queue'][1:], notes=None, receipt=None, inputs=None,
               landed=train.get('landed', []) + [p['number'] for p in receipt['prs']])


def land_tested(train, group, snapshot, trail, hw_evidence, dry_run, branch):
    """Merge GROUP (green on SNAPSHOT, whose trailers are TRAIL) into main and publish.
    The pending STATE-notes commit is merged last, in the same publish.
    Returns False when main moved since the snapshot (the caller re-publishes)."""
    notes_ref = train['notes']
    if notes_ref and notes_ref not in trail.get("include", []) and notes_ref != trail.get("source"):
        raise Refused(f"snapshot {snapshot[:12]} doesn't include the STATE notes {notes_ref[:12]}")
    runs = snapshot_runs(snapshot, branch)
    if ci_state(runs)[0] != "green":
        if (snapshot not in FLAKE_NOTES or any(runs.get(w, {}).get('status') != 'completed' or
                runs[w]['conclusion'] not in ('success', 'failure') for w in WORKFLOWS) or
                not accepted_flake(snapshot, runs, group)):
            raise Refused(f"snapshot {snapshot[:12]} isn't green on {', '.join(WORKFLOWS)}")
    messages = [merge_message(pr, snapshot, runs, hw_evidence) for pr in group]
    if dry_run:
        for m in messages:
            log("merge: dry run, would merge with:\n" + m)
        return True
    base = rev('main')
    if trail.get('main') != base:
        log(f"merge: main moved to {base[:12]} since snapshot {snapshot[:12]}; re-publishing")
        return False
    with staging_tree(base) as wt:
        for pr, msg in zip(group, messages):
            git('merge', '--no-ff', '-m', msg, pr['tip'], cwd=wt)
        numbers = ', '.join(f"#{pr['number']}" for pr in group)
        if notes_ref:
            git('merge', '--no-ff', '-m', f'STATE notes for {numbers}',
                '-m', f'Applied by the merger in the same publish (tested in snapshot {snapshot[:12]}).',
                notes_ref, cwd=wt)
        merged = rev('HEAD', cwd=wt)
        if merged == base:
            raise Refused(f'merge: {numbers} already in main at {base[:12]}; refusing to land again')
        tree = git('rev-parse', f'{merged}^{{tree}}')
        if tree != trail['tree']:
            raise Refused(f"the merge's tree {tree[:12]} isn't the tree CI tested ({trail['tree'][:12]})")
        workspace_check(wt)
        if rev('main') != base:
            return False
        batch = len(train['numbers']) > 1
        for pr in group:
            latest = load(pr['number'], batch)
            if (any(pr[k] != latest[k] for k in ('tip', 'snapshot', 'hw')) or
                    (batch and not latest['approved'])):
                raise Refused(f"merge: #{pr['number']} changed while being tested; repeat this command")
        if len(group) == 1:
            message = f"{group[0]['title']} (#{group[0]['number']})\n\n{group[0]['what']}\n"
        else:
            message = f"Merge {numbers}\n\n" + ''.join(f"- {p['title']} (#{p['number']})\n" for p in group)
        if notes_ref:
            message += '\nWith their STATE notes.\n'
        scan_text(message, 'the public commit message')
        mwt = main_worktree()
        if mwt and git('status', '--porcelain', '--untracked-files=no', cwd=mwt):
            raise Refused(f"the main checkout {mwt} has uncommitted changes; main is left at {base[:12]}")
        if rev('main') != base:
            return False
        train.save(phase='landing', receipt=dict(base=base, merged=merged, snapshot=snapshot, notes=notes_ref,
                   message=message.strip(), prs=[{k: p[k] for k in ('number', 'snapshot', 'state_notes')} for p in group],
                   links=', '.join(f"[{w}]({runs[w]['url']})" for w in WORKFLOWS)))
        if mwt:
            git('merge', '--ff-only', merged, cwd=mwt)
        else:
            git('update-ref', 'refs/heads/main', merged, base)
        log(f"merge: main is now {merged[:12]}")
        finish_landing(train, wt)
    return True


def merge_tree(left, right):
    p = subprocess.run(['git', 'merge-tree', '--write-tree', left, right], cwd=SCRIPTS, capture_output=True, text=True)
    if p.returncode == 1:
        return None  # a content conflict, not a broken preflight
    if p.returncode:
        raise Refused(f'merge-tree failed: {p.stderr.strip()}')
    return p.stdout.splitlines()[0]


def admit(train, hw_evidence):
    batch = len(train['numbers']) > 1
    group, prefixes = [], [('main', rev('main'))]
    for n in train['queue'][0]:
        pr = load(n, batch)
        if batch and not pr['approved']:
            skip(train, n, 'no QA: approved verdict')
            continue
        if batch:
            # Once admitted, the candidate is the proof. Do not silently redo
            # initial admission during a bisect/resume and evict unrelated reds.
            if train.get('admitted', {}).get(str(n)) != pr['snapshot'] and not green(pr, batch=True):
                skip(train, n, pr['red_reason'])
                continue
            tip = pr['tip']
            base, head = prefixes[0][1], prefixes[-1][1]
            tree = merge_tree(base, tip)
            if not tree:
                skip(train, n, 'conflicts with main')
                continue
            if head != base:
                tree = merge_tree(head, tip)
                if not tree:
                    other = next(label for label, commit in prefixes[1:] if not merge_tree(commit, tip))
                    skip(train, n, f'conflicts with {other}')
                    continue
            prefixes.append((f'#{n}', git('commit-tree', tree, '-p', head, '-p', tip, '-m', 'train preflight')))
        if pr['hw'] != 'none' and n not in hw_evidence:
            raise Refused(f"#{n} needs {pr['hw']} hardware: qualify it with hw-batch, then pass --hw {n}=EVIDENCE")
        if not batch and not green(pr, train['phase'] == 'waiting', train.lock):
            raise Refused(f"#{n}: its snapshot isn't green; send it back to the implementer")
        group.append(pr)
    return group


def skip(train, number, reason):
    message = f'skipped: #{number} {reason}'
    log(f'merge: {message}')
    if message not in train['refused']:
        train['refused'].append(message)


def red_reason(runs):
    return 'red: ' + ', '.join(f"{w} {r['url']}" for w, r in runs.items())


def evict_red(train, group, candidate_runs):
    failed = {w for w in WORKFLOWS if w in candidate_runs and ci_state({w: candidate_runs[w]})[0] == 'red'}
    if not failed or len(train['numbers']) == 1:
        return False
    remaining = []
    for p in group:
        own = snapshot_runs(p['snapshot'], p['headRefName'])
        matches = {w: r for w, r in own.items() if w in failed and ci_state({w: r})[0] == 'red'
                   and not accepted_flake(p['snapshot'], {w: r}, [p], own=True)}
        if matches:
            skip(train, p['number'], red_reason(matches))
        else:
            remaining.append(p['number'])
    if len(remaining) == len(group):
        return False
    train.save(phase='ready', inputs=None, queue=([remaining] if remaining else []) + train['queue'][1:])
    return True


def drop_candidate(train, snapshot):
    branch = f"ci/{train['lane']}"
    if remote_head(branch) == snapshot:
        gh('api', '-X', 'DELETE', f'repos/{REPO}/git/refs/heads/{branch}', check=False)


def land(train, group, hw_evidence, dry_run):
    """Process one queue entry. Splits and completions checkpoint the rest atomically."""
    numbers = [p['number'] for p in group]
    rest = train['queue'][1:]
    pr = group[0]
    if (len(train['numbers']) == 1 and not train['notes'] and pr['own_green'] and
            pr['trail'].get('main') == rev('main') and train['phase'] == 'ready'):
        return land_tested(train, group, pr['snapshot'], pr['trail'], hw_evidence, dry_run, pr['headRefName'])
    if dry_run:
        log(f"merge: dry run: would test {numbers} as one candidate" +
            (f" with STATE notes {train['notes'][:12]}" if train['notes'] else ''))
        return True
    ok, snapshot = ci_publish(train, group, train['notes'])
    if not snapshot:
        raise Refused(f'merge: ci.sh could not publish {numbers}; see above')
    lane = train['lane']
    branch = f'ci/{lane}'
    if not ok:
        if evict_red(train, group, snapshot_runs(snapshot, branch)):
            drop_candidate(train, snapshot)
            return True
        ok = rerun_failed_once(train, snapshot)
    if not ok:
        runs = snapshot_runs(snapshot, branch)
        note = accepted_flake(snapshot, runs, group)
        if note:
            FLAKE_NOTES[snapshot] = note
            ok = True
        elif ci_state(runs)[0] != 'red':
            if ci_state(runs)[0] in ('cancelled', 'skipped'):
                train.save(phase='ready')
            raise Refused(f'merge: {snapshot} has no final CI verdict; repeat this command')
    if ok:
        msg = gh_json('api', f'repos/{REPO}/commits/{snapshot}', '--jq', '{m: .commit.message}')['m']
        if train.get('rerun') and snapshot not in FLAKE_NOTES:
            RERUN_NOTES[snapshot] = train['rerun']
        if not land_tested(train, group, snapshot, trailers_of(msg), hw_evidence, dry_run, branch):
            return False
    else:
        if evict_red(train, group, runs):
            drop_candidate(train, snapshot)
            return True
        if len(numbers) == 1:
            train['refused'].append(f'#{numbers[0]}: red when merged with current main')
            todo = []
        else:
            half = len(numbers) // 2
            todo = [numbers[:half], numbers[half:]]
            log(f'merge: the batch is red; bisecting {todo}')
        train.save(phase='ready', inputs=None, queue=todo + rest)
        drop_candidate(train, snapshot)
    return True


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
    parse_known_flakes(a.known_flake, set(a.numbers))
    if a.allow_workflow_changes:
        ALLOW_WORKFLOWS.append("--allow-workflow-changes")
    with (contextlib.nullcontext() if a.dry_run else MergeLock()) as lock:
        key = hashlib.sha256(json.dumps(vars(a), sort_keys=True).encode()).hexdigest()[:24]
        path = os.path.join(CI_DIR, f'merge-{key}.json')
        for other in sorted(glob.glob(os.path.join(CI_DIR, 'merge-*.json'))):
            prior = Train(other, lock)
            if prior['phase'] not in ('landing', 'closing'):
                continue
            if a.dry_run:
                raise Refused(f'merge: dry run: unfinished {prior["phase"]} in {other}; recovery required')
            if prior['phase'] == 'landing' and rev('main') == prior['receipt']['base']:
                if other != path:
                    raise Refused(f'merge: unfinished pre-fast-forward journal {other}; repeat its original command')
                # Re-qualify before the ff using the original command's flags.
                prior.save(phase='waiting' if prior.get('candidate') else 'ready', receipt=None)
            else:
                log(f'merge: finishing {other} ({prior["phase"]})')
                finish_landing(prior)
        train = Train(path, lock, a.numbers)
        if train['phase'] == 'new':
            train.save(phase='ready', queue=[train['numbers']], notes=state_notes_ref(a.notes) if a.notes else None)
        elif train['phase'] == 'done' and train['refused']:
            # Retry the outstanding subset after fixes, never PRs already landed.
            pending = [n for n in train['numbers'] if n not in train.get('landed', [])]
            train.save(phase='ready', queue=[pending] if pending else [], refused=[], inputs=None, admitted={})
        if train['notes'] and state_notes_ref(a.notes) != train['notes']:
            raise Refused('merge: --notes moved since this train started; pass its intended SHA')
        while train['queue']:
            for _attempt in range(3):
                group = admit(train, hw_evidence)
                if not group:
                    train.save(phase='ready', queue=train['queue'][1:])
                    break
                train.save(queue=[[p['number'] for p in group]] + train['queue'][1:],
                           tips={str(p['number']): p['tip'] for p in group},
                           admitted={**train.get('admitted', {}), **{str(p['number']): p['snapshot'] for p in group}})
                if land(train, group, hw_evidence, a.dry_run):
                    if a.dry_run:
                        train.save(queue=train['queue'][1:])
                    break
            else:
                raise Refused('merge: main kept moving; try again')
        if train['notes'] and not a.dry_run:
            log(f"merge: the STATE notes {train['notes'][:12]} didn't land; pass them again")
        for r in train['refused']:
            log(f'merge: {r}')
        train.save(phase='done')
        return 1 if train['refused'] else 0


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
