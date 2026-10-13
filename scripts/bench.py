#!/usr/bin/env python3
"""Measure isocastd on a fixed scenario, optionally A/B against another build.

    scripts/bench.py --work /Volumes/IsocastBuild/me/bench \
        --daemon target/release/isocastd [--compare /path/to/old/isocastd] \
        [--scenario flagship] [--runs 3] [--seconds 20] [--real] \
        [--plugins DIR] [--compare-plugins DIR] \
        [--tone-filters TOML] [--compare-tone-filters TOML]

Each run signs the daemon into its own copy of the development app
(`<work>/Isocast Dev.app`), starts it on its own socket and setup file (never
Ryan's), records (or streams) the scenario, and measures:

- isocastd CPU (% of one core, from its CPU time over the measured window),
  resident memory at the end;
- the machine's VideoToolbox encoder/decoder services' CPU over the same
  window (shared with anything else encoding, so noisier; n/a if a service
  starts, exits or resets during the sample);
- every file's frames, dropped frames and audio dropouts (from the session
  manifest), whether each file has the frames its rate promises, and MB per
  minute.

--plugins gives a build its own plugins folder (ISOCAST_PLUGINS, with plugins
installed by that build) and --tone-filters audio filters on the synthetic
tone (`'{ kind = "gain", gain_db = -6 }'`); the --compare-… forms are the
baseline's. --timeline opens a timeline (`timeline new`) on --daemon's runs
before recording, so each take is added to it, and checks it was.

With --compare, runs alternate between the two builds (A B A B …) so load
from the rest of the machine hits both alike; the summary gives medians.
Each run holds a machine-wide lock (/Volumes/IsocastBuild/.bench.lock), so
benchmarks started by different agents never measure at the same time (they
share the GPU, the hardware encoders and the devices); a run waits for it.
Results are appended to `<work>/results.jsonl`. Recorded media is deleted
after each run (manifests are kept); with --real it contains the screen.

Scenarios (synthetic sources unless --real):
  flagship       4K60 "desktop" + 1080p60 "camera", each recorded as its own
                 HEVC ISO file, plus the 4K60 program (desktop with the camera
                 in a corner) and audio. --real uses the main display,
                 "Studio Video", "Studio Audio" and desktop audio.
  flagship-1080  the same with a 1080p60 program.
  program-1080   the flagship scene on a 1080p60 canvas, only the program
                 recorded (no ISO files); scripts/obs-bench.py records the
                 same in OBS.
  program-4k     the same on the 4K60 canvas.
  flagship+stream the flagship recording plus a 1080p60 6000 kb/s local stream.
  engine         the flagship sources live, nothing recording.
  stream         a 1080p60 program streamed at 6000 kb/s to a local ffmpeg
                 RTMP listener (never a real service), nothing recording.
The synthetic desktop and camera are looping clips made once with ffmpeg
(busy test pattern; the camera's has sensor-like noise).
"""

import argparse
import contextlib
import ctypes
import fcntl
import hashlib
import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
# Scratch space: Ryan's build volume here; elsewhere (the test Mac) set
# ISOCAST_SCRATCH.
SCRATCH = Path(os.environ.get("ISOCAST_SCRATCH", "/Volumes/IsocastBuild"))
LOCK = SCRATCH / ".bench.lock"
DESKTOP_CLIP = "desktop-4k60.mov"
CAMERA_CLIP = "camera-1080p60.mov"
STREAM_PORT = 19400
# Scenarios that record files.
RECORDING = ("flagship", "flagship-1080", "flagship+stream", "program-1080", "program-4k")
SCENARIOS = RECORDING[:3] + ("engine", "stream") + RECORDING[3:]


def run(cmd, env=None, check=True, quiet=True, timeout=None):
    out = subprocess.run(
        [str(c) for c in cmd],
        env=env,
        capture_output=quiet,
        text=True,
        timeout=timeout,
    )
    if check and out.returncode != 0:
        sys.exit(f"bench: {' '.join(map(str, cmd))} failed:\n{out.stderr or out.stdout}")
    return out


