#!/usr/bin/env python3
"""Isocast against OBS Studio on the same Mac, same scenes (PLAN S.1).

    scripts/obs-bench.py --work "$ISOCAST_SCRATCH/obs-bench" \
        [--obs /Applications/OBS.app] [--apps isocast,obs,obs-metal] \
        [--scenarios program-1080,program-4k,flagship,stream] [--runs 3] \
        [--seconds 20] [--powermetrics-log FILE] [--detach]
    scripts/obs-bench.py --work ... --summary     # the table from results.jsonl

OBS needs the desktop session, so on a test Mac run it through the bridge;
--detach returns at once and logs to <work>/log-<time>.txt, so a long run
doesn't hold the bridge (other agents' jobs) for its whole length:

    scripts/testmac.sh run 'cargo build --release -p isocast-daemon -p isocast-cli'
    scripts/testmac.sh gui 'python3 scripts/obs-bench.py --work "$ISOCAST_SCRATCH/obs-bench" --detach'

Every scenario is the same scene in both apps, from scripts/bench.py's
synthetic sources (no devices, so no privacy prompts): a looping 4K60
"desktop" clip filling the canvas, a looping 1080p60 "camera" clip inset in
the bottom-right corner (22% of the canvas, 48 px in), and a 1 kHz beep for
100 ms every second (Isocast's tone source; in OBS a media source looping a
WAV of the same beeps). Video files are hardware HEVC at quality 70 (OBS:
Apple VT HEVC, rate control CRF 70), 1 s keyframes, Main profile, B-frames,
Rec. 709 limited range, AAC 256 kb/s stereo, crash-safe .mov (OBS: Hybrid
MOV).

  program-1080  1080p60 canvas, only the program recorded
  program-4k    4K60 canvas, only the program recorded
  flagship      Isocast's flagship: the 4K60 program plus the desktop and
                camera as ISO files of their own. OBS has no ISO files, so it
                records only the program here (the same as program-4k):
                Isocast does more work in this row.
  stream        1080p60 canvas streamed (hardware H.264 CBR 6000 kb/s, High,
                no B-frames, 2 s keyframes, AAC 160 kb/s) to a local ffmpeg
                RTMP listener, never a real service; nothing recorded

Apps: isocast (isocastd, headless), obs (OBS with its default OpenGL
renderer) and obs-metal (OBS's Metal renderer, new in OBS 32). OBS runs
minimized to the menu bar with its preview off, so it draws no window.

Each app is launched and quit once first (warm-up, not measured: the first
launch of OBS after installing takes seconds longer). Then the apps take
turns, scenario by scenario, round after round, each run holding bench.py's
machine-wide lock. A run measures 4 s of the machine at rest, launches the
app fresh (Isocast: its own signed dev app, socket and setup; OBS: its own
home folder, so OBS's real settings, profiles, scene collections and plugins
are never read or touched, with obs-websocket on port 4466 behind a password
made for the run), builds the scene, lets it settle, starts the output,
waits 2 s, measures for --seconds, waits 2 s and stops. Measured:

- CPU in % of one core, from CPU time over the window: the app's processes
  (isocastd; OBS and any child process) and its VideoToolbox services (VT
  runs each client's hardware encode and decode sessions in XPC service
  processes of their own: the ones started after the app launched). The
  machine's VT services in all (the aerial wallpaper decodes video there
  too) and WindowServer are kept separately.
- memory: the app's physical footprint (Activity Monitor's Memory, which
  counts GPU buffers; resident memory doesn't on Apple silicon) and RSS;
- GPU time of the app, its VT services and WindowServer (the GPU driver's
  per-process counters, via ioreg);
- with powermetrics (root: `sudo -n` must work, or --powermetrics-log names
  a plist log that another powermetrics writes with the cpu_power,
  gpu_power and tasks samplers): CPU, GPU and combined power (mW) over the
  window and at rest, GPU active residency, the app's energy impact;
- frames: Isocast's program frames drawn and missed (status) and each
  file's frames and dropped frames (the session report); OBS's rendered,
  render-skipped and output-skipped frames (GetStats) and the stream's
  (GetStreamStatus) and its log's frames skipped for encoding lag; for
  both, the frames ffprobe decodes from each file (and from what the RTMP
  listener received) against the duration at 60 fps, and how many of them
  repeat the frame before (an encoder that falls behind can keep the frame
  rate by sending the last picture again, as OBS does: the file then has
  every frame but not every picture). The clips move every frame, so
  distinct frames are the ones that count;
- launch (until the control API answers requests), start and stop (to the
  output's started/stopped notice: Isocast's start reply comes once every
  file exists, its stop reply once they are finished), file sizes, codecs,
  bitrates, keyframe spacing, B-frames, colour tags.

Results are appended to <work>/results.jsonl; recorded media is deleted
after each run, ffprobe's findings kept. A summary table is printed at the
end. Python's `websockets` comes from a venv in <work>/venv, made once.
"""

import argparse
import base64
import contextlib
import fcntl
import hashlib
import json
import os
import plistlib
import re
import secrets
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import time
from datetime import timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.dont_write_bytecode = True  # no __pycache__ in scripts/
import bench  # noqa: E402

REPO = bench.REPO
OBS_PORT = 4466
WEBSOCKETS = "websockets==15.0.1"
TONE_CLIP = "tone-1k-beeps.wav"
SCENARIOS = ("program-1080", "program-4k", "flagship", "stream")
APPS = ("isocast", "obs", "obs-metal")
FPS = 60
# Isocast's file settings (crates/isocast-platform-macos/src/writer.rs and
# stream.rs), matched in OBS.
RECORD_AUDIO_KBPS = 256
STREAM_AUDIO_KBPS = 160
STREAM_VIDEO_KBPS = 6000
# A frame whose luma differs from the one before by less than this (mean
# absolute difference, 0-255, at 960 px wide) repeats it. These clips move
# every frame: new frames measured 0.5-2, OBS's repeats under 0.05.
REPEAT_YDIF = 0.1


class BenchError(Exception):
    pass


def must(cmd, env=None):
    out = bench.run(cmd, env=env, check=False)
    if out.returncode != 0:
        raise BenchError(f"{' '.join(map(str, cmd))} failed: {(out.stderr or out.stdout).strip()}")
    return out


def log(text):
    print(f"obs-bench {time.strftime('%H:%M:%S')}: {text}", file=sys.stderr, flush=True)


def canvas_of(scenario):
    return (1920, 1080) if scenario in ("program-1080", "stream") else (3840, 2160)


def inset(cw, ch):
    """The camera's rectangle, as bench.py places it: x, y, width, height."""
    w, h = cw * 22 // 100, ch * 22 // 100
    return cw - w - 48, ch - h - 48, w, h


# Processes ---------------------------------------------------------------


