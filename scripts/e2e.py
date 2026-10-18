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

The frame oracle (#124): the oracle_* scenarios record generated fixture
clips whose every frame carries its frame number as a block code (and whose
sound marks every second), and check every picture of every output (program,
ISOs, canvases, splits, pauses, scene switches, masks, replay saves, edited
exports and NLE project files) against an independent model; a failure
names the first wrong picture, expected vs actual, with a contact sheet.
  --quick      the lanes' suite while they work (about 2-3 min on the VM)
  --full       every scenario (the default), before a merge
  --selftest   the fixture and decoder alone, through every encoder ffmpeg
               has here; runs on any OS (Windows: scripts/oracle-windows.sh)
  --pattern-oracle MOVIE --fps N
               a file of the engine's test pattern, frame by frame (any OS)
It is strict, unconditionally: any mismatch fails with a contact sheet.
"""

import argparse
import array
import base64
import bisect
import copy
import datetime
try:
    import fcntl  # not on Windows, where only --selftest and --pattern-oracle run
except ImportError:
    fcntl = None
import hashlib
import json
import math
import os
import platform
import re
import shutil
import signal
import socket
import stat
import statistics
import struct
import subprocess
import sys
import tempfile
import time
import traceback
import wave
import zipfile
import urllib.parse
import xml.etree.ElementTree as ET
from fractions import Fraction
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
SCENARIOS = ("setup", "record", "iso", "split", "scenes", "stream", "timeline", "edit",
             "data", "plugin", "hotkeys", "shutdown", "pause", "bug_report", "projects",
             "plugin_files", "replay", "vertical",
             # The frame oracle (#124), defined below.
             "oracle_record", "oracle_scenes", "oracle_takes", "oracle_edit", "oracle_masks",
             "oracle_canvases", "oracle_replay", "oracle_governor",
             # The same model on the engine's test pattern (Windows has no media files yet).
             "win_record", "win_takes", "win_scenes")
FPS = 60
RATE = 48000
HOSTED_DROP_LIMIT = 0.25
# The longest run folder whose scenarios' sockets all fit in sun_path.
SHORT_ROOT = 44


class Failure(Exception):
    pass


class Skip(Exception):
    pass


class Pending(Skip):
    """Something this platform doesn't do yet (Windows: previews,
    streaming): reported PENDING, never as a pass."""


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
    if WINDOWS:
        # The engine encodes with NVENC or Media Foundation itself; whether
        # it can is what the scenarios find out. Nothing here streams yet.
        result.update(cores=os.cpu_count(), virtual_machine=0, gpus=[], hevc=True, h264=True,
                      record_codec="hevc", platform="windows",
                      pending=["previews (scene feeds, snapshots)", "streaming", "replay buffer"])
        return result
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


def short_root(work):
    """A run folder short enough for every scenario's sockets: at the top of
    --work's disk if it can be made there, else in /tmp."""
    mount = work
    device = work.stat().st_dev
    while mount.parent != mount and mount.parent.stat().st_dev == device:
        mount = mount.parent
    for base in (mount / ".e2e", Path("/tmp")):
        try:
            base.mkdir(exist_ok=True)
            root = Path(tempfile.mkdtemp(prefix="e2e-", dir=base)).resolve()
        except OSError:
            continue
        if len(os.fsencode(root)) <= SHORT_ROOT:
            return root
        root.rmdir()
    raise Failure("no folder short enough for Unix sockets; give a shorter --work")


def keep_under(root, work, report):
    """Copy a short run folder's kept evidence into --work (sockets left
    out: they can't be copied), point the report at it, and remove the
    short folder."""
    dest = work / root.name
    def sockets(folder, names):
        return [n for n in names if stat.S_ISSOCK(os.lstat(os.path.join(folder, n)).st_mode)]
    shutil.copytree(root, dest, symlinks=True, ignore=sockets)
    shutil.rmtree(root)
    report["work"] = str(dest)
    for result in report["scenarios"]:
        if result.get("artifacts"):
            result["artifacts"] = str(dest / Path(result["artifacts"]).relative_to(root))
    print(f"e2e: kept evidence moved to {dest}", file=sys.stderr)
    return dest


WINDOWS = sys.platform == "win32"


def windows_pipe(path):
    """The named pipe isocastd listens on for the logical socket `path` on
    Windows, computed as isocast-ipc does (crates/isocast-ipc/src/windows/
    name.rs): sha256 of a versioned header, this user's binary SID and the
    invariant-uppercased path (its parent canonicalised as Rust's
    std::fs::canonicalize does, with the \\\\?\\ prefix), length-delimited."""
    import ctypes
    from ctypes import wintypes
    k32 = ctypes.WinDLL("kernel32", use_last_error=True)
    adv = ctypes.WinDLL("advapi32", use_last_error=True)
    H = wintypes.HANDLE
    k32.CreateFileW.restype = H
    k32.CreateFileW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD, ctypes.c_void_p,
                                wintypes.DWORD, wintypes.DWORD, H]
    k32.GetFinalPathNameByHandleW.argtypes = [H, wintypes.LPWSTR, wintypes.DWORD, wintypes.DWORD]
    k32.CloseHandle.argtypes = [H]
    k32.GetCurrentProcess.restype = H
    k32.LCMapStringEx.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.LPCWSTR, ctypes.c_int,
                                  wintypes.LPWSTR, ctypes.c_int, ctypes.c_void_p, ctypes.c_void_p, ctypes.c_void_p]
    adv.OpenProcessToken.argtypes = [H, wintypes.DWORD, ctypes.POINTER(H)]
    adv.GetTokenInformation.argtypes = [H, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD,
                                        ctypes.POINTER(wintypes.DWORD)]
    absolute = os.path.abspath(path)
    parent, name = os.path.split(absolute)
    handle = k32.CreateFileW(parent, 0, 7, None, 3, 0x02000000, None)  # FILE_FLAG_BACKUP_SEMANTICS
    require(handle not in (None, ctypes.c_void_p(-1).value), f"can't open {parent}: {ctypes.get_last_error()}")
    try:
        buf = ctypes.create_unicode_buffer(32768)
        require(k32.GetFinalPathNameByHandleW(handle, buf, 32768, 0), "GetFinalPathNameByHandleW failed")
    finally:
        k32.CloseHandle(handle)
    normalized = buf.value.rstrip("\\") + "\\" + name
    out = ctypes.create_unicode_buffer(len(normalized) * 2 + 2)
    written = k32.LCMapStringEx("", 0x200, normalized, len(normalized), out, len(out), None, None, 0)
    require(written, f"LCMapStringEx failed: {ctypes.get_last_error()}")
    folded = out[:written].encode("utf-16-le")
    token = wintypes.HANDLE()
    require(adv.OpenProcessToken(k32.GetCurrentProcess(), 0x0008, ctypes.byref(token)), "OpenProcessToken failed")
    try:
        size = wintypes.DWORD()
        adv.GetTokenInformation(token, 1, None, 0, ctypes.byref(size))  # TokenUser
        info = ctypes.create_string_buffer(size.value)
        require(adv.GetTokenInformation(token, 1, info, size, ctypes.byref(size)), "GetTokenInformation failed")
    finally:
        k32.CloseHandle(token)
    sid_ptr = ctypes.cast(info, ctypes.POINTER(ctypes.c_void_p))[0]
    adv.GetLengthSid.argtypes = [ctypes.c_void_p]
    sid = ctypes.string_at(sid_ptr, adv.GetLengthSid(sid_ptr))
    digest = hashlib.sha256(b"isocast.local-pipe\0v2\0" + len(sid).to_bytes(8, "little") + sid
                            + len(folded).to_bytes(8, "little") + folded).hexdigest()
    return "\\\\.\\pipe\\isocast-" + digest


class PipeConnection:
    """A named-pipe client with the socket calls the harness uses."""

    def __init__(self, name, timeout):
        deadline = time.monotonic() + timeout
        while True:
            try:
                self.file = open(name, "r+b", buffering=0)
                break
            except OSError:  # busy (ERROR_PIPE_BUSY) or not listening yet
                if time.monotonic() > deadline:
                    raise
                time.sleep(0.02)

    def settimeout(self, _):
        pass  # the worker's own timeout bounds a stuck read

    def sendall(self, data):
        self.file.write(data)

    def makefile(self, _mode):
        import io
        return io.BufferedReader(self.file)

    def close(self):
        try:
            self.file.close()
        except OSError:
            pass

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()


def engine_connection(sock_path, timeout=30):
    """A connection to the engine at its logical socket path: a Unix socket
    on macOS, the named pipe isocast-ipc derives from it on Windows."""
    if WINDOWS:
        return PipeConnection(windows_pipe(sock_path), timeout)
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(timeout)
    sock.connect(str(sock_path))
    return sock


ENGINE_ENV = {}  # --engine-env


class Engine:
    def __init__(self, root, daemon):
        self.root = root
        self.sock = root / "d.sock"
        if WINDOWS:
            # No app bundle or signing; the logical socket names a pipe.
            root.mkdir(parents=True, exist_ok=True)
            self.app = None
            self.binary = Path(daemon)
        else:
            require(len(os.fsencode(self.sock)) < 104,
                    "--work path too long for a macOS Unix socket; use a shorter path")
            self.app = root / "Dev.app"
            binary = self.app / "Contents/MacOS/isocastd"
            binary.parent.mkdir(parents=True)
            shutil.copy2(daemon, binary)
            shutil.copy2(REPO / "crates/isocast-daemon/Info.plist", self.app / "Contents/Info.plist")
            command(["codesign", "--force", "--sign", "-", self.app])
            command(["codesign", "--verify", self.app])
            self.binary = binary
        # Drop inherited Isocast overrides (including any user's stream key).
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("ISOCAST_")}
        self.env.update(ENGINE_ENV)
        self.env.update(ISOCAST_CALIBRATION="off", ISOCAST_SOCKET=str(self.sock), ISOCAST_SETUP=str(root / "setup.toml"),
                        ISOCAST_PLUGINS=str(root / "plugins"),
                        ISOCAST_TIMELINES=str(root / "timelines"), ISOCAST_MONITOR_SILENT="1",
                        ISOCAST_STREAM_KEY="e2e", ISOCAST_LOG_DIR=str(root / "logs"),
                        # Never the real report service: bug_report's submit fails cleanly.
                        ISOCAST_REPORT_ENDPOINT="http://127.0.0.1:9/reports")
        if self.app:
            self.env["ISOCAST_DEV_APP"] = str(self.app)
        self.log = open(root / "daemon.log", "ab", buffering=0)
        self.process = None
        self.start()

    def start(self):
        # Its own folder as the working directory: anything it (or a tool it
        # starts) writes to a relative path stays in the scenario's folder.
        self.process = subprocess.Popen([str(self.binary)], env=self.env, stdout=self.log, stderr=self.log,
                                        cwd=str(self.root))

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
        with engine_connection(self.sock, timeout) as sock:
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
    if WINDOWS:
        raise Pending("streaming / the replay buffer: not on Windows yet")
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


def scenario_vertical(e, cfg, evidence):
    if WINDOWS:
        raise Pending("streaming / the replay buffer: not on Windows yet")
    if not evidence["capabilities"]["h264"]:
        raise Skip("no hardware encoder on this runner (vertical H.264 streaming)")
    # Two HD programs and one HD stream, at a rate the base M2 can protect.
    # Keep this scenario strict even when the hosted suite allows ISO loss.
    cfg["canvas"]["fps"] = min(cfg["canvas"]["fps"], 30)
    fps = cfg["canvas"]["fps"]
    width, height = cfg["canvas"]["height"], cfg["canvas"]["width"]
    for source in cfg["sources"]:
        if source["kind"] == "test_pattern":
            source["fps"] = fps
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    cfg["vertical"] = {
        "width": width, "height": height, "fps": fps, "follow_main": False,
        "recording": {"encoder": cfg["recording"]["program"]["encoder"]},
        "stream": {"service": "custom", "server": f"rtmp://127.0.0.1:{port}/live",
                   "video_bitrate_kbps": 4000, "audio_bitrate_kbps": 160,
                   "keyframe_interval_seconds": 2}}
    cfg["scenes"].append({"id": "second", "items": [{"source": "bars2", "rect": {
        "x": 0, "y": 0, "width": cfg["canvas"]["width"], "height": cfg["canvas"]["height"]}}]})
    e.apply(cfg)
    e.rpc("engine", {"running": True})
    time.sleep(1)
    output = e.root / "vertical.flv"
    takes = []
    with open(e.root / "vertical-listener.log", "wb") as log:
        listener = subprocess.Popen(["ffmpeg", "-nostdin", "-hide_banner", "-v", "warning",
                                     "-listen", "1", "-i", f"rtmp://127.0.0.1:{port}/live/e2e",
                                     "-c", "copy", "-y", str(output)], stdout=log, stderr=log)
        try:
            time.sleep(0.5)
            require(listener.poll() is None, "vertical RTMP listener failed to start")
            takes.append(e.rpc("start", {}))
            e.rpc("stream_start", {"canvas": "vertical"})
            for scene in ("second", "main"):
                time.sleep(2)
                e.rpc("switch_scene", {"scene": scene, "canvas": "vertical"})
                takes.append(e.rpc("split"))
                begins = datetime.datetime.fromisoformat(takes[-1]["begins_at"]).timestamp()
                time.sleep(max(0, begins - time.time()) + 0.6)
            time.sleep(2)
            final = e.rpc("stream_stop", {"canvas": "vertical"})
            listener.wait(timeout=10)
            require(final["frames_sent"] > 0, "vertical stream sent no video")
            require(final["dropped_frames"] == 0 and final["audio_dropouts"] == 0,
                    f"vertical stream lost protected media: {final}")
            # A stale Stop action must be harmless after a failure or normal end.
            e.rpc("stream_stop", {"canvas": "vertical"})
            evidence["stream"] = final
            e.rpc("stop")
        finally:
            if listener.poll() is None:
                listener.kill()
                listener.wait(timeout=5)
    evidence["files"] = []
    require(len({t["manifest"] for t in takes}) == 3, "vertical splits reused a manifest")
    for index, started in enumerate(takes):
        path = Path(started["manifest"])
        manifest = until(lambda: finished(path), description="dual-canvas split finalization")
        require(all(f["dropped_frames"] == 0 and f["audio_dropouts"] == 0
                    for f in manifest["files"]), "dual-canvas recording lost protected media")
        require({s["scene"] for s in manifest["scenes"]} == {"main"},
                "independent vertical switch changed the main scene timeline")
        require(manifest["vertical_scenes"][0]["scene"] == ("second" if index == 1 else "main"),
                "vertical split lost its initial scene")
        evidence["files"].extend(check_take(path, manifest, expected_sources=["program", "vertical-program"],
                                            evidence=evidence))
        main, vertical = (next(f for f in manifest["files"] if f["source"] == source)
                          for source in ("program", "vertical-program"))
        require((vertical["width"], vertical["height"]) == (width, height), "vertical file size is wrong")
        require(abs(main["duration_seconds"] - vertical["duration_seconds"]) <= 1 / fps,
                "dual-canvas split durations differ")
    info = probe(output)
    video = next(s for s in info["streams"] if s["codec_type"] == "video")
    require((video["width"], video["height"]) == (width, height), "vertical stream size is wrong")
    packets = json.loads(command(["ffprobe", "-v", "error", "-show_packets", "-of", "json", output]).stdout)["packets"]
    for kind, gap in (("video", 1 / fps + 0.002), ("audio", 0.023)):
        times = [float(p["pts_time"]) for p in packets if p["codec_type"] == kind]
        require(len(times) > 2 and all(0 < b - a <= gap for a, b in zip(times, times[1:])),
                f"vertical stream has missing or non-monotonic {kind} packets")
    ffmpeg("-xerror", "-i", output, "-map", "0", "-f", "null", "-")


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
    record_targets(e, evidence)


def timeline_of(e, manifest):
    """The timelines whose clips include `manifest`."""
    found = []
    for summary in e.rpc("timeline_list")["timelines"]:
        info = e.rpc("timeline_get", {"timeline": summary["id"]})
        if any(Path(c["take"]).resolve() == Path(manifest).resolve() for c in info["clips"]):
            found.append(info)
    return found


