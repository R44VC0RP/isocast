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
import bisect
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
SCENARIOS = ("setup", "record", "iso", "split", "scenes", "stream", "timeline", "edit",
             "data", "plugin", "hotkeys", "shutdown", "pause", "bug_report", "projects",
             "plugin_files", "replay")
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
                        ISOCAST_STREAM_KEY="e2e", ISOCAST_LOG_DIR=str(root / "logs"),
                        # Never the real report service: bug_report's submit fails cleanly.
                        ISOCAST_REPORT_ENDPOINT="http://127.0.0.1:9/reports")
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
                    if path.is_symlink():
                        continue  # plugin_files plants dangling links
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
                          (f"sources.{cfg['sources'][0]['id']}.fps", lambda c: c["sources"][0].update(fps=0)),
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
    # Record first so the stream joins a parked native encoder (#68). Its
    # bitrate budget must not accumulate while nobody consumes its output.
    started = e.rpc("start", {})
    time.sleep(3)
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
    e.rpc("stop")
    manifest = until(lambda: finished(started["manifest"]), description="stream take finalization")
    evidence.setdefault("files", []).extend(check_take(Path(started["manifest"]), manifest, evidence=evidence))
    packets = json.loads(command(["ffprobe", "-v", "error", "-show_packets", "-of", "json", output]).stdout)["packets"]
    rates = {}
    keyframes = []
    for kind, target in (("video", 6000), ("audio", 160)):
        rows = [p for p in packets if p["codec_type"] == kind]
        require(bool(rows), f"stream has no {kind}")
        times = [float(p["pts_time"]) for p in rows]
        if kind == "video":
            first_size = int(rows[0]["size"])
            require(first_size <= target * 1000 / 8 / 4,
                    f"first video packet {first_size} bytes exceeds a quarter-second bitrate budget")
            evidence["first_video_bytes"] = first_size
        initial_kbps = [sum(int(p["size"]) for p in rows
                            if times[0] + second <= float(p["pts_time"]) < times[0] + second + 1)
                        * 8 / 1000 for second in range(2)]
        # VT's ordinary cold CBR startup averages half the target in its first
        # second, then reaches the target. Bound the upper rate from frame one;
        # don't mistake that normal ramp for the parked-session budget burst.
        first_min = 0.40 if kind == "video" else 0.75
        require(first_min <= initial_kbps[0] / target <= 1.25
                and 0.75 <= initial_kbps[1] / target <= 1.25,
                f"{kind} initial bitrate {initial_kbps}, target {target} kb/s (cold ramp allowed)")
        evidence.setdefault("initial_kbps", {})[kind] = initial_kbps
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


# Gray levels that split the test pattern's 75% bars into bright (white to
# green) and dark (magenta to black), under BT.601 and BT.709 alike.
BAR_LEVELS = bytes(49 if v >= 96 else 48 for v in range(256))


