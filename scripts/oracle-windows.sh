#!/bin/sh
# The frame oracle on Windows (#124), on exon-desk.
#
#   scripts/oracle-windows.sh [--contracts BIN.exe]
#
# Each invocation stages and works in its own folder,
# C:\isocast-dev\scratch\oracle\run-<time>-<pid> (the Windows lanes' rules: our
# own folder, encoder work under C:\isocast-dev\gpu.lock), and copies the
# results to $ISOCAST_ORACLE_OUT (default
# /Volumes/IsocastBuild/oracle-harness/scratch/windows-<time>). Stages:
#
# 1. selftest: the oracle's fixture and decoder through Windows' ffmpeg and
#    every encoder it has (NVENC H.264/HEVC included): scaled, in a corner,
#    cropped, letterboxed, masked; a copy one frame late must fail.
# 2. contracts: the Windows lanes' isolated harness as it is today, the
#    Platform contract worker (scripts/platform-contracts.py --suite media
#    --hardware), recording takes with the native writer and encoders.
# 3. pattern: every one of its takes (at least the normal, pressure and
#    recovery ones) against the test pattern's own frame model
#    (e2e.py --pattern-oracle), independently of its receipts.
# 4. engine scenarios (e2e.py --quick / --full): PENDING until isocastd
#    records on Windows (the daemon switch, W.8) and e2e.py's engine client
#    speaks named pipes. Reported as pending, never as a pass.
#
# Exit 0: stages 1-3 passed. 1: one failed. 2: one couldn't run (no contracts
# binary, the GPU lock stayed busy, the PC unreachable): not verified.
# BIN.exe defaults to C:\isocast-dev\target\release\isocast-platform-contracts.exe
# (built by the Windows lane; we never build or edit Windows platform code).
set -u
cd "$(dirname "$0")/.."
# The verdict of a run's log: 0 every required stage passed, 1 one failed,
# 2 one couldn't run (`--status-of LOG` prints it for a saved log).
verdict() {
  status=0
  for stage in selftest contracts pattern; do
    line="$(grep "^STAGE $stage " "$1" | tail -1)"
    case "$line" in
    *" PASS"*) ;;
    *" FAIL"*) status=1 ;;
    *) [ "$status" = 0 ] && status=2; [ -z "$line" ] && echo "oracle-windows: stage $stage never reported" ;;
    esac
  done
  grep -q "^STAGE lock UNAVAILABLE" "$1" && [ "$status" = 0 ] && status=2
  case $status in
  0) echo "oracle-windows: PASS (selftest, contracts, pattern; engine scenarios pending W.8)" ;;
  1) echo "oracle-windows: FAIL" ;;
  *) echo "oracle-windows: NOT VERIFIED (a stage couldn't run)" ;;
  esac
  return $status
}
if [ "${1:-}" = --status-of ]; then
  verdict "$2"
  exit $?
fi

host="${EXON_DESK_HOST:-exon-desk-lan}"
stamp="$(date +%Y%m%d-%H%M%S)-$$"
out="${ISOCAST_ORACLE_OUT:-/Volumes/IsocastBuild/oracle-harness/scratch/windows-$stamp}"
contracts='C:\isocast-dev\target\release\isocast-platform-contracts.exe'
[ "${1:-}" = --contracts ] && contracts="$2"
remote="C:\\isocast-dev\\scratch\\oracle\\run-$stamp"
remote_scp="C:/isocast-dev/scratch/oracle/run-$stamp"
mkdir -p "$out"
ps() { EXON_DESK_HOST="$host" exon-desk-ps "$@"; }

if ! ps "New-Item -ItemType Directory -Force -Path '$remote' | Out-Null" >"$out/stage.log" 2>&1; then
  echo "oracle-windows: can't reach $host (UNAVAILABLE)"; cat "$out/stage.log"; exit 2
fi
for f in e2e.py platform-contracts.py; do
  scp -q "scripts/$f" "$host:$remote_scp/$f" || { echo "oracle-windows: copying $f failed (UNAVAILABLE)"; exit 2; }
done

# Each stage writes one line "STAGE <name> PASS|FAIL|UNAVAILABLE|PENDING <why>".
ps <<EOF >"$out/run.log" 2>&1
\$ErrorActionPreference = 'Continue'
Set-Location '$remote'
New-Item -ItemType Directory -Force -Path work | Out-Null
\$lock = \$null
for (\$i = 0; \$i -lt 360 -and -not \$lock; \$i++) {
  try { \$lock = [IO.File]::Open('C:\isocast-dev\gpu.lock', 'OpenOrCreate', 'ReadWrite', 'None') }
  catch { Start-Sleep -Seconds 10 }
}
if (-not \$lock) { 'STAGE lock UNAVAILABLE gpu.lock stayed busy for an hour'; exit 2 }
try {
  python e2e.py --selftest --work work > selftest.json 2> selftest.err
  if (\$LASTEXITCODE -eq 0) { 'STAGE selftest PASS' } else { "STAGE selftest FAIL exit \$LASTEXITCODE" }
  if (Test-Path '$contracts') {
    python platform-contracts.py --bin '$contracts' --work contracts --suite media --hardware > contracts.txt 2>&1
    \$c = \$LASTEXITCODE
    if (\$c -eq 0) { 'STAGE contracts PASS' } elseif (\$c -eq 2) { 'STAGE contracts UNAVAILABLE exit 2' } else { "STAGE contracts FAIL exit \$c" }
    \$takes = @(Get-ChildItem -Recurse -Filter take.mp4 contracts -ErrorAction SilentlyContinue)
    if (\$takes.Count -lt 3) { "STAGE pattern FAIL only \$(\$takes.Count) takes written (normal, pressure and recovery expected)" }
    else {
      \$movies = \$takes | ForEach-Object { '--pattern-oracle'; \$_.FullName }
      python e2e.py @movies --fps 30 > pattern.json 2> pattern.err
      if (\$LASTEXITCODE -eq 0) { "STAGE pattern PASS \$(\$takes.Count) takes" } else { "STAGE pattern FAIL exit \$LASTEXITCODE" }
    }
  } else { "STAGE contracts UNAVAILABLE no binary at $contracts"; 'STAGE pattern UNAVAILABLE no takes without the contracts' }
  'STAGE engine PENDING the Windows daemon switch (W.8) and a named-pipe engine client'
} finally { \$lock.Close() }
EOF
for f in selftest.json selftest.err contracts.txt pattern.json pattern.err; do
  scp -q "$host:$remote_scp/$f" "$out/" 2>/dev/null || true
  # PowerShell 5 redirects as UTF-16.
  if [ -f "$out/$f" ] && [ "$(head -c 2 "$out/$f" | od -An -tx1 | tr -d ' ')" = fffe ]; then
    iconv -f UTF-16 -t UTF-8 "$out/$f" >"$out/$f.utf8" && mv "$out/$f.utf8" "$out/$f"
  fi
done
cat "$out/run.log"
printf 'oracle-windows: results in %s (remote %s)\n' "$out" "$remote"
verdict "$out/run.log"
exit $?
