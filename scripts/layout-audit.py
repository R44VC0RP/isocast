#!/usr/bin/env python3
"""Layout audit: render every screen of the app at many window sizes
offscreen, have one small vision model check each picture for layout
problems, in parallel, and write a report with the problems boxed on the
pictures (GitHub #107):

    scripts/layout-audit.py run --bin BIN_DIR --out DIR [--views LIST]
        [--sizes LIST] [--sweep SPEC|none] [--sweep-views LIST]
        [--jobs 6] [--parallel 12] [--model opencode/claude-haiku-5-5]
    scripts/layout-audit.py render …   only draw the pictures (DIR/shots)
    scripts/layout-audit.py check …    check what DIR/shots has (--only ID…,
                                       --recheck to redo checked ones)
    scripts/layout-audit.py report --out DIR   rebuild the report
    scripts/layout-audit.py calibrate --bin BIN_DIR --out DIR
                                       deliberately broken pictures, which
                                       must all be flagged
    scripts/layout-audit.py list       the screens and sizes

BIN_DIR holds isocast, isocastd and isocast-ui from one build. Each picture
is drawn by `isocast-ui --snapshot` against an engine of its own (its own
socket, setup, timelines, recordings and UI prefs in a scratch folder under
/tmp), with the synthetic sources of scripts/screenshots/offscreen.toml, so
states that act (record, go-live) touch nothing real and no window opens.
--views and --sweep-views take names or shell patterns (settings-*); --sizes
takes WxH@SCALE items; --sweep is WIDTH_FROM-WIDTH_TO:STEP@HEIGHTxSCALE
(default 900-2700:200@900x1), drawn for --sweep-views only.

Each check is a restricted `opencode run --standalone --agent checker` (the
model Ryan chose, Claude Haiku 5.5), from a fresh copy of
scripts/layout-audit/checker with an empty global config and its own data
folder; it sees one downscaled picture in a read-only folder and answers
only through its `deliver_layout` tool (validated there and again here). No
valid answer after one retry is "check failed", never a guessed verdict.
Auth: OPENCODE_API_KEY if set (each run then gets its own data folder too),
else this Mac's OpenCode sign-in (its data folder; config stays isolated).

The report (DIR/report.json, report.html, report.md) groups pictures by
screen, draws each issue's box, and merges the same issue across sizes
("at widths <= 1280"). Exit status: 1 if any high-severity issue, 3 if a
picture or a check failed (and nothing is high), else 0.

Python standard library only (plus macOS `sips` to downscale).
"""

import argparse
import concurrent.futures as cf
import fnmatch
import html
import json
import os
import re
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import threading
import time
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
CHECKER = os.path.join(HERE, "layout-audit", "checker")
SHOTS_SRC = os.path.join(HERE, "screenshots")

KINDS = (
    "clipped_text",
    "truncated_text",
    "overlap",
    "misaligned",
    "uneven_spacing",
    "overflow",
    "cut_off_control",
    "icon_problem",
    "contrast",
    "empty_or_broken",
    "other",
)
SEVERITIES = ("low", "medium", "high")
MAX_ISSUES = 10
# The checker's picture: the long side at most this (Claude's vision input
# is downscaled past ~1568 px anyway; boxes are in this picture's pixels).
CHECK_EDGE = 1568

SIZES = [
    "900x600@2",
    "1280x800@2",
    "1440x900@2",
    "1512x982@2",
    "1920x1080@1",
    "2560x1440@2",
    "3440x1440@1",
    "1080x1350@2",
]
# From the window's minimum width (900 points, ui/app.slint) up.
SWEEP = "900-2700:200@900x1"
SWEEP_VIEWS = ["main", "studio-mode", "vertical-canvas", "canvases", "settings-output", "editor"]

