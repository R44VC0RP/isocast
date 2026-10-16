#!/usr/bin/env python3
"""Extreme stress suite (PLAN S.2): push the engine's limits and check it
stays correct. Python stdlib + ffmpeg/ffprobe only.

Build: cargo build --release -p isocast-daemon
Run:   python3 scripts/stress.py [--scenario NAME]… [--profile hardware|quick]
                                 [--out DIR] [--keep]
       python3 scripts/stress.py --summary DIR/report.json   (Markdown)

Each scenario runs in its own worker process with its own isolated engine
(scripts/e2e.py's Engine: a signed background app, socket, setup, plugins,
timelines, logs and recordings under --work; inherited ISOCAST_* variables
dropped; synthetic sources only). The parent bounds each worker and kills
its process group on timeout; a disk image the disk scenario mounted is
detached then too.

Scenarios (hardware profile numbers; --profile quick shrinks them and
records 720p30, for a VM, with drops reported but not failing):
  sources      40 synthetic sources in 20 scenes, scene switches every
               0.25 s and a live setup edit every 2 s while recording
  isos         the capacity estimate's ISO limit (1080p60 test patterns),
               recorded at the limit, then 3 past it with a stream: ISOs
               shed pictures, the program and stream drop none
  switching    100 scene switches a second through the API while
               recording: cuts, then fades interrupting each other
  cycles       hundreds of start/stop (held and immediate), splits,
               pause/resume and markers
  clients      16 process plugins polling status and watching, 32
               obs-websocket clients (and the 33rd refused) and 32 watch
               connections while recording with scene switches
  replay       the replay buffer beside a recording, saved 40 times
               (rapid and paced)
  multistream  4 stream destinations (one with its own encoder if it
               fits) to local receivers plus a recording; one receiver
               drops out and comes back
  text_web     long text sources, data-bound text set 20 times a second
               and 4 animated browser sources while recording
  disk         a small disk image near recording.keep_free_gb: an
               oversized timed start is refused, a recording stops by
               itself before the limit with every file finished, the
               next start is refused

Every scenario checks: every take's manifest is complete and every file it
lists (and every file a start or split returned) exists, passes ffprobe,
decodes fully, and accounts its frames (decoded + dropped - repeated =
frame slots); no orphan media; no audio dropouts; zero program and stream
drops (hardware profile); the engine never crashes, panics or hangs and
shuts down cleanly; API errors are only the documented refusals. It reports
drops per output, API latency p50/p99/max per method, errors, and the
engine's memory footprint, threads and file descriptors before, at peak and
after (growth over --max-*-growth fails).

Writes --out/report.json and --out/summary.md (always, even on failure).
Takes the benchmark lock ($ISOCAST_SCRATCH/.bench.lock) like e2e.py.
"""

import argparse
import base64
import collections
import concurrent.futures
import copy
import datetime
import fcntl
import json
import os
import random
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import traceback
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import e2e  # noqa: E402  (Engine, setup, capabilities, probe, check_file: one isolation model)
from e2e import Failure, RpcError, require, until  # noqa: E402
import soak  # noqa: E402  (process_metrics, scan_logs)

SCENARIOS = ("sources", "isos", "switching", "cycles", "clients", "replay", "multistream",
             "text_web", "disk")
PROFILES = {
    "hardware": dict(width=1920, height=1080, fps=60, strict=True, sources=40, scenes=20,
                     seconds=20, switch_rate=100, cycles=100, plugins=16, ws=32, watchers=32,
                     replay_saves=40, iso_max=16, browsers=4),
    "quick": dict(width=1280, height=720, fps=30, strict=False, sources=40, scenes=20,
                  seconds=8, switch_rate=100, cycles=25, plugins=4, ws=8, watchers=8,
                  replay_saves=12, iso_max=8, browsers=2),
}
WS_MAX = 32  # isocast-obsws accepts at most 32 clients (503 beyond)
WORKER_TIMEOUT = 900


def pct(values, q):
    if not values:
        return None
    v = sorted(values)
    return v[min(len(v) - 1, int(round(q * (len(v) - 1))))]


def lat_summary(values):
    if not values:
        return None
    return {"n": len(values), "p50_ms": round(pct(values, 0.5) * 1000, 2),
            "p99_ms": round(pct(values, 0.99) * 1000, 2), "max_ms": round(max(values) * 1000, 2)}


class Client:
    """One persistent JSON-RPC connection (requests one after another)."""

    def __init__(self, path, timeout=30):
        self.path, self.timeout = path, timeout
        self.sock = self.reader = None

    def connect(self):
        self.close()
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(str(self.path))
        self.reader = self.sock.makefile("rb")

    def call(self, method, params=None):
        if self.sock is None:
            self.connect()
        request = {"jsonrpc": "2.0", "id": 1, "method": method}
        if params is not None:
            request["params"] = params
        try:
            self.sock.sendall((json.dumps(request) + "\n").encode())
            line = self.reader.readline(16 * 1024 * 1024)
            if not line:
                raise OSError("connection closed")
        except OSError:
            self.close()
            raise
        reply = json.loads(line)
        if "error" in reply:
            raise RpcError(reply["error"])
        return reply["result"]

    def close(self):
        for thing in (self.reader, self.sock):
            try:
                if thing:
                    thing.close()
            except OSError:
                pass
        self.sock = self.reader = None


