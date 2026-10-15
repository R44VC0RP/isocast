#!/usr/bin/env python3
"""Long synthetic soak of a release isocastd; Python stdlib + ffmpeg/ffprobe.

Build: cargo build --release -p isocast-daemon
Run:   python3 scripts/soak.py --hours 5 --out soak-out
       python3 scripts/soak.py --summary soak-out/report.json   (Markdown)

One isolated engine (scripts/e2e.py's Engine: its own signed background app,
socket, setup, plugins, timelines, logs and recordings under --work; inherited
ISOCAST_* variables dropped; the stream key comes from the environment, never
the Keychain) records a 720p30 program plus two ISOs of synthetic sources for
--hours, with periodic splits, scene switches, pauses and markers, while it
streams to an ffmpeg RTMP receiver on 127.0.0.1 that decodes what it gets.

It fails on slow problems: the engine crashing or panicking, a command or
status timing out, the recording or stream ending by itself, stream
reconnects or receiver decode errors, memory footprint, thread or file
descriptor growth, stalled writes or a filling disk, take manifests that are
incomplete or miss markers, pauses or scene changes, and any file that fails
ffprobe, a full decode or its frame accounting. Dropped frames and audio
dropouts are reported as information only: hosted VMs encode on paravirtual
or software encoders and drop noisily.

Each finished take is verified while the run goes on and its media deleted
(--keep keeps it), so disk use stays at about two takes. --out gets the
report, samples, events, manifests and logs, never media. --deadline (Unix
time) stops recording early enough to verify and report before a CI job limit.

--profile hardware is the test-Mac soak: synthetic sources at 3840x2160@60
and 1920x1080@60 recorded as HEVC ISOs, a 1920x1080@60 HEVC program, an
H.264 6000 kb/s 1080p60 stream, takes split every 15 min, and two crash tests
at the end (kill -9 while recording and while paused; the relaunched engine
must report the take interrupted and its files must play to within 2.5 s of
the kill). Dropped frames or audio dropouts in the program or the stream fail
it. ISOs are what the engine drops first under overload, so their drops are
reported and fail only above --max-iso-drop-ratio (default 0.1% of an ISO's
frame slots over the run: a sustained overload drops far more, a stray
system hiccup far less). Takes are decoded with VideoToolbox at up to 4x
real time, so verification keeps up with 4K60 without starving the encoders.

--receiver HOST streams to an ffmpeg receiver on another machine: HOST is an
ssh destination (with --receiver-ssh-config FILE if it needs one). Each
listener runs there over ssh and the stream goes through an `ssh -L`
tunnel, so only ssh has to reach HOST. Afterwards nothing may still listen
there.
"""

import argparse
import concurrent.futures
import ctypes
import datetime
import fcntl
import json
import os
import queue
import random
import re
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
import e2e  # noqa: E402  (Engine, setup, capabilities, probe: one isolation model)

SOURCES = ["program", "bars", "bars2"]
# canvas (w, h, fps); each ISO source's size; the program codec (None: the
# best hardware one); the stream; minutes between splits; strict: drops in
# the program and stream fail, plus the crash tests.
PROFILES = {
    "hosted": {"canvas": (1280, 720, 30), "sources": {"bars": (1280, 720), "bars2": (1280, 720)},
               "codec": None, "stream": {"video_bitrate_kbps": 2500, "audio_bitrate_kbps": 128},
               "split_min": 30, "strict": False},
    "hardware": {"canvas": (1920, 1080, 60), "sources": {"bars": (3840, 2160), "bars2": (1920, 1080)},
                 "codec": "hevc", "stream": {"video_bitrate_kbps": 6000, "audio_bitrate_kbps": 160},
                 "split_min": 15, "strict": True},
}
PROFILE = PROFILES["hosted"]
WIDTH, HEIGHT, FPS = PROFILE["canvas"]
TRANSITIONS = [{"kind": "cut"}, {"kind": "fade", "duration_ms": 500},
               {"kind": "slide", "duration_ms": 400, "easing": "linear"}]
# Minutes between events at --scale 1 (scenes: a random gap in the range).
SPLIT_MIN, PAUSE_MIN, MARKER_MIN, SCENE_MIN = 30, 40, 10, (1, 4)
SSH_OPTIONS = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=15", "-o", "ServerAliveInterval=30",
               "-o", "ServerAliveCountMax=4", "-o", "LogLevel=ERROR"]
REMOTE_PATH = "PATH=/opt/homebrew/bin:/usr/local/bin:$PATH"


def use_profile(name):
    global PROFILE, WIDTH, HEIGHT, FPS, SPLIT_MIN
    PROFILE = PROFILES[name]
    WIDTH, HEIGHT, FPS = PROFILE["canvas"]
    SPLIT_MIN = PROFILE["split_min"]
FINISH_RESERVE = 20 * 60  # seconds kept after recording for the last take, shutdown and report

STOP = threading.Event()   # end the session early (fatal failure or signal)
LOCK = threading.Lock()
FAILS = []
OUT = None


def now():
    return time.monotonic()


def append(name, record):
    with LOCK, open(OUT / name, "a") as f:
        f.write(json.dumps(record) + "\n")


def log(message):
    line = time.strftime("%H:%M:%S ") + message
    print(line, flush=True)
    with LOCK, open(OUT / "soak.log", "a") as f:
        f.write(line + "\n")


def fail(kind, message, fatal=False):
    with LOCK:
        FAILS.append({"kind": kind, "message": str(message)[:2000], "at": time.strftime("%H:%M:%S")})
    log(f"FAIL {kind}: {str(message)[:500]}")
    if fatal:
        STOP.set()


# ------------------------------------------------------------- process metrics

_libproc = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)


def process_metrics(pid):
    """Footprint (what Activity Monitor shows), RSS, threads and open fds."""
    usage = (ctypes.c_uint64 * 40)()
    if _libproc.proc_pid_rusage(pid, 2, ctypes.byref(usage)):  # RUSAGE_INFO_V2
        return None
    # rusage_info_v2: 16-byte uuid, then user, system, idle wakeups,
    # interrupt wakeups, pageins, wired, resident, phys_footprint, ...
    resident, footprint = usage[2 + 6], usage[2 + 7]
    task = ctypes.create_string_buffer(96)  # struct proc_taskinfo
    threads = None
    if _libproc.proc_pidinfo(pid, 4, 0, task, 96) == 96:  # PROC_PIDTASKINFO
        threads = int.from_bytes(task.raw[84:88], "little")  # pti_threadnum
    size = _libproc.proc_pidinfo(pid, 1, 0, None, 0)  # PROC_PIDLISTFDS
    fds = None
    if size > 0:
        buf = ctypes.create_string_buffer(size + 8 * 64)
        used = _libproc.proc_pidinfo(pid, 1, 0, buf, len(buf))
        fds = used // 8 if used > 0 else None  # struct proc_fdinfo is 8 bytes
    return {"footprint_mb": round(footprint / 2**20, 1), "rss_mb": round(resident / 2**20, 1),
            "threads": threads, "fds": fds}


