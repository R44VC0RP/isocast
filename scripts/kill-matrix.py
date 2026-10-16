#!/usr/bin/env python3
"""Crash-safe recording kill matrix (PLAN G.3b); Python stdlib + ffmpeg/ffprobe.

Run on a test Mac, never on Ryan's:

    python3 scripts/kill-matrix.py --bin-dir BIN --work DIR [all|matrix|ownership]
        [--runs 21] [--seed 3013] [--modes plain,start,pause,split,resume,stop,recover]
        [--crash-safe default|on|off] [--report FILE]

BIN holds a release `isocastd`. Each run gets a fresh isolated daemon (its
own ad-hoc signed bundle copy, socket, setup, timelines, logs and recordings
folder under DIR; inherited ISOCAST_* variables dropped) recording synthetic
sources only (a test pattern and two tones) into the setup's recordings
folder, which puts every take in a project folder (`<root>/<date> Untitled/`
with `.isocast-project.json`).

matrix: each run records a program plus an ISO (codec and container rotate
through hevc/h264/prores422 and mov/mp4), then SIGKILLs the daemon at a
random moment in one of these modes:

    plain    while recording
    start    0-0.8 s after the start reply (half within 50 ms, before the
             first picture can be in a fragment)
    pause    while paused
    split    just after a split's new take begins
    resume   after a pause and resume
    stop     while a Stop is in flight (finish and header patch)
    recover  while recording, then again while the next launch repairs

Then the next launch must repair the take in place, and the run fails when:
a take is outside its project folder; a manifest stays "recording"; a media
file vanishes, fails ffprobe, a full `ffmpeg -xerror` decode, or (when
swiftc is present) an AVFoundation read of every sample; any track of a
repaired file holds fewer packets than the killed file had complete on disk
(every frame per track is kept, nothing is trimmed to the common A/V end);
a track ends before the `committed_seconds` the last status promised; a
track ends further before the recording's elapsed time at the kill than
its budget: video --interval + 9 frames (8 in flight in the encoder plus the
held-back picture), audio --interval + 2112 + 1024 samples at 48 kHz (AAC
priming plus one packet) + --mixer-latency, --pause-loss while paused (each
loss is logged under "losses"); a further launch changes any repaired file;
or, unless --crash-safe off, the writer never reported committed_seconds (it
isn't the crash-safe writer). A file that is only the writer's placeholder
(ftyp plus a 16-byte free box) both when killed and after repair, with no
committed promise, is an explicit no-media case ("no_media"), not a failure;
a killed file that held packets and is lost still fails.

ownership: the next-launch repair must touch only its own takes. A killed
take is copied elsewhere and offered to a fresh daemon through a symlink, a
hard link, a symlinked folder, a hard link to a still-open file of a live
(SIGSTOPped) recorder, that recorder's file renamed into the other daemon's
folder (so the recorder's own lock, not the link-count guard, must protect
it), and ffmpeg-made movies, each named by a manifest left
"recording" or "failed"; none of those files may change. The control, the
same take in the daemon's own recordings folder, must be repaired.

Every run and case writes one JSON line to --report (default
DIR/report.jsonl), then a summary line. Exit status: 0 all passed, 1 any
failure, 2 usage or environment error. Nothing is deleted on reuse: a
--work that already holds runs/ (matrix) or own/ (ownership) is refused. Each run holds the machine-wide
benchmark lock ($ISOCAST_SCRATCH/.bench.lock, default
/Volumes/IsocastBuild/.bench.lock) when its folder exists.
"""

import argparse
import contextlib
import fcntl
import hashlib
import json
import os
import random
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time
from datetime import datetime
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
MEDIA = (".mov", ".mp4")
CODECS = [("hevc", "mov"), ("h264", "mp4"), ("prores422", "mov"), ("hevc", "mp4"), ("h264", "mov")]
ALL_MODES = ["plain", "start", "pause", "split", "resume", "stop", "recover"]

