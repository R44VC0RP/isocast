#!/usr/bin/env python3
"""Decides which CI jobs a change needs, and skips trees that already passed.

    ci_gate.py WORKFLOW            # in a workflow's `gate` job (ci, e2e or windows)
    ci_gate.py WORKFLOW --paths P… # offline: classify these paths, print the plan
    ci_gate.py --explain           # the path rules and job rules, as a table

Each job gets a mode, written to $GITHUB_OUTPUT as `<job>=<mode>`:

  full   run everything
  docs   (check only) fmt, the reference-doc check and the scans
  warm   (main only) restore the Rust cache; build only if it missed, so
         main keeps the caches every branch restores; run no tests
  skip   don't run

1. Identical trees. On a push (main or ci/**), when another commit with the
   same tree already passed CI, Engine E2E and Windows (push runs), nothing
   runs again: main's jobs only warm their caches, a ci/** candidate's skip.
2. Changed paths. Otherwise the paths that changed (a push to main:
   against main's previous head; a ci/** snapshot: against its parent,
   public main; a pull request: against its merge base) are tagged by
   PATH_RULES, and a job runs when any path carries a tag in its
   JOB_RULES entry. A path no rule knows, and anything under .github/ or
   the Cargo manifests, is tagged `all`: every job runs. A push whose
   base isn't green (its three workflows' latest push runs, or an
   identical green tree) is compared with the newest green first-parent
   ancestor within MAX_ANCESTORS instead (the diff then includes the
   base's own changes); with none, everything runs, as do manual runs and
   other events.
   On main, a job that doesn't need to run warms its cache instead.

The job summary says, for every job, why it runs or not.
"""

import functools
import json
import os
import re
import subprocess
import sys
import urllib.request

WORKFLOW_FILES = {"CI": "ci.yml", "Engine E2E": "e2e.yml", "Windows": "windows.yml"}

# Each pattern matches a whole path; the first match wins. Tags: all, docs, tooling, ui, render, shared, mac,
# windows, linux, e2e, motion, contracts, release_tools.
ENGINE_NOT_RENDER = (
    "audio_filter|capacity|critical|data|doctor|encoder_calibration|estimate|governor|hotkeys|"
    "machine_profile|project|time|trace|tune|wasm")