MAIN = (
    "the main window: a Scenes dock and a Sources dock on the left, the "
    "preview picture in the middle with a viewer line above it (zoom, View), "
    "a source toolbar, an audio mixer with level meters, a transitions dock, "
    "the controls (Record, Go Live and friends) and a status bar along the bottom"
)
# Every screen: (name, states, what it should contain, options). Options:
# seed="editor" (an engine with three recorded takes), env (extra variables;
# @OBS@ and @OPTIMIZED@ are replaced with generated fixtures).
VIEWS = [
    ("main", [], MAIN, {}),
    ("studio-mode", ["studio"], "Studio Mode: " + MAIN + ", with two pictures side by side, Preview "
     "(left) and Program (right), and transition buttons between or under them", {}),
    ("vertical-canvas", ["vertical-canvas:on"], MAIN + "; a second, vertical (9:16) canvas is on, so "
     "a tall vertical preview shows beside the main picture", {}),
    ("vertical-compact", ["vertical-canvas:on", "vertical-layout:compact"], MAIN + "; the vertical "
     "canvas is on in its compact layout (a smaller vertical monitor)", {}),
    ("multiview", ["studio", "projector:multiview", "wait:1000"], "a Multiview projector window: a grid of tiles, "
     "large Preview and Program tiles on top and smaller scene tiles below (top-8 layout), each "
     "labelled with its scene name, with tally borders", {}),
    ("multiview-3x3", ["projector:multiview:3x3", "wait:1000"], "a Multiview projector window in a 3x3 grid of "
     "labelled tiles (preview, program and scenes)", {}),
    ("projector-program", ["projector:program", "wait:1000"], "a projector window showing only the program picture "
     "(the video, letterboxed to fit), with nothing else drawn over it", {}),
    ("projector-vertical", ["vertical-canvas:on", "projector:vertical-program", "wait:1000"], "a projector window "
     "showing the vertical (9:16) canvas's program picture (this setup's scenes are made for 16:9, "
     "so the picture may fill only part of the tall canvas; the rest is black)", {}),
    ("canvases", ["vertical-canvas:on", "extra-canvases:shorts=1080x1920,square=1080x1080",
                  "extra-canvas:square", "wait:800"], MAIN + "; besides the main canvas the setup has "
     "three extra canvases (Vertical, Shorts, Square); the Square canvas's picture shows in a column "
     "of its own at the stage's right, under a line with its name as a tag (SQUARE), its scene and a "
     "size menu (1080×1080 ⌄)", {}),
    ("canvases-menu", ["vertical-canvas:on", "extra-canvases:shorts=1080x1920,square=1080x1080",
                       "wait:800", "menu:viewer:extra", "wait:300"], MAIN + "; a canvas column at the "
     "stage's right, and its menu open: the three extra canvases (Vertical checked, Shorts, Square) "
     "with their sizes, then show, edit, record, stream, replay and projector items", {}),
    ("edit-canvas-extra", ["vertical-canvas:on", "extra-canvases:shorts=1080x1920", "wait:800",
                           "edit-canvas:shorts", "wait:800"], MAIN + "; the editor edits the Shorts "
     "canvas, so the preview is a tall 9:16 picture and the viewer line says Shorts · 1080×1920; "
     "the canvas column is gone while editing it", {}),
    ("projector-canvas", ["extra-canvases:square=1080x1080", "wait:800", "projector:square-program",
                          "wait:1000"], "a projector window showing an extra square (1:1) canvas's "
     "program picture (this setup's scenes are made for 16:9, so the picture may fill only part of "
     "the square; the rest is black)", {}),
    ("multiview-canvases", ["vertical-canvas:on", "extra-canvases:square=1080x1080", "wait:1500",
                            "studio", "projector:multiview", "wait:1000"], "a Multiview projector "
     "window: large Preview and Program tiles on top, scene tiles below, and its last two tiles "
     "show the extra canvases' programs, labelled VERTICAL and SQUARE", {}),
    ("editor", ["editor", "wait:800", "create-thumbs"], "the Editor window: the picture viewer on top "
     "(no transport row: playback is by keys), a toolbar row (edit tools pill, the timeline's name "
     "and length, Export, a zoom slider), and a timeline with a time ruler and playhead, a Program "
     "track of three clips with thumbnails, and Camera and Slides angle tracks", {"seed": "editor"}),
    ("editor-crop", ["editor", "wait:800", "create-thumbs", "create-playhead:3.5", "create-angle:slides", "wait:500",
                     "create-crop:9:16", "wait:500", "create-crop-mode:on", "wait:300"], "the Editor window in crop mode: the viewer shows a 9:16 "
     "crop frame with handles over the picture and crop controls, timeline below", {"seed": "editor"}),
    ("editor-export", ["editor", "wait:800", "create-thumbs", "create-export", "wait:1500"], "the "
     "Editor window just after an export: the toolbar row shows Export and Show in Finder (or "
     "Exporting… with a progress bar while it runs), timeline below", {"seed": "editor"}),
    ("settings-output", ["settings:output"], "the Settings sheet over the main window, Output page: a "
     "sidebar of pages (Output, Video, Audio, Stream, General, Hotkeys, Advanced) and a form "
     "(recording folder, quality, files each take makes, ISO sources)", {}),
    ("settings-video", ["settings:video"], "the Settings sheet, Video page: canvas size and frame rate "
     "fields", {}),
    ("settings-audio", ["settings:audio"], "the Settings sheet, Audio page: the monitoring device and "
     "balance law (this setup has no microphone devices, so there is no microphone list)", {}),
    ("settings-stream", ["settings:stream"], "the Settings sheet, Stream page: a list of streaming "
     "services to choose from (none chosen yet)", {}),
    ("settings-stream-vertical", ["vertical-canvas:on", "settings:stream-vertical"], "the Settings "
     "sheet, Stream page for the vertical canvas's destinations", {}),
    ("settings-general", ["settings:general"], "the Settings sheet, General page: app behaviour, "
     "scenes and Studio Mode options, updates", {}),
    ("settings-hotkeys", ["settings:hotkeys", "hotkeys-sample"], "the Settings sheet, Hotkeys page: a "
     "list of actions with their key shortcuts", {}),
    ("settings-advanced", ["settings:advanced"], "the Settings sheet, Advanced page: advanced "
     "recording, encoder and developer options", {}),
    ("properties-mic", ["dialog:properties:mic"], "a Properties dialog for an audio source (a tone "
     "named Mic): its fields, with Cancel/OK buttons", {}),
    ("properties-camera", ["select:camera-inset", "dialog:properties:camera"], "a Properties dialog "
     "for a video source (a test pattern named Camera) with a live preview and its fields, "
     "Cancel/OK buttons", {}),
    ("properties-slides", ["dialog:properties:slides"], "a Properties dialog for an image source "
     "(Slides) with its file path and fields", {}),
    ("filters", ["dialog:filters:camera"], "the Filters dialog for the Camera source: a list of "
     "filters and the selected filter's settings", {}),
    ("transform", ["select:camera-inset", "dialog:transform"], "the Edit Transform dialog for the "
     "selected camera item: position, size, rotation, crop and alignment fields", {}),
    ("create-source", ["dialog:create-source:image"], "the dialog to add a source (an image): create "
     "new or add existing, name field, buttons", {}),
    ("transition-dialog", ["dialog:transition:fade"], "the transition dialog: kind and duration of the "
     "scene transition", {}),
    ("advanced-audio", ["dialog:advanced-audio"], "the Advanced Audio Properties dialog: one row per "
     "audio source with volume, mono, balance, monitoring and track columns", {}),
    ("rename", ["select:camera-inset", "rename"], "the main window with a source being renamed: an "
     "inline text field (or small dialog) in the Sources dock", {}),
    ("about", ["about"], "the About dialog: app icon, name, version and links", {}),
    ("bug-report", ["bug-report"], "the bug report dialog: a description field, what will be attached, "
     "and buttons", {}),
    ("quit-recording", ["quit:recording"], "the quit question while recording: a message and buttons "
     "to keep recording or stop and quit", {}),
    ("menu-mixer", ["mixer-menu:mic"], MAIN + "; the mixer strip's menu for Mic is open", {}),
    ("menu-add-source", ["add-menu"], MAIN + "; the Add Source menu (source kinds) is open", {}),
    ("menu-scene", ["menu:scenes:main"], MAIN + "; a scene's context menu is open over the window", {}),
    ("menu-source", ["menu:sources:item:camera-inset"], MAIN + "; a source item's context menu is open",
     {}),
    ("menu-viewer-view", ["menu:viewer:view"], MAIN + "; the viewer's View menu is open", {}),
    ("menu-bar-view", ["menu-bar-open:View"], MAIN + "; a menu bar is drawn along the top of the window "
     "with its View menu open", {}),
    ("toasts", ["toasts"], MAIN + "; several notification toasts (info, success, warning, error) are "
     "stacked in a corner", {}),
    ("toast-drops", ["toast-drops"], MAIN + "; a warning toast about dropped frames is shown", {}),
    ("first-run", ["first-run"], "the first-run welcome sheet: a short introduction and the first "
     "step of setting up, with buttons", {}),
    ("first-launch-obs", ["wizard-first-launch", "wait:1500"], "the first-launch sheet offering to import from OBS "
     "(it found OBS scene collections and profiles) or start fresh", {"env": {"ISOCAST_OBS_FOLDER": "@OBS@"}}),
    ("wizard-obs", ["wizard:obs", "wait:1500"], "the OBS import step of the setup assistant: OBS collections and "
     "profiles to import, with buttons", {"env": {"ISOCAST_OBS_FOLDER": "@OBS@"}}),
    ("wizard-goal", ["wizard:goal"], "the Optimize assistant's goal step: choices such as recording or "
     "streaming, with buttons", {}),
    ("optimize-running", ["wizard:goal", "wizard-sample:optimizing"], "the Optimize assistant while "
     "it measures this Mac: a progress indicator and explanation", {}),
    ("optimize-summary", ["wizard:goal", "wizard-sample:@OPTIMIZED@"], "the Optimize assistant's "
     "summary: the recommended settings, the changes it would make, notes, and Apply/Cancel", {}),
    ("permissions", ["permissions-sample:first-launch"], "the permissions sheet on first launch: the "
     "camera, microphone and screen recording permissions with buttons to allow them", {}),
    ("recording", ["record", "wait:1500"], MAIN + "; recording is running: the Record control shows "
     "it is live with a timer, the status bar shows recording", {}),
    ("recording-details", ["record", "wait:1000", "details:rec"], MAIN + "; recording, with the "
     "recording details popover open (files, sizes, dropped frames)", {}),
    ("paused", ["record", "wait:1000", "pause"], MAIN + "; a paused recording: the controls show "
     "Paused and resume", {}),
    ("record-target", ["record", "wait:1500", "stop-record", "wait:2500", "record-target:last",
                       "wait:800"], MAIN + "; next to Start Recording, where the next recording "
     "goes (\"Take 2 · Untitled\", a row under it or a square button beside it), and the status bar "
     "says \"Next recording: take 2 of …\"; a \"Saved as a new project\" toast", {}),
    ("record-target-menu", ["record", "wait:1500", "stop-record", "wait:2500", "record-target-menu",
                            "wait:300"], MAIN + "; the menu of where the next recording goes is "
     "open next to Start Recording: Next Recording, New Project, Add a Take To and a timeline", {}),
    ("streaming", ["stream:live"], MAIN + "; streaming live: the Go Live control shows live with a "
     "timer and the status bar shows the stream's health", {}),
    ("streaming-details", ["stream:live", "details:live"], MAIN + "; streaming, with the stream "
     "details popover open (destination, bitrate, health)", {}),
    ("stream-reconnecting", ["stream:reconnecting"], MAIN + "; the stream is reconnecting: a warning "
     "state on the Go Live control and status bar", {}),
    ("vertical-live", ["vertical-canvas:on", "stream:live", "vertical-stream:live"], MAIN + "; the "
     "vertical canvas is on and both the main and the vertical stream are live", {}),
    ("replay", ["replay:running"], MAIN + "; the replay buffer is running (a control shows it)", {}),
]
VIEW_NAMES = [v[0] for v in VIEWS]