def cpu_times(pids):
    """CPU seconds by PID; keep identities so exiting services don't subtract time."""
    if not pids:
        return {}
    out = run(["ps", "-o", "pid=,cputime=", "-p", ",".join(map(str, pids))], check=False)
    times = {}
    for line in out.stdout.splitlines():
        pid, elapsed = line.split()
        parts = [float(p) for p in elapsed.split(":")]
        while len(parts) < 3:
            parts.insert(0, 0.0)
        times[int(pid)] = parts[0] * 3600 + parts[1] * 60 + parts[2]
    return times


def cpu_seconds(pids):
    return sum(cpu_times(pids).values())


class _RusageInfoV4(ctypes.Structure):
    """Public macOS sys/resource.h layout, for proc_pid_rusage flavor 4."""

    _fields_ = [("uuid", ctypes.c_uint8 * 16)] + [(name, ctypes.c_uint64) for name in """
        user_time system_time pkg_idle_wkups interrupt_wkups pageins wired_size
        resident_size phys_footprint proc_start_abstime proc_exit_abstime
        child_user_time child_system_time child_pkg_idle_wkups child_interrupt_wkups
        child_pageins child_elapsed_abstime diskio_bytesread diskio_byteswritten
        cpu_time_qos_default cpu_time_qos_maintenance cpu_time_qos_background
        cpu_time_qos_utility cpu_time_qos_legacy cpu_time_qos_user_initiated
        cpu_time_qos_user_interactive billed_system_time serviced_system_time
        logical_writes lifetime_max_phys_footprint instructions cycles billed_energy
        serviced_energy interval_max_phys_footprint runnable_time
    """.split()]


_libproc = ctypes.CDLL("/usr/lib/libproc.dylib")
_libproc.proc_pid_rusage.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]
_libproc.proc_pid_rusage.restype = ctypes.c_int


def instruction_counts(pids):
    """Retired instructions by stable process identity.

    Unlike powermetrics' IPC sampler, these cumulative kernel counters work on
    the macOS 27 M2 where that sampler aborts with 'Second underflow occured.'.
    Missing/unsupported counters are unavailable, not a fictitious zero.
    """
    if not pids:
        return None
    counts = {}
    for pid in pids:
        usage = _RusageInfoV4()
        if _libproc.proc_pid_rusage(pid, 4, ctypes.byref(usage)) != 0 or not usage.instructions:
            return None
        counts[(pid, usage.proc_start_abstime)] = usage.instructions
    return counts


def instruction_rate(before, after, seconds):
    """Million instructions/s; don't compare partial or replaced process trees."""
    delta = cpu_delta(before, after) if before is not None and after is not None else None
    return round(delta / seconds / 1e6, 3) if delta is not None else None


def cpu_delta(before, after):
    """None when a service started/exited/reset during the measured window."""
    if before.keys() != after.keys() or any(after[p] < t for p, t in before.items()):
        return None
    return sum(after[p] - t for p, t in before.items())


def pids_of(pattern):
    out = run(["pgrep", "-f", pattern], check=False)
    return [int(p) for p in out.stdout.split()]


def rss_mb(pid):
    out = run(["ps", "-o", "rss=", "-p", str(pid)], check=False)
    return int(out.stdout.strip() or 0) / 1024


def make_clips(work):
    """Clip generation uses VT hardware too: reserve the same lock as a run."""
    clips = work / "clips"
    clips.mkdir(parents=True, exist_ok=True)
    jobs = {
        DESKTOP_CLIP: "testsrc2=size=3840x2160:rate=60",
        CAMERA_CLIP: "testsrc2=size=1920x1080:rate=60,noise=alls=14:allf=t+u",
    }
    with open(LOCK, "a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        # Recheck under the lock: another job may have prepared these while
        # this one waited. Never launch a new VT encoder beside a measured run.
        for name, graph in jobs.items():
            if (clips / name).exists():
                continue
            print(f"bench: making {name} (once)", file=sys.stderr)
            run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", graph,
                 "-t", "10", "-c:v", "hevc_videotoolbox", "-b:v", "60M", "-tag:v", "hvc1",
                 "-pix_fmt", "yuv420p", "-y", clips / name])
    return clips