PATH_RULES = [
    (r"\.github/.*", {"all"}, "workflows and their scripts"),
    (r"(Cargo\.toml|Cargo\.lock|rust-toolchain(\.toml)?|clippy\.toml|rustfmt\.toml|\.cargo/.*|vendor/.*)",
     {"all"}, "the build itself"),
    (r"crates/isocast-(ui|ui-interface)/.*", {"ui"}, "the app"),
    (r"crates/isocast-app/.*", {"ui", "mac"}, "the retired web app"),
    (r"crates/isocast-engine/src/(record|stream|replay)/.*", {"shared"}, "engine (not drawing)"),
    (rf"crates/isocast-engine/src/({ENGINE_NOT_RENDER})\.rs", {"shared"}, "engine (not drawing)"),
    (r"crates/isocast-engine/src/media_engines\.json", {"shared"}, "engine (not drawing)"),
    (r"crates/isocast-engine/.*", {"shared", "render"}, "engine (compositor and render)"),
    (r"crates/isocast-surfaces/.*", {"shared", "render"}, "GPU surfaces"),
    # The Linux browser helper embeds macOS's page-audio script.
    (r"crates/isocast-platform-macos/src/browser/audio\.js", {"mac", "linux"}, "browser page audio script"),
    (r"crates/isocast-platform-macos/src/(gpu|frames|pacing)\.rs", {"mac", "render"}, "macOS render"),
    (r"crates/isocast-platform-macos/.*", {"mac"}, "macOS platform"),
    (r"crates/isocast-vp8/.*", {"mac"}, "macOS VP8"),
    (r"camera-extension/.*", {"mac"}, "macOS camera extension"),
    (r"crates/isocast-platform-windows/.*", {"windows"}, "Windows platform"),
    (r"crates/isocast-platform-linux/.*", {"linux"}, "Linux platform"),
    # main.rs is the stub every platform compiles; the rest is Linux-only.
    (r"crates/isocast-browser/src/main\.rs", {"linux", "mac", "windows"}, "browser helper entry point"),
    (r"crates/isocast-browser/.*", {"linux"}, "Linux browser helper"),
    (r"crates/isocast-(protocol|daemon|cli|client|ipc|discovery|diagnostics|mux|obsws|remote|"
     r"platform-credentials|platform-stub|openh264)/.*", {"shared"}, "shared crates"),
    (r"crates/isocast-platform-contracts/.*", {"shared", "contracts"}, "platform contracts"),
    (r"(examples|wit)/.*", {"shared"}, "plugins"),
    (r"packaging/(linux|deb)/.*", {"linux"}, "Linux packaging"),
    (r"scripts/(package-linux\.sh|package-deb\.sh|linux-[^/]*)", {"linux"}, "Linux scripts"),
    (r"scripts/windows/.*", {"windows"}, "Windows packaging"),
    (r"scripts/(windows-[^/]*|oracle-windows\.sh)", {"windows"}, "Windows scripts"),
    (r"scripts/(e2e\.py|pacing\.py|pacing-probe\.m)", {"e2e"}, "the e2e harness"),
    (r"scripts/(platform-contracts\.py|test_platform_contracts\.py)", {"contracts"}, "contract harness"),
    (r"scripts/(motion\.sh|motion/.*)", {"motion"}, "the motion harness"),
    (r"scripts/screenshots/.*", {"ui"}, "app screenshots"),
    (r"scripts/(visual[-_][^/]*|ui-screenshots\.sh|layout-audit.*)", {"ui"}, "app screenshot tools"),
    # The release asset checks run in the Windows workflow's portable job (ubuntu).
    (r"scripts/release-finalize\.py", {"release_tools"}, "release tooling (tested by the portable job)"),
    (r"scripts/(dmg/.*|sparkle/.*|release[^/]*|sign-dev\.sh|build-camera-extension\.sh|vcam-[^/]*|"
     r"app-dev\.sh|ui-dev\.sh)", {"mac"}, "macOS packaging"),
    (r"scripts/(reference-docs\.py|third-party-notices\.sh)", {"docs"}, "doc generators"),
    (r"scripts/(ci\.sh|pr\.sh|prs\.sh|merge-pr\.sh|pr_flow\.py|publish-snapshot\.sh|remote-build\.sh|"
     r"hw-batch\.py|testmac[^/]*|vm[^/]*\.sh|snapshot\.sh|soak\.py|stress\.py|kill-matrix\.py|bench\.py|"
     r"weak-[^/]*|drag-lag\.py|obs-bench\.py)", {"tooling"}, "local tools (no CI job runs them)"),
    (r"fuzz/.*", {"tooling"}, "fuzz workspace (not built by CI)"),
    (r"(docs|website|changelog)/.*", {"docs"}, "docs"),
    (r"[^/]+\.md", {"docs"}, "docs"),
    (r"(LICENSE-[A-Z]+|THIRD_PARTY_NOTICES)", {"docs"}, "licences"),
]

