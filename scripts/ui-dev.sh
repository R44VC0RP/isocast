#!/bin/sh
# The Slint app as a bundle somewhere else than ~/Applications/Isocast.app
# (test Macs, scratch folders): scripts/app-dev.sh with the bundle at
# $ISOCAST_UI_APP. Since N.3 the Slint app is Isocast.app itself.
#
# Usage: ISOCAST_UI_APP=/path/Isocast.app scripts/ui-dev.sh [--release]
# Run it as `open -g` would, or straight from a terminal (keeping stdio and
# flags): "$ISOCAST_UI_APP/Contents/MacOS/isocast-ui" --bench 30.
set -eu

cd "$(dirname "$0")/.."
ISOCAST_APP="${ISOCAST_UI_APP:-$HOME/Applications/Isocast.app}" ISOCAST_APP_TARGET=isocast-ui \
    exec sh scripts/app-dev.sh "$@"
