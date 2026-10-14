#!/usr/bin/env python3
"""Real-file engine checks on macOS; Python stdlib + ffmpeg/ffprobe only.

Build: cargo build --release -p isocast-daemon
Run:   python3 scripts/e2e.py [--only record] [--keep] [--json]

Each scenario gets a signed, background-only app, socket, setup, library,
plugins, timeline and recordings in its own temporary directory. No capture
devices or default user state are used. The parent bounds each worker and
kills its process group on timeout, even if native shutdown is stuck. SIGINT
and SIGTERM also clean up. JSON is always written (--report, e2e-report.json
by default); --json also prints it instead of human progress on stdout.

Set ISOCAST_SCRATCH on test Macs to share scripts/bench.py's .bench.lock.
--work chooses the parent of the temporary folder, not a folder to overwrite.
--keep preserves all evidence; --keep-failures preserves only failed cases.
Without hardware HEVC/H.264, recordings exercise the engine's ProRes path;
hardware-only streaming is explicitly skipped. No performance fallback is
compared with hardware benchmark numbers.

--hosted qualifies a lighter 720p30 H.264 profile with two program+ISO/stream
runs. Zero drops in qualification keeps the zero-drop bar for the suite.
Otherwise only accounted video losses up to 25% per output are allowed; all
integrity, duration, audio, sync, picture and API checks still apply. Ordinary
local/test-Mac runs remain 1080p60 and require zero drops. --summary REPORT
renders the same hardware, qualification and loss tables used by Actions.
"""

import argparse
import array
import base64
import copy
import datetime
import fcntl
import hashlib
import json
import math
import os
import platform
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import time
import traceback
import wave
import zipfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
SCENARIOS = ("setup", "record", "iso", "split", "scenes", "stream", "timeline",
             "data", "plugin", "hotkeys", "shutdown", "pause", "bug_report")
FPS = 60
RATE = 48000
HOSTED_DROP_LIMIT = 0.25


class Failure(Exception):
    pass


class Skip(Exception):
    pass


class RpcError(Failure):
    def __init__(self, error):
        self.error = error
        super().__init__(json.dumps(error))


def require(ok, reason):
    if not ok:
        raise Failure(reason)


def command(args, timeout=60, data=None, check=True):
    out = subprocess.run([str(a) for a in args], input=data, capture_output=True,
                         timeout=timeout)
    if check and out.returncode:
        raise Failure(f"{args[0]} exited {out.returncode}: "
                      f"{out.stderr.decode(errors='replace')[-4000:]}")
    return out


def ffmpeg(*args, **kw):
    return command(["ffmpeg", "-nostdin", "-hide_banner", "-v", "error", *args], **kw)


def until(fn, seconds=30, description="condition"):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        result = fn()
        if result:
            return result
        time.sleep(0.05)
    raise Failure(f"timed out after {seconds}s waiting for {description}")


def read_json(path):
    return json.loads(Path(path).read_text())


def write_json(path, value):
    Path(path).write_text(json.dumps(value, indent=2) + "\n")


def capabilities():
    """Actually create an encoder, forbidding FFmpeg's software fallback."""
    result = {"system": platform.platform(), "machine": platform.machine()}
    for label, key in (("chip", "machdep.cpu.brand_string"), ("cores", "hw.logicalcpu"),
                       ("physical_cores", "hw.physicalcpu"), ("memory_bytes", "hw.memsize"),
                       ("virtual_machine", "kern.hv_vmm_present")):
        out = command(["sysctl", "-n", key], check=False)
        text = out.stdout.decode().strip()
        result[label] = (text if label == "chip" else int(text)) if out.returncode == 0 else None
    result["runner_environment"] = os.environ.get("RUNNER_ENVIRONMENT", "local")
    result["runner_image"] = os.environ.get("ImageOS")
    result["runner_image_version"] = os.environ.get("ImageVersion")
    displays = command(["system_profiler", "SPDisplaysDataType", "-json"], check=False, timeout=30)
    if displays.returncode == 0:
        # No display names, serial numbers or machine identifiers in the report.
        result["gpus"] = [{k: gpu[k] for k in ("_name", "sppci_model", "sppci_cores",
                          "spdisplays_metal", "spdisplays_mtlgpufamilysupport") if k in gpu}
                          for gpu in json.loads(displays.stdout).get("SPDisplaysDataType", [])]
    else:
        result["gpus"] = []
        result["gpu_reason"] = "system_profiler unavailable"
    for codec in ("hevc", "h264"):
        out = ffmpeg("-f", "lavfi", "-i", "color=size=128x128:rate=60",
                     "-frames:v", "2", "-c:v", codec + "_videotoolbox",
                     "-allow_sw", "0", "-f", "null", "-", check=False, timeout=30)
        result[codec] = out.returncode == 0
        if out.returncode:
            result[codec + "_reason"] = out.stderr.decode(errors="replace")[-2000:]
    result["record_codec"] = ("hevc" if result["hevc"] else
                              "h264" if result["h264"] else "prores422")
    return result