# A small OBS folder for the import screens (two collections, a profile).
OBS_COLLECTION = {
    "name": "Recording",
    "current_scene": "Main",
    "scene_order": [{"name": "Main"}, {"name": "Accent"}],
    "sources": [
        {"id": "scene", "versioned_id": "scene", "name": "Main", "uuid": "a1",
         "settings": {"items": [{"name": "Background", "visible": True, "pos": {"x": 0, "y": 0},
                                 "scale": {"x": 1, "y": 1}, "bounds_type": 0}]}},
        {"id": "scene", "versioned_id": "scene", "name": "Accent", "uuid": "a2", "settings": {"items": []}},
        {"id": "color_source_v3", "versioned_id": "color_source_v3", "name": "Background", "uuid": "a3",
         "settings": {"color": 4280295472, "width": 1920, "height": 1080}},
    ],
}
OBS_PROFILE = """[General]
Name=Recording

[Output]
Mode=Advanced

[AdvOut]
RecType=Standard
RecFormat2=mkv
RecEncoder=com.apple.videotoolbox.videoencoder.ave.hevc

[Video]
BaseCX=1920
BaseCY=1080
OutputCX=1920
OutputCY=1080
FPSType=1
FPSInt=60
"""
OPTIMIZED = {
    "goal": "recording",
    "summary": "1440p60, HEVC High, 3 ISO files",
    "setup": None,  # filled from offscreen.toml's shape at run time (see fixtures)
    "differences": [
        {"path": "recording.iso.enabled", "op": "change", "from": False, "to": True},
        {"path": "recording.program.encoder.codec", "op": "change", "from": "h264", "to": "hevc"},
        {"path": "recording.program.encoder.quality", "op": "change", "from": "medium", "to": "high"},
        {"path": "recording.program.height", "op": "add", "to": 1440},
        {"path": "recording.program.width", "op": "add", "to": 2560},
    ],
    "tuning": {
        "streams": [{"source": "camera", "width": 1920, "height": 1080, "fps": 60},
                    {"source": "program", "width": 3840, "height": 2160, "fps": 60}],
        "disk_bytes_per_second": 573896614.2, "free_bytes": 79754743808, "capacity": "no_margin",
        "realtime_factor": 1.11,
        "candidates": [
            {"encoder": {"codec": "hevc", "quality": "high", "container": "mov", "bit_depth": 8},
             "realtime_factor": 1.11, "bytes_per_second": 14583765.0, "capacity": "no_margin", "feasible": False,
             "reason": "keeps up with no margin: 1.11× real time; fitting takes 1.25×, so a busy Mac doesn't cost frames"},
            {"encoder": {"codec": "h264", "quality": "high", "container": "mov", "bit_depth": 8},
             "realtime_factor": 0.97, "bytes_per_second": 14588947.5, "capacity": "cannot_keep_up", "feasible": False,
             "reason": "can't keep up: 0.97× real time, so about 3% of frames would be dropped"},
            {"encoder": {"codec": "prores422", "quality": "high", "container": "mov", "bit_depth": 8},
             "realtime_factor": 1.67, "bytes_per_second": 62800964.0, "capacity": "fits", "feasible": False,
             "reason": "only 0.3 h fit in 79.8 GB free"},
        ],
        "variants": [{"description": "program at 2560×1440",
                      "change": {"recording": {"program": {"height": 1440, "width": 2560}}},
                      "streams": [{"source": "program", "width": 2560, "height": 1440, "fps": 60}],
                      "realtime_factor": 1.73, "bytes_per_second": 11575378.25, "capacity": "fits",
                      "feasible": True}],
        "recommended": {"codec": "hevc", "quality": "high", "container": "mov", "bit_depth": 8},
        "recommendation": "program at 2560×1440 (1.73× real time)",
        "change": {"recording": {"program": {"height": 1440, "width": 2560}}},
        "notes": ["this Mac keeps up with the setup with no margin: 1.11× real time on hard content, so "
                  "anything else using the encoders (a stream, a call, an export) may cost frames",
                  "the closest change that fits: program at 2560×1440 (1.73× real time)"],
    },
    "notes": [],
}

PRINT_LOCK = threading.Lock()
CHILDREN = set()


def say(*parts):
    with PRINT_LOCK:
        print(*parts, flush=True)


# ---------------------------------------------------------------- sizes


class Size:
    def __init__(self, w, h, scale, sweep=False):
        self.w, self.h, self.scale, self.sweep = w, h, scale, sweep

    @property
    def label(self):
        return f"{self.w}x{self.h}@{self.scale:g}"

    @staticmethod
    def parse(text, sweep=False):
        m = re.fullmatch(r"(\d+)x(\d+)@(\d+(?:\.\d+)?)", text.strip())
        if not m:
            raise SystemExit(f"layout-audit: size {text!r} isn't WxH@SCALE (e.g. 1440x900@2)")
        return Size(int(m[1]), int(m[2]), float(m[3]), sweep)


def sweep_sizes(spec):
    if not spec or spec == "none":
        return []
    m = re.fullmatch(r"(\d+)-(\d+):(\d+)@(\d+)x(\d+(?:\.\d+)?)", spec)
    if not m:
        raise SystemExit(f"layout-audit: --sweep {spec!r} isn't FROM-TO:STEP@HEIGHTxSCALE (e.g. {SWEEP})")
    lo, hi, step, h, scale = int(m[1]), int(m[2]), int(m[3]), int(m[4]), float(m[5])
    if step <= 0 or hi < lo:
        raise SystemExit("layout-audit: --sweep needs FROM <= TO and STEP > 0")
    return [Size(w, h, scale, sweep=True) for w in range(lo, hi + 1, step)]


def pick_views(patterns):
    if not patterns:
        return list(VIEW_NAMES)
    chosen = []
    for pat in patterns:
        hit = [n for n in VIEW_NAMES if fnmatch.fnmatchcase(n, pat)]
        if not hit:
            raise SystemExit(f"layout-audit: no screen matches {pat!r}; screens: {', '.join(VIEW_NAMES)}")
        chosen += [n for n in hit if n not in chosen]
    return chosen


def split_list(values):
    out = []
    for v in values or []:
        out += [x for x in re.split(r"[,\s]+", v) if x]
    return out


def image_id(view, size):
    return f"{view}-{size.label}"


# ---------------------------------------------------------------- PNG helpers


def png_size(path):
    with open(path, "rb") as f:
        head = f.read(24)
    if head[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError(f"{path} is not a PNG")
    return struct.unpack(">II", head[16:24])


def write_png(path, w, h, rgb):
    """A flat-colour PNG (calibration pictures)."""
    row = b"\x00" + bytes(rgb) * w
    raw = row * h

    def chunk(kind, data):
        c = struct.pack(">I", len(data)) + kind + data
        return c + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)

    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n")
        f.write(chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)))
        f.write(chunk(b"IDAT", zlib.compress(raw, 6)))
        f.write(chunk(b"IEND", b""))


def downscale(src, dst):
    """The checker's picture: the long side at most CHECK_EDGE (sips)."""
    w, h = png_size(src)
    if max(w, h) <= CHECK_EDGE:
        shutil.copyfile(src, dst)
    else:
        run([("sips", "-Z", str(CHECK_EDGE), src, "--out", dst)], quiet=True)
    return png_size(dst)


# ---------------------------------------------------------------- processes


def run(cmds, env=None, cwd=None, timeout=120, quiet=False, log=None):
    """Run each command in turn; raise with the tail of the output on failure."""
    for cmd in cmds:
        proc = subprocess.Popen(cmd, env=env, cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                text=True, start_new_session=True)
        CHILDREN.add(proc)
        try:
            out, _ = proc.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL)
            out, _ = proc.communicate()
            raise RuntimeError(f"{os.path.basename(cmd[0])} timed out after {timeout} s: {out[-400:]}")
        finally:
            CHILDREN.discard(proc)
        if log:
            log.write(f"$ {' '.join(cmd)}\n{out}\n")
        if proc.returncode != 0:
            raise RuntimeError(f"{' '.join(os.path.basename(c) if i == 0 else c for i, c in enumerate(cmd))} "
                               f"failed ({proc.returncode}): {out.strip()[-600:]}")
    return out


