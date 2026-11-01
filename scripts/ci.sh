#!/bin/bash
# Publish a scanned snapshot and wait for its hosted checks. Lane pushes run
# Windows; train candidates and explicit --full snapshots also run CI and Engine E2E.
#
#   scripts/ci.sh [--dry-run] [--worktree | --source REF] [--include REF]…
#                 [--merge-main] [--lane NAME] [--workflow NAME]…
#                 [--full] [--allow-workflow-changes] [--no-wait]
#   scripts/ci.sh --wait SNAPSHOT [--full] [--lane NAME] [--workflow NAME]…
#
#   --dry-run    build, check and scan the snapshot only (no commit or
#                push); it stays in $ISOCAST_CI_DIR/<lane>/dry-run
#   --worktree   publish this worktree as it is (tracked files with their
#                uncommitted edits, plus untracked files git doesn't
#                ignore) instead of the committed HEAD
#   --source REF publish this commit (a branch of any worktree: refs are
#                shared) instead of this checkout's HEAD; this worktree's
#                state doesn't matter
#   --include REF
#                also merge these commits into the candidate (repeatable;
#                a tree-only merge, in order, before main): one candidate
#                for several branches (scripts/merge-pr.sh's batches)
#   --merge-main publish the candidate merged with this repo's main (a
#                tree-only merge; nothing changes here). Without it, a
#                candidate that doesn't contain main is refused.
#   --lane NAME  publish to ci/NAME (default: this branch's name after its
#                last "/", e.g. agent/ci-green -> ci/ci-green)
#   --full       opt into early macOS checks on ci/full/NAME, without
#                replacing a PR head. Use --full again to --wait on that ref.
#   --workflow NAME
#                wait for these workflows only (repeatable; default Windows,
#                plus CI and Engine E2E for --full and batch-* lanes)
#   --allow-workflow-changes
#                publish even though the snapshot's .github/ differs from
#                this repo's main (otherwise refused: a candidate's
#                workflows run with the public repo's token and settings)
#   --no-wait    push the snapshot and exit; CI runs from the push
#   --wait SNAPSHOT
#                publish nothing: wait for the workflows on SNAPSHOT, a
#                commit already on ci/<lane>, and judge them as above
#
# The snapshot is `git archive` of the candidate's tree, so .gitattributes
# export-ignore applies (also with --worktree, through a temporary index),
# and it fails loudly if an export-ignored path is in it anyway. It must
# pass publish-snapshot.sh's secret and attribution scan. It is committed as
# ONE commit whose parent is public main and pushed only to
# refs/heads/ci/<lane> with an exact lease, never main or tags, so a
# replaced snapshot is unreachable from any branch. Private history is never
# pushed. The snapshot's message ends in provenance trailers that
# scripts/pr_flow.py reads: Isocast-Source (the commit published),
# Isocast-Include (each --include), Isocast-Main (the private main it
# contains), Isocast-Tree (the private tree before export-ignore) and, with
# uncommitted edits, Isocast-Dirty. The pushed snapshot's id is written to
# $ISOCAST_CI_DIR/<lane>/pushed. Exit 0 only when every workflow succeeded
# on the pushed snapshot (with --no-wait, once it is pushed).
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
no_wait=0
full=0
lane=
source_ref=
wait_for=
workflows=()
includes=()
while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) dry_run=1; shift ;;
        --worktree) worktree=1; shift ;;
        --merge-main) merge_main=1; shift ;;
        --allow-workflow-changes) allow_workflows=1; shift ;;
        --lane) [ $# -ge 2 ] || { echo "ci: --lane needs a name" >&2; exit 2; }; lane="$2"; shift 2 ;;
        --workflow) [ $# -ge 2 ] || { echo "ci: --workflow needs a name" >&2; exit 2; }; workflows+=("$2"); shift 2 ;;
        --source) [ $# -ge 2 ] || { echo "ci: --source needs a commit" >&2; exit 2; }; source_ref="$2"; shift 2 ;;
        --include) [ $# -ge 2 ] || { echo "ci: --include needs a commit" >&2; exit 2; }; includes+=("$2"); shift 2 ;;
        --no-wait) no_wait=1; shift ;;
        --full) full=1; shift ;;
        --wait) [ $# -ge 2 ] || { echo "ci: --wait needs a snapshot commit" >&2; exit 2; }; wait_for="$2"; shift 2 ;;
        -h|--help) sed -n 2,56p "$0"; exit 0 ;;
        *) echo "ci: unknown option $1 (valid: --dry-run, --worktree, --source REF, --include REF, --merge-main," \
                "--lane NAME, --workflow NAME, --full, --allow-workflow-changes, --no-wait, --wait SNAPSHOT)" >&2; exit 2 ;;
    esac
