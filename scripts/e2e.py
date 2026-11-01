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

Linux capture: --only linux_capture records the real capture paths (the
ScreenCast portal over PipeWire, PipeWire desktop audio, a microphone and a
V4L2 camera) on the headless desktop of scripts/linux-capture-fixture.sh
(source its session.env first; without it the scenario is skipped).
"""

import argparse
import array
import base64
import bisect
import copy
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
SCENARIOS = ("setup", "record", "iso", "split", "scenes", "stream", "webrtc", "nle",
             "data", "plugin", "hotkeys", "shutdown", "pause", "bug_report", "projects",
             "plugin_files", "replay", "vertical", "remote_files",
             # The frame oracle (#124), defined below.
             "oracle_record", "oracle_scenes", "oracle_takes", "oracle_masks",
             "oracle_canvases", "oracle_replay", "oracle_governor",
             # The same model on the engine's test pattern; win_media plays a real file.
             "win_record", "win_takes", "win_scenes", "win_recovery", "win_preview_recording", "win_monitor") + (("linux_record", "linux_recovery", "linux_media", "linux_capture", "linux_browser") if sys.platform.startswith("linux") else ()) + (("win_media", "win_metadata") if sys.platform == "win32" else ())
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
# Hosted only: the webrtc scenario's take may lose program frames while its
# recording starts, as hosted takes do without any monitor (#147/#149: 1 and
# 8 frames, all within 1.9 s of start; 18 hosted runs 2026-10-25: the webrtc
# take lost frames in 2, monitor-free projects/nle takes in 22%/8%). After
# this window, with the WHEP viewer and 960-wide monitor still running, the
# take must lose nothing; a test Mac allows no loss at all.
HOSTED_MONITOR_TAKE_START_SECONDS = 3.0
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


def open_shared(path):
    """Share deletion with publishers and close promptly after reading.
    Python's open() on Windows shares read and write but not delete, so a
    manifest held open here would make the engine's rename-over fail. Ask
    CreateFileW for FILE_SHARE_DELETE too, as Rust's File::open does."""
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
    generic_read, share_all, open_existing, normal = 0x80000000, 0x7, 3, 0x80
    handle = kernel32.CreateFileW(os.fspath(path), generic_read, share_all, None,
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
WINDOWS_PROFILES = {"180p30": (320, 180, 30), "270p30": (480, 270, 30), "1080p30": (1920, 1080, 30)}


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

    def start(self, crash_parent=False):
        # Its own folder as the working directory: anything it (or a tool it
        # starts) writes to a relative path stays in the scenario's folder.
        self.crash_parent = crash_parent
        argv = [str(self.binary)]
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
        if crash_parent:
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
    HOSTED_MONITOR_TAKE_START_SECONDS in); any later loss fails, and a source
    missing from that snapshot counts every loss as later. The losses before
    it stay in check_take's hosted per-output and suite budgets."""
    if startup is None:
        for loss in drop_metrics(manifest):
            check_loss(loss, 0)
        return None
    later = {f["source"]: f["dropped_frames"] - startup.get(f["source"], 0)
             for f in manifest["files"] if f.get("dropped_frames") is not None}
    require(not any(later.values()),
            f"monitor take lost frames after its first {snapshot_seconds:g} s with monitors running: {later}")
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
    evidence["files"] = check_take(path, manifest, 5, sync=True, expected_sources=["program"], evidence=evidence)


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
        # This protection take requires zero loss on a test Mac. Hosted, only
        # its recording start may lose frames (within the hosted per-output
        # limit, counted in the suite budget); viewer eviction is allowed
        # instead of a failure.
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
        "hosted: monitor take lost 1 frame after the snapshot": (
            monitor_take(9, {"program": 8}), "fail"),
        "hosted: monitor take source missing from the snapshot": (
            monitor_take(1, {"other": 0}), "fail"),
        "strict: monitor take lost 1 frame at its start": (monitor_take(1, None), "fail"),
        "strict: monitor take lost nothing": (monitor_take(0, None), "pass"),
    })
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
        # Project both edges of the fifth glyph: in the 180p square inset
        # it is only 1.4 x 2 pixels. Flooring the origin and size separately
        # can leave just a black pixel above the '2'. Read the actual mean;
        # resizing a tiny crop to a single row can skip lit strokes.
        left, right = (round(x + col * s * sx) for col in (30, 35))
        top, bottom = (round(y + line * s * sy) for line in (6, 13))
        graph.append(f"[i{3 * k + 1}]crop={max(1, right-left)}:{max(1, bottom-top)}:{left}:{top},"
                     f"geq=lum='psum(W-1,H-1)/(W*H)',scale={src_w}:1:flags=neighbor[c{k}];")
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
            cell = picture[base + src_w]
            band = sum(picture[base + 2 * src_w:base + 3 * src_w]) / src_w
            row[name] = (bars, cell, band)
        rows.append((t, row))
    return rows


