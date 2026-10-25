"""Temporary ASK-36 hosted diagnostic; not a production launch fallback."""

import argparse
import ctypes as C
from ctypes import wintypes as W
import json
import os
from pathlib import Path
import subprocess
import sys
import traceback


CREATE_BREAKAWAY_FROM_JOB = 0x01000000
PRODUCTION_FLAGS = 0x08000000 | 0x00000200 | CREATE_BREAKAWAY_FROM_JOB | 0x00000400 | 0x00080000
VISIBILITY = (
    "QueryInformationJobObject(NULL) reports the calling process's immediate job, "
    "not all ancestors of a nested job hierarchy. IsProcessInJob(NULL) tests any job; "
    "these results alone cannot establish every ancestor's breakaway policy."
)


class BASIC_LIMITS(C.Structure):
    _fields_ = [
        ("PerProcessUserTimeLimit", C.c_int64), ("PerJobUserTimeLimit", C.c_int64),
        ("LimitFlags", W.DWORD), ("MinimumWorkingSetSize", C.c_size_t),
        ("MaximumWorkingSetSize", C.c_size_t), ("ActiveProcessLimit", W.DWORD),
        ("Affinity", C.c_size_t), ("PriorityClass", W.DWORD),
        ("SchedulingClass", W.DWORD),
    ]


class IO_COUNTERS(C.Structure):
    _fields_ = [(name, C.c_uint64) for name in (
        "ReadOperationCount", "WriteOperationCount", "OtherOperationCount",
        "ReadTransferCount", "WriteTransferCount", "OtherTransferCount",
    )]


class EXTENDED_LIMITS(C.Structure):
    _fields_ = [
        ("BasicLimitInformation", BASIC_LIMITS), ("IoInfo", IO_COUNTERS),
        ("ProcessMemoryLimit", C.c_size_t), ("JobMemoryLimit", C.c_size_t),
        ("PeakProcessMemoryUsed", C.c_size_t), ("PeakJobMemoryUsed", C.c_size_t),
    ]


class STARTUPINFOW(C.Structure):
    _fields_ = [
        ("cb", W.DWORD), ("lpReserved", W.LPWSTR), ("lpDesktop", W.LPWSTR),
        ("lpTitle", W.LPWSTR), ("dwX", W.DWORD), ("dwY", W.DWORD),
        ("dwXSize", W.DWORD), ("dwYSize", W.DWORD),
        ("dwXCountChars", W.DWORD), ("dwYCountChars", W.DWORD),
        ("dwFillAttribute", W.DWORD), ("dwFlags", W.DWORD),
        ("wShowWindow", W.WORD), ("cbReserved2", W.WORD),
        ("lpReserved2", C.POINTER(W.BYTE)), ("hStdInput", W.HANDLE),
        ("hStdOutput", W.HANDLE), ("hStdError", W.HANDLE),
    ]


class STARTUPINFOEXW(C.Structure):
    _fields_ = [("StartupInfo", STARTUPINFOW), ("lpAttributeList", C.c_void_p)]


class PROCESS_INFORMATION(C.Structure):
    _fields_ = [
        ("hProcess", W.HANDLE), ("hThread", W.HANDLE),
        ("dwProcessId", W.DWORD), ("dwThreadId", W.DWORD),
    ]


def api():
    kernel = C.WinDLL("kernel32", use_last_error=True)
    signatures = {
        "GetCurrentProcess": (W.HANDLE, []),
        "OpenProcess": (W.HANDLE, [W.DWORD, W.BOOL, W.DWORD]),
        "CloseHandle": (W.BOOL, [W.HANDLE]),
        "IsProcessInJob": (W.BOOL, [W.HANDLE, W.HANDLE, C.POINTER(W.BOOL)]),
        "QueryInformationJobObject": (W.BOOL, [W.HANDLE, C.c_int, C.c_void_p, W.DWORD, C.POINTER(W.DWORD)]),
        "DuplicateHandle": (W.BOOL, [W.HANDLE, W.HANDLE, W.HANDLE, C.POINTER(W.HANDLE), W.DWORD, W.BOOL, W.DWORD]),
        "InitializeProcThreadAttributeList": (W.BOOL, [C.c_void_p, W.DWORD, W.DWORD, C.POINTER(C.c_size_t)]),
        "UpdateProcThreadAttribute": (W.BOOL, [C.c_void_p, C.c_size_t, C.c_size_t, C.c_void_p, C.c_size_t, C.c_void_p, C.c_void_p]),
        "DeleteProcThreadAttributeList": (None, [C.c_void_p]),
        "GetEnvironmentStringsW": (C.c_void_p, []),
        "FreeEnvironmentStringsW": (W.BOOL, [C.c_void_p]),
        "CreateProcessW": (W.BOOL, [W.LPCWSTR, W.LPWSTR, C.c_void_p, C.c_void_p, W.BOOL, W.DWORD, C.c_void_p, W.LPCWSTR, C.POINTER(STARTUPINFOW), C.POINTER(PROCESS_INFORMATION)]),
        "WaitForSingleObject": (W.DWORD, [W.HANDLE, W.DWORD]),
        "GetExitCodeProcess": (W.BOOL, [W.HANDLE, C.POINTER(W.DWORD)]),
        "TerminateProcess": (W.BOOL, [W.HANDLE, W.UINT]),
    }
    for name, (result, arguments) in signatures.items():
        function = getattr(kernel, name)
        function.restype = result
        function.argtypes = arguments
    return kernel


