#!/usr/bin/env python3
"""How far the picture lags the selection frame while dragging a source.

    scripts/drag-lag.py [--work /Volumes/IsocastBuild/lag] [--seconds 6]

With --app web, runs the retired web app (target/release/isocast-app) in a window on a test engine
of its own (never Ryan's setup or socket), with a copy of the interface
that adds two test-only things: a pure magenta block to drag and a pure
green selection outline, plus a script that drags the block back and forth
with the app's own pointer events (the real mouse is never touched). A
second test engine records that window at 60 fps; the recording is then
read frame by frame:

- lag: how many milliseconds the block (the engine's picture) trails the
  outline (drawn by the app at the pointer);
- gap: how far apart they are while moving, in canvas pixels (median, p95);
- picture rate: how many times a second the block actually moved, against
  the outline's.

By default (--app ui) it runs the Slint app (isocast-ui --drag-test) instead,
which drags the item with its own pointer events and draws its box as the
same green outline; there the outline is drawn at the layout of the frame
shown, so the expected lag is 0.

--mode resize|columns|rows (Slint app only) drags the block's lower right
corner handle along its diagonal, the splitter between the Scenes and
Sources docks, or the splitter above the docks. The app then marks where
its scripted pointer is with a pure cyan bar (window captures leave the
pointer out) and draws the dragged edge pure green, and the recording is
read for how far the edge trails the pointer:

- gap: pointer to edge while moving, in window points (median, p95);
- lag: the shift (ms) that best lines the edge up with the pointer;
- edge and pointer moves a second;
- glitch frames: the edge stepping against the pointer's way, standing
  still while the pointer moves, missing, or showing twice; and frames
  where the picture (the magenta block) is missing;
- for resize, the same for the picture's edge (the block).

With --record the test engine records the program and two ISO test
patterns during the drag, and the report's dropped frames (and ffprobe's
frame counts) are added. The app's own frame times and CPU while dragging
are in "app_run.drag".

The window shows on screen for about 15 s. The recording (synthetic
content only) is kept in --work for watching. Needs the release binaries
(`cargo build --release -p isocast-daemon -p isocast-cli -p isocast-ui`, or
-p isocast-app for --app web)
and numpy (it re-runs itself with /Volumes/IsocastBuild/main/venv if
needed).
"""

import argparse
import contextlib
import fcntl
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
# Scratch space: Ryan's build volume here; elsewhere (the test Mac) set
# ISOCAST_SCRATCH.
SCRATCH = Path(os.environ.get("ISOCAST_SCRATCH", "/Volumes/IsocastBuild"))
VENV = SCRATCH / "main/venv/bin/python"
LOCK = SCRATCH / ".bench.lock"
DEV_APP = Path(os.environ.get("DRAG_LAG_DEV_APP", SCRATCH / "main/verify/Isocast Dev.app"))
CANVAS = (1920, 1080)

try:
    import numpy as np
except ImportError:
    if VENV.exists() and Path(sys.executable) != VENV:
        os.execv(VENV, [str(VENV), *sys.argv])
    sys.exit("drag-lag: needs numpy (python3 -m venv /Volumes/IsocastBuild/main/venv && "
             "/Volumes/IsocastBuild/main/venv/bin/pip install numpy)")

# Prints the time of each setup change the engine announces, until timeout.
WATCH = """
import json, socket, sys, time
sock, out, seconds = sys.argv[1], sys.argv[2], float(sys.argv[3])
s = socket.socket(socket.AF_UNIX); s.connect(sock); s.settimeout(0.5)
s.sendall(b'{"jsonrpc":"2.0","id":1,"method":"watch"}\\n')
buf, end, times = b"", time.time() + seconds, []
while time.time() < end:
    try:
        data = s.recv(65536)
    except socket.timeout:
        continue
    if not data:
        break
    buf += data
    *lines, buf = buf.split(b"\\n")
    for line in lines:
        if b'"setup_changed"' in line:
            times.append(time.time())
open(out, "w").write("\\n".join(f"{t:.4f}" for t in times))
"""

TEST_CSS = """
/* drag-lag test only */
.item-box.selected { box-shadow: 0 0 0 3px #00ff00 !important; }
.handle, .item-box::after, .snap-guide, .guide { display: none !important; }
"""

