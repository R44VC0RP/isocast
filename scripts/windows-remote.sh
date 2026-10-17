#!/bin/bash
# Native Windows loop on exon-desk; everything generated lives in C:\isocast-dev.
#
#   scripts/windows-remote.sh status
#   scripts/windows-remote.sh sync
#   scripts/windows-remote.sh cargo check --locked -p isocast-daemon
#   scripts/windows-remote.sh build [isocastd isocast isocast-ui]
#   scripts/windows-remote.sh run 'Get-ChildItem $env:ISOCAST_SCRATCH'
#   scripts/windows-remote.sh fetch [isocastd isocast isocast-ui]
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
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
host="${EXON_DESK_HOST:-exon-desk-lan}"
out="${ISOCAST_WINDOWS_OUT:-/Volumes/IsocastBuild/windows/scratch/native}"
seconds="${ISOCAST_WINDOWS_TIMEOUT:-1800}"
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
    [ $# -gt 0 ] || set -- isocastd isocast isocast-ui
    for bin in "$@"; do
        case "$bin" in ''|*[!a-zA-Z0-9_-]*) echo "windows-remote: use a binary name without .exe" >&2; exit 2 ;; esac
    done
fi
command -v exon-desk-ps >/dev/null || { echo "windows-remote: exon-desk-ps is required" >&2; exit 2; }
mkdir -p "$out"
job="$(python3 -c 'import uuid; print(uuid.uuid4().hex)')"
work="$(mktemp -d "$out/export.XXXXXX")"
result="$out/runs/$job"
mkdir -p "$result"
remote_ps() { EXON_DESK_HOST="$host" exon-desk-ps; }
# shellcheck disable=SC2329 # invoked by the INT/TERM traps
cancel() {
    # Only our own child, with the original creation time (never a reused PID).
    remote_ps <<PS || true
\$ErrorActionPreference = 'Stop'
\$path = 'C:/isocast-dev/scratch/remote/$job/active.json'
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
trap 'rm -rf "${work:?}"' EXIT
trap 'cancel; exit 130' INT
trap 'cancel; exit 143' TERM

head="$(git -C "$here" rev-parse HEAD)"
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
foreach (\$path in @('C:\isocast-dev', 'C:\isocast-dev\staging')) {
    if (Test-Path -LiteralPath \$path) {
        if ((Get-Item -LiteralPath \$path).Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw "Refusing reparse point: \$path"
        }
    } else { New-Item -ItemType Directory -Path \$path | Out-Null }
}
New-Item -ItemType Directory -Path 'C:/isocast-dev/staging/$job' | Out-Null
PS
    git -C "$here" archive "$tree" | /usr/bin/ssh -T "$host" "tar.exe -xf - -C C:/isocast-dev/staging/$job"
fi

request="$(python3 - "$op" "$job" "$tree" "$head" "$seconds" "$@" <<'PY'
import base64, json, sys
op, job, tree, head, seconds, *args = sys.argv[1:]
print(base64.b64encode(json.dumps(dict(op=op, job=job, tree=tree, head=head,
    seconds=int(seconds), args=args)).encode()).decode())
PY
)"

# PowerShell only bootstraps Python. Native stdout/stderr are merged by Python
# before PowerShell sees them, so cargo's progress on stderr isn't an error.
# The request is never persisted or echoed (run scripts can contain private data).
status=0
{
    # shellcheck disable=SC2016 # PowerShell variable, not a shell expansion
    printf '$env:ISOCAST_REMOTE_REQUEST = "%s"\n' "$request"
    cat <<'PS'
$ErrorActionPreference = 'Stop'
$worker = @'
import base64, codecs, ctypes, ctypes.wintypes, hashlib, json, msvcrt, os, pathlib
import shutil, stat, subprocess, sys, threading, time, traceback

r = json.loads(base64.b64decode(os.environ.pop('ISOCAST_REMOTE_REQUEST')))
root = pathlib.Path(r'C:\isocast-dev')
src = root / 'src'
stage = root / 'staging' / r['job']
job = root / 'scratch' / 'remote' / r['job']
marker = src / '.windows-remote.json'

def safe_dir(path):
    if os.path.lexists(path) and path.lstat().st_file_attributes & stat.FILE_ATTRIBUTE_REPARSE_POINT:
        raise RuntimeError(f'refusing reparse point: {path}')
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

def creation_time(process):
    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    times = [ctypes.wintypes.FILETIME() for _ in range(4)]
    kernel.GetProcessTimes.argtypes = [ctypes.wintypes.HANDLE] + [ctypes.POINTER(ctypes.wintypes.FILETIME)] * 4
    kernel.GetProcessTimes.restype = ctypes.wintypes.BOOL
    if not kernel.GetProcessTimes(int(process._handle), *(ctypes.byref(t) for t in times)):
        return None
    return str((times[0].dwHighDateTime << 32) | times[0].dwLowDateTime)

def execute(command, env, input_bytes=None, low=False):
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
    deadline = time.monotonic() + r['seconds']
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
            subprocess.run(['taskkill.exe', '/PID', str(child.pid), '/T', '/F'],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)
            child.wait(timeout=30)
        active.unlink(missing_ok=True)
        output.close()
        try: live_path.unlink()
        except PermissionError:
            print('note: a background child retained output.log; the captured run.log is complete', flush=True)

code = 1
started = time.time()
lock = None
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
        lock = (root/'build.lock').open('a+b')
        if lock.tell() == 0: lock.write(b'0'); lock.flush()
        deadline = time.monotonic() + r['seconds']
        while True:
            lock.seek(0)
            try: msvcrt.locking(lock.fileno(), msvcrt.LK_NBLCK, 1); break
            except OSError:
                if time.monotonic() >= deadline: raise TimeoutError('native build lock timed out')
                time.sleep(1)
        if shutil.disk_usage(root).free < 20 * 2**30:
            raise RuntimeError('less than 20 GiB free on the Windows build drive')
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
    if job.is_dir():
        save(job/'result.json', dict(operation=r['op'], head=r['head'], tree=r['tree'],
            exit_code=code, seconds=round(time.time()-started,2)))
    if lock is not None: lock.close()
    if stage.exists() and not stage.is_symlink(): shutil.rmtree(stage)
sys.exit(code if 0 <= code <= 255 else 1)
'@
$worker | & python -I -X utf8 -
exit $LASTEXITCODE
PS
} | remote_ps 2>&1 | tee "$result/console.log" || status=$?

# Raw tar over SSH, never through PowerShell's text pipeline. Even failed
# cargo commands keep result.json and run.log for diagnosis.
fetch_status=0
/usr/bin/ssh -T "$host" "tar.exe -cf - --exclude=output.log --exclude=./output.log -C C:/isocast-dev/scratch/remote/$job ." |
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
\$path = 'C:/isocast-dev/scratch/remote/$job/bin'
if (Test-Path -LiteralPath \$path) { Remove-Item -LiteralPath \$path -Recurse }
PS
fi
echo "windows-remote: exit $status; evidence $result"
exit "$status"