def membership(kernel, handle):
    value = W.BOOL()
    C.set_last_error(0)
    ok = kernel.IsProcessInJob(handle, None, C.byref(value))
    error = C.get_last_error()
    return {"ok": bool(ok), "last_error": error, "in_job": bool(value.value) if ok else None}


def fields(value):
    return {name: fields(item) if isinstance(item := getattr(value, name), C.Structure) else item
            for name, _ in value._fields_}


def snapshot(kernel):
    limits = EXTENDED_LIMITS()
    returned = W.DWORD()
    C.set_last_error(0)
    ok = kernel.QueryInformationJobObject(None, 9, C.byref(limits), C.sizeof(limits), C.byref(returned))
    error = C.get_last_error()
    flags = limits.BasicLimitInformation.LimitFlags if ok else None
    return {
        "pid": os.getpid(), "parent_pid": os.getppid(),
        "membership": membership(kernel, kernel.GetCurrentProcess()),
        "current_job_extended_limits": {
            "ok": bool(ok), "last_error": error, "returned_bytes": returned.value,
            "buffer_bytes": C.sizeof(limits), "raw": fields(limits) if ok else None,
            "limit_flags_hex": f"0x{flags:08x}" if ok else None,
            "breakaway_ok": bool(flags & 0x800) if ok else None,
            "silent_breakaway_ok": bool(flags & 0x1000) if ok else None,
            "kill_on_job_close": bool(flags & 0x2000) if ok else None,
        },
        "visibility_limit": VISIBILITY,
    }


def require(ok):
    if not ok:
        raise C.WinError(C.get_last_error())