done
if [ -n "$source_ref" ] && [ "$worktree" = 1 ]; then
    echo "ci: --source and --worktree don't combine (--source publishes a commit)" >&2; exit 2
fi
if [ -n "$wait_for" ]; then
    case "$wait_for" in
        *[!0-9a-f]*|'') echo "ci: --wait needs a full snapshot commit id, not '$wait_for'" >&2; exit 2 ;;
    esac
    [ ${#wait_for} = 40 ] || { echo "ci: --wait needs a full 40-character snapshot id" >&2; exit 2; }
    [ "$dry_run$worktree$merge_main$no_wait" = 0000 ] && [ -z "$source_ref" ] && [ ${#includes[@]} = 0 ] || {
        echo "ci: --wait only combines with --lane, --full and --workflow" >&2; exit 2;
    }
fi
if [ -z "$lane" ]; then
    if [ -n "$source_ref" ]; then
        lane="$source_ref"
    else
        lane="$(git -C "$here" branch --show-current)"
    fi
    lane="${lane##*/}"
fi
case "$lane" in
    ''|main|[.-]*|*[.-]|*.lock|*..*|*[!A-Za-z0-9._-]*)
        echo "ci: --lane must be letters, digits, '.', '_' or '-', not starting or ending with '.' or '-'," \
             "not ending in .lock, and not main: '$lane'" >&2; exit 2 ;;
esac
[ "$full" = 0 ] || lane="full/$lane"
branch="ci/$lane"
case "$lane" in
    batch-*|full/*) plan=full ;;
    *) plan=windows ;;
esac
if [ ${#workflows[@]} -eq 0 ]; then
    if [ "$plan" = full ]; then workflows=("CI" "Engine E2E" "Windows"); else workflows=("Windows"); fi
fi
scratch="${ISOCAST_CI_DIR:-/Volumes/IsocastBuild/ci}/$lane"
g() { git -C "$here" "$@"; }

if [ -n "$source_ref" ]; then
    source="$(g rev-parse --verify --quiet "$source_ref^{commit}")" || {
        echo "ci: --source $source_ref is not a commit here" >&2; exit 1;
    }
elif [ -z "$wait_for" ]; then
    source="$(g rev-parse HEAD)"
    if [ "$worktree" = 0 ] && [ -n "$(g status --porcelain)" ]; then
        echo "ci: the worktree has changes; commit them, or pass --worktree to publish them as they are" >&2
        exit 1
    fi
fi
main_ref="$(g rev-parse --verify --quiet main || true)"
if [ -z "$wait_for" ] && [ -n "$main_ref" ] && [ "$merge_main" = 0 ] && ! g merge-base --is-ancestor "$main_ref" "$source"; then
    echo "ci: ${source_ref:-HEAD} doesn't contain main (${main_ref:0:12}); merge main first, or pass --merge-main" >&2
    exit 1
fi

mkdir -p "$scratch"
scratch="$(cd "$scratch" && pwd)"
# One publisher per lane: an flock(2) on fd 9, held until this script and
# every child that inherited fd 9 exit, so a crash never leaves it stale.
# (macOS has no flock(1); python3's fcntl takes the lock on the shared open
# file, which outlives the python process.)
lock="$scratch/.ci.lock"
exec 9>>"$lock"
locked=0
python3 -c '
import fcntl, sys
try:
    fcntl.flock(9, fcntl.LOCK_EX | fcntl.LOCK_NB)
except BlockingIOError:
    sys.exit(3)' || locked=$?
case "$locked" in
    0) ;;
    3) echo "ci: another ci.sh holds $lock (lane $lane is publishing)" >&2; exit 1 ;;
    *) echo "ci: couldn't lock $lock with python3 (exit $locked)" >&2; exit 1 ;;
esac
staging=
tmp="$(mktemp -d "$scratch/tmp.XXXXXX")"
cleanup() {
    [ -z "$staging" ] || rm -rf "${staging:?}"
    rm -rf "${tmp:?}"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if [ -z "$wait_for" ]; then
# 1. The candidate's tree: the source's, or the worktree's through a
#    temporary index (this checkout's own index is untouched).
dirty=0
if [ "$worktree" = 1 ]; then
    export_tree="$(cd "$here" && GIT_INDEX_FILE="$tmp/index" git read-tree HEAD &&
        GIT_INDEX_FILE="$tmp/index" git add -A && GIT_INDEX_FILE="$tmp/index" git write-tree)"
    label="worktree of ${source:0:12}"
    [ "$export_tree" = "$(g rev-parse 'HEAD^{tree}')" ] || { label="$label + uncommitted"; dirty=1; }
    candidate="$(g commit-tree "$export_tree" -p HEAD -m "ci.sh worktree snapshot")"
else
    export_tree="$(g rev-parse "$source^{tree}")"
    label="source ${source:0:12}"
    candidate="$source"
fi
trailers="Isocast-Source: $source"
# merge_into NAME COMMIT: merge COMMIT into the candidate, tree-only.
merge_into() {
    local merged
    g merge-base --is-ancestor "$2" "$candidate" && return 0
    merged="$(g merge-tree --write-tree "$candidate" "$2")" || {
        echo "ci: the candidate conflicts with $1; merge it by hand" >&2; exit 1;
    }
    export_tree="${merged%%$'\n'*}"
    candidate="$(g commit-tree "$export_tree" -p "$candidate" -p "$2" -m "ci.sh merge of $1")"
}
for ref in ${includes[@]+"${includes[@]}"}; do
    inc="$(g rev-parse --verify --quiet "$ref^{commit}")" || { echo "ci: --include $ref is not a commit here" >&2; exit 1; }
    merge_into "$ref" "$inc"
    label="$label + $ref ${inc:0:12}"
    trailers="$trailers"$'\n'"Isocast-Include: $inc"
done
if [ "$merge_main" = 1 ] && [ -n "$main_ref" ] && ! g merge-base --is-ancestor "$main_ref" "$candidate"; then
    merge_into main "$main_ref"
    label="$label merged with main ${main_ref:0:12}"
fi
trailers="$trailers"$'\n'"Isocast-Main: $main_ref"$'\n'"Isocast-Tree: $export_tree"
trailers="$trailers"$'\n'"Isocast-CI: $plan"
[ "$dirty" = 0 ] || trailers="$trailers"$'\n'"Isocast-Dirty: yes"

# Workflows run with the public repo's token and settings: a candidate may
# change them only on purpose. The baseline is this repo's main (what merges
# and publishes next), not public main, which can lag behind it.
if [ -z "$main_ref" ]; then
    echo "ci: no local main to compare .github against" >&2
    exit 1
fi
if [ "$(g rev-parse "$export_tree:.github" 2>/dev/null)" != "$(g rev-parse "$main_ref:.github" 2>/dev/null)" ]; then
    g diff --stat "$main_ref" "$export_tree" -- .github >&2
    if [ "$allow_workflows" = 1 ]; then
        echo "ci: .github/ differs from main (allowed by --allow-workflow-changes)" >&2
    else
        echo "ci: refused, the candidate's .github/ differs from main ${main_ref:0:12};" \
             "pass --allow-workflow-changes if that is intended" >&2
        exit 1
    fi
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
    pg commit-tree "$tree" -p "$public_main" -m "CI candidate ($lane): $label" -m "$trailers")"
pg -c push.followTags=false push --porcelain --no-follow-tags \
    "--force-with-lease=refs/heads/$branch:$expected" "$remote" "$snapshot:refs/heads/$branch"
printf '%s\n' "$snapshot" >"$scratch/pushed"
if [ "$no_wait" = 1 ]; then
    echo "ci: pushed $snapshot to $branch; not waiting (CI runs from the push)"
    exit 0
fi
else
    snapshot="$wait_for"
    label="snapshot ${snapshot:0:12} (already pushed)"
fi
pushed_at=$SECONDS

result="$scratch/runs/$snapshot"
mkdir -p "$result"
printf '%s\n' "$label" >"$result/source.txt"
verb=pushed
[ -z "$wait_for" ] || verb=found
echo "ci: $verb $snapshot; waiting up to 5 minutes for ${workflows[*]} to appear"
declare -a runs=()
deadline=$((SECONDS + 300))
for w in "${workflows[@]}"; do
    run=
    while [ "$SECONDS" -lt "$deadline" ]; do
        # Not -w by name: a workflow needn't exist on main yet.
        run="$(gh run list --repo "$repo" --branch "$branch" --commit "$snapshot" \
            --event push --limit 100 --json databaseId,workflowName \
            --jq ".[] | select(.workflowName == \"$w\") | .databaseId" | head -1)" || run=''
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

# Each run gets 120 minutes from when its first job started (a run's own
# startedAt is when it was queued, so waiting for a runner behind other
# lanes doesn't count); the whole wait ends 150 minutes after the push, so
# a run that never starts times out too. GitHub's API blips are retried.
gh_retry() {  # gh_retry FILE COMMAND…: FILE only ever holds a whole answer
    local file="$1" attempt
    shift
    for attempt in 1 2 3 4 5; do
        if "$@" >"$file.new"; then
            mv "$file.new" "$file"
            return 0
        fi
        echo "ci: '$*' failed (attempt $attempt of 5); retrying" >&2
        sleep $((attempt * 10))
    done
    rm -f "$file.new"
    return 1
}
pending=("${runs[@]}")
while [ ${#pending[@]} -gt 0 ]; do
    if [ $((SECONDS - pushed_at)) -gt 9600 ]; then
        echo "ci: gave up on ${pending[*]} 160 minutes after the push (not cancelled)" >&2
        exit 1
    fi
    still=()
    for run in "${pending[@]}"; do
        # A network blip (GitHub's API timing out) is retried next round.
        if ! gh run view "$run" --repo "$repo" \
            --json status,conclusion,url,headSha,headBranch,workflowName,startedAt,jobs >"$result/$run.json.new"; then
            echo "ci: couldn't read run $run; retrying" >&2
            still+=("$run")
            continue
        fi
        mv "$result/$run.json.new" "$result/$run.json"
        read -r state conclusion running_s < <(python3 -c '
import datetime, json, sys
r = json.load(open(sys.argv[1]))
# Only jobs still running: a re-run keeps the old start times of its completed jobs.
starts = [j.get("startedAt") or "" for j in r.get("jobs") or [] if j.get("status") != "completed"]
starts = [datetime.datetime.fromisoformat(t.replace("Z", "+00:00")) for t in starts
          if t and not t.startswith("0001")]
age = -1  # no job has started yet
if starts:
    age = int((datetime.datetime.now(datetime.timezone.utc) - min(starts)).total_seconds())
print(r["status"], r["conclusion"] or "-", age)' "$result/$run.json")
        if [ "$state" = completed ]; then
            gh_retry "$result/$run-summary.txt" gh run view "$run" --repo "$repo" ||
                echo "ci: couldn't save run $run's summary (its conclusion is in $run.json)" >&2
            echo "ci: run $run finished: $conclusion"
            # Only a failed run has failed logs (a cancelled or skipped one has none).
            [ "$conclusion" != failure ] ||
                gh_retry "$result/$run-failed.log" gh run view "$run" --repo "$repo" --log-failed || true
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
