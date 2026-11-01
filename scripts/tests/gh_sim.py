#!/usr/bin/env python3
"""Fake gh for train_sim.py: the origin is a local bare Git repository."""

import json, os, re, subprocess, sys
from pathlib import Path

root = Path(os.environ["SIM"])
db = json.loads((root / "prs.json").read_text())
args = sys.argv[1:]
ws = ["CI", "Engine E2E", "Windows"]


def val(k):
    return args[args.index(k) + 1]


def say(v):
    print(json.dumps(v))


def event(s):
    with (root / "events").open("a") as f:
        print(s, file=f)


def bare(*cmd):
    return subprocess.check_output(["git", "--git-dir", str(root / "origin"), *cmd], text=True).strip()


def is_open(n):
    return not (root / f"closed-{n}").exists()


def runs(sha, branch):
    result = []
    lane = branch.removeprefix("ci/")
    batch = lane.startswith("batch-")
    for i, w in enumerate(ws):
        status = "completed"
        conclusion = "success"
        if not batch:
            n = lane.removeprefix("p")
            if n == "3" and w == "Windows":
                conclusion = "failure"
            if n in ("1", "2", "7") and is_open(n):
                status = "in_progress"
                conclusion = None
            if n == "8":
                continue
            if n == "9":
                conclusion = "cancelled"
            if (root / f"cancel-{sha}-{i}").exists():
                status = "completed"
                conclusion = "cancelled"
            if n == "7" and w == "Windows" and (root / "red").exists():
                status = "completed"
                conclusion = "failure"
            if (root / "lightweight").exists() and w != "Windows" and not lane.startswith("full/"):
                if w == "CI":
                    continue  # branch-filtered: no push run at all
                status, conclusion = "completed", "skipped"
        else:
            trail = bare("log", "-1", "--format=%B", sha)
            if (root / "hold").exists():
                status = "queued"
                conclusion = None
            elif (root / "cancel-candidate").exists() and w == "CI":
                conclusion = "cancelled"
            elif (root / "skip-candidate").exists() and w == "Engine E2E":
                conclusion = "skipped"
            elif db["7"]["tip"] in trail and w == "Windows" and (root / "late-failure").exists():
                (root / "red").touch()
                conclusion = "failure"
            elif (
                (root / "different-failures").exists()
                and w == "CI"
                and all(db[n]["tip"] in trail for n in ("1", "7"))
            ):
                (root / "red").touch()  # the PR fails Windows while the combination fails macOS
                conclusion = "failure"
            elif (
                (root / "bad-combination").exists()
                and all(db[n]["tip"] in trail for n in ("1", "2"))
                and w == ("Engine E2E" if (root / "lightweight").exists() else "CI")
            ):
                conclusion = "failure"
            elif (root / "flake").exists() and w == "Engine E2E":
                conclusion = "failure"
        id = str(int(sha[:10], 16) * 4 + i)
        (root / f"run-{id}").write_text(f"{sha} {branch} {i}")
        result.append(
            dict(
                databaseId=int(id),
                workflowName=w,
                headBranch=branch,
                headSha=sha,
                status=status,
                conclusion=conclusion,
                createdAt="2026-01-01T00:00:00Z",
                url="https://example.test/" + id,
                jobs=[],
            )
        )
    return result


def pr_data(n):
    pr = db[n]
    sha = pr["sha"]
    c = dict(
        createdAt="2025-01-01T00:00:01Z", body="QA: approved", authorAssociation="OWNER", isMinimized=False
    )
    data = dict(
        number=int(n),
        createdAt="2025-01-01T00:00:00Z",
        title="Fixture " + n,
        body="## What and why\nProtect the train.\n## How it was verified\nExercised the gate.\n"
        + "<!-- isocast-pr "
        + json.dumps(dict(branch="agent/p" + n, lane="p" + n))
        + " -->",
        url="https://example.test/pr/" + n,
        state="OPEN" if is_open(n) else "CLOSED",
        isDraft=False,
        baseRefName="main",
        headRefName="ci/p" + n,
        headRefOid=sha,
        labels=[
            {"name": "needs:" + ("mac" if n in ("2", "3", "5") and (root / "need-hw").exists() else "none")}
        ],
        comments=[c],
        reviews=[],
    )
    if n == "6":
        data["comments"] = [dict(c, authorAssociation="NONE")]
    if (root / "verdicts").exists():
        if n == "1":
            data["comments"] = [dict(c, createdAt="2024-12-31T23:59:59Z")]
        if n == "2":
            data["reviews"] = [
                dict(c, submittedAt="2025-01-01T00:00:02Z", body="QA: changes requested", state="COMMENTED")
            ]
        if n == "4":
            data["comments"] = [dict(c, createdAt="2025-01-01T00:00:00Z")]
        if n == "7":
            data["comments"] = [dict(c, isMinimized=True)]
        if n == "8":
            data["reviews"] = [
                dict(c, submittedAt=c["createdAt"], body="QA: changes requested", state="DISMISSED")
            ]
    return data