# AVFoundation's view: open like an editor, decode every sample of every track.
AVDECODE_SWIFT = r"""
import AVFoundation
import CoreVideo
import Foundation

@main struct Probe {
  static func main() async {
    var rows: [[String: Any]] = []
    for path in CommandLine.arguments.dropFirst() {
      var row: [String: Any] = ["path": path]
      do {
        let asset = AVURLAsset(url: URL(fileURLWithPath: path))
        row["playable"] = try await asset.load(.isPlayable)
        row["duration"] = try await asset.load(.duration).seconds
        var results: [[String: Any]] = []
        for track in try await asset.load(.tracks) {
          let type = track.mediaType
          guard type == .audio || type == .video else { continue }
          let reader = try AVAssetReader(asset: asset)
          let settings: [String: Any] = type == .audio
            ? [AVFormatIDKey: kAudioFormatLinearPCM, AVLinearPCMIsFloatKey: true, AVLinearPCMBitDepthKey: 32]
            : [kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA]
          let out = AVAssetReaderTrackOutput(track: track, outputSettings: settings)
          reader.add(out)
          _ = reader.startReading()
          var count = 0
          var end = 0.0
          while let sample = out.copyNextSampleBuffer() {
            let pts = CMSampleBufferGetPresentationTimeStamp(sample).seconds
            let duration = CMSampleBufferGetDuration(sample).seconds
            end = max(end, pts + (duration.isFinite && duration > 0 ? duration : 0))
            count += 1
          }
          results.append(["type": type.rawValue, "status": reader.status.rawValue,
            "error": reader.error?.localizedDescription ?? "", "buffers": count, "end": end])
        }
        row["tracks"] = results
      } catch { row["error"] = error.localizedDescription }
      rows.append(row)
    }
    let data = try! JSONSerialization.data(withJSONObject: rows, options: [.sortedKeys])
    print(String(data: data, encoding: .utf8)!)
  }
}
"""


class Abort(Exception):
    """An environment or harness problem (exit 2), not a recording failure."""


# --- helpers ---------------------------------------------------------------

def command(args, timeout=600, env=None):
    out = subprocess.run([str(a) for a in args], capture_output=True, text=True,
                         timeout=timeout, env=env)
    return {"rc": out.returncode, "stdout": out.stdout, "stderr": out.stderr[-2000:]}


def media_in(root):
    """Every media file under the recordings root (flat and in project folders)."""
    return sorted(p for p in Path(root).rglob("*") if p.suffix in MEDIA and not p.is_symlink()
                  and p.is_file())


def manifests_in(root):
    return sorted(p for p in Path(root).rglob("*.session.json") if not p.is_symlink())


def read_json(path):
    try:
        return json.loads(Path(path).read_text())
    except (OSError, ValueError):
        return None


def fingerprint(path):
    p = Path(path)
    st = p.stat()
    return [hashlib.sha256(p.read_bytes()).hexdigest(), st.st_size, st.st_mtime_ns]


