#!/usr/bin/env python3
"""Compare two sets of offscreen screenshots (scripts/visual-shots.sh) and
write what changed, for the visual check on pull requests:

    scripts/visual_diff.py BASE_DIR HEAD_DIR OUT_DIR [--pr N] [--head-sha SHA]
        [--base-sha SHA] [--tolerance 8] [--threshold 0.01]

OUT_DIR gets base/, head/ and diff/ (full size), small/{base,head,diff}/
(at most 1024 px wide, for the comment's thumbnails and the vision check)
and manifest.json. A pixel counts as changed when any channel differs by
more than --tolerance (0-255), which absorbs anti-aliasing noise; a view is
"changed" when more than --threshold percent of its pixels changed. The
diff image is the head picture dimmed, with changed pixels in magenta.

Python standard library only; PNGs are decoded through macOS `sips` (to
uncompressed BMP), so this runs on a Mac.
"""

import argparse
import json
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import zlib

SMALL_WIDTH = 1024
MAGENTA = b"\xff\x00\xff"
# head picture at a third of its brightness, so the magenta stands out
DIM = bytes(v // 3 for v in range(256))


def decode(path, scratch):
    """RGBA bytes, width, height of a PNG, through sips -> BMP."""
    bmp = os.path.join(scratch, "decode.bmp")
    subprocess.run(
        ["sips", "-s", "format", "bmp", path, "--out", bmp],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    with open(bmp, "rb") as f:
        data = f.read()
    if data[:2] != b"BM":
        raise ValueError(f"{path}: sips did not write a BMP")
    offset = struct.unpack_from("<I", data, 10)[0]
    header = struct.unpack_from("<I", data, 14)[0]
    width, height = struct.unpack_from("<ii", data, 18)
    bpp = struct.unpack_from("<H", data, 28)[0]
    compression = struct.unpack_from("<I", data, 30)[0]
    if bpp not in (24, 32) or compression not in (0, 3):
        raise ValueError(f"{path}: unexpected BMP ({bpp} bpp, compression {compression})")
    top_down = height < 0
    height = abs(height)
    if bpp == 32 and compression == 3 and header >= 52:
        masks = struct.unpack_from("<IIII", data, 54)
    else:
        masks = (0x00FF0000, 0x0000FF00, 0x000000FF, 0xFF000000 if bpp == 32 else 0)
    shifts = [(m & -m).bit_length() - 1 if m else -1 for m in masks]
    if any(s % 8 for s in shifts if s >= 0):
        raise ValueError(f"{path}: unexpected BMP channel masks")
    step = bpp // 8
    stride = (width * step + 3) & ~3
    rows = []
    for y in range(height):
        src = y if top_down else height - 1 - y
        row = data[offset + src * stride : offset + src * stride + width * step]
        out = bytearray(width * 4)
        for channel, shift in enumerate(shifts):
            if shift < 0:
                out[channel::4] = b"\xff" * width
            else:
                out[channel::4] = row[shift // 8 :: step]
        rows.append(bytes(out))
    return rows, width, height


def write_png(path, rows_rgb, width, height):
    raw = b"".join(b"\x00" + r for r in rows_rgb)

    def chunk(kind, body):
        c = kind + body
        return struct.pack(">I", len(body)) + c + struct.pack(">I", zlib.crc32(c))

    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n")
        f.write(chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)))
        f.write(chunk(b"IDAT", zlib.compress(raw, 6)))
        f.write(chunk(b"IEND", b""))


def rgb(row):
    out = bytearray(len(row) // 4 * 3)
    out[0::3] = row[0::4]
    out[1::3] = row[1::4]
    out[2::3] = row[2::4]
    return out


def compare(base, head, tolerance):
    """Changed-pixel count, max channel delta, bounding box and diff rows."""
    (brows, w, h), (hrows, _, _) = base, head
    changed = 0
    max_delta = 0
    box = None
    diff_rows = []
    for y in range(h):
        a, b = brows[y], hrows[y]
        out = rgb(b).translate(DIM)
        if a != b:
            for x in range(w):
                i = x * 4
                if a[i : i + 4] == b[i : i + 4]:
                    continue
                d = max(abs(a[i + c] - b[i + c]) for c in range(4))
                if d > max_delta:
                    max_delta = d
                if d > tolerance:
                    changed += 1
                    out[x * 3 : x * 3 + 3] = MAGENTA
                    if box is None:
                        box = [x, y, x, y]
                    else:
                        box = [min(box[0], x), min(box[1], y), max(box[2], x), max(box[3], y)]
        diff_rows.append(bytes(out))
    if box:
        box = {"x": box[0], "y": box[1], "width": box[2] - box[0] + 1, "height": box[3] - box[1] + 1}
    return changed, max_delta, box, diff_rows


def sanity(rows, w, h):
    """None if the picture looks drawn, else why it doesn't."""
    colours = set()
    for y in range(0, h, max(1, h // 60)):
        row = rows[y]
        for x in range(0, w, max(1, w // 80)):
            colours.add(row[x * 4 : x * 4 + 3])
    if len(colours) < 8:
        return f"only {len(colours)} distinct colours in a sample (blank or broken render)"
    return None


def small(src, dst):
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    subprocess.run(
        ["sips", "-Z", str(SMALL_WIDTH), src, "--out", dst],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("base")
    p.add_argument("head")
    p.add_argument("out")
    p.add_argument("--pr", type=int)
    p.add_argument("--head-sha", default="")
    p.add_argument("--base-sha", default="")
    p.add_argument("--tolerance", type=int, default=8)
    p.add_argument("--threshold", type=float, default=0.01, help="percent")
    p.add_argument("--gpu", action="append", default=[], help="a gpu.txt from visual-shots.sh")
    args = p.parse_args()

    names = sorted(
        {f for d in (args.base, args.head) if os.path.isdir(d) for f in os.listdir(d) if f.endswith(".png")}
    )
    for sub in ("base", "head", "diff"):
        os.makedirs(os.path.join(args.out, sub), exist_ok=True)
    views = []
    broken = []
    with tempfile.TemporaryDirectory() as scratch:
        for name in names:
            view, _, size = name[:-4].rpartition("-")
            entry = {"name": view, "size": size, "scale": 2, "file": name}
            bpath, hpath = os.path.join(args.base, name), os.path.join(args.head, name)
            have_b, have_h = os.path.exists(bpath), os.path.exists(hpath)
            for side, have, path in (("base", have_b, bpath), ("head", have_h, hpath)):
                if have:
                    shutil.copyfile(path, os.path.join(args.out, side, name))
                    small(path, os.path.join(args.out, "small", side, name))
                    entry[side] = f"{side}/{name}"
            if not (have_b and have_h):
                entry["status"] = "added" if have_h else "removed"
                views.append(entry)
                continue
            b = decode(bpath, scratch)
            hd = decode(hpath, scratch)
            for side, img in (("head", hd), ("base", b)):
                why = sanity(*img)
                if why:
                    broken.append(f"{side}/{name}: {why}")
            total = hd[1] * hd[2]
            if b[1:] != hd[1:]:
                entry.update(status="resized", changed_pixels=total, total_pixels=total, changed_percent=100.0)
                views.append(entry)
                continue
            changed, max_delta, box, diff_rows = compare(b, hd, args.tolerance)
            percent = round(100.0 * changed / total, 4)
            entry.update(
                status="changed" if percent > args.threshold else "unchanged",
                changed_pixels=changed,
                total_pixels=total,
                changed_percent=percent,
                max_delta=max_delta,
                changed_box=box,
            )
            if changed:
                dpath = os.path.join(args.out, "diff", name)
                write_png(dpath, diff_rows, hd[1], hd[2])
                small(dpath, os.path.join(args.out, "small", "diff", name))
                entry["diff"] = f"diff/{name}"
            views.append(entry)
            print(f"{name}: {entry['status']} {percent}% ({changed} px, max delta {max_delta})")

    order = {"changed": 0, "resized": 0, "added": 1, "removed": 1, "unchanged": 2}
    views.sort(key=lambda v: (order[v["status"]], v["size"] != "1440x900", v["name"]))
    manifest = {
        "version": 1,
        "pr": args.pr,
        "head_sha": args.head_sha,
        "base_sha": args.base_sha,
        "tolerance": args.tolerance,
        "threshold_percent": args.threshold,
        "views": views,
        "broken": broken,
        "gpu": sorted({line.strip()[:200] for g in args.gpu if os.path.exists(g) for line in open(g) if line.strip()}),
    }
    with open(os.path.join(args.out, "manifest.json"), "w") as f:
        json.dump(manifest, f, indent=2)
    if broken:
        print("visual_diff: these pictures look broken:", *broken, sep="\n  ", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
