#!/usr/bin/env python3
"""Bounded, passwordless interactive Windows GUI calls. Stdout is always JSON."""
import argparse
import base64
import gzip
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time
import uuid


class Parser(argparse.ArgumentParser):
    def error(self, message):
        raise ValueError(message)


def arguments():
    parser = Parser(description=__doc__)
    parser.add_argument("--host", required=True, choices=("desk", "vm"))
    parser.add_argument("--force-active", action="store_true",
                        help="desk only: use only when Ryan explicitly authorizes active use")
    parser.add_argument("--json", action="store_true")
    sub = parser.add_subparsers(dest="command", required=True, parser_class=Parser)

    def selector(p):
        group = p.add_mutually_exclusive_group()
        group.add_argument("--window", help="exact window title")
        group.add_argument("--process", help="exact process name, with or without .exe")

    p = sub.add_parser("screenshot")
    selector(p)
    p.add_argument("--out", help="PNG beneath a scratch directory; never inside a checkout")
    p = sub.add_parser("tree")
    selector(p)
    p.add_argument("--depth", type=int, default=6)
    p = sub.add_parser("click")
    selector(p)
    group = p.add_mutually_exclusive_group(required=True)
    group.add_argument("--name")
    group.add_argument("--at", help="X,Y pixels relative to the selected screenshot's origin")
    p.add_argument("--type", help="UIA control type, e.g. Button")
    group = p.add_mutually_exclusive_group()
    group.add_argument("--double", action="store_true")
    group.add_argument("--right", action="store_true")
    p = sub.add_parser("type")
    p.add_argument("text")
    p = sub.add_parser("key")
    p.add_argument("combo")
    p = sub.add_parser("scroll")
    p.add_argument("--amount", type=int, default=-3, help="wheel notches; negative scrolls down")
    p = sub.add_parser("wait-for")
    group = p.add_mutually_exclusive_group(required=True)
    for name in ("text", "window", "process"):
        group.add_argument("--" + name)
    p.add_argument("--timeout", type=int, default=30)
    sub.add_parser("windows")
    p = sub.add_parser("launch")
    p.add_argument("path", help="absolute .exe or .lnk path")
    p.add_argument("--args", default="", help="Windows command-line arguments (one string)")
    p = sub.add_parser("close")
    p.add_argument("--process", required=True)
    args = vars(parser.parse_args())
    if not 1 <= args.get("timeout", 30) <= 300:
        raise ValueError("timeout must be 1..300 seconds")
    if not 0 <= args.get("depth", 6) <= 20:
        raise ValueError("depth must be 0..20")
    if not -100 <= args.get("amount", 0) <= 100:
        raise ValueError("amount must be -100..100 notches")
    if args.get("at"):
        args["at"] = [int(x) for x in args["at"].split(",")]
        if len(args["at"]) != 2:
            raise ValueError("at must be X,Y")
    return args