def process_table():
    out = bench.run(["ps", "-A", "-o", "pid=,ppid=,comm="], check=False).stdout
    rows = []
    for line in out.splitlines():
        parts = line.split(None, 2)
        if len(parts) == 3 and parts[0].isdigit():
            rows.append((int(parts[0]), int(parts[1]), parts[2]))
    return rows


def process_tree(root):
    rows = process_table()
    pids = {root}
    grew = True
    while grew:
        grew = False
        for pid, ppid, _ in rows:
            if ppid in pids and pid not in pids:
                pids.add(pid)
                grew = True
    names = {pid: Path(comm).name for pid, _, comm in rows}
    return {pid: names.get(pid, "?") for pid in pids if pid in names}


def named(*names):
    return [pid for pid, _, comm in process_table() if Path(comm).name in names]


def vt_services():
    return named("VTEncoderXPCService", "VTDecoderXPCService")


def rss_mb(pids):
    if not pids:
        return 0.0
    out = bench.run(["ps", "-o", "rss=", "-p", ",".join(map(str, pids))], check=False)
    return sum(int(x) for x in out.stdout.split()) / 1024


def footprint_mb(pids):
    """Physical footprint (Activity Monitor's Memory: includes GPU memory) per top."""
    if not pids:
        return 0.0
    cmd = ["top", "-l", "1", "-s", "0", "-stats", "pid,mem"]
    for pid in pids:
        cmd += ["-pid", str(pid)]
    total = 0.0
    units = {"B": 1 / 2**20, "K": 1 / 1024, "M": 1, "G": 1024}
    for line in bench.run(cmd, check=False).stdout.splitlines():
        parts = line.split()
        if len(parts) == 2 and parts[0].isdigit() and int(parts[0]) in pids:
            m = re.match(r"([\d.]+)([BKMG])", parts[1])
            if m:
                total += float(m.group(1)) * units[m.group(2)]
    return total


def gpu_ns():
    """GPU time (ns) by PID, from the GPU driver's per-client counters."""
    out = bench.run(["ioreg", "-r", "-c", "AGXDeviceUserClient", "-l", "-w0"], check=False).stdout
    totals = {}
    for client in out.split("+-o ")[1:]:
        creator = re.search(r'"IOUserClientCreator" = "pid (\d+),', client)
        if creator:
            used = sum(int(n) for n in re.findall(r'"accumulatedGPUTime"=(\d+)', client))
            pid = int(creator.group(1))
            totals[pid] = totals.get(pid, 0) + used
    return totals


class CpuWindow:
    """CPU and GPU time of the app, its VideoToolbox services and WindowServer.

    VideoToolbox runs each client's hardware encode/decode sessions in XPC
    service processes of its own, so the services started after the app
    launched are the app's (`vt_before` lists those running before). Older
    ones serve other processes (the aerial wallpaper, other engines) and are
    reported separately as machine-wide VT CPU.
    """

    def __init__(self, app_pids, vt_before):
        self.app = list(app_pids)
        self.vt_all = vt_services()
        self.vt_app = [pid for pid in self.vt_all if pid not in vt_before]
        self.vt0 = bench.cpu_times(self.vt_all)
        self.ws = named("WindowServer")
        self.ws0 = bench.cpu_times(self.ws)
        self.gpu0 = gpu_ns()
        self.app0 = bench.cpu_times(self.app)
        self.instructions0 = bench.instruction_counts(self.app)
        self.t0 = time.time()

    def finish(self, app_pids):
        app1 = bench.cpu_times(app_pids)
        vt1 = bench.cpu_times(self.vt_all)
        ws1 = bench.cpu_times(self.ws)
        gpu1 = gpu_ns()
        instructions1 = bench.instruction_counts(app_pids)
        self.t1 = time.time()
        span = self.t1 - self.t0
        # Child processes that started during the window count from zero.
        app = sum(t - self.app0.get(pid, 0.0) for pid, t in app1.items())
        vt_app = sum(vt1[pid] - self.vt0[pid] for pid in self.vt_app if pid in vt1 and pid in self.vt0)
        vt_all = bench.cpu_delta(self.vt0, vt1)
        ws = bench.cpu_delta(self.ws0, ws1)

        def gpu(pids):
            return round(sum(gpu1[p] - self.gpu0.get(p, 0) for p in pids if p in gpu1) / 1e6 / span, 1)

        return {
            "seconds": round(span, 2),
            "app_cpu": round(100 * app / span, 1),
            "app_million_instructions_s": bench.instruction_rate(self.instructions0, instructions1, span),
            "vt_cpu": round(100 * vt_app / span, 1),
            "vt_services": len(self.vt_app),
            "vt_all_cpu": round(100 * vt_all / span, 1) if vt_all is not None else None,
            "windowserver_cpu": round(100 * ws / span, 1) if ws is not None else None,
            "app_gpu_ms_per_s": gpu(app_pids),
            "vt_gpu_ms_per_s": gpu(self.vt_app),
            "windowserver_gpu_ms_per_s": gpu(self.ws),
        }


# powermetrics --------------------------------------------------------------