def trend(samples, key, warmup):
    """Growth after warm-up: median of the last third minus the first third, and slope."""
    points = [(s["t"], s[key]) for s in samples
              if s.get(key) is not None and s["t"] >= warmup and s.get("state") == "recording"]
    if len(points) < 6:
        return None
    third = len(points) // 3
    first = statistics.median(v for _, v in points[:third])
    last = statistics.median(v for _, v in points[-third:])
    mt = statistics.mean(t for t, _ in points)
    mv = statistics.mean(v for _, v in points)
    var = sum((t - mt) ** 2 for t, _ in points)
    slope = sum((t - mt) * (v - mv) for t, v in points) / var * 3600 if var else 0.0
    values = [v for _, v in points]
    return {"start": first, "end": last, "growth": round(last - first, 1), "per_hour": round(slope, 2),
            "min": min(values), "max": max(values), "samples": len(points)}


def decode(file, timeout, paced=False):
    """A full decode of every stream. The hardware profile decodes video with
    VideoToolbox (4K60 HEVC is too slow on one CPU thread to keep up), at
    most 4x real time while recording so it can't starve the encoders."""
    hw = ["-hwaccel", "videotoolbox"] if PROFILE["strict"] else []
    pace = ["-readrate", "4"] if PROFILE["strict"] and paced else []
    return e2e.command(["nice", "-n", "10", "ffmpeg", "-nostdin", "-hide_banner", "-v", "error",
                        "-progress", "pipe:1", "-nostats", "-threads", "1", *hw, *pace, "-i", file,
                        "-map", "0", "-f", "null", "-"], timeout=timeout, check=False)


# ------------------------------------------------------------------- the engine