@contextlib.contextmanager
def bench_lock(enabled):
    scratch = Path(os.environ.get("ISOCAST_SCRATCH", "/Volumes/IsocastBuild"))
    if not enabled or not scratch.is_dir():
        yield
        return
    with open(scratch / ".bench.lock", "a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            print("kill-matrix: waiting for the benchmark lock", file=sys.stderr, flush=True)
            fcntl.flock(lock, fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(lock, fcntl.LOCK_UN)


def setup_toml(folder, codec, container, fps):
    w, h = 640, 360
    encoder = f'{{ codec = "{codec}", container = "{container}", quality = 70 }}'
    return f"""version = 1
name = "kill matrix"

[canvas]
width = {w}
height = {h}
fps = {fps}

[[sources]]
id = "bars"
kind = "test_pattern"
width = {w}
height = {h}
fps = {fps}

[[sources]]
id = "a"
kind = "tone"

[[sources]]
id = "b"
kind = "tone"

[[scenes]]
id = "main"
name = "Main"
items = [{{ source = "bars", rect = {{ x = 0, y = 0, width = {w}, height = {h} }} }}]

[recording]
folder = "{folder}"
keep_free_gb = 0.0

[recording.program]
enabled = true
encoder = {encoder}
audio = ["a", "b"]

[recording.iso]
enabled = true
sources = ["bars"]
encoder = {encoder}
audio = ["a", "b"]
"""


# --- probing ---------------------------------------------------------------

def placeholder(path):
    """The writer's file before its first fragment: ftyp, then a 16-byte free box, nothing else."""
    try:
        data = Path(path).read_bytes()[:4096]
        size = Path(path).stat().st_size
    except OSError:
        return False
    if len(data) < 8 or data[4:8] != b"ftyp":
        return False
    head = int.from_bytes(data[:4], "big")
    return (size == head + 16 and len(data) >= head + 8 and data[head + 4:head + 8] == b"free"
            and int.from_bytes(data[head:head + 4], "big") == 16)


def number(value, default):
    try:
        return int(value)
    except (TypeError, ValueError):
        return default


def tracks(path):
    """Per-track packet counts and ends from ffprobe; only packets wholly in the file."""
    size = Path(path).stat().st_size
    out = command(["ffprobe", "-v", "error", "-show_entries",
                   "stream=index,codec_type,codec_name:packet=stream_index,pts_time,duration_time,pos,size",
                   "-of", "json", path])
    if out["rc"] != 0:
        return None, out["stderr"].strip()[-400:]
    data = json.loads(out["stdout"] or "{}")
    result = []
    for s in data.get("streams", []):
        if s.get("codec_type") not in ("video", "audio"):
            continue
        packets = [p for p in data.get("packets", []) if p.get("stream_index") == s["index"]
                   and number(p.get("pos"), -1) + number(p.get("size"), 0) <= size]
        ends = [float(p.get("pts_time") or 0) + float(p.get("duration_time") or 0) for p in packets]
        result.append({"type": s["codec_type"], "codec": s.get("codec_name"),
                       "packets": len(packets), "end": round(max(ends), 6) if ends else None})
    return result, ""


def decode(path):
    out = command(["ffmpeg", "-nostdin", "-v", "error", "-xerror", "-err_detect", "explode",
                   "-i", path, "-map", "0:v?", "-map", "0:a?", "-f", "null", "-"])
    return out["rc"] == 0, out["stderr"].strip()[-400:]


def avfoundation(binary, paths):
    if binary is None or not paths:
        return {}
    out = command([binary] + [str(p) for p in paths])
    try:
        return {r["path"]: r for r in json.loads(out["stdout"])}
    except ValueError:
        return {str(p): {"error": (out["stderr"] or out["stdout"])[-400:]} for p in paths}


def build_avdecode(work):
    if shutil.which("swiftc") is None:
        return None
    src = work / "avdecode.swift"
    binary = work / "avdecode"
    src.write_text(AVDECODE_SWIFT)
    out = command(["swiftc", "-O", "-parse-as-library", "-o", binary, src], timeout=600)
    if out["rc"] != 0:
        raise Abort(f"swiftc avdecode failed (use --no-avfoundation to skip):\n{out['stderr']}")
    return binary


# --- the isolated daemon ---------------------------------------------------

class RpcError(Exception):
    pass


class Engine:
    """One isolated daemon process over a run's directory."""

    def __init__(self, harness, root, crash_safe, extra_env=None):
        self.root = Path(root)
        self.root.mkdir(parents=True, exist_ok=True)
        self.sock = self.root / "d.sock"
        if len(os.fsencode(self.sock)) >= 104:
            raise Abort(f"--work path too long for a macOS Unix socket: {self.sock}")
        self.binary = harness.app / "Contents/MacOS/isocastd"
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("ISOCAST_")}
        self.env.update(ISOCAST_SOCKET=str(self.sock), ISOCAST_SETUP=str(self.root / "setup.toml"),
                        ISOCAST_DEV_APP=str(harness.app), ISOCAST_PLUGINS=str(self.root / "plugins"),
                        ISOCAST_TIMELINES=str(self.root / "timelines"), ISOCAST_MONITOR_SILENT="1",
                        ISOCAST_LOG_DIR=str(self.root / "logs"),
                        ISOCAST_REPORT_ENDPOINT="http://127.0.0.1:9/reports")
        if crash_safe == "on":
            self.env["ISOCAST_CRASH_SAFE"] = "1"
        elif crash_safe == "off":
            self.env["ISOCAST_CRASH_SAFE"] = "0"
        self.env.update(extra_env or {})
        self.process = None
        self.log = None
        self.committed_seen = False
        self.max_elapsed = 0.0

    def spawn(self):
        if self.log is None or self.log.closed:
            self.log = open(self.root / "daemon.log", "ab", buffering=0)
        self.process = subprocess.Popen([str(self.binary)], env=self.env, stdout=self.log,
                                        stderr=self.log)
        return self.process

    def launch(self, timeout=60):
        self.spawn()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise RpcError(f"daemon exited at launch ({self.process.returncode}); see {self.root}/daemon.log")
            try:
                return self.rpc("status", timeout=1)
            except (OSError, RpcError, ValueError):
                time.sleep(0.03)
        raise RpcError("daemon did not answer status")

    def rpc(self, method, params=None, timeout=30):
        request = {"jsonrpc": "2.0", "id": 1, "method": method}
        if params is not None:
            request["params"] = params
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            s.settimeout(timeout)
            s.connect(str(self.sock))
            s.sendall((json.dumps(request) + "\n").encode())
            with s.makefile("rb") as reader:
                line = reader.readline(64 * 1024 * 1024)
        if not line:
            raise RpcError(f"{method}: daemon closed the connection")
        reply = json.loads(line)
        if "error" in reply:
            raise RpcError(f"{method}: {reply['error']}")
        return reply.get("result")

    def status(self):
        st = self.rpc("status", timeout=5)
        live = st.get("live") or {}
        self.max_elapsed = max(self.max_elapsed, live.get("elapsed_seconds") or 0)
        if any("committed_seconds" in f for f in live.get("files") or []):
            self.committed_seen = True
        return st

    def kill(self):
        if self.process and self.process.poll() is None:
            self.process.send_signal(signal.SIGKILL)
        if self.process:
            self.process.wait(timeout=10)
        return time.monotonic()

    def close(self):
        """Never leave this daemon running (or stopped with SIGSTOP)."""
        try:
            self.kill()
        except (OSError, subprocess.TimeoutExpired):
            pass
        if self.log:
            self.log.close()

    def shutdown(self):
        if self.process and self.process.poll() is None:
            try:
                self.rpc("shutdown", timeout=15)
                self.process.wait(timeout=20)
            except (OSError, RpcError, ValueError, subprocess.TimeoutExpired):
                self.kill()
        if self.log:
            self.log.close()

    def wait_recording(self, timeout=30):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            st = self.status()
            if st.get("state") == "recording" and (st.get("live") or {}).get("elapsed_seconds", 0) > 0:
                return st
            time.sleep(0.02)
        raise RpcError("the recording didn't start")

    def wait_media(self, target, timeout=60):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            st = self.status()
            if ((st.get("live") or {}).get("elapsed_seconds") or 0) >= target:
                return st
            time.sleep(0.02)
        raise RpcError(f"elapsed never reached {target}")


class Harness:
    def __init__(self, args):
        self.args = args
        self.work = Path(args.work).expanduser().resolve()
        self.work.mkdir(parents=True, exist_ok=True)
        daemon = Path(args.bin_dir).expanduser().resolve() / "isocastd"
        if not daemon.is_file():
            raise Abort(f"no isocastd in --bin-dir {args.bin_dir}")
        for tool in ("ffmpeg", "ffprobe"):
            if shutil.which(tool) is None:
                raise Abort(f"{tool} not on PATH")
        # One ad-hoc signed bundle copy for every daemon (synthetic sources need no grants).
        self.app = self.work / "Dev.app"
        binary = self.app / "Contents/MacOS/isocastd"
        binary.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(daemon, binary)
        shutil.copy2(REPO / "crates/isocast-daemon/Info.plist", self.app / "Contents/Info.plist")
        for step in (["codesign", "--force", "--sign", "-", self.app], ["codesign", "--verify", self.app]):
            out = command(step)
            if out["rc"] != 0:
                raise Abort(f"{' '.join(map(str, step))} failed: {out['stderr']}")
        self.daemon_sha = hashlib.sha256(binary.read_bytes()).hexdigest()
        self.avdecode = None if args.no_avfoundation else build_avdecode(self.work)
        self.report = Path(args.report).expanduser() if args.report else self.work / "report.jsonl"
        self.failures = 0
        self.rows = 0

    def write(self, row):
        failed = bool(row.get("failures"))
        self.failures += failed
        self.rows += 1
        with open(self.report, "a") as log:
            log.write(json.dumps(row, default=str) + "\n")
        label = row.get("run", row.get("case"))
        print(f"{row['kind']} {label}: {'FAIL ' + '; '.join(row['failures'])[:600] if failed else 'ok'}",
              flush=True)

    def probe(self, paths):
        av = avfoundation(self.avdecode, paths)
        report = {}
        for p in paths:
            t, err = tracks(p)
            ok, derr = decode(p)
            report[str(p)] = {"size": p.stat().st_size, "tracks": t, "probe_error": err,
                              "decodes": ok, "decode_error": derr, "av": av.get(str(p))}
        return report


# --- the kill matrix -------------------------------------------------------

def plan(args):
    rng = random.Random(args.seed)
    modes = args.modes.split(",")
    unknown = [m for m in modes if m not in ALL_MODES]
    if unknown:
        raise Abort(f"--modes: unknown {unknown}; valid: {','.join(ALL_MODES)}")
    runs = []
    for i in range(args.runs):
        mode = modes[i % len(modes)]
        codec, container = CODECS[(i // len(modes) + i) % len(CODECS)]
        early = rng.random() < 0.5
        at = {"start": rng.uniform(0.0, 0.05) if early else rng.uniform(0.05, 0.8), "split": rng.uniform(1.5, 4.0),
              "stop": rng.uniform(1.0, 6.0), "recover": rng.uniform(1.0, 6.0)}.get(mode, rng.uniform(1.0, 7.0))
        runs.append({"run": i, "mode": mode, "codec": codec, "container": container,
                     "at": round(at, 3), "delay": round(rng.random(), 4)})
    return runs


def snapshot(st):
    live = (st or {}).get("live") or {}
    return {"elapsed": live.get("elapsed_seconds"), "paused": live.get("paused"),
            "files": [{k: f.get(k) for k in ("source", "frames", "dropped_frames", "committed_seconds")}
                      for f in live.get("files") or []]}


def kill_run(h, spec):
    args = h.args
    root = h.work / "runs" / f"{spec['run']:03d}-{spec['mode']}"
    root.mkdir(parents=True)  # refuses an existing run folder: never reuse or delete
    rec = root / "rec"
    rec.mkdir()
    (root / "setup.toml").write_text(setup_toml(rec, spec["codec"], spec["container"], args.fps))
    row = dict(spec, kind="kill", crash_safe=args.crash_safe, daemon_sha256=h.daemon_sha,
               folder=str(root))
    failures = []
    eng = Engine(h, root, args.crash_safe)
    engines = [eng]
    mode, at, delay = spec["mode"], spec["at"], spec["delay"]
    try:
        eng.launch()
        started = eng.rpc("start", {})
        current = [str(Path(p).resolve()) for p in started.get("files", [])]
        row["started_files"] = current
        if mode == "start":
            # From the start reply, without waiting for the first picture.
            time.sleep(at)
        else:
            eng.wait_recording()
            eng.wait_media(at)
        if mode == "pause":
            t0 = time.monotonic()
            row["pause_reply"] = eng.rpc("pause")
            row["pause_ms"] = round((time.monotonic() - t0) * 1000, 1)
            time.sleep(0.2 + delay * 0.5)
        elif mode == "resume":
            row["pause_reply"] = eng.rpc("pause")
            time.sleep(0.3 + delay * 0.4)
            row["resume_reply"] = eng.rpc("resume")
            time.sleep(0.05 + delay * 1.5)
        elif mode == "split":
            reply = eng.rpc("split")
            row["split_reply"] = reply
            current = [str(Path(p).resolve()) for p in (reply or {}).get("files", [])] or current
            begins_in = 0.6
            try:
                # The API writes milliseconds; Python 3.9 needs an explicit UTC offset.
                text = reply["begins_at"].replace("Z", "+00:00")
                when = datetime.fromisoformat(text)
                begins_in = (when - datetime.now(when.tzinfo)).total_seconds()
            except (KeyError, TypeError, AttributeError, ValueError):
                pass
            row["split_begins_in"] = round(begins_in, 4)
            time.sleep(max(begins_in, 0) + delay * 0.3)
        before = eng.status()
        if mode == "stop":
            reply = {}

            def stop():
                try:
                    reply["stop"] = eng.rpc("stop", timeout=60)
                except (OSError, RpcError, ValueError) as exc:
                    reply["error"] = str(exc)
            thread = threading.Thread(target=stop)
            thread.start()
            time.sleep(delay * 0.12)
            eng.kill()
            thread.join(timeout=10)
            row["stop_reply"] = reply
        else:
            eng.kill()
        row["before_kill"] = snapshot(before)
        row["committed_seen"] = eng.committed_seen
        row["max_elapsed"] = eng.max_elapsed
        time.sleep(0.2)
        raw = {}
        for p in media_in(rec):
            t, _ = tracks(p)
            raw[str(p)] = {"size": p.stat().st_size, "tracks": t, "placeholder": placeholder(p)}
        row["raw"] = raw
        if mode == "recover":
            # Kill the next launch a moment into its repair.
            eng.spawn()
            time.sleep(0.02 + delay * 0.4)
            eng.kill()
            row["recover_kill_after_s"] = round(0.02 + delay * 0.4, 4)
            row["raw_mid"] = {str(p): p.stat().st_size for p in media_in(rec)}
        # Next launch repairs.
        t0 = time.monotonic()
        st = eng.launch()
        row["recovery_launch_ms"] = round((time.monotonic() - t0) * 1000)
        row["interrupted"] = [str(r.get("manifest")) for r in st.get("interrupted") or []]
        eng.shutdown()
        repaired = media_in(rec)
        prints = {str(p): fingerprint(p) for p in repaired}
        # A further launch has nothing left to do.
        eng2 = Engine(h, root, args.crash_safe)
        engines.append(eng2)
        eng2.launch()
        eng2.shutdown()
        changed = [p for p in prints if not Path(p).exists() or fingerprint(p) != prints[p]]
        if changed:
            failures.append(f"a second launch changed repaired files: {changed}")
    except (OSError, RpcError, ValueError, subprocess.TimeoutExpired) as exc:
        failures.append(f"harness: {exc}")
        row["failures"] = failures
        return row
    finally:
        for e in engines:
            e.close()

    row["manifests"] = {str(m): {k: (read_json(m) or {}).get(k) for k in ("status", "errors", "pauses")}
                        for m in manifests_in(rec)}
    row["files"] = h.probe(media_in(rec))
    check_run(row, rec, current, failures, args)
    row["failures"] = failures
    if not failures and args.clean:
        for p in media_in(rec):
            p.unlink()
    return row


def check_run(row, rec, current, failures, args):
    mode = row["mode"]
    # Layout: every take in a project folder directly inside the recordings folder.
    for p in list(row["files"]) + list(row["manifests"]):
        parent = Path(p).parent
        if parent.parent != rec or not (parent / ".isocast-project.json").exists():
            failures.append(f"{p} is not in a project folder of {rec}")
    if not row["manifests"]:
        failures.append("no take manifest")
    for m, info in row["manifests"].items():
        if info.get("status") == "recording":
            failures.append(f"{Path(m).name} still says recording after the next launch")
    for p, raw in row["raw"].items():
        if p not in row["files"] and any(t["packets"] for t in raw["tracks"] or []):
            failures.append(f"{Path(p).name} vanished at repair")
    before = row["before_kill"]
    elapsed = before.get("elapsed") or 0
    committed = [f["committed_seconds"] for f in before["files"] if f.get("committed_seconds") is not None]
    floor = min(committed) if committed and len(committed) == len(before["files"]) else None
    row["committed_floor"] = floor
    expect_media = (floor or 0) > 0
    promised = bool(committed) or row["committed_seen"]
    video_budget = args.interval + 9 / args.fps
    audio_budget = args.interval + (2112 + 1024) / 48000 + args.mixer_latency
    row["loss_budget"] = {"video": round(video_budget, 4), "audio": round(audio_budget, 4),
                          "paused": args.pause_loss}
    row["losses"] = {}
    row["no_media"] = []
    for p, f in row["files"].items():
        name = Path(p).name
        raw = row["raw"].get(p) or {}
        new_split = mode == "split" and p in current and not committed and all(
            f["frames"] == 0 for f in before["files"])
        if (not promised or new_split) and raw.get("placeholder") and placeholder(p):
            # Killed before any fragment: the placeholder stays, with nothing promised.
            row["no_media"].append(name)
            continue
        if f["tracks"] is None:
            failures.append(f"{name}: ffprobe failed: {f['probe_error']}")
            continue
        if not f["decodes"]:
            failures.append(f"{name}: full decode failed: {f['decode_error']}")
        av = f.get("av")
        if av is not None:
            if av.get("error") or not av.get("playable"):
                failures.append(f"{name}: AVFoundation: {av.get('error') or 'not playable'}")
            for t in av.get("tracks") or []:
                if t.get("status") != 2:
                    failures.append(f"{name}: AVFoundation {t.get('type')} read status {t.get('status')} {t.get('error')}")
        kinds = {t["type"] for t in f["tracks"] if t["packets"]}
        if p in current and expect_media and kinds != {"video", "audio"}:
            failures.append(f"{name}: has {sorted(kinds)} though {floor}s was committed")
        # Every frame per track: nothing the kill left complete on disk is dropped.
        raw = raw.get("tracks") or []
        for i, rt in enumerate(raw):
            if not rt["packets"]:
                continue  # Recovery omits empty tracks; no sample was lost.
            same = [t for t in f["tracks"] if t["type"] == rt["type"]]
            idx = [t for t in raw[:i] if t["type"] == rt["type"] and t["packets"]]
            got = same[len(idx)] if len(same) > len(idx) else None
            if got is None or got["packets"] < rt["packets"]:
                failures.append(f"{name}: {rt['type']} track kept {got and got['packets']} of the "
                                f"{rt['packets']} packets on disk at the kill")
        if p not in current:
            continue
        for t in f["tracks"]:
            end = t["end"] or 0
            if floor is not None and end < floor - args.committed_slack:
                failures.append(f"{name}: {t['type']} ends at {end}, before committed {floor}")
            if mode in ("plain", "pause", "resume", "stop", "recover"):
                loss = round(elapsed - end, 4)
                row["losses"].setdefault(name, {}).setdefault(t["type"], []).append(loss)
                bound = video_budget if t["type"] == "video" else audio_budget
                if before.get("paused"):
                    bound = min(bound, args.pause_loss)
                if loss > bound:
                    failures.append(f"{name}: {t['type']} ends at {end}, {round(elapsed - end, 3)}s "
                                    f"before the kill at {elapsed} (bound {bound})")
    if args.crash_safe != "off" and not row["committed_seen"] and row["max_elapsed"] >= 1.5:
        failures.append("no live file ever reported committed_seconds: not the crash-safe writer")


def matrix(h):
    if (h.work / "runs").exists():
        raise Abort(f"{h.work / 'runs'} exists; pass a fresh --work (nothing is deleted)")
    for spec in plan(h.args):
        print(f"run {spec['run']}: {spec['mode']} at {spec['at']} {spec['codec']}/{spec['container']}",
              file=sys.stderr, flush=True)
        with bench_lock(not h.args.no_lock):
            h.write(kill_run(h, spec))


# --- ownership of the next-launch repair -----------------------------------

def killed_take(h, name, seconds=2.6):
    """A real killed take (raw files + manifest), never relaunched."""
    root = h.work / "own" / name
    rec = root / "rec"
    rec.mkdir(parents=True)  # refuses an existing folder
    (root / "setup.toml").write_text(setup_toml(rec, "hevc", "mov", h.args.fps))
    eng = Engine(h, root, h.args.crash_safe)
    try:
        eng.launch()
        eng.rpc("start", {})
        eng.wait_recording()
        eng.wait_media(seconds)
    finally:
        eng.close()
    (manifest,) = manifests_in(rec)
    return manifest, read_json(manifest)


def copy_take(manifest, data, dst, status, names=None):
    dst.mkdir(parents=True, exist_ok=True)
    for f in data["files"]:
        if names is None or f["path"] in names:
            shutil.copy2(manifest.parent / f["path"], dst / f["path"])
    m = dict(data, status=status)
    if names is not None:
        m["files"] = [f for f in data["files"] if f["path"] in names]
    (dst / manifest.name).write_text(json.dumps(m))
    return m


def offer(h, case, folder, watched, expect_changed=False, extra=None):
    """Launch a daemon whose recordings folder is `folder`; `watched` must be (un)changed."""
    root = folder.parent
    (root / "setup.toml").write_text(setup_toml(folder, "hevc", "mov", h.args.fps))
    before = {str(p): fingerprint(p) for p in watched}
    eng = Engine(h, root, h.args.crash_safe)
    row = {"kind": "ownership", "case": case, "folder": str(root)}
    failures = []
    try:
        st = eng.launch()
        row["interrupted"] = [str(r.get("manifest")) for r in st.get("interrupted") or []]
    except (OSError, RpcError, ValueError) as exc:
        failures.append(f"harness: {exc}")
    finally:
        eng.shutdown()
        eng.close()
    after = {str(p): fingerprint(p) for p in watched}
    row["changed"] = [p for p in before if before[p] != after[p]]
    row["manifests"] = {str(m): {k: (read_json(m) or {}).get(k) for k in ("status", "errors")}
                        for m in manifests_in(folder)}
    if expect_changed:
        files = h.probe([Path(p) for p in watched])
        row["files"] = files
        if not row["changed"]:
            failures.append("control: its own killed take was not repaired")
        for p, f in files.items():
            if not f["decodes"] or f["tracks"] is None:
                failures.append(f"control: {Path(p).name} does not decode after repair")
    elif row["changed"]:
        failures.append(f"repair touched files it doesn't own: {row['changed']}")
    row.update(extra or {})
    row["failures"] = failures
    h.write(row)


def ownership(h):
    base = h.work / "own"
    if base.exists():
        raise Abort(f"{base} exists; pass a fresh --work (nothing is deleted)")
    with bench_lock(not h.args.no_lock):
        manifest, data = killed_take(h, "src")
    name = data["files"][0]["path"]
    for status in ("recording", "failed"):
        # The target takes live outside every daemon's recordings folder.
        target_dir = base / f"targets-{status}"
        copy_take(manifest, data, target_dir, status)
        target = target_dir / name
        one = dict(data, status=status, files=[f for f in data["files"] if f["path"] == name])
        for case in ("symlink", "hardlink", "folder-symlink"):
            folder = base / f"{case}-{status}" / "rec"
            folder.mkdir(parents=True)
            m = one
            if case == "symlink":
                (folder / name).symlink_to(target)
            elif case == "hardlink":
                os.link(target, folder / name)
            else:
                (folder / "sub").symlink_to(target_dir)
                m = dict(one, files=[dict(one["files"][0], path="sub/" + name)])
            (folder / manifest.name).write_text(json.dumps(m))
            with bench_lock(not h.args.no_lock):
                offer(h, f"{case}-{status}", folder, [target])
        # ffmpeg movies (plain, faststart, fragmented, an Isocast-shaped one) named by a manifest.
        folder = base / f"ffmpeg-{status}" / "rec"
        folder.mkdir(parents=True)
        x264 = "libx264" in command(["ffmpeg", "-hide_banner", "-encoders"])["stdout"]
        src = ["-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30", "-f", "lavfi", "-i",
               "sine=frequency=440:sample_rate=48000", "-t", "3",
               "-c:v", "libx264" if x264 else "h264_videotoolbox", "-g", "30", "-c:a", "aac"]
        variants = {"plain.mp4": [], "faststart.mp4": ["-movflags", "+faststart"], "plain.mov": [],
                    "frag.mp4": ["-movflags", "frag_keyframe+empty_moov+default_base_moof"],
                    "frag.mov": ["-movflags", "frag_keyframe+empty_moov"]}
        for fname, extra in variants.items():
            out = command(["ffmpeg", "-nostdin", "-y", "-v", "error", *src, *extra, folder / fname])
            if out["rc"] != 0:
                raise Abort(f"ffmpeg could not make {fname}: {out['stderr']}")
        frag = (folder / "frag.mp4").read_bytes()
        ftyp = int.from_bytes(frag[:4], "big")
        (folder / "shaped.mp4").write_bytes(frag[:ftyp] + (16).to_bytes(4, "big") + b"free" + bytes(8) + frag[ftyp:])
        movies = sorted(p for p in folder.iterdir() if p.suffix in MEDIA)
        m = dict(data, status=status, files=[dict(data["files"][0], path=p.name) for p in movies])
        (folder / manifest.name).write_text(json.dumps(m))
        with bench_lock(not h.args.no_lock):
            offer(h, f"ffmpeg-{status}", folder, movies)
    # A hard link to a file a live recorder still has open (stopped with SIGSTOP).
    for case in ("live-hardlink", "live-renamed"):
        with bench_lock(not h.args.no_lock):
            live_open(h, base, case)
    # Control: the killed take in the daemon's own recordings folder is repaired.
    folder = base / "control" / "rec"
    copy_take(manifest, data, folder, "recording")
    with bench_lock(not h.args.no_lock):
        offer(h, "control", folder, sorted(p for p in folder.iterdir() if p.suffix in MEDIA),
              expect_changed=True)


def live_open(h, base, case):
    """A file a live recorder (SIGSTOPped, still holding its lock) has open,
    hard-linked or renamed into another daemon's recordings folder."""
    root = base / f"{case}-owner"
    rec = root / "rec"
    rec.mkdir(parents=True)
    (root / "setup.toml").write_text(setup_toml(rec, "hevc", "mov", h.args.fps))
    owner = Engine(h, root, h.args.crash_safe)
    try:
        owner.launch()
        started = owner.rpc("start", {})
        owner.wait_recording()
        owner.wait_media(2.0)
        os.kill(owner.process.pid, signal.SIGSTOP)
        live = Path(started["files"][-1])
        (manifest,) = manifests_in(rec)
        data = read_json(manifest)
        folder = base / case / "rec"
        folder.mkdir(parents=True)
        if case == "live-hardlink":
            os.link(live, folder / live.name)
            watched = live
        else:
            # Same inode, one link, still open in the owner: only its lock protects it.
            os.rename(live, folder / live.name)
            watched = folder / live.name
        m = dict(data, status="recording", files=[f for f in data["files"] if f["path"] == live.name])
        (folder / manifest.name).write_text(json.dumps(m))
        offer(h, case, folder, [watched], extra={
            "owner_alive": owner.process.poll() is None,
            "links": os.stat(watched).st_nlink})
    finally:
        owner.close()


# --- entry -----------------------------------------------------------------

def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0],
                                epilog=__doc__.split("\n\n", 1)[1],
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("command", nargs="?", default="all", choices=["all", "matrix", "ownership"])
    p.add_argument("--bin-dir", required=True, help="folder holding the release isocastd")
    p.add_argument("--work", required=True, help="scratch folder for engines and recordings (short path)")
    p.add_argument("--report", help="JSONL report (default WORK/report.jsonl, appended)")
    p.add_argument("--runs", type=int, default=21)
    p.add_argument("--seed", type=int, default=3013)
    p.add_argument("--modes", default=",".join(ALL_MODES))
    p.add_argument("--fps", type=int, default=30)
    p.add_argument("--crash-safe", choices=["default", "on", "off"], default="default",
                   help="default leaves ISOCAST_CRASH_SAFE unset (the shipped default)")
    p.add_argument("--interval", type=float, default=0.5,
                   help="the writer's fragment interval in seconds (loss budgets build on it)")
    p.add_argument("--mixer-latency", type=float, default=0.2,
                   help="audio still in the mixer at a kill, added to the audio budget")
    p.add_argument("--pause-loss", type=float, default=0.3, help="the same while paused")
    p.add_argument("--committed-slack", type=float, default=0.05)
    p.add_argument("--no-avfoundation", action="store_true", help="skip the AVFoundation read")
    p.add_argument("--no-lock", action="store_true", help="don't take the benchmark lock")
    p.add_argument("--clean", action="store_true", help="delete media of passing kill runs")
    args = p.parse_args()
    if args.runs < 1:
        p.error("--runs must be at least 1")
    try:
        h = Harness(args)
        for name, used in (("runs", ("all", "matrix")), ("own", ("all", "ownership"))):
            if args.command in used and (h.work / name).exists():
                raise Abort(f"{h.work / name} exists; pass a fresh --work (nothing is deleted)")
        print(f"kill-matrix: report {h.report}", file=sys.stderr, flush=True)
        if args.command in ("all", "matrix"):
            matrix(h)
        if args.command in ("all", "ownership"):
            ownership(h)
    except Abort as exc:
        print(f"kill-matrix: {exc}", file=sys.stderr)
        return 2
    summary = {"kind": "summary", "rows": h.rows, "failed": h.failures, "seed": args.seed,
               "crash_safe": args.crash_safe, "daemon_sha256": h.daemon_sha}
    with open(h.report, "a") as log:
        log.write(json.dumps(summary) + "\n")
    print(f"kill-matrix: {h.rows - h.failures}/{h.rows} passed; report {h.report}", flush=True)
    return 1 if h.failures else 0


if __name__ == "__main__":
    sys.exit(main())