def graphql_pr(n):
    pr = pr_data(n)
    sha = pr["headRefOid"]
    pr["labels"] = {"nodes": pr["labels"]}
    pr["commits"] = {
        "nodes": [
            {
                "commit": {
                    "oid": sha,
                    "message": bare("log", "-1", "--format=%B", sha),
                    "committedDate": bare("log", "-1", "--format=%cI", sha),
                }
            }
        ]
    }
    for name in ("comments", "reviews"):
        pr[name] = {"nodes": pr[name], "pageInfo": {"hasPreviousPage": False, "startCursor": None}}
    return pr


if args[:2] == ["api", "graphql"]:
    n = next((s[2:] for s in args if s.startswith("n=")), None)
    result = (
        {"pullRequest": graphql_pr(n)}
        if n
        else {"pullRequests": {"nodes": [graphql_pr(n) for n in db if is_open(n)]}}
    )
    say({"data": {"repository": result}})
elif args[:2] == ["pr", "view"]:
    fields = val("--json").split(",")
    data = pr_data(args[2])
    say({k: data[k] for k in fields})
elif args[0] == "api" and "DELETE" in args:
    bare("update-ref", "-d", args[-1].split("/git/")[1])
    event("delete " + args[-1])
elif args[0] == "api" and "/commits/" in args[1]:
    sha = args[1].rsplit("/", 1)[1]
    msg = bare("log", "-1", "--format=%B", sha)
    if (root / "wrong-tree").exists():
        msg = re.sub(r"Isocast-Tree: .*", "Isocast-Tree: " + "0" * 40, msg)
    say({"m": msg})
elif args[0] == "api" and "/jobs?" in args[1]:
    say({"jobs": [{"name": "e2e", "conclusion": "failure"}]})
elif args[:2] == ["run", "list"]:
    if "--commit" not in args:
        say([r for n in db for r in runs(db[n]["sha"], f"ci/p{n}")])
        sys.exit(0)
    sha = val("--commit")
    branch = val("--branch")
    result = runs(sha, branch)
    if "--jq" in args:
        name = re.search(r'workflowName == "(.*?)"', val("--jq"))[1]
        print(next(r["databaseId"] for r in result if r["workflowName"] == name))
    else:
        once = root / ("rerunning-" + sha)
        if once.exists():
            workflow = once.read_text()
            once.unlink()
            for r in result:
                if r["workflowName"] == workflow:
                    r.update(status="in_progress", conclusion=None)
        # Even a misbehaving API returning the same SHA on main must never cause its cancellation.
        if (root / "older-runs").exists():
            result += [
                dict(r, databaseId=r["databaseId"] - 100, conclusion="failure", status="completed")
                for r in result
                if r["workflowName"] == "CI"
            ]
        say(
            result
            + [dict(r, headBranch="main", databaseId=-r["databaseId"]) for r in result]
            + [
                dict(
                    r, headSha="0" * 40, status="completed", conclusion="failure", databaseId=-r["databaseId"]
                )
                for r in result
            ]
        )
elif args[:2] in (["run", "view"], ["run", "cancel"], ["run", "rerun"]):
    sha, branch, index = (root / ("run-" + args[2])).read_text().split()
    w = ws[int(index)]
    if args[1] == "view":
        if "--json" in args:
            say(next(r for r in runs(sha, branch) if r["workflowName"] == w))
        else:
            print("fixture failed on capacity")
    else:
        event(f"{args[1]} {branch} {w}")
        assert args[1] != "cancel" or (branch.startswith("ci/p") and w != "Windows")
        if args[1] == "cancel":
            (root / f"cancel-{sha}-{index}").touch()
        if args[1] == "rerun":
            (root / ("rerunning-" + sha)).write_text(w)
elif args[:2] == ["pr", "close"]:
    n = args[2]
    assert is_open(n)
    event("close " + n)
    (root / ("closed-" + n)).touch()
    if "--delete-branch" in args:
        bare("update-ref", "-d", "refs/heads/ci/p" + n)
    if n == "1" and (root / "pause-close").exists():
        import time

        (root / "boundary").touch()
        time.sleep(90)
else:
    sys.exit("unknown fake gh command " + str(args))
