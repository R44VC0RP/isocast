#!/usr/bin/env python3
"""The motion harness in a real window, on a test Mac (scripts/motion.sh
--window; docs/motion-testing.md). Moves the real mouse: never on Ryan's Mac.

    scripts/motion/window.py [--suite FILE] [--scenario NAME] [--variant V]
                             [--bin DIR] [--out DIR]

For each scenario × variant:
1. a test engine of its own with scripts/motion/setup.toml, and the app
   (isocast-ui) in a window, in background mode (bottom-right corner);
2. the scenario replayed headless at the window's size (isocast-ui
   --motion, the test picture) for the pointer's path in window points;
3. a second engine recording the app's window (window source, no cursor,
   60 fps), and with the `recording` variant the test engine recording too;
4. that path posted as real macOS mouse events (scripts/motion/pointer.swift)
   at 120 Hz, each event's host time logged;
5. the window recording read frame by frame: where the edge is in the
   pixels against where the pointer was at the frame's host time
   (recordings carry the host clock: manifest `host_time_ns`).

Results: OUT/<scenario>@<variant>/window.json (gap, lag, gain = how far the
edge moves per point the pointer moves, jump-backs, stalls, repeated
frames), the plan's headless result, the recording.
Holds ISOCAST_SCRATCH/.bench.lock while measuring.
"""

import argparse
import bisect
import fcntl
import glob
import json
import os
import signal
import subprocess
import sys
import tempfile
import time
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
SCRATCH = Path(os.environ.get("ISOCAST_SCRATCH", "/Volumes/IsocastBuild"))
TITLE_BAR = 28  # points, a standard macOS title bar
LINE = (0x24, 0x24, 0x24)  # Theme.line: the splitters' hairline
CUA = "/Applications/CuaDriver.app/Contents/MacOS/cua-driver"
SCAN_BELOW = 40  # points: where splitters are looked for, below the pointer


def write(path, text):
    """Write PATH without following a link: refuse a symlink or anything
    but a plain file there, write a new temporary file beside it, then
    rename it over PATH (a rename replaces the name, not a link's target)."""
    path = Path(path)
    if path.is_symlink() or (path.exists() and not path.is_file()):
        sys.exit(f"window.py: {path} is a link or not a plain file; not writing it")
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=".motion-tmp.")
    try:
        os.fchmod(fd, 0o644)
        with os.fdopen(fd, "w") as f:
            f.write(text)
        os.replace(tmp, path)
    except BaseException:
        os.unlink(tmp)
        raise


def new_file(path):
    """A fresh file at PATH for a process to write to (an earlier plain
    file there is replaced; a link or anything else is refused)."""
    path = Path(path)
    if path.is_symlink() or (path.exists() and not path.is_file()):
        sys.exit(f"window.py: {path} is a link or not a plain file; not writing it")
    path.unlink(missing_ok=True)
    return os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o644), "w")


def own_dir(path):
    """PATH as a folder of ours: made if missing, refused if a link."""
    path = Path(path)
    if path.is_symlink() or (path.exists() and not path.is_dir()):
        sys.exit(f"window.py: {path} is a link or not a folder; not writing there")
    path.mkdir(parents=True, exist_ok=True)
    return path


def run(cmd, env, check=True, **kw):
    out = subprocess.run([str(c) for c in cmd], env=env, capture_output=True, text=True, **kw)
    if check and out.returncode != 0:
        sys.exit(f"window.py: {' '.join(map(str, cmd))} failed:\n{out.stderr or out.stdout}")
    return out


def engine_env(work, name):
    return dict(os.environ, ISOCAST_SOCKET=str(work / f"{name}.sock"),
                ISOCAST_SETUP=str(work / f"{name}-live.toml"),
                ISOCAST_TIMELINES=str(work / f"{name}-timelines"), ISOCAST_MONITOR_SILENT="1")


def apply(cli, env, setup):
    rev = json.loads(run([cli, "get", "--json"], env, check=False).stdout or "{}").get("revision", 0)
    run([cli, "apply", "--revision", rev, setup], env)