def setup_toml(scenario, real, work, folder, camera="Studio Video", mic="Studio Audio",
               tone_filters=None):
    clips = work / "clips"
    big = scenario not in ("flagship-1080", "stream", "program-1080")
    cw, ch = (3840, 2160) if big else (1920, 1080)
    if real:
        sources = f"""
[[sources]]
id = "desktop"
kind = "display"
display = "main"

[[sources]]
id = "camera"
kind = "camera"
device = "{camera}"

[[sources]]
id = "mic"
kind = "microphone"
device = "{mic}"

[[sources]]
id = "desktop-audio"
kind = "system_audio"
"""
        audio = '["mic", "desktop-audio"]'
        iso_audio = '["mic"]'
    else:
        sources = f"""
[[sources]]
id = "desktop"
kind = "media"
files = ["{clips / DESKTOP_CLIP}"]
loop = true

[[sources]]
id = "camera"
kind = "media"
files = ["{clips / CAMERA_CLIP}"]
loop = true

[[sources]]
id = "tone"
kind = "tone"
"""
        if tone_filters:
            sources += f"audio_filters = [{tone_filters}]\n"
        audio = '["tone"]'
        iso_audio = '["tone"]'
    cam_w, cam_h = cw * 22 // 100, ch * 22 // 100
    program = scenario in RECORDING
    iso = program and not scenario.startswith("program-")
    text = f"""version = 1
name = "bench {scenario}"

[canvas]
width = {cw}
height = {ch}
fps = 60
{sources}
[[scenes]]
id = "main"
name = "Main"
items = [
  {{ source = "desktop", rect = {{ x = 0, y = 0, width = {cw}, height = {ch} }} }},
  {{ source = "camera", rect = {{ x = {cw - cam_w - 48}, y = {ch - cam_h - 48}, width = {cam_w}, height = {cam_h} }} }},
]

[recording]
folder = "{folder}"

[recording.program]
enabled = {"true" if program else "false"}
encoder = {{ codec = "hevc", quality = 70 }}
audio = {audio}

[recording.iso]
enabled = {"true" if iso else "false"}
sources = ["desktop", "camera"]
encoder = {{ codec = "hevc", quality = 70 }}
audio = {iso_audio}
"""
    if scenario in ("stream", "flagship+stream"):
        text += f"""
[stream]
service = "custom"
server = "rtmp://127.0.0.1:{STREAM_PORT}/live"
video_bitrate_kbps = 6000
width = 1920
height = 1080
"""
    return text