class Soak:
    def __init__(self, args, engine, caps, root):
        self.args, self.e, self.caps, self.root = args, engine, caps, root
        self.status = {}
        self.status_failures = 0
        self.recording = False
        self.paused = False
        self.stream_expected = False
        self.takes = []          # manifest paths in order
        self.checked = {}        # manifest path -> result
        self.verify_queue = queue.Queue()
        self.events = {"switches": [], "markers": [], "pauses": [], "splits": 0}
        self.samples = []
        self.receiver = {"connections": [], "errors": []}
        self.receiver_stop = threading.Event()
        self.listener = None
        self.port = None         # where the engine streams to, on 127.0.0.1
        self.remote_port = None  # the receiver's port on --receiver's host
        self.tunnel = None
        self.lost = None         # why the engine can no longer be trusted (crash or hang)

    # ------------------------------------------------------- remote receiver
    def ssh(self):
        cfg = ["-F", str(self.args.receiver_ssh_config)] if self.args.receiver_ssh_config else []
        return ["ssh", *cfg, *SSH_OPTIONS]

    def remote(self, script, timeout=60):
        return subprocess.run([*self.ssh(), self.args.receiver, script], capture_output=True,
                              timeout=timeout, check=False)

    def remote_listening(self):
        out = self.remote(f"lsof -nP -iTCP:{self.remote_port} -sTCP:LISTEN -t || true")
        if out.returncode:
            raise e2e.Failure(f"ssh {self.args.receiver}: {out.stderr.decode(errors='replace')[-300:]}")
        return out.stdout.decode().split()

    def open_tunnel(self):
        """Pick a free port on the receiver host and forward a local port to it."""
        check = self.remote(f"{REMOTE_PATH}; command -v ffmpeg && hostname")
        e2e.require(check.returncode == 0, f"ssh {self.args.receiver} or its ffmpeg failed: "
                    f"{(check.stdout + check.stderr).decode(errors='replace')[-300:]}")
        self.receiver["host"] = check.stdout.decode().split()[-1]
        rng = random.Random()
        for _ in range(5):
            self.remote_port = rng.randrange(20000, 60000)
            if not self.remote_listening():
                break
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            self.port = reservation.getsockname()[1]
        self.tunnel = subprocess.Popen(
            [*self.ssh(), "-N", "-o", "ExitOnForwardFailure=yes",
             "-L", f"127.0.0.1:{self.port}:127.0.0.1:{self.remote_port}", self.args.receiver],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=open(OUT / "tunnel.log", "wb"))

        def up():
            e2e.require(self.tunnel.poll() is None, "the ssh tunnel exited; see tunnel.log")
            try:
                socket.create_connection(("127.0.0.1", self.port), timeout=2).close()
                return True
            except OSError:
                return False
        e2e.until(up, 30, "the ssh tunnel to the receiver")

    def close_remote(self):
        """Stop the tunnel and anything of ours still on the receiver host; fail if it still listens."""
        if self.tunnel and self.tunnel.poll() is None:
            self.tunnel.terminate()
            try:
                self.tunnel.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.tunnel.kill()
        if not self.remote_port:
            return
        url = f"rtmp://127.0.0.1:{self.remote_port}/"
        try:
            # Killing the local ssh leaves a listener that is still waiting
            # for a connection (the loop opens one after each stream ends)
            # running there: stopping it here is the normal path.
            self.remote(f"pkill -f '{url}'; sleep 2; pkill -9 -f '{url}'; true")
            left = self.remote_listening()
            if left:
                fail("stream", f"port {self.remote_port} on {self.args.receiver} is still listening "
                               f"after the run (pids {left})")
            self.receiver["cleanup"] = "nothing left listening"
        except (e2e.Failure, subprocess.TimeoutExpired) as exc:
            fail("stream", f"could not check the receiver host is clean: {exc}")
        self.remote_port = None

    def lose(self, kind, message):
        if not self.lost:
            self.lost = message
            fail(kind, message, fatal=True)

    def call(self, method, params=None, timeout=60, fatal=True):
        """An RPC; a timeout is a hang, a refused/dead socket a crash."""
        try:
            result = self.e.rpc(method, params, timeout=timeout)
            return {} if result is None else result  # None means it failed
        except (socket.timeout, TimeoutError) as exc:
            self.lose("hang", f"`{method}` did not answer within {timeout}s ({exc})")
        except (ConnectionError, FileNotFoundError) as exc:
            self.lose("crash", f"`{method}`: engine unreachable ({exc}); exit code {self.e.process.poll()}")
        except e2e.Failure as exc:
            fail("command", f"`{method}` failed: {exc}", fatal=fatal)
        return None

    # --------------------------------------------------------------- monitor
    def monitor(self, t0):
        last_sample = -1e9
        last_sizes = {}
        stalls = 0
        while not STOP.is_set():
            started = now()
            code = self.e.process.poll()
            if code is not None:
                self.lose("crash", f"isocastd exited with code {code} during the run; see daemon.log")
                return
            try:
                s = self.e.rpc("status", timeout=20)
                self.status, self.status_failures = s, 0
            except Exception as exc:  # noqa: BLE001
                self.status_failures += 1
                append("events.jsonl", {"t": round(now() - t0, 1), "event": "status_error", "error": repr(exc)})
                if self.status_failures >= 2:
                    self.lose("hang", f"status failed twice in a row: {exc!r}")
                    return
                s = None
            if s is not None:
                if self.recording and s.get("state") == "idle" and not STOP.is_set():
                    fail("recording", "the recording ended by itself: "
                         f"{s.get('last_error') or (s.get('last') or {}).get('errors')}", fatal=True)
                st = s.get("stream") or {}
                if self.stream_expected and not STOP.is_set() and st.get("state") in ("failed", "offline", None):
                    fail("stream", f"the stream ended by itself ({st.get('state')}): {st.get('last_error')}",
                         fatal=True)
            if s is not None and now() - last_sample >= self.args.sample_seconds:
                last_sample = now()
                sample = self.sample(s, now() - t0)
                # Stalled writes: recording, not paused, nothing written since the last sample.
                sizes = {}
                for f in s.get("files") or []:
                    try:
                        sizes[f] = os.path.getsize(f)
                    except OSError:
                        pass
                grew = any(sizes[f] > last_sizes.get(f, -1) for f in sizes)
                live = s.get("live") or {}
                if self.recording and not live.get("paused") and last_sizes and sizes and not grew:
                    stalls += 1
                    if stalls >= 2:
                        fail("disk", f"no recording file grew for two samples ({self.args.sample_seconds:.0f}s apart)")
                        stalls = -1000  # report once
                elif grew and stalls > 0:
                    stalls = 0
                last_sizes = sizes
                if sample.get("free_gb") is not None and sample["free_gb"] < self.args.min_free_gb:
                    fail("disk", f"free space {sample['free_gb']} GB is under {self.args.min_free_gb} GB", fatal=True)
            STOP.wait(max(0.0, 5.0 - (now() - started)))

    def sample(self, s, t):
        rec = {"t": round(t, 1), "wall": time.strftime("%H:%M:%S")}
        rec.update(process_metrics(self.e.process.pid) or {})
        live = s.get("live") or {}
        disk = live.get("disk") or {}
        vfs = os.statvfs(self.root)
        rec.update(state=s.get("state"), paused=live.get("paused"),
                   cpu_percent=round(s["cpu_percent"], 1) if s.get("cpu_percent") is not None else None,
                   free_gb=round(vfs.f_bavail * vfs.f_frsize / 1e9, 2),
                   write_MBps=round(disk.get("write_bytes_per_second", 0) / 1e6, 3) if disk else None,
                   disk_latency_ms=disk.get("latency_ms"),
                   outputs={f.get("source"): [f.get("frames"), f.get("dropped_frames"), f.get("audio_dropouts")]
                            for f in live.get("files", [])},
                   missed_frames=(s.get("engine") or {}).get("missed_frames"),
                   worker_errors=len((s.get("engine") or {}).get("worker_errors") or []),
                   loadavg=e2e.command(["sysctl", "-n", "vm.loadavg"], check=False).stdout.decode().strip())
        st = s.get("stream") or {}
        if st.get("state") not in (None, "offline"):
            rec["stream"] = {k: st.get(k) for k in ("state", "total_kbps", "frames_sent", "dropped_frames",
                                                    "audio_dropouts", "congestion", "reconnects")}
        self.samples.append(rec)
        append("samples.jsonl", rec)
        log("sample t=%.0fs footprint=%sMB threads=%s fds=%s cpu=%s%% free=%sGB write=%sMB/s" % (
            t, rec.get("footprint_mb"), rec.get("threads"), rec.get("fds"), rec.get("cpu_percent"),
            rec.get("free_gb"), rec.get("write_MBps")))
        return rec

    # -------------------------------------------------------------- verifier
    def verifier(self):
        while True:
            path = self.verify_queue.get()
            if path is None:
                return
            try:
                self.checked[str(path)] = self.verify_take(Path(path))
            except Exception as exc:  # noqa: BLE001
                fail("take", f"{Path(path).name}: {exc}")
                self.checked[str(path)] = {"manifest": str(path), "ok": False, "error": str(exc),
                                           "trace": traceback.format_exc()[-2000:]}

    def verify_take(self, path):
        deadline = now() + 180
        manifest = None
        while now() < deadline:
            manifest = e2e.finished(path)
            if manifest or self.lost:
                break
            time.sleep(1)
        shutil.copy2(path, OUT / "manifests" / path.name)
        if not manifest and self.lost:
            return {"manifest": str(path), "ok": False, "problems": [f"not verified: {self.lost}"]}
        if not manifest:
            raise e2e.Failure("take manifest still says recording 180 s after the take ended")
        problems = []
        if manifest["status"] != "complete":
            problems.append(f"status {manifest['status']}")
        if manifest.get("errors"):
            problems.append(f"errors {manifest['errors']}")
        sources = sorted(f["source"] for f in manifest["files"])
        if sources != sorted(SOURCES):
            problems.append(f"files are for {sources}, expected {sorted(SOURCES)}")
        # The hardware profile decodes a take's files at once: one after
        # another, VideoToolbox's 4K60 HEVC decode (~2-3.5x real time while
        # recording) took as long as the take, so the backlog grew.
        with concurrent.futures.ThreadPoolExecutor(3 if PROFILE["strict"] else 1) as pool:
            files = list(pool.map(lambda entry: self.verify_file(path.parent / entry["path"], entry),
                                  manifest["files"]))
        for entry, row in zip(manifest["files"], files):
            problems.extend(f"{Path(entry['path']).name}: {p}" for p in row["problems"])
        if not self.args.keep:
            for entry in manifest["files"]:
                (path.parent / entry["path"]).unlink(missing_ok=True)
        result = {"manifest": str(path), "ok": not problems, "problems": problems,
                  "duration_seconds": manifest.get("duration_seconds"), "files": files,
                  "markers": manifest.get("markers", []), "pauses": manifest.get("pauses", []),
                  "scenes": manifest.get("scenes", [])}
        for p in problems:
            fail("take", f"{path.name}: {p}")
        log(f"verified {path.name}: {'ok' if not problems else '; '.join(problems)[:300]}")
        return result

    def verify_file(self, file, entry):
        problems = []
        row = {"file": file.name, "source": entry["source"], "frames": entry.get("frames"),
               "dropped": entry.get("dropped_frames"), "repeated": entry.get("repeated_frames"),
               "audio_dropouts": entry.get("audio_dropouts"), "duration": entry.get("duration_seconds"),
               "problems": problems}
        for field in ("frames", "dropped", "repeated", "audio_dropouts", "duration"):
            if row[field] is None:
                problems.append(f"manifest has no final {field}")
        if problems or not file.is_file():
            problems.append("file missing" if not file.is_file() else "unfinished counters")
            return row
        row["bytes"] = file.stat().st_size
        out = e2e.command(["ffprobe", "-v", "error", "-count_packets", "-show_streams", "-show_format",
                           "-of", "json", file], timeout=600, check=False)
        if out.returncode or out.stderr.strip():
            problems.append(f"ffprobe: {out.stderr.decode(errors='replace')[-500:]}")
            return row
        info = json.loads(out.stdout)
        video = next((s for s in info["streams"] if s["codec_type"] == "video"), None)
        audio = [s for s in info["streams"] if s["codec_type"] == "audio"]
        if not video:
            problems.append("no video stream")
            return row
        vd = float(video.get("duration") or 0)
        row.update(codec=video["codec_name"], video_seconds=vd, packets=int(video.get("nb_read_packets", -1)),
                   audio_seconds=[float(a.get("duration") or 0) for a in audio])
        if (video["width"], video["height"]) != (entry["width"], entry["height"]):
            problems.append(f"{video['width']}x{video['height']}, manifest {entry['width']}x{entry['height']}")
        if row["packets"] != entry["frames"]:
            problems.append(f"{row['packets']} video packets, manifest {entry['frames']} frames")
        if abs(vd - entry["duration_seconds"]) > 2 / entry["fps"]:
            problems.append(f"video {vd:.3f}s, manifest {entry['duration_seconds']:.3f}s")
        if not audio:
            problems.append("no audio track")
        for a in row["audio_seconds"]:
            if abs(a - vd) > 0.05:
                problems.append(f"audio {a:.3f}s vs video {vd:.3f}s")
        slots = round(entry["duration_seconds"] * entry["fps"])
        accounted = entry["frames"] + entry["dropped_frames"] - entry["repeated_frames"]
        if abs(accounted - slots) > 2:
            problems.append(f"{entry['frames']} frames + {entry['dropped_frames']} dropped - "
                            f"{entry['repeated_frames']} repeated = {accounted}, expected {slots} slots ±2")
        # A full decode of every stream; any error or warning fails the file.
        started = now()
        decoded = decode(file, timeout=max(900, 2 * vd), paced=True)
        frames = [int(m) for m in re.findall(rb"^frame=(\d+)", decoded.stdout, re.M)]
        row["decoded"] = frames[-1] if frames else None
        row["decode_seconds"] = round(now() - started, 1)
        if decoded.returncode or decoded.stderr.strip():
            problems.append(f"decode: {decoded.stderr.decode(errors='replace')[-500:]}")
        if row["decoded"] != entry["frames"]:
            problems.append(f"decoded {row['decoded']} frames, manifest {entry['frames']}")
        return row

    # -------------------------------------------------------------- receiver
    def receive(self):
        remote = bool(self.args.receiver)
        url = f"rtmp://127.0.0.1:{self.remote_port if remote else self.port}/live/e2e"
        n = 0
        while not self.receiver_stop.is_set():
            n += 1
            progress = OUT / f"receiver-{n}.progress"
            with open(OUT / f"receiver-{n}.log", "wb") as errors, open(progress, "wb") as pipe:
                ffmpeg = ["ffmpeg", "-nostdin", "-hide_banner", "-v", "error", "-progress",
                          "pipe:1" if remote else str(progress), "-nostats", "-threads", "1",
                          "-listen", "1", "-i", url, "-map", "0", "-f", "null", "-"]
                if remote:
                    # Progress comes back over ssh's stdout; ffmpeg ends with
                    # the stream, and close_remote() kills a waiting one.
                    argv = [*self.ssh(), self.args.receiver, f"{REMOTE_PATH}; exec " + " ".join(ffmpeg)]
                else:
                    argv = ffmpeg
                self.listener = subprocess.Popen(argv, stdin=subprocess.DEVNULL,
                                                 stdout=pipe if remote else subprocess.DEVNULL, stderr=errors)
                opened = time.strftime("%H:%M:%S")
                code = self.listener.wait()
            text = progress.read_text() if progress.exists() else ""
            frames = re.findall(r"^frame=(\d+)", text, re.M)
            seconds = re.findall(r"^out_time_us=(\d+)", text, re.M)
            # ffmpeg reports the sender closing the connection as an I/O
            # error; disconnects are counted by connections, not this line.
            err = "\n".join(line for line in (OUT / f"receiver-{n}.log").read_text(errors="replace").splitlines()
                            if line.strip() and "Error during demuxing: Input/output error" not in line)
            row = {"n": n, "opened": opened, "closed": time.strftime("%H:%M:%S"), "exit": code,
                   "frames": int(frames[-1]) if frames else 0,
                   "seconds": int(seconds[-1]) / 1e6 if seconds else 0.0, "errors": err[-2000:]}
            if row["frames"] or err:
                self.receiver["connections"].append(row)
                log(f"receiver connection {n} closed: {row['frames']} frames, {row['seconds']:.1f}s, exit {code}")
            progress.unlink(missing_ok=True)

    # --------------------------------------------------------------- session
    def run(self, cfg, planned):
        rng = random.Random(self.args.seed)
        scale = self.args.scale
        t0 = now()
        mon = threading.Thread(target=self.monitor, args=(t0,), daemon=True)
        ver = threading.Thread(target=self.verifier, daemon=True)
        ver.start()
        self.recording = False
        mon.start()
        started = self.call("start", {})
        if not started:
            return t0, None
        self.recording = True
        session_start = now()
        self.takes.append(started["manifest"])
        log(f"recording; {planned / 3600:.2f} h planned, take 1 {Path(started['manifest']).name}")
        if self.stream_on:
            self.receiver_thread = threading.Thread(target=self.receive, daemon=True)
            self.receiver_thread.start()
            time.sleep(1)
            if self.args.receiver:
                e2e.until(self.remote_listening, 60, f"the receiver on {self.args.receiver}")
            if self.call("stream_start", None) is not None:
                self.stream_expected = True
                log(f"streaming to the receiver on {self.receiver.get('host') or 'this Mac'}")
        nxt = {"split": SPLIT_MIN * 60 * scale, "pause": PAUSE_MIN * 60 * scale * 0.5,
               "marker": MARKER_MIN * 60 * scale, "scene": rng.uniform(*SCENE_MIN) * 60 * scale}
        scenes = [sc["id"] for sc in cfg["scenes"]]
        current = scenes[0]
        while not STOP.is_set():
            elapsed = now() - session_start
            if elapsed >= planned:
                break
            kind = min(nxt, key=nxt.get)
            if nxt[kind] >= planned - 10:
                STOP.wait(planned - elapsed)
                break
            if nxt[kind] > elapsed:
                STOP.wait(nxt[kind] - elapsed)
                continue
            at = round(now() - session_start, 1)
            if kind == "scene":
                current = rng.choice([s for s in scenes if s != current])
                result = self.call("switch_scene", {"scene": current, "transition": rng.choice(TRANSITIONS)})
                if result:
                    self.events["switches"].append(result)
                nxt[kind] += rng.uniform(*SCENE_MIN) * 60 * scale
            elif kind == "marker":
                label = f"soak marker {len(self.events['markers']) + 1}"
                result = self.call("marker", {"label": label})
                if result:
                    self.events["markers"].append(result)
                nxt[kind] += MARKER_MIN * 60 * scale
            elif kind == "split":
                result = self.call("split", None)
                if result:
                    self.events["splits"] += 1
                    self.verify_queue.put(self.takes[-1])
                    self.takes.append(result["manifest"])
                    log(f"split: take {len(self.takes)} {Path(result['manifest']).name}")
                    # The new take begins at the next whole second; pause and
                    # stop are refused (or drop the take) until it has.
                    if result.get("begins_at"):
                        begins = datetime.datetime.fromisoformat(result["begins_at"]).timestamp()
                        STOP.wait(max(0.0, begins - time.time()) + 0.5)
                nxt[kind] += SPLIT_MIN * 60 * scale
            elif kind == "pause":
                hold = max(3.0, rng.uniform(10, 60) * min(1.0, scale * 4))
                p0 = now()
                if self.call("pause", None) is not None:
                    self.paused = True
                    STOP.wait(hold)
                    resumed = self.call("resume", None)
                    self.paused = False
                    self.events["pauses"].append({"at": at, "seconds": round(now() - p0, 2),
                                                  "ok": resumed is not None})
                nxt[kind] += PAUSE_MIN * 60 * scale
            append("events.jsonl", {"t": at, "event": kind})
        # Wind down: stream, then recording (the monitor stops expecting both first).
        alive = self.e.process.poll() is None and not self.lost
        was_recording, self.recording = self.recording, False
        if self.stream_expected:
            self.stream_expected = False
            self.receiver_stop.set()
            self.stream_final = self.call("stream_stop", None, fatal=False) if alive else None
        if self.paused and alive:
            self.call("resume", None, fatal=False)
        report = None
        if alive and was_recording:
            report = self.call("stop", None, timeout=180, fatal=False)
        self.session_seconds = now() - session_start
        log(f"stopped after {self.session_seconds / 60:.1f} min")
        if self.takes:
            self.verify_queue.put(self.takes[-1])
        self.verify_queue.put(None)
        if self.stream_on:
            self.stop_receiver()
        ver.join(timeout=1200)
        if ver.is_alive():
            fail("hang", "verifying the last take did not finish within the time allowed")
        STOP.set()
        mon.join(timeout=30)
        return t0, report

    def stop_receiver(self):
        self.receiver_stop.set()
        thread = getattr(self, "receiver_thread", None)
        if thread:
            thread.join(timeout=15)
            if thread.is_alive() and self.listener and self.listener.poll() is None:
                self.listener.kill()
                thread.join(timeout=10)
        if self.args.receiver:
            self.close_remote()

    # ----------------------------------------------------------- crash tests
    def crash_test(self, label, paused):
        """Start a take, kill -9 isocastd while it records (or is paused),
        relaunch it: it must report the take interrupted, and every file must
        play up to within 2.5 s of the kill (of the pause, if paused)."""
        log(f"crash test {label}: starting")
        res = {"label": label, "paused": paused, "problems": []}
        problems = res["problems"]
        started = self.call("start", {}, fatal=False)
        if not started:
            problems.append("start failed")
            return res
        manifest = Path(started["manifest"])
        res["manifest"] = str(manifest)
        files = [Path(f) for f in started["files"]]
        time.sleep(self.args.crash_seconds)
        if paused:
            res["expected_seconds"] = (self.e.rpc("status").get("live") or {}).get("elapsed_seconds")
            if self.call("pause", None, fatal=False) is None:
                problems.append("pause failed")
            time.sleep(10)
        live = self.e.rpc("status").get("live") or {}
        if not paused:
            res["expected_seconds"] = live.get("elapsed_seconds")
        pid = self.e.process.pid
        os.kill(pid, signal.SIGKILL)
        self.e.process.wait(timeout=30)
        log(f"crash test {label}: killed isocastd pid {pid} at {res['expected_seconds']}s; relaunching")
        relaunched = now()
        self.e.start()
        res["relaunch_seconds"] = round(now() - relaunched, 1)
        want = os.path.realpath(manifest)
        reported = None
        deadline = now() + 30
        while now() < deadline and reported is None:
            for item in self.e.rpc("status").get("interrupted") or []:
                if os.path.realpath(os.path.expanduser(str(item.get("manifest")))) == want:
                    reported = item
            if reported is None:
                time.sleep(1)
        if reported is None:
            problems.append("the relaunched engine did not report the take as interrupted")
        man = e2e.read_json(manifest)
        res["manifest_status"] = man.get("status")
        if man.get("status") != "interrupted":
            problems.append(f"manifest status {man.get('status')}, expected interrupted")
        fps = {f["path"]: f.get("fps") or FPS for f in man.get("files", [])}
        res["files"] = []
        expect = res["expected_seconds"]
        for f in files:
            row = {"file": f.name}
            res["files"].append(row)
            if not f.is_file():
                problems.append(f"{f.name}: missing")
                continue
            row["bytes"] = f.stat().st_size
            d = decode(f, timeout=600)
            frames = [int(m) for m in re.findall(rb"^frame=(\d+)", d.stdout, re.M)]
            row["decoded"] = frames[-1] if frames else 0
            row["decode_exit"] = d.returncode
            row["decode_errors"] = d.stderr.decode(errors="replace")[-500:]
            row["playable_seconds"] = round(row["decoded"] / fps.get(f.name, FPS), 3)
            if d.returncode or not row["decoded"]:
                problems.append(f"{f.name}: does not decode (exit {d.returncode}): {row['decode_errors'][-200:]}")
            elif expect is None:
                problems.append("no elapsed time at the kill to compare with")
            else:
                row["short_by"] = round(expect - row["playable_seconds"], 3)
                if not -1.0 <= row["short_by"] <= 2.5:
                    problems.append(f"{f.name}: plays {row['playable_seconds']}s, expected {expect:.1f}s "
                                    "(short by more than 2.5 s)")
        if not self.args.keep:
            for f in files:
                f.unlink(missing_ok=True)
        e2e.until(lambda: self.e.rpc("status").get("state") == "idle", 60, "an idle recorder after the crash test")
        for p in problems:
            fail("crash-test", f"{label}: {p}")
        log(f"crash test {label}: {'PASS' if not problems else 'FAIL'} "
            f"(short by {[r.get('short_by') for r in res['files']]})")
        return res