def record_targets(e, evidence):
    """#114: each recording starts a new project with its own timeline
    unless a timeline was chosen; a chosen one gets the take in its last
    take's folder; a split and a retake stay with their recording; a
    current timeline from before 0.0.8 is migrated, shown and not used."""
    out = evidence["record_target"] = {}
    # `timeline_new` chose "E2E" above; back to new projects.
    e2e_clips = e.rpc("timeline_get", {"timeline": "e2e"})["clips"]
    chose = e.rpc("record_target", {"target": "new"})
    require(chose.get("timeline") is None, f"record_target new: {chose}")
    folders, timelines = [], []
    for _ in range(3):
        path, _ = take(e, 1)
        on = timeline_of(e, path)
        require(len(on) == 1 and len(on[0]["clips"]) == 1,
                f"a default recording's take is not alone on one timeline: {[t['id'] for t in on]}")
        folders.append(path.parent)
        timelines.append(on[0]["id"])
    require(len(set(folders)) == 3 and len(set(timelines)) == 3 and "e2e" not in timelines,
            f"three default recordings share a project or timeline: {folders} {timelines}")
    require(e.rpc("timeline_get", {"timeline": "e2e"})["clips"] == e2e_clips,
            "the E2E timeline changed")
    out["new_projects"] = {"folders": [str(f) for f in folders], "timelines": timelines}
    # Choose the first new project: the next take is its take 2, in its folder.
    target = e.rpc("record_target", {"target": timelines[0]})
    require(target["timeline"] == timelines[0] and target["take"] == 2
            and Path(target["folder"]) == folders[0], f"record_target: {target}")
    require(e.rpc("status")["record_target"]["timeline"] == timelines[0], "status lacks the target")
    path, _ = take(e, 1)
    chosen = e.rpc("timeline_get", {"timeline": timelines[0]})
    require(path.parent == folders[0] and len(chosen["clips"]) == 2
            and Path(chosen["clips"][1]["take"]).resolve() == path.resolve(),
            f"a take into the chosen timeline did not land as its take 2: {path}")
    # `into` for one recording; `new_project` overrides the choice.
    started = e.rpc("start", {"seconds": 1, "into": timelines[1]})
    until(lambda: finished(started["manifest"]), 40)
    until(lambda: e.rpc("status")["state"] == "idle", description="idle recorder")
    path = Path(started["manifest"])
    require(path.parent == folders[1] and len(e.rpc("timeline_get", {"timeline": timelines[1]})["clips"]) == 2,
            "start into did not add a take to that timeline")
    started = e.rpc("start", {"seconds": 1, "new_project": True})
    require(started.get("timeline", {}).get("new_project"), f"start new_project: {started}")
    until(lambda: finished(started["manifest"]), 40)
    until(lambda: e.rpc("status")["state"] == "idle", description="idle recorder")
    require(Path(started["manifest"]).parent not in folders, "new_project reused a project folder")
    require(e.rpc("record_target")["timeline"] == timelines[0], "a one-off start changed the choice")
    for bad in ({"into": "nope"}, {"into": timelines[0], "new_project": True}):
        try:
            e.rpc("start", bad)
            e.rpc("stop")
            raise Failure(f"start accepted {bad}")
        except RpcError as exc:
            field = "into" if "nope" in str(bad) else "new_project"
            require(field in exc.error["message"], f"start {bad}: {exc.error}")
    # A split and a retake while recording stay on the recording's own
    # (new) timeline.
    e.rpc("record_target", {"target": "new"})
    watch = Watch(e, ["stopped"])
    first = e.rpc("start", {})
    time.sleep(1.5)
    second = e.rpc("split")
    time.sleep(1.5)
    e.rpc("retake")
    time.sleep(1.5)
    e.rpc("stop")
    until(lambda: e.rpc("status")["state"] == "idle", description="idle recorder")
    until(lambda: watch.of("stopped"), 10, "stopped event")
    watch.close()
    into = watch.of("stopped")[0].get("timeline") or {}
    session = e.rpc("timeline_get", {"timeline": into.get("timeline", "")}) if into else {}
    require(into.get("new_project") and len(session.get("clips", [])) == 2
            and Path(session["clips"][0]["take"]) == Path(first["manifest"])
            and Path(session["clips"][1]["take"]).parent == Path(first["manifest"]).parent
            and any(Path(r["take"]) == Path(second["manifest"]) for r in session["removed"]),
            f"split and retake did not stay together: {into} {session.get('clips')}")
    require(into["takes"] == 2 and into["clips"] == 2, f"stopped event timeline: {into}")
    out["split_retake"] = {"timeline": into, "clips": [c["take"] for c in session["clips"]]}
    # A current timeline from before #114: migrated, shown, never used.
    e.stop()
    write_json(e.root / "timelines" / "current.json", {"timeline": "e2e"})
    e.start()
    status = e.rpc("status")
    target = status.get("record_target", {})
    require(status["timeline"]["id"] == "e2e" and target.get("timeline") is None
            and any("e2e" in n and "new project" in n for n in target.get("notes", [])),
            f"old current timeline not migrated visibly: {status.get('timeline')} {target}")
    before = len(e.rpc("timeline_get", {"timeline": "e2e"})["clips"])
    path, _ = take(e, 1)
    require(len(e.rpc("timeline_get", {"timeline": "e2e"})["clips"]) == before
            and len(timeline_of(e, path)) == 1, "a migrated current timeline was used")
    require(read_json(e.root / "timelines" / "current.json").get("record") is not None,
            "the migration was not written")
    e.rpc("record_target", {"target": "new"})
    require(not e.rpc("record_target").get("notes"), "the migration notice outlived a choice")
    out["migrated"] = target


# Gray levels that split the test pattern's 75% bars into bright (white to
# green) and dark (magenta to black), under BT.601 and BT.709 alike.
BAR_LEVELS = bytes(49 if v >= 96 else 48 for v in range(256))