TEST_JS = """
// drag-lag test only: drag the block back and forth with pointer events,
// and profile the page meanwhile (component renders, frame gaps).
import { options } from "preact";
const SECONDS = %(seconds)s;
const log = (message) => window.webkit?.messageHandlers?.isocast?.postMessage(JSON.stringify({ native: "log", message }));
const renders = new Map();
// Round trips of the app's calls to the daemon (apply in particular).
const calls = [];
const handler = window.webkit?.messageHandlers?.isocast;
if (handler) {
  const post = handler.postMessage.bind(handler);
  handler.postMessage = (body) => {
    let method = null;
    try { method = JSON.parse(body).method ?? null; } catch {}
    const t0 = performance.now();
    const result = post(body);
    if (method && result?.then) result.then(() => calls.push([method, t0, performance.now() - t0]), () => {});
    return result;
  };
}
// The monitor's stream requests.
const requests = [];
const realFetch = window.fetch;
window.fetch = (url, init) => { if (String(url).includes("/_frames/")) requests.push(String(url)); return realFetch(url, init); };
// Count what the monitor's decoder gets and gives, per phase.
const video = { phase: "idle", idle: { key: 0, delta: 0, out: 0 }, drag: { key: 0, delta: 0, out: 0 } };
if (typeof VideoDecoder === "function") {
  const Real = VideoDecoder;
  window.VideoDecoder = class extends Real {
    constructor(init) {
      super({ ...init, output: (frame) => { video[video.phase].out++; init.output(frame); } });
    }
    decode(chunk) { video[video.phase][chunk.type === "key" ? "key" : "delta"]++; return super.decode(chunk); }
  };
}
let profiling = false;
const before = options.__b, diffed = options.diffed;
options.__b = (v) => { if (profiling && typeof v.type === "function") v.__t0 = performance.now(); before?.(v); };
options.diffed = (v) => {
  if (profiling && v.__t0 != null) {
    const name = v.type.displayName || v.type.name || "anonymous";
    const entry = renders.get(name) || [0, 0];
    entry[0] += 1; entry[1] += performance.now() - v.__t0;
    renders.set(name, entry); v.__t0 = null;
  }
  diffed?.(v);
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
// A window covered by another app's window (or on another Space) is
// hidden to WebKit: the monitor lets go of its stream and the drag loop
// stops, so the run measures nothing. Said at once, since it may never end.
document.addEventListener("visibilitychange", () => {
  if (document.hidden) log("drag-lag-hidden " + Math.round(performance.now()));
});
(async () => {
  let box = null;
  for (let i = 0; i < 200 && !box; i++) {
    box = [...document.querySelectorAll(".item-box")].find((b) => b.title === "Block");
    if (!box) await sleep(100);
  }
  if (!box) return log("drag-lag: no block on the canvas");
  await sleep(3000); // let the monitor settle
  // Select it and hold still, so the outline's offset can be measured at rest.
  let r = box.getBoundingClientRect();
  box.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true, pointerId: 6, pointerType: "mouse", isPrimary: true, button: 0, buttons: 1, clientX: r.left + 5, clientY: r.top + 5 }));
  box.dispatchEvent(new PointerEvent("pointerup", { bubbles: true, pointerId: 6, pointerType: "mouse", isPrimary: true, button: 0, buttons: 0, clientX: r.left + 5, clientY: r.top + 5 }));
  video.idle = { key: 0, delta: 0, out: 0 };
  await sleep(2000);
  video.phase = "drag";
  box = [...document.querySelectorAll(".item-box")].find((b) => b.title === "Block");
  r = box.getBoundingClientRect();
  const x0 = r.left + r.width / 2, y0 = r.top + r.height / 2;
  const span = Math.min(r.width * 2, box.parentElement.getBoundingClientRect().width / 4);
  const ev = (type, x, y, buttons) => new PointerEvent(type, {
    bubbles: true, cancelable: true, pointerId: 7, pointerType: "mouse", isPrimary: true,
    button: 0, buttons, clientX: x, clientY: y, metaKey: true,
  });
  box.dispatchEvent(ev("pointerdown", x0, y0, 1));
  const start = performance.now();
  profiling = true;
  const frames = [];
  await new Promise((done) => {
    const step = (now) => {
      frames.push(now);
      const t = (now - start) / 1000;
      const x = x0 + span * Math.sin((2 * Math.PI * t) / 1.5);
      box.dispatchEvent(ev("pointermove", x, y0, 1));
      if (t < SECONDS) requestAnimationFrame(step);
      else done();
    };
    requestAnimationFrame(step);
  });
  box.dispatchEvent(ev("pointerup", x0, y0, 0));
  profiling = false;
  const gaps = frames.slice(1).map((t, i) => t - frames[i]).sort((a, b) => a - b);
  const top = [...renders].sort((a, b) => b[1][1] - a[1][1]).slice(0, 15)
    .map(([name, [count, ms]]) => ({ name, renders: count, ms: Math.round(ms) }));
  log("drag-lag-profile " + JSON.stringify({
    page_frames_per_s: Math.round((frames.length / SECONDS) * 10) / 10,
    frame_gap_ms_p50_p95_max: [gaps[gaps.length >> 1], gaps[Math.floor(gaps.length * 0.95)], gaps[gaps.length - 1]].map((g) => Math.round(g * 10) / 10),
    components: top,
    stream_requests: requests,
    calls_during_drag: (() => {
      const during = calls.filter(([, t0]) => t0 >= start);
      const by = {};
      for (const [m, , ms] of during) (by[m] ??= []).push(ms);
      return Object.fromEntries(Object.entries(by).map(([m, xs]) => {
        xs.sort((a, b) => a - b);
        return [m, { count: xs.length, ms_p50: Math.round(xs[xs.length >> 1] * 10) / 10, ms_p95: Math.round(xs[Math.floor(xs.length * 0.95)] * 10) / 10, ms_max: Math.round(xs[xs.length - 1]) }];
      }));
    })(),
    decoder_per_s: {
      idle: Object.fromEntries(Object.entries(video.idle).map(([k, v]) => [k, v / 2])),
      drag: Object.fromEntries(Object.entries(video.drag).map(([k, v]) => [k, Math.round((v / SECONDS) * 10) / 10])),
    },
  }));
  log("drag-lag: done");
})();
"""