# ------------------------------------------------------------------- analysis

def analyze(soak, report_stop, args):
    a = {}
    takes = [soak.checked.get(str(p)) or {"manifest": str(p), "ok": False, "problems": ["never verified"]}
             for p in soak.takes]
    a["takes"] = takes
    a["events"] = {"switches": len(soak.events["switches"]), "markers": len(soak.events["markers"]),
                   "pauses": len(soak.events["pauses"]), "splits": soak.events["splits"], "takes": len(takes)}
    # Drops: information only, except in a strict profile (checked below).
    outputs = {}
    for t in takes:
        for f in t.get("files", []):
            o = outputs.setdefault(f["source"], {"frames": 0, "dropped": 0, "repeated": 0, "audio_dropouts": 0})
            for k in o:
                o[k] += f.get(k) or 0
    a["drops"] = outputs
    # Resource trends after warm-up.
    span = soak.samples[-1]["t"] if soak.samples else 0
    warmup = min(600, 0.2 * span)
    a["trends"] = {k: trend(soak.samples, k, warmup) for k in ("footprint_mb", "rss_mb", "threads", "fds")}
    for key, limit in (("footprint_mb", args.max_memory_growth_mb), ("threads", args.max_thread_growth),
                       ("fds", args.max_fd_growth)):
        tr = a["trends"][key]
        if tr and tr["growth"] > limit and not soak.lost:
            fail("leak", f"{key} grew by {tr['growth']} after warm-up ({tr['start']} → {tr['end']}, "
                         f"{tr['per_hour']}/h), limit {limit}")
    if soak.lost:
        # Everything after a crash or hang follows from it; report only that.
        a["engine_lost"] = soak.lost
        return a
    for t in takes:
        if t.get("problems") == ["never verified"]:
            fail("take", f"{Path(t['manifest']).name} was never verified")
    if report_stop is not None:
        listed = sorted(str(Path(p).resolve()) for p in report_stop.get("takes", []))
        if listed and listed != sorted(str(Path(p).resolve()) for p in soak.takes):
            fail("take", f"stop report lists takes {listed}, the soak tracked {soak.takes}")
        if report_stop.get("errors"):
            fail("recording", f"stop report errors: {report_stop['errors']}")
        a["stop_warnings"] = report_stop.get("warnings", [])
    # Every marker, pause and scene switch made it into a manifest.
    markers = [m for t in takes for m in t.get("markers", [])]
    pauses = [p for t in takes for p in t.get("pauses", [])]
    scenes = [s for t in takes for s in t.get("scenes", [])]
    for m in soak.events["markers"]:
        if not any(x["label"] == m["label"] for x in markers):
            fail("take", f"marker '{m['label']}' is missing from the manifests")
    done = [p for p in soak.events["pauses"] if p["ok"]]
    if len(pauses) != len(done):
        fail("take", f"{len(done)} pauses made, {len(pauses)} in the manifests")
    switches = len(soak.events["switches"])
    if not switches <= len(scenes) <= switches + len(takes):
        fail("take", f"{switches} scene switches made, {len(scenes)} scene entries for {len(takes)} takes")
    # Recorded time = session time - paused time.
    recorded = sum(t.get("duration_seconds") or 0 for t in takes)
    paused = sum(p["seconds"] for p in soak.events["pauses"])
    expected = getattr(soak, "session_seconds", 0) - paused
    a["recorded_seconds"] = round(recorded, 2)
    a["expected_seconds"] = round(expected, 2)
    if takes and abs(recorded - expected) > 10:
        fail("take", f"recorded {recorded:.1f}s, expected {expected:.1f}s (session minus pauses) ±10s")
    # Stream and receiver.
    final = getattr(soak, "stream_final", None)
    a["stream"] = final
    a["receiver"] = soak.receiver["connections"]
    if soak.stream_on:
        conns = soak.receiver["connections"]
        if final is None:
            fail("stream", "no final stream report")
        else:
            if final.get("reconnects"):
                fail("stream", f"the stream reconnected {final['reconnects']} times")
            received = sum(c["seconds"] for c in conns)
            sent = final.get("elapsed_seconds") or 0
            if abs(received - sent) > max(10, 0.01 * sent):
                fail("stream", f"receiver got {received:.1f}s, the stream ran {sent:.1f}s")
        if len(conns) != 1:
            fail("stream", f"the receiver saw {len(conns)} connections, expected 1")
        for c in conns:
            if c["errors"]:
                fail("stream", f"receiver connection {c['n']} errors: {c['errors'][-500:]}")
    if PROFILE["strict"]:
        # The program and the stream are protected: any drop fails. ISOs
        # are dropped first under overload by design; fail above a ratio.
        for name, o in outputs.items():
            slots = o["frames"] + o["dropped"]
            if name == "program":
                if o["dropped"] or o["audio_dropouts"]:
                    fail("drops", f"the program dropped {o['dropped']} frames and had "
                                  f"{o['audio_dropouts']} audio dropouts")
            elif slots and o["dropped"] / slots > args.max_iso_drop_ratio:
                fail("drops", f"ISO {name} dropped {o['dropped']} of {slots} frames "
                              f"({o['dropped'] / slots:.3%}), limit {args.max_iso_drop_ratio:.3%}")
        if final and (final.get("dropped_frames") or final.get("audio_dropouts")):
            fail("drops", f"the stream dropped {final.get('dropped_frames')} frames and had "
                          f"{final.get('audio_dropouts')} audio dropouts")
    return a


