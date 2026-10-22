#!/bin/bash
# Run the hosted workflows (CI, Engine E2E, Windows) on a candidate before
# it merges: publish a snapshot of it to ci/<lane> on the public repo and
# wait for every workflow to finish on that exact snapshot.
#
#   scripts/ci.sh [--dry-run] [--worktree] [--merge-main] [--lane NAME]
#                 [--workflow NAME]… [--allow-workflow-changes]
#
#   --dry-run    build, check and scan the snapshot only (no commit or
#                push); it stays in $ISOCAST_CI_DIR/<lane>/dry-run
#   --worktree   publish this worktree as it is (tracked files with their
#                uncommitted edits, plus untracked files git doesn't
#                ignore) instead of the committed HEAD
#   --merge-main publish the candidate merged with this repo's main (a
#                tree-only merge; nothing changes here). Without it, a
#                candidate that doesn't contain main is refused.
#   --lane NAME  publish to ci/NAME (default: this branch's name after its
#                last "/", e.g. agent/ci-green -> ci/ci-green)
#   --workflow NAME
#                wait for these workflows only (repeatable; default "CI",
#                "Engine E2E" and "Windows")
#   --allow-workflow-changes
#                publish even though the snapshot's .github/ differs from
#                public main's (otherwise refused: a candidate's workflows
#                run with the public repo's token and settings)
#
# The snapshot is `git archive` of the candidate's tree, so .gitattributes
# export-ignore applies (also with --worktree, through a temporary index),
# and it fails loudly if an export-ignored path is in it anyway. It must
# pass publish-snapshot.sh's secret and attribution scan. It is committed as
# ONE commit whose parent is public main and pushed only to
# refs/heads/ci/<lane> with an exact lease, never main or tags, so a
# replaced snapshot is unreachable from any branch. Private history is never
# pushed. Exit 0 only when every workflow succeeded on the pushed snapshot.
# Evidence (run ids, summaries, failed logs) goes under
# $ISOCAST_CI_DIR/<lane>/runs/<snapshot>. A lane's newer snapshot cancels
# its older runs; lanes don't cancel each other. Needs git 2.40+, gh, tar
# and python3.
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
repo=R44VC0RP/isocast
remote="https://github.com/$repo.git"
dry_run=0
worktree=0
merge_main=0
allow_workflows=0
lane=
workflows=()
while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) dry_run=1; shift ;;
        --worktree) worktree=1; shift ;;
        --merge-main) merge_main=1; shift ;;
        --allow-workflow-changes) allow_workflows=1; shift ;;
        --lane) [ $# -ge 2 ] || { echo "ci: --lane needs a name" >&2; exit 2; }; lane="$2"; shift 2 ;;
        --workflow) [ $# -ge 2 ] || { echo "ci: --workflow needs a name" >&2; exit 2; }; workflows+=("$2"); shift 2 ;;
        -h|--help) sed -n 2,37p "$0"; exit 0 ;;
        *) echo "ci: unknown option $1 (valid: --dry-run, --worktree, --merge-main, --lane NAME," \
                "--workflow NAME, --allow-workflow-changes)" >&2; exit 2 ;;
    esac