def owned(folder):
    """isocastd processes (an engine, its log relay) with a file open in
    `folder`: the CLI launches the engine detached, so it isn't our child."""
    out = subprocess.run(["lsof", "-a", "-c", "isocastd", "-t", "+D", str(folder)],
                         capture_output=True, text=True)
    return sorted({int(p) for p in out.stdout.split()})


def shutdown(env):
    """Shut the engine down and make sure it and its relay are gone."""
    sock = env["ISOCAST_SOCKET"]
    folder = Path(sock).parent
    if os.path.exists(sock):
        subprocess.run(["nc", "-U", "-w", "5", sock], input='{"jsonrpc":"2.0","id":1,"method":"shutdown"}\n',
                       capture_output=True, text=True, timeout=10)
    for sig in (None, signal.SIGTERM, signal.SIGKILL):
        end = time.time() + 5
        while owned(folder) and time.time() < end:
            time.sleep(0.2)
        pids = owned(folder)
        if not pids:
            return
        for pid in pids:
            if sig is not None:
                try:
                    os.kill(pid, sig)
                except ProcessLookupError:
                    pass


def windows(owner):
    out = subprocess.run(["swift", str(HERE / "pointer.swift"), "windows", owner],
                         capture_output=True, text=True, timeout=120)
    return json.loads(out.stdout or "[]")


def find_window(timeout=20):
    end = time.time() + timeout
    while time.time() < end:
        # Window titles need Screen Recording; without it the name is
        # empty, and the test app is the only isocast-ui running.
        for owner in ("isocast-ui", "Isocast"):
            found = [w for w in windows(owner) if w["layer"] == 0 and w["h"] > 100
                     and (not w["name"] or "Motion test" in w["name"])]
            if found:
                return max(found, key=lambda w: w["w"] * w["h"])
        time.sleep(1)
    sys.exit("window.py: the app's window (\"Isocast — Motion test\") never showed")


def plan_events(frames, origin, start=1.0):
    """Mouse events for the headless run's pointer path: a click first (the
    app is in the background; the first click only brings it forward),
    then the path at 120 Hz."""
    ox, oy = origin
    to_screen = lambda p: (ox + p[0], oy + p[1])
    first = frames[0]["pointer"]
    x, y = to_screen(first)
    events = [{"t": 0.0, "kind": "move", "x": x, "y": y},
              {"t": 0.2, "kind": "down", "x": x, "y": y},
              {"t": 0.25, "kind": "up", "x": x, "y": y}]
    pressed = False
    for i, f in enumerate(frames):
        t = start + f["t"]
        x, y = to_screen(f["pointer"])
        if f["pressed"] and not pressed:
            events.append({"t": t, "kind": "down", "x": x, "y": y})
        elif f["pressed"]:
            # A point between this frame and the one before: 120 Hz.
            px, py = to_screen(frames[i - 1]["pointer"])
            events.append({"t": t - 1 / 120, "kind": "drag", "x": (x + px) / 2, "y": (y + py) / 2})
            events.append({"t": t, "kind": "drag", "x": x, "y": y})
        elif pressed:
            events.append({"t": t, "kind": "up", "x": x, "y": y})
        pressed = f["pressed"]
    if pressed:
        events.append({"t": start + frames[-1]["t"] + 0.05, "kind": "up", "x": x, "y": y})
    return events


def strips(video, crop, _size=None):
    """The video's frames cropped to `crop` (x, y, w, h in video pixels; even
    numbers, as 4:2:0 video needs), as rows of RGB tuples, with each frame's
    time."""
    lines = run(["ffprobe", "-v", "error", "-select_streams", "v", "-show_entries",
                 "frame=best_effort_timestamp_time", "-of", "csv=p=0", video], os.environ).stdout.split()
    times = [float(f) for f in (line.split(",")[0] for line in lines) if f and f != "N/A"]
    x, y, w, h = crop
    raw = subprocess.run(["ffmpeg", "-v", "error", "-i", str(video), "-vf", f"crop={w}:{h}:{x}:{y}",
                          "-f", "rawvideo", "-pix_fmt", "rgb24", "-"], capture_output=True, check=True).stdout
    n = len(raw) // (w * h * 3)
    frames = []
    for i in range(n):
        base = i * w * h * 3
        frames.append([[tuple(raw[base + (r * w + c) * 3: base + (r * w + c) * 3 + 3]) for c in range(w)]
                       for r in range(h)])
    return times[:n], frames