def scan_logs(root):
    hits = []
    for path in [root / "daemon.log", *sorted((root / "logs").rglob("*"))]:
        if path.is_file():
            for line in path.read_text(errors="replace").splitlines():
                if "panicked at" in line or "fatal runtime error" in line:
                    hits.append(f"{path.name}: {line[:300]}")
    return hits


def copy_logs(root):
    target = OUT / "logs"
    target.mkdir(exist_ok=True)
    for path in [root / "daemon.log", *sorted((root / "logs").rglob("*"))]:
        if path.is_file():
            data = path.read_bytes()
            (target / path.name).write_bytes(data[-20 * 2**20:])  # the last 20 MB


# -------------------------------------------------------------------- summary

def summary(report):
    def cell(v):
        return str(v).replace("|", "/").replace("\n", " ")[:400]
    a = report.get("analysis", {})
    verdict = report.get("verdict", "FAIL")
    profile = PROFILES.get(report.get("profile", "hosted"), PROFILES["hosted"])
    (w, h, fps), strict = profile["canvas"], profile["strict"]
    isos = " + ".join(f"{iw}×{ih}" for iw, ih in profile["sources"].values())
    lines = [f"## {'Hardware' if strict else 'Nightly'} soak: {verdict}", ""]
    caps = report.get("capabilities", {})
    receiver = report.get("receiver") or "this Mac"
    lines += [f"{report.get('session_minutes', 0):.0f} min of a {w}×{h}@{fps} program + ISOs {isos} "
              f"({caps.get('record_codec')}), stream {'on, to ' + receiver if report.get('stream') else 'off: ' + cell(report.get('stream_reason'))}; "
              f"{caps.get('chip')}, {caps.get('cores')} cores, "
              f"{(caps.get('memory_bytes') or 0) / 2**30:.1f} GiB; seed {report.get('seed')}.", ""]
    fails = report.get("failures", [])
    if fails:
        lines += ["### Failures", "", "| Kind | When | What |", "|---|---|---|"]
        lines += [f"| {cell(f['kind'])} | {f['at']} | {cell(f['message'])} |" for f in fails[:50]]
        lines.append("")
    if report.get("error"):
        lines += ["### Harness error", "", "```", report["error"][-3000:], "```", ""]
    ev = a.get("events", {})
    lines += [f"Events: {ev.get('takes', 0)} takes, {ev.get('splits', 0)} splits, {ev.get('switches', 0)} scene "
              f"switches, {ev.get('pauses', 0)} pauses, {ev.get('markers', 0)} markers. Recorded "
              f"{a.get('recorded_seconds', 0):.1f}s, expected {a.get('expected_seconds', 0):.1f}s.", ""]
    lines += ["### Resources after warm-up", "", "| Metric | Start | End | Growth | Per hour | Min–max |",
              "|---|---:|---:|---:|---:|---:|"]
    for key, tr in (a.get("trends") or {}).items():
        if tr:
            lines.append(f"| {key} | {tr['start']} | {tr['end']} | {tr['growth']} | {tr['per_hour']} | "
                         f"{tr['min']}–{tr['max']} |")
    lines += ["", "### Takes", "", "| Take | Seconds | Files | Result |", "|---|---:|---:|---|"]
    for t in a.get("takes", []):
        lines.append(f"| {cell(Path(t['manifest']).name)} | {t.get('duration_seconds') or 0:.1f} | "
                     f"{len(t.get('files', []))} | {'ok' if t.get('ok') else cell('; '.join(t.get('problems') or [t.get('error', '')]))} |")
    lines += ["", "### Drops (program and stream must be 0; ISOs up to the limit)" if strict else
              "### Drops (information only, not a failure on hosted VMs)", "",
              "| Output | Frames | Dropped | Ratio | Repeated | Audio dropouts |", "|---|---:|---:|---:|---:|---:|"]
    for name, o in (a.get("drops") or {}).items():
        slots = o["frames"] + o["dropped"]
        lines.append(f"| {name} | {o['frames']} | {o['dropped']} | {o['dropped'] / slots if slots else 0:.2%} | "
                     f"{o['repeated']} | {o['audio_dropouts']} |")
    st = a.get("stream")
    if st:
        slots = (st.get("frames_sent") or 0) + (st.get("dropped_frames") or 0)
        lines.append(f"| stream | {st.get('frames_sent')} | {st.get('dropped_frames')} | "
                     f"{(st.get('dropped_frames') or 0) / slots if slots else 0:.2%} | – | {st.get('audio_dropouts')} |")
        recv = a.get("receiver", [])
        lines += ["", f"Stream: {st.get('elapsed_seconds', 0):.0f}s sent, {st.get('reconnects', 0)} reconnects; "
                  f"receiver {len(recv)} connection(s), {sum(c['seconds'] for c in recv):.0f}s and "
                  f"{sum(c['frames'] for c in recv)} frames decoded."]
    crashes = report.get("crash_tests")
    if crashes is not None:
        lines += ["", "### Crash tests (kill -9, relaunch, files play)", "",
                  "| Test | Killed at | Plays (s) | Result |", "|---|---:|---|---|"]
        for c in crashes:
            plays = ", ".join(str(f.get("playable_seconds")) for f in c.get("files", []))
            lines.append(f"| {c['label']} | {c.get('expected_seconds')} | {plays} | "
                         f"{'ok' if not c.get('problems') else cell('; '.join(c['problems']))} |")
    return "\n".join(lines) + "\n"