class Ctx:
    """A scenario's engine, timed API calls, samples, numbers and problems."""

    def __init__(self, name, root, engine, caps, prof):
        self.name, self.root, self.e, self.caps, self.p = name, root, engine, caps, prof
        self.client = Client(engine.sock)
        self.lock = threading.Lock()
        self.lat = collections.defaultdict(list)
        self.errors = collections.Counter()
        self.problems = []
        self.notes = []
        self.numbers = {}
        self.outputs = []        # per-output loss rows
        self.expected = set()    # every media path a start/split returned
        self.replays = set()     # replay saves (not in take manifests)
        self.files_verified = 0
        self.takes_verified = 0
        self.samples = []
        self.baseline = self.end = None
        self.stop_sampling = threading.Event()
        self.sampler = threading.Thread(target=self.sample_loop, daemon=True)
        self.sampler.start()
        self.missed_start = None

    # --- API
    def call(self, method, params=None, client=None, ok_codes=()):
        """A timed call. Errors are counted; codes in ok_codes are documented
        refusals (re-raised for the caller to handle), anything else is a
        problem as well as re-raised."""
        t = time.monotonic()
        try:
            result = (client or self.client).call(method, params)
        except RpcError as exc:
            took = time.monotonic() - t
            code = exc.error.get("code")
            msg = str(exc.error.get("message", ""))[:400]
            with self.lock:
                self.lat[method].append(took)
                self.errors[f"{method} {code}: {msg}"] += 1
                if code not in ok_codes:
                    self.problems.append(f"{method} failed: {code} {msg}")
            raise
        except OSError as exc:
            with self.lock:
                self.errors[f"{method}: {exc!r}"[:120]] += 1
                self.problems.append(f"{method}: connection error {exc!r}")
            raise Failure(f"{method}: {exc!r}") from exc
        took = time.monotonic() - t
        with self.lock:
            self.lat[method].append(took)
        if method in ("start", "split"):
            self.expected.update(os.path.realpath(f) for f in result.get("files", []))
        return result

    def problem(self, text):
        with self.lock:
            self.problems.append(text)

    def apply(self, cfg):
        rev = self.call("get")["revision"]
        return self.call("apply", {"revision": rev, "setup": cfg})

    def idle(self, seconds=60):
        until(lambda: self.client.call("status")["state"] == "idle", seconds, "an idle recorder")

    # --- samples
    def sample_loop(self):
        while not self.stop_sampling.is_set():
            pid = self.e.process.pid if self.e.process else None
            if pid and self.e.process.poll() is None:
                m = soak.process_metrics(pid)
                if m:
                    m["t"] = round(time.monotonic(), 2)
                    with self.lock:
                        self.samples.append(m)
            self.stop_sampling.wait(0.5)

    def measure(self):
        rows = []
        for _ in range(3):
            m = soak.process_metrics(self.e.process.pid)
            if m:
                rows.append(m)
            time.sleep(0.3)
        return {k: statistics.median(r[k] for r in rows if r.get(k) is not None)
                for k in ("footprint_mb", "threads", "fds")} if rows else None

    def start_baseline(self, cfg):
        """Apply the scenario's setup, bring the engine up, settle, measure."""
        self.apply(cfg)
        self.call("engine", {"running": True})
        time.sleep(2)
        self.baseline = self.measure()
        self.missed_start = (self.client.call("status").get("engine") or {}).get("missed_frames")

    def finish_end(self):
        """After the scenario's load is gone: idle, settle, measure."""
        try:
            self.idle(120)
        except Failure as exc:
            self.problem(f"not idle at the end: {exc}")
        status = self.client.call("status")
        engine = status.get("engine") or {}
        if self.missed_start is not None and engine.get("missed_frames") is not None:
            self.numbers["compositor_missed_frames"] = engine["missed_frames"] - self.missed_start
        if engine.get("worker_errors"):
            self.numbers["worker_errors"] = engine["worker_errors"][:5]
        time.sleep(5)
        self.end = self.measure()

    # --- verification
    def verify_file(self, path, entry):
        loss = None
        for row in e2e.drop_metrics({"files": [entry]}):
            loss = row
        if loss is None:
            raise Failure(f"{path.name}: no final frame/drop counters in the manifest")
        if (entry.get("frames") or 0) < 2:
            # Too short for a frame rate: it must still probe and decode.
            info = e2e.probe(path)
            e2e.ffmpeg("-xerror", "-i", path, "-map", "0", "-f", "null", "-", timeout=90)
            frames = sum(int(st.get("nb_read_frames") or 0) for st in info["streams"] if st["codec_type"] == "video")
            require(frames == (entry.get("frames") or 0), f"{path.name}: {frames} decoded, manifest says {entry.get('frames')}")
            return loss
        info = e2e.check_file(path, entry["duration_seconds"], entry["fps"],
                              (entry["width"], entry["height"]), audio=bool(entry.get("audio")),
                              dropped=loss["dropped"], repeated=loss["repeated"] or 0)
        require(info["frames"] == entry["frames"],
                f"{path.name}: {info['frames']} decoded, manifest says {entry['frames']}")
        return loss

    def audit(self, folders=None, early_stop_ok=False):
        """Verify every take under the recordings folder(s); every file a
        start/split returned is in a manifest; no orphan media."""
        folders = folders or [self.root / "recordings"]
        manifests = sorted(p for f in folders if f.exists() for p in f.rglob("*.session.json")
                           if not p.is_symlink())
        def pending():
            return [p for p in manifests if not e2e.finished(p)]
        try:
            until(lambda: not pending(), 120, "every take to finalize")
        except Failure:
            self.problem(f"{len(pending())} take(s) never finalized: {[p.name for p in pending()][:5]}")
        listed = set()
        jobs = []
        for path in manifests:
            try:
                m = e2e.read_json(path)
            except (OSError, ValueError) as exc:
                self.problem(f"{path.name}: unreadable manifest {exc!r}")
                continue
            errors = m.get("errors") or []
            early = early_stop_ok and m.get("status") == "failed" and errors and \
                all("stopped early" in str(x) and "finished and are playable" in str(x) for x in errors)
            if early:
                self.notes.append(f"{path.name}: {m['status']}: {errors[0]}"[:300])
            else:
                if m.get("status") != "complete":
                    self.problem(f"{path.name}: status {m.get('status')}")
                if errors:
                    self.problem(f"{path.name}: manifest errors {errors}"[:300])
            self.takes_verified += 1
            for entry in m.get("files", []):
                file = Path(os.path.realpath(path.parent / entry["path"]))
                listed.add(str(file))
                jobs.append((file, entry))
        with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
            futures = {pool.submit(self.verify_file, f, entry): (f, entry) for f, entry in jobs}
            for fut in concurrent.futures.as_completed(futures):
                file, entry = futures[fut]
                try:
                    loss = fut.result()
                except Exception as exc:  # noqa: BLE001 - every file is reported
                    self.problem(f"file check: {exc}"[:400])
                    continue
                self.files_verified += 1
                loss["source"] = entry["source"]
                loss["output"] = f"{file.parent.name[:19]}/{file.name}"
                self.add_loss(loss, "program" if entry["source"] == "program" else "iso")
        for path in sorted(self.expected - listed):
            self.problem(f"lost: {Path(path).name} was returned by start/split but no manifest lists it"
                         + ("" if Path(path).exists() else " and it doesn't exist"))
        for folder in folders:
            if not folder.exists():
                continue
            for media in folder.rglob("*"):
                real = os.path.realpath(media)
                if media.suffix in (".mov", ".mp4") and not media.name.startswith(".") \
                        and real not in listed and real not in self.replays:
                    self.problem(f"orphan media file: {media.name}")
        empty = [d.name for f in folders if f.exists() for d in f.iterdir()
                 if d.is_dir() and not any(d.rglob("*.session.json"))]
        if empty:
            self.numbers["empty_project_folders"] = len(empty)
            self.notes.append(f"{len(empty)} project folder(s) with no take, e.g. {empty[:3]}")
        self.numbers["takes"] = self.takes_verified
        self.numbers["files_verified"] = self.files_verified

    def add_loss(self, row, kind):
        row = dict(row, kind=kind)
        with self.lock:
            self.outputs.append(row)
        if row.get("audio_dropouts"):
            self.problem(f"{row['output']}: {row['audio_dropouts']} audio dropouts")
        if kind in ("program", "stream") and row.get("dropped") and self.p["strict"]:
            self.problem(f"{row['output']}: {kind} dropped {row['dropped']} of {row['slots']} frames")

    def stream_loss(self, status, label):
        slots = status["frames_sent"] + status["dropped_frames"]
        self.add_loss({"output": label, "frames": status["frames_sent"], "dropped": status["dropped_frames"],
                       "repeated": 0, "slots": slots, "audio_dropouts": status.get("audio_dropouts", 0),
                       "ratio": status["dropped_frames"] / slots if slots else None,
                       "reconnects": status.get("reconnects")}, "stream")

    def result(self):
        growth = None
        if self.baseline and self.end:
            growth = {k: round(self.end[k] - self.baseline[k], 1) for k in self.baseline}
        peak = None
        if self.samples:
            peak = {k: max(s[k] for s in self.samples if s.get(k) is not None)
                    for k in ("footprint_mb", "threads", "fds")}
        return {"latency": {m: lat_summary(v) for m, v in sorted(self.lat.items())},
                "errors": dict(self.errors), "numbers": self.numbers, "notes": self.notes,
                "outputs": self.outputs, "baseline": self.baseline, "end": self.end, "peak": peak,
                "growth": growth,
                "problems": [p if n == 1 else f"{p} (x{n})" for p, n in collections.Counter(self.problems).items()]}


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Listener:
    """An ffmpeg RTMP receiver on 127.0.0.1 writing what it gets to a file."""

    def __init__(self, root, port, name):
        self.root, self.port, self.name = root, port, name
        self.proc = None
        self.files = []

    def start(self, suffix=""):
        out = self.root / f"{self.name}{suffix}.flv"
        self.files.append(out)
        self.log = open(self.root / f"{self.name}{suffix}.log", "wb")
        self.proc = subprocess.Popen(["ffmpeg", "-nostdin", "-hide_banner", "-v", "warning", "-listen", "1",
                                      "-i", f"rtmp://127.0.0.1:{self.port}/live/e2e", "-c", "copy", "-y",
                                      str(out)], stdout=self.log, stderr=self.log)
        time.sleep(0.5)
        require(self.proc.poll() is None, f"RTMP listener {self.name} failed to start")

    def kill(self):
        if self.proc and self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait(timeout=5)

    def wait(self, seconds=15):
        if self.proc:
            try:
                self.proc.wait(timeout=seconds)
            except subprocess.TimeoutExpired:
                self.kill()

    def check(self, c):
        """Every received file decodes; returns seconds of video received."""
        total = 0.0
        for f in self.files:
            if not f.exists() or f.stat().st_size == 0:
                c.problem(f"receiver {f.name}: nothing received")
                continue
            out = e2e.ffmpeg("-xerror", "-i", f, "-map", "0", "-f", "null", "-", check=False, timeout=120)
            # A receiver killed mid-packet ends with a truncated packet.
            if out.returncode and not f.name.endswith("-killed.flv"):
                c.problem(f"receiver {f.name}: decode failed: {out.stderr.decode(errors='replace')[-300:]}")
            info = json.loads(e2e.command(["ffprobe", "-v", "error", "-show_entries", "format=duration",
                                           "-of", "json", f], check=False).stdout or b"{}")
            total += float(info.get("format", {}).get("duration") or 0)
        return round(total, 2)


def base_setup(c, iso=False):
    p = c.p
    cfg = e2e.setup(c.root, c.caps["record_codec"], iso=iso, width=p["width"], height=p["height"], fps=p["fps"])
    cfg["name"] = f"stress {c.name}"
    return cfg


def full(c):
    return {"x": 0, "y": 0, "width": c.p["width"], "height": c.p["height"]}


def record_for(c, seconds, during=None, params=None):
    """Start, run `during(deadline)` (or sleep), stop; timed. Returns the start result."""
    started = c.call("start", params or {})
    deadline = time.monotonic() + seconds
    if during:
        during(deadline)
    time.sleep(max(0.0, deadline - time.monotonic()))
    c.call("stop")
    c.idle(120)
    return started


# ---------------------------------------------------------------- scenarios