class Power:
    """Reads powermetrics' plist samples by time window (it needs root)."""

    def __init__(self, work, log_path=None, max_seconds=3600):
        self.proc = None
        self.offset = 0
        self.path = None
        if log_path:
            self.path = Path(log_path)
            if not self.path.exists():
                sys.exit(f"obs-bench: --powermetrics-log {self.path} doesn't exist")
            self.offset = self.path.stat().st_size
        elif subprocess.run(["sudo", "-n", "true"], capture_output=True).returncode == 0:
            self.path = work / f"powermetrics-{time.strftime('%Y%m%d-%H%M%S')}.plist"
            self.proc = subprocess.Popen(
                ["sudo", "-n", "/usr/bin/powermetrics", "-i", "1000", "-n", str(max_seconds), "-f", "plist",
                 "--samplers", "cpu_power,gpu_power,tasks", "--show-process-gpu",
                 "--show-process-energy", "-o", str(self.path)],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            time.sleep(2)
        if self.path is None:
            log("no powermetrics (sudo -n doesn't work and no --powermetrics-log): power and GPU are left out")

    def stop(self):
        if self.proc is not None:
            # sudo doesn't pass a signal from here on to powermetrics: it
            # kept running (into the unlinked log) until its sample count.
            for pid in bench.run(["pgrep", "-P", str(self.proc.pid)], check=False).stdout.split():
                subprocess.run(["sudo", "-n", "kill", "-TERM", pid], capture_output=True)
            with contextlib.suppress(subprocess.TimeoutExpired):
                self.proc.wait(timeout=10)
            with contextlib.suppress(OSError):
                self.path.unlink()

    def samples(self, t0, t1):
        """Samples taken wholly inside [t0, t1]; earlier ones are skipped."""
        if self.path is None or not self.path.exists():
            return []
        # Let the sample covering t1 be written.
        time.sleep(1.5)
        with open(self.path, "rb") as f:
            f.seek(self.offset)
            data = f.read()
        end = data.rfind(b"</plist>")
        if end < 0:
            return []
        end += len(b"</plist>")
        self.offset += end
        out = []
        for doc in data[:end].split(b"</plist>"):
            start = doc.find(b"<?xml")
            if start < 0:
                continue
            try:
                sample = plistlib.loads(doc[start:] + b"</plist>")
            except Exception:  # noqa: BLE001 - a damaged sample is skipped
                continue
            stamp = sample.get("timestamp")
            if stamp is None:
                continue
            ts = stamp.replace(tzinfo=timezone.utc).timestamp()
            began = ts - sample.get("elapsed_ns", 1e9) / 1e9
            # Timestamps have whole seconds: allow one second of slack.
            if began >= t0 - 1.0 and ts <= t1 + 1.0:
                out.append(sample)
        return out

    @staticmethod
    def summarize(samples, app_pids):
        if not samples:
            return None
        app_pids = set(app_pids)

        def mean(values):
            values = [v for v in values if v is not None]
            return round(statistics.mean(values), 1) if values else None

        proc = [s.get("processor", {}) for s in samples]
        gpu = [s.get("gpu", {}) for s in samples]
        app_energy = [sum(t.get("energy_impact_per_s", 0) for t in s.get("tasks", []) if t.get("pid") in app_pids)
                      for s in samples]
        return {
            "samples": len(samples),
            "combined_mw": mean(p.get("combined_power") for p in proc),
            "cpu_mw": mean(p.get("cpu_power") for p in proc),
            "gpu_mw": mean(p.get("gpu_power") for p in proc),
            "gpu_active_pct": mean(100 * (1 - g["idle_ratio"]) for g in gpu if "idle_ratio" in g),
            "app_energy_impact": mean(app_energy),
            "machine_cpu_ms_per_s": mean(s.get("all_tasks", {}).get("cputime_ms_per_s") for s in samples),
        }


# Media -------------------------------------------------------------------


def make_tone(clips):
    """Isocast's tone source as a WAV: 1 kHz at half scale for 100 ms each second."""
    path = clips / TONE_CLIP
    if not path.exists():
        bench.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i",
                   "aevalsrc=if(lt(mod(t\\,1)\\,0.1)\\,0.5*sin(2*PI*1000*t)\\,0)|"
                   "if(lt(mod(t\\,1)\\,0.1)\\,0.5*sin(2*PI*1000*t)\\,0):s=48000:d=10",
                   "-c:a", "pcm_s16le", "-y", path])
    return path


def repeats(path):
    """Frames that repeat the one before (an encoder that falls behind can
    keep the frame rate by sending the last picture again, as OBS does), and
    the frames compared."""
    out = bench.run(["ffmpeg", "-v", "error", "-hwaccel", "videotoolbox", "-i", path, "-map", "0:v:0",
                     "-vf", "scale=960:-2,signalstats,metadata=print:key=lavfi.signalstats.YDIF:file=-",
                     "-f", "null", "-"], check=False)
    ydif = [float(y) for y in re.findall(r"YDIF=([\d.]+)", out.stdout)][1:]
    return sum(1 for y in ydif if y < REPEAT_YDIF), len(ydif) + 1 if ydif else 0


def probe(path):
    """What ffprobe finds in a file, decoding every video frame."""
    started = time.time()
    out = bench.run(["ffprobe", "-v", "error", "-count_frames", "-select_streams", "v:0",
                     "-show_entries",
                     "stream=codec_name,profile,width,height,pix_fmt,avg_frame_rate,nb_read_frames,bit_rate,"
                     "duration,color_space,color_primaries,color_transfer,color_range,has_b_frames:"
                     "frame=pict_type:format=duration",
                     "-of", "json", path], check=False)
    try:
        data = json.loads(out.stdout or "{}")
    except ValueError:
        data = {}
    stream = (data.get("streams") or [{}])[0]
    types = [f.get("pict_type") for f in data.get("frames", [])]
    packets = bench.run(["ffprobe", "-v", "error", "-select_streams", "v:0", "-show_entries",
                         "packet=pts_time,flags", "-of", "csv=p=0", path], check=False).stdout
    keys = []
    for line in packets.splitlines():
        parts = line.split(",")
        if len(parts) >= 2 and "K" in parts[1]:
            with contextlib.suppress(ValueError):
                keys.append(float(parts[0]))
    keys.sort()
    gaps = [b - a for a, b in zip(keys, keys[1:])]
    audio = bench.run(["ffprobe", "-v", "error", "-select_streams", "a:0", "-show_entries",
                       "stream=codec_name,sample_rate,channels,bit_rate,duration", "-of", "json", path],
                      check=False)
    try:
        a = (json.loads(audio.stdout or "{}").get("streams") or [{}])[0]
    except ValueError:
        a = {}
    # FLV has no stream durations.
    duration = float(stream.get("duration") or (data.get("format") or {}).get("duration") or 0)
    decoded = int(stream.get("nb_read_frames") or 0)
    repeated, compared = repeats(path)
    size = path.stat().st_size if path.exists() else 0
    return {
        "file": path.name,
        "codec": stream.get("codec_name"),
        "profile": stream.get("profile"),
        "size": f"{stream.get('width')}x{stream.get('height')}",
        "pix_fmt": stream.get("pix_fmt"),
        "color": "/".join(str(stream.get(k)) for k in ("color_primaries", "color_transfer",
                                                        "color_space", "color_range")),
        "avg_frame_rate": stream.get("avg_frame_rate"),
        "video_kbps": round(int(stream.get("bit_rate") or 0) / 1000),
        "duration": round(duration, 3),
        "decoded_frames": decoded,
        "repeated_frames": repeated,
        "distinct_frames": decoded - repeated,
        "compared_frames": compared,
        "expected_frames": round(duration * FPS),
        "complete": bool(duration) and decoded - repeated >= 0.98 * duration * FPS,
        "b_frames": types.count("B"),
        "keyframe_gap_s": round(statistics.median(gaps), 3) if gaps else None,
        "audio": f"{a.get('codec_name')} {a.get('sample_rate')} Hz {a.get('channels')} ch "
                 f"{round(int(a.get('bit_rate') or 0) / 1000)} kb/s" if a else None,
        "bytes": size,
        "mb_per_min": round(size / 1e6 / duration * 60, 1) if duration else None,
        "probe_seconds": round(time.time() - started, 1),
    }