def one_run(args, daemon, label, index):
    with open(LOCK, "a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            print("bench: waiting for another benchmark to finish", file=sys.stderr)
            fcntl.flock(lock, fcntl.LOCK_EX)
        with contextlib.ExitStack() as cleanup:
            return measure(args, daemon, label, index, cleanup)


def measure(args, daemon, label, index, cleanup):
    work = args.work
    dev_app = work / "Isocast Dev.app"
    sock = f"/tmp/isocast-bench-{hashlib.sha1(str(work).encode()).hexdigest()[:8]}.sock"
    env = dict(os.environ, ISOCAST_DEV_APP=str(dev_app), ISOCAST_SOCKET=sock,
               ISOCAST_SETUP=str(work / "live-setup.toml"), ISOCAST_MONITOR_SILENT="1",
               ISOCAST_STREAM_KEY="bench")
    # Each build's own plugins folder and tone filters, if given.
    base = label == "base"
    plugins = args.compare_plugins if base else args.plugins
    if plugins:
        env["ISOCAST_PLUGINS"] = str(plugins.resolve())
    tone_filters = args.compare_tone_filters if base else args.tone_filters
    cli = args.cli
    listener = None

    def shutdown():
        # Also runs on a failed start: never leave an isolated engine or
        # listener competing with the next benchmark after releasing the lock.
        if listener is not None and listener.poll() is None:
            listener.terminate()
            listener.wait(timeout=10)
        subprocess.run(["nc", "-U", sock],
                       input='{"jsonrpc":"2.0","id":1,"method":"shutdown"}\n',
                       capture_output=True, text=True, timeout=10)

    cleanup.callback(shutdown)
    folder = work / "runs" / f"{time.strftime('%Y%m%d-%H%M%S')}-{label}-{index}"
    folder.mkdir(parents=True, exist_ok=True)
    setup = work / f"setup-{args.scenario}{'-real' if args.real else ''}-{label}.toml"
    setup.write_text(setup_toml(args.scenario, args.real, work, folder, args.camera, args.mic,
                                tone_filters))

    run([REPO / "scripts/sign-dev.sh", daemon], env=env)
    if run([cli, "restart"], env=env, check=False).returncode != 0:
        run([cli, "status"], env=env, check=False)
    pid = (pids_of(f"{dev_app}/Contents/MacOS/isocastd") or [None])[0]
    if pid is None:
        sys.exit("bench: isocastd didn't start")
    # Make the scenario the live setup and let its sources settle.
    rev = json.loads(run([cli, "get", "--json"], env=env).stdout)["revision"]
    run([cli, "apply", "--revision", rev, setup], env=env)
    run([cli, "engine", "start"], env=env)
    timeline = args.timeline and not base
    if timeline:
        # Kept beside the socket (/tmp/isocast-bench-….timelines).
        run([cli, "timeline", "new", f"bench {label} {index}"], env=env)
    time.sleep(args.settle)

    streaming = args.scenario in ("stream", "flagship+stream")
    recording = args.scenario in RECORDING
    if streaming:
        listener = subprocess.Popen(
            ["ffmpeg", "-hide_banner", "-loglevel", "error", "-listen", "1", "-i",
             f"rtmp://127.0.0.1:{STREAM_PORT}/live/bench", "-c", "copy", "-f", "null", "-"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        time.sleep(1)
        run([cli, "stream", "start"], env=env)
    if recording:
        run([cli, "start", "--seconds", args.seconds + 4, "--folder", folder], env=env)
    time.sleep(2)

    vt = pids_of("VTEncoderXPCService") + pids_of("VTDecoderXPCService")
    c0, v0 = cpu_seconds([pid]), cpu_times(vt)
    i0, t0 = instruction_counts([pid]), time.time()
    time.sleep(args.seconds)
    vt = pids_of("VTEncoderXPCService") + pids_of("VTDecoderXPCService")
    c1, v1 = cpu_seconds([pid]), cpu_times(vt)
    i1, t1 = instruction_counts([pid]), time.time()
    vt_delta = cpu_delta(v0, v1)
    memory = rss_mb(pid)
    # The compositor's own timings (CPU per tick, frames sent again), from
    # builds that report them (older daemons' read back as zeros).
    status = run([cli, "status", "--json"], env=env, check=False)
    try:
        compositor = (json.loads(status.stdout).get("engine") or {}).get("compositor")
    except ValueError:
        compositor = None
    if not (compositor or {}).get("cpu_us", {}).get("p50"):
        compositor = None
    stream = None
    if streaming:
        stream = json.loads(run([cli, "stream", "status", "--json"], env=env).stdout)
        run([cli, "stream", "stop"], env=env, check=False)
        listener.terminate()
        listener.wait(timeout=10)
    if recording:
        # Wait for the session to end: its manifest exists from the start
        # and says "recording" until the files are finished.
        deadline = time.time() + 60
        while time.time() < deadline:
            manifests = list(folder.glob("*.session.json"))
            try:
                if manifests and all(json.loads(m.read_text()).get("status") != "recording" for m in manifests):
                    break
            except (OSError, ValueError):
                pass  # being rewritten
            time.sleep(0.5)
    result = {
        "label": label, "run": index, "scenario": args.scenario, "real": args.real,
        "daemon": str(daemon), "seconds": round(t1 - t0, 2),
        "isocastd_cpu": round(100 * (c1 - c0) / (t1 - t0), 1),
        "isocastd_million_instructions_s": instruction_rate(i0, i1, t1 - t0),
        "vt_services_cpu": round(100 * vt_delta / (t1 - t0), 1) if vt_delta is not None else None,
        "rss_mb": round(memory), "files": [],
    }
    if timeline:
        shown = json.loads(run([cli, "timeline", "show", "--json"], env=env).stdout)
        result["timeline_clips"] = len(shown["clips"])
    if plugins:
        result["plugins"] = str(plugins)
    if tone_filters:
        result["tone_filters"] = tone_filters
    if compositor:
        result["compositor"] = compositor
    for manifest in folder.glob("*.session.json"):
        session = json.loads(manifest.read_text())
        for f in session.get("files", []):
            path = folder / f["path"]
            size = path.stat().st_size if path.exists() else 0
            duration = f.get("duration_seconds") or 0
            want = (f.get("fps") or 0) * duration
            result["files"].append({
                "source": f.get("source") or "program", "size": f"{f.get('width')}x{f.get('height')}@{f.get('fps')}",
                "frames": f.get("frames"), "dropped": f.get("dropped_frames"),
                "audio_dropouts": f.get("audio_dropouts"),
                "complete": bool(want) and (f.get("frames") or 0) >= 0.98 * want,
                "mb_per_min": round(size / 1e6 / duration * 60) if duration else None,
            })
    if stream is not None:
        result["stream"] = {k: stream.get(k) for k in (
            "state", "width", "height", "fps", "frames_sent", "dropped_frames",
            "audio_dropouts", "video_kbps", "audio_kbps", "total_kbps", "congestion",
            "bitrate_warning")
                            if k in stream}
    # Shut the daemon down so its devices and encoders are free for others.
    run([cli, "engine", "stop"], env=env, check=False)
    shutdown()
    cleanup.pop_all()
    # Media goes (it may show the screen); manifests stay.
    runs = (work / "runs").resolve()
    for media in folder.iterdir():
        if media.suffix in (".mov", ".mp4") and media.resolve().parent.parent == runs:
            media.unlink()
    with open(work / "results.jsonl", "a") as out:
        out.write(json.dumps(result) + "\n")
    return result


def summary(results):
    by = {}
    for r in results:
        by.setdefault(r["label"], []).append(r)
    print("\n| build | runs | isocastd CPU % (median, range) | isocastd M instructions/s | VT services CPU % | RSS MB | dropped frames | audio dropouts | files complete | compositor µs/frame p50 (p99) |")
    print("|---|---|---|---|---|---|---|---|---|---|")
    for label, rs in by.items():
        cpu = [r["isocastd_cpu"] for r in rs]
        instructions = [r["isocastd_million_instructions_s"] for r in rs
                        if r.get("isocastd_million_instructions_s") is not None]
        instruction_text = f"{statistics.median(instructions):.1f}" if instructions else "n/a"
        if instructions and len(instructions) != len(rs):
            instruction_text += f" ({len(instructions)}/{len(rs)} valid)"
        vt = [r["vt_services_cpu"] for r in rs if r["vt_services_cpu"] is not None]
        vt_text = f"{statistics.median(vt):.1f}" if vt else "n/a"
        if len(vt) != len(rs):
            vt_text += f" ({len(vt)}/{len(rs)} valid)"
        rss = [r["rss_mb"] for r in rs]
        dropped = sum((f["dropped"] or 0) for r in rs for f in r["files"])
        if any("stream" in r for r in rs):
            dropped += sum((r["stream"].get("dropped_frames") or 0) for r in rs if "stream" in r)
        dropouts = sum((f["audio_dropouts"] or 0) for r in rs for f in r["files"])
        dropouts += sum((r["stream"].get("audio_dropouts") or 0) for r in rs if "stream" in r)
        files = [f for r in rs for f in r["files"]]
        complete = f"{sum(f['complete'] for f in files)}/{len(files)}" if files else "-"
        timed = [r["compositor"]["cpu_us"] for r in rs if r.get("compositor")]
        timing = (f"{statistics.median(t['p50'] for t in timed):.1f} "
                  f"({statistics.median(t['p99'] for t in timed):.1f})") if timed else "n/a"
        print(f"| {label} | {len(rs)} | {statistics.median(cpu):.1f} ({min(cpu):.1f}–{max(cpu):.1f}) | "
              f"{instruction_text} | {vt_text} | {statistics.median(rss):.0f} | {dropped} | {dropouts} | {complete} | {timing} |")
    for label, rs in by.items():
        streams = [r["stream"] for r in rs if "stream" in r]
        if streams:
            print(f"  {label} stream: {streams[0]['width']}x{streams[0]['height']}@{streams[0]['fps']}, "
                  f"{statistics.median(s['frames_sent'] for s in streams):.0f} frames, "
                  f"{statistics.median(s['video_kbps'] for s in streams):.0f} kb/s video + "
                  f"{statistics.median(s['audio_kbps'] for s in streams):.0f} kb/s audio, "
                  f"{sum(s['dropped_frames'] for s in streams)} dropped, "
                  f"{sum(s.get('audio_dropouts', 0) for s in streams)} audio dropouts")
    for label, rs in by.items():
        timed = [r["timeline_clips"] for r in rs if "timeline_clips" in r]
        if timed:
            print(f"  {label} timeline: {sum(1 for n in timed if n == 1)}/{len(rs)} runs added "
                  f"their take to the open timeline")
    last = results[-1]
    for f in last["files"]:
        print(f"  {last['label']} {f['source']}: {f['size']}, {f['frames']} frames, {f['dropped']} dropped, "
              f"{f['audio_dropouts']} audio dropouts, {f['mb_per_min']} MB/min")


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--work", type=Path, required=True, help="a folder of your own on the APFS build volume")
    p.add_argument("--daemon", type=Path, default=REPO / "target/release/isocastd")
    p.add_argument("--compare", type=Path, help="another isocastd to alternate with (the baseline)")
    p.add_argument("--cli", type=Path, default=REPO / "target/release/isocast")
    p.add_argument("--scenario", default="flagship", choices=SCENARIOS)
    p.add_argument("--runs", type=int, default=3)
    p.add_argument("--seconds", type=int, default=20)
    p.add_argument("--settle", type=float, default=4.0, help="seconds for sources to settle first")
    p.add_argument("--real", action="store_true", help="real display, camera and mics instead of clips")
    p.add_argument("--camera", default="Studio Video", help="--real camera (test Macs: MacBook Pro Camera)")
    p.add_argument("--mic", default="Studio Audio", help="--real microphone (test Macs: MacBook Pro Microphone)")
    p.add_argument("--plugins", type=Path, help="plugins folder (ISOCAST_PLUGINS) for --daemon's runs")
    p.add_argument("--compare-plugins", type=Path, help="plugins folder for --compare's runs")
    p.add_argument("--tone-filters", help='audio filters on the synthetic tone, as TOML inline tables: '
                   '\'{ kind = "gain", gain_db = -6 }\'')
    p.add_argument("--compare-tone-filters", help="the same for --compare's runs")
    p.add_argument("--timeline", action="store_true",
                   help="open a timeline on --daemon's runs before recording (each take is added to it)")
    args = p.parse_args()
    args.work = args.work.resolve()
    if not str(args.work).startswith(str(SCRATCH) + "/"):
        sys.exit(f"bench: --work must be under {SCRATCH}/ (ISOCAST_SCRATCH)")
    args.work.mkdir(parents=True, exist_ok=True)
    if not args.real:
        make_clips(args.work)
    builds = [("new", args.daemon.resolve())]
    if args.compare:
        # Copied, so rebuilding doesn't change the baseline mid-comparison.
        base = args.work / "isocastd-baseline"
        shutil.copy2(args.compare, base)
        builds.insert(0, ("base", base))
        new = args.work / "isocastd-new"
        shutil.copy2(args.daemon, new)
        builds[1] = ("new", new)
    results = []
    for i in range(args.runs):
        for label, daemon in builds:
            r = one_run(args, daemon, label, i)
            files = ", ".join(f"{f['source']} {f['frames']}f/{f['dropped']}d" for f in r["files"])
            print(f"bench: {label} run {i + 1}: isocastd {r['isocastd_cpu']}%, VT {r['vt_services_cpu']}%, "
                  f"{r['rss_mb']} MB {files}{' stream ' + json.dumps(r['stream']) if 'stream' in r else ''}",
                  file=sys.stderr)
            results.append(r)
    summary(results)


if __name__ == "__main__":
    main()