class Engine:
    def __init__(self, root, daemon):
        self.root = root
        self.sock = root / "d.sock"
        require(len(os.fsencode(self.sock)) < 104,
                "--work path too long for a macOS Unix socket; use a shorter path")
        self.app = root / "Dev.app"
        binary = self.app / "Contents/MacOS/isocastd"
        binary.parent.mkdir(parents=True)
        shutil.copy2(daemon, binary)
        shutil.copy2(REPO / "crates/isocast-daemon/Info.plist", self.app / "Contents/Info.plist")
        command(["codesign", "--force", "--sign", "-", self.app])
        command(["codesign", "--verify", self.app])
        # Drop inherited Isocast overrides (including any user's stream key).
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("ISOCAST_")}
        self.env.update(ISOCAST_SOCKET=str(self.sock), ISOCAST_SETUP=str(root / "setup.toml"),
                        ISOCAST_DEV_APP=str(self.app), ISOCAST_PLUGINS=str(root / "plugins"),
                        ISOCAST_TIMELINES=str(root / "timelines"), ISOCAST_MONITOR_SILENT="1",
                        ISOCAST_STREAM_KEY="e2e", ISOCAST_LOG_DIR=str(root / "logs"))
        self.log = open(root / "daemon.log", "ab", buffering=0)
        self.process = None
        self.start()

    def start(self):
        self.process = subprocess.Popen([str(self.app / "Contents/MacOS/isocastd")],
                                        env=self.env, stdout=self.log, stderr=self.log)

        def ready():
            require(self.process.poll() is None, "daemon exited at startup; see daemon.log")
            try:
                return self.rpc("status")
            except (OSError, RpcError):
                return None
        until(ready, description="isolated daemon")

    def rpc(self, method, params=None, timeout=30):
        request = {"jsonrpc": "2.0", "id": 1, "method": method}
        if params is not None:
            request["params"] = params
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
            sock.settimeout(timeout)
            sock.connect(str(self.sock))
            sock.sendall((json.dumps(request) + "\n").encode())
            with sock.makefile("rb") as reader:
                reply = json.loads(reader.readline(16 * 1024 * 1024))
        if "error" in reply:
            raise RpcError(reply["error"])
        result = reply["result"]
        if method in ("start", "split"):
            # Started intentionally lists media paths only. Locate its manifest
            # by those paths rather than assuming the user's file-name template.
            files = {Path(p).name for p in result["files"]}
            folder = Path(result["files"][0]).parent
            def locate():
                for path in folder.glob("*.session.json"):
                    if {f["path"] for f in read_json(path)["files"]} == files:
                        return str(path)
                return None
            result["manifest"] = until(locate, description="take manifest")
        return result

    def optional(self, method, params=None):
        try:
            return self.rpc(method, params)
        except RpcError as exc:
            text = str(exc).lower()
            if exc.error.get("code") == -32601 or ("unknown variant" in text and method in text):
                raise Skip(f"not available: {method}") from exc
            raise

    def apply(self, setup):
        rev = self.rpc("get")["revision"]
        return self.rpc("apply", {"revision": rev, "setup": setup})

    def stop(self):
        if self.process is None or self.process.poll() is not None:
            return
        try:
            self.rpc("shutdown", timeout=15)
            self.process.wait(timeout=15)
        except (OSError, ValueError, Failure, subprocess.TimeoutExpired):
            self.process.kill()
            self.process.wait(timeout=5)

    def close(self):
        try:
            self.stop()
        finally:
            self.log.close()


def setup(root, codec="hevc", iso=False, width=1920, height=1080, fps=FPS):
    sources = [{"id": s, "kind": "test_pattern", "width": width, "height": height,
                "fps": fps} for s in ("bars", "bars2")]
    sources.append({"id": "tone", "kind": "tone"})
    return {"version": 1, "name": "isolated e2e", "canvas": {
        "width": width, "height": height, "fps": fps}, "sources": sources,
        "scenes": [{"id": "main", "name": "Main", "items": [{"source": "bars",
                    "rect": {"x": 0, "y": 0, "width": width, "height": height}}]}],
        "recording": {"folder": str(root / "recordings"), "keep_free_gb": 0.2,
                      "program": {"enabled": True, "encoder": {"codec": codec}, "audio": ["tone"]},
                      "iso": {"enabled": iso, "sources": ["bars", "bars2"],
                              "encoder": {"codec": codec}, "audio": ["tone"]}}}


def finished(path):
    try:
        manifest = read_json(path)
        return manifest if manifest["status"] != "recording" else None
    except (OSError, ValueError):
        return None


def take(engine, seconds=5):
    started = engine.rpc("start", {"seconds": seconds})
    manifest = until(lambda: finished(started["manifest"]), seconds + 35, "recording finalization")
    until(lambda: engine.rpc("status")["state"] == "idle", description="idle recorder")
    return Path(started["manifest"]), manifest


def probe(path):
    out = command(["ffprobe", "-v", "error", "-count_frames", "-show_streams",
                   "-show_format", "-of", "json", path])
    require(not out.stderr.strip(), f"ffprobe errors: {out.stderr.decode(errors='replace')}")
    return json.loads(out.stdout)


def check_file(path, duration=None, fps=FPS, dimensions=None, audio=True, dropped=0, repeated=0):
    info = probe(path)
    video = next(s for s in info["streams"] if s["codec_type"] == "video")
    vd = float(video["duration"])
    frames = int(video["nb_read_frames"])
    if duration is not None:
        require(abs(vd - duration) <= 1 / fps + 0.00001,
                f"{path.name}: video duration {vd}, expected {duration}")
        require(abs(frames + dropped - repeated - round(duration * fps)) <= 1,
                f"{path.name}: {frames} decoded + {dropped} dropped - {repeated} repeated, "
                f"expected {round(duration * fps)} frame slots ±1")
    if dimensions:
        require((video["width"], video["height"]) == dimensions, f"{path.name}: wrong dimensions")
    rate = video["r_frame_rate" if dropped else "avg_frame_rate"]
    require(abs(float(rate.split('/')[0]) / float(rate.split('/')[1]) - fps) < 0.01,
            f"{path.name}: wrong frame rate {rate}")
    audios = [s for s in info["streams"] if s["codec_type"] == "audio"]
    require(bool(audios) == audio, f"{path.name}: unexpected audio tracks ({len(audios)})")
    for a in audios:
        require(abs(vd - float(a["duration"])) < 0.001,
                f"{path.name}: A/V durations differ: {vd} / {a['duration']}")
    decoded = ffmpeg("-xerror", "-i", path, "-map", "0", "-f", "null", "-", timeout=90)
    require(not decoded.stderr.strip(), f"{path.name}: decode warnings: {decoded.stderr.decode()}")
    return {"file": str(path), "frames": frames, "video_seconds": vd,
            "audio_seconds": [float(a["duration"]) for a in audios],
            "codec": video["codec_name"], "width": video["width"], "height": video["height"]}


def pcm(path):
    raw = ffmpeg("-i", path, "-map", "0:a:0", "-af", "pan=mono|c0=c0", "-ar", RATE,
                 "-f", "f32le", "-").stdout
    samples = array.array("f")
    samples.frombytes(raw)
    if sys.byteorder != "little":
        samples.byteswap()
    return samples


def video_samples(path, graph):
    # Preserve actual PTS: indexing decoded pictures by nominal fps moves every
    # later scene/flash earlier when a recording contains dropped frame slots.
    raw = ffmpeg("-i", path, "-map", "0:v:0", "-an", "-vf", graph + ",scale=1:1:flags=area",
                 "-fps_mode", "passthrough", "-pix_fmt", "gray", "-f", "rawvideo", "-", timeout=90).stdout
    frames = json.loads(command(["ffprobe", "-v", "error", "-select_streams", "v:0",
                                 "-show_frames", "-show_entries", "frame=best_effort_timestamp_time",
                                 "-of", "json", path], timeout=90).stdout)["frames"]
    times = [float(f["best_effort_timestamp_time"]) for f in frames]
    require(len(times) == len(raw), "decoded samples and timestamps disagree")
    require(all(b > a for a, b in zip(times, times[1:])), "decoded video timestamps are not monotonic")
    return list(zip(times, raw))