def pooled(n, job, items):
    """Run job over items, n at once. Interrupted (Ctrl-C), the queued jobs
    are dropped and the running ones' processes killed, so each job's own
    cleanup (engine stop, folder removal) runs before this returns."""
    pool = cf.ThreadPoolExecutor(n)
    try:
        list(pool.map(job, items))
    except BaseException:
        pool.shutdown(wait=False, cancel_futures=True)
        kill_children()
        raise
    finally:
        pool.shutdown(wait=True)


def kill_children(*_):
    for proc in list(CHILDREN):
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except OSError:
            pass


# ---------------------------------------------------------------- engines


class Engine:
    """A throwaway engine in a scratch folder (like scripts/snapshot.sh)."""

    def __init__(self, bin_dir, root, seed=None):
        self.bin = bin_dir
        # Short: the socket path must fit in 104 bytes.
        self.dir = tempfile.mkdtemp(prefix="la-", dir=root)
        self.pid = None
        self.env = dict(os.environ)
        for k in list(self.env):
            if k.startswith("ISOCAST_"):
                del self.env[k]
        self.env.update({
            "ISOCAST_CALIBRATION": "off",
            "ISOCAST_SOCKET": os.path.join(self.dir, "engine.sock"),
            # A dev app that doesn't exist: the CLI starts the isocastd beside it.
            "ISOCAST_DEV_APP": os.path.join(self.dir, "none.app"),
            "ISOCAST_UI_PREFS": os.path.join(self.dir, "ui-prefs.json"),
            "ISOCAST_SNAPSHOT_STILL": "1",
            "TZ": "UTC", "LANG": "en_US.UTF-8", "LC_ALL": "en_US.UTF-8",
        })
        self.seed = seed

    def cli(self, *args, timeout=60, log=None):
        return run([(os.path.join(self.bin, "isocast"),) + args], env=self.env, cwd=self.dir,
                   timeout=timeout, log=log)

    def start(self, log=None):
        for name in ("offscreen.toml", "landscape.jpg", "brb.jpg"):
            shutil.copyfile(os.path.join(SHOTS_SRC, name), os.path.join(self.dir, name))
        toml = os.path.join(self.dir, "offscreen.toml")
        with open(toml) as f:
            text = f.read()
        text = text.replace('folder = "recordings"', f'folder = "{self.dir}/recordings"')
        with open(toml, "w") as f:
            f.write(text)
        if self.seed:
            # The seed engine's live setup and timelines (its recordings stay
            # where they are, in the seed's folder, for every copy).
            for name in os.listdir(self.seed):
                if name.startswith("engine.") and not name.endswith((".sock", ".lock", ".log")):
                    src = os.path.join(self.seed, name)
                    dst = os.path.join(self.dir, name)
                    (shutil.copytree if os.path.isdir(src) else shutil.copyfile)(src, dst)
        else:
            self.cli("apply", "--revision", "0", toml, log=log)
        self.cli("engine", "start", log=log)
        try:
            status = json.loads(self.cli("status", "--json"))
            self.pid = (status.get("daemon") or {}).get("pid")
        except (ValueError, RuntimeError):
            self.pid = None
        if not self.seed:
            # Not `preview intro` as visual-shots.sh does: a preview scene
            # turns Studio Mode on, and "main" is the window without it.
            self.cli("scene", "main", "--cut", log=log)

    def stop(self, keep=False):
        try:
            self.cli("engine", "stop", timeout=20)
        except RuntimeError:
            pass
        if self.pid:
            try:
                os.kill(int(self.pid), signal.SIGKILL)
            except (OSError, ValueError):
                pass
        if not keep:
            shutil.rmtree(self.dir, ignore_errors=True)


def make_fixtures(out):
    fx = os.path.join(out, "fixtures")
    os.makedirs(os.path.join(fx, "obs", "basic", "scenes"), exist_ok=True)
    os.makedirs(os.path.join(fx, "obs", "basic", "profiles", "Recording"), exist_ok=True)
    for name in ("Recording", "Streaming"):
        coll = dict(OBS_COLLECTION, name=name)
        with open(os.path.join(fx, "obs", "basic", "scenes", f"{name}.json"), "w") as f:
            json.dump(coll, f)
    with open(os.path.join(fx, "obs", "basic", "profiles", "Recording", "basic.ini"), "w") as f:
        f.write(OBS_PROFILE)
    with open(os.path.join(fx, "obs", "user.ini"), "w") as f:
        f.write("[Basic]\nProfile=Recording\nProfileDir=Recording\nSceneCollection=Recording\n"
                "SceneCollectionFile=Recording\n")
    optimized = dict(OPTIMIZED)
    optimized["setup"] = {
        "version": 1, "name": "Weekly Stream", "canvas": {"width": 3840, "height": 2160, "fps": 60},
        "sources": [{"id": "camera", "name": "Camera", "kind": "test_pattern"}],
        "scenes": [{"id": "main", "name": "Main", "items": [
            {"source": "camera", "rect": {"x": 0, "y": 0, "width": 1920, "height": 1080}}]}],
        "transition": {"kind": "fade", "duration_ms": 300},
        "recording": {
            "folder": "/Users/me/Movies/Isocast",
            "program": {"enabled": True, "filename": "{date} {time}", "audio": [], "width": 2560, "height": 1440,
                        "encoder": {"codec": "hevc", "quality": "high", "container": "mov", "bit_depth": 8}},
            "iso": {"enabled": True, "sources": ["camera"], "filename": "{SOURCE}-{date} {time}", "audio": [],
                    "encoder": {"codec": "hevc", "quality": "high", "container": "mov", "bit_depth": 8}},
            "keep_free_gb": 1.0,
        },
    }
    with open(os.path.join(fx, "optimized.json"), "w") as f:
        json.dump(optimized, f)
    return {"@OBS@": os.path.join(fx, "obs"), "@OPTIMIZED@": os.path.join(fx, "optimized.json")}


def make_editor_seed(bin_dir, root, log):
    """An engine with three short recorded takes on its current timeline."""
    eng = Engine(bin_dir, root)
    eng.start(log=log)
    try:
        eng.cli("timeline", "new", "Episode 12", log=log)
    except RuntimeError as e:
        log.write(f"(timeline new: {e})\n")
    for _ in range(3):
        eng.cli("start", timeout=60, log=log)
        time.sleep(2.5)
        eng.cli("stop", timeout=60, log=log)
    log.write(eng.cli("timeline", "show"))
    eng.stop(keep=True)
    return eng.dir


# ---------------------------------------------------------------- render


def render_one(args, view, size, out_png, fixtures, seed, logs_dir):
    name, states, _, opts = VIEWS[VIEW_NAMES.index(view)]
    states = [replace_all(s, fixtures) for s in states]
    log_path = os.path.join(logs_dir, os.path.basename(out_png)[:-4] + ".log")
    started = time.time()
    with open(log_path, "w") as log:
        eng = Engine(args.bin, args.engines, seed=seed if opts.get("seed") == "editor" else None)
        try:
            for k, v in (opts.get("env") or {}).items():
                eng.env[k] = replace_all(v, fixtures)
            eng.start(log=log)
            tmp = out_png + ".part.png"
            cmd = [os.path.join(args.bin, "isocast-ui"), "--snapshot", tmp, "--size", f"{size.w}x{size.h}",
                   "--scale", f"{size.scale:g}", "--wait-ms", "800"]
            for s in states:
                cmd += ["--state", s]
            run([cmd], env=eng.env, cwd=eng.dir, timeout=args.render_timeout, log=log)
            pw, ph = png_size(tmp)
            os.replace(tmp, out_png)
            return {"ok": True, "seconds": round(time.time() - started, 1), "png_w": pw, "png_h": ph}
        except Exception as e:  # noqa: BLE001 - reported per picture
            log.write(f"FAILED: {e}\n")
            return {"ok": False, "error": str(e)[-500:], "seconds": round(time.time() - started, 1)}
        finally:
            eng.stop()


def replace_all(text, mapping):
    for k, v in mapping.items():
        text = text.replace(k, v)
    return text