# Which tags make a job run fully.
JOB_RULES = {
    "ci": {
        "portable": ({"all", "docs", "tooling", "ui", "render", "shared", "mac", "windows", "linux", "e2e",
                      "motion", "contracts", "release_tools"}, "fmt, scans and the portable crates (ubuntu)"),
        "check": ({"all", "ui", "render", "shared", "mac", "contracts"}, "clippy and tests on macOS"),
        "motion": ({"all", "ui", "render", "motion"}, "the motion suite (UI, compositor, render)"),
    },
    "e2e": {
        "e2e": ({"all", "render", "shared", "mac", "e2e"}, "the engine end to end on macOS"),
    },
    "windows": {
        "portable": ({"all", "ui", "render", "shared", "windows", "linux", "release_tools"},
                     "portable crates on Windows and Linux, release asset checks"),
        "linux": ({"all", "render", "shared", "linux", "e2e"}, "headless Linux daemon"),
        "linux_capture": ({"all", "render", "shared", "linux", "e2e"}, "Linux capture paths"),
        # The only job that compiles isocast-ui (and its dependencies' Linux
        # paths) on Linux: shared and render changes need it too.
        "linux_app": ({"all", "ui", "render", "shared", "linux"}, "Linux app and tarball"),
        # Built from the same code as the app and tarball (Ubuntu's from the
        # tarball itself), so the same changes need them.
        "deb": ({"all", "ui", "render", "shared", "linux"}, "Linux .deb packages and their install tests"),
        "win_build": ({"all", "ui", "render", "shared", "windows", "e2e", "contracts"},
                      "Windows build, unit tests and contracts"),
        "win_release": ({"all", "render", "shared", "windows", "e2e"}, "Windows release qualification"),
        "win_recovery": ({"all", "render", "shared", "windows", "e2e"}, "Windows recording recovery"),
        # The wording lives partly in the shared crates (isocast-cli's help);
        # the installer runs the shared daemon and CLI. Both reuse the build.
        "win_wording": ({"all", "ui", "shared", "windows"}, "Windows wording snapshots"),
        "win_package": ({"all", "ui", "shared", "windows"}, "Windows installer and update packages"),
    },
}
# The jobs whose modes include `docs` (fmt, reference docs, scans).
DOCS_MODE = {("ci", "check")}
# Jobs with no Rust cache of their own (they run from the build job's
# binaries): nothing to warm, so on main they skip instead.
NO_CACHE = {("windows", "win_recovery"), ("windows", "win_wording"), ("windows", "win_package"),
            # The Debian 12 build's cache is saved only by a full run on main.
            ("windows", "deb")}


def idle(workflow, job, on_main):
    """The mode of a job that needn't run: main warms caches."""
    return "warm" if on_main and (workflow, job) not in NO_CACHE else "skip"


def tags_of(path):
    for pattern, tags, what in PATH_RULES:
        if re.fullmatch(pattern, path):
            return tags, what
    return {"all"}, "unknown path (runs everything)"


def git(*args):
    return subprocess.run(["git", *args], check=True, capture_output=True, text=True).stdout.strip()


@functools.lru_cache(maxsize=None)
def api(path):
    req = urllib.request.Request(f"https://api.github.com/{path}", headers={
        "Authorization": f"Bearer {os.environ['GH_TOKEN']}", "Accept": "application/vnd.github+json"})
    with urllib.request.urlopen(req, timeout=20) as r:
        return json.load(r)


# How many recent green commits identical_green compares trees with.
MAX_CANDIDATES = 30
# How far back (first parents) a push whose base isn't green looks for a
# green ancestor to compare with; beyond it, everything runs.
MAX_ANCESTORS = 10


def identical_green(repo, sha, tree):
    """Another commit with TREE that passed every workflow on a push: (sha, run urls) or None."""
    passed = None
    urls, newest = {}, {}
    for name, file in WORKFLOW_FILES.items():
        runs = api(f"repos/{repo}/actions/workflows/{file}/runs?event=push&status=success&per_page=100")
        shas = {r["head_sha"] for r in runs["workflow_runs"] if r["head_sha"] != sha}
        for r in runs["workflow_runs"]:
            urls.setdefault((name, r["head_sha"]), r["html_url"])
            newest[r["head_sha"]] = max(newest.get(r["head_sha"], ""), r["created_at"])
        passed = shas if passed is None else passed & shas
    recent = sorted(passed or (), key=lambda c: newest[c], reverse=True)[:MAX_CANDIDATES]
    for other in recent:
        if api(f"repos/{repo}/git/commits/{other}")["tree"]["sha"] == tree:
            return other, {name: urls[(name, other)] for name in WORKFLOW_FILES}
    return None