def launch(kernel, output, control, result):
    import msvcrt

    flags = PRODUCTION_FLAGS & ~CREATE_BREAKAWAY_FROM_JOB if control else PRODUCTION_FLAGS
    result.update({
        "kind": "diagnostic_only_no_breakaway_control_NEVER_production_fallback" if control else "production_flags",
        "creation_flags": flags, "creation_flags_hex": f"0x{flags:08x}",
        "handle_allowlist": ["stdin_pipe_reader", "append_log_stdout_and_stderr"],
        "environment": "explicit unchanged Unicode environment block (values not logged)",
        "log": str(output),
    })
    handles = []
    attributes = None
    environment = None
    process = PROCESS_INFORMATION()
    read_fd, write_fd = os.pipe()
    # The short child sees EOF; the private writer is never inherited.
    os.close(write_fd)
    try:
        with os.fdopen(read_fd, "rb") as input_file, output.open("ab") as log:
            current = kernel.GetCurrentProcess()
            for file in (input_file, log):
                duplicate = W.HANDLE()
                require(kernel.DuplicateHandle(current, msvcrt.get_osfhandle(file.fileno()), current,
                                               C.byref(duplicate), 0, True, 2))
                handles.append(duplicate.value)
            allowlist = (W.HANDLE * len(handles))(*handles)
            size = C.c_size_t()
            C.set_last_error(0)
            sized = kernel.InitializeProcThreadAttributeList(None, 1, 0, C.byref(size))
            sizing_error = C.get_last_error()
            if sized or sizing_error != 122 or not size.value:
                raise RuntimeError(f"Attribute sizing failed: result={sized}, error={sizing_error}, bytes={size.value}")
            # ctypes allocations have native alignment, including on Windows x64.
            storage = (C.c_size_t * ((size.value + C.sizeof(C.c_size_t) - 1) // C.sizeof(C.c_size_t)))()
            require(kernel.InitializeProcThreadAttributeList(storage, 1, 0, C.byref(size)))
            attributes = C.cast(storage, C.c_void_p)
            require(kernel.UpdateProcThreadAttribute(attributes, 0, 0x00020002, allowlist,
                                                    C.sizeof(allowlist), None, None))
            startup = STARTUPINFOEXW()
            startup.StartupInfo.cb = C.sizeof(startup)
            startup.StartupInfo.dwFlags = 0x100
            startup.StartupInfo.hStdInput = handles[0]
            startup.StartupInfo.hStdOutput = handles[1]
            startup.StartupInfo.hStdError = handles[1]
            startup.lpAttributeList = attributes
            environment = kernel.GetEnvironmentStringsW()
            require(environment)
            command = C.create_unicode_buffer(subprocess.list2cmdline([sys.executable, str(Path(__file__).resolve()), "--child"]))
            C.set_last_error(0)
            ok = kernel.CreateProcessW(sys.executable, command, None, None, True, flags, environment,
                                       None, C.byref(startup.StartupInfo), C.byref(process))
            error = C.get_last_error()  # Capture immediately, before ANY other Win32 call.
            result.update(create_process_result=int(ok), last_error=error,
                          last_error_meaning="failure code" if not ok else "undefined on success; captured after reset to zero")
            if ok:
                result["child_pid"] = process.dwProcessId
                C.set_last_error(0)
                wait = kernel.WaitForSingleObject(process.hProcess, 15000)
                wait_error = C.get_last_error()
                result.update(wait_result=wait, wait_last_error=wait_error)
                if wait != 0:
                    # Only the exact process we created is terminated, never a PID lookup.
                    require(kernel.TerminateProcess(process.hProcess, 124))
                    require(kernel.WaitForSingleObject(process.hProcess, 5000) == 0)
                    result["terminated_after_wait_failure_or_timeout"] = True
                code = W.DWORD()
                require(kernel.GetExitCodeProcess(process.hProcess, C.byref(code)))
                result["exit_code"] = code.value
    finally:
        if process.hThread:
            kernel.CloseHandle(process.hThread)
        if process.hProcess:
            kernel.CloseHandle(process.hProcess)
        if environment:
            kernel.FreeEnvironmentStringsW(environment)
        if attributes:
            kernel.DeleteProcThreadAttributeList(attributes)
        for handle in handles:
            kernel.CloseHandle(handle)
    if result.get("create_process_result"):
        result["child_observation"] = json.loads(output.read_text(encoding="utf-8"))
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--child", action="store_true")
    parser.add_argument("--context", choices=("actions-parent", "scheduled-worker"))
    parser.add_argument("--parent-pid", type=int)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--control", action="store_true")
    args = parser.parse_args()
    if sys.platform != "win32":
        parser.error("Windows only; static validation does not run the probe")
    if os.environ.get("RUNNER_ENVIRONMENT") != "github-hosted":
        parser.error("Temporary ASK-36 diagnostic is restricted to GitHub-hosted runners")
    kernel = api()
    if args.child:
        observation = snapshot(kernel)
        observation["stdin_eof"] = sys.stdin.buffer.read(1) == b""
        print(json.dumps(observation), flush=True)
        return 0
    if not args.context or not args.output or not args.parent_pid:
        parser.error("--context, --output and --parent-pid are required")
    if args.control and args.context != "actions-parent":
        parser.error("Only one no-breakaway control, in the Actions parent context, is permitted")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    report = {"context": args.context, "complete": False, "attempts": []}
    try:
        report["observation"] = snapshot(kernel)
        C.set_last_error(0)
        parent = kernel.OpenProcess(0x1000, False, args.parent_pid)
        error = C.get_last_error()
        report["launching_powershell"] = {"pid": args.parent_pid, "open_process_ok": bool(parent), "last_error": error}
        if parent:
            try:
                report["launching_powershell"]["membership"] = membership(kernel, parent)
            finally:
                kernel.CloseHandle(parent)
        if not parent:
            raise C.WinError(error)
        for control in ([False, True] if args.control else [False]):
            suffix = "control" if control else "production"
            attempt = {}
            report["attempts"].append(attempt)
            launch(kernel, args.output.with_suffix(f".{suffix}.log"), control, attempt)
        report["complete"] = True  # A denied CreateProcessW is a measured outcome, not a failed probe.
        return 0
    except Exception:
        report["error"] = traceback.format_exc()
        return 1
    finally:
        args.output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        print(json.dumps(report, indent=2), flush=True)


if __name__ == "__main__":
    sys.exit(main())
