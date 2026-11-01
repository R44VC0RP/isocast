#!/usr/bin/env python3
"""Run all isolated merge-train scenarios, or pass one scenario name."""

import atexit, json, os, shutil, signal, subprocess, sys, tempfile, time
from pathlib import Path

cases = ("resume", "single", "guards", "ff", "stale", "bisect", "flake", "two", "orphan", "cancelled")
cases += ("post-push", "later-publication", "closing")
cases += ("unrelated-red", "newest")
cases += ("verdicts", "moving-workspace")
if not sys.argv[1:]:
    for case in cases:
        subprocess.run([sys.executable, __file__, case], check=True)
    sys.exit(0)
(mode,) = sys.argv[1:]
if mode not in cases:
    sys.exit(f"unknown scenario {mode!r}: choose one of {cases}")
src = Path(__file__).resolve().parents[2]
helper = Path(__file__).with_name("gh_sim.py")
sandbox = tempfile.TemporaryDirectory(prefix="merge-train-")
root = Path(sandbox.name).resolve()
groups = []


@atexit.register
def clean_up():
    for p in groups:
        try:
            os.killpg(p.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        p.wait()
    sandbox.cleanup()


priv = root / "private"
pub = root / "public"
ci = root / "ci"
bin = root / "bin"
priv.mkdir()
bin.mkdir()
env = dict(
    HOME=str(root),
    LC_ALL="C",
    SIM=str(root),
    GIT_CONFIG_GLOBAL=str(root / "gitconfig"),
    GIT_CONFIG_NOSYSTEM="1",
    GIT_ALLOW_PROTOCOL="file",
    PYTHONDONTWRITEBYTECODE="1",
    PATH=str(bin) + ":" + os.environ["PATH"],
    ISOCAST_CI_DIR=str(ci),
    ISOCAST_MERGE_DIR=str(root / "merge"),
    ISOCAST_PUBLIC_DIR=str(pub),
    GIT_TERMINAL_PROMPT="0",
)


def cmd(*args, cwd=priv, inp=None):
    return subprocess.run(
        args, cwd=cwd, env=env, input=inp, text=True, check=True, stdout=subprocess.PIPE
    ).stdout.strip()


def git(*args, cwd=priv, inp=None):
    return cmd("git", *args, cwd=cwd, inp=inp)


def script(path, body):
    path.write_text(body)
    path.chmod(0o755)


def commit(msg):
    git("add", "-A")
    git("commit", "-qm", msg)
    return git("rev-parse", "HEAD")


def wait(pred, p, log):
    deadline = time.monotonic() + 100
    while time.monotonic() < deadline:
        if pred():
            return
        if p.poll() is not None:
            raise RuntimeError(log.read_text())
        time.sleep(0.15)
    raise RuntimeError("timeout " + log.read_text())


def go(args, label, expected=None):
    path = root / (label + ".log")
    print(label, " ".join(args), flush=True)
    with path.open("w") as f:
        p = subprocess.Popen(
            ["scripts/merge-pr.sh", *args],
            cwd=priv,
            env=env,
            stdout=f,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
    groups.append(p)
    if expected is None:
        return p, path
    return finish(p, path, expected)


def finish(p, path, expected):
    rc = p.wait(240)
    text = path.read_text()
    assert rc == expected, (rc, expected, text)
    groups.remove(p)
    return text


def write(name, data):
    (priv / name).write_text(data + "\n")


def journal():
    return json.loads(next(ci.glob("merge-*.json")).read_text())


git("config", "--global", "user.name", "Fixture")
git("config", "--global", "user.email", "fixture@example.test")
git(
    "config",
    "--global",
    "url." + str(root / "origin") + ".insteadOf",
    "https://github.com/R44VC0RP/isocast.git",
)
git("init", "--bare", "-q", "-b", "main", str(root / "origin"))
git("init", "-q", "-b", "main")
scripts = priv / "scripts"
scripts.mkdir()
shutil.copy(helper, bin / "gh")
(bin / "gh").chmod(0o755)
script(bin / "sleep", "#!/bin/sh\nexec /bin/sleep 0.25\n")
for file in ("ci.sh", "pr_flow.py", "merge-pr.sh", "publish-snapshot.sh", "prs.sh"):
    shutil.copy2(src / "scripts" / file, scripts / file)
script(
    scripts / "remote-build.sh",
    '#!/bin/sh\n[ "$*" = "cargo check --workspace --all-targets --locked" ] || exit 49\necho "workspace $(git rev-parse HEAD)" >> "$SIM/events"\nwhile [ -e "$SIM/hold-workspace" ]; do /bin/sleep .1; done\n[ ! -f "$SIM/fail-workspace" ]\n',
)
wf = priv / ".github/workflows"
wf.mkdir(parents=True)
(priv / ".github/scripts").mkdir()
shutil.copy2(src / ".github/scripts/ci_gate.py", priv / ".github/scripts/ci_gate.py")
for i, w in enumerate(("CI", "Engine E2E", "Windows")):
    (wf / f"{i}.yml").write_text(f"name: {w}\n")
write("old", "base")
write("shared", "base")
(priv / "docs").mkdir()
write("docs/keep", "base")
base = commit("base")
git("clone", "-q", str(priv), str(pub))
git("remote", "set-url", "origin", str(root / "origin"), cwd=pub)
git("push", "-q", "origin", "main", cwd=pub)
(pub / ".git/isocast-published").write_text(
    base + "\n"
)  # publisher knows the private revision of its seeded snapshot
write("old", "main")
main = commit("main changed old")
git("switch", "-qc", "state-notes")
write("docs/notes", "notes")
notes = commit("notes")
prs = {}
for i in range(1, 11):
    n = str(i)
    git("switch", "-qc", "agent/p" + n, base if n == "4" else main)
    write(
        "shared"
        if n in ("1", "5")
        else ("old" if n == "4" else ("docs/file" if n in ("9", "10") else "file") + n),
        "change " + n,
    )
    tip = commit("change " + n)
    tree = git("rev-parse", "HEAD^{tree}")
    env["GIT_COMMITTER_DATE"] = "2025-01-01T00:00:00Z"
    sha = git(
        "commit-tree",
        tree,
        "-p",
        base,
        "-m",
        f"Isocast-Source: {tip}\nIsocast-Main: {base if n == '4' else main}\nIsocast-Tree: {tree}",
    )
    del env["GIT_COMMITTER_DATE"]
    git("push", "-q", str(root / "origin"), f"{sha}:refs/heads/ci/p{n}")
    prs[n] = dict(tip=tip, sha=sha)
git("switch", "-q", "main")
(root / "prs.json").write_text(json.dumps(prs))
(root / "events").touch()
(root / "need-hw").touch()
if mode == "resume":
    args = ["1", "2", "3", "4", "5", "6", "7", "8", "9", "--notes", notes, "--hw", "2=fixture qualified"]
    dry = go(["--dry-run", *args], "admission", 1)
    for line in (
        "skipped: #3 red",
        "skipped: #4 conflicts with main",
        "skipped: #5 conflicts with #1",
        "skipped: #6 no QA:",
        "would test [1, 2, 7, 8, 9] as one candidate",
    ):
        assert line in dry, line
    assert not ci.exists()
    (root / "hold").touch()
    (root / "late-failure").touch()
    p, log = go(args, "first")
    wait(
        lambda: ci.exists() and any(ci.glob("merge-*.json")) and len(journal().get("run_ids", {})) == 3,
        p,
        log,
    )
    saved = journal()
    assert saved["phase"] == "waiting" and set(saved["tips"]) == {"1", "2", "7", "8", "9"}
    os.killpg(p.pid, signal.SIGKILL)
    p.wait()
    groups.remove(p)
    assert git("rev-parse", "main") == main
    print("KILLED mid-wait; candidate", saved["candidate"], "runs", saved["run_ids"])
    (root / "hold").unlink()
    result = go(args, "resumed", 1)
    assert f"ci: found {saved['candidate']}" in result
    assert (
        result.count("merge: publishing ci/") == 1
        and f"skipped: #7 red: Windows https://example.test/{int(prs['7']['sha'][:10], 16) * 4 + 2}" in result
        and "bisecting" not in result
    )
    assert git("rev-list", "--count", "--first-parent", main + "..main") == "5"  # four PR merges + notes
    events = (root / "events").read_text().splitlines()
    assert {x for x in events if x.startswith("close ")} == {"close 1", "close 2", "close 8", "close 9"}
    assert len([x for x in events if x.startswith("workspace ")]) == 1
    assert all(
        x.split()[1].startswith("ci/p") and "Windows" not in x for x in events if x.startswith("cancel ")
    )
    assert "cancel ci/p1 CI" in events and "cancel ci/p1 Engine E2E" in events
    assert git("rev-list", "--count", "main", cwd=pub) == "2"
    assert journal()["phase"] == "done"
    assert (priv / "docs/notes").is_file()
    print(
        "PASS: 5 admitted pending/missing/cancelled, 4 landed, 1 late Windows red evicted; 0 own waits, 0 bisections, 1 publish."
    )
elif mode == "single":
    go(["10"], "fast", 0)
    go(["10"], "again", 0)
    assert not any(ci.glob("batch-*"))
    assert (root / "events").read_text().count("close 10") == 1
    assert git("rev-list", "--count", "main", cwd=pub) == "2"
    print("PASS: single current green uses 0 candidates, repeat no-op")
elif mode == "guards":
    assert "needs mac hardware" in go(["--dry-run", "1", "2"], "nohardware", 1)
    assert "must change docs/ only" in go(["--dry-run", "1", "2", "--notes", "agent/p1"], "badnotes", 1)
    (root / "fail-workspace").touch()
    assert "workspace check failed" in go(["10"], "badworkspace", 1)
    assert git("rev-parse", "main") == main
    (root / "wrong-tree").touch()
    assert "isn't the tree CI tested" in go(["10"], "badtree", 1)
    assert git("rev-parse", "main") == main
    print(
        "PASS: missing hardware, non-docs notes, workspace failure and mismatched tree all leave main unchanged"
    )
elif mode == "flake":
    (root / "flake").touch()
    flag = "9=e2e: fixture failed on capacity"
    assert "batch's paths can affect" in go(["1", "9", "--known-flake", flag], "unsafe-flake", 1)
    assert git("rev-parse", "main") == main
    go(["9", "10", "--known-flake", flag], "docs-flake", 0)
    assert "Known flake accepted by QA" in git("log", "-1", "--format=%B")
    print("PASS: evidence-backed flake allowed only for unaffected paths")
elif mode == "ff":
    hook = priv / ".git/hooks/post-merge"
    script(
        hook, '#!/bin/sh\nif [ "$PWD" = "$SIM/private" ]; then touch "$SIM/moved"; exec /bin/sleep 90; fi\n'
    )
    p, log = go(["1", "8"], "before-publish")
    wait(lambda: (root / "moved").exists(), p, log)
    os.killpg(p.pid, signal.SIGKILL)
    p.wait()
    groups.remove(p)
    saved_main = git("rev-parse", "main")
    assert journal()["phase"] == "landing"
    assert git("rev-list", "--count", "main", cwd=pub) == "1"
    hook.unlink()
    go(["1", "8"], "recover-publish", 0)
    go(["1", "8"], "repeat", 0)
    assert git("rev-parse", "main") == saved_main and git("rev-list", "--count", "main", cwd=pub) == "2"
    assert (root / "events").read_text().count("workspace ") == 1 and (root / "events").read_text().count(
        "close "
    ) == 2
    print("PASS: SIGKILL after private ff, then 1 public snapshot and 2 closures; repeat no-op")
elif mode in ("post-push", "later-publication", "closing"):
    # Stop at three distinct durable boundaries, without patching the merger.
    if mode == "later-publication":
        hook = priv / ".git/hooks/post-merge"
        condition = '[ "$PWD" = "$SIM/private" ]'
    elif mode == "post-push":
        hook = root / "origin/hooks/post-receive"
        condition = 'grep -q " refs/heads/main$"'
    else:
        hook = None
        (root / "pause-close").touch()
    if hook:
        script(hook, f'#!/bin/sh\nif {condition}; then touch "$SIM/boundary"; exec /bin/sleep 90; fi\n')
    p, log = go(["1", "8"], "interrupted")
    wait(lambda: (root / "boundary").exists(), p, log)
    os.killpg(p.pid, signal.SIGKILL)
    p.wait()
    groups.remove(p)
    if hook:
        hook.unlink()
    assert journal()["phase"] == ("closing" if mode == "closing" else "landing")
    write("intervening", "advanced private main")
    current = commit("main advanced during landing")
    if mode == "later-publication":
        assert "cannot prove" in go(["10"], "unpublished", 1)
        assert not (root / "closed-1").exists() and git("rev-parse", "main") == current
        cmd("scripts/publish-snapshot.sh", "--push", "A later snapshot includes the private merge")
    # A foreign command may close a superseded PR, never delete its new head.
    prs["8"]["sha"] = prs["10"]["sha"]
    (root / "prs.json").write_text(json.dumps(prs))
    git("update-ref", "refs/heads/ci/p8", prs["8"]["sha"], cwd=root / "origin")
    # No pushes at all on recovery, even if an idempotent git push would succeed.
    script(pub / ".git/hooks/pre-push", '#!/bin/sh\ntouch "$SIM/unexpected-push"; exit 1\n')
    (root / "pause-close").unlink(missing_ok=True)
    assert "unfinished" in go(["--dry-run", "3"], "read-only", 1) and not (root / "closed-8").exists()
    out = go(["3"], "other-command", 1)  # first finish the old journal, then refuse #3's own red run
    assert "finishing" in out and (root / "closed-1").exists() and (root / "closed-8").exists(), out
    assert git("rev-parse", "refs/heads/ci/p8", cwd=root / "origin") == prs["8"]["sha"]
    assert git("rev-parse", "main") == current and not (root / "unexpected-push").exists()
    assert (root / "events").read_text().count("workspace ") == 1
    print(
        f"PASS: {mode}: foreign train recovered publication without merging, pushing or deleting a moved head"
    )
elif mode == "stale":
    (root / "hold").touch()
    p, log = go(["1", "8"], "wait-old")
    wait(
        lambda: ci.exists() and any(ci.glob("merge-*.json")) and len(journal().get("run_ids", {})) == 3,
        p,
        log,
    )
    os.killpg(p.pid, signal.SIGKILL)
    p.wait()
    groups.remove(p)
    old = journal()["candidate"]
    write("intervening", "main advanced")
    commit("intervening main")
    (root / "hold").unlink()
    out = go(["1", "8"], "rebuilt", 0)
    assert (
        old != journal()["candidate"]
        and out.count("merge: publishing ci/") == 1
        and (priv / "intervening").exists()
    )
    print("PASS: changed main invalidated saved candidate; only the rebuilt tree landed")
elif mode == "bisect":
    (root / "bad-combination").touch()
    (root / "need-hw").unlink()
    # Simulate a post-dispatch completed re-run after exposing the in-progress transition once.
    go(["1", "2"], "bisect", 0)
    assert (root / "events").read_text().count("workspace ") == 2
    print("PASS: red combination bisected, both independently green halves landed")
elif mode == "unrelated-red":
    (root / "different-failures").touch()
    text = go(["1", "7"], "nonmatching", 0)
    assert "bisecting" in text and "skipped: #7" not in text and (root / "closed-7").exists()
    print("PASS: late Windows failure was not blamed for a candidate CI failure, even during bisection")
elif mode == "newest":
    (root / "older-runs").touch()
    go(["10"], "newest", 0)
    assert not any(ci.glob("batch-*")) and (root / "closed-10").exists()
    print("PASS: newest exact-SHA push run won over an older failure with the same timestamp")
elif mode == "verdicts":
    (root / "verdicts").touch()
    reviews = {row["number"]: row["review"] for row in json.loads(cmd("scripts/prs.sh", "--json"))}
    expected = {1: "-", 2: "changes", 4: "-", 6: "-", 7: "-", 8: "approved", 10: "approved"}
    assert {n: reviews[n] for n in expected} == expected, reviews
    out = go(["--dry-run", *map(str, expected)], "freshness", 1)
    assert "would test [8, 10] as one candidate" in out, out
    assert all(f"skipped: #{n} no QA" in out for n in (1, 2, 4, 6, 7)), out
    print(
        "PASS: list and merge agreed on stale, equal, later changes, untrusted, minimized, dismissed and fresh verdicts"
    )
elif mode == "moving-workspace":
    gate = root / "hold-workspace"
    gate.touch()
    p, log = go(["1", "8"], "during-check")
    wait(lambda: "workspace " in (root / "events").read_text(), p, log)
    first = journal()["candidate"]
    write("intervening", "main changed during workspace check")
    advanced = commit("intervening")
    gate.unlink()
    finish(p, log, 0)
    assert (root / "events").read_text().count("workspace ") == 2 and journal()["candidate"] != first
    git("merge-base", "--is-ancestor", advanced, "main")
    assert git("rev-list", "--count", "main", cwd=pub) == "2"
    print("PASS: main moved during workspace check; only the rebuilt and rechecked candidate landed")
elif mode in ("two", "orphan"):
    (root / "hold").touch()
    p, log = go(["1", "8"], "first")
    wait(lambda: any(ci.glob("merge-*.json")) and len(journal().get("run_ids", {})) == 3, p, log)
    candidate = journal()["candidate"]
    if mode == "orphan":
        p.kill()
        p.wait()  # Leave the real ci.sh --wait child holding the lock.
    q, second = go(["1", "8"], "second")
    wait(lambda: "waiting for another merge-pr" in second.read_text(), q, second)
    assert git("rev-parse", "main") == main
    (root / "hold").unlink()
    if mode == "two":
        finish(p, log, 0)
    finish(q, second, 0)
    if mode == "orphan":
        groups.remove(p)
    assert journal()["candidate"] == candidate and journal()["phase"] == "done"
    assert (
        git("rev-list", "--count", "main", cwd=pub) == "2"
        and (root / "events").read_text().count("workspace ") == 1
    )
    print(f"PASS: {mode}: concurrent invocations preserved one candidate and one publication")
elif mode == "cancelled":
    (root / "cancel-candidate").touch()
    out = go(["1", "8"], "cancelled", 1)
    assert "no final CI verdict" in out and git("rev-parse", "main") == main
    old = journal()["candidate"]
    (root / "cancel-candidate").unlink()
    go(["1", "8"], "replace", 0)
    assert old != journal()["candidate"] and (root / "events").read_text().count("workspace ") == 1
    print("PASS: cancelled candidate did not land; fresh green candidate did")