def plan(args):
    views = pick_views(split_list(args.views))
    sizes = [Size.parse(s) for s in (split_list(args.sizes) or SIZES)]
    sweep = sweep_sizes(args.sweep)
    sweep_views = pick_views(split_list(args.sweep_views) or SWEEP_VIEWS) if sweep else []
    items = []
    for v in views:
        for s in sizes:
            items.append((v, s))
        if v in sweep_views:
            have = {s.label for s in sizes}
            items += [(v, s) for s in sweep if s.label not in have]
    return items


def load_manifest(out):
    path = os.path.join(out, "manifest.json")
    if os.path.exists(path):
        with open(path) as f:
            return json.load(f)
    return {"images": {}}


def save_manifest(out, manifest):
    path = os.path.join(out, "manifest.json")
    with open(path + ".tmp", "w") as f:
        json.dump(manifest, f, indent=1)
    os.replace(path + ".tmp", path)


def cmd_render(args, manifest, items=None):
    items = items if items is not None else plan(args)
    shots = os.path.join(args.out, "shots")
    logs = os.path.join(args.out, "logs", "render")
    os.makedirs(shots, exist_ok=True)
    os.makedirs(logs, exist_ok=True)
    fixtures = make_fixtures(args.out)
    seed = None
    if any(VIEWS[VIEW_NAMES.index(v)][3].get("seed") == "editor" for v, _ in items):
        say("render: recording three takes for the editor's seed engine…")
        with open(os.path.join(logs, "editor-seed.log"), "w") as log:
            seed = make_editor_seed(args.bin, args.engines, log)
    started = time.time()
    lock = threading.Lock()
    done = [0]

    def job(item):
        view, size = item
        iid = image_id(view, size)
        png = os.path.join(shots, iid + ".png")
        res = render_one(args, view, size, png, fixtures, seed, logs)
        with lock:
            entry = manifest["images"].setdefault(iid, {})
            entry.update(id=iid, view=view, size=size.label, w=size.w, h=size.h, scale=size.scale,
                         sweep=size.sweep, png=os.path.relpath(png, args.out) if res["ok"] else None,
                         png_w=res.get("png_w"), png_h=res.get("png_h"), render=res)
            entry.pop("check", None)
            done[0] += 1
            save_manifest(args.out, manifest)
        say(f"render [{done[0]}/{len(items)}] {iid}: {'ok' if res['ok'] else 'FAILED ' + res['error'][:160]} "
            f"({res['seconds']} s)")

    try:
        pooled(args.jobs, job, items)
    finally:
        if seed:
            shutil.rmtree(seed, ignore_errors=True)
    manifest.setdefault("timing", {})["render_seconds"] = round(time.time() - started, 1)
    save_manifest(args.out, manifest)


# ---------------------------------------------------------------- check


def validate(v, width, height):
    """The answer, checked as strictly as the tool checks it, or an error."""
    if not isinstance(v, dict) or set(v) != {"verdict", "issues"}:
        return None, "fields must be exactly verdict, issues"
    if v["verdict"] not in ("ok", "issues"):
        return None, "bad verdict"
    issues = v["issues"]
    if not isinstance(issues, list) or len(issues) > MAX_ISSUES:
        return None, "bad issues"
    for i in issues:
        if not isinstance(i, dict) or set(i) != {"kind", "severity", "box", "element", "detail"}:
            return None, "bad issue fields"
        if i["kind"] not in KINDS or i["severity"] not in SEVERITIES:
            return None, "bad issue kind or severity"
        b = i["box"]
        if (not isinstance(b, list) or len(b) != 4 or not all(isinstance(n, (int, float)) for n in b)
                or b[2] < 1 or b[3] < 1 or b[0] < 0 or b[1] < 0 or b[0] + b[2] > width + 2
                or b[1] + b[3] > height + 2):
            return None, f"bad box {b}"
        for field, cap in (("element", 100), ("detail", 400)):
            if not isinstance(i[field], str) or not i[field].strip() or len(i[field]) > cap:
                return None, f"bad issue {field}"
    if (v["verdict"] == "ok") != (not issues):
        return None, "verdict and issues disagree"
    return v, None


def usage_of(jsonl):
    total = {"input": 0, "output": 0, "reasoning": 0, "cache_read": 0, "cache_write": 0, "cost": 0.0, "steps": 0}
    tools = []
    for line in jsonl.splitlines():
        try:
            event = json.loads(line)
        except ValueError:
            continue
        part = event.get("part") or {}
        if part.get("type") == "step-finish":
            t = part.get("tokens") or {}
            for k in ("input", "output", "reasoning"):
                total[k] += t.get(k, 0)
            total["cache_read"] += (t.get("cache") or {}).get("read", 0)
            total["cache_write"] += (t.get("cache") or {}).get("write", 0)
            total["cost"] += part.get("cost") or 0.0
            total["steps"] += 1
        elif part.get("type") == "tool":
            tools.append(part.get("tool"))
    total["cost"] = round(total["cost"], 5)
    return total, tools


def prompt_for(entry, cw, ch):
    view = entry["view"]
    desc = VIEWS[VIEW_NAMES.index(view)][2] if view in VIEW_NAMES else entry.get("describe", "")
    s = entry["scale"]
    # The picture's own size: a projector window keeps its own shape.
    fw, fh = entry.get("png_w") or int(entry["w"] * s), entry.get("png_h") or int(entry["h"] * s)
    w, h = round(fw / s), round(fh / s)
    return (
        f"Screen: {view}. Drawn as a {w}x{h} point window at {s:g}x scale "
        f"({fw}x{fh} pixels), attached downscaled to {cw}x{ch} pixels: give boxes in "
        f"these {cw}x{ch} pixels. It should show {desc}. Check its layout and call deliver_layout once."
    )


def auth_env(env, xdg):
    """With OPENCODE_API_KEY, a data folder of the run's own; without it,
    this Mac's OpenCode data (its console sign-in lives in OpenCode's
    database, so it can't be linked in on its own). Config stays isolated
    either way: that is what decides the agent, tools and plugins."""
    if os.environ.get("OPENCODE_API_KEY"):
        env["OPENCODE_API_KEY"] = os.environ["OPENCODE_API_KEY"]
        for kind in ("DATA", "STATE"):
            env[f"XDG_{kind}_HOME"] = os.path.join(xdg, kind.lower())
    else:
        for kind in ("DATA", "STATE", "CACHE"):
            if os.environ.get(f"XDG_{kind}_HOME"):
                env[f"XDG_{kind}_HOME"] = os.environ[f"XDG_{kind}_HOME"]


def remove_job(job):
    inp = os.path.join(job, "input")
    if os.path.isdir(inp):
        os.chmod(inp, 0o755)
    shutil.rmtree(job, ignore_errors=True)


def forget_sessions(args, stdout, run_dir, env):
    """Delete the checker's OpenCode session(s), so a run of hundreds of
    checks doesn't fill the user's session list (without OPENCODE_API_KEY
    the checks share the user's OpenCode data)."""
    ids = set(re.findall(r'"sessionI[Dd]"\s*:\s*"(ses_[A-Za-z0-9]+)"', stdout))
    for sid in ids:
        subprocess.run([args.opencode, "session", "delete", sid], cwd=run_dir, env=env,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=60)


