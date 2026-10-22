# Isocast state

What an agent needs today to work safely and fast. This file holds current facts, not history.
When a fact changes, replace it. Date every gotcha. Last full refresh: 2026-10-22.

## 1. Product status

- **macOS 0.0.8 is the shipped release** (2026-10-19): Apple silicon, macOS 14+, notarized, Sparkle self-updates.
- **0.0.9** ships from main once hosted CI is green and Ryan says go (PLAN.md, "Next").
- **Windows (#105) is native on main, unreleased and unsigned.** It records program plus ISOs (WGC, MF cameras, WASAPI,
  NVENC H.264/HEVC, MF fallback, crash-safe `isocast-mux`), previews, streams RTMP/RTMPS at matched sizes, and has a
  per-user Inno installer and WinSparkle updates (test keys only).
- **Windows gaps:** CPU and resident RAM are still above OBS. Ryan's targets: at most half OBS's CPU, RAM below OBS,
  faster first preview, stop under 20 ms, zero drops. Missing: scaled streaming, replay buffer, media files, NLE XML
  export (refused). Windows work is tracked in #105 and its follow-up issues.
- **Remote engines R1 is on main**: CLI/MCP only, off by default (`isocast remote enable`), TCP 47470, pinned mutual
  TLS 1.3, local-only code + SPAKE2 pairing, OS DNS-SD. Verified basebook ↔ exon-desk both ways.
- **In progress:** R2 (app on a remote engine) paused as WIP on `agent/remote-r2-core`/`-ui`
  (`/Volumes/IsocastBuild/remote-r2-*/scratch/REPORT.md`); Windows UI DirectComposition port (committed
  on `agent/windows-perf`, CPU target still unmet) merging after pre-merge CI.

## 2. Architecture map

`isocastd` owns sessions, devices, recording and streaming. The app, CLI, MCP and obs-websocket are thin clients of
**one method table**, `isocast-protocol` `api.rs` (`Method`/`Event`), sent as newline JSON-RPC 2.0. Local transport is a
Unix socket on macOS and an owner-only named pipe on Windows; remote is TLS on TCP 47470; obs-websocket is TCP 4455.
A new capability needs all of these:
1. a `Method` appended in api.rs;
2. handling in `Daemon::call` (isocast-daemon `main.rs`);
3. a class in `isocast_protocol::caller::local_only_reason` (exhaustive);
4. a CLI command with `--json` and an MCP tool, both in isocast-cli;
5. regenerated reference docs.

- `isocast-protocol`: setup document (TOML/JSON), validation, schema, API types, caller policy, `platform_text` wording.
- `isocast-engine`: portable engine: shared clock, wgpu compositor, audio mix, recording, governor, the `Platform` trait.
- `isocast-daemon`: `isocastd` and its dispatcher. `platform.rs` picks the platform (macOS, Windows, else the stub);
  also folder roots (`roots.rs`), remote serving (`remote.rs`), projects, recovery, streams.
- `isocast-client`: connect (launching the engine), call, watch; engine selection by `--engine` / `ISOCAST_ENGINE`.
- `isocast-cli`: the `isocast` command and `isocast mcp`.
- `isocast-ui`: the Slint app (`Isocast.app` with `Contents/Helpers/Isocast Engine.app`; `isocast-ui.exe`), with
  offscreen snapshot states (`isocast-ui --help`). `isocast-app`, the retired web app, is not a default member.
- Platforms:
  - `isocast-platform-macos`: ScreenCaptureKit, AVFoundation, VideoToolbox, AVAssetWriter, IOSurface frames.
  - `isocast-platform-windows`: WGC via the helper `isocast-capture.exe`, MF cameras, WASAPI, NVENC/MF, DX12 import.
  - `-stub`: fail-closed, for other OSes. `-contracts`: dev-only contract workers (`scripts/platform-contracts.py`).
  - `-credentials`: Keychain / Credential Manager without media dependencies.
