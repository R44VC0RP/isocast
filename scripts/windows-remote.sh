#!/bin/bash
# Native Windows loop on exon-desk; everything generated lives in its lane root.
#
#   scripts/windows-remote.sh status
#   scripts/windows-remote.sh sync
#   scripts/windows-remote.sh cargo check --locked -p isocast-daemon
#   scripts/windows-remote.sh build [isocastd isocast isocast-ui isocast-capture]
#   scripts/windows-remote.sh run 'Get-ChildItem $env:ISOCAST_SCRATCH'
#   scripts/windows-remote.sh fetch [isocastd isocast isocast-ui isocast-capture]
#
# Sync exports tracked WORKTREE bytes, including uncommitted changes, through
# a temporary Git index and git archive (no commit or real-index mutation).
# New files must be explicitly tracked first (`git add -N path` is enough).
# Remote src is a managed mirror, not a second editing checkout. Targets,
# caches, logs, recordings and other outputs belong outside src.
# Build/fetch copies release .exe files back; cargo/run fetch their logs/results.
# EXON_DESK_HOST: exon-desk-lan (default) or exon-desk.
# ISOCAST_WINDOWS_OUT: local results root (default lane scratch/native).
# ISOCAST_WINDOWS_TIMEOUT: command seconds (default 1800).
# ISOCAST_WINDOWS_ROOT: C:/isocast-dev or a lane below it (default unchanged).
# ISOCAST_WINDOWS_GPU_LOCK: 1 takes the shared GPU lock for a hardware run.
# One timeout budget covers lock acquisition, sync and execution. Lock metadata
# follows the locked first byte; read from byte 1 to inspect a live holder.
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
host="${EXON_DESK_HOST:-exon-desk-lan}"
out="${ISOCAST_WINDOWS_OUT:-/Volumes/IsocastBuild/windows/scratch/native}"
seconds="${ISOCAST_WINDOWS_TIMEOUT:-1800}"
gpu_lock="${ISOCAST_WINDOWS_GPU_LOCK:-0}"
case "$gpu_lock" in 0|1) ;; *) echo "windows-remote: GPU lock must be 0 or 1" >&2; exit 2 ;; esac
# Restricted to this development tree, with shell/PowerShell-safe components.
# Normalize once: every staging, cancellation, fetch and worker path uses it.
root="$(python3 - "${ISOCAST_WINDOWS_ROOT:-C:/isocast-dev}" <<'PY'
import re, sys
path = sys.argv[1].replace('\\', '/').rstrip('/')
if not re.fullmatch(r'C:/isocast-dev(?:/lanes/[A-Za-z0-9_-]+)?', path, re.IGNORECASE):
    sys.exit('windows-remote: root must be C:/isocast-dev or C:/isocast-dev/lanes/NAME')
print(path)
PY
)"
case "$host" in exon-desk|exon-desk-lan) ;; *) echo "windows-remote: unsupported host" >&2; exit 2 ;; esac
case "$seconds" in ''|*[!0-9]*) echo "windows-remote: timeout must be positive seconds" >&2; exit 2 ;; esac
[ "$seconds" -gt 0 ] || exit 2
op="${1:---help}"
case "$op" in
    -h|--help) sed -n 2,18p "$0"; exit 0 ;;
    status|sync) [ $# = 1 ] || exit 2 ;;
    cargo) [ $# -ge 2 ] || { echo "windows-remote: cargo needs arguments" >&2; exit 2; } ;;
    run) [ $# = 2 ] || { echo "windows-remote: run needs one PowerShell script argument" >&2; exit 2; } ;;
    build|fetch) ;;
    *) echo "windows-remote: expected status, sync, cargo, build, run or fetch" >&2; exit 2 ;;
esac
shift
if [ "$op" = build ] || [ "$op" = fetch ]; then
    [ $# -gt 0 ] || set -- isocastd isocast isocast-ui isocast-capture
    for bin in "$@"; do
        case "$bin" in ''|*[!a-zA-Z0-9_-]*) echo "windows-remote: use a binary name without .exe" >&2; exit 2 ;; esac
    done