def pattern_frames(path, width, height):
    """Per decoded picture of a synthetic test pattern: (pts, bar shift in
    pixels, label cell, flash band). The bars scroll by width / (4 s * fps)
    per source frame, so the shift names the source frame (modulo 4 s); the
    cell is the label's fifth character, lit for "BARS2" and dark for
    "BARS"; the band is the bottom quarter that flashes on every pulse."""
    s = max(height // 120, 1)
    graph = (f"[0:v]format=gray,split=3[a][b][c];[a]crop={width}:1:0:{height // 2}[row];"
             f"[b]crop={5 * s}:{7 * s}:{30 * s}:{6 * s},scale={width}:1:flags=area[cell];"
             f"[c]crop={width}:{height // 4}:0:{height - height // 4},scale={width}:1:flags=area[band];"
             "[row][cell][band]vstack=3[out]")
    raw = ffmpeg("-i", path, "-filter_complex", graph, "-map", "[out]", "-fps_mode", "passthrough",
                 "-pix_fmt", "gray", "-f", "rawvideo", "-", timeout=90).stdout
    times = [float(f["best_effort_timestamp_time"]) for f in json.loads(command(
        ["ffprobe", "-v", "error", "-select_streams", "v:0", "-show_frames", "-show_entries",
         "frame=best_effort_timestamp_time", "-of", "json", path], timeout=90).stdout)["frames"]]
    size = 3 * width
    require(len(raw) == len(times) * size, f"{path.name}: decoded pictures and timestamps disagree")
    frames = []
    for i, t in enumerate(times):
        row = raw[i * size:i * size + width]
        bits = row.translate(BAR_LEVELS)
        rise, fall = bits.find(b"01"), bits.find(b"10")
        if 2 < rise < width - 4:
            shift = (width - rise - 1) % width       # black to white: bar 0 starts
        elif 2 < fall < width - 4:
            shift = (width // 2 - fall - 1) % width  # green to magenta: bar 4 starts
        else:
            raise Failure(f"{path.name}: no test-pattern bars in picture {i}")
        cell = raw[i * size + width:i * size + 2 * width]
        band = raw[i * size + 2 * width:(i + 1) * size]
        frames.append((t, shift, sum(cell) / width, sum(band) / width))
    return frames


def beep_onsets(samples):
    onsets, quiet = [], RATE
    for i, sample in enumerate(samples):
        if abs(sample) > 0.01:
            if quiet >= RATE // 20:
                onsets.append(i / RATE)
            quiet = 0
        else:
            quiet += 1
    return onsets


def scenario_edit(e, cfg, evidence):
    """C.3's edits on two program + 2 ISO takes: trim, split, ripple delete,
    an angle, undo and redo, export. Every exported picture must be the take
    picture on screen at its moment (by the test pattern), every pulse must
    flash and beep together at the cuts, joins mustn't click, and the takes'
    files must be unchanged."""
    fps = cfg["canvas"]["fps"]
    cfg["recording"]["iso"]["enabled"] = True
    e.apply(cfg)
    e.rpc("timeline_new", {"name": "E2E edit"})
    takes = [take(e, 5) for _ in range(2)]
    evidence["files"] = [row for path, m in takes for row in check_take(path, m, 5, evidence=evidence)]
    timeline = e.rpc("timeline_get")
    require([Path(c["take"]).resolve() for c in timeline["clips"]] == [p.resolve() for p, _ in takes],
            "the two takes did not append")
    files = {}  # (take, source) -> (file, its first frame on the take's clock)
    for clip in timeline["clips"]:
        for f in clip["files"]:
            path = Path(clip["take"]).parent / f["path"]
            files[(clip["take"], f["source"])] = (path, f.get("start_offset_seconds") or 0.0)
    hashes = {p: hashlib.sha256(p.read_bytes()).hexdigest() for p, _ in files.values()}
    (c1, a, a_len), (c2, b, b_len) = ((c["id"], c["take"], c["out_seconds"]) for c in timeline["clips"])
    require(a_len >= 4.6 and b_len >= 4.6, f"takes too short: {a_len}, {b_len}")
    # Pulses beep for 0.1 s every second of the engine's clock, at their own
    # phase in each take: measure where (sample-accurate) and cut around
    # them. Cuts land in quiet stretches, except the delete's end (quiet into
    # mid-beep: a flash and beep at the cut) and c1's end (mid-beep into
    # quiet: a click if the join were hard).
    pulses_in = {}  # take -> its pulses' starts on the take's clock
    for clip in (a, b):
        program, offset = files[(clip, "program")]
        pulses_in[clip] = [t + offset for t in beep_onsets(pcm(program))]
        require(len(pulses_in[clip]) >= 4, f"{program.name}: only {len(pulses_in[clip])} beeps")
    def grid(x):
        return round(x * fps) / fps
    def phase(clip):  # where pulses start within a second, in [-0.5, 0.5)
        return (pulses_in[clip][1] + 0.5) % 1 - 0.5  # [0] may be cut by the take's start
    qa, ma, qb = grid(phase(a) + 0.5), grid(phase(a) + 3.05), grid(phase(b) + 0.5)

    def expect(label, segments):
        clips = e.rpc("timeline_get")["clips"]
        have = [(c["take"], c["in_seconds"], c["out_seconds"], c.get("angle")) for c in clips]
        require(len(have) == len(segments) and all(
            h[0] == s[0] and abs(h[1] - s[1]) < 1e-4 and abs(h[2] - s[2]) < 1e-4 and h[3] == s[3]
            for h, s in zip(have, segments)), f"after {label}: clips {have}, expected {segments}")
        return clips

    edits = [
        ("timeline_trim", {"clip": c1, "in_seconds": qa, "out_seconds": ma + 1},
         [(a, qa, ma + 1, None), (b, 0, b_len, None)]),
        ("timeline_trim", {"clip": c2, "in_seconds": qb},
         [(a, qa, ma + 1, None), (b, qb, b_len, None)]),
        ("timeline_split", {"at_seconds": 1.0},
         [(a, qa, qa + 1, None), (a, qa + 1, ma + 1, None), (b, qb, b_len, None)]),
        ("timeline_delete_range", {"start_seconds": 2.0, "end_seconds": 2.0 + ma - (qa + 2)},
         [(a, qa, qa + 1, None), (a, qa + 1, qa + 2, None), (a, ma, ma + 1, None), (b, qb, b_len, None)]),
        ("timeline_angle", {"start_seconds": 4.0, "end_seconds": 5.0, "source": "bars2"},
         [(a, qa, qa + 1, None), (a, qa + 1, qa + 2, None), (a, ma, ma + 1, None),
          (b, qb, qb + 1, None), (b, qb + 1, qb + 2, "bars2"), (b, qb + 2, b_len, None)]),
    ]
    for method, params, segments in edits:
        e.rpc(method, params)
        expect(method, segments)
    try:
        e.rpc("timeline_angle", {"start_seconds": 4.0, "end_seconds": 5.0, "source": "nope"})
        raise Failure("an angle from an unknown source was accepted")
    except RpcError as err:
        require("bars2" in str(err), f"angle error doesn't list the sources: {err}")
    e.rpc("timeline_undo")
    expect("timeline_undo", edits[-2][2])
    e.rpc("timeline_redo")
    clips = expect("timeline_redo", edits[-1][2])
    segments, start = [], 0.0  # (timeline start, take, in, out, angle)
    for take_path, cut_in, cut_out, angle in edits[-1][2]:
        segments.append((start, take_path, cut_in, cut_out, angle))
        start += cut_out - cut_in
    duration = start
    require(all(abs(c["start_seconds"] - s[0]) < 1e-4 for c, s in zip(clips, segments)),
            "clip starts differ from the clips' lengths added up")

    export = e.rpc("timeline_export", {"folder": str(e.root / "export")})
    def exported():
        value = read_json(export["manifest"])
        return value if value["status"] != "exporting" else None
    result = until(exported, 90, "timeline export")
    require(result["status"] == "complete" and not result.get("errors"), f"export failed: {result}")
    require(sorted(f["source"] for f in result["files"]) == ["bars", "bars2", "edited", "program"],
            f"exported sources: {[f['source'] for f in result['files']]}, skipped {result.get('skipped')}")

    def in_pulse(clip, x, end=False):  # x is in a pulse (its end: the pulse ran up to it)
        return any((p < x <= p + 0.1) if end else (p <= x < p + 0.1) for p in pulses_in[clip])
    pulses = []  # where the edited timeline should flash and beep
    for i, (start, clip, cut_in, cut_out, _) in enumerate(segments):
        previous = segments[i - 1] if i else None
        if in_pulse(clip, cut_in) and not (previous and in_pulse(previous[1], previous[3], end=True)):
            pulses.append(start)
        pulses += [start + p - cut_in for p in pulses_in[clip] if cut_in < p < cut_out]
    joins = [s[0] for s in segments[1:]]
    sources = {}
    evidence["exported"] = []
    for f in result["files"]:
        path, width = Path(f["path"]), f["width"]
        require(all(abs(p["start_seconds"] - s[0]) < 1e-3 for p, s in zip(f["parts"], segments))
                and len(f["parts"]) == len(segments), f"{path.name}: parts don't follow the clips")
        dropped = 0
        if loss_limit(evidence):
            frames = int(probe(path)["streams"][0]["nb_read_frames"])
            dropped = round(duration * fps) - frames
        row = check_file(path, duration, fps, (cfg["canvas"]["width"], cfg["canvas"]["height"]),
                         dropped=dropped)
        # Every picture: the one on screen in its take at that moment.
        out = pattern_frames(path, width, f["height"])
        bad = []
        for t, shift, cell, _ in out:
            seg = next((s for s in segments if s[0] - 1e-6 <= t < s[0] + s[3] - s[2] - 1e-6), None)
            require(seg is not None, f"{path.name}: a picture at {t:.4f} s, past the timeline")
            source = (seg[4] or "program") if f["source"] == "edited" else f["source"]
            src_path, offset = files[(seg[1], source)]
            if src_path not in sources:
                frames = pattern_frames(src_path, width, f["height"])
                sources[src_path] = (frames, [x[0] - frames[0][0] for x in frames])
            src, src_times = sources[src_path]
            moment = seg[2] + (t - seg[0]) - offset
            want = src[max(0, bisect.bisect_right(src_times, moment + 0.25 / fps) - 1)]
            require((want[2] > 50) == (source == "bars2"), f"{src_path.name}: label cell can't tell the angles apart")
            apart = abs(shift - want[1])
            if min(apart, width - apart) > 3 or (cell > 50) != (want[2] > 50):
                bad.append(f"{t:.4f} s: bars at {shift}, label {cell:.0f}; {source} at "
                           f"{seg[2] + t - seg[0]:.4f} s of its take has {want[1]}, {want[2]:.0f}")
        require(not bad, f"{path.name}: {len(bad)} of {len(out)} pictures aren't their take's "
                         f"at that moment: {bad[:4]}")
        # Pulses: each expected one flashes and beeps; nothing else does.
        flashes = [t for i, (t, _, _, band) in enumerate(out) if band > 128 and (i == 0 or out[i - 1][3] <= 128)]
        samples = pcm(path)
        beeps = beep_onsets(samples)
        end = out[-1][0] + 1 / fps
        def inner(xs):
            return [x for x in xs if 0.15 < x < end - 0.15]
        def near(x, xs, tolerance):
            return min((abs(x - y) for y in xs), default=1) <= tolerance
        for x in inner(pulses):
            require(near(x, flashes, 1.5 / fps) and near(x, beeps, 1.5 / fps),
                    f"{path.name}: no flash or beep at {x:.4f} s (flashes {flashes}, beeps {beeps})")
        for x in inner(flashes) + inner(beeps):
            require(near(x, pulses, 1.5 / fps), f"{path.name}: unexpected pulse at {x:.4f} s")
        offsets = [min(abs(x - y) for y in beeps) for x in inner(flashes)]
        require(max(offsets) <= 1 / fps + 1 / RATE,
                f"{path.name}: flash/beep apart by {max(offsets) * 1000:.2f} ms")
        # Clicks: no sample step near a join larger than the tone's own.
        window = RATE // 200
        steps = [abs(y - x) for x, y in zip(samples, samples[1:])]
        spans = [(max(0, round(t * RATE) - window), min(len(steps), round(t * RATE) + window)) for t in joins]
        at_joins = max(max(steps[lo:hi]) for lo, hi in spans)
        for lo, hi in spans:
            steps[lo:hi] = [0.0] * (hi - lo)
        tone = max(steps)
        require(tone > 0.01, f"{path.name}: no tone")
        require(at_joins <= tone * 1.05 + 1e-4,
                f"{path.name}: a join clicks (step {at_joins:.4f}, the tone's largest {tone:.4f})")
        row.update(pictures_checked=len(out), pulses=len(inner(flashes)),
                   max_sync_ms=round(max(offsets) * 1000, 3), join_step=round(at_joins, 4),
                   tone_step=round(tone, 4), reencoded=[p.get("reencoded_frames") for p in f["parts"]])
        evidence["exported"].append(row)
    changed = [p.name for p, digest in hashes.items() if hashlib.sha256(p.read_bytes()).hexdigest() != digest]
    require(not changed, f"edits or export changed take files: {changed}")
    evidence.update(timeline=e.rpc("timeline_get"), take_files_unchanged=len(hashes))


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

    # A successful native registration does not guarantee delivery to the
    # daemon (window-targeted posted events can reach only the app). Focused
    # edges must remain usable, including repeat suppression and rapid presses.
    bindings = {
        "pause": {"keys": "ctrl+alt+p", "action": "recording.pause_toggle"},
        "marker": {"keys": "ctrl+alt+m", "action": "recording.marker", "args": {"label": "focused"}},
    }
    evidence["registrations"] = e.rpc("hotkeys_set", {"bindings": bindings})["registrations"]
    started = e.rpc("start", {})
    until(lambda: e.rpc("status").get("live"), description="live recording status")
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.settimeout(10)
        sock.connect(str(e.sock))
        with sock.makefile("rb") as reader:
            def edge(keys, phase):
                request = {"jsonrpc": "2.0", "id": 1, "method": "hotkeys_input",
                           "params": {"keys": keys, "phase": phase}}
                sock.sendall((json.dumps(request) + "\n").encode())
                reply = json.loads(reader.readline())
                require(reply.get("result", {}).get("queued"), f"focused edge was ignored: {reply}")

            edge("ctrl+alt+p", "press")
            edge("ctrl+alt+p", "press")  # key repeat must not toggle back
            edge("ctrl+alt+p", "release")
            until(lambda: e.rpc("status")["live"]["paused"], description="hotkey pause")
            time.sleep(0.2)
            edge("ctrl+alt+m", "press")
            edge("ctrl+alt+m", "release")
            edge("ctrl+alt+p", "press")
            edge("ctrl+alt+p", "release")
            until(lambda: not e.rpc("status")["live"]["paused"], description="hotkey resume")
            for _ in range(2):  # distinct fast presses aren't debounce noise
                edge("ctrl+alt+m", "press")
                edge("ctrl+alt+m", "release")
    until(lambda: len(read_json(started["manifest"])["markers"]) == 3,
          description="three focused markers")
    e.rpc("stop")
    manifest = read_json(started["manifest"])
    require(len(manifest["pauses"]) == 1, "focused pause fired more than once")
    evidence["focused_markers"] = len(manifest["markers"])
    evidence["focused_pauses"] = manifest["pauses"]
    evidence["files"] = check_take(Path(started["manifest"]), manifest, evidence=evidence)


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
    folder = Path(killed["manifest"]).parent
    require((folder / ".isocast-project.json").is_file() and folder.parent == e.root / "recordings",
            f"killed take is not in a project folder: {folder}")
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


class Watch:
    """Collect `watch` events on a connection of their own, in a thread."""

    def __init__(self, engine, kinds):
        import threading
        self.events = []
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(str(engine.sock))
        self.sock.sendall((json.dumps({"jsonrpc": "2.0", "id": 1, "method": "watch",
                                       "params": {"kinds": kinds}}) + "\n").encode())
        self.reader = self.sock.makefile("rb")
        require("result" in json.loads(self.reader.readline(16 * 1024 * 1024)), "watch refused")
        self.thread = threading.Thread(target=self.read, daemon=True)
        self.thread.start()

    def read(self):
        for line in self.reader:
            message = json.loads(line)
            if message.get("method") == "event":
                self.events.append(message["params"])

    def of(self, kind):
        return [event for event in self.events if event["type"] == kind]

    def close(self):
        self.sock.close()


def files_of(manifest_path):
    """Every media file a manifest lists, resolved from its own folder."""
    path = Path(manifest_path)
    return [path.parent / f["path"] for f in read_json(path)["files"]]


def scenario_projects(e, cfg, evidence):
    """7.7: a folder per session, every lifecycle event once with its paths,
    rename (refused while recording) keeping files, manifests, timelines and
    exports resolving, and flat recordings from before projects untouched."""
    root = e.root / "recordings"
    root.mkdir(parents=True)
    cfg["recording"]["iso"]["enabled"] = True
    e.apply(cfg)
    # A flat recording from before projects: a finished take and a killed
    # one (status "recording") right in the recordings folder.
    legacy_path, legacy = take(e, 2)
    flat = []
    for f in [legacy_path, *files_of(legacy_path)]:
        flat.append(root / ("legacy " + f.name))
        shutil.copy2(f, flat[-1])
    flat_manifest = root / ("legacy " + legacy_path.name)
    killed = read_json(flat_manifest)
    killed["status"] = "recording"
    for f in killed["files"]:
        f["path"] = "legacy " + f["path"]
    write_json(flat_manifest, killed)
    flat_hashes = {str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in flat[1:]}
    e.rpc("timeline_new", {"name": "Projects"})
    kinds = ["started", "take_finished", "stopped", "marker", "project_created",
             "project_renamed", "recording_paused"]
    watch = Watch(e, kinds)
    first = e.rpc("start", {})
    folder = Path(first["manifest"]).parent
    project = read_json(folder / ".isocast-project.json")["id"]
    require(folder.parent == root and folder.name.endswith(" Untitled"),
            f"recording is not in a new '<date> <time> Untitled' folder: {folder}")
    time.sleep(1)
    e.rpc("marker", {"label": "one"})
    try:
        e.rpc("rename_project", {"project": project, "name": "Too Soon"})
        raise Failure("rename_project was allowed while recording into the project")
    except RpcError as exc:
        require(exc.error["code"] == -32001, f"rename while recording: wrong error {exc.error}")
        evidence["refused_while_recording"] = exc.error["message"]
    second = e.rpc("split")
    begins = datetime.datetime.fromisoformat(second["begins_at"]).timestamp()
    time.sleep(max(0, begins - time.time()) + 1)
    e.rpc("stop")
    until(lambda: e.rpc("status")["state"] == "idle", description="idle recorder")
    until(lambda: watch.of("stopped"), 10, "stopped event")
    time.sleep(0.5)
    watch.close()
    counts = {kind: len(watch.of(kind)) for kind in kinds}
    manifests = [Path(first["manifest"]), Path(second["manifest"])]
    require(Path(second["manifest"]).parent == folder, "the split's take is in another folder")
    # Each event once, with the project, folder, manifest and files.
    created = watch.of("project_created")
    require(len(created) == 1 and created[0]["project"]["id"] == project
            and Path(created[0]["project"]["folder"]) == folder, f"project_created: {created}")
    started = watch.of("started")
    require([(s.get("take"), s.get("cause")) for s in started] == [(1, "start"), (2, "split")],
            f"started events: {started}")
    for event, path in zip(started, manifests):
        require(Path(event["manifest"]) == path and Path(event["folder"]) == folder
                and event["project"] == project, f"started event paths: {event}")
        require(sorted(map(Path, event["files"])) == sorted(files_of(path)), "started files differ")
    finished_takes = watch.of("take_finished")
    require([t["take"] for t in finished_takes] == [1, 2], f"take_finished: {finished_takes}")
    for event, path in zip(finished_takes, manifests):
        require(Path(event["manifest"]) == path and event["status"] == "complete"
                and event["project"] == project, f"take_finished event: {event}")
        require(sorted(map(Path, event["files"])) == sorted(files_of(path)), "take_finished files differ")
        require(all(Path(f).is_file() for f in event["files"]), "take_finished names missing files")
    stopped = watch.of("stopped")
    require(len(stopped) == 1 and stopped[0]["project"] == project
            and Path(stopped[0]["folder"]) == folder
            and Path(stopped[0]["report"]["folder"]) == folder, f"stopped: {stopped}")
    markers = watch.of("marker")
    require(len(markers) == 1 and markers[0]["project"] == project, f"marker: {markers}")
    # Export into the project (the first clip's folder) before renaming.
    exported = e.rpc("timeline_export")
    require(Path(exported["manifest"]).parent == folder, "export did not go into the project folder")
    until(lambda: read_json(exported["manifest"])["status"] != "exporting", 45, "export")
    require(read_json(exported["manifest"])["status"] == "complete", "export failed")
    # Rename, and everything still resolves from the new folder.
    watch = Watch(e, ["project_renamed"])
    renamed = e.rpc("rename_project", {"project": project, "name": "My Talk"})
    new = Path(renamed["project"]["folder"])
    require(new.parent == root and new.name == folder.name.replace("Untitled", "My Talk")
            and not folder.exists() and renamed["project"]["id"] == project,
            f"rename result: {renamed}")
    until(lambda: watch.of("project_renamed"), 10, "project_renamed event")
    time.sleep(0.3)
    watch.close()
    event = watch.of("project_renamed")
    require(len(event) == 1 and Path(event[0]["old_folder"]) == folder
            and Path(event[0]["project"]["folder"]) == new and event[0]["old_name"] == "Untitled",
            f"project_renamed: {event}")
    listed = [p for p in e.rpc("list_projects")["projects"] if p["id"] == project]
    require(len(listed) == 1 and listed[0]["name"] == "My Talk" and len(listed[0]["takes"]) == 2,
            f"list_projects after rename: {listed}")
    evidence["files"] = []
    for path in listed[0]["takes"]:
        require(Path(path).parent == new, "a take is outside the renamed folder")
        evidence["files"].extend(check_take(Path(path), read_json(path), evidence=evidence))
    require(sorted(map(Path, listed[0]["files"])) ==
            sorted(f for t in listed[0]["takes"] for f in files_of(t)), "list_projects files differ")
    timeline = e.rpc("timeline_get")
    require(len(timeline["clips"]) >= 2, "takes did not reach the timeline")
    for clip in timeline["clips"]:
        require(Path(clip["take"]).parent == new and Path(clip["take"]).is_file()
                and all(f["exists"] for f in clip["files"]) and not clip.get("problem"),
                f"timeline clip does not resolve after rename: {clip}")
    export = read_json(new / Path(exported["manifest"]).name)
    for f in export["files"]:
        require(Path(f["path"]).is_file() and all(Path(p["path"]).is_file() for p in f["parts"]),
                f"export manifest does not resolve after rename: {f}")
        check_file(Path(f["path"]), f["duration_seconds"], f["fps"])
    for clip in export["clips"]:
        require(Path(clip["take"]).is_file(), "export clip take does not resolve after rename")
    # Collisions get a number, and bad names name the field.
    try:
        e.rpc("rename_project", {"project": project, "name": "a/b"})
        raise Failure("a name with / was accepted")
    except RpcError as exc:
        require("name" in exc.error["message"], f"bad name error: {exc.error}")
    # A rename behind Isocast's back (the Finder), and one whose timeline
    # update never happened (isocastd stopped right after the folder
    # rename): the timeline finds the takes by project id.
    def resolves(where):
        clips = e.rpc("timeline_get")["clips"]
        return all(Path(c["take"]).parent == where and not c.get("problem")
                   and all(f["exists"] for f in c["files"]) for c in clips)
    finder = new.with_name(new.name.replace("My Talk", "Finder Name"))
    new.rename(finder)
    # Export manifests are healed too, by list_projects alone (#56).
    e.rpc("list_projects")
    export = read_json(finder / Path(exported["manifest"]).name)
    require(all(Path(f["path"]).parent == finder and all(Path(p["path"]).is_file() for p in f["parts"])
                for f in export["files"]) and all(Path(c["take"]).is_file() for c in export["clips"]),
            f"export manifest not healed after a Finder rename: {export['files']}")
    require(resolves(finder), "timeline broke after a Finder rename")
    e.stop()
    killed = finder.with_name(finder.name.replace("Finder Name", "Killed Rename"))
    finder.rename(killed)
    # Clips from before project ids: healed by file name, and given the
    # id (#56).
    timelines = [t for t in (e.root / "timelines").glob("*.json") if "clips" in read_json(t)]
    for path in timelines:
        saved = read_json(path)
        for clip in saved["clips"]:
            clip.pop("project", None)
        write_json(path, saved)
    e.start()
    require(resolves(killed), "timeline broke after a rename with no timeline update")
    for path in timelines:
        require(all(c.get("project") == project for c in read_json(path)["clips"]),
                f"a clip healed by file name has no project id: {path}")
    e.stop()
    e.start()
    require(resolves(killed), "timeline did not stay healed across a restart")
    evidence["healed"] = [str(finder), str(killed)]
    # Flat recordings stay where they were, unchanged, and a killed one is
    # still recovered at launch.
    require({str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in flat[1:]} == flat_hashes,
            "flat recordings changed")
    e.stop()
    e.start()
    interrupted = [Path(r["manifest"]) for r in e.rpc("status").get("interrupted", [])]
    require(flat_manifest in interrupted, f"flat killed take not recovered: {interrupted}")
    require(read_json(flat_manifest)["status"] == "interrupted", "flat manifest not marked")
    counts["project_renamed"] = len(event)
    evidence.update(project=renamed["project"], events=counts,
                    flat=[str(p) for p in flat])


PLUGIN_PROBE = r"""
import json, os, socket, time
data = os.environ["ISOCAST_PLUGIN_DATA"]
def call(method, params=None):
    s = socket.socket(socket.AF_UNIX); s.connect(os.environ["ISOCAST_SOCKET"])
    s.sendall((json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}) + "\n").encode())
    return json.loads(s.makefile().readline())
def attempt(fn):
    try:
        fn(); return "ok"
    except OSError as err:
        return f"denied: {err.strerror}"
for _ in range(200):
    project = call("list_projects")["result"]["projects"][0]
    if project["files"]:
        break
    time.sleep(0.05)
media = project["files"][0]
mine = os.path.join(project["folder"], os.environ["ISOCAST_PLUGIN_ID"] + "-note.txt")
results = {
    "read_media": attempt(lambda: open(media, "rb").read(64)),
    "list_folder": attempt(lambda: os.listdir(project["folder"])),
    "create_file": attempt(lambda: open(mine, "x").write("transcript\n")),
    "append_media": attempt(lambda: open(media, "ab").write(b"x")),
    "truncate_media": attempt(lambda: os.truncate(media, 0)),
    "delete_media": attempt(lambda: os.remove(media)),
    "rename_media": attempt(lambda: os.rename(media, media + ".moved")),
    "hard_link_media": attempt(lambda: os.link(media, os.path.join(data, "link.mov"))),
    "hard_link_in_project": attempt(lambda: os.link(media, os.path.join(project["folder"], "link.mov"))),
    "symlink_in_project": attempt(lambda: os.symlink(os.path.join(data, "steal.mov"),
                                                      os.path.join(project["folder"], "next.mov"))),
    "create_partial": attempt(lambda: open(os.path.join(project["folder"], "x.session.json.partial"), "x").write("{}")),
    "create_in_non_project": attempt(lambda: open(os.path.join(os.path.dirname(project["folder"]), "legacy-sub", "x.txt"), "x").write("x")),
    "create_manifest": attempt(lambda: open(os.path.join(project["folder"], "fake.session.json"), "x").write("{}")),
    "create_in_root": attempt(lambda: open(os.path.join(os.path.dirname(project["folder"]), "x.txt"), "x").write("x")),
    "rewrite_own_file": attempt(lambda: open(mine, "a").write("more\n")),
    "create_in_fake_project": attempt(lambda: open(os.path.join(os.path.dirname(project["folder"]),
                                                               "2026-01-01 00-00-00 Fake", "x.txt"), "x").write("x")),
    "folder": project["folder"],
}
with open(os.path.join(data, "results.json"), "w") as out:
    json.dump(results, out)
while True:
    time.sleep(60)
"""


def scenario_plugin_files(e, cfg, evidence):
    """7.7c: recordings = read / write grants, enforced by the sandbox."""
    e.apply(cfg)
    path, manifest = take(e, 2)
    # A folder in the recordings folder that isn't a project's.
    (e.root / "recordings" / "legacy-sub").mkdir()
    media = files_of(path)
    before = {str(f): hashlib.sha256(f.read_bytes()).hexdigest() for f in media}
    results = {}
    for grant in (None, "read", "write"):
        name = f"probe-{grant or 'none'}"
        folder = e.root / name
        folder.mkdir()
        (folder / "probe.py").write_text(PLUGIN_PROBE)
        permissions = {"methods": ["list_projects"]}
        if grant:
            permissions["recordings"] = grant
        lines = [f'id = "{name}"', 'name = "Probe"', 'version = "0.1.0"', "api = 1",
                 "[permissions]", 'methods = ["list_projects"]']
        if grant:
            lines.append(f'recordings = "{grant}"')
        lines += ["[process]", 'command = ["python3", "probe.py"]', "autostart = true"]
        (folder / "isocast-plugin.toml").write_text("\n".join(lines) + "\n")
        added = e.rpc("plugin_add", {"path": str(folder), "grant": permissions})
        out = e.root / "plugins" / name / "data" / "results.json"
        until(lambda: out.exists() and out.read_text(), 30, f"{name} results")
        time.sleep(0.2)
        results[grant or "none"] = read_json(out)
        evidence.setdefault("asked", {})[name] = added.get("plugin", {}).get("granted")
    evidence["results"] = results
    none, read, write = results["none"], results["read"], results["write"]
    require(none["read_media"].startswith("denied") and none["list_folder"].startswith("denied"),
            f"no grant could read recordings: {none}")
    require(read["read_media"] == "ok" and read["list_folder"] == "ok", f"read grant can't read: {read}")
    require(write["read_media"] == "ok" and write["create_file"] == "ok",
            f"write grant can't add a file: {write}")
    for result in (none, read):
        require(result["create_file"].startswith("denied"), f"created a file without write: {result}")
    for result in (none, read, write):
        for key in ("append_media", "truncate_media", "delete_media", "rename_media",
                    "hard_link_media", "hard_link_in_project", "symlink_in_project",
                    "create_partial", "create_in_non_project", "create_manifest",
                    "create_in_root"):
            require(result[key].startswith("denied"), f"{key} was allowed: {result}")
    require(write["rewrite_own_file"].startswith("denied"), "write grant reopened a file for writing")
    require({str(f): hashlib.sha256(f.read_bytes()).hexdigest() for f in media} == before,
            "a plugin changed the media")
    require((path.parent / "probe-write-note.txt").read_text() == "transcript\n",
            "the write grant's new file is missing or wrong")
    evidence["files"] = check_take(path, read_json(path), 2, evidence=evidence)
    # Links planted (here, unsandboxed) at the names the next take would
    # get: the engine must take other names, never write through them.
    project = path.parent
    bait = e.root / "bait"
    bait.mkdir()
    now = datetime.datetime.now()
    planted = []
    for i in range(0, 40):
        stem = (now + datetime.timedelta(seconds=i)).strftime("%Y-%m-%d %H-%M-%S")
        for name in (stem + ".mov", stem + ".session.json", stem + ".session.json.partial"):
            link = project / name
            if not os.path.lexists(link):
                os.symlink(bait / name, link)
                planted.append(link)
        # The numbered take's manifest scratch file, which the engine
        # must replace rather than write through.
        os.symlink(bait / (stem + " 2.session.json.partial"), project / (stem + " 2.session.json.partial"))
    started = e.rpc("start", {"seconds": 2, "folder": str(project)})
    second = Path(started["manifest"])
    m = until(lambda: finished(second), 40, "take beside planted links")
    until(lambda: e.rpc("status")["state"] == "idle", description="idle recorder")
    for f in [second, *map(Path, started["files"])]:
        require(f.parent == project and not f.is_symlink() and f.is_file(),
                f"the engine wrote through or onto a planted link: {f}")
    require(not any(bait.iterdir()), f"files appeared at link targets: {list(bait.iterdir())}")
    require(all(p.is_symlink() for p in planted), "a planted link was replaced")
    evidence["planted_links"] = len(planted)
    evidence["files"] += check_take(second, m, 2, evidence=evidence)
    for link in project.iterdir():
        if link.is_symlink():
            link.unlink()
    # Sandboxes follow the recordings folders live (#56): a folder named
    # like a project made since the plugins started restarts the write
    # grant's process (only), and its new sandbox keeps it out.
    out = {g: e.root / "plugins" / f"probe-{g}" / "data" / "results.json"
           for g in ("none", "read", "write")}
    fake = e.root / "recordings" / "2026-01-01 00-00-00 Fake"
    fake.mkdir()
    for f in out.values():
        f.unlink()
    started = e.rpc("start", {"seconds": 2})
    until(lambda: out["write"].exists() and out["write"].read_text(), 30, "write probe restarted")
    time.sleep(0.2)
    rerun = read_json(out["write"])
    require(rerun["create_in_fake_project"] == "denied: Operation not permitted",
            f"a folder named like a project made after the plugin started is writable: {rerun}")
    require(rerun["create_file"] == "ok", f"write grant lost its project after a restart: {rerun}")
    until(lambda: finished(Path(started["manifest"])), 40, "take after fake folder")
    until(lambda: e.rpc("status")["state"] == "idle", description="idle recorder")
    require(not out["none"].exists() and not out["read"].exists(),
            "plugins without a write grant were restarted for a folder named like a project")
    # A recordings folder used for the first time: every sandboxed plugin
    # starts again with it in its rules.
    second_root = e.root / "recordings2"
    started = e.rpc("start", {"seconds": 2, "folder": str(second_root)})
    for g in out:
        until(lambda: out[g].exists() and out[g].read_text(), 30, f"{g} probe restarted")
    time.sleep(0.2)
    reruns = {g: read_json(f) for g, f in out.items()}
    evidence["live_rules"] = reruns
    require(all(Path(r["folder"]).parent == second_root for r in reruns.values()),
            f"probes did not see the new recordings folder's project: {reruns}")
    require(reruns["read"]["read_media"] == "ok" and reruns["write"]["read_media"] == "ok"
            and reruns["write"]["create_file"] == "ok",
            f"a grant can't use a recordings folder first used after it started: {reruns}")
    require(reruns["none"]["read_media"].startswith("denied"),
            f"no grant could read a new recordings folder: {reruns['none']}")
    until(lambda: finished(Path(started["manifest"])), 40, "take in the new recordings folder")
    until(lambda: e.rpc("status")["state"] == "idle", description="idle recorder")


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


def check_replay(saved, seconds, keyframe_seconds, fps):
    """A saved replay: H.264 + AAC from a keyframe at 0, at least `seconds`
    long (at most a keyframe interval more), A/V equal, decoding cleanly,
    every flash with its beep."""
    path = Path(saved["path"])
    require(path.is_file() and not path.name.startswith("."), f"replay not saved under its name: {path}")
    info = probe(path)
    video = next(s for s in info["streams"] if s["codec_type"] == "video")
    audio = [s for s in info["streams"] if s["codec_type"] == "audio"]
    require(video["codec_name"] == "h264" and [a["codec_name"] for a in audio] == ["aac"],
            f"{path.name}: streams {video['codec_name']} / {[a['codec_name'] for a in audio]}")
    vd, ad = float(video["duration"]), float(audio[0]["duration"])
    first = json.loads(command(["ffprobe", "-v", "error", "-select_streams", "v:0", "-show_packets",
                                "-read_intervals", "%+#1", "-show_entries", "packet=flags,pts_time",
                                "-of", "json", path]).stdout)["packets"][0]
    require("K" in first["flags"] and abs(float(first["pts_time"])) < 1e-6,
            f"{path.name}: doesn't start with a keyframe at 0: {first}")
    if not saved.get("short"):
        require(seconds - 1e-6 <= vd <= seconds + keyframe_seconds + 2 / fps,
                f"{path.name}: {vd:.3f} s for a {seconds} s buffer")
    require(abs(vd - ad) < 0.025, f"{path.name}: A/V durations differ: {vd} / {ad}")
    frames = int(video["nb_read_frames"])
    require(frames == saved["frames"] and abs(frames + saved["dropped_frames"] - round(vd * fps)) <= 1,
            f"{path.name}: {frames} frames (+{saved['dropped_frames']} missed) for {vd:.3f} s")
    decoded = ffmpeg("-xerror", "-i", path, "-map", "0", "-f", "null", "-", timeout=90)
    require(not decoded.stderr.strip(), f"{path.name}: decode warnings: {decoded.stderr.decode()}")
    # Pulses cut by either end don't count; every other flash has its beep.
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
    inner = [t for t in flashes if 0.15 < t < vd - 0.2]
    require(len(inner) >= int(vd) - 2, f"{path.name}: missing flashes {flashes}")
    offsets = [min((abs(t - b) for b in beeps), default=1) for t in inner]
    require(all(d <= 1 / fps + 1 / RATE for d in offsets),
            f"{path.name}: flash/beep apart by {max(offsets) * 1000:.2f} ms")
    return {"file": path.name, "seconds": round(vd, 4), "frames": frames,
            "dropped": saved["dropped_frames"], "bytes": saved["bytes"],
            "max_sync_ms": round(max(offsets, default=0) * 1000, 3),
            "last_flash_to_end": round(vd - flashes[-1], 4) if flashes else None}


def scenario_replay(e, cfg, evidence):
    """The replay buffer beside a recording and a stream: the same take and
    stream without it, then with it and saves during them. Every output
    keeps the loss bar in both; each save is checked. (ISOs stay off: with
    a stream they shed pictures by design while #41 stands.)"""
    width, height, fps = (cfg["canvas"][k] for k in ("width", "height", "fps"))
    if not evidence["capabilities"]["h264"]:
        raise Skip("no hardware encoder on this runner (H.264 replay buffer)")
    try:
        e.optional("replay_save")
        raise Failure("replay_save worked with no buffer running")
    except RpcError as exc:
        require("isn't running" in exc.error.get("message", ""), f"replay_save while off: {exc}")
    seconds = 5
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    cfg["recording"]["replay"] = {"seconds": seconds}
    cfg["stream"] = {"service": "custom", "server": f"rtmp://127.0.0.1:{port}/live",
                     "video_bitrate_kbps": 6000, "audio_bitrate_kbps": 160,
                     "width": width, "height": height, "keyframe_interval_seconds": 2}
    e.apply(cfg)
    e.rpc("engine", {"running": True})
    time.sleep(1)
    passes = {}
    for with_replay in (False, True):
        name = "with_replay" if with_replay else "without_replay"
        output = e.root / f"{name}.flv"
        with open(e.root / f"{name}-listener.log", "wb") as log:
            listener = subprocess.Popen(["ffmpeg", "-nostdin", "-hide_banner", "-v", "warning",
                                         "-listen", "1", "-i", f"rtmp://127.0.0.1:{port}/live/e2e",
                                         "-c", "copy", "-y", str(output)], stdout=log, stderr=log)
            try:
                time.sleep(0.5)
                require(listener.poll() is None, "RTMP listener failed to start")
                if with_replay:
                    require(e.rpc("replay_start")["state"] == "running", "replay buffer didn't start")
                e.rpc("stream_start")
                if with_replay:
                    require(e.rpc("replay_status")["shared_encoder"],
                            "the buffer should share the stream's encoder (same format)")
                started = e.rpc("start", {"seconds": seconds + 6})
                saves = []
                if with_replay:
                    time.sleep(seconds + 1.5)
                    for pause in (0, 0.4, 1.1):
                        time.sleep(pause)
                        saves.append(e.rpc("replay_save"))
                manifest = until(lambda: finished(started["manifest"]), seconds + 40, "take finalization")
                until(lambda: e.rpc("status")["state"] == "idle", description="idle recorder")
                live = e.rpc("status").get("engine") or {}
                clocks = [{k: c[k] for k in ("id", "wake_max_us", "late_wakes")}
                          for c in live.get("clocks", [])]
                clocks.append({"id": "compositor", "reused": live.get("compositor", {}).get("reused")})
                final = e.rpc("stream_stop")
                listener.wait(timeout=10)
            finally:
                if listener.poll() is None:
                    listener.kill()
                    listener.wait(timeout=5)
        slots = final["frames_sent"] + final["dropped_frames"]
        loss = {"output": f"stream ({name})", "frames": final["frames_sent"],
                "dropped": final["dropped_frames"], "slots": slots,
                "ratio": final["dropped_frames"] / slots if slots else None, "repeated": 0,
                "audio_dropouts": final["audio_dropouts"], "target_fps": fps,
                "delivered_fps": final["frames_sent"] / final["elapsed_seconds"]}
        evidence.setdefault("drops", {})[str(output)] = loss
        check_loss(loss, loss_limit(evidence))
        evidence.setdefault("clocks", {})[name] = clocks
        files = check_take(Path(started["manifest"]), manifest, evidence=evidence)
        ffmpeg("-xerror", "-i", output, "-f", "null", "-")
        replays = [check_replay(saved, seconds, 2, fps) for saved in saves]
        passes[name] = {"files": files, "stream_dropped": final["dropped_frames"],
                        "recording_dropped": sum(r["dropped"] for r in drop_metrics(manifest)),
                        "replays": replays}
    # After the stream stops the buffer keeps its encoder (no one else's).
    status = e.rpc("replay_status")
    require(status["state"] == "running" and not status["shared_encoder"],
            f"the buffer should run on after the stream: {status}")
    require(status["memory_bytes"] > 0 and (status["dropped_frames"] == 0 or loss_limit(evidence)),
            f"replay status: {status}")
    final = e.rpc("replay_stop")
    require(final["state"] == "off" and final["saves"] == 3, f"replay stop: {final}")
    evidence.update(passes=passes, replay=final)


def scenario_bug_report(e, cfg, evidence):
    e.apply(cfg)
    result = e.optional("bug_report", {"out": str(e.root / "bug-report"), "describe": "Synthetic e2e"})
    archives = list(e.root.rglob("*.zip"))
    require(bool(archives), f"bug_report did not create an isolated archive: {result}")
    with zipfile.ZipFile(archives[0]) as archive:
        require(archive.testzip() is None and bool(archive.namelist()), "bug report archive corrupt/empty")
        evidence["archive_entries"] = len(archive.namelist())
    evidence["report"] = result
    # submit to an unreachable service: the redaction check passes, the
    # zip is kept and submit_error says why (never a real upload).
    sent = e.rpc("bug_report", {"out": str(e.root / "bug-report-submit"), "describe": "Synthetic e2e", "submit": True})
    require(sent.get("submitted") is None and "couldn't reach" in (sent.get("submit_error") or ""),
            f"submit to an unreachable service did not fail cleanly: {sent.get('submit_error')}")
    require(Path(sent["archive"]).is_file(), "submit failure lost the zip")
    evidence["submit_error"] = sent["submit_error"]


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
        for path in (root / "recordings").rglob("*.session.json"):
            if path.is_symlink() or not path.is_file():
                continue  # plugin_files plants dangling links
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