- Plumbing:
  - `isocast-ipc` (sockets, SID-namespaced pipes, local-or-TLS `Stream`) and `isocast-remote` (TLS, pairing).
  - `isocast-discovery`: Bonjour / `DnsService*`; it never opens UDP 5353 itself.
  - `isocast-surfaces`: zero-copy previews (IOSurface over Mach; DX12 shared handles on Windows).
  - `isocast-mux`: crash-safe MOV/MP4 packet writer (Windows). `isocast-diagnostics`: bounded crash-safe JSONL logs.
  - `isocast-obsws`: obs-websocket v5 server. `isocast-vp8`: vendored libvpx VP8 decoder (macOS arm64).

## 3. Machines and environments

**basebook** (this Mac): Ryan's work machine, M5 Max 18 cores, 48 GB, macOS 27.2, LAN 10.0.10.26.
- Ryan's own engine runs here (default socket, `~/Library/Application Support/Isocast`). Never touch it.
- Allowed: the delegator's merges; the `ui-dev` offscreen loop and headless motion harness (no windows, no focus
  stealing); being the Mac engine in remote tests (firewall off; the signed "Isocast Dev" already has LAN access).
- The nightly hardware runner starts here (cronctl `isocast-hw-nightly`, 01:30 New York).
- The isocast.com preview (`website/`, static, no build) is served on the LAN at http://basebook.local:8790/ by the
  LaunchAgent `dev.isocast.website` (KeepAlive; it serves the `.claude/worktrees/website` checkout). Port 8788 is
  something else; leave it alone. (2026-10-22)
- Gotcha (2026-10-22): another project's `wrangler dev` (workerd) can bind `localhost:8790` (127.0.0.1 and ::1), and
  then `basebook.local` resolves to ::1 on basebook itself and shows that app. Test the site from basebook via the LAN
  IP (`http://10.0.10.26:8790/`); other devices are unaffected. The hero's video loops are built by
  `/Volumes/IsocastBuild/website/scratch/harness/encode.sh` from copies of Ryan's take in `.../scratch/take/`.
- Never build, test or run gates here as a fallback; when remote-build has no free host, wait and retry.

**IsocastBuild** (`/Volumes/IsocastBuild`): a 120 GiB APFS sparse bundle on the Extreme SSD.
- After a reboot: `hdiutil attach -nobrowse "/Volumes/Extreme SSD/isocast-build.sparsebundle"`.
- `target` → `/Volumes/IsocastBuild/main`; lanes use `/Volumes/IsocastBuild/<lane>/{target,scratch}`.
- Keep ≥ 20 GB free (92 GB on 2026-10-22). Old lanes' reports are in `archive/lanes-2026-10-22/`.

**Build hosts** (`scripts/remote-build.sh`): exonbook (M4 Max, 14 cores), isocast-m2-a and isocast-m2-b (8 cores).
- Auto keeps a lane on its host and skips hosts running a hardware job. Lanes live in `~/isocast-builds/<lane>`; nice 15.
- Nothing starts below 40 GB free (m2-a had 43 GB on 2026-10-22; use `remote-build.sh prune`).
- Exit codes: 255 unreachable, 3 disk guard, 75 lane lock, 76 hardware job, 69 no free host.
- Same Rust 1.99, SDK 27.0 and arm64 everywhere: binaries from any host run on every Mac here.

**Test Macs** (`scripts/testmac.sh`, `ISOCAST_TESTMAC`). Prefer `push BIN_DIR` (~25 s) to `build`. `gui '<cmd>'` runs in
the desktop session through an interactive launch-agent bridge (needed for windows, `open`, cua-driver).
- exonbook (default): M4 Max, 36 GB, macOS 26.6, `ssh exonbook` (user ryanvogel), LAN 10.0.10.165. Screen stays
  unlocked; lid closed (no camera). Its firewall is on: never run a listening engine there. Ryan's own OBS is off
  limits. sudo needs `~/.config/isocast-testmac/exonbook-password`.
