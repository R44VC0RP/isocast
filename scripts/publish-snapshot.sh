#!/bin/bash
# Publishes main's tracked files to the public repo as one clean commit.
#
#   scripts/publish-snapshot.sh "message"           # commit locally, don't push
#   scripts/publish-snapshot.sh --push "message"    # commit and push
#   scripts/publish-snapshot.sh --create --push "message"  # first run: make the GitHub repo (private)
#
# The public repo has its own history: this checkout's history is never
# pushed. Each run copies `git archive <ref>` into the public checkout
# (default ~/dev/isocast-public), scans it for secrets and attribution,
# and refuses to commit if anything matches.
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
public="${ISOCAST_PUBLIC_DIR:-$HOME/dev/isocast-public}"
repo="${ISOCAST_PUBLIC_REPO:-R44VC0RP/isocast}"
ref="${ISOCAST_PUBLIC_REF:-main}"
push=0
create=0
while [ $# -gt 0 ]; do
    case "$1" in
        --push) push=1; shift ;;
        --create) create=1; shift ;;
        -h|--help) sed -n 2,13p "$0"; exit 0 ;;
        *) break ;;
    esac
done
message="${1:-Update from $(git -C "$here" rev-parse --short "$ref") ($(date +%Y-%m-%d))}"

# 1. A fresh copy of the tracked files at $ref.
staging="$(mktemp -d "${TMPDIR:-/tmp}/isocast-publish.XXXXXX")"
trap 'rm -rf "${staging:?}"' EXIT
git -C "$here" archive "$ref" | tar -x -C "$staging"

# 2. Refuse secrets and AI attribution.
patterns=(
    '(^|[^a-z-])[a-z]{4}-[a-z]{4}-[a-z]{4}-[a-z]{4}([^a-z-]|$)'  # Apple app-specific passwords (not slugs)
    'AKIA[0-9A-Z]{16}'                           # AWS access keys
    'gh[pousr]_[A-Za-z0-9]{30,}'                 # GitHub tokens
    'github_pat_[A-Za-z0-9_]{30,}'
    'sk-[A-Za-z0-9_-]{30,}'                      # API keys
    'BEGIN [A-Z ]*PRIVATE KEY'
    'rtmps?://[^ "]*/(live|x|app)/[A-Za-z0-9_-]{16,}'  # stream URLs with a key
    'Co-Authored-By:'
    'Generated with \[?Claude'
    '[0-9]{12}, profile `'                       # an AWS account number in notes
)
hits=0
for p in "${patterns[@]}"; do
    # grep exits 0 on a match, 1 on none and 2 on an error: anything but
    # "none" refuses, so a broken scan can never pass for a clean one.
    rc=0
    grep -R -I -n -E --exclude=Cargo.lock --exclude=publish-snapshot.sh \
        -e "$p" "$staging" >"$staging.hit" 2>&1 || rc=$?
    if [ "$rc" = 0 ]; then
        echo "publish: refused, a file matches /$p/:" >&2
        sed -E 's/(.{160}).*/\1…/' "$staging.hit" | head -5 >&2
        hits=1
    elif [ "$rc" != 1 ]; then
        echo "publish: refused, the scan for /$p/ failed:" >&2
        head -3 "$staging.hit" >&2
        hits=1
    fi
done
rm -f "$staging.hit"
[ "$hits" = 0 ] || exit 1

# 3. The public checkout.
if [ ! -d "$public/.git" ]; then
    mkdir -p "$public"
    git -C "$public" init -q -b main
    git -C "$public" remote add origin "https://github.com/$repo.git"
fi
rsync -a --delete --exclude .git "$staging"/ "$public"/
git -C "$public" add -A
if git -C "$public" diff --cached --quiet; then
    echo "publish: nothing changed since the last snapshot"
else
    git -C "$public" -c user.name="$(git -C "$here" config user.name)" \
        -c user.email="$(git -C "$here" config user.email)" commit -q -m "$message"
    echo "publish: committed $(git -C "$public" log --oneline -1)"
fi

# 4. GitHub.
if [ "$create" = 1 ] && ! gh repo view "$repo" >/dev/null 2>&1; then
    gh repo create "$repo" --private --description "OBS-style recorder for macOS: scenes, per-source ISO recording, streaming, and full control from a CLI and MCP." >/dev/null
    echo "publish: created $repo (private)"
fi
if [ "$push" = 1 ]; then
    git -C "$public" push -q -u origin main
    echo "publish: pushed to https://github.com/$repo"
fi