# ----------------------------------------------------------------------- main

def interrupted(signum, _frame):
    if not any(f["kind"] == "interrupted" for f in FAILS):
        fail("interrupted", f"signal {signum}: stopping early", fatal=True)


def scene_setup(root, codec):
    cfg = e2e.setup(root, codec, iso=True, width=WIDTH, height=HEIGHT, fps=FPS)
    for source in cfg["sources"]:
        if source["id"] in PROFILE["sources"]:
            source["width"], source["height"] = PROFILE["sources"][source["id"]]
    full = {"x": 0, "y": 0, "width": WIDTH, "height": HEIGHT}
    cfg["name"] = "isolated soak"
    cfg["scenes"] = [
        {"id": "one", "name": "One", "items": [{"source": "bars", "rect": full}]},
        {"id": "two", "name": "Two", "items": [{"source": "bars2", "rect": full}]},
        {"id": "both", "name": "Both", "items": [
            {"source": "bars", "rect": full},
            {"source": "bars2", "rect": {"x": WIDTH // 2, "y": HEIGHT // 2,
                                         "width": WIDTH // 2 - 32, "height": HEIGHT // 2 - 32}}]}]
    return cfg


def main():
    global OUT
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    target = Path(os.environ.get("CARGO_TARGET_DIR", e2e.REPO / "target"))
    scratch = Path(os.environ.get("ISOCAST_SCRATCH", tempfile.gettempdir()))
    parser.add_argument("--hours", type=float, default=5.0)
    parser.add_argument("--deadline", type=float, help="Unix time by which the report must be written")
    parser.add_argument("--scale", type=float, default=1.0,
                        help="multiply event intervals (split 30, pause 40, marker 10, scene 1-4 min)")
    parser.add_argument("--sample-seconds", type=float, default=30)
    parser.add_argument("--seed", type=int, default=None)
    parser.add_argument("--daemon", type=Path, default=target / "release/isocastd")
    parser.add_argument("--work", type=Path, default=scratch / "soak")
    parser.add_argument("--out", type=Path, default=Path("soak-out"))
    parser.add_argument("--summary", type=Path, help="render a saved report.json as Markdown and exit")
    parser.add_argument("--keep", action="store_true", help="keep media and the engine folder")
    parser.add_argument("--no-stream", action="store_true")
    parser.add_argument("--min-free-gb", type=float, default=3.0)
    parser.add_argument("--max-memory-growth-mb", type=float, default=200)
    parser.add_argument("--max-thread-growth", type=float, default=16)
    parser.add_argument("--max-fd-growth", type=float, default=32)
    parser.add_argument("--profile", choices=sorted(PROFILES), default="hosted",
                        help="hosted (720p30, drops informational) or hardware (4K60 + 1080p60 HEVC "
                             "ISOs, 1080p60 program and stream, strict drops, crash tests)")
    parser.add_argument("--max-iso-drop-ratio", type=float, default=0.001,
                        help="hardware profile: the share of an ISO's frame slots it may drop")
    parser.add_argument("--receiver", help="stream to an ffmpeg receiver on this ssh host "
                                           "instead of this Mac")
    parser.add_argument("--receiver-ssh-config", type=Path, help="ssh -F file for --receiver")
    parser.add_argument("--crash-seconds", type=float, default=30)
    parser.add_argument("--no-crash-tests", action="store_true")
    args = parser.parse_args()
    if args.summary:
        print(summary(json.loads(args.summary.read_text())), end="")
        return 0
    args.seed = args.seed if args.seed is not None else int(time.time())
    OUT = args.out.resolve()
    if OUT.exists():
        shutil.rmtree(OUT)
    (OUT / "manifests").mkdir(parents=True)
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    use_profile(args.profile)
    report = {"version": 1, "seed": args.seed, "hours": args.hours, "scale": args.scale,
              "profile": args.profile, "receiver": args.receiver,
              "started_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
    root = engine = soak = None
    try:
        e2e.require(sys.platform == "darwin", "macOS is required")
        for tool in ("ffmpeg", "ffprobe", "codesign"):
            e2e.require(shutil.which(tool), f"missing required tool: {tool}")
        e2e.require(args.daemon.is_file(), f"daemon not found: {args.daemon}; build release first")
        scratch.mkdir(parents=True, exist_ok=True)
        with open(scratch / ".bench.lock", "a") as lock:
            log("waiting for the benchmark lock")
            fcntl.flock(lock, fcntl.LOCK_EX)
            caps = e2e.capabilities()
            caps["record_codec"] = PROFILE["codec"] or ("h264" if caps["h264"] else "prores422")
            e2e.require(caps.get(caps["record_codec"], True),
                        f"no hardware {caps['record_codec']} encoder for --profile {args.profile}")
            report["capabilities"] = caps
            planned = args.hours * 3600
            if args.deadline:
                planned = min(planned, args.deadline - time.time() - FINISH_RESERVE)
                e2e.require(planned > 60, "not enough time before --deadline for a soak")
            args.work.mkdir(parents=True, exist_ok=True)
            root = Path(tempfile.mkdtemp(prefix="soak-", dir=args.work)).resolve()
            vfs = os.statvfs(root)
            report["free_gb_at_start"] = round(vfs.f_bavail * vfs.f_frsize / 1e9, 1)
            engine = e2e.Engine(root, args.daemon)
            cfg = scene_setup(root, caps["record_codec"])
            soak = Soak(args, engine, caps, root)
            soak.stream_on = not args.no_stream and caps["h264"]
            report["stream"] = soak.stream_on
            report["stream_reason"] = ("--no-stream" if args.no_stream else
                                       None if caps["h264"] else "no hardware H.264 encoder on this machine")
            if soak.stream_on:
                if args.receiver:
                    soak.open_tunnel()
                    report["receiver"] = f"{args.receiver} ({soak.receiver['host']})"
                    log(f"receiver on {args.receiver} ({soak.receiver['host']}) port {soak.remote_port}, "
                        f"tunnelled from 127.0.0.1:{soak.port}")
                else:
                    with socket.socket() as reservation:
                        reservation.bind(("127.0.0.1", 0))
                        soak.port = reservation.getsockname()[1]
                cfg["stream"] = {"service": "custom", "server": f"rtmp://127.0.0.1:{soak.port}/live",
                                 **PROFILE["stream"],
                                 "width": WIDTH, "height": HEIGHT, "keyframe_interval_seconds": 2}
            elif PROFILE["strict"]:
                e2e.require(args.no_stream, "the hardware profile needs a hardware H.264 encoder to stream")
            engine.apply(cfg)
            report["planned_minutes"] = round(planned / 60, 1)
            log(f"soak: {planned / 60:.1f} min, codec {caps['record_codec']}, stream {soak.stream_on}, "
                f"seed {args.seed}, engine {root}")
            _, stop_report = soak.run(cfg, planned)
            report["session_minutes"] = round(getattr(soak, "session_seconds", 0) / 60, 1)
            report["analysis"] = analyze(soak, stop_report, args)
            signalled = any(f["kind"] == "interrupted" for f in FAILS)
            if PROFILE["strict"] and not args.no_crash_tests:
                if soak.lost or signalled or engine.process.poll() is not None:
                    log("crash tests skipped: the engine was lost or the run was interrupted")
                else:
                    report["crash_tests"] = []
                    for label, paused in (("recording", False), ("paused", True)):
                        if any(f["kind"] == "interrupted" for f in FAILS):
                            break
                        try:
                            report["crash_tests"].append(soak.crash_test(label, paused))
                        except Exception as exc:  # noqa: BLE001
                            fail("crash-test", f"{label}: {exc!r}")
                            report["crash_tests"].append({"label": label, "problems": [repr(exc)],
                                                          "trace": traceback.format_exc()[-2000:]})
                            break
            # A clean shutdown is part of the soak: hung native teardown fails it.
            if engine.process.poll() is None and not soak.lost:
                try:
                    engine.rpc("shutdown", timeout=15)
                    code = engine.process.wait(timeout=60)
                    if code:
                        fail("crash", f"isocastd exited with code {code} at shutdown")
                except (OSError, ValueError, e2e.Failure, subprocess.TimeoutExpired) as exc:
                    fail("hang", f"isocastd did not shut down cleanly: {exc!r}")
            for hit in scan_logs(root):
                fail("crash", f"panic in the logs: {hit}")
    except Exception as exc:  # noqa: BLE001
        report["error"] = traceback.format_exc()
        log(f"FAIL harness: {exc}")
    finally:
        STOP.set()
        if soak:
            soak.stop_receiver()
        if engine:
            engine.close()
        if root:
            try:
                copy_logs(root)
            except OSError as exc:
                log(f"could not copy logs: {exc}")
            if not args.keep:
                shutil.rmtree(root, ignore_errors=True)
                try:
                    args.work.rmdir()  # only if nothing else is there
                except OSError:
                    pass
        report["failures"] = FAILS
        report["verdict"] = "PASS" if not FAILS and not report.get("error") else "FAIL"
        report["finished_at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        e2e.write_json(OUT / "report.json", report)
        (OUT / "summary.md").write_text(summary(report))
        log(f"soak {report['verdict']}: {len(FAILS)} failure(s); report {OUT / 'report.json'}")
    return 0 if report["verdict"] == "PASS" else 1


if __name__ == "__main__":
    sys.exit(main())
