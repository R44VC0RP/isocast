#!/bin/sh
# Land pull requests into main and publish (QA only). Several are tested as one candidate.
#   scripts/merge-pr.sh [--dry-run] [--hw N=EVIDENCE]… N [N…]
# Details: scripts/pr_flow.py.
exec python3 "$(dirname "$0")/pr_flow.py" merge "$@"