def sync_check(path, fps=FPS):
    values = video_samples(path, "crop=iw:ih/4:0:ih*3/4")
    flashes = [t for i, (t, v) in enumerate(values) if v > 128 and (i == 0 or values[i - 1][1] <= 128)]
    samples = pcm(path)
    beeps, quiet = [], RATE
    for i, sample in enumerate(samples):
        if abs(sample) > 0.01:
            if quiet >= RATE // 20:
                beeps.append(i / RATE)
            quiet = 0
        else:
            quiet += 1
    # Exclude only pulses cut by either edge of the file, not missing interior pulses.
    end = values[-1][0] + 1 / fps
    f = [t for t in flashes if 0.15 < t < end - 0.15]
    b = [t for t in beeps if 0.15 < t < end - 0.15]
    require(len(f) >= int(end) - 1 and len(f) == len(b),
            f"{path.name}: missing pulses ({len(f)} flashes / {len(b)} beeps in {end:.3f}s)")
    offsets = [abs(x - y) for x, y in zip(f, b)]
    require(all(d <= 1 / fps + 1 / RATE for d in offsets),
            f"{path.name}: beep/flash drift {max(offsets, default=0) * 1000:.2f}ms exceeds one frame")
    require(all(abs(y - x - 1) <= 1 / fps + 1 / RATE for x, y in zip(b, b[1:])),
            f"{path.name}: beep cadence is not one second")
    return {"flashes": len(flashes), "beeps": len(beeps),
            "max_sync_ms": round(max(offsets, default=0) * 1000, 3)}


def drop_metrics(manifest):
    rows = []
    for f in manifest["files"]:
        if f.get("frames") is None or f.get("dropped_frames") is None:
            continue  # unfinished crash fragments have no final counters, not zero losses
        frames, dropped, repeated = (f.get(k, 0) for k in ("frames", "dropped_frames", "repeated_frames"))
        slots = frames + dropped - (repeated or 0)
        seconds = f.get("duration_seconds") or 0
        rows.append({"output": f["path"], "frames": frames, "dropped": dropped,
                     "repeated": repeated, "slots": slots, "ratio": dropped / slots if slots else None,
                     "delivered_fps": frames / seconds if seconds else None,
                     "target_fps": f["fps"], "audio_dropouts": f.get("audio_dropouts", 0)})
    return rows


def loss_limit(evidence):
    return (evidence["capabilities"].get("max_drop_ratio", 0)
            if evidence and evidence["capabilities"].get("hosted") else 0)


def check_loss(row, limit):
    for field in ("frames", "dropped", "repeated", "audio_dropouts"):
        require(type(row[field]) is int and row[field] >= 0, f"{row['output']}: invalid {field} counter")
    require(row["slots"] > 0, f"{row['output']}: no frame slots measured")
    require(row["audio_dropouts"] == 0, f"{row['output']}: audio_dropouts={row['audio_dropouts']}")
    require(row["ratio"] <= limit,
            f"{row['output']}: dropped {row['dropped']}/{row['slots']} "
            f"({row['ratio']:.1%}), limit {limit:.0%}")
    # Only the engine's single, explicitly counted tail repeat is permitted
    # when accounting for video loss. Zero-drop runs still prohibit repeats.
    require(row["repeated"] <= (1 if limit and row["dropped"] else 0),
            f"{row['output']}: unexpected repeated_frames={row['repeated']}")


def check_take(path, manifest, duration=None, sync=False, expected_sources=None, evidence=None):
    losses = drop_metrics(manifest)
    if evidence is not None:
        evidence.setdefault("drops", {}).update({str(path.parent / r["output"]): r for r in losses})
    require(manifest["status"] == "complete", f"incomplete manifest: {manifest}")
    require(not manifest.get("errors"), f"manifest errors: {manifest.get('errors')}")
    require(len(losses) == len(manifest["files"]), "complete manifest lacks final frame/drop counters")
    if expected_sources:
        require(sorted(f["source"] for f in manifest["files"]) == sorted(expected_sources),
                "manifest source/file list differs from setup")
    rows = []
    for f, loss in zip(manifest["files"], losses):
        check_loss(loss, loss_limit(evidence))
        file = path.parent / f["path"]
        row = check_file(file, duration if duration is not None else f["duration_seconds"],
                         f["fps"], (f["width"], f["height"]),
                         dropped=loss["dropped"], repeated=loss["repeated"])
        require(row["frames"] == f["frames"], f"{file.name}: manifest/decoded frame counts disagree")
        if sync:
            row.update(sync_check(file, f["fps"]))
        rows.append(row)
    require(bool(rows), "manifest lists no files")
    return rows