def base_green(repo, base):
    """Whether BASE (what a push is compared with) passed every workflow: (bool, why).
    Skipping by paths trusts that what didn't change was green there."""
    runs = api(f"repos/{repo}/actions/runs?head_sha={base}&event=push&per_page=50")["workflow_runs"]
    latest = {}
    for r in sorted(runs, key=lambda r: r["created_at"]):
        latest[r["name"]] = r
    states = {name: (latest[name]["conclusion"] or latest[name]["status"]) if name in latest else "no run"
              for name in WORKFLOW_FILES}
    if all(state == "success" for state in states.values()):
        return True, f"base {base[:12]} passed CI, Engine E2E and Windows"
    match = identical_green(repo, base, git("rev-parse", f"{base}^{{tree}}"))
    if match:
        return True, f"base {base[:12]}'s tree passed as {match[0][:12]}"
    return False, f"base {base[:12]} isn't green ({', '.join(f'{n}: {s}' for n, s in states.items())})"


def green_ancestor(repo, base):
    """The newest first-parent ancestor of BASE (within MAX_ANCESTORS) that is
    green by base_green: (sha, why), or None."""
    try:
        git("fetch", "--quiet", "--no-tags", f"--deepen={MAX_ANCESTORS}", "origin")
    except subprocess.CalledProcessError:
        pass  # a full clone, or not fetchable: walk what we have
    try:
        ancestors = git("rev-list", "--first-parent", f"--max-count={MAX_ANCESTORS}", f"{base}^").splitlines()
    except subprocess.CalledProcessError:
        return None
    for ancestor in ancestors:
        ok, why = base_green(repo, ancestor)
        if ok:
            return ancestor, why
    return None


def push_base(sha, on_main):
    """What a push is compared with: main's previous head (the event's
    `before`), or a ci/** snapshot's parent (public main). None if unknown."""
    if on_main:
        with open(os.environ["GITHUB_EVENT_PATH"]) as f:
            before = json.load(f).get("before") or ""
        if before and set(before) != {"0"}:
            try:
                git("cat-file", "-e", f"{before}^{{commit}}")
            except subprocess.CalledProcessError:
                git("fetch", "--quiet", "--no-tags", "--depth=1", "origin", before)
            return before
    try:
        return git("rev-parse", f"{sha}^")
    except subprocess.CalledProcessError:
        return None


def changed_paths(event, sha, repo=None, on_main=False):
    """The changed paths, or None for "run everything" with the reason."""
    if event == "push":
        base = push_base(sha, on_main)
        if not base:
            return None, "no parent commit to compare with"
        try:
            ok, why = base_green(repo, base)
            if not ok:
                # A base that isn't green yet (its runs still queued, say):
                # compare with the newest green ancestor instead. The diff
                # then holds the base's own changes too, so every job they
                # need runs as well.
                found = green_ancestor(repo, base)
                if not found:
                    return None, f"{why}; no green ancestor within {MAX_ANCESTORS} commits"
                base, why = found[0], f"{why}; compared with green ancestor {found[0][:12]}: {found[1]}"
        except Exception as e:  # can't tell: run everything rather than trust it
            return None, f"couldn't check that base {base[:12]} is green ({e})"
        return git("diff", "--name-only", "--no-renames", base, sha).splitlines(), f"against {base[:12]} ({why})"
    if event == "pull_request":
        with open(os.environ["GITHUB_EVENT_PATH"]) as f:
            pr = json.load(f)["pull_request"]
        base = git("merge-base", pr["base"]["sha"], pr["head"]["sha"])
        head = pr["head"]["sha"]
        return git("diff", "--name-only", "--no-renames", base, head).splitlines(), f"against merge base {base[:12]}"
    return None, f"a {event} run"