def run(cmd, env, check=True):
    out = subprocess.run([str(c) for c in cmd], env=env, capture_output=True, text=True)
    if check and out.returncode != 0:
        sys.exit(f"drag-lag: {' '.join(map(str, cmd))} failed:\n{out.stderr or out.stdout}")
    return out


def engine_env(sock, setup):
    return dict(os.environ, ISOCAST_CALIBRATION="off", ISOCAST_DEV_APP=str(DEV_APP), ISOCAST_SOCKET=sock,
                ISOCAST_SETUP=str(setup), ISOCAST_MONITOR_SILENT="1")


def apply(cli, env, setup_path):
    run([cli, "status"], env, check=False)
    rev = json.loads(run([cli, "get", "--json"], env).stdout)["revision"]
    run([cli, "apply", "--revision", rev, setup_path], env)


def shutdown(sock):
    subprocess.run(["nc", "-U", sock], input='{"jsonrpc":"2.0","id":1,"method":"shutdown"}\n',
                   capture_output=True, text=True, timeout=10)


def positions(video, width=960):
    """Left edges of the magenta block and the green outline, per frame."""
    probe = json.loads(subprocess.run(
        ["ffprobe", "-v", "error", "-select_streams", "v", "-show_entries",
         "stream=width,height,r_frame_rate", "-of", "json", str(video)],
        capture_output=True, text=True, check=True).stdout)["streams"][0]
    w, h = probe["width"], probe["height"]
    height = round(h * width / w / 2) * 2
    num, den = map(int, probe["r_frame_rate"].split("/"))
    raw = subprocess.run(["ffmpeg", "-v", "error", "-i", str(video), "-vf", f"scale={width}:{height}:flags=neighbor",
                          "-f", "rawvideo", "-pix_fmt", "rgb24", "-"], capture_output=True, check=True).stdout
    frames = np.frombuffer(raw, np.uint8).reshape(-1, height, width, 3).astype(np.int16)
    r, g, b = frames[..., 0], frames[..., 1], frames[..., 2]
    # Full-strength colours only: the test pattern behind the block has 75%
    # magenta and green bars (191), which looser limits took for the block
    # and the outline.
    magenta = (r > 225) & (g < 90) & (b > 225)
    green = (g > 225) & (r < 110) & (b < 110)

    def left(mask, bands=None):
        out = np.full(len(mask), np.nan)
        for i, frame in enumerate(mask):
            if bands is not None:
                if bands[i] is None:
                    continue
                frame = frame[bands[i]]
            idx = np.flatnonzero(frame.sum(axis=0) >= 3)  # columns with 3+ matching pixels
            if idx.size:
                out[i] = idx[0]
        return out

    # Only the rows the outline spans: elsewhere in the window (the source
    # toolbar's colour swatch) the block's colour shows too.
    bands = []
    for frame in green:
        rows = np.flatnonzero(frame.sum(axis=1) >= 3)
        bands.append(slice(rows[0], rows[-1] + 1) if rows.size else None)
    has_outline = any(b is not None for b in bands)
    return left(magenta, bands if has_outline else None), left(green), num / den, width / w


