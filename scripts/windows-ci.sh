#!/bin/bash
# Check the committed Windows lane on GitHub's runners, never on a local PC:
# scripts/ci.sh for the Windows workflow alone, on ci/windows.
#
#   scripts/windows-ci.sh --dry-run   # export HEAD and scan, no GitHub writes
#   scripts/windows-ci.sh             # snapshot, push only ci/windows, wait
#
# Also forwards --merge-main and --allow-workflow-changes to ci.sh.
# Evidence goes under $ISOCAST_CI_DIR/windows (see scripts/ci.sh).
set -euo pipefail
for arg in "$@"; do
    case "$arg" in
        --dry-run|--merge-main|--allow-workflow-changes) ;;
        -h|--help) sed -n 2,9p "$0"; exit 0 ;;
        *) echo "windows-ci: valid options are --dry-run, --merge-main, --allow-workflow-changes" >&2; exit 2 ;;
    esac
done
exec "$(dirname "$0")/ci.sh" --lane windows --workflow Windows "$@"