def check_one(args, entry, work):
    iid = entry["id"]
    job = os.path.join(work, re.sub(r"[^A-Za-z0-9._-]", "_", iid))
    remove_job(job)
    run_dir, inp, xdg = (os.path.join(job, d) for d in ("run", "input", "xdg"))
    shutil.copytree(CHECKER, run_dir, symlinks=False)
    os.makedirs(os.path.join(xdg, "config", "opencode"))
    with open(os.path.join(xdg, "config", "opencode", "opencode.json"), "w") as f:
        json.dump({"$schema": "https://opencode.ai/config.json", "update": "disable"}, f)
    os.makedirs(inp)
    picture = os.path.join(inp, "screen.png")
    cw, ch = downscale(os.path.join(args.out, entry["png"]), picture)
    os.chmod(picture, 0o444)
    os.chmod(inp, 0o555)
    agent = os.path.join(run_dir, ".opencode", "agents", "checker.md")
    with open(agent) as f:
        text = f.read()
    with open(agent, "w") as f:
        f.write(text.replace("@@INPUT@@", inp))
    out = os.path.join(job, "answer.json")
    tools_log = os.path.join(job, "tools.log")
    env = {
        "PATH": os.environ.get("PATH", ""),
        "HOME": os.environ.get("HOME", job),
        "LANG": "en_US.UTF-8",
        "XDG_CONFIG_HOME": os.path.join(xdg, "config"),
        "LAYOUT_AUDIT_OUT": out,
        "LAYOUT_AUDIT_WIDTH": str(cw),
        "LAYOUT_AUDIT_HEIGHT": str(ch),
        "LAYOUT_AUDIT_TOOLS_LOG": tools_log,
    }
    auth_env(env, xdg)
    cmd = [args.opencode, "run", "--standalone", "--agent", "checker", "-m", args.model, "--format", "json",
           "--file", picture, prompt_for(entry, cw, ch)]
    result = {"status": "check failed", "attempts": 0, "model": args.model, "check_png": None,
              "check_w": cw, "check_h": ch}
    usage_total = None
    started = time.time()
    for attempt in (1, 2):
        result["attempts"] = attempt
        if os.path.exists(out):
            os.remove(out)
        proc = subprocess.Popen(cmd, cwd=run_dir, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                text=True, start_new_session=True)
        CHILDREN.add(proc)
        try:
            stdout, stderr = proc.communicate(timeout=args.check_timeout)
            code = proc.returncode
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL)
            stdout, stderr = proc.communicate()
            code = "timeout"
        finally:
            CHILDREN.discard(proc)
        with open(os.path.join(job, f"run-{attempt}.jsonl"), "w") as f:
            f.write(stdout or "")
        if not args.keep_sessions:
            forget_sessions(args, stdout or "", run_dir, env)
        with open(os.path.join(job, f"run-{attempt}.err"), "w") as f:
            f.write(stderr or "")
        usage, tools = usage_of(stdout or "")
        if usage_total is None:
            usage_total = usage
        else:
            for k in usage_total:
                usage_total[k] = round(usage_total[k] + usage[k], 5)
        result["tools_called"] = tools
        answer, why = None, f"no deliver_layout call (opencode exit {code}): {(stderr or '').strip()[-200:]}"
        if os.path.exists(out):
            try:
                with open(out) as f:
                    answer, why = validate(json.load(f), cw, ch)
            except ValueError:
                answer, why = None, "the answer file is not JSON"
        if answer:
            result.update(status="ok", answer=answer)
            result.pop("error", None)
            break
        result["error"] = why
    result["usage"] = usage_total
    result["seconds"] = round(time.time() - started, 1)
    # Keep the checker's picture beside the report, for triage.
    keep = os.path.join(args.out, "checked", iid + ".png")
    os.makedirs(os.path.dirname(keep), exist_ok=True)
    shutil.copyfile(picture, keep)
    result["check_png"] = os.path.relpath(keep, args.out)
    try:
        with open(tools_log) as f:
            result["tools_offered"] = sorted({t for line in f for t in json.loads(line)})
    except (OSError, ValueError):
        result["tools_offered"] = []
    if not args.keep_work:
        remove_job(job)
    return result


def cmd_check(args, manifest):
    todo = [e for e in manifest["images"].values() if e.get("png")]
    if args.only:
        only = set(split_list(args.only))
        todo = [e for e in todo if e["id"] in only or any(fnmatch.fnmatchcase(e["id"], p) for p in only)]
    elif args.views or args.sizes:
        wanted = {image_id(v, s) for v, s in plan(args)}
        todo = [e for e in todo if e["id"] in wanted]
    if not args.recheck:
        todo = [e for e in todo if (e.get("check") or {}).get("status") != "ok"]
    work = os.path.join(args.out, "work")
    os.makedirs(work, exist_ok=True)
    lock = threading.Lock()
    done = [0]
    started = time.time()

    def job(entry):
        try:
            res = check_one(args, entry, work)
        except Exception as e:  # noqa: BLE001 - recorded as a failed check
            res = {"status": "check failed", "attempts": 0, "error": f"coordinator: {e}", "seconds": 0}
        with lock:
            manifest["images"][entry["id"]]["check"] = res
            done[0] += 1
            save_manifest(args.out, manifest)
        a = res.get("answer") or {}
        say(f"check [{done[0]}/{len(todo)}] {entry['id']}: "
            f"{a.get('verdict', 'CHECK FAILED ' + str(res.get('error'))[:160])} "
            f"{len(a.get('issues', []))} issues ({res['seconds']} s, ${(res.get('usage') or {}).get('cost', 0)})")

    pooled(args.parallel, job, todo)
    t = manifest.setdefault("timing", {})
    t["check_seconds"] = round(time.time() - started, 1)
    t["check_runs_last"] = sum((manifest["images"][e["id"]]["check"] or {}).get("attempts", 0) for e in todo)
    t["check_runs_total"] = t.get("check_runs_total", 0) + t["check_runs_last"]
    save_manifest(args.out, manifest)


# ---------------------------------------------------------------- report


def words(text):
    return set(re.findall(r"[a-z0-9]+", text.lower())) - {"the", "a", "an", "of", "in", "on", "and", "at", "to"}


def merge_issues(entries):
    """Issues of one screen across sizes, merged when they look like the same
    problem (same kind, and similar words or an overlapping place)."""
    clusters = []
    for e in entries:
        a = (e.get("check") or {}).get("answer") or {}
        cw, ch = e["check"]["check_w"], e["check"]["check_h"]
        for n, i in enumerate(a.get("issues", [])):
            x, y, w, h = i["box"]
            norm = (x / cw, y / ch, (x + w) / cw, (y + h) / ch)
            ws = words(i["element"])
            best = None
            for c in clusters:
                if c["kind"] != i["kind"]:
                    continue
                inter = len(ws & c["words"]) / max(1, len(ws | c["words"]))
                if inter >= 0.5 or (inter >= 0.25 and overlaps(norm, c["norm"])):
                    best = c
                    break
            ref = {"image": e["id"], "index": n, "size": e["size"], "w": e["w"], "severity": i["severity"]}
            if best:
                best["refs"].append(ref)
                best["words"] |= ws
                if SEVERITIES.index(i["severity"]) > SEVERITIES.index(best["severity"]):
                    best["severity"] = i["severity"]
            else:
                clusters.append({"kind": i["kind"], "element": i["element"], "detail": i["detail"],
                                 "severity": i["severity"], "words": ws, "norm": norm, "refs": [ref]})
    for c in clusters:
        c.pop("words")
        c.pop("norm")
    clusters.sort(key=lambda c: (-SEVERITIES.index(c["severity"]), -len(c["refs"])))
    return clusters


def overlaps(a, b):
    ix = max(0, min(a[2], b[2]) - max(a[0], b[0]))
    iy = max(0, min(a[3], b[3]) - max(a[1], b[1]))
    inter = ix * iy
    union = (a[2] - a[0]) * (a[3] - a[1]) + (b[2] - b[0]) * (b[3] - b[1]) - inter
    return union > 0 and inter / union >= 0.15


def where_seen(cluster, entries):
    """'at widths <= 1280', 'at widths >= 2560', 'at all sizes', or a list."""
    checked = [e for e in entries if (e.get("check") or {}).get("status") == "ok"]
    seen = {r["image"] for r in cluster["refs"]}
    if len(seen) == len(checked) and len(checked) > 1:
        return "at every size checked"
    widths = sorted({e["w"] for e in checked if e["id"] in seen})
    if widths:
        lo, hi = widths[0], widths[-1]
        below = [e for e in checked if e["w"] <= hi]
        above = [e for e in checked if e["w"] >= lo]
        if len(seen) > 1 and all(e["id"] in seen for e in below):
            return f"at widths <= {hi} ({len(seen)} sizes)"
        if len(seen) > 1 and all(e["id"] in seen for e in above):
            return f"at widths >= {lo} ({len(seen)} sizes)"
    return "at " + ", ".join(sorted(seen, key=lambda s: s))