def pattern_value(bars, cell, width, fps, sampled_width=None):
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
    if sampled_width is not None and sampled_width < period:
        # Less than one output pixel per frame of scroll. Two adjacent
        # frames can put the black/white edge on the same pixel; the other
        # seven edges land at different pixel fractions. Measure how much
        # of each side's luma is in a small window around the coarse edge.
        radius = width // 32
        corrections = []
        for stripe in range(8):
            edge = ((stripe * width + 7) // 8 - shift) % width
            if radius and radius <= edge <= width - radius:
                window = bars[edge-radius:edge+radius]
                left, right = window[0], window[-1]
                if right != left:
                    corrections.append((sum(window) - radius * (left + right)) / (right - left))
        if not corrections:
            return ("garbled",)
        shift = (shift + statistics.mean(corrections)) % width
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
    widths = {name: rect[2] for name, rect, _, _ in specs}
    for t, sampled in pattern_samples(path, specs, *src):
        slot = round(t * fps)
        require(abs(t * fps - slot) < 0.02, f"{Path(path).name}: a picture at {t:.5f} s is off the frame grid")
        rows.append((slot, t, {n: pattern_value(b, c, src[0], fps, widths[n]) for n, (b, c, _) in sampled.items()}))
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
        checked = oracle_check_rows(f"{label} {source}", file, rows, expected(offset), e.root, evidence)
        if f.get("audio") and source in ("program", "bars", "bars2") and not pauses:
            # The tone beeps with every flash of the band (the bottom quarter
            # of the program and ISO pictures; a pause breaks the cadence).
            # Hosted: a flash on a dropped or held slot is accounted loss.
            lost = {round(h["time"] * fps) for h in checked.get("held", [])} if loss_limit(evidence) else None
            evidence.setdefault("sync", []).append(dict(sync_check(file, fps, lost=lost), output=file.name))
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
        # The portal session and PipeWire streams start with the engine
        # (up to 105 s on a slow desktop; the fixture never asks).
        e.rpc("engine", {"running": True}, timeout=150)
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
            # 90%: real runs show 447-450 of 450, and a stream that stalls after a few seconds must fail.
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


def scenario_linux_browser(e, cfg, evidence):
    """A browser source on Linux (the isocast-browser helper next to the
    daemon: WebKitGTK 6 on a private GTK Broadway display) over a green
    color source: a local page draws the frame oracle's block code for its
    current frame and a 50% blue box moving on a transparent background,
    and plays a 660 Hz Web Audio tone, sent to the mixer (control_audio).
    The take's browser ISO and program both show the codes advancing (every
    picture decodes, never backwards, most distinct) and carry the tone; in
    the program, the page's transparent parts show the green beneath and
    the box blends with it; and no file dropped a frame (zero, also under
    --hosted)."""
    if not LINUX:
        raise Skip("Linux only")
    helper = e.binary.parent / "isocast-browser"
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
    evidence["writer_drops"] = {f["source"]: f["dropped_frames"] for f in manifest["files"]}
    lost = {k: v for k, v in evidence["writer_drops"].items() if v}
    require(not lost, f"writer drops (zero required, also under --hosted): {lost}")
    tones = {}
    for f in manifest["files"]:
        tones[f["source"]] = tone_amplitudes(path.parent / f["path"], 0, freqs=(660, 440))
        require(tones[f["source"]][660] > 0.05 and tones[f["source"]][440] < 0.1 * tones[f["source"]][660],
                f"{f['source']}: the page's 660 Hz tone isn't its sound: {tones[f['source']]}")
    evidence["tones"] = {k: {str(f): round(a, 4) for f, a in v.items()} for k, v in tones.items()}

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
        require(len(set(good)) >= len(good) * 2 // 3,
                f"{file.name}: only {len(set(good))} different page frames in {len(good)} pictures")
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


def scenario_win_preview_recording(e, cfg, evidence):
    """Qualify the conservative reference, then protect it from previews.
    Zero losses even under --hosted; paused consumers must survive and resume.
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
        # Retain all counters before strict media validation can fail; never
        # turn an overloaded headless take into a pass.
        file, facts = manifest_file(path, manifest, "program")
        result["files"] = [check_file(file, duration=seconds, fps=profile["fps"],
                                     dimensions=(profile["width"], profile["height"]))]
        require(facts["frames"] == result["files"][0]["frames"], "program manifest/decoded frame mismatch")
        check_loss(result["loss"][0], 0)

    # Retain the larger-profile measurements, but do not select a host's
    # highest single 10-second pass: it can sit on the edge and lose frames
    # during the longer preview take. Gate the same conservative reference
    # named by estimate::SOFTWARE_REFERENCE, with no fallback or loss allowance.
    reference = (320, 180)
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
            case.update(status="PASS", reason="zero loss and strict media checks passed")
            if (width, height) == reference:
                selected = copy.deepcopy(cfg)
                evidence["preview_profile"] = case["profile"].copy()
    require(selected is not None, "320x180@30 reference failed strict zero-loss qualification; see headless_profiles")
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
        held[first] = open(first, "rb")  # shares read and write, not delete
        time.sleep(1.5)
        split = e.rpc("split")
        second = Path(split["manifest"])
        held[second] = open(second, "rb")
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
        with open(path, "rb"):
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
                                 if case.get("status") == "FAIL" and case.get("manifest")}
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
    parser.add_argument("--only", action="append", choices=SCENARIOS)
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
            if args.hosted and not args.capabilities_from:
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
