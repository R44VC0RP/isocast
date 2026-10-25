#!/bin/sh
# One screen of open pull requests: CI, review, hardware, age and the lane's session.
#   scripts/prs.sh [--json]
# Details: scripts/pr_flow.py.
exec python3 "$(dirname "$0")/pr_flow.py" prs "$@"