def plan(workflow, paths, on_main):
    """{job: (mode, reason)} for PATHS (None: everything)."""
    out = {}
    tagged = [(p, *tags_of(p)) for p in paths] if paths is not None else None
    for job, (wants, what) in JOB_RULES[workflow].items():
        if tagged is None:
            out[job] = ("full", "runs everything")
            continue
        hits = [(p, t, w) for p, t, w in tagged if t & wants]
        if hits:
            shown = ", ".join(f"`{p}` ({w})" for p, _, w in hits[:3])
            more = f" and {len(hits) - 3} more" if len(hits) > 3 else ""
            out[job] = ("full", f"{shown}{more}")
            continue
        if (workflow, job) in DOCS_MODE and any("docs" in t for _, t, _ in tagged):
            out[job] = ("docs", "only docs, tooling or other platforms changed: fmt, reference docs, scans")
            continue
        why = f"no change needs {what}"
        mode = idle(workflow, job, on_main)
        out[job] = (mode, why + ("; main warms its cache" if mode == "warm" else ""))
    return out


def summary(workflow, header, modes, paths):
    lines = [f"### What runs ({workflow})", "", header, "", "| job | mode | why |", "|---|---|---|"]
    for job, (mode, why) in modes.items():
        lines.append(f"| {job} | **{mode}** | {why} |")
    if paths:
        lines += ["", f"<details><summary>{len(paths)} changed paths</summary>", ""]
        lines += [f"- `{p}`: {', '.join(sorted(tags_of(p)[0]))}" for p in paths[:300]]
        lines += ["", "</details>"]
    return "\n".join(lines) + "\n"


def main():
    args = sys.argv[1:]
    if args == ["--explain"]:
        for pattern, tags, what in PATH_RULES:
            print(f"{pattern:60} {','.join(sorted(tags)):18} {what}")
        for wf, jobs in JOB_RULES.items():
            for job, (wants, what) in jobs.items():
                print(f"{wf}/{job}: {', '.join(sorted(wants))}")
        return 0
    if not args or args[0] not in JOB_RULES:
        sys.exit(__doc__)
    workflow = args[0]
    if len(args) > 1 and args[1] == "--paths":
        modes = plan(workflow, args[2:], on_main=False)
        print(summary(workflow, "offline", modes, args[2:]))
        return 0

    event = os.environ["GITHUB_EVENT_NAME"]
    sha = os.environ["GITHUB_SHA"]
    repo = os.environ["GITHUB_REPOSITORY"]
    on_main = event == "push" and os.environ["GITHUB_REF"] == "refs/heads/main"
    if event == "pull_request":
        with open(os.environ["GITHUB_EVENT_PATH"]) as f:
            sha = json.load(f)["pull_request"]["head"]["sha"]
    paths = None
    match = None
    if event == "push":
        tree = git("rev-parse", f"{sha}^{{tree}}")
        try:
            match = identical_green(repo, sha, tree)
        except Exception as e:  # the API is down: decide by paths, never skip blindly
            print(f"::warning::identical-tree check failed ({e}); deciding by paths")
    if match:
        other, urls = match
        why = f"tree {tree[:12]} already passed as {other[:12]}"
        modes = {}
        for job in JOB_RULES[workflow]:
            mode = idle(workflow, job, on_main)
            modes[job] = (mode, why + ("; main warms its cache" if mode == "warm" else ""))
        header = (f"Tree `{tree[:12]}` is identical to `{other[:12]}`, which passed every workflow: " +
                  ", ".join(f"[{n}]({u})" for n, u in urls.items()) + ".")
    else:
        paths, how = changed_paths(event, sha, repo, on_main)
        modes = plan(workflow, paths, on_main)
        header = (f"{len(paths)} changed paths {how}." if paths is not None else f"Everything runs: {how}.")
    with open(os.environ["GITHUB_OUTPUT"], "a") as f:
        for job, (mode, _) in modes.items():
            f.write(f"{job}={mode}\n")
    text = summary(workflow, header, modes, paths)
    with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as f:
        f.write(text)
    print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