def generated_clip(root, fps=FPS):
    """Three seconds of numbered frames plus synchronous flashes/beeps, no font dependency."""
    digits = ("111101101101111", "010110010010111", "111001111100111", "111001111001111",
              "101101111001001", "111100111001111", "111100111101111", "111001001001001",
              "111101111101111", "111101111001111")
    width, height, count = 320, 180, 3 * fps
    raw = root / "counter.rgb"
    with open(raw, "wb") as out:
        for frame in range(count):
            pixels = bytearray(bytes((20, 45, 90)) * width * height)
            pixels[135 * width * 3:] = bytes([255 if frame % fps < fps // 10 else 0]) * (45 * width * 3)
            for j, digit in enumerate(f"{frame:03}"):
                for y in range(5):
                    for x in range(3):
                        if digits[int(digit)][y * 3 + x] == "1":
                            for yy in range(6):
                                start = ((8 + y * 6 + yy) * width + 8 + j * 24 + x * 6) * 3
                                pixels[start:start + 18] = bytes([255]) * 18
            out.write(pixels)
    sound = root / "beeps.wav"
    with wave.open(str(sound), "wb") as out:
        out.setparams((1, 2, RATE, 0, "NONE", "not compressed"))
        samples = array.array("h", (int(12000 * math.sin(2 * math.pi * 1000 * i / RATE))
                                   if i % RATE < RATE // 10 else 0 for i in range(3 * RATE)))
        if sys.byteorder != "little":
            samples.byteswap()
        out.writeframes(samples.tobytes())
    path = root / "counter.mov"
    ffmpeg("-f", "rawvideo", "-pixel_format", "rgb24", "-video_size", "320x180", "-framerate", fps,
           "-i", raw, "-f", "wav", "-i", sound, "-c:v", "libx264", "-threads", "2", "-preset", "ultrafast",
           "-crf", "12", "-pix_fmt", "yuv420p", "-c:a", "pcm_s16le", "-y", path)
    raw.unlink()
    sound.unlink()
    return path


def scenario_setup(e, cfg, evidence):
    e.apply(cfg)
    require(e.rpc("get")["setup"]["name"] == cfg["name"], "valid setup was not applied")
    errors = []
    for field, change in (("canvas.width", lambda c: c["canvas"].update(width=0)),
                          ("sources[0].fps", lambda c: c["sources"][0].update(fps=0)),
                          ("canvas.fsp", lambda c: c["canvas"].update(fsp=60))):
        bad = copy.deepcopy(cfg)
        change(bad)
        try:
            e.apply(bad)
            raise Failure(f"invalid {field} was accepted")
        except RpcError as err:
            require(field in str(err), f"{field} missing from error: {err}")
            errors.append(err.error)
    require(e.rpc("get")["setup"]["canvas"]["width"] == cfg["canvas"]["width"], "invalid apply changed state")
    evidence["field_errors"] = errors


def scenario_record(e, cfg, evidence):
    e.apply(cfg)
    path, manifest = take(e)
    evidence["files"] = check_take(path, manifest, 5, sync=True, expected_sources=["program"], evidence=evidence)


def scenario_iso(e, cfg, evidence):
    cfg["recording"]["iso"]["enabled"] = True
    e.apply(cfg)
    path, manifest = take(e)
    evidence["files"] = check_take(path, manifest, 5, sync=True,
                                    expected_sources=["program", "bars", "bars2"], evidence=evidence)
    starts = [f["start_offset_seconds"] for f in manifest["files"]]
    require(max(starts) - min(starts) <= 1 / cfg["canvas"]["fps"], f"ISO start offsets disagree: {starts}")


def scenario_split(e, cfg, evidence):
    e.apply(cfg)
    first = e.rpc("start", {})
    time.sleep(1)
    a = e.rpc("marker", {"label": "first"})
    second = e.rpc("split")
    begins = datetime.datetime.fromisoformat(second["begins_at"]).timestamp()
    time.sleep(max(0, begins - time.time()) + 0.6)
    b = e.rpc("marker", {"label": "second"})
    e.rpc("stop")
    require(first["manifest"] != second["manifest"], "split reused first manifest")
    evidence["files"] = []
    for start, marker in ((first, a), (second, b)):
        path = Path(start["manifest"])
        m = until(lambda: finished(path), description="split finalization")
        require(marker["time_seconds"] > 0 and marker["time_seconds"] < m["duration_seconds"],
                "marker is outside its take")
        require(any(x["label"] == marker["label"] and
                    abs(x["time_seconds"] - marker["time_seconds"]) < 0.001 for x in m["markers"]),
                "marker timestamp missing from manifest")
        evidence["files"].extend(check_take(path, m, evidence=evidence))
    evidence["markers"] = [a, b]


def scenario_scenes(e, cfg, evidence):
    width, height, fps = (cfg["canvas"][k] for k in ("width", "height", "fps"))
    cfg["sources"] = [{"id": name, "kind": "color", "color": color, "width": width, "height": height} for name, color in
                      (("black", "#000000"), ("white", "#ffffff"))] + [{"id": "tone", "kind": "tone"}]
    cfg["scenes"] = [{"id": name, "name": name, "items": [{"source": name,
                       "rect": {"x": 0, "y": 0, "width": width, "height": height}}]}
                     for name in ("black", "white")]
    cfg["recording"]["iso"]["sources"] = []
    e.apply(cfg)
    start = e.rpc("start", {})
    switches = []
    for scene, transition in (("white", {"kind": "cut"}),
                              ("black", {"kind": "fade", "duration_ms": 500}),
                              ("white", {"kind": "slide", "duration_ms": 500, "easing": "linear"})):
        time.sleep(1)
        switches.append(e.rpc("switch_scene", {"scene": scene, "transition": transition}))
    time.sleep(1)
    e.rpc("stop")
    path = Path(start["manifest"])
    manifest = read_json(path)
    evidence["files"] = check_take(path, manifest, evidence=evidence)
    values = video_samples(path.parent / manifest["files"][0]["path"], "null")
    def at(time_seconds):
        t, value = min(values, key=lambda pair: abs(pair[0] - time_seconds))
        require(abs(t - time_seconds) <= 1 / fps, "no picture within one frame of transition sample")
        return value
    for i, switch in enumerate(switches):
        t = switch["time_seconds"]
        before, after = ((0, 255) if switch["scene"] == "white" else (255, 0))
        require(abs(at(t - 0.15) - before) < 8, "picture changed before switch")
        require(abs(at(t + 0.65) - after) < 8, "picture did not finish switching")
        if i == 0:
            edge = next(pts for pts, v in values if v > 128)
            require(abs(edge - t) <= 2 / fps, f"cut timing wrong: frame {edge}, API {t}")
        else:
            midpoint = at(t + 0.25)
            require(60 < midpoint < 195, f"500 ms transition not visible at midpoint: {midpoint}")
            changes = [pts for pts, v in values if t - 0.1 < pts < t + 0.7 and 8 < v < 247]
            require(changes and 0.30 < changes[-1] - changes[0] < 0.55,
                    f"500 ms transition has wrong visible duration: {changes[:1]}..{changes[-1:]}")
        require(any(s["scene"] == switch["scene"] and abs(s["time_seconds"] - t) < 0.001
                    for s in manifest["scenes"]), "scene change absent from manifest")
    evidence["switches"] = switches


def scenario_stream(e, cfg, evidence):
    width, height, fps = (cfg["canvas"][k] for k in ("width", "height", "fps"))
    if not evidence["capabilities"]["h264"]:
        raise Skip("no hardware encoder on this runner (H.264 streaming)")
    # A free loopback port; startup failure is a test failure, never a remote retry.
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    cfg["stream"] = {"service": "custom", "server": f"rtmp://127.0.0.1:{port}/live",
                     "video_bitrate_kbps": 6000, "audio_bitrate_kbps": 160,
                     "width": width, "height": height, "keyframe_interval_seconds": 2}
    e.apply(cfg)
    e.rpc("engine", {"running": True})
    time.sleep(1)
    output = e.root / evidence.get("stream_name", "received.flv")
    with open(e.root / "listener.log", "wb") as log:
        listener = subprocess.Popen(["ffmpeg", "-nostdin", "-hide_banner", "-v", "warning",
                                     "-listen", "1", "-i", f"rtmp://127.0.0.1:{port}/live/e2e",
                                     "-c", "copy", "-y", str(output)], stdout=log, stderr=log)
        try:
            time.sleep(0.5)
            require(listener.poll() is None, "RTMP listener failed to start")
            e.rpc("stream_start")
            time.sleep(7)
            final = e.rpc("stream_stop")
            listener.wait(timeout=10)
            slots = final["frames_sent"] + final["dropped_frames"]
            loss = {"output": "stream", "frames": final["frames_sent"], "dropped": final["dropped_frames"],
                    "slots": slots, "ratio": final["dropped_frames"] / slots if slots else None,
                    "repeated": 0, "audio_dropouts": final["audio_dropouts"], "target_fps": fps,
                    "delivered_fps": final["frames_sent"] / final["elapsed_seconds"]}
            evidence.setdefault("drops", {})[str(output)] = loss
            evidence["stream"] = final
            check_loss(loss, loss_limit(evidence))
            require(e.rpc("status")["stream"]["state"] == "offline", "stream did not stop cleanly")
        finally:
            if listener.poll() is None:
                listener.kill()
                listener.wait(timeout=5)
    packets = json.loads(command(["ffprobe", "-v", "error", "-show_packets", "-of", "json", output]).stdout)["packets"]
    rates = {}
    keyframes = []
    for kind, target in (("video", 6000), ("audio", 160)):
        rows = [p for p in packets if p["codec_type"] == kind]
        require(bool(rows), f"stream has no {kind}")
        times = [float(p["pts_time"]) for p in rows]
        require(all(b > a for a, b in zip(times, times[1:])), f"{kind} timestamps not monotonic")
        if kind == "video" and final["dropped_frames"]:
            # Loss explains missing video slots, never backwards/off-grid PTS
            # or unreported gaps. Audio timestamps retain their original bar.
            steps = [(b - a) * fps for a, b in zip(times, times[1:])]
            require(all(abs(n - round(n)) <= fps * 0.0011 for n in steps), "video timestamps off frame grid")
            missing = sum(round(n) - 1 for n in steps)
            require(missing <= final["dropped_frames"], "stream has unreported missing frame slots")
        else:
            require(max(b - a for a, b in zip(times, times[1:])) <= (1 / fps + (0.018 - 1 / FPS) if kind == "video" else 0.023),
                    f"{kind} stream has a timestamp gap")
        # Ignore startup and final partial seconds for the bitrate measurement.
        middle = [p for p in rows if times[0] + 1 <= float(p["pts_time"]) < times[0] + 6]
        kbps = sum(int(p["size"]) for p in middle) * 8 / 5 / 1000
        require(abs(kbps / target - 1) <= 0.20, f"{kind} bitrate {kbps:.0f}, target {target} kb/s ±20%")
        rates[kind] = round(kbps, 1)
        if kind == "video":
            keyframes = [float(p["pts_time"]) for p in rows if "K" in p.get("flags", "")]
    require(len(keyframes) >= 3 and all(abs(b - a - 2) < 1 / fps + (0.018 - 1 / FPS) for a, b in zip(keyframes, keyframes[1:])),
            f"keyframe interval is not two seconds: {keyframes}")
    ffmpeg("-xerror", "-i", output, "-f", "null", "-")
    evidence.update(kbps=rates, keyframes=keyframes, stream=final)


def scenario_timeline(e, cfg, evidence):
    e.apply(cfg)
    e.rpc("timeline_new", {"name": "E2E"})
    evidence["files"] = []
    for _ in range(2):
        path, manifest = take(e, 2)
        evidence["files"].extend(check_take(path, manifest, 2, evidence=evidence))
    before = e.rpc("timeline_get")
    require(len(before["clips"]) == 2, "two takes did not append")
    dropped = before["clips"][-1]
    retaken = e.rpc("retake", {"seconds": 2})
    until(lambda: e.rpc("status")["state"] == "idle", 35, "retake")
    timeline = e.rpc("timeline_get")
    require(len(timeline["clips"]) == 2 and timeline["clips"][-1]["id"] != dropped["id"],
            "retake did not replace last clip")
    require(any(r["take"] == dropped["take"] for r in timeline["removed"]), "dropped take not listed")
    require(Path(dropped["take"]).exists() and all(Path(f["path"]).is_file() for f in dropped["files"]),
            "retake deleted original files")
    for clip in timeline["clips"]:
        check_take(Path(clip["take"]), read_json(clip["take"]), evidence=evidence)
    export = e.rpc("timeline_export", {"folder": str(e.root / "export")})
    def exported():
        value = read_json(export["manifest"])
        return value if value["status"] != "exporting" else None
    result = until(exported, 45, "timeline export")
    require(result["status"] == "complete", f"export failed: {result}")
    duration = sum(c["duration_seconds"] for c in timeline["clips"])
    require(abs(timeline["duration_seconds"] - duration) < 0.00001, "timeline duration differs from clips")
    require(bool(result["files"]), "export contains no files")
    evidence["joined"] = []
    for f in result["files"]:
        # Export must retain every decoded picture of its parts, including
        # their accounted losses, while spanning the complete clip timeline.
        frames = sum(int(probe(Path(part["path"]))["streams"][0]["nb_read_frames"]) for part in f["parts"])
        dropped = round(duration * f["fps"]) - frames
        row = check_file(Path(f["path"]), duration, f["fps"], dropped=dropped if loss_limit(evidence) else 0)
        require(row["frames"] == frames, "export lost decoded pictures at a join")
        evidence["joined"].append(row)
    evidence.update(timeline=timeline, retake=retaken)


def snapshot(e, name):
    result = e.rpc("snapshot", {})
    path = e.root / (name + ".png")
    path.write_bytes(base64.b64decode(result["png_base64"]))
    return ffmpeg("-i", path, "-pix_fmt", "gray", "-f", "rawvideo", "-").stdout


def scenario_data(e, cfg, evidence):
    cfg["scenes"][0]["items"] = [{"id": "title", "content": {
        "kind": "text", "data": "title", "size": 72, "width": 600, "height": 120},
        "rect": {"x": 0, "y": 0, "width": 600, "height": 120}}]
    e.apply(cfg)
    e.rpc("data_set", {"values": {"title": "OLD 111"}})
    before = snapshot(e, "before")
    rev = e.rpc("get")["revision"]
    e.rpc("data_set", {"values": {"title": "NEW 888"}})
    time.sleep(0.1)
    after = snapshot(e, "after")
    # Compare the program with the same literal text rendered by the engine;
    # a mere image change would also pass if the new value were blank/wrong.
    reference = copy.deepcopy(cfg)
    content = reference["scenes"][0]["items"][0]["content"]
    content.pop("data")
    content["text"] = "NEW 888"
    require(e.rpc("get")["revision"] == rev, "data_set changed setup revision")
    e.apply(reference)
    expected = snapshot(e, "literal-reference")
    require(before != after and after == expected, "bound program text differs from literal new value")
    evidence["matched_literal_pixels"] = len(expected)


def scenario_plugin(e, cfg, evidence):
    e.rpc("plugin_add", {"path": str(REPO / "examples/plugins/gain"), "grant": {}})
    # Also exercise generated media decoding, including its counter and sound.
    media = generated_clip(e.root, cfg["canvas"]["fps"])
    cfg["sources"][0] = {"id": "bars", "kind": "media", "files": [str(media)], "loop": True}
    cfg["recording"]["program"]["audio"] = ["bars"]
    levels = []
    evidence["files"] = []
    for gain in (0, -6):
        cfg["sources"][0]["audio_filters"] = [{"kind": "gain/gain", "gain_db": gain}]
        e.apply(cfg)
        path, m = take(e, 3)
        evidence["files"].extend(check_take(path, m, 3, sync=True, evidence=evidence))
        samples = pcm(path.parent / m["files"][0]["path"])
        active = [s for s in samples if abs(s) > 0.001]
        require(bool(active), "plugin produced silence")
        levels.append(20 * math.log10(math.sqrt(statistics.mean(s * s for s in active))))
    delta = levels[1] - levels[0]
    require(abs(delta + 6) < 0.35, f"Wasm gain changed RMS by {delta:.3f} dB, expected -6 ±0.35")
    evidence["gain_delta_db"] = delta


def scenario_hotkeys(e, cfg, evidence):
    e.apply(cfg)
    catalogue = e.rpc("hotkeys_actions")
    require(any(a["id"] == "recording.toggle" for a in catalogue["actions"]), "record action missing")
    binding = {"keys": "ctrl+alt+cmd+f12", "action": "recording.toggle", "args": {}}
    e.rpc("hotkeys_set", {"bindings": {"e2e": binding}})
    got = e.rpc("hotkeys_get")
    require(got["bindings"]["e2e"]["action"] == binding["action"], "hotkey did not persist")
    e.rpc("hotkeys_set", {"patch": {"e2e": None}, "revision": got["revision"]})
    require(not e.rpc("hotkeys_get")["bindings"], "hotkey removal did not persist")
    evidence["action_count"] = len(catalogue["actions"])


def scenario_shutdown(e, cfg, evidence):
    e.apply(cfg)
    start = e.rpc("start", {})
    time.sleep(3)
    e.rpc("shutdown")
    e.process.wait(timeout=20)
    path = Path(start["manifest"])
    evidence["graceful"] = check_take(path, read_json(path), evidence=evidence)
    e.start()
    killed = e.rpc("start", {})
    time.sleep(7)
    e.process.kill()
    e.process.wait(timeout=10)
    manifest = read_json(killed["manifest"])
    require(manifest["status"] == "recording", "crashed take incorrectly marked complete")
    evidence["fragments"] = []
    for f in manifest["files"]:
        path = Path(killed["manifest"]).parent / f["path"]
        info = probe(path)
        video = next(s for s in info["streams"] if s["codec_type"] == "video")
        require(int(video["nb_read_frames"]) >= 2 * cfg["canvas"]["fps"], "crash did not retain two seconds of video")
        ffmpeg("-xerror", "-i", path, "-f", "null", "-")
        evidence["fragments"].append({"file": str(path), "frames": int(video["nb_read_frames"])})
    e.start()
    recovery = e.rpc("start", {"seconds": 1})
    require("interrupt" in json.dumps(recovery.get("notes", [])).lower(),
            "next recording start did not report interrupted take")
    # The daemon repairs a killed take at its next launch (A.6: "interrupted").
    require(read_json(killed["manifest"])["status"] in ("failed", "interrupted"),
            "crash manifest not recovered")
    until(lambda: finished(recovery["manifest"]), 30, "recovery recording")
    evidence["recovery_notes"] = recovery.get("notes", [])


def scenario_pause(e, cfg, evidence):
    # Probe while idle: a known method gives wrong-state; unknown is an explicit skip.
    try:
        e.optional("pause")
    except RpcError:
        pass
    e.apply(cfg)
    start = e.rpc("start", {})
    time.sleep(1.5)
    paused = e.rpc("pause")
    time.sleep(1)
    resumed = e.rpc("resume")
    time.sleep(1.5)
    e.rpc("stop")
    path = Path(start["manifest"])
    m = read_json(path)
    require(bool(m.get("pauses")), "pause missing from manifest")
    evidence["files"] = check_take(path, m, evidence=evidence)
    require(2.8 < evidence["files"][0]["video_seconds"] < 3.8, "pause was not removed from file duration")
    evidence.update(paused=paused, resumed=resumed)


def scenario_bug_report(e, cfg, evidence):
    e.apply(cfg)
    result = e.optional("bug_report", {"out": str(e.root / "bug-report"), "describe": "Synthetic e2e"})
    archives = list(e.root.rglob("*.zip"))
    require(bool(archives), f"bug_report did not create an isolated archive: {result}")
    with zipfile.ZipFile(archives[0]) as archive:
        require(archive.testzip() is None and bool(archive.namelist()), "bug report archive corrupt/empty")
        evidence["archive_entries"] = len(archive.namelist())
    evidence["report"] = result


def scenario_capacity(e, cfg, evidence):
    """Qualify the actual engine, compositor, three writers and local stream."""
    evidence["machine_info"] = e.rpc("doctor", {"setup": cfg})["machine"]
    evidence["rounds"] = []
    for index in range(2):
        measured = {"capabilities": evidence["capabilities"], "stream_name": f"capacity-stream-{index}.flv"}
        try:
            scenario_iso(e, copy.deepcopy(cfg), measured)
            if evidence["capabilities"]["h264"]:
                scenario_stream(e, copy.deepcopy(cfg), measured)
        finally:
            evidence.setdefault("drops", {}).update(measured.get("drops", {}))
            evidence["rounds"].append({"files": measured.get("files", []), "stream": measured.get("stream")})


def worker(args):
    root = args.worker_root
    evidence = {"capabilities": read_json(root.parent / "capabilities.json")}
    engine = None
    started = time.monotonic()
    status, reason = "PASS", "verified"
    try:
        engine = Engine(root, args.daemon)
        codec = evidence["capabilities"]["record_codec"]
        cfg = setup(root, codec, **evidence["capabilities"].get("profile", {}))
        globals()["scenario_" + args.worker](engine, cfg, evidence)
    except Skip as exc:
        status, reason = "SKIP", str(exc)
    except Exception as exc:
        status, reason = "FAIL", str(exc)
        (root / "failure.txt").write_text(traceback.format_exc())
    finally:
        if engine:
            engine.close()
        # Preserve every output's counters even when an earlier assertion
        # failed. A rejected take must not make the summary look drop-free.
        for path in (root / "recordings").glob("*.session.json"):
            manifest = read_json(path)
            if manifest["status"] != "recording":
                evidence.setdefault("drops", {}).update({str(path.parent / row["output"]): row
                                                        for row in drop_metrics(manifest)})
        write_json(root / "result.json", {"name": args.worker, "status": status, "reason": reason,
                                          "seconds": round(time.monotonic() - started, 3),
                                          "evidence": evidence})
    return 1 if status == "FAIL" else 0


def kill_group(process):
    # The group includes workers, encoders and the engine, never other test jobs.
    def send(sig):
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            pass
        except PermissionError:
            # macOS can return EPERM instead of ESRCH for a group containing
            # only dead, unreaped native children. Do not mistake this for a
            # live child we were unable to kill, or let it suppress the report.
            rows = command(["ps", "-axo", "pgid=,stat="]).stdout.decode().splitlines()
            alive = [r for r in rows if r.split()[0] == str(process.pid)
                     and not r.split()[1].startswith("Z")]
            require(not alive, f"cannot signal live worker process group {process.pid}")
    send(signal.SIGTERM)
    try:
        process.wait(timeout=20)
    except subprocess.TimeoutExpired:
        pass
    send(signal.SIGKILL)
    process.wait(timeout=5)


def interrupted(signum, _frame):
    raise InterruptedError(f"interrupted by signal {signum}")


def run_case(args, root, name):
    case = root / name
    case.mkdir()
    with open(case / "worker.log", "wb") as log:
        process = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), "--worker", name,
                                    "--worker-root", str(case), "--daemon", str(args.daemon.resolve())],
                                   stdout=log, stderr=log, start_new_session=True)
        try:
            process.wait(timeout=args.timeout)
            if (case / "result.json").exists():
                result = read_json(case / "result.json")
            else:
                result = {"name": name, "status": "FAIL", "reason": f"worker exited {process.returncode} without report"}
        except subprocess.TimeoutExpired:
            result = {"name": name, "status": "FAIL", "reason": f"scenario timed out after {args.timeout}s"}
        finally:
            kill_group(process)
    keep = args.keep or (args.keep_failures and result["status"] == "FAIL")
    result["artifacts"] = str(case) if keep else None
    losses = list(result.get("evidence", {}).get("drops", {}).values())
    detail = ""
    if losses:
        dropped, slots = (sum(row[k] for row in losses) for k in ("dropped", "slots"))
        detail = f"; video drops {dropped}/{slots} ({dropped / slots:.1%})" if slots else "; no frame slots"
    print(f"{result['status']:4} {name}: {result['reason']}{detail}",
          file=sys.stderr if args.json else sys.stdout, flush=True)
    if not keep:
        shutil.rmtree(case)
    return result


def summary(report):
    def cell(value):
        return str(value).replace("|", "/").replace("\n", " ")[:500]
    caps = report.get("capabilities", {})
    profile = caps.get("profile", {"width": 1920, "height": 1080, "fps": 60})
    lines = ["## Engine E2E capabilities", "", "| Measurement | Value |", "|---|---|"]
    memory = caps.get("memory_bytes")
    entries = [("OS / architecture", f"{caps.get('system')} / {caps.get('machine')}"),
               ("Runner", f"{caps.get('runner_environment')} / {caps.get('runner_image')} / {caps.get('runner_image_version')}"),
               ("CPU", f"{caps.get('chip')}; {caps.get('cores')} logical / {caps.get('physical_cores')} physical cores"),
               ("Memory", f"{memory / 2**30:.2f} GiB" if memory else "unavailable"),
               ("VM sysctl", caps.get("virtual_machine")),
               ("GPU / Metal", json.dumps(caps.get("gpus", []))),
               ("VT encoder names", ", ".join(caps.get("machine_info", {}).get("hardware_encoders", [])) or
                "not enumerated (hosted engine qualification provides names)"),
               ("Hardware-only encode probes", f"HEVC={caps.get('hevc')}, H.264={caps.get('h264')} (availability, not capacity)"),
               ("Profile", f"{profile['width']}×{profile['height']} @ {profile['fps']} fps, {caps.get('record_codec')}"),
               ("Video drop limit, per output", f"{caps.get('max_drop_ratio', 0):.0%}; audio dropouts must be zero")]
    for name, value in entries:
        lines.append(f"| {name} | {cell(value)} |")
    qualification = caps.get("qualification")
    if qualification:
        lines.extend(["", "### Hosted profile qualification", "",
                      cell(qualification["reason"]), ""])
    lines.extend(["", "## Scenarios", "", "| Scenario | Result | Reason |", "|---|---|---|"])
    for row in report.get("scenarios", []):
        lines.append(f"| {cell(row['name'])} | {cell(row['status'])} | {cell(row['reason'])} |")
    for field in ("error", "cleanup_error"):
        if report.get(field):
            lines.append(f"| harness | FAIL | {cell(report[field])} |")
    lines.extend(["", "## Actual throughput and losses", "",
                  "Counts and ratios include failed outputs; each output must meet its limit independently.", "",
                  "| Scenario / output | Stored/sent frames | Dropped / slots | Ratio | Delivered / target fps | Audio dropouts |",
                  "|---|---:|---:|---:|---:|---:|"])
    groups = [("qualification", qualification.get("drops", []))] if qualification else []
    groups += [(r["name"], list(r.get("evidence", {}).get("drops", {}).values()))
               for r in report.get("scenarios", []) if r["name"] != "capacity"]
    for name, losses in groups:
        for row in losses:
            ratio = f"{row['ratio']:.2%}" if row.get("ratio") is not None else "n/a"
            rate = f"{row['delivered_fps']:.2f}" if row.get("delivered_fps") is not None else "n/a"
            lines.append(f"| {cell(name)} / {cell(row['output'])} | {row['frames']} | {row['dropped']} / {row['slots']} | "
                         f"{ratio} | {rate} / {row['target_fps']} | {row['audio_dropouts']} |")
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    target = Path(os.environ.get("CARGO_TARGET_DIR", REPO / "target"))
    scratch = Path(os.environ.get("ISOCAST_SCRATCH", "/Volumes/IsocastBuild" if
                                 Path("/Volumes/IsocastBuild").exists() else tempfile.gettempdir()))
    parser.add_argument("--daemon", type=Path, default=target / "release/isocastd")
    parser.add_argument("--work", type=Path, default=scratch / "e2e")
    parser.add_argument("--report", type=Path, default=Path("e2e-report.json"))
    parser.add_argument("--summary", type=Path, help="render a saved report as an Actions Markdown summary and exit")
    parser.add_argument("--hosted", action="store_true", help="qualify a 720p30 profile for a constrained hosted VM")
    parser.add_argument("--capabilities-from", type=Path, help="reuse this invocation's hosted qualification report")
    parser.add_argument("--list", action="store_true")
    parser.add_argument("--only", action="append", choices=SCENARIOS)
    parser.add_argument("--keep", action="store_true")
    parser.add_argument("--keep-failures", action="store_true")
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--timeout", type=float, default=180, help="hard timeout per scenario, seconds")
    parser.add_argument("--codec", choices=("hevc", "h264", "prores422"),
                        help="exercise a specific recording path (default: available hardware, else ProRes)")
    parser.add_argument("--capabilities", action="store_true", help="probe hardware and exit (JSON)")
    parser.add_argument("--worker", choices=SCENARIOS + ("capacity",), help=argparse.SUPPRESS)
    parser.add_argument("--worker-root", type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.summary:
        print(summary(read_json(args.summary)), end="")
        return 0
    if args.list:
        print(json.dumps(list(SCENARIOS)) if args.json else "\n".join(SCENARIOS))
        return 0
    if args.worker:
        return worker(args)
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    report = {"version": 1, "scenarios": [], "started_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
    root = None
    try:
        require(sys.platform == "darwin", "macOS is required")
        for tool in ("ffmpeg", "ffprobe", "codesign"):
            require(shutil.which(tool), f"missing required tool: {tool}")
        require(args.timeout > 0, "--timeout must be positive")
        scratch.mkdir(parents=True, exist_ok=True)
        with open(scratch / ".bench.lock", "a") as lock:
            print("e2e: waiting for benchmark lock", file=sys.stderr)
            fcntl.flock(lock, fcntl.LOCK_EX)
            if args.capabilities_from:
                require(args.hosted, "--capabilities-from requires --hosted; local runs always have a zero-drop limit")
                saved = read_json(args.capabilities_from)
                caps = saved["capabilities"]
                require(caps.get("hosted") and caps.get("qualification", {}).get("status") == "PASS",
                        "hosted qualification did not pass")
                require(caps.get("max_drop_ratio") in (0, HOSTED_DROP_LIMIT), "invalid hosted loss limit")
                require(caps.get("profile") == {"width": 1280, "height": 720, "fps": 30}, "invalid hosted profile")
                require(caps.get("daemon_sha256") == hashlib.sha256(args.daemon.read_bytes()).hexdigest(),
                        "daemon changed since qualification")
                require(caps.get("system") == platform.platform(), "qualification came from a different OS")
                require(not args.codec or args.codec == caps["record_codec"], "codec changed since qualification")
            else:
                caps = capabilities()
                caps["hosted"] = args.hosted
                caps["profile"] = {"width": 1280, "height": 720, "fps": 30} if args.hosted else {
                    "width": 1920, "height": 1080, "fps": 60}
                if args.codec:
                    caps["record_codec"] = args.codec
                elif args.hosted and caps["h264"]:
                    caps["record_codec"] = "h264"
                caps["max_drop_ratio"] = HOSTED_DROP_LIMIT if args.hosted else 0
            report["capabilities"] = caps
            if args.capabilities and not args.hosted:
                print(json.dumps(report["capabilities"], indent=2))
                return 0
            require(args.daemon.is_file(), f"daemon not found: {args.daemon}; build release first")
            args.work.mkdir(parents=True, exist_ok=True)
            root = Path(tempfile.mkdtemp(prefix="run-", dir=args.work)).resolve()
            write_json(root / "capabilities.json", report["capabilities"])
            report["work"] = str(root)
            if args.hosted and not args.capabilities_from:
                result = run_case(args, root, "capacity")
                report["scenarios"].append(result)
                measured = result.get("evidence", {})
                losses = list(measured.get("drops", {}).values())
                zero = result["status"] == "PASS" and bool(losses) and all(r["dropped"] == 0 for r in losses)
                caps["max_drop_ratio"] = 0 if zero else HOSTED_DROP_LIMIT
                caps["machine_info"] = measured.get("machine_info", {})
                caps["daemon_sha256"] = hashlib.sha256(args.daemon.read_bytes()).hexdigest()
                caps["qualification"] = {"status": result["status"], "drops": losses,
                    "reason": (f"Two 5-second program+2-ISO takes and {2 if caps['h264'] else 0} 7-second streams: zero drops; "
                               "the complete hosted suite must also have zero drops." if zero else
                               f"720p30 qualification: {result['reason']}. Video loss limit is 25% per output; "
                               "all integrity checks and zero audio loss remain mandatory.")}
                write_json(root / "capabilities.json", caps)
                print(json.dumps(caps, indent=2), file=sys.stderr)
                require(result["status"] == "PASS", "hosted profile qualification failed; see capacity results")
            if not args.capabilities:
                for name in args.only or SCENARIOS:
                    report["scenarios"].append(run_case(args, root, name))
    except Exception as exc:
        report["error"] = str(exc)
        print(f"FAIL harness: {exc}", file=sys.stderr)
    finally:
        try:
            if root and not args.keep and not any(r.get("artifacts") for r in report["scenarios"]):
                shutil.rmtree(root)
        except Exception as exc:
            report["cleanup_error"] = str(exc)
        report["totals"] = {status: sum(r["status"] == status for r in report["scenarios"])
                            for status in ("PASS", "FAIL", "SKIP")}
        args.report.parent.mkdir(parents=True, exist_ok=True)
        write_json(args.report, report)
    if args.json:
        print(json.dumps(report, indent=2))
    return 1 if report.get("error") or report.get("cleanup_error") or report["totals"]["FAIL"] else 0


if __name__ == "__main__":
    sys.exit(main())
