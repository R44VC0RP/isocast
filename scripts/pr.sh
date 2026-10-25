#!/bin/sh
# Open or update this lane's pull request: publish the branch merged with main to
# ci/<lane> (one scanned snapshot) and make the PR from its REPORT. Doesn't wait for CI.
#   scripts/pr.sh [--draft | --ready] [--report FILE] [--hw H] [--area A] [--session ID]
# Details: scripts/pr_flow.py.
exec python3 "$(dirname "$0")/pr_flow.py" pr "$@"