- isocast-m2-a (macOS 27.0) and isocast-m2-b (26.7): AWS mac2-m2.metal in us-east-2, profile `anomaly`, 24 GB, no
  camera or mic, 1024x768 headless, UTC clocks. SSH only from Ryan's IP; they can't reach each other. Dedicated hosts
  billed hourly: never stop, release or add one without Ryan.
- Benchmarks, strict e2e, soaks and real-window motion/drag-lag go through the queue, never your lane:
  `scripts/hw-batch.py submit KIND …` (results in `/Volumes/IsocastBuild/hw-queue/`).

**macOS test VM** (`scripts/vm.sh`, Lume `isocast-vm`, macOS 26.5, on basebook): permissions pre-granted, Cua space
`direct:127.0.0.1:13211`. Use it for prompts, Finder, Gatekeeper and onboarding; e2e with `--hosted`. No perf numbers.

**exon-desk** (Ryan's Windows PC): Ryzen 7 5700, 128 GB, RTX 5070 (one NVENC engine), Windows 10 22H2 (19045).
- `ssh exon-desk-lan` (10.0.10.149, the scripts' default) or `exon-desk` (Tailscale 100.78.27.26), user Ryan. The SSH
  shell is cmd.exe: use `exon-desk-ps '<cmd>'` or a heredoc; never hand-quote PowerShell.
- **Layout:** `C:\isocast-dev\{src,target,cargo,home,appdata,localappdata,scratch,staging,tmp}` is the main Windows
  root; each lane gets `C:\isocast-dev\lanes\<lane>` (`ISOCAST_WINDOWS_ROOT=C:/isocast-dev/lanes/<lane>`); `vm\` has Hyper-V.
- **Locks:** every Cargo build or test takes `C:\isocast-dev\build.lock` and never starts while `gpu.lock` is held.
  GPU/encoder benchmarks and timing runs take `C:\isocast-dev\gpu.lock`. Network-only runs need neither. Release in a
  `finally`, bound the wait and the hold, never delete a lock file.
- **Disk:** keep ≥ 150 GB free on C: (311 GiB on 2026-10-22). After each slice, delete your lane's target and large
  scratch (recordings, ETW traces).
- **Firewall:** one rule, group "Isocast dev": TCP 47470 in, any profile, from LocalSubnet and 100.64.0.0/10. The LAN
  adapter "Ethernet 4" is Public, Tailscale Private: don't change profiles. Discovery uses the built-in "mDNS (UDP-In)"
  rule. No other host-wide settings.
- **Allowed:** builds, tests, real windows, capture, camera, mic, NVENC, cua-driver, privacy settings for dev builds,
  dev tools. `scripts/windows-gui.sh --host desk --force-active` is allowed (Ryan isn't using it, 2026-10-19).
- **Never:** Ryan's files, accounts, browser data or app settings; uninstalling anything; stopping `camera-ndi-bridge`
  or `chatui` (they own the Cam Link 4K); changing default audio devices or volume. Ask the delegator before reboots,
  Windows features, driver changes or spending money.

**Hyper-V VM** `isocast-pkg` (`scripts/windows-vm.sh up|ensure-desktop|ssh|ps|down|revert-clean --yes`): Windows 11
Enterprise Evaluation (expires ~90 days after 2026-10-18), 8 vCPU, 32 GiB, no GPU (WARP): never perf or NVENC evidence.
Users `isocast` (guest admin) and `packager` (standard, for installer tests). Checkpoints `clean` (72dfd346…) and
`clean-prev` (0bb765b6…): keep only these when you finish, and leave the VM off.

**Public repo and CI:** R44VC0RP/isocast has its own history. Only the delegator pushes; agents never write to GitHub.
- `scripts/publish-snapshot.sh [--push] "msg"` copies `git archive main` into `~/dev/isocast-public`, after scanning
  for secrets and AI attribution.
- `scripts/ci.sh [--worktree] [--merge-main] [--lane NAME] [--allow-workflow-changes]` (2026-10-22) publishes a
  candidate (`git archive` of HEAD's tree, or the worktree's through a temporary index, so export-ignore applies and
  is checked) as ONE commit on public main to `ci/<lane>` only, after the same scan, and waits for CI, Engine E2E and
  Windows on it; evidence under `/Volumes/IsocastBuild/ci/<lane>`. It refuses a candidate without main (unless
  `--merge-main`) and a `.github/` that differs from public main (unless `--allow-workflow-changes`). ci, e2e and
  windows run on `ci/**` pushes; a lane's newer push cancels its older run, never another lane's.
  `scripts/windows-ci.sh` is `ci.sh --lane windows --workflow Windows`. Other workflows: release, nightly-soak, visual.

## 4. Commands that matter

- Isolated engine: `ISOCAST_SOCKET`, `ISOCAST_SETUP`, `ISOCAST_DEV_APP`, `ISOCAST_TIMELINES` under
  `/Volumes/IsocastBuild/<lane>/scratch`. Synthetic sources only.
- Build: `scripts/remote-build.sh build [isocastd isocast isocast-ui]` (release, copied to `$CARGO_TARGET_DIR/release`).
- Per commit: `cargo fmt`, `scripts/remote-build.sh cargo clippy --release -p <crate> -- -D warnings`, and
  `… cargo test --release -p <crate>`. Once before hand-off: `scripts/remote-build.sh gates`.
- After CLI, MCP or schema changes: `python3 scripts/reference-docs.py target/release/isocast` (CI runs `--check`).
- Signing: `ISOCAST_DEV_APP="<scratch>/Isocast Dev.app" scripts/sign-dev.sh <isocastd>`. Without `ISOCAST_DEV_APP`
  it re-signs Ryan's own dev app.
- Files: `ffprobe -v error -show_entries stream=codec_name,width,height,r_frame_rate,pix_fmt -of compact <file>`, plus
  durations, frame counts and A/V alignment.
- Frame oracle: `scripts/vm.sh e2e --hosted --quick --keep-failures` (2–3 min; `--only NAME`; `--full` once at
  hand-off). Windows: `scripts/oracle-windows.sh --engine [REF] [--only S]` (PENDING is never a pass). A failing
  oracle scenario is a real defect: read its contact sheet; never rerun until green.
- Windows: `scripts/windows-remote.sh sync|cargo …|build|run '<ps>'|fetch` and
  `scripts/windows-gui.sh --host vm|desk windows|tree|screenshot|click|type|key|scroll|wait-for|launch|close` (JSON).
- UI loop on basebook: `cargo build --profile ui-dev -p isocast-ui --features live-slint` once, then
  `ISOCAST_BIN=$CARGO_TARGET_DIR/ui-dev scripts/snapshot.sh --snapshot-watch DIR --size WxH --scale 2`.
  Compare builds with `scripts/visual-shots.sh` and `scripts/visual_diff.py`.
- Stop an engine: `isocast engine shutdown` (if idle) or
  `printf '{"jsonrpc":"2.0","id":1,"method":"shutdown"}\n' | nc -U "$ISOCAST_SOCKET"`. No top-level `isocast shutdown`.

## 5. Release

- `docs/releasing.md`, "Cut a release": bump the version (and Cargo.lock), write `docs/releases/X.Y.Z.md`
  (`python3 scripts/release-finalize.py --check-notes X.Y.Z`), publish the snapshot, tag in `~/dev/isocast-public`, push only
  the tag, approve `release`, check the draft, README once-over, publish.
- The delegator cuts and publishes releases (Ryan delegated, 2026-10-15). Agents never tag, push or publish.
- **Soak first:** `scripts/hw-batch.py now --detach soak --hours 6 --profile hardware --receiver m2-b --mac m2-a`
  (1080p60 HEVC program, 4K60 and 1080p60 ISOs, a 6 Mb/s stream, two kill tests).
- Pass: program and stream with zero drops, and both crash tests. The base M2 is overloaded by this profile, so its ISOs
  drop ~57% (0.0.8's soak); judge with `--args='--max-iso-drop-ratio 0.6'` (the default is 0.001).
- **README every release:** features match the DMG; unreleased work goes under "coming". Retake changed screenshots
  from the release build: `scripts/screenshots/take.sh` on an AWS Mac (`SHOTS_PREBUILT=1` after `testmac.sh push`) or
  `scripts/snapshot.sh`, never on Ryan's Mac. Commit as `README: X.Y.Z`.
- Windows packaging (Inno, WinSparkle) is in releasing.md. Authenticode and publication aren't set up yet.

## 6. Gotchas that still bite

- 2026-10-11: macOS ties privacy grants to the bundle id plus the signing identity. Re-sign isocastd after every build, or grants and Keychain reads break.
- 2026-10-15: A grant made in System Settings while isocastd runs reaches only processes started later. Restart the engine; "Quit & Reopen" doesn't restart it.
- 2026-10-15: macOS 27 attributes the bundled engine's permissions to the outer Isocast.app. System Settings lists "Isocast", not the engine.
- 2026-10-16: `isocast record` captures inside the CLI process, which over SSH has no screen or mic grant. Record through the engine with `isocast start SETUP` / `isocast stop`.
- 2026-10-15: Over SSH, a test Mac's login keychain is locked. Stream-key moves fail (-25293), and `security find-generic-password -w` blocks on a prompt. For local stream tests, set `ISOCAST_STREAM_KEY`.
- 2026-10-15: On the first `.local` lookup, macOS 27 asks "Allow Isocast Engine to find devices on local networks?". While that is unanswered, getaddrinfo blocks. DNS-SD advertising needs that permission too; the CLI is exempt.
- 2026-10-11: When the signed daemon opens a file in Downloads, Documents or Desktop, it waits for a privacy prompt.
- 2026-10-16: macOS caps Unix socket paths at 104 bytes. Keep `ISOCAST_SOCKET` short.
- 2026-10-15: The test Macs have no `timeout` command. Wrap `testmac.sh` in a local `timeout` instead.
- 2026-10-12: Before benchmarking on exonbook, check `pgrep screensharingd`. A Screen Sharing session competes for the encoders.
- 2026-10-18: The VM's VideoToolbox wedges after about 15 h up (-12903, "did not open within 8 s"). Fix it with `scripts/vm.sh down && scripts/vm.sh up`.
- 2026-10-16: m2-b has the BlackHole 16ch driver installed (it was still there on 2026-10-22). If a default mic source hangs there, suspect it.
- 2026-10-16: A new remote-build lane starts from an APFS clone of another lane's target, so stale crates can link. Before its first build, run `git diff --name-only main...HEAD | xargs touch` (or `cargo clean --release -p …`).
- 2026-10-16: `remote-build.sh` syncs the checkout it lives in, not `$PWD`. Build an A/B base with that worktree's own copy of the script.
- 2026-10-13: `testmac.sh` and `windows-remote.sh` sync only tracked files. Run `git add -N` on new files first.
- 2026-10-18: hw-batch takes its scripts from the commit. Uncommitted harness changes need `--scripts worktree --bin-dir BIN`.
- 2026-10-14: Never run `git add -A` in the main checkout. Ryan keeps untracked files there, and publish-snapshot would publish them.
- 2026-10-16: On macOS, `File::sync_all` is F_FULLFSYNC, about 14 ms on IsocastBuild. Keep it off anything a person or a reply waits on.
- 2026-10-22: Windows SSH logons are network-only and run in session 0. They get no Credential Manager (0x80070520), no desktop capture and no GUI. Use `windows-gui.sh` for those.
- 2026-10-18: A UI launched by Task Scheduler inherits a no-breakaway job, so its daemon launch fails by design. Launch from the Start shortcut or Explorer (`windows-gui.sh launch`).
- 2026-10-18: Hosted Actions steps also run in a no-breakaway job, so a daemon the CLI launches fails with "Access is denied (os error 5)". CI steps start isocastd themselves.
- 2026-10-18: `isocast-ui.exe` is a GUI-subsystem program, so PowerShell `&` neither waits for it nor sets `$LASTEXITCODE`. Use `Start-Process -Wait -RedirectStandardOutput`.
- 2026-10-18: PowerShell 5 writes `>` redirects as UTF-16. When piping Unicode to other tools, set `$OutputEncoding = [Text.UTF8Encoding]::new($false)`.
- 2026-10-18: `windows-remote.sh` refuses to sync a `src` folder that holds files it didn't put there. Never start a process with `src` as its working folder.
- 2026-10-22: On main, `windows-remote.sh` locks `<root>\build.lock`. For a lane under `lanes\`, that is not the shared `C:\isocast-dev\build.lock`. Take the shared lock yourself and check gpu.lock (the windows-perf worktree has an uncommitted fix).
- 2026-10-17: Windows pipe names include the user SID and the path. Stop old engines with their old client before you replace binaries.
- 2026-10-17: A cold native Windows release build takes about 14 min, and a UI relink about 12. The hosted Windows UI build nearly fills the 16 GB runner, so windows.yml builds it alone with `ISOCAST_UI_NO_GALLERY=1` and `-j 1`.
- 2026-10-17: The RTX 5070 (driver 617.42) admits 12 NVENC sessions. The 13th fails with NVENC error 21, not out-of-memory. Don't hard-code 8.
- 2026-10-18: The 5070 has one NVENC engine. A 4K60 program plus 4K60 and 1080p60 ISOs exceeds HEVC P5. Plan presets; never enlarge queues or retry to hide drops.
- 2026-10-17: While NVENC maps an input, nothing else may touch it. Never map a texture that the compositor or another encoder reads just to save a copy.
- 2026-10-17: NVIDIA's H.264 MFT ignores the B-frame count and rejects live FPS changes. MF AAC set to 160 kb/s delivers about 106 kb/s, and its CBR/VBR controls return E_NOTIMPL.
- 2026-10-18: On exon-desk, camera-ndi-bridge owns the Cam Link 4K. Only shared MJPG 1080p60 reads work. The "Elgato 4K S" entry is the wrong device.
- 2026-10-22: exon-desk's default mic is "Microphone (Virtual Desktop Audio)" (mono 48 kHz, ~3 ms QPC phase spikes), not the Wave XLR.
- 2026-10-19: Hyper-V's first DXGI adapter differs from wgpu's choice. Capture, compositor and viewers must use the daemon's pinned adapter LUID.
- 2026-10-18: WGC `CreateForWindow` fails with E_INVALIDARG on WS_EX_NOACTIVATE windows. `TryGetNextFrame` returns S_OK plus null when no frame is ready.
- 2026-10-22: Windows DNS-SD answers a new browse from a cache that keeps dead engines for about 4,000 s. Treat browse results as hints until a resolve answers.
- 2026-10-22: GPU-backed private commit is not physical RAM. Claim CPU or RAM wins only from workloads matched against OBS. `ISOCAST_SOCKET` sets the `.logs` path; `ISOCAST_LOG_DIR` is ignored.
- 2026-10-22: Windows native previews use transactional DirectComposition surfaces beneath Slint's
  premultiplied visual (one GPU copy on the existing DX12 queue through an On12 wrapper). The monitor
  ring must be native too, below selection; a Slint ring paints over its outline and handles.
  Slint 1.18.1 needs the pinned renderer surface hook: choose alpha before first configure, not later
  via ResizeBuffers. The attempted destination-Unwrap path stalled on exon-desk; wrapped-source
  CopySubresourceRegion works. `--preview image` is the control; `ISOCAST_DCOMP_FAIL_SETUP=1` exercises
  setup fallback without touching the system driver. Qualification evidence is in windows-perf's scratch REPORT.
- 2026-10-22: The matched 18-take DComp cohort (n=3 per mode/resolution) reduced total one-core CPU
  from image 26.16/25.09% to 23.28/21.52% at 1080p/4K; OBS was 19.21/19.54%. RAM did not improve.
  All 36 files passed individual media checks; all 12 native takes passed accounting/alignment with zero
  losses. OBS cross-output alignment remains unverified. Windows oracle: 9 PASS, 2 partial-PENDING.
  First-preview capture brackets overlap; no proven no-regression claim. Native submissions stayed ~60/s,
  while a diagnostic trace measured 3.16 full-window UI presents/s. Removing Slint redraws alone did not
  meet the CPU target. No-redirection produced blank native-menu captures (physical screen unconfirmed);
  version metadata and ValidateRect probes gave no established win. None of those experiments is retained.
- 2026-10-22: GitHub's macos-15 runners (macOS 15.7.9, 3 vCPU M1 VM, paravirtual Metal and encoders) drop frames:
  GPU waits stall up to ~143 ms, and a zero-drop qualification never predicted a zero-drop suite (every hosted run
  2026-10-17..22 dropped somewhere; worst 1.5% of the suite's slots). So `e2e.py --hosted` always allows accounted
  loss up to 25% per output and 3% over the suite (dropped + held oracle pictures, at most 3 per output and never two
  in a row + late file starts, at most 3 slots), with every integrity, picture-oracle and audio check still strict.
  The zero-drop bar is the test Macs' (no `--hosted`).
- 2026-10-22: macOS 15 `AVAssetWriterInput appendSampleBuffer:` before `startSessionAtSourceTime:` returns YES; macOS
  26 throws. The pixel-buffer adaptor throws on both, as does any append on a writer that never started writing.

## 7. Open decisions and waiting on Ryan

- **Windows release:** needs Authenticode signing and Ryan's call on the first Windows release. Packages so far are
  unsigned test-key builds.
- **exon-desk:** every reboot, Windows feature (Hyper-V) or driver change needs Ryan's approval.
- **R2** resumes after the streamlining. Ryan confirmed the remote product rules on 2026-10-22 (docs/remote-engines.md,
  "Decisions"); the rest are the delegator's, and he can overrule them.
- **Needs Ryan, nothing waits:** #104 NLE export in Final Cut and Premiere (Resolve passed; needs his Apple ID or a
  licence); #74 Touch Portal / Stream Deck pairing; G.5 media keys (Input Monitoring on a test Mac); Accessibility for
  m2-b's bridge (full motion-harness drag paths); #67 virtual camera in QuickTime/Zoom/FaceTime on a real display.
- **Questions for Ryan, nothing waits:** should replays use the stream's format only while streaming? Is the
  capture card still part of the rig?
- **Tests (Ryan, 2026-10-22):** use CI for testing; tests that run in CI are welcome (AGENTS.md).

## 8. The board

Ryan follows work on the board, `http://basebook.local:8787/?k=<token in ~/dev/isocast-status/token.txt>`.
Agents update it with `~/dev/isocast-status/board` (AGENTS.md, "How a lane works").

## 9. Where the history went

The old PLAN.md, docs/notes.md, docs/handoff/agent-rules.md and the delegator handbook are archived in `docs/archive/2026-10-22/`.
Search them with `grep -rn '<term>' docs/archive/2026-10-22/`. Lane evidence is in
`/Volumes/IsocastBuild/archive/lanes-2026-10-22/`.