def across_screens(views):
    """Issues that recur on several screens (the same dock drawn behind a
    dialog, say): one line each, with the screens they were seen on."""
    groups = []
    for v in views:
        for c in v["clusters"]:
            ws = words(c["element"])
            for g in groups:
                if g["kind"] == c["kind"] and len(ws & g["words"]) / max(1, len(ws | g["words"])) >= 0.5:
                    break
            else:
                g = {"kind": c["kind"], "element": c["element"], "severity": c["severity"], "words": ws,
                     "screens": [], "sightings": 0}
                groups.append(g)
            if v["view"] not in g["screens"]:
                g["screens"].append(v["view"])
            g["sightings"] += len(c["refs"])
            if SEVERITIES.index(c["severity"]) > SEVERITIES.index(g["severity"]):
                g["severity"] = c["severity"]
    out = [dict((k, v) for k, v in g.items() if k != "words") for g in groups if len(g["screens"]) > 1]
    out.sort(key=lambda g: (-len(g["screens"]), -SEVERITIES.index(g["severity"])))
    return out


def build_report(args, manifest):
    images = sorted(manifest["images"].values(), key=lambda e: (
        VIEW_NAMES.index(e["view"]) if e["view"] in VIEW_NAMES else 999, e.get("sweep", False), e["w"], e["h"]))
    by_view = {}
    for e in images:
        by_view.setdefault(e["view"], []).append(e)
    summary = {"images": len(images), "rendered": 0, "render_failed": 0, "checked": 0, "check_failed": 0,
               "ok": 0, "with_issues": 0, "issues": 0, "high": 0, "medium": 0, "low": 0}
    usage = {"cost": 0.0, "input": 0, "output": 0, "runs": 0}
    views = []
    for view, entries in by_view.items():
        for e in entries:
            if e.get("png"):
                summary["rendered"] += 1
            else:
                summary["render_failed"] += 1
            c = e.get("check")
            if not c:
                continue
            u = c.get("usage") or {}
            usage["cost"] += u.get("cost", 0)
            usage["input"] += u.get("input", 0)
            usage["output"] += u.get("output", 0)
            usage["runs"] += c.get("attempts", 0)
            if c["status"] != "ok":
                summary["check_failed"] += 1
                continue
            summary["checked"] += 1
            a = c["answer"]
            summary["ok" if a["verdict"] == "ok" else "with_issues"] += 1
            for i in a["issues"]:
                summary["issues"] += 1
                summary[i["severity"]] += 1
                # The box in the full picture's pixels too.
                fw = e.get("png_w") or int(e["w"] * e["scale"])
                fh = e.get("png_h") or int(e["h"] * e["scale"])
                k = fw / c["check_w"]
                i["box_full"] = [round(i["box"][0] * k), round(i["box"][1] * k),
                                 round(i["box"][2] * k), round(i["box"][3] * (fh / c["check_h"]))]
        clusters = merge_issues([e for e in entries if (e.get("check") or {}).get("status") == "ok"])
        for cl in clusters:
            cl["seen"] = where_seen(cl, entries)
        views.append({"view": view, "images": entries, "clusters": clusters})
    usage["cost"] = round(usage["cost"], 4)
    across = across_screens(views)
    report = {"generated": time.strftime("%Y-%m-%d %H:%M:%S"), "bin": manifest.get("bin"),
              "model": manifest.get("model"), "summary": summary, "usage": usage,
              "timing": manifest.get("timing", {}), "across_screens": across, "views": views}
    with open(os.path.join(args.out, "report.json"), "w") as f:
        json.dump(report, f, indent=1)
    write_html(args.out, report)
    write_md(args.out, report)
    return report


CSS = """
body{font:14px/1.45 -apple-system,system-ui,sans-serif;background:#111;color:#ddd;margin:24px}
a{color:#8cf}h1{font-size:20px}h2{font-size:17px;margin-top:36px;border-top:1px solid #333;padding-top:16px}
.sum span{margin-right:16px}.cl{margin:4px 0}.sev-high{color:#ff6b6b}.sev-medium{color:#ffb84d}.sev-low{color:#9fd3ff}
.grid{display:flex;flex-wrap:wrap;gap:16px}.card{background:#1a1a1a;border:1px solid #333;border-radius:6px;padding:8px;width:520px}
.card.issues{border-color:#7a4b00}.card.failed{border-color:#7a0000}.pic{position:relative;line-height:0}
.pic img{width:100%;height:auto;display:block}.box{position:absolute;border:2px solid #ff3b6b;background:#ff3b6b22}
.box b{position:absolute;top:-2px;left:-2px;background:#ff3b6b;color:#fff;font:bold 11px/14px sans-serif;padding:0 4px}
.box.medium{border-color:#ffb84d;background:#ffb84d22}.box.medium b{background:#ffb84d;color:#000}
.box.low{border-color:#4db8ff;background:#4db8ff22}.box.low b{background:#4db8ff;color:#000}
ol{padding-left:20px;margin:6px 0}small{color:#999}
"""


def write_html(out, report):
    s = report["summary"]
    h = [f"<!doctype html><meta charset=utf-8><title>Layout audit</title><style>{CSS}</style>",
         "<h1>Layout audit</h1><p class=sum>",
         f"<span>{s['images']} pictures</span><span>{s['checked']} checked</span>"
         f"<span>{s['ok']} ok</span><span>{s['with_issues']} with issues</span>"
         f"<span class=sev-high>{s['high']} high</span><span class=sev-medium>{s['medium']} medium</span>"
         f"<span class=sev-low>{s['low']} low</span><span>{s['render_failed']} render failed</span>"
         f"<span>{s['check_failed']} check failed</span></p>",
         f"<p><small>{html.escape(report['generated'])} · model {html.escape(str(report.get('model')))} · "
         f"{report['usage']['runs']} checker runs, ${report['usage']['cost']}</small></p>"]
    if report["across_screens"]:
        h.append("<h2>Seen on several screens</h2>")
        for g in report["across_screens"]:
            h.append(f"<div class=cl><b class=sev-{g['severity']}>{g['severity']}</b> <b>{g['kind']}</b> — "
                     f"{html.escape(g['element'])} <small>({len(g['screens'])} screens, {g['sightings']} "
                     f"sightings: {html.escape(', '.join(g['screens']))})</small></div>")
    h.append("<h2>Screens</h2><ul>")
    for v in report["views"]:
        n = sum(len(cl["refs"]) for cl in v["clusters"])
        h.append(f"<li><a href='#{v['view']}'>{v['view']}</a> — {len(v['clusters'])} distinct issues ({n} sightings)</li>")
    h.append("</ul>")
    for v in report["views"]:
        h.append(f"<h2 id='{v['view']}'>{html.escape(v['view'])}</h2>")
        if v["clusters"]:
            h.append("<div>")
            for n, cl in enumerate(v["clusters"], 1):
                h.append(f"<div class=cl><b class=sev-{cl['severity']}>{cl['severity']}</b> "
                         f"<b>{cl['kind']}</b> — {html.escape(cl['element'])}: {html.escape(cl['detail'])} "
                         f"<small>({html.escape(cl['seen'])})</small></div>")
            h.append("</div>")
        else:
            h.append("<p>No issues reported.</p>")
        h.append("<div class=grid>")
        for e in v["images"]:
            c = e.get("check") or {}
            a = c.get("answer") or {}
            cls = "failed" if not e.get("png") or c.get("status") == "check failed" else (
                "issues" if a.get("issues") else "")
            state = ("render failed: " + html.escape((e.get("render") or {}).get("error", ""))[:300]
                     if not e.get("png") else
                     "not checked" if not c else
                     "check failed: " + html.escape(str(c.get("error")))[:300] if c["status"] != "ok" else
                     a["verdict"])
            h.append(f"<div class='card {cls}'><div><b>{html.escape(e['size'])}</b>"
                     f"{' (sweep)' if e.get('sweep') else ''} — {state}</div>")
            if e.get("png"):
                h.append(f"<div class=pic><a href='{html.escape(e['png'])}'><img loading=lazy src='{html.escape(e['png'])}'></a>")
                for n, i in enumerate(a.get("issues", []), 1):
                    x, y, w, hh = i["box"]
                    cw, ch = c["check_w"], c["check_h"]
                    h.append(f"<div class='box {i['severity']}' style='left:{100 * x / cw:.2f}%;top:{100 * y / ch:.2f}%;"
                             f"width:{100 * w / cw:.2f}%;height:{100 * hh / ch:.2f}%'><b>{n}</b></div>")
                h.append("</div>")
            if a.get("issues"):
                h.append("<ol>")
                for i in a["issues"]:
                    h.append(f"<li><span class=sev-{i['severity']}>{i['severity']}</span> {i['kind']} — "
                             f"<b>{html.escape(i['element'])}</b>: {html.escape(i['detail'])} "
                             f"<small>box {i['box_full']} px</small></li>")
                h.append("</ol>")
            h.append("</div>")
        h.append("</div>")
    with open(os.path.join(out, "report.html"), "w") as f:
        f.write("\n".join(h))


