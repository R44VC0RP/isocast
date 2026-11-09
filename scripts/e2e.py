#!/usr/bin/env python3
"""Real-file engine checks; Python stdlib + ffmpeg/ffprobe only.

Build: cargo build --release -p isocast-daemon -p isocast-cli --bins --examples
Run:   python3 scripts/e2e.py [--only record] [--keep] [--json]

Each scenario gets a signed, background-only app, socket, setup, library,
plugins and recordings in its own temporary directory. No capture
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
runs (each output within 25% video loss), then allows the suite accounted
video losses up to 25% per output; all integrity, duration, audio, sync,
picture and API checks still apply. A zero-drop qualification does not keep
the zero-drop bar: GitHub's paravirtual GPU stalls (gpu_wait p99 up to
143 ms) drop a frame or more in some scenario of nearly every hosted run.
Ordinary local/test-Mac runs remain 1080p60 and require zero drops. --summary REPORT
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

Linux bootstrap: --only linux_record runs synthetic bars, tone and color at
720p30 with the same frame oracle and strict zero-drop/A/V checks. No signing,
capture hardware or media-file decoder is required. For hosted software Vulkan,
--hosted --linux-profile 360p30 uses the existing hosted loss/oracle policy;
strict 720p30 qualification remains a separate run without --hosted.
--only linux_media plays a looping H.264/AAC MP4 through GStreamer (needs
gstreamer1.0-libav) beside a test pattern: 60 s strict, 15 s with --hosted.

Windows media: --only win_media plays a looping H.264/AAC MP4 through Media
Foundation beside the test pattern (program and media ISO, frame oracle and
markers): strict at --windows-profile 180p30 for 18 s in hosted CI, 60 s from
720p up (1080p30 on a GPU machine).

Windows compiler CI: win_program_cut checks a five-second program recording
with a hard cut at 180p30: exact source and frame IDs, zero drops or repeats.
The full five-output win_record and cut/fade/slide win_scenes remain separate
qualification runs on aws-win and exon-desk, not hosted WARP capacity claims.

Linux capture: --only linux_capture records the real capture paths (the
ScreenCast portal over PipeWire, PipeWire desktop audio, a microphone and a
V4L2 camera) on the headless desktop of scripts/linux-capture-fixture.sh
(source its session.env first; without it the scenario is skipped).

Windows browser source: --only win_browser records a local page through
isocast-browser.exe (WebView2) next to the daemon, strictly (zero writer
drops); --windows-profile picks the canvas (1080p30 for a GPU machine, 270p30
or 180p30 for hosted WARP runners).
"""

import argparse
import array
import base64
import bisect
import copy
import contextlib
import datetime
try:
    import fcntl  # Unix scratch locking; Windows scenarios use isolated roots instead
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

# one scenario per line, so concurrent PRs don't conflict
SCENARIOS = (
    "setup",
    "record",
    "video_recovery",
    "iso",
    "split",
    "scenes",
    "stream",
    "webrtc",
    "nle",
    "data",
    "plugin",
    "hotkeys",
    "shutdown",
    "pause",
    "bug_report",
    "projects",
    "plugin_files",
    "replay",
    "vertical",
    "remote_files",
    # The frame oracle (#124), defined below.
    "oracle_record",
    "oracle_scenes",
    "oracle_takes",
    "oracle_masks",
    "oracle_canvases",
    "oracle_replay",
    "oracle_governor",
    # These Windows scenarios are listed on every platform.
    "win_record",
    "win_takes",
    "win_scenes",
    "win_recovery",
    "win_isolated",
    "win_machine_profile",
    "win_preview_recording",
    "win_app_audio",
    "win_audio_exclude",
    "win_monitor",
    "win_browser",
)
if sys.platform == "win32":
    SCENARIOS += (
        "win_program_cut",
        "win_idle_gpu_replacement",
        "win_adapter_preference",
        "win_media",
        "win_encoder_preflight",
        "win_preflight_no_decoder",
        "win_encoder_recovery",
        "win_capture_recovery",
        "win_take_device_recovery",
        "win_live_device_recovery",
        "win_output_rollover",
        "win_output_scale",
        "win_metadata",
        "win_vcam_publisher",
    )
elif sys.platform.startswith("linux"):
    SCENARIOS += (
        "linux_record",
        "linux_recovery",
        "linux_handoff",
        "linux_media",
        "linux_media_faults",
        "linux_capture",
        "linux_audio_exclude",
        "linux_browser",
        "linux_shared_frames",
    )

if sys.platform in ("darwin", "linux", "win32"):
    SCENARIOS += ("browser_audio_gap",)

OWN_CAPACITY = {"win32": {"win_preview_recording"}}


def scenarios_check_capacity(selected, system):
    return bool(selected) and set(selected) <= OWN_CAPACITY.get(system, set())


FPS = 60
RATE = 48000
HOSTED_DROP_LIMIT = 0.25
# Hosted only: held oracle pictures per output, late-starting frame slots per
# file, and accounted losses (dropped + held + late) over the whole suite.
HOSTED_HOLD_CAP = 3
# From every hosted run 2026-10-17..22 (3,300 files): 99.7% started on time,
# 0.3% 1-3 slots late, and one cold first MP4 writer 9 slots (0.3 s) late, so
# 12 slots (0.4 s at the hosted 30 fps) covers the VM with a margin while a
# file that misses half a second still fails.
HOSTED_LATE_START_CAP = 12
HOSTED_SUITE_LOSS_LIMIT = 0.03
# Hosted only: the webrtc scenario's take splits startup (the first 3 s) from
# the steady monitor window after it. On 3-vCPU hosted macOS VMs, paravirtual
# Metal and timer stalls (gpu_wait p99 up to 94 ms, nanosleep p99 90 ms) also
# hit mid-take (runs 41802988928, 42002149053, 42030479512, 42102286577: 3, 1,
# 5 and 11 frames at 5.7-6.2 s, alongside monitor-free scenarios in the same
# runs). Cap post-start losses at the same 12-slot (0.4 s) VM stall bound as
# HOSTED_LATE_START_CAP, still counted in check_take and the 3% suite budget;
# test Macs allow zero loss across the whole take.
HOSTED_MONITOR_TAKE_START_SECONDS = 3.0
HOSTED_MONITOR_TAKE_LATER_FRAMES = 12
# Hosted only: the vertical scenario's stream may drop this many frames, or
# this share of the frames it sent if more, counted in the suite's budget.
# The VM's GPU and CPU stalls cost it 1 and 4 frames of ~300 on 2026-10-23
# (runs 39949752801, 39955241917; 10-18 d3ca9bbd), while a test Mac drops none.
HOSTED_STREAM_DROP_FRAMES = 3
HOSTED_STREAM_DROP_RATIO = 0.02
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


def until(fn, seconds=30, description="condition", interval=0.05):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        result = fn()
        if result:
            return result
        time.sleep(interval)
    raise Failure(f"timed out after {seconds}s waiting for {description}")


def open_shared(path, *, share_delete=True):
    """By default share deletion with publishers, as Rust's File::open does.
    Use CreateFileW to preserve the native error when sharing is denied."""
    if not WINDOWS:
        return open(path, "rb")
    import ctypes
    import msvcrt
    from ctypes import wintypes
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.CreateFileW.argtypes = (wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD, wintypes.LPVOID,
                                     wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE)
    kernel32.CreateFileW.restype = wintypes.HANDLE
    kernel32.CloseHandle.argtypes = (wintypes.HANDLE,)
    generic_read, share_read_write, open_existing, normal = 0x80000000, 0x3, 3, 0x80
    sharing = share_read_write | (0x4 if share_delete else 0)
    handle = kernel32.CreateFileW(os.fspath(path), generic_read, sharing, None,
                                  open_existing, normal, None)
    if handle is None or handle == wintypes.HANDLE(-1).value:
        code = ctypes.get_last_error()
        # OSError maps the Windows error to FileNotFoundError/PermissionError.
        raise OSError(None, ctypes.FormatError(code), os.fspath(path), code)
    try:
        fd = msvcrt.open_osfhandle(handle, os.O_RDONLY | os.O_BINARY)
    except BaseException:
        kernel32.CloseHandle(handle)
        raise
    try:
        return os.fdopen(fd, "rb")  # owns fd, and through it the handle
    except BaseException:
        os.close(fd)
        raise


def read_json(path):
    with open_shared(path) as file:
        raw = file.read()
    return json.loads(raw)


def write_json(path, value):
    Path(path).write_text(json.dumps(value, indent=2) + "\n")


def capabilities():
    """Actually create an encoder, forbidding FFmpeg's software fallback."""
    result = {"system": platform.platform(), "machine": platform.machine()}
    if WINDOWS:
        # The engine encodes with NVENC or Media Foundation itself; whether
        # it can is what the scenarios find out. Matching-size streaming is
        # exercised by stream/vertical; scaled inputs and admission are separate.
        result.update(cores=os.cpu_count(), virtual_machine=0, gpus=[], hevc=True, h264=True,
                      record_codec="hevc", platform="windows",
                      pending=["previews (scene feeds, snapshots)",
                               "scaled streaming / recording budget integration", "replay buffer"])
        return result
    if LINUX:
        result.update(cores=os.cpu_count(), gpus=[], hevc=False, h264=False,
                      record_codec="h264", platform="linux", software_encoder="OpenH264")
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
LINUX = sys.platform.startswith("linux")
# --windows-profile: the hosted no-GPU reference canvas (#128: 180p30; 270p30
# held for win_media on one host but not for preview recording on all) and a
# GPU machine's strict 1080p30.
WINDOWS_PROFILES = {"180p30": (320, 180, 30), "270p30": (480, 270, 30), "1080p30": (1920, 1080, 30),
                    "2160p30": (3840, 2160, 30)}


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


class WindowsChild:
    """Retain the crash parent's daemon identity, even if its parent dies first."""

    def __init__(self, pid):
        import ctypes
        from ctypes import wintypes
        self.k32 = ctypes.WinDLL("kernel32", use_last_error=True)
        self.k32.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
        self.k32.OpenProcess.restype = wintypes.HANDLE
        self.k32.WaitForSingleObject.argtypes = [wintypes.HANDLE, wintypes.DWORD]
        self.k32.WaitForSingleObject.restype = wintypes.DWORD
        self.k32.GetExitCodeProcess.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
        self.k32.TerminateProcess.argtypes = [wintypes.HANDLE, wintypes.UINT]
        self.k32.CloseHandle.argtypes = [wintypes.HANDLE]
        self.handle = self.k32.OpenProcess(0x100000 | 0x1000 | 1, False, pid)  # synchronize/query/terminate
        require(self.handle, f"cannot retain test daemon {pid}: {ctypes.get_last_error()}")

    def wait(self, milliseconds):
        result = self.k32.WaitForSingleObject(self.handle, milliseconds)
        require(result in (0, 258), f"waiting for test daemon failed: {result}")
        return result == 0

    def exit_code(self):
        import ctypes
        from ctypes import wintypes
        code = wintypes.DWORD()
        require(self.k32.GetExitCodeProcess(self.handle, ctypes.byref(code)), "cannot read test daemon exit")
        return code.value

    def created(self):
        import ctypes
        from ctypes import wintypes
        self.k32.GetProcessTimes.argtypes = [wintypes.HANDLE, *([ctypes.POINTER(wintypes.FILETIME)] * 4)]
        times = [wintypes.FILETIME() for _ in range(4)]
        require(self.k32.GetProcessTimes(self.handle, *(ctypes.byref(t) for t in times)),
                "cannot read owned process creation time")
        return (times[0].dwHighDateTime << 32) | times[0].dwLowDateTime

    def close(self):
        if self.handle:
            try:
                if not self.wait(0):
                    require(self.k32.TerminateProcess(self.handle, 1), "could not terminate owned test daemon")
                    require(self.wait(10000), "owned test daemon survived termination")
            finally:
                self.k32.CloseHandle(self.handle)
                self.handle = None


class Engine:
    def __init__(self, root, daemon):
        self.root = root
        self.webrtc_receiver = Path(daemon).parent / "examples" / ("webrtc-receive.exe" if WINDOWS else "webrtc-receive")
        self.sock = root / "d.sock"
        if WINDOWS or LINUX:
            # No app bundle or signing; Windows uses a pipe, Linux a socket.
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
        if LINUX:
            self.env["XDG_CONFIG_HOME"] = str(root / "config")
        self.env.update(ISOCAST_CALIBRATION="off", ISOCAST_SOCKET=str(self.sock), ISOCAST_SETUP=str(root / "setup.toml"),
                        ISOCAST_PLUGINS=str(root / "plugins"),
                        ISOCAST_MONITOR_SILENT="1",
                        ISOCAST_STREAM_KEY="e2e", ISOCAST_LOG_DIR=str(root / "logs"),
                        # Never the real report service: bug_report's submit fails cleanly.
                        ISOCAST_REPORT_ENDPOINT="http://127.0.0.1:9/reports")
        if self.app:
            self.env["ISOCAST_DEV_APP"] = str(self.app)
        self.log = open(root / "daemon.log", "ab", buffering=0)
        self.process = None
        self.child = None
        self.start()

    def start(self, crash_parent=False, launcher=()):
        # Its own folder as the working directory: anything it (or a tool it
        # starts) writes to a relative path stays in the scenario's folder.
        self.crash_parent = crash_parent or bool(launcher)
        argv = [*launcher, str(self.binary)]
        if crash_parent:
            # A real parent/child tree for the Windows crash test, without
            # requiring a capture device or manufacturing a dummy child.
            require(WINDOWS, "crash parent is Windows-only")
            argv = [sys.executable, "-c", "import subprocess,sys; sys.exit(subprocess.call(sys.argv[1:]))", *argv]
        self.process = subprocess.Popen(argv, env=self.env, stdout=self.log, stderr=self.log,
                                        cwd=str(self.root))

        def ready():
            require(self.process.poll() is None, "daemon exited at startup; see daemon.log")
            try:
                return self.rpc("status")
            except (OSError, RpcError):
                return None
        status = until(ready, description="isolated daemon")
        if self.crash_parent:
            self.child = WindowsChild(status["daemon"]["pid"])

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
                    try:
                        manifest = read_json(path)
                    except (PermissionError, FileNotFoundError):
                        # A Windows metadata transaction retains its DELETE
                        # handle briefly after publishing the new name.
                        continue
                    if {f["path"] for f in manifest["files"]} == files:
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
        try:
            if self.process is None or self.process.poll() is not None:
                return
            try:
                self.rpc("shutdown", timeout=15)
                self.process.wait(timeout=15)
            except (OSError, ValueError, Failure, subprocess.TimeoutExpired):
                if self.crash_parent:
                    command(["taskkill", "/PID", str(self.process.pid), "/T", "/F"], check=False)
                else:
                    self.process.kill()
                self.process.wait(timeout=5)
        finally:
            if self.child:
                self.child.close()
                self.child = None

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


def split_slots(first, second, fps):
    """Frame slots in a split's first take: the engine clock from its start
    to the second take's (both manifests' host_time_ns). A file's own counts
    leave out a late start and include any frame it ran past the split."""
    nanos = second["host_time_ns"] - first["host_time_ns"]
    slots = round(nanos * fps / 1e9)
    require(slots > 0 and abs(nanos - slots * 1e9 / fps) < 1e3,
            f"the split is {nanos} ns after the first take, not on a {fps} fps frame boundary")
    return slots


def check_split_end(path, manifest, slots, fps):
    """Nothing in a split's first take runs past the split: every file ends
    by `slots` frames from the take's start, and so does the take. `slots`
    is engine-clock time; a pause in the take cuts its frames out of the
    files' timeline, so the bound shrinks by them."""
    slots -= sum(round((p["end_seconds"] - p["start_seconds"]) * fps)
                 for p in manifest.get("pauses", []) if p.get("end_seconds") is not None)
    for f in manifest["files"]:
        end = round(((f.get("start_offset_seconds") or 0) + f["duration_seconds"]) * fps)
        require(end <= slots, f"{path.name}: {f['source']} ends {end - slots} frame(s) after the split "
                              f"(offset {f.get('start_offset_seconds') or 0:.4f} s + {f['duration_seconds']:.4f} s "
                              f"against {slots} frames)")
    require(round(manifest["duration_seconds"] * fps) <= slots,
            f"{path.name}: the take lasts {manifest['duration_seconds']:.4f} s, past its split at {slots} frames")


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


def sync_check(path, fps=FPS, lost=None):
    """Every interior flash of the band has its beep within one frame, a
    beep a second. `lost` (hosted runs): frame slots counted as accounted
    loss (held pictures); the file's dropped slots are added here."""
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
    end = values[-1][0] + 1 / fps
    if lost is not None:
        shown = {round(t * fps) for t, _ in values}
        lost = set(lost) | (set(range(min(shown), max(shown) + 1)) - shown)
    result = pulse_check(path.name, flashes, beeps, end, fps, lost)
    return {"flashes": len(flashes), "beeps": len(beeps), **result}


def pulse_check(name, flashes, beeps, end, fps, lost=None):
    """Flash onsets and beep onsets (seconds) of a file `end` seconds long.
    Strict (`lost` None): every interior flash pairs with an interior beep
    within one frame. Hosted (`lost`: the slots that are dropped or held):
    a flash whose slots were lost may be late (on the first slot shown) or
    gone (`skipped`), and any other flash still needs its beep within one
    frame; every beep is still required, a second apart."""
    # Exclude only pulses cut by either edge of the file, not missing interior pulses.
    # Put the margin between frame ticks: a flash at exactly 0.15 s and
    # its sine beep's first nonzero sample one audio sample later must not
    # land on opposite sides of the filter. Keep this off-grid at any fps.
    edge = (math.floor(0.15 * fps) + 0.5) / fps
    f = [t for t in flashes if edge < t < end - edge]
    b = [t for t in beeps if edge < t < end - edge]
    near = 1 / fps + 1 / RATE
    if lost is None:
        require(len(f) >= int(end) - 1 and len(f) == len(b),
                f"{name}: missing pulses ({len(f)} flashes / {len(b)} beeps in {end:.3f}s)")
        offsets = [abs(x - y) for x, y in zip(f, b)]
        require(all(d <= near for d in offsets),
                f"{name}: beep/flash drift {max(offsets, default=0) * 1000:.2f}ms exceeds one frame")
        require(all(abs(y - x - 1) <= near for x, y in zip(b, b[1:])),
                f"{name}: beep cadence is not one second")
        return {"max_sync_ms": round(max(offsets, default=0) * 1000, 3)}
    require(len(b) >= int(end) - 1, f"{name}: missing pulses ({len(b)} beeps in {end:.3f}s)")
    require(all(abs(y - x - 1) <= near for x, y in zip(b, b[1:])), f"{name}: beep cadence is not one second")
    paired, offsets, late, skipped = set(), [], [], []
    for beep in b:
        # Edge flashes count too: a flash one frame late can cross the edge.
        match = [x for x in flashes if abs(x - beep) <= near]
        require(len(match) <= 1, f"{name}: {len(match)} flashes within one frame of the beep at {beep:.4f} s")
        if match:
            paired.add(match[0])
            offsets.append(abs(match[0] - beep))
            continue
        slot = shown = round(beep * fps)
        while shown in lost:
            shown += 1
        require(shown > slot, f"{name}: no flash within one frame of the beep at {beep:.4f} s "
                              f"(slot {slot} was shown, not dropped or held)")
        # The flash's slots were lost: it shows from the first slot shown, or
        # it ended unseen.
        match = [x for x in flashes if slot / fps - 1e-6 <= x <= shown / fps + near]
        require(len(match) <= 1, f"{name}: {len(match)} flashes after the lost slots at {beep:.4f} s")
        if match:
            paired.add(match[0])
            late.append(round(beep, 4))
        else:
            skipped.append(round(beep, 4))
    stray = [x for x in f if x not in paired and not any(abs(x - y) <= near for y in beeps)]
    require(not stray, f"{name}: a flash at {stray[0]:.4f} s without its beep" if stray else "")
    return {"max_sync_ms": round(max(offsets, default=0) * 1000, 3), "late_flashes": late,
            "skipped_flashes": skipped}


BROWSER_PULSE_PERIOD = 26 / 27  # 28 + 8/9 frames at 30 fps: 9 pulses sample all 9 sub-frame phases


def browser_sync_check(path, fps=30, period=BROWSER_PULSE_PERIOD, lost=None, check_median=True, near=None):
    """Browser Take 3 A/V oracle (asynchronous ~60 Hz rAF + WebAudio captured at
    `fps`). Unlike engine-internal `test_pattern` + `tone` (shared clock), an
    asynchronous page's rAF paint can land in either adjacent `fps` slot, giving
    an inherent quantization span up to `1/fps + 2 ms` when the capture pacer is
    phase-aligned, while the signed median across phase-stepped pulses (`26/27 s`
    = `28 + 8/9` frames at 30 fps) isolates any constant A/V shift and must stay
    within half a frame (`0.5/fps`) on hardware and `windows-browser`. On hosted
    Linux (`gtk4-broadwayd` + `llvmpipe`), two free-running 30 Hz timers
    (`isocast-browser`'s `glib::timeout` snapshot loop and `isocastd`'s
    `IsoRunner`) plus 60 Hz `rAF` span `1/fps + 1/60 + 2 ms` (`52 ms` at 30 fps,
    still catching any >= 2-frame shift of 66.7 ms) without a median gate."""
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
    end = values[-1][0] + 1 / fps
    if lost is not None:
        shown = {round(t * fps) for t, _ in values}
        lost = set(lost) | (set(range(min(shown), max(shown) + 1)) - shown)
    result = browser_pulse_check(
        path.name, flashes, beeps, end, fps, period, lost, check_median=check_median, near=near
    )
    return {"flashes": len(flashes), "beeps": len(beeps), **result}


def browser_pulse_check(name, flashes, beeps, end, fps, period=BROWSER_PULSE_PERIOD, lost=None,
                        check_median=True, near=None):
    """Check browser flash/beep pulses (Take 3 only):
    - per-pulse bound `near`: defaults to `1/fps + 2 ms` (hardware and
      `windows-browser`), or `1/fps + 1/60 + 2 ms` (`52 ms` at 30 fps) on hosted
      Linux (two free-running 30 Hz timers + 60 Hz rAF);
    - median signed offset (`beep - flash`): `|median| <= 0.5/fps` when
      `check_median` is True (hardware and `windows-browser`);
    - strict (`lost` None): every interior pulse paired, no held/dropped slots;
    - hold-accounted (`lost` set): held/dropped slots accounted as in `pulse_check`."""
    edge = (math.floor(0.15 * fps) + 0.5) / fps
    if near is None:
        near = 1 / fps + 0.002
    half_frame = 0.5 / fps
    b = [t for t in beeps if edge < t < end - edge]
    expected_min = max(1, int((end - 2 * edge) / period))
    require(len(b) >= expected_min,
            f"{name}: missing pulses ({len(b)} beeps in {end:.3f}s, expected >= {expected_min})")
    require(all(abs((y - x) - period) <= near for x, y in zip(b, b[1:])),
            f"{name}: beep cadence is not {period:.4f}s")
    paired, signed, late, skipped = set(), [], [], []
    for beep in b:
        match = [x for x in flashes if abs(x - beep) <= near]
        require(len(match) <= 1, f"{name}: {len(match)} flashes near the beep at {beep:.4f} s")
        if match:
            paired.add(match[0])
            signed.append(beep - match[0])
            continue
        if lost is None:
            require(False, f"{name}: no flash within {near * 1000:.2f}ms of the beep at {beep:.4f} s")
        slot = round(beep * fps)
        if slot not in lost and math.ceil(beep * fps) in lost:
            slot = math.ceil(beep * fps)
        shown = slot
        while shown in lost:
            shown += 1
        require(shown > slot, f"{name}: no flash within {near * 1000:.2f}ms of the beep at {beep:.4f} s "
                              f"(slot {slot} was shown, not dropped or held)")
        after = [x for x in flashes if slot / fps - 1e-6 <= x <= shown / fps + near]
        require(len(after) <= 1, f"{name}: {len(after)} flashes after the lost slots at {beep:.4f} s")
        if after:
            paired.add(after[0])
            late.append(round(beep, 4))
        else:
            skipped.append(round(beep, 4))
    f_interior = [x for x in flashes if edge + near < x < end - edge - near]
    stray = [x for x in f_interior if x not in paired and not any(abs(x - y) <= near for y in beeps)]
    require(not stray, f"{name}: a flash at {stray[0]:.4f} s without its beep" if stray else "")
    require(len(signed) >= max(4, expected_min // 2),
            f"{name}: too few paired pulses ({len(signed)}) to measure median sync")
    max_ms = max(abs(d) for d in signed) * 1000
    median_ms = statistics.median(signed) * 1000
    require(max_ms <= near * 1000 + 1e-6,
            f"{name}: beep/flash offset {max_ms:.2f}ms exceeds bound ({near * 1000:.2f}ms)")
    if check_median:
        require(abs(median_ms) <= half_frame * 1000 + 1e-6,
                f"{name}: median beep/flash offset {median_ms:+.2f}ms exceeds half a frame ({half_frame * 1000:.2f}ms)")
    out = {"max_sync_ms": round(max_ms, 3), "median_sync_ms": round(median_ms, 3)}
    if lost is not None:
        out.update(late_flashes=late, skipped_flashes=skipped)
    return out


def head_slots(f):
    """A program file's slots before its first picture: the engine counts
    them in dropped_frames (#150), though the file itself starts after them.
    An ISO's start offset is a source not yet showing, not a loss."""
    if f.get("kind") != "program":
        return 0
    # Whole slots only, as the engine counts them: an extra canvas's first
    # picture can come half a slot after a take that began on the main
    # canvas's grid, with nothing lost.
    return math.floor((f.get("start_offset_seconds") or 0) * f["fps"] + 1e-3)


def drop_metrics(manifest):
    rows = []
    for f in manifest["files"]:
        if f.get("frames") is None or f.get("dropped_frames") is None:
            continue  # unfinished crash fragments have no final counters, not zero losses
        frames, dropped, repeated = (f.get(k, 0) for k in ("frames", "dropped_frames", "repeated_frames"))
        slots = frames + dropped - (repeated or 0)
        seconds = f.get("duration_seconds") or 0
        rows.append({"output": f["path"], "frames": frames, "dropped": dropped,
                     "repeated": repeated, "slots": slots, "head": head_slots(f), "ratio": dropped / slots if slots else None,
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
    # Hosted: a program's late head is bounded by HOSTED_LATE_START_CAP in
    # check_take, as before it counted as dropped (#150), so the ratio leaves
    # it out. Test Macs: strict, any head is a drop.
    head = row.get("head", 0) if limit else 0
    ratio = (row["dropped"] - head) / (row["slots"] - head) if row["slots"] > head else 0.0
    require(ratio <= limit,
            f"{row['output']}: dropped {row['dropped'] - head}/{row['slots'] - head} "
            f"({ratio:.1%}), limit {limit:.0%}")
    # Only the engine's single, explicitly counted tail repeat is permitted
    # when accounting for video loss. Zero-drop runs still prohibit repeats.
    require(row["repeated"] <= (1 if limit and row["dropped"] else 0),
            f"{row['output']}: unexpected repeated_frames={row['repeated']}")


def monitor_take_losses(manifest, startup, snapshot_seconds):
    """The webrtc monitor take's loss rule. Test Macs (`startup` None): no
    file may lose a frame. Hosted: `startup` is each source's dropped count
    in live status at `snapshot_seconds` (at least
    HOSTED_MONITOR_TAKE_START_SECONDS in); every file must be in `startup`,
    and later losses are bounded by HOSTED_MONITOR_TAKE_LATER_FRAMES while
    staying in check_take's hosted per-output and suite budgets."""
    if startup is None:
        for loss in drop_metrics(manifest):
            check_loss(loss, 0)
        return None
    missing = [f["source"] for f in manifest["files"]
               if f.get("dropped_frames") is not None and f["source"] not in startup]
    require(not missing, f"monitor take source(s) missing from the {snapshot_seconds:g} s snapshot: {missing}")
    later = {f["source"]: f["dropped_frames"] - startup[f["source"]]
             for f in manifest["files"] if f.get("dropped_frames") is not None}
    require(all(0 <= n <= HOSTED_MONITOR_TAKE_LATER_FRAMES for n in later.values()),
            f"monitor take lost more than {HOSTED_MONITOR_TAKE_LATER_FRAMES} frame(s) after its first "
            f"{snapshot_seconds:g} s with monitors running: {later}")
    return {"snapshot_seconds": snapshot_seconds, "during_start": startup, "after_start": later}


def vertical_stream_loss(final, fps, evidence):
    """The vertical stream's losses as a drops row (counted in the suite's
    budget), checked: no audio dropouts ever, and no dropped frames unless
    hosted, where up to max(HOSTED_STREAM_DROP_FRAMES, HOSTED_STREAM_DROP_RATIO
    of the frames sent) are accounted loss."""
    sent, dropped = final["frames_sent"], final["dropped_frames"]
    require(sent > 0, "vertical stream sent no video")
    allowed = (max(HOSTED_STREAM_DROP_FRAMES, math.floor(HOSTED_STREAM_DROP_RATIO * sent))
               if loss_limit(evidence) else 0)
    require(final["audio_dropouts"] == 0 and dropped <= allowed,
            f"vertical stream lost protected media ({dropped} frame(s) dropped, {allowed} allowed): {final}")
    slots = sent + dropped
    return {"output": "vertical stream", "frames": sent, "dropped": dropped, "slots": slots,
            "ratio": dropped / slots, "repeated": 0, "audio_dropouts": final["audio_dropouts"],
            "target_fps": fps, "delivered_fps": sent / final["elapsed_seconds"] if final.get("elapsed_seconds") else None}


def packets_unbroken(times, gap):
    """Packet times that are in order with no gap over `gap` (seconds)."""
    return len(times) > 2 and all(0 < b - a <= gap for a, b in zip(times, times[1:]))


def video_packets_accounted(times, fps, accounted):
    """Video packet times in order whose gaps miss at most `accounted` frames
    in all: a frame the engine dropped (and counted) leaves a gap in the
    received stream; any other gap is lost media and fails, hosted or not."""
    if len(times) <= 2 or not all(b > a for a, b in zip(times, times[1:])):
        return False
    # Each gap over a frame (and its 2 ms slack) misses at least one frame,
    # and a dropped slot's gap lies on the frame grid (as the stream
    # scenario's check): a jittered or late delivery isn't a counted drop.
    gaps = [(b - a) * fps for a, b in zip(times, times[1:]) if b - a > 1 / fps + 0.002]
    if any(abs(n - round(n)) > fps * 0.0011 for n in gaps):
        return False
    return sum(max(1, round(n) - 1) for n in gaps) <= accounted


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
        limit = loss_limit(evidence)
        check_loss(loss, limit)
        file = path.parent / f["path"]
        expect = duration if duration is not None else f["duration_seconds"]
        late = 0
        if limit:
            # Hosted: a file whose encoder started late covers the take
            # from its manifest's start_offset_seconds on; those missing
            # first slots count as accounted loss (test Macs: strict).
            late = round((f.get("start_offset_seconds") or 0) * f["fps"])
            require(late <= HOSTED_LATE_START_CAP,
                    f"{file.name}: started {late} frame(s) late, more than the hosted cap of "
                    f"{HOSTED_LATE_START_CAP}")
            if late:
                loss["late_start_slots"] = late
        # Every file's start, on every machine: how late programs begin (#150).
        loss["start_slots"] = round((f.get("start_offset_seconds") or 0) * f["fps"])
        if duration is not None and limit:
            take_slots = round(duration * f["fps"])
            require(late + loss["dropped"] - loss["head"] <= limit * take_slots,
                    f"{file.name}: started {late} frame(s) late and dropped {loss['dropped']} of {take_slots} "
                    f"slots, more than the hosted {limit:.0%}")
            expect = duration - late / f["fps"]
        row = check_file(file, expect, f["fps"], (f["width"], f["height"]),
                         dropped=loss["dropped"] - loss["head"], repeated=loss["repeated"])
        if late:
            row["late_start_slots"] = late
        require(row["frames"] == f["frames"], f"{file.name}: manifest/decoded frame counts disagree")
        if sync:
            row.update(sync_check(file, f["fps"], lost=set() if limit else None))
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
    require(manifest["video_recovery"] == {"incident_count": 0, "incidents": [], "history_limited": False},
            f"healthy recording reported a video interruption: {manifest['video_recovery']}")
    evidence["files"] = check_take(path, manifest, 5, sync=True, expected_sources=["program"], evidence=evidence)


def scenario_video_recovery(e, cfg, evidence):
    """Fail a running ISO capture; its sibling program must outlive it."""
    e.stop()
    e.env["ISOCAST_TEST_PATTERN_FAIL_AFTER"] = "bars=0"
    e.start()
    cfg = setup(e.root, codec=cfg["recording"]["program"]["encoder"]["codec"], iso=True,
                width=320, height=180, fps=30)
    # Keep deliberately missing slots separate from normal throughput evidence.
    cfg["recording"]["folder"] = str(e.root / "fault-recordings")
    cfg["recording"]["iso"]["sources"] = ["bars"]
    cfg["scenes"][0]["items"][0]["source"] = "bars2"
    e.apply(cfg)
    path = Path(e.rpc("start", {})["manifest"])
    live = lambda: e.rpc("status").get("live") or {}
    def writing():
        state = live()
        return state if len(state.get("files", [])) == 2 and all(f["frames"] >= 3 for f in state["files"]) else None
    before = until(writing, description="both real writers before injection")
    require(before["video_recovery"]["incident_count"] == 0, f"incident before developer mode: {before}")
    # The existing fault is gated by developer mode. Arm it after actual
    # media, rather than racing a fixed timeout against hosted encoder startup.
    e.rpc("debug_set", {"enabled": True})
    observed, = until(lambda: live().get("video_recovery", {}).get("incidents"),
                      description="injected ISO failure in live status")
    require((observed["scope"], observed["owner_id"], observed["fault_kind"], observed["state"],
             observed["attempts_started"]) == ("source", "bars", "capture_stopped", "ended_early", 0)
            and "injected capture failure" in observed["reason"], f"unexpected incident: {observed}")
    e.rpc("stop")
    manifest = until(lambda: finished(path), description="failed source manifest finalized")
    final, = manifest["video_recovery"]["incidents"]
    same = ("incident_id", "scope", "owner_id", "fault_kind", "state", "reason", "fps", "first_lost_slot", "attempts_started")
    require(manifest["status"] == "failed" and all(final[k] == observed[k] for k in same),
            f"live/manifest disagree: {observed} vs {final}")
    first, end = final["first_lost_slot"], final["end_slot_exclusive"]
    require(0 < first < end and end >= observed["end_slot_exclusive"] and final["lost_slots"] == end - first,
            f"invalid final slot interval: {final}")
    until(lambda: e.rpc("status")["state"] == "idle", description="last finished take visible to doctor")
    check, = [c for c in e.rpc("doctor", {})["checks"] if c["name"] == "recording video recovery"]
    require(check["status"] == "warning" and all(s in check["detail"] for s in
            ("last finished take", "bars", final["scope"], final["state"], final["fault_kind"],
             f"[{first}, {end})", final["reason"])),
            f"doctor did not identify the final source gap: {check}")
    evidence.update(live_incident=observed, final_incident=final, doctor=check)


def scenario_iso(e, cfg, evidence):
    cfg["recording"]["iso"]["enabled"] = True
    e.apply(cfg)
    path, manifest = take(e)
    evidence["files"] = check_take(path, manifest, 5, sync=True,
                                    expected_sources=["program", "bars", "bars2"], evidence=evidence)
    starts = [f["start_offset_seconds"] for f in manifest["files"]]
    # Hosted: check_take already capped and accounted late starts; offsets
    # are f32 (one slot at 30 fps is 0.033333334), so compare within 1 us.
    apart = HOSTED_LATE_START_CAP / cfg["canvas"]["fps"] + 1e-6 if loss_limit(evidence) else 1 / cfg["canvas"]["fps"]
    require(max(starts) - min(starts) <= apart, f"ISO start offsets disagree: {starts}")


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


def check_constrained_baseline_file(path, label):
    """The elementary stream is Constrained Baseline and fully decodes; OpenH264
    decodes it too when this ffmpeg was built with libopenh264."""
    stream = next(s for s in probe(path)["streams"] if s["codec_type"] == "video")
    require(stream.get("profile") == "Constrained Baseline",
            f"{label}: expected Constrained Baseline, ffprobe says {stream.get('profile')!r}")
    decoded = ffmpeg("-xerror", "-i", path, "-f", "null", "-", timeout=30)
    require(not decoded.stderr.strip(), f"{label}: FFmpeg decode failed: {decoded.stderr.decode()}")
    result = {"file": str(path), "profile": stream["profile"], "level": stream.get("level"),
              "frames": stream.get("nb_read_frames"), "ffmpeg_decode": True}
    decoders = ffmpeg("-decoders", check=False).stdout.decode(errors="replace")
    if re.search(r"^\s*V\S*\s+libopenh264\s", decoders, re.M):
        decoded = ffmpeg("-xerror", "-c:v", "libopenh264", "-i", path, "-f", "null", "-",
                         timeout=30, check=False)
        require(decoded.returncode == 0, f"{label}: OpenH264 decode failed: {decoded.stderr.decode()}")
        result["openh264_decode"] = True
    else:
        result["openh264_decode"] = "unavailable: ffmpeg has no libopenh264 decoder"
    return result


# ISOCAST_WEBRTC_H264_PROFILE offers of webrtc-receive -> (answer check, SPS
# check, ffprobe profile). Every mode offers packetization-mode=1 only; the
# engine prefers plain High (6400xx) and otherwise sends Constrained Baseline.
def _answered_cb(f):
    plid = f["profile_level_id"]
    return plid is not None and plid[:2] == "42" and int(plid[2:4], 16) & 0x40


def _sps_cb(s):
    return s["profile_idc"] == 66 and s["constraint_set1"] == 1


H264_VIEWER_MODES = {
    "constrained_baseline": (_answered_cb, _sps_cb, "Constrained Baseline"),
    # Constrained High alone isn't guaranteed by the platform encoders.
    "constrained_high_and_baseline": (_answered_cb, _sps_cb, "Constrained Baseline"),
    "plain_baseline": (lambda f: f["profile_level_id"] == "42001f", _sps_cb, "Constrained Baseline"),
    "baseline_no_profile": (lambda f: f["profile_level_id"] is None, _sps_cb, "Constrained Baseline"),
    # Plain High still wins when offered after Constrained Baseline.
    "constrained_baseline_and_high": (lambda f: (f["profile_level_id"] or "")[:2] == "64",
                                      lambda s: s["profile_idc"] == 100, "High"),
}


def start_h264_viewer(e, mode, output, seconds):
    return subprocess.Popen([str(e.webrtc_receiver), str(seconds), str(output)],
                            env=dict(e.env, ISOCAST_WEBRTC_H264_PROFILE=mode),
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)


def collect_h264_viewer(process, output, timeout=20):
    stdout, stderr = process.communicate(timeout=timeout)
    output.with_suffix(".receiver.jsonl").write_text(stdout)
    output.with_suffix(".receiver.log").write_text(stderr)
    return process.returncode, stdout, stderr


def check_h264_viewer(collected, mode, output, label):
    """The answer and every received SPS match the offered profile and the
    elementary stream fully decodes (with OpenH264 too for baseline)."""
    returncode, stdout, stderr = collected
    require(returncode == 0, f"{label} WebRTC receiver failed: {stderr}")
    result = json.loads(stdout.splitlines()[0])
    h264 = result["h264"]
    answer_ok, sps_ok, profile = H264_VIEWER_MODES[mode]
    require(result["video_frames"] > 30 and result["closed"],
            f"insufficient {label} WebRTC media: {result}")
    require(h264["offer_profile"] == mode and h264["answered"]
            and all(answer_ok(f) and f["packetization_mode"] == 1 for f in h264["answered"]),
            f"{label}: unexpected answer: {h264}")
    require(h264["sps"] and all(sps_ok(s) for s in h264["sps"]),
            f"{label}: received SPS does not match the offer: {h264}")
    if profile == "Constrained Baseline":
        result["file_check"] = check_constrained_baseline_file(output, f"WebRTC {label}")
    else:
        stream = next(s for s in probe(output)["streams"] if s["codec_type"] == "video")
        require(stream.get("profile") == profile, f"{label}: expected {profile}, got {stream}")
        decoded = ffmpeg("-xerror", "-i", output, "-f", "null", "-", timeout=30)
        require(not decoded.stderr.strip(), f"{label}: FFmpeg decode failed: {decoded.stderr.decode()}")
        result["file_check"] = {"file": str(output), "profile": stream["profile"], "ffmpeg_decode": True}
    return result


def check_webrtc_constrained_baseline(e, cli, evidence):
    """Baseline-family offers (as a decoder limited to OpenH264 makes) must be
    answered with a profile they offered and carry a matching, decodable SPS.
    Batches stay within the governor's 3 viewer slots."""
    evidence["h264_profile_viewers"] = {}
    modes = list(H264_VIEWER_MODES)
    for batch in (modes[:3], modes[3:]):
        viewers = []
        try:
            for mode in batch:
                output = e.root / f"viewer-{mode}.h264"
                viewers.append((mode, output, start_h264_viewer(e, mode, output, 5)))
            for mode, output, process in viewers:
                evidence["h264_profile_viewers"][mode] = check_h264_viewer(
                    collect_h264_viewer(process, output), mode, output, mode)
        finally:
            for _, _, process in viewers:
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=5)
    evidence["constrained_baseline_viewer"] = evidence["h264_profile_viewers"]["constrained_baseline"]
    # The frames API must expose the same choice; a stale CLI is a failure.
    output = e.root / "frames-constrained-baseline.h264"
    received = subprocess.run([cli, "frames", "--width", "640", "--fps", "30", "--seconds", "2",
                               "--profile", "constrained_baseline", "--json", "-o", str(output)],
                              env=e.env, capture_output=True, timeout=15)
    require(received.returncode == 0, f"constrained baseline frames failed: {received.stderr.decode()}")
    frames = json.loads(received.stdout)
    require(frames["frames"] > 15 and frames["ended"] is None,
            f"constrained baseline frames did not sustain frames: {frames}")
    frames["file_check"] = check_constrained_baseline_file(output, "frames constrained baseline")
    evidence["constrained_baseline_frames"] = frames


def scenario_webrtc(e, cfg, evidence):
    if not evidence["capabilities"]["h264"]:
        raise Skip("no hardware encoder on this runner (H.264 WebRTC)")
    if not e.webrtc_receiver.is_file():
        message = "build the webrtc-receive example alongside isocastd (--examples)"
        if evidence["capabilities"].get("hosted"):
            raise Failure(message)
        raise Skip(message)
    e.apply(cfg)
    for invalid in ("not SDP", " " * 65537):
        try:
            e.rpc("whep_offer", {"sdp": invalid})
            raise Failure("malformed SDP was accepted")
        except RpcError as error:
            require(error.error["code"] == -32602, "wrong SDP error code")
    cli = os.environ.get("ISOCAST_E2E_CLI")
    require(cli, "monitor ladder verification needs the isocast CLI (--cli)")
    evidence["monitor_ladder"] = []
    for width in (480, 640, 960):
        output = e.root / f"monitor-{width}.h264"
        received = subprocess.run([cli, "frames", "--width", str(width), "--fps", "30",
                                   "--seconds", "2", "--json", "-o", str(output)],
                                  env=e.env, capture_output=True, timeout=15)
        require(received.returncode == 0, f"monitor ladder failed: {received.stderr.decode()}")
        result = json.loads(received.stdout)
        target_width = min(cfg["canvas"]["width"], width)
        expected_width = max(2, target_width & ~1)
        expected_height = max(2, ((cfg["canvas"]["height"] * target_width
                                  + cfg["canvas"]["width"] // 2) // cfg["canvas"]["width"]) & ~1)
        require(result["frames"] > 15 and result["ended"] is None,
                f"monitor ladder {width} did not sustain frames: {result}")
        stream = next(s for s in probe(output)["streams"] if s["codec_type"] == "video")
        require((stream["width"], stream["height"]) == (expected_width, expected_height),
                f"monitor ladder encoded wrong dimensions: {stream}")
        decoded = ffmpeg("-xerror", "-i", output, "-f", "null", "-", timeout=30)
        require(not decoded.stderr.strip(), f"monitor ladder decode failed: {decoded.stderr.decode()}")
        evidence["monitor_ladder"].append(result)
    # Feedback is a transport test, not a protected-output pressure test.
    # A recording may legitimately evict the newest WHEP viewer before its
    # scheduled FIR; requiring three IDRs then mislabels shedding as broken
    # PLI/FIR. Keep the strict feedback/content checks on an idle engine.
    e.rpc("engine", {"running": True})
    evidence["viewers"] = []
    for count in (1, 3):
        viewers = []
        legacy = None
        try:
            # Exercise frames and WHEP requests for the same 960-wide key
            # together; both protocols must sustain decodable output.
            legacy = subprocess.Popen([cli, "frames", "--width", "960", "--fps", "30",
                "--seconds", "6", "--json", "-o", str(e.root / f"shared-monitor-{count}.h264")],
                env=e.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            for index in range(count):
                output = e.root / f"viewer-{count}-{index}.h264"
                viewers.append((subprocess.Popen([str(e.webrtc_receiver), "6", str(output)],
                    env=dict(e.env, ISOCAST_WEBRTC_FEEDBACK_TEST="1", ISOCAST_WEBRTC_AV_SYNC_TEST="1", ISOCAST_WEBRTC_SAME_CLOCK="1"), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True), output))
            for process, output in viewers:
                stdout, stderr = process.communicate(timeout=20)
                output.with_suffix(".receiver.jsonl").write_text(stdout)
                output.with_suffix(".receiver.log").write_text(stderr)
                require(process.returncode == 0, f"WebRTC receiver failed: {stderr}")
                measurements = [json.loads(line) for line in stdout.splitlines() if line.strip()]
                result = measurements[0]
                result["av_sync"] = next(m["av_sync"] for m in measurements if "av_sync" in m)
                result["sender_reports"] = next(m["av_sync_sender_reports"] for m in measurements if "av_sync_sender_reports" in m)
                require(result["video_frames"] > 30 and result["decoded_audio_frames"] > RATE,
                        f"insufficient WebRTC media: {result}")
                require(result["audio_peak"] > 0.001 and result["closed"],
                        f"program audio or session close failed: {result}")
                # VM timing isn't a latency qualification. Physical Mac runs
                # enforce the fast-input target; all runs verify SR/content sync.
                if not evidence["capabilities"].get("hosted") and result.get("audio_age_ms_p50") is not None:
                    require(result["audio_age_ms_p50"] < 60, f"monitor audio is still held: {result}")
                decoded = ffmpeg("-xerror", "-i", output, "-f", "null", "-", timeout=30)
                require(not decoded.stderr.strip(), f"WebRTC H.264 decode failed: {decoded.stderr.decode()}")
                evidence["viewers"].append(result)
            stdout, stderr = legacy.communicate(timeout=15)
            require(legacy.returncode == 0, f"concurrent frames client failed: {stderr}")
            result = json.loads(stdout)
            require(result["frames"] > 30 and result["ended"] is None,
                    f"frames client failed beside WHEP: {result}")
            evidence.setdefault("shared_frames_viewers", []).append(result)
        finally:
            if legacy is not None and legacy.poll() is None:
                legacy.kill()
                legacy.wait(timeout=5)
            for process, _ in viewers:
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=5)
    check_webrtc_constrained_baseline(e, cli, evidence)

    # The same shared monitor encoder/scaler must not damage recording.
    # Keep WHEP audio/video running next to the take, but do not require it
    # to survive: the governor may shed it to protect the recording. A frames
    # client keeps the same 960-wide encoder/scaler active throughout.
    output = e.root / "monitor-recording.h264"
    legacy = subprocess.Popen([cli, "frames", "--width", "960", "--fps", "30",
        "--seconds", "60", "--json", "-o", str(output)],
        env=e.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    recording_viewer = None
    try:
        viewer_output = e.root / "recording-viewer.h264"
        recording_env = dict(e.env)
        recording_env.pop("ISOCAST_WEBRTC_FEEDBACK_TEST", None)
        recording_env.pop("ISOCAST_WEBRTC_AV_SYNC_TEST", None)
        recording_viewer = subprocess.Popen([str(e.webrtc_receiver), "60", str(viewer_output)],
            env=recording_env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        until(lambda: output.is_file() and output.stat().st_size > 0
              and viewer_output.is_file() and viewer_output.stat().st_size > 0
              and e.rpc("status")["governor"]["remote_viewers"] == 1,
              seconds=5, description="WHEP media active before recording")
        # Hosted pressure may legitimately shed new viewers to protect this
        # zero-loss take. Require both new viewers on the native hardware run.
        hosted = evidence["capabilities"].get("hosted")
        profile_modes = (() if hosted else
                         ("constrained_baseline", "constrained_high_and_baseline"))
        started = e.rpc("start", {"seconds": 8})
        # Hosted: what the take lost while its recording started, so the
        # rest of the take (monitors still running) can be held to zero.
        startup_losses = snapshot_seconds = None
        if hosted:
            def after_start():
                live = e.rpc("status").get("live")
                return live if live and live["elapsed_seconds"] >= HOSTED_MONITOR_TAKE_START_SECONDS else None
            live = until(after_start, seconds=15, description="monitor take past its start window")
            startup_losses = {f["source"]: f["dropped_frames"] for f in live["files"]}
            snapshot_seconds = live["elapsed_seconds"]
        # Baseline-only and Constrained High + Baseline viewers joining while
        # the take runs must still get decodable Constrained Baseline.
        baseline_viewers = []
        try:
            for mode in profile_modes:
                baseline_output = e.root / f"recording-viewer-{mode}.h264"
                baseline_viewers.append((mode, baseline_output,
                                         start_h264_viewer(e, mode, baseline_output, 3)))
            collected = [collect_h264_viewer(process, baseline_output)
                         for _, baseline_output, process in baseline_viewers]
            require(not baseline_viewers or finished(started["manifest"]) is None,
                    "take ended before the baseline viewers finished; they did not overlap the recording")
            evidence["recording_h264_profile_viewers"] = {
                mode: check_h264_viewer(result, mode, baseline_output, f"{mode} while recording")
                for (mode, baseline_output, _), result in zip(baseline_viewers, collected)}
        finally:
            for _, _, process in baseline_viewers:
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=5)
        manifest = until(lambda: finished(started["manifest"]), description="monitor take finalization")
        # This protection take requires zero loss on a test Mac. Hosted, post-
        # start losses are bounded by HOSTED_MONITOR_TAKE_LATER_FRAMES (and
        # counted in the per-output and suite budgets); viewer eviction is
        # allowed instead of a failure.
        taken = monitor_take_losses(manifest, startup_losses, snapshot_seconds)
        if taken is not None:
            evidence["monitor_take_losses"] = taken
        evidence.setdefault("files", []).extend(check_take(Path(started["manifest"]), manifest, evidence=evidence))
    finally:
        for name, process in (("recording_viewer", recording_viewer), ("recording_monitor", legacy)):
            if process is None:
                continue
            # Lifetime is controlled by take completion, not a short client
            # timer which could expire during slow hosted startup.
            stopped = process.poll() is None
            if stopped:
                process.kill()
            stdout, stderr = process.communicate(timeout=5)
            text = e.root / f"{name}.jsonl"
            log = e.root / f"{name}.log"
            text.write_text(stdout)
            log.write_text(stderr)
            evidence[name] = {"exit_code": process.returncode, "stopped_by_harness": stopped,
                              "stdout": str(text), "stderr": str(log)}
    until(lambda: e.rpc("status")["state"] == "idle", description="idle recorder after monitor take")


    # A single offer must survive a real program-audio graph replacement.
    # Keep this separate from steady-state flash/beep checks: retapping can
    # legitimately omit a pulse while sources reopen.
    e.rpc("engine", {"running": True})
    viewer = subprocess.Popen([str(e.webrtc_receiver), "8"], env=e.env,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        until(lambda: e.rpc("status")["governor"]["remote_viewers"] == 1,
              seconds=5, description="viewer before audio source addition")
        time.sleep(1)
        changed = copy.deepcopy(cfg)
        changed["sources"].append({"id": "monitor-extra-tone", "kind": "tone", "audio": {"muted": True}})
        changed["recording"]["program"]["audio"].append("monitor-extra-tone")
        e.apply(changed)
        until(lambda: any(s["source"] == "monitor-extra-tone" and s["state"] == "running"
                          for s in e.rpc("status")["engine"]["sources"]),
              seconds=5, description="added monitor audio source running")
        time.sleep(2)
        require(e.rpc("status")["governor"]["remote_viewers"] == 1,
                "audio source addition disconnected the viewer")
        stdout, stderr = viewer.communicate(timeout=15)
        (e.root / "viewer-source-add.jsonl").write_text(stdout)
        (e.root / "viewer-source-add.log").write_text(stderr)
        require(viewer.returncode == 0, f"source-add viewer failed: {stderr}")
        result = json.loads(stdout)
        require(result["closed"] and result["decoded_audio_frames"] > 5 * RATE
                and result["video_frames"] > 90,
                f"viewer did not retain signaling and media across source addition: {result}")
        evidence["source_add_viewer"] = result
    finally:
        if viewer.poll() is None:
            viewer.kill()
            viewer.wait(timeout=5)
        e.apply(cfg)


def scenario_stream(e, cfg, evidence):
    width, height, fps = (cfg["canvas"][k] for k in ("width", "height", "fps"))
    # Linux streams with its software encoder (OpenH264, libxaac AAC).
    if not evidence["capabilities"]["h264"] and not LINUX:
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
    if not evidence["capabilities"]["h264"]:
        raise Skip("no hardware encoder on this runner (vertical H.264 streaming)")
    # Two HD programs and one HD stream, at a rate the base M2 can protect.
    # Strict on test Macs; hosted, the stream and recordings may lose what
    # the VM's stalls cost (vertical_stream_loss, check_take), never a gap in
    # the received stream.
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
            evidence.setdefault("drops", {})["vertical stream"] = vertical_stream_loss(final, fps, evidence)
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
        # Hosted, check_take below holds them to the suite's recording rule.
        require(loss_limit(evidence) or all(f["dropped_frames"] == 0 and f["audio_dropouts"] == 0
                                            for f in manifest["files"]),
                "dual-canvas recording lost protected media")
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
    # A frame the engine dropped (hosted: accounted) leaves a gap; any other
    # gap, and any audio gap, is media lost on the way and fails.
    accounted = evidence["drops"]["vertical stream"]["dropped"]
    times = [float(p["pts_time"]) for p in packets if p["codec_type"] == "video"]
    require(video_packets_accounted(times, fps, accounted),
            f"vertical stream has missing or non-monotonic video packets beyond its {accounted} dropped frame(s)")
    times = [float(p["pts_time"]) for p in packets if p["codec_type"] == "audio"]
    require(packets_unbroken(times, 0.023), "vertical stream has missing or non-monotonic audio packets")
    ffmpeg("-xerror", "-i", output, "-map", "0", "-f", "null", "-")


def scenario_nle(e, cfg, evidence):
    """take_export_nle (#104): a take as a Final Cut Pro, Premiere Pro,
    Resolve and OpenTimelineIO project, read back in each format: one video
    track per ISO source with the program's on top (the only one on), each
    the take's own file lined up from its manifest's start offset, every
    file's audio with its video (on only for the program), and the take's
    files untouched."""
    cfg["recording"]["iso"]["enabled"] = True
    e.apply(cfg)
    path, manifest = take(e, 3)
    evidence["files"] = check_take(path, manifest, 3, evidence=evidence)
    files = {f["source"]: f for f in manifest["files"]}
    program = files["program"]
    fps = program["fps"]
    frames = round(program["duration_seconds"] * fps)
    order = [f["source"] for f in manifest["files"] if f["source"] != "program"] + ["program"]
    hashes = {p: hashlib.sha256(p.read_bytes()).hexdigest() for p in files_of(path)}
    rows = []
    # Windows writes OpenTimelineIO only (no XML media URLs there yet).
    formats = [("otio", None)] if WINDOWS else [
        ("fcpxml", "final_cut"), ("fcpxml", "resolve"), ("xmeml", "premiere"), ("xmeml", "resolve"), ("otio", None)]
    for fmt, app in formats:
        params = {"take": str(path), "format": fmt, "folder": str(e.root / "nle")}
        if app:
            params["app"] = app
        result = e.rpc("take_export_nle", params)
        out = Path(result["path"])
        require(out.is_file() and out.suffix == {"fcpxml": ".fcpxml", "xmeml": ".xml", "otio": ".otio"}[fmt],
                f"{fmt}: no project file: {result}")
        require(Path(result["take"]) == path, f"{out.name}: names take {result['take']}, not {path}")
        if fmt != "otio":
            ET.parse(out)  # well-formed
        tracks, rate, audio = nle_tracks(fmt, out)
        require(rate == fps, f"{out.name}: sequence at {rate} fps, the program at {fps}")
        require([t["source"] for t in result["video_tracks"]] == order and len(tracks) == len(order),
                f"{out.name}: video tracks {[t['source'] for t in result['video_tracks']]}, want {order}")
        require(abs(result["duration_seconds"] - frames / fps) < 1e-6,
                f"{out.name}: {result['duration_seconds']} s, the program has {frames / fps}")
        for source, track in zip(order, tracks):
            f = files[source]
            require(len(track) == 1, f"{out.name}: {source} has {len(track)} items, want the take's one file")
            start, end, file, first, enabled, crop = track[0]
            require(Path(file).resolve() == (path.parent / f["path"]).resolve(),
                    f"{out.name}: {source} points at {file}, not the take's {f['path']}")
            require(enabled == (source == "program"), f"{out.name}: {source} enabled {enabled}")
            require(crop is None, f"{out.name}: {source} has a crop {crop}")
            # Lined up on the take's clock: the file's frame at sequence frame
            # `start` is the one on screen then, within a frame.
            offset = start / fps - first / f["fps"]
            require(abs(offset - f["start_offset_seconds"]) <= 1 / min(fps, f["fps"]) + 1e-6,
                    f"{out.name}: {source} placed at {offset:.4f} s, its manifest says "
                    f"{f['start_offset_seconds']:.4f} s")
            require(frames - 2 <= end <= frames, f"{out.name}: {source} covers {start}-{end} of {frames} frames")
        video_items = {(i[2], i[0]): i for t in tracks for i in t}
        program_file = tracks[-1][0][2]
        require(audio, f"{out.name}: no audio")
        for start, end, file, first, enabled, _ in audio:
            v = video_items.get((file, start))
            require(v is not None and v[3] == first and v[1] == end,
                    f"{out.name}: audio of {Path(file).name} at {start} isn't with its video")
            require(enabled == (file == program_file), f"{out.name}: audio of {Path(file).name} enabled {enabled}")
        rows.append({"format": fmt, "app": result["app"], "file": out.name,
                     "video_tracks": len(tracks), "audio_items": len(audio), "notes": result.get("notes", [])})
    # `path`: exactly that file, an earlier export replaced, anything else
    # only with `replace`.
    exact = e.root / "nle" / "exact.otio"
    e.rpc("take_export_nle", {"take": str(path), "format": "otio", "path": str(exact)})
    e.rpc("take_export_nle", {"take": str(path), "format": "otio", "path": str(exact)})
    other = e.root / "nle" / "other.otio"
    other.write_text("not ours")
    try:
        e.rpc("take_export_nle", {"take": str(path), "format": "otio", "path": str(other)})
        raise Failure("take_export_nle replaced a file that isn't an Isocast export")
    except RpcError as exc:
        require("replace" in exc.error["message"], f"replace refusal: {exc.error}")
    require(other.read_text() == "not ours", "a refused export changed the file")
    e.rpc("take_export_nle", {"take": str(path), "format": "otio", "path": str(other), "replace": True})
    require(read_json(other)["OTIO_SCHEMA"].startswith("Timeline"), "replace did not write the project")
    bad_params = [({"take": str(e.root / "nope.session.json"), "format": "otio"}, "take"),
                  ({"take": str(path), "format": "otio", "path": str(e.root / "nle" / "x.xml")}, "path")]
    for bad, field in bad_params:
        try:
            e.rpc("take_export_nle", bad)
            raise Failure(f"take_export_nle accepted {bad}")
        except RpcError as exc:
            require(field in exc.error["message"], f"take_export_nle {bad}: {exc.error}")
    require({p: hashlib.sha256(p.read_bytes()).hexdigest() for p in files_of(path)} == hashes,
            "an export changed the take's files")
    # A project folder with one take is that take.
    by_folder = e.rpc("take_export_nle", {"take": str(path.parent), "format": "otio",
                                          "path": str(e.root / "nle" / "folder.otio")})
    require(Path(by_folder["take"]) == path, f"a project folder as the take: {by_folder['take']}")
    # A take recorded without the program: the top ISO track is the one on.
    cfg["recording"]["program"]["enabled"] = False
    e.apply(cfg)
    isos, _ = take(e, 2)
    result = e.rpc("take_export_nle", {"take": str(isos), "format": "otio", "folder": str(e.root / "nle")})
    tracks, _, audio = nle_tracks("otio", Path(result["path"]))
    on = [t["source"] for t, items in zip(result["video_tracks"], tracks) if items and items[0][4]]
    require(on == [result["video_tracks"][-1]["source"]] and result["video_tracks"][-1]["source"] != "program",
            f"a take without the program: tracks on {on} of {[t['source'] for t in result['video_tracks']]}")
    require(not audio or any(item[4] for item in audio), "a take without the program: no audio on")
    evidence["nle"] = rows


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
    # A 0.0.8 profile with a binding of the retired editor (timeline.retake):
    # it loads disabled, and the first edit still works and drops it.
    old = copy.deepcopy(cfg)
    old["hotkeys"] = {"old-retake": {"keys": "ctrl+alt+r", "action": "timeline.retake", "args": {}}}
    e.apply(old)
    require("old-retake" in (e.rpc("hotkeys_get")["configured"] or {}), "the stale binding wasn't kept")
    e.rpc("hotkeys_set", {"patch": {"first": {"keys": "ctrl+alt+f11", "action": "recording.toggle"}}})
    after = e.rpc("hotkeys_get")
    require("first" in after["bindings"] and "old-retake" not in (after["configured"] or {}),
            f"a patch over a stale timeline binding: {after['configured']}")
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

    def __init__(self, engine, kinds, stop_after=None):
        import threading
        self.events = []
        self.stop_after = stop_after
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
                if message["params"]["type"] == self.stop_after:
                    break

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
    rename (refused while recording) keeping files and manifests resolving,
    flat recordings from before projects untouched, and what the retired
    editor left on disk (timelines, .isoprj files, exports) never read,
    changed or deleted."""
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
    # The retired editor's timelines folder (as 0.0.8 left it) is ignored
    # and kept as it is.
    old_timelines = e.root / "timelines"
    old_timelines.mkdir()
    write_json(old_timelines / "current.json", {"timeline": "e2e", "record": None})
    write_json(old_timelines / "e2e.timeline.json", {"version": 1, "id": "e2e", "name": "E2E", "clips": []})
    old_hashes = {str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in old_timelines.iterdir()}
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
    # An editor project file and export in the folder, as 0.0.8 wrote
    # them: they move with the folder, unchanged.
    leftovers = {"Untitled.isoprj": '{"format": "isocast-project", "version": 1}\n',
                 "Untitled.export.json": '{"status": "complete", "files": []}\n'}
    for name, text in leftovers.items():
        (folder / name).write_text(text)
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
    for name, text in leftovers.items():
        require((new / name).read_text() == text, f"{name} changed in a rename")
    # Collisions get a number, and bad names name the field.
    try:
        e.rpc("rename_project", {"project": project, "name": "a/b"})
        raise Failure("a name with / was accepted")
    except RpcError as exc:
        require("name" in exc.error["message"], f"bad name error: {exc.error}")
    # A rename behind Isocast's back (the Finder): list_projects finds it by
    # its project id, with the new name.
    finder = new.with_name(new.name.replace("My Talk", "Finder Name"))
    new.rename(finder)
    listed = [p for p in e.rpc("list_projects")["projects"] if p["id"] == project]
    require(len(listed) == 1 and Path(listed[0]["folder"]) == finder and listed[0]["name"] == "Finder Name"
            and all(Path(t).parent == finder for t in listed[0]["takes"]),
            f"list_projects after a Finder rename: {listed}")
    for name, text in leftovers.items():
        require((finder / name).read_text() == text, f"{name} changed after a Finder rename")
    evidence["finder_rename"] = str(finder)
    # Flat recordings stay where they were, unchanged, and a killed one is
    # still recovered at launch.
    require({str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in flat[1:]} == flat_hashes,
            "flat recordings changed")
    e.stop()
    e.start()
    interrupted = [Path(r["manifest"]) for r in e.rpc("status").get("interrupted", [])]
    require(flat_manifest in interrupted, f"flat killed take not recovered: {interrupted}")
    require(read_json(flat_manifest)["status"] == "interrupted", "flat manifest not marked")
    require({str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in old_timelines.iterdir()} == old_hashes,
            "the retired editor's timelines changed")
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


def scenario_remote_files(e, cfg, evidence):
    """The engine file browser (`files_list`) as a client on another
    computer sees it, over R1's TLS on loopback (a loopback TCP peer is a
    network caller, not local authority): hidden entries and a symlink out
    of the roots are left out, a root has no parent, and a big folder is
    cut at 2000 entries."""
    cli = os.environ.get("ISOCAST_E2E_CLI")
    if not cli or not Path(cli).is_file():
        raise Skip("no isocast CLI (ISOCAST_E2E_CLI) to pair and call over TLS")
    share, outside = e.root / "share", e.root / "outside"
    (share / "sub").mkdir(parents=True)
    (share / "big").mkdir()
    outside.mkdir()
    (share / "a.txt").write_text("a")
    (share / ".hidden").write_text("h")
    (outside / "secret.txt").write_text("s")
    if WINDOWS:
        # Hidden there is an attribute, not a leading dot.
        command(["attrib", "+h", str(share / ".hidden")])
    try:
        os.symlink(outside / "secret.txt", share / "link-out.txt")
        evidence["symlink"] = True
    except OSError as exc:  # Windows without the symlink privilege
        evidence["symlink"] = f"not made: {exc}"
    for i in range(2001):
        (share / "big" / f"f{i:04}.txt").write_bytes(b"")
    e.rpc("folder_roots_set", {"folders": [str(share)]})
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    try:
        e.rpc("remote_enable", {"bind": "127.0.0.1", "port": port})
    except RpcError as exc:
        # The engine's identity lives in the OS credential store.
        if "credential" in str(exc).lower() or "keychain" in str(exc).lower():
            raise Skip(f"no OS credential store for the engine's identity: {exc}") from exc
        raise
    env = dict(e.env, ISOCAST_REMOTE_CLIENT=str(e.root / "client.json"))
    address = f"127.0.0.1:{port}"
    try:
        code = e.rpc("remote_pair_begin")["code"]
        paired = subprocess.run([cli, "remote", "pair", address, "--code-stdin", "--name", "e2e", "--json"],
                                input=code + "\n", env=env, capture_output=True, text=True, timeout=60)
        if paired.returncode != 0 and "credential" in paired.stderr.lower():
            raise Skip(f"no OS credential store for the client's identity: {paired.stderr.strip()[-300:]}")
        require(paired.returncode == 0, f"pairing over loopback TLS failed: {paired.stderr.strip()[-400:]}")

        def listing(path):
            out = subprocess.run([cli, "files", "list", "--engine", address, str(path), "--json"],
                                 env=env, capture_output=True, text=True, timeout=60)
            require(out.returncode == 0, f"files list {path} over TLS failed: {out.stderr.strip()[-400:]}")
            return json.loads(out.stdout)

        root = listing(share)
        names = sorted(entry["name"] for entry in root["entries"])
        evidence["root_entries"] = names
        require(".hidden" not in names, "a hidden entry was listed for a network caller")
        require("link-out.txt" not in names, "a symlink out of the roots was listed for a network caller")
        require({"a.txt", "sub", "big"} <= set(names), f"expected a.txt, sub and big; got {names}")
        require(root.get("parent") is None,
                f"a root has a parent for a network caller: {root.get('parent')}")
        big = listing(share / "big")
        evidence["big"] = {"entries": len(big["entries"]), "truncated": big.get("truncated", False)}
        require(len(big["entries"]) == 2000 and big.get("truncated") is True,
                f"a 2001-entry folder gave {len(big['entries'])} entries, truncated={big.get('truncated')}")
        # The same root through the owner's local socket has a parent.
        local = e.rpc("files_list", {"path": str(share)})
        evidence["local_parent"] = local.get("parent")
        require(local.get("parent") is not None, "the local caller sees no parent above a root")
    finally:
        e.rpc("remote_disable")


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
    # Program ticks the compositor missed while it ran show in saved replays
    # too: counted, and the CLI shows the count (at most the engine's own).
    missed = status.get("program_missed_frames")
    require(isinstance(missed, int) and (missed == 0 or loss_limit(evidence)),
            f"replay status program_missed_frames: {status}")
    cli = os.environ.get("ISOCAST_E2E_CLI")
    if cli:
        shown = subprocess.run([cli, "replay", "status", "--json"], env=e.env, capture_output=True, timeout=15)
        require(shown.returncode == 0, f"isocast replay status --json failed: {shown.stderr.decode()}")
        shown = json.loads(shown.stdout)
        engine_missed = (e.rpc("status").get("engine") or {}).get("missed_frames", 0)
        require(isinstance(shown.get("program_missed_frames"), int)
                and missed <= shown["program_missed_frames"] <= engine_missed,
                f"replay status --json program_missed_frames {shown.get('program_missed_frames')} "
                f"(RPC {missed}, engine {engine_missed})")
    evidence["replay_program_missed_frames"] = missed
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
ORACLE_SCENARIOS = ("oracle_record", "oracle_scenes", "oracle_takes", "oracle_masks",
                    "oracle_canvases", "oracle_replay", "oracle_governor")
# The lanes' suite while they work (about 3 minutes on the VM, --hosted).
QUICK = ("oracle_record", "oracle_scenes", "oracle_takes", "oracle_masks")
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


def oracle_fixture(folder, tag, fps, seconds=ORACLE_SECONDS, mp4=False):
    """A clip of `seconds` whose frame n carries (tag, n), with a marker
    burst at each whole second. Made once per run and folder. `mp4`: an
    H.264/AAC MP4 instead of a MOV with PCM sound."""
    folder.mkdir(parents=True, exist_ok=True)
    path = folder / f"oracle-v2-{tag}-{fps}-{seconds}.{'mp4' if mp4 else 'mov'}"
    if path.exists():
        return path
    cols, rows = ORACLE_GRID
    stem = folder / f".oracle-v2-{tag}-{fps}-{seconds}"
    raw, sound, partial = (Path(f"{stem}.gray"), Path(f"{stem}.wav"), Path(f"{stem}.partial{path.suffix}"))
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
                     *codec, "-g", fps, *(["-c:a", "aac", "-b:a", "192k"] if mp4 else ["-c:a", "pcm_s16le"]),
                     "-y", partial, timeout=900, check=False)
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
        raise Pending("the block-code fixture scenarios aren't qualified on Windows yet; win_media checks a "
                      "looping media file and the win_* scenarios the test pattern against the same model")
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

    def __init__(self, label, path, root, evidence, hold_ok=None):
        self.label, self.path, self.root, self.evidence = label, Path(path), root, evidence
        self.checked, self.mismatches, self.pictures = 0, [], set()
        # Hosted runs (accounted losses allowed): a source frame that came
        # too late for its slot shows the region's previous picture again,
        # a held frame (the runner's wake jitter, p99 ~26 ms, passes half a
        # frame at 30 fps). A hold must show the previous picture's expected
        # frame of the same source or the one after it, and still before this
        # picture's (exactly that frame at 1x; a 1.5x source can show the one
        # in between; never a frame from across a pause), after a picture
        # that was right, so a run of holds is at
        # most one long; holds count in the output's loss budget (outputs
        # without a take's drop row count in the suite's). Test Macs, and
        # edited exports (made from files, not live), stay strict.
        self.hold_ok = loss_limit(evidence) > 0 if hold_ok is None else hold_ok
        self.held, self.previous = [], {}

    def expect(self, index, t, region, want, got, allowed=()):
        self.checked += 1
        self.pictures.add(index)
        previous = self.previous.get(region)
        right = got == want or got in allowed
        held = (not right and self.hold_ok and previous is not None
                and previous[0] == index - 1 and previous[2]           # right the picture before
                and isinstance(got, tuple) and isinstance(want, tuple)
                and got[:1] == ("id",) and got[:2] == want[:2]          # the same source
                and (got == previous[1]                                 # exactly its expected frame,
                     or (isinstance(previous[1], tuple) and previous[1][:2] == want[:2]
                         and previous[1][2] <= got[2] <= previous[1][2] + 1
                         and got[2] < want[2])))                        # or the next (a 1.5x source)
        self.previous[region] = (index, want, right)
        if right:
            return
        if held:
            self.held.append((index, t, region, want, got))
            return
        self.mismatches.append((index, t, region, want, got))

    def loss_row(self):
        """This output's row of the take's accounted losses, if it has one."""
        for key, row in self.evidence.get("drops", {}).items() if isinstance(self.evidence, dict) else ():
            if Path(key).resolve() == self.path.resolve():
                return row
        return None

    def verdict(self):
        row = {"output": self.path.name, "checks": self.checked, "mismatches": len(self.mismatches)}
        held = sorted({i for i, *_ in self.held})
        if self.held:
            row["held"] = [{"picture": i, "time": round(t, 4), "region": r, "expected": describe(w),
                            "actual": describe(g)} for i, t, r, w, g in self.held]
        self.evidence.setdefault("oracle", []).append(row)
        require(self.checked > 0, f"{self.label}: the oracle checked nothing")
        if held:
            loss = self.loss_row()
            dropped, slots = (loss["dropped"], loss["slots"]) if loss else (0, len(self.pictures))
            if loss:
                loss["held"] = loss.get("held", 0) + len(held)
            else:  # an output no take's drop row covers: its holds still count over the suite
                self.evidence.setdefault("unrowed_holds", []).append(
                    {"output": self.path.name, "held": len(held), "slots": slots})
            require(len(held) <= HOSTED_HOLD_CAP,
                    f"{self.label} ({self.path.name}): {len(held)} held pictures (at {held[:5]}), more than "
                    f"the hosted cap of {HOSTED_HOLD_CAP} per output")
            require(dropped + len(held) <= loss_limit(self.evidence) * max(slots, 1),
                    f"{self.label} ({self.path.name}): {dropped} dropped + {len(held)} held of {slots} slots, "
                    f"over the hosted {loss_limit(self.evidence):.0%}")
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
                not any(-0.005 <= o - c / fps <= 0.045 for c in cuts):
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
        # The fit counts from the program file's first picture; a program that
        # began after the take (start_offset_seconds) shows a later slot there.
        late = round((pf.get("start_offset_seconds") or 0) * fps)
        t0 = (fit_anchor(oracle_decode(program, oracle_regions(layout), fps), "a", count) - late) % count
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
            cuts = [at - offset for at, _ in pauses if at >= offset]
            oracle_markers(f"{label} {source}", file,
                           lambda j, o=offset: media_frame(t0, take_slot(j, o), 1, count), fps, evidence, cuts=cuts)
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
    program, pf = manifest_file(path, manifest, "program")
    full = [dict(r, name="picture") for r in oracle_regions([("a", (0, 0, 1, 1))])]
    decoded = oracle_decode(program, full, fps)
    limit = loss_limit(evidence)
    if limit:
        # Hosted: a program file whose encoder started late shows take slot
        # offset + n at its picture n (as check_record_take reads it); its
        # missing first slots are accounted loss (test Macs: strict).
        late = round((pf.get("start_offset_seconds") or 0) * fps)
        loss = next(row for key, row in evidence["drops"].items() if Path(key) == program)
        require(late <= HOSTED_LATE_START_CAP,
                f"{program.name}: started {late} frame(s) late, more than the hosted cap of {HOSTED_LATE_START_CAP}")
        require(late + loss["dropped"] - loss["head"] <= limit * (loss["slots"] + late - loss["head"]),
                f"{program.name}: started {late} frame(s) late and dropped {loss['dropped']}, more than the "
                f"hosted {limit:.0%}")
        if late:
            loss["late_start_slots"] = late
            decoded = [(slot + late, t, row) for slot, t, row in decoded]
    first_switch = switches[0][0]["time_seconds"]
    t0 = fit_anchor([(s, t, {"a": r["picture"]}) for s, t, r in decoded if s / fps < first_switch - 0.1], "a", count)
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
    # The second take begins where the first ended, on the same engine clock.
    slots = split_slots(takes[0][1], takes[1][1], fps)
    check_split_end(*takes[0], slots, fps)
    check_record_take(e, *takes[1], cfg, RECORD_LAYOUT, count, evidence, isos=("b",), t0=t0 + slots,
                      label="take 2 (split)")
    evidence.update(t0=t0, first_take_slots=slots, pauses=takes[1][1]["pauses"])


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
                   dropped=f["dropped_frames"] - head_slots(f), repeated=f.get("repeated_frames") or 0)
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
    mismatch fails, even one picture one frame late)."""
    def run(changes, pictures=300, evidence=None):
        try:
            oracle = Oracle("rules", work / "none.mov", work, {} if evidence is None else evidence)
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
    # Hosted rules: a held picture (a region repeating its previous expected
    # frame once) is accounted loss; nothing else is.
    def hosted():
        return {"capabilities": {"hosted": True, "max_drop_ratio": HOSTED_DROP_LIMIT}}
    cases.update({
        "hosted: one held picture": (run({100: {"a": ("id", 1, 1099)}}, evidence=hosted()), "pass"),
        "hosted: two held pictures in a row": (
            run({100: {"a": ("id", 1, 1099)}, 101: {"a": ("id", 1, 1099)}}, evidence=hosted()), "fail"),
        "hosted: a hold showing another source": (
            run({100: {"a": ("id", ORACLE_TAGS["b"], 1099)}}, evidence=hosted()), "fail"),
        "hosted: a picture two frames behind": (run({100: {"a": ("id", 1, 1098)}}, evidence=hosted()), "fail"),
        "hosted: more holds than the cap": (
            run({i: {"a": ("id", 1, i + 999)} for i in range(10, 10 + 10 * (HOSTED_HOLD_CAP + 1), 10)},
                evidence=hosted()), "fail"),
        "strict: one held picture": (run({100: {"a": ("id", 1, 1099)}}), "fail"),
    })
    # A 1.5x source (c's frames step 1 or 2): a late frame between the
    # previous picture's expected frame and this one's is a hold (R6).
    def run15(pictures, evidence):
        try:
            oracle = Oracle("rules", work / "none.mov", work, evidence)
            for i in range(120):
                want = ("id", ORACLE_TAGS["c"], math.floor(1.5 * (i + 0.5)))
                oracle.expect(i, i / 30, "c", want, pictures.get(i, want))
            oracle.verdict()
            return "pass"
        except Failure:
            return "fail"
    c15 = lambda i: math.floor(1.5 * (i + 0.5))  # noqa: E731
    step = next(i for i in range(1, 120) if c15(i) - c15(i - 1) == 2)
    cases.update({
        "hosted: a 1.5x source one frame late between two expected frames": (
            run15({step: ("id", 3, c15(step) - 1)}, hosted()), "pass"),
        "strict: a 1.5x source one frame late": (run15({step: ("id", 3, c15(step) - 1)}, {}), "fail"),
        "hosted: a 1.5x source behind the previous picture's frame": (
            run15({step: ("id", 3, c15(step - 1) - 1)}, hosted()), "fail"),
        "hosted: a 1.5x late frame after a wrong picture": (
            run15({step - 1: ("id", 3, c15(step - 1) - 1), step: ("id", 3, c15(step) - 1)}, hosted()), "fail"),
    })
    # Real regressions the hosted rules must still catch.
    cases.update({
        "hosted: a broken frame": (run({100: {"a": ("garbled",)}}, evidence=hosted()), "fail"),
        "hosted: a frozen source": (run({i: {"b": ("id", 2, 1100)} for i in range(101, 131)},
                                        evidence=hosted()), "fail"),
    })
    # Flash/beep pulses: 5 s at 30 fps, a 3-frame flash and a beep every second.
    def pulses(flash_delay=0, drop=(), gone=(), hosted_rules=True, flicker=False):
        beeps = [k + 0.001 / 30 for k in range(5)]
        shown = [j for j in range(150) if j not in drop]
        lit = {k * 30 + flash_delay + d for k in range(5) if k not in gone for d in range(3)}
        if flicker:  # the 2 s flash lit, dark, lit: two onsets within a frame of its beep
            lit = (lit - {60, 61, 62}) | {59, 61}
        flashes = [j / 30 for j in shown if j in lit and (j - 1 not in lit or j - 1 not in shown)]
        try:
            pulse_check("rules.mov", flashes, beeps, 5.0, 30, set(drop) if hosted_rules else None)
            return "pass"
        except Failure:
            return "fail"
    cases.update({
        "pulses as recorded": (pulses(hosted_rules=False), "pass"),
        "hosted: a flash on dropped slots (its whole flash)": (pulses(drop=range(29, 33)), "pass"),
        "hosted: a flash's first two slots dropped": (pulses(drop=(60, 61)), "pass"),
        "strict: a flash's first two slots dropped": (pulses(drop=(60, 61), hosted_rules=False), "fail"),
        "hosted: an A/V offset of 2 frames": (pulses(flash_delay=2), "fail"),
        "hosted: an A/V offset of 2 frames with a flash dropped": (
            pulses(flash_delay=2, drop=range(30, 35)), "fail"),
        "hosted: a flash missing though its slots were shown": (pulses(gone=(2,)), "fail"),
        "hosted: a flickering flash (two onsets at one beep)": (pulses(flicker=True), "fail"),
        "strict: a flickering flash": (pulses(flicker=True, hosted_rules=False), "fail"),
    })
    # Browser Take 3 oracle: allows single-pulse 1/fps + 2 ms rAF quantization
    # when the median is within half a frame, but rejects > 1/fps + 2 ms spikes
    # and rejects constant >= half-frame median shifts.
    def browser_pulses(offsets_ms, lost=None, check_median=True, near=None):
        p = BROWSER_PULSE_PERIOD
        beeps = [0.4 + k * p for k in range(len(offsets_ms))]
        flashes = [b - (d / 1000.0) for b, d in zip(beeps, offsets_ms)]
        try:
            browser_pulse_check("browser.mov", flashes, beeps, 9.0, 30, p, lost,
                                check_median=check_median, near=near)
            return "pass"
        except Failure:
            return "fail"
    cases.update({
        "browser pulses: 33.85 ms quantization spike with 0.52 ms median": (
            browser_pulses([0.46, 0.67, 0.67, 33.85, 0.52, 0.52, 0.52, 0.52, 0.52]), "pass"),
        "browser pulses: 36.5 ms spike exceeds 1/fps + 2 ms": (
            browser_pulses([0.5, 0.5, 0.5, 36.5, 0.5, 0.5, 0.5, 0.5, 0.5]), "fail"),
        "browser pulses: constant +20 ms shift exceeds half-frame median": (
            browser_pulses([20.0] * 9), "fail"),
        "browser pulses: constant -33.3 ms one-frame shift fails": (
            browser_pulses([-33.3] * 9), "fail"),
        "hosted linux browser pulses: 42.81 ms free-running 30 Hz + 60 Hz rAF span": (
            browser_pulses([-42.81, -13.14, -16.81, -20.48, -24.27, -28.0, -31.71, -35.41, -39.21],
                           lost=set(), check_median=False, near=1 / 30 + 1 / 60 + 0.002), "pass"),
        "hosted linux browser pulses: 2-frame (66.7 ms) shift still fails": (
            browser_pulses([-66.7] * 9, lost=set(), check_median=False,
                           near=1 / 30 + 1 / 60 + 0.002), "fail"),
    })
    # A take whose ISO file is missing, and a file far later than the cap.
    def take_rows(files, sources):
        manifest = {"status": "complete", "errors": [], "files": [
            {"path": f"{src}.mov", "source": src, "fps": 30, "width": 1280, "height": 720,
             "frames": 150, "dropped_frames": 0, "repeated_frames": 0, "audio_dropouts": 0,
             "duration_seconds": 5.0 - offset, "start_offset_seconds": offset} for src, offset in files]}
        try:
            check_take(work / "take.session.json", manifest, 5, expected_sources=sources, evidence=hosted())
            return "pass"
        except Failure:
            return "fail"
    cases.update({
        "hosted: a missing ISO": (take_rows([("program", 0), ("bars", 0)], ["program", "bars", "bars2"]), "fail"),
        "hosted: a file later than the late-start cap": (
            take_rows([("program", (HOSTED_LATE_START_CAP + 1) / 30)], ["program"]), "fail"),
    })
    # A split's first take (CI run 40161187404): the program began 4 slots
    # late; take 2 began exactly 2 s (60 slots at 30 fps) after take 1.
    def split_end(program_end, take_end=None):
        first = {"host_time_ns": 1_271_969_827_375, "duration_seconds": take_end or program_end, "files": [
            {"source": "b", "start_offset_seconds": 0.0, "duration_seconds": 2.0},
            {"source": "program", "start_offset_seconds": 4 / 30, "duration_seconds": program_end - 4 / 30}]}
        try:
            slots = split_slots(first, {"host_time_ns": 1_273_969_827_375}, 30)
            check_split_end(work / "take.session.json", first, slots, 30)
            return f"pass {slots}"
        except Failure:
            return "fail"
    def split_end_paused(program_end):
        # 0.5 s of take 1 paused: its files hold 1.5 s of the 2 s to the split.
        first = {"host_time_ns": 0, "duration_seconds": program_end,
                 "pauses": [{"start_seconds": 1.0, "end_seconds": 1.5, "time_seconds": 1.0}],
                 "files": [{"source": "program", "start_offset_seconds": 0.0, "duration_seconds": program_end}]}
        try:
            check_split_end(work / "take.session.json", first, split_slots(first, {"host_time_ns": 2_000_000_000}, 30), 30)
            return "pass"
        except Failure:
            return "fail"
    cases.update({
        "split: a paused take ending on the split": (split_end_paused(1.5), "pass"),
        "split: a paused take a frame past the split": (split_end_paused(1.5 + 1 / 30), "fail"),
        "split: a late program ending on the split": (split_end(2.0), "pass 60"),
        "split: a late program one frame past the split": (split_end(61 / 30), "fail"),
        "split: a take lasting past its split": (split_end(2.0, take_end=61 / 30), "fail"),
    })
    # A stale picture at a pause's resume (#124 re-review B1): 43 frames cut
    # out after slot 99; picture 100 shows a frame 20 behind for every source.
    want = lambda slot, r: ("id", ORACLE_TAGS[r], 1000 + slot + (43 if slot >= 100 else 0))  # noqa: E731
    rows = [(slot, slot / 30, {r: (("id", ORACLE_TAGS[r], 1123) if slot == 100 else want(slot, r))
                               for r in ("a", "b", "c")}) for slot in range(300)]
    for label, evidence in (("a stale picture at a pause's resume", {}),
                            ("hosted: a stale picture at a pause's resume", hosted())):
        try:
            oracle_check_rows("rules", work / "none.mov", rows, want, work, evidence)
            cases[label] = ("pass", "fail")
        except Failure:
            cases[label] = ("fail", "fail")
    # The vertical stream (hosted: max(3, 2%) accounted; strict: none), and
    # its received packets (a gap over one frame fails either way).
    def stream(dropped, sent=300, evidence=None):
        final = {"frames_sent": sent, "dropped_frames": dropped, "audio_dropouts": 0, "elapsed_seconds": 10.0}
        try:
            vertical_stream_loss(final, 30, {} if evidence is None else evidence)
            return "pass"
        except Failure:
            return "fail"
    every = [k / 30 for k in range(300)]
    one_lost = [t for k, t in enumerate(every) if k != 150]
    cases.update({
        "hosted: vertical stream drops 1 of 294": (stream(1, 294, hosted()), "pass"),
        "hosted: vertical stream drops 4 of 291": (stream(4, 291, hosted()), "pass"),
        "hosted: vertical stream drops 10%": (stream(30, 300, hosted()), "fail"),
        "strict: vertical stream drops 1": (stream(1, 300), "fail"),
        "strict: vertical stream drops none": (stream(0, 300), "pass"),
        "received stream, every frame": ("pass" if video_packets_accounted(every, 30, 0) else "fail", "pass"),
        "hosted: a 2-frame gap the engine didn't count": (
            "pass" if video_packets_accounted(one_lost, 30, 0) else "fail", "fail"),
        "hosted: a 2-frame gap for 1 counted drop": (
            "pass" if video_packets_accounted(one_lost, 30, 1) else "fail", "pass"),
        "hosted: a 3-frame gap for 1 counted drop": (
            "pass" if video_packets_accounted([t for k, t in enumerate(every) if k not in (150, 151)], 30, 1)
            else "fail", "fail"),
        "hosted: two 2-frame gaps for 1 counted drop": (
            "pass" if video_packets_accounted([t for k, t in enumerate(every) if k not in (100, 200)], 30, 1)
            else "fail", "fail"),
        "hosted: a gap off the frame grid for 1 counted drop": (
            "pass" if video_packets_accounted([t + (0.01 if k >= 150 else 0) for k, t in enumerate(one_lost)], 30, 1)
            else "fail", "fail"),
        "video packets out of order": (
            "pass" if video_packets_accounted([t if k != 100 else every[98] for k, t in enumerate(every)], 30, 5)
            else "fail", "fail"),
        "hosted: vertical stream drops 6 of 300 (the 2% edge)": (stream(6, 300, hosted()), "pass"),
        "hosted: vertical stream drops 7 of 300": (stream(7, 300, hosted()), "fail"),
        "hosted: vertical stream drops 3 of 100 (the 3-frame floor)": (stream(3, 100, hosted()), "pass"),
        "hosted: vertical stream drops 4 of 100": (stream(4, 100, hosted()), "fail"),
        "received audio with a gap": (
            "pass" if packets_unbroken([k * 0.021333 for k in range(400) if k != 200], 0.023) else "fail", "fail"),
    })

    def monitor_take(dropped, startup, snapshot=3.2):
        manifest = {"files": [{"path": "monitor-take.mov", "source": "program", "fps": 30,
                               "frames": 240 - dropped, "dropped_frames": dropped, "repeated_frames": 0,
                               "duration_seconds": 8.0, "audio_dropouts": 0}]}
        try:
            monitor_take_losses(manifest, startup, snapshot)
            return "pass"
        except Failure:
            return "fail"
    cases.update({
        "hosted: monitor take lost 8 frames, all before the snapshot": (
            monitor_take(8, {"program": 8}), "pass"),
        "hosted: monitor take lost nothing": (monitor_take(0, {"program": 0}), "pass"),
        "hosted: monitor take lost 11 frames after the snapshot (within VM stall cap)": (
            monitor_take(11, {"program": 0}), "pass"),
        "hosted: monitor take lost 12 frames after the snapshot (the cap)": (
            monitor_take(20, {"program": 8}), "pass"),
        "hosted: monitor take lost 13 frames after the snapshot": (
            monitor_take(21, {"program": 8}), "fail"),
        "hosted: monitor take source missing from the snapshot": (
            monitor_take(1, {"other": 0}), "fail"),
        "strict: monitor take lost 1 frame at its start": (monitor_take(1, None), "fail"),
        "strict: monitor take lost nothing": (monitor_take(0, None), "pass"),
    })
    # Pause-cut marker tail (run 42102286577): a 40 ms whole-second beep starts
    # at media frame 150 (paused out) and its last 6.7 ms play at file slot 30
    # (1.0 s), where the resumed picture shows media frame 151; second 6 (frame
    # 180) still plays normally at file slot 59.
    def pause_marker_tail(cuts, stray_at=None):
        samples = [0.0] * int(2.4 * RATE)
        start = int((59 / 30) * RATE)
        for i in range(int(0.04 * RATE)):
            samples[start + i] = 0.5 * math.sin(2 * math.pi * ORACLE_MARKERS[6] * i / RATE)
        # 6.7 ms tail of second 5's beep cut in at file slot 30 (1.0 s).
        for i in range(int((0.04 - 1 / 30) * RATE)):
            samples[RATE + i] = 0.5 * math.sin(2 * math.pi * ORACLE_MARKERS[5] * (1600 + i) / RATE)
        if stray_at is not None:
            start = int(stray_at * RATE)
            for i in range(int(0.02 * RATE)):
                samples[start + i] = 0.5 * math.sin(2 * math.pi * ORACLE_MARKERS[2] * i / RATE)
        frame_at = lambda j: (88 + j + (33 if j >= 30 else 0)) % 3600  # noqa: E731
        try:
            oracle_markers("pause-cut", work / "pause-cut.mov", frame_at, 30, {}, cuts=cuts, samples=samples)
            return "pass"
        except Failure:
            return "fail"
    cases.update({
        "pause cut into a 40 ms marker beep's tail, with cut passed": (pause_marker_tail((30,)), "pass"),
        "pause cut into a 40 ms marker beep's tail, without cut": (pause_marker_tail(()), "fail"),
        "stray burst away from a pause cut still fails": (pause_marker_tail((30,), stray_at=1.5), "fail"),
    })
    # Encode real camera pictures so these cases exercise the same decoder
    # and cadence rule as the V4L2 ISO. Start near a fixture loop boundary.
    fps, period, slots = 30, ORACLE_SECONDS * 30, 30
    for name, frames, error in (
        ("phase beat", [i - (i % 3 == 1) for i in range(slots)], None),
        ("10-frame freeze", [5 if 5 <= i < 15 else i for i in range(slots)], "three identical"),
        ("5-code jump", [i if i < 10 else i + 4 for i in range(slots)], "jump of 5"),
        ("short take", range(slots - 3), "code span"),
        ("drifting repeats", [i - (i >= 5) - (i >= 10) for i in range(slots)], "phase drift"),
    ):
        path = work / f"camera-{name}.mov"
        raw = bytes(ORACLE_LEVELS[c] for i in frames for c in oracle_cells(ORACLE_TAGS["b"], (i - 2) % period))
        ffmpeg("-f", "rawvideo", "-pixel_format", "gray", "-video_size", "16x9", "-framerate", fps,
               "-i", "pipe:0", "-vf", "scale=160:90:flags=neighbor", "-c:v", "libx264", "-y", path, data=raw)
        rows = oracle_decode(path, oracle_regions([("b", (0, 0, 1, 1))]), fps)
        ids = [row[2]["b"] for row in rows]
        require(all(x[:2] == ("id", ORACLE_TAGS["b"]) for x in ids), f"{path.name}: unreadable fixture")
        try:
            camera_code_span(path, [x[2] for x in ids], period, slots)
            outcome = "pass"
        except Failure as exc:
            require(error and error in str(exc), f"{path.name}: unexpected failure: {exc}")
            outcome = "fail"
        cases[f"camera: {name}"] = (outcome, "fail" if error else "pass")
    for is_hosted in (False, True):
        for label, head, dropped, audio, expected in (
            ("one interior drop", 0, 1, 0, "pass" if is_hosted else "fail"),
            ("slot zero dropped", 1, 1, 0, "fail"),
            ("audio dropout", 0, 0, 1, "fail"),
        ):
            row = {"output": "program", "slots": 600, "frames": 600 - dropped, "head": head,
                   "dropped": dropped, "repeated": 0, "audio_dropouts": audio}
            try:
                check_preview_program_loss(row, hosted_reference=is_hosted)
                actual = "pass"
            except Failure:
                actual = "fail"
            cases[f"{'hosted' if is_hosted else 'strict'} preview: {label}"] = actual, expected
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
    pattern = pattern_selftest(work)
    shutil.rmtree(work)
    return {"encoders": encoders, "cases": results, "caught": caught, "rules": rules, "pattern": pattern}


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
PATTERN_LUMA = (1.0, 1.0 - 0.0722, 1.0 - 0.2126, 0.7152, 0.2848, 0.2126, 0.0722, 0.0)


def pattern_period(fps):
    return 4 * fps


def pattern_shift(n, width, fps):
    return n * width // pattern_period(fps) % width


def pattern_sampled_row(n, width, fps, sampled_width):
    """Normalized [0..1] BT.709 bar luma across `sampled_width` output pixels
    under the compositor's 1-tap linear sampling."""
    shift = pattern_shift(n, width, fps)
    out = []
    for p in range(sampled_width):
        tc = max(0.0, min(width - 1.0, (p + 0.5) * width / sampled_width - 0.5))
        i0 = int(tc)
        i1, f = min(width - 1, i0 + 1), tc - i0
        v0 = PATTERN_LUMA[((i0 + shift) % width) * 8 // width]
        v1 = PATTERN_LUMA[((i1 + shift) % width) * 8 // width]
        out.append(v0 * (1.0 - f) + v1 * f)
    return out


def pattern_samples(path, specs, src_w, src_h, period=None):
    """Per decoded picture and per spec: (pts, {name: (bars row, label cell
    level, band level)}). A spec is (name, (x, y, w, h) where the source's
    picture sits in the output, in output pixels, the fraction of the
    source's height the row is read at, (x0, x1) the band's share of the
    source's width): the row is kept at its native width below `period`, else
    scaled back to the source's width."""
    s = max(src_h // 120, 1)
    n = len(specs)
    # Read bar positions from the luma plane directly (`extractplanes=y`),
    # averaged over a clean vertical strip: `format=gray` converts 4:2:0
    # YCbCr through BT.601 RGB, mixing half-resolution left-sited chroma and
    # adjacent-layer border chroma into the bar edges (#266).
    graph, outs, row_widths = [(f"[0:v]split=2[vy][vg];"
                                f"[vy]extractplanes=y,split={n}" + "".join(f"[y{k}]" for k in range(n)) + ";"
                                f"[vg]format=gray,split={2 * n}" + "".join(f"[g{k}]" for k in range(2 * n)) + ";")], [], []
    for k, (name, (x, y, w, h), row, band) in enumerate(specs):
        sx, sy = w / src_w, h / src_h
        rw, rh = max(1, round(w)), max(1, round(0.10 * h))
        ry = round(y + row * h - rh / 2)
        native = period is not None and rw < period and rw <= src_w
        row_widths.append(rw if native else src_w)
        tail = f"pad={src_w}:1:0:0" if native else f"scale={src_w}:1:flags=bilinear"
        graph.append(f"[y{k}]crop={rw}:{rh}:{round(x)}:{ry},scale={rw}:1:flags=area,{tail}[r{k}];")
        # Project both edges of the fifth glyph: in the 180p square inset
        # it is only 1.4 x 2 pixels. Flooring the origin and size separately
        # can leave just a black pixel above the '2'. Read the actual mean;
        # resizing a tiny crop to a single row can skip lit strokes.
        left, right = (round(x + col * s * sx) for col in (30, 35))
        top, bottom = (round(y + line * s * sy) for line in (6, 13))
        graph.append(f"[g{2 * k}]crop={max(1, right-left)}:{max(1, bottom-top)}:{left}:{top},"
                     f"geq=lum='psum(W-1,H-1)/(W*H)',scale={src_w}:1:flags=neighbor[c{k}];")
        bx0, bx1 = band
        graph.append(f"[g{2 * k + 1}]crop={max(1, int(w * (bx1 - bx0)))}:{max(1, int(h / 4))}:"
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
            bars = picture[base:base + row_widths[k]]
            cell = picture[base + src_w]
            band = sum(picture[base + 2 * src_w:base + 3 * src_w]) / src_w
            row[name] = (bars, cell, band)
        rows.append((t, row))
    return rows


def pattern_value(bars, cell, width, fps, sampled_width=None, leads=None):
    """("id", tag, frame mod 4 s) for a sampled source, or ("garbled",)."""
    period = pattern_period(fps)
    if sampled_width is not None and sampled_width < period:
        # Less than one output pixel per frame of scroll: 1-tap linear
        # downscaling skips source texels, so two adjacent frames can leave
        # the black/white and green/magenta edges unchanged while other bar
        # edges advance. Match this picture's native interior output pixels
        # directly against the resampled luma profile of nearby candidates.
        sw = round(sampled_width)
        if sw < 8:
            return ("garbled",)
        px = bars if len(bars) == sw else bytes(bars[round((p + 0.5) * len(bars) / sw - 0.5)] for p in range(sw))
        bits = px.translate(BAR_LEVELS)
        rise, fall = bits.find(b"01"), bits.find(b"10")
        if 1 < rise < sw - 3:
            coarse = (sw - rise - 1) % sw
        elif 1 < fall < sw - 3:
            coarse = (sw / 2 - fall - 1) % sw
        else:
            return ("garbled",)
        guess = round(coarse * period / sw)
        interior = sorted(px[2:sw - 2])
        plateau = max(3, sw // 12)
        lo, hi = statistics.median(interior[:plateau]), statistics.median(interior[-plateau:])
        if hi - lo < 64:
            return ("garbled",)
        obs = [(v - lo) / (hi - lo) for v in px[2:sw - 2]]
        scored = sorted((sum(abs(a - b) for a, b in zip(
            obs, pattern_sampled_row(cand, width, fps, sw)[2:sw - 2])) / len(obs), cand)
            for cand in {n % period for n in range(guess - 3, guess + 4)})
        best_dist, best = scored[0]
        if best_dist > 0.08:
            return ("garbled",)
        if leads is not None and len(scored) > 1:
            leads.append(scored[1][0] - best_dist)
        return ("id", PATTERN_TAGS["bars2" if cell > 50 else "bars"], best)
    bits = bars.translate(BAR_LEVELS)
    rise, fall = bits.find(b"01"), bits.find(b"10")
    if 2 < rise < width - 4:
        shift = (width - rise - 1) % width
    elif 2 < fall < width - 4:
        shift = (width // 2 - fall - 1) % width
    else:
        return ("garbled",)
    guess = round(shift * period / width)
    best = min(range(guess - 2, guess + 3), key=lambda n: min(abs(pattern_shift(n % period, width, fps) - shift),
                                                             width - abs(pattern_shift(n % period, width, fps) - shift)))
    apart = abs(pattern_shift(best % period, width, fps) - shift)
    if min(apart, width - apart) > max(2, width / period / 3):
        return ("garbled",)
    return ("id", PATTERN_TAGS["bars2" if cell > 50 else "bars"], best % period)


def pattern_rows(path, specs, src, fps, leads=None):
    """[(slot, pts, {name: ("id", tag, frame) or ("garbled",)})] of `path`,
    the test pattern read at `specs` (see pattern_samples)."""
    rows = []
    widths = {name: rect[2] for name, rect, _, _ in specs}
    for t, sampled in pattern_samples(path, specs, *src, period=pattern_period(fps)):
        slot = round(t * fps)
        require(abs(t * fps - slot) < 0.02, f"{Path(path).name}: a picture at {t:.5f} s is off the frame grid")
        rows.append((slot, t, {n: pattern_value(b, c, src[0], fps, widths[n], leads=leads)
                               for n, (b, c, _) in sampled.items()}))
    return rows


def pattern_selftest(work):
    """The 180p30 letterboxed square test-pattern oracle (#266): 90 px inset
    over a 120-frame scroll, 1-tap linear sampling and left-sited 4:2:0 NV12
    through H.264, plus source-substitution and one-tick-late controls."""
    if "libx264" not in subprocess.run(["ffmpeg", "-hide_banner", "-encoders"], capture_output=True, text=True).stdout:
        return "skipped: no libx264"
    bars_bgra = ((191, 191, 191), (0, 191, 191), (191, 191, 0), (0, 191, 0),
                 (191, 0, 191), (0, 0, 191), (191, 0, 0), (0, 0, 0))
    glyph_2 = (0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F)
    src_w, src_h, out_side, fps, period = 320, 180, 180, 30, 120

    def source_frame(frame, is_bars2):
        shift = pattern_shift(frame, src_w, fps)
        row = bytearray()
        for x in range(src_w):
            b, g, r = bars_bgra[((x + shift) % src_w) * 8 // src_w]
            row.extend((b, g, r, 255))
        buf = bytearray(bytes(row) * src_h)
        for y in range(4, 33):
            for x in range(4, 105):
                buf[(y * src_w + x) * 4:(y * src_w + x + 1) * 4] = b"\x00\x00\x00\xff"
        if is_bars2:
            for r_idx, bits in enumerate(glyph_2):
                for col in range(5):
                    if bits & (0b10000 >> col):
                        px = ((6 + r_idx) * src_w + 30 + col) * 4
                        buf[px:px + 4] = b"\xff\xff\xff\xff"
        return buf

    def compose_nv12(bars_frame, inset_frame, inset_is_bars2):
        full = source_frame(bars_frame, False)
        inset = source_frame(inset_frame, inset_is_bars2)
        bgra = bytearray(out_side * out_side * 4)

        def blit(src_buf, dx, dy, dw, dh):
            for py in range(dh):
                ty = max(0.0, min(src_h - 1.0, (py + 0.5) * src_h / dh - 0.5))
                y0 = int(ty)
                y1, fy = min(src_h - 1, y0 + 1), ty - y0
                for px in range(dw):
                    tx = max(0.0, min(src_w - 1.0, (px + 0.5) * src_w / dw - 0.5))
                    x0 = int(tx)
                    x1, fx = min(src_w - 1, x0 + 1), tx - x0
                    p00, p10 = (y0 * src_w + x0) * 4, (y0 * src_w + x1) * 4
                    p01, p11 = (y1 * src_w + x0) * 4, (y1 * src_w + x1) * 4
                    dst = ((dy + py) * out_side + dx + px) * 4
                    for c in range(3):
                        top = src_buf[p00 + c] * (1 - fx) + src_buf[p10 + c] * fx
                        bot = src_buf[p01 + c] * (1 - fx) + src_buf[p11 + c] * fx
                        bgra[dst + c] = round(top * (1 - fy) + bot * fy)
                    bgra[dst + 3] = 255

        blit(full, 0, 39, 180, 102)
        blit(inset, 90, 90, 90, 51)
        luma, chroma = bytearray(out_side * out_side), bytearray(out_side * out_side // 2)
        for idx in range(out_side * out_side):
            b, g, r = bgra[idx * 4], bgra[idx * 4 + 1], bgra[idx * 4 + 2]
            luma[idx] = max(16, min(235, 16 + ((11966 * r + 40254 * g + 4064 * b + 32768) >> 16)))
        for y in range(0, out_side, 2):
            for x in range(0, out_side, 2):
                rgb = [0, 0, 0]
                for row_y in (y, y + 1):
                    for col_x, wt in ((max(0, x - 1), 1), (x, 2), (x + 1, 1)):
                        at = (row_y * out_side + col_x) * 4
                        rgb[0] += bgra[at + 2] * wt
                        rgb[1] += bgra[at + 1] * wt
                        rgb[2] += bgra[at] * wt
                r, g, b = rgb
                at = (y // 2) * out_side + x
                chroma[at] = max(16, min(240, 128 + ((-6596 * r - 22189 * g + 28784 * b + 262144) >> 19)))
                chroma[at + 1] = max(16, min(240, 128 + ((28784 * r - 26145 * g - 2639 * b + 262144) >> 19)))
        return bytes(luma) + bytes(chroma)

    def encode_clip(path, raw_nv12):
        subprocess.run(["ffmpeg", "-y", "-v", "error", "-f", "rawvideo", "-pix_fmt", "nv12", "-colorspace", "bt709",
                        "-color_range", "tv", "-s", f"{out_side}x{out_side}", "-r", str(fps), "-i", "-",
                        "-c:v", "libx264", "-crf", "26", "-colorspace", "bt709", "-color_range", "tv", str(path)],
                       input=raw_nv12, check=True, timeout=120)

    specs = pattern_specs([("bars", (0, 0, 1, 1)), ("bars2", (0.5, 0.5, 0.5, 0.5))],
                          out_side, out_side, fit_rect((src_w, src_h), (out_side, out_side)))
    expected = lambda slot, name: ("id", PATTERN_TAGS[name], slot % period)
    good_frames = [compose_nv12(n, n, True) for n in range(period)]
    good = work / "square-good.mov"
    encode_clip(good, b"".join(good_frames))
    evidence, leads = {}, []
    oracle_check_rows("selftest square", good, pattern_rows(good, specs, (src_w, src_h), fps, leads=leads), expected, work, evidence)
    require(len(leads) == period, f"expected {period} sub-pixel lead measurements, got {len(leads)}")
    evidence["oracle"][0]["min_lead"] = round(min(leads), 6)

    # Negative control 1: holding only the 90 px inset at frame 69 on slot 70
    # (where stripes 0 and 4 sit on unsampled blind-spot texels) must fail.
    held_frames = list(good_frames)
    held_frames[70] = compose_nv12(70, 69, True)
    held = work / "square-held-70.mov"
    encode_clip(held, b"".join(held_frames))
    try:
        oracle_check_rows("selftest square held", held, pattern_rows(held, specs, (src_w, src_h), fps), expected, work, {})
        raise Failure("square pattern oracle passed a one-frame-late 90 px inset at slot 70")
    except Failure as exc:
        require("expected bars2:70, got bars2:69" in str(exc) and "1 of 240 checks differ" in str(exc),
                f"square held inset failed unexpectedly: {exc}")

    # Negative control 2: bars2 one tick late on every picture across all 120
    # phases must catch all 120 phases while keeping bars exact.
    all_late = work / "square-all-late.mov"
    encode_clip(all_late, b"".join(compose_nv12(n, (n - 1) % period, True) for n in range(period)))
    late_rows = pattern_rows(all_late, specs, (src_w, src_h), fps)
    require(len(late_rows) == period and all(
        r["bars"] == ("id", PATTERN_TAGS["bars"], n) and r["bars2"] != ("id", PATTERN_TAGS["bars2"], n)
        for n, (_, _, r) in enumerate(late_rows)),
        "square all-late control missed a phase or misread bars")
    try:
        oracle_check_rows("selftest square all-late", all_late, late_rows, expected, work, {})
        raise Failure("square pattern oracle passed an all-late 90 px inset")
    except Failure as exc:
        require("expected bars2:0, got bars2:119" in str(exc) and "120 of 240 checks differ" in str(exc),
                f"square all-late inset failed unexpectedly: {exc}")

    # Negative control 3: substituting BARS into the 90 px inset must fail every picture.
    wrong_src = work / "square-wrong-source.mov"
    encode_clip(wrong_src, b"".join(compose_nv12(n, n, False) for n in range(period)))
    try:
        oracle_check_rows("selftest square source", wrong_src, pattern_rows(wrong_src, specs, (src_w, src_h), fps), expected, work, {})
        raise Failure("square pattern oracle passed BARS in the BARS2 inset")
    except Failure as exc:
        require("expected bars2:0, got bars:0" in str(exc) and "120 of 240 checks differ" in str(exc),
                f"square substituted source failed unexpectedly: {exc}")
    return evidence["oracle"][0]


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
        leads = []
        rows = pattern_rows(file, specs, (w, h), fps, leads=leads)
        if leads:
            lead_min = round(min(leads), 6)
            prev = evidence.get("min_lead")
            evidence["min_lead"] = lead_min if prev is None else min(prev, lead_min)
        if t0 is None:
            t0 = (pattern_fit(rows, "bars", period) - offset) % period
        before_len = len(evidence.get("oracle", ()))
        try:
            checked = oracle_check_rows(f"{label} {source}", file, rows, expected(offset), e.root, evidence)
        finally:
            if leads and len(evidence.get("oracle", ())) > before_len:
                evidence["oracle"][-1]["min_lead"] = lead_min
        if f.get("audio") and source in ("program", "bars", "bars2") and not pauses:
            # The tone beeps with every flash of the band (the bottom quarter
            # of the program and ISO pictures; a pause breaks the cadence).
            # Hosted: a flash on a dropped or held slot is accounted loss.
            lost = {round(h["time"] * fps) for h in checked.get("held", [])} if loss_limit(evidence) else None
            evidence.setdefault("sync", []).append(dict(sync_check(file, fps, lost=lost), output=file.name))
    return t0


def scenario_win_adapter_preference(e, cfg, evidence):
    """A stable setting survives restart; a vanished device is not a startup failure."""
    cli = e.binary.with_name('isocast.exe'); fps=30; size=(320,180)
    def preference(*args):
        p=subprocess.run([cli,'gpu-adapter',*(['--set',args[0]] if args else ['--choices']),'--json'],env=e.env,cwd=e.root,text=True,capture_output=True,timeout=30)
        require(p.returncode==0,p.stderr); return json.loads(p.stdout)
    cfg=setup(e.root,'h264',width=size[0],height=size[1],fps=fps); e.apply(cfg)
    def cold_start(mode):
        e.stop(); began=time.monotonic(); e.start(); path=Path(e.rpc('start',{'seconds':2})['manifest'])
        evidence[mode+'_cold_start_seconds']=time.monotonic()-began
        until(lambda:finished(path),25,'cold take complete'); until(lambda:e.rpc('status')['state']=='idle',description='idle after cold take')
    cold_start('auto'); first=preference()
    picked=first['chosen']; id=picked['id']; expected=e.env.get('ISOCAST_TEST_EXPECT_ADAPTER')
    require(first['preference']=='auto' and first['reason'] and (expected.lower() in picked['name'].lower() if expected else picked['software']),str(first))
    require(picked['luid']==e.rpc('gpu_adapter') and picked in first['candidates'], 'plan and native pin disagree')
    try: e.rpc('gpu_preference',{'adapter':'not-a-valid-id'})
    except RpcError as ex: require('params.adapter:' in str(ex) and 'valid: auto' in str(ex) and id in ex.error['message'].replace('\\\\','\\'), str(ex))
    else: raise Failure('accepted an unknown override')
    pending=preference(id); require(pending['preference']==id and pending['restart_required'] and pending['chosen']==picked,'did not stage preference')
    def selection_warnings(doctor): return [c for c in doctor['checks'] if c['name']=='Windows GPU selection' and c['status']=='warning']
    require(any('restart' in c['detail'] and c.get('fix') for c in selection_warnings(e.rpc('doctor',{}))), 'doctor did not explain pending GPU restart')
    cold_start('explicit'); explicit=e.rpc('gpu_preference',{})
    require(explicit['chosen']==picked and explicit['preference']==id and not explicit['restart_required'] and 'explicit' in explicit['reason'],str(explicit))
    e.stop(); missing='pnp:removed-e2e-adapter'; e.sock.with_suffix('.gpu-adapter').write_text(missing+'\n',encoding='utf8'); e.start()
    doctor=e.rpc('doctor',{}); plan=doctor['windows_gpu']['selection']; require(plan['preference']==missing and missing in plan['warning'] and plan['chosen']==picked, str(plan))
    require(any(missing in c['detail'] and c.get('fix') for c in selection_warnings(doctor)), 'doctor checks omitted missing GPU')
    path=Path(e.rpc('start',{'seconds':5})['manifest'])
    try: e.rpc('gpu_preference',{'adapter':'auto'})
    except RpcError as ex: require('stop recording' in str(ex).lower(),str(ex))
    else: raise Failure('mutated preference during capture')
    live=e.rpc('status')['windows_gpu']; require(live['selection']==plan and all(live[k]['luid']==picked['luid'] for k in ('render_adapter','graphics_adapter')),str(live))
    m=until(lambda:finished(path),35,'fallback recording finalization'); check_take(path,m,5,expected_sources=['program'])
    movie,row=manifest_file(path,m,'program'); rows=pattern_rows(movie,pattern_specs([('p',(0,0,1,1))],*size),size,fps); origin=pattern_fit(rows,'p',pattern_period(fps))
    require(len(rows)==150 and row['dropped_frames']==row['repeated_frames']==0 and all(slot==n and v['p']==('id',PATTERN_TAGS['bars'],(origin+n)%pattern_period(fps)) for n,(slot,_,v) in enumerate(rows)),'expected 150 consecutive decoded program pictures')
    evidence.update(adapter=id,luid=picked['luid'],reason=plan['reason'],warning=plan['warning'],decoded=len(rows))


def scenario_win_idle_gpu_replacement(e, cfg, evidence):
    """Local WARP RemoveDevice only at the next idle Start. Never a desktop TDR."""
    if e.env.get('ISOCAST_TEST_IDLE_SUITE') != '1':
        raise Skip('requires debug daemon and --engine-env ISOCAST_TEST_IDLE_SUITE=1 on WARP')
    for mode in ('remove', 'leak'):
        e.stop(); e.env['ISOCAST_TEST_IDLE_GPU'] = mode; e.start()
        w, h, fps = 320, 180, 30
        cfg = setup(e.root / mode, 'h264', width=w, height=h, fps=fps)
        e.apply(cfg)
        take(e, 2)
        before = e.rpc('status')['windows_gpu']
        logs = e.sock.with_suffix('.logs')
        def receipts():
            return [r['message'] for p in logs.glob('*.jsonl') for line in p.read_text(errors='replace').splitlines()
                    if 'idle GPU replacement: generation' in line and (r := json.loads(line)).get('message','').startswith('idle GPU replacement:')]
        count = len(receipts())
        require(before['render_adapter']['warp'] and before['graphics_adapter'] == before['render_adapter'], str(before))
        e.rpc('engine', {'running': False})
        e.rpc('debug_set', {'enabled': True})
        if mode == 'leak':
            try: e.rpc('start', {'seconds': 5})
            except RpcError as ex:
                why = ex.error['message']; require('did not retire' in why and 'DX12 importer' in why, why)
                evidence['held_owner'] = why
            else: raise Failure('a take started while an old importer was held')
            e.rpc('debug_set', {'enabled': False})
        path, m = take(e, 5)
        check_take(path, m, 5, expected_sources=['program'])
        movie, row = manifest_file(path, m, 'program')
        rows = pattern_rows(movie, pattern_specs([('p',(0,0,1,1))], w,h), (w,h), fps)
        origin = pattern_fit(rows, 'p', pattern_period(fps))
        require(len(rows) == 150 and row['dropped_frames'] == row['repeated_frames'] == 0 and
                all(slot == i and pic['p'] == ('id', PATTERN_TAGS['bars'], (origin+i)%pattern_period(fps))
                    for i,(slot,_,pic) in enumerate(rows)), 'replacement did not record 150 consecutive program pictures')
        after = e.rpc('status')['windows_gpu']
        require(all(before[k] == after[k] for k in ('graphics_adapter','render_adapter','shader_compiler')), f'route changed: {before} vs {after}')
        # A healthy no-op Start cannot pass as a replacement. This line is
        # emitted only after G+1's real queue signalled its fresh fence.
        opened = receipts()[count:]
        require(len(opened) == 1 and 'generation 2,' in opened[0] and 'fresh fence completed' in opened[0], f'new generation receipts: {opened}')
        evidence[mode] = dict(frames=len(rows), adapter=after['graphics_adapter'], compiler=after['shader_compiler'], receipt=opened[0])


def scenario_win_program_cut(e, cfg, evidence):
    """Hosted shader check: one program writer, a hard cut, every source and frame ID.
    No ISO writers, extra canvases or fade/slide passes; win_record and win_scenes
    remain the full strict Windows qualification scenarios."""
    w, h, fps = (cfg["canvas"][k] for k in ("width", "height", "fps"))
    cfg["scenes"] = [{"id": s, "name": s, "items": [item(s, (0, 0, 1, 1), (w, h))]} for s in ("bars", "bars2")]
    cfg["recording"]["iso"].update(enabled=False, sources=[])
    e.apply(cfg)
    seconds = 5
    path = Path(e.rpc("start", {"seconds": seconds})["manifest"])
    time.sleep(2)
    cut = e.rpc("switch_scene", {"scene": "bars2", "transition": {"kind": "cut"}})
    manifest = until(lambda: finished(path), seconds + 35, "program cut finalization")
    # No hosted loss policy for this check, even when included in a mixed suite.
    evidence["files"] = check_take(path, manifest, seconds, sync=True, expected_sources=["program"])
    program, _ = manifest_file(path, manifest, "program")
    rows = pattern_rows(program, pattern_specs([("picture", (0, 0, 1, 1))], w, h), (w, h), fps)
    require(len(rows) == seconds * fps and all(slot == i for i, (slot, _, _) in enumerate(rows)),
            f"{program.name}: expected every one of {seconds * fps} program slots in order")
    first = math.ceil(cut["time_seconds"] * fps - 1e-6)
    require(0 < first < len(rows), f"cut at slot {first} did not exercise both sources")
    period = pattern_period(fps)
    t0 = pattern_fit(rows[:first], "picture", period)
    oracle = Oracle("program cut", program, e.root, evidence, hold_ok=False)
    for slot, t, row in rows:
        scene = "bars" if slot < first else "bars2"
        oracle.expect(slot, t, "picture", ("id", PATTERN_TAGS[scene], (t0 + slot) % period), row["picture"])
    oracle.verdict()
    evidence.update(t0=t0, cut_slot=first)


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


@contextlib.contextmanager
def pinned_window_fixture():
    import threading, ctypes, queue
    from ctypes import wintypes as w
    window = ctypes.WinDLL('user32', use_last_error=True)
    window.CreateWindowExW.argtypes = [w.DWORD, w.LPCWSTR, w.LPCWSTR, w.DWORD, *([ctypes.c_int]*4),
                                        w.HWND, w.HMENU, w.HINSTANCE, w.LPVOID]
    window.CreateWindowExW.restype = w.HWND
    window.SetPropW.argtypes = [w.HWND, w.LPCWSTR, w.HANDLE]
    window.DestroyWindow.argtypes = [w.HWND]
    title = f'Isocast pinned window {os.getpid()}'
    commands, replies = queue.Queue(), queue.Queue()
    def owner():
        h = None
        try:
            while True:
                msg = w.MSG()
                while window.PeekMessageW(ctypes.byref(msg), None, 0, 0, 1):
                    window.TranslateMessage(ctypes.byref(msg)); window.DispatchMessageW(ctypes.byref(msg))
                try: cmd = commands.get(timeout=.01)
                except queue.Empty: continue
                if cmd == 'stop': break
                if cmd != 'bright':
                    if h: require(window.DestroyWindow(h), 'fixture DestroyWindow failed')
                    # Built-in dialog class, borderless visible top-level window.
                    h = window.CreateWindowExW(0, '#32770', title, 0x90000000, 20, 20, 160, 90,
                                               None, None, None, None)
                    require(h, f'fixture CreateWindowExW failed: {ctypes.get_last_error()}')
                    window.ShowWindow(w.HWND(h), 4)  # SW_SHOWNOACTIVATE; notably #32770 dialogs
                require(window.SetPropW(h, 'Isocast.E2E.WindowPixel', 1 if cmd == 'new' else 2), 'fixture pixel')
                window.InvalidateRect(w.HWND(h), None, True)
                replies.put(h)
        except Exception as ex:
            replies.put(ex)
        finally:
            if h: window.DestroyWindow(h)
    def change(cmd):
        commands.put(cmd); value = replies.get(timeout=4)
        if isinstance(value, Exception): raise value
        return value
    thread = threading.Thread(target=owner, daemon=True); thread.start()
    try:
        yield title, change, window, thread
    finally:
        commands.put('stop'); thread.join(timeout=3)


def scenario_win_capture_recovery(e, cfg, evidence):
    """Hosted uses the HWND's pixel marker as input; 'native' uses actual WGC."""
    import ctypes
    from ctypes import wintypes as w
    mode = e.env.get("ISOCAST_TEST_WGC_HELPER")
    if mode not in ("native", "fixture"):
        raise Skip("requires isolated debug daemon and ISOCAST_TEST_WGC_HELPER=fixture or native")
    with pinned_window_fixture() as (title, change, window, thread):
        for case in ('resume', 'decoy', 'after-stop'):
            replace = case != 'resume'
            e.stop(); old = change('new'); e.start()
            rect = w.RECT()
            dwm = ctypes.windll.dwmapi.DwmGetWindowAttribute(w.HWND(old), 9, ctypes.byref(rect), ctypes.sizeof(rect))
            require(window.IsWindow(w.HWND(old)) and thread.is_alive(), 'fixture window did not survive daemon startup')
            evidence['window'] = dict(pid=os.getpid(), hwnd=old, visible=bool(window.IsWindowVisible(w.HWND(old))),
                                      dwm_hresult=f'0x{dwm & 0xffffffff:08x}', width=rect.right-rect.left, height=rect.bottom-rect.top)
            fps = 10
            cfg = setup(e.root, 'h264', iso=True, width=160, height=90, fps=fps)
            cfg['sources'][0] = dict(id='bars', kind='window', app=Path(sys.executable).name, title=title, cursor=False)
            cfg['recording']['program']['enabled'] = False
            cfg['recording']['folder'] = str(e.root / case)
            e.apply(cfg)
            if case == 'after-stop':
                e.rpc('engine', {'running': True})  # Stop must not dispose of the failed source/engine
            path = Path(e.rpc('start', {} if case == 'after-stop' else {'seconds': 11})['manifest'])
            def live(): return e.rpc('status').get('live') or {}
            def committed():
                files = live().get('files') or []
                return len(files) == 2 and all((f.get('committed_seconds') or 0) >= 2 for f in files)
            until(committed, description='both recording prefixes')
            def window_route():
                route = e.rpc('status')['windows_gpu']
                captures = [c for c in route['captures'] if c['source'] == 'bars']
                if not captures: return None
                c, = captures  # never register two generations of a source
                require(c['display_owner'] is None and c['capture_adapter'] == route['graphics_adapter'],
                        f'wrong window capture route: {c}')
                return c if c['first_frame_received'] else None
            initial_route = until(window_route, description='first helper route with a completed frame')
            e.rpc('debug_set', {'enabled': True})  # parent kills only the owned helper job
            until(lambda: any(i['owner_id'] == 'bars' and i['lost_slots'] >= 2 for i in
                (live().get('video_recovery') or {}).get('incidents', [])), 7, 'pinned ISO actually missing ticks')
            require(not any(c['source'] == 'bars' for c in e.rpc('status')['windows_gpu']['captures']),
                    'retired helper is still reported during the outage')
            current = change('replace' if replace else 'bright')  # same thread, process, class and title
            if not replace: require(current == old, 'fixture changed its original HWND')
            e.rpc('debug_set', {'enabled': False})
            if not replace:
                reopened = until(window_route, 5, 'replacement helper registered with its own first frame')
                require(reopened == initial_route, f'replacement window route changed: {reopened}')
                evidence['resumed_capture_route'] = reopened
            if case == 'after-stop':
                def state():
                    return next((s['state'] for s in e.rpc('status')['engine']['sources'] if s['source'] == 'bars'), None)
                until(lambda: state() == 'failed', description='old window ended during the pinned take')
                e.rpc('stop')
                until(lambda: state() == 'running', 12, 'same-definition window reattached after Stop')
                again, manifest = take(e, 2)  # no apply, screen pick, or engine restart
                captured, = [f for f in manifest['files'] if f['source'] == 'bars']
                require(manifest['status'] == 'complete' and captured['frames'] == 2*fps and
                        manifest['video_recovery']['incident_count'] == 0, f'next take: {manifest}')
                pixels = ffmpeg('-xerror','-i',again.parent / captured['path'],'-map','0:v:0','-vf','scale=1:1',
                                '-fps_mode','passthrough','-pix_fmt','gray','-f','rawvideo','-').stdout
                require(len(pixels) == 2*fps and (mode != 'fixture' or min(pixels) > 160), 'next take did not capture the replacement')
                evidence['reattached_after_stop'] = dict(producer=mode, decoded_frames=len(pixels))
                continue
            m = until(lambda: finished(path), 20, 'final manifest with both files')
            inc, = m['video_recovery']['incidents']; rows = {r['source']: r for r in m['files']}
            require(inc['scope'] == 'source' and inc['owner_id'] == 'bars' and inc['attempts_started'] == 1
                    and set(rows) == {'bars','bars2'}, f'owners/attempts: {m}')
            require(inc['state'] == ('ended_early' if replace else 'resumed') and
                    m['status'] == ('failed' if replace else 'complete'), f'incident: {inc}, {m["status"]}')
            for name, row in rows.items():
                movie = path.parent / row['path']
                packets = json.loads(command(['ffprobe','-v','error','-select_streams','v:0','-show_packets',
                    '-show_entries','packet=pts_time,flags,pos,size','-of','json',movie]).stdout)['packets']
                slots = [round((float(p['pts_time']) + row['start_offset_seconds'])*fps) for p in packets]
                require(slots == sorted(set(slots)) and len(slots) == row['frames'], f'packet ordering/count: {row}')
                pixels = ffmpeg('-xerror','-i',movie,'-map','0:v:0','-vf','scale=1:1','-fps_mode','passthrough',
                                '-pix_fmt','gray','-f','rawvideo','-').stdout
                holes = set(range(slots[0], slots[-1]+1)) - set(slots)
                if name == 'bars2':
                    require(not holes and row['dropped_frames'] == 0 and slots[-1] == 11*fps-1, f'sibling: {row}')
                elif replace:
                    require(not holes and slots[-1]+1 == inc['first_lost_slot'] and inc['end_slot_exclusive'] == 11*fps
                            and 'original pinned window' in inc['reason'], f'decoy accepted or wrong terminal interval: {inc}')
                    if mode == 'fixture': require(max(pixels) < 90, 'decoy pixel entered original ISO')
                else:
                    end = inc['end_slot_exclusive']
                    require(holes == set(range(inc['first_lost_slot'], end)) and len(holes) == inc['lost_slots'] == row['dropped_frames'],
                            f'movie slots {slots} vs ledger {inc}')
                    at = slots.index(end)
                    require('K' in packets[at]['flags'], 'first resumed packet is not random access')
                    fresh_decode(movie, packets[at:])
                    if mode == 'fixture': require(max(pixels[:at]) < 90 and min(pixels[at:]) > 160, 'wrong-generation pixels')
            evidence.setdefault('cases', []).append(dict(producer=mode, replaced=replace, incident=inc, sibling_drops=0))


def scenario_win_take_device_recovery(e, cfg, evidence):
    if e.env.get('ISOCAST_TEST_TAKE_GPU') != 'remove' or e.env.get('ISOCAST_TEST_WGC_HELPER') != 'fixture':
        raise Skip('requires isolated debug WARP RemoveDevice and pinned helper fixture')
    fps = 10
    def live(): return e.rpc('status').get('live') or {}
    def committed():
        rows = live().get('files', [])
        return len(rows) == 3 and all((r.get('committed_seconds') or 0) >= 2 for r in rows)
    with pinned_window_fixture() as (title, change, _, _):
        for case in ('developer-off', 'unsupported', 'active-replay', 'single', 'rollover', 'double', 'double-legacy', 'preview', 'stop-held', 'timed-held'):
            e.stop(); change('new')
            if case in ('developer-off','preview','double-legacy'): e.env.pop('ISOCAST_ENABLE_LIVE_GPU_RECOVERY', None)
            else: e.env['ISOCAST_ENABLE_LIVE_GPU_RECOVERY']='1'
            bypass = case in ('developer-off','unsupported','active-replay')
            e.env['ISOCAST_TEST_TAKE_GPU'] = 'developer-off' if bypass else 'remove'
            if case == 'rollover': e.env['ISOCAST_TEST_TAKE_GPU_PROFILE'] = 'iso/bars'
            else: e.env.pop('ISOCAST_TEST_TAKE_GPU_PROFILE', None)
            e.start()
            cfg = setup(e.root / case, 'h264', iso=True, width=160, height=90, fps=fps)
            cfg['sources'][0] = dict(id='bars', kind='window', app=Path(sys.executable).name, title=title, cursor=False)
            cfg['scenes'][0]['items'][0]['source'] = 'bars2'
            if case == 'unsupported':
                # Camera kind with a fixture producer; the other three are real
                # sources, opened in a second scene without requiring hardware.
                cfg['sources'][1] = dict(id='bars2',kind='camera',device='isocast-e2e-camera')
                image=e.root/'fixture.svg'; image.write_text('<svg xmlns="http://www.w3.org/2000/svg" width="160" height="90"><rect width="160" height="90" fill="lime"/></svg>')
                page=e.root/'fixture.html'; page.write_text('<body style="background:#00f">live</body>')
                cfg['sources'].extend([dict(id='still',kind='image',path=str(image)),dict(id='label',kind='text',text='live',width=160,height=90),dict(id='web',kind='browser',file=str(page),width=160,height=90,fps=fps)])
                cfg['scenes'].append(dict(id='gallery', name='Gallery',items=[item(id,(0,0,1,1),(160,90)) for id in ('still','label','web')]))
            e.apply(cfg)
            path = Path(e.rpc('start', {} if bypass or case.startswith('double') else {'seconds': 7 if case=='timed-held' else 14},timeout=60)['manifest'])
            until(committed, description='program and both ISOs committed')
            initial = {f['source']:f['path'] for f in read_json(path)['files']}
            change('bright')  # only a *new* helper reads this level as its generation marker
            logdir = Path(e.rpc('debug_status')['logs_folder'])
            def count_log(text):
                return sum(f.read_text(errors='replace').count(text) for f in logdir.glob('*.jsonl'))
            if bypass:
                require(not e.rpc('debug_status')['active'], 'control requires developer mode off')
                if case=='unsupported':
                    def mixed_ready():
                        snapshot=e.rpc('status'); sources={s['source']:s for s in snapshot['engine']['sources']}
                        if not all(sources.get(id,{}).get('state')=='running' for id in ('bars2','still','label')): return None
                        web=sources.get('web',{})
                        if web.get('state')=='running': return 'running'
                        # aws-win's SSH service has no composition desktop. Exercise the
                        # real configured browser's refusal there; hosted CI must render.
                        if (e.env.get('ISOCAST_TEST_SERVICE_WEBVIEW')=='1' and snapshot['windows_gpu']['session_type']=='service'
                                and web.get('state')=='failed' and 'Compositor: Access is denied. (0x80070005)' in web.get('error','')):
                            return 'service session: WebView2 compositor denied (0x80070005)'
                        return None
                    evidence['mixed_browser']=until(mixed_ready,45,'camera stand-in, image, text and browser source state')
                if case=='active-replay': require(e.rpc('replay_start')['state']=='running','replay not running before removal')
                replacements=count_log('recording GPU replacement: generation'); injections=count_log('test RemoveDevice confirmed: mode=developer-off')
                marker = e.root/'inject-remove-device'; marker.touch()
                try: until(lambda: count_log('test RemoveDevice confirmed: mode=developer-off')>injections, 6, 'native removal with developer mode OFF')
                finally: marker.unlink()
                start = live()['elapsed_seconds']; before = live()['files']; route = e.rpc('status')['windows_gpu']
                def continuing():
                    status = live(); require(status and not finished(path), f'{case} GPU removal stopped take')
                    require(not (status.get('video_recovery') or {}).get('incidents'), f'{case} entered device recovery')
                    return status if status['elapsed_seconds']>=start+2 and all(r['frames']>next(f['frames'] for f in before if f['source']==r['source']) for r in status['files'] if r['source']!='program') else None
                status = until(continuing, 7, 'healthy D3D11 ISOs still recording after DX12 removal')
                after = e.rpc('status')['windows_gpu']
                require(route.get('graphics_adapter') and after['graphics_adapter']==route['graphics_adapter'] and route['captures'] and after['captures']==route['captures'], 'native capture was retired')
                if case=='active-replay': e.rpc('replay_stop'); time.sleep(.3)
                e.rpc('stop'); m = until(lambda: finished(path), 8, 'operator Stop')
                require(not m['video_recovery']['incidents'] and count_log('recording GPU replacement: generation')==replacements, f'{case}: recovered or replaced the GPU')
                window_iso, _ = manifest_file(path, m, 'bars'); pixels = ffmpeg('-xerror','-i',window_iso,'-map','0:v:0','-vf','scale=1:1','-fps_mode','passthrough','-pix_fmt','gray','-f','rawvideo','-').stdout
                require(pixels and max(pixels)<90, 'production relaunched the pinned helper after removal')
                evidence[case] = dict(native_removal=True, continued_seconds=status['elapsed_seconds']-start, incidents=0); continue
            if case in ('stop-held','timed-held'):
                marker = e.root/'hold-gpu-open'; marker.touch(); previous = count_log('test GPU candidate open held')
                try:
                    e.rpc('debug_set', {'enabled':True}); until(lambda: count_log('test GPU candidate open held')>previous, 6, 'post-barrier candidate held')
                    began = time.monotonic()
                    if case=='stop-held': e.rpc('stop')
                    m = until(lambda: finished(path), 7, 'take finalized without releasing native open')
                    require(time.monotonic()-began<7 and all(f['frames']>0 for f in m['files']), 'awaited blocked GPU open')
                    evidence[case] = dict(finalized_while_held=True, waited=round(time.monotonic()-began,2))
                finally: marker.unlink()
                continue
            if case == 'preview':
                with engine_connection(e.sock) as viewer, viewer.makefile('rb') as feed:
                    viewer.sendall(b'{"jsonrpc":"2.0","id":1,"method":"shared_frames","params":{"target":"program","fps":10}}\n')
                    reply = json.loads(feed.readline()); require('result' in reply, f'preview subscription: {reply}')
                    e.rpc('debug_set', {'enabled': True})
                    m = until(lambda: finished(path), 8, 'clean terminal refusal for active preview')
                require(m['status']=='failed' and 'stop the preview' in str(m['errors']), f'wrong refusal: {m}')
                require('no picture was accepted' not in str(m['errors']), 'finalizer forgot a committed prefix')
                for row in m['files']: ffmpeg('-xerror','-i',path.parent/row['path'],'-map','0','-f','null','-')
                evidence['preview_refusal'] = m['errors']; continue
            e.rpc('debug_set', {'enabled': True})
            def resumed():
                if finished(path): raise Failure(f'take ended before resumption: {read_json(path)}')
                incidents = live().get('video_recovery', {}).get('incidents', [])
                return len([i for i in incidents if i['scope']=='output' and i['state']=='resumed']) == 3
            until(resumed, 16, 'verified IDRs on all three outputs')
            if case.startswith('double'):
                entries=live()['video_recovery']['incidents']; count=count_log('test RemoveDevice confirmed: mode=remove'); prev=live()['elapsed_seconds']
                e.rpc('debug_set', {'enabled':False}); time.sleep(.25); e.rpc('debug_set', {'enabled':True})
                until(lambda:count_log('test RemoveDevice confirmed: mode=remove')>count,5,'second native removal')
                kept=until(lambda:s if (s:=live()) and s['elapsed_seconds']>prev+2 else None,6,'take continues after second loss')
                require(kept['video_recovery']['incidents']==entries and not finished(path),'double loss changed the ledger or ended take')
                e.rpc('stop'); m=until(lambda:finished(path),8,'Stop after second loss')
                require(m['video_recovery']['incidents']==entries,'second loss attributed a recovery incident')
                until(lambda:e.rpc('status')['state']=='idle', description='idle after double loss')
                if case=='double-legacy':
                    require(not e.rpc('status')['windows_gpu'].get('idle_replacement_block'),'developer-only recovery armed the opt-in guard')
                    e.rpc('debug_set', {'enabled':False}); _, legacy=take(e,2)
                    require(legacy['status']=='complete', 'developer-only double loss changed the next Start')
                    evidence[case]=dict(idle_start='complete',cooldown=False); continue
                reset='GPU reset twice within 15 s'; previous=count_log('idle GPU replacement: generation')
                for _ in range(2):
                    try: e.rpc('start', {'seconds':2})
                    except RpcError as ex:
                        refusal=ex.error['message']; require(reset in refusal and 'restart Isocast' in refusal, f'wrong Start refusal: {refusal}')
                    else: raise Failure('idle Start replaced the recently failed replacement')
                require(count_log('idle GPU replacement: generation')==previous, 'idle GPU replacement ran during cooldown')
                for method, params in [('status',None), ('doctor',{})]:
                    require(reset in e.rpc(method,params)['windows_gpu']['idle_replacement_block'], f'{method}: repeat loss not visible')
                for f in m['files']: ffmpeg('-xerror','-i',path.parent/f['path'],'-map','0','-f','null','-')
                e.stop(); e.env['ISOCAST_TEST_TAKE_GPU']='disarmed'; e.start(); e.apply(cfg)
                require(not e.rpc('status')['windows_gpu'].get('idle_replacement_block'), 'cooldown survived daemon restart')
                _, fresh=take(e,2); require(fresh['status']=='complete' and len(fresh['files'])==3, f'new daemon could not record: {fresh}')
                evidence['idle_after_double']=dict(refused_starts=2, reason=refusal, after_restart=fresh['status'])
                evidence[case]=dict(second_removal=True,new_incidents=0,finalized=len(m['files'])); continue
            m = until(lambda: finished(path), 25, 'all files finalized')
            require(m['status'] == 'complete', f'{case}: {m}')
            incidents = m['video_recovery']['incidents']
            require({i['owner_id'] for i in incidents if i['scope']=='source' and i['fault_kind']=='device_lost' and i['state']=='resumed'} == {'bars','bars2'}, f'source ledger: {incidents}')
            if case == 'rollover':
                parts = [f for f in m['files'] if f['source']=='bars']
                require(len(m['files'])==4 and len(parts)==2 and parts[0]['path']==initial['bars'], 'expected one ISO rollover')
                inc, = [i for i in incidents if i['owner_id']=='bars' and i['scope']=='output' and i['fault_kind']=='device_lost']
                movie = path.parent/parts[1]['path']; fresh_decode(movie, recovery_packets(movie,'v:0'))
                require(inc['rollover']['state']=='committed' and round(parts[1]['start_offset_seconds']*fps)==inc['end_slot_exclusive'], f'part vs incident: {parts} {inc}')
                evidence['rollover'] = inc; continue
            require({f['source']:f['path'] for f in m['files']} == initial, 'compatible files rolled or disappeared')
            for row in m['files']:
                name = row['source']; movie = path.parent / row['path']
                video = recovery_packets(movie, 'v:0'); ap = recovery_packets(movie, 'a:0')
                slots = [round((float(p['pts_time']) + row['start_offset_seconds'])*fps) for p in video]
                inc, = [i for i in incidents if i['scope']=='output' and i['owner_id']==name and i['state']=='resumed' and i['fault_kind']=='device_lost']
                cut = inc['end_slot_exclusive']; ix = slots.index(cut)
                missing = set(range(slots[0],slots[-1]+1))-set(slots)
                require(slots == sorted(set(slots)) and len(slots)==row['frames'] and missing == set(range(inc['first_lost_slot'],cut)) and len(missing)==inc['lost_slots']>0, f'{name}: {slots} vs {inc}')
                fresh_decode(movie, video[ix:]); ffmpeg('-xerror','-i',movie,'-map','0','-f','null','-')
                if name == 'bars':
                    pixels = ffmpeg('-i',movie,'-map','0:v:0','-vf','scale=1:1','-fps_mode','passthrough','-pix_fmt','gray','-f','rawvideo','-').stdout
                    require(max(pixels[:ix])<90 and min(pixels[ix:])>160, 'stale pinned-helper pixels')
                else:
                    frames = pattern_rows(movie, pattern_specs([('p',(0,0,1,1))],160,90),(160,90),fps)
                    period = pattern_period(fps); origin = pattern_fit(frames[:ix],'p',period)
                    require(all(v['p']==('id',PATTERN_TAGS['bars2'],(origin+slot)%period) for slot,_,v in frames[ix:]), f'{name}: stale pattern after IDR')
                samples = pcm(movie); pulses = beep_onsets(samples)
                require(row['audio_dropouts']==0 and all(a['pts']+a['duration']==b['pts'] for a,b in zip(ap,ap[1:])) and len(pulses)>=int(len(samples)/RATE)-1 and all(abs(b-a-1)<.003 for a,b in zip(pulses,pulses[1:])), f'{name}: missing PCM or displaced pulse: {pulses}')
                evidence.setdefault(case, {})[name] = dict(frames=len(slots), gap=sorted(missing), idr=cut, audio_samples=len(samples))


def scenario_win_live_device_recovery(e, cfg, evidence):
    """One live connection per consumer, opt-in enabled, never debug_set."""
    require(e.env.get('ISOCAST_ENABLE_LIVE_GPU_RECOVERY')=='1','live recovery qualification requires the explicit opt-in')
    if e.env.get('ISOCAST_TEST_TAKE_GPU') != 'developer-off': raise Skip('requires isolated WARP debug injection')
    import threading
    cli = e.binary.with_name('isocast.exe'); fps = 10
    cfg = setup(e.root, 'h264', iso=True, width=160, height=90, fps=fps)
    cfg['governor'] = {'enabled':True,'pins':['preview.fps']}
    with socket.socket() as sock:
        sock.bind(('127.0.0.1',0)); port=sock.getsockname()[1]
    server=f'rtmp://127.0.0.1:{port}/live'
    cfg['stream'] = {'service':'custom','server':server,'video_bitrate_kbps':700,'audio_bitrate_kbps':128,'width':160,'height':90,'reconnect':{'enabled':False}}
    e.apply(cfg); require(not e.rpc('debug_status')['active'],'requires developer mode off')
    path = Path(e.rpc('start', {'seconds':22})['manifest']); output=e.root/'live.flv'
    hold=e.root/'hold-gpu-open'; trigger=e.root/'inject-remove-device'
    logs=Path(e.rpc('debug_status')['logs_folder']); frames=[]; camera_errors=[]
    def rebind_ready():
        if finished(path): raise Failure(f'take ended during live GPU recovery: {read_json(path)}')
        return any('test GPU candidate open held' in p.read_text(errors='replace') for p in logs.glob('*.jsonl'))
    with open(e.root/'rtmp.log','wb') as log, contextlib.ExitStack() as stack:
        def process(argv, **kwargs):
            p=subprocess.Popen(argv,env=e.env,**kwargs)
            def close():
                if p.poll() is None: p.kill(); p.wait(timeout=5)
            stack.callback(close); return p
        listen=process(['ffmpeg','-nostdin','-v','warning','-listen','1','-i',server+'/e2e','-c','copy','-y',str(output)],stdout=log,stderr=log)
        preview=process([str(cli),'frames','program','--shared','--check','--width','160','--height','90','--fps','10','--seconds','16','--json'],stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
        time.sleep(.5); e.rpc('stream_start'); stack.callback(lambda: e.rpc('stream_stop') if e.rpc('status')['stream']['state']=='live' else None)
        if e.env.get('ISOCAST_TEST_LIVE_VCAM')=='1':
            destination=Path(e.rpc('virtual_camera_start')['destination']); stack.callback(lambda:e.rpc('virtual_camera_stop'))
            def read_camera():
                try:
                    with engine_connection(destination) as pc, pc.makefile('rb') as reader:
                        while header:=reader.read(48):
                            magic,w,h,_,repeat,seq,src,delivery=struct.unpack('<8s4IQqq', header)
                            require(magic==b'ICVCNV01','invalid camera header'); data=reader.read(w*h*3//2)
                            if len(data)!=w*h*3//2: break
                            frames.append((seq,src,repeat,hashlib.sha256(data).digest(),time.monotonic()))
                except Exception as ex: camera_errors.append(str(ex))
            threading.Thread(target=read_camera, daemon=True).start()
            until(lambda:len(frames)>=3,6,'active camera reader')
        def established():
            s=e.rpc('status'); files=(s.get('live') or {}).get('files',[])
            return s['stream']['frames_sent']>=10 and len(files)==3 and all((r.get('committed_seconds') or 0)>=2 for r in files)
        until(established,10,'connected RTMP receiver and three committed files'); time.sleep(.5)
        # A second viewer is partway through its one-time NT name handoff.
        # It deliberately withholds ACK: the old exporter still owns those
        # allocations, even though its token is no longer in the offer map.
        slow=stack.enter_context(engine_connection(e.sock)); events=stack.enter_context(slow.makefile('rb'))
        slow.sendall(b'{"jsonrpc":"2.0","id":1,"method":"shared_frames","params":{"target":"program","fps":10,"slots":2}}\n')
        service=json.loads(events.readline())['result']['service']; ring=json.loads(events.readline())['ring']
        handoff=stack.enter_context(engine_connection(Path(service))); names=stack.enter_context(handoff.makefile('rb'))
        handoff.sendall(bytes.fromhex(ring['token'])); header=names.read(16)
        require(header[:8]==b'ISOCDX01' and len(names.read(32*ring['slots']))==32*ring['slots'],'incomplete old ring offer')
        hold.touch(); trigger.touch()
        try:
            until(rebind_ready,7,'old live owners retired'); held=time.monotonic(); time.sleep(1.25)
            handoff.sendall(b'\x02'); require(names.read(1)==b'\x01','old ring was not explicitly revoked')
            if frames:
                frozen=[f for f in frames if f[4]>=held]; require(frozen and all(f[2] and f[3]==frozen[0][3] for f in frozen),'camera did not keep its last complete frame')
        finally: trigger.unlink(missing_ok=True); hold.unlink(missing_ok=True)
        thaw=time.monotonic()
        def resumed():
            if finished(path): raise Failure(f'take ended before live consumers resumed: {read_json(path)}')
            s=e.rpc('status'); entries=s['live'].get('video_recovery',{}).get('incidents',[])
            return s['stream']['state']=='live' and sum(i['scope']=='output' and i['state']=='resumed' for i in entries)==3
        until(resumed,10,'three committed replacement IDRs'); time.sleep(1)
        if frames: require(not camera_errors and any(not f[2] and f[4]>thaw and f[1]>frozen[-1][1] and f[3]!=frozen[-1][3] for f in frames),'camera reader did not resume')
        final=e.rpc('stream_stop'); listen.wait(timeout=8)
        stdout,stderr=preview.communicate(timeout=18); require(preview.returncode==0,f'preview stopped: {stderr} {stdout}')
        viewer=json.loads(stdout); rings=viewer['rings']; require(viewer['ended'] is None and len(rings)==2 and rings[1]['generation']>rings[0]['generation'] and viewer['check']['frames_read_back']==viewer['frames'] and not viewer['problems'],f'preview did not rebind: {viewer}')
        m=until(lambda:finished(path),20,'finalized take'); require(m['status']=='complete',str(m))
    require(final['reconnects']==0 and final['frames_sent']>15,'stream reconnected or stalled')
    # Remux, never transcode: fresh_decode reads exact AVC AU bytes by MP4 file offset.
    received=e.root/'wire.mp4'; ffmpeg('-xerror','-i',output,'-c','copy','-y',received)
    wire=recovery_packets(received,'v:0'); times=[float(p['pts_time']) for p in wire]
    boundaries=[i for i in range(1,len(times)) if times[i]-times[i-1]>1]; require(len(boundaries)==1 and min(boundaries)>1 and len(wire)-boundaries[0]>3,f'missing unique live outage: {times}')
    fresh_decode(received,wire[boundaries[0]:]); ffmpeg('-xerror','-i',output,'-map','0','-f','null','-')
    samples=pattern_samples(received,pattern_specs([('p',(0,0,1,1))],160,90),160,90,period=pattern_period(fps))
    ids=[pattern_value(row['p'][0],row['p'][1],160,fps,160) for _,row in samples]; resumed_ids=ids[boundaries[0]:]
    require(len(set(resumed_ids))>=3 and all(pic[:2]==('id',PATTERN_TAGS['bars']) for pic in resumed_ids), 'RTMP resumed with stale or wrong pictures')
    audio=[float(p['pts_time']) for p in recovery_packets(received,'a:0')]; require(audio and all(0<b-a<.025 for a,b in zip(audio,audio[1:])), 'live AAC clock broke across removal')
    gaps={}
    for f in m['files']:
        movie=path.parent/f['path']; packets=recovery_packets(movie,'v:0'); slots=[round((float(p['pts_time'])+f['start_offset_seconds'])*fps) for p in packets]
        inc,=[i for i in m['video_recovery']['incidents'] if i['scope']=='output' and i['owner_id']==f['source'] and i['fault_kind']=='device_lost']
        holes=set(range(slots[0],slots[-1]+1))-set(slots); require(holes==set(range(inc['first_lost_slot'],inc['end_slot_exclusive'])) and len(holes)==inc['lost_slots']>0,f'{f}: {holes} != {inc}')
        fresh_decode(movie,packets[slots.index(inc['end_slot_exclusive']):]); gaps[f['source']]=len(holes)
    evidence.update(reconnects=final['reconnects'],rtmp_frames=len(wire),wire_idr=boundaries[0],live_aac_packets=len(audio),preview_rings=rings,readbacks=viewer['frames'],stalled_offer_revoked=True,recorded_gaps=gaps,vcam_frames=len(frames))


def fresh_decode(path, video):
    desc = json.loads(command(["ffprobe", "-v", "error", "-select_streams", "v:0", "-show_streams",
        "-show_entries", "stream=extradata", "-show_data", "-of", "json", path]).stdout)["streams"][0]["extradata"]
    avcc = bytes.fromhex("".join(line.split(":")[1].lstrip().split("  ")[0] for line in desc.splitlines() if line.strip()))
    require(avcc[0] == 1 and avcc[4] & 3 == 3, "not four-byte avcC")
    start = b"\x00\x00\x00\x01"
    stream = bytearray()
    at = 5
    for mask in (31, 255):  # SPS then PPS from the movie's *unchanged* sample entry
        count = avcc[at] & mask
        at += 1
        for _ in range(count):
            size = int.from_bytes(avcc[at:at+2], "big"); at += 2
            stream.extend(start + avcc[at:at+size]); at += size
    with open_shared(path) as f:
        for i, p in enumerate(video):
            f.seek(int(p["pos"])); au = f.read(int(p["size"])); at = 0; slices = []
            while at < len(au):
                size = int.from_bytes(au[at:at+4], "big"); at += 4
                require(0 < size <= len(au) - at, "invalid length in recovery AU")
                kind = au[at] & 31
                if 1 <= kind <= 5: slices.append(kind)
                stream.extend(start + au[at:at+size]); at += size
            if not i: require(slices and set(slices) == {5}, f"first resumed AU is not IDR: {slices}")
    # No seek/preroll: the decoder never sees a single pre-gap picture.
    ffmpeg("-xerror", "-f", "h264", "-i", "pipe:0", "-f", "null", "-", data=stream)


def recovery_packets(path, stream):
    return json.loads(command(["ffprobe", "-v", "error", "-select_streams", stream, "-show_packets",
                    "-show_entries", "packet=pts,dts,pts_time,dts_time,pos,size,duration,flags", "-of", "json", path]).stdout)["packets"]


def scenario_win_encoder_recovery(e, cfg, evidence):
    """Debug daemon only. Two real raw ISO writers: no desktop or GPU required."""
    if e.env.get("ISOCAST_TEST_RECOVERY_SUITE") != "1":
        raise Skip("requires debug binary and --engine-env ISOCAST_TEST_RECOVERY_SUITE=1")
    fps = 12
    packets = recovery_packets

    def live():
        return e.rpc("status").get("live") or {}
    def both_committed():
        files = live().get("files", [])
        return files if len(files) == 2 and all((f.get("committed_seconds") or 0) >= 2 for f in files) else None
    def observed(outcome):
        incidents = (live().get("video_recovery") or {}).get("incidents") or []
        return incidents[0] if len(incidents) == 1 and incidents[0]["state"] == outcome else None

    for op, count in (("output", 1), ("input", 3), ("input", 4)):
        e.stop()
        e.env["ISOCAST_TEST_ENCODER_FAILURE"] = f"iso/bars:{op}:{count}"
        e.start()
        cfg = setup(e.root, "h264", iso=True, width=160, height=90, fps=fps)
        cfg["recording"]["program"]["enabled"] = False
        cfg["recording"]["folder"] = str(e.root / f"fault-{count}")
        e.apply(cfg)
        # Fixed grid end: a scheduler-dependent interactive Stop tail repeat
        # must not obscure the sibling's strict zero-drop assertion.
        path = Path(e.rpc("start", {"seconds": 20})["manifest"])
        files = until(both_committed, description="two committed ISO prefixes")
        backend = next(f["encoder"]["backend"] for f in files if f["source"] == "bars")
        # Finalization rewrites the 16-byte preamble after ftyp, not the
        # committed fragments. Hash only through a complete commit trailer.
        row = next(f for f in read_json(path)["files"] if f["source"] == "bars")
        original_path = path.parent / row["path"]
        with open_shared(original_path) as f: prefix = f.read()
        first_byte = int.from_bytes(prefix[:4], "big") + 16
        at = committed_end = first_byte
        while at + 8 <= len(prefix):
            size = int.from_bytes(prefix[at:at+4], "big")
            if size == 1: size = int.from_bytes(prefix[at+8:at+16], "big")
            if size < 8 or at + size > len(prefix): break
            kind = prefix[at+4:at+8]; at += size
            if kind == b"icmt": committed_end = at
        require(committed_end > first_byte, "no complete pre-fault fragment")
        prefix_hash = hashlib.sha256(prefix[first_byte:committed_end]).hexdigest()
        e.rpc("debug_set", {"enabled": True})  # never race injection against encoder startup
        outcome = "resumed" if count <= 3 else "ended_early"
        reported = until(lambda: observed(outcome), 25, f"{outcome} after {op} fault ({backend})")
        require(reported["scope"] == "output" and reported["owner_id"] == "bars" and
                reported["attempts_started"] == min(count, 3) and reported["fault_kind"] == "encoder_failed"
                and "injected" in reported["reason"], f"wrong failure attribution: {reported}")
        m = until(lambda: finished(path), 35, description="recovered movie finalization")
        require(m["status"] == ("complete" if count <= 3 else "failed"), f"bad final status: {m}")
        inc, = m["video_recovery"]["incidents"]
        require(inc["state"] == outcome and inc["incident_id"] == reported["incident_id"], f"lost incident: {inc}")
        require(len(m["files"]) == 2, f"unexpected output/rollover: {m['files']}")
        rows = {f["source"]: f for f in m["files"]}; target, sibling = rows['bars'], rows['bars2']
        require(path.parent / target['path'] == original_path, "recovery changed media path")
        with open_shared(original_path) as f:
            f.seek(first_byte)
            require(hashlib.sha256(f.read(committed_end-first_byte)).hexdigest() == prefix_hash, "pre-fault committed bytes changed")
        missing = set(range(inc["first_lost_slot"], inc["end_slot_exclusive"]))
        require(bool(missing) and inc["lost_slots"] == len(missing), f"wrong interval: {inc}")
        audio = {}
        for row in (target, sibling):
            movie = path.parent / row['path']; video = packets(movie, "v:0")
            ffmpeg("-xerror", "-i", movie, "-map", "0", "-f", "null", "-")
            require(int(next(s for s in probe(movie)["streams"] if s["codec_type"] == "video")["nb_read_frames"]) == row["frames"], f"wrong count: {row}")
            pts = [(float(p["pts_time"]) + row["start_offset_seconds"]) * fps for p in video]
            slots = [round(t) for t in pts]
            require(all(p["pts"] == p["dts"] for p in video) and all(abs(t - i) < 0.001 for t, i in zip(pts, slots))
                    and all(b > a for a,b in zip(slots, slots[1:])), "invalid take-clock DTS/PTS")
            holes = set(range(slots[0], slots[-1]+1)) - set(slots)
            if row is sibling:
                require(not holes and row["dropped_frames"] == 0, f"healthy sibling lost slots: {row}")
            elif count <= 3:
                require(holes == missing and row['dropped_frames'] == len(missing), f"ledger {inc} vs movie holes {sorted(holes)}: {row}")
                fresh_decode(movie, video[slots.index(inc['end_slot_exclusive']):])
            else:
                require(not holes and slots[-1]+1 == inc['first_lost_slot'], f"last good prefix vs {inc}: {slots[-1]}")
            ap = packets(movie, "a:0")
            require(row['audio_dropouts'] == 0 and len(ap) > 4 and all(a['pts']+a['duration'] == b['pts'] for a,b in zip(ap, ap[1:])), "AAC packets discontinuous")
            audio[row['source']] = pcm(movie)
        # Independent AAC streams of the same routed PCM. Compare on the take
        # clock, including the recovery interval, not just container durations.
        a, b = audio['bars'], audio['bars2']
        delta = round((target['start_offset_seconds']-sibling['start_offset_seconds'])*RATE)
        lo, hi = max(0,-delta)+RATE//8, min(len(a), len(b)-delta)-RATE//8
        require(hi > lo+RATE and any(abs(x)>0.1 for x in a[lo:hi]), "no shared tone to verify")
        for j in range(lo,hi-RATE//50,RATE//50):
            rms = math.sqrt(sum((a[k]-b[k+delta])**2 for k in range(j,j+RATE//50))/(RATE//50))
            require(rms < 0.01, f"decoded audio shifted or missing at sample {j}: RMS difference {rms}")
        evidence.setdefault('cases', []).append(dict(operation=op, faults=count, backend=backend, incident=inc,
                sibling_drops=sibling['dropped_frames'], committed_prefix_sha256=prefix_hash, compared_audio_samples=hi-lo))

    # A live admission holder is a wait, not a failed attempt. Its explicit
    # debug gate lets the test ask for a split *during* recovery without
    # racing the once-a-second status checkpoint against a fast MFT reopen.
    e.stop()
    e.env["ISOCAST_TEST_ENCODER_FAILURE"] = "iso/bars:busy_input:1"
    e.start()
    cfg["recording"]["folder"] = str(e.root / "fault-split")
    e.apply(cfg)
    first = Path(e.rpc("start", {})["manifest"])
    until(both_committed, description="committed prefixes before contended reopen")
    e.rpc("debug_set", {"enabled": True})
    def waiting():
        incidents = (live().get("video_recovery") or {}).get("incidents", [])
        return next((i for i in incidents if i["lost_slots"] >= fps*2), None)
    held = until(waiting, 10, "unavailable output during admission contention")
    require((held['owner_id'],held['state'],held['attempts_started']) == ('bars','interrupted',0),
            f"admission contention consumed an encoder attempt: {held}")
    try:
        e.rpc("split")
    except RpcError as err:
        require("encoder is recovering" in err.error['message'], f"wrong split refusal: {err}")
    else:
        raise Failure("split was accepted in the middle of encoder recovery")
    e.rpc("debug_set", {"enabled": False})
    resumed = until(lambda: observed('resumed'), 10, "first admitted attempt after contention")
    require(resumed['incident_id'] == held['incident_id'] and resumed['attempts_started'] == 1,
            f"resume used the wrong incident or ticket: {resumed}")
    second = Path(e.rpc("split")["manifest"])
    old = until(lambda: finished(first), description="first handoff completed")
    require(old['video_recovery']['incidents'] == [resumed], "first take lost the incident")
    until(lambda: not live().get('handover') and both_committed(), description="second take committed")
    third = Path(e.rpc("split")["manifest"])
    until(lambda: finished(second), description="second handoff completed")
    until(both_committed, description="third take committed")
    e.rpc("stop")
    for path in (second, third):
        manifest = until(lambda: finished(path), description="post-recovery split finalization")
        require(manifest['status'] == 'complete' and manifest['video_recovery']['incident_count'] == 0,
                f"old incident contaminated a later take: {manifest}")
    evidence['split_during_recovery'] = dict(busy_attempts=held['attempts_started'],
            successful_attempts=resumed['attempts_started'], later_splits=2)


def scenario_win_output_rollover(e, cfg, evidence):
    """Real replacement SPS/PPS (High -> Constrained Baseline), not mutated bytes."""
    if e.env.get("ISOCAST_TEST_RECOVERY_SUITE") != "1":
        raise Skip("requires debug recording encoder fault injection")
    fps = 12
    for crash in (False, True):
        e.stop()
        e.env['ISOCAST_TEST_ENCODER_FAILURE'] = 'iso/bars:rollover:1'
        if crash:
            e.env.pop('ISOCAST_TEST_PART_ACK_DELAY', None)
            e.env['ISOCAST_TEST_PART_COMMIT_HOLD'] = '1'
        else:
            e.env['ISOCAST_TEST_PART_ACK_DELAY'] = '1'
        e.start()
        cfg = setup(e.root, 'h264', iso=True, width=160, height=90, fps=fps)
        cfg['recording']['program']['enabled'] = False
        # Only one take per output folder; a rollover is not another public split.
        cfg['recording']['folder'] = str(e.root / ('crash-roll' if crash else 'clean-roll'))
        e.apply(cfg)
        path = Path(e.rpc('start', {'seconds': 12})['manifest'])
        def prefix_ready():
            files = (e.rpc('status').get('live') or {}).get('files', [])
            return files if len(files)==2 and all((f.get('committed_seconds') or 0)>=2 for f in files) else None
        prefix = until(prefix_ready, description='both committed movie prefixes')
        backend = next(f['encoder']['backend'] for f in prefix if f['source']=='bars')
        if not crash:
            original = path.parent / next(f['path'] for f in read_json(path)['files'] if f['source']=='bars')
            occupied = original.with_name(original.stem + '.part002' + original.suffix)
            with occupied.open('xb') as private: private.write(b'unrelated occupied name')
        e.rpc('debug_set', {'enabled': True})
        if crash:
            def committed_pending():
                m = read_json(path)
                for i in (m.get('video_recovery') or {}).get('incidents', []):
                    r = i.get('rollover') or {}
                    if r.get('state') == 'pending':
                        with open_shared(path.parent / r['path']) as f: data = f.read()
                        if len(data)>=64 and data[-64:-56]==b'\x00\x00\x00\x40icmt': return m
                return None
            pending = until(committed_pending, 20, 'committed IDR before its active checkpoint')
            e.process.kill(); e.process.wait(timeout=10)
            require(e.process.returncode != 0, 'engine exited normally in crash case')
            e.start()
            m = until(lambda: (m if m['status']=='interrupted' and all(f.get('frames') for f in m['files']) else None)
                      if (m:=read_json(path)) else None, description='both physical parts repaired')
            require([(f['path'],f['start_offset_seconds']) for f in pending['files']] ==
                    [(f['path'],f['start_offset_seconds']) for f in m['files']], 'restart changed part identity or offset')
        else:
            # The actual coordinator reply is withheld for 2 s. Observe the
            # live interval, then compare decoded PCM over it to the sibling.
            def roll_state():
                return next((r['rollover']['state'] for r in (read_json(path).get('video_recovery') or {})
                             .get('incidents', []) if r.get('rollover')), None)
            until(lambda: roll_state() == 'pending', description='outstanding delayed ack')
            pending_at = time.monotonic()
            until(lambda: roll_state() == 'committed', description='admission after delayed ack')
            evidence['observed_pending_seconds'] = time.monotonic() - pending_at
            require(evidence['observed_pending_seconds'] >= 1.5, 'ack delay was not exercised')
            m = until(lambda: finished(path), 30, 'finalization of native rollover')
            require(m['status']=='complete', f'rollover ended take early: {m}')
        require(len(m['files'])==3, f'expected two parts and one sibling: {m}')
        a,b = [f for f in m['files'] if f['source']=='bars']
        sibling, = [f for f in m['files'] if f['source']=='bars2']
        incidents = m['video_recovery']['incidents']
        incident, = [i for i in incidents if i.get('rollover')]; roll = incident['rollover']
        require([f['part']['number'] for f in (a,b)]==[1,2] and not sibling.get('part')
                and roll['state']=='committed' and roll['path']==b['path'] and roll['part']==2
                and incident['state']=='resumed', f'wrong physical/incident identity: {m}')
        cut = b['start_offset_seconds']; require(abs(cut-roll['take_offset_ns']/1e9)<1e-8 and
            round(cut*fps)==incident['end_slot_exclusive'], 'IDR offset does not end the take-clock gap')
        profiles = []
        for f in (a,b,sibling):
            movie=path.parent/f['path']; info=probe(movie); video=next(s for s in info['streams'] if s['codec_type']=='video')
            ffmpeg('-xerror','-i',movie,'-map','0','-f','null','-')
            require(int(video['nb_read_frames'])==f['frames']>0 and abs(float(video['start_time']))<.0001,
                    f'movie has wrong local zero or decoded count: {video}')
            if f.get('part'):
                profiles.append(video['profile'])
                require(abs(float(video['duration'])-f['part']['video_duration_seconds'])<.001,'wrong video extent')
        require(profiles[0] != profiles[1] and 'Baseline' in profiles[1], f'configuration did not actually change: {profiles}')
        if not crash:
            require(occupied.read_bytes()==b'unrelated occupied name' and occupied.name!=b['path'], 'rollover reused an occupied path')
            wait, = [i for i in incidents if i['fault_kind']=='rollover_ack_wait']
            def picture_slots(row):
                out = command(['ffprobe', '-v', 'error', '-select_streams', 'v:0', '-show_packets',
                               '-show_entries', 'packet=pts_time', '-of', 'json', path.parent/row['path']])
                pts = [(float(p['pts_time']) + row['start_offset_seconds'])*fps for p in json.loads(out.stdout)['packets']]
                slots = [round(t) for t in pts]
                require(all(abs(t-s)<.001 for t,s in zip(pts,slots)) and len(set(slots))==len(slots), 'off-grid or duplicate picture')
                return set(slots)
            second = picture_slots(b)
            saved = picture_slots(a) | second
            holes = set(range(min(saved), max(saved)+1)) - saved
            second_holes = set(range(min(second), max(second)+1)) - second
            def interval(i):
                return set(range(i['first_lost_slot'],i['end_slot_exclusive']))
            require(wait['owner_id']==b['source'] and wait['state']=='resumed' and bool(second_holes)
                    and second_holes==interval(wait) and len(second_holes)==wait['lost_slots']==b['dropped_frames'],
                    f'part 2 movie holes {sorted(second_holes)} vs ack ledger {wait}')
            require(holes==interval(incident)|interval(wait) and len(holes)==sum(i['lost_slots'] for i in incidents),
                    f'physical take holes {sorted(holes)} vs ledger {incidents}')
            evidence['ack_wait_holes'] = sorted(second_holes)
            last = until(lambda: (s.get('last') if s['state']=='idle' else None) if (s:=e.rpc('status')) else None,
                         description='multipart Stop report')
            require({Path(f['path']).name:f['start_offset_seconds'] for f in last['files']} ==
                    {f['path']:f['start_offset_seconds'] for f in m['files']}, 'Stop report lost a physical part or offset')
            require(sibling['dropped_frames']==0 and all(f['audio_dropouts']==0 for f in m['files']), 'unexpected sibling/PCM loss')
            require(abs(a['start_offset_seconds']+a['duration_seconds']-cut)<.0001,'old audio does not reach the cut')
            require(round((a['start_offset_seconds']+a['part']['video_duration_seconds'])*fps)==incident['first_lost_slot'], 'old video extends over the gap')
            oracle = pcm(path.parent/sibling['path'])
            for f in (a,b):
                sound=pcm(path.parent/f['path']); delta=round((f['start_offset_seconds']-sibling['start_offset_seconds'])*RATE)
                n=min(len(sound),round(f['duration_seconds']*RATE),len(oracle)-delta)
                require(n>RATE and any(abs(x)>0.1 for x in sound[:n]),'no audible markers in movie part')
                for j in range(max(0,-delta)+2048,n-2048-RATE//50,RATE//50):
                    rms=math.sqrt(sum((sound[k]-oracle[k+delta])**2 for k in range(j,j+RATE//50))/(RATE//50))
                    require(rms<.012, f'part audio misplaced at {j}: {rms}')
        exported=e.rpc('take_export_nle',{'take':str(path),'format':'otio','folder':str(e.root/'nle')})
        tracks,rate,_=nle_tracks('otio',Path(exported['path']))
        angle=next(t for t in tracks if len(t)==2)
        for clip,f in zip(angle,(a,b)):
            require(Path(clip[2]).name==f['path'] and clip[0]==round(f['start_offset_seconds']*rate) and clip[3]==0,
                    f'wrong OTIO part placement: {angle}')
        if crash:
            hashes = {f['path']:hashlib.sha256((path.parent/f['path']).read_bytes()).digest() for f in m['files']}
            e.stop(); e.start()
            require(read_json(path)==m and all(hashlib.sha256((path.parent/n).read_bytes()).digest()==d for n,d in hashes.items()),'recovery not idempotent')
        evidence.setdefault('cases',[]).append(dict(crash=crash,profiles=profiles,backend=backend,offset=cut,incident=incident,
                frames=[f['frames'] for f in (a,b)],sibling_drops=sibling.get('dropped_frames'),nle_starts=[x[0] for x in angle]))

    # Program only: no longer-lived sibling can accidentally mask a take
    # length computed from the first program file. Crash hooks exit without
    # draining or dropping the writers; examine the disk BEFORE any recovery.
    for stage in ('orphan', 'acknowledged', 'first_write_error', 'prior_finish_error', 'complete'):
        e.stop()
        e.env.pop('ISOCAST_TEST_PART_COMMIT_HOLD', None)
        e.env['ISOCAST_TEST_ENCODER_FAILURE'] = 'program/program:rollover:1'
        e.env['ISOCAST_TEST_PART_CRASH'] = stage
        for case, flag in [('first_write_error','ISOCAST_TEST_PART_FIRST_WRITE'), ('prior_finish_error','ISOCAST_TEST_PRIOR_FINISH')]:
            if stage == case: e.env[flag] = '1'
            else: e.env.pop(flag, None)
        e.start()
        e.rpc('debug_set', {'enabled': False})
        cfg = setup(e.root, 'h264', iso=False, width=160, height=90, fps=fps)
        cfg['recording']['folder'] = str(e.root / ('program-' + stage))
        e.apply(cfg)
        seconds = 10
        path = Path(e.rpc('start', {'seconds': seconds})['manifest'])
        until(lambda: any(f['source'] == 'program' and (f.get('committed_seconds') or 0) >= 2
                          for f in (e.rpc('status').get('live') or {}).get('files', [])),
              description='committed program prefix')
        first, = read_json(path)['files']
        prefix = path.parent / first['path']
        part = prefix.with_name(prefix.stem + '.part002' + prefix.suffix)
        e.rpc('debug_set', {'enabled': True})
        if stage == 'first_write_error':
            m = until(lambda: finished(path), 30, 'final manifest after first write refusal')
            kept, = m['files']
            incident, = m['video_recovery']['incidents']
            until(lambda: not part.exists(), description='owned placeholder removed after final publication')
            require(m['status'] == 'failed' and not kept.get('part') and kept['path'] == first['path']
                    and incident['rollover']['state'] == 'reserved_no_media' and not part.exists(),
                    f'failed first write left a phantom part: {m}')
            evidence[stage] = dict(prefix=kept['frames'], header_removed=True)
            continue
        if stage in ('complete', 'prior_finish_error'):
            def resumed():
                live = e.rpc('status').get('live') or {}
                return next((i for i in (live.get('video_recovery') or {}).get('incidents', [])
                             if (i.get('rollover') or {}).get('state') == 'committed'), None)
            incident = until(resumed, description='committed program rollover in live status')
            if stage == 'prior_finish_error':
                until(lambda: 'part 1 finalization failed' in (resumed() or {}).get('reason',''),
                      description='prior failure while program is still recording')
            require('committed_seconds' in incident['reason'], 'live status did not explain the prefix limit')
            m = until(lambda: finished(path), 30, 'rolled program finalization')
            a, b = m['files']
            require(m['status'] == ('failed' if stage=='prior_finish_error' else 'complete') and [f['source'] for f in (a,b)] == ['program']*2
                    and [f['part']['number'] for f in (a,b)] == [1,2], f'wrong program parts: {m}')
            physical_end = 0
            profiles = []
            for f in (a,b):
                movie = path.parent / f['path']
                info = probe(movie)
                video = next(s for s in info['streams'] if s['codec_type'] == 'video')
                ffmpeg('-xerror', '-i', movie, '-map', '0', '-f', 'null', '-')
                require(int(video['nb_read_frames']) == f['frames'] > 0, 'program part count disagrees with decode')
                profiles.append(video['profile'])
                physical_end = max(physical_end, f['start_offset_seconds'] + float(info['format']['duration']))
            require(profiles[0] != profiles[1] and 'Baseline' in profiles[1], f'wrong program profiles: {profiles}')
            require(abs(m['duration_seconds'] - physical_end) < 1/fps and abs(physical_end - seconds) <= 1/fps
                    and m['duration_seconds'] > a['duration_seconds'] + 1, 'take length truncated at program rollover')
            if stage == 'prior_finish_error':
                require('injected prior movie finish failure' in a['part']['finish_error'] and not b['part'].get('finish_error')
                        and b['duration_seconds'] > 2, 'prior failure was not confined to part one')
                evidence[stage] = dict(first_error=a['part']['finish_error'], next_frames=b['frames'], end=physical_end)
                continue
            exported = e.rpc('take_export_nle', {'take': str(path), 'format': 'otio', 'folder': str(e.root/'nle')})
            tracks, rate, _ = nle_tracks('otio', Path(exported['path']))
            angle, = tracks
            require([Path(c[2]).name for c in angle] == [a['path'], b['path']] and
                    angle[-1][1] == round(m['duration_seconds'] * rate), 'program export truncated at part one')
            evidence['program_length'] = dict(seconds=m['duration_seconds'], physical_end=physical_end,
                                              profiles=profiles, nle_end=angle[-1][1])
            continue

        require(e.process.wait(timeout=20) == 86, f'daemon did not exit at {stage}')
        before = read_json(path)
        header = part.read_bytes()
        n = int.from_bytes(header[:4], 'big')
        require(header[4:8] == b'ftyp' and b'isw2' in header[8:n] and
                header[n:] == b'\x00\x00\x00\x10free' + bytes(8), 'expected only the reservation header')
        rolls = [i['rollover'] for i in (before.get('video_recovery') or {}).get('incidents', []) if i.get('rollover')]
        require(before['status'] == 'recording' and
                [f['path'] for f in before['files']] == ([prefix.name, part.name] if stage == 'acknowledged' else [prefix.name]),
                f'crash missed {stage} publication boundary')
        require([(r['state'], r['path']) for r in rolls] == ([('pending', part.name)] if stage == 'acknowledged' else []),
                f'unexpected rollover intent before restart: {rolls}')
        e.start()
        m = until(lambda: (doc if doc['status'] == 'interrupted' else None) if (doc := read_json(path)) else None,
                  description=f'recovered {stage} prefix')
        kept, = m['files']
        video = next(s for s in probe(prefix)['streams'] if s['codec_type'] == 'video')
        require(kept['path'] == prefix.name and kept['frames'] == int(video['nb_read_frames']) > 0
                and m['duration_seconds'] > 0, 'lost verified program prefix')
        rolls = [i['rollover'] for i in (m.get('video_recovery') or {}).get('incidents', []) if i.get('rollover')]
        require([(r['state'], r['path']) for r in rolls] == ([('reserved_no_media', part.name)] if stage == 'acknowledged' else []),
                'restart adopted or promoted an empty reservation')
        digest = hashlib.sha256(prefix.read_bytes()).digest()
        e.stop(); e.start()
        require(read_json(path) == m and not kept.get('part') and
                (part.read_bytes() == header if stage=='orphan' else not part.exists()) and hashlib.sha256(prefix.read_bytes()).digest() == digest,
                f'second restart changed {stage} recovery')
        evidence[stage] = dict(header_bytes=len(header), prefix_frames=kept['frames'], rollovers=rolls)


def scenario_win_encoder_preflight(e, cfg, evidence):
    """An unavailable first provider must not fail the take or hide its replacement."""
    # The knob does not exist in production. This scenario is run against CI's debug binaries.
    if not e.env.get("ISOCAST_TEST_FAIL_NVENC_OPEN"):
        raise Skip("requires debug daemon and --engine-env ISOCAST_TEST_FAIL_NVENC_OPEN=1")
    e.apply(cfg)
    started = e.rpc("start", {})
    def selected():
        files = (e.rpc("status").get("live") or {}).get("files", [])
        return next((f["encoder"] for f in files if f.get("encoder") and f["frames"] > 2), None)
    actual = until(selected, description="live file's validated recording encoder")
    require("injected NVENC open failure" in actual["reason"] and "MediaFoundation" in actual["backend"],
            f"fallback not reported in status: {actual}")
    bad_pixels = bool(e.env.get("ISOCAST_TEST_BAD_PROBE_PICTURE"))
    if bad_pixels:
        # WARP: the first usable candidate encoded real black pixels, then
        # the ordinary software candidate encoded the original two bars.
        require(re.search(r"probe bar luma mismatch: measured \[\d+, \d+\], expected \[71, 181\]", actual["reason"])
                and "software encoder" in actual["reason"], f"missing decoded rejection and fallback: {actual}")
        require("encoder.budget" in actual["reason"], f"lost admission reason: {actual}")
    route = e.rpc("status")["windows_gpu"]
    selected_route, = [o for o in route["encoders"] if o["output"].startswith("program/")]
    require(any("injected NVENC open failure" in r for r in selected_route["fallback_reasons"]),
            f"active provider lost its rejection reason: {selected_route}")
    expected_gpu = None if "Software" in actual["backend"] else route["graphics_adapter"]
    require(selected_route["encoder_adapter"] == expected_gpu and route["render_adapter"] is not None and
            route["render_adapter"]["luid"] == route["graphics_adapter"]["luid"], f"wrong live route: {route}")
    e.rpc("stop")
    path = Path(started["manifest"])
    manifest = until(lambda: finished(path), description="fallback take finalization")
    f, = manifest["files"]
    require(manifest["status"] == "complete" and f["encoder"] == actual, f"unexpected manifest: {manifest}")
    video = next(s for s in probe(path.parent / f["path"])["streams"] if s["codec_type"] == "video")
    require(video["codec_name"] == "h264" and int(video["nb_read_frames"]) == f["frames"] > 2,
            f"not a complete H.264 fallback take: {video}")
    ffmpeg("-xerror", "-i", path.parent / f["path"], "-f", "null", "-")
    evidence.update(encoder=actual, decoded_frames=f["frames"])
    # Exercise the real CLI's typed round-trip, not only the daemon JSON.
    cli = e.binary.parent / "isocast.exe"
    report = json.loads(subprocess.run([str(cli), "doctor", "--json"], env=e.env,
                                       capture_output=True, timeout=40).stdout)
    diagnostic = report["windows_gpu"]
    require(diagnostic["session_type"] in ("service", "rdp", "console") and
            diagnostic["graphics_adapter"] == route["graphics_adapter"] and
            isinstance(diagnostic["display_devices"], list) and diagnostic["captures"] == [] and
            not diagnostic["encoders"], f"wrong idle doctor route: {diagnostic}")
    evidence.update(windows_gpu_live=route, windows_gpu_doctor=diagnostic)
    if bad_pixels:
        e.stop()
        del e.env["ISOCAST_TEST_BAD_PROBE_PICTURE"], e.env["ISOCAST_TEST_FAIL_NVENC_OPEN"]
        e.start()
        check = next(c for c in e.rpc("doctor", {"setup": cfg})["checks"] if c["name"] == "recording encoder self-test")
        require(check["status"] == "ok" and "bar luma checked" in check["detail"] and "mismatch" not in check["detail"],
                f"normal pixel probe did not pass: {check}")
        path, normal = take(e, 2)
        f, = normal["files"]
        require(normal["status"] == "complete" and f["frames"] > 0 and "mismatch" not in f["encoder"].get("reason", ""),
                f"normal take failed: {normal}")
        evidence.update(normal_doctor=check, normal_frames=f["frames"])


def scenario_win_preflight_no_decoder(e, cfg, evidence):
    """No optional pixel oracle is not evidence against an actual recording encoder."""
    if not e.env.get("ISOCAST_TEST_NO_PROBE_DECODER"):
        raise Skip("requires debug daemon with ISOCAST_TEST_NO_PROBE_DECODER=1")
    for codec in ("h264", "hevc"):
        cfg["recording"]["program"]["encoder"]["codec"] = codec
        check = next(c for c in e.rpc("doctor", {"setup": cfg})["checks"] if c["name"] == "recording encoder self-test")
        if codec == "hevc" and check["status"] == "problem" and "no usable Software Media Foundation Hevc encoder is installed" in check["detail"]:
            # An absent encoder must still refuse. The native HEVC preflight
            # test exercises three libx265 AUs on systems missing both codecs.
            evidence[codec] = {"encoder_unavailable": check["detail"]}
            continue
        note = "pixel check skipped: no synchronous Media Foundation"
        require(check["status"] == "warning" and note in check["detail"] and "bar luma checked" not in check["detail"],
                f"{codec}: wrong doctor result without a decoder: {check}")
        e.apply(cfg)
        started = e.rpc("start", {})
        def live_file():
            files = (e.rpc("status").get("live") or {}).get("files", [])
            return next((f for f in files if f.get("frames", 0) > 3), None)
        live = until(live_file, description="recording despite absent pixel decoder")
        actual = live["encoder"]
        require(note in actual["reason"], f"{codec}: missing live skip note: {actual}")
        route, = e.rpc("status")["windows_gpu"]["encoders"]
        require(any(note in r for r in route["fallback_reasons"]), f"{codec}: route lost skip: {route}")
        e.rpc("stop")
        path = Path(started["manifest"])
        manifest = until(lambda: finished(path), description="completed take without decoder")
        f, = manifest["files"]
        require(manifest["status"] == "complete" and f["encoder"] == actual, f"unexpected manifest: {manifest}")
        stream = next(s for s in probe(path.parent / f["path"])["streams"] if s["codec_type"] == "video")
        require(stream["codec_name"] == codec and int(stream["nb_read_frames"]) == f["frames"] > 3,
                f"{codec}: fallback altered stream: {stream}")
        evidence[codec] = {"doctor": check, "encoder": actual, "decoded_frames": f["frames"]}


def scenario_win_output_scale(e, cfg, evidence):
    """One canvas, not three canvases: letterboxed program recording, then
    a full-size program take with two differently sized RTMP receivers."""
    w, h, fps = (cfg["canvas"][k] for k in ("width", "height", "fps"))
    # Known static bars: inspect decoded pixels against their source, not
    # one encode against another encode with the same range/matrix mistake.
    bars = ["bfbfbf", "bfbf00", "00bfbf", "00bf00", "bf00bf", "bf0000", "0000bf", "202020"]
    cfg["sources"] = [s for s in cfg["sources"] if s["id"] == "tone"]
    cfg["sources"].extend({"id": "bar" + str(i), "kind": "color", "color": "#" + color,
                           "width": 16, "height": 16} for i, color in enumerate(bars))
    cfg["scenes"][0]["items"] = [item("bar" + str(i), (i / 8, 0, 1 / 8, 1), (w, h)) for i in range(8)]
    cfg["recording"]["iso"].update(enabled=False, sources=[])

    def check_bars(path, width, height, letterboxed=False):
        raw = ffmpeg("-i", path, "-map", "0:v:0", "-an", "-vf",
                     "scale=in_color_matrix=bt709:in_range=tv,format=rgb24", "-frames:v", "1",
                     "-f", "rawvideo", "-").stdout
        require(len(raw) == width * height * 3, f"{path.name}: unexpected decoded size")

        def pixel(x, y):
            p = (y * width + x) * 3
            return raw[p:p + 3]

        samples = [pixel((2 * i + 1) * width // 16, height // 2) for i in range(8)]
        err = max(abs(a - b) for want, got in zip(bars, samples) for a, b in zip(bytes.fromhex(want), got))
        require(err <= 8, f"{path.name}: incorrect Rec.709 studio-range bars: {[s.hex() for s in samples]}")
        if letterboxed:
            require(max(pixel(width // 16, y)[c] for y in (1, height - 2) for c in range(3)) <= 8,
                    f"{path.name}: did not letterbox the wider canvas")
        evidence.setdefault("bar_rgb_max_errors", {})[path.name] = err

    program = cfg["recording"]["program"]
    side = h // 4 * 2
    program.update(width=side, height=side)
    if program["encoder"]["codec"] == "hevc":
        program["encoder"]["bit_depth"] = 10
    e.apply(cfg)
    path, manifest = take(e, 3)
    evidence["files"] = check_take(path, manifest, 3, expected_sources=["program"], evidence=evidence)
    scaled_file = manifest_file(path, manifest, "program")[0]
    check_bars(scaled_file, side, side, letterboxed=True)
    if program["encoder"].pop("bit_depth", None) == 10:
        video = next(s for s in probe(scaled_file)["streams"] if s["codec_type"] == "video")
        require(video["pix_fmt"] == "yuv420p10le" and video["profile"] == "Main 10", f"not Main10: {video}")

    del program["width"], program["height"]
    destinations = [("main", w // 4 * 2, h // 4 * 2), ("small", w // 6 * 2, h // 6 * 2)]
    listeners = {}
    try:
        for name, width, height in destinations:
            with socket.socket() as s:
                s.bind(("127.0.0.1", 0))
                port = s.getsockname()[1]
            server = f"rtmp://127.0.0.1:{port}/live"
            settings = {"service": "custom", "server": server, "width": width, "height": height}
            if name == "main":
                cfg["stream"] = dict(settings, video_bitrate_kbps=4000, destinations=[])
            else:
                cfg["stream"]["destinations"].append(dict(settings, id=name))
            output = e.root / f"{name}.flv"
            with (e.root / f"{name}-listener.log").open("wb") as log:
                listeners[name] = subprocess.Popen(["ffmpeg", "-nostdin", "-v", "warning", "-listen", "1", "-i",
                                                    server + "/e2e", "-c", "copy", "-y", str(output)],
                                                   stdout=log, stderr=log)
        e.apply(cfg)
        e.rpc("stream_start")
        path, manifest = take(e, 3)
        final = e.rpc("stream_stop")["destinations"]
        evidence["files"].extend(check_take(path, manifest, 3, expected_sources=["program"], evidence=evidence))
        check_bars(manifest_file(path, manifest, "program")[0], w, h)
        for name, width, height in destinations:
            listeners[name].wait(timeout=10)
            status = next(s for s in final if s["id"] == name)
            output = e.root / f"{name}.flv"
            video = next(s for s in probe(output)["streams"] if s["codec_type"] == "video")
            count = int(video["nb_read_frames"])
            require((video["width"], video["height"]) == (width, height), f"{name}: wrong stream size")
            require(count == status["frames_sent"] and count >= 3 * fps - 1, f"{name}: {count} frames, {status}")
            require(video_packets_accounted(video_times(output), fps, status["dropped_frames"]),
                    f"{name}: received timestamps have unaccounted gaps")
            loss = vertical_stream_loss(status, fps, evidence)
            evidence.setdefault("drops", {})[name] = dict(loss, output=name)
            check_bars(output, width, height)
    finally:
        for listener in listeners.values():
            if listener.poll() is None:
                listener.kill()
            listener.wait(timeout=5)


def scenario_linux_record(e, cfg, evidence):
    """Headless bootstrap: two synthetic ISOs and the composited program,
    each with tone, plus a small color layer away from the oracle samples.
    The same frame oracle and A/V checks use the invocation's loss policy."""
    layout = win_setup(e, cfg)
    w, h = cfg["canvas"]["width"], cfg["canvas"]["height"]
    cfg["developer"] = {"enabled": True}
    cfg["sources"].append({"id": "color", "name": "Color", "kind": "color", "color": "#ff0000"})
    cfg["scenes"][0]["items"].append(item("color", (0.8, 0.05, 0.1, 0.1), (w, h)))
    e.apply(cfg)
    path, manifest = take(e, 5)
    evidence["files"] = check_take(path, manifest, 5, evidence=evidence)
    require(len(manifest["files"]) == 3, "expected program and two ISO files")
    evidence["t0"] = check_pattern_take(e, path, manifest, cfg, layout, evidence)
    program, _ = manifest_file(path, manifest, "program")
    colors = ffmpeg("-i", program, "-an", "-vf",
                    f"crop={int(w * .06)}:{int(h * .06)}:{int(w * .82)}:{int(h * .07)},scale=1:1",
                    "-fps_mode", "passthrough", "-pix_fmt", "rgb24", "-f", "rawvideo", "-").stdout
    require(colors and len(colors) % 3 == 0, "color layer: no decoded RGB samples")
    require(all(r > 180 and g < 70 and b < 70 for r, g, b in zip(colors[::3], colors[1::3], colors[2::3])),
            "color layer: program did not render the red source")
    evidence["color_frames"] = len(colors) // 3
    evidence["discovery_without_bus"] = discovery_without_bus(e)


def discovery_without_bus(e):
    """`isocast engines --discover` with no reachable system bus fails at
    once with the connect-by-address hint, well before its 5 s listen."""
    cli = os.environ.get("ISOCAST_E2E_CLI")
    if not cli or not Path(cli).is_file():
        return "skipped: no isocast CLI"
    env = dict(e.env, DBUS_SYSTEM_BUS_ADDRESS="unix:path=/nonexistent",
               ISOCAST_REMOTE_CLIENT=str(e.root / "discovery-client.json"))
    started = time.monotonic()
    try:
        out = subprocess.run([cli, "engines", "--discover", "--timeout", "5"], env=env, cwd=str(e.root),
                             stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=2.5)
    except subprocess.TimeoutExpired as exc:
        raise Failure("discovery without a system bus hung past 2.5 s") from exc
    seconds = round(time.monotonic() - started, 3)
    require(out.returncode != 0, f"discovery without a system bus exited 0: {out.stdout.strip()[-400:]}")
    require("connect by address" in out.stderr,
            f"discovery without a system bus: no connect-by-address hint: {out.stderr.strip()[-400:]}")
    require("panicked" not in out.stderr, f"discovery without a system bus panicked: {out.stderr.strip()[-400:]}")
    require(seconds <= 2.5, f"discovery without a system bus took {seconds}s, expected about 2s at most")
    return {"exit": out.returncode, "seconds": seconds}


def scenario_linux_media(e, cfg, evidence):
    """A looping H.264/AAC MP4 (the frame oracle's block-code clip, 4 s, a
    marker burst each second) decoded by GStreamer beside a synthetic test
    pattern and tone: the program and the media ISO, every picture against
    the oracle across the loops (the file's own rate, clean wraps), each
    burst within a frame of its picture, under the run's loss policy.
    60 s strict; 15 s with --hosted."""
    w, h, fps = (cfg["canvas"][k] for k in ("width", "height", "fps"))
    seconds, clip_seconds = (15 if evidence["capabilities"].get("hosted") else 60), 4
    clip = oracle_fixture(e.root.parent / "fixtures", ORACLE_TAGS["a"], fps, clip_seconds, mp4=True)
    count = clip_seconds * fps
    cfg["developer"] = {"enabled": True}
    cfg["sources"] = [s for s in cfg["sources"] if s["id"] in ("bars", "tone")]
    cfg["sources"].append({"id": "a", "name": "Clip", "kind": "media", "files": [str(clip)], "loop": True})
    layout = [("a", (0, 0, 0.75, 0.75))]
    cfg["scenes"] = [{"id": "main", "name": "Main",
                      "items": [item("a", layout[0][1], (w, h)), item("bars", (0.75, 0.75, 0.25, 0.25), (w, h))]}]
    cfg["recording"]["program"]["audio"] = ["a"]
    cfg["recording"]["iso"].update(enabled=True, sources=["a"], audio=["a"])
    e.apply(cfg)
    path, manifest = take(e, seconds)
    evidence["files"] = check_take(path, manifest, seconds, evidence=evidence)
    require(sorted(f["source"] for f in manifest["files"]) == ["a", "program"],
            f"files: {[f['source'] for f in manifest['files']]}")
    evidence["clip"] = {"frames": count, "loops": round(seconds / clip_seconds, 2)}
    evidence["t0"] = check_record_take(e, path, manifest, cfg, layout, count, evidence, isos=("a",))
    require(any(b'isocast-media: decoded sample stdout probe' in log.read_bytes()
                for log in e.sock.with_suffix('.logs').glob('*-native-*.jsonl')),
            'did not observe GStreamer stdout on the diagnostic stream during decoding')


def scenario_linux_media_faults(e, cfg, evidence):
    """Kill or freeze the owned video decoder, not a probe; the take and other ISO survive."""
    w, h, fps = (cfg['canvas'][k] for k in ('width', 'height', 'fps'))
    clip = oracle_fixture(e.root.parent / 'fixtures', ORACLE_TAGS['a'], fps, 4, mp4=True)
    cfg['sources'] = [s for s in cfg['sources'] if s['id'] in ('bars', 'tone')]
    cfg['sources'].append({'id': 'clip', 'kind': 'media', 'files': [str(clip)], 'loop': True})
    cfg['scenes'] = [{'id': 'main', 'items': [item('bars', (0, 0, 1, 1), (w, h)),
                                             item('clip', (0, 0, .25, .25), (w, h))]}]
    cfg['recording']['program']['audio'] = ['tone']
    cfg['recording']['iso'].update(enabled=True, sources=['bars', 'clip'], audio=['tone'])
    # As in video_recovery: do not count the deliberately killed ISO's missing
    # slots as a suite throughput regression. Still check every unaffected file.
    cfg['recording']['folder'] = str(e.root / 'fault-recordings')
    def frames():
        live = e.rpc('status').get('live') or {}
        return {f['source']: f['frames'] for f in live.get('files', [])}

    def wait_written(sig, phase, targets):
        last = {}
        def ready():
            nonlocal last
            last = frames()
            return all(last.get(src, -1) >= n for src, n in targets.items())
        try:
            until(ready, 15, f'{sig.name} {phase}')
        except Failure as exc:
            raise Failure(f'{sig.name} {phase}: wanted written frames {targets}, observed {last}') from exc

    for sig in (signal.SIGKILL, signal.SIGSTOP):
        e.apply(cfg)
        started = e.rpc('start', {})
        # Start acknowledges file creation, not a frame on the take clock.
        wait_written(sig, 'before injection', {'program': 3, 'bars': 3, 'clip': 3})
        def inject():
            # Only children of threads of this private test daemon. pidfd prevents
            # signalling a reused PID if the 4-second clip wraps while inspecting.
            for children in Path(f'/proc/{e.process.pid}/task').glob('*/children'):
                try:
                    pids = children.read_text().split()
                except FileNotFoundError:
                    continue
                for pid in pids:
                    fd = None
                    try:
                        fd = os.pidfd_open(int(pid))
                        argv = Path(f'/proc/{pid}/cmdline').read_bytes().split(b'\0')
                        if argv[1:] == [b'media-v3', b'video', str(e.process.pid).encode(), b'']:
                            signal.pidfd_send_signal(fd, sig)
                            return pid
                    except (FileNotFoundError, ProcessLookupError):
                        pass
                    finally:
                        if fd is not None:
                            os.close(fd)
        pid = until(inject, description='active video decoder')
        def failure():
            return next((s.get('error') for s in e.rpc('status')['engine']['sources']
                         if s['source'] == 'clip' and s['state'] == 'failed'), None)
        why = until(failure, 18, f'clip failure after {sig.name}')
        require(('timed out' if sig == signal.SIGSTOP else 'isocast-media') in why, why)
        checks = e.rpc('doctor', {})['checks']
        require(any(c['name'] == 'live media source: clip' and c['detail'] == why for c in checks), checks)
        baseline = frames()
        survivors = ('program', 'bars')
        require(all(s in baseline for s in survivors), f'{sig.name}: missing live writers after fault: {baseline}')
        targets = {s: baseline[s] + 2 * fps for s in survivors}
        wait_written(sig, 'after decoder failure', targets)
        e.rpc('stop')
        m = until(lambda: finished(started['manifest']), 15, 'unrelated outputs finalized')
        losses = drop_metrics(m)
        evidence[sig.name] = {'pid': pid, 'reason': why, 'losses': losses, 'frames_at_failure': baseline}
        for src in survivors:
            f, = [f for f in m['files'] if f['source'] == src]
            loss, = [row for row in losses if row['output'] == Path(f['path']).name]
            check_loss(loss, loss_limit(evidence))
            file = Path(started['manifest']).parent / f['path']
            duration = float(command(['ffprobe','-v','error','-show_entries','format=duration','-of',
                                      'default=nw=1:nk=1',file]).stdout)
            require(duration >= 2 and f['frames'] >= targets[src],
                    f'{sig.name} {src}: finalized {duration:.6f} s / {f["frames"]} frames; '
                    f'expected >=2 s and >={targets[src]} frames ({baseline[src]} at fault, {fps} fps): {file}')
            pixel = ffmpeg('-sseof','-1','-i',file,'-an','-frames:v','1','-vf','scale=1:1',
                           '-pix_fmt','rgb24','-f','rawvideo','-').stdout
            require(len(pixel) == 3, f'{file}: no picture near end of finalized file')
        e.rpc('engine', {'running': False})


def scenario_linux_shared_frames(e, cfg, evidence):
    """Linux local previews through shared memory (`frames --shared`, memfd
    slots handed over with SCM_RIGHTS): the engine's readback, direct copy
    and slot release, checked from the client's own mapping. Two viewers at
    once (the program as drawn, and scaled to another width), each must get
    whole pictures, never black or undrawn, and keep getting them after
    every slot was used (slots come back), on one ring each."""
    cli = os.environ.get("ISOCAST_E2E_CLI")
    if not cli:
        raise Skip("no isocast CLI (--cli) to watch shared frames")
    w = cfg["canvas"]["width"]
    e.apply(cfg)
    e.rpc("engine", {"running": True})
    time.sleep(1)
    seconds = 6
    viewers = [(width, subprocess.Popen(
        [cli, "frames", "program", "--shared", "--check", "--width", str(width), "--fps", "30",
         "--seconds", str(seconds), "--json"],
        env=e.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True))
        for width in (w, max(160, w // 2))]
    results = {}
    for width, process in viewers:
        out, err = process.communicate(timeout=seconds + 30)
        require(process.returncode == 0, f"frames --shared --width {width} failed: {err.strip()[-400:]}")
        got = json.loads(out)
        check = got.get("check") or {}
        slots = got["slots"]
        results[width] = {k: got.get(k) for k in ("transport", "width", "height", "slots", "frames", "fps",
                                                  "lag_ms", "rings", "problems")}
        results[width]["check"] = {k: check.get(k) for k in ("frames_read_back", "black_frames", "undrawn_frames")}
        require(got["transport"] == "memfd-unix", f"transport {got['transport']!r}, not memfd-unix")
        require(got["ok"] and got["ended"] is None, f"width {width}: {got['problems']} (ended: {got['ended']})")
        require(check.get("black_frames") == 0 and check.get("undrawn_frames") == 0
                and check.get("frames_read_back") == got["frames"],
                f"width {width}: pictures read back from shared memory: {check}")
        # Several times as many frames as slots: each slot came back and
        # was drawn again, and the stream never stalled on held slots.
        require(got["frames"] >= max(4 * slots, seconds * 5),
                f"width {width}: {got['frames']} frames through {slots} slots in {seconds} s")
        require(len(got["rings"]) == 1 and got["rings"][0]["width"] <= width,
                f"width {width}: rings {got['rings']}")
    evidence["shared_frames"] = results

    # Hold both slots, so there are no already-submitted frames when the
    # governor pauses. Releasing them must not produce frames at 0 fps;
    # the same subscription must deliver later ticks in order on resume.
    import select
    with engine_connection(e.sock, 10) as sock, sock.makefile("rb", buffering=0) as reader:
        def send(value):
            sock.sendall((json.dumps(value) + "\n").encode())

        def frame():
            return json.loads(reader.readline())["frame"]

        def release(f):
            send({"release": {k: f[k] for k in ("generation", "slot")}})

        send({"jsonrpc": "2.0", "id": 1, "method": "shared_frames", "params": {"slots": 2}})
        require(json.loads(reader.readline())["result"]["transport"] == "memfd-unix", "not a memfd viewer")
        require(json.loads(reader.readline())["ring"]["slots"] == 2, "expected two shared slots")
        held = [frame(), frame()]
        require(held[0]["slot"] != held[1]["slot"], "reused a slot before release")
        ticks = [f["time"] for f in held]
        e.rpc("start", {})
        try:
            paused = e.rpc("status")["governor"]["preview_fps"] == 0
            for f in held:
                release(f)
            if paused:  # Software GPU protecting the take, as in hosted CI.
                require(not select.select([sock], [], [], .5)[0], "shared frames continued at 0 fps")
        finally:
            e.rpc("stop")
        for _ in range(8):
            f = frame()
            ticks.append(f["time"])
            release(f)
        require(all(b > a for a, b in zip(ticks, ticks[1:])), f"shared ticks out of order: {ticks}")
        evidence["shared_frame_order"] = {"ticks": ticks, "pause_checked": paused}


LINUX_CAPTURE_SECONDS = 15
LINUX_CAPTURE_CANVAS = (480, 270)


def tone_amplitudes(path, stream, freqs=(440, 880), start=2, seconds=2):
    """The amplitude of each frequency in one audio stream of `path` (mono
    mix of its channels, `seconds` from `start`), by a single-bin DFT."""
    raw = ffmpeg("-ss", start, "-i", path, "-t", seconds, "-map", f"0:a:{stream}", "-ac", "1",
                 "-ar", RATE, "-f", "f32le", "-").stdout
    samples = array.array("f")
    samples.frombytes(raw)
    if sys.byteorder != "little":
        samples.byteswap()
    require(len(samples) >= RATE, f"{Path(path).name} audio {stream}: under a second of sound")
    out = {}
    for f in freqs:
        w = 2 * math.pi * f / RATE
        re_ = sum(x * math.cos(w * i) for i, x in enumerate(samples))
        im = sum(x * math.sin(w * i) for i, x in enumerate(samples))
        out[f] = 2 * math.hypot(re_, im) / len(samples)
    return out


def scenario_linux_audio_exclude(e, cfg, evidence):
    """Tap the actual output as a positive control: a silent monitor is not rejection."""
    sink = os.environ.get("ISOCAST_FIXTURE_DESKTOP_SINK")
    if not sink:
        raise Skip("needs the Linux capture fixture's private default sink")
    require(command(["pactl", "get-default-sink"]).stdout.decode().strip() == sink,
            "refusing to record a non-fixture default output")
    e.stop()
    e.env["ISOCAST_MONITOR_SILENT"] = "0"
    e.start()
    cfg = setup(e.root, "h264", iso=True, width=320, height=180, fps=30)
    cfg["sources"][-1].update(frequency=880, audio={"monitor": "monitor_and_output"})
    cfg["sources"].append({"id": "desktop", "kind": "system_audio"})
    cfg["recording"]["iso"].update(sources=["bars"], audio=["desktop"])
    e.apply(cfg)
    e.rpc("engine", {"running": True})
    # No external app at open. The first player disappears; its replacement
    # has a new stream and pid and must be picked up without reopening capture.
    for hz in (330, 440):
        player = subprocess.Popen(["ffmpeg", "-nostdin", "-loglevel", "error", "-re", "-f", "lavfi",
                                   "-i", f"sine=frequency={hz}:sample_rate={RATE}", "-ac", "2",
                                   "-f", "pulse", "-device", sink, "external-tone"])
        try:
            path = Path(e.rpc("start", {"seconds": 7})["manifest"])
            control = e.root / f"sink-{hz}.wav"
            ffmpeg("-f", "pulse", "-i", f"{sink}.monitor", "-t", 4, control)
            manifest = until(lambda: finished(path), 20, "desktop audio recording")
            iso, _ = manifest_file(path, manifest, "bars")
            output, desktop = (tone_amplitudes(f, 0, (hz, 880), start=2, seconds=1) for f in (control, iso))
            require(min(output[hz], output[880], desktop[hz]) > .002,
                    f"missing playback in control or desktop ISO: {output}, {desktop}")
            rejection = 20 * math.log10(output[880] / max(desktop[880], 1e-12))
            evidence[str(hz)] = {"sink": output, "desktop_iso": desktop, "rejection_db": rejection}
            require(rejection > 60, f"monitor rejected by only {rejection:.1f} dB")
            require(player.poll() is None, "external player stopped early")
            until(lambda: e.rpc("status")["state"] == "idle", 10, "idle after capture")
        finally:
            player.terminate()
            player.wait(timeout=10)


def scenario_win_audio_exclude(e, cfg, evidence):
    """Two real players. One launches the private engine (so its tree *can*
    exclude both); the other is independent. Compare three simultaneous
    WASAPI captures, including the actual audio rendered by the monitor."""
    if not WINDOWS:
        raise Skip("Windows process loopback")
    devices = e.rpc("devices")
    if not any(d.get("default") for d in devices["audio_outputs"]):
        raise Skip("no default render endpoint for the two tone players and monitor")
    require(not any(a["playing"] for a in devices["audio_apps"]),
            "unrelated audio is active; refusing to record it")
    # A distinct executable identity for the named launcher, without an
    # installed fixture or build. Use only scratch files and child processes.
    e.stop()
    python = Path(sys.base_prefix) / "python.exe"
    launcher = e.root / "excluded-tone.exe"
    kept = e.root / "retained-tone.exe"
    shutil.copyfile(python, launcher)
    shutil.copyfile(python, kept)
    e.env.update(PYTHONHOME=sys.base_prefix, PATH=str(python.parent) + os.pathsep + e.env["PATH"],
                 ISOCAST_MONITOR_SILENT="0")
    sounds = []
    for hz in (440, 880):
        wav = e.root / f"{hz}.wav"
        # The retained carrier also marks whole seconds: measure its decoded
        # cadence against the player's PCM, not against packet arrival times.
        gate = "*lt(mod(t\\,1)\\,0.8)" if hz == 880 else ""
        ffmpeg("-f", "lavfi", "-i", f"aevalsrc=0.10*sin(2*PI*{hz}*t){gate}:s={RATE}", "-t", 30, wav)
        sounds.append(str(wav))
    play = ("import winsound,sys,subprocess,time; "
            "winsound.PlaySound(sys.argv[1],winsound.SND_FILENAME|winsound.SND_LOOP|winsound.SND_ASYNC); ")
    e.start(launcher=[str(launcher), "-c", play + "sys.exit(subprocess.call(sys.argv[2:]))", sounds[0]])
    other = subprocess.Popen([str(kept), "-c", play + "time.sleep(60)", sounds[1]], env=e.env)
    try:
        # No monitoring or exclusions: verify the stream the recorder opened,
        # not just the plan. The same live take must refuse an unsafe toggle.
        default = setup(e.root, "h264", width=320, height=180, fps=30)
        default["sources"].append({"id": "desktop", "kind": "system_audio"})
        default["recording"]["program"]["audio"] = ["desktop"]
        e.apply(default)
        path = Path(e.rpc("start", {"seconds": 4})["manifest"])
        def modes():
            return {s["source"]: s for s in e.rpc("status")["engine"]["sources"]}
        desktop = modes()["desktop"]
        require(desktop.get("capture_mode") == "endpoint-loopback" and not desktop.get("warning"),
                f"wrong default capture: {desktop}")
        require(any('mode=endpoint-loopback' in log.read_text(encoding="utf-8") for log in
                    e.sock.with_suffix(".logs").glob("*.jsonl")), "native endpoint loopback did not start")
        default["sources"][-2]["audio"] = {"monitor": "monitor_and_output"}
        try:
            e.apply(default)
            raise Failure("live endpoint accepted monitor feedback")
        except RpcError as err:
            require("endpoint-loopback" in str(err) and "monitor" in str(err), f"unexpected refusal: {err}")
        until(lambda: finished(path), 20, "endpoint take finished")
        until(lambda: e.rpc("status")["state"] == "idle", 10, "idle after endpoint take")
        e.rpc("engine", {"running": False})
        evidence["default_capture_mode"] = desktop["capture_mode"]
        cfg = setup(e.root, "h264", iso=True, width=320, height=180, fps=30)
        cfg["sources"][-1].update(frequency=660, audio={"monitor": "monitor_and_output"})
        cfg["sources"] += [{"id": "selfonly", "kind": "system_audio"},
                           {"id": "filtered", "kind": "system_audio", "exclude": [str(launcher)]},
                           {"id": "rendered", "kind": "application_audio", "app": str(e.binary)}]
        cfg["recording"]["program"]["audio"] = ["filtered"]
        cfg["recording"]["iso"].update(audio=[], source_audio={"bars": ["selfonly"], "bars2": ["rendered"]})
        def preflight():
            return next(c for c in e.rpc("doctor", {"setup": cfg})["checks"] if c["name"] == "source: filtered")
        # Don't apply an impossible filter to the live engine.
        cfg["sources"][-2]["exclude"] = [str(kept)]
        refused = preflight()
        require(refused["status"] == "problem" and "system_audio.exclude[0]" in refused["detail"]
                and "application_audio" in refused["detail"], f"independent tree wasn't refused: {refused}")
        cfg["sources"][-2]["exclude"] = [launcher.name]
        planned = preflight()
        require(planned["status"] == "ok" and "#251" not in planned["detail"] and launcher.name in planned["detail"]
                and "Isocast" in planned["detail"], f"unexpected named tree preflight: {planned}")
        evidence["preflight"] = {"unrelated": refused["status"], "launcher": planned["status"]}
        e.apply(cfg)
        e.rpc("engine", {"running": True})
        path = Path(e.rpc("start", {"seconds": 10})["manifest"])
        actual = modes()
        for source in ("filtered", "selfonly"):
            require(actual[source].get("capture_mode") == "process-loopback-exclude" and
                    not actual[source].get("warning"), f"unexpected live warning or mode: {actual[source]}")
        def playing():
            m = (e.rpc("status").get("live") or {}).get("monitor") or {}
            return m["frames"] if m.get("frames", 0) > RATE else None
        evidence["monitor_frames"] = until(playing, 10, "audio rendered to default output")
        manifest = until(lambda: finished(path), 30, "desktop exclusion recording")
        spectra = evidence["amplitudes"] = {}
        for source in ("program", "bars", "bars2"):
            file, _ = manifest_file(path, manifest, source)
            spectra[source] = tone_amplitudes(file, 0, freqs=(440, 660, 880), start=2, seconds=1)
        filtered, desktop, monitor = (spectra[s] for s in ("program", "bars", "bars2"))
        require(min(desktop[440], desktop[880], filtered[880], monitor[660]) > .001,
                f"both real players and actual monitor playback must be audible in controls: {spectra}")
        require(filtered[440] < .01 * desktop[440] and
                max(filtered[660], desktop[660]) < .01 * monitor[660],
                f"excluded tones must be at least 40 dB below their controls: {spectra}")
        require(other.poll() is None and e.process.poll() is None, "tone fixture exited early")
        file, _ = manifest_file(path, manifest, "program")
        samples = pcm(file)
        peak = max(map(abs, samples))
        onsets = [t for t in beep_onsets((s / peak for s in samples)) if .2 < t < 9.8]
        require(len(onsets) >= 8, f"too few retained tone markers: {onsets}")
        span = onsets[-1] - onsets[0]
        evidence["retained_cadence"] = {"intervals": len(onsets) - 1, "seconds": span,
                                        "error_ms": round(1000 * (span - len(onsets) + 1), 3)}
    finally:
        other.terminate()
        other.wait(timeout=5)


def scenario_win_app_audio(e, cfg, evidence):
    """Read-only listing everywhere; real capture only with explicit scratch
    tone-player executables. Each player is a separate copy of ffplay, so its
    stable executable id identifies one tone across PID changes. The existing
    engine's ISO files need pictures: one test-pattern ISO carries the app;
    the program carries desktop audio so both outputs stay independent."""
    if not WINDOWS:
        raise Skip("Windows only")
    devices = e.rpc("devices")
    apps = devices["audio_apps"]
    evidence["listed_apps"] = len(apps)
    for app in apps:
        require(app.get("id") and app.get("binary") and app.get("name") and app["pid"] > 0,
                f"invalid audio application identity: {app}")
    players = [os.environ.get("ISOCAST_APP_TONE_PLAYER"), os.environ.get("ISOCAST_OTHER_TONE_PLAYER")]
    if not all(players):
        evidence["capture"] = "SKIP: no explicit scratch tone players; hosted Windows has no audio devices"
        return
    require(all(Path(p).is_file() for p in players) and players[0] != players[1],
            "tone fixtures must be two distinct scratch ffplay executables")
    seconds = int(os.environ.get("ISOCAST_APP_AUDIO_SECONDS", "120"))
    require(seconds >= 20, "app audio qualification needs at least 20 seconds")
    processes, logs = [], []

    def start_player(index):
        log = open(e.root / f"player-{index}-{len(processes)}.log", "wb")
        logs.append(log)
        process = subprocess.Popen([players[index], "-nodisp", "-autoexit", "-loglevel", "warning",
                                    "-f", "lavfi", "-i", f"sine=frequency={440 if index == 0 else 880}:sample_rate=44100",
                                    "-volume", "25"], stdin=subprocess.DEVNULL, stdout=log, stderr=log)
        processes.append(process)
        return process

    try:
        # The selected application is deliberately absent when the source opens.
        cfg["canvas"].update(width=480, height=270, fps=30)
        for source in cfg["sources"]:
            if source["kind"] == "test_pattern":
                source.update(width=480, height=270, fps=30)
        cfg["sources"] = [s for s in cfg["sources"] if s["id"] != "tone"]
        cfg["sources"] += [{"id": "app", "kind": "application_audio", "app": str(Path(players[0]).resolve())},
                           {"id": "desktop", "kind": "system_audio"}]
        cfg["scenes"][0]["items"] = [item("bars", (0, 0, 1, 1), (480, 270))]
        cfg["recording"]["program"].update(enabled=True, audio=["desktop"])
        cfg["recording"]["iso"].update(enabled=True, sources=["bars"], audio=[],
                                      source_audio={"bars": ["app"]})
        e.apply(cfg)
        e.rpc("engine", {"running": True})
        start_player(1)
        selected = start_player(0)

        def listed_player():
            return next((a for a in e.rpc("devices")["audio_apps"]
                         if a.get("id", "").lower() == str(Path(players[0]).resolve()).lower()
                         and a.get("pid") == selected.pid), None)

        evidence["selected_app"] = until(listed_player, 15, "selected tone app listed")
        time.sleep(2)
        started = e.rpc("start", {"seconds": seconds})
        time.sleep(seconds // 4)
        # Suspend only our own fixture through Popen's pinned process handle:
        # unlike a restart this leaves a live process that renders no audio.
        import ctypes
        native = ctypes.WinDLL("ntdll")
        for function in (native.NtSuspendProcess, native.NtResumeProcess):
            function.argtypes = [ctypes.c_void_p]
            function.restype = ctypes.c_long
        require(native.NtSuspendProcess(int(selected._handle)) >= 0, "could not pause tone fixture")
        try:
            time.sleep(2)
        finally:
            require(native.NtResumeProcess(int(selected._handle)) >= 0, "could not resume tone fixture")
        evidence["live_pause_seconds"] = 2
        time.sleep(seconds // 2 - seconds // 4 - 2)
        old_pid = selected.pid
        selected.terminate()
        selected.wait(timeout=5)
        time.sleep(1)
        selected = start_player(0)
        evidence["restart"] = {"before_pid": old_pid, "after_pid": selected.pid}
        evidence["restarted_app"] = until(listed_player, 15, "restarted tone app listed")
        path = Path(started["manifest"])
        manifest = until(lambda: finished(path), seconds + 35, "app-audio take finalized")
        evidence["files"] = check_take(path, manifest, seconds, expected_sources=["program", "bars"], evidence=evidence)
        loss = {f["source"]: {k: f.get(k, 0) for k in ("dropped_frames", "repeated_frames", "audio_dropouts")}
                for f in manifest["files"]}
        evidence["loss"] = loss
        require(not any(v for counters in loss.values() for v in counters.values()), f"app audio loss: {loss}")
        app_file, _ = manifest_file(path, manifest, "bars")
        desktop_file, _ = manifest_file(path, manifest, "program")
        spectra = {}
        for label, at in (("before", 3), ("resumed", seconds // 4 + 3), ("after", seconds // 2 + 5)):
            own = tone_amplitudes(app_file, 0, start=at)
            desktop = tone_amplitudes(desktop_file, 0, start=at)
            spectra[label] = {"app": own, "desktop": desktop}
            require(own[440] > 0.001 and own[880] < own[440] * 0.01,
                    f"{label}: app ISO must contain 440Hz with 880Hz at least 40dB down: {own}")
            require(desktop[440] > 0.001 and desktop[880] > 0.001,
                    f"{label}: desktop program must contain both tones: {desktop}")
        evidence["tone_amplitudes"] = spectra
        paused = tone_amplitudes(app_file, 0, start=seconds // 4 + 0.5, seconds=1)
        evidence["paused_tone_amplitudes"] = paused
        require(paused[440] < spectra["before"]["app"][440] * 0.01,
                f"live suspended app must become silent without ending the source: {paused}")
        require(selected.poll() is None, "restarted tone player exited before qualification ended")
    finally:
        for process in processes:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
        for log in logs:
            log.close()


def camera_code_span(file, codes, period, slots):
    """Two unsynchronized 30 Hz clocks can sample either side of a source tick."""
    require(codes and all(0 <= n < period for n in codes), f"{file.name}: missing or out-of-range camera code")
    span = low = high = 0
    for i in range(1, len(codes)):
        step = (codes[i] - codes[i - 1]) % period
        require(step <= 2, f"{file.name}: camera code jump of {step} at picture {i}: {codes[i - 1:i + 1]}")
        require(i < 2 or not (codes[i] == codes[i - 1] == codes[i - 2]),
                f"{file.name}: three identical camera codes ending at picture {i}: {codes[i]}")
        span += step
        # A repeat and a skipped code may alternate with ordinary steps
        # between them. They must not accumulate into persistent loss/gain.
        low, high = min(low, span - i), max(high, span - i)
        require(high - low <= 1, f"{file.name}: camera phase drift {low}..{high} at picture {i}")
    require(abs(span - (slots - 1)) <= 1,
            f"{file.name}: camera code span {span} ({codes[0]} to {codes[-1]}), expected {slots - 1} ±1")
    return span


def scenario_linux_capture(e, cfg, evidence):
    """The real Linux capture paths on scripts/linux-capture-fixture.sh's
    headless desktop: the screen through the ScreenCast portal and PipeWire
    (mpv showing the frame oracle's block-code clip), desktop audio from the
    default sink (a 440 Hz tone), a microphone (880 Hz into a virtual source)
    and, where the kernel has v4l2loopback, a V4L2 camera (the oracle's
    second clip). One take: the program (screen and the camera in a corner,
    desktop and mic audio) and ISOs of the screen (desktop audio) and camera
    (mic audio). Every file decodes, each track carries its own tone and not
    the other, the screen and camera pictures are the clips' moving frame
    codes, and no file dropped a frame or audio (zero, also under --hosted;
    the take's other checks follow the invocation's rules). Application audio joins once the app-audio source
    lands (agent/linux-app-audio); evidence["app_audio"] says so meanwhile."""
    if not LINUX:
        raise Skip("Linux only")
    if not (os.environ.get("WAYLAND_DISPLAY") and os.environ.get("ISOCAST_FIXTURE_MIC")):
        raise Skip("needs the capture fixture: scripts/linux-capture-fixture.sh start DIR, then source DIR/session.env")
    seconds, (w, h), fps = LINUX_CAPTURE_SECONDS, LINUX_CAPTURE_CANVAS, 30
    camera = os.environ.get("ISOCAST_FIXTURE_CAMERA")
    evidence["camera"] = camera or f"skipped: {os.environ.get('ISOCAST_FIXTURE_CAMERA_SKIP', 'no camera')}"
    # The slot for application audio (an app playing its own tone, captured
    # by the application source and kept out of desktop audio).
    evidence["app_audio"] = "not covered yet: the Linux application-audio source is in review (agent/linux-app-audio)"
    fixtures = e.root.parent / "fixtures"
    clips = {tag: oracle_fixture(fixtures, ORACLE_TAGS[tag], fps) for tag in ("a", "b")}
    count = ORACLE_SECONDS * fps
    producers = []

    def produce(name, argv):
        log = open(e.root / f"{name}.log", "wb")
        producers.append((name, subprocess.Popen(argv, stdout=log, stderr=subprocess.STDOUT,
                                                 stdin=subprocess.DEVNULL, start_new_session=True), log))

    tone = ["ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "warning", "-re", "-f", "lavfi"]
    try:
        produce("screen", ["mpv", "--no-config", "--vo=wlshm", "--hwdec=no", "--no-audio", "--loop-file=inf",
                           "--fullscreen", "--osc=no", "--osd-level=0", "--input-default-bindings=no",
                           "--wayland-app-id=isocast-fixture", str(clips["a"])])
        produce("desktop-tone", tone + ["-i", f"sine=frequency=440:sample_rate={RATE}", "-ac", "2", "-f", "pulse",
                                        "-device", os.environ.get("ISOCAST_FIXTURE_DESKTOP_SINK", "isocast_desktop"),
                                        "IsocastDesktopTone"])
        produce("mic-tone", tone + ["-i", f"sine=frequency=880:sample_rate={RATE}", "-ac", "1", "-f", "pulse",
                                    "-device", os.environ.get("ISOCAST_FIXTURE_MIC_FEED", "isocast_mic_feed"),
                                    "IsocastMicTone"])
        if camera:
            produce("camera", ["ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "warning", "-re",
                               "-stream_loop", "-1", "-i", str(clips["b"]), "-an", "-vf", "format=yuyv422",
                               "-f", "v4l2", camera])
        until(lambda: b"isocast-fixture" in command(["swaymsg", "-t", "get_tree"], check=False).stdout,
              20, "the fixture's screen picture on the headless output")
        time.sleep(1)
        for name, process, _ in producers:
            require(process.poll() is None, f"fixture {name} exited early; see {name}.log in the scenario folder")

        cfg["canvas"].update(width=w, height=h, fps=fps)
        cfg["sources"] = [{"id": "screen", "kind": "display", "display": "main", "cursor": False},
                          {"id": "desktop", "kind": "system_audio"},
                          {"id": "mic", "kind": "microphone", "device": os.environ["ISOCAST_FIXTURE_MIC"]}]
        layout = [("a", (0, 0, 1, 1))]
        cfg["scenes"][0]["items"] = [item("screen", (0, 0, 1, 1), (w, h))]
        iso = ["screen"]
        source_audio = {"screen": ["desktop"]}
        if camera:
            cfg["sources"].append({"id": "camera", "kind": "camera", "device": camera,
                                   "width": ORACLE_SIZE[0], "height": ORACLE_SIZE[1], "fps": fps})
            cfg["scenes"][0]["items"].append(item("camera", (0.7, 0.7, 0.3, 0.3), (w, h)))
            layout.append(("b", (0.7, 0.7, 0.3, 0.3)))
            iso.append("camera")
            source_audio["camera"] = ["mic"]
        cfg["recording"]["program"]["audio"] = ["desktop", "mic"]
        cfg["recording"]["iso"].update(enabled=True, sources=iso, audio=["desktop"], source_audio=source_audio)
        e.apply(cfg)
        # The fixture has no saved choice. Pick its headless output before recording.
        e.rpc("engine", {"running": True}, timeout=150)
        e.rpc("screen_pick", {"source": "screen"}, timeout=150)
        time.sleep(2)
        evidence["devices"] = e.optional("devices")
        path, manifest = take(e, seconds)
        evidence["files"] = check_take(path, manifest, seconds, expected_sources=["program"] + iso,
                                       evidence=evidence)
        evidence["writer_drops"] = {f["source"]: {"dropped": f["dropped_frames"], "audio_dropouts": f["audio_dropouts"]}
                                    for f in manifest["files"]}
        # Zero writer drops even under --hosted, as win_preview_recording:
        # 480x270p30 with two ISOs is well inside a hosted runner's capacity.
        lost = {k: v for k, v in evidence["writer_drops"].items() if v["dropped"] or v["audio_dropouts"]}
        require(not lost, f"writer drops (zero required, also under --hosted): {lost}")

        # Sound: every track has its own tone and not the other one.
        tones = {}
        for f in manifest["files"]:
            file = path.parent / f["path"]
            streams = [s for s in probe(file)["streams"] if s["codec_type"] == "audio"]
            require(streams, f"{file.name}: no audio track")
            tones[f["source"]] = [tone_amplitudes(file, i) for i in range(len(streams))]
        evidence["tones"] = {k: [{str(f): round(a, 5) for f, a in t.items()} for t in v] for k, v in tones.items()}
        heard = lambda source, f: max(t[f] for t in tones[source])  # noqa: E731
        require(heard("program", 440) > 0.01 and heard("program", 880) > 0.01,
                f"program sound lacks the desktop (440 Hz) or microphone (880 Hz) tone: {evidence['tones']['program']}")
        for source, want, other in (("screen", 440, 880), ("camera", 880, 440)):
            if source in tones:
                require(heard(source, want) > 0.01 and heard(source, other) < 0.1 * heard(source, want),
                        f"{source} ISO sound should be the {want} Hz tone only: {evidence['tones'][source]}")

        # Pictures: the clips' frame codes, moving forward.
        def codes(file, regions, name):
            rows = oracle_decode(file, regions, fps)
            ids = [r[2][name] for r in rows]
            good = [x[2] for x in ids if x[0] == "id" and x[1] == ORACLE_TAGS[name]]
            back = sum(1 for a, b in zip(good, good[1:]) if 0 < (a - b) % count < count // 2)
            result = {"pictures": len(rows), "decoded": len(good), "unique": len(set(good)), "backwards": back}
            wrong = [x for x in ids if x[0] != "id" or x[1] != ORACLE_TAGS[name]]
            require(not wrong, f"{file.name}: {len(wrong)} of {len(rows)} pictures don't show the {name} clip's "
                               f"frame code (first: {wrong[:1]})")
            require(back == 0, f"{file.name}: the {name} clip's frames went backwards {back} times")
            if name == "b":  # The V4L2 camera ISO, not the mpv screen or program.
                result["span"] = camera_code_span(file, good, count, seconds * fps)
            else:
                require(len(set(good)) >= seconds * fps * 9 // 10,
                        f"{file.name}: only {len(set(good))} different {name} frames in {seconds} s (frozen or stalled)")
            return result

        screen_iso, _ = manifest_file(path, manifest, "screen")
        evidence["screen_codes"] = codes(screen_iso, oracle_regions([("a", (0, 0, 1, 1))]), "a")
        program, _ = manifest_file(path, manifest, "program")
        evidence["program_codes"] = codes(program, [r for r in oracle_regions(layout) if r["name"] == "a"], "a")
        if camera:
            camera_iso, _ = manifest_file(path, manifest, "camera")
            evidence["camera_codes"] = codes(camera_iso, oracle_regions([("b", (0, 0, 1, 1))]), "b")
    finally:
        for name, process, log in producers:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
            log.close()


LINUX_BROWSER_SECONDS = 10


def browser_oracle_page(path, fps, frames):
    """A page for linux_browser: on a transparent background, the frame
    oracle's block code of tag c for the page's current frame (time-based,
    so it advances at `fps` whatever the drawing rate) in its top-left
    quarter, a 50% blue box moving left and right across a band of the
    right half (otherwise transparent), and a 660 Hz Web Audio tone."""
    lo, hi = ORACLE_LEVELS
    codes = ["".join(f"{b}" for b in oracle_cells(ORACLE_TAGS["c"], n)) for n in range(frames)]
    path.write_text(f"""<!doctype html><html><head><style>
html, body {{ margin: 0; background: transparent; overflow: hidden; }}
canvas {{ position: absolute; left: 0; top: 0; width: 50vw; height: 50vh; image-rendering: pixelated; }}
#box {{ position: absolute; top: 65vh; width: 10vw; height: 20vh; background: rgba(0, 0, 255, 0.5); }}
</style></head><body><canvas id=code width=16 height=9></canvas><div id=box></div><script>
const codes = {json.dumps(codes)};
const fps = {fps}, lo = {lo}, hi = {hi};
const ctx = document.getElementById("code").getContext("2d");
const img = ctx.createImageData(16, 9);
const box = document.getElementById("box");
const t0 = performance.now();
const sound = new AudioContext(), tone = sound.createOscillator(), gain = sound.createGain();
tone.frequency.value = 660; gain.gain.value = 0.3;
tone.connect(gain); gain.connect(sound.destination); tone.start(); sound.resume();
function draw() {{
  const n = Math.floor((performance.now() - t0) * fps / 1000) % codes.length;
  const bits = codes[n];
  for (let i = 0; i < 144; i++) {{
    const v = bits[i] === "1" ? hi : lo;
    img.data[4 * i] = v; img.data[4 * i + 1] = v; img.data[4 * i + 2] = v; img.data[4 * i + 3] = 255;
  }}
  ctx.putImageData(img, 0, 0);
  const phase = (n % (2 * fps)) / fps;  // 0..2: out and back in two seconds
  box.style.left = (55 + 30 * (phase < 1 ? phase : 2 - phase)) + "vw";
  requestAnimationFrame(draw);
}}
draw();
</script></body></html>""")


def software_graphics(e, cfg):
    """Whether the engine composites on a software adapter, as the engine
    itself reports it: its capacity estimate lists a `software_renderer`
    resource there and refuses to estimate on a Windows hardware GPU. Any
    other answer counts as hardware, so the strict rules apply."""
    try:
        estimate = e.rpc("estimate_capacity", {"setup": cfg})
    except RpcError:
        return False
    return any(r.get("resource") == "software_renderer" for r in estimate.get("resources", []))


def scenario_browser_audio_gap(e, cfg, evidence):
    """Exercise the real page bridge and stampers without a production fault switch."""
    if LINUX and not (e.binary.parent / "isocast-browser").is_file():
        raise Skip("needs the Linux browser helper next to isocastd")
    page = e.root / "gap.html"

    def render(loss):
        # Mimic the lost Web Audio quanta in #203: callbacks keep arriving,
        # but 21 successive buffers each lack 10 ms of samples. This leaves
        # 60 ms above one resync and 90 ms below two, despite arrival jitter.
        page.write_text("""<body style='background:green'><script>
let last = Math.round(performance.now() * 48), tick = 0;
setInterval(() => {
  const end = Math.round(performance.now() * 48);
  const frames = Math.min(end-last, 48000) - (tick >= 10 && tick < 31 ? LOSS : 0);
  last = end; tick++;
  if (frames > 0) webkit.messageHandlers.isocastAudio.postMessage('48000:' + btoa(String.fromCharCode(0).repeat(8*frames)));
}, 40);
</script>""".replace("LOSS", str(loss)))

    render(0)
    cfg["canvas"].update(width=320, height=180, fps=30)
    cfg["sources"] = [{"id": "web", "kind": "browser", "file": str(page), "width": 320,
                       "height": 180, "fps": 30, "control_audio": True}]
    cfg["scenes"][0]["items"] = [item("web", (0, 0, 1, 1), (320, 180))]
    cfg["recording"]["program"]["audio"] = ["web"]
    cfg["recording"]["iso"].update(enabled=True, sources=["web"], audio=["web"])
    e.apply(cfg)
    # Start straight from cold: the engine, not a client-side snapshot loop,
    # waits for the first Windows web source to paint before arming the ISO.
    started = e.rpc("start", {}, timeout=60 if WINDOWS else 30)
    time.sleep(1)
    render(480)
    e.rpc("refresh_browser", {"source": "web"})
    def losses():
        live = e.rpc("status").get("live") or {}
        counts = {f["source"]: f["audio_dropouts"] for f in live.get("files", [])}
        return counts if counts == {"program": 1, "web": 1} else None
    evidence["live_dropouts"] = until(losses, 10, "one reported gap on the browser ISO and program")
    stop = e.rpc("stop")
    stopped = {f["source"]: f["audio_dropouts"] for f in stop["files"]}
    require(stopped == evidence["live_dropouts"], f"stop disagrees with live dropouts: {stopped}")
    evidence["stopped_dropouts"] = stopped
    manifest = read_json(started["manifest"])
    counts = {f["source"]: f["audio_dropouts"] for f in manifest["files"]}
    require(counts == evidence["live_dropouts"], f"manifest disagrees with live dropouts: {counts}")
    evidence["manifest_dropouts"] = counts

    # A second take exercises the injected tap, not synthetic PCM. Leave
    # one short hole on suspend and one during a close -> secondary handoff.
    # Both must survive in the recording, not disappear into a stitched run.
    render(0)
    started = e.rpc("start", {})
    page.write_text("""<body style='background:green'><script>
const wait = ms => new Promise(resolve => setTimeout(resolve, ms));
function tone() {
  const ctx = new AudioContext({sampleRate:48000}), osc = ctx.createOscillator();
  osc.connect(ctx.destination); osc.start(); return ctx;
}
(async () => {
  const a = tone(); await a.resume(); await wait(700);
  await Promise.all([a.suspend(), wait(75)]); await a.resume(); await wait(700);
  const b = tone(); await b.resume(); await wait(300);
  // Give the new primary a known module-loading delay at handoff.
  const load = b.audioWorklet.addModule.bind(b.audioWorklet);
  b.audioWorklet.addModule = async (...args) => { await wait(75); return load(...args); };
  await a.close();
})();
</script>""")
    e.rpc("refresh_browser", {"source": "web"})
    time.sleep(4)
    e.rpc("stop")
    path = Path(started["manifest"])
    manifest = read_json(path)
    evidence["lifecycle"] = {}
    for source in ("web", "program"):
        file, entry = manifest_file(path, manifest, source)
        require(entry["audio_dropouts"] == 0, f"{source}: lifecycle counted as a dropout")
        samples = pcm(file)
        # Interior quiet runs only: exclude recorder startup and shutdown.
        last = None
        holes = []
        for i, sample in enumerate(samples):
            if abs(sample) > 0.02:
                if last is not None and i - last > RATE * 0.04:
                    holes.append((round(last / RATE, 3), round((i - last) / RATE, 3)))
                last = i
        require(len(holes) == 2 and all(0.04 < duration < 0.2 for _, duration in holes)
                and 0.8 < holes[1][0] - holes[0][0] < 1.4,
                f"{source}: expected distinct suspend and handoff holes, got {holes}")
        evidence["lifecycle"][source] = {"dropouts": entry["audio_dropouts"], "holes_s": holes}

    # A third take checks browser A/V sync on simultaneous flash + beep pulses
    # using `browser_sync_check` (`1/fps + 2 ms` per-pulse bound for ~60 Hz rAF
    # adjacent-slot quantization + `|median| <= 0.5/fps` across `26/27 s`
    # phase-stepped pulses so any constant shift >= half a frame fails):
    # - strict (`lost=None`, zero drops, median gate) on test Macs and Windows GPU hardware (`exon-desk`);
    # - sequence-barcode hold-accounted `1/fps + 1/60 + 2 ms` (`52 ms` at 30 fps)
    #   per-pulse bound (no median gate) on hosted Linux, where two free-running
    #   30 Hz timers (`isocast-browser`'s `glib::timeout` snapshot loop and
    #   `isocastd`'s `IsoRunner`) plus 60 Hz `rAF` span `1/30 + 1/60 = 50 ms` on
    #   software Broadway/llvmpipe while still catching any >= 2-frame (66.7 ms) shift;
    # - skipped on hosted macOS VMs (no CoreAudio hardware, #203) and on
    #   software-WARP Windows VMs (`windows-latest` and `aws-win`, which have no
    #   GPU and no audio output device so `baseLatency + outputLatency == 0 ms`
    #   against 32–66 ms CPU Chromium + WARP DWM composition; `win_browser` stays
    #   strict there, and `exon-desk` runs Take 3 strictly).
    hosted_av = bool(loss_limit(evidence))
    if hosted_av and sys.platform == "darwin":
        evidence["av_sync"] = {
            "skipped": "hosted macOS VMs lack CoreAudio hardware so WebKit's dummy AudioWorklet timer stalls under rAF + software snapshots (#203); strict on test Macs and exon-desk, hold-accounted on hosted Linux",
        }
        return
    if WINDOWS and software_graphics(e, cfg):
        evidence["av_sync"] = {
            "skipped": "Windows software WARP VMs (windows-latest, aws-win) have no GPU and no audio output device (0 ms WASAPI compensation vs 32–66 ms CPU WARP DWM composition); win_browser stays strict here and exon-desk runs Take 3 strictly",
        }
        return
    hold_accounted = hosted_av
    hosted_linux = hosted_av and LINUX
    check_median = not hosted_linux
    pulse_near = (1 / 30 + 1 / 60 + 0.002) if hosted_linux else (1 / 30 + 0.002)
    av_page = e.root / "av_pulse.html"
    av_page.write_text("""<body style='margin:0;background:#000;overflow:hidden'>
<canvas id='c' width='320' height='180' style='width:100vw;height:100vh;display:block'></canvas>
<script>
(async () => {
  const ctx = new AudioContext({sampleRate: 48000});
  await ctx.resume();
  const osc = ctx.createOscillator();
  osc.frequency.value = 1000;
  const gain = ctx.createGain();
  gain.gain.value = 0;
  osc.connect(gain);
  gain.connect(ctx.destination);
  osc.start();
  const P = 26 / 27;
  const t0 = ctx.currentTime + 0.4;
  for (let k = 0; k <= 25; k++) {
    gain.gain.setValueAtTime(0.5, t0 + k * P);
    gain.gain.setValueAtTime(0.0, t0 + k * P + 0.08);
  }
  const g = document.getElementById('c').getContext('2d');
  let seq = 0;
  function frame() {
    seq = (seq + 1) & 0xffff || 1;
    const rel = ctx.currentTime - t0;
    const k = Math.floor(rel / P);
    const on = rel >= 0.0 && k <= 25 && (rel - k * P) < 0.1;
    for (let b = 0; b < 16; b++) {
      g.fillStyle = ((seq >> (15 - b)) & 1) ? '#fff' : '#000';
      g.fillRect(b * 20, 0, 20, 45);
    }
    g.fillStyle = '#fff';
    g.fillRect(0, 45, 320, 45);
    g.fillStyle = on ? '#fff' : '#000';
    g.fillRect(0, 90, 320, 90);
    requestAnimationFrame(frame);
  }
  requestAnimationFrame(frame);
})();
</script></body>""")
    cfg["sources"] = [{"id": "web", "kind": "browser", "file": str(av_page), "width": 320,
                       "height": 180, "fps": 30, "control_audio": True}]
    e.apply(cfg)
    e.rpc("engine", {"running": True}, timeout=60 if WINDOWS else 30)

    def av_ready():
        try:
            png = base64.b64decode(e.rpc("snapshot", {"source": "web"})["png_base64"])
        except RpcError:
            return False
        probe_png = e.root / "av_ready.png"
        probe_png.write_bytes(png)
        px = ffmpeg("-i", probe_png, "-vf", "crop=iw:ih/4:0:ih/4,scale=1:1:flags=area",
                    "-pix_fmt", "gray", "-f", "rawvideo", "-").stdout
        return bool(px) and px[0] > 128

    until(av_ready, 60 if WINDOWS else 30, "browser A/V pulse page painting", interval=0.2)
    time.sleep(1.0)
    av_started = e.rpc("start", {"seconds": 9}, timeout=60 if WINDOWS else 30)
    av_path = Path(av_started["manifest"])
    av_manifest = until(lambda: finished(av_started["manifest"]), 45, "browser A/V pulse finalization")
    until(lambda: e.rpc("status")["state"] == "idle", description="idle recorder")
    evidence["av_sync"] = {"mode": "hold_accounted" if hold_accounted else "strict"}
    for source in ("web", "program"):
        file, entry = manifest_file(av_path, av_manifest, source)
        require(entry["audio_dropouts"] == 0, f"{source}: browser A/V pulse take had audio dropouts")
        if not hosted_av:
            require(entry["dropped_frames"] == 0,
                    f"{source}: browser A/V pulse take dropped {entry['dropped_frames']} frame(s)")
        lost = None
        if hold_accounted:
            frames_info = json.loads(command([
                "ffprobe", "-v", "error", "-select_streams", "v:0",
                "-show_frames", "-show_entries", "frame=best_effort_timestamp_time",
                "-of", "json", file
            ], timeout=90).stdout)["frames"]
            times = [float(f["best_effort_timestamp_time"]) for f in frames_info]
            bars = ffmpeg(
                "-i", file, "-map", "0:v:0", "-an",
                "-vf", "crop=iw:ih/4:0:0,scale=16:1:flags=area",
                "-fps_mode", "passthrough", "-pix_fmt", "gray", "-f", "rawvideo", "-", timeout=90
            ).stdout
            lost = set()
            prev_seq = None
            for idx, t in enumerate(times):
                row = bars[idx * 16:(idx + 1) * 16]
                seq = sum((1 << (15 - b)) for b in range(16) if row[b] > 128) if len(row) == 16 else 0
                if seq == 0 or seq == prev_seq:
                    lost.add(round(t * 30))
                elif prev_seq is not None and ((seq - prev_seq) & 0xffff) >= 3 and idx > 0:
                    # A >= 3 rAF jump in one 30 fps slot means slot idx - 1 got
                    # only an early pre-pulse rAF while >= 2 newer rAF paints
                    # were held uncomposited by software WARP/WebKit.
                    lost.add(round(times[idx - 1] * 30))
                prev_seq = seq
        evidence["av_sync"][source] = browser_sync_check(
            file, 30, BROWSER_PULSE_PERIOD, lost=lost, check_median=check_median, near=pulse_near
        )


def scenario_linux_browser(e, cfg, evidence):
    """A browser source on Linux (the isocast-browser helper next to the
    daemon: WebKitGTK 6 on a private GTK Broadway display); see
    browser_scenario."""
    if not LINUX:
        raise Skip("Linux only")
    browser_scenario(e, cfg, evidence, e.binary.parent / "isocast-browser")


def scenario_win_browser(e, cfg, evidence):
    """A browser source on Windows (isocast-browser.exe next to the daemon:
    a WebView2 composition controller captured offscreen); see
    browser_scenario."""
    if not WINDOWS:
        raise Skip("Windows only")
    browser_scenario(e, cfg, evidence, e.binary.parent / "isocast-browser.exe")


def check_browser_tone_windows(file, evidence):
    # One complex magnitude per full 100 ms, not one over several seconds:
    # a phase jump must not cancel good sound elsewhere in the recording.
    raw = ffmpeg("-i", file, "-map", "0:a:0", "-ac", "1", "-ar", RATE, "-f", "f32le", "-").stdout
    samples = array.array("f")
    samples.frombytes(raw)
    if sys.byteorder != "little":
        samples.byteswap()
    n = RATE // 10
    require(len(samples) >= RATE, f"{file.name}: under a second of sound")
    levels = {}
    for hz in (660, 440):
        w = 2 * math.pi * hz / RATE
        basis = [complex(math.cos(w*i), math.sin(w*i)) for i in range(n)]
        levels[hz] = [2 * abs(sum(x*b for x, b in zip(samples[i:i+n], basis))) / n
                      for i in range(0, len(samples) - n + 1, n)]
    tone = levels[660]
    median = statistics.median(tone)
    evidence.update(window_ms=100, levels_660=[round(x, 5) for x in tone], median_660=median,
                    max_440=max(levels[440]))
    require(median > 0.05, f"{file.name}: missing 660 Hz tone (median level {median:.5f})")
    threshold = median * 0.5
    onset = next(i for i, level in enumerate(tone) if level >= threshold)
    gaps = [i for i in range(onset, len(tone)) if tone[i] < threshold]
    evidence.update(onset_s=onset / 10, gaps_s=[i / 10 for i in gaps])
    require(onset / 10 <= 0.5, f"{file.name}: 660 Hz tone starts late at {onset / 10:.1f} s")
    require(not gaps, f"{file.name}: 660 Hz below {threshold:.5f} at {evidence['gaps_s']} s")
    require(evidence["max_440"] < median * 0.1, f"{file.name}: 440 Hz leaked into the page's tone: {evidence['max_440']:.5f}")


def browser_scenario(e, cfg, evidence, helper):
    """A browser source (the platform's isocast-browser helper) over a green
    color source: a local page draws the frame oracle's block code for its
    current frame and a 50% blue box moving on a transparent background,
    and plays a 660 Hz Web Audio tone, sent to the mixer (control_audio).
    The take's browser ISO and program both show the codes advancing (every
    picture decodes, never backwards, most distinct) and carry the tone; in
    the program, the page's transparent parts show the green beneath and
    the box blends with it; and no file dropped a frame (zero, also under
    --hosted)."""
    if not helper.is_file():
        raise Skip(f"no {helper.name} next to the daemon (cargo build --release -p isocast-browser)")
    w, h, fps = (cfg["canvas"][k] for k in ("width", "height", "fps"))
    seconds = LINUX_BROWSER_SECONDS
    page = e.root / "oracle-page.html"
    browser_oracle_page(page, fps, (seconds + 30) * fps)
    count = (seconds + 30) * fps
    cfg["sources"] = [{"id": "green", "kind": "color", "color": "#00ff00", "width": w, "height": h // 2},
                      {"id": "web", "kind": "browser", "file": str(page), "width": w, "height": h, "fps": fps,
                       "css": "", "control_audio": True}]
    # Green under the bottom half only (where the alpha checks look): two
    # full-canvas layers are more llvmpipe fill than a 2-vCPU VM affords at
    # 720p30 beside the page (a full colour layer under a full test pattern
    # drops there too; either alone, or the colour under half, doesn't).
    cfg["scenes"][0]["items"] = [item("green", (0, 0.5, 1, 0.5), (w, h)), item("web", (0, 0, 1, 1), (w, h))]
    cfg["recording"]["program"]["audio"] = ["web"]
    cfg["recording"]["iso"].update(enabled=True, sources=["web"], audio=["web"])
    e.apply(cfg)
    e.rpc("engine", {"running": True}, timeout=60)
    # The helper, its display and WebKit start and load the page before the
    # source's first picture: record once the source itself shows the
    # page's frame code (up to 60 s; refusals while it has no picture yet
    # are part of the wait).
    started = time.monotonic()
    code_region = oracle_regions([("c", (0, 0, 0.5, 0.5))])

    def page_shown():
        try:
            png = base64.b64decode(e.rpc("snapshot", {"source": "web"})["png_base64"])
        except RpcError:
            return False
        (e.root / "first-picture.png").write_bytes(png)
        pixels = ffmpeg("-i", e.root / "first-picture.png", "-vf", f"scale={w}:{h}", "-pix_fmt", "gray",
                        "-f", "rawvideo", "-").stdout
        got = oracle_read(pixels, w, h, code_region[0])
        return got[0] == "id" and got[1] == ORACLE_TAGS["c"]
    until(page_shown, 60, "the browser source showing its page", interval=0.25)
    evidence["first_picture_s"] = round(time.monotonic() - started, 2)
    # An ISO starts only with a picture from the last 300 ms; a helper
    # stalled briefly on a loaded runner (2 vCPUs, WebKit busy) misses
    # that. Exactly that refusal is retried, for up to 10 s, and counted.
    refused, deadline = 0, time.monotonic() + 10
    while True:
        try:
            path, manifest = take(e, seconds)
            break
        except RpcError as err:
            require("has no input picture yet" in str(err) and time.monotonic() < deadline, str(err))
            refused += 1
            time.sleep(0.25)
    evidence["start_refusals"] = refused
    evidence["files"] = check_take(path, manifest, seconds, expected_sources=["program", "web"], evidence=evidence)
    web = next(f for f in manifest["files"] if f["source"] == "web")
    require(web["audio_dropouts"] == 0, f"continuous browser audio reported {web['audio_dropouts']} dropouts")
    evidence["writer_drops"] = {f["source"]: f["dropped_frames"] for f in manifest["files"]}
    lost = {k: v for k, v in evidence["writer_drops"].items() if v}
    require(not lost, f"writer drops (zero required, also under --hosted): {lost}")
    if WINDOWS:
        evidence["audio_dropouts"] = {f["source"]: f["audio_dropouts"] for f in manifest["files"]}
        require(evidence["audio_dropouts"] == {"web": 0, "program": 0},
                f"continuous browser audio reported dropouts: {evidence['audio_dropouts']}")
        evidence["tone_windows"] = {}
        for f in manifest["files"]:
            track = evidence["tone_windows"][f["source"]] = {}
            check_browser_tone_windows(path.parent / f["path"], track)
    else:
        tones = {}
        for f in manifest["files"]:
            tones[f["source"]] = tone_amplitudes(path.parent / f["path"], 0, freqs=(660, 440))
            require(tones[f["source"]][660] > 0.05 and tones[f["source"]][440] < 0.1 * tones[f["source"]][660],
                    f"{f['source']}: the page's 660 Hz tone isn't its sound: {tones[f['source']]}")
        evidence["tones"] = {k: {str(f): round(a, 4) for f, a in v.items()} for k, v in tones.items()}

    # Whether the engine's own graphics adapter is a software one (WARP):
    # only there does WebView2 render the page in software (below).
    software = WINDOWS and software_graphics(e, cfg)
    evidence["graphics"] = "software" if software else "hardware"
    code_region = oracle_regions([("c", (0, 0, 0.5, 0.5))])
    for source in ("web", "program"):
        file, _ = manifest_file(path, manifest, source)
        rows = oracle_decode(file, code_region, fps)
        ids = [r[2]["c"] for r in rows]
        wrong = [x for x in ids if x[0] != "id" or x[1] != ORACLE_TAGS["c"]]
        require(not wrong, f"{file.name}: {len(wrong)} of {len(rows)} pictures don't show the page's frame code "
                           f"(first: {wrong[:1]})")
        good = [x[2] for x in ids]
        back = sum(1 for a, b in zip(good, good[1:]) if 0 < (a - b) % count < count // 2)
        require(back == 0, f"{file.name}: the page's frames went backwards {back} times")
        # Most pictures are a new page frame. A Windows runner without a GPU
        # renders the page in software beside a software compositor and
        # encoders on four vCPUs, and WebView2 then paints every second
        # frame: the helper sent 289 pictures in 10 s, none dropped, but
        # 151 of 300 were new (hosted runs 40138211826, 40142180703). That
        # is how WebView2 behaves on such machines, not a loss in Isocast,
        # so there (no hardware GPU) 2/5 must be new (delegator, 2026-10-24);
        # every other check stays strict, and 2/3 everywhere else.
        share = (2, 5) if software else (2, 3)
        evidence["unique_share_required"] = f"{share[0]}/{share[1]}"
        require(len(set(good)) >= len(good) * share[0] // share[1],
                f"{file.name}: only {len(set(good))} different page frames in {len(good)} pictures "
                f"(at least {share[0]}/{share[1]} required)")
        evidence[f"{source}_codes"] = {"pictures": len(rows), "unique": len(set(good)), "backwards": back}

    # Alpha in the program: the page's transparent parts show the green
    # beneath; the 50% blue box blends with it (never black or opaque blue).
    program, _ = manifest_file(path, manifest, "program")
    band_y = int(h * 0.75)
    # RGB first: one row of 4:2:0 video can't be cropped.
    raw = ffmpeg("-i", program, "-an", "-vf", f"format=rgb24,crop={w}:1:0:{band_y}", "-fps_mode", "passthrough",
                 "-pix_fmt", "rgb24", "-f", "rawvideo", "-").stdout
    rows = [raw[i:i + w * 3] for i in range(0, len(raw), w * 3)]
    require(rows, "program: no decoded rows")
    clear = blended = 0
    for row in rows:
        px = [tuple(row[x * 3:x * 3 + 3]) for x in range(w)]
        left = px[int(w * 0.05):int(w * 0.45)]  # never under the box
        if all(g > 200 and r < 60 and b < 60 for r, g, b in left):
            clear += 1
        right = px[int(w * 0.55):int(w * 0.95)]
        if any(r < 60 and 80 < g < 180 and 80 < b < 180 for r, g, b in right):
            blended += 1
    evidence["alpha"] = {"rows": len(rows), "transparent_shows_green": clear, "box_blends": blended}
    require(clear == len(rows), f"program: the page's transparent area wasn't green beneath in "
                                f"{len(rows) - clear} of {len(rows)} pictures")
    require(blended >= len(rows) * 9 // 10, f"program: the 50% box blended over green in only {blended} of "
                                            f"{len(rows)} pictures")


def scenario_linux_recovery(e, cfg, evidence):
    """SIGKILL with committed program/ISO fragments, then restart and verify
    finalized indexes, real decoded counts and persisted recovery metadata."""
    fragment_recovery(e, cfg, evidence)


def scenario_linux_handoff(e, cfg, evidence):
    """A CLI upgrade at the default endpoint: finish the old take first."""
    from concurrent.futures import ThreadPoolExecutor

    old = os.environ.get("ISOCAST_E2E_OLD_DAEMON")
    if not old:
        raise Skip("needs a previous build (ISOCAST_E2E_OLD_DAEMON)")
    cli = os.environ["ISOCAST_E2E_CLI"]
    expected = e.rpc("hello")["build"]["commit"]
    e.stop()
    # Exercise default discovery, not a socket override. HOME is private too
    # (a pre-XDG installation can take precedence over XDG_DATA_HOME).
    runtime = e.root / "runtime"
    runtime.mkdir(mode=0o700)
    if "ISOCAST_OPENH264" not in e.env:
        for codec in (Path.home() / ".local/lib/isocast").glob("libopenh264-*.so"):
            e.env["ISOCAST_OPENH264"] = str(codec)
            break
    e.env.update(HOME=str(e.root), XDG_RUNTIME_DIR=str(runtime), XDG_DATA_HOME=str(e.root / "data"))
    del e.env["ISOCAST_SOCKET"]
    e.sock = runtime / "isocast/isocastd.sock"
    # Replace the executable at its installation path as dpkg does, rather
    # than starting two live installations and guessing which commit is older.
    install = e.root / "bin"
    install.mkdir()
    installed_daemon = install / "isocastd"
    shutil.copy2(old, installed_daemon)
    e.binary = installed_daemon
    e.start()
    shutil.copy2(Path(cli).with_name("isocastd"), install / "new")
    (install / "new").replace(installed_daemon)
    shutil.copy2(cli, install / "isocast")
    cli = str(install / "isocast")

    def status(args=(), executable=cli, **env):
        result = subprocess.run([executable, *args, "status", "--json"], env={**e.env, **env},
                                text=True, capture_output=True, timeout=40)
        require(result.returncode == 0, f"isocast status: {result.stderr}")
        return json.loads(result.stdout), result.stderr

    try:
        before = e.rpc("status")["daemon"]
        require(before["commit"] != expected, "old fixture has the client's commit")
        for explicit in (status(["--engine", "local"]), status(ISOCAST_SOCKET=str(e.sock))):
            require(explicit[0]["daemon"]["pid"] == before["pid"], "explicit status replaced an engine")
        e.apply(cfg)
        take = e.rpc("start", {})
        busy, warning = status()
        require(busy["daemon"]["pid"] == before["pid"] and busy["session"] == take["session"],
                "handoff interrupted the take")
        require(warning.strip() == busy["diagnostics"]["handoff_warning"] and "still recording" in warning and
                "isocast restart" in warning and before["commit"][:12] in warning and expected[:12] in warning,
                f"missing or duplicate upgrade instructions: {warning}")
        e.rpc("stop")
        # The app's watcher, worker and preview can all see the old engine
        # at once. A peer that lost the shutdown race must not report 'busy'.
        def hello(_):
            result = subprocess.run([cli, "hello", "--json"], env=e.env, text=True, capture_output=True, timeout=40)
            require(result.returncode == 0 and json.loads(result.stdout)["build"]["commit"] == expected,
                    f"concurrent handoff did not reach the installed engine: {result.stderr}")
        with ThreadPoolExecutor(max_workers=4) as pool:
            list(pool.map(hello, range(4)))
        after, _ = status()
        after = after["daemon"]
        require(e.process.wait(timeout=10) == 0, "old daemon did not exit cleanly")
        require(after["pid"] != before["pid"] and after["commit"] == expected, f"wrong replacement: {after}")
        require(status()[0]["daemon"]["pid"] == after["pid"], "matching daemon restarted")
        # The older client remains resident across the upgrade. Its adjacent
        # helper is now the installed build, not the one it was compiled with.
        install = e.root / "upgraded-bin"
        install.mkdir()
        old_cli = install / "isocast"
        shutil.copy2(Path(old).with_name("isocast-old"), old_cli)
        shutil.copy2(Path(cli).with_name("isocastd"), install / "isocastd")
        for _ in range(2):
            reverse, notice = status(executable=old_cli)
            require(reverse["daemon"]["pid"] == after["pid"] and "client" in notice and "out of date" in notice and
                    reverse["diagnostics"]["handoff_warning"] == notice.strip(), f"reverse handoff: {notice}")
        (install / "isocastd").unlink()
        missing, notice = status(executable=old_cli)
        require(missing["daemon"]["pid"] == after["pid"] and "missing" in notice and
                missing["diagnostics"]["handoff_warning"] == notice.strip(), f"missing helper: {notice}")
        evidence.update(old=before, replacement=after, busy_warning=warning.strip())
        # A second child waiting for the instance lock must not resurrect
        # a daemon the user explicitly stopped just after a handoff.
        stopped = subprocess.run([cli, "engine", "shutdown"], env=e.env, text=True, capture_output=True, timeout=45)
        require(stopped.returncode == 0 and not status()[0].get("daemon"),
                f"engine still running after explicit shutdown: {stopped.stderr}")
    finally:
        # The replacement is detached from the original Engine child handle.
        if e.process.poll() is not None and e.sock.exists():
            e.rpc("shutdown_if_idle")


def scenario_win_vcam_publisher(e, cfg, evidence):
    """Two independent pipe consumers during a live take, then during a
    governor-held take. Decode the program separately: the publisher must
    never be its own reference. The changing right half proves recovery."""
    reader = e.binary.parent / "examples" / "vcam-reader.exe"
    if not reader.is_file():
        raise Skip("needs the Windows qualification build with the vcam-reader example")
    w, h, fps = 320, 180, 30
    seconds = 8 if os.environ.get("GITHUB_ACTIONS") else 60
    cfg = setup(e.root, "h264", width=w, height=h, fps=fps)
    def box(key, x, y, width, height, color):
        return {"id": key, "rect": dict(x=x, y=y, width=width, height=height), "style": {"background": color}}
    cfg["scenes"] = [{"id": name, "name": name, "items": [box("base", 0, 0, w, h, "#2080d0"),
                      box("mark", 8, 12, 80, 60, "#e03070"), box("cut", w//2, 0, w//2, h, color)]}
                     for name, color in [("main", "#20e040"), ("other", "#f09122")]]
    e.apply(cfg)
    def cut(scene):
        e.rpc("switch_scene", {"scene": scene, "transition": {"kind": "cut"}})
    cut("main")
    path, m = take(e, 2)
    movie, _ = manifest_file(path, m, "program")
    # Windows doesn't implement frame_jpeg. Grab a decoded program frame,
    # preserving its limited-range 709 samples (planar 420 -> NV12).
    ref = e.root / "program-frame.nv12"
    picture = ffmpeg("-i", movie, "-frames:v", "1", "-pix_fmt", "nv12", "-f", "rawvideo", "-").stdout
    require(len(picture) == w*h*3//2 and max(picture[:w*h])-min(picture[:w*h]) > 30, "invalid program reference")
    ref.write_bytes(picture)
    try:
        estimate = e.rpc("estimate_capacity", {"setup": cfg})
        software = any(r["resource"] == "software_renderer" for r in estimate["resources"])
    except RpcError as exc:
        # Windows has a capacity estimate for WARP but no hardware table.
        if "only supported on macOS" not in str(exc):
            raise
        software = False
    if not software:
        evidence["skipped_phases"] = ["hold and cold hold: recording on a hardware GPU does not force monitors to 0 fps"]
    for live in ([True, False] if software else [True]):
        phase = "live" if live else "hold"
        cfg["governor"] = {"enabled": True, "pins": ["preview.fps"] if live else []}
        e.apply(cfg)
        cut("main")
        require(e.rpc("virtual_camera_status").get("unavailable_reason"), "Windows control wasn't disabled")
        status = e.rpc("virtual_camera_start")
        require(status["transport"] == "windows-pipe-nv12", f"wrong publisher: {status}")
        clients, logs = [], []
        try:
            for delay in [0, 125]:
                log = e.root / f"{phase}-{delay}.csv"
                with log.open("wb") as out:
                    clients.append(subprocess.Popen([reader, status["destination"], str(seconds+8), str(delay), ref],
                                                   env=e.env, stdout=out, stderr=subprocess.PIPE))
                logs.append(log)
            until(lambda: all(log.stat().st_size > 150 for log in logs), 5, "both readers checked their first pictures")
            started = e.rpc("start", {"seconds": seconds})
            time.sleep(2)
            during = e.rpc("virtual_camera_status")
            require(during.get("governor_fps") == (None if live else 0), f"unexpected governor rate: {during}")
            cut("other")  # live: immediate cut. Held: must reach readers only after the take.
            if not live:
                # Bracket a measured interval with two *confirmed* zero-fps
                # counters. A live tick waiting for the new canvas may repeat
                # the old PTS immediately after resume, but is not a hold tick.
                time.sleep(seconds-4)
                end_hold = e.rpc("virtual_camera_status")
                require(end_hold.get("governor_fps") == 0, "hold ended before the observation window")
            path = Path(started["manifest"])
            m = until(lambda: finished(path), seconds+15, "program recording")
            evidence[phase] = {"files": check_take(path, m, seconds), "camera_during_recording": during}
            require(all(f["audio_dropouts"] == 0 and f["frames"] == seconds*fps for f in m["files"]), "recording loss")
            for delay, proc, log in zip([0, 125], clients, logs):
                _, err = proc.communicate(timeout=15)
                require(proc.returncode == 0, f"{phase} reader {delay}: {err.decode(errors='replace')}")
                rows = [tuple(map(int, line.split(","))) for line in log.read_text().splitlines()]
                gaps = rows[-1][0]-rows[0][0]+1-len(rows)
                require(delay == 0 or gaps >= fps, f"slow consumer didn't skip frames: {gaps}")
                require(any(r[4] for r in rows), f"reader {delay} missed the change of picture")
                result = {"frames": len(rows), "skipped": gaps}
                if not live:
                    frozen = [r for r in rows if during["frames_sent"] < r[0] <= end_hold["frames_sent"]]
                    require(len(frozen) >= (seconds-5)*2 and all(r[3] and not r[4] for r in frozen)
                            and len({r[2] for r in frozen}) == 1,
                            "held pixels/PTS changed or were not delivered")
                    hz = (len(frozen)-1)*1e9/(frozen[-1][1]-frozen[0][1])
                    require(1.9 < hz < 2.06, f"hold cadence {hz} Hz")  # allow scheduler jitter at endpoints
                    resumed = [r for r in rows if r[0] > frozen[-1][0]]
                    require(any(not r[3] and r[4] for r in resumed), "reader didn't recover the new scene on the same pipe")
                    result.update(held_samples=len(frozen), held_hz=hz)
                evidence[phase][f"reader_{delay}ms"] = result
            evidence[phase]["camera_after"] = e.rpc("virtual_camera_status")
        finally:
            for proc in clients:
                if proc.poll() is None:
                    proc.kill(); proc.wait(timeout=5)
            e.rpc("virtual_camera_stop")

    # A cold Start during a hold sends no placeholder. After the take, a
    # differently shaped camera output must actually fit the live canvas.
    cfg["virtual_camera"] = {"width": 720, "height": 1280}
    e.apply(cfg)
    cut("main")
    rec = e.rpc("start", {"seconds": 4}) if software else None
    status = e.rpc("virtual_camera_start")
    try:
        if rec is not None:
            until(lambda: e.rpc("virtual_camera_status").get("governor_fps") == 0, 2, "cold hold")
            time.sleep(1)
            cold = e.rpc("virtual_camera_status")
            require(cold["frames_sent"] == cold["frames_dropped"] == cold["frames_repeated"] == 0, "invented a first picture")
            m = until(lambda: finished(rec["manifest"]), 10, "finish cold-start take")
            evidence["cold_take"] = check_take(Path(rec["manifest"]), m, 4)
        ref.write_bytes(ffmpeg("-i", movie, "-vf", "scale=720:406,pad=720:1280:0:436", "-frames:v", "1",
                               "-pix_fmt", "nv12", "-f", "rawvideo", "-").stdout)
        scaled = command([reader, status["destination"], "1", "0", ref], timeout=10)
        evidence["scaled"] = scaled.stderr.decode().strip()
        require(len(scaled.stdout.splitlines()) >= 2, "scaled publisher didn't recover")
    finally:
        e.rpc("virtual_camera_stop")


def scenario_win_monitor(e, cfg, evidence):
    """Open monitoring mid-take without interrupting recorded audio. Hosted
    Windows has no audio endpoint: exercise the real open and require its
    explicit no-output error before reporting the hardware portion skipped."""
    if not WINDOWS:
        raise Skip("Windows WASAPI monitoring")
    outputs = e.rpc("devices")["audio_outputs"]
    evidence["outputs"] = outputs
    cfg = setup(e.root, "h264", width=320, height=180, fps=30)
    e.apply(cfg)
    started = e.rpc("start", {"seconds": 8})
    time.sleep(1)
    cfg["sources"][-1]["audio"] = {"monitor": "monitor_and_output"}
    before = time.monotonic()
    e.apply(cfg)
    evidence["enable_ms"] = (time.monotonic() - before) * 1000
    monitor = until(lambda: (e.rpc("status").get("live") or {}).get("monitor"),
                    seconds=5, description="monitor open result")
    evidence["monitor"] = monitor
    if outputs:
        require(not monitor.get("error"), f"monitor could not open: {monitor}")
        monitor = until(lambda: (lambda m: m if m and m["frames"] > RATE else None)(
            (e.rpc("status").get("live") or {}).get("monitor")),
            seconds=5, description="WASAPI requesting monitor audio")
        evidence["playing"] = monitor
    else:
        require("no output device" in (monitor.get("error") or ""),
                f"missing explicit no-output error: {monitor}")
    path = Path(started["manifest"])
    manifest = until(lambda: finished(path), seconds=30, description="monitor take finalization")
    until(lambda: e.rpc("status")["state"] == "idle", description="idle recorder after monitor take")
    # Every whole-second beep remains in the recording, including the one
    # during device startup; use the real decoded samples, not only counters.
    evidence["files"] = check_take(path, manifest, sync=True, evidence=evidence)
    if not outputs:
        raise Skip("no output device: real WASAPI open returned the expected error; mid-take recording audio verified")


def check_preview_program_loss(loss, hosted_reference):
    limit = 0.01 if hosted_reference else 0
    require(loss["head"] == 0, f"program started {loss['head']} slot(s) late")
    check_loss(loss, limit)
    require(loss["repeated"] == 0, f"program: unexpected repeated_frames={loss['repeated']}")
    return limit


def scenario_win_preview_recording(e, cfg, evidence):
    """Qualify the conservative reference, then protect it from previews.
    Only hosted reference takes allow interior drops; paused consumers must survive and resume.
    A passing lower profile is not a universal software-renderer capacity claim."""
    if not WINDOWS:
        raise Skip("Windows shared-preview recording qualification")
    cli = os.environ.get("ISOCAST_E2E_CLI")
    require(cli and Path(cli).is_file(), "preview qualification requires --cli")
    profiles = evidence.setdefault("headless_profiles", [])
    evidence["qualification_scope"] = "this host and run only; not universal software-GPU capacity"
    evidence["preview_profile"] = None
    oversized = setup(e.root, "h264", width=1920, height=1080, fps=30)
    try:
        estimate = e.rpc("estimate_capacity", {"setup": oversized})
    except RpcError as exc:
        if "only supported on macOS" in str(exc):
            raise Skip("qualification requires a Windows software graphics adapter") from exc
        raise
    evidence["software_capacity"] = estimate
    require(estimate["capacity"] == "no_margin" and estimate["basis"].startswith("assumed:")
            and "expect dropped frames" in estimate["summary"] and estimate["suggestions"],
            f"missing advisory software capacity warning: {estimate}")
    require(any(r["resource"] == "software_renderer" and r.get("capacity") is None
                and r.get("load") is None for r in estimate["resources"]),
            "software adapter must not claim a measured local capacity")
    doctor = e.rpc("doctor", {"setup": oversized})
    evidence["software_doctor"] = doctor
    require(any(c["name"] == "software graphics capacity" and c["status"] == "warning"
                and c.get("fix") for c in doctor["checks"]), "doctor omitted the software capacity warning")
    capacity_setup = e.root / "software-capacity.json"
    write_json(capacity_setup, oversized)
    capacity_cli = subprocess.run([cli, "capacity", str(capacity_setup)], env=e.env, cwd=e.root,
                                  capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=30)
    evidence["software_capacity_cli"] = capacity_cli.stdout
    require(capacity_cli.returncode == 0 and "not qualified" in capacity_cli.stdout
            and "local capacity is unmeasured" in capacity_cli.stdout
            and "keeps up" not in capacity_cli.stdout and "%" not in capacity_cli.stdout,
            f"capacity CLI misrepresented unmeasured software graphics: {capacity_cli.stdout} {capacity_cli.stderr}")

    def check_program(path, manifest, seconds, profile, result):
        result.update(manifest=str(path), loss=drop_metrics(manifest))
        require(manifest["status"] == "complete" and not manifest.get("errors"),
                f"qualification take did not complete: {manifest}")
        require(len(manifest["files"]) == len(result["loss"]) == 1,
                "qualification requires final counters for exactly one program file")
        file, facts = manifest_file(path, manifest, "program")
        dimensions = (profile["width"], profile["height"])
        loss = result["loss"][0]
        limit = check_preview_program_loss(loss, dimensions == reference and evidence["capabilities"]["hosted"])
        result["files"] = [check_file(file, duration=seconds, fps=profile["fps"],
                                     dimensions=dimensions, dropped=loss["dropped"] - loss["head"],
                                     repeated=loss["repeated"])]
        require(facts["frames"] == result["files"][0]["frames"], "program manifest/decoded frame mismatch")
        if dimensions == reference:
            result["files"][0].update(sync_check(file, profile["fps"], lost=set() if limit else None))

    # Retain the larger-profile measurements, but do not select a host's
    # highest single 10-second pass: it can sit on the edge and lose frames
    # during the longer preview take. Gate the same conservative reference
    # named by estimate::SOFTWARE_REFERENCE, with no fallback.
    reference = WINDOWS_PROFILES["180p30"][:2]
    selected = None
    reference_warning_seen = False
    for width, height in ((1280, 720), (960, 540), (640, 360), (480, 270), (426, 240), (320, 180)):
        cfg = setup(e.root, "h264", width=width, height=height, fps=30)
        # The gated reference has matched-duration headless/preview controls.
        seconds = 20 if (width, height) == reference else 10
        cfg["scenes"].append({"id": "standby", "name": "Standby", "items": [
            {"source": "bars2", "rect": {"x": 0, "y": 0, "width": width, "height": height}}]})
        e.apply(cfg)
        case = {"profile": cfg["canvas"].copy(), "seconds": seconds, "status": "RUNNING", "samples": []}
        profiles.append(case)
        within_reference = "not a capacity guarantee" in e.rpc("estimate_capacity", {})["summary"]
        require(within_reference == ((width, height) == reference),
                "estimate reference disagrees with the gated profile")
        watch = Watch(e, ["warning", "started"], stop_after="started")
        try:
            baseline_start = e.rpc("start", {"seconds": seconds})
            until(lambda: watch.of("started"), 10, "start event after capacity warning")
            events = list(watch.events)
            case["start_events"] = events
            warning = next((i for i, event in enumerate(events) if event["type"] == "warning"
                            and "software graphics adapter" in event["message"]), None)
            started_at = next(i for i, event in enumerate(events) if event["type"] == "started")
            if within_reference and reference_warning_seen:
                require(warning is None, "within-reference warning repeated in the same daemon run")
                require(not any("software graphics adapter" in note for note in baseline_start["notes"]),
                        "within-reference start notes repeated the one-time warning")
            else:
                require(warning is not None and warning < started_at,
                        "software capacity warning was not published before recording started")
                require(any("software graphics adapter" in note for note in baseline_start["notes"]),
                        "start response lost the software capacity warning")
                if within_reference:
                    require("isocast capacity" in events[warning]["message"],
                            "within-reference warning omitted capacity guidance")
                    reference_warning_seen = True
        finally:
            watch.close()
        baseline_path = Path(baseline_start["manifest"])
        case["manifest"] = str(baseline_path)
        began = time.monotonic()

        def baseline_finished():
            case["samples"].append({"at_s": time.monotonic() - began, "status": e.rpc("status")})
            return finished(baseline_path)

        baseline = until(baseline_finished, 45, "headless qualification take", interval=.25)
        try:
            check_program(baseline_path, baseline, seconds, case["profile"], case)
        except Failure as exc:
            case.update(status="FAIL", reason=str(exc))
        else:
            case.update(status="PASS", reason="loss and media checks passed")
            if (width, height) == reference:
                selected = copy.deepcopy(cfg)
                evidence["preview_profile"] = case["profile"].copy()
    require(selected is not None, f"320x180@30 reference failed qualification: {profiles[-1]['reason']}")
    cfg = selected
    e.apply(cfg)
    width, height = (cfg["canvas"][k] for k in ("width", "height"))
    consumers = []
    outputs = {}
    samples = evidence.setdefault("preview_samples", [])
    revisions = evidence.setdefault("preview_revisions", [])
    launched = {}
    try:
        for target in ("program", "scene:standby"):
            launched[target] = time.monotonic()
            process = subprocess.Popen([cli, "frames", target, "--shared", "--check",
                "--width", str(width), "--height", str(height), "--fps", "60",
                "--seconds", "45", "--json"], env=e.env, cwd=e.root,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, encoding="utf-8", errors="replace")
            consumers.append((target, process))
        time.sleep(3)
        for target, process in consumers:
            require(process.poll() is None, f"{target} preview consumer exited before recording")
        evidence["before_preview_take"] = e.rpc("status")
        started = e.rpc("start", {"seconds": 20})
        require(not any("software graphics adapter" in note for note in started["notes"]),
                "within-reference warning repeated on the preview take")
        began = time.monotonic()
        for checkpoint in (2, 8, 16):
            while time.monotonic() - began < checkpoint:
                status = e.rpc("status")
                samples.append({"at_s": time.monotonic() - began, "status": status})
                for target, process in consumers:
                    require(process.poll() is None, f"{target} preview consumer ended during recording")
                time.sleep(.25)
            revision = e.rpc("get")["revision"]
            require(e.rpc("status").get("live") is not None, "recording ended before preview checkpoint")
            changed = e.rpc("apply", {"revision": revision,
                                     "patch": {"name": f"preview checkpoint {checkpoint}"}, "live": True})
            require(changed["revision"] > revision, "preview checkpoint did not advance the setup revision")
            revisions.append(changed["revision"])
            if checkpoint == 8:
                e.rpc("preview", {"scene": "standby"})
                try:
                    e.rpc("frame", {"target": "preview", "timeout_ms": 100})
                except RpcError as exc:
                    evidence["paused_frame_hint"] = str(exc)
                    require("previews paused to protect recording" in str(exc)
                            and "program" in str(exc), f"preview Frame omitted paused hint: {exc}")
                else:
                    raise Failure("preview Frame returned a picture while previews were paused")
        path = Path(started["manifest"])

        def preview_finished():
            samples.append({"at_s": time.monotonic() - began, "status": e.rpc("status")})
            for target, process in consumers:
                require(process.poll() is None, f"{target} preview consumer ended before take finalization")
            return finished(path)

        manifest = until(preview_finished, 45, "preview take finalization", interval=.25)
        stopped = time.monotonic()
        evidence["preview_take"] = {"manifest": str(path), "loss": drop_metrics(manifest)}
        evidence["after_preview_take"] = e.rpc("status")
        revision = e.rpc("get")["revision"]
        resumed_revision = e.rpc("apply", {"revision": revision,
                                           "patch": {"name": "preview resumed"}, "live": True})["revision"]
        require(resumed_revision > revision, "resume check did not advance the setup revision")
        evidence["resume_revision"] = resumed_revision
        # Status is the user-visible explanation; the consumers' actual long
        # inter-frame gap below proves that delivery paused, not just its label.
        protected = [s for s in samples if 4 <= s["at_s"] <= 16]
        require(protected and protected[-1]["at_s"] - protected[0]["at_s"] >= 10 and
                all(s["status"].get("live") is not None and s["status"]["governor"].get("preview_fps") == 0
                    and "previews paused to protect recording" in s["status"]["governor"]["reason"]
                    for s in protected),
                "protected recording did not visibly report paused previews for at least 10 seconds")
        # Poll both together: one stalled subscriber must not gain an extra
        # full timeout merely because the other was collected first.
        deadline = max(launched.values()) + 60
        while any(process.poll() is None for _, process in consumers):
            require(time.monotonic() < deadline, "preview consumers exceeded their 60s wall-time budget")
            evidence.setdefault("resume_samples", []).append(e.rpc("status"))
            time.sleep(.25)
        require(evidence.get("resume_samples") and
                any(s.get("live") is None and s["governor"].get("preview_fps") != 0
                    for s in evidence["resume_samples"]), "previews remained paused after recording")
        for target, process in consumers:
            outputs[target] = process.communicate(timeout=10)
            stdout, stderr = outputs[target]
            require(process.returncode == 0, f"{target} preview failed: {stderr}")
            result = json.loads(stdout)
            evidence.setdefault("previews", []).append(result)
            require(result["ended"] is None and not result["problems"], f"{target} preview ended: {result}")
            require(result["check"]["frames_read_back"] > 0, f"{target} did no GPU readbacks")
            require(45 <= result["seconds"] <= 50, f"{target} preview did not survive its time budget: {result}")
            require(result["frames"] > 1 and result["fps"] > 0 and
                    15000 <= result["interval_ms"]["max"] <= 30000,
                    f"{target} preview did not pause for the take and resume within 30s: {result}")
            # CLI FPS is (count-1)/(last-first); the first ring is reported
            # on its first frame. Bound trailing silence as well as gaps.
            last = result["rings"][0]["at_s"] + (result["frames"] - 1) / result["fps"]
            require(last >= result["seconds"] - 2, f"{target} preview stopped early: {result}")
            require(launched[target] + last >= stopped + 2,
                    f"{target} did not deliver frames after recording stopped: {result}")
            if target == "program":  # scene feeds intentionally have no revision stamp
                seen = {r["revision"] for r in result["revisions"]}
                require(resumed_revision in seen, f"program did not show the post-stop revision: {result}")
        check_program(path, manifest, 20, cfg["canvas"], evidence["preview_take"])
    finally:
        for target, process in consumers:
            if process.poll() is None:
                process.kill()
            if target not in outputs:
                outputs[target] = process.communicate(timeout=10)
            name = target.replace(":", "-")
            (e.root / f"{name}-preview.json").write_text(outputs[target][0])
            (e.root / f"{name}-preview.log").write_text(outputs[target][1])


def scenario_win_isolated(e, cfg, evidence):
    """Real CLI lifetime, dead/reused PID cleanup and recovery-before-deletion."""
    if not WINDOWS:
        raise Skip("Windows isolated record workspace cleanup")
    if os.environ.get("RUNNER_ENVIRONMENT") == "github-hosted":
        raise Skip("GitHub-hosted Windows jobs deny CREATE_BREAKAWAY_FROM_JOB (error 5), "
                   "including the same-user Limited scheduled task; qualify on controlled Windows")
    e.stop()
    temp = e.root / "temp"
    temp.mkdir()
    e.env.update(TMP=str(temp), TEMP=str(temp))
    e.start()
    cfg = setup(e.root, "h264", width=320, height=180, fps=30)
    config = e.root / "record.json"
    write_json(config, cfg)
    # A separate caller registry proves the startup sweep can discover/recover
    # the output even when the main daemon has never recorded in this folder.
    env = dict(e.env, ISOCAST_DAEMON=str(e.binary), ISOCAST_SETUP=str(e.root / "caller" / "setup.toml"))
    cli = e.binary.parent / "isocast.exe"
    argv = [str(cli), "record", str(config), "--json"]
    private = copy.copy(e)  # RPC endpoint only; the CLI owns this daemon.
    other = temp / "isocast-startup-scope"
    other.mkdir()
    write_json(other / "record-owner.json", {"pid": 4294967295, "created": 1})
    os.utime(other / "record-owner.json", (time.time() - 172800,) * 2)
    crash = None
    for label, seconds in (("normal", "10"), ("crash", None)):
        before = set(temp.glob("isocast-*"))
        with (e.root / f"{label}-cli.out").open("wb") as out, (e.root / f"{label}-cli.err").open("wb") as err:
            process = subprocess.Popen(argv + (["--seconds", seconds] if seconds else []),
                                       env=env, stdout=out, stderr=err, cwd=e.root)
            child = None
            try:
                workspace = until(lambda: next(iter(set(temp.glob("isocast-*")) - before), None),
                                  description="CLI workspace")
                private.sock = workspace / "daemon.sock"
                def ready():
                    require(process.poll() is None, "record CLI exited before its daemon became ready")
                    try:
                        return private.rpc("status")
                    except (OSError, RpcError):
                        return None
                status = until(ready, description="CLI-owned daemon")
                child = WindowsChild(status["daemon"]["pid"])
                identity = {"pid": status["daemon"]["pid"], "created": child.created()}
                require(status["daemon"]["parent_owned"], "CLI did not use an owned daemon")
                marker = workspace / "record-owner.json"
                require(marker.exists(), "isolated daemon did not persist its owner identity")
                if seconds:
                    require(other.exists(), "isolated child swept workspaces into its disposable registry")
                    os.utime(marker, (time.time() - 172800,) * 2)
                    e.stop()
                    e.start()
                    require(workspace.exists(), "startup removed a live aged workspace")
                    require(not other.exists(), "main startup did not sweep the stale sentinel")
                    require(process.wait(timeout=45) == 0, "normal record CLI failed")
                    require(child.wait(10000) and child.exit_code() == 0, "normal daemon did not exit cleanly")
                    require(not workspace.exists(), "normal take left its workspace behind")
                    evidence["normal"] = {"owner": identity, "workspace_removed": True}
                else:
                    def committed():
                        files = (private.rpc("status").get("live") or {}).get("files", [])
                        return files if files and all((f.get("committed_seconds") or 0) >= 2 for f in files) else None
                    saved = until(committed, 30, "isolated take durable fragments")
                    killed = command(["taskkill", "/PID", str(process.pid), "/T", "/F"], check=False)
                    require(killed.returncode in (0, 128), "could not terminate isolated tree")
                    process.wait(timeout=10)
                    require(child.wait(10000) and child.exit_code() != 0 and process.returncode != 0,
                            "crash test did not terminate both CLI and daemon")
                    require(read_json(marker) == identity, "stored owner identity differs from the actual process")
                    crash = workspace
                    evidence["crash"] = {"owner": identity, "committed": saved}
            finally:
                if process.poll() is None:
                    command(["taskkill", "/PID", str(process.pid), "/T", "/F"], check=False)
                    process.wait(timeout=10)
                if child:
                    child.close()
    path = next(p for p in (e.root / "recordings").rglob("*.session.json")
                if read_json(p)["status"] == "recording")
    original = read_json(path)
    require(len(original["files"]) == 1, "isolated fixture must have one program file")
    movie = path.parent / original["files"][0]["path"]
    marker = crash / "record-owner.json"
    identity = read_json(marker)
    # Keep young/dead, unmarked, malformed and in-workspace media safe. An
    # unlocked copy tests the PID check independently of the owner's file lock.
    live_pid = os.getpid()
    live = WindowsChild(live_pid)
    live_identity = {"pid": live_pid, "created": live.created()}
    # Release only the handle, not this still-live harness process.
    live.k32.CloseHandle(live.handle)
    live.handle = None
    def fixture(name, owner, old=True):
        directory = temp / f"isocast-{name}"
        directory.mkdir()
        write_json(directory / "record-owner.json", owner)
        if old:
            os.utime(directory / "record-owner.json", (time.time() - 172800,) * 2)
        return directory
    young = fixture("young", identity, old=False)
    reuse = fixture("reused-pid", dict(live_identity, created=live_identity["created"] - 1))
    live_copy = fixture("live-copy", live_identity)
    protected = fixture("media", identity)
    (protected / "take.mp4").write_bytes(b"unrecovered media sentinel")
    malformed = fixture("malformed", {"pid": "not a pid"})
    unmarked = temp / "isocast-unmarked"
    unmarked.mkdir()
    (unmarked / "daemon.log").write_text("legacy workspace")
    os.utime(marker, (time.time() - 172800,) * 2)
    # Hide one committed movie temporarily: recovery must not authorize cleanup
    # based only on a changed manifest status or an empty recovery result.
    held = movie.with_suffix(".held")
    movie.rename(held)
    try:
        e.stop()
        e.start()
        require(crash.exists(), "workspace removed despite unavailable committed media")
        require(not reuse.exists(), "a reused live PID incorrectly protected a dead owner")
        require(live_copy.exists(), "startup removed a workspace held by a live process identity")
    finally:
        held.rename(movie)
    e.stop()
    e.start()
    require(not crash.exists(), "recovered day-old workspace was not removed")
    require(all(p.exists() for p in (young, live_copy, protected, malformed, unmarked)),
            "sweep removed a young/live/unknown/recording workspace")
    require((protected / "take.mp4").read_bytes() == b"unrecovered media sentinel", "workspace media changed")
    recovered = read_json(path)
    require(recovered["status"] == "interrupted", "crashed take was not recovered")
    require(any(Path(t["manifest"]) == path for t in e.rpc("status").get("interrupted", [])),
            "workspace-only recovery was omitted from status")
    file = recovered["files"][0]
    info = probe(movie)
    video = next(s for s in info["streams"] if s["codec_type"] == "video")
    require(int(video["nb_read_frames"]) == file["frames"] and file["frames"] > 0,
            "recovered file and manifest frames disagree")
    require(float(video["duration"]) + .001 >= evidence["crash"]["committed"][0]["committed_seconds"],
            "cleanup lost committed recording media")
    ffmpeg("-xerror", "-i", movie, "-map", "0", "-f", "null", "-")
    digest = hashlib.sha256(movie.read_bytes()).hexdigest()
    e.stop()
    e.start()
    require(hashlib.sha256(movie.read_bytes()).hexdigest() == digest and read_json(path) == recovered,
            "repeated startup changed recovered media or metadata")
    evidence["recovered"] = {"manifest": str(path), "frames": file["frames"], "sha256": digest,
                              "workspace_removed": True, "recovery_failure_retained": True}


def scenario_win_machine_profile(e, cfg, evidence):
    """A real record child reads the main machine profile, but cannot write it."""
    if not WINDOWS:
        raise Skip("Windows isolated machine profile")
    if os.environ.get("RUNNER_ENVIRONMENT") == "github-hosted":
        raise Skip("GitHub-hosted Windows jobs deny CREATE_BREAKAWAY_FROM_JOB (error 5), "
                   "including the same-user Limited scheduled task; qualify on controlled Windows")
    cfg = setup(e.root, "h264", width=320, height=180, fps=30)
    identity = e.rpc("status")["daemon"]
    machine = e.rpc("doctor", {"setup": cfg})["machine"]
    # doctor enriches native().machine() with an encoder probe. Calibration's
    # Windows machine identity is the cheap startup query, with an empty list.
    machine["hardware_encoders"] = []
    marker = f"e2e-117-{os.getpid()}"
    profile = {"schema_version": 2, "measurement_version": 5,
               "app_version": identity["version"], "app_build": identity["build"],
               "measured_at": marker, "machine": machine, "seconds": 3.0,
               "graphics": [], "skipped": [], "complete": True, "notes": [],
               "memory_copy_bytes_per_second": 123456789.0, "disk_write_bytes_per_second": None,
               "encoders": [{"codec": "prores422", "bit_depth": 8, "width": 1920,
                   "height": 1080, "fps": 60, "sessions": 2, "frames": 300, "seconds": 1.0,
                   "frames_per_second": 300.0, "resource": "recording_writer",
                   "backend": "videotoolbox", "evidence": "prores_resource",
                   "rate_control": "constant_quality", "quality": 70,
                   "hardware_accelerated": True, "process_cpu_seconds_per_frame": 0.0001}]}
    # Synthetic valid wire fixture, not a claim Windows measured ProRes. Reading
    # both endpoints proves matching validation, not merely an inherited env var.
    path = e.root / "setup.machine.json"
    config = e.root / "record.json"
    write_json(config, cfg)
    temp = e.root / "temp"
    temp.mkdir()
    env = dict(e.env, ISOCAST_DAEMON=str(e.binary), TMP=str(temp), TEMP=str(temp))
    private = copy.copy(e)
    for label in ("valid", "wrong_build"):
        if label == "wrong_build":
            profile["app_build"] += "-other"
        write_json(path, profile)
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        e.stop()
        e.start()
        main = e.rpc("machine_profile")
        require((main.get("profile") is not None) == (label == "valid"), "main profile fixture was not validated")
        before = set(temp.glob("isocast-*"))
        with (e.root / f"{label}-cli.out").open("wb") as out, (e.root / f"{label}-cli.err").open("wb") as err:
            process = subprocess.Popen([str(e.binary.parent / "isocast.exe"), "record", str(config),
                                        "--seconds", "4", "--json"], env=env, stdout=out, stderr=err, cwd=e.root)
            try:
                workspace = until(lambda: next(iter(set(temp.glob("isocast-*")) - before), None),
                                  description="profile test workspace")
                private.sock = workspace / "daemon.sock"
                def loaded():
                    require(process.poll() is None, "record CLI exited before profile check")
                    try:
                        return private.rpc("machine_profile")
                    except (OSError, RpcError):
                        return None
                inherited = until(loaded, description="isolated machine profile")
                require(inherited.get("profile") == main.get("profile"), "isolated daemon did not read the main profile")
                if label == "valid":
                    require(inherited["profile"]["measured_at"] == marker
                            and inherited["profile"]["memory_copy_bytes_per_second"] == 123456789.0,
                            "main machine-profile setting did not reach the child")
                else:
                    require("No valid machine profile for this build at" in inherited["summary"],
                            "invalid inherited profile did not explain its fallback")
                try:
                    private.rpc("machine_calibrate")
                except RpcError as exc:
                    require(exc.error["code"] == -32001 and "only reads the machine profile" in str(exc),
                            f"inherited profile was not protected read-only: {exc}")
                else:
                    raise Failure("isolated daemon allowed calibration of its parent's profile")
                require(process.wait(timeout=45) == 0, "profile test recording failed")
                evidence[label] = {"profile": inherited.get("profile"), "summary": inherited["summary"],
                                   "parent_unchanged": hashlib.sha256(path.read_bytes()).hexdigest() == digest}
                require(evidence[label]["parent_unchanged"], "isolated daemon modified the main profile")
            finally:
                if process.poll() is None:
                    command(["taskkill", "/PID", str(process.pid), "/T", "/F"], check=False)
                    process.wait(timeout=10)
    require(not list(e.root.glob(".calibration-*")) and not path.with_suffix(".failed-build").exists(),
            "isolated daemon wrote parent calibration state")


def scenario_win_recovery(e, cfg, evidence):
    """Terminate the engine, then its whole tree, without orderly finalization.
    The tree case kills a harness parent and its actual recording-engine child.
    It validates the harness tree path; both cases use TerminateProcess on the engine.
    This models process loss, not loss of the OS/disk's write cache."""
    if not WINDOWS:
        raise Skip("Windows process-tree crash recovery")
    # Recovery integrity, not throughput: also runnable with WARP/MF software
    # encoding on windows-latest. No hosted frame-loss allowance is involved.
    cfg = setup(e.root, "h264", width=640, height=360, fps=30)
    evidence["recovery_profile"] = cfg["canvas"].copy()
    for label, tree in (("engine", False), ("process_tree", True)):
        case = evidence.setdefault("cases", {}).setdefault(label, {})
        fragment_recovery(e, copy.deepcopy(cfg), case, kill_tree=tree)


def fragment_recovery(e, cfg, evidence, kill_tree=False):
    if kill_tree:
        e.stop()
        e.start(crash_parent=True)
    win_setup(e, cfg)
    e.apply(cfg)
    started = e.rpc("start", {})
    path = Path(started["manifest"])

    def committed():
        files = (e.rpc("status").get("live") or {}).get("files", [])
        return files if len(files) == 3 and all((f.get("committed_seconds") or 0) >= 2 for f in files) else None
    saved = until(committed, 30, "two committed seconds in every output")
    evidence["committed_before_kill"] = {f["source"]: f["committed_seconds"] for f in saved}
    children = None
    if WINDOWS:
        children = json.loads(command(["powershell", "-NoProfile", "-NonInteractive", "-Command",
            "[Console]::OutputEncoding=[Text.Encoding]::UTF8; ConvertTo-Json -Compress -InputObject "
            f"@(Get-CimInstance Win32_Process -Filter 'ParentProcessId={e.process.pid}' | "
            "Select-Object ProcessId,Name)"]).stdout.decode("utf-8-sig"))
    require(e.process.poll() is None, "engine exited before crash injection")
    daemon = e.process.pid
    if kill_tree:
        require(WINDOWS, "process-tree crash injection requires Windows")
        daemon = e.rpc("status")["daemon"]["pid"]
        require(any(p["ProcessId"] == daemon for p in children) and daemon != e.process.pid,
                "tree crash must kill an actual recording-engine child, not just its parent")
        killed = command(["taskkill", "/PID", str(e.process.pid), "/T", "/F"], check=False)
        evidence["taskkill_exit"] = killed.returncode  # a child may exit during enumeration
        require(killed.returncode in (0, 128), f"taskkill failed: {killed.returncode}")
    else:
        e.process.kill()
    e.process.wait(timeout=10)
    if not WINDOWS:
        require(e.process.returncode == -signal.SIGKILL, "daemon was not killed with SIGKILL")
    else:
        require(e.process.returncode != 0, "daemon exited normally instead of being terminated")
    evidence["termination"] = {"pid": e.process.pid, "exit": e.process.returncode,
                               "tree": kill_tree, "children": children}
    if kill_tree:
        require(e.child.wait(10000), "recording-engine child survived process-tree termination")
        evidence["termination"]["daemon_exit"] = e.child.exit_code()
        require(evidence["termination"]["daemon_exit"] != 0, "recording-engine child exited normally")
        e.stop()  # release the retained child handle before restarting
    before = read_json(path)
    require(before["status"] == "recording", "killed take was already marked finished")
    require(len(before["files"]) == 3 and {f["source"] for f in before["files"]} == {"program", "bars", "bars2"},
            "killed take did not publish the program and both ISO files")
    partials = {}
    if WINDOWS:
        # Deterministically cover the tiny metadata-write/rename crash window
        # too. These are abandoned metadata, not MOV/MP4 fragments: neither a
        # complete unpublished copy nor a torn JSON copy may replace the last
        # published manifest or masquerade as another take.
        for serial, data in enumerate((path.read_bytes(), b'{"status":"recording",')):
            orphan = path.parent / f".isocast-{daemon}-0-{serial:x}.partial"
            with orphan.open("xb") as file:
                file.write(data)
            partials[orphan] = data

    def header_kind(movie):
        with open(movie, "rb") as file:
            header = file.read(8)
            require(len(header) == 8 and header[4:] == b"ftyp", "missing movie file type")
            size = int.from_bytes(header[:4], "big")
            require(8 <= size <= 1024, "invalid movie file type size")
            file.seek(size + 4)
            return file.read(4)
    for f in before["files"]:
        require(header_kind(path.parent / f["path"]) == b"free", "crash file was already finalized")

    e.start()
    recovered = until(lambda: (m if m["status"] == "interrupted" and
                     all(f.get("frames") is not None for f in m["files"]) else None)
                     if (m := read_json(path)) else None, 30, "startup fragment recovery")
    require(len(recovered["files"]) == 3 and
            {(f["path"], f["source"]) for f in recovered["files"]} ==
            {(f["path"], f["source"]) for f in before["files"]},
            "recovery lost or relabeled an output")
    interrupted = [r for r in e.rpc("status").get("interrupted", []) if Path(r["manifest"]) == path]
    require(len(interrupted) == 1 and interrupted[0]["recording"] == recovered,
            "status did not expose the recovered take with its persisted facts")
    project = read_json(path.parent / ".isocast-project.json")["id"]
    listed = [p for p in e.rpc("list_projects")["projects"] if p["id"] == project]
    require(len(listed) == 1 and sum(Path(t) == path for t in listed[0]["takes"]) == 1,
            "project listing omitted or duplicated the recovered take")
    evidence["manifest"] = str(path)
    evidence["files"] = []
    for f in recovered["files"]:
        movie = path.parent / f["path"]
        require(header_kind(movie) == b"mdat", "startup did not finalize the movie header")
        info = probe(movie)
        video = next(s for s in info["streams"] if s["codec_type"] == "video")
        audio = next(s for s in info["streams"] if s["codec_type"] == "audio")
        require(video["codec_name"] == "h264" and audio["codec_name"] == "aac", "wrong recovered codecs")
        require(int(video["nb_read_frames"]) == f["frames"] and f["frames"] >= 2,
                "recovered manifest disagrees with decoded frames")
        require(min(float(video["duration"]), float(audio["duration"])) + .001 >=
                evidence["committed_before_kill"][f["source"]],
                "committed media was lost during recovery")
        require(abs(max(float(video["duration"]), float(audio["duration"])) - f["duration_seconds"]) < .001,
                "recovered manifest duration disagrees with indexed media")
        require(f.get("dropped_frames") is None and f.get("audio_dropouts") is None,
                "recovery fabricated loss counters from incomplete checkpoints")
        ffmpeg("-xerror", "-i", movie, "-map", "0", "-f", "null", "-")
        evidence["files"].append({"file": movie.name, "frames": f["frames"],
                                  "video_seconds": float(video["duration"]),
                                  "audio_seconds": float(audio["duration"])})
    # A second launch must not append another moov or rewrite stable facts.
    hashes = {f["path"]: hashlib.sha256((path.parent / f["path"]).read_bytes()).hexdigest()
              for f in recovered["files"]}
    e.stop()
    e.start()
    require(read_json(path) == recovered, "repeated startup changed the recovered manifest")
    require(all(hashlib.sha256((path.parent / name).read_bytes()).hexdigest() == digest
                for name, digest in hashes.items()),
            "repeated startup rewrote an already finished movie")
    require(all(orphan.read_bytes() == data for orphan, data in partials.items()),
            "recovery changed unpublished metadata orphans")
    evidence["retained_metadata_partials"] = [p.name for p in partials]


def scenario_win_media(e, cfg, evidence):
    """A looping H.264/AAC MP4 (the frame oracle's block-code clip, 4 s, a
    marker burst each second) decoded by Media Foundation beside the
    engine's test pattern: the program and the media ISO, every picture
    against the oracle across the loops (the file's own rate, clean wraps),
    each burst within a frame of its picture, under the run's loss policy
    (zero drops without --hosted). 18 s below 720p (the hosted no-GPU
    reference, --windows-profile 180p30), else 60 s."""
    w, h, fps = (cfg["canvas"][k] for k in ("width", "height", "fps"))
    seconds, clip_seconds = (18 if h < 720 else 60), 4
    clip = oracle_fixture(e.root.parent / "fixtures", ORACLE_TAGS["a"], fps, clip_seconds, mp4=True)
    count = clip_seconds * fps
    cfg["sources"] = [s for s in cfg["sources"] if s["id"] in ("bars", "tone")]
    cfg["sources"].append({"id": "a", "name": "Clip", "kind": "media", "files": [str(clip)], "loop": True})
    layout = [("a", (0, 0, 0.75, 0.75))]
    cfg["scenes"] = [{"id": "main", "name": "Main",
                      "items": [item("a", layout[0][1], (w, h)), item("bars", (0.75, 0.75, 0.25, 0.25), (w, h))]}]
    cfg["recording"]["program"]["audio"] = ["a"]
    cfg["recording"]["iso"].update(enabled=True, sources=["a"], audio=["a"])
    e.apply(cfg)
    path, manifest = take(e, seconds)
    evidence["files"] = check_take(path, manifest, seconds, evidence=evidence)
    require(sorted(f["source"] for f in manifest["files"]) == ["a", "program"],
            f"files: {[f['source'] for f in manifest['files']]}")
    evidence["clip"] = {"frames": count, "loops": round(seconds / clip_seconds, 2)}
    evidence["t0"] = check_record_take(e, path, manifest, cfg, layout, count, evidence, isos=("a",))


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
    slots = split_slots(takes[0][1], takes[1][1], cfg["canvas"]["fps"])
    check_split_end(*takes[0], slots, cfg["canvas"]["fps"])
    check_pattern_take(e, *takes[1], cfg, layout, evidence, t0=(t0 + slots) % pattern_period(cfg["canvas"]["fps"]),
                       label="take 2 (split)")
    evidence.update(t0=t0, first_take_slots=slots)


def scenario_win_metadata(e, cfg, evidence):
    """#157: a reader holding a take's manifest open without FILE_SHARE_DELETE
    (Python's ordinary open) blocks its final publication. The finished media
    waits: status lists the take in diagnostics.metadata_pending, doctor
    names the problem and take_finished is withheld; once the reader closes,
    the manifest publishes complete with the final counts. Both an earlier
    split take (while recording goes on) and the last take (finalizing)."""
    if not WINDOWS:
        raise Skip("Windows rename-over sharing semantics")
    cfg["recording"]["iso"]["enabled"] = False
    e.apply(cfg)
    # `stopped` follows the final take_finished. End the synchronous Windows
    # pipe read there: closing its file from here during a read can deadlock.
    watch = Watch(e, ["take_finished", "stopped"], stop_after="stopped")
    held = {}

    def hold_manifest(path):
        # A just-renamed manifest can still have the publisher's DELETE handle.
        # Retry acquisition only; once open, this reader must block publication.
        deadline = time.monotonic() + 1
        while True:
            try:
                return open_shared(path, share_delete=False)
            except PermissionError as exc:
                if exc.winerror != 32 or time.monotonic() >= deadline:  # ERROR_SHARING_VIOLATION
                    raise
                time.sleep(0.01)

    def pending(path, status=None):
        status = status or e.rpc("status")
        entries = (status.get("diagnostics") or {}).get("metadata_pending") or []
        return [m for m in entries if path.name in m]

    def status_when(test):
        status = e.rpc("status")
        return status if test(status) else None

    def blocked(number, path):
        status = until(lambda: status_when(lambda s: pending(path, s)), 30,
                       f"take {number}'s metadata to be pending")
        checks = [c for c in e.rpc("doctor", {})["checks"]
                  if c["name"] == "recording metadata publication" and path.name in c["detail"]]
        require(checks and all(c["status"] == "problem" and c.get("fix") for c in checks),
                f"doctor did not report take {number}'s pending metadata: {checks}")
        require(read_json(path)["status"] == "recording", f"take {number}'s manifest published while held")
        require(not [t for t in watch.of("take_finished") if t["take"] == number],
                f"take_finished for take {number} before its manifest published")
        return {"state": status["state"], "finalizing": status.get("finalizing"),
                "pending": pending(path), "doctor": checks}

    def release(number, path):
        held.pop(path).close()
        until(lambda: not pending(path), 30, f"take {number}'s metadata to publish")
        events = until(lambda: [t for t in watch.of("take_finished") if t["take"] == number], 30,
                       f"take_finished for take {number}")
        require(len(events) == 1 and Path(events[0]["manifest"]) == path and events[0]["status"] == "complete",
                f"take_finished for take {number}: {events}")
        manifest = until(lambda: finished(path), 30, f"take {number}'s final manifest")
        return check_take(path, manifest, evidence=evidence)

    try:
        first = Path(e.rpc("start", {})["manifest"])
        held[first] = hold_manifest(first)
        time.sleep(1.5)
        split = e.rpc("split")
        second = Path(split["manifest"])
        held[second] = hold_manifest(second)
        evidence["split"] = blocked(1, first)
        require(evidence["split"]["state"] == "recording", f"recording stopped behind take 1: {evidence['split']}")
        require(not evidence["split"]["finalizing"], "a pending earlier take must not disable Stop on the current take")
        evidence["files"] = release(1, first)
        time.sleep(1.0)
        accepted = e.rpc("request_stop", {})
        require(accepted.get("finalizing"), f"request_stop: {accepted}")
        evidence["stop"] = blocked(2, second)
        require(evidence["stop"]["state"] != "idle" and evidence["stop"]["finalizing"] is True,
                f"status while take 2's metadata is pending: {evidence['stop']}")
        time.sleep(2.0)  # the finalizer keeps retrying, not giving up
        evidence["stop_after_retry"] = blocked(2, second)
        evidence["files"] += release(2, second)
        status = until(lambda: status_when(lambda s: s["state"] == "idle"), 30, "idle recorder")
        require(not status.get("finalizing") and not (status.get("diagnostics") or {}).get("metadata_pending"),
                f"idle with metadata still pending: {status.get('diagnostics')}")
        watch.thread.join(timeout=10)
        require(not watch.thread.is_alive() and watch.of("stopped"), "watch did not finish at stopped")
        require([t["take"] for t in watch.of("take_finished")] == [1, 2], "completion callbacks must arrive exactly once")
    finally:
        for file in held.values():
            file.close()
        # On an earlier assertion failure the worker shuts down the daemon;
        # don't block reporting that failure by closing a still-reading pipe.
        if not watch.thread.is_alive():
            watch.close()

    # Exact terminal facts must survive both an abrupt process loss and a
    # graceful shutdown while the public manifest remains locked (#158 B1).
    for mode in ("kill", "shutdown"):
        e.apply(cfg)
        path = Path(e.rpc("start", {"seconds": 2})["manifest"])
        with hold_manifest(path):
            until(lambda: pending(path), 30, f"{mode}: terminal metadata pending")
            saved = read_json(str(path) + ".final")
            require(saved["status"] == "complete", f"{mode}: sidecar has no complete outcome")
            began = time.monotonic()
            if mode == "kill":
                e.process.kill()
                e.process.wait(timeout=15)
            else:
                # A synchronous pipe read has no timeout on Windows. Bound the
                # process exit while we still hold the manifest, then read the reply.
                with engine_connection(e.sock, timeout=15) as sock:
                    sock.sendall(b'{"jsonrpc":"2.0","id":1,"method":"shutdown"}\n')
                    e.process.wait(timeout=15)
                    with sock.makefile("rb") as reader:
                        reply = json.loads(reader.readline())
                    require("result" in reply, f"shutdown failed: {reply}")
            elapsed = time.monotonic() - began
            require((e.process.returncode == 0) == (mode == "shutdown"),
                    f"{mode}: unexpected daemon exit {e.process.returncode}")
            require(read_json(path)["status"] == "recording", "held manifest was unexpectedly replaced")
        e.start()
        recovered = until(lambda: finished(path), 30, f"{mode}: final sidecar adoption")
        require(recovered == saved, f"{mode}: recovery changed the durable final outcome")
        evidence.setdefault("restart", {})[mode] = {
            "exit_seconds": elapsed, "manifest": str(path),
            "files": check_take(path, recovered, evidence=evidence),
        }


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
        if args.worker == 'linux_media':
            # Print from native stdout while the parent is expecting video bytes.
            ENGINE_ENV['ISOCAST_TEST_MEDIA_STDOUT'] = '1'
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
            try:
                engine.close()
            except Exception as exc:
                status, reason = "FAIL", f"{reason}; engine cleanup failed: {exc}"
        # Preserve every output's counters even when an earlier assertion
        # failed, except the qualification sweep's deliberately rejected
        # profiles: their counters stay in headless_profiles, not suite loss.
        try:
            rejected_profiles = {Path(case["manifest"]) for case in evidence.get("headless_profiles", [])
                                 if case.get("status") == "FAIL" and case.get("manifest")
                                 and (case["profile"]["width"], case["profile"]["height"]) != WINDOWS_PROFILES["180p30"][:2]}
            for path in (root / "recordings").rglob("*.session.json"):
                if path.is_symlink() or not path.is_file():
                    continue  # plugin_files plants dangling links
                if path in rejected_profiles:
                    continue
                manifest = read_json(path)
                if manifest["status"] != "recording":
                    evidence.setdefault("drops", {}).update({str(path.parent / row["output"]): row
                                                            for row in drop_metrics(manifest)})
        except Exception as exc:
            status, reason = "FAIL", f"{reason}; recording evidence collection failed: {exc}"
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
    failed_profiles = any(p["status"] == "FAIL" for p in
                          result.get("evidence", {}).get("headless_profiles", []))
    keep = args.keep or failed_profiles or (args.keep_failures and result["status"] == "FAIL")
    result["artifacts"] = str(case) if keep else None
    losses = list(result.get("evidence", {}).get("drops", {}).values())
    detail = ""
    if losses:
        dropped, slots = (sum(row[k] for row in losses) for k in ("dropped", "slots"))
        detail = f"; video drops {dropped}/{slots} ({dropped / slots:.1%})" if slots else "; no frame slots"
    for phase in result.get("evidence", {}).get("skipped_phases", []):
        print(f"SKIP {name}: {phase}", file=sys.stderr if args.json else sys.stdout, flush=True)
    print(f"{result['status']:4} {name}: {result['reason']}{detail}",
          file=sys.stderr if args.json else sys.stdout, flush=True)
    if not keep:
        shutil.rmtree(case)
    return result


def suite_losses(report):
    """Accounted video losses over every scenario's outputs (not the
    qualification): dropped slots, held oracle pictures and late-starting
    slots, against all slots."""
    scenarios = [r for r in report.get("scenarios", []) if r["name"] != "capacity"]
    rows = [row for r in scenarios for row in r.get("evidence", {}).get("drops", {}).values()]
    unrowed = [row for r in scenarios for row in r.get("evidence", {}).get("unrowed_holds", [])]
    dropped = sum(row["dropped"] for row in rows)
    held = sum(row.get("held", 0) for row in rows + unrowed)
    # A program's late head is already in its dropped and slots (#150).
    late = sum(max(0, row.get("late_start_slots", 0) - row.get("head", 0)) for row in rows)
    slots = sum(row["slots"] for row in rows + unrowed) + late
    return {"dropped": dropped, "held": held, "late_start_slots": late, "accounted": dropped + held + late,
            "slots": slots, "ratio": (dropped + held + late) / slots if slots else 0.0}


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
    if caps.get("max_drop_ratio"):
        entries.append(("Hosted caps", f"suite {HOSTED_SUITE_LOSS_LIMIT:.0%} accounted; {HOSTED_HOLD_CAP} held "
                                       f"pictures per output; {HOSTED_LATE_START_CAP} late-start slots per file"))
    suite = report.get("suite_losses")
    if suite:
        entries.append(("Suite accounted losses", f"{suite['dropped']} dropped + {suite['held']} held + "
                                                  f"{suite['late_start_slots']} late / {suite['slots']} slots "
                                                  f"({suite['ratio']:.2%})"))
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
                  "| Scenario / output | Stored/sent frames | Dropped / slots | Ratio | Held | Late start | "
                  "Delivered / target fps | Audio dropouts |",
                  "|---|---:|---:|---:|---:|---:|---:|---:|"])
    groups = [("qualification", qualification.get("drops", []))] if qualification else []
    groups += [(r["name"], list(r.get("evidence", {}).get("drops", {}).values()))
               for r in report.get("scenarios", []) if r["name"] != "capacity"]
    for name, losses in groups:
        for row in losses:
            ratio = f"{row['ratio']:.2%}" if row.get("ratio") is not None else "n/a"
            rate = f"{row['delivered_fps']:.2f}" if row.get("delivered_fps") is not None else "n/a"
            lines.append(f"| {cell(name)} / {cell(row['output'])} | {row['frames']} | {row['dropped']} / {row['slots']} | "
                         f"{ratio} | {row.get('held', 0)} | {row.get('late_start_slots', 0)} | {rate} / {row['target_fps']} | "
                         f"{row['audio_dropouts']} |")
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
    parser.add_argument("--only", action="extend", type=lambda s: s.split(","), metavar="SCENARIO[,SCENARIO...]",
                        help="run only the named scenarios (repeatable; see --list)")
    parser.add_argument("--keep", action="store_true")
    parser.add_argument("--keep-failures", action="store_true")
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--timeout", type=float, default=180, help="hard timeout per scenario, seconds")
    parser.add_argument("--codec", choices=("hevc", "h264", "prores422"),
                        help="exercise a specific recording path (default: available hardware, else ProRes)")
    parser.add_argument("--linux-profile", choices=("720p30", "360p30"),
                        help="Linux synthetic profile: 720p30 by default; 360p30 for four-vCPU software-GPU CI")
    parser.add_argument("--windows-profile", choices=tuple(WINDOWS_PROFILES),
                        help="Windows canvas: 180p30 is the hosted no-GPU reference (strict, without --hosted); "
                             "1080p30 a GPU machine's strict run")
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
    for name in args.only or []:
        if name not in SCENARIOS:
            parser.error(f"unknown --only scenario {name!r}; choose from {', '.join(SCENARIOS)}")
    if args.linux_profile and not LINUX:
        parser.error("--linux-profile is only for Linux synthetic recording checks")
    if args.windows_profile and not WINDOWS:
        parser.error("--windows-profile is only for Windows")
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
        try:
            require(len(SCENARIOS) == len(set(SCENARIOS)), "duplicate name in SCENARIOS")
            for system, selected, expected in (
                ("win32", ["win_preview_recording"], True),
                ("win32", ["win_preview_recording", "win_record"], False),
                ("win32", ["win_record"], False),
                ("darwin", ["win_preview_recording"], False),
                ("win32", [], False),
            ):
                require(scenarios_check_capacity(selected, system) == expected,
                        f"wrong hosted capacity preflight decision for {selected} on {system}")
            print(json.dumps(oracle_selftest(args.work), indent=2))
        except Failure as exc:
            print(f"FAIL selftest: {exc}")
            return 1
        return 0
    cli_name = "isocast.exe" if WINDOWS else "isocast"
    if args.cli is None and (args.daemon.parent / cli_name).is_file():
        args.cli = args.daemon.parent / cli_name
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
        require(sys.platform == "darwin" or WINDOWS or LINUX, "macOS, Windows or Linux is required")
        for tool in ("ffmpeg", "ffprobe") + (("codesign",) if sys.platform == "darwin" else ()):
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
                caps["profile"] = {"width": 1280, "height": 720, "fps": 30} if args.hosted or LINUX else {
                    "width": 1920, "height": 1080, "fps": 60}
                if args.linux_profile == "360p30":
                    caps["profile"] = {"width": 640, "height": 360, "fps": 30}
                if args.windows_profile:
                    width, height, fps = WINDOWS_PROFILES[args.windows_profile]
                    caps["profile"] = {"width": width, "height": height, "fps": fps}
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
            qualify = args.hosted and not args.capabilities_from
            if qualify and not args.capabilities and scenarios_check_capacity(args.only or SCENARIOS, sys.platform):
                print(f"e2e: skipping generic hosted capacity test: {', '.join(args.only or SCENARIOS)} "
                      "runs its own capacity sweep", file=sys.stderr)
            elif qualify:
                result = run_case(args, root, "capacity")
                report["scenarios"].append(result)
                measured = result.get("evidence", {})
                losses = list(measured.get("drops", {}).values())
                zero = result["status"] == "PASS" and bool(losses) and all(r["dropped"] == 0 for r in losses)
                # Hosted runs always account losses: a zero-drop sample here
                # did not predict a zero-drop suite (every hosted run from
                # 2026-10-17 to 10-22 dropped frames in some later scenario).
                # Zero drops are the test Macs' bar (no --hosted).
                caps["max_drop_ratio"] = HOSTED_DROP_LIMIT
                caps["machine_info"] = measured.get("machine_info", {})
                caps["daemon_sha256"] = hashlib.sha256(args.daemon.read_bytes()).hexdigest()
                caps["qualification"] = {"status": result["status"], "drops": losses,
                    "reason": (f"Two 5-second program+2-ISO takes and {2 if caps['h264'] else 0} 7-second streams: "
                               + ("zero drops. " if zero else f"{result['reason']}. ")
                               + "Hosted video loss limit is 25% per output (zero drops are enforced on test Macs); "
                               "all integrity and picture checks and zero audio loss remain mandatory.")}
                write_json(root / "capabilities.json", caps)
                print(json.dumps(caps, indent=2), file=sys.stderr)
                require(result["status"] == "PASS", "hosted profile qualification failed; see capacity results")
            if not args.capabilities:
                for name in args.only or SCENARIOS:
                    report["scenarios"].append(run_case(args, root, name))
                if caps.get("max_drop_ratio"):
                    report["suite_losses"] = suite = suite_losses(report)
                    require(suite["ratio"] <= HOSTED_SUITE_LOSS_LIMIT,
                            f"the suite's accounted losses (dropped + held + late starts) are "
                            f"{suite['accounted']}/{suite['slots']} ({suite['ratio']:.1%}), over the hosted "
                            f"{HOSTED_SUITE_LOSS_LIMIT:.0%} for the whole suite")
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