done
[ ${#workflows[@]} -gt 0 ] || workflows=("CI" "Engine E2E" "Windows")
if [ -z "$lane" ]; then
    lane="$(git -C "$here" branch --show-current)"
    lane="${lane##*/}"
fi
case "$lane" in
    ''|main|[.-]*|*[.-]|*.lock|*..*|*[!A-Za-z0-9._-]*)
        echo "ci: --lane must be letters, digits, '.', '_' or '-', not starting or ending with '.' or '-'," \
             "not ending in .lock, and not main: '$lane'" >&2; exit 2 ;;
esac
branch="ci/$lane"
scratch="${ISOCAST_CI_DIR:-/Volumes/IsocastBuild/ci}/$lane"
g() { git -C "$here" "$@"; }

source="$(g rev-parse HEAD)"
if [ "$worktree" = 0 ] && [ -n "$(g status --porcelain)" ]; then
    echo "ci: the worktree has changes; commit them, or pass --worktree to publish them as they are" >&2
    exit 1
fi
main_ref="$(g rev-parse --verify --quiet main || true)"
if [ -n "$main_ref" ] && [ "$merge_main" = 0 ] && ! g merge-base --is-ancestor "$main_ref" HEAD; then
    echo "ci: HEAD doesn't contain main (${main_ref:0:12}); merge main first, or pass --merge-main" >&2
    exit 1
fi

mkdir -p "$scratch"
scratch="$(cd "$scratch" && pwd)"
lock="$scratch/.ci-lock"
if ! mkdir "$lock" 2>/dev/null; then
    holder="$(cat "$lock/pid" 2>/dev/null || true)"
    if [ -n "$holder" ] && kill -0 "$holder" 2>/dev/null; then
        echo "ci: pid $holder holds $lock (lane $lane is publishing)" >&2
        exit 1
    fi
    echo "ci: taking over a stale lock (pid ${holder:-unknown} is gone)" >&2
    rm -f "$lock/pid"
    rmdir "$lock"
    mkdir "$lock"
fi
echo $$ >"$lock/pid"
staging=
tmp="$(mktemp -d "$scratch/tmp.XXXXXX")"
cleanup() {
    [ -z "$staging" ] || rm -rf "${staging:?}"
    rm -rf "${tmp:?}"
    rm -f "$lock/pid"
    rmdir "$lock" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# 1. The candidate's tree: HEAD's, or the worktree's through a temporary
#    index (this checkout's own index is untouched).
if [ "$worktree" = 1 ]; then
    export_tree="$(cd "$here" && GIT_INDEX_FILE="$tmp/index" git read-tree HEAD &&
        GIT_INDEX_FILE="$tmp/index" git add -A && GIT_INDEX_FILE="$tmp/index" git write-tree)"
    label="worktree of ${source:0:12}"
    [ "$export_tree" = "$(g rev-parse 'HEAD^{tree}')" ] || label="$label + uncommitted"
    candidate="$(g commit-tree "$export_tree" -p HEAD -m "ci.sh worktree snapshot")"
else
    export_tree="$(g rev-parse 'HEAD^{tree}')"
    label="source ${source:0:12}"
    candidate="$source"
fi
if [ "$merge_main" = 1 ] && [ -n "$main_ref" ] && ! g merge-base --is-ancestor "$main_ref" "$candidate"; then
    export_tree="$(g merge-tree --write-tree "$candidate" "$main_ref")" || {
        echo "ci: the candidate conflicts with main; merge it by hand" >&2; exit 1;
    }
    export_tree="${export_tree%%$'\n'*}"
    label="$label merged with main ${main_ref:0:12}"
fi

# 2. Export it: export-ignore applies (attributes come from the tree).
staging="$tmp/tree"
mkdir "$staging"
g archive "$export_tree" | tar -x -C "$staging"
ignored="$tmp/export-ignored"
g ls-tree -r -z --name-only "$export_tree" |
    g check-attr --source="$export_tree" --stdin -z export-ignore |
    python3 -c '
import sys
f = sys.stdin.buffer.read().split(b"\0")
for path, _, value in zip(f[0::3], f[1::3], f[2::3]):
    if value == b"set":
        sys.stdout.buffer.write(path + b"\n")' >"$ignored"
leaked=0
while IFS= read -r path; do
    if [ -e "$staging/$path" ] || [ -L "$staging/$path" ]; then
        echo "ci: refused, export-ignored $path is in the snapshot" >&2
        leaked=1
    fi
done <"$ignored"
[ "$leaked" = 0 ] || exit 1
echo "ci: $(wc -l <"$ignored" | tr -d ' ') export-ignored file(s) left out"
"$here/scripts/publish-snapshot.sh" --scan-only "$staging"
for w in "${workflows[@]}"; do
    grep -lqx "name: $w" "$staging"/.github/workflows/*.yml 2>/dev/null || {
        echo "ci: the snapshot has no workflow named '$w'" >&2; exit 1;
    }
done

# 3. The snapshot as a tree of the public repo (a bare repo per lane that
#    holds only public main and our own objects).
public="$scratch/public.git"
[ -d "$public" ] || git init --quiet --bare "$public"
pg() { git --git-dir="$public" "$@"; }
pg fetch --quiet --no-tags "$remote" "+refs/heads/main:refs/remotes/origin/main"
public_main="$(pg rev-parse refs/remotes/origin/main)"
tree="$(GIT_INDEX_FILE="$tmp/public-index" GIT_WORK_TREE="$staging" pg add -A --force . &&
    GIT_INDEX_FILE="$tmp/public-index" GIT_WORK_TREE="$staging" pg write-tree)"
rm -rf "${staging:?}"
staging=
if [ "$(pg rev-parse "$tree:.github" 2>/dev/null)" != "$(pg rev-parse "$public_main:.github" 2>/dev/null)" ]; then
    pg diff --stat "$public_main" "$tree" -- .github >&2
    if [ "$allow_workflows" = 1 ]; then
        echo "ci: .github/ differs from public main (allowed by --allow-workflow-changes)" >&2
    else
        echo "ci: refused, the snapshot's .github/ differs from public main ${public_main:0:12};" \
             "pass --allow-workflow-changes if that is intended" >&2
        exit 1
    fi
fi
echo "ci: $label -> tree $tree on public main ${public_main:0:12} -> https://github.com/$repo/tree/$branch"
if [ "$dry_run" = 1 ]; then
    rm -rf "${scratch:?}/dry-run"
    mkdir "$scratch/dry-run"
    pg archive "$tree" | tar -x -C "$scratch/dry-run"
    echo "ci: dry run passed; the snapshot is in $scratch/dry-run (no commit or push)"
    exit 0
fi

# 4. One commit on top of public main, pushed with an exact lease (a
#    concurrent replacement, or a branch created meanwhile, is refused).
case "$branch" in ci/?*) ;; *) echo "ci: refusing to push $branch" >&2; exit 1 ;; esac
expected="$(git ls-remote --heads "$remote" "refs/heads/$branch" |
    awk -v ref="refs/heads/$branch" '$2 == ref { print $1 }')"
snapshot="$(GIT_AUTHOR_NAME="$(g config user.name)" GIT_AUTHOR_EMAIL="$(g config user.email)" \
    GIT_COMMITTER_NAME="$(g config user.name)" GIT_COMMITTER_EMAIL="$(g config user.email)" \
    pg commit-tree "$tree" -p "$public_main" -m "CI candidate ($lane): $label")"
pg -c push.followTags=false push --porcelain --no-follow-tags \
    "--force-with-lease=refs/heads/$branch:$expected" "$remote" "$snapshot:refs/heads/$branch"
pushed_at=$SECONDS

result="$scratch/runs/$snapshot"
mkdir -p "$result"
printf '%s\n' "$label" >"$result/source.txt"
echo "ci: pushed $snapshot; waiting up to 5 minutes for ${workflows[*]}"
declare -a runs=()
deadline=$((SECONDS + 300))
for w in "${workflows[@]}"; do
    run=
    while [ "$SECONDS" -lt "$deadline" ]; do
        # Not -w by name: a workflow needn't exist on main yet.
        run="$(gh run list --repo "$repo" --branch "$branch" --commit "$snapshot" \
            --event push --limit 100 --json databaseId,workflowName \
            --jq ".[] | select(.workflowName == \"$w\") | .databaseId" | head -1)"
        [ -z "$run" ] || break
        sleep 10
    done
    [ -n "$run" ] || {
        echo "ci: no '$w' run appeared for $snapshot; does the workflow trigger on ci/** pushes?" >&2
        exit 1
    }
    echo "ci: $w: https://github.com/$repo/actions/runs/$run"
    runs+=("$run")
done
printf '%s\n' "${runs[@]}" >"$result/run-ids.txt"

# Each run gets 120 minutes from when it started (queueing behind other
# lanes doesn't count); the whole wait ends 150 minutes after the push, so
# a run that never starts times out too.
pending=("${runs[@]}")
while [ ${#pending[@]} -gt 0 ]; do
    still=()
    for run in "${pending[@]}"; do
        gh run view "$run" --repo "$repo" \
            --json status,conclusion,url,headSha,headBranch,workflowName,startedAt,jobs >"$result/$run.json"
        read -r state conclusion running_s < <(python3 -c '
import datetime, json, sys
r = json.load(open(sys.argv[1]))
started = r.get("startedAt") or ""
age = -1
if started and not started.startswith("0001"):
    t = datetime.datetime.fromisoformat(started.replace("Z", "+00:00"))
    age = int((datetime.datetime.now(datetime.timezone.utc) - t).total_seconds())
print(r["status"], r["conclusion"] or "-", age)' "$result/$run.json")
        if [ "$state" = completed ]; then
            gh run view "$run" --repo "$repo" >"$result/$run-summary.txt"
            echo "ci: run $run finished: $conclusion"
            [ "$conclusion" = success ] || gh run view "$run" --repo "$repo" --log-failed >"$result/$run-failed.log" || true
        elif [ "$running_s" -gt 7200 ] || [ $((SECONDS - pushed_at)) -gt 9000 ]; then
            echo "ci: run $run timed out (running ${running_s}s, ${state}); not cancelled:" \
                 "https://github.com/$repo/actions/runs/$run" >&2
            exit 1
        else
            still+=("$run")
        fi
    done
    pending=("${still[@]+"${still[@]}"}")
    [ ${#pending[@]} -eq 0 ] || sleep 30
done
echo "ci: evidence in $result"
python3 - "$result" "$snapshot" "$branch" "${runs[@]}" <<'PY'
import json
import sys

result, snapshot, branch, *runs = sys.argv[1:]
failed = []
for run in runs:
    with open(f"{result}/{run}.json") as file:
        r = json.load(file)
    if r["headSha"] != snapshot or r["headBranch"] != branch:
        sys.exit(f"ci: run {run} does not match the pushed snapshot")
    print(f"  {r['workflowName']:<12} {r['conclusion']:<10} {r['url']}")
    if r["conclusion"] != "success":
        failed.append(f"{r['workflowName']} ({r['conclusion']}): {result}/{run}-failed.log")
if failed:
    sys.exit("ci: failed: " + "; ".join(failed))
print("ci: every workflow passed on", snapshot)
PY