def scenario_sources(c):
    p = c.p
    W, H = p["width"], p["height"]
    cfg = base_setup(c)
    sources = []
    for i in range(p["sources"]):
        kind = i % 4
        if kind in (0, 1):
            sources.append({"id": f"tp{i}", "kind": "test_pattern", "width": W // 3, "height": H // 3,
                            "fps": p["fps"]})
        elif kind == 2:
            sources.append({"id": f"col{i}", "kind": "color", "color": f"#{(i * 2654435761) & 0xffffff:06x}",
                            "width": W // 3, "height": H // 3})
        else:
            sources.append({"id": f"txt{i}", "kind": "text", "text": f"Source {i}", "size": 48,
                            "width": W // 3, "height": H // 3})
    cfg["sources"] = [{"id": "bars", "kind": "test_pattern", "width": W, "height": H, "fps": p["fps"]},
                      *sources, {"id": "tone", "kind": "tone"}]
    cfg["recording"]["iso"]["sources"] = []
    cells = [{"x": (j % 3) * W // 3, "y": (j // 3) * H // 3, "width": W // 3, "height": H // 3} for j in range(6)]
    cfg["scenes"] = []
    for s in range(p["scenes"]):
        items = [{"source": "bars", "rect": full(c)}]
        items += [{"source": sources[(s * 6 + j) % len(sources)]["id"], "rect": cells[j]} for j in range(6)]
        cfg["scenes"].append({"id": f"s{s}", "name": f"Scene {s}", "items": items})
    c.numbers.update(sources=len(cfg["sources"]), scenes=len(cfg["scenes"]))
    c.start_baseline(cfg)
    switches = edits = 0

    def during(deadline):
        nonlocal switches, edits
        i, next_edit = 1, time.monotonic() + 2
        while time.monotonic() < deadline:
            c.call("switch_scene", {"scene": f"s{i % p['scenes']}", "transition": {"kind": "cut"}})
            switches += 1
            i += 1
            if time.monotonic() >= next_edit:
                live = copy.deepcopy(cfg)
                live["sources"][3]["color"] = f"#{random.randrange(1 << 24):06x}"
                live["sources"][4]["text"] = f"Edit {edits}"
                c.apply(live)
                edits += 1
                next_edit += 2
            time.sleep(0.25)
    started = record_for(c, p["seconds"], during)
    c.numbers.update(switches=switches, live_edits=edits)
    scenes = e2e.read_json(started_manifest(c, started)).get("scenes", [])
    c.numbers["manifest_scene_changes"] = len(scenes)
    if len(scenes) < switches:
        c.problem(f"manifest lists {len(scenes)} scene changes for {switches} switches")
    c.finish_end()
    c.audit()


def started_manifest(c, started):
    files = {Path(f).name for f in started["files"]}
    folder = Path(started["files"][0]).parent

    def locate():
        for path in folder.glob("*.session.json"):
            try:
                if {f["path"] for f in e2e.read_json(path)["files"]} == files:
                    return path
            except (OSError, ValueError, KeyError):
                pass
        return None
    return until(locate, 30, "the take's manifest")


def scenario_isos(c):
    p = c.p
    W, H, F = p["width"], p["height"], p["fps"]
    cfg = base_setup(c, iso=True)
    ids = [f"iso{i}" for i in range(1, p["iso_max"] + 1)]
    cfg["sources"] = [{"id": s, "kind": "test_pattern", "width": W, "height": H, "fps": F} for s in ids]
    cfg["sources"].append({"id": "tone", "kind": "tone"})
    cfg["scenes"] = [{"id": "main", "name": "Main", "items": [
        {"source": ids[0], "rect": full(c)},
        {"source": ids[1], "rect": {"x": W // 2, "y": H // 2, "width": W // 2 - 32, "height": H // 2 - 32}}]}]
    port = free_port()
    stream = {"service": "custom", "server": f"rtmp://127.0.0.1:{port}/live", "video_bitrate_kbps": 6000,
              "audio_bitrate_kbps": 160, "width": W, "height": H, "keyframe_interval_seconds": 2}
    estimates = []
    fit = 0
    for k in range(1, len(ids) + 1):
        trial = copy.deepcopy(cfg)
        trial["recording"]["iso"]["sources"] = ids[:k]
        est = c.call("estimate_capacity", {"setup": trial, "stream": False})
        estimates.append({"isos": k, "load": round(est["load"], 3), "capacity": est["capacity"],
                          "iso_drop_percent": est["iso_drop_percent"]})
        if est["capacity"] in ("fits", "no_margin"):
            fit = k
    c.numbers["estimates"] = estimates
    c.numbers["chip"] = est.get("chip")
    c.numbers["iso_limit"] = fit
    require(fit >= 1, f"the estimate says not even one 1080p ISO fits: {estimates[:2]}")
    past = min(len(ids), fit + 3)
    if past == fit:
        c.notes.append(f"the estimate fits all {fit} ISOs; nothing past the limit to try")
    cfg["recording"]["iso"]["sources"] = ids[:fit]
    c.start_baseline(cfg)
    watch = e2e.Watch(c.e, ["warning", "encoder_overload"])
    # Twice: the first take of a session also pays for its encoders' startup.
    a = record_for(c, p["seconds"])
    a2 = record_for(c, p["seconds"])
    c.numbers["at_limit_files"] = len(a["files"])
    # Past the limit, with a stream: the program and stream keep every frame.
    cfg["recording"]["iso"]["sources"] = ids[:past]
    cfg["stream"] = stream
    c.apply(cfg)
    est = c.call("estimate_capacity", {"stream": True})
    c.numbers["past_limit"] = {"isos": past, "load": round(est["load"], 3), "capacity": est["capacity"],
                               "iso_drop_percent_estimated": est["iso_drop_percent"]}
    listener = Listener(c.root, port, "isos-stream")
    listener.start()
    try:
        c.call("stream_start")
        started = record_for(c, p["seconds"])
        final = c.call("stream_stop")
        listener.wait()
    finally:
        listener.kill()
    c.stream_loss(final, "stream")
    time.sleep(0.5)
    hang_up(watch.sock, watch.reader)
    c.numbers["events"] = dict(collections.Counter(ev["type"] for ev in watch.events))
    c.numbers["event_examples"] = [json.dumps(ev)[:300] for ev in watch.events[:4]]
    c.numbers["stream_received_seconds"] = listener.check(c)
    c.numbers["past_limit_files"] = len(started["files"])
    c.finish_end()
    c.audit()
    for label, take in (("at_limit", a), ("at_limit_again", a2), ("past_limit", started)):
        names = {Path(f).name for f in take["files"]}
        rows = [r for r in c.outputs if Path(r["output"]).name in names]
        isos = [r for r in rows if r["kind"] == "iso"]
        slots = sum(r["slots"] for r in isos) or 1
        c.numbers.setdefault(label, {}).update(
            iso_drop_percent_measured=round(100 * sum(r["dropped"] for r in isos) / slots, 2),
            program_dropped=sum(r["dropped"] for r in rows if r["kind"] == "program"),
            audio_dropouts=sum(r.get("audio_dropouts") or 0 for r in rows))


def scenario_switching(c):
    p = c.p
    W, H, F = p["width"], p["height"], p["fps"]
    cfg = base_setup(c)
    cfg["sources"] = [{"id": "bars", "kind": "test_pattern", "width": W, "height": H, "fps": F},
                      {"id": "bars2", "kind": "test_pattern", "width": W // 2, "height": H // 2, "fps": F},
                      {"id": "blue", "kind": "color", "color": "#2040c0"}, {"id": "tone", "kind": "tone"}]
    cfg["scenes"] = [
        {"id": "a", "name": "A", "items": [{"source": "bars", "rect": full(c)}]},
        {"id": "b", "name": "B", "items": [{"source": "blue", "rect": full(c)},
                                          {"source": "bars2", "rect": {"x": 0, "y": 0, "width": W // 2, "height": H // 2}}]},
        {"id": "c", "name": "C", "items": [{"source": "bars2", "rect": full(c)}]}]
    c.start_baseline(cfg)
    rate = p["switch_rate"]
    counts = collections.Counter()
    done = []

    def burst(seconds, transition, label):
        t0 = time.monotonic()
        n = 0
        while time.monotonic() - t0 < seconds:
            due = t0 + n / rate
            time.sleep(max(0.0, due - time.monotonic()))
            try:
                r = c.call("switch_scene", {"scene": "abc"[(n + 1) % 3], "transition": transition})
                done.append((label, r.get("scene"), r.get("time_seconds")))
                counts[label] += 1
            except RpcError:
                counts[label + "_errors"] += 1
            n += 1
        c.numbers[f"{label}_rate_per_s"] = round(n / (time.monotonic() - t0), 1)

    def during(_deadline):
        burst(p["seconds"], {"kind": "cut"}, "cuts")
        burst(p["seconds"] / 2, {"kind": "fade", "duration_ms": 300}, "fades")
        time.sleep(1)
    started = record_for(c, 0, during)
    c.numbers.update(counts)
    scenes = e2e.read_json(started_manifest(c, started)).get("scenes", [])
    c.numbers["manifest_scene_changes"] = len(scenes)
    missing = []
    entries = [(s.get("time_seconds") or 0, s.get("scene")) for s in scenes]
    superseded = 0
    for i, (label, scene, t) in enumerate(done):
        if t is None:
            continue
        if any(abs(et - t) <= 0.001 and es == scene for et, es in entries):
            continue
        if i + 1 < len(done) and done[i + 1][2] is not None and abs(done[i + 1][2] - t) <= 0.001:
            superseded += 1  # the next switch landed on the same frame: never on air
            continue
        prev = done[i - 1][2] if i else None
        missing.append({"kind": label, "scene": scene, "t": round(t, 4),
                        "since_previous_ms": round((t - prev) * 1000, 2) if prev is not None else None,
                        "same_as_previous": i > 0 and done[i - 1][1] == scene})
    c.numbers["switches_superseded_in_same_frame"] = superseded
    c.numbers["switches_missing_from_manifest"] = len(missing)
    if missing:
        c.numbers["missing_by_kind"] = dict(collections.Counter(m["kind"] for m in missing))
        c.numbers["missing_examples"] = missing[:8]
        c.numbers["manifest_scene_example"] = scenes[:2]
        c.problem(f"{len(missing)} of {len(done)} acknowledged switches are missing from the manifest")
    c.finish_end()
    c.audit()


WRONG_STATE = -32001


def scenario_cycles(c):
    p = c.p
    n = p["cycles"]
    cfg = base_setup(c)
    c.start_baseline(cfg)
    stats = collections.Counter()

    def start(params=None):
        """Start; a start refused while the last take finalizes is retried."""
        for _ in range(200):
            try:
                return c.call("start", params or {}, ok_codes=(WRONG_STATE,))
            except RpcError as exc:
                if exc.error.get("code") != WRONG_STATE:
                    raise
                stats["start_refused_busy"] += 1
                time.sleep(0.05)
        raise Failure("start refused for 10 s")

    log = []
    # (a) start, hold 0.2-0.8 s, stop.
    for _ in range(n):
        s = start()
        hold = random.uniform(0.2, 0.8)
        time.sleep(hold)
        log.append((s, hold, c.call("stop")))
        stats["held_takes"] += 1
    # (b) start and stop at once.
    for _ in range(n // 2):
        s = start()
        log.append((s, 0, c.call("stop")))
        stats["instant_takes"] += 1
    c.idle(120)
    # (c) one recording split n times, with a marker after each split.
    first = start()
    t0 = time.monotonic()
    splits = []
    for i in range(n):
        time.sleep(0.6)
        try:
            sp = c.call("split", ok_codes=(WRONG_STATE,))
            sp["_acked_at"] = time.time()
            splits.append(sp)
            stats["splits"] += 1
        except RpcError:
            stats["split_refused"] += 1
        c.call("marker", {"label": f"m{i}"})
        stats["markers"] += 1
    c.call("stop")
    wall = time.monotonic() - t0
    c.idle(120)
    folder = Path(first["files"][0]).parent
    program = 0.0
    for path in folder.glob("*.session.json"):
        for f in e2e.read_json(path).get("files", []):
            if f["source"] == "program":
                program += f.get("duration_seconds") or 0
    c.numbers["split_take"] = {"wall_seconds": round(wall, 2), "program_seconds": round(program, 2),
                               "manifests": len(list(folder.glob("*.session.json"))),
                               "acknowledged_without_file": [Path(f).name for sp in splits for f in sp["files"]
                                                             if not Path(f).exists()][:10]}
    superseded = never = 0
    for i, sp in enumerate(splits):
        if all(Path(f).exists() for f in sp["files"]):
            continue
        begins = datetime.datetime.fromisoformat(sp["begins_at"]).timestamp() if sp.get("begins_at") else None
        nxt = splits[i + 1]["_acked_at"] if i + 1 < len(splits) else None
        if begins and nxt and nxt < begins:
            superseded += 1
        else:
            never += 1
    c.numbers["split_take"].update(
        acknowledged=len(splits), superseded_before_begin=superseded, missing_otherwise=never,
        begins_after_ack_ms=[round((datetime.datetime.fromisoformat(sp["begins_at"]).timestamp() - sp["_acked_at"]) * 1000)
                             for sp in splits[:5] if sp.get("begins_at")])
    if program < wall - 1:
        c.problem(f"the split recording holds {program:.2f} s of program for {wall:.2f} s recorded")
    # (d) one recording paused and resumed n times.
    started = start()
    for _ in range(n):
        time.sleep(0.1)
        c.call("pause")
        time.sleep(0.1)
        c.call("resume")
        stats["pauses"] += 1
    time.sleep(0.5)
    c.call("stop")
    c.idle(120)
    m = e2e.read_json(started_manifest(c, started))
    stats["manifest_pauses"] = len(m.get("pauses", []))
    if stats["manifest_pauses"] != stats["pauses"]:
        c.problem(f"pause take: manifest lists {stats['manifest_pauses']} pauses for {stats['pauses']}")
    c.numbers.update(stats)
    c.finish_end()
    c.audit()
    vanished = []
    for s, hold, stopped in log:
        if not all(Path(f).exists() for f in s["files"]):
            vanished.append({"files": [Path(f).name for f in s["files"]], "held_s": round(hold, 2),
                             "start": {k: v for k, v in s.items() if k != "files"},
                             "stop": json.dumps(stopped)[:600]})
    c.numbers["vanished_takes"] = len(vanished)
    c.numbers["vanished_examples"] = vanished[:3]
    markers = sum(len(e2e.read_json(path).get("markers", []))
                  for path in (c.root / "recordings").rglob("*.session.json"))
    c.numbers["manifest_markers"] = markers
    if markers != stats["markers"]:
        c.problem(f"manifests list {markers} markers for {stats['markers']} marker calls")


PLUGIN_SCRIPT = r"""
import json, os, socket, threading, time
path = os.environ["ISOCAST_SOCKET"]
out = os.path.join(os.environ["ISOCAST_PLUGIN_DATA"], "stress.json")
st = {"ok": 0, "errors": 0, "events": 0, "watch_error": None, "lat": []}
def watch():
    try:
        s = socket.socket(socket.AF_UNIX); s.connect(path)
        s.sendall(b'{"jsonrpc":"2.0","id":1,"method":"watch","params":{"kinds":["scene_changed"]}}\n')
        f = s.makefile("rb"); f.readline()
        for line in f:
            if json.loads(line).get("method") == "event":
                st["events"] += 1
    except Exception as e:
        st["watch_error"] = repr(e)
threading.Thread(target=watch, daemon=True).start()
s = f = None
last = 0
while True:
    t = time.monotonic()
    try:
        if s is None:
            s = socket.socket(socket.AF_UNIX); s.connect(path); f = s.makefile("rb")
        s.sendall(b'{"jsonrpc":"2.0","id":1,"method":"status"}\n')
        r = json.loads(f.readline())
        st["ok" if "result" in r else "errors"] += 1
    except Exception:
        st["errors"] += 1; s = None; time.sleep(0.2)
    st["lat"].append(time.monotonic() - t)
    st["lat"] = st["lat"][-5000:]
    if time.monotonic() - last > 0.5:
        lat = sorted(st["lat"])
        doc = {k: st[k] for k in ("ok", "errors", "events", "watch_error")}
        doc["p50_ms"] = lat[len(lat) // 2] * 1000
        doc["p99_ms"] = lat[min(len(lat) - 1, int(len(lat) * 0.99))] * 1000
        with open(out, "w") as fh:
            json.dump(doc, fh)
        last = time.monotonic()
    time.sleep(0.05)
"""


class WsRefused(Exception):
    def __init__(self, status):
        super().__init__(f"HTTP {status}")
        self.status = status


class WsClient:
    """A minimal obs-websocket v5 client (JSON, no password)."""

    def __init__(self, port):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=10)
        self.reader = self.sock.makefile("rb")
        key = base64.b64encode(os.urandom(16)).decode()
        self.sock.sendall((f"GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\n"
                           f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
                           "Sec-WebSocket-Protocol: obswebsocket.json\r\n\r\n").encode())
        status = int(self.reader.readline().split()[1])
        while self.reader.readline() not in (b"\r\n", b""):
            pass
        if status != 101:
            self.close()
            raise WsRefused(status)
        self.events = collections.Counter()
        self.next_id = 0
        hello = self.recv()
        require(hello["op"] == 0, f"obs-websocket: expected Hello, got {hello}")
        self.send({"op": 1, "d": {"rpcVersion": 1, "eventSubscriptions": 2047}})
        while True:
            msg = self.recv()
            if msg["op"] == 2:
                break

    def send(self, obj):
        data = json.dumps(obj).encode()
        mask = os.urandom(4)
        n = len(data)
        head = bytes([0x81]) + (bytes([0x80 | n]) if n < 126 else
                                bytes([0x80 | 126]) + n.to_bytes(2, "big") if n < 65536 else
                                bytes([0x80 | 127]) + n.to_bytes(8, "big"))
        self.sock.sendall(head + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(data)))

    def read(self, n):
        data = self.reader.read(n)
        if len(data) < n:
            raise OSError("websocket closed")
        return data

    def recv(self):
        payload = b""
        while True:
            b0, b1 = self.read(2)
            n = b1 & 0x7F
            if n == 126:
                n = int.from_bytes(self.read(2), "big")
            elif n == 127:
                n = int.from_bytes(self.read(8), "big")
            mask = self.read(4) if b1 & 0x80 else None
            data = self.read(n)
            if mask:
                data = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
            op = b0 & 0x0F
            if op == 9:
                self.sock.sendall(bytes([0x8A, 0x80]) + os.urandom(4))
                continue
            if op == 8:
                raise OSError("websocket closed by the server")
            if op in (0, 1, 2):
                payload += data
                if b0 & 0x80:
                    msg = json.loads(payload)
                    if msg.get("op") == 5:
                        self.events[msg["d"]["eventType"]] += 1
                    return msg

    def request(self, kind, data=None):
        self.next_id += 1
        rid = str(self.next_id)
        self.send({"op": 6, "d": {"requestType": kind, "requestId": rid, **({"requestData": data} if data else {})}})
        while True:
            msg = self.recv()
            if msg["op"] == 7 and msg["d"]["requestId"] == rid:
                return msg["d"]

    def drain(self, seconds):
        self.sock.settimeout(seconds)
        try:
            while True:
                self.recv()
        except (OSError, socket.timeout, ValueError):
            pass

    def close(self):
        hang_up(self.sock, self.reader)


def hang_up(sock, reader):
    """Really close: a makefile() reader keeps the descriptor (and the
    connection) open after sock.close()."""
    for step in (lambda: sock.shutdown(socket.SHUT_RDWR), reader.close, sock.close):
        try:
            step()
        except (OSError, ValueError):
            pass


def read_plugin(path):
    for _ in range(40):
        try:
            return json.loads(path.read_text())
        except (OSError, ValueError):
            time.sleep(0.05)  # mid-write
    raise Failure(f"unreadable plugin results {path}")


def scenario_clients(c):
    p = c.p
    W, H, F = p["width"], p["height"], p["fps"]
    cfg = base_setup(c)
    cfg["sources"].append({"id": "blue", "kind": "color", "color": "#2040c0"})
    cfg["scenes"] = [{"id": "a", "name": "A", "items": [{"source": "bars", "rect": full(c)}]},
                     {"id": "b", "name": "B", "items": [{"source": "blue", "rect": full(c)},
                                                       {"source": "bars2", "rect": {"x": 0, "y": 0, "width": W // 2,
                                                                                     "height": H // 2}}]}]
    c.start_baseline(cfg)
    # Process plugins.
    plugin_ids = []
    for i in range(p["plugins"]):
        folder = c.root / "plugin-src" / f"stress-{i}"
        folder.mkdir(parents=True)
        (folder / "probe.py").write_text(PLUGIN_SCRIPT)
        (folder / "isocast-plugin.toml").write_text(
            f'id = "stress-{i}"\nname = "Stress {i}"\nversion = "0.1.0"\napi = 1\n'
            '[permissions]\nmethods = ["status", "watch"]\n'
            '[process]\ncommand = ["python3", "probe.py"]\nautostart = true\n')
        c.call("plugin_add", {"path": str(folder), "grant": {"methods": ["status", "watch"]}})
        plugin_ids.append(f"stress-{i}")
    data = [c.root / "plugins" / pid / "data" / "stress.json" for pid in plugin_ids]
    until(lambda: all(d.exists() for d in data), 60, "every plugin process polling")
    # obs-websocket.
    port = free_port()
    c.call("websocket_enable", {"port": port, "no_password": True})
    until(lambda: c.client.call("websocket_status")["running"], 10, "obs-websocket listening")
    stop = threading.Event()
    ws_stats = []

    def ws_worker(i):
        st = {"requests": 0, "errors": 0, "lat": [], "events": {}, "error": None}
        ws_stats.append(st)
        try:
            ws = WsClient(port)
        except Exception as exc:  # noqa: BLE001
            st["error"] = f"connect: {exc!r}"
            return
        try:
            kinds = ("GetCurrentProgramScene", "GetRecordStatus", "GetSceneList", "GetStats")
            while not stop.is_set():
                t = time.monotonic()
                r = ws.request(kinds[st["requests"] % len(kinds)])
                st["lat"].append(time.monotonic() - t)
                st["requests"] += 1
                if not r["requestStatus"]["result"]:
                    st["errors"] += 1
                    st["error"] = str(r["requestStatus"])[:200]
                time.sleep(0.1)
            ws.drain(2)
        except Exception as exc:  # noqa: BLE001
            st["error"] = repr(exc)
        finally:
            st["events"] = dict(ws.events)
            ws.close()
    threads = [threading.Thread(target=ws_worker, args=(i,), daemon=True) for i in range(p["ws"])]
    for t in threads:
        t.start()
    until(lambda: c.client.call("websocket_status")["clients"] >= p["ws"], 20, "every websocket client")
    # Fill to the limit, then one more must be refused.
    fillers = []
    try:
        for _ in range(WS_MAX - p["ws"]):
            fillers.append(WsClient(port))
        try:
            extra = WsClient(port)
            extra.close()
            c.problem(f"obs-websocket client {WS_MAX + 1} was accepted (limit {WS_MAX})")
        except WsRefused as exc:
            c.numbers["ws_over_limit"] = f"HTTP {exc.status}"
            if exc.status != 503:
                c.problem(f"obs-websocket client {WS_MAX + 1}: HTTP {exc.status}, expected 503")
    finally:
        for f in fillers:
            f.close()
    # Raw watch connections.
    watchers = [e2e.Watch(c.e, ["scene_changed"]) for _ in range(p["watchers"])]
    c.numbers["ws_clients_at_peak"] = c.client.call("websocket_status")["clients"]
    switches = 0
    before = {d: read_plugin(d).get("events", 0) for d in data}

    def during(deadline):
        nonlocal switches
        while time.monotonic() < deadline:
            c.call("switch_scene", {"scene": "ab"[(switches + 1) % 2], "transition": {"kind": "cut"}})
            switches += 1
            time.sleep(0.5)
    record_for(c, p["seconds"], during)
    time.sleep(2)
    stop.set()
    for t in threads:
        t.join(timeout=15)
    time.sleep(3)
    teardown = {"websocket clients gone": c.measure()}
    for w in watchers:
        hang_up(w.sock, w.reader)
    time.sleep(3)
    teardown["watch connections closed"] = c.measure()
    c.numbers["switches"] = switches
    got = [len(w.of("scene_changed")) for w in watchers]
    c.numbers["watch_events_min_max"] = [min(got), max(got)]
    if min(got) != switches:
        c.problem(f"watch connections got {min(got)}..{max(got)} scene_changed events for {switches} switches")
    ws_lat = [x for st in ws_stats for x in st["lat"]]
    c.lat["obs-websocket request"] = ws_lat
    ws_events = [st["events"].get("CurrentProgramSceneChanged", 0) for st in ws_stats]
    c.numbers["ws_requests"] = sum(st["requests"] for st in ws_stats)
    c.numbers["ws_scene_events_min_max"] = [min(ws_events), max(ws_events)]
    for st in ws_stats:
        if st["error"] or st["errors"]:
            c.problem(f"obs-websocket client: {st['errors']} failed requests, {st['error']}")
            break
    if min(ws_events) != switches:
        c.problem(f"obs-websocket clients got {min(ws_events)}..{max(ws_events)} CurrentProgramSceneChanged "
                  f"for {switches} switches")
    plugin_rows = [read_plugin(d) for d in data]
    c.numbers["plugin_status_calls"] = sum(r["ok"] for r in plugin_rows)
    c.numbers["plugin_status_errors"] = sum(r["errors"] for r in plugin_rows)
    c.numbers["plugin_p50_p99_ms"] = [round(statistics.median(r["p50_ms"] for r in plugin_rows), 2),
                                      round(max(r["p99_ms"] for r in plugin_rows), 2)]
    pev = [r["events"] - before[d] for r, d in zip(plugin_rows, data)]
    c.numbers["plugin_scene_events_min_max"] = [min(pev), max(pev)]
    if c.numbers["plugin_status_errors"]:
        c.problem(f"plugins saw {c.numbers['plugin_status_errors']} failed status calls")
    if min(pev) != switches or any(r["watch_error"] for r in plugin_rows):
        c.problem(f"plugins got {min(pev)}..{max(pev)} scene events for {switches} switches "
                  f"({[r['watch_error'] for r in plugin_rows if r['watch_error']][:2]})")
    listed = c.call("plugin_list")["plugins"]
    restarts = {pl["id"]: (pl.get("process") or {}).get("restarts") for pl in listed}
    states = collections.Counter((pl.get("process") or {}).get("state") for pl in listed)
    c.numbers["plugin_states"] = dict(states)
    if any(restarts.values()) or states.get("running", 0) != len(plugin_ids):
        c.problem(f"plugin processes: {dict(states)}, restarts {restarts}")
    for pid in plugin_ids:
        c.call("plugin_remove", {"id": pid})
    time.sleep(3)
    teardown["plugins removed"] = c.measure()
    c.call("websocket_disable")
    time.sleep(3)
    teardown["websocket disabled"] = c.measure()
    c.numbers["teardown_footprint_threads_fds"] = {k: [v["footprint_mb"], v["threads"], v["fds"]]
                                                  for k, v in teardown.items() if v}
    c.finish_end()
    c.audit()


def scenario_replay(c):
    p = c.p
    cfg = base_setup(c)
    seconds = 5
    cfg["recording"]["replay"] = {"seconds": seconds}
    c.start_baseline(cfg)
    c.call("replay_start")
    keyframe = c.call("replay_status").get("keyframe_interval_seconds") or 2
    started = c.call("start", {})
    time.sleep(seconds + 1.5)
    saves = []
    errors = 0
    for i in range(p["replay_saves"]):
        time.sleep(0.1 if i < p["replay_saves"] // 2 else 0.5)
        try:
            saved = c.call("replay_save", ok_codes=(WRONG_STATE,))
            saves.append(saved)
            c.replays.add(os.path.realpath(saved["path"]))
        except RpcError:
            errors += 1
    time.sleep(1)
    c.call("stop")
    c.idle(120)
    status = c.call("replay_status")
    final = c.call("replay_stop")
    c.numbers.update(saves=len(saves), save_refusals=errors, replay_memory_mb=round(status.get("memory_bytes", 0) / 2**20, 1),
                     replay_dropped=status.get("dropped_frames"), final_saves=final.get("saves"))
    if final.get("saves") != len(saves):
        c.problem(f"replay_stop says {final.get('saves')} saves, {len(saves)} returned a file")
    paths = [s["path"] for s in saves]
    if len(set(paths)) != len(paths):
        c.problem("two saves returned the same file")
    checked = []
    for saved in saves:
        try:
            checked.append(e2e.check_replay(saved, seconds, keyframe, p["fps"]))
        except Exception as exc:  # noqa: BLE001
            c.problem(f"replay {Path(saved['path']).name}: {exc}"[:300])
    c.numbers["saves_verified"] = len(checked)
    c.numbers["short_saves"] = sum(1 for s in saves if s.get("short"))
    if checked:
        c.numbers["save_seconds_min_max"] = [min(r["seconds"] for r in checked), max(r["seconds"] for r in checked)]
    for saved in saves:
        if saved.get("dropped_frames"):
            c.problem(f"replay {Path(saved['path']).name}: {saved['dropped_frames']} dropped frames")
    c.finish_end()
    c.audit()
    del started


def scenario_multistream(c):
    p = c.p
    W, H = p["width"], p["height"]
    cfg = base_setup(c)
    names = ["main", "d2", "d3", "d4"]
    listeners = {n: Listener(c.root, free_port(), f"ms-{n}") for n in names}
    cfg["stream"] = {"service": "custom", "server": f"rtmp://127.0.0.1:{listeners['main'].port}/live",
                     "video_bitrate_kbps": 6000, "audio_bitrate_kbps": 160, "width": W, "height": H,
                     "keyframe_interval_seconds": 2,
                     "destinations": [{"id": n, "service": "custom",
                                       "server": f"rtmp://127.0.0.1:{listeners[n].port}/live"} for n in names[1:]]}
    own = cfg["stream"]["destinations"][2]
    own.update(video_bitrate_kbps=3000, width=W * 2 // 3 // 2 * 2, height=H * 2 // 3 // 2 * 2)
    c.start_baseline(cfg)
    for listener in listeners.values():
        listener.start()
    try:
        try:
            c.call("stream_start", ok_codes=(WRONG_STATE, -32602, -32000))
            c.numbers["d4_own_encoder"] = True
        except RpcError as exc:
            c.notes.append(f"d4's own encoder refused ({exc.error.get('message', '')[:160]}); d4 shares instead")
            c.problems = [x for x in c.problems if not x.startswith("stream_start failed")]
            for key in ("video_bitrate_kbps", "width", "height"):
                own.pop(key)
            c.apply(cfg)
            c.numbers["d4_own_encoder"] = False
            c.call("stream_start")
        third = p["seconds"] / 3

        def during(deadline):
            time.sleep(third)
            # d3's ingest dies, then comes back: the others never notice.
            listeners["d3"].kill()
            listeners["d3"].files[-1].rename(listeners["d3"].files[-1].with_name("ms-d3-killed.flv"))
            listeners["d3"].files[-1] = listeners["d3"].files[-1].with_name("ms-d3-killed.flv")
            time.sleep(3)
            listeners["d3"].start("-again")
        record_for(c, p["seconds"], during)
        status = c.call("status")
        final = c.call("stream_stop")
        for listener in listeners.values():
            listener.wait()
    finally:
        for listener in listeners.values():
            listener.kill()
    rows = {d["id"]: d for d in final.get("destinations", [])}
    c.numbers["destinations"] = {i: {k: d.get(k) for k in ("frames_sent", "dropped_frames", "audio_dropouts",
                                                            "reconnects", "state")} for i, d in rows.items()}
    c.numbers["streams_live_at_stop"] = len(status.get("streams") or [])
    for n in names:
        if n not in rows:
            c.problem(f"stream_stop lists no destination {n}")
            continue
        if n == "d3":
            if not rows[n].get("reconnects"):
                c.problem("d3 lost its ingest and came back but reports no reconnect")
            continue
        c.stream_loss(rows[n], f"stream {n}")
        if rows[n].get("reconnects"):
            c.problem(f"{n} reconnected {rows[n]['reconnects']} times while only d3's ingest failed")
    c.numbers["received_seconds"] = {n: lst.check(c) for n, lst in listeners.items()}
    for n in ("main", "d2", "d4"):
        if c.numbers["received_seconds"][n] < p["seconds"] - 2:
            c.problem(f"{n}: received {c.numbers['received_seconds'][n]} s of a {p['seconds']} s stream")
    c.finish_end()
    c.audit()


PAGE = """<!doctype html><html><body style="margin:0;background:#18304f;overflow:hidden">
<div id="b" style="width:160px;height:160px;background:#e33;position:absolute;animation:m 1.3s linear infinite alternate"></div>
<p id="t" style="font:40px Helvetica;color:#fff;position:absolute;left:20px;bottom:10px"></p>
<style>@keyframes m{from{transform:translate(0,0) rotate(0)}to{transform:translate(900px,400px) rotate(360deg)}}</style>
<script>let n=0;setInterval(()=>{document.getElementById('t').textContent='frame '+(n++)},16)</script>
</body></html>"""

LOREM = ("Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore "
         "et dolore magna aliqua. Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris nisi ut "
         "aliquip ex ea commodo consequat. Ünïcødé ✓ 漢字 العربية ")


def scenario_text_web(c):
    p = c.p
    W, H, F = p["width"], p["height"], p["fps"]
    cfg = base_setup(c)
    page = c.root / "page.html"
    page.write_text(PAGE)
    browsers = [{"id": f"web{i}", "kind": "browser", "file": str(page), "width": 1280, "height": 720, "fps": 30}
                for i in range(p["browsers"])]
    texts = [{"id": f"long{i}", "kind": "text", "text": (LOREM * (12 if i else 120)).strip(), "size": 24 if i else 12,
              "wrap_width": 1800, "color": "#ffffff", "background": "#000000"} for i in range(4)]
    cfg["sources"] = [*browsers, *texts, {"id": "bars", "kind": "test_pattern", "width": W, "height": H, "fps": F},
                      {"id": "tone", "kind": "tone"}]
    cfg["recording"]["iso"]["sources"] = []
    cols = max(1, len(browsers))
    items = [{"source": "bars", "rect": full(c)}]
    items += [{"source": b["id"], "rect": {"x": i * W // cols, "y": 0, "width": W // cols, "height": H // 3}}
              for i, b in enumerate(browsers)]
    items += [{"source": t["id"], "rect": {"x": i * W // 4, "y": H // 3, "width": W // 4, "height": H // 3}}
              for i, t in enumerate(texts)]
    items += [{"id": f"live{i}", "content": {"kind": "text", "data": f"key{i}", "size": 40, "width": W // 2,
                                              "height": 60},
               "rect": {"x": (i % 2) * W // 2, "y": H - 120 + (i // 2) * 60, "width": W // 2, "height": 60}}
              for i in range(4)]
    cfg["scenes"] = [{"id": "main", "name": "Main", "items": items}]
    c.start_baseline(cfg)
    time.sleep(3)
    gray = e2e.snapshot(c.e, "browsers")
    top = gray[: len(gray) // 3]
    c.numbers["browser_band_mean"] = round(sum(top) / len(top), 1) if top else None
    sets = 0

    def during(deadline):
        nonlocal sets
        while time.monotonic() < deadline:
            c.call("data_set", {"values": {f"key{i}": f"{sets} {LOREM[: 20 + sets % 40]}" for i in range(4)}})
            sets += 1
            time.sleep(0.05)
    record_for(c, p["seconds"], during)
    c.numbers["data_sets"] = sets
    c.finish_end()
    c.audit()


def scenario_disk(c):
    image = c.root / "small.sparseimage"
    mount = c.root / "disk"
    mount.mkdir()
    e2e.command(["hdiutil", "create", "-size", "2g", "-type", "SPARSE", "-fs", "HFS+J", "-volname",
                 "IsoStress", str(c.root / "small")], timeout=120)
    e2e.command(["hdiutil", "attach", "-nobrowse", "-noautoopen", "-mountpoint", str(mount), str(image)],
                timeout=120)
    (c.root / "mount.txt").write_text(str(mount))
    keep = 0.5
    filler = mount / "filler.bin"
    refusals = (WRONG_STATE, -32602, -32000, -32002, -32003)
    try:
        cfg = base_setup(c, iso=True)
        cfg["recording"]["folder"] = str(mount / "recordings")
        cfg["recording"]["keep_free_gb"] = keep
        c.start_baseline(cfg)

        def free():
            v = os.statvfs(mount)
            return v.f_bavail * v.f_frsize / 1e9
        c.numbers["free_gb_at_start"] = round(free(), 3)
        # Takes too long for the disk are refused up front, naming the field;
        # the longest one that fits starts.
        started = None
        reasons = collections.Counter()
        for seconds in (3600, 600, 120, 90, 60, 45, 30, 20):
            # Each size twice in a row: a refusal must leave the recorder idle.
            for _ in range(2):
                try:
                    started = c.call("start", {"seconds": seconds}, ok_codes=refusals)
                    c.numbers["timed_take_seconds"] = seconds
                    break
                except RpcError as exc:
                    text = exc.error.get("message", "")
                    reasons["keep_free_gb" if "keep_free_gb" in text else text[:80]] += 1
                    c.numbers.setdefault("refused", {})[seconds] = text[-160:]
                    if "keep_free_gb" not in text:
                        c.problem(f"a {seconds} s take right after a refused one was refused with: {text[:160]}")
            if started:
                break
        c.numbers["refusal_reasons"] = dict(reasons)
        folders = [d for d in (mount / "recordings").iterdir() if d.is_dir()]
        c.numbers["project_folders_after_refusals"] = len(folders)
        require(started, "no timed take fits a 2 GB disk")
        c.errors.clear()
        # Another program fills the disk under it, past keep_free_gb: the
        # recording stops by itself at once, every file finished.
        watch = e2e.Watch(c.e, ["stopped", "warning"])
        time.sleep(3)
        t0 = time.monotonic()
        lowest = free()
        try:
            with open(filler, "wb") as f:
                chunk = bytes(8 << 20)
                while free() > keep - 0.1:
                    f.write(chunk)
                    f.flush()
                lowest = min(lowest, free())
            stopped = until(lambda: watch.of("stopped"), 60,
                            "the recording to stop as free space went under keep_free_gb")
            c.numbers["stopped_after_fill_seconds"] = round(time.monotonic() - t0, 1)
            while time.monotonic() - t0 < 5:
                lowest = min(lowest, free())
                time.sleep(0.1)
        finally:
            time.sleep(0.5)
            hang_up(watch.sock, watch.reader)
        c.numbers["free_gb_lowest"] = round(lowest, 3)
        c.numbers["warnings"] = [w.get("message", "")[:200] for w in watch.of("warning")][:5]
        c.numbers["stop_event"] = json.dumps(stopped[0])[:600]
        c.idle(120)
        if c.numbers["stopped_after_fill_seconds"] > c.numbers["timed_take_seconds"] - 5:
            c.problem("the take ran to its end instead of stopping when free space went under keep_free_gb")
        if lowest < 0.05:
            c.problem(f"the disk ran out: {lowest:.3f} GB free at the lowest")
        # The next start is refused while there is no room.
        try:
            c.call("start", {"seconds": 10}, ok_codes=refusals)
            c.problem(f"a start with {free():.2f} GB free and keep_free_gb {keep} was allowed")
            c.call("stop")
            c.idle(120)
        except RpcError as exc:
            c.numbers["next_start_refused"] = exc.error.get("message", "")[-200:]
        c.errors.clear()
        filler.unlink()
        c.finish_end()
        c.audit(folders=[mount / "recordings"], early_stop_ok=True)
    finally:
        try:
            c.e.stop()
        finally:
            e2e.command(["hdiutil", "detach", "-force", str(mount)], check=False, timeout=60)


# ---------------------------------------------------------------- workers

def worker(args):
    case = args.worker_root
    caps = e2e.read_json(case.parent / "capabilities.json")
    prof = PROFILES[caps["stress_profile"]]
    started = time.monotonic()
    engine = ctx = None
    link = None
    status, reason, trace = "PASS", "", None
    extra = {}
    try:
        root = case
        if len(os.fsencode(case / "d.sock")) >= 100:
            # macOS caps Unix socket paths at 104 bytes: run the engine through a short link.
            link = Path("/tmp") / f"isocast-stress-{os.getpid()}"
            link.symlink_to(case)
            root = link
        engine = e2e.Engine(root, args.daemon)
        ctx = Ctx(args.worker, root, engine, caps, prof)
        globals()["scenario_" + args.worker](ctx)
    except Exception as exc:  # noqa: BLE001
        status, reason = "FAIL", f"{type(exc).__name__}: {exc}"[:600]
        trace = traceback.format_exc()
        (case / "failure.txt").write_text(trace)
    finally:
        if engine:
            alive = engine.process.poll() is None
            if not alive and args.worker != "disk":
                extra["crash"] = f"isocastd exited with {engine.process.returncode}"
            elif alive:
                try:
                    engine.rpc("shutdown", timeout=15)
                    code = engine.process.wait(timeout=60)
                    if code:
                        extra["crash"] = f"isocastd exited with {code} at shutdown"
                except (OSError, ValueError, Failure, subprocess.TimeoutExpired) as exc:
                    extra["crash"] = f"isocastd did not shut down cleanly: {exc!r}"
            engine.close()
        if ctx:
            ctx.stop_sampling.set()
            extra.update(ctx.result())
        problems = extra.get("problems", [])
        if extra.get("crash"):
            problems.append(extra["crash"])
        for hit in soak.scan_logs(case):
            problems.append(f"panic in the logs: {hit}"[:300])
        growth = extra.get("growth") or {}
        for key, limit in (("footprint_mb", args.max_memory_growth_mb), ("threads", args.max_thread_growth),
                           ("fds", args.max_fd_growth)):
            if growth.get(key) is not None and growth[key] > limit:
                problems.append(f"{key} grew {growth[key]} (limit {limit}) from before the load to after it")
        extra["problems"] = problems
        if status == "PASS" and problems:
            status, reason = "FAIL", "; ".join(problems[:3])[:600]
        if status == "PASS":
            reason = "verified"
        if link:
            link.unlink(missing_ok=True)
        e2e.write_json(case / "result.json", {"name": args.worker, "status": status, "reason": reason,
                                               "seconds": round(time.monotonic() - started, 1),
                                               "trace": trace, **extra})
    return 1 if status == "FAIL" else 0


def detach(case):
    mount = case / "mount.txt"
    if mount.exists():
        e2e.command(["hdiutil", "detach", "-force", mount.read_text().strip()], check=False, timeout=60)


def run_case(args, root, name):
    case = root / name
    case.mkdir()
    print(f"stress: {name}…", file=sys.stderr, flush=True)
    with open(case / "worker.log", "wb") as log:
        process = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), "--worker", name,
                                    "--worker-root", str(case), "--daemon", str(args.daemon.resolve()),
                                    "--max-memory-growth-mb", str(args.max_memory_growth_mb),
                                    "--max-thread-growth", str(args.max_thread_growth),
                                    "--max-fd-growth", str(args.max_fd_growth)],
                                   stdout=log, stderr=log, start_new_session=True)
        try:
            process.wait(timeout=args.timeout)
            result = (e2e.read_json(case / "result.json") if (case / "result.json").exists() else
                      {"name": name, "status": "FAIL", "reason": f"worker exited {process.returncode} without report"})
        except subprocess.TimeoutExpired:
            result = {"name": name, "status": "FAIL", "reason": f"scenario timed out after {args.timeout:.0f}s"}
        finally:
            e2e.kill_group(process)
            detach(case)
            Path(f"/tmp/isocast-stress-{process.pid}").unlink(missing_ok=True)
    for log_name in ("daemon.log", "worker.log", "failure.txt"):
        if (case / log_name).exists():
            shutil.copy2(case / log_name, args.out / f"{name}-{log_name}")
    for logs in (case / "logs",):
        if logs.is_dir():
            shutil.copytree(logs, args.out / f"{name}-logs", dirs_exist_ok=True)
    keep = args.keep or (args.keep_failures and result["status"] == "FAIL")
    result["artifacts"] = str(case) if keep else None
    print(f"{result['status']:4} {name}: {result['reason']} ({result.get('seconds', 0):.0f} s)", flush=True)
    if not keep:
        shutil.rmtree(case, ignore_errors=True)
    return result


def cell(value):
    return str(value).replace("|", "/").replace("\n", " ")[:300]


def summary(report):
    caps = report.get("capabilities", {})
    lines = [f"# Stress suite: {report.get('verdict')}", "",
             f"{caps.get('chip')}, {caps.get('system')}; profile {report.get('profile')} "
             f"({report.get('settings', {}).get('width')}x{report.get('settings', {}).get('height')}@"
             f"{report.get('settings', {}).get('fps')}, {caps.get('record_codec')}); daemon {report.get('daemon_sha256', '')[:12]}",
             "", "| scenario | result | s | takes/files | worst drops (program / stream / ISO) | API p50 / p99 ms "
             "(slowest method) | growth MB / threads / fds | reason |", "|---|---|---|---|---|---|---|---|"]
    for r in report.get("scenarios", []):
        outs = r.get("outputs") or []
        def worst(kind):
            rows = [o for o in outs if o["kind"] == kind]
            if not rows:
                return "-"
            o = max(rows, key=lambda o: (o["dropped"], o["slots"]))
            return f"{o['dropped']}/{o['slots']}"
        lat = {m: v for m, v in (r.get("latency") or {}).items() if v}
        slow = max(lat.items(), key=lambda kv: kv[1]["p99_ms"]) if lat else None
        allp = [v for v in lat.values()]
        api = (f"{statistics.median(v['p50_ms'] for v in allp):.1f} / {slow[1]['p99_ms']:.1f} ({slow[0]})"
               if slow else "-")
        g = r.get("growth") or {}
        growth = f"{g.get('footprint_mb', '-')} / {g.get('threads', '-')} / {g.get('fds', '-')}" if g else "-"
        n = r.get("numbers") or {}
        lines.append(f"| {r['name']} | {r['status']} | {r.get('seconds', 0):.0f} | {n.get('takes', '-')}/"
                     f"{n.get('files_verified', '-')} | {worst('program')} / {worst('stream')} / {worst('iso')} | "
                     f"{api} | {growth} | {cell(r.get('reason'))} |")
    for field in ("error",):
        if report.get(field):
            lines += ["", f"Harness error: {cell(report[field])}"]
    lines += ["", "## Numbers", ""]
    for r in report.get("scenarios", []):
        lines.append(f"### {r['name']}: {r['status']}")
        lines.append("")
        for k, v in (r.get("numbers") or {}).items():
            lines.append(f"- {k}: {cell(json.dumps(v) if not isinstance(v, str) else v)}")
        for note in r.get("notes") or []:
            lines.append(f"- note: {cell(note)}")
        lat = r.get("latency") or {}
        if lat:
            lines.append("- latency ms (n, p50, p99, max): " + "; ".join(
                f"{m} {v['n']}, {v['p50_ms']}, {v['p99_ms']}, {v['max_ms']}" for m, v in lat.items() if v))
        if r.get("errors"):
            lines.append(f"- API errors: {cell(json.dumps(r['errors']))}")
        if r.get("baseline"):
            lines.append(f"- footprint MB / threads / fds: before {r['baseline']}, peak {r.get('peak')}, after {r.get('end')}")
        for prob in (r.get("problems") or [])[:12]:
            lines.append(f"- PROBLEM: {cell(prob)}")
        lines.append("")
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    target = Path(os.environ.get("CARGO_TARGET_DIR", e2e.REPO / "target"))
    scratch = Path(os.environ.get("ISOCAST_SCRATCH", "/Volumes/IsocastBuild" if
                                 Path("/Volumes/IsocastBuild").exists() else tempfile.gettempdir()))
    parser.add_argument("--daemon", type=Path, default=target / "release/isocastd")
    parser.add_argument("--work", type=Path, default=scratch / "stress")
    parser.add_argument("--out", type=Path, default=Path("stress-out"))
    parser.add_argument("--scenario", "--only", action="append", choices=SCENARIOS, dest="scenario")
    parser.add_argument("--profile", choices=sorted(PROFILES), default="hardware")
    parser.add_argument("--codec", choices=("hevc", "h264", "prores422"))
    parser.add_argument("--keep", action="store_true")
    parser.add_argument("--keep-failures", action="store_true")
    parser.add_argument("--timeout", type=float, default=WORKER_TIMEOUT, help="seconds per scenario")
    parser.add_argument("--max-memory-growth-mb", type=float, default=300)
    parser.add_argument("--max-thread-growth", type=float, default=24)
    parser.add_argument("--max-fd-growth", type=float, default=32)
    parser.add_argument("--summary", type=Path, help="render a saved report.json as Markdown and exit")
    parser.add_argument("--list", action="store_true")
    parser.add_argument("--worker", choices=SCENARIOS, help=argparse.SUPPRESS)
    parser.add_argument("--worker-root", type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.summary:
        print(summary(e2e.read_json(args.summary)), end="")
        return 0
    if args.list:
        print("\n".join(SCENARIOS))
        return 0
    if args.worker:
        return worker(args)
    signal.signal(signal.SIGTERM, e2e.interrupted)
    signal.signal(signal.SIGINT, e2e.interrupted)
    args.out = args.out.resolve()
    args.out.mkdir(parents=True, exist_ok=True)
    report = {"version": 1, "profile": args.profile, "settings": PROFILES[args.profile], "scenarios": [],
              "started_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
    root = None
    try:
        require(sys.platform == "darwin", "macOS is required")
        for tool in ("ffmpeg", "ffprobe", "codesign", "hdiutil"):
            require(shutil.which(tool), f"missing required tool: {tool}")
        require(args.daemon.is_file(), f"daemon not found: {args.daemon}; build release first")
        report["daemon_sha256"] = e2e.hashlib.sha256(args.daemon.read_bytes()).hexdigest()
        scratch.mkdir(parents=True, exist_ok=True)
        with open(scratch / ".bench.lock", "a") as lock:
            print("stress: waiting for the benchmark lock", file=sys.stderr, flush=True)
            fcntl.flock(lock, fcntl.LOCK_EX)
            try:
                caps = e2e.capabilities()
            except subprocess.TimeoutExpired as exc:
                # A wedged VideoToolbox (seen in the shared VM) hangs its probe.
                caps = {"system": e2e.platform.platform(), "machine": e2e.platform.machine(),
                        "hevc": False, "h264": False, "probe_error": str(exc)[:300]}
            caps["record_codec"] = args.codec or ("hevc" if caps["hevc"] else "h264" if caps["h264"] else "prores422")
            caps["stress_profile"] = args.profile
            report["capabilities"] = caps
            args.work.mkdir(parents=True, exist_ok=True)
            root = Path(tempfile.mkdtemp(prefix="st-", dir=args.work)).resolve()
            e2e.write_json(root / "capabilities.json", caps)
            report["work"] = str(root)
            for name in args.scenario or SCENARIOS:
                report["scenarios"].append(run_case(args, root, name))
                e2e.write_json(args.out / "report.json", report)
    except Exception as exc:  # noqa: BLE001
        report["error"] = str(exc)
        print(f"FAIL harness: {exc}", file=sys.stderr)
    finally:
        if root and not args.keep and not any(r.get("artifacts") for r in report["scenarios"]):
            shutil.rmtree(root, ignore_errors=True)
        report["totals"] = {s: sum(r["status"] == s for r in report["scenarios"]) for s in ("PASS", "FAIL")}
        report["verdict"] = "PASS" if not report.get("error") and not report["totals"]["FAIL"] else "FAIL"
        report["finished_at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        e2e.write_json(args.out / "report.json", report)
        (args.out / "summary.md").write_text(summary(report))
        print(f"stress {report['verdict']}: {report['totals']}; {args.out / 'summary.md'}", flush=True)
    return 0 if report["verdict"] == "PASS" else 1


if __name__ == "__main__":
    sys.exit(main())