def main():
    request = arguments()
    here = Path(__file__).resolve().parent
    job = uuid.uuid4().hex
    remote = "C:/isocast-dev/scratch/windows-gui/" + job
    task = "Isocast-GUI-" + job
    budget = request.get("timeout", 30) + 30
    request.update(job=job, folder=remote, task=task, budget=budget,
                   user="Ryan" if request["host"] == "desk" else "packager")
    output = None
    if request["command"] == "screenshot":
        output = Path(request.get("out") or
                      f"/Volumes/IsocastBuild/windows-gui/scratch/{job}.png").expanduser().resolve()
        if "scratch" not in output.parts or output.suffix.lower() != ".png":
            raise ValueError("out must be a .png beneath a scratch directory")
        # Scratch in a checkout is deliberately not accepted, even when ignored.
        if any((parent / ".git").exists() for parent in (output.parent, *output.parents)):
            raise ValueError("screenshots must stay outside Git checkouts")
        if output.exists():
            raise ValueError("out already exists; choose a new screenshot path")
        output.parent.mkdir(parents=True, exist_ok=True)

    env = dict(os.environ, EXON_DESK_HOST="exon-desk-lan")
    if request["host"] == "vm":
        vm = Path(os.environ.get("ISOCAST_WINDOWS_VM_SCRIPT", str(here.parent / "windows-vm.sh")))
        if not vm.is_file():
            raise ValueError("scripts/windows-vm.sh is not merged yet; set ISOCAST_WINDOWS_VM_SCRIPT to the packaging lane's script")
        transport = [str(vm), "ssh"]
        env["ISOCAST_WINDOWS_VM_USER"] = "isocast"
    else:
        transport = ["ssh", "-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10",
                     "-o", "ServerAliveInterval=10", "-o", "ServerAliveCountMax=3", "exon-desk-lan"]

    def run(script, timeout):
        # Keep cmd.exe's command line small; payload goes through one stdin frame.
        # Windows OpenSSH/PowerShell can stall on large text stdin frames before
        # our controller/watchdog even starts. Compress the frame, and bound the
        # raw pipe reads too so a broken transfer cannot strand an SSH worker.
        boot = """
$ErrorActionPreference='Stop'
[Console]::OutputEncoding=[Text.Encoding]::UTF8
try {
 $inputStream=[Console]::OpenStandardInput()
 $frame=[IO.MemoryStream]::new()
 $buffer=New-Object byte[] 32768
 $deadline=[DateTime]::UtcNow.AddSeconds(30)
 do {
  $read=$inputStream.ReadAsync($buffer,0,$buffer.Length)
  $remaining=[int][Math]::Max(1,($deadline-[DateTime]::UtcNow).TotalMilliseconds)
  if (-not $read.Wait($remaining)) { throw 'GUI stdin transfer timed out before controller startup' }
  $count=$read.Result
  if ($count -eq 0) { break }
  $frame.Write($buffer,0,$count)
  if ($frame.Length -gt 2097152) { throw 'GUI transfer exceeds 2 MiB' }
 } while ($buffer[$count-1] -ne 10)
 $compressed=[IO.MemoryStream]::new([Convert]::FromBase64String([Text.Encoding]::ASCII.GetString($frame.ToArray()).Trim()))
 $gzip=[IO.Compression.GZipStream]::new($compressed,[IO.Compression.CompressionMode]::Decompress)
 $reader=[IO.StreamReader]::new($gzip,[Text.Encoding]::UTF8)
 $source=$reader.ReadToEnd()
 $reader.Dispose(); $frame.Dispose()
 & ([scriptblock]::Create($source))
} catch { [Console]::Error.WriteLine($_); exit 1 }
"""
        encoded = base64.b64encode(boot.encode("utf-16le")).decode()
        proc = subprocess.Popen(transport + ["powershell.exe -NoProfile -NonInteractive -EncodedCommand " + encoded],
                                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                env=env, start_new_session=True)
        try:
            out, err = proc.communicate(base64.b64encode(gzip.compress(script.encode())) + b"\n", timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.wait()
            raise RuntimeError(f"Windows transport exceeded {timeout}s") from None
        except BaseException:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.wait()
            raise
        if proc.returncode:
            raise RuntimeError(err.decode("utf-8", errors="replace").strip() or "Windows transport failed")
        return out.decode("utf-8-sig").strip()

    files = {name: base64.b64encode((here / name).read_bytes()).decode()
             for name in ("gui-worker.ps1", "gui-native.cs", "gui-task.ps1", "gui-task.vbs")}
    files["request.json"] = base64.b64encode(json.dumps(request).encode()).decode()
    payload = base64.b64encode(json.dumps(dict(files=files, request=request,
                                              expires=time.time() + budget + 60)).encode()).decode()
    controller = "$payload='" + payload + "'\n" + (here / "gui-controller.ps1").read_text()
    cleanup = f"""
$ErrorActionPreference='Stop'
$t=Get-ScheduledTask -TaskName '{task}' -ErrorAction SilentlyContinue
if ($t) {{
 try {{ Stop-ScheduledTask -InputObject $t }} catch {{}}
 try {{ Unregister-ScheduledTask -InputObject $t -Confirm:$false }} catch {{}}
}}
for ($i=0; $i -lt 10 -and (Test-Path -LiteralPath '{remote}'); $i++) {{
 try {{ Remove-Item -LiteralPath '{remote}' -Recurse -Force }} catch {{ Start-Sleep -Milliseconds 200 }}
}}
if ((Test-Path -LiteralPath '{remote}') -or (Get-ScheduledTask -TaskName '{task}' -ErrorAction SilentlyContinue)) {{ throw 'GUI cleanup incomplete; independent watchdog remains armed' }}
"""
    # A second interrupt must not strand resources during best-effort cleanup.
    def interrupted(signum, frame):
        raise KeyboardInterrupt("interrupted")
    signal.signal(signal.SIGTERM, interrupted)
    result = None
    try:
        result = json.loads(run(controller, budget + 100))
        if "png" in result:
            data = base64.b64decode(result.pop("png"), validate=True)
            with output.open("xb") as stream:
                stream.write(data)
            result["path"] = str(output)
    finally:
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        # The independent interactive supervisor also cleans up after a lost SSH
        # connection. Never turn a cleanup failure into a successful receipt.
        run(cleanup, 45)
    print(json.dumps(result, ensure_ascii=False))
    return 0 if result.get("ok") else 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (Exception, KeyboardInterrupt) as error:
        print(json.dumps({"ok": False, "error": str(error) or "interrupted"}))
        sys.exit(1)