def near(a, b, tol):
    return all(abs(p - q) <= tol for p, q in zip(a, b))


def find_line(strip, start, lift=5):
    """The hairline nearest `start`: the column of a strip of rows that is
    lighter than the columns two away on both sides in most rows. (By
    contrast, not colour: the window recording's levels differ from the
    theme's, #242424 on #121212 came out #141414 on #080808.)"""
    n = len(strip[0])
    lum = lambda p: sum(p) / 3

    def ok(i):
        if i < 2 or i >= n - 2:
            return False
        return sum(lum(r[i]) - max(lum(r[i - 2]), lum(r[i + 2])) >= lift for r in strip) >= len(strip) - 2
    for d in range(n):
        for i in (start - d, start + d):
            if 0 <= i < n and ok(i):
                return i
    return None


def magenta(c):
    return c[0] > 180 and c[1] < 100 and c[2] > 180


def find_block_edge(row, anchor, step):
    if not (0 <= anchor < len(row)) or not magenta(row[anchor]):
        return None
    i = anchor
    while 0 <= i + step < len(row) and magenta(row[i + step]):
        i += step
    return i + 1 if step > 0 else i


def analyse(video, manifest, file, events, plan, origin_px, scale, axis, target):
    host0 = manifest["host_time_ns"] + file["start_offset_seconds"] * 1e9
    ev = [e for e in events if e["kind"] in ("down", "drag", "up")][2:]  # after the activation click
    if not ev:
        sys.exit("window.py: no drag events were posted")
    ev_t = [e["host_ns"] for e in ev]
    pos = lambda e: e["x"] if axis == 0 else e["y"]

    def pointer_at(ns):
        i = bisect.bisect_right(ev_t, ns) - 1
        if i < 0:
            return None, False
        e = ev[i]
        return pos(e), e["kind"] != "up"

    probe = json.loads(run(["ffprobe", "-v", "error", "-select_streams", "v", "-show_entries",
                            "stream=width,height", "-of", "json", video], os.environ).stdout)["streams"][0]
    vw, vh = probe["width"], probe["height"]
    scan = plan["scan_line"]
    ox, oy = origin_px  # the window's content origin in video pixels
    to_px = lambda points: points * scale
    if target.startswith("splitter"):
        # Below the pointer: the captured cursor would cover the line.
        y = int(oy + to_px(scan + SCAN_BELOW))
        times, fr = strips(video, (0, y - 4, vw, 10), None)
        edges, last = [], None
        start0 = int(ox + to_px(plan_rest_edge(plan)))
        for f in fr:
            i = find_line(f, last if last is not None else start0)
            edges.append(None if i is None else (i - ox) / scale)
            last = i if i is not None else last
    elif target == "row-splitter":
        sys.exit("window.py: row-splitter isn't measured in a real window yet (column splitters and the canvas are)")
    else:  # canvas
        y = int(oy + to_px(scan))
        times, fr = strips(video, (0, y // 2 * 2, vw, 2), None)
        edges = []
        step = 1 if target == "canvas-corner" else -1
        # Inside the block, next to the edge where it last was.
        last = int(ox + to_px(plan_rest_edge(plan)))
        for f in fr:
            e = find_block_edge(f[0], last - step * int(12 * scale), step)
            edges.append(None if e is None else (e - ox) / scale)
            last = e if e is not None else last
    # Window points; the pointer in screen points minus the content origin.
    content = plan["content_origin_points"][axis]
    rows = []
    for t, e in zip(times, edges):
        p, down = pointer_at(host0 + t * 1e9)
        rows.append({"t": t, "edge": e, "pointer": None if p is None else p - content, "pressed": down})
    return rows, {"video": str(video), "frames": len(rows), "size": [vw, vh]}


def analyse_cursor(video, plan, target, scale, origin_px):
    """Each frame's edge and the captured pointer's tip, both from the same
    picture (no clocks to line up): the pointer is the window recording's
    cursor, white in the dark docks or on the picture."""
    probe = json.loads(run(["ffprobe", "-v", "error", "-select_streams", "v", "-show_entries",
                            "stream=width,height", "-of", "json", video], os.environ).stdout)["streams"][0]
    vw = probe["width"]
    ox, oy = origin_px
    grab = plan["grab"]
    py = int(oy + grab[1] * scale)
    # The arrow hangs down and right from its tip.
    times, cursor_strips = strips(video, (0, (py - 2) // 2 * 2, vw, int(22 * scale) // 2 * 2), None)
    if target.startswith("splitter"):
        _, edge_strips = strips(video, (0, int(oy + (plan["scan_line"] + SCAN_BELOW) * scale) - 4, vw, 10), None)
        last = int(ox + plan_rest_edge(plan) * scale)
        edges = []
        for f in edge_strips:
            i = find_line(f, last)
            edges.append(None if i is None else (i - ox) / scale)
            last = i if i is not None else last
    else:
        step = 1 if target == "canvas-corner" else -1
        _, edge_strips = strips(video, (0, int(oy + plan["scan_line"] * scale) // 2 * 2, vw, 2), None)
        last = int(ox + plan_rest_edge(plan) * scale)
        edges = []
        for f in edge_strips:
            e = find_block_edge(f[0], last - step * int(12 * scale), step)
            edges.append(None if e is None else (e - ox) / scale)
            last = e if e is not None else last
    rows = []
    for t, strip, e in zip(times, cursor_strips, edges):
        bright = [c for row in strip for c, px in enumerate(row) if min(px) > 225]
        tip = (min(bright) - ox) / scale if bright else None
        rows.append({"t": t, "edge": e, "cursor": tip})
    return rows, {"video": str(video), "frames": len(rows), "size": [vw, probe["height"]]}


def judge_cursor(rows, commanded, grab):
    """The edge against the captured pointer: how far each moved since the
    pointer reached the grab point, frame by frame. Frames keep their own
    places (a frame where either wasn't found isn't skipped over: lag and
    adjacency are in capture frames), and a frame of the drag where either
    is missing makes the measurement invalid."""
    # The pointer starts where the last run left it, then jumps to the grab.
    start = next((i for i, r in enumerate(rows)
                  if r["cursor"] is not None and r["edge"] is not None and abs(r["cursor"] - grab) < 3), None)
    if start is None:
        return {"valid": False, "error": f"the captured pointer never reached the grab point ({grab})"}
    e0, c0 = rows[start]["edge"], rows[start]["cursor"]
    de = [None if r["edge"] is None else r["edge"] - e0 for r in rows]
    dc = [None if r["cursor"] is None else r["cursor"] - c0 for r in rows]
    moving = [i for i in range(start + 1, len(rows))
              if dc[i] is not None and dc[i - 1] is not None and abs(dc[i] - dc[i - 1]) > 0.5]
    if not moving:
        return {"valid": False, "error": "the captured pointer never moved"}
    first, last = start, moving[-1]
    span = list(range(first, min(len(rows), last + 30)))
    missing = [i for i in span if de[i] is None or dc[i] is None]
    if missing:
        return {"valid": False, "error": f"edge or pointer missing in {len(missing)} of {len(span)} frames of the drag",
                "first_missing_t": rows[missing[0]]["t"]}
    gaps = sorted(abs(de[i] - dc[i]) for i in span)
    pct = lambda v, q: v[min(len(v) - 1, round((len(v) - 1) * q))]
    best, lag = None, 0
    for s in range(0, 20):
        errs = [abs(de[i] - dc[i - s]) for i in span if i - s >= first]
        if len(errs) < len(span) // 2:
            break
        m = sum(errs) / len(errs)
        if best is None or m < best - 0.05:
            best, lag = m, s
    end = last + 29 if last + 29 < len(rows) else len(rows) - 1
    gain = de[end] / dc[end] if dc[end] else None
    # Jumps and stalls are judged against the pointer delayed by the lag
    # when the edge goes about as far as the pointer, else against the
    # pointer itself (a delay fitted to a frozen edge is meaningless there;
    # the lag is still reported and the gain says why).
    used = lag if gain is not None and abs(gain - 1) < 0.2 else 0
    back = stall = 0
    anchor = None
    for i in range(first + 1, last + 1):
        if i - used - 1 < first:
            continue
        e_prev, e_now = dc[i - used - 1], dc[i - used]
        # Where the pointer was when the edge last moved (or when it
        # started standing still): kept until the edge moves again.
        if anchor is None:
            anchor = e_prev
        since = anchor
        got = de[i] - de[i - 1]
        if abs(got) >= 0.5:
            if abs(e_now - e_prev) > 0.5 and got * (1 if e_now > e_prev else -1) < -1.01:
                back += 1
            anchor = e_now
        elif abs(e_now - since) > 1.5:
            stall += 1
    ts = [r["t"] for r in rows]
    return {
        "valid": True,
        "frames_measured": len(span),
        "pointer_moved_pt": round(dc[end], 1),
        "edge_moved_pt": round(de[end], 1),
        "gain": None if gain is None else round(gain, 3),
        "commanded_pt": commanded,
        "gap_pt_median": round(pct(gaps, 0.5), 2),
        "gap_pt_p95": round(pct(gaps, 0.95), 2),
        "gap_pt_max": round(gaps[-1], 2),
        "gap_pt_after_release": round(abs(de[end] - dc[end]), 2),
        "lag_frames": lag,
        "jump_back_frames": back,
        "stall_frames": stall,
        "capture_gaps_over_1_5_frames": sum(1 for a, b in zip(ts[first:end + 1], ts[first + 1:end + 1])
                                            if b - a > 1.5 / 60),
        "note": "pointer = the cursor in the window recording, both in the same captured frame",
    }


def selfcheck():
    """judge_cursor on rows whose answer is known (the window runner's
    controls; scripts/motion/controls.toml has the headless ones). Prints
    each and returns 1 if any came out wrong."""
    def rows(edge_at, n=120, missing=lambda i: False):
        out = []
        for i in range(n):
            c = 100 + (0 if i < 10 else min(120, (i - 10) * 2))
            e = None if missing(i) else edge_at(i)
            out.append({"t": i / 60, "edge": e, "cursor": c})
        return out

    follow = lambda d: lambda i: 50 + (0 if i < 10 + d else min(120, (i - 10 - d) * 2))
    cases = [
        ("follows", rows(follow(0)), lambda v: v["valid"] and v["lag_frames"] == 0
         and v["stall_frames"] == 0 and v["gap_pt_max"] == 0),
        ("4-frame lag", rows(follow(4)), lambda v: v["valid"] and v["lag_frames"] == 4),
        ("frozen edge", rows(lambda i: 50.0), lambda v: v["valid"] and v["gain"] == 0
         and v["stall_frames"] >= 55),
        ("frozen, 1 pt steps", [{"t": i / 60, "edge": 0.0, "cursor": float(i)} for i in range(100)],
         lambda v: v["valid"] and v["stall_frames"] >= 60),
        ("half speed", rows(lambda i: 50 + (0 if i < 10 else min(60, (i - 10)))),
         lambda v: v["valid"] and abs(v["gain"] - 0.5) < 0.01 and v["stall_frames"] == 0),
        ("every other edge missing", rows(follow(0), missing=lambda i: i > 12 and i % 2),
         lambda v: not v["valid"]),
    ]
    bad = 0
    for name, data, ok in cases:
        verdict = judge_cursor(data, 120, data[0]["cursor"])
        good = ok(verdict)
        bad += not good
        print(f"{'ok  ' if good else 'BAD '} {name}: {json.dumps({k: verdict.get(k) for k in ('valid', 'gain', 'lag_frames', 'stall_frames', 'gap_pt_max', 'error')})}")
    return 1 if bad else 0


def plan_rest_edge(plan):
    return plan["rest_edge"]


def judge(rows, fps=60.0):
    pressed = [i for i, r in enumerate(rows) if r["pressed"]]
    missing = [i for i in pressed if rows[i]["edge"] is None or rows[i]["pointer"] is None]
    if missing:
        return {"valid": False, "error": f"edge or pointer missing in {len(missing)} of {len(pressed)} pressed frames"}
    act = pressed
    if len(act) < 10:
        return {"error": f"the edge was found in {len(act)} frames while pressed"}
    first = act[0]
    rest = [r for r in rows[:first] if r["edge"] is not None and r["pointer"] is not None]
    e0 = rest[-1]["edge"] if rest else rows[first]["edge"]
    p0 = rows[first]["pointer"]
    exp = {i: e0 + rows[i]["pointer"] - p0 for i in act}
    gaps = sorted(abs(rows[i]["edge"] - exp[i]) for i in act)
    pct = lambda v, q: v[min(len(v) - 1, round((len(v) - 1) * q))]
    # Gain: how far the edge moved per point the pointer moved (1 = follows).
    xs = [rows[i]["pointer"] - p0 for i in act]
    ys = [rows[i]["edge"] - e0 for i in act]
    sxx = sum(x * x for x in xs)
    gain = sum(x * y for x, y in zip(xs, ys)) / sxx if sxx else None
    # Lag: the shift (frames) that best lines the edge up with the pointer.
    best, lag = None, 0
    for s in range(0, 20):
        errs = [abs(rows[i]["edge"] - exp[i - s]) for i in act if (i - s) in exp]
        if len(errs) < len(act) // 2:
            break
        m = sum(errs) / len(errs)
        if best is None or m < best - 0.05:
            best, lag = m, s
    back = stall = repeated = 0
    for a, b in zip(act, act[1:]):
        if b != a + 1 or a - lag not in exp or b - lag not in exp:
            continue
        want = exp[b - lag] - exp[a - lag]
        got = rows[b]["edge"] - rows[a]["edge"]
        if abs(want) > 0.5 and got * (1 if want > 0 else -1) < -0.5:
            back += 1
        elif abs(want) > 0.5 and abs(got) < 0.25:
            stall += 1
    ts = [r["t"] for r in rows]
    gaps_t = [b - a for a, b in zip(ts, ts[1:])]
    late = sum(1 for g in gaps_t if g > 1.5 / fps)
    return {
        "frames_measured": len(act),
        "gain": None if gain is None else round(gain, 3),
        "gap_px_median": round(pct(gaps, 0.5), 2),
        "gap_px_p95": round(pct(gaps, 0.95), 2),
        "gap_px_max": round(gaps[-1], 2),
        "lag_frames": lag,
        "jump_back_frames": back,
        "stall_frames": stall,
        "capture_gaps_over_1_5_frames": late,
        "note": "lag is from host-time stamps of the posted events and of the captured frames (about ±1 frame)",
    }


def now_ns():
    return time.clock_gettime_ns(time.CLOCK_UPTIME_RAW)


def cua_drag(suite_path, name, frames, origin, work):
    """One straight drag with cua-driver: from the grab point by the
    scenario's first move, over its time, at 120 Hz."""
    suite = tomllib.loads(Path(suite_path).read_text())
    steps = next(s["steps"] for s in suite["scenario"] if s["name"] == name)
    move = next(s for s in steps if "move" in s)
    dx, dy = move["move"]
    ms = int(move.get("ms", 500))
    x, y = origin[0] + frames[0]["pointer"][0], origin[1] + frames[0]["pointer"][1]
    args = {"scope": "desktop", "from_x": x, "from_y": y, "to_x": x + dx, "to_y": y + dy,
            "duration_ms": ms, "steps": max(1, round(ms / 1000 * 120))}
    before = now_ns()
    out = run([CUA, "call", "drag", json.dumps(args)], os.environ, check=False, timeout=60)
    after = now_ns()
    if out.returncode != 0:
        sys.exit(f"window.py: cua-driver drag failed: {out.stderr or out.stdout}")
    write(work / "posted.json", json.dumps({"driver": "cua-driver", "drag": args,
                                                  "host_ns_before": before, "host_ns_after": after,
                                                  "events": []}))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--suite", type=Path, default=HERE / "suite.toml")
    ap.add_argument("--scenario")
    ap.add_argument("--variant")
    ap.add_argument("--bin", type=Path, default=REPO / "target/release")
    ap.add_argument("--out", type=Path, default=SCRATCH / "motion-window" / time.strftime("%Y%m%d-%H%M%S"))
    ap.add_argument("--analyse", type=Path, help="only measure this run folder's recording again")
    ap.add_argument("--selfcheck", action="store_true",
                    help="check the recording analysis on known rows (no window, no test Mac)")
    args = ap.parse_args()
    # SIGTERM unwinds like Ctrl-C, so the engines are shut down.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    if args.analyse:
        print(json.dumps(measure(args.analyse.resolve()), indent=2))
        return
    if args.selfcheck:
        sys.exit(selfcheck())
    cli, ui = args.bin / "isocast", args.bin / "isocast-ui"
    suite = tomllib.loads(args.suite.read_text())
    out = own_dir(args.out.resolve())
    listed = run([ui, "--motion", args.suite, "--list"] + (["--scenario", args.scenario] if args.scenario else [])
                 + (["--variant", args.variant] if args.variant else []), os.environ).stdout.split("\n")
    todo = [l.split() for l in listed if l.strip()]
    targets = {s["name"]: s["target"] for s in suite.get("scenario", [])}
    results = []
    lock = open(SCRATCH / ".bench.lock", "a")
    print("window.py: waiting for the benchmark lock", flush=True)
    fcntl.flock(lock, fcntl.LOCK_EX)
    try:
        for name, variant in todo:
            target = targets[name]
            if not target.startswith("splitter:"):
                # The row splitter isn't tracked in a window recording yet;
                # on the canvas the captured pointer sits on the white
                # corner handle, which the cursor finder would take for it.
                print(f"skip  {name}@{variant}: {target} not measured in a window yet", flush=True)
                continue
            results.append(one(cli, ui, args.suite, name, variant, target, out))
    finally:
        fcntl.flock(lock, fcntl.LOCK_UN)
    write(out / "window-summary.json", json.dumps(results, indent=2))
    print(json.dumps(results, indent=2))


def one(cli, ui, suite, name, variant, target, out):
    run_dir = own_dir(out / f"{name}@{variant}")
    work = own_dir(run_dir / "work")
    test, rec = engine_env(work, "test"), engine_env(work, "rec")
    setup = (HERE / "setup.toml").read_text().replace('folder = "recordings"', f'folder = "{work}/test-rec"')
    write(work / "setup.toml", setup)
    write(work / "rec.toml", f"""version = 1
name = "Motion recorder"

[canvas]
width = 1920
height = 1080
fps = 60

[[sources]]
id = "window"
name = "Window"
kind = "window"
app = "isocast-ui"
title = "Motion test"
cursor = true

[[scenes]]
id = "main"
name = "Main"
items = [{{ source = "window", rect = {{ x = 0, y = 0, width = 1920, height = 1080 }} }}]

[recording]
folder = "{work}/window-rec"

[recording.program]
enabled = false
encoder = {{ codec = "hevc", quality = 70 }}

[recording.iso]
enabled = true
sources = ["window"]
encoder = {{ codec = "hevc", quality = 95 }}
""")
    app = None
    try:
        apply(cli, test, work / "setup.toml")
        run([cli, "engine", "start"], test)
        run([cli, "scene", "main", "--cut"], test)
        app = subprocess.Popen([str(ui), "--no-float"], env=dict(test, ISOCAST_BACKGROUND="1",
                                                       # Its own layout file: a fresh one each run.
                                                       ISOCAST_UI_PREFS=str(work / "ui-prefs.json")),
                               stdout=new_file(work / "app.log"), stderr=subprocess.STDOUT)
        time.sleep(4)
        win = find_window()
        content_w, content_h = int(win["w"]), int(win["h"] - TITLE_BAR)
        origin = (win["x"], win["y"] + TITLE_BAR)
        # The pointer's path, from the same scenario headless at this size.
        plan_out = run_dir / "plan"
        res = run([ui, "--motion", suite, "--scenario", name, "--variant", "idle", "--size",
                   f"{content_w}x{content_h}", "--picture", "test", "--clock", "lockstep", "--no-timing",
                   "--out", plan_out], test, check=False)
        result_path = plan_out / f"{name}@idle" / "result.json"
        if not result_path.exists():
            sys.exit(f"window.py: the headless plan failed:\n{res.stderr[-2000:]}")
        plan = json.loads(result_path.read_text())
        frames = [json.loads(l) for l in (plan_out / f"{name}@idle" / "frames.jsonl").read_text().splitlines()]
        plan["rest_edge"] = frames[0]["model"]
        plan["grab"] = frames[0]["pointer"]
        plan["content_origin_points"] = list(origin)
        apply(cli, test, work / "setup.toml")  # the plan's edits undone
        time.sleep(1)
        events = plan_events(frames, origin)
        write(work / "events.json", json.dumps({"events": events}))
        seconds = int(events[-1]["t"] + 6)
        apply(cli, rec, work / "rec.toml")
        run([cli, "start", "--seconds", seconds], rec)
        if variant == "recording":
            run([cli, "start", "--seconds", seconds], test)
        time.sleep(1)
        replay = run(["swift", HERE / "pointer.swift", "replay", work / "events.json", work / "posted.json"],
                     os.environ, check=False, timeout=180)
        driver = "cgevent"
        if replay.returncode != 0:
            if "may not post events" not in replay.stderr:
                sys.exit(f"window.py: {replay.stderr.strip()}")
            # No Accessibility for this process: cua-driver (which has it)
            # drags instead, one straight drag of the first move.
            driver = "cua-driver"
            cua_drag(suite, name, frames, origin, work)
        time.sleep(2)
        run([cli, "stop"], rec, check=False)
        if variant == "recording":
            run([cli, "stop"], test, check=False)
    finally:
        if app:
            app.terminate()
        shutdown(test)
        shutdown(rec)
    meta = {"name": name, "variant": variant, "target": target, "driver": driver, "win": win,
            "content": [content_w, content_h], "plan": plan}
    write(run_dir / "meta.json", json.dumps(meta))
    return measure(run_dir)


def measure(run_dir):
    """Read a run's window recording (again: `--analyse RUN_DIR`)."""
    meta = json.loads((run_dir / "meta.json").read_text())
    name, variant, target, driver = meta["name"], meta["variant"], meta["target"], meta["driver"]
    win, plan = meta["win"], meta["plan"]
    content_w, content_h = meta["content"]
    work = run_dir / "work"
    manifests = sorted(glob.glob(str(work / "window-rec" / "**" / "*.session.json"), recursive=True))
    if not manifests:
        sys.exit(f"window.py: no window recording in {work}/window-rec")
    manifest = json.loads(Path(manifests[-1]).read_text())
    file = next(f for f in manifest["files"] if f["source"] == "window")
    video = Path(manifests[-1]).parent / file["path"]
    if not video.exists():
        video = Path(file["path"])
    vw = json.loads(run(["ffprobe", "-v", "error", "-select_streams", "v", "-show_entries", "stream=width",
                         "-of", "json", video], os.environ).stdout)["streams"][0]["width"]
    scale = vw / win["w"]
    if driver == "cua-driver":
        rows, info = analyse_cursor(video, plan, target, scale, (0, TITLE_BAR * scale))
        write(run_dir / "window-frames.jsonl", "\n".join(json.dumps(r) for r in rows) + "\n")
        drag = json.loads((work / "posted.json").read_text())["drag"]
        verdict = judge_cursor(rows, drag["to_x"] - drag["from_x"] if target != "row-splitter"
                               else drag["to_y"] - drag["from_y"], plan["grab"][0])
        result = {"id": f"{name}@{variant}", "target": target, "driver": driver,
                  "window_points": [content_w, content_h], "pixels_per_point": round(scale, 3), **info,
                  "real_window": verdict,
                  "headless_same_size": {k: plan.get(k) for k in ("on_screen", "in_state")}}
        write(run_dir / "window.json", json.dumps(result, indent=2))
        print(f"done  {name}@{variant} ({driver}): {json.dumps(verdict)}", flush=True)
        return result
    posted = json.loads((work / "posted.json").read_text())["events"]
    rows, info = analyse(video, manifest, file, posted, plan, (0, TITLE_BAR * scale), scale,
                         1 if target == "row-splitter" else 0, target)
    write(run_dir / "window-frames.jsonl", "\n".join(json.dumps(r) for r in rows) + "\n")
    verdict = judge(rows)
    result = {"id": f"{name}@{variant}", "target": target, "window_points": [content_w, content_h],
              "pixels_per_point": round(scale, 3), **info, "real_window": verdict,
              "headless_same_size": {k: plan.get(k) for k in ("on_screen", "in_state")}}
    write(run_dir / "window.json", json.dumps(result, indent=2))
    print(f"done  {name}@{variant}: {json.dumps(verdict)}", flush=True)
    return result


if __name__ == "__main__":
    main()
