#!/bin/bash
# Check the committed Windows lane on GitHub's runners, never on a local PC.
#
#   scripts/windows-ci.sh --dry-run   # export HEAD and scan, no GitHub writes
#   scripts/windows-ci.sh             # snapshot, push only ci/windows, wait
#
# Requires git, gh, rsync, tar and python3. Commit this worktree first. The
# public checkout has its own history; private commits never get pushed.
# Results go under $ISOCAST_WINDOWS_CI_DIR (default below), one folder per run.
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
repo=R44VC0RP/isocast
remote="https://github.com/$repo.git"
branch=ci/windows
scratch="${ISOCAST_WINDOWS_CI_DIR:-/Volumes/IsocastBuild/windows/scratch}"
dry_run=0
case "${1:-}" in
    --dry-run) dry_run=1; shift ;;
    -h|--help) sed -n 2,9p "$0"; exit 0 ;;
esac
[ $# = 0 ] || { echo "windows-ci: only --dry-run is accepted" >&2; exit 2; }

source="$(git -C "$here" rev-parse HEAD)"
if [ -n "$(git -C "$here" status --porcelain)" ]; then
    echo "windows-ci: commit this worktree first; only HEAD is exported" >&2
    exit 1
fi
mkdir -p "$scratch"
scratch="$(cd "$scratch" && pwd)"
public="$scratch/public"
# Keep the source and public checkouts disjoint.
case "$here/" in "$public/"*) echo "windows-ci: public checkout overlaps source" >&2; exit 1 ;; esac
case "$public/" in "$here/"*) echo "windows-ci: public checkout overlaps source" >&2; exit 1 ;; esac
if ! mkdir "$scratch/.ci-lock" 2>/dev/null; then
    echo "windows-ci: another invocation holds $scratch/.ci-lock" >&2
    exit 1
fi
staging=
trap '[ -z "$staging" ] || rm -rf "${staging:?}"; rmdir "$scratch/.ci-lock"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
staging="$(mktemp -d "$scratch/export.XXXXXX")"
git -C "$here" archive "$source" | tar -x -C "$staging"
"$here/scripts/publish-snapshot.sh" --scan-only "$staging"
[ -f "$staging/.github/workflows/windows.yml" ] || {
    echo "windows-ci: HEAD has no Windows workflow; commit it first" >&2; exit 1;
}
echo "windows-ci: source $source -> https://github.com/$repo/tree/$branch"
if [ "$dry_run" = 1 ]; then
    echo "windows-ci: dry run passed; no public checkout, commit or push"
    exit 0
fi

# Use our own public checkout, not publish-snapshot.sh's main checkout.
# An existing checkout must be clean, including ignored files: rsync deletes
# obsolete snapshot files, but must not erase somebody's local work.
if [ ! -e "$public" ]; then
    git clone --quiet --no-tags --single-branch --branch main "$remote" "$public"
    git -C "$public" switch --quiet -c "$branch"
fi
[ "$(git -C "$public" rev-parse --show-toplevel)" = "$public" ] &&
    [ "$(git -C "$public" remote get-url origin)" = "$remote" ] &&
    [ "$(git -C "$public" branch --show-current)" = "$branch" ] || {
        echo "windows-ci: public must be this repo's ci/windows checkout" >&2; exit 1;
    }
[ -z "$(git -C "$public" status --porcelain --ignored)" ] || {
    echo "windows-ci: public checkout is dirty; inspect $public before retrying" >&2; exit 1;
}
# An exact lease refuses a concurrent replacement, including a branch created
# while this run was preparing. No other remote ref is fetched or pushed here.
expected="$(git ls-remote --heads "$remote" "refs/heads/$branch" | cut -f1)"
rsync -a --delete --exclude .git "$staging/" "$public/"
git -C "$public" add --all --force
git -C "$public" -c user.name="$(git -C "$here" config user.name)" \
    -c user.email="$(git -C "$here" config user.email)" commit --quiet --allow-empty \
    -m "Windows CI: source ${source:0:12} (#105)"
snapshot="$(git -C "$public" rev-parse HEAD)"
git -C "$public" -c push.followTags=false push --porcelain --no-follow-tags \
    "--force-with-lease=refs/heads/$branch:$expected" "$remote" "HEAD:refs/heads/$branch"

result="$scratch/runs/$snapshot"
mkdir -p "$result"
printf '%s\n' "$source" >"$result/source.txt"
echo "windows-ci: pushed $snapshot; waiting up to 5 minutes for the workflow"
run=
deadline=$((SECONDS + 300))
while [ "$SECONDS" -lt "$deadline" ]; do
    # Do not resolve -w by name: a new workflow needn't exist on main yet.
    run="$(gh run list --repo "$repo" --branch "$branch" --commit "$snapshot" \
        --event push --limit 100 --json databaseId,workflowName \
        --jq '.[] | select(.workflowName == "Windows") | .databaseId' | head -1)"
    [ -z "$run" ] || break
    sleep 10
done
[ -n "$run" ] || {
    echo "windows-ci: no Windows run appeared for $snapshot; inspect https://github.com/$repo/actions" >&2
    exit 1
}
echo "windows-ci: https://github.com/$repo/actions/runs/$run"
printf '%s\n' "$run" >"$result/run-id.txt"
deadline=$((SECONDS + 2700))
while [ "$SECONDS" -lt "$deadline" ]; do
    gh run view "$run" --repo "$repo" \
        --json status,conclusion,url,headSha,headBranch,jobs >"$result/run.json"
    state="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["status"])' "$result/run.json")"
    if [ "$state" = completed ]; then
        gh run view "$run" --repo "$repo" >"$result/summary.txt"
        cat "$result/summary.txt"
        gh run view "$run" --repo "$repo" --log-failed >"$result/failed.log"
        cat "$result/failed.log"
        echo "windows-ci: evidence in $result"
        python3 - "$result/run.json" "$snapshot" "$branch" <<'PY'
import json
import sys

with open(sys.argv[1]) as file:
    run = json.load(file)
if run["headSha"] != sys.argv[2] or run["headBranch"] != sys.argv[3]:
    sys.exit("windows-ci: result does not match the pushed snapshot")
sys.exit(0 if run["conclusion"] == "success" else 1)
PY
        exit 0
    fi
    sleep 20
done
echo "windows-ci: timed out; inspect https://github.com/$repo/actions/runs/$run (not cancelled)" >&2
exit 1