def write_md(out, report):
    s = report["summary"]
    lines = ["# Layout audit", "",
             f"{s['images']} pictures, {s['checked']} checked: {s['ok']} ok, {s['with_issues']} with issues "
             f"({s['high']} high, {s['medium']} medium, {s['low']} low issues). Render failed: "
             f"{s['render_failed']}; check failed: {s['check_failed']}. {report['usage']['runs']} checker runs, "
             f"${report['usage']['cost']}. Timing: {report['timing']}.", ""]
    if report["across_screens"]:
        lines += ["## Seen on several screens", ""]
        for g in report["across_screens"]:
            lines.append(f"- **{g['severity']}** {g['kind']} — {g['element']} ({len(g['screens'])} screens, "
                         f"{g['sightings']} sightings: {', '.join(g['screens'])})")
        lines.append("")
    for v in report["views"]:
        if not v["clusters"]:
            continue
        lines.append(f"## {v['view']}")
        for cl in v["clusters"]:
            first = cl["refs"][0]["image"]
            lines.append(f"- **{cl['severity']}** {cl['kind']} — {cl['element']}: {cl['detail']} "
                         f"({cl['seen']}; e.g. shots/{first}.png)")
        lines.append("")
    clean = [v["view"] for v in report["views"] if not v["clusters"]]
    if clean:
        lines += ["## No issues reported", "", ", ".join(clean), ""]
    with open(os.path.join(out, "report.md"), "w") as f:
        f.write("\n".join(lines))


def exit_status(report):
    s = report["summary"]
    if s["high"]:
        return 1
    if s["render_failed"] or s["check_failed"]:
        return 3
    return 0


# ---------------------------------------------------------------- calibrate

CALIBRATION = [
    # (id, view, size, how it's broken)
    ("main", Size(420, 300, 2), "the main window forced far below its minimum size"),
    ("settings-output", Size(460, 320, 2), "Settings forced into a tiny window"),
    ("editor", Size(480, 300, 2), "the editor forced into a tiny window"),
    ("properties-camera", Size(400, 280, 2), "a dialog forced into a tiny window"),
]


def cmd_calibrate(args, manifest):
    items = [(v, s) for v, s, _ in CALIBRATION]
    cmd_render(args, manifest, items)
    # And a blank picture claiming to be the main window.
    blank = os.path.join(args.out, "shots", "main-blank-1440x900@2.png")
    write_png(blank, 2880, 1800, (30, 30, 32))
    manifest["images"]["main-blank-1440x900@2"] = {
        "id": "main-blank-1440x900@2", "view": "main", "size": "1440x900@2", "w": 1440, "h": 900, "scale": 2.0,
        "sweep": False, "png": os.path.relpath(blank, args.out), "render": {"ok": True, "seconds": 0}}
    save_manifest(args.out, manifest)
    args.only, args.recheck = None, True
    cmd_check(args, manifest)
    flagged = 0
    for iid, e in manifest["images"].items():
        a = (e.get("check") or {}).get("answer") or {}
        hit = a.get("verdict") == "issues"
        flagged += hit
        say(f"calibrate {iid}: {'flagged' if hit else 'MISSED'} "
            f"({', '.join(i['kind'] + '/' + i['severity'] for i in a.get('issues', [])) or e.get('check', {}).get('error')})")
    total = len(manifest["images"])
    say(f"calibrate: {flagged}/{total} broken pictures flagged")
    return 0 if flagged == total else 1


# ---------------------------------------------------------------- main


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0],
                                formatter_class=argparse.RawDescriptionHelpFormatter, epilog=__doc__)
    p.add_argument("command", choices=("run", "render", "check", "report", "calibrate", "list"))
    p.add_argument("--bin", help="folder with isocast, isocastd and isocast-ui")
    p.add_argument("--out", help="output folder (shots/, report.*)")
    p.add_argument("--views", action="append", help="screens (names or patterns, comma separated)")
    p.add_argument("--sizes", action="append", help=f"WxH@SCALE, comma separated (default {' '.join(SIZES)})")
    p.add_argument("--sweep", default=SWEEP, help=f"width sweep FROM-TO:STEP@HEIGHTxSCALE or none (default {SWEEP})")
    p.add_argument("--sweep-views", action="append", help=f"screens swept (default {' '.join(SWEEP_VIEWS)})")
    p.add_argument("--jobs", type=int, default=6, help="pictures drawn at once (default 6)")
    p.add_argument("--parallel", type=int, default=12, help="checkers at once (default 12)")
    p.add_argument("--model", default="opencode/claude-haiku-5-5")
    p.add_argument("--opencode", default="opencode")
    p.add_argument("--only", action="append", help="check: only these picture ids (or patterns)")
    p.add_argument("--recheck", action="store_true", help="check: redo pictures already checked")
    p.add_argument("--render-timeout", type=int, default=120)
    p.add_argument("--check-timeout", type=int, default=180)
    p.add_argument("--keep-work", action="store_true", help="keep each checker's run folder (debugging)")
    p.add_argument("--keep-sessions", action="store_true", help="keep each checker's OpenCode session (deleted by default)")
    args = p.parse_args()

    if args.command == "list":
        for name, states, desc, _ in VIEWS:
            print(f"{name:26} {' '.join('--state ' + s for s in states) or '(no state)'}")
        print("\nsizes:", " ".join(SIZES))
        print("sweep:", SWEEP, "for", " ".join(SWEEP_VIEWS))
        return 0
    if not args.out:
        p.error("--out is required")
    args.out = os.path.realpath(args.out)
    os.makedirs(args.out, exist_ok=True)
    manifest = load_manifest(args.out)
    signal.signal(signal.SIGTERM, lambda *_: (kill_children(), sys.exit(143)))
    if args.command in ("run", "render", "calibrate"):
        if not args.bin:
            p.error("--bin is required to render")
        args.bin = os.path.realpath(args.bin)
        for b in ("isocast", "isocastd", "isocast-ui"):
            if not os.access(os.path.join(args.bin, b), os.X_OK):
                p.error(f"--bin {args.bin} has no {b}")
        manifest["bin"] = args.bin
        os.makedirs("/tmp/isocast-la", exist_ok=True)
        args.engines = tempfile.mkdtemp(prefix=f"{os.getpid()}-", dir="/tmp/isocast-la")
    manifest["model"] = args.model
    try:
        if args.command == "calibrate":
            code = cmd_calibrate(args, manifest)
            build_report(args, manifest)
            return code
        if args.command in ("run", "render"):
            cmd_render(args, manifest)
        if args.command in ("run", "check"):
            cmd_check(args, manifest)
        report = build_report(args, manifest)
    except KeyboardInterrupt:
        kill_children()
        raise
    finally:
        if getattr(args, "engines", None):
            shutil.rmtree(args.engines, ignore_errors=True)
    s = report["summary"]
    say(f"report: {os.path.join(args.out, 'report.html')} — {s['checked']} checked, {s['ok']} ok, "
        f"{s['with_issues']} with issues ({s['high']} high), {s['render_failed']} render failed, "
        f"{s['check_failed']} check failed; {report['usage']['runs']} checker runs, ${report['usage']['cost']}")
    return exit_status(report) if args.command != "render" else (3 if s["render_failed"] else 0)


if __name__ == "__main__":
    sys.exit(main())