fi
command -v exon-desk-ps >/dev/null || { echo "windows-remote: exon-desk-ps is required" >&2; exit 2; }
# Every remote transport gets a local wall-clock cap: SSH keepalives notice a
# dead network, not a remote PowerShell that never starts the worker.
tmo="$(command -v timeout || command -v gtimeout || true)"
[ -n "$tmo" ] || { echo "windows-remote: GNU timeout is required (brew install coreutils)" >&2; exit 2; }
mkdir -p "$out"
job="$(python3 -c 'import uuid; print(uuid.uuid4().hex)')"
work="$(mktemp -d "$out/export.XXXXXX")"
result="$out/runs/$job"
mkdir -p "$result"
remote_ps() { EXON_DESK_HOST="$host" "$tmo" --foreground -k 10 120 exon-desk-ps; }
# shellcheck disable=SC2329 # invoked by the INT/TERM traps
cancel() {
    # Only our own child, with the original creation time (never a reused PID).
    remote_ps <<PS || true
\$ErrorActionPreference = 'Stop'
\$path = '$root/scratch/remote/$job/active.json'
if (Test-Path -LiteralPath \$path) {
    \$active = Get-Content -Raw -LiteralPath \$path | ConvertFrom-Json
    \$process = Get-Process -Id \$active.pid -ErrorAction SilentlyContinue
    if (\$process -and \$active.created -and
        \$process.StartTime.ToFileTimeUtc().ToString() -eq \$active.created) {
        & taskkill.exe /PID \$active.pid /T /F
    }
}
PS
}
# shellcheck disable=SC2329 # invoked by the traps and after a failed launch
disarm() {
    # A worker that starts late must refuse before touching src: sync() rejects
    # a stage holding the reserved marker. The stage is never moved or deleted
    # here (a missing stage would sync as empty). CreateNew never recreates a
    # stage the worker already consumed, and result.json means it finished.
    remote_ps <<PS || true
\$ErrorActionPreference = 'Stop'
\$jobdir = '$root/scratch/remote/$job'
\$stage = '$root/staging/$job'
if (Test-Path -LiteralPath "\$jobdir/result.json") { 'disarm: worker already published result.json'; return }
if (Test-Path -LiteralPath "\$jobdir/active.json") { 'disarm: worker child is active; cancelling it' }
if (-not (Test-Path -LiteralPath \$stage -PathType Container)) {
    'disarm: no stage (status/fetch, or the worker already synced it)'; return
}
if ((Get-Item -LiteralPath \$stage).Attributes -band [IO.FileAttributes]::ReparsePoint) {
    throw "Refusing reparse point: \$stage"
}
try {
    [IO.File]::Open("\$stage/.windows-remote.json", 'CreateNew', 'Write').Close()
    'disarm: reserved marker written; a late worker refuses to sync'
} catch [IO.IOException] {
    "disarm: marker not written (worker may be syncing or done): \$(\$_.Exception.Message)"
}
PS
}
trap 'rm -rf "${work:?}"' EXIT
trap 'disarm; cancel; exit 130' INT
trap 'disarm; cancel; exit 143' TERM

head="$(git -C "$here" rev-parse HEAD)"
lane="$(git -C "$here" symbolic-ref --short -q HEAD || basename "$here")"
tree=
if [ "$op" != status ] && [ "$op" != fetch ]; then
    [ -z "$(git -C "$here" diff --name-only --diff-filter=U)" ] || {
        echo "windows-remote: resolve merge conflicts before syncing" >&2; exit 1;
    }
    cp "$(git -C "$here" rev-parse --git-path index)" "$work/index"
    GIT_INDEX_FILE="$work/index" git -C "$here" -c core.splitIndex=false add -u
    tree="$(GIT_INDEX_FILE="$work/index" git -C "$here" write-tree)"
    remote_ps <<PS
\$ErrorActionPreference = 'Stop'
\$paths = @()
\$parent = [IO.DirectoryInfo]'$root/staging'
while (\$parent -and \$parent.FullName -ne \$parent.Root.FullName) {
    \$paths = @(\$parent.FullName) + \$paths
    \$parent = \$parent.Parent
}
foreach (\$path in \$paths) {
    if (Test-Path -LiteralPath \$path) {
        if ((Get-Item -LiteralPath \$path).Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw "Refusing reparse point: \$path"
        }
    } else { New-Item -ItemType Directory -Path \$path | Out-Null }
}
New-Item -ItemType Directory -Path '$root/staging/$job' | Out-Null
PS
    git -C "$here" archive "$tree" | "$tmo" --foreground -k 10 "$seconds" /usr/bin/ssh -T "$host" "tar.exe -xf - -C $root/staging/$job"
