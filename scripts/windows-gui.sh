#!/usr/bin/env bash
# GUI calls run in the logged-on Windows session, never in the SSH desktop.
set -euo pipefail
exec python3 "$(cd "$(dirname "$0")" && pwd)/windows/gui.py" "$@"
