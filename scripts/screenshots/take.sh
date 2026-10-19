#!/bin/sh
# Retakes the README screenshots (docs/images/{app,studio,multiview,
# settings,vertical,recordings,cli}.png) on a test Mac, from the checkout this runs in.
#
#   export ISOCAST_TESTMAC=<test Mac>         # an SSH host, as for testmac.sh
#   scripts/testmac.sh build                  # build and sign it there first
#   scripts/screenshots/take.sh [--keep]      # then this
#   SHOTS="app studio settings" scripts/screenshots/take.sh   # only these
#
# It copies this folder to the test Mac, and in its desktop session
# (testmac.sh gui) switches the display to its largest 2x mode, adds a
# virtual 2x display for the 1440x900 app window (virtual-display.m),
# starts an isolated engine with setup.toml (a Terminal window running
# cli.command, two generated pictures, text, a tone as the mic), opens the app, takes each window with cua-driver, and
# puts everything back (--keep leaves it running to look at). The PNGs are
# trimmed to the window here (Pillow, through uv) and written to
# docs/images/.
set -eu

here="$(cd "$(dirname "$0")/../.." && pwd)"
host="${ISOCAST_TESTMAC:?set ISOCAST_TESTMAC to the SSH host of a test Mac, as for scripts/testmac.sh}"
export ISOCAST_TESTMAC="$host"
shots="$here/scripts/screenshots"
keep=0
[ "${1:-}" = "--keep" ] && keep=1
# Which images to write to docs/images (all of them by default).
only="${SHOTS:-app studio multiview settings vertical recordings cli}"

ssh "$host" 'mkdir -p "$HOME/isocast-scratch/docs-shots" && rm -rf "$HOME/isocast-scratch/docs-shots/out"'
scp -q "$shots/remote.sh" "$shots/shoot.sh" "$shots/setup.toml" "$shots/cli.command" \
    "$shots/display-mode.swift" "$shots/cursor.swift" "$shots/virtual-display.m" \
    "$shots/landscape.jpg" "$shots/brb.jpg" \
    "$host:isocast-scratch/docs-shots/"

if [ "$keep" = 1 ]; then
    steps='sh "$ISOCAST_SCRATCH/docs-shots/remote.sh" start && sh "$ISOCAST_SCRATCH/docs-shots/remote.sh" shoot'
else
    steps='sh "$ISOCAST_SCRATCH/docs-shots/remote.sh" all'
fi
# SHOTS_PREBUILT=1 uses the binaries already pushed (scripts/testmac.sh push)
# instead of building the app on the test Mac.
[ -n "${SHOTS_PREBUILT:-}" ] && steps="export SHOTS_PREBUILT=1; $steps"
"$here/scripts/testmac.sh" gui "$steps"

work="$(mktemp -d "${TMPDIR:-/tmp}/isocast-shots.XXXXXX")"
trap 'rm -rf "${work:?}"' EXIT
scp -q "$host:isocast-scratch/docs-shots/out/*.png" "$work/"

# Window captures come with transparent margins (and sometimes the pointer
# in them): keep the rows and columns that are mostly opaque.
# Then quantize to a 256-colour palette with dithering (libimagequant, as
# pngquant does): about a third of the size, no visible banding.
(cd "$work" && uv run --quiet --no-project --with pillow --with imagequant python3 - "$work" "$here/docs/images" $only) <<'PY'
import sys
from pathlib import Path
import imagequant
from PIL import Image

src, dst = Path(sys.argv[1]), Path(sys.argv[2])
dst.mkdir(parents=True, exist_ok=True)
for name in sys.argv[3:]:
    path = src / f"{name}.png"
    if not path.exists():
        print(f"missing: {name}.png")
        continue
    im = Image.open(path).convert("RGBA")
    alpha = im.getchannel("A")
    w, h = im.size
    data = alpha.load()
    rows = [y for y in range(h) if sum(1 for x in range(0, w, 4) if data[x, y] > 200) > w / 4 * 0.3]
    cols = [x for x in range(w) if sum(1 for y in range(0, h, 4) if data[x, y] > 200) > h / 4 * 0.3]
    box = (cols[0], rows[0], cols[-1] + 1, rows[-1] + 1)
    out = im.crop(box)
    out = imagequant.quantize_pil_image(out, dithering_level=1.0, max_colors=256, min_quality=80, max_quality=98)
    out.save(dst / f"{name}.png", optimize=True)
    size = (dst / f"{name}.png").stat().st_size
    print(f"{name}.png {out.size[0]}x{out.size[1]} {size / 1e6:.2f} MB")
PY