fi

request="$(python3 - "$op" "$job" "$tree" "$head" "$seconds" "$root" "$gpu_lock" "$lane" "$@" <<'PY'
import base64, json, sys
op, job, tree, head, seconds, root, gpu_lock, lane, *args = sys.argv[1:]
print(base64.b64encode(json.dumps(dict(op=op, job=job, tree=tree, head=head,
    seconds=int(seconds), root=root, gpu_lock=bool(int(gpu_lock)), lane=lane, args=args)).encode()).decode())
PY
)"

# PowerShell only bootstraps Python. Native stdout/stderr are merged by Python
# before PowerShell sees them, so cargo's progress on stderr isn't an error.
# The request is never persisted or echoed (run scripts can contain private data).
launch_script() {
    # shellcheck disable=SC2016 # PowerShell variable, not a shell expansion
    printf '$env:ISOCAST_REMOTE_REQUEST = "%s"\n' "$request"
    cat <<'PS'
$ErrorActionPreference = 'Stop'
$worker = @'
import base64, codecs, ctypes, ctypes.wintypes, datetime, hashlib, json, msvcrt, os, pathlib
import shutil, stat, subprocess, sys, threading, time, traceback

r = json.loads(base64.b64decode(os.environ.pop('ISOCAST_REMOTE_REQUEST')))
root = pathlib.Path(r['root'])
src = root / 'src'
stage = root / 'staging' / r['job']
job = root / 'scratch' / 'remote' / r['job']
marker = src / '.windows-remote.json'
started = time.time()
deadline = time.monotonic() + r['seconds']
finished = threading.Event()

def hard_timeout():
    # A blocked SSH stdout or sync must not retain a GPU/build lock forever.
    # The ordinary deadline unwinds finally; this emergency watchdog kills
    # our own worker tree after a bounded cleanup grace. Windows then closes
    # its lock handles even when Python cannot unwind a blocked native call.
    if finished.is_set(): return
    try:
        subprocess.run(['taskkill.exe', '/PID', str(os.getpid()), '/T', '/F'],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
    finally:
        os._exit(124)

watchdog = threading.Timer(r['seconds'] + 30, hard_timeout)
watchdog.daemon = True
watchdog.start()

def utc(seconds):
    return datetime.datetime.fromtimestamp(seconds, datetime.timezone.utc).isoformat()

class Lock:
    def __init__(self, path, label):
        # Do not append or truncate before acquiring the byte-range lock:
        # a waiting lane must never replace the current holder's metadata.
        self.file = os.fdopen(os.open(path, os.O_RDWR | os.O_CREAT, 0o600), 'r+b', buffering=0)
        self.held = False
        self.owner = dict(pid=os.getpid(), lane=r['lane'], started_utc=utc(started),
                          deadline_utc=utc(started+r['seconds']), job=r['job'],
                          process_created=creation_time_handle(ctypes.windll.kernel32.GetCurrentProcess()))
        try:
            while True:
                if time.monotonic() >= deadline:
                    raise TimeoutError(f'{label} lock timed out')
                self.file.seek(0)
                try:
                    msvcrt.locking(self.file.fileno(), msvcrt.LK_NBLCK, 1)
                    self.held = True
                    break
                except OSError:
                    time.sleep(min(1, max(0, deadline-time.monotonic())))
            self.file.write(b'0')
            self.metadata('held')
        except BaseException:
            self.close()
            raise

    def metadata(self, state):
        data = dict(self.owner, state=state, updated_utc=utc(time.time()))
        self.file.seek(1)
        self.file.write(('\n'+json.dumps(data, indent=2)+'\n').encode('utf-8'))
        self.file.truncate()
        self.file.flush()

    def close(self):
        if self.file.closed: return
        try:
            if self.held:
                try:
                    self.metadata('released')
                finally:
                    self.file.seek(0)
                    msvcrt.locking(self.file.fileno(), msvcrt.LK_UNLCK, 1)
                    self.held = False
        finally:
            self.file.close()

def safe_dir(path):
    for parent in (path, *path.parents):
        if os.path.lexists(parent) and parent.lstat().st_file_attributes & stat.FILE_ATTRIBUTE_REPARSE_POINT:
            raise RuntimeError(f'refusing reparse point: {parent}')
    path.mkdir(parents=True, exist_ok=True)

def files(folder):
    found = {}
    if not folder.exists():
        return found
    for base, dirs, names in os.walk(folder, followlinks=False):
        for name in dirs + names:
            path = pathlib.Path(base) / name
            info = path.lstat()
            if info.st_file_attributes & stat.FILE_ATTRIBUTE_REPARSE_POINT:
                raise RuntimeError(f'refusing source reparse point: {path}')
            if name in names:
                if info.st_nlink > 1:
                    raise RuntimeError(f'refusing source hard link: {path}')
                found[path.relative_to(folder).as_posix()] = path
    return found

def digest(path):
    with path.open('rb') as f:
        return hashlib.file_digest(f, 'sha256').digest()

def save(path, value):
    temp = path.with_suffix('.partial')
    temp.write_text(json.dumps(value, indent=2) + '\n', encoding='utf-8')
    temp.replace(path)

def sync():
    safe_dir(src)
    actual = files(src)
    previous = json.loads(marker.read_text(encoding='utf-8')) if marker.exists() else {}
    if previous.get('managed') is not True and actual:
        raise RuntimeError('src is not a managed snapshot; refusing to overwrite it')
    old = set(previous.get('files', []))
    for name in old:
        path = pathlib.PureWindowsPath(name)
        if path.anchor or '..' in path.parts:
            raise RuntimeError('invalid path in previous snapshot metadata')
    unknown = set(actual) - old - {marker.name}
    if unknown:
        raise RuntimeError('unmanaged files in src; preserve them in scratch before syncing: ' +
                           ', '.join(sorted(unknown)[:5]))
    incoming = files(stage)
    if marker.name in incoming:
        raise RuntimeError('source archive contains the reserved snapshot marker')
    # A killed sync stays recognizable as our work, but never claims a complete tree.
    save(marker, dict(managed=True, tree=None, files=sorted(old | set(incoming))))
    removed = 0
    for name in sorted(old - set(incoming), reverse=True):
        path = src / name
        if path.exists():
            path.unlink()
            removed += 1
        parent = path.parent
        while parent != src:
            try: parent.rmdir()
            except OSError: break
            parent = parent.parent
    copied = 0
    for name, source in incoming.items():
        target = src / name
        if target.is_file() and source.stat().st_size == target.stat().st_size and digest(source) == digest(target):
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, target)  # changed files get Windows' current mtime
        copied += 1
    save(marker, dict(managed=True, tree=r['tree'], head=r['head'], files=sorted(incoming)))
    print(f'sync: {copied} changed, {removed} removed, {len(incoming)-copied} unchanged; tree {r["tree"]}', flush=True)

def creation_time_handle(handle):
    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    times = [ctypes.wintypes.FILETIME() for _ in range(4)]
    kernel.GetProcessTimes.argtypes = [ctypes.wintypes.HANDLE] + [ctypes.POINTER(ctypes.wintypes.FILETIME)] * 4
    kernel.GetProcessTimes.restype = ctypes.wintypes.BOOL
    if not kernel.GetProcessTimes(handle, *(ctypes.byref(t) for t in times)):
        return None
    return str((times[0].dwHighDateTime << 32) | times[0].dwLowDateTime)

def creation_time(process):
    return creation_time_handle(int(process._handle))

def execute(command, env, input_bytes=None, low=False):
    if time.monotonic() >= deadline:
        return 124
    flags = subprocess.CREATE_NEW_PROCESS_GROUP | subprocess.CREATE_NO_WINDOW
    if low: flags |= subprocess.BELOW_NORMAL_PRIORITY_CLASS
    # File-backed capture cannot be held open by a background grandchild which
    # inherited stdout. Follow the requested process, not EOF on a pipe.
    live_path = job / 'output.log'
    output = live_path.open('wb', buffering=0)
    child = subprocess.Popen(command, cwd=src, env=env, stdin=subprocess.PIPE if input_bytes else subprocess.DEVNULL,
        stdout=output, stderr=subprocess.STDOUT, creationflags=flags)
    active = job / 'active.json'
    save(active, dict(pid=child.pid, created=creation_time(child)))
    def write():
        try: child.stdin.write(input_bytes); child.stdin.close()
        except (OSError, ValueError): pass
    if input_bytes: threading.Thread(target=write, daemon=True).start()
    decoder = codecs.getincrementaldecoder('utf-8')('replace')
    cutoff = None
    try:
        with (job / 'run.log').open('wb') as log, live_path.open('rb', buffering=0) as reader:
            while True:
                ended = child.poll()
                if ended is not None and cutoff is None:
                    cutoff = live_path.stat().st_size
                count = 65536 if cutoff is None else min(65536, cutoff-reader.tell())
                data = reader.read(count)
                if data:
                    log.write(data); log.flush()
                    sys.stdout.buffer.write(decoder.decode(data).encode('utf-8'))
                    sys.stdout.buffer.flush()
                if cutoff is not None and reader.tell() >= cutoff:
                    sys.stdout.buffer.write(decoder.decode(b'', final=True).encode('utf-8'))
                    sys.stdout.buffer.flush()
                    return ended
                if time.monotonic() >= deadline:
                    print(f'command timed out after {r["seconds"]} s', flush=True)
                    return 124
                if not data: time.sleep(0.1)
    finally:
        if child.poll() is None:
            try:
                subprocess.run(['taskkill.exe', '/PID', str(child.pid), '/T', '/F'],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
            finally:
                # Popen holds the original process HANDLE: this fallback cannot
                # target a recycled PID if tree cleanup failed.
                if child.poll() is None: child.kill()
                child.wait(timeout=10)
        active.unlink(missing_ok=True)
        output.close()
        try: live_path.unlink()
        except PermissionError:
            print('note: a background child retained output.log; the captured run.log is complete', flush=True)

code = 1
lock = None
build_lock = None
gpu_lock = None
try:
    for path in [root, root/'staging', root/'scratch', root/'scratch'/'remote', root/'target',
                 root/'cargo', root/'tmp', root/'home', root/'appdata', root/'localappdata']:
        safe_dir(path)
    safe_dir(job)
    if r['op'] == 'status':
        source = json.loads(marker.read_text(encoding='utf-8')) if marker.exists() else {}
        print(json.dumps(dict(root=str(root), free_gib=round(shutil.disk_usage(root).free/2**30,1),
            source={k:source.get(k) for k in ('head','tree')}, tracked_files=len(source.get('files',[])))))
        code = 0
    else:
        # Same order as the measurement harness: global build, lane, GPU.
        # Taking the lane first deadlocks with a measurement holding global
        # while waiting for that lane, and permits foreign-lane compilation.
        build_path = pathlib.Path('C:/isocast-dev/build.lock')
        build_lock = Lock(build_path, 'global native build')
        if root/'build.lock' != build_path:
            lock = Lock(root/'build.lock', 'native lane')
        if shutil.disk_usage(root).free < 20 * 2**30:
            raise RuntimeError('less than 20 GiB free on the Windows build drive')
        if r['gpu_lock']:
            gpu_lock = Lock(pathlib.Path('C:/isocast-dev/gpu.lock'), 'shared GPU')
        if r['op'] != 'fetch':
            sync()
            shutil.rmtree(stage)
        env = os.environ.copy()
        # Keep the installed toolchain while isolating all newly generated caches.
        rustup = shutil.which('rustup')
        if rustup and not env.get('RUSTUP_HOME'):
            env['RUSTUP_HOME'] = subprocess.check_output([rustup, 'show', 'home'], text=True).strip()
        git_config = root/'home'/'.gitconfig'
        if not git_config.exists(): git_config.touch()
        env.update(CARGO_HOME=str(root/'cargo'), CARGO_TARGET_DIR=str(root/'target'), CARGO_TERM_COLOR='never',
            TEMP=str(root/'tmp'), TMP=str(root/'tmp'), HOME=str(root/'home'), USERPROFILE=str(root/'home'),
            APPDATA=str(root/'appdata'), LOCALAPPDATA=str(root/'localappdata'), PYTHONDONTWRITEBYTECODE='1',
            PYTHONNOUSERSITE='1', GIT_CONFIG_GLOBAL=str(git_config), GIT_CONFIG_NOSYSTEM='1',
            PSModuleAnalysisCachePath=str(root/'scratch'/'ps-module-cache'),
            ISOCAST_BUILD_COMMIT=r['head']+'-worktree', ISOCAST_SCRATCH=str(root/'scratch'),
            ISOCAST_SOCKET=str(root/'scratch'/'isocastd.sock'), ISOCAST_SETUP=str(root/'scratch'/'setup.toml'),
            ISOCAST_TIMELINES=str(root/'scratch'/'timelines'), ISOCAST_PLUGINS=str(root/'scratch'/'plugins'))
        code = 0
        if r['op'] in ('cargo', 'build'):
            cargo = shutil.which('cargo') or 'cargo.exe'
            args = r['args']
            if r['op'] == 'build':
                # --bin alone still selects all default workspace packages and
                # unifies the UI's client features into a daemon/CLI-only build.
                metadata = subprocess.run([cargo, 'metadata', '--locked', '--no-deps', '--format-version=1'],
                    cwd=src, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                if metadata.returncode:
                    raise RuntimeError(metadata.stderr.strip())
                owners = {}
                for package in json.loads(metadata.stdout)['packages']:
                    for target in package['targets']:
                        if 'bin' in target['kind']:
                            owners.setdefault(target['name'], []).append(package['name'])
                packages = set()
                for name in r['args']:
                    if len(owners.get(name, [])) != 1:
                        raise RuntimeError('binary needs one workspace owner: ' + name)
                    packages.add(owners[name][0])
                args = ['build', '--release', '--locked'] + [x for p in sorted(packages) for x in ('-p', p)]
                args += [x for b in r['args'] for x in ('--bin', b)]
            code = execute([cargo, *args], env, low=True)
        elif r['op'] == 'run':
            # The same stdin/base64 strategy as exon-desk-ps: no cmd.exe quoting,
            # no command-line length limit and no persisted PowerShell source.
            boot = '''$ProgressPreference='SilentlyContinue'
[Console]::OutputEncoding=[Text.Encoding]::UTF8
$s=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String([Console]::In.ReadToEnd().Trim()))
$global:failed=$false
try { & ([scriptblock]::Create($s)) *>&1 | ForEach-Object {
 if ($_ -is [Management.Automation.ErrorRecord]) { $global:failed=$true; [Console]::Error.WriteLine("ERROR: $_") }
 else { $_ }
} | Out-String -Stream -Width 300 } catch { [Console]::Error.WriteLine("ERROR: $_"); exit 1 }
if ($global:failed) { exit 1 }; if ($LASTEXITCODE) { exit $LASTEXITCODE }; exit 0'''
            encoded = base64.b64encode(boot.encode('utf-16le')).decode()
            code = execute(['powershell.exe','-NoProfile','-NonInteractive','-EncodedCommand',encoded], env,
                           base64.b64encode(r['args'][0].encode('utf-8')))
        if r['op'] in ('build','fetch') and code == 0:
            safe_dir(job/'bin')
            for name in r['args']:
                source = root/'target'/'release'/(name+'.exe')
                if not source.is_file() or source.lstat().st_file_attributes & stat.FILE_ATTRIBUTE_REPARSE_POINT:
                    raise RuntimeError(f'missing or linked release binary: {name}')
                shutil.copyfile(source, job/'bin'/source.name)
            save(job/'binaries.json', {p.name:digest(p).hex() for p in (job/'bin').iterdir()})
except Exception as error:
    print(f'windows-remote: {error}', flush=True)
    traceback.print_exc(file=sys.stdout)
    code = 1
finally:
    # Release every lock even if result publication or another close fails.
    # Keep the watchdog armed through cleanup, including blocked SSH output.
    try:
        try:
            if gpu_lock is not None: gpu_lock.close()
        finally:
            try:
                if lock is not None: lock.close()
            finally:
                if build_lock is not None: build_lock.close()
        if job.is_dir():
            save(job/'result.json', dict(operation=r['op'], head=r['head'], tree=r['tree'],
                exit_code=code, seconds=round(time.time()-started,2)))
        if stage.exists() and not stage.is_symlink(): shutil.rmtree(stage)
    finally:
        finished.set()
        watchdog.cancel()
sys.exit(code if 0 <= code <= 255 else 1)
'@
$worker | & python -I -X utf8 -
exit $LASTEXITCODE
PS
}
# One newline-terminated base64 frame, read with ReadLine: unlike the
# exon-desk-ps ReadToEnd bootstrap, launch never waits for stdin EOF. The fixed
# shim otherwise matches exon-desk-ps (UTF-8, error records, exit status).
frame_max=5592408 # base64 length of 4 MiB
frame="$(launch_script | base64 | tr -d '\n')"
[ "${#frame}" -le "$frame_max" ] || { echo "windows-remote: launch frame exceeds 4 MiB" >&2; exit 2; }
boot="$(cat <<PS
\$ProgressPreference = "SilentlyContinue"
[Console]::OutputEncoding = [Text.Encoding]::UTF8
\$global:failed = \$false
try {
  \$line = [Console]::In.ReadLine()
  if (\$null -eq \$line -or \$line.Length -gt $frame_max -or \$line -cnotmatch '\A[A-Za-z0-9+/]+={0,2}\z') {
    throw 'windows-remote: malformed launch frame'
  }
  \$s = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String(\$line))
  & ([scriptblock]::Create(\$s)) *>&1 | ForEach-Object {
    if (\$_ -is [Management.Automation.ErrorRecord]) { \$global:failed = \$true; [Console]::Error.WriteLine("ERROR: \$_") }
    elseif (\$_ -is [Management.Automation.WarningRecord]) { [Console]::Error.WriteLine("WARNING: \$(\$_.Message)") }
    else { \$_ }
  } | Out-String -Stream -Width 300
} catch { [Console]::Error.WriteLine("ERROR: \$_"); exit 1 }
if (\$global:failed) { exit 1 }
if (\$LASTEXITCODE) { exit \$LASTEXITCODE }
exit 0
PS
)"
enc="$(printf '%s' "$boot" | iconv -f UTF-8 -t UTF-16LE | base64 | tr -d '\n')"
# The worker's own watchdog fires at seconds+30; this cap is the backstop for
# a launch that never reaches it.
status=0
printf '%s\n' "$frame" |
    "$tmo" --foreground -k 15 "$((seconds + 60))" /usr/bin/ssh -T -o ServerAliveInterval=15 -o ServerAliveCountMax=3 \
        "$host" "powershell -NoProfile -NonInteractive -EncodedCommand $enc" 2>&1 |
    tee "$result/console.log" || status=$?