def pattern_frames(path, width, height, region=None):
    """Per decoded picture of a synthetic test pattern: (pts, bar shift in
    pixels, label cell, flash band). The bars scroll by width / (4 s * fps)
    per source frame, so the shift names the source frame (modulo 4 s); the
    cell is the label's fifth character, lit for "BARS2" and dark for
    "BARS"; the band is the bottom quarter that flashes on every pulse.
    `region` (x, y, w, h): the part of each picture the pattern fills (a
    vertical file's letterboxed picture), scaled to width x height first."""
    s = max(height // 120, 1)
    first = "[0:v]format=gray"
    if region:
        x, y, w, h = region
        first = f"[0:v]crop={w}:{h}:{x}:{y},scale={width}:{height}:flags=bicubic,format=gray"
    graph = (f"{first},split=3[a][b][c];[a]crop={width}:1:0:{height // 2}[row];"
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


def picture_region(path, width, height, shape):
    """Where a picture of `shape` (w, h) sits in `path`'s letterboxed
    pictures (width x height): (0, top, width, rows). The pattern's bars
    start at its top (its bottom quarter is black between flashes)."""
    raw = ffmpeg("-i", path, "-frames:v", "1", "-pix_fmt", "gray", "-f", "rawvideo", "-").stdout
    require(len(raw) == width * height, f"{path.name}: first picture isn't {width}x{height}")
    lit = [y for y in range(height) if max(raw[y * width:(y + 1) * width]) > 40]
    require(lit, f"{path.name}: the first picture is black")
    rows = round(width * shape[1] / shape[0])
    require(abs((height - rows) / 2 - lit[0]) <= 2,
            f"{path.name}: the picture starts at row {lit[0]}, not centred ({(height - rows) / 2})")
    return (0, lit[0], width, min(rows, height - lit[0]))


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


def picture_rows(path, width, height, at):
    """Per decoded picture, its gray rows at fractions `at` of its height."""
    graph = "[0:v]format=gray,split={n}{outs};{crops}{ins}vstack={n}[out]".format(
        n=len(at), outs="".join(f"[s{i}]" for i in range(len(at))),
        crops="".join(f"[s{i}]crop={width}:1:0:{int(y * height)}[r{i}];" for i, y in enumerate(at)),
        ins="".join(f"[r{i}]" for i in range(len(at))))
    if len(at) == 1:
        graph = f"[0:v]format=gray,crop={width}:1:0:{int(at[0] * height)}[out]"
    raw = ffmpeg("-i", path, "-filter_complex", graph, "-map", "[out]", "-fps_mode", "passthrough",
                 "-pix_fmt", "gray", "-f", "rawvideo", "-", timeout=90).stdout
    size = len(at) * width
    require(len(raw) % size == 0, f"{path.name}: rows don't divide into pictures")
    return [[raw[i + k * width:i + (k + 1) * width] for k in range(len(at))]
            for i in range(0, len(raw), size)]


def cropped_picture(rows, crop, width, height, want):
    """Why a picture of `crop` (scaled to fill width x height) isn't the
    source picture `want` (pattern_frames' row), if it isn't: its bar edges
    halfway down must sit where the source's do in the crop, the band row
    (70% down: source 77.5%, the flashing band) must be the band, and the
    row at 60% (source 70%) the bars."""
    mid, bars, band = rows
    scale = crop["width"]  # source pixels per output pixel
    rise = (width - want[1] - 1) % width
    fall = (width // 2 - want[1] - 1) % width
    bits = mid.translate(BAR_LEVELS)
    edges = [(i, bits[i:i + 2]) for i in range(3, width - 4) if bits[i] != bits[i + 1]]
    if not edges:
        return "no bar edge halfway down"
    for i, kind in edges:
        source_x = crop["x"] * width + (i + 0.5) * scale - 0.5
        target = rise if kind == b"01" else fall
        apart = abs(source_x - target)
        if min(apart, width - apart) > 3:
            return (f"a bar edge at {i} shows source column {source_x:.1f}; the source's "
                    f"{'rise' if kind == b'01' else 'fall'} is at {target}")
    if max(band) - min(band) > 48:
        return f"the row at 70% isn't the band (levels {min(band)}–{max(band)})"
    if abs(sum(band) / width - want[3]) > 48:
        return f"the band is {sum(band) / width:.0f}, the source's {want[3]:.0f}"
    if max(bars) - min(bars) < 96:
        return f"the row at 60% isn't bars (levels {min(bars)}–{max(bars)})"
    return None


def video_packets(path):
    """The video stream's packets, each hashed (copied ones hash the same)."""
    lines = ffmpeg("-i", path, "-map", "0:v:0", "-c", "copy", "-f", "framemd5", "-").stdout.decode()
    return [line.rsplit(",", 1)[-1].strip() for line in lines.splitlines() if line and not line.startswith("#")]


def video_descriptions(path):
    """How many sample descriptions the file's video track has (#94: one,
    or Chrome can't decode HEVC exports)."""
    data = Path(path).read_bytes()

    def boxes(start, end):
        while start + 8 <= end:
            size, kind = struct.unpack(">I4s", data[start:start + 8])
            head = 8
            if size == 1:
                size, head = struct.unpack(">Q", data[start + 8:start + 16])[0], 16
            yield kind, start + head, start + (size or end - start)
            start += size or end - start

    def walk(start, end):
        for kind, s, e in boxes(start, end):
            if kind in (b"moov", b"trak", b"mdia", b"minf", b"stbl"):
                yield from walk(s, e)
            elif kind == b"hdlr" and data[s + 8:s + 12] == b"vide":
                yield "video"
            elif kind == b"stsd":
                yield struct.unpack(">I", data[s + 4:s + 8])[0]

    counts, video = [], False
    for item in walk(0, len(data)):
        if item == "video":
            video = True
        elif video:
            counts.append(item)
            video = False
    return counts[0] if len(counts) == 1 else counts


def scenario_edit(e, cfg, evidence):
    """C.3's edits on two program + 2 ISO takes: trim, split, ripple delete,
    an angle, undo and redo, export (with the vertical file, #94); then crops
    (#87) and export again. Every
    exported picture must be the take picture on screen at its moment (by
    the test pattern; a cropped one the crop of it, scaled up), every pulse
    must flash and beep together at the cuts, joins mustn't click, crops
    must change only the edited file, and the takes' files must be
    unchanged."""
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

    def export_into(folder, vertical=False):
        export = e.rpc("timeline_export", {"folder": str(e.root / folder), "vertical": vertical})
        def exported():
            value = read_json(export["manifest"])
            return value if value["status"] != "exporting" else None
        result = until(exported, 90, "timeline export")
        require(result["status"] == "complete" and not result.get("errors"), f"export failed: {result}")
        want = ["bars", "bars2", "edited", "program"] + (["vertical"] if vertical else [])
        require(sorted(f["source"] for f in result["files"]) == want,
                f"exported sources: {[f['source'] for f in result['files']]}, skipped {result.get('skipped')}")
        return result

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

    def check_export(result, cropped=None):
        """Every file of an export, picture by picture; `cropped` (the crop
        and the indexes of the segments it covers) applies to the edited
        file's pictures there."""
        rows = []
        for f in result["files"]:
            path, width, height = Path(f["path"]), f["width"], f["height"]
            require(all(abs(p["start_seconds"] - s[0]) < 1e-3 for p, s in zip(f["parts"], segments))
                    and len(f["parts"]) == len(segments), f"{path.name}: parts don't follow the clips")
            dropped = 0
            if loss_limit(evidence):
                frames = int(probe(path)["streams"][0]["nb_read_frames"])
                dropped = round(duration * fps) - frames
            canvas = (cfg["canvas"]["width"], cfg["canvas"]["height"])
            upright = f["source"] == "vertical"  # #94: the program's size on its side
            require((width, height) == (canvas[::-1] if upright else canvas),
                    f"{path.name}: the manifest says {width}x{height}")
            row = check_file(path, duration, fps, (width, height), dropped=dropped)
            descriptions = video_descriptions(path)
            require(descriptions == 1, f"{path.name}: {descriptions} video sample descriptions, not one")
            row["video_descriptions"] = descriptions
            # Every picture: the one on screen in its take at that moment.
            # A vertical file: each (uncropped) picture fitted into its
            # middle, letterboxed; checked scaled back to the canvas.
            region = picture_region(path, width, height, canvas) if upright else None
            if upright:
                row["picture_region"] = region
                width, height = canvas
            out = pattern_frames(path, width, height, region)
            crop_rows = crop_check = None
            if cropped and f["source"] == "edited":
                crop_check, crop_segments = cropped
                crop_rows = picture_rows(path, width, height, (0.5, 0.6, 0.7))
                require(len(crop_rows) == len(out), f"{path.name}: rows and pictures disagree")
            bad, checked_cropped = [], 0
            for n, (t, shift, cell, _) in enumerate(out):
                index = next((i for i, s in enumerate(segments)
                              if s[0] - 1e-6 <= t < s[0] + s[3] - s[2] - 1e-6), None)
                require(index is not None, f"{path.name}: a picture at {t:.4f} s, past the timeline")
                seg = segments[index]
                source = (seg[4] or "program") if f["source"] in ("edited", "vertical") else f["source"]
                src_path, offset = files[(seg[1], source)]
                if src_path not in sources:
                    frames = pattern_frames(src_path, width, height)
                    sources[src_path] = (frames, [x[0] - frames[0][0] for x in frames])
                src, src_times = sources[src_path]
                moment = seg[2] + (t - seg[0]) - offset
                want = src[max(0, bisect.bisect_right(src_times, moment + 0.25 / fps) - 1)]
                require((want[2] > 50) == (source == "bars2"), f"{src_path.name}: label cell can't tell the angles apart")
                if crop_rows is not None and index in crop_segments:
                    checked_cropped += 1
                    problem = cropped_picture(crop_rows[n], crop_check, width, height, want)
                    if problem:
                        bad.append(f"{t:.4f} s (cropped {source} at {seg[2] + t - seg[0]:.4f} s of its "
                                   f"take): {problem}")
                    continue
                apart = abs(shift - want[1])
                # Scaled down and up again: a column of the vertical file is
                # width / region width of the canvas's.
                slack = 3 if region is None else 3 * width / region[2] + 1
                if min(apart, width - apart) > slack or (cell > 50) != (want[2] > 50):
                    bad.append(f"{t:.4f} s: bars at {shift}, label {cell:.0f}; {source} at "
                               f"{seg[2] + t - seg[0]:.4f} s of its take has {want[1]}, {want[2]:.0f}")
            require(not bad, f"{path.name}: {len(bad)} of {len(out)} pictures aren't their take's "
                             f"at that moment: {bad[:4]}")
            if crop_rows is not None:
                want_cropped = sum(round((segments[i][3] - segments[i][2]) * fps) for i in crop_segments)
                require(abs(checked_cropped - want_cropped) <= len(crop_segments),
                        f"{path.name}: {checked_cropped} cropped pictures, expected {want_cropped}")
                row["cropped_pictures_checked"] = checked_cropped
            # Pulses: each expected one flashes and beeps; nothing else does
            # (a crop keeps the band's bottom, so it flashes too).
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
            rows.append(row)
        return rows

    # The first export also writes the vertical (9:16) file (#94).
    result = export_into("export", vertical=True)
    evidence["exported"] = check_export(result)
    vertical = next(f for f in result["files"] if f["source"] == "vertical")
    require(all(p["reencoded_frames"] == p["frames"] for p in vertical["parts"]),
            f"vertical: not every frame was encoded again: {vertical['parts']}")

    # Crop (#87): the edited file shows the lower-right three quarters of a
    # program clip and of the bars2 angle, scaled up to the program's size
    # (encoded again); every other picture, and the program and ISO files,
    # are as before (copied the same).
    crop = {"x": 0.25, "y": 0.25, "width": 0.75, "height": 0.75}
    crop_segments = (1, 4)
    for i in crop_segments:
        start, _, cut_in, cut_out, _ = segments[i]
        e.rpc("timeline_crop", {"start_seconds": start, "end_seconds": start + cut_out - cut_in, "crop": crop})
    clips = e.rpc("timeline_get")["clips"]
    require([c.get("crop") for c in clips] == [crop if i in crop_segments else None for i in range(len(segments))],
            f"crops: {[c.get('crop') for c in clips]}")
    try:
        e.rpc("timeline_crop", {"start_seconds": 0, "end_seconds": 1,
                                "crop": {"x": 0.5, "y": 0, "width": 0.75, "height": 1}})
        raise Failure("a crop past the picture's edge was accepted")
    except RpcError as err:
        require("crop.x, crop.width" in str(err), f"crop error doesn't name the fields: {err}")
    cropped = export_into("export-crop")
    require([c.get("crop") for c in cropped["clips"]] == [c.get("crop") for c in clips],
            "the export's manifest doesn't list the crops")
    evidence["exported_cropped"] = check_export(cropped, (crop, crop_segments))
    by_source = {f["source"]: f for f in result["files"]}
    for f in cropped["files"]:
        before = by_source[f["source"]]
        for i, (part, was) in enumerate(zip(f["parts"], before["parts"])):
            if f["source"] == "edited" and i in crop_segments:
                require(part["reencoded_frames"] == part["frames"],
                        f"edited: cropped clip {part['clip']} was not encoded again whole: {part}")
            else:
                require(part.get("reencoded_frames") == was.get("reencoded_frames"),
                        f"{f['source']}: clip {part['clip']} encoded {part.get('reencoded_frames')} frames "
                        f"again, {was.get('reencoded_frames')} before the crop")
        if f["source"] != "edited":
            # Copied packets are the same bytes; only the frames encoded
            # again at cuts (as before) may differ.
            now, was = video_packets(Path(f["path"])), video_packets(Path(before["path"]))
            again = sum(p.get("reencoded_frames") or 0 for p in f["parts"])
            differ = sum(a != b for a, b in zip(now, was))
            require(len(now) == len(was) and differ <= again,
                    f"{f['source']}: the crop changed {differ} packets of a file that isn't the edited "
                    f"one ({again} were encoded again at cuts)")
    # NLE projects (#104): every track of each format must show, frame by
    # frame, the take frames Isocast's export of that source shows, and the
    # track that is on at each frame what the edited file shows.
    evidence["nle"] = check_nle(e, cropped, segments, crop, crop_segments, fps, sources)
    changed = [p.name for p, digest in hashes.items() if hashlib.sha256(p.read_bytes()).hexdigest() != digest]
    require(not changed, f"edits or export changed take files: {changed}")
    evidence.update(timeline=e.rpc("timeline_get"), take_files_unchanged=len(hashes))


def seconds_of(text):
    """FCPXML's rational seconds ("1001/60000s", "5s")."""
    return Fraction(text.rstrip("s"))


def nle_tracks(fmt, path):
    """A project file's video tracks, bottom up: each a list of items
    (start, end in sequence frames, file, first file frame, enabled, crop),
    plus the sequence's frame rate and its audio items (the same). The crop
    is read back in each format's own units, as the spec defines them, to
    (x, y, width, height) fractions of the picture (None without one):
    xmeml's Crop filter in percent of each side; FCPXML's trim-rect in
    percent of the picture's height (Apple's DTD), left and right too;
    OTIO's metadata.isocast.crop."""
    def fractions(left, right, top, bottom):
        return (left, top, 1 - left - right, 1 - top - bottom)
    tracks, audio = [], []
    if fmt == "xmeml":
        seq = ET.parse(path).getroot().find("sequence")
        rate = int(seq.find("rate/timebase").text)
        files = {f.get("id"): urllib.parse.unquote(f.find("pathurl").text.removeprefix("file://localhost"))
                 for f in seq.iter("file") if f.find("pathurl") is not None}
        def crop_of(clip):
            for e in clip.iter("effect"):
                if e.findtext("effectid") == "crop":
                    v = {p.findtext("parameterid"): float(p.findtext("value")) / 100 for p in e.iter("parameter")}
                    return fractions(v["left"], v["right"], v["top"], v["bottom"])
            return None
        def items(track):
            for c in track.findall("clipitem"):
                yield (int(c.find("start").text), int(c.find("end").text), files[c.find("file").get("id")],
                       int(c.find("in").text), c.find("enabled").text == "TRUE", crop_of(c))
        tracks = [list(items(t)) for t in seq.findall("media/video/track")]
        audio = [i for t in seq.findall("media/audio/track") for i in items(t)]
    elif fmt == "fcpxml":
        root = ET.parse(path).getroot()
        fps = {f.get("id"): 1 / seconds_of(f.get("frameDuration")) for f in root.iter("format")}
        aspect = {f.get("id"): int(f.get("width")) / int(f.get("height")) for f in root.iter("format")}
        asset_aspect = {a.get("id"): aspect[a.get("format")] for a in root.iter("asset")}
        def crop_of(clip):
            r = clip.find("adjust-crop/trim-rect")
            if r is None:
                return None
            v = {k: float(r.get(k, "0")) / 100 for k in ("left", "right", "top", "bottom")}
            a = asset_aspect[clip.get("ref")]
            return fractions(v["left"] / a, v["right"] / a, v["top"], v["bottom"])
        seq = root.find("library/event/project/sequence")
        rate = fps[seq.get("format")]
        assets = {a.get("id"): (urllib.parse.unquote(a.find("media-rep").get("src").removeprefix("file://")),
                                fps[a.get("format")]) for a in root.iter("asset")}
        lanes = {}
        for gap in seq.find("spine").findall("gap"):
            base = seconds_of(gap.get("offset"))
            for c in gap.findall("asset-clip"):
                file, file_fps = assets[c.get("ref")]
                start = (base + seconds_of(c.get("offset"))) * rate
                end = start + seconds_of(c.get("duration")) * rate
                item = (int(start), int(end), file, int(seconds_of(c.get("start")) * file_fps),
                        c.get("enabled", "1") == "1", crop_of(c))
                require(start.denominator == 1 and end.denominator == 1, f"{path.name}: item off the frame grid")
                if c.get("srcEnable") == "audio":
                    audio.append(item)
                else:
                    lanes.setdefault(int(c.get("lane")), []).append(item)
        tracks = [lanes[k] for k in sorted(lanes)]
    else:
        stack = read_json(path)["tracks"]
        rate = None
        for track in stack["children"]:
            at, items = 0, []
            for c in track["children"]:
                r = c["source_range"]
                seq_rate = rate or r["duration"]["rate"]
                if c["OTIO_SCHEMA"].startswith("Gap"):
                    at += round(r["duration"]["value"])
                    continue
                rate = rate or seq_rate
                url = c["media_references"]["DEFAULT_MEDIA"]["target_url"]
                file = str((path.parent / urllib.parse.unquote(url)).resolve())
                length = round(r["duration"]["value"] * rate / r["duration"]["rate"])
                crop = c["metadata"].get("isocast", {}).get("crop")
                items.append((at, at + length, file, round(r["start_time"]["value"]), c["enabled"],
                              crop and (crop["x"], crop["y"], crop["width"], crop["height"])))
                at += length
            if track["kind"] == "Video":
                tracks.append(items)
            else:
                audio.extend(items)
        rate = round(rate)
    return tracks, int(rate), audio


def check_nle(e, export, segments, crop, crop_segments, fps, sources):
    """timeline_export_nle in each format, checked against Isocast's own
    export `export` frame by frame."""
    exported = {f["source"]: f for f in export["files"]}
    order = [s for s in exported if s not in ("program", "edited")] + ["program"]
    want = {}  # export file -> its pictures
    for f in export["files"]:
        want[f["source"]] = pattern_frames(Path(f["path"]), f["width"], f["height"])
    def same(a, b, width):
        apart = abs(a[1] - b[1])
        return min(apart, width - apart) <= 3 and (a[2] > 50) == (b[2] > 50)
    rows = []
    for fmt, app in (("fcpxml", "final_cut"), ("fcpxml", "resolve"), ("xmeml", "premiere"),
                     ("xmeml", "resolve"), ("otio", None)):
        params = {"format": fmt, "folder": str(e.root / "nle")}
        if app:
            params["app"] = app
        result = e.rpc("timeline_export_nle", params)
        path = Path(result["path"])
        require(path.is_file() and path.suffix == {"fcpxml": ".fcpxml", "xmeml": ".xml", "otio": ".otio"}[fmt],
                f"{fmt}: no project file: {result}")
        if fmt != "otio":
            ET.parse(path)  # well-formed
        tracks, rate, audio = nle_tracks(fmt, path)
        require(rate == fps, f"{path.name}: sequence at {rate} fps, the program at {fps}")
        require([t["source"] for t in result["video_tracks"]] == order and len(tracks) == len(order),
                f"{path.name}: video tracks {[t['source'] for t in result['video_tracks']]}, want {order}")
        width = exported["program"]["width"]
        bad, compared = [], 0
        for source, track in zip(order, tracks):
            require(len(track) == len(segments), f"{path.name}: {source} has {len(track)} clips, {len(segments)} segments")
            for (start, end, file, first, enabled, cropped), seg in zip(track, segments):
                require(enabled == ((seg[4] or "program") == source),
                        f"{path.name}: {source} at {start}: enabled {enabled}, the clip's angle is {seg[4] or 'program'}")
                index = segments.index(seg)
                want_crop = (crop["x"], crop["y"], crop["width"], crop["height"]) if index in crop_segments else None
                require((cropped is None) == (want_crop is None) and (cropped is None or all(
                        abs(a - b) < 1e-3 for a, b in zip(cropped, want_crop))),
                        f"{path.name}: {source} at {start}: crop read back as {cropped}, the clip's is {want_crop}")
                src = sources.get(Path(file))
                if src is None:
                    sources[Path(file)] = src = (pattern_frames(Path(file), width, exported[source]["height"]), None)
                frames = src[0]
                for j in range(start, end):
                    k = first + j - start
                    got = want[source][j] if j < len(want[source]) else None
                    compared += 1
                    if got is None or k >= len(frames) or not same(frames[k], got, width):
                        bad.append(f"{source} frame {j}: file frame {k}")
                    if enabled and index not in crop_segments:
                        edited = want["edited"][j]
                        compared += 1
                        if k >= len(frames) or not same(frames[k], edited, width):
                            bad.append(f"edited frame {j}: {source} file frame {k}")
        covered = [sum(e - s for s, e, *_ in t) for t in tracks]
        require(all(c == len(want["program"]) for c in covered),
                f"{path.name}: tracks cover {covered} frames, the export has {len(want['program'])}")
        require(not bad, f"{path.name}: {len(bad)} of {compared} frames differ from Isocast's export: {bad[:5]}")
        # Audio: each file's audio sits with its video (same start and first
        # frame), on only for the program.
        video_items = {(i[2], i[0]): i for t in tracks for i in t}
        program_files = {i[2] for i in tracks[order.index("program")]}
        require(audio, f"{path.name}: no audio")
        for start, end, file, first, enabled, _ in audio:
            v = video_items.get((file, start))
            require(v is not None and v[3] == first and v[1] == end,
                    f"{path.name}: audio of {Path(file).name} at {start} isn't with its video")
            require(enabled == (file in program_files), f"{path.name}: audio of {Path(file).name} enabled {enabled}")
        rows.append({"format": fmt, "app": result["app"], "file": path.name, "frames_compared": compared,
                     "video_tracks": len(tracks), "audio_items": len(audio), "notes": result.get("notes", [])})
    return rows


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
        self.sock = engine_connection(engine.sock)
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
    # The project's timeline (#114: the take before it has a timeline of
    # its own, in another project).
    timelines = [t for t in (e.root / "timelines").glob("*.json") if "clips" in read_json(t)
                 and any(Path(c["take"]).parent == finder for c in read_json(t)["clips"])]
    require(timelines, "no timeline names the renamed project's takes")
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
    if WINDOWS:
        raise Pending("streaming / the replay buffer: not on Windows yet")
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


# ---------------------------------------------------------------------------
# The frame oracle (#124).
#
# Fixtures: media clips the harness makes (never checked in) whose every
# frame carries its frame number and a source tag as a 16x9 grid of black and
# white blocks: a 32-bit word (16-bit frame, 4-bit tag, CRC-12) written in
# each corner (8x4 blocks), so it survives scaling, cropping into a corner,
# other items covering part of it, 4:2:0, HEVC/H.264/ProRes and masks. Their
# sound has a 40 ms burst starting exactly on the sample of every whole
# second, whose pitch names the second (mod 8).
#
# The oracle is a model of the documented behaviour, not of the engine's
# code: a source playing from the engine's start shows, at the take's frame
# slot n, media frame T0 + n (at speed s: the newest frame due by half a
# frame after the tick). T0, one number per recording, is fitted once; every
# output of the take, the split after it, its pauses, scene switches, edits,
# exports and project files are then predicted from it. A check decodes the
# output, compares every picture, and reports the first mismatch with the
# expected and actual ids and a contact sheet of the pictures around it.

ORACLE_GRID = (16, 9)
ORACLE_SIZE = (640, 360)
ORACLE_SECONDS = 120
ORACLE_QUADS = {"tl": (0, 0), "tr": (8, 0), "bl": (0, 5), "br": (8, 5)}
ORACLE_LEVELS = (24, 232)
ORACLE_TAGS = {"a": 1, "b": 2, "c": 3, "m": 4}
ORACLE_MARKERS = [600 + 200 * k for k in range(8)]
ORACLE_SCENARIOS = ("oracle_record", "oracle_scenes", "oracle_takes", "oracle_edit", "oracle_masks",
                    "oracle_canvases", "oracle_replay", "oracle_governor")
# The lanes' suite while they work (about 3 minutes on the VM, --hosted).
QUICK = ("oracle_record", "oracle_scenes", "oracle_takes", "oracle_edit", "oracle_masks")
# Measured on main (2026-10-18): a media source restarting on activation
# shows its first frame at the first picture its scene is on air.
RESTART_FIRST_FRAME = 0


def crc12(value, bits=20):
    crc = 0
    for i in reversed(range(bits)):
        top = (crc >> 11) & 1
        crc = (crc << 1) & 0xFFF
        if ((value >> i) & 1) ^ top:
            crc ^= 0x80F
    return crc


def oracle_word(tag, frame, quad):
    # The CRC also covers which corner the copy is in (not stored): a
    # picture moved by a slide or a wrong crop, read at a corner's place,
    # doesn't pass for another corner's copy.
    data = (frame & 0xFFFF) | (tag & 0xF) << 16
    return data | crc12(data | quad << 20, 22) << 20


def oracle_unword(word, quad):
    data = word & 0xFFFFF
    if word >> 20 != crc12(data | quad << 20, 22) or (data >> 16) not in ORACLE_TAGS.values():
        return None
    return data >> 16, data & 0xFFFF


def oracle_cells(tag, frame):
    """The fixture frame's 16x9 blocks, row by row, 0 or 1."""
    cols, rows = ORACLE_GRID
    cells = bytearray(cols * rows)
    for quad, (qx, qy) in enumerate(ORACLE_QUADS.values()):
        word = oracle_word(tag, frame, quad)
        for bit in range(32):
            cells[(qy + bit // 8) * cols + qx + bit % 8] = (word >> bit) & 1
    for x in range(cols):  # the middle row: a fixed alternation
        cells[4 * cols + x] = x % 2
    return cells


def oracle_fixture(folder, tag, fps, seconds=ORACLE_SECONDS):
    """A clip of `seconds` whose frame n carries (tag, n), with a marker
    burst at each whole second. Made once per run and folder."""
    folder.mkdir(parents=True, exist_ok=True)
    path = folder / f"oracle-v2-{tag}-{fps}-{seconds}.mov"
    if path.exists():
        return path
    cols, rows = ORACLE_GRID
    stem = folder / f".oracle-v2-{tag}-{fps}-{seconds}"
    raw, sound, partial = (Path(f"{stem}.gray"), Path(f"{stem}.wav"), Path(f"{stem}.partial.mov"))
    lo, hi = ORACLE_LEVELS
    with open(raw, "wb") as out:
        for n in range(seconds * fps):
            out.write(bytes(hi if c else lo for c in oracle_cells(tag, n)))
    with wave.open(str(sound), "wb") as out:
        out.setparams((1, 2, RATE, 0, "NONE", "not compressed"))
        samples = array.array("h", bytes(2 * seconds * RATE))
        for s in range(seconds):
            f = ORACLE_MARKERS[s % 8]
            for i in range(int(0.04 * RATE)):
                samples[s * RATE + i] = int(14000 * math.sin(2 * math.pi * f * i / RATE))
        if sys.byteorder != "little":
            samples.byteswap()
        out.writeframes(samples.tobytes())
    width, height = ORACLE_SIZE
    for codec in (["-c:v", "libx264", "-preset", "ultrafast", "-crf", "8", "-pix_fmt", "yuv420p"],
                  ["-c:v", "prores_ks", "-profile:v", "0", "-pix_fmt", "yuv422p10le"]):
        out = ffmpeg("-f", "rawvideo", "-pixel_format", "gray", "-video_size", f"{cols}x{rows}",
                     "-framerate", fps, "-i", raw, "-i", sound, "-vf", f"scale={width}:{height}:flags=neighbor",
                     *codec, "-g", fps, "-c:a", "pcm_s16le", "-y", partial, timeout=900, check=False)
        if out.returncode == 0:
            break
    else:
        raise Failure(f"could not make the oracle fixture: {out.stderr.decode(errors='replace')[-800:]}")
    raw.unlink()
    sound.unlink()
    partial.rename(path)
    return path


def oracle_base(root, cfg, sources, fps):
    """`cfg` with fixture media sources {id: (tag name, restart, speed)}
    in place of the synthetic ones; returns (cfg, the clips' frame count)."""
    if WINDOWS:
        raise Pending("media files (the block-code fixtures) don't play on Windows yet; the win_* scenarios "
                      "check the same model on the engine's test pattern")
    folder = root.parent / "fixtures"
    cfg["sources"] = []
    for sid, (tag, restart, speed) in sources.items():
        source = {"id": sid, "kind": "media", "files": [str(oracle_fixture(folder, ORACLE_TAGS[tag], fps))],
                  "loop": True, "restart_on_activate": restart}
        if speed != 1:
            source["speed"] = speed
        cfg["sources"].append(source)
    cfg["recording"]["program"]["audio"] = []
    cfg["recording"]["iso"].update(sources=[], audio=[])
    return cfg, ORACLE_SECONDS * fps


def item(source, rect, canvas, **extra):
    w, h = canvas
    value = {"fit": "stretch", "rect": {"x": round(rect[0] * w), "y": round(rect[1] * h),
                                        "width": round(rect[2] * w), "height": round(rect[3] * h)}}
    if source:
        value["source"] = source
    value.update(extra)
    return value


def fit_rect(inner, outer):
    """Where a picture of aspect `inner` (w, h) sits fitted and centred in
    `outer` (w, h), as fractions of `outer`."""
    scale = min(outer[0] / inner[0], outer[1] / inner[1])
    w, h = inner[0] * scale / outer[0], inner[1] * scale / outer[1]
    return ((1 - w) / 2, (1 - h) / 2, w, h)


def oracle_regions(layout, crop=(0, 0, 1, 1), place=(0, 0, 1, 1)):
    """The decode regions of a picture made of `layout` (items back to
    front: (source, rect as fractions of the canvas); the source's id is
    its fixture's tag name), with the canvas's `crop` (x, y, w, h) shown at
    `place` in the output. A corner's code counts only where it is wholly
    inside the crop and no later item covers it."""
    cx, cy, cw, ch = crop
    px, py, pw, ph = place
    regions = []
    for i, (name, rect) in enumerate(layout):
        x, y, w, h = rect
        quads = []
        for quad, (qx, qy) in ORACLE_QUADS.items():
            q = (x + qx / 16 * w, y + qy / 9 * h, x + (qx + 8) / 16 * w, y + (qy + 4) / 9 * h)
            inside = (cx - 1e-9 <= q[0] and q[2] <= cx + cw + 1e-9 and
                      cy - 1e-9 <= q[1] and q[3] <= cy + ch + 1e-9)
            covered = any(q[0] < r[0] + r[2] and r[0] < q[2] and q[1] < r[1] + r[3] and r[1] < q[3]
                          for _, r in layout[i + 1:])
            if inside and not covered:
                quads.append(quad)
        if quads:
            ox, oy = px + (x - cx) / cw * pw, py + (y - cy) / ch * ph
            regions.append({"name": name, "tag": ORACLE_TAGS[name], "quads": quads,
                            "rect": (ox, oy, w / cw * pw, h / ch * ph)})
    return regions


def video_times(path):
    # A raw elementary stream (a scene feed's .h264) has no timestamps: its
    # pictures count at 30 a second, the rate it was asked for.
    return [float(f.get("best_effort_timestamp_time", i / 30)) for i, f in enumerate(json.loads(command(
        ["ffprobe", "-v", "error", "-select_streams", "v:0", "-show_frames", "-show_entries",
         "frame=best_effort_timestamp_time", "-of", "json", path], timeout=300).stdout)["frames"])]


def gray_frames(path, side=480):
    """Every decoded picture, scaled so its longer side is `side`, in gray:
    (width, height, [(pts, bytes)])."""
    info = json.loads(command(["ffprobe", "-v", "error", "-show_streams", "-of", "json", path]).stdout)
    video = next(s for s in info["streams"] if s["codec_type"] == "video")
    scale = side / max(video["width"], video["height"])
    w, h = max(2, round(video["width"] * scale / 2) * 2), max(2, round(video["height"] * scale / 2) * 2)
    raw = ffmpeg("-i", path, "-map", "0:v:0", "-an", "-vf", f"scale={w}:{h}:flags=area,format=gray",
                 "-fps_mode", "passthrough", "-f", "rawvideo", "-", timeout=300).stdout
    times = video_times(path)
    require(len(raw) == len(times) * w * h, f"{Path(path).name}: decoded pictures and timestamps disagree")
    return w, h, [(t, raw[i * w * h:(i + 1) * w * h]) for i, t in enumerate(times)]


def oracle_read(pixels, w, h, region):
    """Decode one region of one picture: ("id", tag, frame), ("blank",
    level), ("garbled",) or ("conflict", [...])."""
    ox, oy, ow, oh = region["rect"]
    rx, ry = max(0.5, 0.22 * ow / 16 * w), max(0.5, 0.22 * oh / 9 * h)

    def level(u, v):
        x, y = (ox + u * ow) * w, (oy + v * oh) * h
        x0, x1 = max(0, int(x - rx)), min(w - 1, int(x + rx))
        y0, y1 = max(0, int(y - ry)), min(h - 1, int(y + ry))
        return sum(sum(pixels[yy * w + x0:yy * w + x1 + 1]) for yy in range(y0, y1 + 1)) / \
            ((x1 - x0 + 1) * (y1 - y0 + 1))

    found, flat, levels = [], 0, []
    for quad in region["quads"]:
        qx, qy = ORACLE_QUADS[quad]
        values = [level((qx + bit % 8 + 0.5) / 16, (qy + bit // 8 + 0.5) / 9) for bit in range(32)]
        levels.extend(values)
        lo, hi = min(values), max(values)
        if hi - lo < 64:
            flat += 1
            continue
        threshold, margin = (lo + hi) / 2, 0.18 * (hi - lo)
        if any(abs(v - threshold) < margin for v in values):
            continue  # mid-grey blocks: a blend, not a picture
        decoded = oracle_unword(sum(1 << bit for bit, v in enumerate(values) if v > threshold),
                                list(ORACLE_QUADS).index(quad))
        if decoded:
            found.append(decoded)
    # Two corners must agree where two or more should show (one where the
    # layout leaves only one): a chance CRC match is never an id.
    if len(set(found)) > 1:
        return ("conflict", sorted(set(found)))
    if len(found) >= min(2, len(region["quads"])):
        return ("id", *found[0])
    if flat == len(region["quads"]):
        return ("blank", round(sum(levels) / len(levels)))
    return ("garbled",)


def region_level(pixels, w, h, rect):
    """The mean grey level of `rect` (fractions), inset by a tenth."""
    x, y, rw, rh = rect
    x0, x1 = int((x + rw * 0.1) * w), int((x + rw * 0.9) * w)
    y0, y1 = int((y + rh * 0.1) * h), int((y + rh * 0.9) * h)
    return sum(sum(pixels[yy * w + x0:yy * w + x1]) for yy in range(y0, y1)) / max(1, (x1 - x0) * (y1 - y0))


def describe(value):
    if isinstance(value, tuple) and value and value[0] == "id":
        tag = next((k for k, v in list(ORACLE_TAGS.items()) + [("bars", 11), ("bars2", 12)] if v == value[1]),
                   value[1])
        return f"{tag}:{value[2]}"
    if isinstance(value, tuple):
        return value[0] if len(value) == 1 else f"{value[0]} {value[1]}"
    return str(value)


def contact_sheet(path, index, out):
    """Five pictures around picture `index` of `path`, side by side."""
    start = max(0, index - 2)
    ffmpeg("-i", path, "-vf", f"select='between(n\\,{start}\\,{start + 4})',scale=384:-2,tile=5x1",
           "-frames:v", "1", "-fps_mode", "passthrough", "-y", out, timeout=120, check=False)
    return out if out.exists() else None


class Oracle:
    """Expected-vs-actual checks of one output; `verdict` fails with the
    first mismatch and a contact sheet."""

    def __init__(self, label, path, root, evidence):
        self.label, self.path, self.root, self.evidence = label, Path(path), root, evidence
        self.checked, self.mismatches, self.pictures = 0, [], set()

    def expect(self, index, t, region, want, got, allowed=()):
        self.checked += 1
        self.pictures.add(index)
        if got == want or got in allowed:
            return
        self.mismatches.append((index, t, region, want, got))

    def verdict(self):
        row = {"output": self.path.name, "checks": self.checked, "mismatches": len(self.mismatches)}
        self.evidence.setdefault("oracle", []).append(row)
        require(self.checked > 0, f"{self.label}: the oracle checked nothing")
        if not self.mismatches:
            return row
        index, t, region, want, got = self.mismatches[0]
        (self.root / "sheets").mkdir(exist_ok=True)
        sheet = contact_sheet(self.path, index, self.root / "sheets" / f"{self.path.stem}-{index}.png")
        if sheet:  # what each of its five pictures should have been
            rows = {i: (t2, r2, w2, g2) for i, t2, r2, w2, g2 in self.mismatches}
            sheet.with_suffix(".txt").write_text("".join(
                f"picture {i}: " + (f"{rows[i][0]:.4f} s {rows[i][1]}: expected {describe(rows[i][2])}, "
                                    f"got {describe(rows[i][3])}\n" if i in rows else "as expected\n")
                for i in range(max(0, index - 2), max(0, index - 2) + 5)))
        row.update(first={"picture": index, "time": round(t, 4), "region": region,
                          "expected": describe(want), "actual": describe(got)}, sheet=sheet and str(sheet))
        later = "; ".join(f"{t2:.3f} s {r2}: expected {describe(w2)}, got {describe(g2)}"
                          for _, t2, r2, w2, g2 in self.mismatches[1:3])
        raise Failure(f"{self.label} ({self.path.name}): first mismatch at picture {index} ({t:.4f} s), "
                      f"{region}: expected {describe(want)}, got {describe(got)}; {len(self.mismatches)} of "
                      f"{self.checked} checks differ{' (then ' + later + ')' if later else ''}; "
                      f"contact sheet sheets/{sheet.name if sheet else None} in the scenario's folder")


def oracle_decode(path, regions, fps):
    """[(slot, pts, {region: decoded})] for every picture of `path`."""
    w, h, frames = gray_frames(path)
    rows = []
    for index, (t, pixels) in enumerate(frames):
        slot = round(t * fps)
        require(abs(t * fps - slot) < 0.02, f"{Path(path).name}: picture {index} at {t:.5f} s is off the frame grid")
        rows.append((slot, t, {r["name"]: oracle_read(pixels, w, h, r) for r in regions}))
    return rows


def oracle_compare(label, path, regions, expected, fps, root, evidence):
    """Every picture's decoded regions against `expected(slot, region
    name)` (None: no check); see oracle_check_rows."""
    return oracle_check_rows(label, path, oracle_decode(path, regions, fps), expected, root, evidence)


def oracle_check_rows(label, path, rows, expected, root, evidence):
    """`rows` [(slot, pts, {region: decoded})] against `expected`."""
    oracle = Oracle(label, path, root, evidence)
    for index, (slot, t, row) in enumerate(rows):
        for name, got in row.items():
            want = expected(slot, name)
            if want is not None:
                oracle.expect(index, t, name, want, got)
    return oracle.verdict()


def fit_anchor(decoded, region, count):
    """T0: the most common (frame - slot) of `region`'s decoded ids."""
    votes = {}
    for slot, _, row in decoded:
        got = row.get(region)
        if got and got[0] == "id" and got[1] == ORACLE_TAGS[region]:
            key = (got[2] - slot) % count
            votes[key] = votes.get(key, 0) + 1
    require(votes, f"no picture of {region} decoded: nothing to anchor the take on")
    return max(votes, key=votes.get)


def media_frame(t0, slot, speed, count):
    """The media frame a source playing from the engine's start at `speed`
    shows at the take's slot: the newest due by half a frame after it."""
    return math.floor(speed * (t0 + slot + 0.5) + 1e-9) % count if speed != 1 else (t0 + slot) % count


def burst_pitch(samples, at):
    """Which marker pitch the burst starting at `at` seconds has."""
    start = int(at * RATE) + 48
    window = samples[start:start + int(0.03 * RATE)]
    best, which = 0, None
    for k, f in enumerate(ORACLE_MARKERS):
        c = 2 * math.cos(2 * math.pi * f / RATE)
        s1 = s2 = 0.0
        for x in window:
            s1, s2 = x + c * s1 - s2, s1
        power = s1 * s1 + s2 * s2 - c * s1 * s2
        if power > best:
            best, which = power, k
    return which


def oracle_markers(label, path, frame_at, fps, evidence, cuts=(), samples=None):
    """The sound against the oracle: a burst (its pitch naming media second
    s mod 8) exactly where the picture shows media frame s * fps, at each
    slot `frame_at(slot)` says so, and nowhere else (except a burst cut in
    at an edit in `cuts`, a slot list). `samples`: the sound itself (mono,
    RATE) in place of `path`'s."""
    samples = pcm(path) if samples is None else samples
    end = len(samples) / RATE
    onsets = [o for o in beep_onsets(samples) if 0.15 < o < end - 0.15]
    every = [(j, frame_at(j)) for j in range(0, int(end * fps) + 1)]
    every = [(j, f // fps) for j, f in every if f is not None and f % fps == 0]
    want = [(j, second) for j, second in every if 0.2 < j / fps < end - 0.2]
    problems = []
    for j, second in want:
        near = [o for o in onsets if abs(o - j / fps) <= 1 / fps + 0.002]
        if not near:
            problems.append(f"no burst at {j / fps:.4f} s, where the picture shows media second {second}")
        elif burst_pitch(samples, near[0]) != second % 8:
            problems.append(f"the burst at {near[0]:.4f} s names second {burst_pitch(samples, near[0])} "
                            f"(mod 8), the picture shows {second % 8}")
    for o in onsets:
        if not any(abs(o - j / fps) <= 1 / fps + 0.002 for j, _ in every) and \
                not any(0 <= o - c / fps <= 0.045 for c in cuts):
            problems.append(f"a burst at {o:.4f} s where the picture shows media frame {frame_at(round(o * fps))}, "
                            f"not a whole second")
    evidence.setdefault("markers", []).append({"output": Path(path).name, "bursts": len(onsets),
                                               "expected": len(want)})
    if problems:
        raise Failure(f"{label} ({Path(path).name}) sound: {problems[0]}"
                      f"{f' (+{len(problems) - 1} more)' if len(problems) > 1 else ''}")


def manifest_file(path, manifest, source):
    f = next((f for f in manifest["files"] if f["source"] == source), None)
    require(f is not None, f"no {source} file in the take: {[x['source'] for x in manifest['files']]}")
    return path.parent / f["path"], f


RECORD_LAYOUT = [("a", (0, 0, 1, 1)), ("b", (0.5, 0.5, 0.5, 0.5)), ("c", (0.75, 0, 0.25, 0.25))]
RECORD_SOURCES = {"a": ("a", False, 1), "b": ("b", False, 1), "c": ("c", False, 1.5)}


def oracle_scene(cfg, layout):
    w, h = cfg["canvas"]["width"], cfg["canvas"]["height"]
    cfg["scenes"] = [{"id": "main", "name": "Main", "items": [item(s, r, (w, h)) for s, r in layout]}]


def oracle_canvases_cfg(cfg, record=True):
    """The vertical canvas (its own arrangement: b on top, a below) and a
    square canvas that letterboxes the main picture; returns the vertical
    layout."""
    w, h, fps = (cfg["canvas"][k] for k in ("width", "height", "fps"))
    vw, vh = h, w
    enc = {"encoder": dict(cfg["recording"]["program"]["encoder"], container="mov")}
    cfg["vertical"] = {"width": vw, "height": vh, "fps": fps, "follow_main": True}
    cfg["canvases"] = [{"id": "square", "width": h, "height": h, "fps": fps, "follow_main": True}]
    if record:
        cfg["vertical"]["recording"] = copy.deepcopy(enc)
        cfg["canvases"][0]["recording"] = copy.deepcopy(enc)
    band = vw * 9 / 16 / vh
    vertical = [("b", (0, 0, 1, band)), ("a", (0, band, 1, band))]
    for scene in cfg["scenes"]:
        scene["vertical"] = {"items": [item(s, r, (vw, vh)) for s, r in vertical]}
    return vertical


def check_record_take(e, path, manifest, cfg, layout, count, evidence, vertical=None, isos=("a", "b"),
                      t0=None, label="take"):
    """Every output of one take against the oracle; returns its T0."""
    w, h, fps = (cfg["canvas"][k] for k in ("width", "height", "fps"))
    speeds = {s["id"]: s.get("speed", 1) for s in cfg["sources"]}
    program, pf = manifest_file(path, manifest, "program")
    if t0 is None:
        t0 = fit_anchor(oracle_decode(program, oracle_regions(layout), fps), "a", count)
    pauses = [(round(p["time_seconds"] * fps), round((p["end_seconds"] - p["start_seconds"]) * fps))
              for p in manifest.get("pauses", []) if p.get("end_seconds") is not None]

    def take_slot(file_slot, offset):
        # A file's frame slot on the take's uncut clock: its start offset,
        # plus the frames every pause before it cut out.
        slot = file_slot + offset
        return slot + sum(n for at, n in pauses if slot >= at)

    def expected(offset):
        return lambda slot, name: ("id", ORACLE_TAGS[name], media_frame(t0, take_slot(slot, offset),
                                                                         speeds[name], count))
    outputs = [("program", oracle_regions(layout))]
    outputs += [(s, oracle_regions([(s, (0, 0, 1, 1))])) for s in isos]
    for f in manifest["files"]:
        if f["source"] == "vertical-program" and vertical:
            outputs.append((f["source"], oracle_regions(vertical)))
        elif f.get("kind") == "program" and f["source"] not in ("program", "vertical-program"):
            outputs.append((f["source"], oracle_regions(layout, place=fit_rect((w, h), (f["width"], f["height"])))))
    for source, regions in outputs:
        file, f = manifest_file(path, manifest, source)
        offset = round((f.get("start_offset_seconds") or 0) * fps)
        oracle_compare(f"{label} {source}", file, regions, expected(offset), fps, e.root, evidence)
        if source in ("program", "a") and (f.get("audio") or []):
            oracle_markers(f"{label} {source}", file,
                           lambda j, o=offset: media_frame(t0, take_slot(j, o), 1, count), fps, evidence)
    return t0


def oracle_seconds(evidence, quick, full):
    return quick if evidence["capabilities"].get("quick") else full


def scenario_oracle_record(e, cfg, evidence):
    """Program, two ISOs, the vertical canvas's own arrangement and a
    square canvas's letterboxed picture, a source at 1.5x speed, and the
    sound's markers: every picture of every file is the oracle's."""
    fps = cfg["canvas"]["fps"]
    cfg, count = oracle_base(e.root, cfg, RECORD_SOURCES, fps)
    oracle_scene(cfg, RECORD_LAYOUT)
    cfg["recording"]["program"]["audio"] = ["a"]
    cfg["recording"]["iso"].update(enabled=True, sources=["a", "b"], audio=["a"])
    vertical = oracle_canvases_cfg(cfg)
    e.apply(cfg)
    path, manifest = take(e, oracle_seconds(evidence, 3, 5))
    evidence["files"] = check_take(path, manifest, evidence=evidence)
    require({f["source"] for f in manifest["files"]} >= {"program", "a", "b", "vertical-program"}
            and len(manifest["files"]) == 5, f"files: {[f['source'] for f in manifest['files']]}")
    evidence["t0"] = check_record_take(e, path, manifest, cfg, RECORD_LAYOUT, count, evidence, vertical=vertical)


def cell_profiles(pixels, w, h):
    """Per row of fixture cells (the whole picture is one fixture): the
    prefix sums of its centre band's mean level across the picture."""
    out = []
    for r in range(ORACLE_GRID[1]):
        y0, y1 = int((r + 0.28) / 9 * h), int((r + 0.72) / 9 * h)
        cols = [0] * w
        for yy in range(y0, y1 + 1):
            cols = [c + v for c, v in zip(cols, pixels[yy * w:(yy + 1) * w])]
        prefix, total, n = [0.0], 0.0, y1 - y0 + 1
        for c in cols:
            total += c / n
            prefix.append(total)
        out.append(prefix)
    return out


def cells_at(profiles, w, dx):
    """The 144 cell levels of a fixture picture moved right by `dx` of the
    picture's width (None where a cell is off the picture)."""
    out = []
    for prefix in profiles:
        for c in range(ORACLE_GRID[0]):
            x = (c + 0.5) / 16 + dx
            x0, x1 = int((x - 0.22 / 16) * w), int((x + 0.22 / 16) * w) + 1
            out.append((prefix[x1] - prefix[x0]) / (x1 - x0) if 0 <= x0 and x1 <= w else None)
    return out


def fit_blend(levels, a_bits, b_bits):
    """Least squares level = k + u*a + v*b over the cells: (u, v, rms)."""
    rows = [(1.0, float(x), float(y), z) for x, y, z in zip(a_bits, b_bits, levels) if z is not None]
    m = [[sum(r[i] * r[j] for r in rows) for j in range(3)] for i in range(3)]
    rhs = [sum(r[i] * r[3] for r in rows) for i in range(3)]
    for i in range(3):  # Gauss-Jordan on the 3x3 normal equations
        pivot = max(range(i, 3), key=lambda k: abs(m[k][i]))
        m[i], m[pivot], rhs[i], rhs[pivot] = m[pivot], m[i], rhs[pivot], rhs[i]
        if abs(m[i][i]) < 1e-9:
            return None
        for k in range(3):
            if k != i:
                f = m[k][i] / m[i][i]
                m[k] = [x - f * y for x, y in zip(m[k], m[i])]
                rhs[k] -= f * rhs[i]
    k, u, v = (rhs[i] / m[i][i] for i in range(3))
    rms = math.sqrt(sum((r[3] - k - u * r[1] - v * r[2]) ** 2 for r in rows) / len(rows))
    return u, v, rms


def match_score(levels, bits, mid):
    """The share of visible cells whose level is on the side of `mid` its
    bit says, and how many were visible."""
    pairs = [(z > mid, bool(b)) for z, b in zip(levels, bits) if z is not None]
    return (sum(x == y for x, y in pairs) / len(pairs) if pairs else 0.0), len(pairs)


def scenario_oracle_scenes(e, cfg, evidence):
    """Cut, fade and slide between a scene of a (playing all along) and one
    of b (restarting each time it comes on air), against the documented
    transitions: a cut changes at once; a fade is the next scene over the
    current one, its share rising linearly over the duration; a slide (left)
    pushes the current scene off to the left while the next comes in from
    the right, linearly. Every picture, mid-transition included, must show
    the right frames of the right sources in the right proportion or place."""
    w, h, fps = (cfg["canvas"][k] for k in ("width", "height", "fps"))
    cfg, count = oracle_base(e.root, cfg, {"a": ("a", False, 1), "b": ("b", True, 1)}, fps)
    cfg["scenes"] = [{"id": s, "name": s, "items": [item(s, (0, 0, 1, 1), (w, h))]} for s in ("a", "b")]
    cfg["recording"]["program"]["audio"] = ["a"]
    e.apply(cfg)
    plan = (("b", {"kind": "cut"}), ("a", {"kind": "fade", "duration_ms": 500}),
            ("b", {"kind": "slide", "duration_ms": 500, "easing": "linear"}), ("a", {"kind": "cut"}))
    switches = []
    start = e.rpc("start", {})
    for scene, transition in plan:
        time.sleep(1)
        switches.append((e.rpc("switch_scene", {"scene": scene, "transition": transition}), transition))
    time.sleep(1)
    e.rpc("stop")
    path = Path(start["manifest"])
    manifest = until(lambda: finished(path), description="scenes take")
    evidence["files"] = check_take(path, manifest, evidence=evidence)
    require([s["scene"] for s, _ in switches] == [p[0] for p, _ in zip(plan, switches)],
            f"switch_scene answered other scenes: {[s['scene'] for s, _ in switches]}")
    program, _ = manifest_file(path, manifest, "program")
    full = [dict(r, name="picture") for r in oracle_regions([("a", (0, 0, 1, 1))])]
    decoded = oracle_decode(program, full, fps)
    first_switch = switches[0][0]["time_seconds"]
    t0 = fit_anchor([(s, t, {"a": r["picture"]}) for s, t, r in decoded if t < first_switch - 0.1], "a", count)
    # A switch at take time s takes effect at the first slot at or after s
    # (the requested plan's scenes, kinds and durations, the engine's times).
    spans = [(math.ceil(s["time_seconds"] * fps - 1e-6), scene, tr["kind"], round(tr.get("duration_ms", 0) * fps / 1000))
             for (s, _), (scene, tr) in zip(switches, plan)]

    def state(slot):
        scene, mixing, b_since = "a", None, None
        for first, to, kind, length in spans:
            if slot < first:
                break
            if to == "b":
                b_since = first
            mixing = (scene, first, kind, length) if kind != "cut" and slot < first + length else None
            scene = to
        return scene, mixing, b_since

    def frame(source, slot, b_since):
        if source == "a":
            return ("id", 1, (t0 + slot) % count)
        return ("id", 2, (slot - b_since + RESTART_FIRST_FRAME) % count)

    w2, h2, frames = gray_frames(program)
    steady = [oracle_read(p, w2, h2, full[0]) for _, p in frames[:3]]
    lo, hi = ORACLE_LEVELS
    mid = (lo + hi) / 2
    oracle = Oracle("scenes program", program, e.root, evidence)
    seen = {}
    for index, ((slot, t, row), (_, pixels)) in enumerate(zip(decoded, frames)):
        scene, mixing, b_since = state(slot)
        if not mixing:
            oracle.expect(index, t, f"scene {scene}", frame(scene, slot, b_since), row["picture"])
            continue
        old, first, kind, length = mixing
        k = slot - first
        new_bits = oracle_cells(ORACLE_TAGS[scene], frame(scene, slot, b_since)[2])
        old_bits = oracle_cells(ORACLE_TAGS[old], frame(old, slot, b_since)[2])
        profiles = cell_profiles(pixels, w2, h2)
        lows, highs = (k / length, (k + 1) / length)  # the share the model allows at this slot
        if kind == "fade":
            fit = fit_blend(cells_at(profiles, w2, 0), old_bits, new_bits)
            contrast = fit[0] + fit[1] if fit else 0
            share = fit[1] / contrast if fit and contrast > 0 else None
            ok = (share is not None and fit[2] < 14 and contrast > 0.75 * (hi - lo)
                  and lows - 0.08 <= share <= highs + 0.08)
            seen.setdefault("fade", []).append((k, share and round(share, 3), fit and round(fit[2], 1)))
            got = ("blend", None if share is None else f"{share:.2f} of {describe(frame(scene, slot, b_since))} "
                   f"over {describe(frame(old, slot, b_since))}, rms {fit[2]:.0f}, contrast {contrast:.0f}"
                   if fit else "no fit")
            want = ("blend", f"{lows:.2f}-{highs:.2f} of {describe(frame(scene, slot, b_since))} over "
                    f"{describe(frame(old, slot, b_since))}")
        else:  # slide left: the next scene enters from the right
            best = None
            for share in [lows + (highs - lows) * i / 8 for i in range(9)]:
                new_score, new_n = match_score(cells_at(profiles, w2, 1 - share), new_bits, mid)
                old_score, old_n = match_score(cells_at(profiles, w2, -share), old_bits, mid)
                score = min(new_score if new_n >= 16 else 1.0, old_score if old_n >= 16 else 1.0)
                if best is None or score > best[0]:
                    best = (score, share, new_n, old_n)
            ok = best[0] >= 0.95
            seen.setdefault("slide", []).append((k, round(best[1], 3), round(best[0], 3)))
            got = ("slide", f"best match {best[0]:.0%} of cells at {best[1]:.2f}")
            want = ("slide", f"{describe(frame(scene, slot, b_since))} from the right, {describe(frame(old, slot, b_since))} "
                    f"pushed left, {lows:.2f}-{highs:.2f} of the way")
        oracle.expect(index, t, f"{kind} {old}->{scene}, slot {k + 1} of {length}", want, want if ok else got)
    for first, scene, kind, length in spans:  # each transition was seen whole
        if kind != "cut":
            require(len([x for x in seen.get(kind, []) if x]) >= length,
                    f"scenes: the {kind} into {scene} has {len(seen.get(kind, []))} pictures, {length} expected")
    oracle.verdict()
    evidence.update(switches=[s for s, _ in switches], t0=t0, switch_slots=spans, transitions=seen,
                    steady=[describe(x) for x in steady])


def scenario_oracle_takes(e, cfg, evidence):
    """A split and a pause: the split's second take starts with the frame
    after the first take's last (none lost, none twice), and a pause cuts
    out exactly its own frames, in the program, an ISO and the sound."""
    fps = cfg["canvas"]["fps"]
    cfg, count = oracle_base(e.root, cfg, RECORD_SOURCES, fps)
    oracle_scene(cfg, RECORD_LAYOUT)
    cfg["recording"]["program"]["audio"] = ["a"]
    cfg["recording"]["iso"].update(enabled=True, sources=["b"], audio=["a"])
    e.apply(cfg)
    first = e.rpc("start", {})
    time.sleep(1.2)
    second = e.rpc("split")
    begins = datetime.datetime.fromisoformat(second["begins_at"]).timestamp()
    time.sleep(max(0, begins - time.time()) + 0.8)
    e.rpc("pause")
    time.sleep(0.7)
    e.rpc("resume")
    time.sleep(1.0)
    e.rpc("stop")
    takes = []
    for started in (first, second):
        path = Path(started["manifest"])
        manifest = until(lambda: finished(path), description="split take")
        evidence.setdefault("files", []).extend(check_take(path, manifest, evidence=evidence))
        takes.append((path, manifest))
    require(takes[1][1].get("pauses"), "the pause is missing from the second take's manifest")
    t0 = check_record_take(e, *takes[0], cfg, RECORD_LAYOUT, count, evidence, isos=("b",), label="take 1")
    program = next(f for f in takes[0][1]["files"] if f["source"] == "program")
    slots = program["frames"] + program["dropped_frames"] - (program.get("repeated_frames") or 0)
    # The second take begins where the first ended, on the same engine clock.
    check_record_take(e, *takes[1], cfg, RECORD_LAYOUT, count, evidence, isos=("b",), t0=t0 + slots,
                      label="take 2 (split)")
    evidence.update(t0=t0, first_take_slots=slots, pauses=takes[1][1]["pauses"])


class EditModel:
    """The oracle's own timeline: the takes appended whole, then each
    requested edit applied as the API documents it (trim a clip's in/out
    points; split at a time; delete a range, closing the gap; an angle or a
    crop over a range, splitting clips at its ends). Times are frame slots.
    The engine's timeline is compared with it, never used in its place."""

    def __init__(self, takes, frames):
        self.segs = [{"take": str(t), "in": 0, "out": frames, "angle": None, "crop": None} for t in takes]

    def starts(self):
        out, at = [], 0
        for seg in self.segs:
            out.append(at)
            at += seg["out"] - seg["in"]
        return out, at

    def split(self, at):
        starts, _ = self.starts()
        for i, (seg, start) in enumerate(zip(self.segs, starts)):
            if start < at < start + seg["out"] - seg["in"]:
                cut = seg["in"] + at - start
                self.segs[i:i + 1] = [dict(seg, out=cut), dict(seg, **{"in": cut})]
                return

    def trim(self, index, cut_in=None, cut_out=None):
        seg = self.segs[index]
        seg["in"] = seg["in"] if cut_in is None else cut_in
        seg["out"] = seg["out"] if cut_out is None else cut_out

    def over(self, lo, hi):
        self.split(lo)
        self.split(hi)
        starts, _ = self.starts()
        return [i for i, start in enumerate(starts) if lo <= start and start + self.segs[i]["out"] - self.segs[i]["in"] <= hi]

    def delete(self, lo, hi):
        for i in reversed(self.over(lo, hi)):
            del self.segs[i]

    def set(self, lo, hi, key, value):
        for i in self.over(lo, hi):
            self.segs[i][key] = value

    def compare(self, clips, fps):
        """Why the engine's clips aren't this timeline, if they aren't."""
        if len(clips) != len(self.segs):
            return f"{len(clips)} clips, the requested edits make {len(self.segs)}"
        for i, (c, seg) in enumerate(zip(clips, self.segs)):
            got = (str(Path(c["take"]).resolve()), round(c["in_seconds"] * fps, 3), round(c["out_seconds"] * fps, 3),
                   c.get("angle"), c.get("crop"))
            want = (str(Path(seg["take"]).resolve()), seg["in"], seg["out"], seg["angle"], seg["crop"])
            if got[0] != want[0] or abs(got[1] - want[1]) > 0.01 or abs(got[2] - want[2]) > 0.01 \
                    or got[3] != want[3] or (got[4] is None) != (want[4] is None) \
                    or (got[4] and any(abs(got[4][k] - want[4][k]) > 1e-4 for k in want[4])):
                return (f"clip {i} is {Path(got[0]).stem} slots {got[1]}-{got[2]} angle {got[3]} crop {got[4]}; "
                        f"the edits make {Path(want[0]).stem} slots {want[1]}-{want[2]} angle {want[3]} crop {want[4]}")
        return None


def scenario_oracle_edit(e, cfg, evidence):
    """Two takes edited (trim, split, ripple delete, an angle, crops),
    exported (an MP4 program where the codec allows, MOV ISOs, the edited
    file and its 9:16 copy) and as FCPXML (Final Cut, Resolve), xmeml
    (Premiere, Resolve) and OTIO. The expected timeline comes from the
    requested edits (EditModel), never from the engine; the engine's
    timeline must equal it; every exported picture and burst and every
    project file's tracks, in and out points, crops and sound are checked
    against it."""
    w, h, fps = (cfg["canvas"][k] for k in ("width", "height", "fps"))
    cfg, count = oracle_base(e.root, cfg, {"a": ("a", False, 1), "b": ("b", False, 1)}, fps)
    layout = [("a", (0, 0, 1, 1)), ("b", (0.5, 0.5, 0.5, 0.5))]
    oracle_scene(cfg, layout)
    cfg["recording"]["program"]["audio"] = ["a"]
    if cfg["recording"]["program"]["encoder"]["codec"] in ("h264", "hevc"):
        cfg["recording"]["program"]["encoder"]["container"] = "mp4"
    cfg["recording"]["iso"].update(enabled=True, sources=["a", "b"], audio=["a"])
    e.apply(cfg)
    e.rpc("timeline_new", {"name": "Oracle edit"})
    seconds = oracle_seconds(evidence, 3, 4)
    takes = [take(e, seconds) for _ in range(2)]
    evidence["files"] = [r for p, m in takes for r in check_take(p, m, seconds, evidence=evidence)]
    anchors = {str(Path(p).resolve()): fit_anchor(oracle_decode(manifest_file(p, m, "program")[0],
                                                                oracle_regions(layout), fps), "a", count)
               for p, m in takes}
    model = EditModel([Path(p).resolve() for p, _ in takes], seconds * fps)
    problem = model.compare(e.rpc("timeline_get")["clips"], fps)
    require(problem is None, f"edit: the two takes didn't append whole: {problem}")
    ids = [c["id"] for c in e.rpc("timeline_get")["clips"]]  # names only
    crop = {"x": 0.25, "y": 0.25, "width": 0.75, "height": 0.75}
    f = lambda x: round(x * fps)  # noqa: E731
    edits = [("timeline_trim", {"clip": ids[0], "in_seconds": 0.5, "out_seconds": seconds - 0.5},
              lambda: model.trim(0, f(0.5), f(seconds - 0.5))),
             ("timeline_trim", {"clip": ids[1], "in_seconds": 0.3}, lambda: model.trim(1, f(0.3))),
             ("timeline_split", {"at_seconds": 1.0}, lambda: model.split(f(1.0))),
             ("timeline_delete_range", {"start_seconds": 1.4, "end_seconds": 1.7},
              lambda: model.delete(f(1.4), f(1.7))),
             ("timeline_angle", {"start_seconds": seconds - 0.7, "end_seconds": seconds - 0.2, "source": "b"},
              lambda: model.set(f(seconds - 0.7), f(seconds - 0.2), "angle", "b"))]
    for method, params, apply in edits:
        e.rpc(method, params)
        apply()
        problem = model.compare(e.rpc("timeline_get")["clips"], fps)
        require(problem is None, f"edit: after {method} {params}: {problem}")
    # Crops on a program clip and on the angle's clip, chosen in the model.
    starts, _ = model.starts()
    for i in (1, len(model.segs) - 2):
        lo, hi = starts[i], starts[i] + model.segs[i]["out"] - model.segs[i]["in"]
        e.rpc("timeline_crop", {"start_seconds": lo / fps, "end_seconds": hi / fps, "crop": crop})
        model.set(lo, hi, "crop", dict(crop))
        problem = model.compare(e.rpc("timeline_get")["clips"], fps)
        require(problem is None, f"edit: after timeline_crop of slots {lo}-{hi}: {problem}")
    require(sum(1 for s in model.segs if s["crop"]) == 2 and any(s["angle"] == "b" for s in model.segs),
            "edit: the model lost its crops or angle")
    segments = model.segs
    starts, total = model.starts()
    evidence["segments"] = [dict(s, start=st) for s, st in zip(segments, starts)]
    export = e.rpc("timeline_export", {"folder": str(e.root / "export"), "vertical": True})
    result = until(lambda: (lambda v: v if v["status"] != "exporting" else None)(read_json(export["manifest"])),
                   180, "timeline export")
    require(result["status"] == "complete" and not result.get("errors"), f"export failed: {result}")
    kinds = sorted(f["source"] for f in result["files"])
    require(kinds == ["a", "b", "edited", "program", "vertical"],
            f"export: files for {kinds}, expected a, b, edited, program and vertical")

    def segment(slot):
        i = max(k for k, st in enumerate(starts) if st <= slot)
        return i, segments[i]

    def take_frame(i, slot):
        # An export slot -> its take's slot: the clip's in point plus the
        # slots since the clip started, on the take's own frame grid.
        return (anchors[segments[i]["take"]] + segments[i]["in"] + slot - starts[i]) % count

    for f_ in result["files"]:
        path, kind = Path(f_["path"]), f_["source"]
        frames_at = video_times(path)
        require(len(frames_at) == total and [round(t * fps) for t in frames_at] == list(range(total)),
                f"{path.name}: {len(frames_at)} pictures, the edits make {total} (one per slot)")
        regions_of = []
        for seg in segments:
            if kind in ("a", "b"):
                items, c = [(kind, (0, 0, 1, 1))], None
            elif kind == "program":
                items, c = layout, None
            else:  # edited, vertical: the clip's angle and crop
                items = [(seg["angle"], (0, 0, 1, 1))] if seg["angle"] else layout
                c = seg["crop"]
            rect = (c["x"], c["y"], c["width"], c["height"]) if c else (0, 0, 1, 1)
            place = fit_rect((rect[2] * w, rect[3] * h), (f_["width"], f_["height"])) if kind == "vertical" \
                else (0, 0, 1, 1)
            regions = oracle_regions(items, crop=rect, place=place)
            require(regions, f"{path.name}: the oracle has nothing to read in a clip of it")
            regions_of.append(regions)
        w2, h2, frames = gray_frames(path)
        oracle = Oracle(f"export {kind}", path, e.root, evidence)
        for index, (t, pixels) in enumerate(frames):
            slot = round(t * fps)
            i, seg = segment(slot)
            names = {}
            for region in regions_of[i]:
                name = (f"{region['name']} (clip {i}: take {Path(seg['take']).stem} "
                        f"slot {seg['in'] + slot - starts[i]})")
                names[name] = region
                oracle.expect(index, t, name, ("id", region["tag"], take_frame(i, slot)),
                              oracle_read(pixels, w2, h2, region))
        evidence.setdefault("exports", []).append(oracle.verdict())
        if kind in ("program", "a", "edited"):
            oracle_markers(f"export {kind}", path, lambda j: take_frame(segment(j)[0], j), fps, evidence,
                           cuts=starts[1:])
    evidence["nle"] = oracle_nle(e, segments, starts, total, anchors, fps, count, evidence)


def xmeml_sound_channels(path):
    """An xmeml project's sound items with the channel each plays (its
    sourcetrack's trackindex): {(start, end, file, first frame, enabled):
    [channels]}, one entry per item, duplicates kept."""
    seq = ET.parse(path).getroot().find("sequence")
    files = {f.get("id"): urllib.parse.unquote(f.find("pathurl").text.removeprefix("file://localhost"))
             for f in seq.iter("file") if f.find("pathurl") is not None}
    out = {}
    for track in seq.findall("media/audio/track"):
        for c in track.findall("clipitem"):
            key = (int(c.find("start").text), int(c.find("end").text), files[c.find("file").get("id")],
                   int(c.find("in").text), c.find("enabled").text == "TRUE")
            out.setdefault(key, []).append(int(c.findtext("sourcetrack/trackindex") or 0))
    return out


def oracle_nle(e, segments, starts, total, anchors, fps, count, evidence):
    """Each project format against the oracle's timeline: video tracks for
    a, b and the program, each tiling the whole sequence with one item per
    clip at the clip's slots; every sequence frame showing the media frame
    the oracle expects (from the item's file, first frame and start); the
    clip's crop on every track; only the clip's angle enabled; sound with
    its picture, on for the program only."""
    files = {}
    for take_path in {s["take"] for s in segments}:
        for f in read_json(take_path)["files"]:
            files[str((Path(take_path).parent / f["path"]).resolve())] = (
                str(Path(take_path).resolve()), round((f.get("start_offset_seconds") or 0) * fps), f["source"])
    rows = []
    for fmt, app in (("fcpxml", "final_cut"), ("fcpxml", "resolve"), ("xmeml", "premiere"),
                     ("xmeml", "resolve"), ("otio", None)):
        params = {"format": fmt, "folder": str(e.root / "nle")}
        if app:
            params["app"] = app
        path = Path(e.rpc("timeline_export_nle", params)["path"])
        name = f"{path.name} ({app or fmt})"
        tracks, rate, audio = nle_tracks(fmt, path)
        require(rate == fps, f"{name}: sequence at {rate} fps, the takes at {fps}")
        sources = []
        for track in tracks:
            known = {files.get(str(Path(i[2]).resolve()), (None, None, None))[2] for i in track}
            require(len(known) == 1 and None not in known, f"{name}: a track mixes or misses sources: {known}")
            sources.append(known.pop())
        require(sources == ["a", "b", "program"], f"{name}: video tracks {sources}, expected a, b, program")
        bad, compared = [], 0
        for source, track in zip(sources, tracks):
            items = sorted(track, key=lambda i: i[0])
            bounds = [(i[0], i[1]) for i in items]
            want_bounds = [(st, st + s["out"] - s["in"]) for st, s in zip(starts, segments)]
            require(bounds == want_bounds, f"{name}: the {source} track's items are at {bounds}, the clips at "
                                           f"{want_bounds} (all {total} sequence frames, no gaps or overlaps)")
            for (start, end, file, first, enabled, crop), seg in zip(items, segments):
                take_path, offset, _ = files[str(Path(file).resolve())]
                want_crop = seg["crop"] and (seg["crop"]["x"], seg["crop"]["y"], seg["crop"]["width"],
                                             seg["crop"]["height"])
                if (crop is None) != (want_crop is None) or (crop and any(abs(x - y) > 1e-3
                                                                          for x, y in zip(crop, want_crop))):
                    bad.append(f"{source} item at {start}: crop {crop}, the clip's is {want_crop}")
                if enabled != ((seg["angle"] or "program") == source):
                    bad.append(f"{source} item at {start}: enabled {enabled}, the clip's angle is "
                               f"{seg['angle'] or 'program'}")
                for j in range(start, end):
                    compared += 1
                    shows = (anchors[take_path] + offset + first + j - start) % count
                    want = (anchors[seg["take"]] + seg["in"] + j - start) % count
                    if take_path != seg["take"] or shows != want:
                        bad.append(f"{source} track, sequence frame {j}: {Path(file).name} frame {first + j - start} "
                                   f"shows media frame {shows}, the oracle expects {want} from "
                                   f"{Path(seg['take']).stem}")
        require(compared == 3 * total, f"{name}: compared {compared} frames, expected {3 * total}")
        # Sound (#124 re-review B3): exactly one enabled item per clip, the
        # program's sound with the program track's picture (same file, start,
        # end and first frame), tiling the sequence; any other item disabled
        # and with its own picture. Then the program sound as the project
        # plays it (the referenced stretches of the take files laid end to
        # end) must have the oracle's bursts where the oracle says.
        video = {(i[2], i[0]): i for t in tracks for i in t}
        program_items = sorted(tracks[2], key=lambda i: i[0])
        # Exactly one enabled item per channel per program item, counted as
        # written (never deduplicated first): one item per clip in FCPXML and
        # OTIO, whose clips carry all their channels; in xmeml, Premiere's
        # one stereo item (channel 1) per clip, Resolve's one item per
        # channel (1..channels of the program's sound) per clip.
        program_keys = [(i[0], i[1], i[2], i[3]) for i in program_items]
        channels = {take: next((a["channels"] for f in read_json(take)["files"] if f["source"] == "program"
                                for a in f.get("audio", [])), 2) for take in {s["take"] for s in segments}}
        if fmt == "xmeml":
            got = {k[:4]: sorted(v) for k, v in xmeml_sound_channels(path).items() if k[4]}
            want = {k: (list(range(1, channels[files[str(Path(k[2]).resolve())][0]] + 1)) if app == "resolve"
                        else [1]) for k in program_keys}
        else:
            got = {}
            for i in audio:
                if i[4]:
                    got.setdefault((i[0], i[1], i[2], i[3]), []).append(None)
            want = {k: [None] for k in program_keys}
        if got != want:
            show = lambda m: [(k[0], k[1], Path(k[2]).name, k[3], v) for k, v in sorted(m.items())]  # noqa: E731
            bad.append(f"the enabled sound items (span: channels) are {show(got)}; the oracle expects {show(want)}: "
                       f"exactly one per channel per program clip")
        enabled = sorted({(i[0], i[1], i[2], i[3]): i for i in audio if i[4]}.values(), key=lambda i: i[0])
        for start, end, file, first, on, _ in audio:
            v = video.get((file, start))
            if not on and (v is None or v[3] != first or v[1] != end or file in {i[2] for i in program_items}):
                bad.append(f"disabled sound of {Path(file).name} at {start} isn't an angle's with its picture")
        if not bad:
            sound, cache = array.array("f", bytes(4 * round(total / fps * RATE))), {}
            for start, end, file, first, _, _ in enabled:
                if file not in cache:
                    cache[file] = pcm(Path(file))
                a, n = round(first / fps * RATE), round((end - start) / fps * RATE)
                piece = cache[file][a:a + n]
                require(len(piece) == n, f"{name}: {Path(file).name} has no sound at frames {first}-{first + end - start}")
                sound[round(start / fps * RATE):round(start / fps * RATE) + n] = piece
            seg_at = lambda j: max(k for k, st in enumerate(starts) if st <= j)  # noqa: E731
            try:
                oracle_markers(name, path, lambda j: (anchors[segments[seg_at(j)]["take"]] + segments[seg_at(j)]["in"]
                                                      + j - starts[seg_at(j)]) % count,
                               fps, evidence, cuts=starts[1:], samples=sound)
            except Failure as exc:
                bad.append(str(exc))
        if bad:
            raise Failure(f"{name}: {len(bad)} problems against the oracle; first: {bad[0]}")
        rows.append({"format": fmt, "app": app, "frames_compared": compared, "tracks": sources,
                     "sound_items": len(audio), "program_sound_items": len(enabled)})
    return rows


def bar_center(pixels, w, h, rect):
    """Where the bright vertical bar is across `rect`: its centre as a
    fraction of the rect's width, or None."""
    x, y, rw, rh = rect
    row = int((y + rh / 2) * h) * w
    x0, x1 = int(x * w) + 1, int((x + rw) * w) - 1
    lit = [i for i, v in enumerate(pixels[row + x0:row + x1]) if v > 128]
    if not lit or lit[-1] - lit[0] + 1 != len(lit):
        return None
    return (x0 - x * w + (lit[0] + lit[-1] + 1) / 2) / (rw * w)


def lit_fraction(pixels, w, h, rect):
    x, y, rw, rh = rect
    row = int((y + rh / 2) * h) * w
    values = pixels[row + int(x * w) + 1:row + int((x + rw) * w) - 1]
    return sum(v > 128 for v in values) / max(1, len(values))


def scenario_oracle_masks(e, cfg, evidence):
    """The masks' fixed bugs (#115 review B1-B5), each named when it
    fails: a mask-only media source plays (B1), a missing mask hides what
    it masks (B2), a clock-started mask animation is the same in the
    program and a filtered ISO (B3), a scene mask is where the same scene is
    drawn, not half a frame ahead (B4), and a fit = none scene mask keeps
    its size in a reduced scene feed (B5)."""
    w, h, fps = (cfg["canvas"][k] for k in ("width", "height", "fps"))
    if WINDOWS:  # no media files on Windows yet: B1's mask-only clip can't play
        cfg["sources"] = [{"id": "tone", "kind": "tone"}]
        cfg["recording"]["iso"].update(sources=[], audio=[])
        count = 0
    else:
        cfg, count = oracle_base(e.root, cfg, {"m": ("m", True, 1)}, fps)
    fw, fh = ORACLE_SIZE
    cfg["sources"] += [{"id": i, "kind": "color", "color": "#ffffff", "width": fw, "height": fh}
                       for i in ("fill1", "fill2", "fill3", "fill4", "fill5")]
    cfg["sources"] += [{"id": "bar", "kind": "color", "color": "#ffffff", "width": 16, "height": 16},
                       {"id": "white", "kind": "color", "color": "#ffffff", "width": w, "height": h},
                       {"id": "gone", "kind": "media", "files": [str(e.root / "missing.mov")], "loop": True}]
    rects = {"b1": (0, 0, 1 / 3, 0.5), "b2": (1 / 3, 0, 1 / 3, 0.5), "b3": (2 / 3, 0, 1 / 3, 0.5),
             "b4mask": (0, 0.5, 0.5, 0.5), "b4scene": (0.5, 0.5, 0.5, 0.5)}
    travel, speed = 0.7 * w, 48 * fps  # half a frame of its motion: 24 canvas pixels
    cfg["keyframes"] = [{"id": "sweep", "frames": [{"at": 0, "translate": [0, 0]},
                                                    {"at": 1, "translate": [travel, 0]}]}]
    filters = {
        "fill1": [{"kind": "source_mask", "mask_source": "m"}],
        "fill2": [{"kind": "source_mask", "mask_source": "gone", "invert": True}],
        "fill3": [{"kind": "source_mask", "mask_scene": "barscene"}],
        "fill4": [{"kind": "shape_mask", "shape": "rectangle", "width": 0.2, "height": 1.0,
                   "animations": [{"duration_ms": 60000, "easing": "linear", "start": "clock",
                                   "frames": [{"at": 0, "width": 0.2}, {"at": 1, "width": 0.8}]}]}],
        "fill5": [{"kind": "source_mask", "mask_scene": "whitescene", "fit": "none", "scale": 1.0}]}
    if WINDOWS:
        filters["fill1"] = []
    for s in cfg["sources"]:
        if s["id"] in filters:
            s["filters"] = filters[s["id"]]
    cfg["scenes"] = [
        {"id": "main", "name": "Main", "items": [
            item("fill1", rects["b1"], (w, h)), item("fill2", rects["b2"], (w, h)),
            item("fill4", rects["b3"], (w, h)), item("fill3", rects["b4mask"], (w, h)),
            item(None, rects["b4scene"], (w, h), scene="barscene")]},
        {"id": "barscene", "name": "bar", "items": [item("bar", (0.1, 0, 0.05, 1), (w, h), animations=[
            {"keyframes": "sweep", "duration_ms": round(travel / speed * 1000), "iterations": "infinite",
             "easing": "linear", "start": "clock"}])]},
        {"id": "whitescene", "name": "white", "items": [item("white", (0, 0, 1, 1), (w, h))]},
        {"id": "b5scene", "name": "b5", "items": [item("fill5", (0.25, 0.25, 0.5, 0.5), (w, h))]}]
    sound = "tone" if WINDOWS else "m"
    cfg["recording"]["program"]["audio"] = [sound]
    cfg["recording"]["iso"].update(enabled=True, sources=["fill4"], audio=[sound], filtered=True)
    e.apply(cfg)
    problems, feed = {}, None
    cli = os.environ.get("ISOCAST_E2E_CLI")
    if cli and Path(cli).is_file() and not WINDOWS:  # scene feeds are previews: none on Windows yet
        feed = subprocess.Popen([cli, "frames", "scene:b5scene", "--width", "384", "--fps", "30",
                                 "--seconds", "4", "--output", str(e.root / "b5.h264"), "--json"],
                                env=e.env, stdout=open(e.root / "b5.json", "w"), stderr=open(e.root / "b5.err", "w"))
    path, manifest = take(e, oracle_seconds(evidence, 3, 4))
    evidence["files"] = check_take(path, manifest, evidence=evidence)
    program, _ = manifest_file(path, manifest, "program")
    w2, h2, frames = gray_frames(program, side=960)
    # B1: the mask-only clip plays a frame a slot, so its code shows through.
    b1 = {"name": "b1", "tag": 4, "rect": rects["b1"], "quads": list(ORACLE_QUADS)}
    decoded = [] if WINDOWS else [(round(t * fps), t, oracle_read(p, w2, h2, b1)) for t, p in frames]
    ids = [(s, g[2]) for s, _, g in decoded if g[0] == "id"]
    if WINDOWS:
        pass
    elif not ids:
        problems["B1"] = f"no mask frame shows through fill1 (first picture: {describe(decoded[0][2])})"
    else:
        votes = {}
        for slot, frame in ids:
            votes[(frame - slot) % count] = votes.get((frame - slot) % count, 0) + 1
        m0 = max(votes, key=votes.get)
        oracle = Oracle("mask-only media (B1)", program, e.root, evidence)
        for index, (slot, t, got) in enumerate(decoded):
            oracle.expect(index, t, "b1", ("id", 4, (m0 + slot) % count), got)
        try:
            oracle.verdict()
        except Failure as exc:
            problems["B1"] = str(exc)
    # B2: a mask with no picture hides what it masks, inverted or not.
    level = max(region_level(p, w2, h2, rects["b2"]) for _, p in frames)
    if level > 48:
        problems["B2"] = f"a missing, inverted mask shows its picture (grey level {level:.0f}, expected black)"
    # B4: the scene mask's bar is where the same scene draws it.
    gaps = []
    for _, p in frames:
        a, b = bar_center(p, w2, h2, rects["b4mask"]), bar_center(p, w2, h2, rects["b4scene"])
        # Not where the sweep starts again (one bar at each end).
        if a is not None and b is not None and 0.15 < b < 0.85 and abs(a - b) < 0.25:
            gaps.append((a - b) * w)
    if len(gaps) < len(frames) // 4:
        problems["B4"] = f"the bars were found in only {len(gaps)} of {len(frames)} pictures"
    elif max(abs(g) for g in gaps) > 8:
        problems["B4"] = (f"the scene mask is {statistics.median(gaps):+.1f} canvas pixels from the scene "
                          f"(worst {max(gaps, key=abs):+.1f}; half a frame of its motion is {speed / fps / 2:.0f})")
    # B3: the same clock-started mask in the program and the filtered ISO.
    iso, iso_f = manifest_file(path, manifest, "fill4")
    wi, hi, iso_frames = gray_frames(iso)
    offset = round((iso_f.get("start_offset_seconds") or 0) * fps)
    program_widths = {round(t * fps): lit_fraction(p, w2, h2, rects["b3"]) for t, p in frames}
    diffs = [lit_fraction(p, wi, hi, (0, 0, 1, 1)) - program_widths[round(t * fps) + offset]
             for t, p in iso_frames if round(t * fps) + offset in program_widths]
    if not diffs:
        problems["B3"] = "no ISO picture lines up with the program"
    elif max(abs(d) for d in diffs) > 0.02:
        problems["B3"] = (f"the filtered ISO's clock-started mask is {statistics.median(diffs):+.3f} of the "
                          f"picture wider than the program's at the same moment (program "
                          f"{statistics.median(program_widths.values()):.3f})")
    # B5: in a reduced scene feed the fit = none mask still shows all of it.
    if feed:
        try:
            feed.wait(timeout=30)
        except subprocess.TimeoutExpired:
            feed.kill()
            feed.wait()
        stream = e.root / "b5.h264"
        if not stream.exists() or stream.stat().st_size == 0:
            problems["B5"] = f"the scene feed wrote nothing: {(e.root / 'b5.err').read_text()[-300:]}"
        else:
            fw2, fh2, feed_frames = gray_frames(stream)
            visible = min(region_level(p, fw2, fh2, (0.25, 0.25, 0.5, 0.5)) for _, p in feed_frames[-10:])
            if visible < 200:
                problems["B5"] = (f"a fit = none scene mask in a {fw2}-wide scene feed shows only part of the "
                                  f"picture (grey level {visible:.0f}, expected white)")
    else:
        evidence["b5"] = "skipped: no isocast CLI (ISOCAST_E2E_CLI)"
    pending = {"B1": "media files", "B5": "previews (it reads a scene feed)"} if WINDOWS else {}
    evidence["mask_checks"] = {k: problems.get(k, f"PENDING: {pending[k]}" if k in pending else
                                                "PASS" if k != "B5" or feed else "SKIP")
                               for k in ("B1", "B2", "B3", "B4", "B5")}
    evidence["b4_gaps_px"] = gaps[:10]
    require(not problems, "masks: " + "; ".join(f"{k}: {v}" for k, v in sorted(problems.items())))
    if WINDOWS:
        raise Pending("B2, B3, B4 verified; B1 pending (media files) and B5 pending (previews): "
                      "Windows has neither yet")


def scenario_oracle_canvases(e, cfg, evidence):
    """Per-destination canvases (#84): each canvas's recording is the
    oracle's, and each live encoder is warmed once as a recording starts
    (a68b6f3f warmed every one twice)."""
    if WINDOWS:
        # Its sources are media files and its check needs stream encoders:
        # neither on Windows yet. win_record checks every canvas's recording.
        raise Pending("the encoder warm-once check needs streaming (and media files): not on Windows yet; "
                      "the canvases' recordings are checked by win_record")
    if not evidence["capabilities"]["h264"]:
        raise Skip("no hardware encoder on this runner (stream encoders)")
    fps = cfg["canvas"]["fps"]
    cfg, count = oracle_base(e.root, cfg, RECORD_SOURCES, fps)
    oracle_scene(cfg, RECORD_LAYOUT)
    cfg["recording"]["program"]["audio"] = ["a"]
    vertical = oracle_canvases_cfg(cfg)
    # Configured, never started: a recording warms their encoders anyway.
    cfg["stream"] = {"service": "custom", "server": "rtmp://127.0.0.1:9/live", "video_bitrate_kbps": 4000,
                     "audio_bitrate_kbps": 128, "keyframe_interval_seconds": 2}
    cfg["vertical"]["stream"] = dict(cfg["stream"], video_bitrate_kbps=3000)
    cfg["canvases"][0]["stream"] = dict(cfg["stream"], video_bitrate_kbps=2500)
    cfg["developer"] = {"enabled": True, "auto_off_hours": 0}
    e.apply(cfg)
    e.rpc("engine", {"running": True})
    time.sleep(1)
    watch = Watch(e, ["stopped"])
    path, manifest = take(e, oracle_seconds(evidence, 3, 4))
    until(lambda: watch.of("stopped"), 15, "stopped event")
    watch.close()
    evidence["files"] = check_take(path, manifest, evidence=evidence)
    check_record_take(e, path, manifest, cfg, RECORD_LAYOUT, count, evidence, vertical=vertical, isos=())
    # The daemon logs beside its socket (`<socket>.logs`).
    folder = e.sock.with_suffix(".logs")
    logs = list(folder.glob("*developer*"))
    require(logs, f"developer mode wrote no log: {sorted(p.name for p in folder.glob('*'))}")
    text = "".join(p.read_text(errors="replace") for p in logs)
    # One H.264 session per canvas (each canvas's stream), each warmed once:
    # on the main canvas the stream and replay share one warm-up; on each
    # other canvas its replay's warm-up joins its stream's session (by
    # design, #84 M5), and nothing else may join.
    sessions = re.findall(r"h264 stream session \d+ (\d+x\d+) at (\d+) kb/s", text)
    joins = re.findall(r"the recording's warm encoder shares the program encoder \((\d+x\d+) at (\d+) kb/s\)", text)
    warnings = (watch.of("stopped")[0].get("report") or {}).get("warnings", [])
    main = (f"{cfg['canvas']['width']}x{cfg['canvas']['height']}", str(cfg["stream"]["video_bitrate_kbps"]))
    extra = {spec for spec in sessions if spec != main}
    evidence.update(sessions=sessions, warm_joins=joins, warnings=warnings)
    require(len(sessions) == 3 and len(set(sessions)) == 3, f"canvases: {len(sessions)} H.264 sessions for "
                                                             f"three canvases' streams: {sessions}")
    twice = [f"{spec[0]} at {spec[1]} kb/s joined {joins.count(spec)} times"
             for spec in set(sessions) if joins.count(spec) > (0 if spec == main else 1)]
    require(not twice, f"canvases: encoders warmed twice as the recording started: {'; '.join(sorted(twice))} "
                       f"(expected the main canvas's 0, each other canvas's 1: its replay)")
    require(extra <= set(joins), f"canvases: a canvas's replay was not warmed with its stream: {joins}")
    require(len(warnings) == len(set(warnings)), f"canvases: a warning given twice: {warnings}")


def scenario_oracle_replay(e, cfg, evidence):
    """The replay buffer: a save is consecutive frames of the program, the
    same ones a recording beside it has, ending at the save."""
    if WINDOWS:
        raise Pending("the replay buffer: not on Windows yet")
    if not evidence["capabilities"]["h264"]:
        raise Skip("no hardware encoder on this runner (replay buffer)")
    fps = cfg["canvas"]["fps"]
    cfg, count = oracle_base(e.root, cfg, RECORD_SOURCES, fps)
    oracle_scene(cfg, RECORD_LAYOUT)
    cfg["recording"]["program"]["audio"] = ["a"]
    cfg["recording"]["replay"] = {"seconds": 5}
    e.apply(cfg)
    e.rpc("engine", {"running": True})
    require(e.rpc("replay_start")["state"] == "running", "replay buffer didn't start")
    started = e.rpc("start", {"seconds": 9})
    began = time.monotonic()
    time.sleep(7)
    asked = time.monotonic() - began
    saved = e.rpc("replay_save")
    manifest = until(lambda: finished(started["manifest"]), 45, "take")
    until(lambda: e.rpc("status")["state"] == "idle", description="idle recorder")
    e.rpc("replay_stop")
    path = Path(started["manifest"])
    evidence["files"] = check_take(path, manifest, evidence=evidence)
    t0 = check_record_take(e, path, manifest, cfg, RECORD_LAYOUT, count, evidence, isos=())
    replay = Path(saved["path"])
    r0 = fit_anchor(oracle_decode(replay, oracle_regions(RECORD_LAYOUT), fps), "a", count)
    speeds = {"a": 1, "b": 1, "c": 1.5}
    oracle_compare("replay", replay, oracle_regions(RECORD_LAYOUT),
                   lambda slot, name: ("id", ORACLE_TAGS[name], media_frame(r0, slot, speeds[name], count)),
                   fps, e.root, evidence)
    oracle_markers("replay", replay, lambda j: media_frame(r0, j, 1, count), fps, evidence)
    last = round(video_times(replay)[-1] * fps)
    ends = (r0 + last - t0) % count  # the replay's last frame, as a slot of the take
    evidence.update(replay_first_slot=(r0 - t0) % count, replay_last_slot=ends, save_asked_slot=round(asked * fps))
    require(abs(ends - asked * fps) <= fps // 2,
            f"replay: ends at the take's slot {ends}, the save was asked at about slot {asked * fps:.0f}")


def scenario_oracle_governor(e, cfg, evidence):
    """The governor's vertical restore (#110 review B1): pinning the
    vertical canvas back to full rate after the governor limited it must
    not count its earlier, intentional drops as new protected loss (an ISO
    hold with no new program drops). Needs real encoder pressure: a test
    Mac, never a VM or hosted runner."""
    caps = evidence["capabilities"]
    if caps.get("virtual_machine") or caps.get("hosted") or not caps["hevc"]:
        raise Skip("needs a test Mac's encoders under real pressure (not a VM or hosted runner)")
    cfg = setup(e.root, "hevc", iso=True, fps=30)
    cfg["governor"] = {"enabled": True, "allow_iso_thinning": True}
    cfg["vertical"] = {"width": 360, "height": 640, "fps": 30, "recording": {"encoder": {"codec": "hevc"}}}
    try:
        e.apply(cfg)
    except RpcError as exc:  # builds before 7c08ca3d thin ISOs without being asked
        require("allow_iso_thinning" in str(exc), f"governor setup refused: {exc}")
        cfg["governor"].pop("allow_iso_thinning")
        e.apply(cfg)
    foreign = Engine(e.root / "foreign", e.app / "Contents/MacOS/isocastd")
    samples = []
    try:
        load = setup(e.root / "foreign", "hevc", iso=True, width=3840, height=2160)
        load["governor"] = {"enabled": False}
        load["sources"] = [{"id": f"load{i}", "kind": "test_pattern", "width": 3840, "height": 2160, "fps": 60}
                           for i in range(4)]
        load["scenes"][0]["items"][0]["source"] = "load0"
        load["recording"]["program"].update(enabled=False, audio=[])
        load["recording"]["iso"].update(sources=[f"load{i}" for i in range(4)], audio=[])
        foreign.apply(load)
        started = e.rpc("start", {})

        def sample(seconds, phase):
            end = time.monotonic() + seconds
            while time.monotonic() < end:
                s = e.rpc("status")
                files = (s.get("live") or {}).get("files") or []
                protected = sum(f.get("dropped_frames") or 0 for f in files if f.get("source") == "program")
                samples.append({"phase": phase, "governor": s["governor"], "program_dropped": protected})
                time.sleep(0.1)
        sample(5, "normal")
        foreign.rpc("start", {})
        sample(30, "load")
        foreign.rpc("stop", timeout=60)
        sample(8, "settle")
        if not any(x["governor"]["vertical_percent"] < 100 for x in samples):
            e.rpc("stop", timeout=60)
            raise Skip("the governor never limited the vertical canvas here: nothing to restore")
        current = e.rpc("get")["setup"]
        current["governor"] = dict(current.get("governor") or {}, pins=["vertical.fps"])
        e.apply(current)
        sample(8, "pinned")
        e.rpc("stop", timeout=60)
    finally:
        foreign.close()
        evidence["governor_samples"] = samples[::5]
    manifest = until(lambda: finished(started["manifest"]), 60, "governor take")
    pinned = [x for x in samples if x["phase"] == "pinned"]
    holds = [x for x in pinned if x["governor"]["iso_percent"] == 0 and "protected" in x["governor"]["reason"]
             and x["program_dropped"] == pinned[0]["program_dropped"]]
    if holds:
        raise Failure(f"governor: restoring the vertical canvas held the ISOs for protected loss with no new "
                      f"program drops ({len(holds)} samples: {holds[0]['governor']['reason']})")
    # The governor and native admission shed ISO and vertical pictures here
    # by design: every file must still account for each lost one.
    path = Path(started["manifest"])
    for f in manifest["files"]:
        check_file(path.parent / f["path"], f["duration_seconds"], f["fps"], (f["width"], f["height"]),
                   dropped=f["dropped_frames"], repeated=f.get("repeated_frames") or 0)
    evidence["accounted"] = drop_metrics(manifest)


def pattern_oracle(path, fps):
    """The engine's test pattern names its frame by how far its bars have
    scrolled: frame n is shifted floor(n * width / (4 s * fps)) pixels. A
    file of such frames (Windows' Platform contract takes, #105) against
    that model, at its own size: the first picture that isn't the frame its
    timestamp says, or none."""
    info = json.loads(command(["ffprobe", "-v", "error", "-show_streams", "-of", "json", path]).stdout)
    video = next(s for s in info["streams"] if s["codec_type"] == "video")
    width, height = video["width"], video["height"]
    frames = pattern_frames(Path(path), width, height)
    bad = []
    for index, (t, shift, _, _) in enumerate(frames):
        n = round(t * fps)
        want = n * width // (4 * fps) % width
        apart = abs(shift - want)
        if min(apart, width - apart) > 1:
            bad.append((index, t, n, want, shift))
    if bad:
        index, t, n, want, shift = bad[0]
        later = round((shift - want) / (width / (4 * fps)))
        raise Failure(f"{Path(path).name}: first mismatch at picture {index} ({t:.4f} s): expected frame {n} "
                      f"(bars at {want}), got bars at {shift} (about frame {n + later}); "
                      f"{len(bad)} of {len(frames)} pictures differ")
    return {"file": str(path), "pictures": len(frames), "mismatches": 0}


def oracle_rules_selftest(work):
    """The oracle's own rules on made-up pictures: it is strict (any
    mismatch fails, even one picture one frame late), and the edit model
    catches an engine timeline that ignored or misplaced an edit."""
    def run(changes, pictures=300):
        try:
            oracle = Oracle("rules", work / "none.mov", work, {})
            for i in range(pictures):
                want = {r: ("id", ORACLE_TAGS[r], 1000 + i) for r in ("a", "b", "c")}
                got = dict(want)
                got.update(changes.get(i, {}))
                for r in want:
                    oracle.expect(i, i / 30, r, want[r], got[r])
            oracle.verdict()
            return "pass"
        except Failure:
            return "fail"
    cases = {
        "every picture as expected": (run({}), "pass"),
        "every source one frame behind at one picture": (
            run({100: {r: ("id", ORACLE_TAGS[r], 1099) for r in "abc"}}), "fail"),
        "one source one frame behind": (run({100: {"a": ("id", 1, 1099)}}), "fail"),
        "all sources two frames behind": (run({100: {r: ("id", ORACLE_TAGS[r], 1098) for r in "abc"}}), "fail"),
        "a short output, one picture late": (run({40: {r: ("id", ORACLE_TAGS[r], 1039) for r in "abc"}},
                                                  pictures=60), "fail"),
    }
    # A stale picture at a pause's resume (#124 re-review B1): 43 frames cut
    # out after slot 99; picture 100 shows a frame 20 behind for every source.
    want = lambda slot, r: ("id", ORACLE_TAGS[r], 1000 + slot + (43 if slot >= 100 else 0))  # noqa: E731
    rows = [(slot, slot / 30, {r: (("id", ORACLE_TAGS[r], 1123) if slot == 100 else want(slot, r))
                               for r in ("a", "b", "c")}) for slot in range(300)]
    try:
        oracle_check_rows("rules", work / "none.mov", rows, want, work, {})
        cases["a stale picture at a pause's resume"] = ("pass", "fail")
    except Failure:
        cases["a stale picture at a pause's resume"] = ("fail", "fail")
    model = EditModel(["/t/1", "/t/2"], 90)
    model.set(30, 60, "crop", {"x": 0.25, "y": 0.25, "width": 0.75, "height": 0.75})
    engine = [{"take": "/t/1", "in_seconds": 0, "out_seconds": 1, "angle": None},
              {"take": "/t/1", "in_seconds": 1, "out_seconds": 2, "angle": None},
              {"take": "/t/1", "in_seconds": 2, "out_seconds": 3, "angle": None},
              {"take": "/t/2", "in_seconds": 0, "out_seconds": 3, "angle": None}]
    cases["an ignored crop"] = ("fail" if model.compare(engine, 30) else "pass", "fail")
    engine[1]["crop"] = {"x": 0.25, "y": 0.25, "width": 0.75, "height": 0.75}
    cases["the crop as asked"] = ("fail" if model.compare(engine, 30) else "pass", "pass")
    engine[2]["in_seconds"] = 2 + 1 / 30
    cases["a cut one frame late"] = ("fail" if model.compare(engine, 30) else "pass", "fail")
    wrong = {k: v for k, v in cases.items() if v[0] != v[1]}
    require(not wrong, f"oracle rules: {wrong}")
    return {k: v[0] for k, v in cases.items()}


def oracle_selftest(work):
    """The fixture, decoder and oracle alone, on any machine with ffmpeg
    (Windows too): every frame of a clip survives scaling, a corner, a crop,
    a letterbox and a mask through every encoder ffmpeg has here, and a
    copy one frame late is caught with a clear message."""
    work = Path(tempfile.mkdtemp(prefix="oracle-selftest-", dir=work))
    fps, count = 30, 4 * 30
    clip = oracle_fixture(work, ORACLE_TAGS["a"], fps, seconds=4)
    encoders = [c for c in ("libx264", "libx265", "prores_ks", "h264_videotoolbox", "hevc_videotoolbox",
                            "h264_nvenc", "hevc_nvenc")
                if ffmpeg("-f", "lavfi", "-i", "color=size=320x180:rate=30", "-frames:v", "2", "-c:v", c,
                          "-f", "null", "-", check=False, timeout=60).returncode == 0]
    full = [("a", (0, 0, 1, 1))]
    cases = [("1080p", "scale=1920:1080", oracle_regions(full)),
             ("corner", "pad=1280:720:640:360", oracle_regions([("a", (0.5, 0.5, 0.5, 0.5))])),
             ("crop", "crop=480:270:160:90,scale=1280:720", oracle_regions(full, crop=(0.25, 0.25, 0.75, 0.75))),
             ("letterbox", "scale=1080:608,pad=1080:1920:0:656",
              oracle_regions(full, place=fit_rect((16, 9), (1080, 1920)))),
             # A mask hiding the left half: two of the four copies are gone.
             ("mask", "geq=lum='if(lt(X\\,W/2)\\,16\\,p(X\\,Y))':cb=128:cr=128", oracle_regions(full))]
    results = []
    for encoder in encoders:
        for name, graph, regions in cases:
            out = work / f"{encoder}-{name}.mov"
            pix = [] if "prores" in encoder else ["-pix_fmt", "yuv420p"]
            ffmpeg("-i", clip, "-vf", graph, "-c:v", encoder, *pix, "-b:v", "4M", "-an", "-y", out, timeout=300)
            evidence = {}
            oracle_compare(f"selftest {encoder} {name}", out, regions, lambda slot, n: ("id", 1, slot % count),
                           fps, work, evidence)
            results.append(dict(evidence["oracle"][0], encoder=encoder, case=name))
    late = work / "late.mov"
    ffmpeg("-i", clip, "-vf", "trim=start_frame=1,setpts=PTS-STARTPTS", "-an", "-c:v", "libx264",
           "-preset", "ultrafast", "-crf", "12", "-y", late, timeout=120)
    try:
        oracle_compare("selftest one frame late", late, oracle_regions(full),
                       lambda slot, n: ("id", 1, slot % count), fps, work, {})
        raise Failure("the oracle passed a copy one frame late")
    except Failure as exc:
        caught = str(exc)
        require("expected a:0, got a:1" in caught, f"the late copy failed for another reason: {caught}")
    rules = oracle_rules_selftest(work)
    shutil.rmtree(work)
    return {"encoders": encoders, "cases": results, "caught": caught, "rules": rules}


# ---------------------------------------------------------------------------
# The frame oracle on the engine's own test pattern (Windows, #124 follow-up).
#
# Windows can't play media files yet, so the block-code fixtures can't be
# sources there. The engine's test pattern names its frame instead: frame n's
# bars are shifted floor(n * width / (4 s * fps)) pixels (unique for 4 s) and
# its label's fifth character is lit for "BARS2", dark for "BARS". Like a
# media source playing from the engine's start, a test pattern draws frame n
# at the engine's tick n, so the same model holds: the take's slot j shows
# frame T0 + j (mod 4 s of frames); T0 is fitted once per take and predicts
# every other output of it, its split and its pause.

PATTERN_TAGS = {"bars": 11, "bars2": 12}
PATTERN_NAMES = {v: k for k, v in PATTERN_TAGS.items()}


def pattern_period(fps):
    return 4 * fps


def pattern_shift(n, width, fps):
    return n * width // pattern_period(fps) % width


def pattern_samples(path, specs, src_w, src_h):
    """Per decoded picture and per spec: (pts, {name: (bars row, label cell
    level, band level)}). A spec is (name, (x, y, w, h) where the source's
    picture sits in the output, in output pixels, the fraction of the
    source's height the row is read at, (x0, x1) the band's share of the
    source's width): the row is scaled back to the source's width."""
    s = max(src_h // 120, 1)
    graph, outs = [f"[0:v]format=gray,split={3 * len(specs)}" + "".join(f"[i{k}]" for k in range(3 * len(specs))) + ";"], []
    for k, (name, (x, y, w, h), row, band) in enumerate(specs):
        sx, sy = w / src_w, h / src_h
        ry = int(y + row * h)
        graph.append(f"[i{3 * k}]crop={max(1, int(w))}:1:{int(x)}:{ry},scale={src_w}:1:flags=bicubic[r{k}];")
        cx, cy, cw, ch = x + 30 * s * sx, y + 6 * s * sy, max(1, 5 * s * sx), max(1, 7 * s * sy)
        graph.append(f"[i{3 * k + 1}]crop={max(1, int(cw))}:{max(1, int(ch))}:{int(cx)}:{int(cy)},"
                     f"scale={src_w}:1:flags=area[c{k}];")
        bx0, bx1 = band
        graph.append(f"[i{3 * k + 2}]crop={max(1, int(w * (bx1 - bx0)))}:{max(1, int(h / 4))}:"
                     f"{int(x + w * bx0)}:{int(y + h * 0.75)},scale={src_w}:1:flags=area[b{k}];")
        outs += [f"[r{k}]", f"[c{k}]", f"[b{k}]"]
    graph.append("".join(outs) + f"vstack={len(outs)}[out]")
    raw = ffmpeg("-i", path, "-filter_complex", "".join(graph), "-map", "[out]", "-fps_mode", "passthrough",
                 "-pix_fmt", "gray", "-f", "rawvideo", "-", timeout=300).stdout
    times = video_times(path)
    size = src_w * 3 * len(specs)
    require(len(raw) == len(times) * size, f"{Path(path).name}: decoded pictures and timestamps disagree")
    rows = []
    for i, t in enumerate(times):
        picture = raw[i * size:(i + 1) * size]
        row = {}
        for k, (name, *_rest) in enumerate(specs):
            base = 3 * k * src_w
            bars = picture[base:base + src_w]
            cell = sum(picture[base + src_w:base + 2 * src_w]) / src_w
            band = sum(picture[base + 2 * src_w:base + 3 * src_w]) / src_w
            row[name] = (bars, cell, band)
        rows.append((t, row))
    return rows


def pattern_value(bars, cell, width, fps):
    """("id", tag, frame mod 4 s) for a sampled source, or ("garbled",)."""
    bits = bars.translate(BAR_LEVELS)
    rise, fall = bits.find(b"01"), bits.find(b"10")
    if 2 < rise < width - 4:
        shift = (width - rise - 1) % width
    elif 2 < fall < width - 4:
        shift = (width // 2 - fall - 1) % width
    else:
        return ("garbled",)
    period = pattern_period(fps)
    guess = round(shift * period / width)
    best = min(range(guess - 2, guess + 3), key=lambda n: min(abs(pattern_shift(n % period, width, fps) - shift),
                                                             width - abs(pattern_shift(n % period, width, fps) - shift)))
    apart = abs(pattern_shift(best % period, width, fps) - shift)
    if min(apart, width - apart) > max(2, width / period / 3):
        return ("garbled",)
    return ("id", PATTERN_TAGS["bars2" if cell > 50 else "bars"], best % period)


def pattern_rows(path, specs, src, fps):
    """[(slot, pts, {name: ("id", tag, frame) or ("garbled",)})] of `path`,
    the test pattern read at `specs` (see pattern_samples)."""
    rows = []
    for t, sampled in pattern_samples(path, specs, *src):
        slot = round(t * fps)
        require(abs(t * fps - slot) < 0.02, f"{Path(path).name}: a picture at {t:.5f} s is off the frame grid")
        rows.append((slot, t, {n: pattern_value(b, c, src[0], fps) for n, (b, c, _) in sampled.items()}))
    return rows


def pattern_fit(rows, name, period):
    votes = {}
    for slot, _, row in rows:
        got = row.get(name)
        if got and got[0] == "id":
            votes[(got[2] - slot) % period] = votes.get((got[2] - slot) % period, 0) + 1
    require(votes, f"no picture of {name} could be read: nothing to anchor the take on")
    return max(votes, key=votes.get)


def win_setup(e, cfg, pip=True):
    """bars on the whole canvas and bars2 in the lower-right quarter, the
    tone in the program and ISOs; returns the layout as source rects."""
    w, h = cfg["canvas"]["width"], cfg["canvas"]["height"]
    layout = [("bars", (0, 0, 1, 1))] + ([("bars2", (0.5, 0.5, 0.5, 0.5))] if pip else [])
    cfg["scenes"] = [{"id": "main", "name": "Main", "items": [item(s, r, (w, h)) for s, r in layout]}]
    cfg["recording"]["program"]["audio"] = ["tone"]
    cfg["recording"]["iso"].update(enabled=True, sources=["bars", "bars2"], audio=["tone"])
    return layout


def pattern_specs(layout, out_w, out_h, place=(0, 0, 1, 1)):
    """Read specs for a picture of `layout` (canvas fractions) shown at
    `place` in an out_w x out_h output: bars read 40% down
    (clear of the label and of a lower-right picture-in-picture), the band
    on the left half."""
    px, py, pw, ph = place
    specs = []
    for name, (x, y, w, h) in layout:
        rect = ((px + x * pw) * out_w, (py + y * ph) * out_h, w * pw * out_w, h * ph * out_h)
        covered = any(other != name for other, _ in layout[layout.index((name, (x, y, w, h))) + 1:])
        # Below the label (the top ~26%), above a lower-right picture-in-picture.
        specs.append((name, rect, 0.4 if covered else 0.5, (0, 0.5) if covered else (0, 1)))
    return specs


def check_pattern_take(e, path, manifest, cfg, layout, evidence, t0=None, label="take", vertical=None):
    """Every output of a test-pattern take against the oracle; returns T0."""
    w, h, fps = (cfg["canvas"][k] for k in ("width", "height", "fps"))
    period = pattern_period(fps)
    pauses = [(round(p["time_seconds"] * fps), round((p["end_seconds"] - p["start_seconds"]) * fps))
              for p in manifest.get("pauses", []) if p.get("end_seconds") is not None]

    def take_slot(slot, offset):
        slot += offset
        return slot + sum(n for at, n in pauses if slot >= at)

    def expected(offset):
        return lambda slot, name: ("id", PATTERN_TAGS[name], (t0 + take_slot(slot, offset)) % period)
    outputs = [("program", layout, (0, 0, 1, 1))]
    outputs += [(s, [(s, (0, 0, 1, 1))], (0, 0, 1, 1)) for s in ("bars", "bars2")
                if any(f["source"] == s for f in manifest["files"])]
    for f in manifest["files"]:
        if f["source"] == "vertical-program" and vertical:
            outputs.append((f["source"], vertical, (0, 0, 1, 1)))
        elif f.get("kind") == "program" and f["source"] not in ("program", "vertical-program"):
            outputs.append((f["source"], layout, fit_rect((w, h), (f["width"], f["height"]))))
    for source, items, place in outputs:
        file, f = manifest_file(path, manifest, source)
        specs = pattern_specs(items, f["width"], f["height"], place)
        offset = round((f.get("start_offset_seconds") or 0) * fps)
        rows = pattern_rows(file, specs, (w, h), fps)
        if t0 is None:
            t0 = (pattern_fit(rows, "bars", period) - offset) % period
        oracle_check_rows(f"{label} {source}", file, rows, expected(offset), e.root, evidence)
        if f.get("audio") and source in ("program", "bars", "bars2") and not pauses:
            # The tone beeps with every flash of the band (the bottom quarter
            # of the program and ISO pictures; a pause breaks the cadence).
            evidence.setdefault("sync", []).append(dict(sync_check(file, fps), output=file.name))
    return t0


def scenario_win_record(e, cfg, evidence):
    """The test pattern recorded into the program (bars, bars2 in a corner),
    both ISOs, the vertical canvas's own arrangement and a square canvas's
    letterboxed picture: every picture of every file is the frame the oracle
    expects, and every flash has its beep."""
    w, h = cfg["canvas"]["width"], cfg["canvas"]["height"]
    layout = win_setup(e, cfg)
    vw, vh = h, w
    band = vw * 9 / 16 / vh
    vertical = [("bars2", (0, 0, 1, band)), ("bars", (0, band, 1, band))]
    enc = {"encoder": dict(cfg["recording"]["program"]["encoder"], container="mov")}
    cfg["vertical"] = {"width": vw, "height": vh, "fps": cfg["canvas"]["fps"], "follow_main": True, "recording": enc}
    cfg["canvases"] = [{"id": "square", "width": h, "height": h, "fps": cfg["canvas"]["fps"], "follow_main": True,
                        "recording": copy.deepcopy(enc)}]
    cfg["scenes"][0]["vertical"] = {"items": [item(s, r, (vw, vh)) for s, r in vertical]}
    e.apply(cfg)
    path, manifest = take(e, 5)
    evidence["files"] = check_take(path, manifest, 5, evidence=evidence)
    require(len(manifest["files"]) == 5, f"files: {[f['source'] for f in manifest['files']]}, expected program, "
                                         f"bars, bars2, vertical-program and square")
    evidence["t0"] = check_pattern_take(e, path, manifest, cfg, layout, evidence, vertical=vertical)


def scenario_win_takes(e, cfg, evidence):
    """A split and a pause on the test pattern: the second take starts with
    the frame after the first's last; the pause cuts out exactly its frames."""
    layout = win_setup(e, cfg)
    e.apply(cfg)
    first = e.rpc("start", {})
    time.sleep(1.5)
    second = e.rpc("split")
    begins = datetime.datetime.fromisoformat(second["begins_at"]).timestamp()
    time.sleep(max(0, begins - time.time()) + 1.0)
    e.rpc("pause")
    time.sleep(0.8)
    e.rpc("resume")
    time.sleep(1.2)
    e.rpc("stop")
    takes = []
    for started in (first, second):
        path = Path(started["manifest"])
        manifest = until(lambda: finished(path), description="split take")
        evidence.setdefault("files", []).extend(check_take(path, manifest, evidence=evidence))
        takes.append((path, manifest))
    require(takes[1][1].get("pauses"), "the pause is missing from the second take's manifest")
    t0 = check_pattern_take(e, *takes[0], cfg, layout, evidence, label="take 1")
    program = next(f for f in takes[0][1]["files"] if f["source"] == "program")
    slots = program["frames"] + program["dropped_frames"] - (program.get("repeated_frames") or 0)
    check_pattern_take(e, *takes[1], cfg, layout, evidence, t0=(t0 + slots) % pattern_period(cfg["canvas"]["fps"]),
                       label="take 2 (split)")
    evidence.update(t0=t0, first_take_slots=slots)


def scenario_win_scenes(e, cfg, evidence):
    """Cut, fade and slide between a scene of bars and one of bars2 (test
    patterns: both run all along): each steady picture is the right source
    at the right frame; a fade's pictures show the incoming scene's share
    (its label cell's level) rising linearly; a slide (left) shows the
    incoming scene's label cell moving in from the right at the model's
    offset."""
    w, h, fps = (cfg["canvas"][k] for k in ("width", "height", "fps"))
    period = pattern_period(fps)
    cfg["scenes"] = [{"id": s, "name": s, "items": [item(s, (0, 0, 1, 1), (w, h))]} for s in ("bars", "bars2")]
    cfg["recording"]["program"]["audio"] = ["tone"]
    cfg["recording"]["iso"].update(enabled=False)
    e.apply(cfg)
    plan = (("bars2", {"kind": "cut"}), ("bars", {"kind": "fade", "duration_ms": 500}),
            ("bars2", {"kind": "slide", "duration_ms": 500, "easing": "linear"}), ("bars", {"kind": "cut"}))
    start = e.rpc("start", {})
    switches = []
    for scene, transition in plan:
        time.sleep(1)
        switches.append(e.rpc("switch_scene", {"scene": scene, "transition": transition}))
    time.sleep(1)
    e.rpc("stop")
    path = Path(start["manifest"])
    manifest = until(lambda: finished(path), description="scenes take")
    evidence["files"] = check_take(path, manifest, evidence=evidence)
    program, _ = manifest_file(path, manifest, "program")
    spans = [(math.ceil(s["time_seconds"] * fps - 1e-6), scene, tr["kind"], round(tr.get("duration_ms", 0) * fps / 1000))
             for s, (scene, tr) in zip(switches, plan)]

    def state(slot):
        scene, mixing = "bars", None
        for first, to, kind, length in spans:
            if slot < first:
                break
            mixing = (scene, first, kind, length) if kind != "cut" and slot < first + length else None
            scene = to
        return scene, mixing
    specs = [("picture", (0, 0, w, h), 0.5, (0, 1))]
    rows = [(round(t * fps), t, sampled["picture"]) for t, sampled in pattern_samples(program, specs, w, h)]
    decoded = [(slot, t, {"picture": pattern_value(b, c, w, fps)}) for slot, t, (b, c, _) in rows]
    t0 = pattern_fit([d for d in decoded if d[1] < switches[0]["time_seconds"] - 0.1], "picture", period)
    lit = statistics.median([c for (slot, t, (_, c, _)) in rows if state(slot) == ("bars2", None)] or [255])
    dark = statistics.median([c for (slot, t, (_, c, _)) in rows if state(slot) == ("bars", None)] or [0])
    oracle = Oracle("scenes program", program, e.root, evidence)
    # The bars' look (not their timing): a steady picture's row, at its frame.
    j0 = next(i for i, (slot, t, row) in enumerate(decoded) if state(slot)[1] is None and row["picture"][0] == "id")
    template, n0 = rows[j0][2][0].translate(BAR_LEVELS), decoded[j0][2]["picture"][2]

    def bars_bits(n, x):  # what column x of a picture of frame n shows
        return template[(x + pattern_shift(n, w, fps) - pattern_shift(n0, w, fps)) % w]
    seen = {}
    for index, ((slot, t, row), (_, _, (bars, cell, _))) in enumerate(zip(decoded, rows)):
        scene, mixing = state(slot)
        if not mixing:
            oracle.expect(index, t, f"scene {scene}", ("id", PATTERN_TAGS[scene], (t0 + slot) % period),
                          row["picture"])
            continue
        old, first, kind, length = mixing
        k = slot - first
        lows, highs = k / length, (k + 1) / length
        n = (t0 + slot) % period
        if kind == "fade":  # bars2's share: (cell - dark) / (lit - dark); into bars, 1 - that
            share2 = (cell - dark) / max(1, lit - dark)
            share = share2 if scene == "bars2" else 1 - share2
            ok = lows - 0.1 <= share <= highs + 0.1
            seen.setdefault("fade", []).append((k, round(share, 3)))
            got = f"share {share:.2f}"
        else:  # slide left: the old scene pushed p of the width left, the new in from the right
            bits = bars.translate(BAR_LEVELS)
            best = (0.0, None)
            for p in [lows + (highs - lows) * i / 8 for i in range(9)]:
                o = round((1 - p) * w)
                want_bits = bytes(bars_bits(n, x + round(p * w)) if x < o else bars_bits(n, x - o) for x in range(w))
                score = sum(a == b for a, b in zip(bits, want_bits)) / w
                best = max(best, (score, p))
            ok = best[0] >= 0.95
            seen.setdefault("slide", []).append((k, round(best[1], 3), round(best[0], 3)))
            got = f"best match {best[0]:.0%} of the row at {best[1]:.2f} of the way"
        want = f"{kind} into {scene}, {lows:.2f}-{highs:.2f} of the way"
        oracle.expect(index, t, f"{kind} {old}->{scene}, slot {k + 1} of {length}", want, want if ok else got)
    for first, scene, kind, length in spans:
        if kind != "cut":
            require(len(seen.get(kind, [])) >= length, f"scenes: the {kind} into {scene} has "
                                                       f"{len(seen.get(kind, []))} pictures, {length} expected")
    oracle.verdict()
    evidence.update(t0=t0, switch_slots=spans, transitions=seen, label_levels=(dark, lit))


def worker(args):
    root = args.worker_root
    evidence = {"capabilities": read_json(root.parent / "capabilities.json")}
    engine = None
    started = time.monotonic()
    status, reason = "PASS", "verified"
    try:
        if args.cli and args.cli.is_file():
            os.environ["ISOCAST_E2E_CLI"] = str(args.cli)
        for kv in args.engine_env:
            key, _, value = kv.partition("=")
            ENGINE_ENV[key] = value
        engine = Engine(root, args.daemon)
        codec = evidence["capabilities"]["record_codec"]
        cfg = setup(root, codec, **evidence["capabilities"].get("profile", {}))
        globals()["scenario_" + args.worker](engine, cfg, evidence)
    except Pending as exc:
        status, reason = "PENDING", str(exc)
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
    if WINDOWS:
        # The worker's own process tree only (by its PID), never anyone else's.
        if process.poll() is None:
            subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"], capture_output=True, timeout=60)
        process.wait(timeout=30)
        return

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
                                    "--worker-root", str(case), "--daemon", str(args.daemon.resolve()),
                                    *(["--cli", str(args.cli.resolve())] if args.cli else []),
                                    *(x for kv in args.engine_env for x in ("--engine-env", kv))],
                                   stdout=log, stderr=log, cwd=str(case),
                                   **({"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP}
                                                               if WINDOWS else {"start_new_session": True}))
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
    parser.add_argument("--quick", action="store_true",
                        help="the frame-oracle suite lanes run while they work (about 3 min on the VM "
                             "with --hosted): " + ", ".join(QUICK))
    parser.add_argument("--full", action="store_true",
                        help="every scenario, the oracle's included (the default; run before a merge)")
    parser.add_argument("--selftest", action="store_true",
                        help="check the oracle's fixture and decoder through every encoder ffmpeg has here, "
                             "on any OS (no engine); prints JSON")
    parser.add_argument("--engine-env", action="append", default=[], metavar="KEY=VALUE",
                        help="environment for the isolated engines (repeatable), e.g. a scratch build's fault switch")
    parser.add_argument("--pattern-oracle", type=Path, metavar="MOVIE", action="append",
                        help="check a file of the engine's test pattern frame by frame (any OS); with --fps")
    parser.add_argument("--fps", type=int, default=30, help="--pattern-oracle's frame rate")
    parser.add_argument("--cli", type=Path, default=None,
                        help="the isocast CLI, for scene feeds (default: beside --daemon, if there)")
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
    if args.pattern_oracle:
        results, failed = [], False
        for movie in args.pattern_oracle:
            try:
                results.append(pattern_oracle(movie, args.fps))
            except Failure as exc:
                results.append({"file": str(movie), "error": str(exc)})
                failed = True
        print(json.dumps(results, indent=2))
        return 1 if failed else 0
    if args.selftest:
        args.work.mkdir(parents=True, exist_ok=True)
        print(json.dumps(oracle_selftest(args.work), indent=2))
        return 0
    if args.cli is None and (args.daemon.parent / "isocast").is_file():
        args.cli = args.daemon.parent / "isocast"
    if args.quick:
        if args.only or args.full:
            parser.error("--quick runs its own scenarios; not with --only or --full")
        args.only = list(QUICK)
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    report = {"version": 1, "scenarios": [], "started_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
              # Which harness ran which scenarios (a merge check must run the merged one).
              "harness": {"e2e_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                          "scenarios": list(args.only or SCENARIOS)}}
    root = None
    try:
        require(sys.platform == "darwin" or WINDOWS, "macOS or Windows is required")
        for tool in ("ffmpeg", "ffprobe") + (() if WINDOWS else ("codesign",)):
            require(shutil.which(tool), f"missing required tool: {tool}")
        require(args.timeout > 0, "--timeout must be positive")
        scratch.mkdir(parents=True, exist_ok=True)
        with open(scratch / ".bench.lock", "a") as lock:
            if fcntl:  # on Windows the runner holds exon-desk's gpu.lock
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
            caps["quick"] = bool(args.quick)
            report["capabilities"] = caps
            if args.capabilities and not args.hosted:
                print(json.dumps(report["capabilities"], indent=2))
                return 0
            require(args.daemon.is_file(), f"daemon not found: {args.daemon}; build release first")
            args.work.mkdir(parents=True, exist_ok=True)
            root = Path(tempfile.mkdtemp(prefix="run-", dir=args.work)).resolve()
            if not WINDOWS and len(os.fsencode(root)) > SHORT_ROOT:
                # macOS caps Unix socket paths at 104 bytes, and the deepest
                # socket (a plugin's, ROOT/plugin_files/plugins/<id>/socket)
                # needs ~60 of them: a deep --work (hw-batch's, a lane with
                # a long name) runs under a short folder on the same disk
                # (strict numbers stay that disk's, #100), else in /tmp.
                root.rmdir()
                root = short_root(args.work.resolve())
                report["work_note"] = f"--work is too deep for Unix sockets; ran in {root}"
                print(f"e2e: {report['work_note']}", file=sys.stderr)
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
            elif root and root.parent != args.work.resolve():
                # Kept evidence of a run in a short folder goes back under
                # --work, so nothing is left in /tmp (#100).
                root = keep_under(root, args.work.resolve(), report)
        except Exception as exc:
            report["cleanup_error"] = str(exc)
        report["totals"] = {status: sum(r["status"] == status for r in report["scenarios"])
                            for status in ("PASS", "FAIL", "SKIP", "PENDING")}
        args.report.parent.mkdir(parents=True, exist_ok=True)
        write_json(args.report, report)
    if args.json:
        print(json.dumps(report, indent=2))
    return 1 if report.get("error") or report.get("cleanup_error") or report["totals"]["FAIL"] else 0


if __name__ == "__main__":
    sys.exit(main())