def start_listener(path):
    """A local RTMP server (ffmpeg) that keeps what it receives."""
    proc = subprocess.Popen(
        ["ffmpeg", "-hide_banner", "-loglevel", "error", "-listen", "1", "-i",
         f"rtmp://127.0.0.1:{bench.STREAM_PORT}/live/bench", "-c", "copy", "-f", "flv", "-y", str(path)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(1)
    return proc


def stop_listener(proc):
    if proc is None:
        return
    with contextlib.suppress(subprocess.TimeoutExpired):
        proc.wait(timeout=5)  # it ends when the stream does
    if proc.poll() is None:
        proc.send_signal(signal.SIGINT)
        with contextlib.suppress(subprocess.TimeoutExpired):
            proc.wait(timeout=10)
    if proc.poll() is None:
        proc.kill()
        proc.wait()


# Isocast -----------------------------------------------------------------


class Isocast:
    name = "isocast"

    def __init__(self, args):
        self.args = args
        self.cli = args.cli
        self.dev_app = args.work / "Isocast Dev.app"
        self.sock = f"/tmp/isocast-obsbench-{hashlib.sha1(str(args.work).encode()).hexdigest()[:8]}.sock"
        self.env = dict(os.environ, ISOCAST_DEV_APP=str(self.dev_app), ISOCAST_SOCKET=self.sock,
                        ISOCAST_SETUP=str(args.work / "isocast-live-setup.toml"),
                        ISOCAST_MONITOR_SILENT="1", ISOCAST_STREAM_KEY="bench")
        self.pid = None
        bench.run([REPO / "scripts/sign-dev.sh", args.daemon], env=self.env)
        self.version = bench.run([self.cli, "--version"], env=self.env, check=False).stdout.strip()

    def rpc(self, method, params=None, timeout=120):
        request = {"jsonrpc": "2.0", "id": 1, "method": method}
        if params is not None:
            request["params"] = params
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            s.settimeout(timeout)
            s.connect(self.sock)
            s.sendall((json.dumps(request) + "\n").encode())
            data = b""
            while not data.endswith(b"\n"):
                chunk = s.recv(1 << 16)
                if not chunk:
                    break
                data += chunk
        reply = json.loads(data)
        if "error" in reply:
            raise BenchError(f"isocastd {method}: {reply['error']}")
        return reply.get("result")

    def shutdown(self):
        with contextlib.suppress(OSError, ValueError, BenchError):
            self.rpc("engine", {"running": False}, timeout=20)
        with contextlib.suppress(OSError, ValueError, BenchError):
            self.rpc("shutdown", timeout=10)
        deadline = time.time() + 15
        while time.time() < deadline and bench.pids_of(f"{self.dev_app}/Contents/MacOS/isocastd"):
            time.sleep(0.2)
        self.pid = None

    def launch(self, scenario, folder):
        self.shutdown()
        t0 = time.monotonic()
        live = json.loads(must([self.cli, "get", "--json"], env=self.env).stdout)
        launch = time.monotonic() - t0
        pids = bench.pids_of(f"{self.dev_app}/Contents/MacOS/isocastd")
        if not pids:
            raise BenchError("isocastd didn't start")
        self.pid = pids[0]
        setup = self.args.work / f"isocast-{scenario}.toml"
        setup.write_text(bench.setup_toml(scenario, False, self.args.work, folder))
        must([self.cli, "apply", "--revision", live["revision"], setup], env=self.env)
        must([self.cli, "engine", "start"], env=self.env)
        return launch

    def tree(self):
        return process_tree(self.pid) if self.pid else {}

    def pids(self):
        return list(self.tree())

    def counters(self):
        status = self.rpc("status")
        engine = status.get("engine") or {}
        return {"rendered": engine.get("program_frames"), "missed": engine.get("missed_frames"),
                "stream": status.get("stream")}

    def start_record(self, folder):
        t0 = time.monotonic()
        self.rpc("start", {"folder": str(folder)})
        return time.monotonic() - t0

    def stop_record(self):
        t0 = time.monotonic()
        report = self.rpc("stop", timeout=180)
        took = time.monotonic() - t0
        reported = sorted(report.get("files", []), key=lambda f: (f.get("source") or "program") != "program")
        files = [{"file": Path(f["path"]).name, "source": f.get("source") or "program",
                  "frames": f.get("frames"), "dropped": f.get("dropped_frames"),
                  "audio_dropouts": f.get("audio_dropouts"),
                  "duration": round(f.get("duration_seconds") or 0, 3)}
                 for f in reported]
        paths = [Path(report["folder"]) / Path(f["path"]).name for f in reported]
        return took, files, paths, {"warnings": report.get("warnings"), "errors": report.get("errors")}

    def start_stream(self):
        t0 = time.monotonic()
        self.rpc("stream_start")
        deadline = time.time() + 20
        while time.time() < deadline:
            stream = self.rpc("status").get("stream") or {}
            if stream.get("state") == "live":
                return time.monotonic() - t0
            if stream.get("state") == "failed":
                raise BenchError(f"isocast stream failed: {stream.get('last_error')}")
            time.sleep(0.02)
        raise BenchError("isocast stream didn't go live in 20 s")

    def stop_stream(self):
        t0 = time.monotonic()
        status = self.rpc("stream_stop", timeout=60)
        return time.monotonic() - t0, status

    def quit(self):
        self.shutdown()


# OBS ---------------------------------------------------------------------


class ObsSocket:
    """A minimal obs-websocket v5 client (protocol: obs-websocket's docs)."""

    def __init__(self, port, password, timeout):
        from websockets.sync.client import connect

        deadline = time.time() + timeout
        while True:
            try:
                self.ws = connect(f"ws://127.0.0.1:{port}", subprotocols=["obswebsocket.json"],
                                  open_timeout=2, max_size=None)
                break
            except Exception as err:  # noqa: BLE001 - refused or half-open while OBS starts
                if time.time() > deadline:
                    raise BenchError(f"obs-websocket didn't answer on port {port}: {err}") from err
                time.sleep(0.1)
        hello = json.loads(self.ws.recv(timeout=10))["d"]
        identify = {"rpcVersion": 1, "eventSubscriptions": 1 | 64}  # General, Outputs
        auth = hello.get("authentication")
        if auth:
            secret = base64.b64encode(hashlib.sha256((password + auth["salt"]).encode()).digest())
            identify["authentication"] = base64.b64encode(
                hashlib.sha256(secret + auth["challenge"].encode()).digest()).decode()
        self.ws.send(json.dumps({"op": 1, "d": identify}))
        reply = json.loads(self.ws.recv(timeout=10))
        if reply.get("op") != 2:
            raise BenchError(f"obs-websocket refused to identify: {reply}")
        self.events = []
        self.next_id = 0

    def _receive(self, timeout):
        message = json.loads(self.ws.recv(timeout=timeout))
        if message.get("op") == 5:
            self.events.append((time.monotonic(), message["d"]))
        return message

    def call(self, request, data=None, timeout=30):
        """A request's response data; waits while OBS is still starting (207)."""
        deadline = time.time() + timeout
        while True:
            self.next_id += 1
            rid = str(self.next_id)
            self.ws.send(json.dumps({"op": 6, "d": {"requestType": request, "requestId": rid,
                                                     "requestData": data or {}}}))
            while True:
                message = self._receive(max(0.1, deadline - time.time()))
                d = message.get("d", {})
                if message.get("op") == 7 and d.get("requestId") == rid:
                    break
            status = d.get("requestStatus", {})
            if status.get("result"):
                return d.get("responseData") or {}
            if status.get("code") == 207 and time.time() < deadline:
                time.sleep(0.05)
                continue
            raise BenchError(f"OBS {request} failed: {status.get('code')} {status.get('comment')}")

    def wait_event(self, kind, state, since, timeout=60):
        deadline = time.time() + timeout
        while True:
            for at, event in self.events:
                data = event.get("eventData", {})
                if at >= since and event.get("eventType") == kind and data.get("outputState") == state:
                    return at, data
            remaining = deadline - time.time()
            if remaining <= 0:
                raise BenchError(f"OBS sent no {kind} {state} in {timeout} s")
            with contextlib.suppress(TimeoutError):
                self._receive(remaining)

    def close(self):
        with contextlib.suppress(Exception):
            self.ws.close()


class Obs:
    def __init__(self, args, renderer):
        self.args = args
        self.renderer = renderer
        self.name = "obs" if renderer == "OpenGL" else "obs-metal"
        self.app = args.obs
        self.binary = self.app / "Contents/MacOS/OBS"
        if not self.binary.exists():
            raise BenchError(f"no OBS at {self.app} (--obs)")
        self.version = bench.run(["defaults", "read", self.app / "Contents/Info.plist",
                                  "CFBundleShortVersionString"], check=False).stdout.strip()
        # Its own home folder: OBS keeps everything in ~/Library/Application
        # Support/obs-studio, so the real settings, profiles, scene
        # collections and plugins are never read or changed.
        self.home = args.work / "obs-home"
        self.config = self.home / "Library/Application Support/obs-studio"
        self.proc = None
        self.ws = None

    def write_config(self, scenario, folder, password):
        cw, ch = canvas_of(scenario)
        major, minor, patch = (int(x) for x in (re.findall(r"\d+", self.version) + ["0", "0", "0"])[:3])
        config = self.config
        profile = config / "basic/profiles/Bench"
        for d in (profile, config / "basic/scenes", config / "plugin_config/obs-websocket"):
            d.mkdir(parents=True, exist_ok=True)
        os.chmod(self.home, 0o700)
        (config / "global.ini").write_text(f"""[General]
EnableAutoUpdates=false
MacOSPermissionsDialogLastShown=1
InfoIncrement=-1
LastVersion={(major << 24) | (minor << 16) | patch}
MaxLogs=50

[Video]
Renderer={self.renderer}
""")
        (config / "user.ini").write_text("""[General]
Pre19Defaults=false
Pre21Defaults=false
Pre23Defaults=false
Pre24.1Defaults=false
ConfirmOnExit=false
FirstRun=true

[BasicWindow]
PreviewEnabled=false
SysTrayEnabled=true
SysTrayWhenStarted=true

[Basic]
Profile=Bench
ProfileDir=Bench
SceneCollection=Bench
SceneCollectionFile=Bench.json
""")
        stream = scenario == "stream"
        (profile / "basic.ini").write_text(f"""[General]
Name=Bench

[Output]
Mode=Advanced
FilenameFormatting=obs-%CCYY-%MM-%DD %hh-%mm-%ss
Reconnect=false

[AdvOut]
ApplyServiceSettings=false
UseRescale=false
TrackIndex=1
Encoder=com.apple.videotoolbox.videoencoder.ave.avc
AudioEncoder=CoreAudio_AAC
RecType=Standard
RecFilePath={folder}
RecFormat2=hybrid_mov
RecUseRescale=false
RecTracks=1
RecEncoder=com.apple.videotoolbox.videoencoder.ave.hevc
RecAudioEncoder=CoreAudio_AAC
Track1Bitrate={STREAM_AUDIO_KBPS if stream else RECORD_AUDIO_KBPS}

[Video]
BaseCX={cw}
BaseCY={ch}
OutputCX={cw}
OutputCY={ch}
FPSType=1
FPSInt={FPS}
ScaleType=bicubic
ColorFormat=NV12
ColorSpace=709
ColorRange=Partial

[Audio]
SampleRate=48000
ChannelSetup=Stereo
MonitoringDeviceId=default
MonitoringDeviceName=Default
""")
        # Apple VT encoder settings, as OBS's Settings > Output writes them.
        (profile / "recordEncoder.json").write_text(json.dumps(
            {"rate_control": "CRF", "quality": 70, "keyint_sec": 1, "profile": "main", "bframes": True}))
        (profile / "streamEncoder.json").write_text(json.dumps(
            {"rate_control": "CBR", "bitrate": STREAM_VIDEO_KBPS, "keyint_sec": 2, "profile": "high",
             "bframes": False}))
        (profile / "service.json").write_text(json.dumps(
            {"type": "rtmp_custom", "settings": {"server": f"rtmp://127.0.0.1:{bench.STREAM_PORT}/live",
                                                 "key": "bench", "use_auth": False, "bwtest": False}}))
        # A new, empty scene collection each launch.
        for old in (config / "basic/scenes").glob("Bench.json*"):
            old.unlink()
        (config / "plugin_config/obs-websocket/config.json").write_text(json.dumps(
            {"alerts_enabled": False, "auth_required": True, "first_load": False, "server_enabled": True,
             "server_password": password, "server_port": OBS_PORT}))

    def launch(self, scenario, folder):
        if bench.run(["lsof", "-nP", f"-iTCP:{OBS_PORT}", "-sTCP:LISTEN"], check=False).stdout.strip():
            raise BenchError(f"something already listens on port {OBS_PORT} (another OBS?)")
        password = secrets.token_hex(16)
        self.write_config(scenario, folder, password)
        env = dict(os.environ, HOME=str(self.home), CFFIXED_USER_HOME=str(self.home))
        out = open(self.args.work / "obs-stdout.txt", "ab")
        t0 = time.monotonic()
        self.proc = subprocess.Popen(
            [self.binary, "--minimize-to-tray", "--disable-updater", "--disable-shutdown-check",
             "--disable-missing-files-check", "--multi", "--profile", "Bench", "--collection", "Bench"],
            env=env, stdout=out, stderr=subprocess.STDOUT, start_new_session=True)
        out.close()
        self.ws = ObsSocket(OBS_PORT, password, timeout=60)
        self.ws.call("GetVideoSettings", timeout=60)  # once OBS has loaded
        launch = time.monotonic() - t0
        self.build(scenario)
        return launch

    def build(self, scenario):
        ws = self.ws
        video = ws.call("GetVideoSettings")
        cw, ch = canvas_of(scenario)
        want = {"baseWidth": cw, "baseHeight": ch, "outputWidth": cw, "outputHeight": ch,
                "fpsNumerator": FPS, "fpsDenominator": 1}
        if any(video.get(k) != v for k, v in want.items()):
            raise BenchError(f"OBS video settings are {video}, not {want}")
        for category, name, value in (("AdvOut", "RecEncoder", "com.apple.videotoolbox.videoencoder.ave.hevc"),
                                      ("AdvOut", "RecFormat2", "hybrid_mov"),
                                      ("Output", "Mode", "Advanced")):
            got = ws.call("GetProfileParameter", {"parameterCategory": category, "parameterName": name})
            if got.get("parameterValue") != value:
                raise BenchError(f"OBS profile {category}.{name} is {got.get('parameterValue')!r}, not {value!r}")
        # Settings whose default is false aren't listed; hw_decode's use
        # shows in the VT decoder service's CPU and OBS's log.
        defaults = ws.call("GetInputDefaultSettings", {"inputKind": "ffmpeg_source"})["defaultInputSettings"]
        for key in ("looping", "restart_on_activate", "is_local_file"):
            if key not in defaults:
                raise BenchError(f"OBS's media source has no {key!r} setting (defaults: {sorted(defaults)})")
        scene = ws.call("GetCurrentProgramScene")["currentProgramSceneName"]
        clips = self.args.work / "clips"
        x, y, w, h = inset(cw, ch)
        placed = (("desktop", clips / bench.DESKTOP_CLIP, (0, 0, cw, ch)),
                  ("camera", clips / bench.CAMERA_CLIP, (x, y, w, h)),
                  ("tone", clips / TONE_CLIP, None))
        for name, path, rect in placed:
            item = ws.call("CreateInput", {
                "sceneName": scene, "inputName": name, "inputKind": "ffmpeg_source",
                "inputSettings": {"is_local_file": True, "local_file": str(path), "looping": True,
                                  "hw_decode": True, "close_when_inactive": False,
                                  "restart_on_activate": False},
                "sceneItemEnabled": True})["sceneItemId"]
            if rect:
                ws.call("SetSceneItemTransform", {"sceneName": scene, "sceneItemId": item, "sceneItemTransform": {
                    "positionX": rect[0], "positionY": rect[1], "alignment": 5,
                    "boundsType": "OBS_BOUNDS_STRETCH", "boundsAlignment": 0,
                    "boundsWidth": rect[2], "boundsHeight": rect[3]}})
        deadline = time.time() + 20
        while True:
            states = [ws.call("GetMediaInputStatus", {"inputName": n})["mediaState"] for n, _, _ in placed]
            if all(s == "OBS_MEDIA_STATE_PLAYING" for s in states):
                break
            if time.time() > deadline:
                raise BenchError(f"OBS media sources aren't playing: {states}")
            time.sleep(0.2)

    def tree(self):
        return process_tree(self.proc.pid) if self.proc else {}

    def pids(self):
        return list(self.tree())

    def counters(self):
        stats = self.ws.call("GetStats")
        out = {"rendered": stats.get("renderTotalFrames"), "missed": stats.get("renderSkippedFrames"),
               "output_frames": stats.get("outputTotalFrames"), "output_skipped": stats.get("outputSkippedFrames"),
               "obs_cpu": stats.get("cpuUsage"), "obs_memory_mb": stats.get("memoryUsage"),
               "render_ms": stats.get("averageFrameRenderTime")}
        if self.streaming:
            out["stream"] = self.ws.call("GetStreamStatus")
        return out

    streaming = False

    def start_record(self, folder):
        t0 = time.monotonic()
        self.ws.call("StartRecord")
        at, _ = self.ws.wait_event("RecordStateChanged", "OBS_WEBSOCKET_OUTPUT_STARTED", t0)
        return at - t0

    def stop_record(self):
        t0 = time.monotonic()
        self.ws.call("StopRecord")
        at, data = self.ws.wait_event("RecordStateChanged", "OBS_WEBSOCKET_OUTPUT_STOPPED", t0, timeout=180)
        path = Path(data.get("outputPath") or "")
        return at - t0, [], [path] if path.name else [], {}

    def start_stream(self):
        self.streaming = True
        t0 = time.monotonic()
        self.ws.call("StartStream")
        at, _ = self.ws.wait_event("StreamStateChanged", "OBS_WEBSOCKET_OUTPUT_STARTED", t0)
        return at - t0

    def stop_stream(self):
        status = self.ws.call("GetStreamStatus")
        t0 = time.monotonic()
        self.ws.call("StopStream")
        at, _ = self.ws.wait_event("StreamStateChanged", "OBS_WEBSOCKET_OUTPUT_STOPPED", t0)
        self.streaming = False
        return at - t0, status

    def save_log(self, folder):
        """Keep OBS's log of the run (its encoder and output settings) beside
        its findings; return what it says about frames."""
        logs = sorted((self.config / "logs").glob("*.txt"), key=lambda f: f.stat().st_mtime)
        if not logs:
            return None
        shutil.copy2(logs[-1], folder / "obs-log.txt")
        text = logs[-1].read_text(errors="replace")
        found = {}
        lag = re.search(r"skipped frames due to encoding lag: (\d+)/(\d+)", text)
        if lag:
            found["encoding_lag_skipped"] = int(lag.group(1))
            found["encoding_lag_of"] = int(lag.group(2))
        for key, pattern in (("frames_output", r"Total frames output: (\d+)"),
                             ("frames_drawn", r"Total drawn frames: (\d+)")):
            m = re.search(pattern, text)
            if m:
                found[key] = int(m.group(1))
        return found

    def quit(self):
        if self.ws is not None:
            self.ws.close()
            self.ws = None
        if self.proc is not None and self.proc.poll() is None:
            self.proc.terminate()  # OBS shuts down cleanly on SIGTERM
            try:
                self.proc.wait(timeout=20)
            except subprocess.TimeoutExpired:
                log("OBS didn't quit on SIGTERM in 20 s; killing it")
                self.proc.kill()
                self.proc.wait()
        self.proc = None
        self.streaming = False


# Runs --------------------------------------------------------------------


def one_run(args, app, scenario, index, power):
    with open(bench.LOCK, "a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            log("waiting for another benchmark to finish")
            fcntl.flock(lock, fcntl.LOCK_EX)
        listeners = []
        try:
            return measure(args, app, scenario, index, power, listeners)
        finally:
            # Also after a failure: never leave an app or a listener running
            # against the next benchmark once the lock is released.
            app.quit()
            for listener in listeners:
                if listener.poll() is None:
                    listener.kill()
                    listener.wait()


def measure(args, app, scenario, index, power, listeners):
    folder = args.work / "runs" / f"{time.strftime('%Y%m%d-%H%M%S')}-{app.name}-{scenario}-{index}"
    folder.mkdir(parents=True, exist_ok=True)
    result = {"app": app.name, "version": app.version, "scenario": scenario, "run": index,
              "mac": args.mac, "seconds": args.seconds, "when": time.strftime("%Y-%m-%d %H:%M:%S")}
    # The machine at rest, for power above idle (after the last run's
    # decoding has wound down).
    time.sleep(3)
    t0 = time.time()
    time.sleep(4)
    result["idle_power"] = Power.summarize(power.samples(t0, time.time()), [])
    vt_before = set(vt_services())
    result["launch_s"] = round(app.launch(scenario, folder), 3)
    time.sleep(args.settle)

    listener = None
    streaming = scenario == "stream"
    if streaming:
        listener = start_listener(folder / "stream.flv")
        listeners.append(listener)
        result["start_s"] = round(app.start_stream(), 3)
    else:
        result["start_s"] = round(app.start_record(folder), 3)
    time.sleep(2)

    pids = app.pids()
    before = app.counters()
    window = CpuWindow(pids, vt_before)
    time.sleep(args.seconds)
    tree = app.tree()
    pids_end = list(tree)
    cpu = window.finish(pids_end)
    after = app.counters()
    result.update(cpu)
    result["processes"] = sorted(set(tree.values()))
    result["rss_mb"] = round(rss_mb(pids_end))
    result["footprint_mb"] = round(footprint_mb(pids_end))
    result["power"] = Power.summarize(power.samples(window.t0, window.t1), pids_end)
    result["rendered"] = (after["rendered"] or 0) - (before["rendered"] or 0)
    result["render_missed"] = (after["missed"] or 0) - (before["missed"] or 0)
    for key in ("output_frames", "output_skipped"):
        if key in after:
            result[key] = (after[key] or 0) - (before[key] or 0)
    for key in ("obs_cpu", "obs_memory_mb", "render_ms"):
        if key in after:
            result[key] = round(after[key], 2) if after[key] is not None else None
    time.sleep(2)

    paths = []
    if streaming:
        took, status = app.stop_stream()
        result["stop_s"] = round(took, 3)
        stop_listener(listener)
        if isinstance(app, Obs):
            result["stream"] = {k: status.get(k) for k in ("outputSkippedFrames", "outputTotalFrames",
                                                           "outputBytes", "outputCongestion")}
        else:
            result["stream"] = {k: status.get(k) for k in ("frames_sent", "dropped_frames", "audio_dropouts",
                                                           "video_kbps", "audio_kbps", "bytes_sent", "congestion")}
        paths = [folder / "stream.flv"]
    else:
        took, files, paths, notes = app.stop_record()
        result["stop_s"] = round(took, 3)
        if files:
            result["session"] = files
        if notes:
            result["notes"] = notes
    app.quit()
    if isinstance(app, Obs):
        result["obs_log"] = app.save_log(folder)
    result["files"] = [probe(p) for p in paths if p.exists()]
    if not result["files"]:
        result["error"] = f"no files in {folder}"
    # Media goes; findings stay.
    runs = (args.work / "runs").resolve()
    for media in [] if args.keep_media else folder.iterdir():
        if media.suffix in (".mov", ".mp4", ".flv") and media.resolve().parent.parent == runs:
            media.unlink()
    with open(args.work / "results.jsonl", "a") as out:
        out.write(json.dumps(result) + "\n")
    return result


def warm_up(args, app):
    """Launch and quit once, unmeasured, so no measured launch is the first."""
    folder = args.work / "runs" / "warm-up"
    folder.mkdir(parents=True, exist_ok=True)
    with open(bench.LOCK, "a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        try:
            took = app.launch("program-1080", folder)
            log(f"{app.name} warm-up launch: {took:.2f} s")
        except BenchError as err:
            log(f"{app.name} warm-up failed: {err}")
        finally:
            app.quit()


def describe(r):
    files = ", ".join(f"{f['file'][:24]} {f['distinct_frames']}/{f['decoded_frames']}/{f['expected_frames']}f"
                      for f in r.get("files", []))
    power = r.get("power") or {}
    return (f"{r['app']} {r['scenario']} run {r['run'] + 1}: app {r['app_cpu']}% + VT {r['vt_cpu']}% CPU, "
            f"{r.get('app_million_instructions_s')} M instructions/s, "
            f"{r['footprint_mb']} MB, GPU {r['app_gpu_ms_per_s']} ms/s, {power.get('combined_mw')} mW, render {r['rendered']}/{r['render_missed']} missed, "
            f"launch {r['launch_s']} s, start {r['start_s']} s, stop {r['stop_s']} s; {files}")


# Summary -----------------------------------------------------------------


def med(values, digits=1):
    values = [v for v in values if v is not None]
    if not values:
        return "-"
    m = statistics.median(values)
    return f"{m:.{digits}f}"


def spread(values, digits=1):
    values = [v for v in values if v is not None]
    if not values:
        return "-"
    if len(values) == 1:
        return f"{values[0]:.{digits}f}"
    return f"{statistics.median(values):.{digits}f} ({min(values):.{digits}f}–{max(values):.{digits}f})"


def summary(results):
    if not results:
        print("no results")
        return
    macs = sorted({r.get("mac") or "?" for r in results})
    for mac in macs:
        rows = [r for r in results if (r.get("mac") or "?") == mac]
        versions = sorted({f"{r['app']} {r['version']}" for r in rows})
        print(f"\n### {mac} ({'; '.join(versions)})\n")
        print("| scenario | app | runs | app CPU % | app M instructions/s | VT CPU % | app+VT % | WindowServer % | memory MB | "
              "power mW (above idle) | GPU active % | app GPU ms/s | frames distinct/decoded/expected | "
              "skipped or dropped | launch s | start s | stop s | video kb/s |")
        print("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|")
        for scenario in SCENARIOS:
            for app in APPS:
                rs = [r for r in rows if r["scenario"] == scenario and r["app"] == app and "app_cpu" in r]
                if not rs:
                    continue
                total = [r["app_cpu"] + r["vt_cpu"] for r in rs if r.get("vt_cpu") is not None]
                power = [r["power"] for r in rs if r.get("power")]
                above = [p["combined_mw"] - r["idle_power"]["combined_mw"] for r in rs
                         if (p := r.get("power")) and r.get("idle_power")
                         and p.get("combined_mw") is not None and r["idle_power"].get("combined_mw") is not None]
                power_text = f"{med([p['combined_mw'] for p in power], 0)} ({med(above, 0)})" if power else "-"
                files = [f for r in rs for f in r.get("files", [])]
                decoded = sum(f["decoded_frames"] for f in files)
                distinct = sum(f.get("distinct_frames", f["decoded_frames"]) for f in files)
                expected = sum(f["expected_frames"] for f in files)
                skipped = 0
                for r in rs:
                    skipped += r.get("render_missed") or 0
                    skipped += r.get("output_skipped") or 0
                    skipped += sum(f.get("dropped") or 0 for f in r.get("session", []))
                    stream = r.get("stream") or {}
                    skipped += (stream.get("dropped_frames") or 0) + (stream.get("outputSkippedFrames") or 0)
                program = [f for r in rs for f in r.get("files", [])[:1]]
                print(f"| {scenario} | {app} | {len(rs)} | {spread([r['app_cpu'] for r in rs])} | "
                      f"{med([r.get('app_million_instructions_s') for r in rs])} | "
                      f"{spread([r.get('vt_cpu') for r in rs])} | {med(total)} | "
                      f"{med([r.get('windowserver_cpu') for r in rs])} | {med([r.get('footprint_mb') for r in rs], 0)} | "
                      f"{power_text} | {med([p.get('gpu_active_pct') for p in power])} | "
                      f"{med([r.get('app_gpu_ms_per_s') for r in rs])} | {distinct}/{decoded}/{expected} "
                      f"({len(files)} files) | {skipped} | {med([r['launch_s'] for r in rs], 2)} | "
                      f"{med([r['start_s'] for r in rs], 3)} | {med([r['stop_s'] for r in rs], 3)} | "
                      f"{med([f.get('video_kbps') for f in program], 0)} |")


# Main --------------------------------------------------------------------


def ensure_websockets(work):
    try:
        import websockets.sync.client  # noqa: F401
        return
    except ImportError:
        pass
    venv = work / "venv"
    python = venv / "bin/python"
    if Path(sys.prefix).resolve() == venv.resolve():
        sys.exit(f"obs-bench: {WEBSOCKETS} isn't importable in {venv}")
    if not python.exists():
        log(f"making {venv} with {WEBSOCKETS} (once)")
        subprocess.run([sys.executable, "-m", "venv", venv], check=True)
        subprocess.run([python, "-m", "pip", "install", "-q", WEBSOCKETS], check=True)
    os.execv(python, [str(python), *sys.argv])


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--work", type=Path, required=True, help="a folder of your own under $ISOCAST_SCRATCH")
    p.add_argument("--obs", type=Path, default=Path("/Applications/OBS.app"))
    p.add_argument("--daemon", type=Path, default=REPO / "target/release/isocastd")
    p.add_argument("--cli", type=Path, default=REPO / "target/release/isocast")
    p.add_argument("--apps", default=",".join(APPS), help=f"comma-separated, in turn each round: {', '.join(APPS)}")
    p.add_argument("--scenarios", default=",".join(SCENARIOS), help=f"comma-separated: {', '.join(SCENARIOS)}")
    p.add_argument("--runs", type=int, default=3, help="rounds; each runs every app on every scenario")
    p.add_argument("--seconds", type=int, default=20, help="the measured window")
    p.add_argument("--settle", type=float, default=4.0, help="seconds for sources to settle first")
    p.add_argument("--powermetrics-log", help="a powermetrics plist log being written by root, when sudo -n doesn't work")
    p.add_argument("--mac", default=None, help="a label for this Mac (default: its host name and chip)")
    p.add_argument("--no-warmup", action="store_true", help="skip the unmeasured first launch of each app")
    p.add_argument("--summary", action="store_true", help="print the table from <work>/results.jsonl and exit")
    p.add_argument("--keep-media", action="store_true", help="keep the recorded files (big: ~400 MB/min at 4K)")
    p.add_argument("--detach", action="store_true", help="run in the background, logging to <work>/log-<time>.txt")
    args = p.parse_args()
    args.work = args.work.resolve()
    if not str(args.work).startswith(str(bench.SCRATCH) + "/"):
        sys.exit(f"obs-bench: --work must be under {bench.SCRATCH}/ (ISOCAST_SCRATCH)")
    args.work.mkdir(parents=True, exist_ok=True)
    results_file = args.work / "results.jsonl"
    if args.summary:
        summary([json.loads(line) for line in results_file.read_text().splitlines() if line.strip()]
                if results_file.exists() else [])
        return
    if args.detach:
        out = args.work / f"log-{time.strftime('%Y%m%d-%H%M%S')}.txt"
        argv = [a for a in sys.argv if a != "--detach"]
        with open(out, "ab") as f:
            subprocess.Popen([sys.executable, *argv], stdout=f, stderr=subprocess.STDOUT,
                             stdin=subprocess.DEVNULL, start_new_session=True)
        print(f"obs-bench: running in the background; log: {out}")
        return
    ensure_websockets(args.work)
    apps = [a.strip() for a in args.apps.split(",") if a.strip()]
    scenarios = [s.strip() for s in args.scenarios.split(",") if s.strip()]
    for a in apps:
        if a not in APPS:
            sys.exit(f"obs-bench: --apps: unknown app {a!r}; valid: {', '.join(APPS)}")
    for s in scenarios:
        if s not in SCENARIOS:
            sys.exit(f"obs-bench: --scenarios: unknown scenario {s!r}; valid: {', '.join(SCENARIOS)}")
    if args.mac is None:
        chip = bench.run(["sysctl", "-n", "machdep.cpu.brand_string"], check=False).stdout.strip()
        osv = bench.run(["sw_vers", "-productVersion"], check=False).stdout.strip()
        args.mac = f"{socket.gethostname().split('.')[0]} ({chip}, macOS {osv})"
    args.daemon = args.daemon.resolve()
    args.cli = args.cli.resolve()
    clips = bench.make_clips(args.work)
    make_tone(clips)
    runners = {}
    for a in apps:
        if a == "isocast":
            runners[a] = Isocast(args)
        else:
            runners[a] = Obs(args, "OpenGL" if a == "obs" else "Metal")
    power = Power(args.work, args.powermetrics_log,
                  max_seconds=args.runs * len(scenarios) * len(apps) * 150 + 600)
    results = []
    try:
        if not args.no_warmup:
            for a in apps:
                warm_up(args, runners[a])
        for i in range(args.runs):
            for scenario in scenarios:
                for a in apps:
                    try:
                        r = one_run(args, runners[a], scenario, i, power)
                    except Exception as err:  # noqa: BLE001 - one failed run doesn't end the benchmark
                        log(f"{a} {scenario} run {i + 1} failed: {err}")
                        with open(results_file, "a") as out:
                            out.write(json.dumps({"app": a, "scenario": scenario, "run": i, "mac": args.mac,
                                                  "error": str(err)}) + "\n")
                        continue
                    log(describe(r))
                    results.append(r)
    finally:
        power.stop()
    summary(results)


if __name__ == "__main__":
    main()