unset frame
case "$status" in
    124|137|255)
        # Timed out or lost transport: disarm a not-yet-started worker, then
        # kill only our own verified child. Both are no-ops after result.json.
        echo "windows-remote: launch ended with $status (transport timeout/loss, or worker timeout); disarming job $job" >&2
        disarm
        cancel
        ;;
esac

# Raw tar over SSH, never through PowerShell's text pipeline. Even failed
# cargo commands keep result.json and run.log for diagnosis.
fetch_status=0
"$tmo" --foreground -k 10 "$seconds" /usr/bin/ssh -T "$host" "tar.exe -cf - --exclude=output.log --exclude=./output.log -C $root/scratch/remote/$job ." |
    tar -xf - -C "$result" || fetch_status=$?
if [ "$status" = 0 ] && [ "$fetch_status" != 0 ]; then status=$fetch_status; fi
if [ -d "$result/bin" ]; then
    mkdir -p "$out/bin"
    python3 - "$result" "$out/bin" <<'PY'
import hashlib, json, pathlib, shutil, sys
result, out = map(pathlib.Path, sys.argv[1:])
for name, expected in json.loads((result/'binaries.json').read_text()).items():
    source = result/'bin'/name
    with source.open('rb') as f:
        if hashlib.file_digest(f, 'sha256').hexdigest() != expected:
            sys.exit(f'windows-remote: fetched binary hash mismatch: {name}')
    temp = out/(name+'.partial')
    shutil.copyfile(source, temp)
    temp.replace(out/name)
PY
    # Keep only one local copy and no remote per-job binary duplicate.
    rm -rf "${result:?}/bin"
    remote_ps <<PS
\$path = '$root/scratch/remote/$job/bin'
if (Test-Path -LiteralPath \$path) { Remove-Item -LiteralPath \$path -Recurse }
PS
fi
echo "windows-remote: exit $status; evidence $result"
exit "$status"