def block_scale(video):
    """Recorded pixels per canvas pixel: the outline's width (the block is
    240 canvas px wide; the outline adds 3 px a side)."""
    _, _, _, scale = positions(video)
    raw = subprocess.run(["ffmpeg", "-v", "error", "-i", str(video), "-vf", "scale=960:-2:flags=neighbor",
                          "-f", "rawvideo", "-pix_fmt", "rgb24", "-"], capture_output=True).stdout
    h = len(raw) // (960 * 3)
    frames = np.frombuffer(raw[: len(raw) - len(raw) % (h * 960 * 3)] if h else raw, np.uint8)
    frames = frames.reshape(-1, h, 960, 3).astype(np.int16) if h else frames
    widths = []
    for img in frames[:: max(1, len(frames) // 20)]:
        g = (img[..., 1] > 170) & (img[..., 0] < 110) & (img[..., 2] < 110)
        cols = np.flatnonzero(g.sum(axis=0) >= 3)
        if cols.size:
            widths.append(cols[-1] - cols[0] + 1)
    if not widths:
        return 1.0
    return (float(np.median(widths)) / scale) / 246


def analyse(video, window_points_per_canvas_px):
    block, outline, fps, scale = positions(video)
    ok = ~np.isnan(block) & ~np.isnan(outline)
    if ok.sum() < fps * 2:
        sys.exit(f"drag-lag: found the block in {int((~np.isnan(block)).sum())} frames and the outline in "
                 f"{int((~np.isnan(outline)).sum())}; is the window visible?")
    # At rest the outline sits a fixed offset left of the block (it is drawn
    # outside the box): measure it before the drag starts.
    first_move = np.flatnonzero(ok & (np.abs(np.diff(outline, prepend=outline[0])) > 0.5))
    if first_move.size == 0:
        sys.exit("drag-lag: the outline never moved; the drag script didn't run")
    start = first_move[0]
    rest = np.nanmedian((block - outline)[max(0, start - int(fps)):start])
    moving = np.arange(start, min(len(block), start + int(fps * 6)))
    moving = moving[ok[moving]]
    o, bl = outline[moving] + rest, block[moving]
    # Lag: the shift (in frames) that best lines the block up with the outline.
    best, best_err = 0, np.inf
    for s in range(0, int(fps * 1.0)):
        err = np.nanmean(np.abs(bl[s:] - o[: len(o) - s])) if s else np.nanmean(np.abs(bl - o))
        if err < best_err:
            best, best_err = s, err
    to_canvas = 1 / (scale * window_points_per_canvas_px)
    gap = np.abs(bl - o) * to_canvas
    moved = lambda xs: int((np.abs(np.diff(xs)) > 0.5).sum())
    seconds = len(moving) / fps
    if best >= int(fps * 1.0) - 1:
        print("drag-lag: the lag search hit its limit (1 s); the result is a floor", file=sys.stderr)
    return {
        "video": str(video),
        "frames_measured": int(len(moving)),
        "lag_ms": round(best * 1000 / fps, 1),
        "gap_canvas_px_median": round(float(np.median(gap)), 1),
        "gap_canvas_px_p95": round(float(np.percentile(gap, 95)), 1),
        "picture_moves_per_s": round(moved(bl) / seconds, 1),
        "outline_moves_per_s": round(moved(o) / seconds, 1),
        "recording_fps": round(fps, 2),
    }


AXIS = {"resize": "x", "resize-select": "x", "columns": "x", "rows": "y"}


def track(video, mode):
    """Per frame: the pointer mark's and the dragged edge's positions along
    the drag's axis (recorded pixels), the picture's edge (resize), and
    whether the edge showed twice and whether the picture (the magenta
    block) was there at all."""
    probe = json.loads(subprocess.run(
        ["ffprobe", "-v", "error", "-select_streams", "v", "-show_entries",
         "stream=width,height,r_frame_rate", "-of", "json", str(video)],
        capture_output=True, text=True, check=True).stdout)["streams"][0]
    w, h = probe["width"], probe["height"]
    num, den = map(int, probe["r_frame_rate"].split("/"))
    proc = subprocess.Popen(["ffmpeg", "-v", "error", "-i", str(video), "-f", "rawvideo",
                             "-pix_fmt", "rgb24", "-"], stdout=subprocess.PIPE)
    size = w * h * 3
    pointer, edge, picture, double, block, outline = [], [], [], [], [], []
    last = None
    while True:
        raw = proc.stdout.read(size)
        if len(raw) < size:
            break
        img = np.frombuffer(raw, np.uint8).reshape(h, w, 3)
        r, g, b = img[..., 0], img[..., 1], img[..., 2]
        magenta = (r > 225) & (g < 90) & (b > 225)
        block.append(bool(magenta.any()))
        cyan = (r < 110) & (g > 225) & (b > 225)
        green = (g > 225) & (r < 110) & (b < 110)
        if AXIS[mode] == "y":
            cyan, green, magenta = cyan.T, green.T, magenta.T
        # The mark is a bar a few pixels wide; scaled test patterns in the
        # picture can show a cyan column too: the narrow group nearest
        # where the mark was.
        cols = np.flatnonzero(cyan.sum(axis=0) >= 3)
        groups = [g for g in np.split(cols, np.flatnonzero(np.diff(cols) > 2) + 1)
                  if g.size and g[-1] - g[0] <= 8] if cols.size else []
        if groups:
            centres = [(g[0] + g[-1]) / 2 for g in groups]
            if last is None:
                at = max(range(len(groups)), key=lambda i: cyan[:, groups[i]].sum())
            else:
                at = min(range(len(groups)), key=lambda i: abs(centres[i] - last))
            last = centres[at]
            pointer.append(last)
        else:
            pointer.append(np.nan)
        counts = green.sum(axis=0)
        twice = False
        if mode.startswith("resize"):
            cols = np.flatnonzero(counts >= 3)
            edge.append(cols[-1] if cols.size else np.nan)
            outline.append(cols[-1] - cols[0] if cols.size else np.nan)
            rows = np.flatnonzero(green.sum(axis=1) >= 3)
            if rows.size:
                inside = magenta[rows[0]:rows[-1] + 1]
                pcols = np.flatnonzero(inside.sum(axis=0) >= 3)
                picture.append(pcols[-1] if pcols.size else np.nan)
            else:
                picture.append(np.nan)
        else:
            # The splitter is the longest green line; the box's outline
            # (also green) is shorter.
            top = counts.max()
            cols = np.flatnonzero(counts >= max(3, top * 0.6)) if top >= 20 else np.array([])
            if cols.size:
                groups = np.split(cols, np.flatnonzero(np.diff(cols) > 4) + 1)
                twice = len(groups) > 1
                main = max(groups, key=len)
                edge.append((main[0] + main[-1]) / 2)
            else:
                edge.append(np.nan)
            picture.append(np.nan)
        double.append(twice)
    proc.wait()
    return (np.array(pointer, float), np.array(edge, float), np.array(picture, float),
            np.array(double), np.array(block), num / den, np.array(outline, float))


def follow(pointer, edge, start, end, fps, points_per_px):
    """How `edge` follows `pointer` over frames start..end."""
    ok = ~np.isnan(pointer) & ~np.isnan(edge)
    # At rest before the drag, or after it (it ends where it began) when
    # the recording started late.
    rest_frames = np.arange(max(0, start - int(fps)), start)
    rest_frames = rest_frames[ok[rest_frames]]
    if rest_frames.size < 10:
        after = np.arange(min(len(edge), end + int(fps * 0.5)), min(len(edge), end + int(fps * 1.5)))
        rest_frames = after[ok[after]]
    rest = np.median(edge[rest_frames] - pointer[rest_frames]) if rest_frames.size else 0.0
    idx = np.arange(start, end)
    good = idx[ok[idx]]
    p, e = pointer[good], edge[good] - rest
    best, best_err = 0, np.inf
    for shift in range(0, int(fps * 0.5)):
        err = np.mean(np.abs(e[shift:] - p[:len(p) - shift]))
        if err < best_err:
            best, best_err = shift, err
    gap = np.abs(e - p) * points_per_px
    # Glitches, frame by frame: the edge stepping against the way the
    # (lag-shifted) pointer went, or standing still while it moved a
    # point or more.
    back = stall = 0
    for i in range(best + 1, len(e)):
        dp = p[i - best] - p[i - best - 1]
        de = e[i] - e[i - 1]
        if abs(dp) * points_per_px >= 1.0:
            if de * dp < 0 and abs(de) * points_per_px >= 1.0:
                back += 1
            elif abs(de) * points_per_px < 0.25:
                stall += 1
    moved = lambda xs: int((np.abs(np.diff(xs)) * points_per_px >= 0.25).sum())
    seconds = len(idx) / fps
    return {
        "gap_points_median": round(float(np.median(gap)), 1),
        "gap_points_p95": round(float(np.percentile(gap, 95)), 1),
        "lag_ms": round(best * 1000 / fps, 1),
        "edge_moves_per_s": round(moved(e) / seconds, 1),
        "pointer_moves_per_s": round(moved(p) / seconds, 1),
        "back_steps": back,
        "stalls": stall,
        "missing": int(len(idx) - good.size),
    }


def analyse_edge(video, mode, span_points, direction=(1.0, 1.0)):
    pointer, edge, picture, double, block, fps, outline = track(video, mode)
    seen = np.flatnonzero(~np.isnan(pointer))
    if seen.size < fps * 2:
        sys.exit(f"drag-lag: the pointer mark showed in {seen.size} frames; is the window visible?")
    moving = np.flatnonzero(np.abs(np.diff(pointer)) > 0.5)
    if moving.size == 0:
        sys.exit("drag-lag: the pointer mark never moved; the drag script didn't run")
    start, end = moving[0], moving[-1] + 2
    swing = np.nanmax(pointer[start:end]) - np.nanmin(pointer[start:end])
    # The swing along the measured axis (resize goes along the diagonal).
    along = abs(direction[1] if AXIS[mode] == "y" else direction[0]) or 1.0
    points_per_px = 2 * span_points * along / swing if swing else 1.0
    result = {"video": str(video), "mode": mode, "frames_measured": int(end - start),
              "recording_fps": round(fps, 2), "points_per_recorded_px": round(points_per_px, 3)}
    result.update(follow(pointer, edge, start, end, fps, points_per_px))
    result["double_edge_frames"] = int(double[start:end].sum())
    result["picture_missing_frames"] = int((~block[start:end]).sum())
    if mode.startswith("resize"):
        result["picture"] = follow(pointer, picture, start, end, fps, points_per_px)
    if mode == "resize-select":
        # The app selects the scene's bottom item (the whole canvas) as it
        # lets go, and the mark disappears then: frames until the outline
        # goes round that item (its width within 10% of where it ends).
        gone = next((i for i in range(end - 2, len(pointer)) if np.isnan(pointer[i])), None)
        final = np.nanmedian(outline[-30:]) if len(outline) >= 30 else np.nan
        if gone is None or np.isnan(final):
            sys.exit("drag-lag: no selection change at the end of the drag")
        right = next((i for i in range(gone, len(outline))
                      if not np.isnan(outline[i]) and outline[i] >= 0.9 * final), len(outline))
        result["wrong_outline_frames"] = int(right - gone)
        result["wrong_outline_ms"] = round((right - gone) * 1000 / fps, 1)
    result["glitch_frames"] = (result["back_steps"] + result["missing"] + result["double_edge_frames"]
                               + result["picture_missing_frames"])
    return result


def probe_files(report):
    """The test engine's files: the report's drops and ffprobe's count."""
    files = []
    for f in report.get("files", []):
        out = subprocess.run(["ffprobe", "-v", "error", "-select_streams", "v:0", "-count_packets",
                              "-show_entries", "stream=nb_read_packets,r_frame_rate:format=duration",
                              "-of", "json", f["path"]], capture_output=True, text=True)
        info = json.loads(out.stdout or "{}")
        stream = (info.get("streams") or [{}])[0]
        files.append({
            "source": f["source"],
            "frames": f["frames"],
            "dropped_frames": f["dropped_frames"],
            "ffprobe_packets": int(stream.get("nb_read_packets", 0)),
            "ffprobe_duration": round(float(info.get("format", {}).get("duration", 0)), 2),
        })
    return files


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--work", type=Path, default=SCRATCH / "lag")
    p.add_argument("--seconds", type=float, default=6)
    p.add_argument("--label", default=time.strftime("%Y%m%d-%H%M%S"))
    p.add_argument("--app", choices=["web", "ui"], default="ui",
                   help="ui: the Slint app, isocast-ui (default, Isocast.app since N.3); "
                        "web: the retired web app, isocast-app")
    p.add_argument("--bin-dir", type=Path, default=REPO / "target/release",
                   help="where the release binaries are")
    p.add_argument("--analyse", type=Path, help="only analyse this existing window recording")
    p.add_argument("--drag", help="with --analyse and --mode resize|columns|rows: the run's "
                                  "app_run.drag JSON (its span and direction)")
    p.add_argument("--mode", choices=["move", "resize", "resize-select", "columns", "rows"],
                   default="move",
                   help="what to drag (resize, columns and rows: Slint app only)")
    p.add_argument("--hide", default="",
                   help="docks to hide, comma-separated: scenes,sources,mixer,transitions,controls")
    p.add_argument("--record", action="store_true",
                   help="record the program and two ISO test patterns meanwhile")
    p.add_argument("--diagnose", action="store_true",
                   help="also count layout changes reaching the engine and analyse the engine's own frames")
    args = p.parse_args()
    if args.analyse:
        if args.mode != "move":
            # The app's "drag" (span and direction) from that run's results.
            if not args.drag:
                sys.exit("drag-lag: --analyse with --mode resize|columns|rows needs --drag JSON")
            drag = json.loads(args.drag)
            print(json.dumps(analyse_edge(args.analyse, args.mode, drag["span_points"],
                                          drag.get("direction", (1.0, 1.0))), indent=2))
            return
        print(json.dumps(analyse(args.analyse, block_scale(args.analyse)), indent=2))
        return
    if args.mode != "move" and args.app != "ui":
        sys.exit("drag-lag: --mode resize, columns and rows need --app ui")
    work = args.work.resolve()
    if not str(work).startswith(str(SCRATCH) + "/"):
        sys.exit(f"drag-lag: --work must be under {SCRATCH}/ (ISOCAST_SCRATCH)")
    work.mkdir(parents=True, exist_ok=True)
    rel = args.bin_dir
    app_name = "isocast-ui" if args.app == "ui" else "isocast-app"
    cli, daemon, app = rel / "isocast", rel / "isocastd", rel / app_name
    # The window source finds windows by the app's name: isocast-ui calls
    # itself Isocast (its Info.plist); the title tells it from the real app.
    window_app = "Isocast" if args.app == "ui" else app_name
    for b in (cli, daemon, app):
        if not b.exists():
            sys.exit(f"drag-lag: build {b.name} first (cargo build --release)")

    # The interface copy with the test additions.
    ui = work / "ui"
    if ui.exists():
        shutil.rmtree(ui)
    shutil.copytree(REPO / "crates/isocast-app/ui", ui)
    # Experiments on the test copy only: DRAG_PAINTER=flat draws monitors on
    # a 2D canvas instead of WebGL.
    if os.environ.get("DRAG_PAINTER") == "flat":
        painter = ui / "ui/painter.js"
        painter.write_text(painter.read_text().replace(
            "painter = glPainter(canvas) ?? flatPainter(canvas);", "painter = flatPainter(canvas);"))
    (ui / "drag-test.css").write_text(TEST_CSS)
    (ui / "drag-test.js").write_text(TEST_JS % {"seconds": args.seconds})
    index = (ui / "index.html").read_text()
    index = index.replace('<script type="module" src="app.js"></script>',
                          '<script type="module" src="app.js"></script>\n<link rel="stylesheet" href="drag-test.css">\n'
                          '<script type="module" src="drag-test.js"></script>')
    (ui / "index.html").write_text(index)

    test_sock, rec_sock = "/tmp/isocast-lag.sock", "/tmp/isocast-lagrec.sock"
    test_env = engine_env(test_sock, work / "test-live.toml")
    rec_env = engine_env(rec_sock, work / "rec-live.toml")
    run([REPO / "scripts/sign-dev.sh", daemon], test_env)
    # Resizing wants room: a larger block, clear of the test patterns.
    block = "x = 720, y = 300, width = 480, height = 480" if args.mode.startswith("resize") else \
        "x = 840, y = 420, width = 240, height = 240"
    patterns = """
[[sources]]
id = "bars1"
name = "Bars 1"
kind = "test_pattern"

[[sources]]
id = "bars2"
name = "Bars 2"
kind = "test_pattern"
""" if args.record else ""
    pattern_items = """
  { source = "bars1", rect = { x = 40, y = 40, width = 320, height = 180 } },
  { source = "bars2", rect = { x = 40, y = 260, width = 320, height = 180 } },""" if args.record else ""
    (work / "test.toml").write_text(f"""version = 1
name = "Lag test"

[canvas]
width = {CANVAS[0]}
height = {CANVAS[1]}
fps = 60

[[sources]]
id = "background"
name = "Background"
kind = "color"
color = "#2a2d31"

[[sources]]
id = "block"
name = "Block"
kind = "color"
color = "#ff00ff"
width = 240
height = 240
{patterns}
[[scenes]]
id = "main"
name = "Main"
items = [
  {{ source = "background", rect = {{ x = 0, y = 0, width = {CANVAS[0]}, height = {CANVAS[1]} }} }},{pattern_items}
  {{ source = "block", rect = {{ {block} }} }},
]

[recording]
folder = "{work}/test-rec/{args.label}"

[recording.program]
enabled = {"true" if args.record else "false"}
encoder = {{ codec = "hevc", quality = 70 }}
""" + ("""
[recording.iso]
enabled = true
sources = ["bars1", "bars2"]
encoder = { codec = "hevc", quality = 70 }
""" if args.record else ""))
    folder = work / "runs" / args.label
    folder.mkdir(parents=True, exist_ok=True)
    (work / "rec.toml").write_text(f"""version = 1
name = "Lag recorder"

[canvas]
width = 1920
height = 1080
fps = 60

[[sources]]
id = "window"
name = "Window"
kind = "window"
app = "{window_app}"
title = "Lag test"

[[scenes]]
id = "main"
name = "Main"
items = [{{ source = "window", rect = {{ x = 0, y = 0, width = 1920, height = 1080 }} }}]

[recording]
folder = "{folder}"

[recording.program]
enabled = false
encoder = {{ codec = "hevc", quality = 70 }}

[recording.iso]
enabled = true
sources = ["window"]
encoder = {{ codec = "hevc", quality = 95 }}
""")
    with open(LOCK, "a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)  # no benchmark measures meanwhile
        app_proc = None
        try:
            apply(cli, test_env, work / "test.toml")
            run([cli, "engine", "start"], test_env)
            # Background mode: the window shows without taking focus.
            # Its own layout file, so every run starts from the default
            # layout (and Ryan's is never touched).
            prefs = work / "ui-prefs.json"
            prefs.unlink(missing_ok=True)
            hidden = [d for d in args.hide.split(",") if d]
            if hidden:
                prefs.write_text(json.dumps({d: False for d in hidden}))
            app_env = dict(test_env, ISOCAST_UI_DIR=str(ui), ISOCAST_BACKGROUND="1",
                           ISOCAST_UI_PREFS=str(prefs))
            if args.record:
                run([cli, "start"], test_env)
            if args.app == "ui":
                # It drags after the picture has settled (3 s), measures the
                # run (and says whether the window was covered) and quits.
                command = [str(app), "--drag-test", str(args.seconds), "--drag-mode", args.mode,
                           "--check", "--bench", str(args.seconds + 8), "--no-float"]
            else:
                command = [str(app)]
            app_proc = subprocess.Popen(command, env=app_env, stdout=open(work / "app.json", "w"),
                                        stderr=open(work / "app.log", "w"))
            time.sleep(1)  # the window and its title
            apply(cli, rec_env, work / "rec.toml")
            # Soon, so the drag's resting start is recorded; a cold start (a
            # test Mac's first run of a new build) can take longer to show
            # the window.
            for attempt in range(40):
                started = run([cli, "start", "--seconds", str(int(args.seconds + 9))], rec_env,
                              check=attempt == 39)
                if started.returncode == 0:
                    break
                time.sleep(0.4)
            probes = []
            if args.diagnose:
                # Layout changes reaching the engine, one line per event.
                probes.append(subprocess.Popen(
                    [sys.executable, "-c", WATCH, test_sock, str(work / "applies.txt"), str(args.seconds + 10)]))
                # The engine's own picture, straight from its frame stream.
                probes.append(subprocess.Popen(
                    [str(cli), "frames", "program", "--width", os.environ.get("DIAG_WIDTH", "960"), "--fps", os.environ.get("DIAG_FPS", "60"), "--seconds",
                     str(int(args.seconds + 9)), "-o", str(folder / "engine.h264")],
                    env=test_env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
            time.sleep(args.seconds + 11)
            for probe in probes:
                probe.wait(timeout=30)
            if args.record:
                out = run([cli, "stop", "--wait", "--json"], test_env, check=False)
                (work / "test-report.json").write_text(out.stdout)
        finally:
            if app_proc:
                app_proc.terminate()
            shutdown(test_sock)
            shutdown(rec_sock)
    # In the recording's project folder.
    videos = sorted(folder.glob("*/WINDOW-*.mov")) or sorted(folder.glob("WINDOW-*.mov"))
    if not videos:
        sys.exit(f"drag-lag: no recording in {folder}; see {work}/app.log")
    app_log = (work / "app.log").read_text(errors="replace")
    if "drag-lag-hidden " in app_log or "this measurement is not valid" in app_log:
        sys.exit("drag-lag: the test window was hidden during the run (covered by another app's "
                 "window, or on another Space), so WebKit paused the page: the monitor let go of "
                 "its stream (as it should) and the drag stopped. Nothing to measure; keep the "
                 f"window uncovered and run again (recording in {folder})")
    app_run = None
    if args.app == "ui" and (work / "app.json").exists():
        with contextlib.suppress(ValueError):
            app_run = json.loads((work / "app.json").read_text())
    if args.mode == "move":
        result = analyse(videos[-1], block_scale(videos[-1]))
    else:
        drag = (app_run or {}).get("drag") or {}
        if not drag.get("span_points"):
            sys.exit(f"drag-lag: the app didn't say what it dragged; see {work}/app.log")
        result = analyse_edge(videos[-1], args.mode, drag["span_points"],
                              drag.get("direction", (1.0, 1.0)))
    result["label"] = args.label
    result["app"] = args.app
    result["recording"] = args.record
    result["hide"] = args.hide
    if app_run is not None:
        result["app_run"] = app_run
    if args.record:
        with contextlib.suppress(ValueError, OSError):
            report = json.loads((work / "test-report.json").read_text())
            result["recorded_files"] = probe_files(report)
    for line in (work / "app.log").read_text(errors="replace").splitlines():
        if "drag-lag-profile " in line:
            result["page"] = json.loads(line.split("drag-lag-profile ", 1)[1])
    if args.diagnose:
        times = [float(t) for t in (work / "applies.txt").read_text().split()] if (work / "applies.txt").exists() else []
        if len(times) > 2:
            span = times[-1] - times[0]
            gaps = sorted(b - a for a, b in zip(times, times[1:]))
            result["engine_layout_changes_per_s"] = round((len(times) - 1) / span, 1) if span else None
            result["engine_layout_gap_ms_p50_p95_max"] = [round(gaps[len(gaps) // 2] * 1000, 1),
                                                          round(gaps[int(len(gaps) * 0.95)] * 1000, 1),
                                                          round(gaps[-1] * 1000, 1)]
        engine = folder / "engine.h264"
        if engine.exists():
            block, _, fps, _ = positions(engine)
            seen = block[~np.isnan(block)]
            moving = np.abs(np.diff(seen)) > 0.5
            first, last = (np.flatnonzero(moving)[[0, -1]] if moving.any() else (0, 0))
            dur = max(1, last - first) / 60
            result["engine_picture_moves_per_s"] = round(int(moving[first:last + 1].sum()) / dur, 1)
    with open(work / "results.jsonl", "a") as out:
        out.write(json.dumps(result) + "\n")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
