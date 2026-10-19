#!/bin/sh
# The frame oracle on Windows (#124), on exon-desk.
#
#   scripts/oracle-windows.sh [--contracts BIN.exe]
#   scripts/oracle-windows.sh --engine [REF] [--only SCENARIO]...
#
# --engine: the engine scenarios on Windows. Builds REF (default
# agent/windows) natively on exon-desk with that ref's own
# scripts/windows-remote.sh in our lane root C:\isocast-dev\lanes\oracle
# (its own src, target and scratch; never the Windows lanes'), then runs this
# checkout's e2e.py there through windows-remote.sh's run under the shared
# GPU lock (C:\isocast-dev\gpu.lock), driving that isocastd.exe over its
# named pipe with synthetic sources only. Default scenarios: record, iso,
# split, scenes (test patterns and the tone), win_record (program, ISOs,
# vertical and square canvases), win_takes (split, pause), win_scenes (cut,
# fade, slide), stream, vertical, oracle_canvases and oracle_masks. Stream and
# vertical use matching canvas/encoder sizes. What Windows doesn't do yet
# (previews, scaled streaming, the replay buffer, media files) is PENDING, never a
# pass. Results (report, per-scenario lines, kept failures with contact
# sheets; ORACLE_WINDOWS_KEEP=--keep keeps everything) come back to
# $ISOCAST_ORACLE_OUT. Exit 0: no scenario failed (some
# may be pending); 1: one failed; 2: it couldn't run.
#
# ISOCAST_ORACLE_ENGINE_WORKTREE may explicitly select a checked-out worktree
# at REF's HEAD, including tracked uncommitted changes. The native helper's
# tree receipt, not the HEAD alone, then identifies the tested source.
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
# 4. engine scenarios: not in this mode; --engine (above) runs them.
#
# Exit 0: stages 1-3 passed. 1: one failed. 2: one couldn't run (no contracts
# binary, the GPU lock stayed busy, the PC unreachable): not verified.
# BIN.exe defaults to C:\isocast-dev\target\release\isocast-platform-contracts.exe
# (built by the Windows lane; we never build or edit Windows platform code).
set -u
cd "$(dirname "$0")/.."
host="${EXON_DESK_HOST:-exon-desk-lan}"
ps() { EXON_DESK_HOST="$host" exon-desk-ps "$@"; }
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
  0) echo "oracle-windows: PASS (selftest, contracts, pattern; the engine scenarios: --engine)" ;;
  1) echo "oracle-windows: FAIL" ;;
  *) echo "oracle-windows: NOT VERIFIED (a stage couldn't run)" ;;
  esac
  return $status
}
if [ "${1:-}" = --engine ]; then
  shift
  ref=agent/windows
  case "${1:-}" in ""|--only) ;; *) ref="$1"; shift ;; esac
  only=""
  while [ $# -gt 0 ]; do
    [ "$1" = --only ] && [ $# -ge 2 ] || { echo "oracle-windows: --engine [REF] [--only NAME]..." >&2; exit 2; }
    only="$only --only $2"; shift 2
  done
  [ -n "$only" ] || only=" --only record --only iso --only split --only scenes --only win_record --only win_takes --only win_scenes --only stream --only vertical --only oracle_canvases --only oracle_masks"
  sha="$(git rev-parse --verify "$ref^{commit}")" || exit 2
  stamp="$(date +%Y%m%d-%H%M%S)-$$"
  out="${ISOCAST_ORACLE_OUT:-/Volumes/IsocastBuild/oracle-windows/scratch/engine-$stamp}"
  mkdir -p "$out"
  if [ -n "${ISOCAST_ORACLE_ENGINE_WORKTREE:-}" ]; then
    tree="$(cd "$ISOCAST_ORACLE_ENGINE_WORKTREE" && pwd)" || exit 2
    [ "$(git -C "$tree" rev-parse HEAD)" = "$sha" ] || {
      echo "oracle-windows: explicit worktree HEAD differs from $ref"; exit 2;
    }
    [ -z "$(git -C "$tree" diff --name-only --diff-filter=U)" ] || {
      echo "oracle-windows: resolve worktree conflicts before qualification"; exit 2;
    }
    git -C "$tree" status --porcelain >"$out/worktree-status.txt"
    git -C "$tree" diff --binary HEAD >"$out/worktree.patch"
    echo "oracle-windows: explicit worktree $tree; tracked uncommitted bytes included"
  else
    # The ref's own tree (its windows-remote.sh and sources), detached.
    tree="/Volumes/IsocastBuild/oracle-windows/src/$(echo "$sha" | cut -c1-12)"
    [ -d "$tree" ] || git worktree add --detach "$tree" "$sha" >/dev/null || exit 2
  fi
  lane="C:/isocast-dev/lanes/oracle"
  export ISOCAST_WINDOWS_ROOT="$lane" ISOCAST_WINDOWS_OUT="$out/native" EXON_DESK_HOST="$host"
  echo "oracle-windows: building $ref ($(echo "$sha" | cut -c1-12)) in $lane"
  (cd "$tree" && ISOCAST_WINDOWS_TIMEOUT=3500 scripts/windows-remote.sh build isocastd isocast) >"$out/build.log" 2>&1 \
    || { tail -20 "$out/build.log"; echo "oracle-windows: the build failed (NOT VERIFIED)"; exit 2; }
  run="$lane/scratch/oracle/run-$stamp"
  ps "New-Item -ItemType Directory -Force -Path '$run' | Out-Null" >/dev/null || exit 2
  scp -q scripts/e2e.py "$host:$run/e2e.py" || exit 2
  echo "oracle-windows: running$only (under the GPU lock)"
  (cd "$tree" && ISOCAST_WINDOWS_GPU_LOCK=1 ISOCAST_WINDOWS_TIMEOUT=3500 scripts/windows-remote.sh run "
Set-Location '$run'
\$env:ORACLE_KEEP = '${ORACLE_WINDOWS_KEEP:---keep-failures}'
# windows-remote.sh refuses to sync a src with stray files: a tool started
# with src as its working directory once left a PowerShell cache there.
\$stray = '$lane/src/Microsoft'
python e2e.py --daemon '$lane/target/release/isocastd.exe' --cli '$lane/target/release/isocast.exe' \
  --work '$run/work' --report '$run/e2e-report.json' \${env:ORACLE_KEEP}$only --timeout 300
\$code = \$LASTEXITCODE
if (Test-Path -LiteralPath \$stray) { Remove-Item -Recurse -Force -LiteralPath \$stray; 'oracle-windows: removed a stray src/Microsoft cache' }
exit \$code") >"$out/run.log" 2>&1
  scp -q "$host:$run/e2e-report.json" "$out/" 2>/dev/null
  scp -q -r "$host:$run/work" "$out/" 2>/dev/null
  grep -E '^(PASS|FAIL|SKIP|PENDING) ' "$out/run.log"
  printf 'oracle-windows: report %s/e2e-report.json; kept failures (contact sheets) in %s/work (remote %s)\n' "$out" "$out" "$run"
  [ -f "$out/e2e-report.json" ] || { tail -20 "$out/run.log"; echo "oracle-windows: no report (NOT VERIFIED)"; exit 2; }
  python3 - "$out/e2e-report.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
t = r.get("totals", {})
print(f"oracle-windows: {t.get('PASS', 0)} pass, {t.get('FAIL', 0)} fail, {t.get('PENDING', 0)} pending, "
      f"{t.get('SKIP', 0)} skipped" + (f"; harness error: {r['error']}" if r.get("error") else ""))
sys.exit(1 if t.get("FAIL") or r.get("error") else 0)
PY
  exit $?
fi
if [ "${1:-}" = --status-of ]; then
  verdict "$2"
  exit $?
fi

stamp="$(date +%Y%m%d-%H%M%S)-$$"
out="${ISOCAST_ORACLE_OUT:-/Volumes/IsocastBuild/oracle-harness/scratch/windows-$stamp}"
contracts='C:\isocast-dev\target\release\isocast-platform-contracts.exe'
[ "${1:-}" = --contracts ] && contracts="$2"
remote="C:\\isocast-dev\\scratch\\oracle\\run-$stamp"
remote_scp="C:/isocast-dev/scratch/oracle/run-$stamp"
mkdir -p "$out"
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
  'STAGE engine NOT RUN here: scripts/oracle-windows.sh --engine runs the engine scenarios'
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
