# Working notes

Short, dated facts a future session needs. Newest entries go at the top of
each section. Delete entries that stop being true.

## Environment

- 2026-10-22 (R1): native remote keys reuse the existing Keychain/Credential
  Manager code through `isocast-platform-credentials`, not the full media
  backend (otherwise the Mac CLI gained 17 media-framework dependencies).
  `ISOCAST_REMOTE_CLIENT` isolates a client's public pin file and credential
  account; engine pins/settings live at `<setup>.remote.json`. No private
  key files or bearer tokens. Sign development CLI builds consistently;
  inaccessible/missing keys never silently rotate a stored identity.
- 2026-10-22 (R1 Windows qualification): network-only SSH logons cannot use
  Credential Manager (0x80070520). The approved scratch desktop runner used
  the signed-in desktop token, owner-only IPC, a bounded GPU lease and job
  cleanup. Its SSH caller granted only same-user query rights on its own
  ephemeral process/token objects to satisfy IPC peer inspection across
  logons. Production IPC/credential permissions were unchanged. Runner
  source and exact two-machine driver stay in remote-r1/scratch, not the repo.
- 2026-10-22 (#105 microphone): actual default is Microphone (Virtual Desktop
  Audio), not Wave XLR. At native48k, continuous positions plus ~3ms QPC phase
  spikes caused ISO timestamp gaps/silence/trim. Warmed five-packet phase median
  now applies only to microphones; startup/reset, loopback and SRC retain prior
  timing. Real gaps reset it; uncertain timing still fails. Six inline tests
  pass. Real4K80s proof corrected4spikes with0output losses; proof preceded the
  mic-only scope restriction, whose mic path is unchanged. Historical fatal
  nonadvancing-QPC failure, other devices/soaks/physical A/V sync unqualified.
  Mono48k mic ISO legitimately fails W19's stereo-only workload assertion;
  separate interpretation preserves that gate and verifies other file checks.
- 2026-10-22 (#105 final handoff): exact frozen tree
  bc2b5b737b1b4dffd39b37ff654f97ddb8a4e122 passed native checks, full portable
  gates on exonbook,12interleaved matched takes and Windows oracle9PASS/2partial.
  CPU and resident RAM still exceed OBS; no hidden helper-CPU gain found.
  Full-window preview redraw remains; native-child preview is design-only.
  Evidence: /Volumes/IsocastBuild/windows-perf/scratch/REPORT.md. Owned Windows
  target/target-installer caches and5ETLs cleaned, C:328.6GiB free afterward;
  frozen final-bin-20261022, recordings, exported analyses and hashes preserved.
  No commits; delegator owns rollout. ISOCAST_SOCKET determines .logs path;
  ISOCAST_LOG_DIR is ignored. Empty untracked p.md left untouched.

- 2026-10-22 (#105): wgpu's default max_non_sampler_bindings=1million
  eagerly creates a DX12 descriptor heap per device. UI and engine now request
 65,536; measured~55MiB less private commit together, but private working set
  stays unchanged. Do not equate GPU-backed commit with physical CPU RAM.
- 2026-10-22 (#105): LiveFile.output_active is optional, supported on Windows
  only after the mux writes a video fragment. The first encoded packet can
  still be buffered: progress.video_frames, not accepted_video_frames, drives
  the one-shot callback. The existing coordinator publishes that event early;
  fsync durability and actual encoder startup are separate. Unknown backends
  remain omitted; every split gets a new flag.
- 2026-10-22 (#105 measurement): plain release matches installer optimization
  settings, but old profiling builds also removed the gallery/added debug info.
  Final runs use full gallery/no debug override. Private stages deliberately
  omit WinSparkle to avoid shared updater preference writes. Fresh lightweight
  ETW found Native60presents/s vsOBS60 at1080,54 at4K; viewports also differ.
  Do not claim full preview-work parity or normalize all CPU by previewfps.

- 2026-10-19 (#105 scheduling): WindowsPlatform wires the existing deadline
  timer, Audio MMCSS and recording power-lease hooks. Video/GPU/file workers
  now AboveNormal, not blanket MMCSS Capture. Win10's unsupported power-policy
  query remains no-op; other power-hint failures warn without aborting record.
  Matched n=3/oracle show zero native losses but no consistent CPU improvement;
  do not advertise a CPU win or Win11 restoration qualification.

- 2026-10-19 (#105 Windows performance): symbolized ETW attributes roughly
  69% of UI sampled CPU to Slint's wgpu render path, including full-window
  drawing, imported-texture transitions and presentation. Preview textures
  already match physical display size; do not fix an imaginary full-canvas
  resize or claim a30fps cap as a matched60fps improvement. UI MemoryUsage
  hint cuts private commit by~180MiB at1080/4K without changing cadence;
  resident memory is effectively unchanged. Report/evidence:
  `/Volumes/IsocastBuild/windows-perf/scratch/REPORT.md`.
- 2026-10-19 (#105 benchmark): merged UI's parallel preview/GPU startup can
  launch two daemon candidates; the process lock selects one. W19's adapter
  pinned the first candidate even after it exited, rejecting the actual owned
  pipe server forever. Scratch adapter now reselects only after the held
  candidate exits, only from verified owned family members; per-connection
  server-PID and executable identity checks remain. Failed cohorts retained;
  fresh n=3 baseline/candidate both complete. This is not a production startup
  fix or grounds for discarding a genuine application failure.

- 2026-10-19 (#105 integration): Windows capture, compositor and viewers
  must use one DX12-compatible adapter LUID. Hyper-V exposed different
  first-DXGI and wgpu-selected adapters; matching power preferences was
  insufficient. `gpu_adapter` returns the daemon's pinned identity before
  a client renderer opens; capture helpers use their parent's LUID. Device
  recreation keeps the identity; adapter removal requires restart. The
  importer still rejects mismatches. Legacy engines may fall back only on
  typed unknown/unsupported-method replies, not arbitrary startup failures.
- 2026-10-19 (#105 integration): software H264 MFT activated in the clean
  VM, but its input converter failed QueryInterface(ID3D11VideoDevice),
  E_NOINTERFACE. Software BGRA8 now uses RGB readback and CPU Rec.709 NV12
  conversion; hardware still uses the video processor. Installed VM CLI
  recording verified at720p30,10s,300 frames without the UI. Live UI/WARP
  recording still loses frames on an idle host under the shared hardware
  reservation (8vCPU/32GiB Hyper-V, WARP); closing the UI gives300/300 at
 720p30. Do not infer GUI performance from successful encoder opening.
  No speculative preview throttling fix was applied. Evidence and current
  status: windows-integrate/scratch/REPORT.md on the build volume.
- 2026-10-19 (#105 integration): early native frame loss was intermittent
  on the unchanged merged tree, so no bad merge was established. A120ms
  pause of the owned NVENC completion thread reproduced11lost/540 with
  compositor missed_frames0. File-writer admission now shares a100ms wait
  window across queued frames, followed by100ms without waits; many-audio-
  track files use20ms windows. Three repeats kept540/540; a300ms stall still
  reported loss and preserved audio. Native queues and streaming unchanged.
- 2026-10-19 (oracle): sync_check's strict0.15s margin excluded a flash at
  exactly0.150000 but retained its beep's first nonzero sample at0.150021.
  Keep edge margins halfway between frame ticks at the actual fps. The
  retained failing movie has all300frames and5 synchronized pulses; altered
  missing-pulse and shifted-audio signals still fail the corrected check.

- 2026-10-18 (#105 Windows menus): the menus are platform-correct through
  `if PlatformText.windows` items in ui/app.slint; the Mac items are
  unchanged.
  - File ends with Exit (Alt+F4); Close Window (Ctrl+W) is macOS-only.
  - Redo is Ctrl+Y. The studio's and the editor's keys still take
    Ctrl+Shift+Z.
  - The Window menu has Maximize/Restore (`Ui.window-maximized`, set on
    Resized) instead of Zoom, and Full Screen / Exit Full Screen on F11
    instead of Ctrl+Win+F. It has no Bring All to Front.
  - Settings › Hotkeys' Window group says F11 and Alt+F4.
  - **Gotcha:** Windows doesn't send `WM_UAHDRAWMENU` for a borderless
    full-screen window, so its menu bar draws white. Full screen therefore
    hides the menu bar (as F11 full screen does in other Windows apps), and
    F11 leaves through the window's keys.
  - `style_windows_soon` reinstalls the chrome subclass so it sits above
    muda's re-created one.

- 2026-10-18 (#105 Windows chrome): the app's windows draw Windows' title
  bar, border and menu bar in Console's colours (src/chrome_windows.rs).
  winit's dark theme is requested at window creation (winit then keeps it
  when Windows' own mode changes); DWM caption/border/text colours come
  from `surface-chrome`, `line-strong` and `text` (Windows 11 only).
  - The menu bar is muda's native one. A window subclass installed after
    muda's handles `WM_UAHDRAWMENU`/`WM_UAHDRAWMENUITEM` with
    `surface-chrome`/`overlay-hover`/`text`, and repaints the 1px line
    under the bar in `line`.
  - If muda re-creates its menu (its subclass then sits above ours), the
    bar falls back to muda's dark #2b2b2b: still dark, just not the token.
  - Dropdowns use uxtheme's `SetPreferredAppMode(ForceDark)` (ordinal 135)
    and `FlushMenuThemes` (136).
  - Console is the only theme and it is dark (`DARK` in that file), so the
    chrome stays dark in Windows' light and dark modes, as macOS forces
    DarkAqua.
  - App icon: `crates/isocast-ui/windows/isocast.ico`, drawn from
    Isocast.icon by `scripts/windows/make-icon.py`. build.rs writes it into
    a .res file for link.exe (no rc.exe or new crate), with RT_GROUP_ICON
    ordinal 1. winit's window and taskbar icons load that ordinal, and
    Inno's `SetupIconFile` uses the same .ico.
  - The first window's size comes from the shell's monitor
    (`STARTF_USEMONITOR`) or the primary one: work area at its DPI, minus
    `AdjustWindowRectExForDpi` with a menu, and centred. It is set before
    `show()` so nothing jumps.

- 2026-10-18 (#105 first run, Ryan's 0.0.7.801 report): **the console
  window was isocast-ui.exe itself**, a console-subsystem program (PE
  subsystem 3 in `test-build/packages/0.0.7.801`). It is now a GUI program
  (`windows_subsystem = "windows"`, subsystem 2); with arguments and no
  redirected output it attaches to the calling terminal, so `--help` and
  `--snapshot` still print there. The CLI, daemon and helper stay console
  programs. The daemon is started with `CREATE_NO_WINDOW` instead of
  `DETACHED_PROCESS` (breakaway/new group unchanged): a hidden console of
  its own, so a console program it starts can't open a visible window.
  - **Gotcha:** PowerShell's `&` neither waits for a GUI program nor sets
    `$LASTEXITCODE` (StrictMode then throws). `package.ps1` now runs
    `--update-info` with `Start-Process -Wait -RedirectStandardOutput`.
    Anything else that runs isocast-ui.exe from PowerShell must do the same
    (CI's wording step already does).
  - Windows permissions are read from the consent store Settings writes
    (`HKLM`/`HKCU ...\CapabilityAccessManager\ConsentStore\{webcam,microphone}`
    `Value`, and `HKCU ...\NonPackaged` for "Let desktop apps access"):
    `Deny` is Denied with the exact `ms-settings:privacy-webcam`/
    `-microphone` page; else Granted. Screen and output audio have no
    permission on Windows (Granted). Read-only; nothing is prompted.
  - Folders: env var first (USERPROFILE, APPDATA, LOCALAPPDATA, so lane and
    CI isolation keep working), then `SHGetKnownFolderPath`. `~/Videos`,
    `~/Documents`, `~/Desktop`… expand to their known folder (moved to
    another drive or OneDrive) only when `~` is the real profile.
  - `shared_frames` refusal is `RpcError::UNSUPPORTED` (-32003): viewers
    stop asking until their target changes and the monitor says "Preview
    not available"; logged once at info, not every 5 s at error.
  - `tune`/Optimize skip an ISO source that can't be opened (`skipped` in
    Tuning, a note first in Optimize) and measure the rest.
  - doctor: `Platform::video_encoders`, Windows opens NVENC briefly per codec
    on the recording GPU (never while this engine encodes; cached), then
    lists that GPU's MF hardware transforms, and software MF as the CPU
    fallback.

- 2026-10-18 (Windows GUI, live-qualified): `scripts/windows-gui.sh --host
  vm|desk COMMAND` runs in the interactive desktop, not SSH session 0.
  Per-call UUID tasks/folders live under `C:\isocast-dev\scratch\windows-gui`;
  a separate worker timeout plus SYSTEM watchdog cover UIA hangs/SSH loss.
  A hidden-at-creation WScript launcher and CreateNoWindow worker are necessary:
  PowerShell's own `-WindowStyle Hidden` briefly stole focus and broke Ctrl+A
  between calls. Compressed stdin frames avoid observed large-frame stalls;
  raw asynchronous pipe reads time out after 30s even before task registration
  (withheld-input probe exited in 33.74s including SSH startup). No passwords
  are used by GUI calls. Launch uses the running Explorer desktop dispatch,
  not in-process Shell.Application, so installers outlive the helper and use
  the user's TEMP. Close sends WM_CLOSE only to titled windows, not Slint's
  untitled message-pump HWNDs; it reports a request, not process termination.
  `tree` reports sparse trees, omits password values, caps at 2000 nodes and
  accepts depth 0..20. PNGs use real DPI-aware pixels and stay outside Git under
  scratch. Window PrintWindow can be blank for GPU-only surfaces; full virtual
  desktop screenshots are VM-only. Isocast previews can contain private content.
  Desk requires 120s idle even for reads; own input resets this gate too.
  Foreground changes fail closed; no ALT-tap focus workaround. Desk was active
  (2s), so its read-only listing/refusal checks were skipped without force.
  VM .802 installer, named Install/Finish clicks, explicit Isocast launch,
  window/full screenshots (inspected), 125-node tree, UIA Invoke, pixel/double
  click, Ctrl+A + exact typed field value, scroll, window/process/text waits,
  timeout, concurrent readers and graceful window close all passed. Final
  normal/error checks: zero GUI tasks and per-call directories. Evidence and
  screenshots: `/Volumes/IsocastBuild/windows-gui/scratch/REPORT.md`.

- 2026-10-18 (Windows VM desktop baseline, Ryan approved):
  `scripts/windows-vm.sh ensure-desktop` starts the guest, configures packager
  autologon once through LsaStorePrivateData, disables that user's screensaver
  and workstation locking. If configuration or a missing/locked desktop needs
  a restart, it refuses unless `--restart` is explicitly supplied (make sure
  other lanes are not using the VM). It then waits for real Explorer in
  session 1. Credential stays in the guest's LSA secret; never a plaintext
  Winlogon DefaultPassword value. Repeats do not rotate/retrieve the password;
  first setup refuses an already-installed Isocast account. Do not reuse the
  old plaintext-registry autologon preparation. `up` remains start-only.
  Replacement offline `clean` (72dfd346-3473-4238-9a33-94d7f772897e) derives
  from the old clean, not the packaging test state: no Isocast, no Inno,
  no cached answer XML, same SSH public host key and standard/non-admin
  packager. Only desktop/account configuration and first-logon OS state changed.
  Original clean is `clean-prev` (0bb765b6-b101-464d-b403-88c217311467).
  Prior installed state is preserved separately as `windows-gui-before-desktop`
  (dffa9c8f-57b3-4925-88c7-d3411d63b6d9); no checkpoint was deleted.

- 2026-10-19 (#105, NVENC reconcile): pinned capturee726ff44 merged into
  writeref80263a with --no-commit; conflicts resolved, A/V tail patch preserved.
  Capture acknowledged the4native/8pending+ready contract through REPORT.
  Accepted frames retain their source until the shared completion worker proves
  the copy query, then unlocks/unmaps output. Rejected attempts wait for query
  proof before release; proved pre-encode failures drain/close/reopen without
  adapter poison. Unproved copy/encode/cleanup/deadline failures still quarantine.
  No rejected frame becomes accepted. Fault replay: e726 safe retention but
  permanent poison; final candidate safe source release,3/3 prefix,9/9 peer,
  30/30 reopen; timeout still quarantines. Inline healthy-query wait showed
  11–15ms stalls and was replaced by worker-side proof:150healthy submits
  max1.47ms, no>5ms; completions max4.9ms. No global timer/spin/pool change.
  Native strictClippy/111unit+1doc, Mac250incldoc/1hardwareignore, and7native
  media contracts pass. Full sources/hashes/raw limits in windows/scratch/
  w15-nvenc-reconcile/REPORT.md. Merged real A/V take passes56checks:361frames
  each, endpoint spread247samples/5.146ms (under1024/48k), no trimming/loss.
  UI idle gate blocked window checks; native shared-frame program/source pixel
  checks65+65 passed, not a window-render claim. Fresh one-burst WaveXLR proof
  passes12checks:996.826Hz/−53.48dBFS, A/V−8.333ms,180frames/no gaps or losses,
  no device/volume changes. w16-merged-av andw13-audible retain evidence.
  Merge still uncommitted, A/Vtail hunk unstaged; don't replace NVENC wholesale.
  Next: planner-backed camera-free OBS comparison. Budget API ISOs beginP3,
  notP5; aggregate writer wiring still needed before claiming planned presets.

- 2026-10-19 (#105, Stop re-review): CLI/app now fall back to blocking Stop
  only on structured METHOD_NOT_FOUND. Actual old0.0.7daemon a232cc90 stays
  alive across the mixed-version test: BEFORE reproduces failure40/40; final
  AFTER43/43 returns real completed Reports and stops both CLI/app takes.
  Idle Stop still errors. UI keeps finishing/busy through old-server replies;
  Create/main/status/LCD/footer consistently show Finishing. Actual offscreen
  image verified, not just source: w14-stop-compat/evidence/sc14-after-6be3d3/
  out/finishing.png. Native strictClippy/build pass; Mac250incldoc/oneexisting
  ignore pass. Source/binary/cleanup receipts are in windows/scratch/REPORT.md.
  All work remains uncommitted. A/V tail writer.rs is a separate qualified
  change; fresh audible proof is staged but NOT played, OBS/planner deferred.

- 2026-10-18 (#105, Stop ACK ready for CI): additive request_stop returns a
  StopAccepted snapshot; old RPC Stop/Shutdown and foreground recording still
  wait for the durable Report. CLI stop now acknowledges, --wait preserves
  report/error behavior; MCP exposes both. App shows Finalizing, rejects stale
  status replies and reports finalization errors instead of a success toast.
  Stopped is enqueued after state update but before releasing/waking Stop or
  Shutdown; diagnostics stay outside the state lock. Actual20 normal stops,
  1080p60program+4K60screenISO: ACKp50=0.13425ms,p99=0.1822ms (nearest-rank,
  observed max, not population-tail guarantee). Durable completion remains
  31.70–134.59ms. Four edge cases also pass;48files/5266frames fully decode,
  zero reported loss,881checks. Video-only; A/V tail error remains separate.
  Native strictClippy/213tests and Mac250incldoc/one hardwareignore pass; ProRes
  capacity test is green natively. Regular Windows UI build completed16m42s
  after the earlier combined-run timeout; receipt0d983bb3, UI SHAeb2673ec….
  Evidence: windows/scratch/w11-stop/REPORT.md and w11-stop-percentiles.json.
  No claim of cold-CLI/UI latency or observed transient UI state. Uncommitted;
  audio proof then camera-free OBS comparison remain the next ordered items.

- 2026-10-18 (#105, Windows disk capacity): `space::device` now reads a local
  volume's GUID + format serial from an opened closest ancestor, following
  links and refusing dangling/inaccessible targets instead of guessing a disk.
  Platform free space uses GetDiskFreeSpaceExW caller-available bytes on the
  handle-derived GUID root; GUID-less shares keep the previous handle query.
  Windows estimates now use the recorder's USERPROFILE-aware home resolver.
  Previously failing ProRes capacity test now passes:85/85 native engine tests,
  strict native Clippy, full Mac gates250 incldoc/one hardwareignore, and native
  51/51 volume checks +6/6 driver checks. Existing C:/G: identities differ;
  aliases/Unicode/missing children agree; free space matches WinAPI exactly;
  HOME-unset synthetic calibration gives Fits only for the matching disk.
  No settings changes or calibration backfill. UNC/denied mounts not runtime
  qualified; GUID-less shares stay uncalibrated. Old Windows profiles with no
  disk identity need fresh calibration. Evidence: windows/scratch/w10-volume/
  v10-20261018T193825Z-292c5185dc19.driver.json and REPORT.md. Ready for CI,
  uncommitted; next slice is immediate Stop acknowledgement, then audio/OBS.

- 2026-10-18 (#105, REVIEW-W6 B1): NVENC now retains attempted source leases
  before its private GPU copy, including failed mapping. Four preallocated
  EVENT queries prove completion before release/reuse; unprovable copies retain
  their leases through bounded teardown/quarantine. Rejected attempts never
  enter accepted-picture accounting. Scratch before/after tests proved pooled
  early reuse and WGC-helper early Ack on 0c47af4c, neither on the fix; a 5.7s
  stall verified timeout/Drop quarantine and preserved map + copy errors.
  Final native media contracts 7/7 and strict all-target Clippy pass. Broad
  engine tests expose one unchanged Windows ProRes-estimate failure: non-Unix
  space::device returns None, so unknown disk capacity yields NoMargin rather
  than the test's Fits. Real A/V take hit the separately observed audio-end
  finalization issue (159 accepted samples past video end); no audio workaround
  in B1. Evidence: windows/scratch/w9-nvenc-lifetime/REPORT.md. No production
  injection hooks, macOS changes, commits or CI launches; tier-1 re-review due.
  Final video-only daemon take passes37/37: 1080p60program+4K60ISO,361frames
  each, complete manifest/empty errors/zero reported loss, full decode and
  owned cleanup. It isolates NVENC; the failed A/V take remains preserved.

- 2026-10-18 (#105, audible-signal proof): explicit WaveXLRrender loopback,
  neverdefault-LG playback, recordsone0.600s997Hzburst atPCMpeak0.003. Decoded
  nativeHEVC/AAC3s file has180/180frames/no losses, FFT996.826Hz, −53.48dBFS;
  audioonset0.875s vsclockedvisual0.900s gives−25ms (declared±40ms). Defaults,
  mute andglobal/sessionvolume untouched; all152160capturedframes accounted
  as6024preroll+144000inrange+2136postend. Zero gap/timestamp/writer losses;
  oneaggregate discontinuity isnotindependentlyattributedperpacket. Native
  selected-endpoint API proof withsyntheticmarker, NOTselected-output CLI,
  WGC oracoustic-latencyproof. Production96-samplejitterpolicy preservesall
  accepted samples; firstdirect-writerprobe failedone-sampleoverlap, second
  hadgenuine384-framegap, bothretained. Finalcapturethread usesproduction
  real_time_thread hook; no causalMMCSS-fixclaim fromonepass. Sources/hashes/
  commands: windows/scratch/w7-audio-proof/corrected-c/REPORT.md. No moretones.

- 2026-10-18 (#105 transport): large Windowslaunch couldstall BEFOREworker/
  watchdog; lastjob329a4259 hadstagingbutnoactive/result andwasdisarmed.
  windows-remote.sh now uses a finite newline/base64 ReadLine frame andlocal
  transporttimeouts, not EOFdependentReadToEnd foritsmainlaunch. Rootcause
  remainsunproven; wallcapsboundevenpre-shimstalls. Alltimeoutcallsuse
  --foreground afterreviewcaughtdeferredCtrl-C; localpgrpSIGINTtrap0.302s.
  Nativeechoexit0, explicitexit1,5sbudgetSleep30exit124/cleanup andfullnative
  3binbuild pass. Logs/scripts only; no defaultdevices/locks/otherlanes touched.
  Source/nativebuild b7be2655, signalproof6e5f755b, finaltransportechoac6ac39d.
  Nativecontractworker refreshedfororacle: SHAbe76331b99fd4d4eb440ee08dbff05d60275070d6836c8317e716b4adbfe9c96,
  receipt54b9caa3, HEAD0c47af4c+transportonly. OracleownsitsGPU-lockedruntime.

- 2026-10-19 (#105 Windows streaming slices, uncommitted): native AAC requested
  at 160 kbps produced about 106 kbps in the stream oracle. ICodecAPI's VBR/CBR
  controls return E_NOTIMPL. The optional live constructor adds eight standard
  ID_FIL elements before the original access unit, preserving byte alignment.
  Budgeting uses encoded 1024-sample units, not elapsed wall time. The recording
  packet API is unchanged. All 24 rate/channel/content cases preserve original
  payloads and decode to identical float PCM. Oversized native packets remain
  intact: this is not a hard peak limiter. Main and portrait stream oracles pass
  without relaxed assertions; main measures 6000.4 video / 160.4 audio kbps.
  The 615-second 1080p60 soak kept 36,900 frames in each program/ISO file. Both
  RTMP destinations received 36,746 frames over 612.434 seconds, with no drops,
  full decode and relative bar-progression checks. First-picture calibration
  does not prove absolute latency. A ten-second receiver outage reconnected
  main while backup stayed live; all three recording files kept 2100 frames.
  The soak used the legacy encoder factory, not the writer's admission ledger.
  RTMPS used private MediaMTX v1.21.2 on loopback, with no system trust changes.
  Untrusted-root failure preserved two 900-frame recordings; the trusted case
  preserved two 1800-frame recordings and sent 610 frames with no publisher
  drops. Its late-joined reader decoded 368 video / 375 AAC frames; video began
  at 1.869 seconds while waiting for a keyframe. All relative frame checks pass.
  MediaMTX discovers tracks over two seconds of media: wait for its API path
  readiness, not just `stream.state=live`. Failed control attempts are retained.
  A 3 Mbps proxy triggered native NVENC acknowledgements of 6000→3000→1500 kbps;
  received video in seconds 25–35 measured 1500.288 kbps. The congested stream
  reported 151 drops and decoded cleanly; program and ISO kept 2400 frames each.
  An earlier proxy forgot to close its backend on EOF; that was a harness
  cleanup failure, not recording loss.
  Credential Manager uses per-user, machine-persistent generic UTF-8 blobs.
  Targets hex-encode service/account because Windows folds target-name case
  while RTMP paths can be case-sensitive. There is no file fallback. SSH lacks
  a credential store (error 1312); positive desktop persistence is NOT verified.
  The desktop launch guard refused active use and was not bypassed. ConPTY did
  verify hidden CLI input, echo restoration and no key in output/private files.
  Its test launcher must clear inherited SSH standard handles before spawning
  the terminal child. The existing diagnostic!() supplies the prompt newline.
  Only generated test keys were used, never real credentials.
  Developer logs confirm native live NVENC and shared destination hubs. The
  soak's three-session NVML snapshots do not identify all four logical outputs'
  backends. Budget integration, GPU live scaling and positive desktop key
  persistence remain open. Exact artifacts and limits are in capture REPORT.

- 2026-10-19 (#105 camera drop investigation, verification-only handoff):
  Original10s CamLink ISO+program both lackPTS ticks6,7,8;4K Sboth lacktick6.
  Both end at9.983333s withcontinuouslaterPTS. This is an early common-mode
  pipeline gap, not normal59.94→60cadence drift. Exact historical cause is
  unresolved: do NOT call it fixed or blame the camera/decoder from counters.
  Waited for writer's GPU lock; unmodified60s GeneralProfile+GPU and unprofiled
  runs each produced all6×3600frames, no gaps/fullFFmpegdecode. A third60s series
  used temporary bounded in-memory QPC timing plusCSwitch-only ETW(no sampled
  profiler): same6×3600,0loss. BothETLs report0lostevents/buffers. Source timings
  filtered to manifestepoch..epoch+60s: CamLink3600samples,59.994Hz, MFJPEGdecode
  mean9.080/p9911.000/max12.315ms, upload/convert submission mean.650/max1.365ms.
  These are IN-TAKE values, not startup maxima: rawpre-roll hasCamLink
  upload-submit24.784ms/devicegap32.770ms and4K Sdevice→worker46.946ms. They
  precede the recording epoch (capture begins~.42–.45s earlier), so they do not
  establish the cause of oldepoch+0.1s gaps. Summaries now include explicit
  epoch/window and pre-roll sections. In-takeCamLink timestamp→submission
  max19.063ms,23samples>16.667ms; file completeness does not mean every source
  sample met a one-period latency budget. Hardware timestamp semantics differ
  between readers: device→worker is not a cross-camera sensor-latency metric.
  The latter measures CPU submission, NOT GPU completion. NV12GPUcopy submission
  max.602ms virtual/.723ms4K S. Device→worker p99:virtual1.575ms,CamLink6.067ms,
  4K S12.328ms. No empty camera output/pool refusals in the measured windows.
  Virtual placeholder1918samples/60s(~31.960Hz), black4K S3525(~58.753Hz), with
  no25ms+intervals on4K S/CamLink; these are adapter-delivered timestamps, NOT
  separately proven physical-camera cadence. Output clocks hold pictures to
  fill3600ticks. Existing repeated_frames reports writer tail padding, not
  source-picture reuse; exact reuse/selection counts were not measured.
  Unprofiled developer traces:0encoderrefusals/programmisses, queue≤2,
  maximumwake virtual7.319ms/CamLink1.333ms/4K S1.506ms. No evidence justifying
  changing timer/pacing/writer semantics; no speculative fix retained.
  Temporary timing source snapshotb6f5c89a and diagnosticdaemon/PDB are retained
  privately; final crates matchcommittedd18f2515 exactly. Rebuilt uninstrumented
  daemon7d6b4a4e. All18movies fullydecoded(64,800frames). Exports initially failed
  because legacyWPApreset filters included unsupportedROI tables; CPU-only
  export completed. Use xperf contextswitch CPU totals, NOT the inconsistent
  legacyWPA precise-column sum (annotated). Source camera thread37.94CPU seconds
  agrees with38.05sampled; mfmjpegdec.dll leaf29.18sampledCPU seconds. These
  whole-process-lifetime CPU sums are not per-frame latency or old-gap proof.
  Receipts and qualifiers: windows-capture/scratch/REPORT.md; small local data
  in scratch/camera-drop-evidence. BridgePID30224 remained unchanged; no camera
  owner/configuration, chatui, priority, timer-resolution or cache changes.
  Provenance caveat: native run's tree is the SYNCEDsource, not necessarily the
  executable. Unprofiled4776b314 synced temporarysource86d60c85 but did NOT
  build; its command asserted existingdaemonSHA2563852943f…402f2 beforelaunch,
  the same pre-instrumentationproductionbinary as18de2a5a. Firstinstrumented
  build7cfda4af came later. Finalrestorehash14631ae3…af34e; productioncrates
  diff versus bothd18f2515 and originalc588tree is empty. No claim of CIexecution.

- 2026-10-19 (#105 common camera formats): Elgato Virtual Camera advertises
  9 modes (ARGB32 bottom-up, NV12, YUY2;1080/720/540@60), all without colour
  metadata. Explicit-metadata-only qualification was the rejection, not absent
  NV12. Treat unknown SDR YUV as limited709 for HD/601 for SD, RGB full; record
  that interpretation and native/decoded subtype plus exact rate in manifest.
  Nominal1000/1001/UVC rates qualify within0.11%; select nominal rate then format
  cost before exact-ratio ties so NV12 at29.97 wins over converted30fps.
  One raw-byte GPU upload handles padded/bottom-up RGB and packed422, including
  RGB24 at7680px without an R8 texture-width ceiling. CPU NV12 stays NV12 in one
  two-plane upload. Native DXGI samples never map/read back. The Windows10 MF
  MJPEG decoder outputs YUY2 or NV12, full range; prefer422 to avoid another
  chroma downsample. Black/white JPEG oracle verified decoded Y0..255 and GPU
  RGB0..255. WIC is no longer used by camera capture. SharedReadOnly retains
  the owner's mode; MemoryPreference staysAuto, notCpu (which could hide a GPU
  readback). Shared raw CPU bitmaps are supported; GPU-only FrameServer surfaces
  fail explicitly rather than downloading. Single fields/HDR and missing QPC
  timestamps remain explicit errors. No new repository test files were added.
  Real CLI10s, program+ISO, six clean full decodes: virtual600frames/file,
  Cam Link597 (3drops),4K S599 (1drop); no capture errors. Virtual's delivered
  image is the Camera Hub placeholder (~30newframes/s despite advertised60),
  4K S is black (no live-input qualification); Cam Link is live via sharedMJPG.
  Reported repeated_frames=0 does not mean source pictures are unique: the
  engine's scheduled output can reuse a slower source. Preserve both counts.
  Final native build/UI-inclusive strictClippy and raw-camera/pixel checks pass;
  portable250tests pass onexonbook, native90protocoltests pass. Final duplicate
  take could not acquire writer's GPU lock; earlier10s take receipts are valid
  for the preceding snapshot, before nominal-rate tie and MFT stream-change
  output-buffer reallocation fixes. Exact trees/receipts are in capture REPORT.

- 2026-10-19 (#105 startup/#13): WPR/ETW showed the compositor's eager
  pipeline creation costing847ms sampled CPU in the profiled startup.
  Windows now builds Renderer/Filters/Transitions concurrently but joins
  before returning; no lazy first-use compilation on recording ticks. UI
  pre-warms through a tiny existing shared_frames subscription before GPU
  creation, never a global Engine.hold. This can open capture before a window
  appears; app death leaves the ordinary15s idle grace, not a pin. GPU-init
  failure was not independently injected and may leave a launched daemon.
  Early-death qualification saw the engine then its removal after the grace;
  kill completion was observed within995ms, not a precise450ms kill timestamp.
  Scene-projector resizing during a take followed by app kill preserved
  3×300frames/10s, zero losses and full decode. Macro/source clocks unchanged.
  Five qualifying app-only cold-process pairs (default image mode, explicit
  --fps30, same1280×720 colour scene, no audio): first-positive observation
  median1.471s vs OBS32.1.2portable1.857s. Two native cases~1.58s. Warm OS/
  driver caches, fixed Isocast→OBS order, screenshot sampling not scanout;
  sampling intervals overlap, so no exact presentation-time ordering or
  before/after speedup claim. Seven pairs retained, two excluded by a
  predeclared compiler-contention check (including the fastest native sample).
  Cua0.34 list_windows(pid) does desktop-wide UIA before filtering and can
  add2s waits. Timing now discovers only owned HWNDs with Win32 in the existing
  Session1 controller, then uses Cua's exact-window capture-only path. No
  accessibility feature was disabled in the app. Independent fresh daemon
  guards start after the preparation daemon exits; reusing a nested job after
  its parent closes makes Windows reject new process assignment.
  Native tool receipts368ce056/ae6d6265 and runtimec953f789/e1af5e12 all use
  tree d17a4b5c; these are run IDs, not commit IDs. Clippy, release builds and
  portable250 tests pass; oracle7pass/2pending (stream warm-once and media/
  H264 scene-feed gates). Provenance, raw/validated failures, timing bounds
  and private screenshots stay in windows-capture/scratch/startup-evidence.

- 2026-10-18 (#105 W.6 shared previews): Windows uses owner-ACL random named
  DX12 allocations and authenticated one-shot offers, same login session and
  exact adapter LUID only. Register the rendering queue before opening a
  shared viewer; releases follow its native GPU fence. At most current plus
  one retired ring per viewer; resize coalesces while old frames remain held.
  Reconnect caches need process-unique ring identities, not wire generations.
  Real program/studio, source Properties (bars→bars2) and projectors verified
  on exon-desk; app kill leaves three 10s/300-frame files intact. Native
  6ea81dd3 checks/build, 86827de6 measurements/kill, e59f1f89 Properties;
  portable final gates250 pass including doctest, one existing hardware ignore.
  Oracle7 pass/2 pending: its B5 tests H.264 scene feeds/snapshots, NOT shared
  textures. Initial oracle API errors fixed by importing main04760a00; one
  initial ISO stop timeout did not reproduce in the final complete run.
  First visible picture upper bounds3.39s Isocast (daemon warm),5.73s OBS32.1.2
  (engine starts too), so not a matched cold-engine speed win. A later
  matched fresh-daemon+app / fresh-OBS pair (warm filesystem caches)
  observed pictures by4.98s/3.45s, with broad2.79–4.98s/3.08–3.45s sampling
  brackets: no precise startup delta or optimization claim. At720p30 one
  sample per stage: app+daemon CPU13.91→16.95→21.34%onecore for1/2/3 viewers;
  summed per-PID GPU-engine counters6.85→7.77→9.02%, not whole-device utilization.
  Program/source app-arrival medians36.25/3.10ms; not display/photon latency.
  Private receipts/screenshots in windows-capture/scratch; none committed.
  GUI qualification must issue API commands at the same integrity level:
  a medium-token daemon cannot authenticate the elevated SSH process handle.
  Cua Windows launch arguments are joined unquoted; interpreter expressions
  must be Windows-quoted or whitespace-free. Adopt a tagged supervisor while
  launch waits for its window. No security bypass or protected-process changes.

- 2026-10-18 (#105 capture/main integration): coordinator committed performance
  and orphaned audio probes as1b9fc578. Merged main565729db with --no-commit;
  leave the merge staged. Main's canvas/governor/retention behavior survives,
  with camera metadata and opt-in Windows pacing/scheduling added. Manifest
  calls now pass canvas_controls plus camera readers; masks and camera exports
  both retained. Main1b6761bb UI memory strategy wins: separate -j1 UI step,
  ISOCAST_UI_NO_GALLERY, no global jobs override; native platform build coverage
  retained. Portable gates248unit+1doc/onehardwareignore pass onexonbook;
  native release/all-target services build127942c4 passes, separate CI-config
  UI+test buildb7d48d82 passes (28m27s). Both compiled tree91ec7b7a. One LAN
  bootstrap stalled before any remote job/Cargo; only its owned SSH transport
  was cancelled, Tailscale retry passed. actionlint and merge-delta checks pass;
  seven docs/cli.md whitespace warnings are inherited unchanged from main.
  Read-only merge review has no blockers. Writer REPORT receives the budget
  planner/cache and shared completion APIs; daemon wiring remains writer-owned.

- 2026-10-18 (#110/#27 merge qualification): HEADbb8790a5 + main59f78393
  merged with `--no-commit`. Governor conflict keeps BOTH `frame budget`
  exclusion from ISO pin diagnosis and named-secondary `unpinned_secondary`;
  per-hub ownership and budget-full/5s-tail +30s-quiet restoration retained.
  Notes conflict keeps both histories. Remote gates249 pass, strict112811
 18/18 zero-loss once. Combined AWS112520:24 tiles saturated50s; lastbudget
  refusal53.65s, preview15→30 at88.20s/full118.61s, no early recovery. Healthy
  hub1500/failed4000 stay isolated, one VT refusal; three130s files7800/7800
  frames with equal A/V, no drops. Congested streams lose309/235 video pictures
  (audio intact), explicitly not a loss-free-upload claim. First combined
  runner failed due to missing proxy dictionary; corrected experiment only,
  no production patch/assertion relaxation. Scratch budget-merge/REPORT.md
  has hashes/results. Merge remains staged/uncommitted; no owned helpers left.

- 2026-10-18 (#110 remaining handoff): branch fast-forwarded to main6d6d58fd
  before edits; current work uncommitted. Mixed H26x overlay, per-hub refusal
  ownership and named secondary pins qualified onAWS. Gates243/strict18/18,
  stream3-pair A/B0loss. Real `memory_pressure -p33 -s1` (NOT `-S`) reached
 7.92–8.40GiB available and actual warninglevel2; three94.7s files2841frames
  each and stream2793frames lose nothing, preview fully restores59.91s after
  release. Helpers/watchdogs reaped; resumed inspection found no lane jobs
  or helpers left running. Report: self-tuning-2/scratch/remaining/REPORT.md.
  Regular reviewer unavailable>1h; Astra fallback review/re-review and final
  evidence acceptance have no blockers. Regular-model review before release
  remains required. Prior slices are committed6c4663ca/merged6d6d58fd, not
  pending approval despite the historical entries below.
- 2026-10-18 (#27 review fixes): 5d06bfef + main6d6d58fd merged without a
  commit (notes conflict keeps both histories). B1 auxiliary cache eviction
  happens before the next GPU batch and yields that admission turn, so the
  same first sixteen owners cannot continually reclaim every permit. Hidden
  auxiliary inputs and idle compositors release caches too; bindings/reuse
  state are cleared, older GPU submissions keep their own input leases.
  B2 notices/counters are copied under the ledger mutex and reported through
  bounded deferred logging outside it; status JSON is built outside it too.
  B3 full-budget occupancy and the five-second refusal/release tail both
  block the governor's 30-second recovery timer (memory pressure kept).
  Ordinary clones now share a domain claim; independent deliveries still
  deduplicate. Final evidence: frame-budget/scratch/REPORT.md and fix1/.
- 2026-10-18 (#27 review verification): matched native pixel run103710,
  24 staggered masked tiles:23 frozen before. Final130s run110027 keeps all
  24 subscriptions open through50s saturation (8–10 changes/33s off air),
  then12 open through recovery (34 changes/35s); program/ISO7800/7800.
  Disable client read timeouts when testing deliberately shed frames: the
  early5s client timeout could close a stream while JPEG demand continued.
  The3s looping-counter experiment
  aliased with the new~3s update cadence; use test_pattern's six-digit counter
  for freshness, not repeated clip numbers or frame timestamps. Log-writer
  delay102500→103117:299→0 program drops. Pressure50s: no false restoration;
  last refusal53.66s, restore89.09s then118.48s, remaining12 viewers continue.
  Stress103319:zero protected/audio loss. A/B103220:CPU42.3→42.1%, instructions
  494.8→493.9M/s, footprint140.4→133.1MB. Gates247 unit +doctest, strict104755
  18/18, all pass. Binary8e6501b8. Main6d6d58fd is merged without commit.
  B2's isolated `ISOCAST_BUDGET_LOG_STALL_MS=AFTER_MS:DURATION_MS` probe is
  capped at10s, once, developer-only; markers go to native-relay JSONL in
  d.logs, not daemon.log. Original native-priority/device limits remain open.

- 2026-10-18 (#27, frame-budget lane): replacement design is per-origin,
  distinct-buffer accounting, not delivery permits. Independent reserves are
  8 producer, 32 file-recording, 16 auxiliary and 36 protected-compositor
  pictures; the CV pool's physical ceiling is their sum (92), allocated on
  demand. The composition reserve is (GPU in-flight limit + one cached input)
  × (main + MAX_EXTRA_CANVASES), not another arbitrary queue size. A buffer
  shared across domains counts once in the physical union; repeat deliveries within a
  domain do not spend another permit. Native VT callbacks retain the same
  lease, as do GPU inputs/outputs through their existing completion fences.
  Streams/replay, previews, scene tiles, source-mask caches and virtual-camera
  reads use the auxiliary reserve; recorded-canvas inputs use composition,
  and files/ISOs use recording. This fourth domain was added during audit:
  a stalled passthrough ISO can fill the file reserve, but must not freeze
  the program reading that same source. Auxiliary refusal feeds the governor
  without asking it to thin ISOs. This protects against other domains, not a stalled protected
  writer exceeding its own reserve or the machine's physical encode capacity.
- 2026-10-18 (#27 design decisions): copy scarce SCK/AVCapture buffers once
  at ingress, preserving format/bit depth/attachments, rather than letting
  many outputs retain an eight-surface device pool. Decoder frames remain
  zero-copy and have a per-reader ledger keyed by native CV buffer identity:
  two samples aliasing the same native buffer share a charge too, not merely
  Rust clones of one wrapper. AVAssetWriter's opaque lifetime is
  isolated in a private threshold-limited copy pool and charged at its full
  reservation, never reported as an invented exact native count. The camera
  helper likewise copies before acknowledging enqueue: CMIO retains the
  helper's pool, never the parent's recycled pixels. Shared IOSurface rings
  are counted separately (including texture wrappers and Mach offers), with
  their existing per-ring limit. This is a frame-pool invariant, not a
  machine-wide RAM allocator or a count of Apple's private codec allocations.
  Status/doctor expose the ledger and sticky refusal/allocation-failure totals.
  Windows' platform hook remains explicitly unavailable, not a false pass.
- 2026-10-18 (#27 diagnostics): isolated developer mode accepts
  `ISOCAST_FRAME_STALL=iso|live`: hold one real consumer worker for 600 ms,
  once, after two seconds. It does not stop VT callbacks or the daemon.
  Normal mode ignores it. Use it to distinguish file-queue retention from
  capture freshness; numbered media checks detect a frozen input even if
  a program file continues storing repeated pictures at its nominal fps.
- 2026-10-18 (#27 device qualification): M2-a's existing signed development
  app reports screen recording not granted, and `virtual_camera_start` says
  the camera is not installed/approved. No permission, setting, or install
  was changed. The fallback AVAssetWriter path did qualify there: program
  and two ISOs, each 360/360 at 60 fps, zero audio/video loss, full decode.
  The saved `command` in hw-queue/done/20261018-090156-frame-budget-cmd.json
  is the exact permission-aware retry through `hw-batch.py now cmd --gui`.
  Pixel-copy format/plane tests are not a claim of real capture/CMIO success.
- 2026-10-18 (#27 final verification, a16084a6 daemon): failing 24 s AWS
  workload (six ISOs, three extra recorded canvases, stream/replay, masks,
  40 multiview tiles, 1,320 cuts) loses 461 program frames on main; new
  records every frame in all ten files and the stream/replay. All 45 budget
  snapshots hold; refusals [0,0,1157,0], auxiliary peak16. Governor lowers
  previews60→30→15. Final gates244 unit tests + one doctest, strict18/18.
  Flagship-1080 A/B3 pairs: CPU41.6→42.4% (+0.8 points), instructions+0.5%,
  GPU86.2→86.0ms/s, footprint125.6→124.7MB; zero media loss on both.
- 2026-10-18 (#27 remaining native-policy limit): a600ms stopped ISO worker
  fills its32-frame reserve (5 refused), but the program stays480/480 with
  every numbered picture advancing. Its own ISO loses22 frames, accounted.
  A600ms live-worker stall leaves every frame-budget domain unexhausted yet
  loses51 ISO frames (program480/480, all audio intact): stream.rs includes
  the hold in native submit latency/health, setting native priority pressure;
  crash_safe::append_frame then refuses ISO before budget admission. This
  priority contract is unchanged from main and separate from pool starvation.
  After three diagnostic attempts, stopped expanding the fix: the wrong
  assumption was that a native-consumer stall tests only buffer retention.
  Keep #27 open for the stronger requested no-ISO-loss contract; reconcile
  native stream-vs-ISO priority with #110 rather than tuning pool caps again.
  Evidence/retry commands: hw-queue/done/20261018-092625-frame-budget-cmd.json,
  frame-budget/scratch/consumer-stalls/stalls.json and REPORT.md. No assertion
  was relaxed to hide this failure; no native policy change was made.

- 2026-10-18 (#110 self-tuning-2 handoff): uncommitted on063a21a5. Gates242
  pass; the full strict suite ran once under CPU4+Metal+2GiB resident memory,
  18/18 zero-loss. M2-b's three interleaved flagship1080 A/B pairs lose no
  frames/audio (daemon CPU median9.8% both); a288 s strict30fps soak has21
  decoded files,8436 frames per role, zero losses/repeats,8601 stream frames
  and both crash checks pass. These M2s have24GB, not8GB. Actual kernel
  memory-pressure samples stayed normal; scoped per-daemon injection separately
  proves warning/critical, pin/ISO-consent and30s-step recovery. No new M5 run.
  Scratch REPORT.md lists every job, hashes and failures honestly. Formal
  Tier-1 review failed before reading code three times (Endpoint unavailable);
  no approval, commit or GitHub write. T.1b.1/T.3a/T.4a are ready for review,
  not completion of full T.1b/T.3/T.4 or #110.
- 2026-10-18 (#110 qualification): four calibrations take22.26–22.86s,
  including outside CPU load. Every supported ProRes variant, live CBR H.264,
  UHD60 HEVC10 and three full fit cells are covered;19–21/24 cells total,
  skipped diagnostics explicit. Multi-canvas and low-end fitting files lose
  nothing. M2 overloaded ISO loss44.39–53.67% vs measured53.09%/table52.62%:
  no improved percentage-accuracy claim. New-only init/blank profiles choose30;
  explicit/copy60, pins and no ISO consent survive restarts. Late-canvas floor
  now joins at25%; native rollback restores real properties even when numbers
  equal the last acknowledgement. Same-canvas multi-hub refusal remains
  canvas-wide (verified one refusal, sibling restored, separate canvas adapts).

- 2026-10-18 (#110 review fixes, current): uncommitted on7705e90. The
  governor NEVER thins ISO implicitly: `governor.allow_iso_thinning` is false
  by default; explicit opt-in permits it, pins still override it, and opt-out
  cancels a running hold. Existing native safety may still drop ISO pictures.
  Settings exposes the choice; Optimize/setup-check mention it without
  enabling it or assuming it makes the workload fit. Owner-local native
  intentional-refusal counters prevent false losses after vertical recovery;
  preview retune refusals are nonfatal with per-feed error lifetime.
- 2026-10-18 (#110 fix evidence): real pre-fix035640 reproduces vertical
  pin→false ISO hold and two disconnected previews under injected VT-property
  refusal. Rejected partial fix041634 proved native admission attribution
  was also needed. After043111 pin/natural restoration and down/up refusals
  pass; consent043746 and corrected Optimize/choice043938 pass. Idle CPU
  medians0.02589%→0.00988% of one core; disabled0.00939%, overlapping noise.
  Final gates240 pass once; strict05573918/18 passes once, zero reported
  media loss. Report has the fix-by-fix evidence table and known F2 limits.
- 2026-10-18 (#110 storage): preserved current binaries on M2-b at
  `~/isocast-scratch/machine-profile-preserved/review-candidate` (daemon
  SHA25688ed7bea5fa07ef2), baseline in its sibling`baseline-7705`. Old,
  completed synthetic directories `/tmp/govab-*`, `mpmerge-spke3bm4`,
  `govfix-ew539qol`, `govfix-p27wualk` were checksum-relocated to the lane's
  local `scratch/retained-m2-b/<name>/`, not discarded. This freed enough
  M2-b space for gates without lowering its40 GiB guard. Never confuse
  hw-batch's standard pushed old bin-dir with these explicitly executed
  remote candidate binaries; strict's `binaries.json` verifies them.

- 2026-10-18 (#110 earlier T.2 handoff): ca4cc010 contains the first slice.
  Main257ea057 (Windows plumbing, GPU lifetime and masks115) was merged as
  eaf0b6a7; its results were committed as7705e90c by the delegator.
  Shared-frame conflict keeps monitor cap AND new max-size argument; ISO
  keeps governor pin AND GPU retry/presentation-time path. Daemon governor
  now uses main's NativePlatform alias. Merged gates240 pass; strict e2e18/18
  and seven native media contracts pass; job20261018-032243 verifies animated
  program/filtered-ISO masks, split during emergency hold, native readback
  and full recovery. Binaries stay onM2-b in ~/isocast-builds/machine-profile/
  target/release (local shared volume only4.8 GiB free; no binary/movie fetch).
- 2026-10-18 (#110 evidence limits):162024/195658/195715 were interrupted
  before hardware, not governor failures. Detached201445/201446/201447-cmd/
  e2e all passed. CPU/GPU contention was loss-free with zero policy steps.
  Severe foreign4×UHD60 demands: main program1498 video/1695 audio dropouts,
  governor1329/1623; a single pair is not repeatable improvement. ISO audio
  stayed whole but native admission held ISO pictures for about121 s: only
  the governor's extra overlay is bounded to3 s, not physical starvation.
  Six policy changes/no cycling; native share recovered25.6 s and preview
  61.1 s after foreign load. Custom-job PASS proves decode/accounting, not
  zero drops. Independent merge-review tool failed twice (endpoint unavailable);
  coordinator Tier-1 review remains. Full evidence/limits: lane scratch REPORT.md.

- 2026-10-17 (#110 T.2 history): user approved initial exonbook baseline
  while retaining AWS qualification. Runtime slice later committed ca4cc010: preview,
  ISO/vertical admission and shared-stream bitrate controls, profile pins/off,
  status/events/UI. A zero-ISO limit held for30+ seconds failed real A/B and
  was replaced by a non-renewable3 s emergency overlay, rearmed after5 quiet
  seconds. Native ISO_SHARE remains the only encoder-sharing controller.
  Network shedding is not production loss and never triggers ISO blanking.
  Exonbook strict e2e18/18 and233 tests pass;600 s fitting workload has zero
  steps/losses. Real CBR/replay6000→1500→6000 verified; severe encoders can
  still exceed protected capacity. Ten-minute overloaded run has four changes
  without cycling, but preview cannot restore until remaining load fits.
  Initial AWS runners were lost; detached replacements and their results are
  described above. Report/evidence under the lane's scratch folder.

- 2026-10-17 (#110 F4/F8): `setup check` uses only a reachable existing
  daemon; otherwise its capacity check says "not checked" (validation still
  succeeds/fails locally). Automatic calibration also defers while the live
  engine previews/starts. `ensure_engine` holds the start gate and cancels
  only an automatic helper before source startup; manual calibration remains
  available during preview. Exonbook job 20261017-161756-machine-profile-cmd:
  both offline cases and preview/manual/preemption cases pass, 90/90 frames
  and 3.000 s A/V. Included in ca4cc010; original gates233 pass.
- 2026-10-17 (#110 T.2 scheduling): the 0.0.7 release soak
  `20261017-160535-delegator-soak` owns **both** AWS hardware locks for six
  hours: m2-a records, m2-b receives. `hw-batch now --mac any` says "no test
  Mac reachable" when both locks are held, even though SSH is reachable.
  No lock was bypassed. Required competitors-only baseline on main 24bea09d
  was queued as `20261017-162024-machine-profile-cmd` (later interrupted). The user
  subsequently approved exonbook as the initial baseline; implementation
  continued without waiving AWS qualification. The detached replacements
  completed overnight; no jobs remain waiting. Report/integration map are in
  `/Volumes/IsocastBuild/machine-profile/scratch/REPORT.md`.

- 2026-10-18 (#105 final capture performance handoff): all scoped jobs ended,
  work remains uncommitted. ReadyforCI API slices #1a/#3/#6a/#7a/#11a/#12;
  not whole daemon integration/OBS parity. DDA's nominal1ms pending Acquire
  wait measured p9916.071ms. Highres wait + zero-time latest refresh improves
  CPU6.635→5.699%onecore and update-agep9916.35→2.879ms; matched WGCAboveNormal
  remains4.685%CPU and DDA still23longgaps. Actual pixels,16GPUcursorcases,
  pressure/reopen/idle-final-update/two-change-burst and final re-review pass;
  source/strictClippy receipts in DUPLICATION-REPORT. Both workersAboveNormal.
  No default DDA switch; dedicated helper only, cursor=false/window usesWGC.
  Aggregate budget/split/live identity and calibration-cache integration remain
  writer-owned; preserve its four-slotf1264f87 rather than overwrite with this
  branch's qualified8-native/16-total policy. Sub1ms and Win11power gates open.
  Native/portable checks pass as scoped; final optional62688d6d check never
  ran because writer heldGPUlock. Allown child markers/probes/guards closed;
  foreignGPUlock left untouched. Localvolume17.808GiB despite removingonly
  own local ETLduplicates after freshremote/local hashes match. Windows
  originals retained; no local builds/large fetches/foreign cleanup. See
  capture scratch REPORT handoff index and owned-only patches; unowned audio
  probes/exports still byte-identical to inherited snapshot.

- 2026-10-18 (#105 performance qualification): measured P1/P3/P5 capacity
  on 5070/617.42 is H2641102.879/846.201/347.689, HEVC865.599/617.507/622.934
  completed1080p-equivalent fps (one short workload, not universal). The HEVC
  inversion exposed our unsupported monotonic validation rule; removed after
  review, preserved exact labelled data, and skip ISO downgrades that increase
  load. Native Clippy/API checks655fa805 pass: measured H2643/3/3 load0.638146,
  HEVC5/1/3 load0.759703. No automatic daemon cache/integration installed.
  Real Wave XLR48k scheduling AB/BA14e4908e: first normal steady loses2304
  frames versus MMCSS0; second steady pair ties0 (normal warm-up loses1824).
  Process remainsNormal, only own threads pinned, no PCM stored; WPR0lost.
  Stronger WASAPI061de49d invokes actual registered disconnect callback from
  main MTA with worker idle250ms/0packets: callback alone wakes infinite wait,
  stopped once4.433ms, reopen/no-late-callback pass. Earlier worker-side
  injection proved propagation, not that isolated cross-thread wake alone.
  DDA functional pixel/cursor/final-idle-update tests and re-review pass, but
  initial matched CPU4.609→6.635%onecore/gapp9917→33ms regresses; one bounded
  attribution batch remains, no default replacement/performance-win claim.
  Local disk hit19.730GiB; removed only hash-verified duplicate local ETLs,
  retained Windows originals, restored21.740GiB. New large traces stay remote.

- 2026-10-18 (#105 performance continuation): probe_microphone/probe_loopback
  and their exports were inherited uncommitted, not authored by this session
  or the writer lane. Preserve them; owned #12 event/disconnection patch is
  separately retained as capture/scratch/audio-event-only.patch against
  fa061065, excluding all probe edits. Final WASAPIWPR e52faa8e proves idle
  capturethread4.02wakeups/s→0; active~100.8/s unchanged,720000frames/15s,
  no gaps/timestamp errors. Match ETW byPID+TID (TIDs reuse).32audio/video
  schedulingcycles174handles; own contention288/600misses→0. Powerpolicy
  GetProcessInformation returns87 onWin10 evenwithdocumentedversion/size;
  guard leavespolicyuntouched instead of inventingpriorstate (Win11gate).
  Nativehighrestimer p99~1.05ms, no<1ms/CPUclaim. Fullportablegates nowpass
  fmt/strictalltargets/233unit+1doc onm2-b, onehardwareignore; initialhost
  reservations/diskfloors and thenoneformattingfailure were resolved, not
  bypassed. NVENCcompletion review found/fixed registration afterfatalexit;
  deterministiccounterfactual reproducesoldrace, fixedrejects,12sessionsone
  worker and32parallelreopens pass. DDAWIP reviewfinding(finalupdate loss
  underthrottlethenidle) is being fixed before anyreadyclaim. Reports in
  capture scratch; traces private, no other apps/process settings touched.
  Further#7diagnostic5c57b692: AboveNormalvideo p991005us vsbalancedownprocess
  timeBeginPeriod1 p991024us, both0misses. Threeprecisionapproachesdidnot
  establishsub1ms; stoptweakingtimer, no timerperiodrequestshipped. Native
  actualMediaClockopt-in testb503a2a6 passesfirsttickselection, diagnostics,
  StopandnoPROCESS_STRETCHEDcontamination. Nativecombinedfmt/Clippy/165units
  pass e38e49aa;900sbudgetendedatlastdocsummary, protocol-doc rerun3ec2ac00
  passes. #3/#12readyforCI scopednativechanges; notautomaticengineadmission
  orWin11powerqualification.
  #7mechanismcorrection: actualexon-desk rustc1.99.1(58cf8f9c3) stdsleep
  alreadyusesCreateWaitableTimerExW(HIGH_RESOLUTION), closesiteverycall.
  OurTLShelper removesper-waitCreate/Close andmakesQPCdeadlinesexplicit;
  it doesNOTremoveanexistingIsocastspinloop. Officialcommit-sourcechecked,
  consistentwithordinary/explicit~samejitter. Don'trepeatOBSspinsavingsas
  anIsocastbefore/afterclaim.

- 2026-10-18 (#105 performance#1): exactprogram4K60+screenISO4K60+cameraISO
  1080p60 confirmsH264P5NVENCsaturation, NOT blanketHEVCP5failure.30swarm/
  60ssteady: H264P5=6938/10800pictures,625deadline misses+3237refusals,
  NVENC100%; P3all=10800/10800,0losses,NVENC76.3%. HEVCP5allalreadyfits
  10800/10800at88.05%; programP5/screenP1/cameraP3alsofitsat78.80%.
  CPUonecore rises8.72→17.68%H264 and12.83→15.96%HEVC; noCPU/OBSwin claim.
  WPRCPU+GPU0lostevents+NVML, ownedguardsclose;3f27bf57summarizesreceipts.
  OfficialSDK13HEVCP3/LL=529fps, NOT HQ947. Addednative explicitpreset and
  driver-keyed aggregatebudgetAPI, but enginepre-open call/calibrationcache
  still writerintegration. Reference estimates remain uncalibrated. Reject
  planningwithactiveNVENCsessions until their workload is accounted;12-output
  plannerceiling is applicationpolicy, not theSDK's outdated8-sessionclaim.
  StrictnativeClippy, plan/open/rejection checks,49files1470decodedframes,
  fallbackAstrareview/re-review pass. Audio probeedits/export left untouched.

- 2026-10-18 (#105, main merge ready for CI): coordinator committed native
  recording as5e2d496b after78a04a8b. Merged main1dbb44c9 with--no-commit,
  preserving nativeWindows paths alongside canvases, FrameBudget, self-tuning,
  masks and followups. PendingMERGE_HEAD retained; no unmergedentries.
  Main's1b6761bb split/no-gallery WindowsUI CI memory fix retained; only
  additionalservices package is platform-windows so its capturehelper builds.
  Macgates onexonbook: fmt, fullstrictall-targetClippy,249unit+1doctest pass,
  oneexisting hardwareignore. Windowsreleaseall-targetcheckplatform/daemon/
  CLI/contracts passesdc58f10e; source treedd55bde4. No newWindowsruntimeclaim.
  Ordinaryreview endpoint unavailable; independentfallback merge audit found
  noblocker. Evidence: windows/scratch/main-merge-mac-gates.log andREPORT.md.
  New work remainsuncommitted. Nextuserordered audio proof uses a short quiet
  realtone, no defaultdevice/globalvolume changes; OBSbenchmark omitscamera.
  User now explicitly requests immediateStopack withworker finalization:
  completion/durability must remain separately and honestly observable.

- 2026-10-18 (#105, ready for CI: Windows daemon recording): pendingmerge
  fa061065 now includes sharedCamLink capture inWindowsPlatform andrealdaemon
  selection. Initial/split rawISO writers choose actualFrame.pixel_format;
  program/staged outputs stayBGRA. Siblingisocast-capture remainsmandatory.
  CLI900frames/3files andfinalapp1005frames/3files passfullA/Vdecode/zero
  losses, actualRecord/Stop/Exit actions; allownedprocessesexit0. Due-PTS
  helperpublication fixesWGC's~26msfuturepictures evicting eligiblehistory
  (screenmissing600→0), without timestamp rebasing orqueuegrowth. Windows
  backgroundlaunch noactivation andpost-cleanup selftermination fix UIexit
  statusoverride2170; exactnativecallbackresponsible remainsunidentified.
  Killproof preserved179video frames perfile plus139/139/130AACpackets;
  actualstartup repair andsecondstartup byte-idempotence pass. Workspace
  migration creates a setup directory, not necessarilylegacysetup.toml.
  Native190tests, fullstrictClippy andMac233unit+1doctest pass. Ordinarymerge
  review passed; helperPTS/Stopwake/UIexit have fallbackread-onlyaudits after
  repeatedordinaryreviewendpointfailures. Evidenceinwindows/scratch/REPORT.md,
  w6-ui/FINAL-DISTRIBUTION-PROOF.md, w6-kill/verified, w6-daemon-cli.
  Allchangesuncommitted, HEADaf594c6f/MERGE_HEADfa061065; no unmergedentries.

- 2026-10-18 (#105 performance limits, not a success claim): actualheadless
  daemon+WGChelper20s measurementswithscreen4K+CamLink1080+mic:1080program
  58.90%onecore/RSS403.25MiB/private1050.64MiB;4Kprogram56.85%/481.91/1291.43.
  Each25stake4,500pictures/no losses/fullDecode. These measurement snapshots
  precede finalPTS/Stop/UI fixes; they are not finalapp/OBScomparisons. OBS
  cannot open the busyCamLink throughDirectShow evenatobservedMJPG1080p60,
  error0x800705AA; protectedbridgePID30224 unchanged. Never substituteold
  two-window benchmark as matchedrealcamera proof. Micendpoint supplies
  silence; noaudible-inputclaim. Sharedpreview/JPEG/playback stillunsupported.
  Stopcoordinator cut0.06–0.08ms now, butcompleteRPC53.8–134.5ms: required
  inputtail, finalmuxsync andcleanup remain. No sync/acceptedmedia removed,
  no earlyack;2→1fsync orbackgroundfinalization would require separate
  recovery/durability design qualification. TemporaryW6STOP/W6ISO/W6OPEN
  instrumentation is archived andremoved; writer.rs exactlymatchesHEAD.
  Foregroundnativecommands avoid an observed backgroundSSH stdin stall before
  worker/watchdog creation. Three suchowned jobs weredisarmed/closed, not
  left holdinglocks. ProofcontrolRPCusesSession1; Session0→Session1 returned233
  without weakeningIPCauth. Do not touchotherlane worktrees orunownedprobes.

- 2026-10-18 (#105, earlier daemon checkpoint; superseded above): deliberately merged
  committed fa061065 into af594c6f without committing; MERGE_HEAD retained,
  conflicts resolved. No uncommitted capture-lane performance/audio edits
  imported. Windows daemon now uses one lazy WindowsPlatform; camera discovery,
  capture and delivery metadata are wired. Raw ISO writers select their input
  from an actual Frame, including splits; composited/staged outputs stay BGRA.
  Mac defaults are unchanged. Real `isocast record` screen4K+CamLink1080+mic
  passes three5s files/900frames/full A/V decode/zero losses (58checks), receipt
  80ad68f8. Defaultmic is mono silence, not audible-input proof. Camera is
  shared FrameServer MJPG CPU-decode/upload, explicitly in the manifest.
  Native full four-bin build and all-target check pass; Mac strictClippy and
  233unit+1doctest pass (one existing hardware ignore). Real app clicks,
  committed-prefix kill/recovery and recorder+helper performance are active.
  Preview/JPEG remain unsupported; record controls are independent of preview.
  Sources/binary hashes: windows/scratch/w6-daemon-cli/first-proof. New changes
  remain uncommitted; Windows CI runner-memory work belongs to its helper.
  The first real app launch exposed Windows background mode as a no-op and
  took focus; no Record click was issued. A Windows-only winit initial
  with_active(false) hook is being rebuilt/qualified before the retry, leaving
  ordinary launches and macOS unchanged. NativeClippy for daemon/CLI/UI and
  platform/contracts passes2eb54726; actual end-to-end proofs remain decisive.

- 2026-10-18 (#105, ready for CI: WGC helper isolation and lifetime): production
  WindowsPlatform now resolves isocast-capture.exe beside its executable and
  uses a job-owned helper per capture. No fallback to in-process WGC; raw API
  remains for the helper/diagnostics. Child publishes only completed GPU copies;
  parent pulls NT handles, checks fences, and returns a lease only after the
  last actual GPU consumer releases its Frame. No added pixel copy/readback.
  Both bounded identity caches retire old-size references on geometry changes.
  Twenty reopens keep parentALPC at5; helper kill/reopen, parent kill with a
  separately surviving source, retained post-exit encoding,32-frame blockedGPU
  pressure/recovery and640→800→640resize pass. Engine9A/Vfiles/1080frames and
  4K60HEVC program+ISO/AAC25s/3000frames fully decode, zero reported losses.
  Native strictClippy/81unit, Mac233unit+1doctest/oneexistingignore, non-Windows
  helper check and ordinary review pass. Source treeb5f3c245, final helperSHA
  ca6499b2cfd64c4dba945bf3e51270fc62a6df0e09b17090a0dc253494269487.
  Evidence/source archive: windows/scratch/w5-isolation/verified; fullreceipt
  list in REPORT.md. New work remains uncommitted; f1264f87 already contains
  initial wire/process scaffolding, so include their final modified versions.
  Default windows-remote build/fetch includes the helper; packaging must too.
  Daemon remainsstub for the next slice. Maxobserved capture shutdown33.05ms
  is not the <20ms recording-stop target; totalCPU/RSS must include helpers.

- 2026-10-18 (#105, measured NVENC memory reduction): NVENC's private input,
  output and completion ring is now four slots, down from eight. Rust and C++
  bounds match; P5, QP22, spatial AQ8, B0/lookahead0 and the exclusive private
  GPU copy are unchanged. Four is NVIDIA's documented asynchronous CPU-output
  minimum of 4+B; MF remains at eight. Three production 25-second takes each
  delivered 3,000/3,000 pictures (4K60 HEVC program+one4KISO, native AAC), full
  decode and zero reported losses. Matched post-Hyper-V OBS comparison uses
  the same external source generator, 997Hz WASAPI stimulus and no preview:
  native median6.172% of one core versus OBS17.734%. Private memory fell from
  ~1,503 to~1,085MiB, below OBS~1,160MiB; RSS~368MiB still exceeds OBS~349MiB.
  This is recorder-only, not a full-app,1080p,startup/stop or all-output gate.
  Moderate CUDA contention (median25%,peak54% GPU busy) retained1,200/1,200
  encoder pictures; polled receive maximum30.55ms, below four60Hz periods.
  Single4K H264/P5 remains~88.9fps: fewer slots do NOT fix its aggregate
  throughput limit. Native strictClippy,81unit tests, seven native contracts
  and1080p/HEVC8/10 writer regression pass; capture contract stays unavailable.
  Ordinary review found no code blocker; its follow-up service failed twice.
  Sources/hashes: windows/scratch/w5-native-steady/results, w5-ring-latency.
  Coordinator committed this ring change asf1264f87. WGC ring and portable compositor allocation
  experiments did not help and were restored exactly toHEAD.

- 2026-10-18 (#105, capture lifetime gate): retention is per Win10 capture
  method, not frame count or worker lifetime: IsSupported,CreateForWindow and
  CreateForMonitor each retain one ALPC port/call. Factory lookup/resolution,
  frame pools, handlers and D3D wrappers do not; held agile factories, reused
  workers, fresh MTA/STA threads and explicit wrapper Close do not fix it.
  Real USER32-pumped STA and DispatcherQueue (including completed asynchronous
  shutdown), and supported RPC idle cleanup with120s idle, also fail. Keep the
  process MTA pin: removing it previously unloaded a cached factory's vtable.
  The user approved GPU-shared WGC process isolation IF allocation-stack
  tracing confirms runtime-owned retention. The trace now joins40exact ports
  to GraphicsCapture→COM→RPCRT4!AlpcConnect, with closure only atObKillProcess;
  all frames resolve, no third-party image was observed,0lostevents. This
  identifies the runtime path, not the precise source-level ownership defect.
  Helper qualification is complete above; no daemon platform switch. Exact controls are
  retained under w5-lifetime-*, w5-rpc-cleanup and w5-pump-control. Exon-desk
  rebooted separately for approved Hyper-V at11:57UTC; pre/post-reboot
  performance epochs are kept separate. OS19045.6466, NVIDIA617.42 unchanged.
  After isolation, sum recorder+helpers in CPU/memory measurements rather
  than reusing the earlier recorder-only comparison as a post-isolation win.

- 2026-10-18 (#105, after coordinator commits4a2063e8/f175c172): final
  branch Mac gates now PASS on m2-b: strict all-target Clippy,233 unit tests,
  one doctest; one existing hardware ignore. Log:
  windows/scratch/w4-final-mac-gates.log. Build-space blocker is resolved.
  The4K investigation now belongs to this lane. WPR reproduces the exact
  retained3×4K H264/P5 binary at10.174s for5s media,zero lost trace events.
  Bounded native-call timing accounts9.223s inside nvEncEncodePicture;
  copy/flush/map total114ms. NVML encoder utilization100%; independent
  NV12/P5 FFmpeg control produces900pictures in10.396s(~86.6fps), not an
  app/OBS comparison. Two4K60 H264 outputs exceed this measured capacity.
  Actual Engine H264 program+ISO dropped114/28frames; explicit existing
  HEVC/P5 comparison delivered600/600frames withzero countedlosses and
  full decode. Codec outcomes are separate; no hidden preset/queue changes.
  Temporary C++ timing hooks were removed byte-for-byte back toHEAD and
  retained as scratch/w5-etw/native/nvenc-timing.cpp +timing.patch. ETLs stay
  underC:\isocast-dev scratch; exactprobes/drivers/hashes are retained.

- 2026-10-18 (#105 W.4a, real WGC engine): productionWindowsPlatform
  app/titlewindowresolution+actualEnginecompositor+nativequality/MF AAC+
  WASAPIpasses3takes9A/Vfiles1080frames(120/2s each),full decode/program-ISO
  pixelprovenance/997Hzsound. OwnSession1windows only,kill-on-closeworker
  adoptionbeforeGUI/GPU,cleanupverified. Nativephysicaldisplay/window/mic
  discoveryusesstablemonitorinterfaceIDsandexplicitUnknownconsent(noquery
  canauthoritativelyestablishunpackageddesktopgrants). Requestreturnsreport,
  neverfakeprompt; explicitcaptureattemptsstillallowed. Native/webUnknown
  UIneutral,nopromptclaim; initialreportmustcalltake()toinitializehidden
  Generalstate.32discoverycycleshandles151constant;mainprobe3840x2160.
  Stableinventorydoesnotgateprimaryprobing(mirrormodeidentitycanbeambiguous).
  Actualmirroredtopology/visualGeneralviewunverified. Daemonstillstub; native
  media7/7isNOTfullparity/sharedpreview/camera/4Kgateapproval. Exactsources,
  binarySHA/tree andreceipts: windows/scratch/w4-wgc-engine/result and
  w4-discovery/result. Strictnative+UIClippypassese5e12e49;178nativeunit
  tests passb8486a83afterboundedwarmresumeofthecoldUIcompile. Finalfull
  contractprofilee1fae5f7:7passed/captureunavailable,notfullnativeparity.

- 2026-10-18 (#105 W.4a, production A/V): WindowsPlatform has real BGRA
  output pools/importer targets, compositor/filteredISO path, native quality
  writer, MF AAC and WASAPI. Production media7/7passes WITHOUT testpolicy
  override; actualEngine+loopback3takes9files540pictures+AAC all2s/full-decode.
  NativeAACdelay1024isexplicitpreroll; its2–3blockbacklogisnotpriming.
  Commitof1sinputtruthfullyacknowledges0.938666666s; realkillpreservesall
  committedpackets andrepairisbyte-idempotent.20AACedgecases243checkspass,
  including audio-onlycancel/drop andinvalidinputprefix preservation.64x48
  edgevideo useddisclosedsoftwareMFfallback(belowNVENCminimum), notGPUclaim.
  Nativeowner snapshots: AACdc913612, audio46ea8879 fromtreeee72b90c,
  recordingencoder/nativebackendsfrom35afc639. Do notcopytheirworktreewholesale.
  Allsources/hash/receiptsinwindows/scratch/REPORT.md andw4-*; uncommitted.
  DaemonstillStubPlatform. Capturelane nowownsNV12 INPUT extensions basedon
  ourqualifiedBGRAframes/gpu/output; do noteditthosefilesconcurrently.

- 2026-10-18 (#105 W.4a): first real video-only native program+2ISO take
  now passes1080p60:300frames/5s each, ffprobe/full decode/pixel provenance,
  real commit watermarks. The program compositor is explicitly a GPU-copy
  stand-in, NOT engine wgpu/Platform. Forced kill preserves150frames/2.5s in
  all3files; same mux repair finishes them and is byte-idempotent. Actual
  Cua-launched Session1 process is adopted into a kill-on-close Job Object
  before a marker permits ANY GPU/window work; the unadopted process exits
  after20s. Sources/driver/tree/rlib/exe hashes and results retained under
  windows/scratch/w4-recording/run-* and matching C:\isocast-dev scratch.
  ThelaterproductionA/Vproofabove supersedesthestand-in'sintegrationlimits,
  notits1080/4K performance measurements; no productiondaemon/backend switch.

- 2026-10-18 (#105, Windows lock follow-up): user found/killed stale
  exon-desk-ps PID4944 holding gpu.lock for8h; the capture lane's active UI
  diagnostic build was not killed or bypassed. windows-remote.sh now gives
  lock acquisition, sync and execution one deadline, releases both locks in
  nested finally blocks before result publication, and has a timeout+30s
  watchdog that kills its own worker tree if stdout/I/O blocks. Holder JSON
  (PID, lane, UTC start/deadline, process creation time, job/state) follows
  locked byte0; inspect from byte1. A waiting contender never overwrites it.
  Real non-GPU proof: old worker still held a lock after8s with a2s timeout
  when stdout was blocked; candidate killed itself at32.245s and the lock
  was immediately reacquired. Normal exit, exit17, ordinary timeout124,
  result-publication failure and contended acquisition all release/preserve
  ownership correctly. Exact driver/helper sources and tree hashes retained
  under windows/scratch/w4-lock and Windows scratch/w4-lock-*.

- 2026-10-18 (#105 W.4a, agent/windows): GPU fixea67be57 is committed
  separately. This lane has a pending, conflict-resolved merge of capture
  lane5d05c5ce (including its native crate/CI changes); DO NOT discard it or
  treat all ~10k inherited native lines as newly authored adapter code.
  New work remains uncommitted: video-only writer.rs, read-only system.rs,
  mux reservation/empty cancellation and their wiring. Windows release build
  and strict scoped clippy pass;81 engine tests and12 verifier tests pass.
  Native cancellation and system/MMCSS evidence is in
  /Volumes/IsocastBuild/windows/scratch/native/runs/. Actual WGC writer
  qualification was blocked acquiring the shared gpu.lock for180s; no worker
  launched. Do not bypass the other lane's lock. Two independent review
  launches failed with an unavailable service endpoint, not a code finding.
- 2026-10-18 (#105 Cam Link shared-mode proof): controlling MF SourceReader
  fails0xC00D3704 while the protected camera-ndi-bridge owns Cam Link4K.
  Windows10 MediaCapture SharedReadOnly works, using the bridge's MJPG
  1920x1080@60 unchanged. Shared NV12/BGRA conversion is unsupported here;
  the new busy-start fallback decodes JPEG with WIC and uploads BGRA through
  OutputPool. No GPU readback; native NV12 still stays onGPU. Added windows-rs
  capture/imaging/stream features only. Manifest.delivery identifies the
  CPU-decode/upload path; writer must choose actual Frame.pixel_format().
  Real180-frame render/NVENC run passes GPU validation. Independent reader
  matches30QPC-stamped JPEGs to248,832,000productionGPU bytes exactly;
  field-swap/line-double negative controls differ106.3M/53.1Mbytes. All30
  nonuniform/distinct. Media mode2 but sampleInterlaced/BFF=true; weave keeps
  rows unchanged.12reopens stable876–877handles, Drop9.0/warmmax17.2ms (not
  engine-stop timing). Physical unplug/replug deliberately not attempted:
  it would interrupt the bridge. Receipts/report in capture scratch.
  Safe instrumented own-reader StopAsync after6frames now proves the actual
  five-second starvation/stopped boundary and successful shared reopen with
  no late callbacks (452cc360); not a physical-unplug claim. Full portable
  gates233unit+1doc and native strictClippy/165tests pass. Fallback Astra
  review finds no blockers; regular endpoint unavailable. Native-only
  source30reopens warm to538handles then stay538(cycles3..29); combined
  Renderer probe's earlier12-cycle guard failed756→760and is NOT a pass.
  Capture-library cameras ready forCI, writer integration remains separate.

- 2026-10-18 (#105 device correction from Ryan): exon-desk's physical capture
  device is **Elgato Cam Link 4K**, NOT Elgato4K S. The4K S belongs to a Mac
  and is unplugged. Prior runs selected an MF entry named Elgato4K S; those
  black-frame/format/interlace observations are WRONG-TARGET diagnostics,
  not Cam Link evidence. Do not relabel historical artifacts or infer Cam
  Link behavior from them. Requalify against its actual MF symbolic link and
  formats; metadata reader always uses the resolved id/name, never a preset
  device identity. Default weave remains generic; no device-specific quirk.
  Leave camera-ndi-bridge and chatui untouched if Cam Link is exclusively held.

- 2026-10-18 (#105 cameras, delegator decision after42e2a922): sample flags
  override media type; default handling is GPU weave. MF's full NV12 sample
  already interleaves both fields, so preserve every Y/UV row unchanged;
  BottomFieldFirst describes temporal order, not permission to swap rows.
  This keeps mislabelled progressive pixels exact but does not remove combing
  inherent in truly interlaced content. Single-field pairing and per-source
  off/weave/bob setting remain later. WRONG-TARGET MF entry Elgato4K S mode2 reports sample
  Interlaced=true/BFF=true, SingleField/RepeatFirst absent. Capture-lifetime
  observations and resolved device identity now have a manifest reader bridge;
  writer lane owns connecting its native Platform hook and camera ISO input.
  Its180-frame MF-source→NV12 aspect views→Renderer plusNVENC run passes under
  D3D12 GPU validation; warmed handles stable. Stop20.939/warmmax24.734ms and
  an earlier2.66s stop do NOT satisfy20ms. Pixel-copy probe60frames is exact
  but uniform black, so no real nonuniform/no-line-doubling proof yet.

- 2026-10-18 (#105 capture takeover): current tree801a6521 passes native
  fmt, strict Windows all-target Clippy and81engine tests. NV12 array/crop/
  32-slot retention and exact345,600-component NV12/P010 code checks pass;
  6hardwareNVENC/MF files/180frames plus1softwareMF file/30frames fully decode.
  Software NV12 defaultHigh selects actual72. BGRA regression adds37files/1110
  fully decoded frames (quality/barriers/reuse/live CBR). Receipts and exact
  source boundaries are at the top of capture/scratch/REPORT.md. Portable
  gates retry exits69 (remote38/39/39GiB); no local build or disk-floor bypass.
  Regular review endpoint unavailable; no independent NV12 review claimed.

- 2026-10-18 (#105 historical wrong-target blocker): capture_camera on the
  MF entry named Elgato4K S (NOT exon-desk's Cam Link4K)
  still fails before publication: media type progressive, sample attributes
  Interlaced=1/BottomFieldFirst=1. Fresh receipt9973c885 confirms earlier
  strict/advanced-reader probes. Microsoft Video Interlacing docs say sample
  attributes override media type; do not discard those flags to claim success.
  Need known-progressive source/device or separately scoped GPU deinterlacing.
  camera.rs/lib exports remain WIP; NV12 compositor plane views and writer
  input-format negotiation are also pending. No driver/settings/apps changed.

- 2026-10-18 (#105 gates): remote space later recovered to43GiB on m2-b.
  Full scripts/remote-build.sh gates now passes59f36cd4+MFnearest edits:
  fmt, strict all-target clippy,233unit+1doctest;1hardware-only ignored. Log
  capture/scratch/mf-nearest-portable-gates.log. No cache cleanup or floor
  bypass occurred; prior exit69 entries below are historical.

- 2026-10-18 (#105 MF rounding): after59f36cd4, new uncommitted encoder.rs/
  mf.rs fix negotiates nearest representable quality and reportsactual.
  Microsoft software Set/Get echoes request until media types are reapplied;
  GetParameterRange/ValuesE_NOTIMPL.101command oracle yields29levels;70→72,
  30→29,50→51,85→86. Search is only pre-stream, exactquality fast-path, native
  tie choice retained; no cached cross-provider grid. Rawinfo requested_quality
  vsquality(actual); factoryrecording_quality().mf_quality is actual. All101
  softwarequalities and5production64×48presets pass;115files711frames decoded
  inclNVIDIA3codec regression,454stablehandles. Source tree0427e636…, reports
  have receipts. AAC/WASAPI already in59f36cd4, unchanged. Portable gate still
  exits69 onREMOTE36–39GiB hosts despite LOCAL35GiB; no cache cleanup performed.

- 2026-10-18 (#105 historical wrong-target discovery): MF SourceReader+DXGI
  manager on the entry named Elgato4K S (NOT Cam Link4K)
  really returns1080p60 NV12 IMFDXGIBuffer textures, format103/defaultusage/
  singleslice. MFSampleExtension_DeviceTimestamp is QPC100ns (MS documented;
  observedsameasSampleTime onthisdriver), duration166666; STREAMTICK !=frame.
  Without manager it returnsCPU NV12, not a production fallback. CamLink4K
  ReadSample fails0xC00D3704 with/withoutGPUmanager; cause unresolved, do not
  close other apps without approval. Receipt709375f2… and retainedprobes in
  capture scratch. Peer still owns frames.rs/gpu.rs with onlyBgra8/Rgba10:
  requested sharedNV12 Frame/importer ownership in writer REPORT before camera
  code/plumbing edits. Keep camera/ISO NV12 native, no forcedRGB roundtrip.

- 2026-10-18 (#105 recording APIs): native recording factory/barrier slice
  ready for CI, uncommitted. WindowsVideoEncoder::recording acceptsVideoTrackSpec
  and inputPixelFormat; explicit CONSTQP30/26/22/18/14 orMFQuality30/50/70/85/100,
  no VT.value()/bitrate guess orMac-quality parity claim. barrier snapshots
  accepted work, acknowledges each packet; callbackErr retains that packet.
  begin_segment after barrier forces next acceptedIDR, globalPTS must continue;
  only mux rebases. finish remains terminal. Native late errors now preserve
  queued prefix; fatal/timeout injection not performed. Microsoft software
  MF roundsQuality30→29,70→72, rejected rather than silently changing policy;
  100works. NVIDIA MF H264/HEVC/Main10 exact30/70/100verified.41files/1230frames,
  strict native clippy/81engine tests/read-only review pass. Full portable
  gates twice exit69: allMac build hosts below40GB floor (35/39/37GB); no bypass.
  API/evidence in capture and writer REPORTs. No peer adapter or daemon switch.

- 2026-10-18 (#105 native audio): capture lane's new MF AAC and WASAPI APIs
  are qualified standalone, not wired to the peer Platform yet. AAC inserts
  exactly1024silent pre-roll frames (otherwise sample0 can disappear); mux
  priming=1024, rawpacket DTS includes prefix, terminal samples trim padding.
  Nonterminal polling must not pad/flush. Real24k Flightdeck Stick positions
  stay24k despite48k delivery: first417frames then nativeposition240. Correct
  bounded SRC debt against each currentQPC, not an invented fixed clock;
  uncertain resampled discontinuities stop.48k Wave XLR/HDMI also qualified.
  Resolve selected endpoints before activation; GetDevice(friendlyname)
  returns E_INVALIDARG and must fall through to identity-only matching.
  Idle loopback has no packets; engine fills timestamp gaps. Process/app
  exclusions unsupported, so monitoring's exclude_own must fail closed.
  24capture/AAC reopen counts stable151/147; stop1.49–7.05ms is standalone,
  not a full-engine guarantee. WPR from System32 (19041) cannot merge traces
  (0x80010106); explicit Windows Performance Toolkit26100 wpr.exe works.
  CPU+Audio trace0lostevents;2s steady probe samples2.7%one-core including
  playback, PCM collection and AAC. Full OBS/performance gates still open.
  Source/receipt details in capture REPORT and peer "Capture-lane response".

- 2026-10-18 (#105 lock helper): capture lane adopted the peer's verified
  tree83f33006 scripts/windows-remote.sh byte-for-byte (blob a9185e45ac12c4ffe379b87aa7b43f29593105ac),
  without editing the peer worktree. One acquire/sync/run budget, watchdog,
  finally-release and byte1+ owner metadata now apply here too. bash/ShellCheck/
  worker AST pass; non-GPU held-owner metadata smoke f8eb900… and intentional
  exit17/released-lock smoke be4bae29… pass. GPU lock was free at07:04:45UTC;
  no reservation retained. Helper adoption is separate from the CI env fix.

- 2026-10-18 (#105 W.4 pressure handoff): answered the peer directly in
  `/Volumes/IsocastBuild/windows/scratch/REPORT.md` under "Capture-lane
  response — ownership/API handoff". Agent/windows owns writable BGRA pool,
  portable compositor preference and minimal Frame/DX12 target hooks. It
  already has a TLS MMCSS OS helper; reuse it rather than implementing another.
  Capture lane owns native AAC/recording policy/barriers and encoder-pressure
  diagnosis. Retained two-output4K run-065131 reaches23ISO accepted/15completed/
  8pending at tick23, with0.573mslateness and append maxima1.750/2.376ms:
  completion backlog, not a normal-append blocking receive loop. Three4K
  outputs need10.155s for5s media. No real-time/CPU victory or hardware-capacity
  root cause yet. Inspect/adopt peer's bounded self-identifying windows-remote
  lock helper before new hardware runs; never bypass incumbent locks.

- 2026-10-18 (#105 ownership): agent/windows now owns new Platform/FileWriter
  adapter files, writable output pool, minimal Frame/importer plumbing,
  daemon selection and W.3 contracts. Capture lane will not concurrently edit
  frames.rs/gpu.rs while that slice is owned there. First output contract is
  honest BGRA capability with existing Mac YUV default retained; never fake
  NV12 as two unrelated textures. Plane-view/YUV input changes require an
  agreed resource/format/fence contract. Capture lane owns native encoder
  input/quality/drain/reuse, conversion, MF-AAC/MMCSS, camera/WASAPI and WGC
  lifetime fixes; no adapter pieces implemented here. Stable video APIs
  are committed5d05c5ce; AAC/camera/WASAPI and writable pool draw/take are NOT
  ready. TexturePool.copy is not engine FramePool; Dx12Importer.target and
  ycbcr_target still reject writes. Encoder finish is terminal, returns the
  tail and has a5s failure deadline, not a<20ms stop guarantee or reusable
  session API. Prioritize hooks currently have no native admission policy.
  Keep HEVC config/codec explicit and off H.264 live/FLV paths; feed mux
  length-prefixed samples, not AnnexB. Coordinate lib.rs/manifest hooks before
  touching shared files; no production FFmpeg AAC fallback.

- 2026-10-18 (#105 adapter boundary): recording construction must take the
  existing VideoTrackSpec/EncoderQuality through a recording-specific native
  policy (CQ/QP), not infer bitrate from VideoToolbox's quality.value(). The
  mapping/method is not shipped yet. Nonterminal commit snapshots accepted
  input and collects its output without EOS; persist each received packet
  before surfacing timeout/error, never discard a partial drain Vec. Existing
  try_encode/receive_timeout allow a video barrier in the adapter now. Keep
  session timestamps monotonic across file reuse; rebase only at the mux and
  force an IDR for a new file. AAC partial input must not be padded merely to
  force a mid-stream commit: it limits the honest committed watermark until
  a complete block exists. Terminal tail padding/trim is a separate contract.

- 2026-10-18 (#105 output contract): pinned wgpu30 guarantees only binding
  usage for NV12/P010 (texture/format.rs1005–1006); the engine currently asks
  for separate plane Textures. Agent/windows owns honest BGRA compositor
  preference/output-pool plumbing, not fictitious NV12 planes. This preference
  must describe composed output only, not constrain future native camera/ISO
  input formats. Renderability requires explicit output-pool provenance:
  capture slots already have native RENDER_TARGET bind flags but remain
  immutable. Target-cache usage/state must distinguish writable output from
  binding-only capture. Publication needs real producer completion/ordering,
  not a ready marker at allocation/target creation. Do not let DX12 signal
  the capture D3D11 fence counter without total queue ordering; use the proven
  PendingFrame completion handoff or a separately ordered output timeline.

- 2026-10-18 (#105 CI): uncapped PowerShell samples and Cargo timings show
  the69.2MB generated Slint Rust (67,760functions) makes a single Windows UI
  compiler peak~13.5GiB working set/~15GiB private memory. Linker peak is much
  smaller and late; changing linker is not justified. UIopt1/opt-s take~19min
  for the binary versus~6min at defaultopt0, with similar memory; CGU256/16
  also have similar peaks. Keep defaults. Native CI now only sets jobs1 and
  Rust debug-info0 in that job; Slint metadata, assertions, overflow checks,
  compiled UI and all targets remain. Exact merged-main nine-package build
  passes in20m19s, contracts strict clippy/12verifier/core-stub checks and13UI
  unit tests pass. GitHub's16GB runner confirmation is the next CI rerun,
  not claimed from the128GB diagnostic host. Mac CI normally codegens the
  real UI in release, not the Windows job's dev profile.

- 2026-10-18 (#105): Ryan's standing Windows full-engine performance targets
  on exon-desk are <=half OBS CPU at1080p60/4K60 with program+ISOs, memory below
  OBS at every size, faster first preview/record readiness, stop<20ms and zero
  drops. Use WPR/ETW and matched settings. Integrate native MF AAC, GPU NV12,
  reduced synchronization and MMCSS with camera/audio/Platform work. Removing
  NVENC's copy requires proving exclusive mapped-input ownership; never map
  a texture concurrently read by the compositor or another encoder just to
  reduce the measured copy count. Old one-window FFmpeg-AAC numbers are not
  qualification for these new full-engine targets.

- 2026-10-17 (#105 W.5c2): delegator stopped the lane's isolated OBS PID13804.
  Verified its exact executable was no longer running, then removed only its
  generated `scratch/obs-portable/config/obs-studio/plugin_config/obs-websocket/config.json`.
  No credential contents read or logged. W.5c1 commit692314a and all three
  Actions39086165352 jobs are green.

- 2026-10-17 (#105 W.5a): capture lane uses
  `ISOCAST_WINDOWS_ROOT=C:/isocast-dev/lanes/capture` and
  `ISOCAST_WINDOWS_OUT=/Volumes/IsocastBuild/windows-capture/scratch/native`
  with windows-remote.sh. `ISOCAST_WINDOWS_GPU_LOCK=1` takes the shared
  `C:\isocast-dev\gpu.lock`; source/target/cache/home/IPC remain lane-local.
  Cua's existing Session 1 daemon launches native probes without activation;
  SSH is Session 0, unsuitable for real desktop capture. OBS 32.1.2 binaries
  copied into the lane's `scratch/obs-portable` (portable_mode.txt), not
  Ryan's profile. No OBS recording comparison yet. Evidence in capture
  lane REPORT.md. The daemon still selects StubPlatform on Windows.
- 2026-10-18 (#110 ProRes resumed handoff): user freed exonbook; final source
  built/gated remotely,250 tests/doctests (one native ignored), strict18/18
  once. Five noisy HW ProRes variants and7HD422 files lose nothing; fresh HW
  calibration22.40s and mixed HEVC/ProRes/H264 files360/360 plus stream890
  frames decode/count cleanly. SW selection comes from actual session ID;
  six full SW cells/positive fresh CPU in22.48s. Four tested noisy SW variants
  are loss-free; pairedSW4444 ISO startup loses43/240 (later46/120), program
  and audio whole. Model now explicitly warns startup_recovery/NoMargin,
  never silently reduces ISO or claims a zero-loss startup. Program-only SW
 4444 passes240/240. ProRes A/B3pairs0loss, CPU medians47.3→47.0% overlapping.
  Controlled HW/SW governor runs preserve main/pinnedshorts/ISOs and correctly
  thin only unpinned square; final PTS/pins verified. Uncommitted report:
  self-tuning-2/scratch/prores/REPORT.md. The old host blocker below is resolved.

- 2026-10-18 (#110 ProRes external blocker): base5703950e, current source
  uncommitted and NOT fully verified. Twenty-minute remote-build retry ended
 69; final check exonbook39GiB below40GiB guard, bothAWS Macs reserved. Pruned
  only this lane's generated exonbook UI cache then its remote build lane;
  insufficient unshared space, no other lane/user files touched. No local
  build or loweredguard. Early native measurements/calibration and source
  review fixes are retained in self-tuning-2/scratch/prores/REPORT.md; final
  source (software identity, whole-system CPU, live-backend invalidation,
  estimator fixes) still needs compile/gates/current-binary qualification.
  Exact resume command in REPORT; Windows waits for daemon switch.

- 2026-10-18 (#105 hosted CI daemon launch): a hosted Actions step runs
  inside a job object that forbids breakaway. The CLI deliberately launches
  isocastd with `CREATE_BREAKAWAY_FROM_JOB` and never retries attached, so
  a recording can't die with its caller. On the runner that launch fails
  with "Access is denied (os error 5)", which is not a file lock or a path
  problem. Reproduced exactly on exon-desk and in the VM by running the
  step inside a fresh default job.
  - The wording and install steps now start their throwaway isocastd
    themselves, as the step's child. They wait until `isocast status
    --json` reports that PID, then use the CLI, and wait for exit on
    shutdown.
  - The wording step prints the step's job flags as evidence.
  - The CLI's own adjacent launch stays qualified by the clean-VM
    Start-menu lifecycle.
- 2026-10-18 (#105 B1 review follow-ups): `Err_Generic` (the appcast check)
  now says "Couldn't check for updates" with an internet hint. Every refused
  download reports `Err_BadSignature` with the "couldn't be verified" text.
  `CleanLeftovers` also sweeps up to 64 `Isocast-Update-<uuid>` folders
  with the same owner, protected-ACL and fixed-name checks, so a download
  killed mid-transfer no longer leaves its partial file behind. Folders in
  use stay: they are held without delete sharing.
- 2026-10-18 (#105 B1 updater confinement): upstream WinSparkle 0.9.4
  wrote a download under the server's `Content-Disposition` name before
  checking the signature, and no fixed release exists. `winsparkle.ps1`
  builds the pinned source with `scripts/windows/winsparkle-download.patch`,
  cached by patch SHA. The patch saves only `Isocast-update.exe` (`CREATE_NEW`)
  in `%LOCALAPPDATA%\Isocast-Update-<uuid>`, protected for the user and
  SYSTEM. It requires a `[A-Za-z0-9._-]+\.exe` name without `..` or a device
  name, and Ed25519. It removes refused folders without recursion. Its UI
  says "This update couldn't be verified and wasn't installed". The app
  requires the DLL export `win_sparkle_isocast_confined_downloads()==1`.
  Gotchas:
  - Pass `-p:TrackFileAccess=false`: MSBuild's FileTracker crashed with
    redirected TEMP.
  - `.gitattributes` keeps `*.patch` byte-exact: under CRLF `git apply`
    rejects it.
  - Feed staging persists per registry path, so the probe harness and the
    app don't clean each other's leftovers.
  - WinSparkle's wx buttons expose no UIA Invoke; click their rectangle.
  - Isocast's Updates button is in the accessibility tree only once
    scrolled into view.
  - Text sent over guest `ssh` stdin loses non-ASCII characters: write `…`
    as `[char]0x2026`.
  - The 0.0.7.4/.5 test packages are in
    `C:\isocast-dev\vm\native\packages\security-*`.
- 2026-10-18 (#105 CI wording step): debug `isocast.exe` overflowed the
  1 MiB Windows main-thread stack while parsing arguments (`--version` too,
  exit 0xC00000FD). The step's "could not shut down" was the second overflow
  masking the first. `crates/isocast-cli/build.rs` now links `/STACK:8388608`
  on MSVC. The native debug build runs the exact step in 43 s with 9 PNGs and
  `{"stopped":true}`. Daemon and UI keep 1 MiB and never overflowed.
- 2026-10-18 (#105 merge-main handoff): main565729db merged into095385e0
  with `--no-commit`, staged and uncommitted. Main's canvases/self-tuning
  behavior is retained; capacity/estimate differ from main only in platform
  words/imports. UI retains ExtraCanvas, Segment and the generic secondary
  canvas stream hint, with Windows vocabulary reapplied. Notes/PLAN preserve
  both histories. Native build input treeeaa33b0207410513fca59fe1ce21ef6dcc40f858
  passes on exon-desk (17m35s); Inno0.0.7.3 package40,458,171bytes has SHA256
  `1fa67e50f65f5a677a732172d54c8a20fe8744037e87535fe0965dda88168ac7`.
  Own host source/packages: `C:\isocast-dev\vm\native\merge-src-eaa33b02`
  and `packages\merged-eaa33b02`; no other-lane files touched or release used.
- 2026-10-18 (#105 merge verification): full Mac gates248unit+1doctest,
 1existing hardwareignore, strict all-target Clippy/fmt pass once on exonbook.
  Separate main565729db and merged release builds produce14 real offscreen
  snapshot pairs. All compared UI regions are pixel-identical, including
  named-canvas context menu, two extra canvas video settings and secondary
  stream's Keychain hint. Dynamic picture/meter/temp-path differences are
  outside those documented regions; no whole-frame equality claim. Evidence:
  `windows-pkg/scratch/merge-main/{ui-pixel-comparison.json,main,merged}`.
  Binaries are in the approved temporary folder `isocast-windows-pkg-merge`
  rather than the nearly full build volume. Tier1 review remains the
  coordinator's step after CI, as requested.
- 2026-10-18 (#105 hosted merge): preserve main's measured **debug** profile
  with `CARGO_PROFILE_DEV_DEBUG=0`, service build separate from UI `-j1`, and
  `ISOCAST_UI_NO_GALLERY=1` for every potentially-UI-compiling step (including
  Clippy/tests/second candidate). Package/snapshot/diagnostic paths use debug.
  Native release package still has the gallery; no claim its memory usage
  proves the hosted recipe. Workflow actionlint passes; hosted CI awaits the
  coordinator. Existing main docs/cli.md has7 trailing-space diagnostics in
  generated help; preserved unchanged rather than editing unrelated docs.

- 2026-10-18 (#105 W.7 qualified natively): user explicitly approved building
  on exon-desk while hosted UI builds exceed runner RAM. Packaging owns
  `C:\isocast-dev\vm\native\{src,target,cargo,tmp,home,appdata,localappdata,logs,tools,packages}`;
  no other lane folder is used. Source09bf240e plus the uncommitted native
  fixes produced separate test-key builds0.0.7.1/.2. Inno/CRT/WinSparkle are
  pinned/recorded; binaries remain on the host to spare the shared Mac disk.
  Official release credentials/Authenticode signing/publication were not used.
- 2026-10-18 (#105 actual VM lifecycle): final `clean` restored before testing
  as standard user packager. Fresh install/Start-menu launch, direct .1→.2
  upgrade, explicit Ed25519-verified rollback to.1, actual WinSparkle tampered
  signature refusal and valid update to.2, active-UI uninstall refusal and
  successful idle-daemon uninstall all pass. Invalid update kept daemon6260
  and old UI alive; valid update ended both and relaunched new daemon10308.
  Injected unsigned `/FORCECLOSEAPPLICATIONS /DIR=...` feed arguments were
  ignored (captured child command line contains only our fixed safe flags).
  Two real1s H264/AAC movie copies, custom state and3real setup files survived
  byte-identically; the movie fully decodes after uninstall. Receipts/screenshots
  are `windows-pkg/scratch/qualified`; exact hashes are in REPORT.md. Ten native
  offscreen wording images and CLI help also pass. No recording/backend/GPU
  throughput claim: the daemon is still this branch's stub; renderer is CPU.
- 2026-10-18 (#105 native gotchas fixed): Windows-only updater callback
  returned toast's i32 instead of unit (native E0308); now discards it.
  `Gpu::new` selected Microsoft Basic Render Driver, but Slint1.18.1 rejects
  CPU adapters unless SLINT_WGPU_CPU exists. The very first Windows main
  operation now permits that fallback process-locally, before threads, without
  forcing CPU or changing Mac. PS5 decoded BOM-less release notes as ANSI;
  appcast.ps1 explicitly reads UTF8. Validated with real native builds and
  notes roundtrip. Native all-target strict Clippy and14unit+1doctest pass.
- 2026-10-18 (#105 automation boundary): a UI launched by a Task Scheduler
  GUI-tool worker inherits a non-breakaway job, so daemon launch correctly
  fails rather than risking a recording's lifetime. Launch through the actual
  Start shortcut or Explorer instead; that works without changing daemon
  policy. Cua native wheel produced no change; an explicit WM_MOUSEMOVE then
  WM_MOUSEWHEEL in the same interactive guest session scrolled correctly.
  Cross-session SSH PostMessage is denied. These were automation issues, not
  reasons to weaken process isolation. GUI/feed/input tasks were removed,
  no Isocast/updater/tool processes remain in the guest, VM left off.

- 2026-10-18 (#105 W.6 wording): `platform_text` selects vocabulary by target;
  Slint `PlatformText` is initialized in App::new for both native/offscreen.
  Windows privacy rows follow the engine's action and next_step, explaining
  desktop Camera/Mic access for isocastd.exe, optional capture-policy settings,
  and output loopback separately. Native backend operations remain their own
  lane; words do not qualify capture/settings/reveal. Fresh Windows setups use
  `~/Videos/Isocast`; existing saved recording folders are not rewritten.
- 2026-10-18 (#105 W.6 proof): full remote gates on exonbook pass239 unit
  tests+1doctest, strict all-target Clippy/fmt,1 existing hardware ignore.
  Nine actual isolated Mac snapshot pairs have identical upper2880×1200 UI
  pixels; bottom live counters/private temporary paths vary. Evidence under
  `windows-pkg/scratch/w6-compare`. Windows native screenshots/help are queued
  in the existing windows.yml and NOT yet claimed; inspect the retained
  `windows-wording-snapshots` artifact. Fallback source review and recheck
  pass; regular reviewer endpoint remains unavailable. Work is uncommitted.

- 2026-10-18 (#105 W.7b, uncommitted): two Windows CI qualification builds
  are `X.Y.Z.1` and `.2`, explicitly test-only, sharing a public test key and
  `http://127.0.0.1:17745/appcast-windows-test.xml`. The private test key is
  only in the host's ACL-restricted `C:\isocast-dev\vm\test-signing` folder,
  never in GitHub/artifacts or the app. Production uses the committed Sparkle
  public key and HTTPS latest Windows appcast; test overrides require the
  compiled test flag and are rejected by production signing/feed generation.
- 2026-10-18 (#105 W.7b verification): native WinSparkle0.9.4 SDK checksum
  verified; its verifier accepts a valid real-PE signature and refuses byte
  tamper/wrong key (supplemental, not an app-update claim). Mac strict release
  all-target Clippy for client/CLI/UI passes on m2-b in3m38s. After resource
  waits,15 unit tests+1doctest pass and normal CLI/UI release build passes.
  Real isolated Mac development Updates pixels match the baseline exactly;
  this does not qualify a released Sparkle bundle. No override/local build
  fallback. Configured reviewer unavailable; documented fallback source review
  and fix re-review have no remaining identified blockers. Native Windows
  compilation, real packages and full lifecycle checks still await CI.
  W.7a run39129808275 built Windows successfully but failed its version regex
  on CRLF (native reproduction: LF0.0.7, CRLFempty), so no artifact exists.
  W.7b removes that parser in favour of actual compiled app identity; CI now
  retains diagnostic executables even if packaging fails.
- 2026-10-18 (#105 VM GUI preparation): a real `packager` desktop (session1)
  was created with guest-only one-shot auto-login; its password was neither
  printed nor exported, and a SYSTEM logon task removed DefaultPassword and
  disabled auto-login immediately. Desktop and cleanup verified. This is a
  disposable post-clean preparation, not a replacement of `clean`; restore
  `clean` before qualification and prepare the standard-user desktop again
  before installing anything. Do not reset account passwords during an
  upgrade/uninstall data-preservation sequence.

- 2026-10-18 (#105 W.7, packaging lane): `isocast-pkg` is an isolated Gen 2
  Hyper-V guest on exon-desk-lan, rooted at `C:\isocast-dev\vm`, with 8 vCPU,
  32 GiB fixed RAM, a 200 GiB dynamic VHDX, Default Switch, Secure Boot and
  vTPM. Windows 11 Enterprise Evaluation 26H2 comes from Microsoft's official
  evaluation centre; ISO SHA-256 matches KB5130867
  `BC3F24086EBADC94489066B5AD78089E2CF5C3491E90E790BB81A2B199C10E38`.
  Unattended installation and key-only OpenSSH login passed. Guest hostname
  `ISOCAST-PKG`, build 26300, `Get-WindowsEdition` reports `EnterpriseEval`
  (Get-ComputerInfo's legacy product label misleadingly says Windows 10).
  `clean` is an offline standard checkpoint; down/revert/up removed a deliberate
  guest-only marker, retained SSH host-key trust, and the `packager` login still
  reports administrator=false. All five wrapper operations and an exact exit-7
  propagation check pass. No GPU is passed through: never
  use this guest as GPU/NVENC or real-device performance evidence.
- 2026-10-18 (#105 W.7 VM control): `scripts/windows-vm.sh up`, `ssh [command]`,
  `ps 'PowerShell'` (or stdin), `down`, and `revert-clean --yes`. Revert requires
  the guest to be off and discards only this guest's changes. SSH uses the
  host's existing personal identity through `exon-desk-lan` as a jump host;
  the NAT address is discovered every time. Guest host-key trust is bootstrapped
  over authenticated PowerShell Direct, then pinned in this lane's private
  scratch `known_hosts`. No host forwarding/firewall/Hyper-V setting is changed.
  `ISOCAST_WINDOWS_VM_USER=packager` selects the standard (non-administrator)
  account for installer qualification; the default `isocast` account administers
  only the guest. Both accept the same existing personal public key, not passwords.
  The generated local-account password is never printed: host LocalMachine
  DPAPI plus an explicit file ACL limited to Ryan/SYSTEM/Administrators protects the retained
  bootstrap credential. CurrentUser DPAPI fails under key-only OpenSSH logons.
  Hyper-V adds access rules to its root folder, so do not rely on that folder's
  inheritance for credentials. Only this VM also has read access to the answer
  ISO. Unattend media and cached guest answer files were removed before the final
  `clean` checkpoint. Windows also retained `Panther\unattend-original.xml`:
  after discovering that by filename only, the earlier point was renamed
  `bootstrap-before-answer-cleanup` and a fresh offline `clean` was taken.
  Reverting the final `clean` and booting verifies zero cached answer XML and
  no compiler scratch. Use `clean`, not the earlier provisioning point.
  The lane leaves the VM off between package runs to free its 32 GiB; `up`
  waits up to three minutes for guest SSH. No Inno compiler or Isocast is installed
  in `clean`.
  Windows 11 initially enabled the SSH firewall rule for Private networks only,
  while Default Switch appeared Public: host→guest TCP/22 timed out despite a
  listening server. The guest-only rule now allows LocalSubnet on all profiles;
  the host firewall was untouched. The evaluation expires after 90 days; it is not a production
  Windows licence. VM provisioning/evidence stays in `windows-pkg/scratch`.

- 2026-10-18 (#110/#27 merge qualification): HEADbb8790a5 + main59f78393
  merged with `--no-commit`. Governor conflict keeps BOTH `frame budget`
  exclusion from ISO pin diagnosis and named-secondary `unpinned_secondary`;
  per-hub ownership and budget-full/5s-tail +30s-quiet restoration retained.
  Notes conflict keeps both histories. Remote gates249 pass, strict112811
 18/18 zero-loss once. Combined AWS112520:24 tiles saturated50s; lastbudget
  refusal53.65s, preview15→30 at88.20s/full118.61s, no early recovery. Healthy
  hub1500/failed4000 stay isolated, one VT refusal; three130s files7800/7800
  frames with equal A/V, no drops. Congested streams lose309/235 video pictures
  (audio intact), explicitly not a loss-free-upload claim. First combined
  runner failed due to missing proxy dictionary; corrected experiment only,
  no production patch/assertion relaxation. Scratch budget-merge/REPORT.md
  has hashes/results. Merge remains staged/uncommitted; no owned helpers left.

- 2026-10-18 (#110 remaining handoff): branch fast-forwarded to main6d6d58fd
  before edits; current work uncommitted. Mixed H26x overlay, per-hub refusal
  ownership and named secondary pins qualified onAWS. Gates243/strict18/18,
  stream3-pair A/B0loss. Real `memory_pressure -p33 -s1` (NOT `-S`) reached
 7.92–8.40GiB available and actual warninglevel2; three94.7s files2841frames
  each and stream2793frames lose nothing, preview fully restores59.91s after
  release. Helpers/watchdogs reaped; resumed inspection found no lane jobs
  or helpers left running. Report: self-tuning-2/scratch/remaining/REPORT.md.
  Regular reviewer unavailable>1h; Astra fallback review/re-review and final
  evidence acceptance have no blockers. Regular-model review before release
  remains required. Prior slices are committed6c4663ca/merged6d6d58fd, not
  pending approval despite the historical entries below.
- 2026-10-18 (#27 review fixes): 5d06bfef + main6d6d58fd merged without a
  commit (notes conflict keeps both histories). B1 auxiliary cache eviction
  happens before the next GPU batch and yields that admission turn, so the
  same first sixteen owners cannot continually reclaim every permit. Hidden
  auxiliary inputs and idle compositors release caches too; bindings/reuse
  state are cleared, older GPU submissions keep their own input leases.
  B2 notices/counters are copied under the ledger mutex and reported through
  bounded deferred logging outside it; status JSON is built outside it too.
  B3 full-budget occupancy and the five-second refusal/release tail both
  block the governor's 30-second recovery timer (memory pressure kept).
  Ordinary clones now share a domain claim; independent deliveries still
  deduplicate. Final evidence: frame-budget/scratch/REPORT.md and fix1/.
- 2026-10-18 (#27 review verification): matched native pixel run103710,
  24 staggered masked tiles:23 frozen before. Final130s run110027 keeps all
  24 subscriptions open through50s saturation (8–10 changes/33s off air),
  then12 open through recovery (34 changes/35s); program/ISO7800/7800.
  Disable client read timeouts when testing deliberately shed frames: the
  early5s client timeout could close a stream while JPEG demand continued.
  The3s looping-counter experiment
  aliased with the new~3s update cadence; use test_pattern's six-digit counter
  for freshness, not repeated clip numbers or frame timestamps. Log-writer
  delay102500→103117:299→0 program drops. Pressure50s: no false restoration;
  last refusal53.66s, restore89.09s then118.48s, remaining12 viewers continue.
  Stress103319:zero protected/audio loss. A/B103220:CPU42.3→42.1%, instructions
  494.8→493.9M/s, footprint140.4→133.1MB. Gates247 unit +doctest, strict104755
  18/18, all pass. Binary8e6501b8. Main6d6d58fd is merged without commit.
  B2's isolated `ISOCAST_BUDGET_LOG_STALL_MS=AFTER_MS:DURATION_MS` probe is
  capped at10s, once, developer-only; markers go to native-relay JSONL in
  d.logs, not daemon.log. Original native-priority/device limits remain open.

- 2026-10-18 (#27, frame-budget lane): replacement design is per-origin,
  distinct-buffer accounting, not delivery permits. Independent reserves are
  8 producer, 32 file-recording, 16 auxiliary and 36 protected-compositor
  pictures; the CV pool's physical ceiling is their sum (92), allocated on
  demand. The composition reserve is (GPU in-flight limit + one cached input)
  × (main + MAX_EXTRA_CANVASES), not another arbitrary queue size. A buffer
  shared across domains counts once in the physical union; repeat deliveries within a
  domain do not spend another permit. Native VT callbacks retain the same
  lease, as do GPU inputs/outputs through their existing completion fences.
  Streams/replay, previews, scene tiles, source-mask caches and virtual-camera
  reads use the auxiliary reserve; recorded-canvas inputs use composition,
  and files/ISOs use recording. This fourth domain was added during audit:
  a stalled passthrough ISO can fill the file reserve, but must not freeze
  the program reading that same source. Auxiliary refusal feeds the governor
  without asking it to thin ISOs. This protects against other domains, not a stalled protected
  writer exceeding its own reserve or the machine's physical encode capacity.
- 2026-10-18 (#27 design decisions): copy scarce SCK/AVCapture buffers once
  at ingress, preserving format/bit depth/attachments, rather than letting
  many outputs retain an eight-surface device pool. Decoder frames remain
  zero-copy and have a per-reader ledger keyed by native CV buffer identity:
  two samples aliasing the same native buffer share a charge too, not merely
  Rust clones of one wrapper. AVAssetWriter's opaque lifetime is
  isolated in a private threshold-limited copy pool and charged at its full
  reservation, never reported as an invented exact native count. The camera
  helper likewise copies before acknowledging enqueue: CMIO retains the
  helper's pool, never the parent's recycled pixels. Shared IOSurface rings
  are counted separately (including texture wrappers and Mach offers), with
  their existing per-ring limit. This is a frame-pool invariant, not a
  machine-wide RAM allocator or a count of Apple's private codec allocations.
  Status/doctor expose the ledger and sticky refusal/allocation-failure totals.
  Windows' platform hook remains explicitly unavailable, not a false pass.
- 2026-10-18 (#27 diagnostics): isolated developer mode accepts
  `ISOCAST_FRAME_STALL=iso|live`: hold one real consumer worker for 600 ms,
  once, after two seconds. It does not stop VT callbacks or the daemon.
  Normal mode ignores it. Use it to distinguish file-queue retention from
  capture freshness; numbered media checks detect a frozen input even if
  a program file continues storing repeated pictures at its nominal fps.
- 2026-10-18 (#27 device qualification): M2-a's existing signed development
  app reports screen recording not granted, and `virtual_camera_start` says
  the camera is not installed/approved. No permission, setting, or install
  was changed. The fallback AVAssetWriter path did qualify there: program
  and two ISOs, each 360/360 at 60 fps, zero audio/video loss, full decode.
  The saved `command` in hw-queue/done/20261018-090156-frame-budget-cmd.json
  is the exact permission-aware retry through `hw-batch.py now cmd --gui`.
  Pixel-copy format/plane tests are not a claim of real capture/CMIO success.
- 2026-10-18 (#27 final verification, a16084a6 daemon): failing 24 s AWS
  workload (six ISOs, three extra recorded canvases, stream/replay, masks,
  40 multiview tiles, 1,320 cuts) loses 461 program frames on main; new
  records every frame in all ten files and the stream/replay. All 45 budget
  snapshots hold; refusals [0,0,1157,0], auxiliary peak16. Governor lowers
  previews60→30→15. Final gates244 unit tests + one doctest, strict18/18.
  Flagship-1080 A/B3 pairs: CPU41.6→42.4% (+0.8 points), instructions+0.5%,
  GPU86.2→86.0ms/s, footprint125.6→124.7MB; zero media loss on both.
- 2026-10-18 (#27 remaining native-policy limit): a600ms stopped ISO worker
  fills its32-frame reserve (5 refused), but the program stays480/480 with
  every numbered picture advancing. Its own ISO loses22 frames, accounted.
  A600ms live-worker stall leaves every frame-budget domain unexhausted yet
  loses51 ISO frames (program480/480, all audio intact): stream.rs includes
  the hold in native submit latency/health, setting native priority pressure;
  crash_safe::append_frame then refuses ISO before budget admission. This
  priority contract is unchanged from main and separate from pool starvation.
  After three diagnostic attempts, stopped expanding the fix: the wrong
  assumption was that a native-consumer stall tests only buffer retention.
  Keep #27 open for the stronger requested no-ISO-loss contract; reconcile
  native stream-vs-ISO priority with #110 rather than tuning pool caps again.
  Evidence/retry commands: hw-queue/done/20261018-092625-frame-budget-cmd.json,
  frame-budget/scratch/consumer-stalls/stalls.json and REPORT.md. No assertion
  was relaxed to hide this failure; no native policy change was made.

- 2026-10-18 (#110 self-tuning-2 handoff): uncommitted on063a21a5. Gates242
  pass; the full strict suite ran once under CPU4+Metal+2GiB resident memory,
  18/18 zero-loss. M2-b's three interleaved flagship1080 A/B pairs lose no
  frames/audio (daemon CPU median9.8% both); a288 s strict30fps soak has21
  decoded files,8436 frames per role, zero losses/repeats,8601 stream frames
  and both crash checks pass. These M2s have24GB, not8GB. Actual kernel
  memory-pressure samples stayed normal; scoped per-daemon injection separately
  proves warning/critical, pin/ISO-consent and30s-step recovery. No new M5 run.
  Scratch REPORT.md lists every job, hashes and failures honestly. Formal
  Tier-1 review failed before reading code three times (Endpoint unavailable);
  no approval, commit or GitHub write. T.1b.1/T.3a/T.4a are ready for review,
  not completion of full T.1b/T.3/T.4 or #110.
- 2026-10-18 (#110 qualification): four calibrations take22.26–22.86s,
  including outside CPU load. Every supported ProRes variant, live CBR H.264,
  UHD60 HEVC10 and three full fit cells are covered;19–21/24 cells total,
  skipped diagnostics explicit. Multi-canvas and low-end fitting files lose
  nothing. M2 overloaded ISO loss44.39–53.67% vs measured53.09%/table52.62%:
  no improved percentage-accuracy claim. New-only init/blank profiles choose30;
  explicit/copy60, pins and no ISO consent survive restarts. Late-canvas floor
  now joins at25%; native rollback restores real properties even when numbers
  equal the last acknowledgement. Same-canvas multi-hub refusal remains
  canvas-wide (verified one refusal, sibling restored, separate canvas adapts).

- 2026-10-18 (#110 review fixes, current): uncommitted on7705e90. The
  governor NEVER thins ISO implicitly: `governor.allow_iso_thinning` is false
  by default; explicit opt-in permits it, pins still override it, and opt-out
  cancels a running hold. Existing native safety may still drop ISO pictures.
  Settings exposes the choice; Optimize/setup-check mention it without
  enabling it or assuming it makes the workload fit. Owner-local native
  intentional-refusal counters prevent false losses after vertical recovery;
  preview retune refusals are nonfatal with per-feed error lifetime.
- 2026-10-18 (#110 fix evidence): real pre-fix035640 reproduces vertical
  pin→false ISO hold and two disconnected previews under injected VT-property
  refusal. Rejected partial fix041634 proved native admission attribution
  was also needed. After043111 pin/natural restoration and down/up refusals
  pass; consent043746 and corrected Optimize/choice043938 pass. Idle CPU
  medians0.02589%→0.00988% of one core; disabled0.00939%, overlapping noise.
  Final gates240 pass once; strict05573918/18 passes once, zero reported
  media loss. Report has the fix-by-fix evidence table and known F2 limits.
- 2026-10-18 (#110 storage): preserved current binaries on M2-b at
  `~/isocast-scratch/machine-profile-preserved/review-candidate` (daemon
  SHA25688ed7bea5fa07ef2), baseline in its sibling`baseline-7705`. Old,
  completed synthetic directories `/tmp/govab-*`, `mpmerge-spke3bm4`,
  `govfix-ew539qol`, `govfix-p27wualk` were checksum-relocated to the lane's
  local `scratch/retained-m2-b/<name>/`, not discarded. This freed enough
  M2-b space for gates without lowering its40 GiB guard. Never confuse
  hw-batch's standard pushed old bin-dir with these explicitly executed
  remote candidate binaries; strict's `binaries.json` verifies them.

- 2026-10-18 (#110 earlier T.2 handoff): ca4cc010 contains the first slice.
  Main257ea057 (Windows plumbing, GPU lifetime and masks115) was merged as
  eaf0b6a7; its results were committed as7705e90c by the delegator.
  Shared-frame conflict keeps monitor cap AND new max-size argument; ISO
  keeps governor pin AND GPU retry/presentation-time path. Daemon governor
  now uses main's NativePlatform alias. Merged gates240 pass; strict e2e18/18
  and seven native media contracts pass; job20261018-032243 verifies animated
  program/filtered-ISO masks, split during emergency hold, native readback
  and full recovery. Binaries stay onM2-b in ~/isocast-builds/machine-profile/
  target/release (local shared volume only4.8 GiB free; no binary/movie fetch).
- 2026-10-18 (#110 evidence limits):162024/195658/195715 were interrupted
  before hardware, not governor failures. Detached201445/201446/201447-cmd/
  e2e all passed. CPU/GPU contention was loss-free with zero policy steps.
  Severe foreign4×UHD60 demands: main program1498 video/1695 audio dropouts,
  governor1329/1623; a single pair is not repeatable improvement. ISO audio
  stayed whole but native admission held ISO pictures for about121 s: only
  the governor's extra overlay is bounded to3 s, not physical starvation.
  Six policy changes/no cycling; native share recovered25.6 s and preview
  61.1 s after foreign load. Custom-job PASS proves decode/accounting, not
  zero drops. Independent merge-review tool failed twice (endpoint unavailable);
  coordinator Tier-1 review remains. Full evidence/limits: lane scratch REPORT.md.

- 2026-10-17 (#110 T.2 history): user approved initial exonbook baseline
  while retaining AWS qualification. Runtime slice later committed ca4cc010: preview,
  ISO/vertical admission and shared-stream bitrate controls, profile pins/off,
  status/events/UI. A zero-ISO limit held for30+ seconds failed real A/B and
  was replaced by a non-renewable3 s emergency overlay, rearmed after5 quiet
  seconds. Native ISO_SHARE remains the only encoder-sharing controller.
  Network shedding is not production loss and never triggers ISO blanking.
  Exonbook strict e2e18/18 and233 tests pass;600 s fitting workload has zero
  steps/losses. Real CBR/replay6000→1500→6000 verified; severe encoders can
  still exceed protected capacity. Ten-minute overloaded run has four changes
  without cycling, but preview cannot restore until remaining load fits.
  Initial AWS runners were lost; detached replacements and their results are
  described above. Report/evidence under the lane's scratch folder.

- 2026-10-17 (#110 F4/F8): `setup check` uses only a reachable existing
  daemon; otherwise its capacity check says "not checked" (validation still
  succeeds/fails locally). Automatic calibration also defers while the live
  engine previews/starts. `ensure_engine` holds the start gate and cancels
  only an automatic helper before source startup; manual calibration remains
  available during preview. Exonbook job 20261017-161756-machine-profile-cmd:
  both offline cases and preview/manual/preemption cases pass, 90/90 frames
  and 3.000 s A/V. Included in ca4cc010; original gates233 pass.
- 2026-10-17 (#110 T.2 scheduling): the 0.0.7 release soak
  `20261017-160535-delegator-soak` owns **both** AWS hardware locks for six
  hours: m2-a records, m2-b receives. `hw-batch now --mac any` says "no test
  Mac reachable" when both locks are held, even though SSH is reachable.
  No lock was bypassed. Required competitors-only baseline on main 24bea09d
  was queued as `20261017-162024-machine-profile-cmd` (later interrupted). The user
  subsequently approved exonbook as the initial baseline; implementation
  continued without waiving AWS qualification. The detached replacements
  completed overnight; no jobs remain waiting. Report/integration map are in
  `/Volumes/IsocastBuild/machine-profile/scratch/REPORT.md`.

- 2026-10-17 (#105 W.1e): Windows UI now builds with Slint 1.18.1/Skia
  and renders offscreen through RTX 5070/DX12. First native release build
  of daemon/CLI/UI took 14m11s; relinking generated UI after a client change
  still took 12m17s (warm dependencies, not a fast-edit-loop claim). MSVC
  build-script main stack is 1 MiB; build.rs uses a Windows-host 8 MiB compiler
  worker. Runtime app main did not need a stack override for the smoke.
  Native panels/key hooks, editor decode/audio, shared surfaces and capture
  are still stubs; W.6 owns real-window/DPI/font parity. Snapshot evidence:
  windows/scratch/w1e/windows-main.png. Isolated smoke daemon was stopped.

- 2026-10-18 (#105 hosted Windows CI memory): run 39135916369 died with
  `rustc-LLVM ERROR: out of memory` compiling `isocast-ui (bin "isocast-ui"
  test)` on GitHub's 16 GB runner (bin and test rustc side by side). Slint
  puts the whole interface into ONE generated Rust module (65 MB of source)
  and every rustc phase scales with it, so cargo's codegen-units and jobs
  can't split it: measured on exon-desk, dev profile, debug=0, `-j 1`, rustc
  peak working set / commit for the bin: **13.60 / 15.13 GiB** (`-Z
  time-passes` rss: expand 3.0, typeck 5.4, borrowck 8.4, mono 10.6, LLVM
  11.9). Not worth chasing: `with_debug_info(false)` saves 2% of the source;
  Slint 1.18's only real split is the experimental multi-crate
  `as_library` mode, which would mean new crates and rewiring every global.
  What worked: the kit gallery (ui/kit-gallery.slint, `--state kit`, a design
  board) is 24% of the generated code (65.3 -> 49.4 MB). build.rs
  `ISOCAST_UI_NO_GALLERY=1` compiles a copy of ui/ with an empty `KitGallery`
  (same two properties); peak becomes **10.62 / 12.04 GiB** for the bin and
  10.31 / 11.76 for the test binary. windows.yml builds the UI in its own
  step with that variable, `-j 1` and `CARGO_PROFILE_DEV_DEBUG=0`; the rest
  of the workspace builds first at normal parallelism. **Coverage gap:** the
  hosted Windows build doesn't compile the gallery's .slint (the macOS CI,
  every normal build and the exon-desk qualification do, the variable is
  unset there); nothing else differs, and `--state kit` on that binary draws
  nothing. Measure again after the UI grows: at 13 GiB commit the hosted
  runner has less than 3 GiB of headroom.
- 2026-10-17 (#105 W.1c): `scripts/windows-remote.sh status|sync|cargo|build|run|fetch`
  uses exon-desk-lan, the PowerShell wrapper and binary git-archive over SSH.
  Managed source is `C:\isocast-dev\src`; warm target/cache, isolated home,
  AppData and scratch stay under that root. Uncommitted tracked bytes are
  exported via a temporary index; `git add -N` includes new files. Unchanged
  file mtimes survive sync. Results go to the lane scratch `native/runs/`;
  release executables to `native/bin/`. Run accepts one PowerShell script;
  use `$OutputEncoding = [Text.UTF8Encoding]::new($false)` when piping Unicode
  source to Python/rustc. Process plugins and recording still refuse the stub.
  Real control-plane CLI/MCP work; W.1d record/UI/writer remain separate.
- 2026-10-17 (#110, fix round 1): automatic calibration is default-daemon
  only; either ISOCAST_SOCKET or ISOCAST_SETUP disables it unless explicitly
  opted in with ISOCAST_CALIBRATION=on. Snapshot/visual/layout/e2e/bench/
  stress/soak/motion/drag-lag harnesses set off. Failed builds are remembered
  in `setup.machine.failed-build`; explicit calibration retries, success
  clears it, cancellation does not write it. Dead-owner calibration dirs
  are swept (live/unknown owners and links retained). Profile measurement
  version 3 prioritizes the three fit cells and rejects short/non-monotonic
  samples. Recording-only Optimize retains its exact selected tune pass;
  setup check/doctor now use the daemon's same profile-aware estimate.
  Release `--snapshot-watch` is invalid (needs ui-dev/live-slint); use
  `--snapshot … --state wait:10000` for a release-mode idle-window check.
  Re-review evidence: machine-profile/scratch/REPORT.md, Fix round 1.

- 2026-10-17 (#110, machine-profile lane): T.1a measurements live in
  `setup.machine.json` beside the isolated setup/tune file; CLI
  `isocast machine calibrate|show|cancel`, same method table/MCP. The helper
  is nice 15/utility QoS, synthetic only, parent-killed/reaped before output
  starts and at 29 s (its own watchdog also exits after 28 s). M2 wall
  23.5 s, M4 Max 20.7 s; M2 omits 4K60 HEVC10 to meet the budget. Do not
  equate file finish time with encoder throughput: the initial trial did,
  and was rejected. Read-only completed-picture counter deltas are now used.
  Only covered HEVC High/8-bit costs replace the table; codec/topology/live
  H.264/recovery assumptions and unfinished T.1b are explicit in REPORT.md.
  No governor or defaults change. Main advanced to GPU merge `9783eb22`
  while this lane was uncommitted; coordinator owns commit/integration/review.
  Evidence: `/Volumes/IsocastBuild/machine-profile/scratch/`.
- 2026-10-17 (#110/T.4): bounded `scripts/weak-machine.py` + `weak-gpu.swift`
  prototype uses an active hw-batch reservation, not a hostname allowlist:
  m2-a's LocalHostName is `Mac-mini`. `taskpolicy` is `/usr/sbin/taskpolicy`.
  Eight CPU competitors + Metal compute + background scheduling made strict
  e2e record/iso lose 86.1%/96.2% of pictures and one audio dropout each;
  unpressured controls were loss-free. These are contention, not true core
  limits. Failed results propagate; helpers are cleaned up. No claim of
  protection from this load until T.2. Job 20261017-135605-machine-profile-cmd.

- 2026-10-17 (#100): **hw-batch `--mac any` jobs hold `.bench.lock` only
  while they build and pick a Mac**, then just that Mac's lock (as the
  docs said); `pick_mac` skips a Mac whose local lock another runner
  holds. Runners record `scutil --get LocalHostName` (lowercased) as their
  host, and `reclaim` compares names without domain or case, so entries
  stay reclaimable when the network renames the Mac. `reclaim` checks that
  a test Mac pid is the job's (older than its pid file), survives rsync/ssh
  timeouts, and stops a dead runner's soak relay (`ssh -R`, by pid, only if
  that pid is still an `ssh -R`) and removes its key on the receiver. A
  second TERM no longer cuts a runner's cleanup short. **`reap`'s
  ownership rule** (read before scheduling `reap --kill`): an isocastd on
  an M2 is "owned" when a running/ job on that Mac started (by its
  runner's `started`, else the entry's ctime) before the engine did, or
  its entry is unreadable; the Mac's own Dev.app engine and installed
  apps are never touched; only engines older than `--hours` (12) are
  listed. A job that leaks an engine and then finishes leaves it for reap.
- 2026-10-17 (#100): e2e.py's short run folder (a `--work` too deep for
  sockets) is now on `--work`'s own disk when it can be
  (`/Volumes/IsocastBuild/.e2e/e2e-XXXXXXXX`, 39 bytes), else /tmp as
  before; kept evidence (`--keep`, `--keep-failures`) is copied back under
  `--work` (sockets left out) and the short folder removed, so nothing
  piles up in /private/tmp on the test Macs.

- 2026-10-16 (#91, #97, #98): **hw-batch keeps itself tidy.** A runner now
  records its pid and host in its running/ entry, and stops its test Mac
  job and writes its summary on TERM/HUP (a `timeout` around `now` used to
  kill it outright, leaving the job running there with no lock and a dead
  entry). `run-queue` and `now` first reclaim dead entries (runner gone, by
  pid, or for entries from older runners by no process holding the job's
  log.txt; and no live `pid` in the job's scratch on its Mac) to done/ as
  LOST with a summary; `hw-batch.py reclaim [--dry-run]` does it by hand.
  **Long soaks: `hw-batch.py now --detach soak --hours H …`**, never by
  hand outside the per-Mac lock (#62's losses). `hw-batch.py reap [--hours
  N] [--kill]` lists (dry run by default) isocastd processes on the M2s
  older than N h (default 12) that no running job owns (one that started
  on that Mac after the engine did); never `~/Library/Application
  Support/Isocast/Isocast Dev.app`, /Applications, or exonbook. GUI cmd
  jobs leak an engine each (11 older than 1 h on m2-a at 19:30). e2e.py
  runs in `/tmp/isocast-e2e-XXXX` when the run folder is longer than 44
  bytes (hw-batch's is ~90: sockets past the 104-byte limit).
  **There is no `isocast shutdown` command** (#98): it's a usage error
  (exit 2), so scripts that ran it with `>/dev/null 2>&1` left the daemon
  up. Stop an isolated daemon with the `shutdown` method:
  `printf '{"jsonrpc":"2.0","id":1,"method":"shutdown"}\n' | nc -U "$ISOCAST_SOCKET"`.

- 2026-10-16 (S.2 stress): **hw-batch runs one job per test Mac, not one
  job at a time.** A job with `--mac m2-a|m2-b` (and a soak's receiver)
  holds `/Volumes/IsocastBuild/.bench.lock.<mac>`; `--mac any` holds
  `.bench.lock` while it builds and picks, then the Mac's lock. So `now
  … --mac m2-b` runs while a soak holds m2-a. Each job exports its source
  to `hw-queue/src/<job>/hw-batch`. Runners started before this (an old
  `run-queue` process) still take only `.bench.lock`, so they don't see a
  pinned job's Mac lock: don't pin new jobs to a Mac an old runner may
  pick until it exits. That includes a soak's `--receiver`: the stress
  lane's m2-b runs (02:40-03:35 New York) overlapped the main soak's
  receiver ffmpeg on m2-b; the soak's stream kept 0 drops, 0 reconnects.
- 2026-10-16 (S.2 stress): the shared test VM's VideoToolbox was wedged
  (every hevc/h264/ProRes session failed with -12903 / -12900 after 3.6
  days up, with another lane's engine running): e2e's encoder probe hangs
  30 s and recordings fail. `scripts/vm.sh` can't fix it; a VM restart
  would (not done: another lane was using it).

- 2026-10-15: Lane speed (audit /Volumes/IsocastBuild/agent-audit/REPORT.md).
  exonbook builds: `scripts/remote-build.sh` keeps one source + target per
  lane in `~/isocast-builds/<lane>` there (nice 15, lane lock, refuses
  below 40 GB free; `status`, `prune`); never touches `~/dev/isocast` or
  the dev engine there. Binaries it fetches are minos 11.0 / sdk 27.0,
  ad-hoc linker-signed like local ones; `testmac.sh push` and e2e/bench
  sign their own dev apps. Nightly hardware queue:
  `/Volumes/IsocastBuild/hw-queue/` (`scripts/hw-batch.py`), cronctl job
  `isocast-hw-nightly` 01:30 New York, registered paused until merged; on
  the M2s it uses `~/dev/isocast-worktrees/hw-batch` and
  `~/isocast-scratch/hw-batch/<job>`. Test VM: lume can report
  `isocast-vm` "stopped" while an older `lume run` still runs it
  (`lume run` then fails with "Failed to lock auxiliary storage"); vm.sh
  falls back to the DHCP lease (192.168.64.5). The VM has Homebrew ffmpeg
  since 2026-10-15 (not in `isocast-vm-clean`; `vm.sh e2e` reinstalls it).
  Strict e2e drops frames in `projects` in the VM; use `--hosted`.

- 2026-10-13: OBS Studio on the test Macs (for S.1): isocast-m2-a has
  32.2.2 in /Applications from the Homebrew cask `obs`; exonbook keeps
  Ryan's own 32.0.4 in /Applications (with his profiles, NDI and other
  plugins: don't change it), and the benchmark uses a 32.2.2 copy from the
  official DMG (sha256 checked against the cask) at
  `~/isocast-scratch/obs-bench/apps/OBS-32.2.2/OBS.app`. Both are signed
  "Developer ID Application: Wizards of OBS LLC (2MMRE5MTB8)", notarized
  (spctl accepted). obs-bench runs OBS with `HOME` and `CFFIXED_USER_HOME`
  set to `<work>/obs-home`, so it reads and writes only that copy of
  `~/Library/Application Support/obs-studio` (checked: nothing in Ryan's
  folder changed). powermetrics: `sudo -n` works on the M2s (obs-bench
  runs it itself); on exonbook sudo needs the password, so start it from
  Ryan's Mac first and pass the log: `ssh exonbook "sudo -S -p '' sh -c
  'nohup /usr/bin/powermetrics -i 1000 -n 4500 -f plist --samplers
  cpu_power,gpu_power,tasks --show-process-gpu --show-process-energy -o
  ~/isocast-scratch/obs-bench/pm.plist </dev/null >/dev/null
  2>&1 &'" < ~/.config/isocast-testmac/exonbook-password` (stop it the
  same way with `pkill -x powermetrics`; ~1 GB an hour). A backgrounded
  `ssh ... sudo -S powermetrics` (no nohup) once ended after a minute with
  ssh exit 255; the nohup form above ran for the whole benchmark.

- 2026-10-12: Desktop test bridge now declares `ProcessType=Interactive`.
  Its former default daemon policy coalesced even a standalone 10 ms sleep
  to ~75–85 ms (including threads requesting user-interactive QoS); the
  corrected bridge measured ~13–14 ms. Reload only with its job queue idle.
  `launchctl print gui/$(id -u)/dev.isocast.testmac.bridge` must show
  `spawn type = interactive (4)`. The installed bridge on exonbook was
  updated; fresh installs get the corrected plist.

- 2026-10-12: Test Mac: exonbook (MacBook Pro, M4 Max, 36 GB, macOS 26.6;
  Ryan's Mac is an M5 Max). Use it for builds, tests, benchmarks and
  anything that opens windows, so nothing runs on or steals focus from
  Ryan's Mac: `scripts/testmac.sh build | run '<cmd>' | gui '<cmd>'` (run =
  over SSH; gui = inside the desktop session via the launch-agent bridge in
  scripts/testmac-bridge.sh, needed for windows, `open` and cua-driver).
  Checkout ~/dev/isocast there (tracked files rsynced, uncommitted edits
  included), scratch ~/isocast-scratch (ISOCAST_SCRATCH, used by bench.py and
  drag-lag.py). The login keychain is locked over SSH, so the dev app is
  signed from ~/.config/isocast-testmac/signing.keychain-db (self-signed
  "Isocast Test Signing", unlocked by testmac.sh); its screen recording,
  camera and microphone grants (Ryan, 2026-10-12) are tied to that
  certificate and survive rebuilds. CuaDriver there has Accessibility and
  Screen Recording: `scripts/testmac.sh gui '/Applications/CuaDriver.app/Contents/MacOS/cua-driver call <tool> <json>'`
  (start its daemon with `open -g -n -a CuaDriver --args serve`). No
  `timeout` command there. Main's build for A/B: `git archive main | pssh
  exonbook 'tar -x -C ~/dev/isocast-main'` and build it there.
- 2026-10-12: Two more test Macs on AWS (Ryan asked; AWS profile
  `anomaly`, region us-east-2): `isocast-m2-a` (macOS 27.0, us-east-2b) and
  `isocast-m2-b` (macOS 26.7, us-east-2c); their instance and host ids:
  `aws ec2 describe-instances --profile anomaly --region us-east-2
  --filters Name=tag-key,Values=isocast`. mac2-m2.metal (base M2, 8 cores, 24
  GB), 300 GB gp3, no camera or microphone (synthetic sources only). The
  low-end and clean-machine checks: performance floor, first run on a fresh
  macOS, two OS versions. Use `ISOCAST_TESTMAC=isocast-m2-a
  scripts/testmac.sh …`. Security group isocast-testmac-sg allows SSH only
  from Ryan's IP (add the new one if his IP changes); Screen Sharing goes through an SSH tunnel (`scripts/testmac.sh
  vnc`, the ec2-user password is in ~/.config/isocast-testmac/<host>-password
  on Ryan's Mac). Prepared with `scripts/testmac.sh setup` and `desktop`.
  Dedicated hosts bill per hour (24 h minimum) until released: stopping the
  instances doesn't stop the bill; release with `aws ec2 terminate-instances`
  then `aws ec2 release-hosts --host-ids …` (ask Ryan first). The public IPs
  change if an instance is stopped; update ~/.ssh/config then.
- 2026-10-15: The two M2s can't reach each other (m2-a -> m2-b:22 times
  out: the security group admits ssh only from Ryan's IP). `hw-batch.py
  soak --receiver m2-b` relays through the runner (ssh -R on m2-a to m2-b's
  sshd, a per-job `restrict,port-forwarding` key on m2-b, removed after);
  ~150 Mb/s, so a 6 Mb/s stream is fine, but it needs Ryan's Mac and home
  connection up. The hardware soak (`soak.py --profile hardware`) fails on
  ISO drops while #41 stands (~40% on both ISOs with the stream on, from
  the first seconds; program and stream 0).
- 2026-10-15: The M2s are build hosts too: `scripts/remote-build.sh`
  picks exonbook, isocast-m2-a or isocast-m2-b (auto, sticky per lane,
  never a Mac a hardware job holds; docs/handoff/README.md section 7).
  Lanes live in ~/isocast-builds there as on exonbook; same Rust 1.99 and
  SDK 27.0, so binaries are interchangeable (m2-b's passed e2e record/iso
  strict on m2-a, run on exonbook and Ryan's Mac). The M2s idle at load
  2-3 (WirelessRadioManagerd ~100% CPU) and their clocks are UTC.
  exonbook peaked at load 80 from other lanes' builds that evening.
- 2026-10-12: The test-Mac bridge must be a ProcessType Interactive launch
  agent (Astra found it): with launchd's default policy its jobs' timers
  overslept 5-8x (a 10 ms sleep took 75-85 ms on exonbook), so every
  benchmark run through `testmac.sh gui` before this showed fake drops
  (late ticks upstream of the encoders), including the first exonbook A/Bs
  and M2 baselines. Now a 10 ms sleep takes ~15 ms there, the same as over
  SSH (macOS's normal timer slack; 12.4 ms on Ryan's Mac). Reload a bridge
  with `sudo launchctl bootout gui/$(id -u)/dev.isocast.testmac.bridge;
  sudo launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.isocast.testmac.bridge.plist`
  (over SSH, when no job is running; exonbook's sudo password is in
  ~/.config/isocast-testmac/exonbook-password on Ryan's Mac).
- 2026-10-12: Base M2 baselines (main, synthetic, interactive bridge, 3x
  24 s each): 4K60 desktop + 1080p60 camera ISOs + 1080p60 program record
  with zero drops on both M2s (9/9 files); adding a 4K60 program instead
  loses ~27% of frames in every file (~1050/1440) plus ISO audio dropouts.
  So the base-M2 floor for Ryan's flagship is a 1080p program. (An
  apparent 54% vs 35% daemon CPU gap between macOS 27.0 and 26.7 was
  noise: matched A/B shows 51.0% vs 49.95%; only Apple's VT services cost
  ~16% more on 27. See the CPU-comparison entry below.)
- 2026-10-12: The AWS M2s' headless display is 1024x768 at most (no
  higher modes without a connected viewer); fine for engine tests and
  off-screen snapshots, small for real-window UI tests (use exonbook,
  1920x1080).
- 2026-10-12: exonbook benchmarks differ from Ryan's Mac: main sometimes
  drops ~340 frames per file in the synthetic flagship. A hardware-capacity
  explanation is not established: some identical runs are clean, and
  audio configuration affects even video-only outputs. A connected
  screen-sharing session competes for the encoders. Check `pgrep
  screensharingd` before benchmarking; compare builds A/B on this machine.

- 2026-10-11: Test devices Ryan approved: a display (or window) capture,
  "Studio Audio" and the Elgato mic ("Wave") for audio, "Studio Video" for
  the camera. Don't use the iPhone screen for now. Real-device test runs use
  `examples/recording.toml` patched to drop `iphone` (or a scratch setup
  with those devices).
- 2026-10-11: Ryan's devices, confirmed: "Studio Video" (his primary camera,
  an NDI virtual camera, 1920×1080 BGRA 60 fps), "Studio Audio" (his XLR
  mic over NDI, 48 kHz stereo), and "Ryan iPhone" (the iPhone screen over
  USB, 1206×2622 portrait at 60 fps; it reports no formats, so its size comes
  from its first frame). He allowed looking at Studio Video frames for
  checks.
- 2026-10-11: Disk speeds measured by the recorder's probe: Extreme SSD
  (exFAT, USB) ~850–880 MB/s. `/Volumes/obs-recording` is an SMB share on
  10.0.10.149; don't write test files there.
- 2026-10-11: This is Ryan's work machine (M5 Max, 18 cores, 48 GB). Keep
  builds light: run them as `nice -n 10 cargo …`, and a local, uncommitted
  `.cargo/config.toml` (excluded in `.git/info/exclude`) caps them at
  `jobs = 8`. Use debug builds unless a task needs release performance. Run
  recordings at normal priority (deprioritizing them causes fake drops). Don't
  run builds in parallel subagents.
- 2026-10-11: The internal disk is 99% full (~31 GiB free). `target/` is a
  symlink to `/Volumes/Extreme SSD/isocast-target` (exFAT, USB, ~680 GiB
  free), so build output and `target/scratch/` recordings live on that SSD.
  If the SSD isn't mounted, builds fail on the dangling link: ask Ryan to
  reconnect it, don't build onto the internal disk. Never write large files
  to the internal disk.
- 2026-10-11: Dev machine is an Apple M5 Max running macOS 27.2, with Rust
  1.99 (rustup via Homebrew) and FFmpeg at `/opt/homebrew/bin/ffmpeg`.
- 2026-10-11: `isocast devices` on this Mac: two "LG HDR 4K" displays (3840×2160
  panels in a scaled HiDPI mode, so CoreGraphics reports 5120×2880 pixels;
  main is "LG HDR 4K (2)"); cameras Insta360 X3 (UVC, 1080p30), MacBook Pro
  Camera, "Ryan iPhone Camera" (Continuity, up to 1920×1440 at 30/60 fps),
  Desk View cameras, and virtual cameras (OBS, NDI, "Studio Video"); mics
  Elgato Wave:3, MacBook Pro Microphone (the system default input!), iPhone,
  Insta360 X3, CalDigit TS5 Plus rear input, and virtual NDI/Studio audio.
  No capture card was connected. Screen recording, camera and microphone
  permission were all already granted to the app running the agent.
- 2026-10-11: Ryan's devices, from his OBS scene collection and the ISO files
  in `~/dev/obs-recordings`: the facecam is a capture card ("Capture Card
  Device"; FACECAM files are HEVC 1920×1080 60 fps Rec. 709 with stereo
  AAC); "iPhone Record" is the iPhone's **screen over USB**, not its camera
  (IPHONE files are 1152×2504 portrait at 60 fps); the main display at
  3840×2160; a microphone; system audio.
- 2026-10-11: Ryan's current OBS recordings live in `~/dev/obs-recordings`:
  `YYYY-MM-DD hh-mm-ss.mov` (program, HEVC 3840×2160 60 fps) plus
  `FACECAM-…`, `IPHONE-…` ISO files from the Source Record plugin (HEVC,
  quality 70, mic audio attached). FilmCraft currently lines these up by
  audio; Isocast's `session.json` should make that unnecessary.
- 2026-10-11: macOS grants screen, camera and microphone permission to the
  app that launches the process (for example the terminal running Claude
  Code). If capture returns nothing or black frames, check System Settings →
  Privacy & Security for that app, and ask Ryan to grant it.

## Waiting on Ryan

- 2026-10-18 (#105 W.5d2 correction): no request for an Elgato4K S signal is
  outstanding. That was the wrong MF device entry; Ryan identified Cam Link4K
  as the physical device with live HDMI. SharedMJPG capture and safe
  reader-loss/reopen are now qualified. Physical unplug/replug remains
  deferred, not a request to Ryan; never interrupt camera-ndi-bridge/chatui
  or change drivers to complete that gate.

- 2026-10-17 (#104): NLE export checked in DaVinci Resolve only. Final Cut
  Pro's trial now comes from the App Store (needs an Apple ID) and Premiere
  Pro needs a paid licence, so neither was opened. To check: on a Mac with
  them, `isocast timeline export --nle fcpxml` (File › Import › XML in
  Final Cut) and `isocast timeline export --nle xmeml` (File › Import in
  Premiere), then compare frames at each cut with the EDITED file. Watch
  Premiere's Basic Motion scale (written in the picture's own pixels) and
  stereo audio tracks (one stereo clip per file track).
- 2026-10-17 (#106): **Refresh 0.0.6's update notes.** After this branch
  is merged and the snapshot published (the workflow and
  `docs/releases/0.0.6.md` must be on the public main), run
  `gh workflow run refresh-appcast.yml --repo R44VC0RP/isocast --ref main -f version=0.0.6`
  and approve the **release** environment. Check:
  `curl -sL https://github.com/R44VC0RP/isocast/releases/download/v0.0.6/appcast.xml | grep -c 'Projectors, multiview'`
  prints 1. Older releases needn't be refreshed: apps only read the
  latest release's appcast. From 0.0.7 on, write `docs/releases/X.Y.Z.md`
  before tagging (docs/releasing.md, "Cut a release").

- 2026-10-17 (#74): **Touch Portal / Stream Deck pairing check** not done
  by an agent: Touch Portal is a GUI app whose first launch asks to accept
  its licence and to pair a phone (or a Stream Deck), answers only Ryan
  can give. The server side is covered by the e2e and the scratch client
  in /Volumes/IsocastBuild/followups-007/scratch/obsws.py. To check:
  `isocast websocket enable` (note the password), install Touch Portal
  (touch-portal.com) on a test Mac, add its OBS plugin with
  ws://127.0.0.1:4455 and that password, and press a "Start Recording"
  and a "Switch Scene" button; `isocast status` should follow.

- 2026-10-16 (#89): isocast-m2-b has BlackHole 16ch installed (`brew
  install --cask blackhole-16ch`, then `sudo killall coreaudiod` instead
  of the restart the installer asks for). Since then any microphone source
  there, main's default one included, never finishes opening
  (AVCaptureSession, "did not open within 8 s") and bluetoothd's audio XPC
  fails every 10 s; BlackHole is also m2-b's default input now, so a
  `device = "default"` microphone there finds it and hangs instead of
  failing at once. Needs: a reboot of isocast-m2-b (or `brew uninstall
  --cask blackhole-16ch` and a reboot). Blocks nothing of #89 (verified in
  the test VM instead); it may slow other lanes' microphone sources there.

- 2026-10-15 (#67): the two Developer ID profiles exist (tied to the
  2027 release certificate; secrets set) and a notarized Developer ID build
  activated and streamed on isocast-m2-b. Left for Ryan: a QuickTime/Zoom/
  FaceTime look on a Mac with a real display, and coexistence with OBS's
  or NDI's camera (m2-b has none). m2-b still lists old Isocast builds as
  "terminated waiting to uninstall on reboot"; they go at its next reboot.
- 2026-10-14 (motion harness): the test Macs' desktop bridge may not post
  mouse events (no Accessibility), so `scripts/motion/pointer.swift`
  can't replay a scenario's whole path in a real window; window.py falls
  back to one straight cua-driver drag. To get full paths and timed lag:
  on isocast-m2-b, System Settings › Privacy & Security › Accessibility,
  add the bridge's shell (or /usr/bin/swift), then retry
  `ISOCAST_TESTMAC=isocast-m2-b scripts/testmac.sh gui 'scripts/motion.sh --window --bin target/release'`.
  Blocks nothing else.

- 2026-10-14 (8.2h auto-update, #40): the Sparkle update key. Until
  `scripts/sparkle/public-ed-key.txt` holds the real public key, every
  Developer ID `release.sh` build (and the Release workflow's source check)
  stops with "no Sparkle public key"; `--ad-hoc` builds still work, without
  an updater. Ryan runs docs/releasing.md step 5 once (generate_keys,
  commit the public key, `gh secret set SPARKLE_ED_PRIVATE_KEY`), which
  blocks the first hosted run (step 6). Retry check:
  `scripts/sparkle/sparkle.sh public-key && gh secret list --repo R44VC0RP/isocast --env release`.
- 2026-10-14 (8.2e release workflow): the `release` environment exists
  (reviewer R44VC0RP, main and v* only). Remaining: its six secrets
  (docs/releasing.md steps 2–4), then one real hosted run. Tracked in GitHub
  #11. Note: the v0.0.1 tag push queued a Release run waiting for approval;
  it was cancelled (the release was already published, which the workflow
  refuses to touch).
- 2026-10-14 (8.2g release-secrets, #11): readiness checked, nothing blocks
  the first hosted run except Ryan's part. The copy-paste sequence is
  docs/releasing.md steps 2–6: export the .p12, create the team API key,
  `gh secret set` ×6 (secrets still 0 on 2026-10-14), then the first run.
  Since 0.0.1 is published, a run for it now ends green in preflight
  ("nothing to build or draft"); the first real run is a v0.0.2 tag after
  bumping Cargo.toml and publishing the snapshot (step 5). Afterwards
  rotate the app-specific password behind the local `isocast-notary`
  profile (step 6). Full readiness table and dry-run evidence in
  `/Volumes/IsocastBuild/release-secrets/scratch/REPORT.md`.
- 2026-10-14 (A.2 R3 hotkeys): the focused/desktop keyboard check passed on
  isocast-m2-a but not yet on exonbook, which was locked then and stays
  unlocked since 2026-10-14 (handoff §7). Repeat the m2-a check from
  `/Volumes/IsocastBuild/hotkeys-fire/scratch/final-binary.log`'s evidence
  there. Gate: `scripts/testmac.sh gui 'lsappinfo info -only name "$(lsappinfo front)"'`
  must not report loginwindow. GitHub #6.
- 2026-10-14 (A.2 QA-1 #3): the QA Documents clip now has access on m2-a,
  including the isolated QA bundle, so a fresh pending TCC prompt could not
  be reproduced without changing grants. No grants reset or prompts answered.
  On a Mac with fresh grants, run an isolated engine with a media source
  pointing into Documents, then `ISOCAST_SOCKET=<scratch>/d.sock isocast
  snapshot clip-b -o <scratch>/clip.png` while leaving the prompt pending.
  FIFO-blocked native I/O verifies the timeout/status path; this is not a
  claim of a newly denied device/folder grant check.
- 2026-10-13 (hosted-pacing): coordinator publication is needed to distinguish
  inherited timer slack from paravirtualized GPU/VT stalls on the actual VM.
  After merging `agent/hosted-pacing` to main, run
  `scripts/publish-snapshot.sh --push "Bound media pacing and collect hosted diagnostics"`,
  then `gh workflow run e2e.yml --repo R44VC0RP/isocast --ref main`. Read the
  diagnostic step and download `engine-e2e`: `pacing.json` contains native
  1000 x 10 ms sleeps, Metal name, requested process/thread policy, ten seconds
  of compositor CPU/wake/draw/GPU-wait timings, drops and file checks. Task
  policy getters can return zero even for a throttled launchd job; the timer
  measurements are essential. Exact watch/download/inspection commands:
  `/Volumes/IsocastBuild/hosted-pacing/scratch/REPORT.md`. No push or dispatch
  was performed by the branch. Hosted loss/integrity gates are unchanged.

- 2026-10-13 (G.3b investigation, compatibility check only): QuickTime on
  exonbook opens the one-second-rotation HEVC export and draws its picture,
  but its locked desktop exposes no AX window; playback-control routes are
  unavailable and cleanup File > Close was refused. No unlock or permission
  was attempted.
  A paused scratch QuickTime window remains. After unlocking, retry through
  the desktop bridge: `scripts/testmac.sh gui '/Applications/CuaDriver.app/Contents/MacOS/cua-driver call launch_app '\''{"bundle_id":"com.apple.QuickTimePlayerX","urls":["/Users/ryanvogel/isocast-scratch/g3b-segments/rotation2/r0-n1/export/r0-n1 PROGRAM.mov"],"session":"g3b-playback"}'\'''`, then inspect/play/close that window.
  This is not a G.3b qualification: no crash-safe writer was implemented.
  Update 2026-10-14: exonbook stays unlocked now, so this can be retried.
- 2026-10-13 (8.4, branch agent/visual-ci): the visual check's first real
  run on GitHub. The runner's GPU (macOS 15 arm64 VM, paravirtual Metal)
  has not drawn the app yet; only this Mac has. After merging to main:
  `gh workflow run visual.yml -R R44VC0RP/isocast --ref main -f base=main`
  (warms the release cargo cache and must show 15 views, all "unchanged",
  plus a `gpu: ...` line that isn't a CPU device), then
  `gh run download -R R44VC0RP/isocast -n visual-screenshots` and look at
  head/*.png. The judge needs the repo secret OPENCODE_API_KEY (added);
  optional repo variable VISION_MODEL (default opencode/claude-sonnet-5-5).
  To require the check: Settings > Branches > main > Require status checks
  > `isocast/visual-check` (only after one PR has reported it).

- 2026-10-13 (G.5): Ordinary global keys and push-to-talk need no new
  permissions and are verified. Global **media** keys need Input Monitoring
  for the signed daemon; it is absent on isocast-m2-b, so only their warning
  and focused API fallback were verified. No prompt was requested/answered.
  To qualify the passive media tap, grant the test app in System Settings >
  Privacy & Security > Input Monitoring, then restart an isolated daemon
  and bind a media key. Retry (desktop session):
  `ISOCAST_TESTMAC=isocast-m2-b scripts/testmac.sh gui 'export ISOCAST_SOCKET="$ISOCAST_SCRATCH/g5-hotkeys/qualification/d.sock" ISOCAST_SETUP="$ISOCAST_SCRATCH/g5-hotkeys/qualification/setup.toml"; "$ISOCAST_SCRATCH/g5-hotkeys/bin/isocast" hotkeys set media media_play_pause studio.toggle --json'`.
  The global registration should become true; press the real media key
  while another app is frontmost. Cua Driver 0.34 cannot synthesize media
  keys or F13–F20: F13–F20 and numpad were registration-checked, ordinary
  modifier chords were physically dispatched through Cua's desktop route.
- 2026-10-13: **G.1 review fixes, remaining installed checks.** Use the
  updated `scripts/vcam-install.sh`, not older ad-hoc/manual packages. The
  host now preserves camera/audio-input and Wasmtime executable-memory
  exceptions, and each package has a fresh host/extension build version.
  Approval requests exit 2 with instructions instead of waiting forever;
  complete approval then run `scripts/vcam-install.sh --check`. This run
  did not provision, sign with Ryan's identities, copy to /Applications or
  activate/deactivate an extension. Still required: real provisioned host
  launch/activation, camera/mic/screen capture with TCC, installed consumers
  in both open orders, slow-consumer IOSurface lifetime, and an actual
  version replacement. Signature policy and negotiation state were verified
  without publishing a camera; this does not qualify the extension sandbox.

- 2026-10-13: **G.1b / Follow-up 1 — Ryan's one-command installation:** from
  this worktree run **`scripts/vcam-install.sh`**. Accept the terminal's
  separate account-provisioning, /Applications-copy and activation steps,
  then enable **Isocast Virtual Camera** in System Settings → General →
  Login Items & Extensions → Camera Extensions. It builds with Xcode
  automatic Apple Development signing, team **9G68SMNHEU**, and verifies
  exact-id profiles for `dev.isocast.isocastd` and `.camera` before copying.
  Bare codesigning without those profiles is no longer the install recipe.
  The command then checks AVFoundation discovery and changing frames using
  an isolated synthetic engine; no default daemon/setup is restarted.
  If a restart/ordinary Camera permission is needed, follow the message
  and run `scripts/vcam-install.sh --check`. Preview with `--dry-run`;
  removal is `scripts/vcam-uninstall.sh` (each system step confirmed).
  Account updates, real-identity signing, /Applications copy and activation
  remain **unexecuted**, per this follow-up's limits. The unsigned/ad-hoc
  Xcode build and dry-run are the completed preparation; G.1b/A.3 and true
  installed-client latency/multi-client retention still need Ryan. See
  docs/virtual-camera.md and REPORT.md's Follow-up 1. Studio NDI is not an
  install/removal target and its exact ids/groups are refused.

- 2026-10-12: isocast-m2-b shows several "Isocast Dev is requesting to
  bypass the system private window picker" prompts (macOS 26 direct
  capture consent) for the N.2.D worktree's dev app
  (~/isocast-scratch/wf_1a0bdae5-ee6-5/Isocast Dev.app), raised when the
  first run's Create setup made a display source there. Agents don't
  answer privacy prompts; Allow (or close) them in Screen Sharing
  (`scripts/testmac.sh vnc` with ISOCAST_TESTMAC=isocast-m2-b). Blocks
  nothing; they cover the middle of the screen for gui jobs.
- 2026-10-12: exonbook's screen was locked, so the M4/P3 real-window
  checks (N.2.E drag-lag web vs ui, colours, CPU) ran on the M2 test Macs
  instead. exonbook stays unlocked since 2026-10-14 (handoff §7), so they
  can run there now: check the lock as in handoff §7, then
  `scripts/testmac.sh gui 'python3 scripts/drag-lag.py --app ui'`.
- 2026-10-12: M5 requalification after the M4 encoder rollback requires an
  approved M5 test window/device. The M4 brief prohibits benchmarks on
  Ryan's Mac and permits synthetic sources only; earlier M5 zero-drop
  results do not qualify the new fallback's capture-buffer retention.
  Retry on an approved M5: `python3 scripts/bench.py --work
  /Volumes/IsocastBuild/astra-record/bench --daemon target/release/isocastd
  --scenario flagship --runs 4 --seconds 20` (synthetic first; real rig
  requires its own permission).

- 2026-10-12: use.computer (a second Mac): parked by Ryan for now.
- 2026-10-12: exonbook's lid is closed, so its camera is off (camera
  sources there produce no frames). Real-camera tests on a test Mac need
  the lid open; until then they run on Ryan's Mac with the approved devices
  (Studio Video/Studio Audio, daemon only). Retry: `ssh exonbook 'ioreg -r
  -k AppleClamshellState -d 4 | grep AppleClamshellState'` says No.


## Ready for Ryan

- 2026-10-16 (#99 default): **release notes line for 0.0.6:** "Changes
  recorded audio for interfaces with more than two inputs: a microphone
  source without `channels` now records inputs 1 + 2 as stereo instead of
  macOS's mix of every input. Pick other inputs with `channels` (or the
  source's Channels menu). Devices with one or two inputs are unchanged."

- 2026-10-15 (#71): **OBS tools can drive Isocast.** `isocast websocket
  enable` prints ws://127.0.0.1:4455 and a password once; enter both in the
  tool's OBS WebSocket settings (Touch Portal, Streamer.bot, Companion,
  obs-cmd). Elgato's official Stream Deck OBS actions use their own OBS
  plugin, not obs-websocket, so they don't connect.

- 2026-10-14 (motion harness, #24): `scripts/motion.sh` (after
  `cargo build --release -p isocast-ui -p isocast-daemon -p isocast-cli`)
  runs the motion suite headless in ~3 minutes and writes a table plus a
  contact sheet per failing run; docs/motion-testing.md says what each
  number means. It currently fails on main as #24 says it should.

- 2026-10-14 (8.2): **Isocast 0.0.2 is published**, the first release that
  updates itself: https://github.com/R44VC0RP/isocast/releases/tag/v0.0.2
  (full release, Latest). Built locally from main 33b9b73 (public snapshot
  a681a63), notarized with isocast-notary, appcast signed with the login
  keychain's Sparkle key. DMG SHA-256 matches its .sha256; Gatekeeper
  accepts it; releases/latest/download/appcast.xml resolves (200) and points
  at the v0.0.2 DMG with the right length. The tag push's Release run ended
  green at preflight (already published). Install it by hand once; the #41
  ISO-drop and #24 resize fixes will arrive as 0.0.3 through the updater.

- 2026-10-14 (8.2h auto-update, #40): branch `ui/auto-update`. Testers
  on 0.0.1 have no updater: they install 0.0.2 from its DMG once; from
  then on each published release reaches them (daily check or Isocast ›
  Check for Updates…). Before the first 0.0.2 run, do docs/releasing.md
  step 5 (Sparkle key). Evidence and screenshots:
  `/Volumes/IsocastBuild/auto-update/scratch/REPORT.md`.
- 2026-10-14 (8.2): **Isocast 0.0.1 Alpha is published** (prerelease, at
  Ryan's request): https://github.com/R44VC0RP/isocast/releases/tag/v0.0.1
  Built from main 70933b7 (Send Report and the live report service
  included), tag v0.0.1 on the public snapshot 77ceb13. DMG SHA-256
  `16a86d302985d9ee6860bee9fcbccd0fa0d77ea9dd700974dda1574f2b57d917`,
  66,878,397 bytes; notarized, stapled, Gatekeeper accepted, and the
  downloaded asset re-hashed to match. Install it like a user: download the
  DMG, drag Isocast to Applications. Evidence:
  `/Volumes/IsocastBuild/release/scratch/REPORT.md` "Rebuild for
  publishing". The optional camera extension still needs its one-time install.

- 2026-10-13 — The Slint app as Isocast.app (N.3, branch ui/app-alpha):
  `scripts/app-dev.sh --release` then `open ~/Applications/Isocast.app`
  (it replaces the web app's bundle there; same id and icon).

- 2026-10-13 — **A.6 adversarial review fixes supersede the original
  handoff.** Panics are contained, poisoned-state emergency stops finalize
  media, logging failures do not gate startup, and reports are compact and
  paged. `isocast report --preview --private --json` returns an index and
  preview ID; `isocast report-file ID ISSUE.md --json` reads it. No upload.
  The remaining strict CPU claim is +0.1 point in the whole-daemon 6+6
  against current main; private AVAssetWriter counters and the redesigned
  app menu retain the documented availability/integration boundary.

- 2026-10-13 — **A.6 diagnostics backend**, branch agent/a6-diagnostics:
  after merging/building, `isocast report --preview --json` shows everything
  the private offline zip would contain. CLI/MCP methods are ready for the
  redesigned app's menu. The parent A.6 box stays open: strict CPU
  no-regression is not established (+0.7 points of one core), and exact
  AVAssetWriter-internal queue/latency counters are unavailable through its
  public API. No report is uploaded and the repository URL remains unset.
- 2026-10-13 — Pause recording (G.3, branch `agent/g3-pause`): while
  recording, `isocast pause --json`, then `isocast resume --json`. The take
  and every file stay the same, REC time freezes, and a concurrent stream
  continues. `live.paused` and `live.paused_seconds` explain the state;
  stop while paused finalizes, and split/retake ask you to resume first.
  MCP: `pause_recording` / `resume_recording`. The native app's timer follows
  the pause state; adding its pause button belongs to the controls work.
  Hotkeys: `isocast hotkeys set pause 'ctrl+alt+cmd+p' recording.pause_toggle`
  (also `recording.pause`, `recording.resume`). Recoverable duration after a
  crash is unknown: manifest `recoverable_seconds_at_pause: null`, including
  after Resume. Pause/Resume replies do not acknowledge committed media.

- 2026-10-14 — The Create view (C.2, branch ui/create-view): in the app,
  View → Create (⌘⇧C). New Timeline, record a few takes, Retake, drag a
  clip, Export. To see it without a window: `isocast-ui --snapshot
  out.png --size 1440x900 --scale 2 --state create --state wait:800
  --state create-thumbs` against an engine with a timeline.
- 2026-10-13 — Repo docs and release files (8.1, 8.3, branch
  agent/docs-release): read README.md as a visitor would (the screenshots
  are real, from the Console redesign), then `git diff main --stat`. To
  retake the screenshots from main once the redesign merges:
  `export ISOCAST_TESTMAC=isocast-m2-b; scripts/testmac.sh build &&
  scripts/screenshots/take.sh`.
- 2026-10-13 — Create timeline MVP (C.1, branch engine/timeline-mvp):
  once merged and built, `isocast timeline new "My video"`, then record
  takes as usual (`isocast start` … `isocast stop`, or `isocast split`
  mid-recording): each lands at the end of the timeline. `isocast retake`
  redoes the last one (mid-recording it drops the take in progress and
  returns once the new take has begun, about a quarter second later: start
  your line then), `isocast timeline show` lists the clips, `isocast timeline
  remove c2` and `isocast timeline move c3 1` edit them (files are never
  deleted), and `isocast timeline export` writes `My video PROGRAM.mov`
  plus one file per ISO source next to the takes. Agents: new_timeline,
  retake, export_timeline and friends over MCP. The app has no view yet
  (C.2).
- 2026-10-13 — Engine hotkeys (G.5, `agent/g5-hotkeys`): after merging,
  `isocast hotkeys defaults` installs the opt-in Control–Option–Command
  shortcuts (R record, S split, M marker, D studio, Return take), global
  even with no Isocast window. `docs/hotkeys.md` covers bindings and the
  client integration. The native Settings page/focused-input bridge and
  consuming the daemon's Studio Mode state belong to the app-redesign
  follow-up; no native UI behavior was replaced in this branch.
- 2026-10-13 — The Console redesign of the native app (N.5, branch
  ui/console-integrate, not merged): `open "/Volumes/IsocastBuild/console-fix/Isocast Console.app"`
  (a release build of the branch, bundled by scripts/ui-dev.sh; it talks
  to your own isocastd and live setup, as the web app does). Final Cut's
  frame, Logic's LCD and Tower's capsules from the "C. Console" Paper
  boards: try the Welcome screen (no setup), Studio Mode, Settings, a
  right-click on a scene, a strip's ⋯. Text is drawn a cut lighter than
  Paper's weights to match the boards (Skia draws heavier). Taste calls
  are in Questions for Ryan; screenshots in .claude/artifacts/console-app.
  To rebuild it yourself: `CARGO_TARGET_DIR=/Volumes/IsocastBuild/console-fix/target ISOCAST_UI_APP="/Volumes/IsocastBuild/console-fix/Isocast Console.app" scripts/ui-dev.sh --release`
  from the worktree.

- 2026-10-13 — Profile and Scene Collection menus (P.2, branch
  app/p2-menus): once merged and built, both apps have them, as in OBS.
  Try Scene Collection → Duplicate…, change the copy, then switch back
  and forth from the list at the bottom of the menu; Export… writes the
  one in use to a .toml you can Import… on another Mac (Import… also
  takes an OBS scene collection .json or an OBS profile's basic.ini).
  `isocast collection list` shows the same. Open Setup File… (⌘O) and
  Import from OBS… are now folded into Import…, which adds to the library
  instead of replacing the collection in use.
- 2026-10-13 — Animations (5.2a, branch scenes/5.2a-keyframes): once
  merged and built, add a keyframe set and play it on an item, e.g.
  `[[keyframes]]` `id = "slide-in"`, `frames = [{ at = 0.0, opacity =
  0.0, translate = [-40, 0] }, { at = 1.0, opacity = 1.0, translate = [0,
  0] }]`, and on the facecam item `animations = [{ keyframes =
  "slide-in", duration_ms = 400, easing = { cubic_bezier = [0.2, 0.8,
  0.2, 1.0] } }]`: it slides in each time its scene goes on air
  (`start = "clock"` for loops that never restart, `iterations =
  "infinite"`, `direction = "alternate"`). `isocast setup check` names
  any mistake's field. The apps don't edit animations yet.

- 2026-10-13 — Live text (5.5a, branch scenes/5.5a-data): once merged and
  built, give a scene an item like `{ id = "title", content = { kind =
  "text", data = "title", size = 72 }, layout = { position = "absolute",
  left = 64, bottom = 64 } }` (or set `data = "title"` on a text source in
  place of `text`), then `isocast data set title "Hello"` changes it at
  the next frame, with no setup change; `--persist` keeps it across
  restarts with the scene collection. Agents do the same with MCP
  set_data.
- 2026-10-13 — WebAssembly audio filters (7.4a, branch
  plugins/7.4a-wasm-audio): once merged and built, `isocast plugin add
  examples/plugins/gain --allow`, then give a source `audio_filters = [{
  kind = "gain/gain", gain_db = -6 }]` (or `{ kind = "gain/ducker",
  sidechain_source = "mic" }` on music). `isocast status` shows what each
  costs per 10 ms block; `fault = "trap"` on the gain shows a broken
  plugin being contained (warnings, no gap, off after three).

- 2026-10-13 — Plugins (7.1, branch plugins/7.1-registry): once merged
  and built, `isocast plugin add examples/plugins/deck` shows what the
  deck asks for; add `--allow` to install it, then open
  http://127.0.0.1:7650/ (its log, from `isocast plugin list`, says the
  address) for a button per scene, Record, Marker and Split. `isocast
  plugin remove deck` takes it away. It runs sandboxed and can only call
  the eight methods it was granted.

- 2026-10-12 (evening) — Installed main cad6095 (app and signed engine;
  your grants carried over, doctor clean): `open "$HOME/Library/Application
  Support/Isocast/Isocast.app"`. New since this morning: the cleaner
  look (docs/design.md; taste calls to overrule if you dislike them, one
  token each: neutral grey selection with a copper bar instead of brown,
  grey menu hover instead of copper, neutral faders), keyboard navigation
  in Scenes/Sources/menus/dialogs, quality presets Low to Incredible in
  Settings › Output, 37 hard-test fixes (crash-interrupted recordings are
  reported, stop during warm-up works, OBS import keeps your recording
  tracks, stream reconnect loop fixed…). Your setup's stream says
  "twitch" but its server is rtmps://ca.pscp.tv/x (X/Periscope): check
  Settings › Stream. Test Macs: exonbook, isocast-m2-a/-b (docs/notes.md
  Environment); the AWS hosts bill until released.

- 2026-10-12 — Version one, first look: open the app,
  `open "$HOME/Library/Application Support/Isocast/Isocast.app"` (release
  builds of the app and the signed engine are installed; your live setup
  is empty, so it starts on the welcome screen). "Start with this Mac"
  makes Screen + Camera / Camera / Screen; it prefers a real camera, so it
  picks the Insta360 over Studio Video until the onboarding's device step
  lands — or from a terminal: `target/release/isocast setup init --camera
  "Studio Video" --microphone "Studio Audio" --apply`. Then drag things
  around, switch scenes (1–3), try Studio, press Record. Read
  docs/getting-started.md. Recordings go to ~/Movies/Isocast, but the
  internal disk has only ~12 GB free (about 17 minutes of your rig's
  screen + camera + program), so point it at the SSD: add `--folder
  "/Volumes/Extreme SSD/Isocast"` to `setup init`, or change Recordings
  in the inspector's Setup section.

- 2026-10-11: Audible monitoring check (4.13b). Lower the volume, then
  apply a setup with `[audio] monitoring_device = "default"` and a tone
  source with `audio = { gain_db = -30.0, monitor = "monitor_and_output" }`,
  `isocast start --seconds 30`, and try `isocast audio <tone> --balance -1`
  (left ear only), `--monitor off` (silence), `--monitor monitor_only`
  (heard, not in the program). `isocast status` shows the monitor line.

- 2026-10-11 — Milestone (phase 3 done): Isocast can replace OBS for your
  recordings: ISO files, the 4K60 program with a mixed track, scenes with
  cuts and fades, preview and take, markers, splits, and agents can see
  the program. A one-hour soak held sync to 0.02 ms. Record your rig
  through the daemon, to the SSD:
  `cargo build && scripts/sign-dev.sh && target/debug/isocast restart &&
  target/debug/isocast start examples/recording.toml --folder
  "/Volumes/Extreme SSD/isocast-test"`, then `target/debug/isocast scene
  closeup`, `target/debug/isocast marker intro`, `target/debug/isocast
  snapshot -o program.png`, and `target/debug/isocast stop`. Keep the
  iPhone unlocked. `target/debug/isocast audio mic --gain -3` or `--mute`
  changes the program mix live; `isocast status` shows the levels;
  `isocast preview closeup` then `isocast take --fade 500` switches with a
  fade.

## Questions for Ryan

- 2026-10-15 (#18): with a `[stream]` in the profile the replay buffer
  always encodes as the stream does (its size, bitrate, 2 s keyframes),
  even while not streaming, so it can share the stream's encoder: a profile
  streaming 720p/6 Mb/s gets 720p replays of a 4K program. Keep that, or
  use the stream's format only while a stream runs (a
  `recording.replay.follow_stream` switch)? Nothing waits on it.

- 2026-10-15 (#18): may `replay/mp4.rs` and the replay ring's trimming get
  unit tests (box layout, edit lists, keyframe/memory trimming with
  synthetic samples)? They were verified with real saves (ffprobe, ffmpeg
  -xerror, AVFoundation decoding every sample) and the e2e `replay`
  scenario only. Nothing waits on it.

- 2026-10-15 (#61): may the EBML reader (`matroska.rs`) get unit tests
  with tiny byte-level fixtures built in the test (lacing kinds,
  unknown-size clusters, truncated files, BlockAdditions, Cues)? It was
  verified with real ffmpeg files only. Nothing waits on it.

- 2026-10-11: Ryan's primary camera is "Studio Video" and his mic "Studio
  Audio" (XLR, network); `examples/recording.toml` now uses them instead of
  the OBS collection's "Capture Card" facecam. Asked whether a capture card
  (HDMI capture device) is still used for anything, e.g. a second camera or
  a console, or can be dropped from the rig. Nothing waits on it.

## Benchmarks

- 2026-10-15 (#3 default-on, final runtime b240e043, main 505145d baseline):
  M2-b, `bench.py flagship-1080`, three interleaved 20 s pairs: daemon CPU
  31.1% (31.0–31.1) → 25.7% (25.7–25.8), instructions 557.6 → 486.1 M/s,
  VT CPU 13.7% both, RSS 121 → 138 MB, disk 251.1 → 246.4 MB, billed energy
  0.20 → 0.19 J (tiny rusage counter, not machine power). Zero losses, 18/18
  complete; both benchmark batches' 36 movies decode. Final M4 record-first
  #68 matrix 35/35 files, ISO/program/stream 0/0/0; M2 fitting/heavy split
  matrix 39/39, all protected outputs and audio loss-free, overloaded ISOs
  still shed. Final kill matrix 7/7 + ownership 11/11: max video loss .454 s,
  audio .635 s, next-launch repair 104–132 ms; earlier 21-case matrix .512 s
  video/.682 s audio, 101–167 ms repair. Empty tracks omitted by recovery are
  not lost packets: the initial checker falsely failed "None of 0"; corrected
  checks pass on the exact retained files. Raw failing reports remain.
  HEVC 10-bit, all ProRes variants, WebM playback and Create thumbnails pass;
  full strict e2e 16/16 once, final affected scenarios 5/5, workspace 201 tests.
  Evidence: `/Volumes/IsocastBuild/g3b-crash-safe/scratch/REPORT.md`, jobs
  20261015-202302-g3b-005-cmd and 20261015-205333-g3b-start-fix-cmd. No m2-a use.

- 2026-10-14: **G.3b crash-safe prototype (branch `agent/g3b-crash-safe`)**,
  isocast-m2-a (base M2, macOS 27.0), synthetic sources, common lock, no
  screensharingd. Kill matrix (`kill -9` at random media times, during a
  pause, right after start, 160 ms into a split's new take, after resume;
  HEVC/H.264/ProRes 422, MOV/MP4; 640×360/30 ISO + program; isolated
  engines; recovery by `isocast restart` on the folder): flag on, 15 runs /
  36 files: **36/36 decode fully (`ffmpeg -xerror`) after recovery, 36/36
  unfinished files read by AVAssetReader before it**; video loss vs the
  last status ≤ **0.499 s** (median 0.146), program audio ≤ 0.681 s
  (median 0.209); a kill during a pause recovers the video exactly to the
  cut and the audio exactly to the stated `recoverable_seconds_at_pause`
  (cut − 47 ms); recovery 135–157 ms including the restart; pause replies
  260–269 ms (25–86 ms with AVAssetWriter). Same 25 cases with the flag
  off: 30/60 files unreadable (every take under ~2 s), loss up to 2.161 s,
  a pause kill loses 1.9 s. Fix round 1 (review repros re-run on the
  fixed build, `/Volumes/IsocastBuild/g3b-crash-safe/scratch/REPORT.md`
  "Fix round 1"): the reviewer's four SIGSTOP stall cases now finish with
  `mdhd` = `stts` = edit = ffprobe video end = AVFoundation decoded end
  (3.983/4.333/5.683/5.100 s, gaps of 39000–59000 ticks kept in the
  table); their 28-run matrix: 48/48 picture-bearing files decode in
  ffmpeg with every AVFoundation track complete (15 files were killed
  before any frame); the EIO write, fsync-at-pause and fsync-at-split
  injections fail the file loudly (pause reply `errors`, manifest) and
  the file is finished from its fragments; the torn 8-byte header patch
  is recovered. Fix round 2 (re-review repros): the VT drop interposer's
  injected drops reach the manifest (84/6, 89/1), all 17 torn header
  states (0–16 bytes) recover on real media, the live-alias probe leaves
  the owner's file untouched (refused as a symbolic link), a stall right
  after the first picture leaves no raw `tfdt` gap, the EIO'd file's
  manifest says 1 frame / 77 dropped (the movie's content); flag off vs
  main on `camera-1080` (no 4K ISO, #41): 449.2 vs 448.2 M
  instructions/s, 41.7 vs 41.5 % CPU, 6/6 complete, 0 drops; flag on
  404.8 M/s, +7 MB RSS. A/B `scripts/bench.py` flagship-1080, 3
  interleaved 20 s runs: main e9753d4 vs branch flag off **562.3 vs 562.3
  M instructions/s**, 49.8 vs 50.0 % CPU, 124 vs 126 MB, 0 drops; flag off
  vs on (final build) 563.5 vs **499.9 M instructions/s** (−11 %), 51.7 vs
  44.0 % CPU, VT 29.2 vs 29.9 %, RSS 124 vs **147 MB**, disk 248 vs 221 MB
  per 20 s, sizes 466/215/75 vs 468/209/76 MB/min, 0 drops, 9/9 complete.
  Evidence: /Volumes/IsocastBuild/g3b-crash-safe/scratch/REPORT.md.
- 2026-10-14 (iso-drops, #42-only qualification): baseline 63be94c versus
  10fc1a8 on isocast-m2-a, SSH, synthetic benchmark clips, 3 interleaved
  pairs/scenario, both common locks, no screen sharing. Healthy flagship-1080:
  zero video/audio drops in 18 files, daemon CPU median 47.8→49.5% of one
  core (ranges overlap), instructions 563.1→559.8 M/s. Flagship+stream:
  program and stream remain zero-drop; ISO losses per run [2562,2560,2566]
  → [2567,2562,2552], median 2562 both, daemon CPU 53.2→53.2%. All 36 files
  ffprobe packet-count/A-V-duration matched before benchmark cleanup. Normal
  e2e 13/13 zero-drop on both M4 and M2. This validates summary-only behavior,
  not a #41 fix. Original six-take/ten-minute #41 M2 qualification is deferred
  until a real throughput candidate exists. Logs in iso-drops scratch.

- 2026-10-14 (process-wide escalation, supersedes the adaptive blocker): same
  reviewer tools, three interleaved ten-second program+two-ISO pairs per cell
  on exonbook and m2-a, compared with e7b4531 merged onto the same main.
  All **162/162** final-build/control files fully decode, have 10 s A/V and
  correct frame accounting; zero audio dropouts. Drop totals, critical ->
  process-wide, default30/Background30/default60/Background60: M4
  **0/331/0/380 -> 0/295/0/199**, M2 **0/6/12/25 -> 0/3/7/22**. M4 Background30
  includes three additional targeted pairs: its initial candidate ISO outlier
  (148/147, program 0) was reproduced in the reference (158/158, program 0).
  All six pairs retained; this is the existing Background ISO throughput
  limit, not a claim that either build is loss-free. Fifty-three of 54
  sampled candidate ISO clocks inherited critical mode; one independently
  detected stretching. Restarting the live engine preserved evidence: all four
  new engine/source clocks and both later ISO clocks inherited critical with
  zero local escalations, three 90-frame/3 s files passed integrity. Strict
  normal E2E **13/13 on both**, fmt/clippy and 180 tests pass. Prior normal
  CPU (+1.4..3.7% medians, overlapping ranges) accepted by the delegator;
  not remeasured or relabelled exact equality. Evidence: hosted-pacing
  REPORT.md **Process-wide escalation**, `scratch/process-wide/summary.json`.

- 2026-10-14 (adaptive pacing, historical ffe3025; superseded above): powermetrics, same reviewer
  `energy.py`/`take.py`, three alternating pairs per state, main 18b8d4c versus
  adaptive; all final normal clocks ordinary, zero recording loss. Median
  daemon CPU ms/s: M4 preview **214.50 -> 219.82**, record **466.39 -> 473.06**;
  M2 preview **266.97 -> 276.74**, record **629.12 -> 644.73**. Ranges overlap;
  final samples contain no active compiler, but other app/system load remains.
  This is much closer than the review's +9–12% preview cost, **not proof of
  exact CPU equality**. Normal strict E2E **13/13 on both Macs**.
  Three interleaved ten-second pairs per throttled cell (always-critical ->
  adaptive, all output drops summed): M4 default30 **0 -> 5** (one ISO short),
  Background30 **0 -> 99**, default60 **0 -> 4**, Background60 **1259 -> 1017**;
  M2 **0 -> 2**, **0 -> 1**, **14 -> 18**, **18 -> 18**, respectively. Do not
  use the overall M4 improvement to hide the failing cells. Qualification is
  blocked. Evidence under hosted-pacing/scratch/adaptive/{m4,m2}/r/{q30,q60,
  qp1,qp2,qp2b,qp3}; original powermetrics plists retained.

- 2026-10-14 (pacing adversarial review, e7b4531): **narrow the original
  qualification.** In the builder's 720p30/1080p60 program+two-ISO matrix,
  critical timers remove 98–100% of launchd loss (M2 default/Background
  1080p60: 891 -> 7 and 2065 -> 15; M4: 1854 -> 0 and 2787 -> 12).
  With heavier Background ISO loads the worst takes improve only 11–77%;
  all three Macs still drop, and strict E2E still fails in throttled contexts.
  M2b's two-hour daemon-context take loses 1.6% despite no drift. The app and
  `isocast restart` did not reproduce the timer defect: this is headless/CI
  robustness, not a normal-user recording fix. Normal preview powermetrics
  measured +12% M4 (224 -> 251 ms/s), +9% M2 (239 -> 260), higher in five of
  six pairs; earlier flagship CPU alone did not establish neutrality.
  Review: `/Volumes/IsocastBuild/pacing-review/scratch/REVIEW.md`.

- 2026-10-13 (hosted-pacing, baseline f12c8b1): default launchd on exonbook
  stretches a 10 ms sleep to p50/p99 79.5/89.9 ms; Background 199.5/210.0 ms.
  Interactive QoS and mach_wait_until do not fix it; time-constraint policy
  fixes default but not Background. Critical kqueue timers work in both;
  shipped timers retain explicit slack of min(remaining/4, 5 ms) for batching.
  One original short M4 SSH/default/Background/Interactive matrix, 720p30 H.264 and
  1080p60 HEVC with program + two ISOs: 24 files, 10,800 frames, 0 drops.
  M2 same matrix: 10,791 frames, 9 reported drops (1 per file in default60,
  2 per file in Background60). All 48 files fully decode, A/V both 10 s.
  Both Macs pass all 11 then-available strict E2E scenarios **over SSH only**
  (pause/report absent); this did not exercise the defect.
  Final normal flagship-1080 bench.py measurements, three interleaved pairs
  with the common lock held for the full batch: M4 median CPU 32.6 -> 30.1%,
  VT 21.9 -> 20.9%, instructions 577.3 -> 582.4 M/s, all 18 files zero-loss.
  Earlier M4 batches were noisier/higher (29.8 -> 30.6% strict timers;
  30.4 -> 35.1% bounded timers with other benchmarks between rows). This is
  not evidence of a CPU speedup. M2 CPU rows are not a performance gate:
  another lane's live app/engine and new builds overlapped despite the lock.
  Report includes every trial, binary hashes, uncertainties and runner steps:
  `/Volumes/IsocastBuild/hosted-pacing/scratch/REPORT.md`.

- 2026-10-14: **A.6 hardening M-1/M-2**, isocast-m2-a, common lock,
  no screensharingd. Reviewer's 3000 distinct 1 KiB failed Keychain writes:
  corrected warmed comparison (initial Keychain call + learned-cache warmup
  + first inventory pass), pinned count **0 -> 0**, logger CPU/error median
  **19.84 -> 21.22 us**, whole-daemon CPU/error **38.01 -> 39.24 us**,
  RSS **47.58 -> 48.12 MB** (0.54 MB residual, not the original linear growth).
  Initial baseline reproduced logger **19.18 -> 66.40 us** and RSS growth;
  cold framework residency is why the final comparison warms startup first.
  SIGKILL relay: warning + automatic new PID + redacted fatal tail. Holding
  the owned scratch helper non-executable during 12000-token churn gives
  **0 pending seed bytes at every sample**, 12000 discarded transport messages;
  daemon RSS **103.37 -> 105.69 MB** during a concurrent 8 s recording,
  both files **240 frames, no drops, clean decode**. Restore executable mode:
  relay reseeds/restarts and retains its redacted native tail. Two real scoped
  Keychain accounts rotated eight times each: pin counts stop at **3/6**;
  current + two previous values redacted in JSONL and native output. Test
  items removed. Reviewer relay harness: 100000/100000 native lines ordered,
  no seed leaks, expected 100 redactions; peer-close idle CPU 0.0 s/5 s.
  Evidence: a6-diagnostics/scratch/hardening and matching m2-a a6-hardening.
- 2026-10-14 (A.2 QA engine): final synthetic engine suite on m2-a,
  release build, common benchmark lock and no screensharingd: 13/13 PASS,
  no skips, strict 1080p60 zero-drop policy, zero video/audio loss reported.
  Additional actual QA clip routing matrix: 18 complete decoded 3 s files,
  90 frames each, matched A/V and zero drops. Mono round-trip: 3 more files,
  L−R RMS 0.149967 → 0 → 0.149995. This is functional qualification, not
  a performance comparison. Report/evidence at
  /Volumes/IsocastBuild/qa-engine/scratch/REPORT.md; app API integration and
  the unqualified fresh TCC-prompt/device check are listed there.

- 2026-10-13: **G.3b feasibility, not a delivered crash-safe writer.** Main
  runtime f6f287d on exonbook M4 Max/macOS 26.6.2, synthetic inputs, common
  lock, no screensharingd. Three interleaved 16 s flagship rounds each of
  ordinary recording / existing split every ~1 s / split every ~2 s:
  daemon CPU medians 26.97/31.97/30.42%, instructions 628.09/1015.84/808.64
  M/s, VT CPU 18.25/25.05/20.91%, RSS 113.84/112.89/111.41 MiB. Stop alone
  80.4/90.1/74.0 ms, then join 284.8/427.1/384.4 ms median. Ordinary and
  1 s runs zero-drop; one 2 s run reports 698 ISO video drops, program and
  all audio remain whole. This measures reuse of public split/timeline/join
  (including its metadata/RPC cost), not a lower bound for every internal
  rolling design. The 1 s schedule's first segment is 2 s; terminal chunks
  are partial. File sizes vary substantially, so no same-quality/size claim.
  All 27 joined MOVs fully decode, have equal A/V durations, and import all
  54 tracks into AVMutableComposition. Existing join copies compressed video
  but decodes/re-encodes AAC: it is not bit-preserving audio assembly.
  Repeated baseline crash matrix: 55 exact cuts, 111/165 files decode, 54
  lack a movie header, every recovery claim remains null. Twenty seeded
  plain kills: 45/60 files decode, 15 lack a header; worst common A/V loss
  2.249517 s, two files exceed 2 s. No 1/2 s guarantee is qualified. E2E:
  12 PASS, bug_report unavailable; fmt/clippy and 180 tests pass. Evidence:
  /Volumes/IsocastBuild/g3b-segments/scratch/REPORT.md and adjacent JSON;
  full media under ~/isocast-scratch/g3b-segments on exonbook.

- 2026-10-13: **A.6 two-hour retention/recording proof**, isocast-m2-b,
  the review setup (640×360/30 test pattern, text, tone), program+ISO and
  10 Hz status polling under the common lock. **7200 s, 216000 frames per
  file, zero video/audio drops**, ffprobe A/V durations both 7200.000 s.
  **67056 polls**, **40706334 log bytes** written over two hours (19.4 MiB/h),
  still within the 40 MiB daemon budget; the prior SIGKILL marker/tail was
  retained at every 30 s checkpoint. This exercised the retention/polling
  fixes while privacy/report work continued, not the later relay's memory.
  Evidence: m2-b `~/isocast-scratch/a6-diagnostics/review-fixes/hours/`,
  copied summary/run/status into the local review-fixes/evidence/m2-b folder.

- 2026-10-13: **A.6 review-fix final whole-daemon 6+6**, isocast-m2-a,
  main **a619986** (includes newer recording/animation/timeline work) versus
  this branch, synthetic flagship-1080, common lock, no screensharingd:
  daemon+descendants CPU **48.9 → 49.0%** of one core; ranges 42.6–49.3
  versus 48.6–49.3 (the low first main run retained, not discarded).
  Instructions **553.1 → 558.7 M/s**, VT **31.1 → 30.6%**, summed RSS
  **109 → 122 MB**, compositor p50 **679.3 → 695.4 µs**, p99 **846.2 →
  899.5 µs**. Both builds **18/18 complete files**, 1440 frames/file,
  **zero video/audio drops**. Report the small nonzero overhead honestly;
  this is whole-build comparison against current main, not an isolated
  claim about each diagnostic component. Evidence: m2-a
  `~/isocast-scratch/a6-diagnostics/review-fixes/ab-main/`.
- 2026-10-13: **A.6 review final functional evidence.** Reviewer scripts:
  contained panic during stream+record finalized **1174 frames / 39.1333 s**
  per file, no decode errors; SIGKILL preserved **420 frames / 14.016 s**,
  now flagged interrupted. A genuinely full log-only image (4 KiB fill,
  zero free bytes) still recorded **810 frames / 27 s**, restarted and
  recorded again; its report retains ENOSPC history and warning entries.
  The original 1 MiB fill left ~170 KiB, enough for the now-compact logger,
  so it no longer induced failures without the smaller fill block.
  Before/after privacy scripts: late secret after 130 hockey/token entries,
  URL path tokens, old client description and all plugin planted secrets
  no longer leak. Chroma green/keyframe interval retained; >512 credentials
  fails closed. Actual Keychain seeding before any stream start passed in
  a local isolated engine, and its uniquely scoped test item was removed.
  Strict previews use different HMAC pseudonyms and hide device identities;
  only system/recording volume retained, actual-executable signing checked.
  Three clean restarts after SIGKILL: warnings **[1,1,1] → [1,0,0]**.
  MCP preview **71,227,715 → 6,815 bytes**; paged response **7,492 bytes**.
  CommonMark recognizes the JSON triage fence after an unclosed user fence.
  Native fatal tail (no newline) survived immediate _exit, with planted
  native credentials absent. Evidence: `review-fixes/` under A.6 scratch.

- 2026-10-13: **A.6 final functional pass** on m2-b: program + ISO each
  240/240 frames over 8 s, zero drops; all 25 GPU samples available;
  open descriptors **12 logged / 12 independently read**, sampler mean
  **983 µs** for this small held engine (not the flagship benchmark).
  Eleven planted credential/title values were absent from on-disk logs
  and uncompressed archive contents, including Bearer/Basic headers,
  uppercase RTMPS, URL password/query, and window-selection error text.
  Relative `--out` resolved from the CLI's cwd; strict-path preview created
  no zip. About 80 MB of API log traffic rotated/pruned to **63,853,294
  bytes**, largest segment **3,976,577 bytes**; every retained line parsed.
  The cursor retired deleted files and the bounded zip kept the newest
  sentinel/error plus native crash evidence (nine old segments explicitly
  omitted). Issue bodies **7,157 / 18,749 characters**, both triage JSON
  blocks parsed. Final crash classification: a fresh worker panic matched
  its launch; a subsequent SIGKILL had no falsely attributed panic, while
  retaining the older crash in history. Results under `final-check/` and
  `final-crash/` beside the A.6 report's remote evidence.

- 2026-10-13: **A.6 diagnostics, isocast-m2-b (M2, macOS 26.7),** isolated
  synthetic flagship-1080, release, common lock, no screensharingd; baseline
  d6c8aec. Three interleaved 4+4 rounds: CPU medians main/candidate
  45.85/46.50, 45.65/46.10, **45.65/46.35% of one core**. Every round:
  **12/12 files complete per build, zero video/audio drops** (1440 frames
  per file, 24 s, 4K60 + 1080p60 ISOs and 1080p60 program). Final round:
  VT 25.0/25.1%, RSS 104/107 MB, retired instructions 552.5/558.8 M/s,
  compositor p50 662/673 µs, p99 969/934 µs. The sampler's steady CPU
  mean is **2142–2346 µs per one-second cycle (0.214–0.235%)**, its median
  1407–1496 µs. All 92 recording samples had real per-process GPU counters.
  Logger CPU ~0.051 s over each ~22 s window (~0.23% of a core; includes
  sampler-written logs, so it is not additive with sampler CPU).
  Native device notifications replaced polling the full inventory every
  five seconds (sixty-second fallback); VT discovery uses `proc_name`
  instead of resolving 719 executable paths (direct check: 7.1 vs 11.8 ms
  in Python, identical VT PIDs). **Strict whole-process no-regression is
  not established.** Stop this optimization branch after three rounds:
  the wrong assumption was that process/device enumeration explained all
  the extra CPU; redaction, serialization and synchronous event writes
  remain. Future work can target that control/event path, not media code.
  Evidence: `~/isocast-scratch/a6-diagnostics/bench{,-final,-qualified}`
  on m2-b; local report/evidence under `/Volumes/IsocastBuild/a6-diagnostics/scratch`.
- 2026-10-13: **G.3 re-review R1/R2**, exonbook, synthetic reviewer rig,
  common bench lock. 55 exact-target killed takes / 165 files: H.264, HEVC,
  ProRes 422 at 0.05/2.0/2.05/3.7/4.0/4.05 s, both delayed and immediate
  kills, during/after Resume; 13 three-4K60 HEVC q100 cases. 111 files have
  readable fragments and decode cleanly, 54 have no movie header. Metadata
  consistently says unknown (null), never a fabricated lower bound; all
  failed manifests and recovery notes retain that uncertainty. Legacy main
  reproduced 2.0 s claimed but 3/3 unreadable, then the new daemon removed
  that field on recovery. Two focused toggle presses 0.283 ms apart queued
  and succeeded (pause 1.30–1.35 s, two success events, three clean files).
  Full e2e: 12 PASS, 0 FAIL, bug_report unavailable; all measured outputs
  zero-drop. Full release gates: 180 tests, fmt and all-target clippy clean.
  Every matrix row, including per-output AAC durations, is in REPORT.md
  "Re-review fixes" at /Volumes/IsocastBuild/g3-pause/scratch.

- 2026-10-13 (engine-e2e, first hosted run **38493967068**): the `macos-15`
  arm64 VM ran macOS 15.7.9 (24G830), image 20260907.0337.1. GitHub's
  documented standard allocation is **3 M1 vCPUs / 7 GB RAM**; this run did
  not log actual core/RAM values or GPU identity, so those are advertised
  specs, not measurements. Both hardware-only VT probes passed (two tiny
  frames prove availability, not 1080p60 pipeline throughput). Retained
  recordings delivered **37.14–48.50 fps**, losing **19.17–38.10%** of their
  slots; program-only 79/299 (26.42%), three-output program 99/297 (33.33%),
  RTMP 151/412 (36.65%). All ten MOVs fully decode with equal A/V durations,
  but ISO files ended at 4.900/4.950 s instead of 5 s: tolerating drop
  counters alone must not turn those into passes.
  `e2e.yml` now explicitly uses `--hosted`: reports measured sysctl cores,
  RAM, chip/VM flag, GPU/Metal and native VT encoder names; qualifies
  **720p30 H.264** with two 5 s program+2-ISO takes and two 7 s RTMP streams.
  If every output loses zero frames, the entire suite keeps the **0-drop**
  bar. Otherwise **at most 25%** accounted video loss per output is allowed
  (not an average across files); audio loss remains zero. Decodes, complete
  manifests, target durations, equal A/V, frame accounting, sync and scene
  timing still gate. A failed qualification does not silently select an
  easier budget. Local/test Macs without the flag remain 1080p60/0 drops.
  Both modes and the two-step workflow logic pass on the local M5 Max;
  actual hosted 720p30 qualification awaits publication/re-run.
  **Base-M1 users:** this establishes a constrained VM failure, not a native
  M1 hardware limit. A full M1 has more CPU cores and a different GPU/media
  path and scheduling environment; don't claim native M1 cannot do 1080p60
  or that 720p30 is its ceiling. Measure a real M1 with the normal strict
  suite/tune before recommending its profile. The lighter profile reduces
  canvas pixels/second to 2/9 of 1080p60; its VM success is not yet proven.
  Evidence, hardware source and exact retry commands: the **Hosted run**
  section of `/Volumes/IsocastBuild/e2e/scratch/REPORT.md`.

- 2026-10-13: **G.3 pause**, exonbook M4 Max, synthetic only, common
  benchmark lock and no screen sharing. Final build: 39 finalized lifecycle
  files and 3 SIGKILL fragments decode cleanly; every finalized file has
  matching A/V duration and zero reported video/audio drops. Burnt-in
  frame counters in 21 files show no unexpected repeats/skips, including
  all 300 joins from 100 pause/resume pairs (9.965 s). Beep/flash edges are
  within 0.667 ms; 60/30 fps, split after pause, stop during pause, and
  +/-80 ms audio offsets pass. A concurrent stream retains 1,623 frames,
  zero drops/reconnects, monotonic PTS with <=17 ms video / <=22 ms audio
  gaps. One 21.133333 s paused/resumed take becomes one timeline clip.
  Four interleaved flagship pairs versus frozen main ace7e0a: median daemon
  CPU 27.9 -> 27.9%, retired instructions 570.539 -> 569.766 M/s, Stop
  84.5 -> 82.5 ms, zero video/audio drops both; all 24 MOVs match durations,
  counts and decode. Footprint 351.5 -> 362.5 MiB, overlapping ranges
  351–385 / 351–376; no memory-saving claim. Evidence and full numbers:
  `/Volumes/IsocastBuild/g3-pause/scratch/REPORT.md`.
- 2026-10-13 (engine-e2e): local M5 Max and isocast-m2-a each pass 11/11
  available scenarios, with explicit pause/bug_report skips on a619986.
  Program and two ISO files: each 300 frames, 5.000 s video/audio, zero
  drops, full decode; maximum beep/flash offsets 0.021 ms locally and
  1.167 ms on M2. RTMP 6000.1/160.4 kb/s, keyframes at 0/2/4/6 s, no
  video/audio drops; retake export 240 frames/4.000 s and removed takes
  preserved. ProRes record/ISO/scenes/plugin/crash checks also pass locally.
  CI-driver A/A on M2 (same release bytes, three alternating pairs each):
  program CPU 33.1 vs 33.4%, stream 29.8 vs 30.2%, both within spread,
  zero drops; flagship correctly skipped on a base M2. Another lane's
  existing g1-review stream was active, so this qualifies automation, not
  isolated performance. All runs held the common lock; no screen sharing.
  Evidence and hosted first-run steps: `/Volumes/IsocastBuild/e2e/scratch/REPORT.md`.

- 2026-10-13: **S.3 round 3, final delivery** (main 85c5d6d versus SHA
  `3ddf919913e6`; M4 eight interleaved flagship pairs, M2 four per scenario).
  Same synthetic inputs, common lock, no competing engine CPU in measured
  windows. Explicit HEVC look-ahead zero is enabled only on the M4 Max.

  | Mac / scenario | Stop ms main -> delivery | Program drops main -> delivery | ISO drops main -> delivery |
  |---|---|---|---|
  | M4 / flagship | 747 -> 83 | 1055 -> 0 | 1299 -> 0 |
  | M2 / flagship | 984.5 -> 153 | 1605 -> 0 | 3105 -> 7781 |
  | M2 / flagship + stream | 322.5 -> 70.5 | 3711 -> 0 | 329 -> 10378 |
  | M2 / flagship-1080 | 133 -> 104 | 0 -> 0 | 0 -> 0 |

  M4: **35,040/35,040 encoded/expected**, 35,004 distinct pictures (small
  source repeats, separately measured), no tail padding. App CPU 40.1 ->
  29.6%, retired instructions 600.2 -> 572.4 M/s, footprint 695.5 -> 355.5
  MiB. CPU time is clock-sensitive; don't equate its whole percentage drop
  with less work. M2 concurrent stream: main 3606 video / 8672 audio drops,
  delivery 0/0; received candidate video gaps at most 17 ms. All 60 candidate
  matrix files are complete, duration/count matched and decoded cleanly
  (main: nine duration mismatches). Another 28 normal lifecycle/codec files
  pass; six killed files decode with intact fragments. H.264, HEVC Main 10
  quality 100, ProRes 422, split/cancel and real WAV tails verified on both
  Macs. M5 real capture remains unqualified. Evidence: `delivery-*`,
  `safety-delivery`, `delivery-verification.json`, and REPORT.md under
  `/Volumes/IsocastBuild/astra-s3r3/scratch`.

- 2026-10-13: **S.3 round 3, graceful-overload qualification**, frozen main
  85c5d6d versus candidate SHA `017811a9037b`. Eight M4 flagship pairs;
  four pairs for the other rows. Synthetic reviewer clips, common lock,
  isolated engines, no competing engine CPU in measured windows. Medians:

  | Mac / scenario | Stop ms main -> candidate | Program drops main -> candidate | ISO drops main -> candidate |
  |---|---|---|---|
  | M4 / flagship | 723 -> 127.5 | 1209 -> 0 | 1730 -> 10094 |
  | M4 / flagship-1080 | 122 -> 64.5 | 0 -> 0 | 0 -> 0 |
  | M2 / flagship | 989 -> 150 | 1575 -> 0 | 3105 -> 7858 |
  | M2 / flagship + stream | 346 -> 73.5 | 3687 -> 0 | 410 -> 10345 |
  | M2 / flagship-1080 | 131 -> 105.5 | 0 -> 0 | 0 -> 0 |

  M2 concurrent stream: main 4388 video / 8651 audio drops, candidate 0/0.
  All 72 candidate MOVs have complete manifests, matching durations and
  counts, and clean video/audio decodes (main mismatches 10/72). Identity:
  `dropped = expected - frames + repeated_frames`; 17 explicit tail repeats.
  Both Macs passed split, pending split cancellation, continuous WAV and
  -80 ms offset tails (22 additional normal files), plus six killed files
  whose fragments decode cleanly. Stop retains native finalization and real
  audio; overloaded Stop remains somewhat above the candidate's own healthy
  median. ISO losses are the trade-off, not an equal-throughput CPU win.
  Evidence: round-three `final-{flagship,healthy}` (M4), `final-matrix` (M2),
  `safety-final` (M4), `safety-final-v2` (M2), copied beside REPORT.md.
- 2026-10-13: **Timelines (C.1)** on isocast-m2-b (base M2, macOS 26.7,
  release, isolated engines, benchmark lock; main 9cda72d vs da10547 with
  `bench.py --timeline`, so every branch run recorded into an open
  timeline): flagship-1080, 3+3 alternating 20 s runs: isocastd CPU
  median 45.8% (45.5–46.3) vs 45.8% (45.5–46.1), 548.8 vs 549.2 M
  instructions/s, VT services 24.8 vs 24.8%, RSS 105 vs 106 MB, 0 dropped
  frames and 0 audio dropouts both, 9/9 files complete both, compositor
  p50 673.5 vs 669.4 µs; 3/3 branch runs added their take to the
  timeline. Nothing runs per frame for a timeline (a take is added once,
  when it finishes). Export there: two 20 s flagship-1080 takes (493 MB:
  4K60 desktop + 1080p60 camera ISOs, 1080p60 program) joined in 5.4 s
  (~7x real time) while a third take recorded: that take 0 dropped, 0
  audio dropouts in all three files, `status` answered in 7–11 ms during
  the export; every joined file 40.000 s, 2400 frames, packets and
  decoded frames identical to its two parts'. Scripts:
  /Volumes/IsocastBuild/timeline-mvp/scratch (e2e2.sh, e2e3.sh,
  verify.py, snap.py, m2-export.sh, mcp_smoke.py); bench results in
  ~/isocast-scratch/timeline-mvp/bench/results.jsonl on isocast-m2-b.
- 2026-10-13: **G.1a virtual camera**, base M2/macOS 27, isolated release
  engines on isocast-m2-a, synthetic benchmark clips, common lock, no
  screensharingd. Base d6c8aec vs this branch, three interleaved rounds per
  mode (20 s measured, 24 s takes); **on = diagnostic IOSurface receiver,
  not installed CMIO**. Medians (CPU = % of one core):

  | Scenario | Build | Daemon CPU | M instructions/s | GPU ms/s | Recording drops (3 runs) |
  |---|---|---:|---:|---:|---:|
  | flagship-1080 | base | 40.4 | 559.7 | 71.6 | 0 |
  | flagship-1080 | camera off | 41.7 | 557.7 | 74.9 | 0 |
  | flagship-1080 | probe on | 49.9 | 643.1 | 138.4 | 0 |
  | flagship 4K | base | 41.2 | 580.7 | 148.6 | 3,459 |
  | flagship 4K | camera off | 40.6 | 579.8 | 134.1 | 3,470 |
  | flagship 4K | probe on | 45.8 | 643.7 | 274.7 | 3,427 |

  Follow-up four 1080p base/off pairs (the first round's CPU difference
  needed checking): **40.65 → 40.2%**, instructions **559.51 → 559.92 M/s**,
  GPU **71.44 → 69.03 ms/s**, all 24 files' manifests 1,440 frames/zero
  drops. All seven base/off pairs: mean CPU **41.06 → 41.17%**, median
  instructions **559.648 → 559.815 M/s (+0.03%)**. No consistent off-state
  regression; neither a CPU speedup nor a literal zero-cost proof.
  Probe-on adds ~8.2 CPU points/~63.5 GPU ms/s at 1080p; its separate reader
  costs 2.74% CPU (3.24% at 4K). 1080p camera sustained **60 fps**, PTS to
  reader p50 **24.2–27.6 ms**, p99 **28.5–31.6 ms**. At overloaded 4K it
  receives **46.8–46.9 fps**, p50 **29.1–30.4 ms**, and records drops
  honestly; the baseline's encoder/pool limitation remains. Cold starts
  count ~9–11 unavailable ticks during engine warm-up; not an all-zero-drop
  camera claim. Program PTS precedes drawing by about a frame, so these
  numbers are not program-ready-to-camera-client latency.
  Evidence and exact binary hashes:
  `/Volumes/IsocastBuild/g1-vcam/scratch/REPORT.md`; raw results in
  `remote/bench-1791913544/` and `remote/bench-1791914367/` beside it.

- 2026-10-13: **S.3 round 2, final comparison** (`28ddad1` main versus
  runtime `3a047b9`, OBS 32.2.2 OpenGL; four interleaved rounds for every
  scenario/Mac, 96 rows). Synthetic reviewer clips, isolated apps/setups,
  common lock, no screen sharing. App CPU excludes VT; instructions are
  actual retired application work, not samples. Medians:

  | Mac | Scenario | App CPU % main -> candidate (OBS) | M instructions/s main -> candidate (OBS) | Candidate / OBS footprint MiB |
  |---|---|---|---|---|
  | M4 Max | program-1080 | 27.1 -> 26.6 (58.5) | 339.9 -> 339.4 (1225.1) | 278 / 1158 |
  | M4 Max | program-4k | 28.1 -> 28.6 (70.6) | 343.8 -> 343.7 (1468.5) | 356 / 1344 |
  | M4 Max | flagship | 38.0 -> 39.2 (69.8) | 598.1 -> 600.5 (1465.7) | 694 / 1346 |
  | M4 Max | stream | 24.6 -> 25.1 (64.1) | 311.3 -> 312.1 (1048.9) | 276 / 1168 |
  | M2 | program-1080 | 27.0 -> 27.8 (67.6) | 330.9 -> 329.2 (1216.9) | 88 / 630 |
  | M2 | program-4k | 27.9 -> 27.5 (68.5) | 336.5 -> 336.8 (1295.6) | 165 / 836 |
  | M2 | flagship | 40.5 -> 38.8 (68.0) | 579.8 -> 579.8 (1292.5) | 513 / 832 |
  | M2 | stream | 25.2 -> 26.1 (68.2) | 302.8 -> 303.2 (1036.1) | 88 / 619 |

  No additional whole-process CPU win against current main is established.
  The candidate uses 47-53% of OBS's app+VT CPU for equivalent work, but
  these product-level advantages largely predate this branch. On M2 4K,
  candidate 6210/6241 distinct/decoded pictures versus OBS 1645/6290 (OBS
  duplicates); the 4K flagship remains overloaded (Isocast writes three
  files, OBS one). Full CPU/VT/GPU/power/latency/frame tables, binary hashes,
  and raw data: `/Volumes/IsocastBuild/astra-s3r2/scratch/REPORT.md`, with
  `final-{m4,m2}/` beside it and `~/isocast-scratch/astra-s3r2/final-qualification/`
  on each test Mac. Runtime SHA starts `88ea7ab6a7fe`.

- 2026-10-13: **Round-2 final-byte stall and safety checks.** M2: twelve
  normal pairs per build all clean, then encoder freezes (~110, 210-240,
  310 ms actual): main 18 video/30 audio drops, candidate 0/0. Four more
  ~310 ms pairs verified original RTMP wire timestamps via loopback pcap:
  all candidate A/V monotonic, video steps <=17 ms and audio <=22 ms;
  recovered arrival offset returns to baseline (~303 ms, not a calibrated
  capture-to-display metric). Temporary catch-up bursts are reported;
  encoded rate stays 6000/160 kb/s. Both recording-stall files complete
  with expected frames and equal A/V duration. Across the final matrix,
  all 80 MOVs finalize and decode, and all 42 zero-drop files have equal
  A/V durations. **Overload exception:** 9 flagship files have audio tails
  16.667-66.667 ms longer than video: main 6/40, candidate 3/40, on both
  Macs. Last video frames were dropped under load; this baseline behavior
  remains open, not an all-green integrity result. Four killed program
  files (both builds/Macs) retain 360 cleanly decoded frames / 6 s, with
  correctly unfinished manifests. No recording path was changed.

- 2026-10-13: **M4 memory after the merged renderer, controlled.** Four
  pairs of round-1 `df7055f` and current main `28ddad1`, identical two
  still-color sources, no decoding/recording, 1 fps preview: footprint
  median **464 -> 118 MiB**, RSS 135.4 -> 136.4 MiB. Old snapshots have
  42 resident 8 MiB graphics regions (non-purgeable in three, purgeable in
  one); current main has 0-7, all purgeable, none non-purgeable. Startup
  peaks still reach ~462 MiB. The fixed excess is not inevitable GPU-core
  memory: merged unchanged-frame reuse lets graphics residency subside.
  Moving 4K/flagship profiles still have 24 non-purgeable 8 MiB regions.
  This is a main-versus-historical control, not a new memory saving from
  the stall branch. Evidence: `color-memory-m4/`, `memory-analysis.json`,
  `main-profile-m4/` beside the report.

- 2026-10-13: **Rejected round-2 CPU changes.** Removing propagated canvas
  color metadata: four M4 pairs, healthy 4K program 27.8 -> 28.3% CPU and
  328.566 -> 327.701 M instructions/s (only 0.26% less), same throughput;
  no accepted win, all tags restored. Reusing a media reader through
  AVAssetReader random-access reset: repeated one/two-frame loop gaps on
  M2 (main partners clean), also M4 missed ticks and ~15 MiB more memory;
  restored fresh readers. Wrong assumption: reset would cheaply reuse
  the decoder/pool without disrupting delivery. Both patches are retained
  only in scratch; no scene/evaluation/filter/app changes were retained.

- 2026-10-13: **A reproduced cause of mid-stream VT churn: clip preparation
  bypassed the benchmark lock.** During S.3's fourth M2 wire test, another
  benchmark made its missing clips: unified logs show FFmpeg 23895 encoding
  600 HEVC 4K frames at 10:54:29–40 UTC and FFmpeg 23916 encoding the camera
  clip at 10:54:40–45, each spawning its own encoder service. The generated
  desktop clip's path/mtime corroborated the attribution. Main's stream
  overlapped both, losing 26 video/51 audio blocks including the deliberately
  induced freeze; earlier freeze-only controls lost 12/20–21. Its owned
  decoder process did not churn. `make_clips` now reserves the same lock as
  measurement (both benchmark entry points use it), rechecking files after
  acquiring it. A real held-lock check left preparation blocked with no MOV
  or FFmpeg started. This identifies this observed interference, not every
  historical VT-None row; global VT identity changes alone don't prove an
  Isocast encoder/decoder restarted.

- 2026-10-13: **Retired instructions on both Macs (S.3 round 2).**
  `bench.py` and `obs-bench.py` now read public `proc_pid_rusage` flavor 4
  cumulative `ri_instructions`, keyed by PID plus process start time.
  Missing, reset or changed process trees report unavailable. This works
  without sudo on isocast-m2-a, where powermetrics IPC still prints
  `Second underflow occured.` even with `--handle-invalid-values`, and
  xctrace is not installed. Five initial paired stream windows main/final:
  304.778/304.255 M instructions/s, despite 26.2/28.1% CPU medians; all
  zero-drop. Frequency-sensitive CPU time alone would be misleading.
  Both scripts save and display the counts next to real frame throughput;
  the final four-round OBS comparison is in the round report.

- 2026-10-13: **S.3 round 2 stream reserve**, baseline `28ddad1` (compiled
  renderer), on isocast-m2-a: 12 interleaved 20 s stream windows per build,
  all main/candidate zero video and audio drops through Stop. SIGSTOP/CONT
  of the owned VT encoder for ~110/210/310 ms: main 18 video / 31 audio
  drops, candidate 0 / 0. Received FLV video gaps main 117/217 ms versus
  candidate at most 17 ms; both audio timelines at most 22 ms (main fills
  lost audio with silence, so its timestamps alone do not prove continuity).
  Recording after the same pulses: both 1308/1308 frames, zero drops,
  complete manifests, equal A/V durations. Two 65 s looping-media streams
  retained the same owned encoder/decoder process identities throughout;
  decoder *sessions* reopen at loops, but process churn was not reproduced.
  Full round report: `/Volumes/IsocastBuild/astra-s3r2/scratch/REPORT.md`.
  Its initial M2 missing-clip trials are excluded, not qualification.
- 2026-10-13: **Keyframe animations (5.2a)**, isocast-m2-a (base M2, macOS
  27.0), isolated engines, synthetic sources, under the bench lock; 5.2a
  e9f70d1 (isocastd 5a754d78) against its base, 5.5a's tip 84e9a77
  (19d51467), both built on Ryan's Mac.
  - In a 60 fps ProRes program (640×360, anim_check.py, 3,922 frames, 0
    dropped): a box translated 0 → 300 px linearly over 1 s moves exactly
    5.000 px every frame (max centroid error 0.0 px, 0 px on the first
    frame); `cubic_bezier = [0.2, 0.8, 0.2, 1]` within 0.034 px of the
    computed curve; `steps = 4` jumps exactly 4 times, at frames 15, 30,
    45, 60; an infinite alternating loop is on schedule at every frame for
    62 s (3,723 frames, 0.0 px; frames 3,600–3,660 at 0/75/150/225/300
    px); `on_air` restarts at 0 px on the first frame after switching
    back, a `clock` loop doesn't (the phase fitted before the cut predicts
    it after within 0.0 px).
  - props_check.py (662 frames, 0 dropped): every property draws (opacity
    luma 254 → 127 → 0 at frames 0/30/60); `shown` starts at 0 px on the
    first frame showing the item; a nested scene's box starts with the
    scene showing it; a Move transition between scenes with an animated
    box: median step 5.0 px (the Move's own travel at most 12.1 px a
    frame); studio mode's preview starts its own clock (0 px on its first
    frame) while the program's loop keeps its schedule (0.0 px).
  - Existing transitions (trans_check.py, both builds): item show and hide
    transitions record the same frames (framemd5, 61 of 61 each way);
    scene transitions' per-frame progress against the formula, base vs
    5.2a: fade max 2.27 vs 2.31 luma levels (medians 1.02 / 1.00), swipe
    0.50 / 0.50 px, slide 0.50 / 0.50 px, move 1.19 / 1.24 px.
  - Per animated property (cost_check.py: a 1080p60 HEVC recording over a
    moving test pattern; s = 100 boxes with shadows, 201 draws; t = 1,000
    boxes, 1,001 draws; `engine.compositor.cpu_us` p50, median of runs).
    With the clocks up (`--spin`, a core kept busy; 2 rounds, spread under
    4 µs): s0 113.5 µs (base 114.3), s100 (100 translates) 137.8, s1000
    (each box animating all 10 properties: whole records) 149.8; t0 148.0
    (base 149.6), t1000 (1,000 translates) 204.2: 0.056 µs a property for
    1,000 (t), 0.036 (s1000), 0.24 for 100 (s100: mostly a fixed ~20 µs a
    tick once anything animates, most likely uploading changed records,
    the staging belt and its copy), record patching and upload included:
    within the design's 0.2 µs. At rest (3 rounds) the
    whole tick runs ~8× slower (s0 899 µs, t0 1,051; base 1,019 / 1,239;
    spreads of 200–300 µs between identical runs): t1000 − t0 439 µs
    (0.44 µs a property), s1000 − s0 423, s100 − s0 187; over the 0.2 µs
    there, as the design's other per-tick budgets are. The compact pass
    (e9f70d1) against the build before it (0bb357c): t1000 204.2 vs 214.2
    µs clocks up, 1,490 vs 1,588 at rest; s100 1,085 vs 1,280 at rest.
    0 dropped or missed frames in every run.
  - Flagship gate, isocast-m2-a, alternating, two rounds of 3 runs per
    build (another agent's idle engine at ~30% of a core throughout):
    flagship-1080 isocastd CPU per pair (base, 5.2a) 43.3/42.9,
    45.3/44.7, 44.9/44.5, 48.4/48.3, 45.5/49.0, 49.0/48.9 (median of the
    differences −0.25 points); dropped 0 vs 14 (one 5.2a run, program
    1,426/1,440, compositor p50 890 µs that run against 600–730 in the
    rest: the Mac was disturbed); compositor p50 620–707 vs 605–727 µs.
    A third round of 4 runs each: 49.2/48.8, 48.3/48.5, 42.8/47.7,
    49.1/48.3, 0 vs 0 dropped, 12/12 files complete both, p50 713 vs 706
    µs. Per round (bench.py's medians): 44.9 vs 44.5, 48.4 vs 48.9, 48.7
    vs 48.4%. Flagship (4K), the first two rounds: per pair 42.1/43.5,
    42.5/41.4, 44.0/43.4, 45.9/46.0,
    46.6/45.6, 46.0/46.2 (median difference −0.25), dropped 6,994 vs
    6,970 in all (the M2's 4K encoder limit), compositor p50 within
    the same spread (895–1,076 vs 821–1,066 µs).
  - Locally (M5 Max): the fixed every-feature scene (no animations)
    snapshots identical to the base build (main and inner, max difference
    0); examples/recording.toml, a copy of Ryan's setup and a migrated
    library load and save byte for byte as with the base; a setup with
    10 keyframe sets round-trips its own JSON with 0 changes, and a set
    patched by id and back saves byte for byte.
  - Scripts and results: /Volumes/IsocastBuild/scenes-5.2a/scratch
    (anim_check.py, props_check.py, trans_check.py, cost_check.py,
    cost.sh, costsum2.py, checks.sh, ab.sh, snapcmp.sh, roundtrip.sh;
    results/), copies in ~/isocast-scratch/scenes-5.2a on both M2s
    (results-*.txt, ab-*-final*.log, bench-*/results.jsonl).

- 2026-10-13: **Data store and content (5.5a)**, base M2s, isolated
  engines, synthetic sources, under the bench lock; builds made the same
  way on Ryan's Mac (5.5a 7c50fea, main 28ddad1); isocast-m2-a (macOS
  27.0) and isocast-m2-b (26.7). m2-a's lock was held by Astra's
  qualification for an hour, so m2-b ran everything first and m2-a after.
  - Flagship gate, isocast-m2-a, alternating, 3 runs per build:
    flagship-1080 isocastd CPU main 42.9% (42.5–44.1) vs 42.9% (42.3–43.1),
    0 / 0 dropped, 9/9 files complete both, compositor p50 581 vs 588 µs,
    p99 841 vs 866; flagship (4K) 43.2% (40.8–44.2) vs 42.2% (41.8–43.6),
    dropped 3,483 vs 3,483 (the M2's 4K encoder limit), p50 974 vs 924,
    p99 1,370 vs 1,323.
  - Change to program (60 `data_set`s of a title in a fixed box while
    recording a 1080p60 ProRes program, a marker right after each reply,
    `framemd5` of the box's crop): every change shows in exactly one new
    frame, whose pts is at most +0.4 ms (m2-b; +4.3 ms m2-a) after the
    change, median −7.5 ms: drawn (pts + 1 frame) 9.1 ms median, 17.1 ms
    at most (m2-a: +1.8 ms, drawn 8.3 / 18.4): within a frame of the change and its
    resolve (the text drawn in ~5 ms at rest, 0.7 ms with the clocks up:
    over the design's 2 ms, see Gotchas).
  - 1,000 `data_set` a second (bursts of 10 every 10 ms) on keys nothing
    shows, recording: 1,000 calls/s, `data_changed` 20 a second (21 in
    one second bucket) of up to 87 keys; four alternating 10 s idle/flood
    windows: compositor p99 median 695 µs (648–876) idle vs 745 (714–774)
    flood, p50 510 vs 547, isocastd 20.4 → 26.9% of a core; 0 dropped,
    0 missed in 83 s. isocastd RSS 101.3 / 101.9 / 101.2 / 100.4 MB and
    footprint 92 / 93 / 90 / 89 MB at the ends of idle, flood, title
    flood, idle (m2-a: compositor p99 856 idle, 827 flood; RSS 103.0 /
    103.6 / 113.1 / 104.4 MB, footprint 95 / 95 / 107 / 96; 0 dropped).
    Store time per set p50 2.8 µs, p99 7.5–11 µs; a client's round trip
    ~0.3 ms.
  - The same flood also setting a title shown (redrawn ≤ 60 a second):
    compositor p50 1,075 / p99 1,494 µs (an adopt and a texture write a
    frame), 0 missed or dropped. With nothing drawing the program, 60,000
    such sets in 60 s: RSS 76.5 → 84.0 MB, flat from 5 s (before
    7c50fea it grew ~33 MB a second).
  - And on isocast-m2-b, alternating, two rounds (3 and 4 runs per
    build): flagship-1080 isocastd CPU main 46.3 / 46.6% vs 45.5 / 46.8%,
    0 / 0 dropped both, 21/21 files complete both, compositor p50 667 / 681
    vs 658 / 670 µs, p99 905 / 964 vs 973 / 923; flagship (4K) 44.7 / 44.6
    vs 44.8 / 44.5%, dropped 3,509 / 4,685 vs 3,517 / 4,673 (the M2's 4K
    encoder limit; within 0.3%), p50 1,035 / 1,044 vs 1,033 / 1,039, p99
    1,366 / 1,444 vs 1,458 / 1,556 (higher both rounds, within the spread
    5.1 saw between identical builds; nothing new runs per tick).
  - Scripts and results: /Volumes/IsocastBuild/scenes-5.5a/scratch
    (latency.py, flood.py, flood2.py, bound_soak.py, persist.py,
    snap_eq.py, checks.sh, ab.sh; m2*-*.log), copies in
    ~/isocast-scratch/scenes-5.5a on both M2s (results-*.txt,
    bench-*/results.jsonl).
- 2026-10-13: **WebAssembly audio filters (7.4a)** on isocast-m2-a
  (base M2, macOS 27, release, isolated engines, benchmark lock; branch
  build of fa11d82, the fuel checks on bbb3f57 against main 28ddad1):

  | what | main (built-in gain on the tone) | branch (example's wasm gain) |
  |---|---|---|
  | 4K flagship, 5+5 alternating: dropped frames per run, median (range) | 1162 (1144-1171) | 1165 (1159-1174) |
  | isocastd CPU % median / RSS MB | 42.5 / 124 | 42.4 / 128 |
  | flagship-1080, 3+3: dropped, files complete, CPU % | 0, 9/9, 44.4 | 0, 9/9, 43.6 |

  The 4K difference (+3 frames a run of ~1160) is within the runs'
  spread; RSS +4 MB is wasmtime and two instances (mix and filtered ISO).
  Per 10 ms block, live engine (`status`, engine.audio_filters), three
  30 s runs: wasm gain p50 9.3-10.8 µs (p99 21-30), 9,301 fuel; wasm
  ducker with a sidechain p50 52-64 µs (p99 183-190), 165,877 fuel. An
  endless loop is stopped 7-14 ms into the call with 20,000,000 fuel and
  2.1-4.9 ms with 5,000,000 (the budget now). Faults set 3 s into 14 s
  recordings (test pattern + tone with the wasm gain, program and filtered
  ISO): every file 14.00 s of audio and video, every beep present and 0.02
  ms from its flash, 840/840 frames, 0 dropped, 0 audio dropouts.
  Scripts: /Volumes/IsocastBuild/plugins-7.4a/scratch (m2-*.sh,
  avcheck.py, watchcount.py, withlock.py); results in
  ~/isocast-scratch/p74a/bench-{watch,wasm}/results.jsonl on isocast-m2-a.
- 2026-10-13: **Events by kind (7.6)** on isocast-m2-a (main 28ddad1 vs
  c1b38f4, each with its own deck installed by its own build; the deck
  idle, the engine running a 1080p60 test pattern and tone, 3 alternating
  45 s rounds): a watcher without params gets 17.6-17.8 events/s (levels);
  with the deck's filter or MCP's "all but levels" 0. The deck's CPU
  0.311% of a core on main vs 0.022% (14x less); isocastd 10.8-11.5%
  either way. 4K flagship with the deck installed, two rounds of 5+5:
  dropped per run median 1159 (main) vs 1158.5, isocastd CPU 41.85 vs
  42.2%, RSS 125 both, 0 audio dropouts. So the 7.1 entry's ~0.2% more
  drops with the deck didn't come from its wakeups (or are below this
  noise): the M2's 4K flagship drops are its encoders'. The compositor's
  p50 read 890/849 µs on main and 939/924 on the branch, which changed no
  compositor code; 5.1 measured the same spread between two builds that
  differ by a branch never taken.

- 2026-10-13: **S.3 follow-up 1 supersedes the eager-live-mixer change.**
  Reviewer reproduced candidate 9 video / 22 audio drops (main clean).
  Restored 200 ms during live work. Only stopping recording routes receive
  their complete audio tail early; live taps retain original deadlines.
  A first correction still broadcast that tail to a concurrent stream
  (three audio drops); rejected and replaced with selective delivery.
  Meter SIMD reverted: no established neutral-or-better process CPU.
  Final A/B using the reviewer's exact clips and saved main binary:
  M2 **10 pairs**, M4 **4 pairs**, all candidate and main streams zero
  video/audio drops through Stop. App CPU medians main/fixed: M2 30.9/30.9%,
  M4 29.6/28.65%. Four stop pairs/resolution: M4 1080p/4K
  **363.5/363 → 71/65.5 ms**; M2 **340.5/372 → 79/93.5 ms**. All candidate
  MOVs have expected decoded frames, complete manifests, equal A/V
  durations and no drops. Four concurrent record/stream lifecycle pairs
  remained clean, including timed stops; live meter lag returns to ~200 ms
  afterward (old round-1 build was ~0 ms throughout). Seven safety cases
  were rerun, including split/cancel and killed-fragment recovery.
  M2 forced encoder suspension, requested 100/200/300 ms (~110/210/310
  measured): recordings lose nothing in either build; stream totals main
  18 video/33 audio, fixed 17/29. **Not immune to over-budget hard stalls**;
  no queue/drop limit was relaxed. Evidence in REPORT.md "Follow-up 1" and
  `followup1/{m4-final,m2-final,integrity}` beside it. Final binary SHA starts
  `433e85a18a5c`; earlier `followup1` remote runs are intermediate evidence.

- 2026-10-13: **S.3b CPU and memory remain open.** Same four-round
  qualification as S.3a; app CPU excludes VT. Main/candidate medians for
  1080p, 4K, stream: M4 **31.95/31.6, 33.55/33.5, 29.3/29.25%**;
  M2 **34.8/34.0, 35.65/35.5, 29.55/33.35%**. Meter SIMD reduces its
  sampled hotspot (e.g. M2 4K 40/3052 → 12/2949 top-of-stack/thread
  samples), not the entire process by that fraction. M4 retired
  instructions/s move only 0.04–0.23%, with variable effective clocks.
  M2 powermetrics IPC fails `Second underflow occured.`; no normalized
  CPU-saving claim. One M2 candidate stream reported 11 video drops and
  29 audio dropouts in its first seven seconds (VT process identities
  also changed, not proven causal). Four extra main/candidate pairs were
  all zero-drop, but CPU still varied widely: 24.6/28.35% medians.
  Keep the outlier; no universal zero-drop stream or broad CPU win.
  Footprint is essentially unchanged: M4 main/candidate 423/422.5,
  500.5/499.5, 414/413 MiB; M2 85/87, 164/165, 79/79.5 MiB.

- 2026-10-13: **M4's 336 MiB graphics gap (S.3, not yet fixed).** vmmap
  finds 42 extra 8 MiB owned-unmapped graphics regions; mapped IOSurface
  totals and RSS are nearly the same as M2. Still-color sources, no
  recording or media decoding: engine held with no frames drawn is 99.8
  MiB; a preview at 1 fps is 464.5 MiB, at 60 fps 467.3 MiB, both with
  the same 345.7 MiB owned-unmapped graphics category. Native Metal System
  Trace confirms 42×8 MiB wire events in a 25 ms burst at first drawing.
  It also records 1,576 private `draw` plus 1,576 shared staging buffer
  allocations (128 KiB each) from `Renderer::draw_mode/create_buffer_init`
  in 15 s. That churn belongs to the concurrent compiled-scenes work;
  no renderer/draw/bind-group code was changed here. Do not equate it with
  the entire 336 MiB yet: the kernel allocation table is empty and public
  allocation records don't attribute those backing regions. Cache
  recycling at 1 s and a diagnostic command-queue budget of 8 instead of
  4096 did not remove them; both experiments rejected. Next round needs
  driver residency attribution and the compiled renderer, not smaller
  capture queues. Evidence: `results-m4/native-metal` beside the S.3
  report; full private `.trace` stays in exonbook's scratch directory.

- 2026-10-13: **S.3a recording stop**, branch `agent/astra-s3`, baseline
  main df7055f. Four interleaved rounds per scenario/Mac against OBS 32.2.2
  OpenGL, synthetic clips, isolated apps, benchmark lock. Median stop at
  1080p/4K: M4 **346/356.5 → 64.5/83.5 ms** (OBS 127.5/144); M2
  **364.5/386.5 → 84.5/93 ms** (OBS 151/2104, its 4K encoder overloaded).
  All 16 candidate MOVs: exact expected decoded frames, complete manifests,
  equal A/V durations, zero reported drops. Nine further finalized files
  (continuous WAV, negative offset, no audio, three files, split/cancel)
  fully decode and preserve their audio tails; killed output preserves
  4 s / 240 frames, with AAC within one frame. The reply still waits for
  native completion and manifests. Delayed input still waits: WAV 152 ms,
  -80 ms offset 235 ms; no unconditional latency promise for late captures.
  Final tables, hashes and reproduction in
  `/Volumes/IsocastBuild/astra-s3/scratch/REPORT.md`; local evidence in
  `results-m4/final-ab` and `results-m2/final-exclusive` beside it. Earlier
  exploratory folders are not qualification (an orphaned trial engine
  contaminated some early M4 runs; cleaned up and rerun). Final rows record
  app PIDs/windows; all 72 qualified rows had no competing engine CPU.
- 2026-10-13: **Layout (5.1)** on isocast-m2-a (release, isolated engines,
  benchmark lock; final build a10d5ea against 5.0's 107e2e6).
  - Flagship gate, 3 runs per build per round, alternating: flagship-1080
    (two rounds) daemon CPU median 43.2 -> 43.2%, dropped 0 -> 0,
    compositor p50 580 -> 621 us; flagship 4K (three rounds, 9 runs each)
    daemon CPU median 41.9 -> 42.3%, dropped 10,423 -> 10,420 (the M2's
    4K encoder limit), compositor p50 898 -> 939 us. A control of two 5.1 builds
    that differ only by a shader branch never taken (5 runs each) differs
    as much (912 vs 842 us, 41.9 vs 41.5%): within run-to-run spread, no
    regression. One round alone swung from +98 us to -34 us.
  - Resolve (`scenes.resolve_us` after each of 20 changes, back to back
    with 5.0, 2-3 rounds): a 1,000-item flex-wrap scene 857-861 us median
    (budget 1 ms); the same 1,000 placed by rect 342-344 us (5.0: 377-384,
    item paths built in one buffer); 50 items by flex 90 us, by rect 43
    (5.0: 35-39). Other sessions measured the 50-item flex median at 114
    and 205 us: cold execution, see Gotchas.
  - 100 live applies of layout changes (gap, a sidebar's width and
    padding) at 30/s while recording 1080p60: 0 missed program frames,
    420/420 frames decoded, 0 dropped, apply 1.17 ms median (2.5 p99).
  - Scripts: /Volumes/IsocastBuild/scenes-5.1/scratch (all_m2.sh runs
    everything: perf_check.py, layout_check.py, live_check.py,
    transitions_check.py, pixels_ab.sh, roundtrip.sh, resolve_ab.sh,
    noclip_ab.sh), copies in ~/isocast-scratch/scenes-5.1 on isocast-m2-a
    and exonbook.

- 2026-10-13: **Compositor per tick, 5.0 compiled scenes** (isocast-m2-a,
  release, isolated engines, benchmark lock; `engine.compositor.cpu_us` is
  the program thread's CPU per tick that gave a frame, GPU waits not
  counted). Before = 5.0a (main plus the timings), 3 runs each, medians:

  | scenario | before p50 (p99) µs | after p50 (p99) µs | after/before | frames sent again | dropped |
  |---|---|---|---|---|---|
  | flagship-1080 (2 videos, 1080p60 program, 2 draws) | 1173 (1824) | 579 (898) | 0.49 | 3–5 of ~1440 | 0 / 0 |
  | flagship (4K60 program) | 1580 (2304) | 873 (1354) | 0.55 | 2–5 | 3481 / 3485 (M2 4K limit) |
  | 40-item static scene (10 text, 20 images/colours, 10 styled boxes; 50 draws) | 4784 (5883) | 42 (67) | 0.01 | 1246–1247 of 1247–1248 | 0 / 0 |

  Daemon CPU (bench.py, against main): flagship-1080 49.9 -> 42.4%,
  flagship 45.7 -> 41.7%. Where the rest goes (`sample`, flagship-1080):
  CoreVideo handing out the program's next frame
  (CVPixelBufferPoolCreatePixelBufferWithAuxAttributes) ~37%, releasing
  sources' previous frames ~17%, wgpu encoding/submit ~15%; the GPU wait
  is not CPU. Per extra draw on a drawn tick: ~3.9 µs (live video under
  10 vs 160 items: 528 vs 1253 µs). Scripts: /Volumes/IsocastBuild/
  scenes-5.0/scratch (measure.py, bench_all.sh, run_final.sh; copies on
  isocast-m2-a in ~/isocast-scratch/scenes-5.0).
- 2026-10-13: **Plugins installed but idle (7.1).** `scripts/bench.py`
  with `ISOCAST_PLUGINS` pointing at a folder with the deck installed:
  each branch run starts the deck (sandboxed Python watching events and
  serving its page, nothing pressed); main ignores the folder. Branch
  1100974 vs main df7055f, interleaved, medians:

  | Mac | scenario | runs | isocastd CPU % main → branch | RSS MB | dropped frames main → branch |
  |---|---|---|---|---|---|
  | isocast-m2-a | flagship-1080 | 3+3 | 48.7 (48.4–51.6) → 47.0 (46.0–47.9) | 105 → 105 | 0 → 0 (9/9 files complete) |
  | isocast-m2-a | flagship | 3+3 | 43.4 (35.1–44.5) → 46.2 (37.0–47.9) | 113 → 114 | 3311 → 3359 |
  | isocast-m2-a | flagship, again | 5+5 | 45.3 (43.7–47.4) → 45.6 (44.1–46.2) | 117 → 117 | 5763 → 5807 |
  | exonbook | flagship-1080 | 3+3 | 46.1 (44.4–48.5) → 46.8 (44.7–49.1) | 107 → 111 | 0 → 0 (9/9) |
  | exonbook | flagship | 3+3 | 44.8 (41.0–47.2) → 44.1 (43.9–44.9) | 114 → 114 | 1073 → 330 |

  The M2's 4K flagship is past its encoders (~27% of frames dropped on
  main, the known limit); there the branch dropped 0.8–1.4% more of those
  (about 9 frames a run in the tight second round, ~0.2% of all frames),
  which is within the spread between rounds (main's own run totals moved
  ~5% between rounds) but went the same way twice; exonbook's flagship
  dropped fewer with the deck, and both 1080p programs dropped none.
  Measured directly (isocast-m2-a, live engine on the 1080p60 test-pattern
  setup, 30 s): the idle deck used 0.40% of one core, and isocastd 13.3%
  with it vs 13.2% with it turned off. The deck still wakes for every
  event, levels included (20 a second while the engine runs): a `watch`
  filter by event kind would let it (and MCP's event thread) skip them.
  Results:
  `~/isocast-scratch/plugins-7.1/bench-*/results.jsonl` on both Macs.

- 2026-10-13: **Isocast vs OBS Studio 32.2.2, same Mac, same scene (S.1,
  `scripts/obs-bench.py`).** bench.py's synthetic scene in both apps (4K60
  desktop clip filling the canvas, 1080p60 noisy camera clip inset at 22%,
  1 kHz beeps), matched encoders (VT HEVC quality/CRF 70, 1 s keyframes,
  Main, B-frames, NV12 Rec. 709 limited, AAC 256; stream VT H.264 CBR 6000
  High, no B-frames, 2 s keyframes, AAC 160), both headless (isocastd; OBS
  minimized to the menu bar, preview off), a fresh launch per run, 20 s
  windows, 3 interleaved rounds per Mac. Apps: isocast = main ff3d2f7
  (release); obs = OBS's default OpenGL renderer (checked: a config without
  `Renderer` logs "Initializing OpenGL"); obs-metal = OBS 32's Metal
  renderer. Medians; CPU is % of one core for the app's processes plus its
  own VideoToolbox XPC services; memory is physical footprint; power is the
  whole machine (powermetrics CPU+GPU), with the rest-of-machine idle
  before each run in brackets as "above idle"; frames are distinct
  pictures (not repeats of the frame before) / decoded / expected at 60
  fps over all files of the 3 takes. Second full run below; the first
  (decoded frames only) agreed within a few percent.

  Headline:
  - Same work (program-1080, program-4k, stream): Isocast uses about half
    OBS's CPU (0.47-0.56x: M4 Max 43-48% vs 80-91%, base M2 42-48% vs
    84-91%), 27-66% less GPU time, 2.7-2.9x less memory on the M4 Max
    (413-501 MB vs 1.18-1.36 GB) and 5-8x less on the M2 (80-165 MB vs
    0.6-0.85 GB), and the machine draws 30-49% less power. Isocast's CPU
    barely changes from 1080p to 4K (32 -> 33%); OBS's grows (64 -> 74%).
    OBS's Metal renderer doesn't change its CPU; its GPU time is higher
    on the M4 Max and a little lower on the M2 at 1080p and 4K.
  - Base M2 at 4K60, the same hardware encoder and settings: OBS can't
    keep up. Its log: "skipped frames due to encoding lag: 1306/1587
    (82.3%)"; it keeps 60 fps timestamps by repeating the last picture, so
    its files have every frame but 27% distinct pictures (1261/4714, ~16
    real fps) at a quarter of the bitrate, and stop takes 2.2 s. Isocast:
    4646/4669 distinct, 0 dropped, stop 0.38 s.
  - Flagship (Isocast also writes the desktop and camera ISO files: three
    encodes vs OBS's one): M4 Max: Isocast 69% CPU vs OBS's 89% for its
    single file; Isocast's program and desktop ISO lost 3-4% of frames
    (59-69 and 43-48 per 26 s take; camera ISO clean; the first full run
    lost 14% over three takes, one take clean), OBS 0. Base M2: Isocast
    keeps 73% of frames in each of its three files (~425 dropped per file
    per take, the known M2 4K-flagship limit); OBS keeps 26% distinct in
    its one.
  - Times: launch to a ready API 0.06 s vs 0.37-0.69 s; start equal (~0.1
    s); stream stop 0.007 s vs 0.12-0.14 s; recording stop: OBS is faster
    when it keeps up (0.14 vs 0.34 s M4 Max, 0.17 vs 0.36 s M2 at 1080p):
    a target for S.3.

  exonbook (M4 Max, macOS 26.6.2):

  | scenario | app | app CPU % (range) | VT CPU % | app+VT % | memory MB | power mW (above idle) | GPU active % | app GPU ms/s | frames distinct/decoded/expected | skipped or dropped | launch s | start s | stop s | video kb/s |
  |---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
  | program-1080 | isocast | 32.0 (31.8–34.4) | 13.6 | 45.6 | 421 | 470 (262) | 10.9 | 55.6 | 4672/4685/4685 | 0 | 0.06 | 0.103 | 0.336 | 13069 |
  | program-1080 | obs | 63.5 (61.3–66.1) | 17.9 | 81.4 | 1192 | 672 (468) | 16.6 | 98.7 | 4712/4721/4723 | 0 | 0.42 | 0.106 | 0.138 | 11626 |
  | program-1080 | obs-metal | 63.9 (63.6–64.9) | 17.6 | 81.5 | 1186 | 738 (542) | 24.9 | 114.2 | 4706/4717/4718 | 0 | 0.37 | 0.102 | 0.127 | 11615 |
  | program-4k | isocast | 33.2 (30.7–33.9) | 14.4 | 47.6 | 501 | 638 (366) | 13.6 | 109.5 | 4670/4683/4683 | 0 | 0.06 | 0.098 | 0.345 | 50343 |
  | program-4k | obs | 74.1 (71.2–75.3) | 17.0 | 90.9 | 1362 | 920 (732) | 17.9 | 149.6 | 4711/4719/4721 | 0 | 0.42 | 0.107 | 0.137 | 57812 |
  | program-4k | obs-metal | 72.6 (72.1–72.9) | 17.2 | 89.8 | 1338 | 856 (629) | 29.8 | 166.4 | 4710/4720/4723 | 0 | 0.37 | 0.110 | 0.139 | 57906 |
  | flagship (Isocast 3 files) | isocast | 42.8 (41.7–43.6) | 26.6 | 69.4 | 832 | 756 (549) | 15.3 | 104.5 | 13703/13722/14055 | 482 | 0.06 | 0.114 | 0.750 | 48410 |
  | flagship (OBS program) | obs | 72.3 (65.5–73.6) | 17.1 | 89.4 | 1366 | 958 (706) | 18.5 | 146.9 | 4702/4724/4726 | 13 | 0.42 | 0.107 | 0.139 | 57937 |
  | flagship (OBS program) | obs-metal | 73.6 (71.7–74.8) | 17.4 | 91.0 | 1352 | 934 (752) | 30.2 | 166.7 | 4709/4719/4721 | 0 | 0.37 | 0.105 | 0.123 | 57874 |
  | stream | isocast | 29.0 (28.1–29.1) | 13.5 | 42.6 | 413 | 457 (258) | 12.2 | 55.8 | 4681/4690/4692 | 0 | 0.06 | 0.108 | 0.008 | 6144 |
  | stream | obs | 61.0 (60.7–61.5) | 18.7 | 79.7 | 1180 | 896 (703) | 21.5 | 87.3 | 4718/4728/4738 | 0 | 0.42 | 0.136 | 0.118 | 6144 |
  | stream | obs-metal | 66.2 (65.2–66.8) | 18.9 | 85.3 | 1171 | 677 (395) | 25.7 | 115.8 | 4717/4727/4737 | 0 | 0.37 | 0.133 | 0.122 | 6144 |

  isocast-m2-a (base M2, 8 cores, macOS 27.0):

  | scenario | app | app CPU % (range) | VT CPU % | app+VT % | memory MB | power mW (above idle) | GPU active % | app GPU ms/s | frames distinct/decoded/expected | skipped or dropped | launch s | start s | stop s | video kb/s |
  |---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
  | program-1080 | isocast | 31.5 (30.6–34.4) | 14.7 | 46.1 | 86 | 483 (321) | 27.6 | 62.7 | 4643/4670/4670 | 0 | 0.06 | 0.118 | 0.357 | 12797 |
  | program-1080 | obs | 65.1 (59.8–65.7) | 19.1 | 84.8 | 628 | 845 (711) | 47.7 | 185.1 | 4696/4703/4705 | 0 | 0.64 | 0.121 | 0.176 | 11329 |
  | program-1080 | obs-metal | 63.5 (61.0–63.6) | 20.3 | 83.8 | 612 | 814 (681) | 47.4 | 158.4 | 4701/4705/4707 | 0 | 0.58 | 0.124 | 0.157 | 11338 |
  | program-4k | isocast | 32.7 (31.8–34.6) | 15.4 | 47.7 | 165 | 836 (649) | 37.1 | 154.9 | 4646/4669/4669 | 0 | 0.06 | 0.119 | 0.382 | 50194 |
  | program-4k | obs | 69.1 (63.4–72.7) | 19.6 | 88.7 | 845 | 1243 (1066) | 44.9 | 265.5 | **1261**/4713/4714 | 2913 | 0.69 | 0.130 | 2.212 | 12422 |
  | program-4k | obs-metal | 67.7 (60.3–68.8) | 19.5 | 87.7 | 835 | 1140 (1007) | 55.6 | 241.0 | **1299**/4701/4704 | 2909 | 0.58 | 0.128 | 2.123 | 12672 |
  | flagship (Isocast 3 files) | isocast | 42.2 (41.9–43.0) | 24.7 | 66.9 | 505 | 778 (626) | 31.2 | 107.5 | 10147/10195/13992 | 4864 | 0.06 | 0.143 | 0.975 | 37035 |
  | flagship (OBS program) | obs | 69.1 (68.4–72.5) | 19.8 | 88.9 | 848 | 1217 (1076) | 47.1 | 276.7 | **1217**/4712/4714 | 2914 | 0.69 | 0.128 | 2.217 | 12363 |
  | flagship (OBS program) | obs-metal | 71.0 (68.5–71.7) | 20.6 | 91.6 | 834 | 1118 (948) | 59.1 | 250.6 | **1230**/4704/4707 | 2910 | 0.58 | 0.128 | 2.168 | 12584 |
  | stream | isocast | 27.4 (27.3–29.1) | 15.1 | 42.4 | 80 | 481 (287) | 26.3 | 62.7 | 4652/4672/4675 | 0 | 0.06 | 0.126 | 0.007 | 6144 |
  | stream | obs | 67.7 (66.2–68.0) | 23.0 | 90.6 | 620 | 796 (663) | 33.9 | 162.4 | 4715/4718/4728 | 0 | 0.69 | 0.169 | 0.143 | 6144 |
  | stream | obs-metal | 68.3 (65.1–71.9) | 21.9 | 90.2 | 599 | 797 (617) | 50.7 | 168.0 | 4709/4713/4722 | 0 | 0.58 | 0.171 | 0.132 | 6144 |

  "skipped or dropped" adds the apps' own counters over the windows:
  Isocast's missed program frames and dropped file frames, OBS's render-
  and output-skipped frames and stream drops. Video kb/s is the program
  file's (FLV metadata for streams).

  Where it isn't apples to apples:
  - OBS has no ISO files: the flagship rows set Isocast's three files
    against OBS's one (OBS's flagship is its program-4k).
  - OBS runs its UI hidden in the menu bar with the preview off; Isocast a
    headless daemon. Neither draws a preview here: OBS with its preview
    on, or Isocast's app (a separate process) open, would add to each.
  - Audio: OBS's CoreAudio AAC is CBR 256 kb/s; Isocast's AVAssetWriter
    AAC asked for 256 kb/s comes out ~26 kb/s on the mostly silent beep
    track (VBR). Negligible CPU, ~1.7 MB/min of file size.
  - The tone: Isocast synthesizes it; OBS decodes a looping WAV of the
    same beeps with a media source. Both decode the video clips in
    hardware (OBS: "is_hw_decoding: yes" in its log).
  - OBS's VT encoder adds spatial AQ (its default); at the same quality 70
    its 4K files run 57.8 vs Isocast's 50.3 Mb/s (1080p: 11.6 vs 13.1).
  - Containers: OBS Hybrid MOV (its own muxer), Isocast fragmented MOV
    (AVAssetWriter); both survive a crash.
  - Memory is physical footprint (counts GPU allocations; RSS doesn't on
    Apple silicon). Isocast's is 421 MB on the M4 Max but 86 MB on the M2
    for the same 1080p scene (RSS ~94 MB on both): Metal heap sizes differ
    by GPU.
  - Power is the whole machine. exonbook had other agents' work running
    (node at ~100% of a core, Blender, the aerial wallpaper decoding
    video), so its idle baseline wanders (one first-run "above idle" was
    negative): compare the absolute medians, and the CPU and GPU times,
    which are per process. WindowServer (exonbook ~15%, the M2's headless
    display ~41%) is the same with either app and isn't counted.
  - Launch is to `isocast get` answering (including the CLI's own start)
    vs obs-websocket answering requests (not "not ready"); start and stop
    are Isocast's replies (files created; files finished) vs OBS's
    RecordStateChanged/StreamStateChanged events.
  - Not measured yet: encode latency and preview latency (S.1b).

  Reproduce (one command per Mac; OBS needs the desktop session, so via
  the bridge; --detach logs to `<work>/log-*.txt`):
  `ISOCAST_TESTMAC=isocast-m2-a scripts/testmac.sh run 'cargo build --release -p isocast-daemon -p isocast-cli'`, then
  `ISOCAST_TESTMAC=isocast-m2-a scripts/testmac.sh gui 'python3 scripts/obs-bench.py --work "$ISOCAST_SCRATCH/obs-bench/m2-a" --mac isocast-m2-a --detach'`;
  on exonbook add `--obs "$ISOCAST_SCRATCH/obs-bench/apps/OBS-32.2.2/OBS.app"
  --powermetrics-log <log>` (see the Environment entry for starting
  powermetrics there). `--summary` prints the table from results.jsonl.
  Raw results: `~/isocast-scratch/obs-bench/{exonbook-2,m2-a-2}/results.jsonl`
  (OBS's log of each run in `runs/*/obs-log.txt`), copies in
  `/Volumes/IsocastBuild/obs-bench/combined2/`; the first run's in
  `{exonbook,m2-a}/` and `combined/`.

## Decisions

- 2026-10-22 (R1 remote API, third review): every stream-key resolver
  holds the state lock (`stream_key`/`has_stream_key` take `&State` as
  proof), so a resolved key always belongs to the destination as checked
  and legacy migration can't recreate a key a destination change cleared:
  `speed_test` (and `optimize` through it) keeps the lock from choosing
  the destination to having the key, releasing it before the upload;
  stream start resolves under its lock (key and server come from one
  snapshot); statuses resolve under their read or write guard. Doctor's
  disk test is skipped (a warning check) when the folder it writes in, the
  nearest existing one, is outside a network caller's roots
  (`doctor_writing`). Tune's folder (given or the setup's) and replay
  save's folder are checked right before writing. Path comparison: existing
  components are canonical and compare exactly; a missing component
  compares ignoring ASCII case only on a caseless macOS volume
  (`pathconf`), exactly on Windows (folders can be case sensitive there).

- 2026-10-22 (R1 remote API, second review; decision 26): hard boundaries
  versus guard rails. Hard: `library_import_obs` with `stream_key` is
  local-only; `stream_key_set` resolves and writes under the state lock
  (as destination changes do) and takes an optional `revision`; key
  clearing runs before `engine.update` in `apply` and `library_switch`
  (checks first, then deletions, a failed deletion reports the keys
  already gone; an engine refusal after clearing says so too);
  `library_switch` checks the effective setup (developer settings, roots)
  for network callers every time, so stored documents no longer need to
  be trusted and `library_create` needs no check of its own. Roots are now
  only the locally chosen list (or defaults), recordings folders a local
  caller's live setup has had (`recording_folders` in `<setup>.roots.json`,
  20 kept) and fonts: recording history and remote-set folders never
  authorize. Guard rails: derived destinations are checked where decided
  (start's folder after `into`/`record_target`, export and NLE defaults,
  bug_report's default, relink's joined manifest, the screenshot hotkey,
  per-request transition files); request paths are handed on resolved;
  OBS bare names never read the working folder; browser URLs are parsed
  with the `url` crate and stored normalized; UNC paths from network
  callers are refused; the not-yet-existing part of a path compares
  without case on caseless volumes (pathconf on macOS, always on Windows).
  obs-websocket: in-flight reservations apart from failures, successes
  never count, loopback outside the aggregate budget. Blocking `stop` waits
  on its session's own completion. `hello.build` is `{version, commit,
  provenance}`; `compatibility` compares version and commit (a dev side
  warns).

- 2026-10-22 (R1 remote API, after review; decision 25): network callers
  are checked on the **effective operation**. Folder roots live in
  `isocast-daemon/src/roots.rs` (`folder_roots`, local-only
  `folder_roots_set`, saved as `<setup>.roots.json`; defaults
  Movies/Videos, Pictures, Music, Desktop, Downloads, plus this machine's
  recordings folders and fonts read-only). Paths resolve through symlinks
  (longest existing part canonicalized; `..`, relative paths and dangling
  links refused). Params are checked at the top of `Daemon::call`
  (`check_params`), setups at `apply` (outside the state lock, then the
  revision must be unchanged) and in library imports (`add_documents`):
  only paths, URLs and hotkey folders the old setup didn't have are judged,
  and stored library documents are trusted (every remote way in is
  checked), so remote library switches aren't. A remotely fired hotkey's
  configured paths go through `check_params` too (fail closed). Browser
  URLs: http(s) to anything but loopback/unspecified/link-local (literal
  and resolved at apply; DNS failure refuses), `file://` inside roots,
  nothing else. Key clearing fails closed (check or delete error refuses
  the change; `ISOCAST_STREAM_KEY` refuses any remote move; the legacy
  service is cleared too). Speed tests from network callers go only to the
  configured destination (checked in `speed_test`, so `optimize` is too).
  Benchmarks write `owned_temp_name`/`owned_temp_dir` files
  (`create_new`/`create_dir`), so a planted `.isocast-speed-test` link no
  longer receives 64 MB. Provenance comes from build.rs
  (`ISOCAST_BUILD_PROVENANCE`: release when `ISOCAST_BUILD_VERSION` on macOS
  or `ISOCAST_WINDOWS_BUILD_VERSION` on Windows is set); `hello.build` is
  `{version, build, commit, provenance}` only. Session ids are a random
  per-run id plus a counter. `start` admission is serialized
  (`start_admission`), and blocking `stop` reads its own session's outcome.

- 2026-10-22 (R1 remote API and policy): the caller policy is
  `isocast_protocol::caller` (not `remote.rs`, which the transport lane
  owns): `Caller { origin }` with `Local`, `Remote { client_id, name,
  addr }` and a third origin, `ObsWebsocket { addr }`, because its clients
  are network callers (decision 14) even on 127.0.0.1 (other users on the
  machine reach the port). `local_only_reason` matches the Method table
  exhaustively, so every new method must be classified there (the remote
  lane's `remote_*` arm is written out as a comment). Beyond decision 4 it
  also refuses `virtual_camera_remove`, `request_permissions`,
  `open_permission_settings` (OS prompts on the engine's screen) and
  `timeline_export_nle` with `replace` (overwrites any file). The caller
  rides a thread-local (`callers::enter`) like `PluginCaller`, so
  attribution and key clearing need no new parameters through the call
  tree. Stream keys are cleared at the account the destination uses
  *after* the change (the one an RTMP publish would send), in `apply` and
  `library_switch`, for every non-local caller, including new
  destinations. obs-websocket: each try counts as a failure under one lock
  until the password proves right; 5 free per address (IPv4-mapped as IPv4,
  IPv6 per /64) and 30 across all addresses, then waits of 1, 2, 4… s (at
  most 5 min, forgotten after 15 quiet minutes); tries during a wait
  aren't checked.

- 2026-10-22 (R1.d discovery): DNS-SD lives in its own small crate,
  `isocast-discovery` (free functions `register`/`browse`/`watch`, a
  backend per OS behind `cfg`, like `isocast-ipc`), not on the engine's
  `Platform` trait: the CLI and the app browse too, and they don't link
  the media platform crates (AVFoundation, WebKit, libvpx). Raw FFI to
  libSystem's `dns_sd.h` and `windows-sys` `DnsService*`; no new crate.
  Instance names: dots become `-` everywhere (Windows splits service names
  at dots), control characters dropped, cut to 63 bytes. TXT carries only
  `id`, `proto`, `name`; other received keys are dropped, values capped
  at 200 bytes, at most 64 engines and 16 addresses each are tracked.
  Found engines are untrusted hints: identity comes from pinned keys.
  Revised after review: macOS keeps each `DNSServiceResolve` running
  (it reports SRV/port/TXT changes; TXT updates arrived at once) and
  follows a new host with a new address query; callbacks read results
  only without an error; async failures (e.g. `PolicyDenied`) become an
  `Event::Error` and make `browse` fail with the Local Network
  explanation (not forced in a run: the CLI is exempt from Local Network
  privacy); resolve and address failures have separate counters and
  backoff, 5 at most each, and a fresh announcement re-arms an instance
  whose retries ran out.
  `Registration::name()` follows later macOS renames.

- 2026-10-19 (Ryan): **0.0.9 is paused until Windows is "insanely good".** All lanes go to Windows (#105): parity, performance against OBS, first-run polish and the alpha. Mac work resumes only for bugs that affect 0.0.8 users.

- 2026-10-18 (#105 W.4a, in progress): the adapter reserves a new movie
  before start, then transfers that SAME owned handle into the packet mux
  when MF reveals actual configuration. Native packets are length-prefixed,
  not Annex B; fixed codec/depth and descriptions are guarded. Refusals are
  not accepted inputs/losses. Commit drains accepted timestamps without EOS;
  finish checks all packets/counts and adds a real encoded tail repeat only
  when needed. Audio is refused until native AAC supplies actual priming;
  recording quality is not guessed from a live bitrate. The factory currently
  takes an explicitly caller-configured, fresh WindowsVideoEncoder; no
  production Platform selects it yet. Native recording runtime proof remains
  pending, so these are implementation invariants, not qualified parity.
- 2026-10-18 (#105 W.4a): writable frame integration needs capture-lane
  agreement. Current TexturePool exposes only copied immutable RGB leases;
  Dx12Importer target/ycbcr_target refuse, and wgpu30 guarantees only texture
  binding for NV12/P010, not render attachments. Proposed Windows encoder
  output is BGRA (NVENC accepts it; MF owns GPU conversion), via a portable
  preference defaulting to unchanged Mac YUV. No fake NV12 planes, CPU
  readback shim or native file edits have been introduced. Native frame
  ownership, AAC, recording-quality API, nonterminal drain/reuse and WGC's
  ALPC reopen gate remain capture-lane coordination points.

- 2026-10-18 (#105 W.6 shortcuts): distinguish UI logical modifiers from
  configured physical bindings. Slint Control means Command on Mac and Ctrl
  on Windows; stored hotkey `cmd` means physical Win on Windows. Never rewrite
  saved bindings to get a Ctrl label. Windows Exit uses Alt+F4, not a nonexistent
  Ctrl+Q binding. Mac words/glyphs remain unchanged. User content is not passed
  through vocabulary substitutions; the narrow description helper accepts
  only authored static tool text. Windows generated help/schema use target-
  specific overrides, leaving Mac descriptions exact.

- 2026-10-18 (#105 W.7b): **manual Windows checks only**, until notifications
  can be idle-gated even when a recording starts during a check. WinSparkle's
  install callback is too late to prevent an unsolicited window entering a
  capture. The SDK's automatic preference is forced off before init; the
  disabled switch explains why. Mac daily checks and copy are unchanged.
  A verified download executes only fixed arguments, never unsigned appcast
  arguments. Explicit rollback verifies the previous installer's detached
  signature under the embedded key before idle shutdown/execution. It is not
  automatic recovery from a failed application launch; Inno does not restore
  overwritten old files merely because an install aborts.
- 2026-10-18 (#105 W.7b): installation uses the incoming CLI extracted with
  its CRT to a temporary directory, so an old installed CLI need not know
  `engine shutdown`. That command and MCP `shutdown_idle_daemon` share the
  bounded client call to existing `ShutdownIfIdle`: outputs are refused;
  idle calibration is cancelled, not refused. An unused/broken installation
  may uninstall without its CLI; active images still block deletion.

- 2026-10-18 (#105 W.7a): Inno Setup instead of MSIX: per-user installation
  without elevation, ordinary adjacent UI/CLI/daemon executables, existing
  AppData paths and no packaged-process virtualization. Pin compiler 7.1.0
  and its SHA-256; ship the x64 CRT app-local. No PATH modification, service,
  automatic process termination or recursive uninstall deletion. Deliberate
  rollback uses the previous trusted installer with `/ALLOWDOWNGRADE=1`;
  authentic auto-update/idle-engine integration remains W.7b, not implied by
  successful installer generation. Authenticode credentials and publication
  stay with Ryan/the delegator; WinGet can follow after alpha qualification.

- 2026-10-18 (#64): **HE-AAC with implicit SBR signalling in Matroska**
  (the LC core's two-byte AudioSpecificConfig plus OutputSamplingFrequency
  = 2 x SamplingFrequency, as mkvmerge writes it) played 44 ms late:
  AudioToolbox finds the SBR data by itself but adds its 2112-frame HE
  delay, and our ASC parse saw plain LC so nothing was dropped. matroska.rs
  now reads OutputSamplingFrequency; webm.rs rewrites such a config into
  explicit (hierarchical, AOT 5) signalling before the decoder sees it, so
  the existing HE path (cookie + delay) applies. Only plain LC with 1-2
  channels, a table rate and an output rate exactly twice it is rewritten.
  Legacy codec ids (`A_AAC/MPEG4/LC/SBR` with no CodecPrivate) are still
  refused with a message: ffmpeg can't write them, so nothing to test with.
  Not done: an ASC-driven `kAudioFormatProperty_FormatInfo` setup instead of
  our own parse (works as is; follow-up).
- 2026-10-18 (#94): `collection switch` / `library_switch` renames sources
  whose ids became reserved (`edited`, `vertical`) the same way loading at
  start does (notes in the reply), and saves the renamed collection at once;
  before, the switch was refused with a validation error naming the id.
- 2026-10-18 (#94): system audio always leaves out the Isocast app by its
  bundle id `dev.isocast.app` exactly, kept apart from the user's fuzzy
  `exclude` names (review R3: a look-alike named "dev.isocast.app Player"
  was left out too). Process tap: a process whose own Core Audio bundle id
  is that, or that is the app itself (not helpers or tools it launched,
  never isocastd); the set is logged ("audio tap: leaves out …") whenever
  it changes, and follows processes as they come. ScreenCaptureKit: apps
  with that bundle id when the capture starts (its list is a snapshot, so
  an Isocast started later isn't left out there). VM check with real
  recordings: committed head dropped the look-alike's tone, the fix keeps
  it and drops only the dev.isocast.app process (followups-008
  t94excl-tap.log). The app's own silencing while
  capturing stays as a backstop until this is checked on hardware with
  BlackHole (rig: editor-007/scratch/rig) with the real editor; a
  development binary run from a terminal has no bundle id and isn't
  covered by the engine rule.
- 2026-10-18 (#94 review R1): `library_switch` chooses the files a renamed
  collection is saved to under the same state lock as the switch (as
  `apply` does): two switches at once wrote the renamed collection over
  the other one 198 times in 200 (t94race.py), 0 after.
- 2026-10-18 (#94 review R2): renaming a reserved source id also renames
  it inside every `recording.iso.source_audio` list and in source masks'
  `mask_source` (`rename_refs` now covers every source reference
  `validate` checks).
- 2026-10-18 (#94): the editor forgets thumbnails and waveforms of files no
  longer on the timeline (each `show_rows`); a waveform read that fails is
  no longer stored on disk as "no sound" (a file without an audio track
  still is).
- 2026-10-18 (#102 FU10): a recording stopping while a stream runs no
  longer pushes the editor behind the studio when the editor has the
  keyboard (Behind becomes Show then). Checked in code only: proving it
  needs a real window plus a live stream on a test Mac.
- 2026-10-18 (#110 ProRes final corrections): Apple omits both the optional
  UsingHardware Boolean and catalogue IsHardware flag for built-in SW ProRes.
  Explicit flags win; otherwise only five exact catalogue-present software
  EncoderIDs verified with hardware disabled are mapped false. Other missing
  identities remain unknown. The software ExpectedFrameRate hint is advisory;
  timestamps/durations still define file cadence. A running mismatch, unknown
  backend or missing first snapshot invalidates previous HW observations
  locally, never rewrites the saved profile. An unpaced throughput test cannot
  qualify native software startup/recovery with both ISO and protected outputs;
  that resource remains unknown and cannot auto-admit/resize an ISO.

- 2026-10-18 (#110 ProRes in progress): default AWS M2 native sessions report
  hardware for every supported ProRes variant; base chip is not synonymous
  with software. Capacity's prior ProRes0/Fits ignores native/disk resources:
  noisy three-UHD-HQ test was then correctly refused by Start (558MB/s measured,
 661MB/s planned). Add separate native/CPU/GPU/frame-bandwidth/disk observations
  and full concurrent calibration cells; no numeric ProRes ISO-loss promise.
  Existing H.26x-only table fallback remains unchanged; unqualified ProRes
  native/destination capacity must warn and cannot auto-resize/admit an ISO.
- 2026-10-18 (#110 software ProRes): native software encoder rejects the
  advisory ExpectedFrameRate hint and does not implement the optional
  UsingHardwareAcceleratedVideoEncoder property. Actual EncoderID is exposed
  (`com.apple.videotoolbox.videoencoder.prores-422` in the scoped experiment).
  Resolve the actual ID against the native catalogue's explicit HW flag;
  missing property/ID is unknown, never automatically software. Native service
  CPU is outside isocastd, so process CPU alone is not its cost: whole-system
  busy CPU deltas during calibration include services and outside activity.
  No production force-software switch or mid-recording backend transition.
  Runtime qualification is still pending in prores/REPORT.md; Windows waits
  for its daemon switch.

- 2026-10-18 (#110 remaining): a failed retune belongs to its native hub,
  not its canvas. Destinations sharing that hub/replay/warm owner retain the
  same failure latch; independent siblings continue. Destroying/recreating
  the actual encoder retries. Aggregate stream percentage is the minimum
  active acknowledgement. Congestion targeting/ladder remains per canvas.
  `canvases.<id>.fps` pins one secondary canvas; legacy `vertical.fps` keeps
  all-secondary meaning. File writers must pass realcanvasID, not flatten
  every non-main file toVertical. Native overload safety remains independent.
- 2026-10-18 (#110 mixed model): use measured HEVC costs beside separately
  normalized table H264 recording/live costs; no diagnostic FPS becomes a
  pool-capacity coefficient. Mixed basis is explicit; every suggested or
  admitted workload selects its own coverage, unknown dimensions fall back.
  Mixed file overload actual74.06–77.01% vs predicted67.16%; two-live-hub
  overload actual61.11% vs69.04%. These qualify categorical boundaries, not
  improved loss-percentage accuracy. ProRes/10bit resource models and local
  H264 coefficients remain open; Windows waits for the daemon switch.
- 2026-10-18 (#126): media's decoded next frame is visible to timed
  FrameSlot readers before its delivery wake, only when its PTS fits their
  cutoff. It shares the producer's existing buffer; ordinary history,
  untimed previews and capture counters keep their delivery schedule.
  Media video also uses real-time QoS, but QoS alone was insufficient:
  asynchronous tracing caught a decoded picture waiting 20 ms to publish.
  Pending frames cancel on stop/off-air; restart waits recheck every 5 ms.
  M2-b: five strict oracle passes (21,108 checks), full strict e2e 26/26,
  A/B gate passes. See media-pacing/scratch/REPORT.md.

- 2026-10-18 (#110 native rollback): the scoped VT-property experiment
  `20261018-085056-self-tuning-2-cmd` reproduced a real uncertainty bug:
  ExpectedFrameRate accepted 30, AverageBitRate and its rollback both failed,
  the FPS rollback was skipped, and rejoining a 60-fps monitor cleared the
  error without making any native call. The 19-second program remained whole.
  Native setters now attempt both independent rollbacks, retain an uncertain
  state, and force a full property write when restoring even the last
  acknowledged rate. Preview/stream owners don't mistake that numeric target
  for confirmed native state. Repeated identical refusals stay suppressed.
  This is injected per-process refusal, not evidence of a spontaneous driver
  failure. Scratch REPORT.md carries after-verification and remaining hubs.

- 2026-10-18 (#110, self-tuning-2): new daemon-generated setups and blank
  profiles start at 30 fps when physical memory is at most 8 GB or the
  machine has at most 8 CPU cores. This is a portable conservative starting
  point, not a backend/session-capacity claim. Explicit initialization fps,
  imports, copies and existing profiles are unchanged; the ordinary governor
  still never edits intended settings or implicitly enables ISO thinning.
  Base AWS M2s have **24 GB**, not 8 GB: contention runs there cannot certify
  an actual 8 GB machine. NVENC/QSV/AMF/software qualification stays with the
  Windows lane. No saved M5 Max machine profile was present beside Ryan's
  default setup; its existing measured per-chip row is retained, not replaced
  by a new run on his Mac.
- 2026-10-18 (#110, self-tuning-2): measured HEVC coverage checks every
  recorded canvas's actual file dimensions/depth/quality/rate, instead of
  rejecting all setups with extra canvases. Live H.264 retains BOTH its
  table pixel cost and its table per-picture cost: a recording-only fit must
  not silently change the normalized stream estimate. Other codec/depth
  measurements remain diagnostic until their aggregate-resource fit is
  qualified. Reports identify unmeasured cells rather than treating them as
  free. Runtime qualification is recorded in this lane's scratch REPORT.md.
- 2026-10-18 (#120 review fixes): leaving a virtual camera `canvas`
  target clears the size it set (a 1080×1920 picked for a portrait canvas
  no longer letterboxes the program); the camera's caption shows the next
  start's size while stopped (a stopped status keeps the last start's).
  `projector_open` stores the parsed (trimmed) canvas id. View › Reset UI
  clears the chosen canvas (after going back to editing the main one).
  The app reads an extra canvas's own scene/preview only while it doesn't
  follow main: the daemon drops them without an event when a canvas starts
  following (`canvas_link` → `Some(true)`), so `Session::scene_on`/
  `preview_on` go by the setup shown. Offscreen `menu:OWNER` opens menus
  at the preview's centre; `press:LABEL` on the chevron shows the canvas
  menu under it, as a click does. Snapshot state `vcam-target:VALUE`.
- 2026-10-18 (#120, #96, #95): **Extra canvases in the app.** One canvas
  monitor beside the others shows the *chosen* extra canvas (vertical or
  any `[[canvases]]`; the first by default, the choice kept in the window
  prefs as `canvas_monitor`); its line's tag is the canvas's name and its
  ⌄ menu lists the canvases (with sizes, a check on the shown one) before
  show/edit/record/stream/replay/settings and the projector items. Picked
  over a row of tabs above the monitor (both rendered offscreen,
  canvas-ui/scratch/design/): the menu scales to 8 canvases in a 20%
  column, takes no height from the picture, and keeps the line as it was
  with one canvas; tabs were one click faster but unreadable past ~4 in a
  narrow column. Edit <name> Canvas edits the chosen canvas (gestures go
  to `scenes[].canvases.<id>` through `model::route_canvas`, generalising
  `route_vertical`); Studio Mode's own preview for an independent canvas
  is generalised the same way (`App::stage_extra_preview`). Session keeps
  each canvas's scene/preview in maps (`canvas_scenes`/`canvas_previews`),
  so a `canvas_scene_changed` for one canvas no longer overwrites another's
  (it used to: every non-main event set `vertical_scene`). Projectors:
  `projector_open` `canvas` takes any extra canvas id of the live setup
  (refused with the canvases listed otherwise); multiview gains
  `canvases: [ids]` (their programs fill the last tiles, at most half the
  grid's scene tiles: 4 in top-8, 3 in 3x3, 1 in 2x2; refused beyond, with
  the numbers); `vertical: true` still works and counts as `vertical` in
  that list. The app's multiview shows every extra canvas that fits.
  Virtual camera: `target = "canvas"` with `canvas = ID` (the vertical one
  stays `target = "vertical"`); choosing a canvas whose size the camera
  doesn't advertise picks 1080×1920 or 1920×1080 by its shape (letterboxed).
  Replay per canvas in the app: the canvas monitor's menu starts, stops and
  saves the chosen canvas's buffer (the Controls dock stays the main
  canvas's). Settings tabs take `stream@ID` and `video@ID`. The daemon's
  waiting full-screen projector list is capped at 16 (#95).

- 2026-10-18 (#84 review fixes): **Governor per canvas; inline ids.**
  The live governor (#110) classifies stream slots by canvas
  (`streams::slot_canvas`), with one bit per canvas
  (`governor::canvas_slot`: main 0, vertical 1, the rest in first-seen
  order, capped at 32): a congested extra canvas retunes only its own
  encoders (verified: shorts 5000 → 2500 → 1250 kb/s while main stayed
  6000 with 0 drops). Every canvas's program file is protected, not an
  ISO; `info.canvases` is sampled; protected loss routes per program file
  (`record::program_canvas`). `CanvasId` now stores its id inline (≤ 32
  bytes, `Copy`), replacing the interner: requests can't exhaust
  anything, and `program()` (a `String`) can't fall back to `program`. A
  `[[canvases]]` `recording = {}` (file name left at the vertical
  default) records as "{date} {time} <id>" (`filename_for`). The engine
  refuses a setup whose new canvas ids don't fit its 16 lifetime output
  slots (apply/plan, with the field path) instead of skipping them. Every
  extra canvas's replay encoder is placed before the writers (#68).
  Aitum vendor requests follow the plugin's `vertical-canvas.cpp`
  (`current_scene`, size matching with `success: false`, `switch_scene`
  on every independent canvas). `<id>-preview` is reserved as a source
  id too.
- 2026-10-18 (#84): **Canvases are ids; `[vertical]` is one of them.**
  `CanvasId` went from a two-variant enum to an id (`Copy`, stored
  inline since the review fixes, ≤ 32 chars; `CanvasId::Main`/`Vertical`
  stay as constants usable in patterns), so every existing vertical path
  generalised by compiler error rather than a parallel copy. More canvases
  are `[[canvases]]`: the same `VerticalCanvas` type with an `id` (and
  `name`), arrangements under `scenes[].canvases.<id>`; `[vertical]` keeps
  its file shape (existing setups unchanged, byte for byte). Program
  names `<id>-program`, monitor targets `<id>-program`, `<id>-preview`,
  `<id>-scene:ID`, stream slots `<id>:<dest>`, Keychain accounts
  `canvas:<id>:<dest>:<account>` (vertical's unchanged); at most 8 extra
  canvases. Engine: `ExtraOutputs` is an insert-only array of `OnceLock`
  slots (as the vertical OnceLock was: a canvas turned off keeps its
  output, disabled), compositors in a map, worker names `<id>
  compositor`; recording plans one program file per recorded canvas, the
  manifest's `canvas_scenes.<id>` for independent `[[canvases]]` (vertical
  keeps `vertical_scenes`). Every extra canvas's program file shares the
  "vertical" writer priority (protected, after main). A canvas follows the
  main scene (its own layout, or letterboxed) or switches its own
  (`follow_main = false`), as vertical did. Destinations stay nested in
  their canvas's `stream` (binding = nesting); a destination may now set
  its own rate control, keyframe interval and audio bitrate, and equal
  formats share one encoder hub per canvas (a different audio bitrate is
  its own encode). Recording a canvas is opt-in (`record`). Capacity /
  Optimize: every canvas's program and stream encodes count; the live
  estimate's second flag means "any non-main canvas streams"; Optimize
  patches `[[canvases]]` entries by id (the setup merge is by id).
  Method table: `canvas_add|set|remove` (API, CLI `isocast canvas`, MCP),
  `status.canvases` and `engine.canvases` (the `vertical*` fields stay).
  obs-websocket: GetCanvasList, and CallVendorRequest for Aitum Vertical
  Canvas's `aitum-vertical-canvas` (version, switch_scene, get_scene,
  get_scenes, status, start/stop_streaming). OBS import: Aitum
  Multistream outputs → main destinations (own bitrate/size), Vertical
  Canvas canvases → `[vertical]` + `[[canvases]]` `vertical-N` with their
  outputs; their own scenes and WHIP/SRT outputs are reported, keys never
  read. App: Settings › Video › Canvases (list of disclosure rows, the open
  canvas's size/rate/follow/record/remove, Add with shape presets), picked
  over a segmented picker + editor (scales to 8, shows each canvas's
  summary, matches Stream's destination rows); Stream › Streams of lists
  every canvas; Start Streaming starts every canvas whose destinations
  have keys. Not done (follow-ups): replay for `[[canvases]]` in the app,
  projectors/monitors and the editor for `[[canvases]]` in the app (the
  engine serves their targets), virtual camera target for them, live
  scene linking during a take (as #96).

- 2026-10-18 (#105 W.3): Mac independent texture lifetime is now fixed,
  not relaxed. `CVMetalTextureCache` must survive the client CVMetalTexture
  for buffer recycling: raw CoreVideo-only tests with explicit autorelease
  scopes give32/31/30/29 slots if the cache dies first, all32 if it dies last.
  `gpu.rs` Keep holds CVMetalTexture, PixelBuffer, then a cloned cache (Rust
  field drop order); its HAL retirement callback drains a native autorelease
  scope. No new cache flush, lock, fence, wait or pixel copy; cached IOSurface
  import/target paths unchanged. Native GPU work-only contract and256 BGRA/
  NV12 cross-thread lifecycles pass. E2e19/19 and full Mac gates pass. Normal
  filtered ISO stop loops passed on both baseline/candidate; the failing case
  specifically requires owned textures surviving their importer. Evidence:
  windows/scratch/gpu-lifetime. This is a standalone Tier1 fix, no Windows
  adapter implementation mixed in.

- 2026-10-18 (#105 #110 #114): merge qualification retains main's profile-
  aware CLI capacity_check and estimate::check, not the isolated check or
  check_estimates helper. `isocast record` explicitly sends new_project:true,
  into:None. Non-mac calibration loads compatible existing status but never
  launches a worker (even ISOCAST_CALIBRATION=on); explicit calibration is
  refused before scratch/worker creation, not remembered as a failed build.
  Mac behaviour stays unchanged. Native merged e2e19/19 and Ctrl-C pass;
  an isolated recording's self-contained .isoprj opens in the real editor
  after its private timeline workspace is gone (preview +3file lanes).
  Evidence: windows/scratch/merge-main/{e2e,record-editor}. GPU lifetime
  investigation is a separate subsequent Tier-1 task, not part of this merge.

- 2026-10-17 (#105 W.3): dedicated Platform contract harness/test files were
  explicitly approved. New isocast-platform-contracts workers plus Python
  runner use one generic implementation per case, isolated fresh directories,
  bounded/killed/reaped children and explicit unavailable results. Windows
  native is unavailable until the capture lane supplies Platform/FileWriter;
  StubPlatform core results never qualify native Windows. Mac normal/pressure
  recordings, A/V, commit/kill/recovery and capture teardown pass, as do the
  original GPU tests. The stronger early-importer-drop GPU test exposed:
  after three completed command-only rounds, availability is31,30,29 of32.
  Pixel correctness and completion callbacks pass; source-free render and
  empty submission do not restore slots. No production backend edits made.
  Ryan delegated the lifetime decision: retain the independent-lifetime
  contract, because the API returns owned GPU textures. Full qualification
  was blocked pending Mac importer/runtime investigation, resolved by the
  2026-10-18 cache-ownership fix above without changing the contract.
  Final review finds no source-frame retainer in the test: wgpu-core drops
  its trackers before the completion callback. The correlated boundary is
  CVMetalTexture outliving CVMetalTextureCache; exact native ownership is
  was still unproved at this checkpoint. Normal importer-outlives-textures production ordering was
  not shown broken. Evidence: windows/scratch/w3a/gpu-three-rounds/gpu.err.
- 2026-10-17 (#105 W.3): don't equate capture callback-arrival and presentation
  time. Real ScreenCaptureKit samples were7.5–13.3ms ahead of arrival; the
  contract permits one requested presentation period, not an invented10ms
  latency target. Core tick checks are engine arithmetic at a native origin,
  not a two-hour drift measurement. GPU readback proves pixels/lifetimes, not
  absence of internal copies; no zero-copy performance claim from these tests.

- 2026-10-17 (#105 W.2b/W.2c): Windows startup recovery is storage-level,
  independent of NativePlatform's capture/encoder implementation. Engine cfg
  routes to `isocast_mux::recovery`; Unix bodies and Mac writers are unchanged.
  A pinned exclusive Directory spans manifest reads, media repair and atomic
  replacement. Live Windows checkpoints keep their original shared Directory;
  ownership failure fails closed. Read-only inspect first: already finished
  movies can be open in players or readonly. Historical Failed/Interrupted
  facts survive failed revalidation; fresh Recording accepted counts become
  unknown unless indexes prove them. Preserve unknown JSON extensions, pauses
  and producer errors; pause verification uses every track, not longest end.
- 2026-10-17 (#105): Windows10 rejects RootDirectory with Win32
  SetFileInformationByHandle(FileRenameInfo), error87, despite newer docs.
  Use native NtSetInformationFile(FileRenameInformation) with the WDK ABI and
  the still-open synchronous DELETE-owned temp. Flush before/after publication;
  never delete a published file after a flush error. Crash-orphan dot-prefixed
  .partial files are ignored and retained, not deleted through guessed names.
  Unsupported cloud/reparse/locking volumes still fail closed; no power-loss
  directory-durability or write/fsync fault-injection guarantee is claimed.
- 2026-10-17 (#105): enumerate the pinned folder while retaining only100
  newest manifest names, not all media names or a10000-entry cutoff. Return
  caller-spelled paths for status/root matching, never reopen them during the
  pass. At record start inspect only still-Recording manifests; historical
  Failed/Interrupted media is rechecked at launch, avoiding two full scans
  per new take. Native100-manifest fixture234ms→9ms; not a full encoder/startup
  benchmark. Windows native recording contract tests still need the capture
  lane's encoder integration; packet writer and daemon repair are qualified.

- 2026-10-17 (#105 W.2a): `isocast-mux` is a separately qualified packet
  writer, not a switch of Mac recording backends. Both Mac crash-safe VT/AAC
  and legacy AVAssetWriter paths stay untouched. Inputs are NVENC/MF-compatible
  compressed access units + avcC/hvcC, no raw pixels; 48 kHz mono/stereo AAC-LC
  with explicit encoder priming. New identifying brand `isw2` prevents old
  Mac recovery from guessing Apple's 2112-sample priming for Windows files.
  B-frames/config changes are refused; adapters must drain encoders before
  commit, preserve time origin and split before the 1.5M total-sample budget
  (headroom includes pending/held outputs). W.2b now wires anchored recovery
  and atomic truthful manifests into the daemon (see the newer entry above).
- 2026-10-17 (#105 W.2a): a fragment's index is initially `free`, followed
  by mdat and a metadata-hashed `icmt` trailer; only then is its kind patched
  to moof. Recovery validates complete records, keeps all populated tracks,
  and seals an abandoned tail with one `!` byte before appending its final
  index. Necessary, not cosmetic: a missing final hash byte equal to zero
  was completed by recovery's next moov-size byte, promoting an uncommitted
  fragment on retry. Native repro and regression live in windows/scratch
  REPORT.md. The seal never touches verified media; no truncation, no payload
  reread/hash of hours of media. `unverified_tail` explains omitted bytes.
- 2026-10-17 (#105 W.2a): pinned-directory leases bind creation/repair and
  `.isocast.lock` to one object, including through a rebound folder junction.
  Windows share-read-only writable media allows compatible live readers but
  refuses competing writers/deletes; all reparse children (including cloud
  placeholders) and hard links are refused. Use local non-synced folders;
  OneDrive/network volumes are not qualified. Read inspections retain shared
  leases, so keep them short before exclusive recovery. This is process-kill
  and explicit file-flush qualification, not a power-cut/directory-fsync claim.

- 2026-10-17 (#105 W.1d): `isocast record` is now a private daemon client,
  not in-process MacPlatform. IsolatedDaemon overrides only child storage
  paths, keeps a sole lifetime-pipe writer and reaps the real child. The
  daemon finishes on EOF (including killed CLI); `parent_owned` status
  handshake refuses older signed apps that ignore the launch flag. Successful
  exit cleans its runtime directory; failure/crash retains diagnostics.
  Private engines do not register global hotkeys. Start/Stop timeouts and
  lost event streams follow Status until the actual outcome; completed
  reports print even if later cleanup fails. `setup check` keeps the same
  capacity JSON through EstimateCapacity on a private engine. Windows still
  refuses media via StubPlatform; owned-process control/lifetime is verified.

- 2026-10-17 (#105, Ryan): Windows startup/recording performance is a
  standing design requirement. Keep WGC D3D11 textures on GPU into wgpu
  interop and native DirectX-resource NVENC; hardware MF fallback, software
  last. Async low-latency recording-quality presets and per-ISO sessions
  obey detected driver/session caps and existing priority/shedding. Measure
  launch-to-preview/ready-to-record before setting targets; parallel device
  opens, lazy work/pre-warm, no UI-thread blocking. From W.5 every report
  compares matched 1080p60/4K60+ISO scenes against isolated OBS on exon-desk
  (install allowed): startup/record-start latency, CPU/GPU/memory and drops.

- 2026-10-17 (#105 #28, W.1b): the daemon selects `NativePlatform` once
  in `platform.rs`; it is a re-export of MacPlatform on Mac (no wrapper or
  dynamic dispatch), and StubPlatform elsewhere. Media/clock/credential,
  snapshot, export and recovery calls use the trait. `app_identifier`
  gains a trait hook, delegating to the same Keychain bundle-id lookup on
  Mac. The stub is deliberately CPU-synthetic only, not a working renderer
  or recorder; unsupported native operations fail rather than pretending
  to capture, write, flush, protect secrets or measure free disk space.
  Unix IPC/process hooks and native diagnostics/hotkeys/plugins are still
  W.1c; their policy is not changed here.

- 2026-10-17 (#105 #28, W.1a): W.0 hosted run 39043755843 is green on
  both OSes, merged in 603c3ac. W.1 is sliced into engine build boundary,
  daemon platform selection, OS services/IPC, CLI and UI. The engine keeps
  exactly its macOS Metal features; Windows selects DX12. Non-Unix manifest
  creation uses std's atomic `create_new` (Windows also opens the reparse
  point), not an unchecked create/truncate. File/folder sync failures still
  propagate; this is compile groundwork, not a claim of qualified Windows
  durability. Security-103 is integrated; no daemon plugin policy changes.
- 2026-10-18 (#115): **Masks are filters on the source, three kinds.**
  `shape_mask`, `gradient_mask` and `source_mask` rather than one `mask`
  kind with a sub-type, so the schema-made forms and field-path errors
  stay flat. Positions and sizes are fractions of the masked picture (a
  mask fits any resolution; the plugin's pixel units were its most
  common complaint), radii and feathers its pixels. A source mask's
  source is `mask_source` / `mask_scene`, not `source`/`scene`, because
  list patches key unnamed filters by `source` before `kind`. Masks
  combine with what masks before them left of the picture before the run
  (`intersect` multiplies; `add`/`subtract` read that picture through an
  extra binding). Mask animations carry their stops in the filter (not
  `[[keyframes]]`, whose properties are an item's); `clock` runs on the
  engine clock, `on_air`/`shown` from when the source's filters first
  drew. Source masks apply where the program draws (and scene
  snapshots); a source's own preview and its ISO file stay unmasked. A
  mask source with no frame yet hides what it masks. Scene masks draw
  the scene on its own canvas just before the masked source's filters,
  their animations on the engine clock. Handles: the "frame" design (the
  canvas's selection ring, 8 handles and a turn knob) over the "corner
  brackets" one: the same language as the canvas, edges to pull one side.
- 2026-10-18 (#115, review fixes): **What a mask shows is on air with
  what it masks; a missing mask hides; filters run at the tick.** The
  on-air list (media playback, browser loading) now includes what masks
  show, so a mask-only video plays and restarts like a shown one. A
  source mask without a picture yet makes its source fully transparent,
  whatever invert, threshold or combine say. Every live filter owner
  (program, scene feeds, filtered ISO files, source previews) gets the
  engine's epoch, so `clock` mask animations agree everywhere; snapshots
  show mask animations at their start. The program draws filters and
  mask scenes for the frame's presentation tick, not the later
  frame-selection limit (scroll and render delay move with it: half a
  frame earlier, consistently). A scene mask's placement uses the
  scene's canvas size, whatever size a monitor draws it at. Masks may
  reach at most 16 deep (MAX_MASK_DEPTH), validated with a field path.
- 2026-10-18 (#115): **Advanced Masks import reads menus by their order.**
  Without the plugin's code, its numbered settings (mask_type, shape_type,
  feather type, channel list) are read in the order its interface lists
  them; the scene collections on GitHub agree (mask_type 2 with a
  mask_source, shape_type 6 with heart_size). Unknown keys (polygon/star
  sizes, gradient width, thresholds) aren't guessed; a note says so.

- 2026-10-17 (#114): **A recording goes to a new project unless one was
  chosen.** The current timeline used to take every finished take, and it
  never ended, so separate recordings became takes of one edit. Now the
  engine keeps a record target (`current.json`'s `record`): none (each
  recording makes a new project folder and, when its first take begins, a
  timeline named like the folder) or a timeline (the take goes into the
  folder of its last take). A session keeps the timeline it started with
  (splits and retakes too); `timeline open`/`project open` only change
  the current timeline (what the editor shows); `timeline new` also
  chooses the new one; idle `retake` records into the timeline it retakes.
  A `current.json` without `record` is migrated once: target none, a notice
  (`record_target.notes`, `migrated`) until a choice is made.
  App, two designs tried offscreen (`record-target/scratch/shots`): A, a
  quiet full-width row under Start Recording ("New project" / "Take 4 ·
  Untitled", with a menu); B, a square button beside it (folder or stack
  icon, lit when adding to a timeline). Picked A because the words are
  visible where you press Record; a dock too short for one more row (a
  window under about 1000 px tall) shows B instead, and a squeezed one
  says "Record" beside it (#108's pattern). The status bar always names
  the target; the stop toast says where it went and replaces "Recording
  saved" (its Show in Finder action became Open in Editor).

- 2026-10-17 (#111 F1–F4): **A recent publication isn't evidence that a
  scene is still on air.** Keeping tile compositors avoided pipeline churn
  but left the 100 ms reuse lease's cut hitch. Track program and preview
  publishers separately; a cut replaces that publisher's scene at completed
  hand-off, transitions/idle clear it. Reuse is time-only, pixel publication
  still rejects old versions/timestamps. Keep pools across reuse and rate-
  limit raster rebuilds to one/second. One-off scene JPEGs skip stale small
  rasters. Repeated ISO GPU losses also back off one/second, preserving
  expected-minus-written drop accounting; developer `lost:2:6` exercises
  six losses without resetting hardware. Main 215db9e integrated; fix-only
  re-review evidence in `/Volumes/IsocastBuild/perf-2/scratch/REPORT.md`.

- 2026-10-17 (perf-2 split): **Ship multiview/#111; park #27** (delegator).
  `perf-2/scratch/issue27-draft.patch` preserves the full frame-permit,
  native retention, capture-copy and diagnostics draft, removed from the
  source tree. It counted shared recording/live buffers twice and refused
  protected frames in overload; future work needs distinct-buffer accounting
  or independent output pools, not further cap tuning. Main a9b661ff is
  integrated. Tile-size composition, unmixed completed-scene reuse, GPU
  poll containment, snapshot/ISO loss recovery and camera pacing remain.
  Reduced-tree full ISO stress (18 files) has zero program/stream/audio
  losses; strict e2e 18/18, recovery/camera checks and workspace gates pass.
  An initial stress loss also reproduced on baseline in an unchanged-code
  paired control; retain both results, not just the pass. Multiview A/B
  confirms GPU/memory savings, with a smaller one-pair CPU improvement.
  Evidence: `/Volumes/IsocastBuild/perf-2/scratch/REPORT.md`.

- 2026-10-17 (#104): **NLE export (`timeline_export_nle`) writes stacked
  tracks, not a multicam clip**: one video track per ISO source with the
  program on top, every clip's stretch of every file on its track, only
  the clip's angle switched on (FCPXML: lanes off one gap per clip, audio
  lanes below with `srcEnable`). Same layout in all three formats, an
  angle switch stays a visible cut, and Resolve reads it; multicam in
  FCPXML is a follow-up. A clip's first frame in each file is the one on
  screen at its in point (`floor((in - start_offset) * fps)`), as the
  export picks it; whole-number rates only today (the writer handles
  `num/den` rates like 60000/1001, unit-tested, but recordings tick at
  whole rates). Media paths are absolute everywhere (Resolve imported no
  OTIO clip with a relative or `file://` path). The project file is the
  only file written (atomically: a uniquely named partial made with
  create_new, then renamed; numbered names are taken by making them with
  create_new). `--output` must end in the format's extension, so it can't
  be a take's file, and replaces a file only if it carries the export's
  marker ("Isocast timeline export": an XML comment after the DOCTYPE,
  OTIO `metadata.isocast.export`) or with `replace` / `--replace`.
- 2026-10-17 (#104): **Resolve gets xmeml (`--app resolve`)**, measured on
  Resolve 21.1.1 free on isocast-m2-b: xmeml renders frame for frame as the
  EDITED file (pattern decode 0/212 frames off, PSNR >= 46 dB, same level);
  Resolve ignores FCPXML `srcEnable` (doubled program sound, +6 dB, and the
  uncropped program around a 1:1 crop), and OTIO has no crops. In xmeml
  Resolve fits a picture of another size first and counts Basic Motion's
  scale from that fit (Premiere: from the picture's own pixels), and reads
  a clip's `trackindex` as one channel (a stereo clip became a mono track
  of its left channel, -3 dB), so its file has a track per channel, panned.
  FCPXML `trim-rect` values are all percent of the picture's **height**
  (Apple's DTD: "a percentage of original frame height"), left and right
  too; positions are percent of the sequence's height. Resolve 21.1 reads
  trim-rect that way (fix round 1: the first version wrote left/right in
  percent of the width, and Resolve's re-export showed them scaled by
  height/width); the "Resolve units" others reverse-engineered aren't used.
- 2026-10-17 (#112): **Letterboxed vertical canvas, nested scenes and
  keyframes.** A nested scene is drawn once per canvas at that canvas's
  size, so on the vertical canvas a letterboxed scene (`Scene::letterboxed`:
  no vertical arrangement, no `layout`) is a 1080x1920 picture with the
  main one in a band. `fit_items` now crops items that show one to that
  band (`CanvasFit::crop`, rounded like the fitted rects), so they look as
  on the main canvas, smaller, instead of fitted twice; at `fit = "none"`
  they are placed like rect items (the engine sizes them from the crop),
  and inside a fitted `fit = "none"` item they become `stretch` at their
  main size. A scene with its own vertical arrangement (or layout) nested
  in a letterboxed one is still its vertical picture squeezed into the
  item. Keyframes: `for_canvas` gives fitted items fitted copies of their
  sets (`<id>-vertical-fit-…`, appended to `keyframes`; their problems are
  the main canvas's): pixel translations, border, radius and shadow scale
  for rect items, translations (px and %) for `fit = "none"` ones, crops
  map for nested scenes. Keyframe `scale` multiplies the item's own
  (fitted) transform (animate.rs `place`), so it never changes. The app's
  first vertical edit (`route_vertical`) still stores the items with the
  original sets, so their pixel keyframes are then unscaled.
  The engine-less `get --resolved --canvas vertical` sizes sources from
  the main canvas (`layout::resolve_canvas`), as the engine's frames are.
- 2026-10-17 (#109): **A scene without a vertical arrangement is
  letterboxed on the vertical canvas**, replacing "re-laid out, not
  scaled" (#86), which drew a 16:9 scene's rect items at main-canvas
  pixels at the top of the 9:16 canvas. `isocast_protocol::fit_items`
  (used by `Setup::for_canvas`, and by the app's `model::vertical_view`,
  `canvas_items` and `route_vertical`, so the editor's first vertical
  edit starts from what the canvas shows) scales rect-placed items by
  min(vw/mw, vh/mh) about the top-left and centres them (OBS's Fit to
  Screen; 1920x1080 on 1080x1920: y 656, 1080x608), with children,
  translations, radii, borders and shadows; `fit = "none"` items keep
  their size and get the factor in `transform.scale`; `layout` items are
  still laid out again on the vertical size. Not handled: keyframe
  animations in pixels and nested-scene items (drawn at the vertical
  canvas's size) are not scaled.
- 2026-10-17 (#108): **Layout audit fixes.** Narrow docks adapt instead of
  cutting: Controls says "Replay"/"Virtual Cam" when the dock is under
  200px or squeezed (read from Ui widths and the Viewport like
  `room-for-words`; reading the dock's own width loops through the
  labels); the source toolbar's idle text is "No source" under 160px and
  only the frame under 96px; the vertical monitor's head drops its size,
  then the name, then the chevron (menu still on right-click and View);
  a narrow mixer strip's fader goes down to 72px. Source rows without ISO
  let the name run into the empty ISO lane (`ListRow.label-room`).
  Gotcha: Slint `Text` layout info feeds the docks' layout, so any label
  chosen from its own dock's `width` is a binding loop error.
- 2026-10-17 (#106): **Release notes live in `docs/releases/X.Y.Z.md`.**
  They are read at the tag (preflight via the contents API, the release
  job from its checkout), embedded in the appcast as markdown and used as
  the GitHub body with GitHub's `**Full Changelog**: …/compare/vPREV...vX.Y.Z`
  line appended (PREV = newest published lower semver). GitHub's
  `--generate-notes` and the old built-in alpha notes are gone: they let
  0.0.6 ship an update window that said only "Full Changelog". A published
  release's notes are fixed by **Refresh appcast notes**
  (refresh-appcast.yml): it re-signs only the feed. Ed25519 is
  deterministic, so the same key and DMG give the same enclosure
  signature; the job requires it, and pubDate is kept by moving the
  downloaded DMG's mtime (and with it the creation date generate_appcast
  reads) back to the published one. Gotchas: generate_appcast leaves the
  enclosure **unsigned** (no error) when the key doesn't match the app's
  SUPublicEDKey, and prints pubDate in local time. Sparkle 2.10 renders
  headings, bold/italic, lists, code, quotes and links; tables lose their
  text, rules are blank and images show alt text. Later option:
  `sparkle:releaseNotesLink` to an HTML page with screenshots/video.
- 2026-10-17 (#26/#29 fix round 1): **Timeouts must not stop completion
  polling.** Once a producer's fences age past 1 s it stops submitting; the
  GPU completion worker now retries Timeout until the work completes. A
  developer-only `ISOCAST_GPU_FAULT=stall:2:2.5` withholds callbacks for
  2.5 s: main recovers, reviewed head freezes, fixed head recovers at 60 fps.
  `lost:2` simulates the device-lost flag: the cache opens a fresh device,
  and a persistent live compositor rebuilds without resetting the engine's
  clock/control. The next 4 s take keeps 240/240 frames, and projector plus
  H.264 preview/scene readbacks work in the same process/live engine.
  **Split demand is published before the split message**, separately from
  writer demand so an old writer update cannot erase it. Isolated developer
  `ISOCAST_DEBUG_SPLIT_STALL_MS=450`, seven shedding ISOs, start/stop/split/
  pause cycles: 19 takes/152 files, 4,408 shed frames, zero uncounted ticks,
  all ISO start offsets zero. Two very sparse files fooled the existing
  inferred-FPS check; direct PTS-grid/full-decode checks pass. Main 603c3ac
  integrated without committing; gates and strict e2e 18/18 pass. Re-review
  and completion of the pending merge belong to the coordinator. Details:
  `/Volumes/IsocastBuild/perf-gpu/scratch/REPORT.md`, section Fix round 1.

- 2026-10-17 (#26/#29, hardware-qualified; review pending): **GPU completion is a
  hand-off, not a wait on every producer.** Engine `Gpu::new` shares one
  process-lifetime device. Command-buffer callbacks retain native inputs
  and outputs until completion; producer-local fences preserve tick order
  when wgpu dispatches callbacks on different submit threads. Each producer
  has at most three GPU pictures in flight; one completion thread sleeps on
  the GPU and drains callbacks at the platform's real-time scheduling class.
  Program/stream frames remain YUV; native
  monitors use pooled BGRA canvases, only allocated/drawn while watched.
  Preview and watched scenes skip YUV conversion entirely. ISO pressure
  moved to `record/pressure.rs`; skipped ticks still reach file workers for
  split/stop/audio/commit accounting, without pixels. File workers request
  every picture through startup and split boundaries; native writers still
  hold if pressure rose after admission, and drain accepted work on skipped
  ticks. This does not close #27's aggregate capture/encoder pool isolation.
  Final remote gates, strict e2e (17/17), cycles/disk/ISO stress and native
  surface/camera readbacks pass. Final three-pair A/B keeps all protected
  outputs whole; the populated multiview + two projectors use 71.6→66.2%
  daemon CPU and 580.7→483.3 GPU ms/s, but live footprint is 305.6→438.1 MiB.
  One earlier program loss led to restoring real-time QoS on the completion
  worker; the final 3×64 s pairs lose zero of 11,520 program frames per build.
  These are not across-the-board CPU/memory wins. Tier-1 review remains:
  `/Volumes/IsocastBuild/perf-gpu/scratch/REPORT.md`.

- 2026-10-17 (#96 / 0.0.7): **Canvas failures are local, mixer failures shared.**
  A main compositor panic must not end healthy vertical streams; the daemon
  now selects that canvas's stream slots, just as for vertical failures.
  Poison recovery replaces the stop reason atomically (never clears it).
  Vertical Stop succeeds even when the stream already ended, preserving its
  failure reason/counters. Ordinary vertical failures recover by off/on after
  recording stops; poisoned scene state still needs an engine restart.
  Failed-file durations remain the finalized media duration, but manifest
  dropped_frames counts through the take's actual, pause-mapped end.
  **Live follow_main remains guarded:** recording binds one SessionControl
  for its entire vertical scene history; no atomic link/transition splice
  exists across its main/independent histories, split boundaries and pauses.
  Lifting only the refusal would write incorrect vertical_scenes.
  **Ryan approved feature e2e scenarios:** `vertical` covers dual recordings,
  splits, independent scenes and a local vertical stream, with strict zero
  protected loss. Replay has independent per-canvas buffers (shared profile
  settings, CLI/MCP/hotkeys; OBS stays main-only). The virtual camera's
  `target = "vertical"` uses the vertical size/fps, including 1080×1920.
  The vertical replay encoder is placed before recording even with default
  replay settings/no stream: a mid-take start must not disturb protected
  writers (#68). M2-b's default start kept both 8 s/240-frame files loss-free.
  Qualification: vertical-007/scratch/REPORT.md.
- 2026-10-17 (#64): **HE-AAC in Matroska: AudioToolbox's HE-AAC decoder
  plays 2112 frames (at the stream's output rate) later than its LC
  decoder** and reports no PrimeInfo; webm.rs parses the
  AudioSpecificConfig (explicit SBR/PS object types, or the
  backward-compatible 0x2b7 sync extension) and drops that much more after
  CodecDelay. Measured by cross-correlating the decode with the source
  burst (aac_at files: HE-AAC 48 kHz and 44.1 kHz, HE-AAC v2 48 kHz): lag
  0, 0, 1 frame (was 2112, 2299, 2112). HE-AAC v2's core is mono, so the
  decoder's input format takes the ASC's channel count (a stereo input was
  refused with '!dat': v2 never played). Untagged VP8 stays Rec. 709 (the
  2026-10-15 choice; OBS 28+ treats untagged video as 709 too). No
  seeking: media sources only play from the start, so there is nothing to
  exercise until a seek API exists.
- 2026-10-17 (#95): **a full-screen projector whose display isn't
  connected when the engine starts waits for it** (`waiting` in
  `<setup>.projectors.json`) instead of being dropped: it opens again on
  `device_changed` (display added) or the next `projector_list` (the app's
  first call, which covers a display that attached before the first device
  inventory), with reason "the display NAME was connected". Opening the
  same view by hand, or closing all, forgets it. A windowed projector last
  on a gone display opens with no frame (the app places it) rather than
  off screen. obs-websocket GetMonitorList lists Isocast's displays by
  index (positions 0: Isocast doesn't know them); OpenVideoMixProjector and
  OpenSourceProjector map onto `projector_open` (monitorIndex -1 or absent:
  a window; projectorGeometry is accepted and ignored).
- 2026-10-17 (#102): the .isoprj writer warns once per folder (until the
  two agree) when the file's revision of a timeline is higher than the
  engine's, re-checks the recording gate right before each write
  (`Written::Busy` re-queues), and `graceful_shutdown` flushes it (an
  empty path on its channel; waits up to 3 s). The start-up pass probes
  each folder on a helper thread (2 s) so a sleeping network disk can't
  hold up the writer. Relink by folder remaps other timelines only for
  offline takes (an online clip is relinked alone). Two loose copies of a
  file opened from two folders keep sharing one uid: giving the second a
  new one would make re-opening it import again (the file isn't
  rewritten read-only), so the harmless duplicate stays.

- 2026-10-17 (#103): **A plugin can't make a new recordings folder.**
  Recordings folders become sandbox rules for every plugin with a
  `recordings` grant. Before this, a plugin granted `start` (or `apply`)
  could name any folder and then read it, or write it with a `write`
  grant. Now a folder a plugin's request gives must already be one, or
  the default `~/Movies/Isocast`, or else the request is refused with
  -32002 and the error lists the folders it may use. This covers `start`'s
  `folder` (its `projects::root_of`, checked before the setup applies and
  again when it is remembered), a `recording.folder` that `apply`
  changes (which also covers `start{setup}` and `init_setup`), and
  imported profiles (`add_documents`). Paths are compared as written, not
  resolved: otherwise a plugin could pass a link in its data folder that
  points to a recordings folder, then point it somewhere else.
  `library_switch` to a profile the user made stays allowed (the user
  chose that folder). How a request is known to be a plugin's: `handle`
  sets the thread-local `PluginCaller` for each request it answers on a
  plugin connection, and those calls run on that thread. Rejected: keeping
  plugin-added folders but leaving them out of the sandboxes, because it
  would mean tracking where each folder came from through the registry
  and the setup files. Plugin-added folders remembered before this fix
  stay in the registry. Verified with a real attacker plugin, base against
  head (security-103 scratch t103-*.json).
- 2026-10-17 (#56 F3/F4, #77): plugin sandboxes are also refreshed by a
  5 s poll (`SANDBOX_POLL`, started with the first plugin process), so a
  standalone `isocast record` or a project-named folder made while idle
  reaches them without waiting for an engine request. A sandbox is also
  remade when a folder it denies as "named like a project" has since
  become a project (the poll can see a recording's folder between its
  `mkdir` and its project file). Export healing now rewrites only the
  paths that exist in the folder the export is in now, so takes of a
  project that moved somewhere else keep their paths. The stream
  encoder's `retap` returns false when the parked worker has already
  ended, and the join then starts a new hub instead of failing.
- 2026-10-17 (#105, W.0 CI round 3): **`.isoprj` relative paths use `/`
  on disk on both OSes.** Normalize known take paths only at Windows'
  `write_atomic` boundary (takes, clips, removed/recording, undo/redo).
  Absolute/drive-relative/UNC/verbatim references keep their native syntax
  and may need relinking on a different OS; no guesses about drive mappings.
  Readers/schema/version stay unchanged, unknown fields are untouched, and
  Unix names with literal backslashes stay as they were. The failing test
  used `/Users/...` as absolute on Windows (which needs a drive too).
  HOME audit: native Windows uses USERPROFILE, Isocast state/logs under
  LOCALAPPDATA/Isocast, OBS under APPDATA/obs-studio; Unix paths unchanged.
  Named-pipe isolation and Windows ACL/crash-durability qualification remain
  W.2/W.6/W.7, not promises made by portable-crate CI.

- 2026-10-17 (#105, W.0 CI round 2): **obs-websocket randomness on Windows
  comes from the OS, not `/dev/urandom`.** Hosted run 39042059588 reaches
  the tests after clippy passes: password generation and both server tests
  fail opening that Unix path. Non-Unix now uses getrandom 0.4 (already
  locked for diagnostics), with `std` for `io::Error` conversion; its
  Windows 10+ backend calls ProcessPrng. Failure propagates, never a
  predictable fallback. Unix retains its exact random source and error
  behavior. No test is skipped; protocol tests await the next Windows run.

- 2026-10-17 (#105, W.0 CI round 1): snapshot runner qualified end to end
  by the delegator, Actions run 39041442122. Ubuntu build/clippy/tests pass;
  Windows builds but clippy rejects diagnostics' Unix-only I/O imports and
  native-relay seed snapshot helpers. Their consumers already have
  `cfg(unix)`; matching that gate changes no macOS/Linux behavior and does
  not disable the portable redactor. Windows tests have not run yet.

- 2026-10-17 (#105, W.0): **Windows starts on Actions, not a local Windows
  machine.** PLAN W is the ordered lane; Ryan supplies hardware after the
  hosted alpha checklist. W.0 covers protocol, diagnostics and obs-websocket
  only, not a runnable recorder. The engine still imports Unix
  `OpenOptionsExt` in `record/mod.rs::write_manifest_with`; client IPC is
  `UnixStream`. Those need real seams, not success-shaped Windows stubs.
  `scripts/windows-ci.sh` exports clean HEAD, scans it with
  `publish-snapshot.sh --scan-only DIR`, and uses its own public checkout
  under `/Volumes/IsocastBuild/windows/scratch/public`, never the release
  snapshot checkout. Its only push ref is `ci/windows`, with an exact lease;
  results are keyed by the public snapshot SHA under `scratch/runs/`.
  The first real Actions run is still pending the delegator's commit/push.
  `docs/architecture.md`, `recording.md` and `alpha-test.md` contain older
  2 s/AVAssetWriter language: the current implementation and
  `docs/crash-safe-takes.md` define the 500 ms/default-on durability target.

- 2026-10-16 (#101): **An audio source that stops while recording is in
  the manifest per file**: `files[].audio_stops` (source, track,
  `time_seconds` on the take's clock, optional `resumed_seconds`,
  reason), the same in the stop reply's `files`, and a line in `errors`
  (status `failed`, like a video file that ended early). Optional and
  skipped when empty: other manifests and replies are byte-identical.
  The time is the end of the last audio the source delivered (sync offset
  included), not when the stop was noticed: in the VM the 600 Hz tone
  ended within 10 ms of it in every file. Media and browser audio (sparse)
  use the time the stop was noticed. The housekeeping thread that marks a
  source failed notes the stop; the recording rewrites the manifest at
  once (crash-safe). Only the program mix picks up a reopened source
  (ISO routes are fixed, and ISO-listed sources are pinned, so never
  retried), so `resumed_seconds` is on program tracks only. Pre-existing,
  not changed: when a program-mix input fails and when it reopens, the
  mixer restarts and the whole program track loses ~0.2 s (main and this
  branch alike, in the VM's resume case).
  Test kit: /Volumes/IsocastBuild/audio-stop-manifest/scratch/vm
  (t101.py, analyse101.py).
- 2026-10-16 (#96): **A contained vertical failure ends the vertical
  streams and clears on off/on.** The `"vertical compositor"` WorkerFailed
  fails only `vertical:` stream slots (`fail_canvas_streams`); main's
  mixer/compositor failure still fails every stream (the vertical ones
  share the mix; a main compositor failure ending healthy vertical
  streams is a known follow-up). `stream_start --canvas vertical` checks
  `canvas_stopped` under the state lock that adds the streams, so a
  failure after the check still ends them (the worker sets the stop
  before the event). `ensure_vertical_compositor` clears the program
  slot's stop (`FrameSlot::resume`) when it spawns a new compositor,
  except when the canvas's scene state is broken (scene-lock poison):
  then it stays stopped and says only an engine restart recovers it.
  Off/on is refused while recording with a vertical canvas, so recovery
  in place waits for the take to end. The developer hook
  `ISOCAST_COMPOSITOR_FAIL_AFTER=<main|vertical>=<s>` (seconds since the
  engine's epoch) now fires once per canvas per process, so the
  recovered compositor keeps running. Test kit:
  /Volumes/IsocastBuild/vertical-stop/scratch/vstop.py, vpoison.py (run
  with `hw-batch.py now cmd`).
- 2026-10-16 (#96 UI): **Scene linking stays put during a take.** The
  recording binds the vertical scene timeline to one control (main's while
  linked, the vertical canvas's own while not) when it starts; switching
  it mid-take would splice two timelines into the manifest and is tier-1
  engine work. The engine keeps refusing (`vertical.follow_main`), and
  Settings › Video now disables Follow Main Scenes while recording, with
  the reason in the group's caption. **A contained vertical failure needs
  an engine restart** (superseded: turning the canvas off and on now
  recovers it, see the entry above): the vertical CanvasOutput lives in a OnceLock and its
  program slot stays stopped, so `engine.vertical.stopped` reports why and
  the app offers Restart Engine (`shutdown_if_idle`, refused while anything
  records or streams; the window's link starts it again). **Stream page
  canvas choice:** a "Canvas" group with a Streams of segmented row (picked
  over a bar above the page: it matches the grouped rows and carries the
  explanation). **Multiview from the app shows the vertical program** in
  its last tile whenever the setup has a vertical canvas (`vertical` on
  projector_open; the CLI/MCP default stays off).
- 2026-10-16 (#95, Ryan): **Broadcast tally everywhere: red = program,
  green = preview**, the multiview's tokens (`danger`/`danger-text`/
  `danger-soft` for PGM, `onair`/`onair-text`/`onair-soft` for PVW).
  Studio Mode's PREVIEW tag was yellow (accent) and PROGRAM green; the
  live program ring and the scenes' PGM/LIVE badge were green. Studio
  Mode's preview scene gets a green PVW badge (the yellow selection
  stays: it is the list's selection). Stream state (status bar LIVE,
  stream button, LCD, Settings "Live") stays green: it is the stream's
  health, not the tally. Only the Console (dark) theme exists in the
  native app; the retired web app (isocast-app) wasn't changed.
- 2026-10-16 (#90, fix round 1): **Outside-load warnings never change ISO
  admission.** Review rejected the initial CPU/thermal hold: it reduced ISOs
  to ~2.4 fps for the entire episode without a protected-output gain. All
  three hold hooks, the 25% share cap and their shared flag are removed.
  Three counterbalanced CPU pairs against 12c6bad keep ISO medians at
  30.54 -> 31.57 fps (within baseline variation) with zero protected losses.
  Encoder-contention warnings still lead sampled stream losses by 210–428 ms;
  severe overload remains lossy (program/stream medians 242/257 -> 241/257,
  program audio 272 -> 276). No protection improvement is claimed.
  The 100 ms detector infers contention; public APIs cannot identify another
  app's sessions. It allows eight native in-flight pictures plus 150 ms of
  sustained excess over its baseline. Latency/CPU detection is blind for
  five seconds after session changes (including splits), documented on
  `Status.load_warning`; thermal/allocation failures bypass that baseline.
  Status follows each episode and clears after five quiet seconds, but
  warning events are limited to one per cause per three minutes across
  takes/splits. Four injected episodes produce four status episodes and two
  events 186 s apart. Recording, pause and connection text precede the
  appended warning (real app screenshots verified). Replay's shared main
  encoder remains warning-eligible: replay is a recorded output too; this
  has no admission effect. Thermal/allocation failures were not forced on
  hardware. Full strict e2e 17/17 and remote fmt/clippy/227 tests pass.
  Review attributed the earlier control's single compositor miss to host
  activity, not this branch; that run had no headroom warning or active hold.
  Its scratch wrapper ignored `soak.main()`'s FAIL return: do not reuse it
  without propagating the exit status. Tier-1 re-review remains; evidence:
  `/Volumes/IsocastBuild/outside-load/scratch/REPORT.md` (Fix round 1).

- 2026-10-16 (#96): **Main > vertical > ISO on shared video encoders.**
  Both canvas programs/streams still protect ISOs; persistent main pressure
  after the ISO admission share falls to 10% additionally sheds vertical
  video. ProRes files are not shed for pressure on separate H.264/HEVC engines.
  Three base-M2 full-HEVC dual-stream overloads kept main program/stream and
  all audio loss-free while vertical and ISOs shed. M2 live H.264 is now 2.4x
  HEVC, distinct from the 1.9x quality-file cost: dual HD60 6 Mb/s streams fit
  beside HD30 ISOs but not HD45 ISOs (measured bound 2.25–2.53x). Other chips
  retain the assumed cost. Live estimates count streaming per canvas.
  `streams_main()` excluding an explicitly disabled `[stream]` with no enabled
  destinations is a correction: no encoder starts for that default selection;
  explicitly starting a disabled destination with `--only` is a separate case.
  Failed-source files end at accepted media, not a timed take's future end;
  the manifest retains the finalized duration/counters despite reporting failure.
  Evidence: vertical-followups/scratch/REPORT.md. UI and live linking stay later.
- 2026-10-17 (#94): **The editor's sound is an AVPlayer of the app's own**
  over an AVComposition of each clip's program audio (what EDITED has),
  started with `setRate:time:atHostTime:` against the playhead's clock, on
  `audio.monitoring_device` (its Core Audio UID; "default" follows the
  system output). Not the engine's monitoring unit: a recording's monitoring
  is untouched, and the editor plays silently while a recording takes in
  system audio (it would record it). 2x/4x play pitch-corrected (spectral),
  reverse is silent. Picture pacing: a CVDisplayLink asks the editor window
  for each frame and says when it shows; the picture due then is drawn (the
  4 ms timer stays for offscreen and the studio's Edit view, and now steps
  if the editor has no rendering notifier). The window shows a frame ~37 ms
  after the link's output time at 60 Hz on isocast-m2-b (Metal's queue):
  `PICTURE_LATENCY` in src/sound.rs, ISOCAST_AV_OFFSET_MS overrides it;
  measured with editor-007/scratch/rig (`take.py avsync`, `av_analyze.py`):
  beeps +1.8 ms (1x) and -0.5 ms (2x, median) from their flashes.
  Rejected: an AudioUnit fed by our own AVAssetReader (resampling, latency
  and pitch by hand); one constant didn't need it.
- 2026-10-17 (#94): **An HEVC part of another bit depth is encoded again**
  in the joined file's (`join.rs matches`): an 8-bit program's EDITED file
  with a 10-bit ISO angle copied the angle's Main 10 samples (a second
  sample description, the Chrome failure #94 fixed); VideoToolbox takes
  10-bit pictures into a Main session without complaint (checked: EDITED,
  its crop export and VERTICAL all Main, one description, frame-accurate).
  The same applies to program/ISO joins of takes recorded at different
  depths (re-encoded into the first take's depth: one description beats a
  copied but unplayable second one).
- 2026-10-17 (#94, review B1): **the editor is silent whenever a capture
  could take its sound in**: a recording (or one starting), a stream not
  offline/failed on either canvas, or a running replay buffer, with a
  system_audio source (its `exclude` isn't trusted) or application_audio
  naming Isocast. Decided again on every status, setup, stream and replay
  change while playing (the sound stops at once, and comes back from the
  playhead when they stop). Streams and the main replay buffer are read
  from their events (`Session.streams`, `.replay`): `status.stream*` lags
  until the next status read, and with it the sound leaked into a stream
  and a replay (scratch/cap2). Verified on isocast-m2-b (scratch/cap3): the
  recording, the received stream and the saved replay hold afplay's 2.5 kHz
  marker and none of the editor's 1 kHz beeps; BlackHole goes silent for
  each and sounds again after. The engine excluding the app from its tap
  would be simpler (follow-up).
- 2026-10-17 (#94): setups with a source id `edited`/`vertical` are renamed
  on load (`Setup::rename_reserved_sources`, a JSON rewrite of `source`,
  `sidechain_source`, `sources`/`audio` lists and `source_audio`/
  `held_sources` keys), saved at once, with a note; OBS import and the app's
  new ids avoid every reserved id (`RESERVED_SOURCE_IDS`).
- 2026-10-17 (#94): `edited` and `vertical` are reserved source ids (the
  export's labels); an old take with such an ISO source skips that derived
  file with a reason. VERTICAL is skipped for a program already taller than
  wide or square.
- 2026-10-17 (#94): the editor motion suite is deterministic (compare.py
  identical across two lockstep runs): its takes are trimmed to exactly
  3 s / 2 s, record only the block's ISO file without sound, and mute the
  tone (the test pattern and beeps follow the engine's clock). CI runs it
  after the standard suite (~40 s).
- 2026-10-16 (#94): **One sample description per export, parameter sets
  in-band.** Frames encoded again keep the joined file's format; every
  keyframe carries its own VPS/SPS/PPS (SPS/PPS) in-band (copied keyframes
  the recording's) when the export has any re-encoded or differently
  described frame, so any keyframe decodes after a seek. Rejected: hev1
  (AVFoundation reports "Cannot Decode"); making VideoToolbox emit the
  recording's parameter sets (it can't be told, and the PPS differ);
  renumbering the re-encode's parameter set ids (needs a slice header
  rewriter). The hvcC still says array_completeness=1, which strictly
  forbids in-band sets; Chrome, Safari, AVFoundation and ffmpeg all accept
  it (checked). A sample of another profile (bit depth) keeps its own
  description; HEVC re-encodes now use the joined file's bit depth.
- 2026-10-16 (#94): **Vertical export size = the program's size on its
  side** (1920x1080 -> 1080x1920, 1280x720 -> 720x1280): standard platform
  sizes, predictable, no new setting; a separate VERTICAL file, so EDITED
  stays as it was.
- 2026-10-16 (#94): **Forward playback decodes ahead** (AVAssetReader per
  clip, 8 frames, scaled by VideoToolbox) instead of an AVPlayer over an
  AVComposition: the picture must land in a Slint image anyway, and the
  reader keeps the exact angle/clip mapping the exact-seek path uses. The
  editor's rendering notifier picks the picture due at each drawn frame;
  a timer only asks for redraws. Reverse play keeps exact seeks. No audio
  in playback (as before).

- 2026-10-16 (#86): **Two program outputs, one capture/mix/ISO set.** Each
  canvas has its own compositor, size-specific bounded output pool and
  stream encoder group; source frame slots, clock and mix are shared. Both
  program writers use protected priority. One synchronized recording/take
  intentionally starts/stops its configured program files together.
  Main replay stays main-only. Linking uses the main program/preview;
  unlinking seeds their current values (not stale independent choices),
  and collection switches reset to that collection's restored main view.
  Vertical key accounts use `canvas:vertical:<id>:<account>`: a suffix can
  collide with a valid custom RTMP application's path (reproduced and fixed).
  Main key accounts are untouched. Full HEVC recording + two HD60 streams
  exceeds the base M2; priority cannot promise clean programs when protected
  workload itself is too high. Capacity wording now says so. ProRes program
  files offload that workload: both streams/programs had zero loss while
  HEVC ISOs shed. Three fitting recording-only trials lose nothing. The
  compact side preview was chosen over a full-height 30%-width variant;
  actual app pictures and media evidence are in vertical-canvas/scratch.
- 2026-10-16 (#93): a writer whose capture stopped keeps writing for
  three frames + 50 ms after it sees the stop (the ISO frame clock also
  delivers a frame newer than its last pick before giving up), so the
  file ends on the capture's last frame. Split channels close under a
  lock shared with the sender: a split is received and answered, or
  refused at once. A file whose source gave the current take no frame a
  second in is still announced by `split`, with a note, since it may yet
  start. MIN_TAKE stays 1 s (not clearly safe to shorten).
- 2026-10-16 (#95): a remembered full-screen placement on a missing
  display: the daemon keeps it closed at load and opens a window on
  `projector open VIEW`; the app never falls back to another monitor for
  a named one. Scene feeds keep the fastest rate 2 s after its last ask;
  a failing scene-feeds thread is retried after 1 s, doubling to 30 s,
  and viewers' streams end with its error meanwhile.

- 2026-10-16 (#80, #81): **One recording split handover at a time.**
  It owns the busy flag until the new writers start and the previous take's
  writers finalize; early requests get WRONG_STATE with a retry explanation.
  This bounds the scheduled start to the existing 0.5–1.5 s window instead
  of queueing future native sessions. Normal Stop honors the newest promised
  take's one-second minimum, not only the first take's: an immediate Stop
  after Split can take ~2.5 s rather than delete announced files. A timed
  end too close to fit the new take refuses the split. Pause's durable
  commit and the minimum take are unchanged. Start's error is sent after
  its daemon session is cleared, so an immediate retry sees the real state.
  M2-b final verification: 0 phantom files / 0 VT errors in 200 split
  attempts, all second-run starts 504–1496 ms ahead; 0 stale-busy refusals.
  `/Volumes/IsocastBuild/rapid-splits/scratch/REPORT.md` (Tier-1 review pending).
- 2026-10-16 (#87): a crop is a normalized rectangle per clip
  (`Clip.crop`, fractions of the shown angle's picture, on its 2-pixel
  grid, ≥ 32 px), set over a span like an angle, so the vertical canvas
  (#86) can reuse it (a 9:16 preset is the bridge). Export applies it only
  in the EDITED file: the program and ISO files stay whole, so a crop can
  never lose picture. A cropped clip is decoded and encoded again whole,
  its crop marked as the decoded picture's clean aperture so
  VideoToolbox's letterbox transfer scales just that part to the
  program's size (black bars when the shapes differ); the scaled frame's
  clean aperture is removed so no `clap` reaches the file (checked: none).
- 2026-10-16 (#87): waveforms are made in the app (src/waves.rs, like
  thumbnails), not on the method table: one call per connection at a time
  means a seconds-long decode on the app's link would stall its edits.
  Peaks (100/s) cache in `$TMPDIR/isocast-waveforms`. Drawn as a strip
  under the frames: an overlay over the frames' lower half was tried and
  hid them (scratch/snaps/waves-compare.png on the branch's lane).
- 2026-10-16 (#87): trims stay ripple (the clips have no gaps); a roll
  (`timeline_roll`, ⌥-drag of an edge) is the non-ripple trim. Snapping
  covers clip edges, takes' markers, the marks and, for nothing but
  scrubs, not the playhead (it follows the trimmed edge).
- 2026-10-16 (G.4 projectors, #19): the daemon keeps the projector list
  (`<setup>.projectors.json` beside the setup, so throwaway engines keep
  their own) and where each view was last shown; the app opens a window
  per projector and reports its place (`projector_place`). Not the daemon
  drawing windows: the GUI stays a client. A CLI-opened projector with no
  app running waits (`shown: false`) and opens when the app starts.
  Full screen is winit's simple full screen at NSMainMenuWindowLevel+1,
  not a macOS space (instant, any display, works for accessory apps).
  Multiview tally follows the brief and OBS (red program, green preview),
  unlike Studio Mode's tags (yellow PREVIEW, green PROGRAM); its tags say
  PVW/PGM in those tally colours. Default grid: top-8 (picked over 3x3
  offscreen: bigger PVW/PGM). Scene tiles and scene projectors watch
  `scene:ID`, drawn by an engine thread of their own (one compositor per
  watched scene on one GPU device, only while watched); scene tiles ask
  30 fps, PVW/PGM and single projectors the canvas rate. A `canvas`
  field (`main`) is there for #86's second canvas.
- 2026-10-17 (#88 fix round 1): **Nothing from a project file becomes a
  path unchecked.** An imported timeline's id is used as is only if it is
  a valid id (`[a-z0-9-]`, `timeline::valid_id`), else slugged; every
  timeline write refuses an invalid id; a uid that isn't short hex-ish is
  replaced; a timeline whose takes aren't `….session.json` isn't taken in.
  **A loose .isoprj opens read-only**: the folder must be the project's
  (its `.isocast-project.json` has the file's project id, or, copied
  without it, the folder is named with the file's date prefix and holds one
  of its takes); otherwise the timelines are imported but nothing is
  written there (no identity, no writer, no rename, no damaged-file move),
  the folder never becomes a project, and Open Recent keeps the file's own
  path. Chosen over asking (an agent/CLI can't answer a dialog) and over
  refusing (a project file mailed alone should still show its edits).
  **Timelines get a random `uid`** (16 hex digits; given to old ones on
  load and written back) and match by uid when both have one; a file from
  before uids matches only on the same `created_at` and the same id or
  name, so two Macs' `untitled` timelines never replace each other.
- 2026-10-16 (#88, project files): **`.isoprj` beside
  `.isocast-project.json`, not instead of it.** The hidden file is the
  project's identity: written once, on the recording path, when the
  folder is made, and read by the engine, plugin sandboxes and recovery;
  the `.isoprj` is rewritten on every edit, so it must not be what makes a
  folder a project. The `.isoprj` carries the identity too (id, prefix,
  created_at), and `project_open` restores a missing hidden file from it.
  **The timelines folder stays the engine's working copy** (CLI
  `timeline` commands unchanged, crash recovery of takes being recorded);
  each project's file holds every timeline that uses one of its takes
  (a timeline spanning projects is in each, with `../` paths to the
  others), written from it by one background writer. That keeps every
  write into a recording folder off the recording path: the recording
  callbacks only send the folder on a channel, and the writer leaves a
  folder alone while a recording writes into it. "Migration" is the
  writer writing, at start, the files of every folder a timeline uses and
  of every project folder without one. **Merge rules:** the writer never
  overwrites a file it can't read (a newer version's or damaged; open
  moves a damaged one aside to `.isoprj.damaged`), keeps timelines in the
  file the engine doesn't have, and keeps a file's timeline whose
  `revision` is higher than the engine's; two timelines are the same if
  their ids match or (imported under another id) their name and
  `created_at` do. `project_open` takes the file's newer edits, relinks
  a timeline whose project folder moved (by project id), and imports the
  rest under a free id. **The file's name follows the project's name**
  (the folder name after its date), so a copy in a folder named otherwise
  is renamed to match when it is opened. **Open while recording** behaves
  as `timeline open`: the take being recorded lands on the opened
  timeline. **`recording.open_editor` defaults off**: a window after every
  stop gets in the way of many-take sessions and of CLI/MCP-driven
  recordings; it is a profile setting so the CLI and MCP reach it.
  **Per-clip fields the build doesn't know are kept** (`Clip::extra`), so
  #87's crops survive in project files written by either build.
- 2026-10-16 (#92): a plugin's sandbox includes a recordings folder that
  doesn't exist yet, as its nearest existing parent resolved plus the
  rest, instead of leaving it out. The first take into a new folder (the
  setup's folder, made on first use) restarted every sandboxed plugin
  (#56's refresh) even without a recordings grant; the rule (deny or
  allow) is now in place from the start. A folder later made as a link
  elsewhere resolves differently and still restarts them; a folder first
  used with `start --folder` is a new root and restarts them as before.
  The stress clients check (zero restarts) was right; no scenario change.
- 2026-10-16 (#89): a microphone's `channels` (1-based) picks device
  inputs. One channel is a 1-channel source, so the mixer's existing mono
  path plays it centred at full level (no −6 dB, unlike `audio.mono` on a
  stereo pair); two are stereo. **The ISO track holds exactly the picked
  channels** (a mono track for one), not a stereo copy: it is the raw
  source an editor wants, it matches what the device delivers for that
  input, and it halves a mono mic's AAC data; gain, balance and Mono stay
  program and monitoring only, as for every ISO track. **AVFoundation is
  asked for all of the device's channels** and only the picked ones go on
  to the engine: asked for fewer than a device has, it mixes instead of
  dropping (BlackHole 16ch asked for 2: each channel a mix of all 16 tones,
  +7.4 dB; asked for 3 or 4: silence; asked for 16: channel 16 alone at
  +0.001 dB). Reading 16 floats a frame costs nothing next to the encode.
  Without `channels` the request was exactly main's at the time (two, no
  pick), so on a >2-input interface the default was that mixdown, not
  "the first two inputs" as the 2026-10-11 note said; since the 2026-10-16
  #99 default entry below it is inputs 1 + 2.
  App picker: one Channels dropdown (Default, each input as mono, each
  odd-even pair as stereo, with Core Audio channel names) over a
  two-dropdown Left/Right design: one choice covers the common cases,
  can't pick a channel twice, and says mono or stereo.
- 2026-10-16 (#99): picked `channels` are checked against the device's
  **current** input count (Core Audio's input stream configuration), and
  AVFoundation is asked for that many; `isocast devices` lists the same
  count. AVFoundation's `formats` are cached per process: after an
  aggregate device went 2 → 18 inputs, main's engine still refused input
  5, and after 18 → 2 it accepted input 5 and recorded silence with no
  error. While recording, a Core Audio listener (stream configuration,
  nominal sample rate) and the delegate (delivered channel count) stop the
  source, like a disconnect, once a picked input is gone; the take records
  on. **A change that keeps the picked inputs records on**: in the VM an
  aggregate going 16 → 18 and 18 → 16 inputs kept BlackHole input 3 exact
  on main (−15.0 dB, no other tone), so stopping there would lose good
  audio. It can't see a reorder that keeps the count (an aggregate's
  sub-devices swapped); it logs every count change. Without `channels`
  nothing is watched (main's behaviour). An audio source that stops while
  recording (this or a disconnect) shows in status and events, but not in
  the manifest or the stop reply: its track goes on silent (pre-existing).
  Test kit: /Volumes/IsocastBuild/audio-followups/scratch/vm (agg.swift
  makes "Isocast Test Agg" and changes its sub-devices without a prompt;
  t99.py records the cases; hw/analyse99.py and hw/summarise99.sh).
- 2026-10-16 (#99 default, Ryan's call on the issue): a microphone source
  **without `channels` on a device with more than two inputs records
  inputs 1 + 2** (stereo), through the pick path (`[1, 2]`: the whole
  device read, counted in its current format, watched), instead of
  macOS's mix of every input. One- and two-input devices take main's
  exact path (same request, nothing watched). Unlike a picked source, the
  default never stops: if the device later has fewer than two inputs
  (Core Audio's count or the delivered count), input 1 is recorded in
  both channels until input 2 is back, with a warn diagnostic; every
  count change is logged. The source's channel count (2) is unchanged, so
  `setup check --json` and `devices --json` are identical to main's; the
  text `isocast devices` adds "default: inputs 1 + 2" under >2-input
  devices and the app's Channels "Default" says "inputs 1 + 2, stereo".
  Release notes must say it changes recorded audio for such interfaces.
  VM kit: /Volumes/IsocastBuild/audio-default/scratch/vm (tdef.py,
  tones.py).

- 2026-10-16 (engine hardening, #27/#30/#31/#35): engine and daemon locks
  use parking_lot. Only selected control state uses `Critical` transactions:
  an unwound mutation marks it broken for emergency finalization; a reader
  panic does not. The existing isolated panic modes add `state_read` for
  that distinction. Monitoring uses bounded per-input rings and atomic
  meters; input-list ownership and reclamation stay off the IO callback.
  AVAssetWriter audio/video appends catch Objective-C exceptions, and both
  streaming and crash-safe VT sessions drain callbacks before invalidating.
  Live taps evict oldest video above eight pictures without spending their
  separate audio reserve; source history is three frames. **#27 remains
  partial:** these bounds do not establish the aggregate budget across
  independent native encoders/file queues or isolate unconverted display
  ISOs from capture's eight surfaces. Do not close it on this evidence.
  Verification: 4,000 render calls during source churn allocate/reallocate/
  free nothing; real AVFoundation append exceptions become errors; sixteen
  eight-picture VT teardown cycles drain completely. Main's read-only panic
  ended a take; `state_read` here preserves recording and streaming, while
  `state` still finalizes all files and exits 70. Hosted e2e 16/16, M2-b
  strict recording smoke 5/5; three interleaved flagship pairs have zero
  drops and median daemon CPU 26.3% main / 26.1% head. The overloaded
  4K60+1080p60+1080p60-program+stream split workload loses ISO pictures on
  **both main and head**, not program/stream/audio; it is not a zero-ISO-drop
  qualification. Evidence and remaining checks:
  `/Volumes/IsocastBuild/hardening/scratch/REPORT.md`.
  Final c46f6de integration: gates clean (208 tests plus the separate native
  teardown check); strict M2-b record/ISO/pause/shutdown/stream 5/5, zero
  drops. Silent CoreAudio monitoring for an hour with polling and source
  churn played 172,753,920 sample frames and counted 50 underruns; its file
  kept all 108,056 frames with equal 3601.866667 s video/audio. This is a
  measured nonzero underrun count, not a claim of glitch-free monitoring.
- 2026-10-16 (#86): **Vertical canvas model.** `Setup.vertical:
  Option<VerticalCanvas>` (width 1080, height 1920, `fps` inherits the
  main canvas, `follow_main` true, `recording: Option<VerticalRecording>`,
  `stream`) and `Scene.vertical: Option<SceneLayout>` (`layout`, `items`);
  all omitted when unset, so existing files keep their bytes. A scene
  without an arrangement re-lays out its own items on the vertical size
  (not scaled). `VerticalRecording` is its own type so an explicit
  `recording = {}` gets the file name "{date} {time} vertical" and a
  default encoder; its `audio` must be empty (one shared mix,
  `recording.program.audio`), and its file name may not equal the main
  program's. `Setup::for_canvas(CanvasId)` materializes the vertical
  canvas as a plain setup (ISO off, replay/virtual camera defaults, no
  `vertical`); validation runs it once and reports only what it adds,
  under `vertical.*` and `scenes[i].vertical.*`. Source id
  `vertical-program` is reserved (`VERTICAL_PROGRAM`). Library split:
  the collection keeps size/`follow_main` (and the scenes' arrangements),
  the profile keeps `vertical.fps/recording/stream`; only the collection
  turns the canvas on. The store and profile export retain parked vertical
  output settings while the live collection has no vertical canvas.

- 2026-10-16 (#74): **obs-websocket's stream is every destination.** OBS
  has one stream output and Start Streaming starts every enabled
  destination, so GetStreamStatus, Start/Stop/ToggleStream and
  StreamStateChanged combine them as the app's status bar does (live when
  any is; counters summed over the live ones). `websocket enable` changes
  only what it is given, authentication included: after --no-password, a
  port or bind change stays without a password (a wide bind is refused);
  --password turns it back on. Its `warning` field (enable only) says when
  ISOCAST_WEBSOCKET_PASSWORD overrides the Keychain password.

- 2026-10-16 (#82): **A given-up source open frees the way for a fresh
  one.** Native opens can't be cancelled, so `bounded_open` keeps each
  timed-out open's thread until macOS returns, but keys them by source and
  device (the kind's settings) and allows 2 stuck per key (32 in all); at
  the limit a new open waits within the same 8 s for one to return. The
  engine retries failed sources every 5 s, so a hung driver costs at most
  2 threads per device. Logs: "source open given up" and "given-up source
  open returned" (with seconds and outcome).
- 2026-10-16 (#83): **The release engine's executable is "Isocast
  Engine".** macOS's camera-extension approval alert names the requesting
  executable, so `release.sh` copies Cargo's `isocastd` to
  `Isocast.app/Contents/Helpers/Isocast Engine.app/Contents/MacOS/Isocast Engine`
  (CFBundleExecutable to match). Kept: the bundle id `dev.isocast.isocastd`
  (signing identity, provisioning profile, app group, TCC), Cargo's binary
  name, the development app (`Isocast Dev.app/Contents/MacOS/isocastd`) and
  every script that runs `target/release/isocastd`, and the logs' process
  label `isocastd` (log file names, crash/diagnostics bundles). Finders of
  the release engine accept both names (`api::is_engine_executable`): the
  client's bundled helper, the permissions preflight self-spawn and the
  extension's producer path pin. Not "Isocast": the app's own window and
  menus already are, and two "Isocast" processes would be confusing.

- 2026-10-16 (#79): **Recording-native submission pressure, not just the
  writer queue.** On m2-b, five 1080p60 ISOs plus program blocked all six
  crash-safe writer threads in VTCompressionSessionEncodeFrame (~17.7 ms
  per picture), leaving `pending_video` empty while each upstream 64-packet
  mixed A/V channel filled. These files record the tone directly: not mixer
  starvation. Program input age reached ~431 ms; native samples found
  968–976/983 samples per writer in EncodeFrame. The crash-safe writer now
  publishes a protected native submission's frame-budget overrun, excluding
  its first picture (#60/#41), and keeps it across audio-only drains. ISO
  startup backlogs also wait while a protected output is stalled; accepted
  pictures are retained, audio/output still drain, and commit/finalization
  explicitly force accepted video through. Merely time-bounding each drain
  did not stop startup audio loss: every audio packet still submitted more
  blocked ISO video. That experiment and diagnostic logging were removed.
  Three interleaved 20 s A/B pairs: baseline six takes lose 48–55 program
  frames / 494–561 audio chunks; final 0/0. Program submit ~17.7→0.35 ms,
  input age ~431→22 ms (medians of three runs). #60/#68 controls and full
  strict e2e 16/16 pass; final stock stress isos and workspace fmt/clippy/
  212 tests/doctests pass. Evidence: capacity-limit/scratch/REPORT.md on the
  build volume. Legacy AVAssetWriter is unchanged; all proof uses the
  default crash-safe path.
- 2026-10-16 (#79): **M2 capacity has per-picture overhead and overload
  recovery loss.** Keep the measured 800 Mpx/s engine, add 250k equivalent
  pixels per H.264/HEVC picture on the measured base M2 only: three sessions
  (4K+HD ISOs, HD program) are 792 Mpx/s, while six HD sessions at the same
  raw 747 Mpx/s are 837 Mpx/s. Both behaviors were measured; lowering the
  engine rate alone would falsely reject the first. Four HD ISOs remain
  loss-free. Under overload, the M2 table budgets half the theoretical spare
  ISO share for pressure pauses and slow recovery: five HD ISOs predict
  ~53% loss (observed 38–55% over 20 s), eight plus stream ~82% (73–87%).
  Other chips retain their previous estimates; stream H.264's 1.9x cost
  remains assumed. This is a cautious short-take estimate, not an exact
  throughput forecast. No controller constants or codec settings changed.

- 2026-10-16 (S.2): **The stress suite checks correctness, not speed**
  (`scripts/stress.py`, hw-batch kind `stress`). Each scenario gets its
  own engine; the bar is no lost or damaged file (every file a start or
  split returned must exist in a complete manifest and decode with its
  frames accounted), no program/stream drop or audio dropout, no crash,
  panic or slow shutdown, only documented refusals from the API, and
  footprint/threads/fds back near their pre-load values (300 MB / 24 /
  32). ISO drops are reported, never failed: shedding them is the design.
  The `disk` scenario accepts a manifest `failed` with "stopped early …
  the files were finished and are playable" (the engine's own wording
  for the keep-free stop). Scene switches acknowledged within the same
  frame as the next one may be missing from the manifest (never on air).
- 2026-10-16 (#45): **`tune` stays outside the `starting` gate.** A
  benchmark takes minutes, and holding the gate made `shutdown_if_idle`
  (restart, the updater) wait instead of refuse. Tune checks
  `shutting_down` under the state lock before setting `tuning`, and
  shutdown_if_idle sets `shutting_down` before taking that lock, so one
  always sees the other. Optimize still holds the gate (no busy flag). The
  app's own busy check can't see a tune, so after an engine Busy refusal it
  waits 15 s before asking again.
- 2026-10-16 (#55): **The Pacing row is "N late · p99 X ms"**, count
  first, so the value column never elides the part that matters. The
  auto-off switch keeps a profile's custom hours and names them.

- 2026-10-16 (#77): **Detach the warm program tap, not the native session.**
  After the warm picture completes, remove both routes before publishing
  readiness. A typed `ProgramTapSource` observes engine stop while parked;
  first real join restores the routes synchronously and hands the tap to the
  worker on a one-slot channel. Share the sinks lock with parking and the
  engine-state lock with canvas updates: asynchronous re-tap let a running
  60-fps replay accept a 30-fps canvas edit (review B1, reproduced with four
  drops); ten final attempts refuse it. Keep the original audio-loss counter
  and #68's contiguous native timeline. Once used, continue encoding until
  all outputs/reservations leave, as before. A parked canvas edit is allowed;
  changed-FPS joining can use another session while the old reservation
  survives the take. Heavy M2-b edit→join qualification has no stream/ISO
  loss. Final CPU median old/fixed/no-stream: 2.919/2.359/2.432% of eight
  cores, program draws 1219–1220/0/0 per 20 s. 150 s park: 5,406 B first
  packet, +1.91 MiB RSS over 20 s after join, zero losses. Tier-1 re-review
  approves; full gates 206 tests. Full strict e2e was 15/16 (replay sync
  33.31 ms, not reproduced by two baseline/two fixed controls or the final
  targeted stream/replay pass); do not claim an established baseline cause.
  Evidence: `/Volumes/IsocastBuild/parked-tap/scratch/REPORT.md`.

- 2026-10-16 (#4): console processes (CLI, app, UI) write their clean-exit
  mark with a plain `fsync`, not `sync_all` (F_FULLFSYNC), and prune their
  logs folder on the maintenance thread instead of before the command;
  `clean_exit` waits for that prune (5 s cap) unless isocastd answered the
  process (`isocast_diagnostics::daemon_answered`, called by the client),
  since the daemon prunes the same folder every 30 s. The daemon keeps
  F_FULLFSYNC and its synchronous start-up prune. Why: the scene-switch
  "API round trip" QA timed is `isocast scene X --cut` wall time, and the
  daemon's part is 0.02-0.05 ms; the A.6 diagnostics added 5-6 ms of disk
  syncing and scanning to every CLI run. Left: the CLI starts ~0.9 ms slower
  than at QA-2 without any logging (a larger binary and command tree).
- 2026-10-16 (#56, plugin follow-ups): (1) the writer's and repair's
  hard-link check (AssetWriter `not_plain`, `open_for_repair`) refuses a
  file with more than one name only where the volume can make hard links
  (`pathconf(_PC_LINK_MAX)` > 1; APFS 32767, exFAT and FAT say 1); any
  other count is logged and recorded/repaired. Symlinks, non-files and the
  pre-existing-entry refusal are unchanged; an APFS hard link still stops
  repair. (2) Healing a clip by manifest name gives it its project's id
  (`move_folder` takes the id; rename_project passes it too). (3) Export
  manifests are healed by `list_projects` (and with a timeline heal): one
  whose own files' folder is gone while the files are beside it is
  rewritten from that folder to its own, skipping `exporting` ones. Exports
  written outside the renamed project aren't rewritten (as rename_project).
  (4) Seatbelt rules can't change in a running process, so
  `Registry::refresh_sandboxes` (after `start` picks its project, and on
  `setup_changed`) restarts a sandboxed plugin process whose sandbox lacks
  a recordings folder that exists now or, for `write`, a not-a-project
  folder named like one; not counted as a crash. Such a plugin can miss
  that recording's `started`/`project_created`. (5) The recordings-folders
  list moved to `isocast_protocol::project::{remembered_roots,
  remember_root}`, and standalone `isocast record` adds its folder, so
  `list_projects` shows its projects. Fix round 1 (review B1): two
  processes write it, so the read/merge/write holds an flock on
  `….json.lock`, writes a `create_new` file of its own and renames it in,
  and keeps `….json.bak`; an unparseable registry is read from the `.bak`
  and moved aside as `….json.unreadable` on the next write, never
  silently replaced by `[]`. Repair asks the link limit via `fpathconf` on
  the opened file (F1).

- 2026-10-16 (#76): crash-safe files carry the take's start as creation
  and modification time in `mvhd`, `tkhd` and `mdhd` (QuickTime epoch,
  whole seconds; version 1 headers only when a time or duration passes
  32 bits, i.e. after 2040). The time is `plan_take`'s clock, the one the
  file names use, so every file of a take shares it; it is written into the
  fragmented `moov` at creation and read back from there by `recover`, so a
  repaired file keeps it. AVFoundation reads it as `creationDate`.
- 2026-10-15 (#73): **Multistream = one Stream per destination on the
  shared program encoder.** `[stream]` stays the main destination (id
  `main`, Keychain account unchanged, no migration); `[[stream.destinations]]`
  inherit everything but service, server, enabled, bitrate and size. Each
  destination is a full `Stream` (queue, reconnects, counters), so failures
  stay per destination; equal `EncoderSpec`s join one hub (`encoder.rs`
  already fanned packets out to sinks), so matching destinations are one
  encode. Keyframe requests (a reconnect) hit every sink of the hub; queues
  that aren't starting ignore `start_at`, so the live ones only get an
  extra keyframe. A different format is a second encode, admitted by
  `estimate_capacity` with the recording counted only while one runs; no
  estimate (non-macOS) admits. Added destinations' key account is
  `<service or server> [id]`. `status.stream` / obs-websocket stay the
  main destination; `status.streams` has all; StreamStart/Stop return
  `Streaming` (main status flattened, so single-destination clients parse
  it as before, plus `destinations`). Start refuses as a whole on a missing
  key/server (nothing half-starts); a connection or encoder failing at start
  fails only that destination. Settings shows the per-destination state in
  the status bar's LIVE details; a dot per destination on the dock button
  was tried and dropped (can't name them). #68's record-first warm encoder
  is still the main destination's spec only: an own-format destination
  isn't pre-placed (follow-up).

- 2026-10-15 (#70, C.3c, Ryan's requirement): **The editor is its own
  window, "Isocast Editor — <timeline>"** (Window › Editor, ⌘E, or a
  clip's "Open in Editor"), never a change to the studio. Tried offscreen
  against a full-window mode over the studio
  (edit-view/scratch/snaps2/editor-window.png, editor-mode.png) and chose
  the window: the studio keeps running beside it (scenes, monitors,
  recording), it can sit on another display, and which one you are in is
  the window you are looking at. Its layout: a viewer of the angle at the
  playhead over the timeline with every ISO track. The studio's Create
  view keeps C.2's clip bar exactly (record into a timeline, retake,
  reorder, export; its thumbnails pick C.2's 8 frames of the 24 now
  made). The editor has its own keys (S, I, O, Delete, J K L, arrows,
  1–9 angles, Esc, ⌘Z/⇧⌘Z), so the studio's keys are unchanged. Clip
  names are the take's number in recording order, so a split take keeps
  one. Offscreen, `--state editor` opens it and snapshots draw it.

- 2026-10-15 (#70, C.3): **Edits are clips.** Trim, split, ripple delete
  and angle selection only change a timeline's clips (in/out points on the
  take's clock, order, and a new `angle` per clip); an angle over a span
  splits the clips at its ends, so the edited video is still a list of
  clips. Points snap to the take's program frame grid. Each edit pushes the
  clips and removed takes before it onto `undo` (at most 100, kept in the
  timeline file so undo survives a restart); a recorded take (append,
  retake) clears the history instead of being undoable, because undoing an
  edit made before a take must not drop the take. A take whose last clip is
  deleted is listed under `removed`, as `timeline_remove` does.
- 2026-10-15 (#70, C.3): **Edited export.** The edited file shows each
  clip's angle with the program's sound (the program mix; ISO files carry
  their own sources' audio, whose tracks differ between angles), in the
  program's codec, size and frame rate. Video is copied except (a) from a
  cut that isn't on a keyframe to the next keyframe (the frame on screen at
  the in point, then the rest of that interval), decoded and encoded again
  with VideoToolbox in the same codec at quality 0.85 (the take's own
  quality isn't in its manifest; 0.85 is above recordings' default, so the
  re-encoded second matches the copied ones: MSE ≤ 0.8 on the test
  pattern's bars against the source frame), and (b) angles in another
  format, encoded again whole on the program's frame grid, letterboxed. A
  part ends at its last frame starting before its end, which needs nothing
  encoded again because recordings have no reordered frames (an export
  refuses a file that has them). Audio: each part's track is read from
  its in point (a ranged reader, aligned by the first buffer's time), cut
  to the sample, and the outgoing part's next 480 samples are faded out
  under the incoming part's first 480 (linear, 10 ms, or over less where
  the outgoing file's audio ends sooner), at every join including C.1's
  take joins. The joined file's frame rate is the take's grid (the
  manifest's fps), not the file's average, which dropped frames pull
  down. Undo restores clips and the takes edits took off; takes recording
  took off meanwhile (dropped, interrupted) stay listed. An edit that
  changes nothing is no revision.
- 2026-10-15 (#3, G.3b 0.0.5): **default on, 500 ms, keep every frame.**
  Ryan chose a temporary `ISOCAST_CRASH_SAFE=0` legacy fallback for one
  release, not a new setup field; automatic next-launch repair suffices,
  including remembered project folders. Recovery presents every kept
  track to its own end, not the shortest A/V end. The approved retained
  crash harness is `scripts/kill-matrix.py`. Integrate by merging main
  505145d into the reviewed 96dc52d branch, preserving the three repair
  fix rounds. New recording encoders must take #41's first-picture turns
  (and drain startup backlog before enforcing the small ISO queue), while
  #68's shared stream/replay session stays placed before recording and
  retained across splits. Default-on qualification is in the new section
  of `/Volumes/IsocastBuild/g3b-crash-safe/scratch/REPORT.md`; it is not
  implied by the earlier default-off approval.

- 2026-10-14 (G.3b lane, branch `agent/g3b-crash-safe`, not merged in the
  alpha week): **crash-safe takes are our own encoders plus a fragmented
  movie we write and finish ourselves** (`docs/crash-safe-takes.md`).
  A `VTCompressionSession` per file with writer.rs's settings and the
  existing AudioToolbox AAC encoder feed `crash_safe/mp4.rs`: `ftyp`, a
  16-byte `free` placeholder, a fragmented `moov`, then `moof`+`mdat` per
  500 ms of media (`ISOCAST_CRASH_SAFE_INTERVAL_MS`), the first picture
  at once. Stop appends the real `moov` and turns the placeholder into one
  `mdat` header, the layout AVAssetWriter's finished movies have (their
  old fragment indexes sit inside the `mdat` the same way; checked on an
  8 s HEVC take). An unfinished file is recognised by the placeholder and
  finished from its fragments by `Platform::recover_file` at the next
  launch. Rejected: rolling sealed segments (+29–62 % instructions, a
  second AAC encode in the join, measured 2026-10-13) and a write-ahead
  sample log (a copy of every byte at Stop, or nothing but Isocast can
  open the take). The AAC priming (2112 samples) is an audio edit list
  plus `roll` sample groups in the finished file (AVAssetReader needs the
  groups to start at 0) and a video `tfdt` delay while fragmented, as in
  AVAssetWriter's fMP4 output. A pause asks every crash-safe file to
  drain its own queue and its encoder, write and flush up to the cut
  before the manifest checkpoint, so `recoverable_seconds_at_pause` (and
  the Pause reply's `recoverable_seconds`) is read back from the writers
  (video exact to the cut, audio 44–65 ms short: the priming and the
  partial packet, when the inputs arrived within 500 ms of the cut). A
  commit that fails or does not answer in 3 s leaves the pause with no
  number and its error in the reply, the manifest and the file's failure.
  Each picture is held until the next one fixes its duration, so fragment
  and final timing agree and gaps stay where they fell. Files a write
  error failed are finished from their fragments at Stop and again at
  every launch until that works. Originally `ISOCAST_CRASH_SAFE=1` only;
  the 2026-10-15 decision above supersedes that default and shipping form.
- 2026-10-15 (#64): **VP8 is decoded by a vendored libvpx, Vorbis by
  lewton.** macOS decodes neither. Pure-Rust VP8 crates were scaffolds or
  keyframe-only (WebP); the libvpx crates clone or download libvpx while
  building. crates/isocast-vp8 holds libvpx 1.17.0's VP8 decoder sources
  only (BSD-3, unchanged; provenance and the configure line in its
  build.rs; LICENSE and PATENTS go into THIRD_PARTY_NOTICES) built with
  cc, NEON chosen at compile time, ~310 KB. VP8 without alpha is copied
  into 420v frames; with alpha (a second VP8 stream in BlockAdditional,
  decoded every packet so its references stay in step) or full range,
  vImage converts to BGRA and the alpha luma becomes the alpha channel.
  Untagged VP8 is Rec. 709 like all untagged video (the VP8 spec says
  601; OBS 28+ and our VP9 path use 709). Vorbis packets are decoded to
  floats and fed to the same AudioConverter (it only resamples), so
  trimming and looping are shared; the first packet gives no sound, so the
  first sound gets its time. lewton (2021, mature, rodio's decoder) adds
  only byteorder.
  Cost on a base M2 (isocast-m2-b, hw job 20261015-220535-vp8-cmd, a
  12 Mb/s 1080p60 testsrc2 stinger, harder than real ones): decode plus
  conversion 4.4 ms a frame with alpha (3.8x real time; ffmpeg's libvpx
  1 thread 4.6 ms), 2.3 ms without; looping as a media source while
  recording, isocastd used 67% of a core with alpha and 54% without
  (baseline 7%, VP9 alpha 24%, ProRes 4444 22%), 0 dropped frames in 90 s,
  RSS flat at 243 MB (VP9 alpha 211, ProRes 393). Libvpx decodes on one
  thread unless the stream has several token partitions.
- 2026-10-15 (#33): **System and app audio through Core Audio process
  taps, ScreenCaptureKit as the fallback.** `tap.rs`: a private
  `CATapDescription` (global-but-exclude for system audio, mixdown of
  processes for an app) read through a private aggregate device that holds
  only the tap (no sub-device: an output with inputs, like AirPods, would
  otherwise open its microphone and add its streams; review B1), IOProc on Core Audio's thread stamping each
  buffer from `inInputTime.mHostTime`. Which backend: taps when System
  Audio Recording (`kTCCServiceAudioCapture`) is granted; if it isn't but
  screen recording is, SCK, so an existing setup never stops at a new
  prompt mid-recording (or fails once it's turned down); with neither, a
  tap (its prompt asks for less). Before 14.2 and with
  `ISOCAST_AUDIO_CAPTURE=screencapturekit`, SCK. `AudioHardwareCreate/
  DestroyProcessTap` are looked up with dlsym (deployment target 14.0).
  Apps match by bundle id, name, part of the name or a tool's executable
  name; a helper process belongs to the app `responsibility_get_pid_
  responsible_for_pid` names (browsers' helpers, WebKit's GPU process);
  a tool started from a terminal belongs to itself. The tap's process
  list follows `kAudioHardwarePropertyProcessObjectList` (set
  `kAudioTapPropertyDescription`, same UUID); a change of default output,
  of the tap's format or a dead aggregate rebuilds tap and device (the
  engine fills the gap). `system_audio.exclude` lists apps to leave out
  (OBS-style "all but"); under SCK only apps SCK lists can be left out.
- 2026-10-15 (#33): **A fourth permission, `system_audio`.** Preflight and
  request through TCC's SPI (`TCCAccessPreflight`/`TCCAccessRequest`,
  dlopen'd), because opening a tap without a decision shows macOS's prompt
  and blocks `AudioDeviceCreateIOProcID` until it's answered (the source
  then times out after 8 s). It is reported granted before 14.2, is never
  "needed" in the app's sheet (screen recording also records system
  audio) and doctor flags it only when a setup plays system/app audio and
  neither it nor screen recording is granted. `isocast permissions
  --request` (all) asks for it too, so first-launch onboarding does.

- 2026-10-15 (#68 review B1): **Exclude a parked gap from the native H.264
  timeline, not the engine timeline.** VT's bitrate controller treated the
  gap after the warm picture as unused budget (3 s: 1.96 MB first packet;
  reviewer 150 s: 13.6 MB and a ~0.9 GB memory spike). On the first live
  picture, subtract its distance from `warm_pts + frame_duration` from all
  native submissions, then add the offset back to returned PTS/DTS before
  interleaving. AAC, keyframe scheduling and loss accounting stay on the host
  clock. Always discard the warm output, even if a subscriber joins before
  it returns. A 150 s park now starts with 4,935 B, memory grows 0.84 MiB,
  and both streams/replay/recordings remain clean. Native cold CBR normally
  averages ~3 Mb/s in the first second at a 6 Mb/s target, then 6 Mb/s
  (also measured on main); do not call that a regression or hide an upper
  burst. The existing stream e2e now records first and checks first-packet
  size plus the first two seconds' rates. Q4's parked-tap optimization is
  deferred: retapping must preserve typed receiver ownership, audio-drop
  counter baselines and engine-stop notification, beyond this timestamp fix.

- 2026-10-15 (#68, iso-overshed follow-up): **Place, then park the shared
  H.264 session before recording writers.** With `[stream]` or non-default
  replay settings, recording reserves the existing shared program encoder,
  waits for one native picture to complete, then starts its writers. The
  reservation survives every split and ends with recording; actual stream
  and replay subscribers keep the session when recording ends. Before any
  real subscriber joins, discard tap packets and warm output, and do not
  feed AAC or advance live timestamps. Once used, keep encoding through the
  recording (including stream Stop) to preserve audio continuity and native
  pressure cleanup. The reservation is not counted in `shared_encoder`.
  A failed optional warmup warns instead of preventing a valid recording;
  readiness waits at most 3 s and honors Stop. The first continuous-encode
  reservation fixed M4 losses but retained M2 startup program drops; parking
  after one picture fixed both. Final: M4 heavy record-first 6 s ×3, 181 s,
  6×30 s splits all outputs 0 drops; M2 fit 0 and heavy program/stream 0,
  with ISO overload still counted. No admission/gate/native codec changes.
  On exonbook (14 cores), three interleaved 30 s recording-only trials with
  a configured-but-unused 720p60 stream: daemon CPU 2.266% → 2.319% of the
  whole machine (+0.743 percentage points of one core); continuous warming
  was 2.863%. ISO-only (program disabled), three 20 s pairs: 1.376% → 1.963%
  machine-wide (+8.230 core-points), all files loss-free. A parked encoder's
  tap still keeps the otherwise-idle compositor at full rate; optimizing
  that tap is a follow-up. This is CPU, not a watt/energy measurement. Default replay
  settings are always present in the model, so only non-default replay
  settings trigger a reservation without `[stream]`. Evidence and prior
  rejected-policy investigation: `/Volumes/IsocastBuild/iso-overshed/scratch/`.
- 2026-10-15 (#49, fixes-005): **an engine older than the bundled one is
  restarted; a newer one never is.** `isocast_client::stale_engine`
  compares `status.daemon.build` (absent before #40: stale) with the
  bundled helper's `CFBundleVersion`, as numbers (release.sh and build.rs
  both number builds by time), at any executable path (an older copy's or
  a dev engine on the default socket too). Only distributions check (a
  bundled helper next to the binary, no `$ISOCAST_DAEMON`); dev builds'
  plist build (`vcam-package.py stamp`) isn't the binary's, so they can't.
  The app stops it with `shutdown_if_idle` and lets the link launch the
  bundled one, with a notice; while busy a sticky notice says it restarts
  after, and it does. The CLI only warns (`status`, `start`, `stream
  start`) and suggests `isocast restart`. Two apps of different builds
  can't ping-pong: the older one sees a newer engine and leaves it.
- 2026-10-15 (#71, G.9): **obs-websocket is a client of the method table,
  in-process.** Crate isocast-obsws speaks the protocol; isocastd hands it
  an `Engine` (daemon.call with the API audit log, and an ordinary event
  watcher). OBS events that Isocast has no event for (mute, volume, names,
  scene items, transitions, settings) come from diffing the live setup on
  each `setup_changed`, so they fire whoever changed it. Names and ids:
  OBS name = Isocast name or id (later duplicates get ` [id]`); lookups
  also take the id; uuid = SHA-256(`isocast:<kind>:<id>`) as a v8 UUID;
  sceneItemId = 31-bit hash of scene id + item key (stable across
  reorders). Studio mode = a staged preview, as the hotkey does. The
  settings are per engine (`<setup>.websocket.json`), not per profile, as
  in OBS; the password is a Keychain item (`<app id>.websocket`) and the
  first enable generates and shows one; with auth on and no readable
  password it stays off (fails closed). Unmapped OBS requests answer 204
  UnknownRequestType with "Isocast does not support X" (the protocol has
  no separate "unsupported" code). It reports obsVersion 30.2.0 and
  obs-websocket 5.5.0, the versions whose requests it maps.
- 2026-10-15 (#72): **per-ISO audio is `recording.iso.source_audio`, a map
  from an ISO source's id to its audio tracks**, beside the shared
  `recording.iso.audio` (both in the scene collection, as they name its
  sources). Not `sources[].iso_audio`: sources is a list of tables and
  `audio.iso` is already that source's own-sound bool, while the ISO choice
  itself lives in `recording.iso.sources`; a map merges by key in patches
  (`{"cam": null}` = back to the default). An entry replaces the shared
  list for that file only; `audio.iso` still adds/drops the file's own
  sound on top of either list. Keys must be video sources (not necessarily
  listed in `recording.iso.sources`, so turning a file off and on keeps its
  routing); the app's Remove cascades the key and values. OBS import maps
  Source Record's "different audio" source to that source's entry; without
  it Source Record records the source's own sound (source-record.c uses the
  filter's parent), so the shared list is now left omitted instead of `[]`.
  App: Settings › Output shows "Audio: Same as Others / each source"
  checkboxes under each recorded ISO source (a menu button inside the
  sheet was tried: in the offscreen loop the sheet's PopupWindow offsets
  `absolute-position`, so the menu opened ~(500, 274) pt from its button);
  the Sources dock's ISO badge gets a right-click/Control-click menu with
  the same choices flat under Record ISO File (picked over an "ISO Audio ▸"
  submenu there: the badge menu is short, so one click), and the item's
  context menu has an "ISO Audio ▸" submenu (that menu is long).

- 2026-10-15 (#18, G.2): **the replay buffer is a sink of a shared program
  encoder.** The stream's H.264/AAC encode thread became
  `stream/encoder.rs`: outputs with an equal `EncoderSpec` subscribe to one
  running encoder (one hardware session, #41/#60), the last to leave stops
  it, as OBS's outputs share encoders. The buffer asks for the stream's spec
  whenever the setup has a `[stream]`, so stream + replay is one session
  whichever starts first, and the buffer keeps the encoder when the stream
  stops; without a `[stream]` it encodes on its own (program file size,
  1 s keyframes, ~0.1 bit/pixel up to 40 Mb/s). Recordings encode inside
  AVAssetWriter, so sharing the program *recording's* encoder needs our own
  VT session feeding a passthrough writer (G.3b's direction): follow-up.
  Not chosen: handing the buffer from its own encoder to the stream's
  mid-run (two parameter sets in one buffer, a gap at each handover).
- 2026-10-15 (#18): **saved replays are muxed by our own MOV/MP4 writer**
  (`replay/mp4.rs`, ~500 lines), not AVAssetWriter passthrough: every
  sample is in memory, so `moov` goes first with exact offsets and no
  CMSampleBuffer/format-description FFI is needed; files are written under
  a hidden `.partial` name, fsynced, then hard-linked to a free
  `Replay <date> <time>` name (rename fallback where hard links fail).
  A save ends at the picture showing when it was asked for (waiting up to
  3 s for it and its audio to leave the encoder); it starts at the newest
  keyframe at least `seconds` before that. It goes into the running
  recording's project folder, else the recordings folder (as OBS saves
  beside recordings). The buffer's own H.264 encoder publishes pressure
  like the stream's (protected ahead of ISOs).
- 2026-10-15 (#18, app): the Controls dock's seventh row (Replay Buffer +
  Save Replay, between recording and the virtual camera, OBS's order)
  moves Exit beside Settings below a 272 px dock row and tightens spacing
  below 240 px. Settings › Output has a Replay Buffer group (length, memory).
- 2026-10-15 (#66): **capacity estimate = per-chip data, not a benchmark.**
  `estimate_capacity` packs every H.264/HEVC encoder (program, stream, ISO
  files) onto the chip's encode engines, largest first onto the least busy
  (a session stays on one engine, #41), and judges the busiest engine's
  pixel rate against `media_engines.json`: fits <= 0.85, no margin <= 1.
  H.264 costs 1.9x HEVC per picture (tune on m2-b: the same ISO files
  1.28x vs 0.72x and 1.04x vs 0.60x real time), so H.264 files are offered
  HEVC and never the reverse; the stream's H.264 is counted the same
  (unmeasured on its own). ProRes isn't counted (tune ran ProRes ISOs at
  3.2x beside an HEVC program on the M2: its own engines). Chips not
  in the table get their generation's per-engine rate and their tier's
  engine count (base/Pro 1, Max 2, Ultra 4), labelled "assumed". A tune
  measurement that couldn't keep up overrules the prediction. Suggestions
  are tune's closeness costs, singles and pairs, at most one per target;
  "lower an ISO's size/fps" only exists for cameras and test patterns (a
  screen records at its own size). Adding an ISO file (Sources dock) only
  ever changes that source: as is if it keeps up (no margin counts), else
  smaller/slower, else off, and says why. "Don't show again for this
  setup" keys on `workload_key` (the outputs' sizes, rates and codecs) in
  `isocast-ui-notices.json` beside the layout prefs. Integration point for
  first-launch Optimize (#46) and the record button: call
  `Method::EstimateCapacity(CapacityParams { setup: Some(candidate), stream,
  add_iso })` per ISO candidate and apply `admission.change`; `start`
  already notes a can't-keep-up prediction.
- 2026-10-15 (#67): **The release app ships the camera extension.** It sits
  in `Isocast Engine.app/Contents/Library/SystemExtensions` (the engine,
  `dev.isocast.isocastd`, is the producer the extension admits and the app
  that activates it), built with plain swiftc by
  scripts/build-camera-extension.sh rather than Xcode, so release signing
  stays manual Developer ID codesign with the given profiles (no Xcode
  account/automatic signing on CI). Signed entitlements add
  `com.apple.application-identifier` and `com.apple.developer.team-identifier`
  (what Xcode would add; the profiles grant them) and, on the engine only,
  system-extension.install and the Team-ID app group; the CLI keeps the plain
  set (a restricted entitlement without a profile gets a binary killed).
  Profiles must be Developer ID (no device list) with >=30 days left and
  carry the signing certificate; the app group isn't required in the
  profile (Team-ID-prefixed groups need no registration for Developer ID,
  as the guide tells Ryan). **Start activates it** in release builds only:
  a development engine (no embedded extension) still never installs
  anything. Activation runs in a re-exec'd child (`--install-camera-extension
  --wait-for-approval`) because OSSystemExtensionRequest delivers to a main
  run loop the daemon doesn't run; after the approval prompt the child
  keeps the request alive (15 min) so the engine can start the camera when
  the user allows it, unless Stop came first.
- 2026-10-15 (#67, first real activation, m2-b macOS 26.7): system
  extensions must be **notarized** (an unnotarized Developer ID build fails
  activation with "code signature invalid"). CMIO reports the producer's
  signingID as "unknown", and the extension's sandbox can't look up another
  process's code by pid (EPERM) but can read its path (proc_pidpath); the
  extension now admits an app's Contents/MacOS/isocastd under /Applications.
  The approval alert names "isocastd"; the password sheets name "Login
  Items" / "Isocast Engine". Re-activating a newer build replaces the old
  one without approval; deactivation needs an admin password and the same
  build as the active one. Finder's Move to Trash leaves the camera active.
  ffmpeg's avfoundation input accepts only `-framerate 240` for this camera
  (its range check vs. the advertised 1–240 range); frames still arrive at
  the producer's rate.
- 2026-10-16 (#69, m2-b macOS 26.7, notarized Developer ID builds): an
  `OSSystemExtensionRequest` for properties lists every build sysextd knows
  (enabled, awaiting approval, uninstalling); `isocastd
  --camera-extension-state` prints it as JSON. Start replaces another active
  build with the app's in about 1.3 s with no prompt, and the camera runs
  (151/151 distinct frames). Removal from a newer or older app copy works
  after replacing the active build first; deactivating when nothing is active
  still shows a password prompt, so removal now answers "already absent"
  from the properties instead. A deactivation from an engine started over
  ssh (no login session) fails at once with OSSystemExtensionErrorDomain 13
  (authorization required) and no prompt; run the engine in the desktop
  session (`scripts/testmac.sh gui`) to test removal. The approval alert
  names the requesting executable ("isocastd") even with the helper
  registered in Launch Services and Terminal as the responsible process; the
  password sheets name the bundle ("Isocast Engine"). The extension runs as
  `_cmiodalassistants`, so it shares no app group container with the user's
  engine (no nonce channel). The SecurityAgent password sheet is not in
  screenshots; type into it blind (the typepw helper) after it appears.
- 2026-10-15 (#67): `x-apple.systempreferences:com.apple.LoginItems-Settings.extension`
  opens System Settings › General › Login Items & Extensions on macOS 26.7
  (isocast-m2-b); Camera Extensions is further down that page.

- 2026-10-15 (#58): a disabled primary footer button uses the kit's
  disabled footer style (hover fill, text-disabled label) everywhere, not
  only in the permissions step: at 40% opacity the yellow read as enabled.
  "Refused without asking" is flagged only in a login session with a
  screen (SessionGetInfo's sessionHasGraphicAccess): over ssh nobody can be
  asked, so "update" was the wrong advice. That keys on the session, not
  on who launched the engine: a Terminal-started engine on the Mac's screen
  does get prompts, so a fast refusal there is still a real no-prompt.

- 2026-10-15 (#46, first launch and Auto-Configuration): the flow is a
  wizard sheet (area I, src/areas/wizard.rs), chosen from three variants
  drawn in the real app (scratch/shots/variants in the lane's folder):
  A, a "Found in OBS" list inside Welcome; B, the sheet opening on From
  OBS before Welcome; C, Welcome's primary button importing OBS's current
  collection. B because it is the first run's own sheet (same header
  steps, footer and size, so no new visual language), it has room for
  choosing several collections, the stream-key consent and OBS's
  unreadable files, it continues straight into Goal and Summary, and
  Tools › Auto-Configuration Wizard… reuses it (OBS opens its wizard the
  same way). A crowded Welcome and had no room for the key or errors; C
  hid the other collections and every profile's settings. The order is
  permissions (area H), then the sheet if OBS has collections, else
  Welcome; Not Now leads to Welcome, whose Import from OBS… reopens the
  sheet (a file picker when nothing was found). Start with this Mac's
  Create setup leads on to Goal. Tools › Set Up from This Mac… keeps the
  old "start over" (the device steps).
- 2026-10-15 (#46): optimize's rules (isocast_protocol::optimize):
  recording favours quality, HEVC High (ProRes or a better preset kept)
  for the program and ISO files, and ISO files of every captured source
  when none are listed; streaming uses 75% of the upload in 500 kb/s
  steps, within the service's ceiling (Twitch 6000, YouTube 12000 at
  60 fps/9000 at 30, Facebook 9000, Kick 8000, custom 12000), sends
  1080p from 6 Mb/s at 60 fps (4.5 at 30), 720p from 2.5, else 480p;
  streaming alone turns ISO files off and drops to 30 fps under 4.5 Mb/s;
  Both keeps the recording's rate and ISO files. Then `tune` checks the
  result and its change is folded in; the summary line is built from the
  final setup. The stream's encoder isn't benchmarked (tune knows
  recording files only): a note says so.
- 2026-10-15 (#46): the upload test publishes filler H.264 (SPS/PPS, one
  black IDR so ffmpeg-like receivers learn the size, then filler NALs) and
  measures by the server's RTMP acknowledgements (window 256 KB), falling
  back to bytes sent in the second half; capped at 50 Mb/s, 8 s by
  default. Only Twitch (its `?bandwidthtest=true` publish never goes on
  air) and custom servers are tested; YouTube, Facebook and Kick have no
  test mode, so a test would go live: they, and a missing key, get 6 Mb/s
  (within the ceiling) and the reason. Applying in the app is one
  undoable edit (replace_setup_then), not a second optimize.
- 2026-10-15 (#61, WebM/MKV): Matroska and WebM files (EBML magic in the
  first four bytes, whatever the extension) bypass AVFoundation:
  `matroska.rs` (our own EBML reader; matroska-demuxer, the maintained
  crate, drops BlockAdditions, where VP9 alpha lives) feeds
  `webm.rs` (VideoToolbox, AudioToolbox). Untagged VP9 is treated as
  Rec. 709, like untagged video everywhere in the engine (and OBS 28+);
  ffmpeg's RGB->YUV default is 601, so an untagged ffmpeg-made file's
  reds look slightly orange (G=24 for pure red), same as an untagged
  MP4. A file's duration is its last video frame's end (from the last
  cue point on) unless the stated Duration is >100 ms longer: Matroska's
  Duration includes AAC/Opus padding, which held the last frame and
  broke beep cadence (21 ms per loop) on looping MKVs. Audio past that
  end is cut. Alpha is written into VideoToolbox's own BGRA buffer when
  it says the buffer is modifiable (it always did here), else into a
  pool copy. There is no seeking API, so Cues are only read for the end.
- 2026-10-15 (#63): stream keys live under `<outermost app's bundle id>.stream`
  (`dev.isocast.release.stream`, `dev.isocast.isocastd.stream` for the dev
  app, `dev.isocast.update-test.stream`, `dev.isocast.unbundled.stream` for
  a bare isocastd). The engine's own bundle id is `dev.isocast.isocastd` in
  every build, so it can't separate them; the enclosing Isocast.app can. A
  key under the 0.0.3 service `dev.isocast.stream` moves once if readable
  without a prompt (then the old item is removed, so a cleared key can't
  come back); an unreadable one is ignored for the daemon's lifetime. Set
  and clear also remove the old item when this build may.
- 2026-10-15 (#57): Local Network isn't in `isocast permissions`: macOS has
  no API to read its state without triggering the prompt (NWBrowser or a
  connect is the only probe), so it'd be a guess.

- 2026-10-14 (motion harness, fix round 2): suites are trusted input
  but boxed: plain scenario names, run folders only as plain children of
  --out with only the harness's own files removed, an allowlist of state
  steps, and --motion refuses to run without ISOCAST_SOCKET and
  ISOCAST_SETUP. Lag is reported only when the gain is within 20% of 1.

- 2026-10-14 (motion harness, review fix round): the app reads time
  through `crate::clock` (src/clock.rs): the wall clock normally, the
  virtual display's under `--motion`, so lockstep is deterministic
  (repeated runs draw identical frames, checked by
  scripts/motion/compare.py). A measurement that couldn't be made
  (target missing in any gesture frame, engine not caught up within 2 s)
  fails the run. Stalls are judged on the device-pixel grid.
  scripts/motion/controls.toml (a good drag passes; injected lag, jump
  and blank frame fail) is the harness's own regression check.

- 2026-10-14 (motion harness, GitHub #24): Ryan approved new test
  infrastructure for this. `isocast-ui --motion SUITE.toml` (src/motion)
  runs one scenario per process on its own offscreen platform (the
  snapshot code's, plus a virtual clock and a redraw counter), with the
  preview in image mode (no layers offscreen), and `scripts/motion.sh`
  gives each run a fresh throwaway engine. Lockstep (each frame waits up
  to 250 ms for the engine) for CI, realtime for this Mac, the real window
  for the test Macs. The lag/gap is measured against the pointer, not
  against the drawn box: drag-lag.py's "0 px" compared the box with the
  picture, both drawn from the same frame, while both trail the pointer.
  Suites are TOML (the toml crate, already the setup format).
- 2026-10-15 (nightly soak): **the hosted soak checks slow failures, never
  drop counts.** `scripts/soak.py` reuses e2e.py's `Engine` (same
  isolation) and verifies each take while recording, deleting its media,
  so disk stays at about two takes (30-min splits). Memory is the process
  footprint (`proc_pid_rusage`, what Activity Monitor shows) rather than
  RSS, measured only while recording, after a warm-up of min(10 min, 20%);
  limits: +200 MB, +16 threads, +32 fds (median of the last third minus the
  first). Baseline on isocast-m2-a: ~125 MB, 47 threads, 18 fds, flat over
  12 min. ffmpeg's receiver logs "Error during demuxing: Input/output
  error" when the sender closes; it's filtered, and disconnects are judged
  by the connection count and `reconnects`. After a crash or hang the
  report shows only that failure, not what follows from it.
- 2026-10-15 (#60): **Stream pressure must include time blocked inside
  VideoToolbox, not just our eight-frame submission limit.** In the M5 soak,
  bad takes had zero writer refusals and empty writer queues; compositor
  misses rose, while the stream's callbacks slowed from ~5–11 ms to
  ~115–123 ms with seven frames in flight and zero native encoder drops.
  On m2-b, 4K60 + 1080p60 synthetic ISOs, 1080p60 program and a 6 Mb/s
  local RTMP stream reproduced this: native EncodeFrame blocked with only
  seven outstanding submissions (six at return), never reaching our check
  for eight. Its mixed A/V tap filled and exhausted the compositor's
  32-frame pool, dropping program pictures and stream audio while ISOs
  stayed clean. Checking seven instead also missed it; do not guess Apple's
  internal window. `H264Encoder::encode` now publishes pressure when the
  native call takes a frame period, as well as before our existing bounded
  wait. A fast submission clears it. The first native picture is excluded:
  its initialization exceeded the budget on an otherwise idle encoder,
  costing 51 pictures per ISO in the review's light-load control (baseline
  0). With the exclusion, three stream starts during one healthy recording
  have zero video/audio losses, including the ISOs. No changes to #41's start gate,
  split/finalization, queues, codec settings, or the admission controller.
  Before that review fix, three interleaved six-take A/B pairs (30 s splits): baseline 18/18 bad
  program takes, 16,882 program / 18,655 stream drops and 16,629 stream
  audio dropouts; fixed 0/18 bad, zero protected video/audio losses. The
  overloaded M2 now sheds ISO pictures as intended (26,734 total); six
  recording-only split takes keep all three files loss-free. The final build
  has **zero losses through 16 post-split takes**, but two first takes lose
  9/12 program and stream pictures during cold stream startup. In the 9-drop
  run all losses already existed at callbacks=0 / in_flight=1: the first
  native submission can block beyond the pool's reserve before either
  post-call signal can act. A baseline cold-start control also loses pictures
  immediately (20 by the first sample) and keeps losing them. Do not claim
  the requested zero-total-drop acceptance passed. After three investigated
  variants, stop changing policy; next investigate isolating native warmup
  from the full program tap instead of guessing another queue threshold.
  M2-a/macOS 27 additionally verified four pre-review candidate takes with
  pause/scene switches, zero protected losses. All **249 files**, including
  failed qualification runs, passed ffprobe/full decode; final e2e 15/15,
  workspace fmt/clippy/191 tests clean. Reviewer cleared the startup
  regression; coordinator owns the remaining acceptance decision.
  Evidence and limitations:
  `/Volumes/IsocastBuild/split-drops/scratch/REPORT.md`.
- 2026-10-15 (ui/fast-loop): **the layout loop runs on Slint's
  interpreter, never the shipped app.** `--features live-slint` turns on
  Slint 1.18.1's `live-preview`: the generated `AppWindow` becomes a typed
  wrapper over slint-interpreter that compiles ui/app.slint (an absolute
  path baked in at build time) when the app starts, so the Rust side
  compiles unchanged. Default and release builds are untouched (same
  pixels in all 15 visual-shots views; the optional crates aren't in
  THIRD_PARTY_NOTICES). `--snapshot-watch` reloads in place through an
  `AppWindow::live_reload()` that build.rs adds to the generated module
  (Slint's own watcher needs an event loop the offscreen platform lacks).
  The `ui-dev` profile (release + incremental, own folder) makes an
  isocast-ui .rs edit ~3-4 s. Numbers: .slint edit -> PNG ~345 s before,
  0.9 s watching / 1.4-3.3 s one-off now.

- 2026-10-14 (Ryan): **reading OBS source is allowed; copying it isn't.**
  Agents may clone OBS Studio and its plugins outside the repo and read them
  to understand behaviour and compatibility. Nothing from them is copied,
  translated or pasted into Isocast: everything committed is written by us.
  Replaces the earlier "never read OBS source" clean-room rule.
- 2026-10-14 (Ryan): **the delegator may use cua-driver to click inside
  Isocast's own windows on Ryan's Mac** when a step needs a click and the
  command line can't do it (for example Sparkle's Install button during an
  update). Still never: answering macOS privacy/permission prompts, or
  clicking in any other app.
- 2026-10-14 (#47 developer mode): **Developer mode is the profile's
  setting, its logs are separate segments, its traces are per second.**
  - *Profile, through apply.* `[developer]` lives in the profile
    (Setup.developer, skipped when default), so Settings, the CLI, MCP and
    every app see one switch and a profile switch carries it. `debug_set`
    is a convenience that applies a merge patch (retrying a raced revision
    up to 3 times); isocastd follows the setup at once after each
    SetupChanged and once a second from the diagnostics sampler.
  - *One gate.* `isocast_diagnostics::developer()` is a relaxed atomic
    load; `emit("debug", …)`, `debug!`, the per-call API records and the
    per-frame trace counters in record/file.rs all check it first. Off
    costs nothing measurable (A/B below).
  - *Same log family, own segments, own budget.* Verbose records and traces
    go to `<launch>-developer-NNNN.jsonl` (family "developer", 200 MB),
    not a sibling *.trace.jsonl: one reader (`isocast logs`, filter by
    component "trace" or level "debug"), one redaction path, one prune, and
    the ordinary 40 MB history is never evicted. Bug reports take only each
    launch's newest developer segment.
  - *Aggregated per second.* Per-frame records would be ~240 lines/s at
    4K60 + 3 ISOs; atomics per output (frames, drops, late, age, submit,
    writer queue) are drained into one record a second instead: 2.9 KB/s
    (10.4 MB/h) at 4K60 + 3 ISOs on isocast-m2-a, 2.25 KB/s (8.1 MB/h) for
    flagship-1080. The record is written whenever the live engine runs
    (outputs empty when not recording), ~1.5 KB/s with the app open.
  - *Auto-off.* 24 h by default (0: never). When it was turned on is kept in
    `<logs>/developer-mode.json`, so restarting isocastd keeps counting;
    turning it on again restarts the clock.
  - *What "callback latency" means for files.* AVAssetWriter has no
    per-frame callback, so files report the append (submit) time and the
    writer's own queue; the H.264 stream/monitor encoders' real
    submit-to-callback latency is in `stream_encoders`. Capture-to-composite
    isn't measured per source; `age_us` is a frame's time (capture, or the
    program's tick) to its writer taking it.
- 2026-10-14 (7.7, plugin events and file access): **A project folder per
  recording session.**
  - *Name:* `<recordings>/<YYYY-MM-DD HH-MM-SS> Untitled/` (local time at
    start; ` 2`, ` 3`… if taken). A hidden `.isocast-project.json`
    (`version`, `id` = `20261014-153012[-n]`, `prefix`, `created_at`) makes
    a folder a project and keeps its id across renames. The *name* is the
    folder name after `<prefix> `, never stored, so a Finder rename shows
    up as a rename and `rename_project` is one atomic `renamex_np
    (RENAME_EXCL)` with no second file to update. The date prefix stays on
    rename (sorting, uniqueness); a taken name gets ` 2`.
  - *What a session is:* one `start` … `stop`, with all its splits and
    retakes-while-recording. A `start` (or idle `retake`) whose `folder` is
    a project folder records into it (an idle retake goes into the folder
    of the take it replaces, so retakes join their project); any other
    folder (or the setup's `recording.folder`) gets a new project inside
    it. `--folder` therefore names the recordings folder, not the files'
    folder. A start that fails before writing removes its empty folder.
  - *Engine:* record/* is unchanged; the daemon (and standalone `isocast
    record`) hand the engine the project folder as its folder, so
    manifests (paths relative to themselves), the disk checks and the
    folder lock work as before.
  - *Rename:* refused (wrong state) while the session recording into the
    project hasn't ended (including a stop finishing) or while any export
    runs; serialized with `start`'s project choice (a `projects` mutex) and
    with `retake` (the `retaking` lock, since an idle retake picks its
    folder before starting). Timelines' manifest paths are rewritten all
    or nothing (on failure the folder is renamed back); export manifests in
    the folder and `status.last` are rewritten too.
  - *Migration:* none. Flat recordings stay where they are; recovery,
    `start`'s interrupted-take check, `doctor` and bug reports look in each
    recordings folder and its newest project folders (50 at launch, 5 at
    start and in doctor, 10 for reports), so flat and project recordings
    are both recovered.
  - *Events:* appended, none renamed (`take_finished`, `project_created`,
    `project_renamed`, `device_changed`; optional `take`, `manifest`,
    `folder`, `project`, `cause` on `started`, `project`/`folder` on
    `stopped`, `project` on `marker` and `recording_paused`). The brief's
    `project.created`/`project.renamed` are spelled `project_created` /
    `project_renamed` like every other event type. `device_changed` is a
    diff of the diagnostics inventory, which already re-reads devices on
    AVFoundation/Core Audio/Core Graphics notifications (and once a
    minute), so no new polling.
- 2026-10-14 (7.7c, corrected in 7.7 fix round 1): **`recordings = "read"
  | "write"`: Seatbelt plus engine checks.** `file-write-create` permits
  creating a file and writing it through the creating descriptor, and
  denies reopening an existing file for writing, truncating, unlinking and
  renaming it. It does **not** stop creating symlinks, fifos or hard links
  (the first version claimed hard links were denied; the review showed an
  in-project hard link and a symlink being made). A planted dangling
  symlink at the next take's name made the engine record through it into
  the plugin's folder (review B1). Now:
  - the write rule requires `(vnode-type REGULAR-FILE DIRECTORY)` (no
    links or fifos), and `(deny file-link (subpath root))` applies to every
    recordings folder for every plugin;
  - it matches only folders named `<YYYY-MM-DD HH-MM-SS> …` (Isocast's
    project names), and folders so named without `.isocast-project.json`
    when the process starts are denied (Seatbelt can't test for a file);
  - denied names: `*.session.json`, `*.partial`, `.isocast*`;
  - the engine treats any entry at a name (a dangling link too) as taken
    (`unique_paths`), makes a manifest's `.partial` with
    `O_CREAT|O_EXCL|O_NOFOLLOW`, and the writer refuses a path where an
    entry exists and cancels unless what AVAssetWriter made is a plain file
    with one name (AVAssetWriter takes a path, so it can't get O_NOFOLLOW).
  Consequences documented in docs/plugins.md: a plugin can't reopen its own
  file either. Without the grant the recordings folders are explicitly
  unreadable (before, one outside the home folder was readable by every
  plugin). The roots are those known when the plugin's process starts.
- 2026-10-14 (7.7 fix round 1): `rename_project` falls back to check-then-
  `rename(2)` when `renamex_np(RENAME_EXCL)` gives ENOTSUP/EINVAL (exFAT,
  FAT, some shares), under the projects lock. Timeline clips store their
  project id; takes missing at their path are found by id (or, for older
  clips, the one project folder holding that manifest name) before
  `timeline_get`, exports and idle retakes, and the timelines rewritten, so
  a Finder rename or a kill between the folder rename and the timeline
  writes heals itself. Export manifests are not healed after a Finder
  rename (they are rewritten only by `rename_project`).
- 2026-10-15 (#51 permissions onboarding): the permissions step is area H
  (src/areas/permissions.rs, ui/areas/permissions/), shown before Welcome
  on the first launch, at launch when the live setup needs a missing
  permission (once per run; Skip hides it until next launch), from
  Settings › General › Review… and from a source's permission warning
  (`Ui.menu("permissions")`). The first run's Permissions step draws the
  same rows. Rows layout over three columns (compared in real snapshots):
  statuses and actions line up in one lane, details get the full width.
  Per-row actions: Allow… = `request_permissions` with one permission on a
  connection of its own (the engine waits up to a minute; the window's
  calls must not); Open Settings = the exact pane plus the floating
  helper; Relaunch = `shutdown_if_idle` (the app itself never needs to
  relaunch: isocastd is its own responsible process and the only one
  holding the stale answer); Check for Updates… for a refusal without a
  prompt. The helper is a native non-activating NSPanel (src/helper.rs)
  placed by CGWindowListCopyWindowInfo (System Settings' pid, layer 0),
  drawn as Console's floating surface with Mona Sans; it drags a file URL
  of `app_path`. Radii follow theme.slint (sheets 12px), not design.md's
  superseded 3px.

- 2026-10-14 (8.2h fix round 1, review findings): **Trust and safety
  corrections.**
  - *The EdDSA private key is the sole root of trust for installed copies.*
    With a valid EdDSA signature Sparkle 2.10 only checks that the new app
    is validly code signed, not that team 9G68SMNHEU signed it (the review
    installed an ad-hoc-signed build through a correctly signed appcast);
    the team is compared only in the key-rotation fallback. Developer ID
    and notarization are what the release pipeline guarantees, not
    something installed apps enforce. Custody of SPARKLE_ED_PRIVATE_KEY
    (release environment with Ryan as reviewer, 1Password) is the control.
  - *Full releases only.* `releases/latest` skips drafts and pre-releases.
    v0.0.1 is a full release now, so `latest` resolves (no appcast in it,
    so 0.0.1 finds nothing, and it has no updater anyway); 0.0.2 and every
    release meant for testers must be published as full releases.
  - *Atomic idle shutdown.* New method `shutdown_if_idle` (API; CLI and MCP
    through `isocast restart`/`restart_isocastd`; app updates): one call
    that refuses while a take (incl. paused), stream, export, virtual camera
    or encoder benchmark runs. A `starting` RwLock read-held by every
    starting call and write-held by the check makes a racing start either
    finish first (then refused shutdown) or see `shutting_down` (then
    refused start). Replaces the status-then-shutdown pair, which cut an
    accepted start in 3 of 122 restarts under a client delay
    (scratch/race/race.py).
  - *Virtual camera is busy* for updates and restart. Install on quit still
    happens when the app quits while only the camera runs (no quit
    question for the camera); the engine keeps feeding it from the old
    binary and the next launch restarts it only once the camera stops.
  - *Abort recovery.* Sparkle reports some aborts (didAbortWithError,
    didFinishUpdateCycle with an error) and the app then lets the link
    start isocastd again; it does not report an installer that dies after
    the go-ahead, so a one-minute safety net does the same (proved).
- 2026-10-14 (8.2h auto-update, #40): **Sparkle 2.10.0** updates the
  native app. Decisions:
  - *App packaging, not a method.* Updating replaces the bundle the GUI,
    CLI and engine run from; Sparkle needs AppKit, the user's consent in its
    window and the GUI process's lifetime (it installs when that process
    terminates). The engine's capabilities are unchanged by it, so "no
    GUI-only capabilities" isn't touched; a CLI `update` would be a CLI
    inside the bundle it replaces, run by agents that shouldn't install
    software. What agents do need is on the API: `status.daemon` (version,
    build, commit, executable, pid) says which engine runs.
  - *Bridge in the UI crate* (`crates/isocast-ui/src/update.rs`, objc2
    msg_send like platform.rs/quit.rs), not isocast-platform-macos, which
    links the engine, wgpu and capture: the thin client must not. Sparkle
    is loaded at run time only when Info.plist has SUFeedURL and
    SUPublicEDKey, so dev builds have no updater and need no framework.
  - *Archive: the notarized, stapled DMG*, not a zip: Sparkle installs from
    DMGs, it is the artifact already shipped and verified, and with
    `SUVerifyUpdateBeforeExtraction` a Developer ID-signed DMG is the only
    way to rotate the EdDSA key (Sparkle docs, "Rotating keys").
  - *Info.plist*: SUEnableAutomaticChecks (daily, toggle in Settings ›
    General), SUAllowsAutomaticUpdates=NO and SUAutomaticallyUpdate=NO (no
    silent install-on-quit; every install is asked), SUVerifyUpdateBeforeExtraction
    and SURequireSignedFeed. Sparkle reads those security keys from
    Info.plist only (SUHost boolForInfoDictionaryKey); the feed URL it would
    take from user defaults first, so the delegate returns Info.plist's
    and `clearFeedURLFromUserDefaults` runs at start (proved: a defaults
    SUFeedURL was cleared and never fetched; SURequireSignedFeed=NO in
    defaults was ignored).
  - *Never during a take*: scheduled checks are refused while recording
    (or paused), streaming or exporting and run when idle; Install and
    Relaunch is always postponed, then isocastd is stopped like `isocast
    restart` (status re-checked; refused if busy) before Sparkle relaunches.
    Quit › Keep Recording with a staged update lets Sparkle replace the
    bundle on termination while the old engine records (unlinked binary,
    take intact, proved); the next launch restarts that engine once idle.
  - *Embedding*: arm64-thinned, XPC services removed (only sandboxed apps
    use them), Autoupdate/Updater.app/framework signed with hardened runtime
    and no entitlements before the app (Sparkle's sandboxing guide's order).
  - *Camera extension*: since #67 (2026-10-15) the released app carries
    it in its engine helper. A Sparkle update replaces the embedded copy
    but not the activated extension, which keeps its installed version
    until something asks macOS to activate the new one (follow-up: compare
    versions at Start and re-request activation). If a release changes the
    extension's protocol, that follow-up must land first.
- 2026-10-14 (Ryan): **exonbook's "'Python' is requesting to bypass the
  system private window picker" alert stays unanswered.** Ryan doesn't use
  exonbook and leaves it open; it blocks nothing (the engine is already
  allowed as dev.isocast.isocastd). Agents never answer it. Lanes on
  exonbook prefer SSH runs without display capture; a GUI run there must
  expect the alert on screen. Root cause: GitHub #44.
- 2026-10-14 (#41, iso-drops-2): **hardware encoder sessions start in order,
  apart, and never during a teardown.** VideoToolbox places a session on one
  of the M4 Max's two video encode engines when it gets its first picture,
  by what the engines are doing at that moment, for the session's life.
  Sessions starting within a few milliseconds of each other (or while
  another one is being torn down) can land so that three share an engine in
  a mode where each runs at ~45 fps with eight pictures in flight; the
  program's queue then trips the pressure controller, the ISO share goes to
  13% and takes 43 s to recover. So the macOS writer (`writer.rs`,
  `ENCODER_START_GAP`/`claim_encoder_start`) hands each video input its first
  picture in a process-wide order: the protected program first, then the
  other files from the largest down, each at least 25 ms after the previous
  session started; pictures queue meanwhile (the ISO pending limit and the
  stall detector do not apply before the first picture). The H.264
  stream/monitor encoder takes the same turn (blocking, at most a few gaps).
  At a split, a file the split ended stays open `SPLIT_FINISH_DELAY` (200 ms)
  past the split even once its audio is complete, so no session is torn down
  while the next take's start (order alone left 17 of 20 split takes lossy:
  every other split badly placed; with the delay 0 of 20, every take's engine
  ratio 1.00). Not chosen: delaying ISO frame delivery in `run_iso` (tried:
  the gaps came out 5–40 ms through wake jitter and the GPU stage, and it
  retains frames in the source slot); an engine-side session recreation on a
  detected bad placement (AVAssetWriter cannot swap its encoder: a new file
  would be needed); any private VideoToolbox key (`PreemptiveLoadBalancing`,
  `Priority` exist but are not public). Evidence and the standalone
  benchmark: `/Volumes/IsocastBuild/iso-drops-2/scratch/REPORT.md`.
  After review: a writer that never got its first picture withdraws its
  request in `Drop`; requests older than 500 ms are pruned; ProRes writers
  take no turn. Fix round 1 (review blocker B1): the "wait for a protected
  writer that exists but has not started" hold is gone. It counted from
  `prioritize_video`, which `create_writers` calls on the *next* take's
  program writer when a split or retake is requested, so a split/retake
  within ~100 ms of a take's start held that take's not-yet-started ISOs
  for the full 500 ms and the backlog was then refused against
  `PENDING_ISO` (5–38 % of the 1 s take, 6/6 runs). Counting it from the
  file's first frame instead would be the same moment the writer's own
  ticket is requested (`start` → `append_video` → `drain_ready`, one call
  chain, microseconds apart), so the ticket's rank already gives that
  ordering and the hold was removed rather than moved. The ISO queue limit
  (`PENDING_ISO` 4) now only applies once the pictures that queued during
  the wait for the start have drained below it (`AssetWriter::backlog`),
  so a wait can never turn into refusals. A program whose first frame
  arrives > ~65 ms after the ISOs' would now start after a 1080p ISO (the
  measured-bad order) and have one #41-style take; not seen in any run.
  Known, documented: during the 200 ms closing window the old program
  writer's pressure flag is not re-evaluated, so a program already stalled
  at the split (real overload) can refuse the new take's ISOs for up to
  ~12 pictures before the new program's guard takes over.

- 2026-10-14 (#42, iso-drops): summary warnings are assembled once from the
  completed `FileReport`s in record/engine.rs, after all split writers join.
  Source id, take number, count, percentage and path identify every affected
  file on Stop and status.last (including MCP). Expected original pictures =
  frames - repeated_frames + dropped_frames; a tail repeat is stored but is
  not an extra original picture. Existing warnings/errors remain intact and
  a clean file adds no warning. The native admission policy is unchanged.

- 2026-10-14 (release-secrets): the release workflow gets a **preflight job**
  (ubuntu, no environment, read-only token) ahead of the macOS job. It
  validates the version, resolves the tag, compares Cargo.toml at the tag,
  and asks `releases/tags/<tag>` (published releases only; drafts are
  invisible to a read token and stay the finalizer's business). Published
  → `proceed=false`, the release job is skipped, the run is green with a
  notice and summary, and no approval is requested. Before, a tag push or
  manual run for a published version waited for approval, built, signed,
  notarized twice and only then failed red in `draft_release` ("absent or
  published; refusing"). The macOS job re-checks after checkout (published
  while waiting for approval → stops before any secret is decoded) and the
  finalizer's guard stays as the last word. Chosen over a verify-only mode
  for published versions because a rebuild that can't draft proves little
  and would need artifact uploads; the real first-run test is a new
  version. Runner scratch paths moved to a one-line step because `runner.*`
  is not available in job-level `env`.
- 2026-10-14: **Installer DMG via dmgbuild (8.2f).** dmgbuild 1.6.7
  (MIT, with ds_store 1.3.3 and mac_alias 2.2.3, hash-pinned in
  scripts/dmg/requirements.txt) writes the window's .DS_Store directly, so
  it runs on headless CI with no Finder/AppleScript. One layout file,
  scripts/dmg/settings.py, used by release.sh and release-finalize.py
  through scripts/dmg/make-dmg.sh (python3 if it has the exact pin, as in
  CI; otherwise a private venv keyed by requirements.txt's SHA-256 under
  ISOCAST_DMGBUILD_CACHE, default ~/Library/Caches/isocast/dmgbuild,
  installed with `uv pip install --require-hashes --no-deps` or Python
  3.10+ venv/pip; never the system Python). Installs for a key are
  serialized with /usr/bin/lockf; an invalid entry is set aside and the new
  venv renamed in, and staging is removed on exit. HFS+/UDZO as
  before; dmgbuild finds background@2x.png itself and runs `tiffutil
  -cathidpicheck` into /.background.tiff (not a .background/ folder).
  Labels: Finder draws icon labels black on a picture background in Light
  and Dark (macOS 26.7/27.0), invisible on the black art, and no .DS_Store
  setting changes that. So the art carries "Isocast" and "Applications" in
  #BBBBBB at content y 290-306, under Finder's own (~270-286 at 13 pt).
  Delegator's call; Ryan can overrule. Selecting an icon shows Finder's
  blue name pill (y 274-291) just above ours (from y 293): no overlap, the
  name just appears twice while selected.
- 2026-10-14 (resize-lag, GitHub #24): **a dragged handle's box follows
  the pointer; a moved item's box stays with the picture.** While a resize
  or crop handle is dragged, the box is drawn from each edit at once (the
  picture follows about a frame later); after the drag it waits (at most
  500 ms) for a frame that shows it there, so it never steps back. Moving
  an item keeps the 2026-10-12 choice (box drawn at the shown frame's
  layout). The held box belongs to the gesture's item and scene: any
  other selection or scene drops it at once (fix round 1). Column
  splitters set every shown dock (the mixer too) to its on-screen width
  when pressed; the dragged dock then takes room from the shown docks
  across the splitter, the mixer first, then the nearest, each down to its
  minimum, so it works with any docks hidden; Transitions | Controls
  trades between those two. A dock grows to 640pt by dragging, but one
  laid out wider isn't pulled in on press. Monitors ask isocastd for a
  size that keeps changing at most every 500 ms, and for one that has held
  for 120 ms then; every request restarts the 500 ms period.

- 2026-10-14 (Ryan): **GitHub issues are the work tracker now.** Known
  issues, follow-ups and roadmap items are filed on R44VC0RP/isocast
  (#2–#23), with milestones "Alpha week" (freeze: docs and fixes only) and
  "After alpha", plus labels `tracking`, `parked`, `maintainer`,
  `needs-hardware`, `performance`, `area:ci`, `area:release`. Issues are
  public: no machine names, scratch paths or secrets in them.
- 2026-10-14 (Ryan): **every open question answered yes.** Board-check and
  A/B scripts go into `scripts/` (#14, unblocks T.3a); the `alloc-count`
  feature is committed (#15); a cargo feature keeps wasmtime out of the
  CLI (#13); both Console contrast values change (#12); theme decisions as
  proposed, with "Blender" renamed before release (#17); the migration's
  "Recording"/"Default" names stay; the source toolbar's Properties and
  Filters use quiet buttons (docs/design.md 3.5, 3.18). Recordings go to
  the Extreme SSD: live setup `recording.folder = "/Volumes/Extreme
  SSD/Isocast"` (applied through `isocast apply`; doctor: 495 GB free,
  ~11.9 h, 621 MB/s). The capture-card question stays open.

- 2026-10-14 (8.2e): release workflow builds an existing validated version
  tag for both triggers (manual starts from main), serializes all releases,
  and gates secrets on the release environment. Only its one hosted job has
  contents:write. CI uses Xcode 26.3 because the macos-15 image still defaults
  to Xcode 16, which cannot compile the Icon Composer asset. Signing keychain
  and fingerprint are explicit overrides; the .p8 API auth trio selects
  notarization without changing local profile defaults. Temporary keychain
  password/identity masked, decoded files 0600, original search list restored
  and keychain/key files removed in always(). No credentials enter the child
  recorder. Explicit --replace-draft/--generate-notes/--verify-tag modes
  refresh the same draft, never a published release. Runtime gate uses the
  mounted signed executable and existing E2E validators (not a re-signed
  copy): 720p30 hosted, two 5 s qualifications then one checked take; <=25%
  accounted video loss per output only, tightened to zero if qualification
  is clean; audio, integrity and pulse sync remain strict. Missing tools or
  unavailable backends fail the release gate, never silently skip.

- 2026-10-14 (8.2 draft): the restored isocast-notary profile works with
  explicit login-keychain selection. Native app and DMG accepted on their
  first submissions; saved-id finalization resumed without uploading again.
  The local macOS 27.2 mount check refused a custom path inside IsocastBuild
  (Permission denied), while the OS-selected mount succeeded. The finalizer
  now parses hdiutil's plist for its exact mount point and detaches it in
  finally; strict signatures/tickets and prepared binary hashes stay required.
  Replace generated release notes with the requested short alpha feature
  summary and virtual-camera installation link. Actual matching-draft reuse
  confirms one release remains (id 412748947, draft=true). No engine/UI
  implementation changes; no publication, settings changes or git push.
- 2026-10-14 (report worker): bug reports are filed by
  services/report-worker (Cloudflare Worker, GitHub App, private R2), not the
  prefilled link. `bug_report` gains `submit` and `preview_id`. With
  `preview_id`, the zip is written from the cached preview, so the issue text
  is byte-identical to what was shown. The engine posts with /usr/bin/curl
  (no HTTP/TLS crate). It sends ISSUE.md verbatim; the worker appends only a
  footer (report id, version, Mac, crash signature). The engine and the worker
  run the same secret scan (diagnostics/src/scan.rs and
  services/report-worker/src/scan.ts; keep them in step). The rules are
  publish-snapshot.sh's patterns plus anything the redactor should have
  replaced, and the engine checks before uploading. Install id: a random 32-hex
  file beside the socket (`<socket>.install-id`), sent only as a rate-limit
  header. Endpoint: `ISOCAST_REPORT_ENDPOINT` or `REPORT_ENDPOINT` (None until
  deployed); https only, except 127.0.0.1/localhost. The app's sheet now
  reads Update Preview (left), Cancel, Save Zip Only, Send Report (primary).
  After sending: Open Issue. If sending fails, the zip is saved and Open
  GitHub Issue is offered. ISSUE.md no longer says "Nothing was uploaded".
  Dedupe compares a digest of the panic message and thread, with digits
  folded, against the newest 300 open `from-app` issues.

- 2026-10-14 (QA-3 R1): every file's audio advances with its pause-mapped
  video clock, padding silence only behind a 500 ms input allowance (plus
  any negative audio offset). This also covers an audio source that never
  emits during the take or stops at EOF. Media/browser gaps are legitimate;
  their refused routing packets, samples arriving after committed silence,
  and encoder refusals still count as losses. Continuous capture/program
  input retains timestamp-gap detection against received audio, separately
  from padded audio. No mixer/source-playback/encoder changes. Local 48 s
  repro and exonbook browser/edge checks pass; R1-REPORT.md in qa-engine
  scratch holds the evidence, plus strict 13/13 E2E and 178 release tests.
- 2026-10-14 (A.2 QA-3 R3): hotkeys_input now accepts registered-global chords.
  A single worker pairs opposite global/focused press edges of a binding and
  generation within 100 ms, using enqueue time rather than slow action time.
  It fires the first path immediately, pairs at most one counterpart, retains
  release history for delayed duplicates, suppresses repeat-down, and keeps
  fast same-path press/release pairs distinct. Either release or focused
  disconnect ends a paired push hold once. No app bridge change is necessary.
  Five edge-tracker cases plus a real-recording regression in scripts/e2e.py
  cover this; 183 tests and release all-target clippy pass.
- 2026-10-14 (A.2 R3 native delivery): the QA 301e404 daemon on macOS 27
  receives Cua desktop-targeted Ctrl-Opt-P/M globally with Finder frontmost;
  the same Cua hotkey with pid/window_id and delivery_mode foreground goes
  to the target window with no Carbon callback. Both tool replies misleadingly
  label their route global_input. A minimal Carbon/AppKit receiver confirms
  desktop delivery. No native registration/event-loop defect or new permission
  is needed; focused fallback repairs the window-targeted Isocast case.
  Optional ISOCAST_DEBUG_KEYS on isocastd records registration and native edges
  in diagnostics; the app's existing switch records its forwarding. Use a
  desktop target for unfocused automation, not a window-targeted Finder event.
- 2026-10-14 (qa-app): **A stream key stored by another build is flagged,
  not hidden.** The daemon tries to read the saved key without UI (cached
  10 s per account, cleared when a key is set or cleared) and reports
  `key_problem` in the stream status. Note: an item made by the `security`
  tool can't be replaced by Isocast at all (-25244, owner change refused);
  only items from another Isocast build are fixed by Save Key.

- 2026-10-14 (qa-app): **Never make a cached layer again.** Slint 1.18's
  skia renderer keys `layer_cache` by component address + item index and
  doesn't release it in `free_graphics_resources` (nor when the hint goes
  false): an element with `cache-rendering-hint` inside an `if` that is
  made again can find the destroyed one's entry, whose dependency tracker
  never goes dirty, and show its old picture for good (QA-3 R2). Keep such
  elements made and hide them with `visible` and a zero size; the few
  cached layers left inside an `if` (Pause, Studio Mode's heads) are now
  uncached. Report upstream with a one-line fix (component_destroyed on
  layer_cache).

- 2026-10-14 (8.2d): release preparation now stops after Developer ID signing
  the native Slint app and DMG. One `app_target` line defaults to isocast-ui,
  using its plist; the web app is opt-in. `--notarize` resumes a saved build
  independently of the checkout's current version/target, never recompiles
  or re-signs executables, preserves prepared originals, and repacks the DMG
  only to include the stapled app. The Python finalizer persists notary ids
  and archive hashes, waits at most 60 s per call with 20 s progress, and
  fails closed if an upload returned no id. An output lock serializes it.
  Drafts are looked up before creation and both asset digests checked; a
  published/mismatched release is never changed. Staged app and app actually
  mounted from the DMG must match prepared executable/plist hashes. Shared
  version and camera-compatible helper ID retained; no new entitlement or
  dependency. Main's notices were refreshed for four already-added crates.

- 2026-10-13 (8.2 follow-up): merged main 905d360's virtual-camera, hotkey
  and docs work. Keep the UI's one-line target switch and production ID;
  the packaged helper now uses canonical `dev.isocast.isocastd` (replacing
  `dev.isocast.release.engine`) because the camera extension admits that
  exact signed producer. It remains a nested Isocast Engine.app; no system
  extension is installed/activated by the DMG. Resolve version-stamping
  conflicts by preserving camera build numbers while deriving product
  versions from Cargo in embedded, dev, release and camera-host metadata.
  Distribution app/helper share one numeric build version; ship main's
  THIRD_PARTY_NOTICES. fmt, clippy, 179 tests, notices and ShellCheck pass;
  merged ad-hoc UI/CLI/engine and hotkey/vcam/timeline methods work. Three
  synthetic HEVC/AAC outputs (baseline, Wasm-filtered ISO and program) each
  have 120 frames/4.000 s, 0 drops, equal A/V, clean decode; the program take
  appears as timeline clip c1. This local-mode check does not replace the
  current notarized-DMG check blocked on the keychain profile.

- 2026-10-13 (8.2a): production bundles are `dev.isocast.release` and the
  nested `dev.isocast.release.engine` (Isocast Engine.app), independent of
  development privacy grants. The common client prefers its bundled helper
  over a dev app, also through a CLI symlink; explicit ISOCAST_DAEMON still
  wins. Distinct `isocast-app`/`isocast` filenames avoid case-insensitive-disk
  collisions. UI has no hardened-runtime exceptions; engine and standalone
  recording CLI have camera, audio-input and unsigned-executable-memory.
  Wasmtime 49 uses mmap/mprotect(RX), not MAP_JIT, so allow-jit alone would
  not cover it. An ad-hoc hardened-runtime recording with the Wasm gain
  measured -5.9994 dB for -6 dB; two 4 s HEVC/AAC takes had 120/120 frames,
  equal A/V durations, no drops and clean decodes on isocast-m2-b. This is
  local-mode evidence, not a substitute for blocked signed first launch.
  `--draft-release` uploads existing tested bytes, never rebuilds/publishes
  or pushes this checkout's history; target is the public repo's main.
  Version remains workspace 0.0.1; production, embedded and dev daemon
  metadata derive from it. No dependency added. See docs/install.md.
- 2026-10-14 (qa-app): **Resizing an item without bounds changes its
  scale.** Since qa-engine, new items have `fit: none`: the rect is the
  source's own size and `style.transform.scale` (one factor, about the
  rect's centre) sizes it. Canvas handles then keep the shape on every
  handle (Shift too: one factor can't stretch) and move the rect so the
  far edge stays; Edit Transform shows and edits the box shown, about the
  anchor; Fit to Screen scales; Stretch to Screen gives the item stretch
  bounds (OBS does the same). `fit: stretch` items resize the rect as
  before. Offscreen `wait:` now moves the snapshot clock, so a drag's
  paced edits land before the snapshot.

- 2026-10-14 (qa-app): **Interface English is en-US like OBS**
  ("Color", "Fade to Color", "Color Source"); comments may keep British
  spelling. **Dropdown menus size to their widest choice** (a hidden
  layout measures label + detail, capped at 420px), so encoder names
  aren't cut at 1024px. The Controls dock's "Stop Recor…" beside Marker
  is left to ui/app-alpha, which is reworking that row (Pause).

- 2026-10-14 (qa-app): **Paired size fields apply on their own.** The
  engine wants width and height together (color, browser, test pattern,
  camera, text); a Properties field set alone sends the other from the
  source as it is now (its running size, else the canvas). Refused edits
  show `sources[N]`/`scenes[N]` as the name (app.rs `named_paths`) until
  qa-engine names them itself.

- 2026-10-14 (qa-app): **Groups are items with children; the app makes
  them.** Group Selected Items puts sibling items in a new item (id
  `group`, `group-2`…) whose rect covers them, children offset so nothing
  moves; Ungroup is the inverse; Add → Group adds an empty one (a 1x1 rect
  at 0,0) and Move to Group moves a top-level item in, growing the group.
  Groups don't scale their children (the engine places children
  absolutely), unlike OBS. Multi-select is app-only state (⌘/Shift-click
  marks rows); every change is one setup edit. On macOS Slint reports ⌘
  as `control` and Control as `meta`, so ListRow's Control-click menu
  now checks `meta` (it opened on ⌘-click before).

- 2026-10-14 (qa-app): **Filter names are their ids.** A filter has no
  name field (its id, or kind, is shown humanized), so Rename sets the id
  from the typed name ("Warm Glow" -> `warm-glow`) and rewrites the list
  in place; Duplicate and Paste make ids unique (`-2`, `-3`). The copied
  filters live in the app until pasted (as the copied scene item does);
  pasting is one setup edit, so Cmd-Z undoes it whole.

- 2026-10-14 (qa-app): **Filtered source previews in the engine's shared
  frames** (`SharedFramesParams.filtered`, record/shared_frames.rs): the
  viewer thread runs `filter::Filters` over each frame with the source's
  filters read from the live setup per frame (a change redraws a still
  picture at once). Snapshots apply filters too but reopen the source
  each time, too slow for a live preview. Minimal hook in qa-engine's
  area: engine.rs `shared_frames` gains one parameter.

- 2026-10-14 (qa-app): **Toasts: errors fade after 8 s too** (they stayed
  until dismissed and piled up over dialogs, QA-2 #3); the pointer over one
  holds it. In a dialog the popup's stack goes right of the sheet, else
  under it, else stays hidden behind it (`Toasts.shown-in-dialog` hides
  the window's own stack meanwhile).

- 2026-10-14 (qa-app): **A list row click focuses its list** (List
  `focus-list()`; a row TouchArea used to take the press and the list never
  got focus, so ↑ ↓ F2 did nothing). Canvas presses bump
  `CanvasArea.press-serial`, which focuses the window keys again.

- 2026-10-14 (qa-app): **Canvas corner handles keep the shape; Shift frees
  them** (OBS muscle memory, QA-1 #2). Side handles stretch; Shift on a side
  keeps the shape. A shape-keeping corner snaps one edge and the other
  follows.

- 2026-10-14 (qa-app): **Quitting while recording asks, as OBS does.**
  "Keep Recording" quits the app and leaves isocastd recording (a relaunch
  shows it and Stop works); Cancel stays. ⌘Q is caught by adding
  `applicationShouldTerminate:` to winit's app delegate class at runtime
  (src/quit.rs), since winit doesn't answer it; the close button through
  Slint's `on_close_requested`.
- 2026-10-14 (delegator, implemented): **recording never waits for pacing
  qualification**. Once any media clock measures stretched ordinary sleeps,
  a process-lifetime atomic flag publishes that evidence. New clocks inherit
  critical mode at construction; already-created clocks adopt it at their
  next wait too (on a cold M4, ISO allocation can precede the first probe).
  A new daemon still starts ordinary and retains the program/source startup
  probes. Live-engine stop/start does not erase evidence. No process/RT policy,
  leeway, skip-window or admission change. `inherited_critical` distinguishes
  shared evidence from `escalations` based on the clock's own measurements.
  A constructor-only first trial still started an M4 take 100 ms late; its
  9.9 s files were genuinely missing the start, not a tail-duration fix to
  make in the writer. Adopting evidence at wait boundaries fixed that race;
  the final matrix's every file passes integrity. CPU from the preceding
  adaptive review is accepted within measurement noise (not exact equality).

- 2026-10-14 (adaptive implementation before process-wide sharing): each
  media clock owns its controller. Ordinary sleep is the default. Two actual
  sleeps more than max(period/2, 20 ms) late select critical deadlines; during
  the first two startup probes, one wake more than max(4*period, 40 ms) late
  also suffices. The first two long waits are sliced at 20 ms to learn during
  pre-roll; the steady ordinary cadence is unchanged. Overdue catch-up calls
  do not vote. The selection stays until the clock exits: successful critical
  wakes cannot establish recovery of ordinary sleeps, so there are no periodic
  lossy retries. No process/RT policy or leeway/drop-tolerance changes.
  `engine.clocks` lists active `program`, `iso:<id>`, `source:<id>:video|audio`
  and `virtual_camera` clocks, mode/escalations, 600 recent wait percentiles,
  lifetime max and >1-period count. Program and ISOs sample every tick; source
  and camera waits can be cancellation-sized slices. Atomic u64 samples and
  counters keep status readers off media locks and preserve >4.29 s stalls;
  concurrent percentile snapshots are best-effort. Camera output exercised
  the existing non-installing diagnostic IOSurface transport on both Macs,
  not installed camera clients. Original native kqueue implementation retained.

- 2026-10-13 (hosted-pacing, historical e7b4531 policy; adaptive replaces it):
  media deadlines use `Platform::sleep_until`:
  portable default sleep, macOS per-thread RAII kqueue, EV_ONESHOT with
  NOTE_ABSOLUTE|NOTE_MACHTIME|NOTE_CRITICAL|NOTE_LEEWAY. Convert host time with
  CoreMedia; retry interrupted waits at the same deadline. Coalescing is
  limited to min(remaining/4, 5 ms), not disabled everywhere. No busy waiting,
  process policy changes, RT priority, additional dependencies or hidden loss
  tolerance. Program/ISO clocks and timed synthetic/media sources use it;
  housekeeping keeps ordinary sleeps. Status adds 600-sample p50/p99 rings
  for wake lateness (including missed ticks), draw wall time and final GPU
  submission/completion wait (wall time, not GPU execution timestamps).

- 2026-10-14 (delegator): **media-clock pacing is adaptive.** The kqueue
  critical-deadline fix (agent/hosted-pacing) removes 98-100% of the drops
  in the reviewed 720p30/1080p60 program+two-ISO launchd matrix (not arbitrary
  Background loads; see Benchmarks),
  but the review measured +9-12% daemon CPU while previewing in normal
  contexts, where the defect never occurred (the app and `isocast restart`
  inherit an interactive policy). Ordinary sleeps stay the default; a clock
  switches to precise deadlines only when its wakes are measurably being
  stretched. Performance against OBS is a goal; robustness for headless
  launches shouldn't cost every user CPU.
  Follow-up (same night): recording doesn't wait for clocks to qualify;
  instead escalation is process-wide, so once any clock has seen stretched
  sleeps, clocks created later (ISOs at record start, sources, the virtual
  camera) start on precise deadlines. Normal-context CPU of the adaptive
  build (+1.4..3.7% medians, overlapping ranges, all clocks ordinary) is
  accepted as within noise.

- 2026-10-14: **A.6 hardening supersedes lifetime/unbounded pinning.**
  `stream_key_set` pins after the Keychain write succeeds. Pins are keyed by
  service/account and retain current + previous two values, mirrored in the
  relay's private seed protocol; failed writes only churn the 512-entry LRU.
  A native-relay supervisor publishes failure/recovery warnings and retries
  once a second. It discards the dead peer's seed backlog; new credentials
  remain in the bounded registry, not an outage queue. A new helper starts
  with text disclosure disabled and receives a registry snapshot before
  the ready marker. Generation-specific native log files preserve earlier
  evidence. Darwin returns ENOBUFS (not WouldBlock) for a full nonblocking
  local datagram receive buffer: this must retry, not be treated as peer death.
  No recording-engine files changed.
- 2026-10-14: **Dark window chrome.** The app sets NSApp.appearance to
  DarkAqua at launch (platform.rs `dark_appearance`, since the N.3 commit
  98d790c), so on a Mac in light appearance the title bar, traffic lights,
  menus and the open/save panels are dark like the Console window (the
  sheets are Slint's and were always dark). Checked on isocast-m2-b, which
  is in light appearance (no AppleInterfaceStyle): the real window's title
  bar is dark (/Volumes/IsocastBuild/app-alpha/scratch/shots/chrome/).
  docs/images/app.png still shows the old light bar: retake the README
  shots (scripts/screenshots/take.sh) after this merges.

- 2026-10-14: **Help › Report a Bug… (A.6 app, branch ui/app-alpha).** A
  sheet: "What happened" (one line; Return or Update Preview previews it
  again), the report's ISSUE.md (summary and triage block, hard-wrapped by
  Rust to the well's 84 columns: wrapped text in a ScrollArea is a binding
  loop; screen readers get it unwrapped) and every file with its size, all
  from `bug_report` with `preview: true` (nothing written). Create Report
  writes the zip (`bug_report`); then Open GitHub Issue opens
  github.com/R44VC0RP/isocast/issues/new?template=bug_report.yml with the
  form's fields (title, summary, triage, version, Mac and macOS from the
  preview's environment.json, attachments naming the zip) and shows the zip
  in the Finder; Show in Finder alone too. Nothing is sent: the issue page
  is posted by hand, signed in. The protocol's ISSUE_REPOSITORY_URL is still
  None, so the app falls back to R44VC0RP/isocast (the brief's). The triage
  block is often too long for a link (it carries the last metrics record):
  its largest keys are left out (named in `_left_out`) until the link is
  under 7,500 characters; the zip's ISSUE.md has all of it. A multi-line
  TextArea took no typing in this sheet (offscreen and real window); not
  chased, the one-line TextField works.

- 2026-10-14: **Pause in the app (G.3 app, branch ui/app-alpha).** While
  recording: a pause transport button left of record in the control bar
  (two bars; paused, a play triangle on the yellow tint: a new TransportKind,
  plain, for a Paper pass), a Pause/Resume toggle between Stop Recording and
  Marker in the Controls dock (so "Stop Recording" shortens further in a
  narrow dock; the tooltip keeps the words), the LCD's first cell reads
  PAUSED in the warning tone with the take's time standing still, the status
  line says how long it has been paused, and the REC details list the take's
  pauses (where each began, how long, the open one counting). The list
  follows `recording_paused` events only; a Status reply can be older than
  the events (it once duplicated pauses), so Status only seeds an open pause
  for a window opened mid-pause. Pause hotkeys are rows in Settings ›
  Hotkeys. Pause/Resume use `pause`/`resume`; nothing app-only.

- 2026-10-14: **Settings → Hotkeys and the focused-key bridge (G.5 app,
  branch ui/app-alpha).** The page lists the profile's bindings in OBS's
  groups (Recording incl. pause, Streaming, Scenes: a switch per scene,
  Studio Mode, Sources: mute/unmute/push-to-talk/push-to-mute per audio
  source and show/hide per scene item, Timeline), then "Other" for
  bindings no row matches (CLI/MCP ones with other arguments), then
  Isocast's fixed keys. A row's chord field records on click (Space or
  Return from the keyboard); each change is one `hotkeys_set` `patch` at
  the `revision` shown (a stale revision is read again and retried once);
  refusals and the OS's registration warnings show under the row, the
  profile's `warnings` as a callout. Changes apply at once (not with the
  sheet's OK), as the daemon owns them. New binding ids are
  action-plus-arguments ("scene-switch-intro"), never reusing a configured
  id. Chords come from AppKit key codes through an NSEvent local monitor
  (src/keys.rs; Slint only gives characters, and the daemon's chords are
  physical US positions); the same monitor is the focused bridge: every
  press/release that `Chord::parse` accepts goes to `hotkeys_input` on one
  persistent connection from its own thread, keys still held are released
  when the app resigns active or the window loses focus. New dependency:
  block2 (already in the lock through objc2) for the monitor's block.
  Gotcha (updated by QA-3 R3): a registered chord can still reach the app,
  especially for window-targeted posted events; the daemon accepts both
  paths and pairs their press edges instead of trusting registration alone.
  A key-up of a key pressed with Command
  never arrives (AppKit), see the next entry. ISOCAST_DEBUG_KEYS=1 logs
  every edge and the daemon's reply to stderr.

- 2026-10-13: **Virtual camera in the app (G.1, branch ui/app-alpha).**
  Controls dock: a "Virtual Camera" toggle under recording (OBS's place)
  with a gear beside it; the gear opens a popover of grouped rows: Show
  (Program, Preview, A scene, A source), the scene or video source, Size
  (the canvas's, or one of the extension's six advertised sizes), and a
  caption with the size and fps from `virtual_camera_status` (the next
  start's while stopped, frames sent/dropped while running). Choices are
  merge patches to the live setup's `virtual_camera`, disabled while the
  camera runs (the engine wants it stopped). State comes from Status and
  `virtual_camera` events (Session.vcam). The button is named as Studio
  Mode is ("Virtual Camera", lit while on; screen readers and the tooltip
  get OBS's "Start/Stop Virtual Camera"), because OBS's words don't fit
  beside the gear in the 172px dock. The engine can't tell "not
  installed" before a start, so the app reads it from a start's error
  ("not installed"): the popover then opens itself, explains, shows
  `scripts/vcam-install.sh` with a Copy button (NSPasteboard) and the
  gear turns into a warning glyph; Start then opens the explanation
  instead of failing again. Nothing is installed. Six buttons need 208px
  under the header, so the dock's buttons are 4px apart with 10px above
  and below (Paper: 6 and 12, five buttons), and Exit gives way when the
  dock row is under 240px (Quit Isocast in the app menu stays): for a
  Paper pass.

- 2026-10-13: **N.3: the Slint app is Isocast.app (branch ui/app-alpha).**
  `scripts/app-dev.sh` builds `crates/isocast-ui` into
  `~/Applications/Isocast.app` with today's app's bundle id
  (`dev.isocast.app`, crates/isocast-ui/Info.plist) and icon (Isocast.icon,
  still in crates/isocast-app/bundle/ because the release branch's script
  reads it there); the target is one line (`app_target`, or
  `ISOCAST_APP_TARGET=isocast-app` for the web app), as in agent/release's
  release.sh, whose `app_target` default should flip to `isocast-ui` when
  it merges (it isn't on main). The binary keeps its name `isocast-ui`
  inside the bundle. `scripts/ui-dev.sh` is a wrapper (bundle at
  `$ISOCAST_UI_APP`). No CLI command opens the app today (no `isocast
  open`), so nothing there changed. **The web app is retired but kept**:
  `crates/isocast-app` is outside the workspace's `default-members`, so
  `cargo build/clippy/test` and CI skip it and nothing ships it
  (THIRD_PARTY_NOTICES no longer lists its scripts or crates; the fonts
  are now the Slint app's real ones), but `cargo build -p isocast-app`
  still works. Delete the crate after Ryan's alpha week with the Slint app.
  The app's system parts (open/save panels, menus) are dark now
  (NSApp.appearance DarkAqua; the notes' N.3 item).
- 2026-10-14 (A.2 QA-2 #7): only newly added/imported collections/profiles
  get numbered display names on collision, under the library write lock;
  existing names/ids are untouched. `sources.<id>.audio.mono` averages L/R
  before balance in the program and monitoring, retaining raw ISO audio.
  CLI `audio ID --mono|--stereo --json`, MCP `set_audio {source,mono:true}`,
  and ordinary apply patches share the setting. The Advanced Audio checkbox
  is for the app lane to expose; this finding changes no UI source.

- 2026-10-14 (A.2 QA-2 #5): save the departing collection's program and
  preview ids in `<collection>.view.json`, outside the v1 scene document.
  Restore on collection switches, not profile-only switches; absent/deleted
  scene ids fall back to the first scene (invalid preview to program,
  preserving Studio Mode). Sidecar follows deletion into the archive.
  `clear_preview`, CLI `preview --clear`, MCP `clear_preview` let the native
  Studio-off button clear engine state. App hooks adopt preview presence
  from events/status instead of leaving a stale local Studio toggle.

- 2026-10-14 (A.2 QA-2 #4): omitted `recording.iso.audio` means each
  ISO's own audio, if the opened source actually has sound. Explicit lists,
  including `[]`, keep legacy shared routing exactly; omission remains
  omitted across TOML/collection/restart round-trips. Per-source
  `audio.iso = false/true` overrides own-audio inclusion, never duplicates
  the same track. Cameras with separate microphones still use the explicit
  shared microphone list (no guessed device pairing). Minimal mixer hooks
  display effective routing and patch the per-source choice in auto mode.

- 2026-10-14 (A.2 QA-2 #1): `encoder_options` returns
  `[{codec,label,containers}]`; CLI `isocast encoders --json`, MCP
  `list_encoder_options`, and setup schema `x-encoder-options` share it.
  H.264/HEVC allow mov/mp4; all five ProRes codecs require mov. Both program
  and ISO pairs validate even while disabled. Native writer repeats the
  compatibility guard before file creation; generic AVFoundation rejection
  no longer falsely asserts the Mac cannot encode that codec.

- 2026-10-14 (A.2 QA-1 #4): source validation paths now use stable ids
  (`sources.colour.height`), with the display name in the message. Paired
  fields use the existing atomic merge patch, not a new API:
  `apply {revision:N,patch:{sources:[{id:"colour",width:400,height:300}]}}`.
  The app must buffer paired inputs and send both in one call; a lone width
  still correctly names the missing height. Existing tests updated for the
  public path change; no test files added.

- 2026-10-14 (A.2 QA-1 #3): each source publishes its opening/result
  independently; native opening has an 8 s deadline and one outstanding
  native call per source id (32 total). Timed-out native calls retain their
  slots until the OS returns, preventing retry thread accumulation.
  `waiting_for_permission` plus `error` reaches status/events/CLI/source row.
  Device hints use non-prompting permission status. macOS has no public file
  TCC preflight: Documents/Desktop/Downloads/Volumes hints are provisional
  while access is attempted and clear on success. Source rows show the
  warning/tooltip; full state is available to the app via SourceStatus.

- 2026-10-14 (A.2 QA-1 #1): new merge-patch source/scene items default to
  `fit = "none"`; their box follows cropped native size and
  `style.transform.scale` controls scaling. Rect-based legacy files retain
  their sizes. Omitted legacy fit is distinguished internally from explicit
  stretch (which now survives serialization); on source edits only unchanged
  legacy boxes equal to the previous native size migrate. CSS layout keeps
  its own sizing rules. App hooks only: initial placement uses native rect
  plus scale, Reset chooses none, explicit Stretch is saved. The app's
  canvas-resize owner must patch scale for no-bounds items, not rect size.

- 2026-10-13 (delegator, overnight): **G.3b (crash-safe takes) waits until
  after the alpha week.** The probes show it needs our own video/audio
  encoders feeding a segment writer, which replaces the core recording path;
  landing that the night before Ryan's alpha testing risks more than the
  rare crash it protects against. Until then a crash loses what's after the
  last 2 s fragment (a take under 2 s entirely), and pauses report the
  recoverable length as unknown. First engine project after the alpha week.

- 2026-10-13: **G.3b is not a segment-API toggle.** Recording uses raw
  CVPixelBuffers via AVAssetWriterInputPixelBufferAdaptor and PCM via AAC
  AVAssetWriterInputs (writer.rs); only streaming/monitors use the explicit
  VTCompressionSession in stream.rs. Fifteen native probes on M5/macOS 27.2
  confirm: manual MP4 segmentation rejects non-nil output settings; delegate
  MOV output fails with -11875; ProRes rejects MP4; the HLS profile refuses
  two audio inputs. Automatic MP4 segmentation *does* encode raw H.264/HEVC,
  but flushSegment then raises and the pause tail remains unwritten. Earlier
  shorthand saying all delegate output is passthrough-only was too broad.
  Reusing existing split/join is the smallest measured equivalent and fails
  the unchanged-cost bar (Benchmarks). Retain the honest unknown recovery
  contract; no new committed_seconds field or cut-derived number is shipped.
  A complete replacement needs persistent recording encoders, a muxer that
  supports MOV/ProRes/multiple AAC tracks, recoverable track-time accounting,
  fsync/error propagation, and an all-writer Pause commit barrier. Encoder
  draining alone is insufficient; AAC priming/tail and sample durations must
  be resolved explicitly. No replacement, dependency or runtime edit retained.

- 2026-10-14: **A.6 fix round 3 supersedes round 2's credential tradeoff.**
  Env/Keychain/explicit stream keys (including rotated values) are pinned
  for the process lifetime in both producer and native relay. Only inferred
  API fields use the 512-entry LRU. The relay polls stderr and its private seed
  socket, whose receive budget is 256 KiB; a sender retry worker delivers a
  backlog off the caller's thread, with ordered pending/ready pipe markers.
  Only lines emitted during that backlog are withheld. Seeds are fragmented
  at 1024 bytes: Darwin's independent default maxdgram is 2048, so a larger
  receive buffer alone does not allow 4 KiB datagrams. A stalled-relay check
  (600 credentials and a 9 KiB key) confirms temporary withholding, recovery,
  redacted later text, and a final no-newline tail after producer exit.
  Startup recovery now shares main's pause annotation (recoverability unknown,
  open pause retained, null recoverable_seconds_at_pause); incident triage
  includes paused. Failed `frames` returns nonzero with its partial-file
  summary; Start/Resume name a failed compositor and Resume leaves the clock
  paused. Only these small failure/reporting hooks touch record/*.

- 2026-10-14: **A.6 fix round 2 (supersedes the fail-closed registry).**
  Credential memory is a 512-entry LRU (by supply); >4 KiB values are scrubbed
  in place, never cached; text is never wholesale withheld. Reports build a
  fresh redactor from the live cache snapshot plus Keychain/env seeds, and its
  learning doesn't touch the live cache. A compositor panic stops the program
  slot: the manifest gets `program: …; program output failed` (status
  failed), `status` shows the engine "degraded" with the worker error, and a
  new recording is refused until the live engine restarts (no in-place
  restart: the compositor owns GPU state the panic may have left half-built).
  Failed files' manifest durations come from the finalized media
  (`media_info`), else their newest handled frame. Detached engine threads use
  `isocast_diagnostics::spawn(name, run, cleanup)`: the cleanup repairs the
  owner (frames registry entry removed and viewers told, shared-frame ring
  closed, connect/stream joins fail), then the daemon publishes a warning.
  Errors bypass the per-peer log limit; the omission count flushes on
  disconnect. Console processes (CLI/apps) with unwritable logs print one
  warning and no JSONL. Engine edits are limited to these hooks.

- 2026-10-13: **A.6 review privacy/reporting.** Exact credential names and
  suffixes replace substring `key` matching; arbitrary short enum values
  aren't learned. A bounded registry always accepts recent credentials and
  fails closed for text after overflow, exposed in status. Reports seed
  configured profile keys from the non-prompting Keychain API; client logs
  omit params/prose. HMAC-SHA256 uses a fresh per-report random key (never
  exported) for strict paths/titles/device identities. Serial fields and
  unrelated mounts are removed; signing describes current_exe and omits
  certificate-owner/team identity. hmac/subtle are new for the requested
  construction; getrandom was already locked and supplies OS entropy.
- 2026-10-13: **A.6 native stderr survives abrupt exit.** A small mode of
  the same daemon executable drains stderr in an independent relay after
  producer death. Credentials arrive on a private inherited datagram FD;
  a failed seed channel closes text disclosure. Native text is redacted
  before JSONL, bounded per line/segment, and bootstrap stderr appends.
  Plugin output is redacted into 1 MiB current/previous files and included
  in bundles. Camera callbacks use a bounded nonblocking log queue. Benchmark
  CPU, retired instructions and RSS now include daemon descendants, so relay
  work is not hidden outside the measurement.

- 2026-10-13: **A.6 review #2–4.** Logger init now falls back to stderr
  without gating startup; immutable-log-folder test and full log-only HFS+
  volume both start/record. A totally full filesystem containing the IPC
  socket still cannot allocate a socket inode (`bind`: ENOSPC); the corrected
  log-only reproduction keeps IPC/recordings on the writable volume.
  Levels are not logged, polling is compact per-peer/per-method summaries,
  mutations/errors have per-peer log limits; pruning moved off callbacks.
  Budgets: daemon 40 MiB, CLI and each app 8 MiB, protected fault tails.
  Reviewer scripts m2-a: poll 145.2→15.7 MiB/h, engine 34.1→10.9 MiB/h,
  recording 38.9→15.6 MiB/h. A 30 s get flood lost the old crash file before;
  afterwards >2 million calls retained its crash file and marker. CPU tick
  bug reproduced ps/log ratio 41.685; corrected ratio **1.0031** (8.38 vs
  8.3542 CPU s over 30 s). mach2 is now a direct dependency for its current
  timebase binding; it was already locked through isocast-surfaces.

- 2026-10-13: **A.6 review #1 supersedes the abort policy below.** Panic
  hooks only capture/sync (try-lock, fail-closed redaction), then unwind.
  Contained connection/worker panics leave independent recordings/streams
  alive; poisoned state invokes emergency finalization, interrupted markers
  and exit 70, with a ten-second stuck-shutdown watchdog. Startup repairs
  orphaned manifests and reports them in `status.interrupted`; unknown frame
  counts remain null. Reviewer crash.sh on m2-a: b72536b aborted at ~27 s,
  retaining 26 s/780 frames; fixed worker and connection cases finalized
  39.4667 s/1184 frames each, zero drops, stream continued. Poisoned control
  state finalized 28.1667 s/845 frames each (zero drops), both manifests
  interrupted. SIGKILL retained 14.016 s/420 frames; next launch repaired
  markers and obtained native duration 14.0333 s (OS metadata, not inferred
  frame count). Scripts/evidence: a6-diagnostics/scratch/review-fixes.

- 2026-10-13: **A.6 diagnostics (agent/a6-diagnostics).** Shared
  `isocast-diagnostics` uses synchronous redacted JSONL at API/event rates,
  not an asynchronous tracing queue: every complete line reaches the OS
  immediately, errors flush, panic/clean markers sync. Segments are 4 MiB,
  scope budget 64 MiB/seven days/twenty completed launches per process;
  current active segments/markers are protected. `ISOCAST_SOCKET` scopes
  all clients' logs; default is `~/Library/Logs/Isocast`. The only new
  archive dependency is `zip` (deflate via the existing flate2/zlib-rs);
  regex, SHA-256 and chrono were already in the lockfile. Reports are
  offline and private, with a complete sanitized preview, not an upload.
  `docs/bug-reports.md` describes the method table and bundle layout.
- 2026-10-13: **A.6 panic policy and availability.** A Rust panic is logged
  with a forced backtrace, then aborts the process rather than leaving a
  dead worker/poisoned locks; this deliberately bypasses earlier local
  `catch_unwind` recovery. Completed movie fragments survive. The isolated
  `ISOCAST_DEBUG_PANIC_AFTER=N` fault worker exercises this. Reports match
  a panic signature to the specific abnormal launch, never attribute an
  older panic to a later SIGKILL. VT CPU is explicitly system-wide with
  PID/start identity; GPU uses AGX's per-PID accumulatedGPUTime. H.264
  queues/callback latency are measured; AVAssetWriter's opaque internal
  queues remain null with a reason. No sudo/subprocess runs in the sampler.
- 2026-10-13: **G.3 R1 replaces the cut-derived durability claim.** Neither
  FileWriter acceptance (which can mean only queued) nor AVAssetWriter input
  acceptance acknowledges an on-disk fragment. No numeric lower bound can
  be substantiated for all files and A/V tracks. Each pause now explicitly
  records `recoverable_seconds_at_pause: null` (unknown, not zero), retained
  after Resume and in failed manifests. Legacy `crash_safe_seconds` is
  ignored on input and omitted on rewrite; interruption errors and start
  notes no longer invent saved/lost seconds. No new wait, encoder, routing
  or hot-path change. A future numeric bound must come from actual committed
  output/track evidence, not another cut-time formula. R2: hotkey actions
  queue serially, so rapid double toggles execute on successive boundaries;
  only concurrent direct RPCs can hit the pending-cut refusal.

- 2026-10-13: **G.3 recording-only cut clock.** All writers share one
  clock; capture, composition, mix and streaming keep host time. Cuts land
  on 1/gcd(1e9, 48000, every recorded fps), after the newest inspected
  packet plus a 5 ms fade allowance. The writer read lock and change write
  lock prevent retroactive edits even with future-stamped audio. Audio
  chunks straddling cuts are sliced on the same sample boundary, with 5 ms
  fades in place; no native encoder, fragment or crash-safety policy changes.
  Markers/switches map onto the compact take clock; paused markers share the
  frozen playhead. Split/retake are refused while paused; stop closes an
  open manifest pause. `--seconds` keeps its existing wall-clock timeout.
  Pauses are additive in manifest v1 (old files default to an empty list).
  A pause/resume reply waits for the manifest checkpoint and its cut time.
  `state: recording` continues to mean an active take; `live.paused` says
  whether its clock is frozen. The UI timer hook consumes that same state.

- 2026-10-13: **G.3 review fixes.** (F2) The G.5 ids `recording.pause`,
  `recording.resume`, `recording.pause_toggle` are wired to `Method::Pause` /
  `Method::Resume`; toggle reads the recording clock's `paused`. (F3) A pause
  is no longer clamped to 1 s after the start nor refused while the first
  take's epoch is still ahead (only a later take still starting, from a
  split/retake, is refused). (F5) The daemon state mutex is released before
  taking the cut lock (a recording-epoch check guards against a new
  recording in between), and writers drop the cut read guard before native
  appends (cut-touching audio is staged first; the common path still borrows).
  (F1, superseded by R1 above) The original cut-derived recovery value was
  wrong at exact boundaries and during encoder draining; it is now unknown.
  Rejected alternatives, measured: AVAssetWriter can't change
  `movieFragmentInterval` after writing starts ("Cannot call method when
  status is 1"), `flushSegment` only works in manual delegate segment mode
  (which is passthrough-only; automatic mode can compress, see G.3b), and
  padding past the fragment boundary would put paused time back into the
  file. A real fix needs a writer that encodes itself and writes segments.
  (F4) Documented the cut steps and concurrent direct-RPC refusal; hotkey
  actions queue instead (R2). (F6) Pre-existing 30 fps ISO duplicate/skip,
  not G.3: below.
- 2026-10-14: **The Create view (C.2).** Overrule any of these:
  - *A View menu item plus a Build/Create switch in the clip bar's
    header*, not Paper's workspace bar (Build/Create/Live workspaces are
    parked); the choice is remembered ("create" in the UI prefs).
  - *The clip bar takes the docks' place and height* (the same splitter),
    shown in Create even with the docks hidden; the 280px track-header
    column narrows to 200px under a 1200px window.
  - *Clips are named by their id* ("Take 3" for c3, which survives moves)
    after their place ("03"); the engine has no clip names yet.
  - *Thumbnails*: 8 frames per take's program file (nearest keyframe,
    at most 160x90), made by AVFoundation's image generator on one thread
    of the app's (msg_send, no new crates: two more features on
    objc2-core-graphics), cached in memory by file for the session. Not
    on disk: nothing is written next to recordings. A 51-clip timeline
    costs ~5 s of one core once, at open. None off macOS.
  - *Optimistic move and remove*: shown at once, then the
    `timeline_changed` read replaces them; a refused call reloads.
  - *Next-take placeholder* is drawn solid: Slint has no dashed edges.
  - *The take being recorded* is a red clip growing from the end, its
    length the LCD's REC seconds less those at the last `started` event
    (a split or retake's next take; the LCD keeps counting the
    recording). A window opened mid-recording counts from the LCD.
  - *No export button on the boards*: Export (and Show in Finder after
    one) sit in the header's tools in Paper's 24px capsule style; it
    exports to the engine's default folder (the first take's).
  - Not in the MVP, as the brief says: rundown, segments, ratings,
    inspector, ISO rows, trimming, splitting, playback at the playhead.
- 2026-10-13: **Visual check on pull requests (8.4).** Split in two
  workflows because one can't both run untrusted code and hold write
  tokens or secrets: visual.yml (`pull_request`, read-only token, no
  secrets, works for forks) builds the merge base and the head in one
  checkout and target folder, renders with the head's
  scripts/visual-shots.sh and uploads PNGs + manifest.json;
  visual-report.yml (`workflow_run`, runs the default branch's files only)
  never checks out PR code: it validates the artifact (known names, PNG
  magic, size caps, no links), checks the PR's head SHA against the API
  (forks have no PR in the event, so the artifact's number is verified),
  pushes the images to the orphan branch `ci-screenshots` under
  pr/<n>/<sha>/ (comment images need a URL GitHub serves; artifacts aren't
  viewable inline; a separate branch keeps main's history clean and can be
  reset any time), runs the judge, upserts one comment
  (`<!-- isocast-visual -->`), labels `visual-change` /
  `visual-regression?` and sets the status `isocast/visual-check`
  (success: nothing changed, or every changed view no_change /
  intended_change and ready_to_merge; failure: possible_regression,
  broken or not ready; error: couldn't judge). visual.yml runs on every PR
  and skips the build when no UI path changed (a `paths:` filter would
  leave other PRs without the required status).
- 2026-10-13: **Deterministic offscreen renders.** The same build drew
  0.05-5% different pixels run to run: the status bar's CPU reading (its
  width shifts the right-aligned bar), the audio meters (real-time
  decay) and the camera's live preview in its Properties. Fix:
  ISOCAST_SNAPSHOT_STILL (offscreen.rs, a small hook) drops Levels events
  and nulls `cpu_percent` in replies; visual-shots.sh shows the tone's
  Properties (no preview) and keeps its engine in a fixed folder
  (/tmp/isocast-visual-shots, the image path shows in dialogs). Result: 0
  px changed across 15 views, repeated. Diff tolerance 8/255 per channel,
  a view "changed" above 0.01% of pixels. Base builds made before this
  hook still show live CPU and meters, so PRs opened before it merges get
  small status-bar diffs.
- 2026-10-13: **The judge is a restricted OpenCode agent.** One
  `opencode run --standalone --agent judge` per changed view
  (@opencode/cli 2.0.28 pinned; model `opencode/claude-sonnet-5-5`,
  Anthropic Messages via https://opencode.ai/inference/v1, key from env
  OPENCODE_API_KEY; `opencode/gpt-6-luna` also verified). The run folder is
  a fresh copy of .github/visual-judge (from the default branch) and
  XDG_CONFIG_HOME points at an empty config, so OpenCode's upward config,
  agent, plugin and AGENTS.md discovery sees nothing from the PR; the PR's
  images, diff and file copies sit in a sibling input folder. Three layers
  keep it a judge: permissions deny `*` and allow only
  external_directory/read for that folder, glob, grep and deliver_verdict;
  the plugin deletes every other tool from each request and refuses any
  other tool in `execute.before`; formatter, LSP, snapshots, compaction and
  the built-in agents are off. The `{env:...}` substitution doesn't apply
  in agent frontmatter, so visual_judge.py writes the input path into
  judge.md. The workflow, not the model, writes the comment, labels and
  status; verdict text is escaped and capped. Steps 12, 4000 output
  tokens a request, 150 s a run, one retry, at most 15 views a run.
- 2026-10-13 (engine-e2e): `scripts/e2e.py` is the approved stdlib-only
  real-file harness. Each scenario owns an ad-hoc signed background dev app,
  socket, setup/library, plugins, timeline and recordings; it holds the same
  `.bench.lock` as bench.py and bounds a whole worker process group. Default
  cleanup removes scratch files; `--keep` keeps all, `--keep-failures` keeps
  failed cases for CI. `--report PATH` always writes JSON; `--json` also
  prints it. Hardware-only FFmpeg probes select HEVC/H.264 or the engine's
  ProRes path; streaming explicitly skips without H.264 hardware. `--codec
  prores422` exercises the fallback directly. No capture sources are used.
  The PR benchmark runs three interleaved base/head pairs, flags changes
  beyond full run-to-run spread, and is informational. Its workflow_run
  publisher executes only default-branch code, validates bounded numeric
  JSON and current PR/head identity, and updates one bot comment. No optional
  commit-status gate is enabled on noisy VM measurements.

- 2026-10-13: **Contribution policy (Ryan).** Agent-written contributions
  are encouraged (latest state-of-the-art models at their highest reasoning
  setting, AGENTS.md as the rulebook, the evidence is the agent's job, no
  AI attribution). Small fixes and docs PR directly; bigger changes need an
  issue with the `accepted` label first (.github/SAVED_REPLIES.md has the
  kind closing reply). Enforced by .github/workflows/pr-checks.yml: on
  `pull_request` with a read-only token, it runs the base branch's
  .github/scripts/pr_checks.py and fails a PR that changes
  crates/isocast-ui/** or crates/isocast-app/ui/** with no image in its
  description, or whose "How it was verified" section is empty
  (pr_checks_test.py, 15 cases, runs in CI). No labelling step: with a
  read-only token it can't, and pull_request_target isn't worth the risk.
  scripts/ui-screenshots.sh draws main, Studio Mode, Properties, Settings
  and the mixer menu offscreen at 1440x900@2x against a throwaway engine
  (~9 s after the build). CI runs on push to main and pull requests,
  GitHub-hosted runners only, rust-cache saved from main only.
- 2026-10-13: The benchmark card's OBS numbers on the M2 4K run (Ryan):
  3,904 is OBS's own skipped-frames count; 1,645 of its 6,292 frames were
  distinct pictures. The README caption and docs/benchmarks.md say both.
  Repository URL is https://github.com/R44VC0RP/isocast (Cargo.toml fixed);
  private vulnerability reporting is on, and SECURITY.md and the code of
  conduct link to it.
- 2026-10-13 (docs-release): I answered isocast-m2-b's "Isocast Dev is
  requesting to bypass the system private window picker" prompt (Allow)
  for ~/isocast-scratch/docs-release-shots/Isocast Dev.app. Ryan: fine,
  it's our test Mac.
- 2026-10-13: **README is for people downloading the app** (Ryan). Pitch,
  screenshots, benchmark card, download, first steps, FAQ, doc links;
  building, architecture and contributing live in .github/CONTRIBUTING.md
  and docs/architecture.md, linked once at the bottom. docs/benchmarks.md
  carries the measured table and conditions (S.3 round 2: app-only CPU and
  footprint, both Macs); the card in docs/images/benchmark.png is Ryan's.
  For its OBS M2 numbers see the benchmark-card entry above.
- 2026-10-13: **Reference docs are generated** from the release binary by
  scripts/reference-docs.py (CLI help, `setup schema`, MCP tools/list
  against a throwaway socket), and THIRD_PARTY_NOTICES by
  scripts/third-party-notices.sh (cargo metadata, normal and build deps
  for aarch64-apple-darwin); both have --check, CI runs the notices one.
  Rerun them when the CLI, schema, tools or dependencies change.
- 2026-10-13: **The README screenshots' Screen scene is a window capture of
  Terminal**, not a display capture: on a test Mac the display capture
  shows the app inside itself and whatever dialogs other jobs left on the
  desktop (a Gatekeeper "IsocastReleaseCheck" prompt and a python3.14
  screen-recording prompt sat on isocast-m2-b's screen). The window keeps
  the shot clean and still records a TERMINAL ISO file.
- 2026-10-13 (G.5 follow-up 2, after ff5ea77): regular keys require Ctrl or
  Cmd, never Shift/Option alone or together. Common Command editing/tab/
  navigation/screenshot and Control terminal/editor chords are refused, as
  are known qualified system chords; unused Ctrl/Cmd combinations and
  F13–F20/keypad/media remain available. Reviewer's t_type: before A/Option-L/!
  were swallowed (3 fires); after rejection they reach TextEdit, no fires.
- 2026-10-13 (G.5 follow-up 2 storage): read_profile is now pure. Legacy
  inline metadata takes precedence until an explicit profile write migrates
  it, sidecar first; listing/get/switch/start leave the TOML and .bak bytes
  and mtimes alone. This supersedes the review-hardening migration-on-read
  choice. Creating any new profile archives orphan .hotkeys.json/.bak files
  under deleted/profiles/orphan … before writing, so old-daemon deletion and
  id reuse cannot restore old keys. Blank profiles get none; copies/imports
  get their own supplied bindings. Reviewer t_store2 passes: 10 no-hotkey
  library files remain byte-identical to d6c8aec's operation sequence.
- 2026-10-13 (G.5 timeline hotkeys): 39 catalogue ids now include real
  timeline new/open/close, retake, remove/move/export and marker (an alias of
  the recording marker, not a standalone timeline marker). Reuse the one
  dispatcher, no timeline/recording implementation changes. Move index is
  zero-based as on the API; old reserved `id` is an alias for clip on
  remove/move and timeline on export. Pause ids remain reserved; timeline.clear
  stays unavailable because there is no such method. Test Mac global input
  exercised idle/live retake and kept original files, reorder/removal,
  two completed exports (57 frames/1.9 s and 30/1 s, equal A/V, full decode),
  and 188 focus samples all Terminal. Release fmt/clippy and 179 tests pass.

- 2026-10-13: **Themes design revised after two critiques
  (docs/themes.md section 14).** A built-in theme is a *preview* until
  every signature piece in Ryan's pitch draws (a per-theme checklist,
  4.2), then *ready* (picker, Ready for Ryan); each ready step waits for
  its data slices (Workbench T.7b/c/d/f, Daylight T.7a/c/d/e, Blender
  T.7a/b/d and T.7f's action table, so G.5). Built-ins set every token
  and slot (no inheriting from Console); `base` is for plugins.
  Dispatchers are generated from SLOTS with enum `Look` values, branch
  only on `Look` (a data condition rebuilds the piece mid-drag), inherit
  the behaviour base (focus works; TextInput pieces take a
  `focus-pending` counter in `init`, since `changed` doesn't fire for a
  piece built after the request). Theme cargo features default on so
  clippy and tests cover all themes. Accessibility parity is over
  actions (every DockAction and action-table entry reachable by
  keyboard), not elements; Tab follows each piece's order. New: launch
  marker and `--safe`, relaunch without activation and with the
  daemon's choice and loop guard in the environment, queued edits
  drained before exec, `theme_report`/`theme_applied`, `theme_set`
  restarts only on a change (`--reload` for authors), `sidebar-dialog`,
  `template-field` and `lcd` slots, canvas handle tokens, file/size/
  number/image limits checked again at launch. Spike CPU numbers are
  indicative only (baseline was the old look with the system font);
  T.3a re-baselines on Console with an A/A pass.

- 2026-10-13: **Swappable themes (docs/themes.md, PLAN T).** A theme is
  tokens, fonts, one piece per slot and layout tokens, chosen when the
  app launches; switching relaunches the app (exec of itself, ~0.5 s on a
  base M2). Recording and streaming run in isocastd and carry on: the
  spike (c72831d, `spike/themes`, not merged) did 24 relaunches during a
  take and a stream with 0 dropped frames, 0 audio dropouts, 0
  reconnects. Built-in pieces are compiled into the app; a `Look` global
  set before the window shows picks one per slot with `if` (no measurable
  CPU: 17.8% vs main's 18.1% of a core with meters; +2.1 MB for two
  pieces and fonts). `Look` defaults to "" so `AppWindow::new()` builds no
  piece before Rust chooses. Containers (Dock, Dialog, form rows,
  sections) stay shared and read layout tokens, because Slint 1.18 can't
  put `@children` in an `if`. Console (N.5) is the default and every
  slot's fallback; the old copper look retires with N.5. The choice is an
  engine-wide app setting in `app.toml` beside the socket (`theme_list`,
  `theme_get`, `theme_set`, `theme_changed`; `isocast theme`; MCP
  `list_themes`/`get_theme`/`set_theme`), not a profile field. The app
  resolves it itself at launch (same code as isocastd), so its first
  frame is right. Theme plugins (`[[themes]]`) are data only: tokens.css
  (Paper's get_tokens, verbatim), fonts, previews, `base`, and a compiled
  piece per slot; they ask for no permissions. Plugin code pieces
  (slint-interpreter in a ComponentContainer) wait: experimental and
  hidden Slint APIs, +8 MB, +2.2 points of CPU for three meters and a
  field, no sandbox. Rejected: the whole UI interpreted (+27% CPU, +245
  MB, +450 ms per launch), one UI build per theme (~+19 MB each).

- 2026-10-13: **M4 Max HEVC look-ahead (S.3g).** Set the public VT key
  `LookAheadFrames` to zero through AVAssetWriter's compression dictionary
  on the qualified M4 Max only, macOS 15+. A bulk-property interposer
  verified forwarding into all three actual VT sessions. Use the string
  value plus availability check instead of linking the newer symbol, so
  macOS 14 still loads. Leave other chips and H.264 on their defaults:
  applying this hint globally regressed M2, including its otherwise healthy
  1080p-program workload. A hardware/OS-specific result, not a universal
  recommendation. The no-B-frame durability policy and requested quality,
  frame rate, bit depth, resolution and keyframe interval remain intact.

- 2026-10-13: **Recording overload policy (S.2c/S.3f).** A protected
  program's raw queue and a streaming encoder's full submission window
  publish pressure to the process's native writers. ISO admission pauses
  immediately, then resumes at a reduced fraction with slow recovery;
  its pending raw queue is four rather than 32 while protected outputs
  exist. Capture/composition never wait on this policy. Every refusal
  counts as a drop. Stop may encode one explicitly reported repeat at the
  final slot; it never discards accepted pictures or trims audio to win
  latency. A handled-but-refused final slot no longer imposes the capture
  tail timeout. H.264/HEVC recordings use no B-frames: the former reordered
  path produced backward presentation time across unfinished fragments
  under overload. This changes compression efficiency, not requested
  quality, bit depth, colour tags, fragment interval or keyframe interval.
- 2026-10-13: **Timelines (C.1, branch engine/timeline-mvp).** Overrule
  any of these if you'd rather:
  - *Where they live:* in the library, beside the socket and setup
    (`timelines_path()`: `$ISOCAST_TIMELINES`, else `<socket>.timelines`
    for a daemon on its own socket, else `~/Library/Application
    Support/Isocast/timelines/`), one `<id>.timeline.json` each plus
    `current.json`. Not next to the recordings: a timeline can hold takes
    from several folders (`start --folder`), "current" is app state, and
    listing needs no folder scan. The export's `<name>.export.json` (next
    to the joined files) lists the clips and their takes for editors.
    Every change is written to a temporary file and renamed (as session
    manifests are), not fsynced, so a kill -9 never leaves half a change.
  - *Clips point at takes* (their session manifest; files are read from
    it on `timeline_get`), whole for now: `in_seconds` 0 and `out_seconds`
    the take's length on the program's frame grid
    (`timeline::take_length`: the program file's frames at the canvas rate,
    else the first file's), not the take's wall-clock duration, which a
    stop leaves between two frames. So a timeline's length is exactly each
    exported file's.
  - *A take joins the timeline current when it finishes* (`on_take` in
    `RecordOptions`, called in take order as each take's final manifest is
    written: a split's take once its files finish, about 0.5–1 s after the
    split; the last before `stop` returns), as Ryan put it, even if
    another timeline was current when it began. Failed takes (files with
    errors) are still added: their files exist. While a take records, the
    current timeline lists its manifest under `recording` (it moves with
    `timeline open/new/close`); one still there when isocastd starts again
    (kill -9, crash) or when its recording fails moves to `removed`, reason
    `interrupted`: it never finished, so it is never a clip, but the
    timeline says where it is. (Fix round, 2026-10-13.)
  - *`retake`* (reworked in the fix round, 2026-10-13): one at a time (a
    lock; a second waits for the first). Not recording, it takes the last
    clip off (kept under `removed`, reason `retaken`) and starts recording
    in the same folder as that take (or `folder`); if the recording can't
    start, that clip goes back. While a stop is finishing (or the
    recording is ending on its own), it waits for the end and does the
    same, taking off the take just stopped (nothing, if that take was
    itself dropped). Recording, it drops the take being recorded (listed
    under `removed`, reason `dropped`, on the timeline the retake acted
    on, when it finishes) and begins the next with a split timed
    `SplitTiming::Soonest`: the next 1/10 s (for 60 and 30 fps files; see
    Gotchas) at least 0.25 s away, so 0.25–0.35 s after the call instead
    of `split`'s 0.5–1.5 s; the reply's `begins_at`/`begins_in_seconds`
    say when (the `started` event's `begins_at` too), and `isocast retake`
    returns once the new take has begun. A retake before the new take has
    begun drops nothing more (a double press costs nothing). A take still
    lasts at least 1 s, so a retake within a second of the previous new
    take's start waits for that. The take marked dropped is the one the
    engine's split ended (its number comes back with the split), marked
    after the split succeeds: it ends at the split time, at least 0.25 s
    later, and only finishes after that, so the mark is always in time,
    and a refused split drops nothing. Refused without a current timeline,
    and while the recording is still starting.
  - *Export = AVAssetReader/AVAssetWriter, video passthrough, audio
    re-encoded once* (crates/isocast-platform-macos/src/join.rs). Not an
    AVMutableComposition passthrough export: each take's AAC starts with
    encoder priming, so passthrough needs an edit-list entry per join;
    QuickTime plays that right, but FFmpeg (and editors built on it)
    placed the second take's audio ~20 ms early, more than a frame
    (measured on two real takes). Decoding and encoding the audio again
    (AAC 256 kb/s stereo, as recorded) gives one continuous track with one
    priming, cut or padded with silence to each clip, identical in FFmpeg
    and AVFoundation. The video packets are copied bit for bit.
  - *Every file of a clip fills the clip's length* (`JoinPart::length`):
    files at the program's rate are exact and stay constant-rate; a file
    at another rate (a 30 fps camera ISO beside a 60 fps program) gets its
    last frame of a clip shortened or held so every joined file switches
    clips at the same moment (the file is then slightly variable-rate
    around joins). Without it a 30 fps ISO drifted up to 1/30 s per stopped
    take against the program. Frames starting past a clip's end would be
    left out and reported (`frames_left_out`); normally none.
  - *Sources missing from some clips are skipped, saying which clips lack
    them* (also a size, frame-rate or audio-track change between clips:
    joining those needs re-encoding). Except the program: if the takes
    recorded one and it can't be joined (a file missing, a clip without
    its audio), nothing is exported and the error names the clip; the one
    joined file is the point. A clip whose take manifest never got its
    final version (the folder turned read-only or the disk filled at stop)
    refuses the export naming that clip, rather than a trimming message.
    Names: `<timeline name> PROGRAM.mov`, `<name> <SOURCE>.mov`,
    `<name>.export.json`, numbered (" 2") rather than overwriting, the name
    cut so every file name fits 255 bytes; joins write `….partial.mov` and
    rename it with `renamex_np(RENAME_EXCL)`, so a file made meanwhile is
    never replaced. Export ids are `<timeline>-<YYYYmmdd-HHMMSS>`, made
    unique (`-2`) within an isocastd.
  - *One export at a time*, enforced: the slot is taken before planning
    (the first fix round found three concurrent exports running and two
    sharing an id, so `isocast timeline export` reported the other's
    files). It may run while recording (it only reads finished takes), but
    never on the space the recording keeps free: to the same disk it needs
    its files plus the recording's floor (`recording.keep_free_gb`, or
    10 s of its write rate, at least 64 MB) plus 512 MiB, and it checks
    every second and stops (partial file removed, manifest `failed` saying
    why) if free space falls within 256 MiB of that floor. `isocast
    restart` is refused while an export runs (`status` shows it); a
    `shutdown` stops it cleanly (manifest `failed`, "isocastd was stopped
    …"). While it runs, `export.json` in the timelines folder names its
    manifest and partial files, so after a kill -9 the next isocastd marks
    that manifest `failed` and removes the partial files (files it had
    finished are complete and stay).
  - *CLI positions count from 1* (`isocast timeline move c3 1` = first);
    the API's and MCP's `index` from 0.
  - *Clips show dropped frames* (`dropped_frames` per clip and per file,
    `audio_dropouts` per file; `timeline show` prints them), so a bad take
    stands out. A current timeline whose file couldn't be read at start is
    said in `status` (`timeline_problem`) and in `start`'s notes;
    `timeline open ID` reads a fixed file again.
  - *Left:* the app's view (C.2); splitting, trimming and silence removal
    (clips have in/out, the export refuses anything but whole takes);
    restoring a removed take or deleting its files; joining clips whose
    sizes, rates or audio tracks differ (skipped, said why); an export to
    another container than the takes'.
- 2026-10-13 (G.5 review hardening): hotkey metadata is now lossless raw data
  on API Setup/Profile, interpreted into usable bindings plus warnings. Bad
  fields/actions/shapes/chords cannot invalidate recording settings; strict
  hotkeys_set validates the resulting map. Stored bindings moved to
  `profiles/<id>.hotkeys.json`: immutable old binaries reject a hotkeys field
  in v1 TOML, so merely making the new reader tolerant would not fix downgrade.
  Inline a4e3945 profiles now migrate on explicit writes (follow-up 2 above), writing the sidecar before removing
  the field and retaining the original TOML .bak. Older daemons ignore and
  preserve the sidecar; unknown future entries also survive an old-daemon
  save and new-daemon restart. Corrupt JSON sidecars remain untouched on
  unrelated saves. Exports/imports still carry inline metadata through the API.
  Omitted hotkeys on whole-setup apply preserve the profile's configuration
  with a note; explicit {} or merge-patch null clears it durably.
  A sidecar-path obstruction is also checked: a failed migration cannot strip
  inline data during profile rename; original bytes remain and recording
  still works, while the failed rename reports the filesystem error.
- 2026-10-13 (G.5 review input): require modifiers except F13–F20/keypad/media;
  refuse common Command and Shift–Command clipboard/window/application keys.
  Defaults merge by id/chord unless --replace. CLI input can hold a focused
  connection with --hold-ms; missing unset ids fail. Diagnostics cap at 8 KiB
  and conflict groups are linear/bounded (300-binding repro: 68 bytes).
  Native timeout cancels queued registration, and the registered-state lock
  is not held while waiting on main; pending focused calls fail promptly.
  Startup finishes registration before serving clients (found on the hold
  rerun). Failed hold updates roll back; removed sources lose overrides;
  Studio-off emits preview_cleared. Distinct press/release pairs deliberately
  remain distinct recording toggles; repeat presses without release are
  suppressed. No arbitrary time debounce was added.

- 2026-10-13 (G.5 review, quit): AppKit's default quit handler bypassed
  SIGTERM cleanup. Reproduced with the reviewer's t_quit.py on exonbook:
  quit left 60 frames/2 s and an in-progress manifest, versus SIGTERM's
  complete 114-frame take. The daemon now installs an NSApplicationDelegate
  returning TerminateLater, runs the shared graceful shutdown on a worker,
  and replies only after files/engine/persistence finish. Signals and RPC
  use the same routine; shutdown rejects new recording starts and waits
  beyond a stop timeout rather than exiting mid-take. Same repro now yields
  117 frames/3.9 s, complete manifest, no disk probe, fully decoded; SIGTERM
  remains complete. No recording implementation changed; 165 tests and
  release all-target clippy/fmt passed.

- 2026-10-13 (G.5, storage revised above): Optional Setup/Profile hotkey
  metadata uses the P.1 library's lifecycle, now with a compatible sidecar,
  strict update validation and optimistic revisions. Existing d6c8aec
  profile bytes were identical after loading/saving with this branch.
  Defaults are opt-in to avoid silently grabbing keys on existing rigs.
  Thirty-five stable action ids include explicitly unavailable G.3/timeline
  reserves; scene/source/library references resolve at firing time because
  one profile can accompany different collections. Duplicate chords reject
  the whole edit; OS registration conflicts persist with focused-fallback
  warnings. Push keys overlay the live setup without saving or incrementing
  its revision; release restores the latest configured mute, push-to-mute
  wins overlapping holds. Rebinding/library switches clear holds; focused
  holds are owned by their socket and released on disconnect. Native input
  only enqueues into a bounded worker; overflow cancels holds. No work is
  added to media ticks and no new dependency was needed.
- 2026-10-13 (G.5/macOS): Carbon exclusive RegisterEventHotKey handles both
  press and release without Accessibility/Input Monitoring. NSRunLoop::run
  alone doesn't dispatch its application events, so run_main_queue now uses
  NSApplication::run with activation still Prohibited (no window/Dock icon).
  Media keys use a passive session event tap only when Input Monitoring is
  already granted; no prompt, no suppression of macOS media behavior.
  Test Mac isocast-m2-b (26.7/25G229): all global recording, streaming,
  scene/mute/push/profile/collection tests retained frontmost System Settings;
  final run sampled its pid 680 times at ~10 ms intervals. Initial idle
  numbers omitted Mach timebase conversion and must not be used. Corrected
  review rerun on exonbook (three 20 s windows each): d6c8aec 0.00302%, new
  empty 0.00306%, new 35-key 0.00277% of one core; all 1 interrupt wakeup/s,
  0 idle wakeups. No measurable regression or CPU-improvement claim.
- 2026-10-13: **G.1 adversarial review.** CoreMediaIO has no cancellable
  synchronous-call API, so isolate every sink call in a private authenticated
  re-exec of isocastd. Open/frame/close exchanges have 2 s/250 ms/500 ms
  deadlines; expiry kills only the child. A watchdog handles parent death;
  an unreaped child prevents spawning more. Keep a main run loop in the
  child, with native work on a separate thread. Camera worker start/stop
  waits are bounded at 3 s; no daemon state lock crosses setup, stop or a
  join, and no extra worker is started while an old one is unfinished.
  Control socket: atomic 0600 creation plus Darwin peer-euid check. Helper
  channel: anonymous socketpair, audit token and matching designated code
  requirement. Extension: CMIO signingID plus live Apple-anchored team/id
  requirement, and a single reservation keyed by clientID. CMIO exposes
  pid/signingID, not a client audit token in this SDK.
- 2026-10-13: Camera negotiation now keeps consumer preferences separately
  from sink input. Valid consumer requests (including 30/29.97 fps) are
  accepted in either order; a connected producer wins and effective values
  are notified. Standby resumes the consumer choice. A pure production
  negotiation/clock model separates this policy from CMIO delivery (which
  cannot notify from an unpublished standalone device). Card→live retiming
  preserves monotonic PTS and original program PTS without copying pixels.
  Empty reads wait one input frame instead of 2 ms. Diagnostics are behind
  `isocast-daemon/vcam-diagnostics`; ordinary release builds refuse their
  environment overrides. Native child faults have a diagnostic-only label.

- 2026-10-13: **G.1 Follow-up 1 installer.** `scripts/vcam-install.sh`
  wraps Cargo's actual daemon in the XcodeGen app target (a declared
  script output, no stub), embeds its system-extension dependency, and
  uses Xcode Automatic/Apple Development signing for team 9G68SMNHEU.
  Keep the original dev.isocast.isocastd[.camera] ids so engine identity,
  activation and handoff stay aligned, distinct from Studio NDI. Inspect
  resolved target ids/groups before confirming `-allowProvisioningUpdates`;
  verify exact-id embedded profiles before copying. `sign-dev.sh` real-key
  camera embedding is replaced by this provisioned path; its ad-hoc option
  remains for scratch only. Native maintenance switches add deactivation,
  readonly AVFoundation discovery and a moving-frame probe. Reboot-pending
  requests return 2 so uninstall keeps the host. Only bootstrap/platform
  camera code changed; recording and frame-delivery paths did not.

- 2026-10-13: **G.1a** uses a CMIO sink stream, not a shared-memory file
  opened by the extension (CMIO extensions run as a different user).
  Program/preview/source frames are converted/scaled into a bounded BGRA
  IOSurface pool in one GPU pass, then passed by reference through CMIO;
  the extension forwards the sample as-is. A selected named scene uses
  the existing compiled compositor on its own worker. Camera-only sources
  are runtime demands, removed on all stop/failure paths, not persistent
  capture demands while off. The source-opening recheck must include this
  demand too. Minimal hooks in record/engine.rs and record/mod.rs; no
  recording queue, writer, encoder or A/V scheduling changes. One native
  app event arm refreshes status; native UI controls are not part of this
  brief. No new Cargo dependencies. Settings stay with the collection
  because they reference its ids; stop before settings/canvas/library
  switches. Six advertised dimensions, optional letterboxed output size,
  integer canvas fps 1–240. Details in docs/virtual-camera.md.
- 2026-10-13: **Console critique fixes (N.5.2, branch ui/console-integrate,
  console-fix).** The fidelity and function critiques of 2f22005, each
  checked against Paper (get_jsx of the Welcome board 1-0 and the
  Components "Empty state" 5BS-0, "Menu" 3KN-0, "Destructive" R5-0,
  "Sliders and meters" 3EY-0, Properties' "Add Files…" 5RO-0, Settings'
  summary 5NP-0) and the app, then fixed:
  - Text weight: Theme's `weight-*` hold the drawn weight, Paper's name
    kept (Paper 400 → Mona Sans Light, 500 → Regular, 600 → Medium (Hubot
    Medium), 700 → SemiBold, Martian Mono 300 → ExtraLight; the window's
    default too). Ink against the main board: 400 1.48-1.54x → 1.24-1.28x,
    500 1.41-1.43x → 1.24-1.28x, 600 1.17-1.23x → 1.07-1.14x, Martian Mono
    1.27x → 1.03x; Welcome's title 1.20x → 1.04x. The rest of the gap is
    Skia's (Gotchas); lines are ~2% narrower (Gotchas).
  - No setup (Welcome's window) as board 1-0 draws it; EmptyState (with
    Paper's glyphs) in every empty dock and the empty canvas; menus keep
    the 16px lane; Remove red; LIVE/green ring only while streaming;
    the LCD in templates ("00.00", "00.0%": the CPU cell is 11px wider
    than the board's 79 so the bar never moves at 10%); Remove sheets use
    the destructive button; a muted strip as "Camera Audio" (its meter
    still shows what it hears, in `meter-muted`); tokens for the meter's
    peak and the streaming glyph's white; the 12-unit plus.
  - Settings' "~4.3 GB/h" is the program file's rate from the shared
    estimate (protocol `ProgramRecording::bytes_per_second`, which the
    engine's disk check uses too; Status.program_bytes_per_second for the
    CLI and MCP). The status bar's free space without a setup is
    ~/Movies/Isocast's disk. The master level shows left and right (the
    engine's program level gained `channels`).
  - Keyboard and screen readers: a ⋯ "More for <name>" in each strip's
    volume slot (hover or focus; always a Tab stop; Paper's strip has no
    menu button, so it hides in the volume's place) and ⇧F10 / Menu key;
    the quick transitions one Tab stop with arrows, Space/Return and
    `accessible-enabled`; Segmented's whole-capsule ring and → / ← from a
    Custom quality to the next stop; reset glyphs are IconButtons; the
    file in the selected source's line; the path field's
    "/Volumes/…/scratch/rec"; Welcome's names; LCD descriptions.
  - Small windows: Sources folds empty ISO lanes under 220px (the badge
    lane is badge-wide); faders keep 100px (Theme.fader-min: 0.6 dB a
    pixel), and a strip under 300px drops the headphones (its ⋯ menu has
    Audio Monitoring); band fields shrink to 64px before labels elide;
    "Stop Recording" shortens ("Stop Recor…") beside Marker.
  - Cost: the critic's cached layers (viewer line, transport, pane
    toggles, Studio's heads and column); the master level is Core
    Animation layers under the window in layers mode (layers.rs `Meter`,
    moved by the meter timer in whole device pixels): the control bar
    paints its fill in four pieces round the capsule's inside and the
    MasterMeter only its edge, so anything Slint draws over the bar (a
    menu, a sheet, its scrim) still covers the layer. Offscreen and image
    mode draw it in Slint (pixel steps, 15 Hz when no strip meter shows).
  - CPU on isocast-m2-b (real window, layers preview, benchmark lock, 20 s
    runs alternating main 9cda72d / 2f22005 / this, 3 each, medians):
    at rest 3.1 / 3.7 / 3.3%; three tones, mixer shown 20.0 / 22.3 /
    20.2%, hidden 4.0 / 18.0 / 4.6%; pink noise with a 3 Hz tremolo (a
    level that moves like speech), mixer shown 17.3 / 20.3 / 17.3%,
    hidden 3.9 / 15.6 / 4.1%; offscreen full redraws at 60 Hz (per-redraw
    cost, `--soak --still`) 14.1 / 19.6 / 15.1%. The pixel-steps-at-15-Hz
    build before the layer: three tones hidden 10.8%.
  - Real window (isocast-m2-b, cua-driver, lock held): the meter draws in
    its capsule and a strip's menu and the Settings sheet cover it; a
    strip's ⋯ opens its menu by AXPress, by Tab then Return, and by
    ⇧F10; a scene's right-click menu (lane, red Remove); recording by the
    transport button (PGM, no green ring, "Stop Recor…" beside Marker;
    ffprobe: H.264 1920x1080 60 fps + AAC); Studio Mode's Fade row takes
    the preview; a canvas drag (title 120,840 → 257,750) and Edit › Undo
    back; the Remove sheet's destructive button; Properties.
    Shots: /Volumes/IsocastBuild/console-fix/compare/m2b.
  - Left as they were (reasons): the Properties well's "00:00:30" needs a
    media position the engine doesn't report; About's glyph, Cut's gear
    and the right pane toggle stay as N.5.1a/b decided; Welcome's "+"
    stays disabled (no setup to add a scene to, the board draws it
    enabled); the idle LCD's 0.35 (2.99:1) and white on `onair-fill`
    (2.64:1) are Paper's values (Questions for Ryan); the items the
    function critic found on main too (Sources' blank Tab stop, a sheet's
    own stop, the first typed character offscreen, "In preview" after
    Studio Mode).
  - Scores (/Volumes/IsocastBuild/console-fix/compare, scripts/pairs.sh):
    the whole Welcome window 0.77% → 0.49% (1x and 2x); main viewer 0.81
    → 0.57%, control bar 2.82 → 2.68%, status bar 1.94 → 1.54%,
    Settings' footers 2.87 → 2.27; the card-only and dock scores rose
    (Welcome card 3.10 → 4.81, docks 1.04 → 1.25%, Properties 1.58 →
    1.86) from the narrower glyphs, not from layout (rows unmoved).

- 2026-10-13: **Console integration (N.5.1c, branch ui/console-integrate).**
  ui/console with ui/console-main, then ui/console-dialogs, merged.
  - Both areas added an icon `broadcast` and one `sliders`, drawn
    differently; each matches its own board, so both stay. The sheets'
    16-unit glyphs (Settings' Stream and Advanced, the profile and
    Advanced Audio sheets) are `sheet-broadcast` and `sheet-sliders`.
  - console-main's SvgPath (stroked paths at Paper's size) now draws the
    sheets' glyphs too: Settings' sidebar icons and the header glyph tile
    were ~10% small on ui/console-dialogs and now match the board's.
  - Settings' summary has its disk badge ("217 GB free", the drive glyph)
    from Status.recording_free_bytes; `gigabytes_free` and
    `source_format` live once, in model.rs.
  - Scores against the boards (/Volumes/IsocastBuild/console-integrate/
    compare, scripts/pairs.sh): main window 0.86% / 0.79% (1x/2x, pictures
    masked), Studio Mode 0.92% / 0.75%, Properties 1.58 / 1.39 (0-255),
    Settings › Output 2.66 / 2.59, Welcome 3.10 / 2.95, the kit gallery
    1.48% / 1.22%. The main window is pixel-identical to
    ui/console-main's but for live content; the sheets differ from
    ui/console-dialogs' only in the window behind them and the glyphs.
  - Cost, isocast-m2-b (real window, layers preview, benchmark lock,
    20 s runs alternating with main's 9cda72d, `--bench`): at rest
    3.7/3.8/4.1% vs 3.5/3.6/3.4%; three tones 21.1/22.5/22.0 and
    21.6/20.8/21.0% vs 19.5/20.1/20.2 and 19.6/21.0/18.9% (medians 21.4
    vs 19.9; ui/console-main's own build 22.4 in the same session, so
    the merge adds nothing); console-main's review60 setup 21.4 vs
    19.6. With the mixer dock hidden ({"mixer": false}) and three tones:
    16.6% vs 3.7%, 28.7 redraws/s vs 0.3: the control bar's master
    level (a signature piece of the boards) moves with the sound, and
    Slint redraws the whole window for it. The ~1.5 points with the
    mixer shown are the same cause (more drawn per frame). Left as it
    is: halving its rate or drawing it as a native layer are the
    options if the budget matters more than the board's meter; for
    Ryan to weigh. Scripts: /Volumes/IsocastBuild/console-integrate/
    remote/ab.sh and abp.sh (on the test Mac in
    ~/isocast-scratch/console-integrate/perf).

- 2026-10-13: **Console main window and Studio Mode (N.5.1a, branch
  ui/console-main).** Restyled from the Main window (8-0) and Studio
  Mode (9B-0) boards with the foundation's kit: OBS's layout as the
  boards draw it (viewer line, preview with its 1px `line` ring 8px clear
  of the sides, the 44px control bar, the 240px dock row, the status
  bar). Board-vs-app choices:
  - The fader keeps its −60..0 dB law. The boards' knob positions are
    mock values no law fits (0 dB at 77% of the track, −12 dB at 58% on
    the Main window; −9 dB at 72% on Components), so neither a Logic
    curve nor headroom above 0 dB comes from them.
  - The right pane toggle shows and hides Studio Mode's program. On the
    boards it is off in both, because it is the Workspace boards'
    Inspector, which today's layout doesn't have (no workspaces yet); a
    dead button would be worse. Lit while Studio Mode is on.
  - Scene Transitions' gear stays disabled for Cut and Fade (no
    properties, as OBS); the board draws it enabled with Cut.
  - Controls dock, recording: Marker stays beside "Stop Recording", Split
    lives in the take's details (status bar REC, beside Mark): with the
    icon and words from the left, the button and two 28px buttons don't
    fit 172px ("Stop Rec…"); one does (138 of 140px).
  - Toasts sit under the viewer line (y 40) so Fit and View stay usable.
  - The mixer below a 300px strip (a ~1024px window) drops the dB words;
    the fader's tooltip says the volume then.
  - The status bar's free space comes from the daemon (`Status`
    `recording_free_bytes`: the recording folder's disk, or the nearest
    folder above it that exists; also in `isocast status` and MCP).
  - Scores (chrome, pictures masked; mean absolute difference, % of full
    scale): main window 0.84% at 1x and 0.76% at 2x, Studio Mode 0.92% and
    0.74%; the rest is the text weight (Gotchas), live content (LCD, the
    meters' live peak and hold, paths) and the pictures (the engine's,
    not Paper's mock).
  - Cost, isocast-m2-b (base M2, real window, layers preview, one
    isolated engine, benchmark lock, 20 s runs alternating with main's
    9cda72d build): idle 3.4/3.8/3.6% of a core vs main's 3.8/3.9/3.5%;
    meters moving 20.7/21.6/21.5% vs 19.6/21.2/21.7% (medians 21.5 vs
    21.2), 30 redraws/s, 60 fps both. No regression.
- 2026-10-13: **Console sheets, Settings and Welcome (N.5.1b, branch
  ui/console-dialogs).** Every dialog is a kit Dialog sheet: Properties
  (Paper's "Properties, media clip": glyph with the kind's yellow part,
  the preview well, grouped rows, Defaults left), Settings (Paper's
  "Settings, Output": NavList sidebar the sheet's height, the page's
  header, grouped rows with reset glyphs and captions, SheetSummary),
  Welcome (Paper's "Welcome": LcdHero, Hubot display title, three 34px
  capsule choices), and, with no board of their own, in the same
  manners: Filters (Settings' layout: lists in the sidebar), Create/
  Select Source, Rename, Edit Transform (grouped rows), Transition
  Properties, Advanced Audio, the profile and collection sheets, About
  and the first run. Decisions and compromises:
  - Paper's Settings board shows Medium chosen with "Quality value 60";
    the engine's Medium is 50, so the app chooses a segment only when
    the quality is exactly a preset (60 shows no segment chosen).
  - Settings' summary shows what the setup says (size and rate, sound,
    codec · container); Paper's "~4.3 GB/h" needs an estimate the engine
    doesn't expose (isocast-engine space.rs has one for its disk check),
    and the disk badge waits on Status.recording_free_bytes, which
    ui/console-main adds: after both merge, set the SheetSummary badge
    ("drive", "212 GB free") from it.
  - The Properties well's time ("00:00:30") is left empty: the engine
    reports no media position or duration (SourceStatus).
  - Welcome's line is 470px, not Paper's 460: WebKit sets its first line
    ("…all on one clock.") in 459.5px and Skia a pixel or two wider, so
    460 broke it a word early. The lines are centred, so only the break
    changes.
  - Sheets whose title is only a name keep a descriptive name for
    screen readers (Dialog `accessible-name`: "Properties for “Intro
    Clip”").
  - Speed shows "1.00×" (NumberField `suffix` + `fixed`); other numbers
    keep the web app's "as few decimals as needed".
  - About keeps the brand mark in its body (the header's glyph tile
    takes line icons; the mark is a picture). Remove's confirmation keeps
    the yellow main button ("Remove"), as before.
  - Kit fixes found on the way (their own commit): the leading tokens,
    the Toast's edge and gaps, LcdHero's column height (Gotchas).

- 2026-10-13: **Console foundation (N.5.0, branch ui/console).** The
  native app's look is Paper's "C. Console" (01M4SHX06Z4XBZDXMWN2YEB9EA;
  the original Screens boards Main window, Studio Mode, Properties,
  Settings, Welcome, and the Components page; the Workspace boards beside
  them are a later idea, not built). Values were read with get_tokens,
  get_jsx (inline styles) and get_computed_styles, never from pictures.
  - Tokens: ui/theme.slint holds all 97 Paper tokens under Paper's names
    (minus the `--`; ems turned into px at their size), and the literals
    the boards repeat get names of ours there (r-menu 8, r-compact 5,
    r-mini 4, r-cap 3, r-glyph 10, the LCD's 18/21 and 44/52 type, sheet
    padding 20/24, sheet-top 44, capsule and transport sizes, the three
    shadows, frame-line #000). The old names areas use all still exist.
  - Fonts: Mona Sans 2.027 (static/ttf 400, 500, 600, 700), Hubot Sans
    600 (fonts/ttf) and Martian Mono 300 (fonts/ttf), the projects' own
    static cuts from github/mona-sans, github/hubot-sans and
    evilmartians/mono, unmodified (a cut we instanced ourselves would be
    a Modified Version under the OFL and couldn't keep the Reserved Font
    Name), with LICENSE-mona-sans/-hubot-sans/-martian-mono. Paper's
    Mona Sans has wght and wdth axes; hb-shape widths of 33 of the
    boards' labels in the static cuts are within 1-2px of Paper's
    (ceil-rounded) node widths, so the tracking tokens are 0 now (the
    WIP's +0.3px came from the system font's correction). The only
    weights the boards use: Hubot 600 for titles, tags and LCD labels;
    Martian Mono 300 for every live number (the Components board's
    token captions use 400; the gallery draws them in 300).
    Instrument Sans left the Slint app (Hubot Sans takes the brand
    titles; the web app keeps it); the brand mark is unchanged.
  - Board inconsistencies, resolved one way: badges are 600 (every
    screen; the Components "Badges" sample omits it); a disabled button
    fades to 40% (Components) except a sheet's footer buttons, drawn as
    the Settings sheet's Apply (`hover` fill, `text-disabled`); the
    control bar's master meter is MasterMeter and the Components band's
    two separate capsules are `MasterMeter { split: true }`; a sheet's
    footer is 68px (20 above the buttons) everywhere, where the "Recording
    output" sample has 16 above; list rows' trailing controls are 8px
    apart (Paper's row gap; the old kit had 0).
  - Themes later: each signature piece is a kit component with its own
    interface (kit.slint lists them) and only the kit reads Theme for
    them, so a theme can swap Theme's values and, where its look differs
    in shape (an LCD that isn't an LCD), its own version of a component
    with the same properties and callbacks. Interactive components take
    `shown: ShownState` (live, hover, pressed, focus) so the gallery can
    draw every state; the app leaves it alone.
  - Slint compromises: text draws heavier than Paper's WebKit export
    (Gotchas); one shadow per element, so Paper's two-layer "floating"
    sample (0 12 32 #00000073 + 0 2 6 #0000004D) keeps its main layer,
    and the real menus, toasts, sheets and tooltips are one layer in
    Paper anyway; inset shadows (a keycap's lower edge, a swatch's
    `line-soft` ring) are 1px lines and borders inside the element; no
    blur or blend modes are needed; menus, dropdowns and popovers are
    in-window popups as before; the "Editing" field's caret is drawn
    for the gallery's focus state (a TextInput only shows one when it
    has focus).

- 2026-10-13: **S.2 scale repairs ported onto main after 5.0
  (stress/scale-port).** Ported: c1e3d40 + 9f40df8 (ISO frames that
  aren't Y'CbCr, or are filtered, drawn into 4:2:0 frames on the GPU by
  record/iso_frames.rs, its stage set up before the clock starts; failed
  files report what they lost) and 8db69d7's filter.rs (one submission
  per source, kept per-pass uniform buffers and bind groups; the
  recording's SourceFilters as Arc<[Filter]>). Main's ToYCbCr::encode
  replaced convert, so the ISO stage makes its own encoder and plane
  views. The ISO stage imports with plain import/ycbcr_target, as
  measured on the repair, not 5.0's per-surface cache. Not ported:
  b3751cf (compositor reuses unchanged imports, texture-cache flush at
  most every 4 ms). 5.0 already keeps each source's last frame, draws
  only when its capture time changes, and imports IOSurface frames with
  import_cached, which never touches the CVMetalTextureCache: main's own
  build does src192 at 163-167% CPU, 57 fps min (the repair: 187%; old
  main 230-239%, 33 fps). The throttle would change nothing left on
  main either: an ISO importer imports once per 16.7 ms. Also left out:
  8db69d7's `unchanged` flag (reuse still output), because 5.0's
  compositor only filters an unchanged frame again when its filters
  move (scroll, render delay) or the scenes were adopted again;
  stillfilters4 is 2.5% CPU on main already.
- 2026-10-13: **Stream encoder stall reserve (S.3d).** The 16-packet tap
  was shared by 60 video frames and 100 audio blocks per second (~100 ms).
  Reserve 96 packets, consumed immediately, with the existing 32-picture
  pool cap still bounding surfaces. Keep only eight VT submissions in
  flight; when full, a streaming worker waits on completion for at most
  400 ms while capture/compositing continue using nonblocking routing.
  Monitors retain immediate discard at eight. Notify completion under the
  same mutex used by the wait predicate, including native dropped frames.
  Encoder bitrate/keyframe settings, PTS, interleaving, congestion dropping
  and recording queues/finalization are unchanged. This qualifies the
  measured 60 fps 310 ms stall, not arbitrary duration or encoder overload.
- 2026-10-13: **Profile and Scene Collection menus (P.2)**, in both apps,
  laid out as OBS's (its KB: New makes a blank one, Duplicate copies the
  one in use, Rename and Remove act on the one in use, Import/Export):
  New…, Duplicate…, Rename…, Remove…, Import…, Export…, then the library's
  entries (name order, the one in use checked; choosing one switches).
  New and Duplicate switch to what they make (`library_create` with
  `switch`, as OBS does); while recording the engine makes it but refuses
  the switch, and its message shows. Remove switches to the first other
  usable entry, then deletes (the engine refuses to delete the one in
  use; it keeps the file in `deleted/`); disabled when there is no other.
  Import adds without switching (as OBS), with a toast offering "Switch to
  it"; it takes an exported collection or profile (.toml/.json), a whole
  setup (both parts) or OBS's own files: a scene collection .json (told
  apart by `current_scene`/`scene_order`) or a profile's basic.ini/folder
  (`library_import_obs`). Export writes the one in use through the save
  panel, TOML or JSON by extension. The files are read and written by
  `isocast_client::library`, shared with the CLI. Open Setup File… (⌘O)
  and Import from OBS… left the Scene Collection menu: Import… does both
  without overwriting the collection in use (the Welcome screen keeps
  both for the first run). After any switch or migration (from any
  client) both apps forget their undo history, so an undo can't write the
  previous collection over the new one. Same-named entries show their id
  ("Interview (interview)"), since the engine allows equal names.
- 2026-10-13: P.2 in the web app: its two menus are native NSMenus whose
  items the interface sets (`{"native": "menu_items"}`) from one model
  (`menuItems` in ui/ui/library.js); snapshots draw the same model in the
  window (`app.libraryMenu("profile")`). In the Slint app the MenuBar's
  menus repeat over `LibraryArea.profiles/collections`; Slint 1.18 flips
  a checkable item's `checked` itself when it is chosen (breaking the
  binding to `current`), so the lists are replaced with new models after
  every choice and library change (a new model makes new items). Its
  snapshots show Slint's in-window menu bar (`--state menu-bar-open:Profile`),
  the declared menus themselves.
- 2026-10-13: Keyframe animations (5.2a, docs/scenes-as-code.md 5.2).
  Where it went beyond or decided what the design left open:
  - One `Easing` for transitions and animations: `{ cubic_bezier = [...] }`
    and `{ steps = n, position = "end" }` are taken wherever an easing is,
    and the four names keep their formulas bit for bit (a test compares
    1,001 points). Its schema is now `anyOf` [the names, the two objects];
    both apps' schema forms read such a choice as the names (Slint
    `enum_options`, web `enumOptions`), so the transition dialogs still
    show a dropdown (checked against 5.5a's form output in node).
  - Animations' `easing` defaults to `ease_in_out` (Isocast's default for
    transitions), not CSS's `ease` (taste call; TSX will write CSS's
    béziers out). It is each stretch's easing unless a stop sets one (CSS's
    `animation-timing-function`), not the whole run's.
  - A property no stop at 0 (or 1) sets runs from (to) the value under it,
    per property, which is the item's own or an earlier animation's (CSS);
    later animations win. Colours interpolate premultiplied.
  - Animated translate/rotate/scale go around the item's own
    `style.transform`: place = translate(centre + t) · rotate · scale ·
    own transform about the centre (CSS's individual transform
    properties); `translate` % is of the item's own size; opacity
    multiplies its own and its children's. Keyframe `scale` may be 0.
  - `params` (filter params) isn't animatable yet: items have no filters
    until 5.3a, and a source's filters also draw its filtered ISO file
    and every item showing it.
  - 5.2b's built-in keyframe ids (`fade-in`, `fade-out`,
    `slide-in|out-left|right|up|down`) are refused as setup ids now, so
    setups written today keep loading after 5.2b.
  - Clocks. `on_air`: per feed, by scene id; the program notes its scene
    (and the one a transition leaves) at every tick in `visible()`, drawn
    or not; the preview notes its scene when it draws it, and forgets it
    when nobody wants the preview. Nested scenes use the scene drawing
    them. A scene both feeds draw is evaluated before each draw with its
    own start (each draw submits before the next writes records).
    `shown`: per node id, from the adopt tick at which the item and its
    parents became visible (or it appeared), kept while it hides.
    `clock`: the engine's epoch. Evaluated at the tick (the frame's pts);
    show/hide transitions keep `tick + frame / 2`, unchanged.
  - Compile: items animating background or border get a (transparent)
    fill draw, items animating a shadow a shadow draw whose margin covers
    the largest shadow (×1.25 for overshoot). Per tick the draw list walks
    only the scene's animated items and a compact list of the nodes they
    touch (`animate::Touched`, parents first, with each node's draws,
    clip, parent, outer and own placement copied in at compile, so the
    pass doesn't read the big `Node`/`Draw` structs): one affine multiply
    per node; items that only move or fade get the transform rows and
    opacity patched into their records in place, items animating their
    style a whole `render::record`; its clip rewritten if it clips.
    `rewrite` skips a record that comes out the same, so held (finished)
    animations let frames be sent again; they are still evaluated each
    tick (cheap; the design's "drop off the list" isn't built).
  - Records go to the GPU through a `wgpu::util::StagingBelt` in the
    frame's own encoder (`upload(encoder)`, `finish_and_recall_on_submit`)
    instead of `queue.write_buffer` (see Gotchas).
  - Move transitions give layers of animated items their evaluated
    transform, opacity and style (crop rounded); a moving clip keeps its
    laid-out place in Move's layers (Move draws only the nearest clip).
    Snapshots show animations at the start of their clocks; `get
    --resolved` gives rects as laid out. `engine.compositor.animated`
    counts properties evaluated for the last program frame.
  - The apps don't list or edit animations yet (P.2/N.x); they keep them
    through patches.

- 2026-10-13: Data store, content items, bound text (5.5a,
  docs/scenes-as-code.md). Where it went beyond or decided what the
  design left open:
  - The store is `isocast_engine::data::DataStore` (the engine reads it);
    the daemon owns one (`crates/isocast-daemon/src/data.rs`: events,
    saving) and hands it to engines through
    `SessionControl::with_data`. Listeners are called on the setting
    thread after the store's lock is let go, and only note keys and wake
    whoever draws them.
  - Kept keys belong to the scene collection
    (`collections/<id>.data.json`): the design's "Fit with P and S", which
    is the "setup file" of its 5.5 text since P.1. A collection switch
    flushes the old file and loads the new one's kept keys (unkept keys
    stay); delete moves the file to `deleted/` with the collection;
    duplicate copies it; before any collection exists kept keys wait in
    memory and are saved once the first apply makes one. A file that can't
    be read is moved to `<id>.data.broken.json`, never overwritten. Saving
    is debounced (300 ms quiet, 1 s at most) and atomic with a `.bak`, as
    setups are; shutdown and signals flush it.
  - `persist` is three-way (true keeps, false stops keeping, absent
    leaves it), so a scoreboard kept once stays kept when a script updates
    it. A set to the value a key has changes nothing (no revision, no
    event). `null` shows as nothing, like a missing key. Revisions start
    at the time in milliseconds, as setup revisions do.
  - `data_changed` is sent at once for a lone change, then at most every
    50 ms with every key changed meanwhile (a 1,000/s flood: 20 events a
    second of ~50 keys each).
  - Content: the scene thread draws pictures itself while resolving and
    publishes scenes only with them (no worker pool or 50 ms timeout yet;
    text takes ~0.5-1 ms). Pictures are keyed by everything that decides
    them and shared; the compositor makes RGBA textures at adopt
    (`DrawList::adopt_contents`), sampled as captured BGRA frames are, so a
    content item and its equivalent source draw identically (max
    difference 0). Nothing runs per tick.
  - Text placed by `layout` is drawn the size of its box (fixed
    width/height = the box, wrapped at the box's inner width when its own
    is wider), so layout never stretches text; its least width is its
    widest word (cosmic-text Wrap::Word at 1 px), its width in a block is
    the room it has. Text placed by a `rect` draws exactly as its source
    would.
  - Data changes resolve the scenes at most once a canvas frame (the
    scene thread waits until a frame after its last resolve, collecting
    changes); a setup change never waits. Global text sources with `data`
    run a generator that redraws on change (at most once a frame), with a
    new frame pool when the size changes; size follows the text unless
    `width`/`height` are set. It looks for a stop every 50 ms while idle.
  - Plugins: `data_read`/`data_write` now hold `data_get`, `data_set` and
    `data_delete` to their prefixes (a get needs keys or a prefix). Not
    done: `watch` still sends a plugin every `data_changed` (key names,
    not values) whatever it may read, as it sends every other event.
- 2026-10-13: **WebAssembly audio filters (7.4a,** docs/scenes-as-code.md
  "7.4 WebAssembly tier"). Where it went beyond or against the design:
  - **Fuel, not epoch deadlines.** Each call gets 5,000,000 fuel (about
    one wasm instruction each); past it the call traps and the block
    passes through. 20,000,000 at first: a tight loop ran out after
    1.2-1.6 ms on the M5 but 7-14 ms on the base M2 (its audio threads
    may run on efficiency cores), over the design's 3 ms, so it was cut
    to a quarter; the example's ducker uses 165,877 a block. A work
    budget is the same however loaded the Mac is, so the overloaded base
    M2 (4K flagship) never bypasses a healthy filter because the audio
    thread was descheduled mid-call, and there is no ticker thread waking
    1,000 times a second. Wall time per call is still measured and shown
    (`status`: `engine.audio_filters`, p50/p99 µs and fuel per block).
    Overrule by adding `epoch_interruption` with a ticker that runs only
    while plugin filters exist.
  - **Results are lifted into a host `Vec<f32>`, not read as `WasmList`.**
    wasmtime 49 runs a function's post-return (which frees the guest's
    result) inside `TypedFunc::call`, so a `WasmList` points at freed
    guest memory (the example's allocator writes its free list into the
    first word: the first sample would be wrong). One 4 KB allocation per
    block; the gain costs 2.7-5.7 µs a block on the M5 all in.
  - **Default signal-based traps** (wasmtime's Mach-port handler, which
    forwards faults outside wasm), so bounds checks and Spectre
    mitigations stay as wasmtime ships them. No WASI at all: a component
    may import only `isocast:plugin/host@1.0.0` (and its types); `plugin
    add` refuses one that imports anything else, naming the imports and
    pointing at the example's `no_std` build. 64 MB memory
    (`trap_on_grow_failure`).
  - **Containment.** A trap, fuel or memory overrun leaves that block dry
    (the host's buffer is only overwritten on success), drops the instance
    and makes a fresh one on a "plugin instances" thread (the block's
    chain stays dry until it arrives, a few ms); the third failure in a
    minute turns that filter off until its settings change or its plugin
    is added again. One warning per failure, naming
    `sources[i].audio_filters[j] (plugin/kind)`, what happened and how
    long into the call it was stopped. The first instance is made on the
    mixer thread when the chain is set up (from cached machine code: 26 µs
    on the M5), so a recording's first block is filtered too.
  - **Plugin kinds in setups** (only audio filters so far; 7.3 adds the
    others): `AudioFilterKind::Plugin(PluginKind)` keeps `kind =
    "plugin/kind"` and every param as given (params save in name order;
    serde_json has no preserve_order here), so a setup naming a missing
    plugin loads, runs (bypassed, one warning) and saves unchanged.
    `Setup::validate` checks only the kind's name and `sidechain_source`;
    isocastd's `plan` checks params against the installed manifest
    (field paths, ranges, did-you-mean) and adds a note with the install
    command for a missing or turned-off plugin. `plugin remove` is refused
    while the live setup uses the plugin (listing the paths) unless
    `--force` (MCP `force`). The `schema` method is still 7.3's.
  - **Sidechains.** Chains now hand sidechain filters the other source's
    sound as stereo samples (the built-in compressor takes its loudest
    channel from them, the same numbers as before); a plugin with
    `sidechain = true` gets them for its block's frames, or none while
    that source is muted. Filtered ISO audio still has no sidechains.
  - **Machine code cache**: `plugins/<id>/compiled/<kind>.cwasm`, written
    only by isocastd (outside the plugin's writable data folder, since
    loading it is trusted), keyed by the component file's size and mtime
    and wasmtime's compatibility hash; compiled at `plugin add`, else on
    first use.
  - **The guest example** is `no_std` Rust (wit-bindgen without std, its
    own size-class allocator, `cabi_realloc`, `memcmp`, a trapping panic
    handler, exp/ln in f64 so its gain is bit-exact with the built-in
    `10f32.powf`); built components (gain 16 KB, ducker 9 KB) are committed beside
    their source so `plugin add examples/plugins/gain` works without the
    wasm32-wasip2 target. wasmtime 49.0.2 (cranelift, component-model,
    runtime, std): 59 new crates in the lock file, isocastd 29.0 -> 42.8
    MB and the CLI 32.3 -> 46.1 MB (release; the CLI links the engine).

- 2026-10-13: **Events by kind (7.6).** `watch` takes optional params
  `kinds` (only these) and `except` (all but these), named as events'
  `type`s (`EventKind`, checked against the Event schema by a test); a
  misspelt kind is refused with its path (`params.kinds[1]: unknown
  variant ...`, listing the kinds) and a filter that lets nothing
  through is refused. isocastd keeps each watcher's set as a bitmask and
  doesn't even serialize an event no watcher wants. A watcher asking for
  rare kinds could go hours without a write, so its thread peeks at the
  socket every second (`recv` MSG_PEEK|MSG_DONTWAIT) and leaves when the
  client has hung up, removing itself from the list. `isocast watch`
  asks isocastd to leave levels out unless `--levels` (it used to drop
  them client-side); MCP's event thread asks for all but levels (it never
  kept them); the deck asks for setup_changed, scene_changed, started and
  stopped. The apps still take everything (their meters use levels).

- 2026-10-13: **Stop-only, recording-only mix draining (S.3a-f1).** The
  earlier ready-input live mixer removed useful slack and was rejected.
  `SessionControl.mix_drain_until` is set only once recording reaches its
  end, bounded by the last required video-frame boundary across started
  takes (so pending splits cannot hide the active file), then cleared at
  writer completion. Include the audio block straddling the boundary; the
  file trims excess samples. Program-mix recording routes are tagged:
  they receive complete Stop-tail blocks early, while live taps receive
  the same immutable blocks in timestamp order at their 200 ms deadlines.
  Pending live audio is bounded by MIX_DELAY; flush older pending blocks
  before publishing newer ones after any scheduler stall. The mixer can
  also drain while ending, but live delivery stays paced. Missing-input,
  finalization, fragment and frame-tick safeguards are unchanged. Scalar
  metering is restored; the SIMD decision below is historical, not current.

- 2026-10-13: **Meter reduction trial (S.3b, reverted in S.3a-f1).**
  The trial reduced contiguous blocks in four peak/power lanes (NEON),
  then merged totals under the meter mutex. Odd-sized chunks matched the
  strided monitor path's peak/RMS within 1e-10 dB; audio was unchanged.
  Retained in round 1 as a narrow sampled-hotspot improvement, then removed
  in follow-up 1 because neutral-or-better process CPU was not established.

- 2026-10-13: **Original S.3a (live behavior superseded by S.3a-f1).** The
  mixer can emit a block after its time once every input, including
  sidechains/filter lookahead, has arrived. Missing audio retains MIX_DELAY
  (200 ms); the recording retains its 300 ms bounded tail for incomplete
  files. A file whose final video frame and every audio track are present
  finalizes immediately; the recording waits for its thread and native
  writer, then writes the final manifest. Compare video completion in
  frame ticks: `last_pts + one_frame` can be 1 ns below the same boundary
  calculated from the file start at 60 fps. That rounding mistake caused
  intermittent full-tail waits in the first trial. No encoder, quality,
  queue cap, fragment interval, disk safety, or finalization barrier changed.
- 2026-10-13: Layout (5.1, docs/scenes-as-code.md). Where it went beyond
  or decided what the design left open:
  - Values stay as written: a `Length` is a number or a string (`"12px"`,
    `"50%"`, `"auto"`) read by validation and the engine, so a setup
    saves back as it was (whole numbers save as integers: `gap = 24`, not
    `24.0`). Grid tracks and placements are strings read by our parser
    (`layout.rs` in isocast-protocol); taffy's `parse` stays off.
  - Resolve = lay out a copy of the scenes and write every item's rect
    (whole pixels, taffy's rounding) into it; compile, snapshots, Move and
    `get --resolved` all read rects as before. A scene with no `layout`
    anywhere skips taffy (5.0's draw lists exactly); `rect` items in a
    laid-out scene are absolute at their numbers and keep them verbatim.
  - `display` defaults to block (CSS); taffy's own default is flex, so it
    is set on every node. A rect is `position = "absolute"` with those
    numbers, so it is relative to the parent's padding box edge (no
    borders in layout), as in CSS.
  - Pictures (items with a source or scene) are CSS replaced elements:
    their natural size is the picture as drawn (the compositor's size
    after filters, else the slot's newest frame or the settings' size
    through the filters, else a 16:9 shape), so a flex row of 320x180
    patterns sits at 320x180; stretched in a flex line (align_items
    stretch, the default) they keep their shape, as `<img>` does. Unlike CSS, a picture's least
    size is 0 unless `min_width`/`min_height` say otherwise (CSS's
    automatic minimum made a 1080p camera in a 1080p row refuse to
    shrink and push its siblings off the canvas); `fit` decides how the
    picture fills whatever box it gets. `flex_grow` is CSS's longhand:
    to share a row by grow alone, add `flex_basis = 0` (CSS's `flex: 1`);
    the design's example now does.
  - `visible = false` is `display: none`; a hidden item that took part in
    layout keeps its last rect (by node id) so a hide transition finishes
    where it was while its siblings move up at once.
  - The compositor compares each shown picture's size with what layout
    took (CompiledScenes::sized) and tells the scene thread (once per
    size); the scenes are laid out again and adopted a tick later.
    The scene thread takes each picture's size from the slot's newest
    frame (or the settings' size) through `filter::output_size`, so
    scenes off air, which the compositor doesn't draw, get cropping and
    scaling filters right too; sizes the compositor reported win until
    the next change (a source may have reopened or its filters changed),
    so a size the scene thread can't foresee is reported once and kept,
    never relaid out tick after tick. The scene thread keeps the default
    QoS: user-interactive (tried in 5.1a, undone in 5.1c) measures the
    same on isocast-m2-a run after run (50 items ~205 us, 1,000 ~757 us
    median, either way); 5.1c's message blamed it for the 50-item time,
    but an earlier run without it had measured 114 us and the final one
    without it 205 us: that difference is between runs, not QoS.
  - Generation: one counter per process, bumped only when a resolve moves
    something (node ids, transforms, sizes, visibility), so a rename keeps
    it. Program frames are stamped (revision, generation): H.264 revision
    records carry two u64s (clients read the first 8 bytes, as before);
    shared and JPEG frames say `layout_generation` (`generation` already
    numbers shared rings). `get --resolved` waits up to 250 ms for the
    live engine to lay out the current revision; with no engine it lays
    out on the spot with settings' sizes and says generation 0.
  - `overflow = "hidden"`: clips are 64-byte records (the clipping item's
    inverse transform, size, radii, enclosing clip) in a second storage
    buffer; a draw names its clip in `sampling.w`, spare until now, so
    unclipped draws cost one branch (a 4K flagship A/B against a build
    without the branch, 5 runs each: no difference beyond the spread).
    Coverage is antialiased from the
    transform's scale, not `fwidth` (no derivatives inside the chain
    loop). Move transitions carry only the nearest clip (interpolated if
    both scenes clip the item).
  - `Method::Get(Option<GetParams>)`, so `{"method":"get"}` parses as
    before; `Request::located_error` says "takes no params" only for
    methods that take none.

- 2026-10-13: Compiled scenes (5.0, docs/scenes-as-code.md). The scene
  thread (`record/scenes.rs`) compiles the live setup's scenes whenever
  publish() runs (setup change, sources reopened) into `CompiledScenes`
  (`compiled.rs`): every item, visible or not, as a node (id = its
  item_path interned in `NodeIds`, kept across compiles) and draws
  (shadow, then picture-or-fill) with their 272-byte records built by
  `render::record`, the arithmetic of `scene::layers_with` in the same
  order (so pictures are bit-identical). The compositor adopts a version
  with one atomic load per tick; `draw_list.rs` keeps the records in one
  storage buffer and rewrites only those that change. Two choices beyond
  the design: (1) the frame-dependent fields of a record (crop in texture
  coordinates, fitted picture rect, Y'CbCr matrix, texel kind) are filled
  by the compositor when a source's picture changes size or format, not by
  the scene thread (they depend only on the frame; 5.1 will hand the scene
  thread source sizes for intrinsic layout); (2) a frame is sent again
  unchanged when, for its feed (program or preview), the compiled version,
  the record generation (any rewrite), the scene, and every shown source's
  frame time are the same as when it last drew, and no scene transition,
  stinger, show/hide transition or moving filter (scroll, render delay)
  runs. Captured frames get stable textures per IOSurface
  (`FrameImporter::import_cached`, made with
  `newTextureWithDescriptor:iosurface:plane:` rather than
  CVMetalTextureCache, whose textures retain the CVPixelBuffer and would
  pin pool buffers); the compositor holds each source's frame until its
  GPU work is done, so the textures need not. Kept surfaces go after 2 s
  unused once 16 are kept (at most 64), and cached bind groups (which hold
  textures) after 120 drawn ticks unused. The working set is bigger than
  it looks: the flagship's two decoders and the program's frame pool cycle
  through more than 32 surfaces; capping at 32 (5.0e) made every frame
  miss and cost +18% compositor CPU (610 -> 717 us) and p99 2.5x, so it
  was undone (5.0f). A kept texture keeps its surface's memory after its
  pool lets go of it: at most 64 surfaces, for 2 s (4K flagship RSS 114 ->
  125 MB on isocast-m2-a). Move transitions still blend `Layer`s (built from
  the scenes each tick, so allocating while a Move runs).
- 2026-10-13: Plugin registry and process plugins (7.1, design
  docs/scenes-as-code.md "7. Plugins"). The plugins folder
  (`api::plugins_path`: `$ISOCAST_PLUGINS`, else beside an isolated
  engine's socket, `/tmp/x.sock` → `/tmp/x.plugins`, else
  `~/Library/Application Support/Isocast/plugins`) holds `<id>/install.toml`
  (source, linked, enabled, why disabled, the grant), `<id>/plugin/` (the
  copy; absent with `--link`), `<id>/data/` (the only writable place; the
  process's HOME and TMPDIR), `<id>/socket`, `<id>/log` (moved to `log.1`
  past 10 MB at a start) and `<id>/pid` (pid and start time, so the next
  isocastd kills a process a crashed one left). The manifest is parsed in
  `isocast_protocol::plugin` (all sections, params with types, ranges and
  defaults, field-path errors); `plugin add` needs `grant` equal to what it
  asks for (the CLI's `--allow` reads the manifest and sends it); an
  upgrade asking for no more needs none; `plugin_add` and
  `plugin_set_enabled` can't be granted (a plugin could install another
  with more). Scoping: each process plugin gets a socket of its own (its
  `ISOCAST_SOCKET`), the only one its sandbox may connect to; a connection
  on the main socket from a plugin process or its children (no sandbox) is
  scoped too, by `LOCAL_PEERPID` and parents (looked up only while plugin
  processes run). Why a socket each besides the design's peer-pid: a
  plugin's double-forked child is re-parented to launchd and would escape
  peer-pid scoping. Processes are spawned with posix_spawn: own process
  group (stop = SIGTERM to the group, SIGKILL after 3 s),
  POSIX_SPAWN_CLOEXEC_DEFAULT (no inherited recording files or client
  sockets), default signals, responsibility disclaimed (TCC sees the
  plugin, not isocastd's grants), under `sandbox-exec -p` with an
  allow-default profile: home unreadable but its folders and granted
  files (parents readable as metadata only), writes only in data, exec
  only of its command, its own folder and its interpreter's installation
  (a framework's version folder, a Homebrew keg, or for /usr/bin stubs the
  developer tools), no outbound network but its socket unless `network` is
  granted, no camera/mic devices, no tccd, WindowServer, replayd, Launch
  Services, Apple events, pasteboard, SecurityServer, coreaudiod or
  locationd lookups, no signals to other processes. Restarts back off 1,
  2, 4, 8 s… (≤ 60) counted from the last start (a plugin that ran a while
  restarts at once); 5 exits in 5 minutes turn it off, kept in
  install.toml with the reason, until `plugin enable`. `remove` stops it
  and deletes its folder, keeping a non-empty data folder in
  `plugins/.removed/<id>-<time>`. The deck's grant adds `status` to the
  design's seven (it needs what is on air and whether it records when it
  starts). Left for 7.3: the `schema` method (`remove --force` came with
  7.4a, when audio filters became the first plugin kinds setups use); MCP
  has no enable/disable, as designed.

- 2026-10-13: Stream reconnects (chaos test): each outage gets all of
  `stream.reconnect.max_retries` (the count resets whenever the server
  accepts the stream, as OBS does); the guard against a server that
  accepts and drops at once is a separate count of connections in a row
  that lasted under 10 s, also capped at `max_retries`. The stream-key
  hint is only given when every connection was accepted and more than
  one dropped. A stop cuts a connection that hasn't said goodbye within
  0.5 s.
- 2026-10-13: Profiles and scene collections (P.1). The live setup is kept
  as two named parts, as in OBS, each with the setup's own field names
  (`isocast_protocol::library`; `Setup::from_parts`/`into_parts`
  destructure exhaustively, so a new setup field must be placed). A
  **collection** has version, name (the setup's), canvas.width/height,
  sources, scenes, transition, and recording.program.audio,
  recording.iso.sources and recording.iso.audio. A **profile** has its own
  name, canvas.fps, audio (pan law, monitoring device), stream and the rest
  of recording (folder, keep_free_gb, program/iso enabled, file names,
  encoders, program width/height, iso.filtered). Why: items are placed in
  canvas pixels, so the canvas size goes with the layout and a profile
  switch never moves or clips a scene (OBS keeps the base resolution in the
  profile and has exactly that problem); the frame rate is an output choice
  (record at 60, stream at 30), as in OBS. Audio devices are sources, so
  the collection's (OBS also saves its global audio devices in the
  collection); monitoring device and pan law are output settings (OBS's
  Settings → Audio, per profile). The sources recordings take must exist in
  the collection; whether and how ISO files are written is the profile's.
  No hotkeys in profiles yet (the app's keys are fixed); an OBS profile's
  hotkeys are listed as not imported. apply, plan, start and the app keep
  working on the merged live setup; a save splits it and writes only a part
  whose text changed.
- 2026-10-13: The library on disk (P.1): a folder named after the setup
  file (`…/Isocast/setup.toml` → `…/Isocast/setup/`, `/tmp/x.setup.toml` →
  `/tmp/x.setup/`) with current.toml (the ids in use, and `legacy`, an
  FNV-1a hash of setup.toml as last taken in), collections/<id>.toml and
  profiles/<id>.toml (atomic, with .bak), backups/ (setup.toml copies) and
  deleted/ (deleted files are moved there, never unlinked). Ids are file
  names; names are labels. The first start without current.toml takes
  setup.toml in as collection `default` (keeping the setup's name, like
  "Recording") and profile `default` ("Default") and leaves setup.toml as
  it was: older isocastd builds only know setup.toml, so they keep loading
  it and can't touch the library. At every start, a setup.toml whose hash
  differs from `legacy` (an older build saved it, or a hand edit) is taken
  in again as a new pair named after it and made current (a broken one is
  reported once). Tune results stay in setup.tune.json. A switch is refused
  (not deferred) while recording or streaming, since the take's files
  belong to the live setup, and when the merged pair has problems (each
  named by field and part); it flushes a pending live save first, applies
  the pair to the live engine (a new collection goes on air from its first
  scene, preview cleared), then writes current.toml. The documents of the
  pair in use are read from the live setup (newer than the files while a
  live save waits). Deleting the one in use is refused.

- 2026-10-13: Visual design is iterated in Paper first, then brought into
  the app (Ryan: "we want the layout of OBS, but we don't want the design
  style of OBS"). The Paper file "Isocast Design"
  (https://app.paper.design/file/01M4SEFKJS83A60T66PRV0H869) holds 97
  design tokens named exactly like app.css's custom properties (--font is
  Paper's system sans standing in for SF Pro), five screens (Main window,
  Studio Mode live, Settings › Output, Properties, Welcome) and a
  Components sheet, all token-driven; a References page holds research
  moodboards and candidate directions. To port a change: read exact values
  from Paper (get_tokens, get_jsx, get_computed_styles; never from
  screenshots) into app.css, crates/isocast-ui/ui/theme.slint and the
  widget kit, and docs/design.md. Slint can't do in-window background blur
  or blend modes (and has no tabular-figures switch), so designs avoid
  them.

- 2026-10-12: Field paths through `flatten` (V1.8 mcp-field-paths).
  serde_path_to_error can't see into serde's `flatten` or internally
  tagged enums (they buffer fields first), so sources, filters, audio
  filters, transitions, the key filters' adjustments and the flattened
  params (`apply`, `init_setup`, `stream_key_set`) read themselves through
  `isocast_protocol::located`: on an error they read the kind's or part's
  fields again to find the field, and pass its path out in the message
  (`` `direction`: unknown variant ...``), which `located::from_value`,
  `Setup::from_value`/`from_json` and the daemon join onto the outer path
  (`params.transition.direction: ...`). TOML setup errors show that inner
  path in the message. The daemon parses requests as before and only
  works out the path when one fails (`Request::located_error`); socket
  paths start with `params.`. Flattened params now list their valid
  fields in unknown-field errors; `shared_frames` refuses unknown params
  like every other method.
- 2026-10-12: MCP events (V1.8 mcp-events). `isocast mcp` watches
  isocastd on a thread of its own (`crates/isocast-cli/src/mcp/events.rs`)
  and keeps the newest 500 events for a `get_events` tool: `after` cursor
  (the reply's `next`), `kinds` filter (checked against the Event schema,
  listing the valid kinds), `limit`, and `wait_seconds` (up to 30) to wait
  for one. Without `after`, it returns the newest. A tool, not MCP
  notifications: standard clients (Claude Code among them) don't put
  server notifications in front of the model, and a tool also works for
  an agent that only polls. `levels` aren't kept (20/s); only the newest
  `status` and `setup_changed` are kept (1/s while recording, and many a
  second while the app drags), so they can't push warnings out. The
  server adds `connection` events when the watch starts or ends (saying
  so if a recording was running, from started/status/stopped seen), and
  reconnects every second or as soon as a tool needs isocastd: every tool
  first makes sure the watch is up (launching isocastd), so the events a
  tool causes are kept. The buffer is per MCP server process, like the
  CLI's `isocast watch --json` stream it mirrors: it is that stream's form
  for clients that can't read a stream, not a new engine capability.
- 2026-10-13: Encoder capacity (V1.8). `tune` judges the setup's own
  speed on hard content: fits >= 1.25x (HEADROOM, unchanged), no_margin
  1-1.25x, cannot_keep_up < 1x (the benchmark matched recordings: 0.70x
  vs 1044-1054/1440 frames on isocast-m2-a). Not fitting, it benchmarks
  changes in a fixed "closeness" order (costs in tune.rs): program file
  1440p (2), 1080p (4), one preset lower for program and ISO (6), a codec
  switch that fits (7), half the frame rate (12), one ISO fewer (20), and
  sums for combinations. Keeping every file and frame rate comes first
  (ISO files are what Ryan edits from), dropping a file last. It stops at
  the first that fits (max 8 benchmarks; changes whose pixel rate says
  they can neither fit nor be the closest that keeps up are skipped) and
  falls back to the closest that keeps up. On a base M2 that is 30 fps;
  the 1080p program (1.04x, no margin, 0 drops in 24 s) is printed as the
  closer no-margin option. "Program at 1080p" is a new setup field,
  `recording.program.width/height` (the encoder scales the canvas, like
  OBS's Output (Scaled) Resolution), not a smaller canvas, which would
  move every item. Measurements go to `<setup file>.tune.json` (an
  isolated engine keeps its own), keyed by the files' workload (size,
  rate, codec, depth, quality per file) and the Mac (chip, cores, macOS).
  `start` warns only for cannot_keep_up (that workload, or one covering a
  measured failing one file for file), not for no_margin or unmeasured
  setups, so a recommended setup never nags. A lower preset barely speeds
  VideoToolbox HEVC up (1.02x vs 1.04x on the M2): it saves disk, not
  encoder time. On the M4 Max at 120 fps a 1080p program didn't help
  (0.70x both): one 4K120 stream is the limit there, so the pixel-rate
  prediction is only used to skip, never reported.
- 2026-10-13: A picture source's definition includes the canvas frame
  rate (test patterns excepted), so a canvas fps change reopens screens,
  cameras, media and the like; before, a held engine kept them at the old
  rate and their ISO files recorded at it (a 30 fps patch on the M2 still
  wrote 60 fps ISO files that dropped 11%).
- 2026-10-13: N.2 critiques (parity and design critics on slint/n2):
  every high-priority item is fixed; these were left, with reasons.
  Duplicate source names in lists stay as they are (the web app and OBS
  show them alike; only the camera dropdown says "1 of 2"). NumberField
  doesn't take "1,5" (Slint 1.18 has no string replace; parse it in Rust
  if it matters). Settings… stays in File: Slint builds the macOS app
  menu itself and takes no items into it. List rows stay AXStaticText
  (AccessKit's list item; a row role needs Slint). The colour field's ×
  sits 6px further right than the web app's (the web form keeps a scroll
  gutter; the Slint form's text wraps, so it can't), and the LIVE
  details popover is placed by its item, not where WebKit put it: both
  small. The source toolbar's quiet vs bordered buttons waits on Ryan
  (Questions).

- 2026-10-12: N.2 integration: the Slint app has one settings form.
  Areas A and C each built one (A's in areas/sources, C's shared
  src/schema.rs + form_choices.rs). Kept A's (Properties, Filters and
  Create Source already used it; its rows change in place, so a scrub
  survives a live edit, which C's whole-model rows didn't) and moved it
  to src/form/ (rows, `change`, devices, files, schema) and
  ui/widgets/settings-form.slint; its controls report to the
  `SettingsForms` global by form id and `form::dispatch` routes them
  ("transition" → controls, "props"/"filter" → sources). Transition
  Properties moved onto it. Dropped with C's form: its in-Slint colour
  picker (the web app uses the system colour panel, as A does) and the
  `form:DEF:KIND` gallery states. One open panel: `platform::choose`
  (area D's); snapping and the preview rate are `Ui.snap-enabled`,
  `Ui.snap-distance`, `Ui.preview-rate` (area E's names) for both
  Settings and the canvas.

- 2026-10-12: N.2.B mixer decisions: "not in mix" follows the program
  audio list only, as the web app has it (the spike also counted Monitor
  Only); the hidden strips are per-viewer layout (`mixer_hidden` in the
  UI prefs, kept by View → Reset UI, as `mixerHidden` is); Advanced
  Audio's columns are the web app's, and in a narrow window the name
  gives way first (to 80px), then Balance (40%) and Audio Monitoring
  (60%); number fields show their step's decimals ("0.0 dB" where the
  web app writes "0").
- 2026-10-12: N.2.D (area D). Settings keeps the web app's draft model:
  the setup's name, canvas, recording, audio and stream as JSON, changed
  by field name and sent on Apply/OK as one undoable merge patch
  (object_patch in src/areas/settings.rs). Quality stops use the engine's
  numbers (EncoderQuality::value: 30/50/70/85/100), not the web app's
  placeholder 40/55/70/80/90. The web app's "GB per hour" reads a status
  field (`recording_estimate`) the API doesn't have, so neither app shows
  it. Snapping and the preview frame rate are this viewer's (Ui global,
  prefs.rs); the canvas (area E) is to follow them. Settings opens at a
  tab with `open_dialog("settings", "TAB")`, or "stream:TEXT" to say why
  (Start Streaming may instead set SettingsArea.notice first). File
  choosing is NSOpenPanel through objc2 msg_send in platform.rs (no new
  dependency); Open a setup reads TOML/JSON in the app as the CLI does.
- 2026-10-12: N.2.E canvas (branch slint/n2-e-canvas). The pointer over
  the edited monitor goes to Rust (src/areas/canvas.rs: hit tests,
  gestures, snapping, as the web app's canvas.js); Slint only draws what
  it says (CanvasArea: boxes, hover-item, guides, readout, cursor) and
  the native layers draw the selected box with the frame. Shared
  additions other areas use: Ui.snap-enabled/snap-distance and
  Ui.preview-rate (Settings → General, area D: bind them; prefs.rs keeps
  them), CanvasArea.hover-item (area A: set it from a hovered Sources row
  to outline the item, as the web app's `hover`). A right click on an item
  asks for `sources:item:PATH` (the web app adds Preview Scaling and the
  preview items to that menu on the canvas: area A), elsewhere on the
  stage for `canvas`. Native pictures are tagged sRGB, not BT.709: on
  isocast-m2-a sRGB/untagged matched the web app and the image path
  within 1/255, BT.709 darkened them (#1f3a4a showed as #122f41).

- 2026-10-12: N.2 foundation (branch slint/n2). The Slint app is split so
  five agents can build it side by side; src/areas/mod.rs is the guide.
  Areas: A scenes/sources (ui/areas/sources, src/areas/sources.rs,
  `SourcesArea`), B audio (mixer, `MixerArea`), C controls, transitions,
  studio column, status bar, toasts (controls, `ControlsArea`), D
  settings, first run, Welcome, connecting, About (settings,
  `SettingsArea`), E the canvas and preview (canvas + preview.rs,
  layers.rs, frames.rs, picture.rs, `CanvasArea`). Shared and rarely
  changed: ui/app.slint (only places things), ui/state.slint (`Ui`:
  connection, scenes, Studio Mode, selection, open dialog, layout),
  ui/theme.slint (every design.md token by its name), ui/icons.slint
  (the web app's 77 Lucide icons; Rust names icons, Slint resolves them
  with `Icons.named`), the kit (ui/kit.slint over ui/widgets/), and in
  Rust app.rs (`App`: `call` any API method with a reply closure, `edit`
  merge patches as the web app does: shown at once, one in flight, keyed
  edits merge and go `live`, a stale revision retries once, undo/redo),
  session.rs, model.rs, kit.rs (MenuBuilder, disambiguate), toasts.rs,
  prefs.rs. Dialogs are full-window PopupWindows (modal for free: Tab
  stays inside, focus returns, the window behind can't be clicked); menus
  are data shown by one MenuHost (`Menus.request(owner, …)` → the area's
  `context_menu` → `MenuBuilder`); the menu bar and keys go through
  `Ui.menu(id)` with the web app's ids. The per-viewer layout is
  isocast-ui.json in the support folder (`$ISOCAST_UI_PREFS`), never read
  or written by snapshots. A widget gallery (`--state kit`, `kit-menu`,
  `kit-dropdown`, `kit-dialog`) extends the approved snapshot harness for
  checking the kit; it isn't reachable from the interface.
- 2026-10-12: (Superseded 2026-10-13 by the Console fonts: Mona Sans.)
  The Slint app uses the system face ("System Font", SF) via
  `Theme.font-ui`; Slint's own default is Helvetica (fontique puts
  SansSerif before SystemUi). Docks have preferred widths and minimums
  (140, 170, 240, 160, 160 = 874px with splitters) so every dock fits the
  900px minimum window; the mixer gives way first.

- 2026-10-12: Hard testing, setup/OBS import (V1.8). A canvas must be an
  even number from 16 to 8192 on each side (`canvas.width`/`.height`
  name the nearest valid size): wgpu's canvas texture is limited to 8192,
  and 4:2:0 files rounded odd canvases down silently. Media without video
  that a scene has an item for (hidden or not) is on air only while that
  scene is (OBS mixes sources only while active); with no item anywhere
  it plays all along, as before. Decided when the source opens, so
  adding or removing its only item applies when the engine next opens it.
  OBS import: program audio is the first recorded track's sources
  (Advanced `RecTracks`; Simple mode only when `RecQuality` isn't
  "Stream", else track 1), more tracks are listed as not imported; the
  Output (Scaled) Resolution or an Advanced recording rescale is listed
  as not imported (Isocast records at the canvas size); the scene OBS had
  on air becomes a note with the command to put it on air.
- 2026-10-12: M4 ISO follow-up: removed the direct HEVC Q<=70 recording
  encoder. Its four-input window called synchronous CompleteFrames on the
  same thread that feeds the file's audio: one camera trace spent 23.582 s
  completing 270 times in a 24 s take. AVAssetWriter now encodes every file;
  the independent bounded audio/video queues remain. No quality mapping,
  hardware-only requirement, capture copy, clock or fragment-policy change.
  Four interleaved runs each: main 1116 video drops/265 audio dropouts,
  old branch 1930/966, fallback 1065/0. Camera fallback 1124–1440/1440;
  no camera-only collapse. This does not qualify zero-drop recording.
- 2026-10-12: Agent surface (V1.8 hard testing): a setup loaded at start
  gets the time in ms as its revision, so revisions never repeat across
  restarts (no counter file to go stale after a crash). Applies that
  change nothing keep the revision. The method table denies unknown
  params (serde deny_unknown_fields; flattened params name the field but
  can't list the valid ones). isocastd refuses relative folders (params
  must be absolute; recording.folder absolute or ~/); the CLI makes a
  setup's relative recording.folder absolute from its own folder. After a
  recording's end is known, markers are refused and switches carry no
  take/time. MCP results must be objects (live_engine returns {engine}).
- 2026-10-12: App UI polish (branch ui/polish, docs/design.md P1-P3):
  one slate grey ramp named by role, copper only for state (selection
  marker, focus, primary button, on), status fills that pass 4.5:1, a
  4px grid, 24/28/32px controls, 28px rows and panel headers. Taste calls
  Ryan may overrule, each one rule in `app.css`: selected rows are grey
  with a 2px copper bar (were brown), menu hover is grey (was solid
  copper), sliders and the mixer faders are neutral (were copper).
  Meter colours come from `--meter-*` tokens (keep them #rrggbb:
  mixer.js appends an alpha byte for the peak bar).

- 2026-10-12: After two reviews of ui/polish: keyboard focus is a bone
  ring with a 1px gap (`--focus`), not copper, since copper vanished on
  copper and red fills; the focus rules sit last in `app.css` so they beat
  selected states. Selected rows are g9 with a 3px bar and don't change on
  hover; the chosen segment is copper-tinted like Studio Mode on. Stream
  health is a square light with a word when it isn't fine. Teal means
  "the program" (PROGRAM tag, scene dot, the Program monitor's 2px frame
  while recording or streaming); red means a running Stop control or
  danger. `Icon` sizes other than 16 are set inline (`.icon` pinned them
  all to 16). Scenes/Sources rows, sliders, menus and dialogs work from
  the keyboard; arrows on a row only act when it has keyboard focus, so a
  click-selected source still nudges with the arrows.

- 2026-10-12: Event watchers share one serialized, newline-terminated
  notification. Each has a 64-message queue; a full queue or a socket
  write blocked for 2 s disconnects that watcher, which must reconnect
  for a fresh setup. Producers never wait for a slow client. Release
  measurement with 64 stalled readers on the real rig: memory grew
  551→572 MB before, versus 544.5→543 MB after over the same ~46 s.
  CPU and idle-wakeup samples were noisy; no CPU/wakeup gain claimed.

- 2026-10-12: Brand (Ryan): the 3.1 "Equal" mark (copper record dot, two
  bars, bone inside the dot and verdigris outside, on slate), Fraunces for
  the wordmark and Instrument Sans as the secondary font; the app uses
  Instrument Sans only for brand moments and the system font elsewhere.
  The app icon is a Liquid Glass Icon Composer document
  (`crates/isocast-app/bundle/Isocast.icon`), compiled by actool in both
  bundle scripts; `ictool` (in Icon Composer.app) renders previews. Paper
  board "Final — Equal, Instrument Sans, Liquid Glass".

- 2026-10-12: The app (V1.4–V1.7) is a native AppKit shell with a web
  interface (Preact + htm vendored, no build step), a client of isocastd
  only. Studio mode, panel sizes and the mixer being open are per-viewer
  preferences in localStorage. Scene items gained `locked` (OBS's lock,
  carried over by the OBS import; only the app's canvas uses it).
  Settings edits apply at once through `apply`; the stream key goes only
  through `stream_key_set`. The binary is `isocast-app` because
  `Isocast` collides with `isocast` on case-insensitive disks; the bundle
  is still Isocast.app. Ryan's dev app runs release builds (a debug
  daemon spends as long serialising a frame's JSON as encoding it).

- 2026-10-12: Setup file (V1.3): `$ISOCAST_SETUP`; else, with
  `$ISOCAST_SOCKET`, beside the socket (`/tmp/x.sock` → `/tmp/x.setup.toml`);
  else `~/Library/Application Support/Isocast/setup.toml`. Loaded at
  a clock-based revision (see Decisions, agent surface); saves are atomic, read back before replacing, and keep a
  `.bak`. `setup init` defaults: 1080p60 (screens benefit most), HEVC
  q70, ISO of screen and camera with the mic, 300 ms fade, `display =
  "main"`, `mic = "default"`; cameras by name; refused permissions give
  colour placeholders with the same id. Unknown fields are caught by a
  hidden `unknown` map (`#[serde(remote = "Self")]`) checked against each
  kind's field names from schemars.
- 2026-10-12: Streaming (V1.2), see `docs/streaming.md`: no B-frames (so
  FLV needs no composition offsets); every keyframe forced on the
  interval; AverageBitRate plus a 1 s data-rate limit (Apple advises
  against padded CBR); AAC times moved back by the encoder delay; over 1 s
  behind it drops non-keyframes, then video until the next keyframe
  (keyframes and audio kept, 5 s hard cap); fixed-delay reconnects with
  `max_retries`, each new connection starting at a keyframe; the key is a
  Keychain generic password (service `dev.isocast.stream`, account = the
  service or the custom server) and the daemon never lets the Keychain
  prompt; the default output size fits 1920×1080; TLS via native-tls,
  with `ISOCAST_STREAM_CA_FILE` adding one test CA.

- 2026-10-12: Show/hide transitions (4.12d) are Isocast's own: `fade`
  (opacity) or `slide` (the picture moves within the item's rect, which
  clips it; the item's style and children don't move). The compositor
  compares each new setup with the last by item path (scene id, then each
  item's key) and animates items whose `visible` flipped; a hiding item
  keeps drawing (and its source's frames keep coming) until its transition
  ends. OBS's luma wipes, stingers etc. on items import as fades.

- 2026-10-12: The live engine (V1.1, `record/engine.rs`). Like OBS,
  sources, the program and the mix run while anything needs them (the app
  showing frames, `engine start`, a recording, a stream), and outputs
  attach to it through `Routes` (per-producer lists of channels, added and
  removed while it runs). Sources open in a background reconciler (all at
  once) whenever the setup changes, retrying failed ones; scenes, filters
  and audio settings take effect at the next frame. A recording picks its
  start on the program's frame grid (≥200 ms ahead), routes the program,
  the mix and ISO audio to its files, runs its own frame clock per ISO
  source, and pins the sources it records: device changes to those wait
  until it stops, and the canvas can't change while the program is being
  recorded or streamed. The mix is rebuilt only when its inputs change,
  taking over at the same sample. `record()` (the CLI's in-process
  recording) is now "start an engine, record once, stop", and still fails
  if any source can't open. Behaviour change: a capture that stops in the
  middle of a recording no longer stops the whole recording; its ISO file
  ends there (reported as an error), the rest keep going. The daemon stops
  the engine 15 s after the last frame request when nothing else needs it.
- 2026-10-12: Live frames are JPEGs made by VideoToolbox (any pixel format
  to CGImage), Core Graphics (scaling) and ImageIO (encoding), not Rust
  code, so they stay fast in debug builds: 1280×720 program frames at
  60 fps over the JSON socket, ~165 KB each.

- 2026-10-11: OBS import (4.14) lives in `isocast_protocol::obs` and runs
  in the daemon, which supplies this Mac's devices and file/text sizes
  (`isocast_engine::content_size`, each read given 3 s so a privacy prompt
  can't hang an import). OBS anchors an item's box at `pos` by its
  alignment and rotates about that point; Isocast rotates about the
  centre, so the centre is carried over. Bounds are converted to an exact
  rect and crop using the size OBS saw (from settings, the display's mode
  in pixels, the camera format Isocast would pick, or the engine); else a
  boxed item fills its box and an unboxed one assumes the canvas (noted).
  The canvas is the collection's saved `resolution`; the profile gives fps,
  folder, container, codec and file name. Transitions other than the
  current one are listed; audio and video filters, balance (2·b−1) and
  monitoring map onto Isocast's (monitoring desktop audio is left off).
  Filter, transition and audio filter settings are converted through
  serde so their defaults and validation stay in one place.

- 2026-10-11: Transitions (4.11) are `kind` plus settings like filters;
  the compositor draws both scenes on canvases and one pass in
  `isocast-engine/src/transition.rs` combines them. Luma patterns are our
  own formulas generated in the shader (no OBS wipe images). Swipe, slide
  and move ease cubic in-out by default. Precedence: switch >
  `scenes[].transition` > setup `transition`; `duration_ms` defaults to
  300 (OBS's default). Move matches items by key (same source or scene),
  interpolates centre, angle, scale, size, crop, opacity and style, draws
  in the new scene's stacking order, and falls back to a fade if layer
  counts differ. Stingers open with the session as video-only media
  (`stinger:<file>` ids), go on air a frame early and pick frames by the
  video's own clock; a matte stinger's matte half must be opaque. Media
  with alpha decodes to BGRA with straight alpha; restart-on-show
  playlists keep their first file cued (open plus first decode took
  60–220 ms in debug).

- 2026-10-11: Audio filters (4.10) run in the mixer, per input, in place
  on its 10 ms block just before summing: sidechains need the other source
  at the same moment, and only the mixer has every source aligned (200 ms
  behind real time), so filter changes affect audio from about 200 ms
  earlier, like gain and mute. A sidechain-only source is opened and fed
  unmixed (only if it was a sidechain at start). Sidechains hear the source
  after its filters, before its gain, silent while muted; output gain is
  not applied with a sidechain (per OBS's KB). Formulas are ours: peak
  detection holds 10 ms then falls with a 10 ms time constant (expander and
  upward compressor can use 10 ms RMS); attack/release are time constants
  of a linear-amplitude follower; hard-knee curves; the limiter is
  threshold / held peak; the gate has hysteresis, hold and linear fades;
  the EQ uses LR4 crossovers at 250 Hz and 4 kHz (a taste call). Noise
  suppression is `nnnoiseless` 0.5.2 (pure Rust, BSD-3) with a constant
  960-sample delay fed ahead; the dev profile builds it and its FFT crates
  at opt-level 3 (11% → 0.7% of a core per channel). Filtered ISO audio
  keeps its own chain per source (noise suppression runs twice if a source
  is in both).

- 2026-10-11: Balance (4.13a) uses OBS's three law names with our
  formulas: the far side gets √2·sin(π/4·(1−|b|)) (sine), √(1−|b|)
  (square) or 1−|b| (linear), the near side stays 1, so balance never
  boosts and 0 changes nothing. The pan law is setup-wide and live; mono
  sources pan too.
- 2026-10-11: Monitoring (4.13b): each audio source's runner passes its
  chunks (after gain, mute, balance) to a per-session `Monitor`, not the
  mix (which runs 200 ms behind). One buffer per source, summed when the
  device pulls; a source starts once it holds its largest chunk, the
  device buffer and 5 ms (each underrun adds 5 ms, up to 100 ms), and
  audio piling up 50 ms beyond is dropped. macOS: the default-output unit
  for `default` (follows the system output), the HAL output unit for a
  named device, via `objc2-audio-toolbox`; outputs listed via
  `objc2-core-audio`. The device opens at start if anything is monitored
  (or when a source is first monitored live) and stays open until the
  session ends. Mute silences monitoring too, as in OBS.
  `ISOCAST_MONITOR_SILENT=1` plays silence while metering, for checks on
  a shared machine. Monitoring only works while recording.

- 2026-10-11: Item controls (4.12). `fit` uses CSS object-fit names for
  OBS's bounding box types; the rect always clips (OBS's "scale to outer
  bounds" without "crop to bounding box" overflows its box; Isocast's
  doesn't). Picture offsets round to whole item pixels so native-size
  pictures stay sharp, and inner edges are antialiased per axis so
  whole-pixel edges stay hard. `scale_filter` defaults to bilinear (OBS's
  "disable"); item kernels sample half a texel inside the crop, unlike the
  Scaling filter, which extends edges. `blend` blends encoded values
  (OBS's "sRGB off"); subtract, multiply and darken keep the target's
  alpha, so they draw nothing on a nested scene's transparent canvas;
  darken treats a faded colour as over white and lighten as over black.
  All 21 pipelines (3 shaders × 7 modes) are made up front (making them on
  first use dropped 4 frames at the start of a cold run). An item with a
  source and a background now draws the background behind the picture's
  transparent parts, and crops no longer bleed half a texel.
- 2026-10-11: Parallel agents. Independent phase 4 tasks run in their own
  git worktrees (`.claude/worktrees/NAME`, branch `agent/NAME`, excluded
  locally), each building into "/Volumes/Extreme SSD/isocast-wt/NAME" and
  testing with its own unsigned daemon on `ISOCAST_SOCKET` and synthetic
  sources only; the orchestrator merges, applies PLAN/notes text, and runs
  device checks with the signed app.

- 2026-10-11: Filtered ISO files (4.9f) get a GPU stage of their own per
  source (its own wgpu device, importer and frame pool), run on the
  source's frame clock, so they work whether or not the program shows
  the source. Transparency becomes black (the files have no alpha). The
  file's size is the chain's output size at the start; if a live filter
  change alters it, frames are scaled to the file's size.

- 2026-10-11: Render Delay (4.9e) keeps GPU copies of the picture at its
  place in the chain, one per program frame (stamped with the frame's
  time, so a fade drawing a source twice keeps one), and shows the newest
  at least the delay old. Capture frames can't be held instead: their
  pools are small and holding them would starve the capture. Memory is
  delay × fps copies at the source's size (a 4K camera at 30 fps for
  500 ms: about 500 MB), so the delay is capped at 500 ms like OBS.
  Snapshots skip delays.

- 2026-10-11: Sharpen, Image Mask/Blend, Apply LUT (4.9d), ours:
  sharpen adds 2·sharpness times the 4-neighbour Laplacian (premultiplied,
  alpha untouched). A colour mask reads the channels the filter's colour
  names (white: their mean) times the image's alpha; opacity is the
  mask's strength. Unless stretched, a mask image covers the picture
  keeping its shape (centred, overflow cut), so no part is left unmasked.
  LUTs become 3D Rgba8 textures sampled trilinearly by the GPU; .png LUTs
  are tiles of red across and green down, one tile per blue level
  (512×512 = 64 levels, or a 1024×32 strip). A missing or bad file skips
  its filter (logged once) rather than stopping a recording.

- 2026-10-11: Geometry filters (4.9c). A chain now keeps one canvas per
  pass (sizes can change), and filters get the frame's time; moving ones
  count from when the source's filters first ran (the recording's first
  frame, or the first snapshot). Scaling's undistort maps output x in
  −1..1 to input x = s·x + (1−s)·x³, where s is the squeeze that keeps the
  middle's proportions, so the edges take the squeeze. Aspect changes keep
  the height. Bicubic is Catmull-Rom, Lanczos has three lobes, and area
  averages the covered texels (bilinear when enlarging). Crop/pad and
  point scaling read texels exactly, so they don't blur.

- 2026-10-11: Keys (4.9b), our formulas: Chroma Key measures a pixel's
  distance from the key colour in BT.709 Cb/Cr (each ±0.5), with
  similarity and smoothness divided by 1000; under that, OBS's defaults
  (400, 80) remove a dull real-world green screen (about 0.37 from pure
  green) and keep skin and greys (about 0.6). Spill removes only the key's
  direction of chroma, fading out up to spill/1000 beyond the edge. Color
  Key uses RGB distance (similarity 80, smoothness 50 by default; the OBS
  knowledge base lists 400/80, which looks copied from Chroma Key). Luma
  Key's minimum defaults to 0 (the knowledge base says 1.0, which would
  remove everything). Keyed from the original colours; the contrast,
  brightness and gamma adjustments apply afterwards.

- 2026-10-11: Video filters (4.9) run per source on the GPU: the frame is
  drawn onto a canvas of its own size (premultiplied RGBA) and each
  enabled filter is one full-screen pass to a second canvas (ping-pong),
  shown in place of the frame. They run where the source is shown, not
  in ISO files (4.9f adds that as an option). Filter settings, unlike
  the rest of the setup, reach a running recording at once (like gain
  and mute), since filters are tuned while watching. Color Correction's
  formulas are ours (OBS publishes only names and ranges): gamma g raises
  to 1/(1+g) for g ≥ 0 or 1−g below; contrast c scales about 0.5 by 1+c,
  or 1/(1−c) below 0; brightness adds; saturation s mixes from Rec. 709
  luma by 1+s; hue rotates about the grey axis; then multiply, add, and
  opacity on alpha. Applied in that order to encoded (not linear) values.

- 2026-10-11: Nested scenes (4.8) are items with `scene = "id"` rather
  than a `scene` source kind: items already address what they show, and a
  separate field keeps scene and source ids from colliding. Each nested
  scene is drawn first (deepest first, each once per frame) on a
  transparent canvas-sized texture kept between frames, which the item
  then draws like a source (a premultiplied-alpha pixel kind), so its
  style applies to the scene as a whole, as in OBS. Recording a nested
  scene as its own ISO file is not supported yet.

- 2026-10-11: Browser page options (4.7c) follow the program's on-air
  sources (the timeline map media already uses): a watcher thread per
  browser source with `shutdown_when_hidden` or `refresh_when_shown` polls
  it every 20 ms and calls the platform's `BrowserControl`. It waits until
  the program has published once, or the first look (nothing on air yet)
  would unload or reload every page at the start. Refreshing is only
  possible while recording, since pages only run then.

- 2026-10-11: Browser source audio (4.7b) is captured inside the page, not
  from WebKit's GPU process: a user script (`browser/audio.js`) redirects
  connections to any AudioContext's speakers, and media elements as they
  start playing, into one capture context at 48 kHz, whose
  ScriptProcessor posts base64 stereo floats to a `WKScriptMessageHandler`.
  This keeps each source's audio separate and silent locally, which a
  process tap couldn't (all pages share one GPU process). Chunks arrive in
  bursts, so they are stamped back to back and resynced only after 150 ms
  of drift. Known gaps: audio in iframes, and cross-origin media without
  CORS (Web Audio would silence it, so it is left playing aloud);
  ScriptProcessor runs on the page's main thread, so a page that blocks it
  for long can glitch. Media autoplays (no click needed) and local pages
  get `allowFileAccessFromFileURLs` so `fetch()` of neighbouring files
  works, as OBS's local file mode allows.

- 2026-10-11: Browser sources (4.7a) use WebKit, the engine macOS ships,
  not CEF (OBS's): no 200 MB framework to bundle or sign. Each page lives in
  an offscreen borderless window on the main thread and is snapshotted with
  `takeSnapshotWithConfiguration` at the source's fps (a tick is skipped
  while one is in flight or the page is loading), drawn into a BGRA pool
  frame and un-premultiplied. Pages get the light appearance so a
  recording doesn't depend on the Mac's dark mode. Snapshots of a fresh
  browser source wait for the page to load (up to 8 s).

- 2026-10-11: "On air" (4.5b) means shown by the program: the compositor
  publishes, at each tick, the video sources of the scene on air (and of
  the scene it fades from) into the session `Timeline`, with when each
  came on air. Without a program everything counts as on air from the
  start. Media restarts from that moment, which both its video and audio
  generators read, so they restart in step; media without video always
  plays.
- 2026-10-11: Text (4.4) is rasterised on the CPU with cosmic-text into a
  picture (one shared FontSystem, created on first use, default sans
  serif set to Helvetica Neue) and drawn like an image, so it gets every
  item style and transform; outlines are the text drawn around two rings,
  shadows an offset copy. Text from a file is polled every 500 ms and
  keeps the size of its first render unless width/height are set.
- 2026-10-11: Image decoding (4.3) is portable Rust in the engine (`image`
  for PNG/JPEG/GIF/WebP/BMP/TGA, `resvg` for SVG), not a platform API.
  Still pictures push one frame that the frame clock repeats; GIFs and
  slide shows are generator threads on the timeline. Relative file paths
  in a setup file are made absolute against its folder when the CLI loads
  it (`Setup::resolve_paths`); setups sent as JSON over the API must use
  absolute or `~/` paths. ISO files of sources with transparency show
  transparent areas as black (the encoder ignores alpha).
- 2026-10-11: Ryan wants Isocast to match OBS as closely as possible for
  first-time users, not just his rig: every standard OBS source, effect
  and audio filter, transition and item control on the Mac, each source
  and filter as its own small module. Mapped in `docs/obs-parity.md` and
  queued as phase 4 (OBS parity), before scenes as code (now phase 5). ISO
  files stay unfiltered by default (`recording.iso.filtered` opts in); the
  program is filtered.
- 2026-10-11: Apple silicon only (aarch64-apple-darwin). The macOS crate
  refuses to compile for Intel; release builds are arm64.
- 2026-10-11: Ryan approved small unit tests (`#[cfg(test)]` modules next
  to the code) for pure logic that is fiddly to cover with real runs: the
  id-keyed merge patch and diff, `Time` tick math, audio track trimming and
  padding, the mixer's input continuity, colours and corner radii. Real
  runs stay the main verification. `cargo test` must pass before commits.
- 2026-10-11: Transitions (3.5). A switch records (time, scene,
  transition) on the session's scene timeline; at each canvas tick the
  compositor asks what is on air and, during a fade, draws the previous
  and next scenes into two scratch canvases and blends the next over the
  previous by progress (linear in code values). A switch during a fade
  fades from the previous switch's target. The default transition is a
  cut (no `[transition]` in the setup), matching how the rig recorded
  before. `take` swaps preview and program, as OBS's studio mode does.
- 2026-10-11: Program recording (3.3). The compositor is one more source
  runner on the session frame clock: at each canvas tick it takes the
  newest frame of each visible item's source at or before tick + ½ frame
  (peeking, so ISO frame clocks are untouched; repeating a source's last
  frame if none is new), draws into a BGRA IOSurface buffer from a pool,
  waits for the GPU, and sends it to the program file like any frame. The
  encoder converts BGRA to 4:2:0; a final 420v pass on the GPU is the next
  optimisation if encoders get tight. All scenes' sources open with the
  session so switches never wait. `program` is a reserved source id (it
  names the program file in manifests).
- 2026-10-11: The mixer (3.4) is a runner fed by its sources' audio like a
  file is; it keeps each input continuous (2 ms jitter, gaps as silence),
  mixes 10 ms blocks 200 ms behind real time so late chunks still count,
  and sends the stereo mix to the program file's single track (source
  "program" in the manifest). Gain and mute live in SessionControl and
  change at once; offsets shift a source's timestamps for every file and
  apply from the next recording. Meters are peak/RMS per status interval,
  before gain.
- 2026-10-11: Ryan asked for the full scene language (styles, text,
  images, layout, keyframes, TSX, web source, shader transitions, external
  APIs like Twitch chat, plugin nodes). It is phase 4 in the plan, after
  phase 3, so the program recording and switching that replace OBS come
  first and the compositor that draws scenes already runs at the canvas
  rate. The old phases 4–7 became 5–8. Live data re-renders components on
  change (React-style, merged by id), never per frame.
- 2026-10-11: The renderer is wgpu 30.0.1 (pinned; wgpu-hal 29+ shares
  objc2 0.6/objc2-metal 0.3 with our bindings). The engine owns the GPU and
  the drawing (`gpu.rs`, `render.rs`); platforms implement
  `Platform::frame_importer`, which turns their frames into wgpu textures
  (macOS: CVMetalTextureCache → `hal::metal::Device::texture_from_raw` →
  `create_texture_from_hal`, the texture keeping the frame alive). Y'CbCr is
  converted in the shader from the frame's matrix attachment (BT.709 if
  untagged), range and bit depth. PNGs are read back with
  `copy_texture_to_buffer` and the `png` crate; they cross the socket as
  base64 (`base64` crate), which MCP images need anyway. MCP got
  `preview_frame` now (part of 3.6), since every CLI capability needs an
  MCP counterpart.
- 2026-10-11: Privacy permissions belong to Isocast itself. `isocastd` has
  an embedded Info.plist (camera/mic usage strings), is packaged by
  `scripts/sign-dev.sh` as a background-only app bundle on the internal
  disk, signed with Ryan's "Apple Development" identity under
  `dev.isocast.isocastd`, and launched by the CLI with posix_spawn plus
  `responsibility_spawnattrs_setdisclaim`, so macOS treats it as its own
  responsible process. Grants then don't depend on which terminal or agent
  started it, and survive rebuilds (verified: rebuild, re-sign, restart, all
  three still granted). `isocast devices`, `doctor` and `permissions` ask
  the daemon. Release builds (7.2) will use the real app bundle and a
  Developer ID with hardened runtime and entitlements.
- 2026-10-11: Takes. A split is scheduled at the next whole second of the
  session clock at least 0.5 s ahead (whole seconds lie on every integer-fps
  file's frame grid, since files start at the session epoch). The main loop
  creates the next take's writers, then each writer thread finishes its file
  exactly at that time and switches to the new one; audio before the split
  goes to the old file and the new file trims what came before its first
  frame. Each take has its own files (named for its start time) and
  manifest; markers belong to the take whose span holds them, and each take
  starts with the scene on air. The report lists every take's files and
  manifests. `split`, not `take`: "take" is reserved for Preview → Program
  (3.5).
- 2026-10-11: MCP. `isocast mcp` (rmcp 3.5, stdio, a current-thread tokio
  runtime) is a thin client: each tool makes one daemon call in
  `spawn_blocking`, launching `isocastd` if needed. Tools return
  `Json<T>` (structured content plus output schema); daemon errors become
  tool errors (`isError`) with the message, so the model sees why. To use it
  from Claude Code: `claude mcp add isocast -- <repo>/target/debug/isocast mcp`.
- 2026-10-11: Markers and scenes. The daemon keeps the scene on air (first
  scene by default, reset when an apply removes it). A recording starts
  with that scene at 0 s; switches and markers go to the engine through
  `SessionControl` (host-clock times) and into the manifest's `scenes` and
  `markers` right away. Until the compositor (3.x), a scene switch is only
  a timeline entry: ISO files are unaffected, and FilmCraft can cut from it.
- 2026-10-11: Live state. The daemon keeps one live setup and a revision
  (0 = none; +1 per apply), in memory only for now. A change is a whole
  setup or a JSON merge patch in which lists of items with ids (sources and
  scenes by `id`, scene items by `source`) merge by id and `{"id": "x",
  "remove": true}` removes one; other lists are replaced. Diffs name items
  by id (`scenes.desktop.items.facecam.visible`). `apply` needs the revision
  the change was planned against and refuses changes that leave problems. A
  running recording keeps its setup; changes apply to the next one until
  live switching (3.5). `watch` turns a connection into JSON-RPC
  notifications: setup_changed, started, status (1/s), warning, stopped.
- 2026-10-11: Daemon. `isocastd` speaks JSON-RPC 2.0, one message per line,
  on a Unix socket: `$ISOCAST_SOCKET`, else `~/Library/Application
  Support/Isocast/isocastd.sock`, with `isocastd.log` beside it. Plain std
  threads, no async runtime. The method table and report types are in
  `isocast_protocol::api`. `isocast start` launches `isocastd` (next to the
  `isocast` binary, own process group so Ctrl-C in the terminal doesn't stop
  a recording) when nothing listens. Launched from a terminal, it inherits
  the terminal's privacy permissions. Its main thread runs jobs for others
  (`run_main_queue`), because AppKit only answers on the main thread;
  `dispatch_main` doesn't work for that (blocks then run on other threads).
  SIGINT/TERM/HUP stop the recording cleanly before exiting. `isocast record`
  still runs the engine in-process for scripts.
- 2026-10-11: `encoder.bit_depth` 10 means HEVC Main 10 (VideoToolbox's
  HEVC_Main10_AutoLevel) and 10-bit 4:2:0 display capture ('x420'); cameras
  stay at their native depth. ProRes is always 10-bit 4:2:2; H.264 is 8-bit
  only (Apple's encoders have no High 10). All files are tagged Rec. 709.
  ProRes 422 HQ of the 4K60 screen runs ~1.5 Gb/s (~11 GB/minute).
- 2026-10-11: Disk speed. Before creating files, the engine writes and
  fsyncs a 64 MB incompressible probe file in the recording folder: slower
  than the estimated data rate refuses to start, slower than 2× warns. It
  measures a burst, not sustained speed. Network volumes (no `MNT_LOCAL`,
  like the `/Volumes/obs-recording` SMB share) get a warning. The report
  includes the volume, free space, measured speed and estimate.
- 2026-10-11: Disk space. A recording won't start unless the volume has
  room for its estimated size (planned length, or 10 minutes if
  open-ended) plus `recording.keep_free_gb` (default 1). The estimate is
  deliberately generous: HEVC at ~0.12 bits/pixel for quality 70, ProRes
  at Apple's published rates, 128 kbps per audio channel. While recording,
  free space is checked every second and the session stops cleanly (files
  finished, manifest `failed` with the reason) when it drops below
  `keep_free_gb`. Open-ended takes warn if less than an hour fits.
- 2026-10-11: Files are written as QuickTime movie fragments every 2 s
  (`movieFragmentInterval`), so a crash or `kill -9` loses at most ~2 s;
  a normal finish rewrites the file into a standard indexed movie
  (`ftyp wide mdat moov`). Apple suggests ≥10 s fragments for best speed on
  external drives; revisit if 1.8's disk tests show drops.
- 2026-10-11: Session manifest. Each session writes
  `<date> <time>[ N].session.json` next to its files (Ryan's folder is flat
  and shared, so a fixed `session.json` would be overwritten). Types are in
  `isocast_protocol::manifest`: per file the source, kind, size, fps, audio
  tracks and `start_offset_seconds` on the session timeline, plus the
  session's wall-clock start and host time. It is written atomically
  (`.partial` then rename) at the start (status `recording`), again once
  every file has started (offsets known), and at the end (`complete` or
  `failed`), so a crash leaves a usable manifest.
- 2026-10-11: Audio capture. Microphones use an AVCaptureSession audio data
  output asked for 48 kHz interleaved f32 (AVFoundation resamples), up to 2
  channels (multi-channel interfaces: first two inputs for now; until the
  #99 default entry this was in fact macOS's mix of every input), matched like
  cameras, with "default" meaning the system default input. System audio is
  a ScreenCaptureKit stream on the main display with `capturesAudio` and a
  2×2, 1 fps video stream that is ignored; 48 kHz stereo planar float. Each
  capture feeds a runner thread that forwards chunks to the files; the
  session also waits (≤3 s) for audio captures to start.
- 2026-10-11: Cameras (AVCaptureSession + video data output, 4:2:0 video
  range) pick, unless the setup gives width/height/fps: the highest frame
  rate up to the canvas fps, then the largest size, then a 4:2:0 pixel
  format. So a card offering 1080p60 and 4K30 records 1080p60. Matching a
  `device`: id, exact name, name ignoring case, then the shortest name
  containing it. iPhone/iPad screens are enabled with the CoreMediaIO
  `AllowScreenCaptureDevices` property and listed as `device_screen`
  ("muxed" AVFoundation devices). A session waits up to 3 s for every
  capture's first frame before its clock starts, so files start together.
- 2026-10-11: Captured video goes through a frame clock: the engine emits
  one frame per tick of the session clock (canvas fps), taking the newest
  frame captured no later than half a frame after the tick and waiting a
  full frame for it to arrive. A still picture repeats the last frame, since
  ScreenCaptureKit only delivers frames when something changes. Files are
  constant frame rate with exact timestamps; the compositor will reuse it.
- 2026-10-11: Displays are captured at the panel's native resolution (the
  IOKit native-mode flag), not the scaled backing size: 3840×2160 here, not
  5120×2880. ScreenCaptureKit delivers 4:2:0 video range converted to
  Rec. 709, shows the cursor, and uses its maximum queue depth (8), because
  the frame clock and encoder each hold a few of its buffers. Displays have
  no fps of their own; they follow `canvas.fps`.
- 2026-10-11: `isocast devices` never triggers a permission prompt: it
  checks `CGPreflightScreenCaptureAccess` and AVFoundation authorization
  status, lists windows (via ScreenCaptureKit) only when screen recording is
  granted, and reports the permissions and what's missing in `notes`. The
  device types live in `isocast_protocol::devices` so MCP returns the same
  JSON.
- 2026-10-11: Apple bindings are the `objc2-*` framework crates (0.3.x) for
  everything: AVFoundation, CoreMedia, CoreVideo, and later ScreenCaptureKit
  and VideoToolbox. One family means captured `CMSampleBuffer`s and
  `CVPixelBuffer`s go straight to AVAssetWriter with no conversion between
  different crates' wrapper types (the `screencapturekit` crate has its own).
  Features are trimmed per crate to keep builds light.
- 2026-10-11: Encoding goes through AVAssetWriter output settings, which drive
  VideoToolbox on the media engines. HEVC/H.264 use constant quality
  (`AVVideoQualityKey` = quality/100), keyframes at least every 1 s, and
  Rec. 709 colour tags. ProRes gets no compression properties. Move to a raw
  `VTCompressionSession` only if a task needs control AVAssetWriter lacks.
- 2026-10-11: Timing. The master clock is the host clock; engine `Time` is
  nanoseconds on it. Each file starts at its first video frame (so there's no
  empty edit at the head) and ends on the first frame boundary after the
  session end. Frame and sample times are passed to AVAssetWriter as exact
  tick counts from the file start (video timescale 60000, or fps×1000 if 60000
  doesn't divide; audio 48000), so frame durations are exactly even.
- 2026-10-11: Audio inside the engine is 48 kHz interleaved f32. Each audio
  source is its own AAC track (128 kbps per channel). Tracks are kept
  continuous: gaps over 10 ms are filled with silence, overlaps and audio
  from before the first frame are trimmed, and every track is padded to the
  file's end.
- 2026-10-11: Synthetic sources pulse (100 ms flash and beep) at every whole
  second of the session clock, so files can be checked against each other.
- 2026-10-11: `isocast record [SETUP] [--source SPEC]... [--seconds N]
  [--folder DIR] [--json]`. Quick `--source` specs build a setup via
  `Setup::quick` in the protocol crate, so MCP can reuse them. Program
  recording is skipped with a warning until the compositor exists. Files are
  never overwritten: if any name is taken, every file in the session gets the
  same " 2", " 3"… suffix.
- 2026-10-11: Name "Isocast" is a working name; renaming is cheap until the
  first public release.
- 2026-10-11: Licence MIT OR Apache-2.0. Clean-room with respect to OBS.
- 2026-10-11: Mac recording path is Apple-native: ScreenCaptureKit (screens,
  system audio), AVFoundation (cameras, capture cards, Continuity Camera),
  VideoToolbox (HEVC/H.264/ProRes), AVAssetWriter (.mov/.mp4). FFmpeg or
  GStreamer only later, for streaming protocols.
- 2026-10-11: Setups are TOML for people, JSON for the API, one set of serde
  types in `isocast-protocol`. Sources and scenes are addressed by ids.
- 2026-10-11: The MCP server will use `rmcp` (official Rust MCP SDK). Tools
  are intent-level (plan, apply, start, stop, switch scene, preview frame),
  not one tool per API call.

- 2026-10-15 (#75, small-fixes): an ISO audio source with no sound (a media
  file without an audio track) is skipped with a take-report warning naming
  the field, not refused by validation: a media source may or may not have
  audio, which is only known once its file is open. The skip is in
  plan_take (record/mod.rs), the warning beside the other start warnings
  (engine.rs); the writer is untouched.
- 2026-10-15 (#74, small-fixes): obs-websocket without a password allows
  only loopback binds (`WebsocketSettings::problem`, field `no_password`;
  stored settings that break it keep the server off) and refuses upgrades
  with an Origin header (403); at most 32 clients (503 beyond); Sleep
  outside a batch answers 206 like OBS.
- 2026-10-15 (#69, small-fixes): the camera extension's path fallback
  accepts only `/Applications/<one name>.app/Contents/Helpers/Isocast
  Engine.app/Contents/MacOS/isocastd` and `/Applications/Isocast Camera
  Host.app/Contents/MacOS/isocastd` (`producerPathAllowed`).
- 2026-10-15 (#49, small-fixes): `isocast mcp` checks for an older engine
  once, on its first tool call, and warns on stderr; the app's
  couldn't-restart warning stays until dismissed (OK).
- 2026-10-17 (#107, layout-audit): `scripts/layout-audit.py` renders 54
  screens × 8 sizes + a 900→2700 width sweep (482 pictures, ~2 min, each
  with its own throwaway engine; no `preview intro`, so "main" isn't
  Studio Mode) and has one Claude Haiku 5.5 checker per picture (12 at
  once, ~5 min, ≈ $0.0006 each) answer through a validated
  `deliver_layout` tool, isolated like the visual judge
  (scripts/layout-audit/checker). Pictures are downscaled to 1568 px on
  the long side for the checker; its boxes are in those pixels and the
  report scales them back. Without OPENCODE_API_KEY it uses this Mac's
  OpenCode data folder: the console sign-in lives in OpenCode's database,
  and a fresh data folder answers "Model unavailable". Small models
  report false positives (content vs. the screen's description, menus
  over controls, below-minimum sizes): triage before filing. The sweep
  starts at 900 because the window's minimum width is 900.

- 2026-10-18: the frame oracle is strict unconditionally. #126 (media
  pacing, 7df7983b) is merged, so `--tolerate-issue-126`, its "late by
  #126" rule (`issue_126`, `late_by_126`), the `previous`-frame plumbing
  behind it and the "KNOWN ISSUE #126" marking are gone from
  scripts/e2e.py, merge-branch.sh and agent-rules.md. Any oracle mismatch
  fails with a contact sheet; don't rerun to green, read the sheet.
- 2026-10-18 (#124 follow-up, Windows engine scenarios): **e2e.py drives
  isocastd on Windows over its named pipe.** `windows_pipe()` derives the
  pipe name exactly as isocast-ipc does (sha256 of "isocast.local-pipe\0v2\0",
  the user's binary SID and the invariant-uppercased path whose parent is
  canonicalised with the \\?\ prefix, length-delimited); the engine runs
  unsigned from its build, in the scenario's folder; workers are killed by
  their own process tree (taskkill /T on the worker PID). Windows can't play
  media files yet, so the block-code fixtures can't be sources there: the
  win_record/win_takes/win_scenes scenarios apply the same model (T0 + slot,
  one T0 per take) to the engine's test pattern, whose bars name the frame
  (mod 4 s) and whose label tells bars from bars2; fades are checked by the
  label cell's share, slides by the bars row against the model's offsets.
  What Windows lacks (previews: masks B5; streaming: the canvases' warm-once
  check and the replay buffer; media files: masks B1 and the block-code
  scenarios) raises Pending: reported PENDING, never PASS.
  `scripts/oracle-windows.sh --engine [REF]` builds REF natively in our own
  lane root `C:\isocast-dev\lanes\oracle` with REF's windows-remote.sh and
  runs the scenarios through its `run` under the shared GPU lock.

## Gotchas

- 2026-10-22 (R1.d, Windows 10 22H2 DNS-SD on exon-desk; revised after
  review): what the watch works around, all seen in real runs:
  - A new `DnsServiceBrowse` is first answered from the DNS Client cache
    (all at once, within ms), which still held stopped engines' PTR/SRV
    records with ~4,000 s TTL after their goodbyes (and 300 flood
    instances an hour later). Network answers come ~1 s later. So browse
    answers are hints: an engine is reported once a `DnsServiceResolve`
    answers, rechecked every 30 s (which is also how TXT changes arrive,
    within ~30 s), lost after 2 failed resolves or a goodbye.
  - A running browse reports goodbyes (PTR TTL 0) but NOT engines that
    start after it: the watch restarts its browse every 10 s (known
    engines are kept), so a new engine appears within ~10–16 s; a one-shot
    `isocast engines --discover` finds it within its 5 s.
  - Resolves for stale cache instances never call back at all, and with 64
    running at once none completed (not even live ones). So at most 4 run
    at once (5 s timeout, backoff 2–30 s), ranked known engines first,
    then instances *heard* (an answer >300 ms after the browse started,
    i.e. from the network), then TTL; a heard engine pre-empts a
    cache-only resolve and can evict a cache-only instance when all 64
    slots are full. With ~300 stale instances cached, a live engine was
    found in 5 of 6 four-second one-shots.
  - Resolve gives at most one IPv4 and one IPv6 address, sometimes none;
    A/AAAA records from browse answers fill in, kept until their TTL
    (cap 4500 s), pruned, 64 hosts max (link-local IPv6 dropped).
  - Callbacks run on threadpool threads, including after a cancel
    (status 1223), so contexts are tokens in a global table. Microsoft
    documents only that a cancel handle must live until cancelled, so
    each request, its strings and its cancel handle live in the token's
    route until the final callback has arrived and the owner let go.
  - One long-lived cleanup worker per process owns every registration
    Windows hasn't finished with: one that missed its 5 s confirmation
    (cancelled; a success that still arrives is withdrawn) and one whose
    withdrawal wasn't confirmed within 3 s of `Drop`. It keeps their
    memory until the completion arrives, however long; registering fails
    before `DnsServiceRegister` if it can't start. Forced with a 300 ms
    scratch timeout and no cancel: the late success was withdrawn 0.55 s
    later and the Mac saw it lost 1.2 s after it appeared.
  - Outstanding operations are budgeted (256 unfinished, 64 abandoned
    awaiting their final callback); past it nothing new starts and the
    watch reports `Event::Error("discovery is paused: …")`.
  - Killing the advertising process (Stop-Process -Force) still sends the
    goodbye; Windows renames a conflict to `name(1)`, macOS to `name (2)`.
- 2026-10-22 (R1.d, macOS): `DNSServiceProcessResult` doesn't handle one
  reply: it loops while the socket has bytes (`do { … } while
  (morebytes)` in dnssd_clientstub.c), and a dispatch queue's handler runs
  the same loop, so neither bounds a flood. The supported exit is
  `DNSServiceRefDeallocate` from inside a callback (it clears `morebytes`;
  the call then returns without touching the ref). Every watch callback
  checks the stop flag, a per-operation slice (10 ms, 50 ms per loop) and
  a 512-reply queue first and, past any, ends its own operation, which the
  browser restarts (a restarted browse sweeps instances it no longer
  announces once settled). Resolves and lookups are served before the
  browse. Replies carry their operation's generation, so an old host's
  queued addresses never attach to a new one. Under a sustained flood
  (600 instances, 200 re-registered every 0.1 s, ~400 browse replies/s)
  a watch exited 59–204 ms after Ctrl-C and 2 s one-shots returned in
  2.10–2.19 s; while the flood starts, a watch may report nothing for
  several seconds.
- 2026-10-22 (R1.d, both OSes): a flood (300 instances from exonbook, 50
  re-registered every 0.1 s) can hide engines past the 64 tracked, but
  browse deadlines and shutdown hold: macOS one-shots returned in
  3.13–3.22 s for `--timeout 3`, a flooded watch exited 114–169 ms after
  Ctrl-C; Windows one-shots in 3.1–3.4 s (10.1 s for 10). macOS bounds
  reading per loop (64 `DNSServiceProcessResult` per socket, 50 ms, a
  512-reply queue drained at half) and Windows applies 64 messages per
  loop from a 1,024-message queue, checking for stop between each.
- 2026-10-22 (R1.d): exon-desk's LAN adapter ("Ethernet 4") is on the
  **Public** network profile; Tailscale is Private. mDNS works anyway (the
  built-in "mDNS (UDP-In)" rule for svchost/dnscache allows all profiles,
  and Isocast needs no 5353 rule), but the "Isocast dev" TCP 47470 rule is
  Private-only, so LAN connections to an engine there will be blocked until
  the profile or the rule changes (transport lane / Ryan).

- 2026-10-18 (#64): the media-source A/V check (two burst tracks mixed in
  one program, burst length ~100 ms in step, ~144 ms if one is 44 ms late)
  is only valid when both sources open together: changing one source's
  `files` with `apply` restarts only that source, its loop phase moves, and
  the bursts stop overlapping (peak -6.9 dB instead of ~-1 dB), which reads
  as "in step" whatever the decoder does. followups-007's t64.sh second pair
  was measured that way. Start a fresh engine per pair and check the peak
  (followups-008/scratch/t64.sh).

- 2026-10-18 (#110 pins): live file counters can lag pin time by a full
  status interval. A post-pin RPC comparison counted six old square skips as
  new losses. Retained final-file PTS proved last gap20.200s, pin request
 20.190s, then139 continuous30fps pictures; no new engine change/rerun needed.
  The earlier real bug was `create_writers` flattening every extra canvas to
  Vertical, now fixed. Persistence uses library documents, not a guaranteed
  legacy setup.toml: verify through get/apply/restart instead of copying it.

- 2026-10-18 (#110 experiments): `e2e.Engine` re-signs its copied daemon in
  a bundle, so its whole-file SHA differs from the source binary. Preserve
  original bytes before A/B and assert those hashes; don't silently waive a
  failed checksum. One weak-soak job refused before workload startup because
  `.hw-job` was absent after rapidly handing the Mac lock between queued
  jobs; the serial retry passed. The marker's check/remove is not atomic
  (`release_builds`), but no orchestrator change was made in this lane. TERM
  must raise KeyboardInterrupt in the weak wrapper: InterruptedError is an
  OSError, which a best-effort ps reader can swallow and incorrectly PASS.
- 2026-10-18 (#105, native recording probe): valid, visible own windows in
  Session1 with WS_EX_NOACTIVATE gave WGC CreateForWindow E_INVALIDARG.
  Clearing ONLY that extended style in-run made both capturable; keep
  ShowWindow/SetWindowPos NOACTIVATE and WM_MOUSEACTIVATE refusal instead.
  No user's window style changed. The generic native error mentioning a
  service session was not the cause here; Session1 was independently logged.
- 2026-10-18 (#105, measured4K pressure): three full-size4K NVENC outputs
  finish valid300-frame files but5s media takes10.155s wall; p99lateness4.759s.
  CPU3.385% at that throughput is NOT a60fps win. Two4K outputs twice refuse
  ISO tick23; diagnostic shows8pending,15completed,24program/23ISO accepted
  in484ms including100ms preroll. Compose totals5.27ms, append totals12.51/
  10.22ms (max1.75/2.38ms), p99deadline not yet missed. The native encode
  pipeline's completions fall behind; exact GPU stage/hardware limit still
  needs capture-lane tracing. Do not enlarge the queue or retry falsely to
  claimzero drops. Both aborted runs retain full-decodable24/23frame prefixes.
  Three4K diagnostic trials done: stop speculative changes and coordinate
  native throughput diagnosis rather than changing quality or timestamps.
- 2026-10-18 (#105, verifier): file counts alone falsely suggested the4K
  probe was real-time. Added explicit wall<=5.35s for5s media andp99lateness
  <=oneframe. Probe's original last3ms/tick busy-yield also consumed CPU;
  removed before1080/4K timing.1080single sample4.293%onecore,154.14MiB RSS,
  519.09MiB private, all900frames in5.096s withnewsource ticks300/300. These
  are preliminary stand-in costs, not matched OBS/full-engine target passes:
  OBS's normal recorder still runs AAC even when silent, whereas our proof
  is video-only. Native AAC is needed before a fair audio-matched comparison.

- 2026-10-18 (#105 W.4a): Windows empty cancellation cannot simply add
  DELETE to live media handles: that would reject ordinary live readers.
  Keep original sharing unchanged, verify only the owned initial header,
  capture full file ID/volume/creation time, close and reopen the same child
  under the SAME pinned directory lease, revalidate, then use handle-only
  disposition. A swapped/moved/linked/nonempty file or deny-delete reader is
  retained with an error. Native probes passed empty remove/recreate, live
  reading, hard-link/reader refusal, nonempty continued finish and junction
  rebinding. Active name-swap race, reparse-refusal matrix and non-NTFS
  filesystems are not yet qualified.
- 2026-10-18 (#105 W.4a): do not identify an owned disk probe merely by
  `.isocast-disk-probe` name and size<=4KiB: it could be valuable existing
  content. The new Windows write-latency implementation was removed before
  integration for this reason. System queries now make no writes. The engine
  DiskProbe currently removes that pathname even when the hook returns None;
  establish Windows probe ownership/cleanup before native Platform selection.
  Native system proof: volume's NTFS free space exactly matched Win32;
  same-volume junction queries and process/thread CPU counters passed;
  64 MMCSS thread-local registration/destruction cycles held handles85→85.

- 2026-10-17 (#105 W.5c3): NVIDIA VideoProcessorBlt's P010 output is NOT
  correctly quantized on RTX5070/617.42: before any MFT, GPU readback gives
  raw16 black4129/white60423/neutralUV32899 with nonzero low6bits; decoded10bit
  white944/UV514 instead940/512. MF10bit now uses explicit GPU Rec709 matrix,
  left-sited chroma filter and quantized q<<6 through P010 plane UAVs. Raw
  4096/60160/32768, low6bits0, actual GPU-ramp decode877 distinct Y64..940/UV512
  verified. NVENC direct RGB10 was already correct. Whole CS bind/dispatch/
  unbind sequence holds ID3D11Multithread: four simultaneous MF colour encoders
  remain exact. No CPU pixels/copies added; eight-bit native VP stays as before.

- 2026-10-17 (#105 W.5c3): RGB10A2 storage is DXGI R10G10B10A2_UNORM and
  NVENC ABGR10 (red occupies low bits), not ARGB10. GPU-generated1024-level
  ramp survives the bounded frame pool and DX12 import exactly. HEVC Main10
  may expand BGRA8 on GPU but cannot invent source precision; this is SDR709,
  not HDR. NVIDIA HEVC MFT supports Main/Main10; no software HEVC MFT is
  installed here. At12 occupied sessions the factory must refuse HEVC clearly,
  never silently choose H.264 or eight-bit; existing sessions survive refusal.

- 2026-10-17 (#105 W.5c3): HEVC hvc1 uses frozen SPS-derived hvcC and strips
  in-band VPS/SPS/PPS; changed complete parameter sets fail closed because
  the writer already has its description BEFORE the first picture. Main/Main10
  IDR sync flags match decoded automatic/forced keys[0,60,90,150]. Current
  EncodedVideo.parameter_sets is H.264-only: keep live constructors H.264 and
  add an engine codec tag/guard before Platform/writer integration. QSV/AMF,
  GPU-less software and HDR remain unqualified. W.5c2 Actions39089312953 failed
  only when compiling isocast-ui with LLVM OOM; portable jobs passed and native
  platform crate compiled. Coordinator owns CI retry/configuration.

- 2026-10-17 (#105 W.5c2): Windows10 MFStartup/MFShutdown around an activated
  encoder retains14 handles per reopen (5Event,2IRTimer,1IoCompletion,
  1TpWorkerFactory,5WaitCompletionPacket). Startup/shutdown without activating
  an MFT does not reproduce it. One process-lifetime MFStartup fixes growth;
  actual16 hardware+16 software frame sessions stayed469/482 handles after
  warm-up. Only the runtime is pinned: each transform shuts down and each
  encoder worker joins. This is distinct from WGC's two ALPC ports per worker.

- 2026-10-17 (#105 W.5c2): NVIDIA H.264 MFT is async/D3D11-aware but does
  not implement AVEncMPVDefaultBPictureCount (IsSupported=E_NOTIMPL,
  SetValue=E_INVALIDARG); AVLowLatencyMode works. Require low latency, set
  zero B pictures when supported, and verify every output PTS/DTS. Microsoft
  H264 Encoder MFT is synchronous software. Both accept live bitrate changes
  but reject live FPS changes on this machine; do not report a rate change
  as successful or silently recreate a recording session. 12 occupied native
  NVENC sessions produce NVENC21 and MF hardwareC00D6D76, then the explicit
  software fallback records normally. Hardware MFT enumeration is adapter-
  matched with MFTEnum2; Intel/AMD hardware remains unqualified.

- 2026-10-17 (#105 W.5c2): D3D11 NV12 staging Map(DO_NOT_WAIT) can transiently
  return STILL_DRAWING after a completed GPU fence. The explicit software
  path retries at1ms under a2s deadline; no hardware pixel readback. Both
  hardware and software red patches decode YUV63/102/240 (709 limited).
  Software1080p60 counter/red runs passed120/120 on Ryzen5700, not a complex-
  scene throughput guarantee; overload must still shed/count honestly.
  MF's NeedInput callback can lag output consumption by a work-queue hop:
  a300-frame low-latency run dropped1 before a bounded2ms credit wait.
  Afterward, three1080p60+one4K60 low-latency MF runs each decoded300/300 with
  zero losses. Final software1080p and NVENC1080p/4K regression runs did too.
  Native handle counts in the final32-open check stayed464/481 after warmup;
  both providers cap cached output at8 and report55 drops from64 overloaded
  submissions, with all9 accepted pictures drained.

- 2026-10-17 (#105 W.5c1): NVENC's D3D11 MapInputResource synchronizes prior
  graphics writes, but forbids client access while mapped (SDK13 header4130).
  Shared Frames also feed compositor/other encoders, so NVENC originally had eight
  private BGRA GPU inputs: one GPU copy, no CPU readback, map until completion
  then unmap before reuse. Source leases remain held through encode completion
  for now; budget/shorten retention before multi-encoder fan-out (32-frame
  source pool). A4K input ring costs~253MiB VRAM; NV12 is a later optimization.
  `hwEncodeStatus` is NOT NVENCSTATUS:2 means frame complete, not an error.
  Readiness is the completion event plus successful LockBitstream. A red patch
  decodes YUV63/103/240, confirming709-limited conversion rather than601 tags.

- 2026-10-17 (#105 W.5c1): RTX5070/driver617.42 admits12 NVENC sessions;
  the13th returnsNVENC21, NOT out-of-memory. Do not hard-code8 or make fallback
  recognize only error10. Existing session remains usable, all12 release and
  another opens. This is admission, not a12×4K60 throughput claim. MF fallback
  and program/ISO priority wiring must precede selecting WindowsPlatform.

- 2026-10-17 (#105 W.5c1): OBS32.1.2 native recording requires an audio
  encoder; disabling sources gives silentAAC, not video-only. First comparison
  matches WGC,CBR24Mbps,P5/ll,single-pass,AQ8spatial-only,709-limited,noB/lookahead.
  Isocast's test helper encodes silent48k stereo160kbpsAAC with FFmpeg; its CPU
  and RSS are included, but it is NOT a native audio backend. OBS settings:
  `obs_nvenc_h264_tex`, `bf=0`, `device=-1` (explicit0 forces texture fallback),
  `opts=enableAQ=1 aqStrength=8 enableTemporalAQ=0 enableLookahead=0 lookaheadDepth=0`.
  OBS logs confirmed WGC and native NVENC. First numbers are one-window direct
  source recording, not the full program+ISO engine or a complex-scene benchmark.

- 2026-10-17 (#105 W.5b): DX12 import must wait on
  `gpu.queue.as_hal::<Dx12>().as_raw()`, NOT `Device::raw_queue()` (that is
  wgpu's separate presentation queue). D3D11 CreateSharedHandle is once per
  texture; the slot owns its unnamed NT handle. OpenSharedHandle maps the
  same GPU allocation into a distinct DX12 resource/private-data store.
  Noncached imports attach an IUnknown frame lease there until wgpu releases
  the resource after completion. Cached imports keep only native texture
  references; the existing engine contract retains frames through GPU work.
  BGRA input imports are read-only and REQUIRE ALLOW_SIMULTANEOUS_ACCESS;
  on RTX5070/617.42 Win10, flags=33, COMMON promotion/decay validates. If
  another driver omits that flag, refuse it; the future alternative is
  explicit release-to-COMMON barriers, never weakening the flag check.

- 2026-10-17 (#105 W.5b): destroying the last MTA after capture invalidates
  windows-rs's process-cached WinRT activation factories on Win10. A real
  reopen crashed in GraphicsCaptureSession::IsSupported (unmapped vtable).
  One process-lifetime CoIncrementMTAUsage reference, matching those cached
  factories' lifetime, fixes the crash (120 real reopen/render cycles).
  Individual capture workers still balance RoInitialize/RoUninitialize.
  Handle-type measurement also found two ALPC ports retained per fresh
  capture worker, with capture only AND with DX12 import, even after3s idle
  and dropping the D3D11 device; this is NOT a leak-free reopen claim.
  Qualify RPC scavenging or reuse long-lived MTA workers before selecting
  WindowsPlatform. GPU cache references are bounded separately at64 surfaces.

- 2026-10-17 (#105 W.5a): WGC `TryGetNextFrame` returns S_OK + null when
  empty; windows-rs 0.62.2 represents this as Error::empty(), not E_POINTER.
  On Win10 build19045, Item.Closed did not fire when the target exited;
  HWND capture checks liveness with an idle wait bounded at250ms. QPC is
  10MHz on exon-desk. Real WGC presentation stamps arrive13–16ms ahead of
  callback time (same QPC domain): preserve them, report signed offsets,
  never call a saturating zero difference measured capture latency. Synthetic
  WinForms windows need explicit DPI awareness: otherwise1920x1080 becomes
  3840x2160 at the desktop's200% scale. WGC's own pre-delivery losses are
  not exposed; our pool/rate/resize counters do not claim to count those.
- 2026-10-18 (#64): the media-source A/V check (two burst tracks mixed in
  one program, burst length ~100 ms in step, ~144 ms if one is 44 ms late)
  is only valid when both sources open together: changing one source's
  `files` with `apply` restarts only that source, its loop phase moves, and
  the bursts stop overlapping (peak -6.9 dB instead of ~-1 dB), which reads
  as "in step" whatever the decoder does. followups-007's t64.sh second pair
  was measured that way. Start a fresh engine per pair and check the peak
  (followups-008/scratch/t64.sh).
- 2026-10-18 (#110 ProRes CPU): HOST_CPU_LOAD_INFO is cached about1s on the
  AWS Macs, so a complete0.6s active cell can report0CPU. Direct test under
  owned workers: host counter0then481/594tickjumps, per-CPU info65–68ticks/0.1s.
  Calibration now uses fresh host_processor_info and frees each Mach buffer;
  zero/reset deltas remain unknown. Live monitor's sampling is unchanged.
- 2026-10-18 (#110 evidence transport): hw-batch's rsync failed after mixed
  run130327 passed, then its unchecked cleanup deleted remoteout anyway.
  Retained profile/movies allowed evidence recovery130735 without a new take.
  Software governor130216 failed during SSH push (never started);130831 passed.
  A separate orchestrator fix should check fetch success before removing
  remote evidence; none was mixed into this recording/model lane.

- 2026-10-18 (#110 pins): live file counters can lag pin time by a full
  status interval. A post-pin RPC comparison counted six old square skips as
  new losses. Retained final-file PTS proved last gap20.200s, pin request
 20.190s, then139 continuous30fps pictures; no new engine change/rerun needed.
  The earlier real bug was `create_writers` flattening every extra canvas to
  Vertical, now fixed. Persistence uses library documents, not a guaranteed
  legacy setup.toml: verify through get/apply/restart instead of copying it.
- 2026-10-18 (#126): per-frame stderr tracing perturbs scheduling. The
  diagnostic rerun queued trace lines with try_send to a separate thread
  and stamped delivery inside the slot lock, after insertion. Both tracing
  patches are scratch-only; no diagnostic overhead remains in the fix.
  A separate 6 fps restart probe with 40/50 ms off-air gaps still shows the
  old play's picture at the first resumed slot (2 wrong pictures vs 9 on
  the baseline). This is the existing on-air notification/first-slot race,
  not the steady-state media delivery race fixed here; follow-up needed.

- 2026-10-18 (#110 experiments): `e2e.Engine` re-signs its copied daemon in
  a bundle, so its whole-file SHA differs from the source binary. Preserve
  original bytes before A/B and assert those hashes; don't silently waive a
  failed checksum. One weak-soak job refused before workload startup because
  `.hw-job` was absent after rapidly handing the Mac lock between queued
  jobs; the serial retry passed. The marker's check/remove is not atomic
  (`release_builds`), but no orchestrator change was made in this lane. TERM
  must raise KeyboardInterrupt in the weak wrapper: InterruptedError is an
  OSError, which a best-effort ps reader can swallow and incorrectly PASS.

- 2026-10-18 (#124, frame oracle): **e2e.py checks every picture against
  an independent model.** Fixtures are generated per run (never checked
  in): 640x360 clips whose frame n carries (tag, n) as a 16x9 block code,
  a 32-bit word (16-bit frame, 4-bit tag, CRC-12 over the word and its
  corner) in each corner, so any corner survives scaling, cropping into a
  corner, being covered, 4:2:0, HEVC/H.264/ProRes and masks; a decode needs
  two corners to agree where two should show. Sound: a 40 ms burst at
  every whole second, pitch = second mod 8. The model: a media source
  playing from the engine's start (restart_on_activate = false) shows
  media frame T0 + n at the take's slot n (speed s: floor(s(T0+n+0.5)));
  T0 is fitted once per take and predicts every other output (ISOs,
  vertical, extra canvases), the split take (T0 + the first take's slots),
  pauses (their slots skipped), scene switches (a switch at take time t
  takes effect at slot ceil(t*fps); a restart-on-activate source shows its
  frame 0 at its first slot on air: both measured on main and then made
  the spec), edits/exports (slot -> clip in point + offset on the take's
  grid) and NLE projects (each item's file frame -> media frame, compared
  without decoding). Why not the test pattern: its bars repeat every 4 s
  and blur below 3 px per frame; the block code is unique for 120 s and
  exact. The pattern model stays for Windows' contract takes
  (`--pattern-oracle`).
- 2026-10-18 (#124 review fixes): **The oracle is strict; #126 is a
  named, temporary allowance.** Under load (strict 1080p60 on M2-b) a
  live picture now and then shows every media source's previous frame
  (#126: media sources pace on an ordinary thread; test patterns got
  `real_time_thread` in #78). By default such a failure is reported
  "KNOWN ISSUE #126 ..." and fails. `e2e.py --tolerate-issue-126` accepts
  exactly that pattern and counts it: only in live recording outputs (not
  exports, transitions or self-tests), only at isolated pictures (never two
  in a row), only where EVERY region of the picture equals what the oracle
  expects at the previous frame slot (so speed, pauses and cuts are the
  model's), at most 1% of the output's pictures (none under 100). The
  merge commands pass the flag until #126 lands; then remove the flag and
  `issue_126`. Re-review (B1): the previous frame is the oracle's at the
  previous capture tick, and only where that tick was presented just
  before: at a pause's resume (or any cut in the file) nothing is excused.
  #126 also shows in shapes the flag never excuses (two or three pictures
  in a row; only some sources late): seen on M2-a/M2-b strict runs of main
  on 2026-10-18; such a failure on a merge check is rerun once and, if it
  repeats, checked on its contact sheet before blaming #126. `--selftest` proves the rule rejects one lagging source, a
  two-frame lag, two pictures in a row, >1%, exports and short outputs.
- 2026-10-18 (#124 review fixes): **Expectations come from the oracle,
  never from the engine's output.** The edit scenario keeps its own
  timeline (EditModel: the takes whole, then each requested trim, split,
  ripple delete, angle and crop as the API documents them); after each
  edit the engine's timeline must equal it, and exports and NLE projects
  are checked against the model (export inventory a, b, edited, program,
  vertical; one picture per slot; NLE tracks a, b, program tiling the
  sequence, one item per clip at the clip's slots, crops, angles; the
  enabled sound exactly one item per channel per program clip, counted as
  written (FCPXML and OTIO: one item per clip; xmeml: Premiere one stereo
  item, sourcetrack 1; Resolve one item per channel, 1..channels), the
  rest disabled; and the program sound
  as the project plays it, cut from the take files at the items' in
  points, has the oracle's second bursts where the oracle says).
  Transitions are modelled from the docs: a fade is the next scene over the
  current one, its share rising linearly (measured: share = (k + 0.75) / n
  at slot k, inside the model's [k/n, (k+1)/n] +- 0.08); a slide (left)
  pushes the current scene off to the left as the next comes in from the
  right, linearly. Each mid-transition picture is fitted as a blend of the
  two expected frames (residual < 14 grey levels, contrast > 75%) or
  matched at the model's offsets (95% of cells): a cut, black, the wrong
  frame or a stalled slide fails. The only fitted numbers are one T0 per
  take (and per replay save).
- 2026-10-18 (#124): **A/B gate rule (bench.py --gate, hw-batch bench):**
  a heuristic screen for material regressions, not a confidence test.
  First validity: >= 3 pairs; every run of both builds produced exactly
  the outputs the scenario asks for (setup_toml's own rules:
  expected_outputs), finished, with every loss counter the gate reads
  (stream audio_dropouts included; a missing counter is never zero), else
  INCONCLUSIVE, exit 2; when the base lost no video
  or audio, the new build must lose none (else FAIL). One discarded
  warm-up per build, then interleaved pairs in alternating order (ABBA...).
  Per metric (CPU %, compositor us p50/p99 required; GPU ms/s optional,
  "not measured" when neither build reports it) the median paired
  difference must stay under max(2.5 x noise, 3-10% of the base, an
  absolute floor), noise = the largest of 1.4826 x MAD of the base runs,
  of the new runs and of the differences / sqrt 2, and it fails only when
  two thirds of the pairs agree. A required metric missing from any run is
  INCONCLUSIVE. hw-batch adds --gate only where the job's own bench.py has
  it (older queued jobs run and say NOT GATED). Calibrated A/A (the same
  main build against itself, default settings, 2026-10-18): 12 batches on
  M2-a and M2-b, 0 non-PASS (exact 95% upper bound 22%); resampling
  their 72 runs into 4,000 random 3-pair batches per Mac (no same-time
  pairing, so pessimistic): 0.00% on M2-a, 1.65% on M2-b, after the
  relative floors were raised to 2.5 x the measured run spread (with the
  first floors: 5.8% on M2-b). A regression must be about 8% CPU or
  compositor p50 (15% p99) to fail by the floors alone.
- 2026-10-18 (#124): `scripts/vm.sh e2e` runs in its own folder per
  invocation, `~/isocast-e2e/<lane>/run-<time>-<pid>` (removed after),
  in the VM: two runs at once replaced each other's sources and deleted
  each other's work in the shared `~/isocast-e2e` (seen today). e2e.py's
  lock still runs them one at a time. It also copies the CLI (scene feeds)
  and brings the kept `work/` (contact sheets) back.
- 2026-10-18 (#124): merge checks run the MERGED scripts: `hw-batch
  --scripts worktree` (with --bin-dir) snapshots the worktree, uncommitted
  merge included, as a commit no branch points at; e2e reports carry
  `harness.e2e_sha256` and the scenario list. `scripts/oracle-windows.sh`
  stages each run in its own folder and exits 0 only when selftest,
  contracts and pattern all passed (1 failed, 2 couldn't run).
- 2026-10-17 (#105 W.1e): NPFS refuses WAIT→NOWAIT with ERROR_PIPE_BUSY
  while this endpoint has buffered outgoing bytes (even an 8-byte release),
  or a blocking operation is pending. Stream keeps its previous mode and
  exposes WouldBlock. On Windows only, SharedFrames::next(ZERO) defers that
  poll instead of disconnecting; the next call retries. Native probes cover
  buffered control, split JSON lines and peer-close-with-unread-ack at both
  endpoints (mode change then succeeds, read EOF, no perpetual deferral).
  A mode switch is not cancellation of old I/O. Pre-existing release writes
  discard errors in Drop; full-control-buffer backpressure remains W.6 work.
- 2026-10-17 (#105 W.1e): Pipe namespace v2 hashes binary owner SID plus
  normalized path with domain/length separation. Stop old Windows engines
  with their old client/API before replacing binaries; there is no insecure
  legacy-name fallback. SID is public: namespacing prevents accidental
  cross-user sharing, not targeted squatting. First-instance reservation,
  protected owner-only DACL, kernel PID/token/pipe-owner checks and remote
  refusal remain required. Cross-account negative testing remains unqualified.

- 2026-10-17 (#105 W.1d): Do not build a baseline source tree into the same
  Cargo target and then trust unchanged candidate fingerprints: source paths
  and archive mtimes reused a baseline protocol artifact, which was caught by
  the new parent_owned field failing to compile. Only the lane's affected
  workspace packages were cleaned (third-party caches kept), then rebuilt.
  Separate baseline target dirs avoid this; account for their disk cost.
  Windows WMI executable paths may start `\\?\`; normalize that prefix before
  checking that a process belongs to C:\isocast-dev (one probe wrongly counted
  zero processes). New NLE XML exporters use Unix file URL assumptions;
  Windows explicitly refuses XML until that seam is ported, not malformed
  successful exports. OTIO plain-path behavior is unchanged.
- 2026-10-18 (#115): **schemars drops a field's default when it has
  `skip_serializing_if`.** The Filters form then shows "+ Add" for it
  (`combine` did); serialize such enums always if the form should show
  them.
- 2026-10-18 (#115): **Offscreen `drag:` only reaches a TouchArea while
  the pointer is over it.** Drags that left the picture did nothing, so
  the mask handles' TouchArea reaches 24 px past the picture.

- 2026-10-17 (#114): **A `self.x.lock()` temporary in a struct literal
  lives to the end of the `let`.** Two `self.timelines.lock()` calls as
  fields of `Status { … }` deadlocked `status` (parking_lot isn't
  reentrant): read everything under one guard in a block first.

- 2026-10-17 (#104): **Driving DaVinci Resolve (free) on a test Mac**:
  only Studio takes external scripts, its Scripts menu lists Lua only
  (no Python found), and its Lua has no `io` (bmd.writestring only
  serializes), so results leave as Resolve's own files (renders and
  `Timeline:Export` OTIO/XML). Its windows have no usable AX tree: drive it
  with cua-driver desktop clicks (`{"scope":"desktop"}`) and `press_key`
  with pid + window_id; Workspace > Console's input takes a pasted
  `dofile("…")`. A quit with a modal dialog up leaves it running: `pkill
  -9`. The harness is in /Volumes/IsocastBuild/nle-export/scratch/resolve
  (drive.py, nle_test.lua, run.sh; run with `hw-batch.py now cmd --gui
  --mac m2-b --command 'bash ~/nle-export/resolve/run.sh 420'`). Resolve
  21.1.1 needed Rosetta (installed on m2-b).
- 2026-10-17 (#105 W.1c): Windows `std::process::Command` with redirected
  stdio still inherited other caller handles: detached isocastd kept a
  PowerShell capture pipe open after the CLI exited. Native launcher uses
  STARTUPINFOEX/HANDLE_LIST (only NUL and its log) and job breakaway, failing
  explicitly when a containing job forbids breakaway. Runner output is
  file-backed and follows the requested process, never grandchild-held EOF.
  Pipes keep a first-instance reservation and same-user kernel identity;
  mixed elevated/non-elevated launches remain unqualified, not bypassed.
  `hung_up` stays nonblocking during reads; shutdown wakes both directions
  and gives the peer EOF even while a Stream clone remains alive (native
  probes). Unsupported recording startup briefly creates then asynchronously
  removes its empty project; the root `.isocast.lock` is intentionally kept.

- 2026-10-17 (#85): **take.sh on isocast-m2-a: a wedged Terminal.** The
  Terminal there had run since 2026-10-12 with one stale window ("Grant
  Isocast permissions.command", its shell gone) and answered neither
  `open -a Terminal X.command` nor Accessibility (cua-driver found 0 AX
  windows), so the CLI window never came. remote.sh now restarts a
  Terminal that has no child processes (nobody's shell in it) before
  opening cli.command, waits up to 30 s for the window, and on failure
  writes out/terminal.txt (Terminal's windows, the last 2 min of its,
  launchservicesd's and syspolicyd's log). Restarting restores the stale
  window with a fresh zsh, so later runs leave Terminal alone. Note:
  inside remote.sh `log` is its own function; use /usr/bin/log.

- 2026-10-16 (#96 UI): on the test Macs' desktop session, `security
  find-generic-password … -w` on a stream key isocastd stored opens a
  Keychain password prompt and blocks; it also eats the keystrokes meant for
  the app. Compare keys by attributes instead (`security
  find-generic-password -s SERVICE -a ACCOUNT | grep mdat`, works over SSH,
  no prompt). A `cmd --gui` job that prints nothing for 10 minutes is
  marked FAIL ("no answer from the desktop-session bridge") and its lock
  released while the script keeps running: keep such jobs short or chatty.
  Slint password fields (the stream key) take no synthetic keystrokes,
  AXValue or paste from cua-driver; save keys through `stream_key_set`
  (CLI) when automating.

- 2026-10-16: `scripts/remote-build.sh` syncs the checkout it lives in
  (`dirname $0`), not the current directory: to build another worktree
  (a base for A/B), run that worktree's own copy of the script.
- 2026-10-16: on isocast-m2-a, `screencapture` from a hw-batch `--gui`
  job fails ("could not create image from display"); cua-driver's
  `get_desktop_state` works (~3 pictures a second). `isocast shutdown`
  there left the isolated daemon running in 3 of 3 tries; kill it by pid.
- 2026-10-16 (#94): measuring the editor's playback in a window: the
  video006 capture rig (a virtual display recorded at 60 fps by a second
  engine) with a test-pattern studio, scratch/rig/director on the
  editor-followups lane (`take.py playback`, then `analyze.py OUT`: picture
  changes in the viewer row, intervals, source frames advanced by bar
  shift). App-side times: ISOCAST_PREVIEW_STATS=1 prints `play:` lines.
  Remaining jitter (16% of pictures held 1 or 3 refreshes) also shows in
  the app's own render times, so not all of it is the capture.
- 2026-10-17 (#94): the editor window had no MenuHost and its Menus
  global wasn't wired, so `Menus.request` from it (a clip's context menu)
  did nothing; `App::connect_editor_menus` wires it (Export's menu needs
  it). An AVAssetTrack outlives nothing: drop its AVURLAsset and
  `insertTimeRange:ofTrack:` fails, so keep the asset alive.
- 2026-10-17 (#94): A/V sync in a real window: isocast-m2-b has BlackHole
  16ch; a recorder engine (the job's dev app) with a microphone source on
  BlackHole and a display source records the editor's picture and sound on
  one clock (mic sources there opened fine on 2026-10-17).
- 2026-10-16 (#94): a Chrome window launched by running its binary from
  the hw-batch desktop bridge crashed on first start (a "quit
  unexpectedly" dialog then covers later screenshots: kill
  UserNotificationCenter); `open -na` works. isocast-m2-a has Chrome 155
  in ~/Applications (installed for this check).

- 2026-10-16 (#87): headless Chrome (154) refused (MEDIA_ERR_DECODE, a
  third of a second in) both HEVC files of the check export, program and
  EDITED, whose cuts were encoded again (C.3's smart render gives them a
  second sample description, the likely cause). The takes themselves play, and H.264/.mp4
  exports play to the end; Safari, ffmpeg and AVFoundation play both.
  Repro: scratch/tools/export_check.py on the editor-round2 lane, in the VM.
- 2026-10-16 (#87): the first `Icon` of a kind that appears (an SVG
  path) can cost ~30 ms of one frame (the crop mark on the first cropped
  clip: 41.9 ms frame in the motion run, 9.9 ms with the mark drawn from
  rectangles). Draw small marks that appear mid-interaction.
- 2026-10-16 (#87): Slint callbacks can't declare properties; compute in
  a function. `rotation-angle` is `transform-rotation` in Slint 1.18.
- 2026-10-16 (#87): the motion harness can drive the editor window
  (`editor-…` targets); its takes follow the wall clock, so lockstep runs
  of it aren't pixel-identical.
- 2026-10-16 (G.4): Slint/winit windows on macOS: Slint keeps the native
  window's decorations in step with `no-frame` (winit set_decorations is
  undone), AppKit keeps titled windows inside the visible frame, and
  Slint's set_position is the content's top left before the native window
  exists but the title bar's after; projectors.rs `put_frame` handles it.
  Slint's rendering notifier gave no samples with the Skia/wgpu renderer.
  A dropped `frames::Feed` used to keep streaming (now `Feed::stop`).
  CGVirtualDisplay (scripts/screenshots/virtual-display.m) is a real
  display plug/unplug for tests; cua-driver's list_windows doesn't list
  windows above the normal level (use CGWindowListCopyWindowInfo).
- 2026-10-16 (#92): **"no hardware encoder is free" (-12908) can mean
  out of file descriptors.** VideoToolbox returns -12908 when the process
  has no descriptors left (a session needs about 4). macOS starts
  processes at a 256 soft limit; isocastd now raises it at launch (logged,
  and in `status` diagnostics.open_files), and the encoder errors check
  errno/the count first. To test under a low limit, lower the soft limit
  before the hard one (`ulimit -S -n 64; ulimit -H -n 64`): the other order
  fails in sh and leaves the hard limit, which isocastd then raises to.
  Each obs-websocket/watch client holds 2 daemon descriptors.
- 2026-10-16 (#89): multi-channel audio checks run in the test VM: it has
  BlackHole 16ch (installed from Homebrew's cached pkg with `sudo
  installer`, then `sudo killall coreaudiod`; works there without a
  reboot), its SIP is off and the microphone is granted to dev apps signed
  like `scripts/sign-dev.sh` does. ffmpeg plays a 16-channel WAV into it
  over SSH (`-f audiotoolbox -audio_device_index N`). `scripts/vm.sh sh`
  exports ISOCAST_MONITOR_SILENT=1, so unset it to hear (or record)
  monitoring. The #89 harness: /Volumes/IsocastBuild/audio-channels/
  scratch/hw/{t-vm.py,analyse.py}.
- 2026-10-16 (#89): `isocast record` records in the CLI process, so on a
  test Mac a microphone source asks TCC for the unsigned CLI (a prompt in
  the desktop session, "Cannot use <device>" over SSH); record through
  isocastd with `isocast start SETUP` / `isocast stop`. `isocast engine
  stop` leaves isocastd running: kill the pid `isocast status --json`
  reports (daemon.pid) when a scratch engine is done. Over SSH,
  `isocast devices` can time out in ScreenCaptureKit on the M2s; in the
  desktop session (`hw-batch … --gui`) it lists.

- 2026-10-16 (#74): a CLI-stored stream key can't be read by an ad-hoc
  signed isocastd from another build (702, "stored by another build"):
  for local streaming tests start the engine with ISOCAST_STREAM_KEY set.
  Also `isocast apply --live` takes thousands of patches a second from a
  script, far faster than a real drag; an obs-websocket session's watch
  queue (64) then overflows and resyncs.

- 2026-10-16 (#62): **A soak's "first minutes" loss can be another engine,
  not cold start.** The 2 h m2-a soak on 79bf8c7 (run by hand, outside
  hw-batch's per-Mac lock) lost 179 program / 194 stream frames between
  17:40:19 and 17:41:19 UTC, then nothing. The unified log shows a different
  isocastd (pids 78352 and 78368: the vertical-canvas lane's hw-batch job
  `20261016-133701-vertical-canvas-cmd`) creating 4 and 6 HEVC sessions at
  17:40:24 and 17:40:41; the same lane's 3-session trials ran every ~17 s
  from 17:38. Isocast's pressure feedback can't see another process's
  VideoToolbox sessions. The overnight d66fbb8 soak's 27 frames
  (06:42:27-06:42:57) coincide with a softwareupdated/mobileassetd burst
  (2,032/343 log lines, only that minute). Check
  `log show --start … --predicate 'eventMessage CONTAINS "new compression session"'`
  and `ps` for foreign engines before blaming a take start; run hardware
  soaks through hw-batch so the Mac's lock keeps other jobs off it.
  Current main's cold starts lose nothing (see the #62 log line).
- 2026-10-16 (#80): a VideoToolbox session create that can't get file
  descriptors fails with -12908, which the writer reports as "no hardware
  Hevc encoder is free". isocastd inherits a 256 open-file soft limit; the
  stress `clients` scenario (16 plugins, 32 websocket, 32 watchers) reaches
  ~240 and its first start fails this way on every build and Mac. Raise
  `ulimit -n` before blaming encoder leaks. Developer-mode fault hook for
  "a source dies while recording":
  `ISOCAST_TEST_PATTERN_FAIL_AFTER=<test_pattern id>=<seconds>`.

- 2026-10-16 (#82): a third-party HAL driver that stops answering makes
  every Core Audio call into it wait for coreaudiod's 60-150 s timeouts:
  on Ryan's Mac, Studio Audio (com.ryanvogel.studiondi.audio, from his
  Studio NDI app) stalled AVCaptureSession.startRunning 05:17:51-05:29:21
  after a 7 h take and failed with 0x10000004 (MACH_SEND_TIMED_OUT); its
  dealloc then blocked the main queue 90 s. The unified log shows it
  (`log show --predicate 'processID == <isocastd> AND subsystem ==
  "com.apple.coreaudio"'`: AUHAL `SelectDevice ... connecting device`
  with minutes between lines). Repro without such a driver: in the test
  VM, `sudo kill -STOP $(pgrep -x coreaudiod)` for under a minute, then
  `-CONT` (scratch /Volumes/IsocastBuild/mic-timeout/scratch/vm/hang.sh).
- 2026-10-16 (#70, e2e `edit`): the synthetic flash and beep follow the
  engine's clock, not the take's: a second take's pulses can sit half a
  second off the first's. Checks that cut at a chosen pulse phase must
  measure each take's beep onsets first (e2e.py `scenario_edit` does).
  The test pattern's bars scroll width / (4 s × fps) pixels per source
  frame, so the bar edge in one decoded row names the source frame
  (`pattern_frames`), and the label's fifth character tells BARS2 from
  BARS.
- 2026-10-16 (#83): TCC keys a Developer ID app's grants by its designated
  requirement (identifier + team), never the helper's file name: on
  isocast-m2-b an update-test app granted camera, microphone, screen and
  system audio as build 1792300000 (engine `isocastd`) kept every
  TCC.db row unchanged (same csreq and last_modified; the screen row's
  last_modified moved only when I toggled it by hand in between) after
  both a bundle swap and a Sparkle update to build 1792300100 (engine
  `Isocast Engine`), with no prompt. The camera alert then said
  "Isocast Engine" would like to use a new camera extension. Gotchas met on
  the way: the test Mac's screen-recording `+` and the extension toggle ask
  for the password in System Settings' own sheet (type with cua-driver's
  desktop scope, not into SecurityAgent); ffmpeg's avfoundation input
  rejects `-framerate 60` for the Isocast camera (use 240, the range's top).

- 2026-10-16 (S.2 stress): Python's `sock.close()` doesn't close a
  socket whose `makefile()` reader is still open, so the connection stays
  up on the daemon's side (e2e.py's `Watch.close` does this). stress.py's
  first clients run "leaked" 31 threads and 64 fds that were really its
  own 32 open watch connections; close the reader too (stress.py
  `hang_up`).
- 2026-10-16 (S.2 stress): macOS caps Unix socket paths at 104 bytes; a
  hw-batch job's work folder is ~90 bytes deep, so stress.py runs each
  engine through a short `/tmp/isocast-stress-<pid>` link when needed.

- 2026-10-16 (#4): Rust's `File::sync_all`/`sync_data` on macOS is
  F_FULLFSYNC: 3.6 ms on Ryan's internal disk, ~14 ms on
  /Volumes/IsocastBuild, per call. Keep it off anything a person or a
  script waits on (a CLI command, a reply); `libc::fsync` is ~0.1 ms.
  To time a CLI command, run several builds interleaved against one
  isolated daemon: single runs vary by ~0.5 ms.
- 2026-10-16 (#78): a sync offset of exactly two frames (33.31 ms at
  60 fps) in an e2e file is a source picture arriving late, not a muxing
  bug: the program takes each source's newest frame at draw time and never
  waits, while audio is stamped by sample. Read the burnt-in `FRAME`/`T+`
  label around the flash (`crop=1000:290:0:0`): a repeated frame then a
  jump is a late source. The replay scenario's evidence keeps every media
  clock's `wake_max_us`/`late_wakes` and the compositor's `reused` count.
  At the default QoS a generator thread in the background-only daemon woke
  up to 104 ms late with *no* load (CPU load with 8× `yes` made it better,
  14-26 ms), so loading the CPU doesn't reproduce it; repeated idle runs do
  (2/15 on m2-b).
- 2026-10-16: **A new remote-build lane can inherit another lane's crates.**
  Its target is an APFS clone of the last-used lane's and sync keeps
  mtimes, so a crate this branch doesn't touch but the other lane changed
  (here isocast-protocol, from a vcam branch) counted as fresh and the
  build failed on a variant main doesn't have. Touching every crate source
  once fixed it: `git ls-files 'crates/*.rs' 'crates/*.slint' | grep -v
  libvpx | xargs touch` (a full rebuild, ~8 min on an M2).
- 2026-10-16: **Offscreen `press:LABEL` fails in a run's first snapshot**
  ("no element named …") for elements of a dialog opened by a state in the
  same snapshot; put a plain snapshot of that state first (the later
  snapshots in the run find it).

- 2026-10-15 (#64): a media source doesn't advance while nothing records
  or streams: `snapshot` on an idle engine shows the clip's first frame
  every time. Start a recording before snapshotting media. Snapshots of a
  playing alpha clip compared with ffmpeg's composite sometimes match no
  single reference frame (31-46 dB on both VP8 and VP9, while others are
  exact or 48-52 dB); take several.
- 2026-10-16 (#76): `flock` on an NFS mount with `-o nolocks` fails with
  ENOTSUP (45); Rust's `try_lock` reports it as `TryLockError::Error`
  ("lock acquisition failed due to I/O error"), not `WouldBlock`. Before
  #76 that refused every crash-safe take there as "already being written
  by another recorder". A lockless share to test on: on an M2 test Mac,
  `/etc/exports` with `/Users/Shared/csf-nfs -mapall=501:20 -network
  127.0.0.0 -mask 255.0.0.0`, `sudo nfsd start`, `sudo mount -t nfs -o
  nolocks,vers=3 localhost:/Users/Shared/csf-nfs DIR`. Repair at launch
  needs the folder's exclusive lock, so a take killed there stays
  "recording" with its fragmented (playable) files; kill-matrix can't run
  with --work on exFAT or NFS (the daemon's Unix socket can't live there).
- 2026-10-15 (#33): Process taps need their own TCC grant (System Audio
  Recording Only) even where screen recording is granted; an engine
  with screen recording but no decision blocks in
  `AudioDeviceCreateIOProcID` on the prompt. A copied /usr/bin/afplay is
  SIGKILLed (platform binary signature); `codesign -f -s -` the copy.
  On the M2s the tap and the aggregate's input stream report 48 kHz even
  with the speakers set to 44.1 kHz (the HAL drift-compensates; beep
  stamps then jitter ±3 ms instead of 0), so the resampler path (rate ≠
  48 kHz) has only its unit test so far. afplay itself goes silent for
  ~1 s when the default output changes (SCK records the same gap).
  `tccutil reset AudioCapture dev.isocast.isocastd` brings back "not
  asked" on a test Mac. A private aggregate device is visible to its own
  process as an AVFoundation microphone (filtered by its UID prefix
  `dev.isocast.tap.`). The test VM's "Apple Virtual Sound Device" has
  inputs and outputs, but a daemon started over SSH there can't play
  (afplay: AudioQueueStart 89) or write files (-12903), so audio checks
  need the M2s.
- 2026-10-15 (#73): **check a received FLV's A/V sync from the audio's
  first packet time.** e2e's `pcm()` numbers decoded samples from 0, but an
  FLV has no edit list and its audio starts a few ms (up to ~50 ms on an
  M2) after the first picture, so beep/flash "offsets" of 0.7–49 ms that
  vary per run are that start, not drift. Adding the first audio packet's
  pts gives <= 0.4 ms on every destination, before and after a reconnect.
  (The lane's scratch ms.py does this; e2e's stream scenario doesn't run
  `sync_check` on the FLV, so e2e isn't affected.)
- 2026-10-15 (#70): **A new remote-build lane can build stale sources.**
  Its target is an APFS clone of another lane's, whose artifacts can be
  newer than files this branch changed long ago (sync keeps mtimes), so
  cargo skipped isocast-protocol and reported "no `TimelineTrimParams` in
  `timeline`" for files that were right there. Touching the branch's own
  files before the first build on a new lane or host fixed it:
  `git diff --name-only --diff-filter=d "$(git merge-base main HEAD)" |
  xargs touch` (against `main` itself, once main has moved on, touch
  creates empty files for main's new paths).

- 2026-10-15 (#70): **`hw-batch.py … cmd --gui` jobs must finish within
  10 minutes**: `testmac.sh gui` gives up on the desktop bridge after 10
  minutes, and the runner then deletes the job's folder and frees the lock
  while the job still runs there (a 30-minute recording kept going into a
  deleted folder until killed). Do long work in an SSH `cmd` job, keep its
  output outside the job folder, then a short `--gui` job for the window.

- 2026-10-15 (#70): **A passthrough AVAssetWriterInput takes samples with
  another format description** (VideoToolbox's re-encoded keyframe
  interval between copied ones): it writes a new sample description, in
  .mov and .mp4, H.264 and HEVC, and FFmpeg decodes across them with no
  errors. FFmpeg then attaches new-extradata side data to the first packet
  after each switch, which adds a column to `-f framemd5` lines: compare
  the hash column by name, not "the last column" (7 copied keyframes
  looked new that way). Also decode with `-fps_mode passthrough` when
  matching frames by index: an export of a 30 fps ISO whose parts start on
  the 60 fps grid probes as r=60, and FFmpeg's default duplicates frames to
  that.

- 2026-10-15 (#59, fixes-005): `stream stop` drops `state.stream`, but a
  stream that *failed* stays in the status until the next start, and its
  own status hard-codes `key_set: true` (engine stream/mod.rs). `status`
  now takes service, server, key_set and key_problem from the live setup
  once the stream isn't active. Test it with a failed start (nothing
  listening), not a stop. Keychain writes on a test Mac must go through
  the bridge (`testmac-bridge.sh queue`): over SSH the login keychain is
  locked.
- 2026-10-15 (#71): **obs-websocket-js uses MessagePack by default under
  Node** (`import 'obs-websocket-js'` resolves to the msgpack build; JSON
  is `obs-websocket-js/json`), so a JSON-only server fails its handshake
  with HTTP 400. Typed clients (obws/obs-cmd, goobs, Streamer.bot) refuse
  `2326.0` where OBS sends integers (durations, bytes), and expect
  responses the docs omit: ToggleRecordPause returns `outputPaused`
  (checked every mapped handler in obs-websocket's source; that is the
  only one). Level meters (InputVolumeMeters) flow only while the live
  engine runs. An unsigned rebuild of isocastd can't read the Keychain
  password the previous build stored (as with stream keys): the server
  stays off and says to set it again; tests use ISOCAST_WEBSOCKET_PASSWORD.

- 2026-10-15 (#3 integration): **A direct VT encoder's first keyframe is
  not the end of startup.** With a 4K program and 4K/1080p/1080p ISOs,
  three M4 starts kept every frame on main but the crash-safe writer
  refused 3/3/2 ISO frames at 0.217–0.250 s. Refusal-only traces showed
  queue 4, native in-flight 8, newest encoded PTS 0: its startup backlog
  flag had cleared too early. Keep the bounded 32-picture startup queue
  until two encoded pictures return and pending is below 4. Afterward
  all three starts are loss-free; the shared 25 ms first-picture gate and
  the steady ISO admission policy are unchanged. Never use a heavy VT
  property-query interposer to qualify losses: it perturbed both builds
  substantially. Also, create movie files exclusively before locking;
  truncate-then-lock destroys an existing file even when the lock fails.

- 2026-10-14 (G.3b fix round 1, from the adversarial review): a muxer that
  writes a picture's duration before it knows the next picture's time
  silently removes every gap that falls on a fragment boundary; the
  reviewer's SIGSTOP stalls produced 0.6–1.0 s of A/V drift in finished
  files that decoded cleanly. Hold the newest picture until its successor
  arrives, and rebuild the final table from the sample times. "Decodes
  without errors" never certifies timing: compare `mdhd`, `stts`, the
  edit, ffprobe's packet end and AVFoundation's decoded end.
- 2026-10-14 (G.3b fix round 1): `File::sync_data()` is `fcntl(F_FULLFSYNC)`
  on Apple platforms (Rust std), so there is no "plain fsync" to choose
  from Rust and a `fsync` interposer does not see it; trace `fcntl` op 51.
- 2026-10-14 (G.3b fix round 1): a 16-byte `pwrite` can tear under a
  kill (a test interposer wrote 8 bytes and stopped); a recovery that
  trusts a header's own bytes then strands intact data. Validate against
  what follows (the header must end exactly on a complete `moov`).
- 2026-10-14 (G.3b fix round 1): AVAssetReader fails a track whose edit
  presents media it does not have (an audio track with zero packets and a
  33 ms edit): leave such a track out, and present only available media.
- 2026-10-14 (G.3b fix round 2): adding a method with a default body to a
  trait silently leaves every delegating wrapper (`MacWriter`'s enum
  dispatch) on the default: the re-review's VideoToolbox drop interposer
  showed `lost_frames` never reaching the engine. Delegate every method,
  and check new accounting end to end with an injected event, not by
  reading the code.
- 2026-10-14 (G.3b fix round 2): a header patch can tear after *any*
  byte; a recognizer that accepts two exact prefixes (the review's 8
  bytes) still strands 4–7. Decide from the structures behind the
  reserved area, never from the area's own bytes, and test every length.
- 2026-10-14 (G.3b fix round 2): a folder lock does not protect an inode
  reachable through another folder (a symlink or hard link staged in a
  failed manifest's folder). Repair must open through its own directory
  with `openat(O_NOFOLLOW)`, refuse `st_nlink > 1`, and keep that
  descriptor; `OpenOptions::open` by path follows links.
- 2026-10-14 (G.3b fix round 3): `rename` keeps `st_nlink == 1`, so a
  live movie moved into another folder passed every alias check; the
  recorder now `flock`s its movie while writing and repair try-locks.
  A movie "as long as its last picture" hides audio saved after it, and
  with no picture at all presents nothing although ffmpeg read a second
  of sound from the raw file: size the movie by its longest kept track.
  Identify own files by an exact `ftyp` (with a private brand), never by
  layout: a foreign fragmented MP4 with a 16-byte box after its 28-byte
  `ftyp` got the header patch at the wrong offset and stopped opening.
- 2026-10-14 (G.3b lane): a shared `CARGO_TARGET_DIR` between two source
  trees (main's archive and the worktree) lets `cargo build -p …` link a
  crate against the *other* tree's rlib of the same name when feature
  unification differs between invocations ("struct `LiveFile` has no
  field named `committed_seconds`" although the source has it). `cargo
  clean --release -p` the workspace crates and rebuild; copy each build's
  daemon away immediately, since the next tree's build overwrites
  `target/release/isocastd`.
- 2026-10-14 (G.3b lane): a QuickTime `mp4a` track with an edit list that
  skips the AAC priming needs the `roll` sample group (`sgpd`/`sbgp`, roll
  distance -1) AVAssetWriter writes: without it AVAssetReader (what
  join.rs and editors use) starts the audio at the first whole packet
  after the edit, 64 ms late; ffmpeg decodes from packet 0 either way.
- 2026-10-14 (G.3b lane): AVAssetReader without output settings reports
  a few more video "samples" than the track's frame count (244 for 240
  frames of AVAssetWriter's own file, 249 for 245 of ours); compare frame
  counts with ffprobe `-count_frames`, not AVAssetReader sample counts.
- 2026-10-15 (#68, iso-overshed, BLOCKED): **Estimated spare capacity is
  not proof that stream pressure is false.** On exonbook/M4 Max, synthetic
  4K60 + 1080p60 HEVC ISOs, 1080p60 program and 6 Mb/s 1080p60 stream:
  stream-first is loss-free; record-first loses 1,973/1,971 ISO pictures in
  a 181 s stream window and 2,024/2,021 across six 30 s split takes, with
  protected outputs clean. Disabling only #60's post-call pressure produces
  0 ISO drops but 5,523 program / 6,058 stream drops over 181 s; three split
  takes then lose 883 / 897 / 0 program pictures. Native placement/start order
  is the leading hypothesis (#41), not measured engine assignment. Do not
  dismiss those calls as harmless based on the capacity estimate. Traces
  confirm rising pressure edges bypass the controller's 100 ms cut limit;
  recovery is per guard (usually two), not process-wide. An eight-call
  capped-average signal worsened M4 long ISO losses to 5,906 each; limiting
  cuts to 100 ms and recovering +5%/500 ms improved short starts but worsened
  long/split runs (3,100/3,099 and 4,946/4,940). Both candidates and temporary
  traces were removed. M2-b fitting 1080p60 ISO + program + 720p60 stream is
  already loss-free on main for 6 s, 181 s and six split takes; heavy runs
  retain the known 11–14 startup protected losses but zero post-split losses.
  Stop tuning constants: next measure native engine placement/throughput for
  the two start orders. Evidence, per-take counts and full decodes:
  `/Volumes/IsocastBuild/iso-overshed/scratch/REPORT.md`. #68 is not resolved.

- 2026-10-15 (#18): **AAC in a .mov for AVFoundation** needs QuickTime's
  version 1 sound description (compression id -2, 1024 samples per packet)
  with the `esds` inside a `wave` atom (`frma`, `mp4a`, `esds`,
  terminator); version 0 with a bare `esds` (fine for ffmpeg) fails to open
  with -11829/-12848. And without an `sgpd`/`sbgp` `roll` of -1 on the
  audio track AVFoundation drops ~600 samples of the edit list's start
  (its audio came out 13 ms short of the video). Check muxed files with
  AVAssetReader, not only ffprobe.
- 2026-10-15 (#66): **ISO files can drop with a stream on even when the
  encoders have room.** On the M5 Max (load 0.47 by the table) a 6 s take of
  a 4K60 + 1080p60 ISO, 1080p60 program and a local stream dropped ~69% of
  ISO frames (program and stream whole): the stream encoder's pressure
  (#60) cut the ISO share and it recovers at +1%/500 ms. On m2-b the same
  takes without a stream were clean and the predicted-fitting fix with the
  stream was clean, so the M2 numbers stand, but the overload notice says
  "the encoders fell behind, though this setup should fit" rather than
  blaming capacity when the table says it fits. Worth a look by whoever
  owns the admission controller.
- 2026-10-15 (#66): **remote-build lanes seeded from another lane can
  reuse that lane's crates.** A new lane's target is an APFS clone of the
  last-used lane's; cargo judges freshness by mtime, so a checkout whose
  files are older than that lane's last build (a fresh `git worktree add`
  of an older commit) compiled against the other lane's protocol (a base
  build of main failed on this branch's `EstimateCapacity`). Fix used:
  `ISOCAST_LANE=<lane> scripts/remote-build.sh cargo clean --release -p
  <every workspace crate>` once, then build.

- 2026-10-15 (#58): System Settings' sheets (the password prompt, "Quit &
  Reopen") are separate layer-0 windows of its pid, in front of the main
  window; pick its largest window to find the main one. Over ssh on
  isocast-m2-b, TCC reads camera and microphone as denied for every app
  (not "not determined"), so no request is made there at all.

- 2026-10-15 (#46): `scripts/remote-build.sh` on a new lane clones the
  most recent lane's target and keeps local mtimes, so crates edited
  before that lane's last build look up to date and the build fails with
  "no `optimize` in isocast_protocol" (or worse, links stale code).
  `git diff --name-only main...HEAD | grep '\.rs$' | xargs touch` before
  the first remote build of a lane fixes it.
- 2026-10-15 (#46): over SSH a test Mac's login keychain is locked, so
  moving a stream key fails (-25293); `scripts/testmac.sh gui` works.
  Don't read the item with `security find-generic-password -w` there: it
  prompts (the item belongs to isocastd) and blocks.
- 2026-10-15 (#46): test Macs have no `timeout` (coreutils); bound remote
  scripts with the local `timeout` around testmac.sh instead.
- 2026-10-15 (#46): a speed-test key that is a common word ("test") makes
  the redactor blank that word everywhere in messages; use an odd key in
  ISOCAST_STREAM_KEY for local tests.
- 2026-10-15 (#61, WebM/MKV): Apple's Opus AudioConverter drops the
  first 120 samples it decodes (2.5 ms; macOS 27.2) and reports no
  PrimeInfo, so applying the full OpusHead pre-skip put sound 2.5 ms
  early. The reader decodes the first packet alone on a throwaway
  converter and subtracts what's missing from the pre-skip;
  AudioConverterReset does NOT bring the drop back (a reset converter
  measured 120 and then played 120 late), so playback uses a fresh one.
  AAC's magic cookie must be an ES descriptor (esds) around the
  AudioSpecificConfig: the raw ASC from CodecPrivate is rejected with
  '!dat'. VideoToolbox has no VP8 decoder; VP9 profile 1 (4:4:4) fails
  with -12910; AV1 needs VTIsHardwareDecodeSupported (M3+; true on the M5
  Max). A VP9-alpha 1080p stinger costs ~130 MB more RSS than its ProRes
  4444 twin (two hardware VP9 sessions' surfaces, BGRA output), flat over
  time.
- 2026-10-15 (#57): macOS 27 asks "Allow “Isocast Engine” to find devices
  on local networks?" (UserNotificationCenter alert) on the engine's first
  .local lookup even without NSLocalNetworkUsageDescription; while it's
  unanswered getaddrinfo blocks (stream stays "connecting"), after Don't
  Allow the lookup fails ("nodename nor servname"). The Mac's own LAN
  address is not restricted, so a one-machine test can only show the
  .local half; the AWS test Macs can't reach another LAN host. The switch
  is "Isocast Engine" in Privacy & Security → Local Network (cua-driver
  sees it as AXCheckBox "Isocast Engine_Toggle").
- 2026-10-15 (#63): Keychain items a Developer ID engine creates trust it
  by designated requirement (path recorded, not checked): a copy at
  another path and another build with the same requirement read it
  silently. Deleting a same-team item works; a different signer's
  (e.g. `security add-generic-password`) can't be deleted (-25244). Once,
  a 0.0.3-saved item failed to read from the fixed build (ignored as
  designed); two repeats moved fine. `isocast status --json` prints only
  state and files; the stream's key_set is in the `status` RPC.

- 2026-10-14 (motion harness, fix round 3): a gain computed as an
  unaligned regression drops for a late edge (0.79 at 14 frames) and
  must never gate the lag; use travel over travel. A SIGKILLed motion.sh
  leaves its isocast-ui --motion child running (parent 1, no lock), and
  it would relaunch the engine: the marker records UI pids for recovery.
  Shell redirection and write_text follow symlinks: publish outputs by
  writing a temp file and renaming it.

- 2026-10-14 (motion harness, fix round 2): a lock made of mkdir + a
  pid file has a window where the pid isn't written yet and a second run
  "reclaims" it; hold an flock on an inherited fd instead (python3 -c
  'fcntl.flock(9, …)' on the shell's fd 9 keeps the lock with the
  shell's open file) and close fd 9 for children, or a detached engine
  keeps it. A script started as a background job of a non-interactive
  shell inherits SIGINT ignored and can't trap it (POSIX), so it runs to
  the end on Ctrl-C to its group: test signal cleanup from a foreground
  or new-session launch.

- 2026-10-14 (motion harness, review fix round): `isocast engine stop`
  only lets isocastd idle, and the CLI launches it detached (parent
  launchd, no socket path in its argv), so `pkill -f "$work"` never ends
  it: shut it down over its socket and TERM/KILL what `lsof -a -c
  isocastd -t +D "$work"` still lists (without `-c isocastd` the script's
  own shell matches, through its open files, and kills itself). Things
  that made "identical" lockstep runs draw different pixels: the work
  folder's path in the status bar (use a fixed folder), setup revisions
  (the engine numbers them from the clock), the engine's CPU and the
  recording disk's free space in status replies, the picture during a
  preview resize (the engine draws at the size asked for when the window
  was last drawn; wait for that exact fitted size), and replies vs events
  racing on their two connections.

- 2026-10-14 (motion harness): Slint's TouchArea `mouse-x` is relative to
  where the area is now; a splitter that moves with the drag and sizes
  itself from `mouse-x - pressed-x` settles at half the pointer's distance
  (#24). Window recordings: 4:2:0 video can't be cropped to an odd height
  (ffmpeg fails quietly in a pipe), and levels come out shifted
  (#242424 on #121212 → #141414 on #080808): find lines by contrast. The
  real app needs ISOCAST_UI_PREFS per test run, or a drag's layout carries
  into the next run. CGWindowList shows isocast-ui's windows with owner
  `isocast-ui` and an empty title without Screen Recording.
- 2026-10-15 (ui/fast-loop): under the interpreter (`live-slint`) the
  window's `init` ran before the Rust side hid the in-window menu bar and
  `changed height` never fired after, so `Viewport` was 40 points short
  (docks 229 instead of 240). offscreen.rs resizes there and back after
  show and after every reload (`live_resize`); keep that if the menu bar
  or Viewport logic changes. The interpreter also logs "The use of the
  ElementHandle API requires … debug info" for each of ~2000 internal
  items on every element search (`--crop`, `press:`); the offscreen
  platform drops that line, as printing it made a reload 3.8 s instead
  of 0.9 s. Watch reloads keep properties, models and callbacks the Rust
  side set, so dialogs, menus, recording carry over; hover does not.
  `--state stream:live` before `wait:` is undone by the daemon's next
  stream status (compiled builds too): put `stream:` after the wait.
  visual-shots pictures include the disk's free space, so render base
  and head back to back or unrelated text shows as changed.

- 2026-10-14 (#47 fix round): building a `git archive` of main into the
  same CARGO_TARGET_DIR (for "before" screenshots) left main's
  isocast-protocol/diagnostics artifacts in place; the branch then failed
  to compile against them until their sources were touched. Give an
  archived checkout its own target, or touch the workspace crates after.
- 2026-10-14 (#47): Settings' draft only carries the setup parts `pick`
  lists (name, canvas, recording, audio, stream, now developer): a new
  profile section shown in Settings must be added there, or its switches
  read the default. `isocast start --seconds N` returns once the files
  exist, not when the take ends. Offscreen `press:LABEL` finds a dialog's
  buttons only after a render: put `type:` (renders, types nothing) first.
- 2026-10-14 (7.7): scripts that look for a recording's files in the
  folder they passed as `--folder` / `recording.folder` must now look one
  level down (the project folder): e2e's drop collection, bench.py and
  drag-lag.py were changed; the e2e harness already followed the paths in
  `started`.
- 2026-10-14 (7.7): SBPL rules are last-match-wins; `regex` filters take a
  POSIX regex in `#"..."` (no `"` possible), so the recordings root is
  regex-escaped and a root with a quote gets no write rule.
- 2026-10-15 (#51): macOS 27 attributes every privacy permission of the
  bundled engine to the outer Isocast.app (tccd log: "executable path
  …/Isocast Engine.app/Contents/MacOS/isocastd resolves to attributed
  bundle: …/Isocast.app/", team ids matched), for screen recording too, so
  System Settings lists "Isocast" and the helper drags Isocast.app. A
  first reading on m2-a looked like screen recording went to the engine's
  own bundle; that was the dev app's TCC entry for the shared
  dev.isocast.isocastd id. tccd's reasons: `sudo log show --predicate
  'subsystem == "com.apple.TCC"' --info` (BUNDLE_ATTRIBUTION lines).
- 2026-10-15 (#51): any grant made in System Settings while isocastd
  runs reaches only processes started afterwards: screen recording ("may
  not be able to record … until it is quit") and also camera and
  microphone turned on after Don't Allow ("will not have access … until
  it is quit"; isocast-m2-b, macOS 26.7). macOS's Quit & Reopen quits
  only the UI, never isocastd. A child that inherits isocastd's
  responsibility inherits the stale answer too: the
  `--preflight-permissions` child disclaims responsibility and exits with
  a bit per permission it lacks; any it has and the engine doesn't gets
  action `restart`.
- 2026-10-15 (#51): an app without the camera/audio-input entitlement
  (0.0.2) gets its media request answered "denied" in a few ms with no
  prompt; the engine marks requests refused within 600 ms
  `denied_without_prompt` (reproduced with an ad-hoc copy of the release
  app on m2-a). `tccutil reset` needs the bundle registered first
  (`lsregister -f`), else "No such bundle identifier".
- 2026-10-15 (#51): on isocast-m2-a, cua-driver's desktop `drag` performs
  a real drag into System Settings' privacy list (it asks for the admin
  password, typed with desktop `press_key` per character; `type_text`
  can't reach the auth sheet), and Finder windows cover the dock. Resetting
  dev.isocast.isocastd there also resets the shared dev app's grants;
  after the #51 run camera, microphone and screen recording were granted
  to Isocast Dev again (same certificate, so every worktree's dev app).
  If it held Input Monitoring or Accessibility before (hotkey tests), those
  were reset too and not restored: grant them again if a hotkey run on
  m2-a says so.

- 2026-10-14 (8.2h): Slint's macOS menu bar (muda) rebuilds the whole
  application menu whenever a tracked property changes, so an NSMenuItem
  added once disappears. update.rs re-adds Check for Updates… on every
  NSMenuDidAddItemNotification (tagged, so it is added once per rebuild).
- 2026-10-14 (8.2h): an app relaunched through LaunchServices (Sparkle's
  relaunch, `open`) gets none of the shell's ISOCAST_* variables, so a test
  app would talk to the default engine. `release.sh --update-test` writes
  ISOCAST_UPDATE_TEST_ENV into the test bundle's LSEnvironment instead.
- 2026-10-14 (8.2h): on isocast-m2-a, `screencapture` from the bridge fails
  ("could not create image from display"); cua-driver's get_desktop_state
  works. Two stuck system notification banners there cover the window's
  top-right corner, exactly where toasts draw: use a window-only capture
  (get_window_state's screenshot) to see toasts. Sparkle's error alert has
  no AX tree for cua (window unresolved); click it with desktop coordinates.
  cua-driver CLI calls need one `session` label across calls (snapshot
  tokens are per session) and a session ends after idling; use a new label.
- 2026-10-14 (8.2h): macOS 27 prints a deprecation warning for `hdiutil
  attach/detach` (use `diskutil image attach`); it still works.
- 2026-10-14 (#41, iso-drops-2): **VideoToolbox engine placement is the ISO
  loss.** A standalone VT benchmark (`iso-drops-2/scratch/vtbench`, same
  settings as the writer: hardware HEVC, RealTime, quality 0.7, no
  reordering, look-ahead 0) with 2× 4K60 + 2× 1080p60 sessions reproduces
  the slow mode in a few seconds: started together, 3–30% of trials (94%
  when one session already runs) put three sessions on one engine, each at
  ~45–53 fps with ~170 ms latency while the fourth sits at 13 ms; started
  ≥ 17 ms apart it is deterministic in the order of sizes (4K,4K,1080,1080 /
  1080,1080,4K,4K / 4K,1080,4K,1080 always fine; 4K,1080,1080,4K and
  1080,4K,1080,4K always bad), 10 ms apart 3%, 5 ms 22%. Capacity is not the
  problem: the same four sessions fed at 120 fps each all hold 120 fps when
  started apart. `IOReport` (private, read-only, `/usr/lib/libIOReport.dylib`)
  exposes per-engine counters, "Interrupt Statistics (by index) | ave0 0 /
  ave1 0 | First Level Interrupt Handler Count" (about 7 per 4K60 picture,
  2 per 1080p60): a balanced take reads ave0 ≈ ave1 (1.00), {program} +
  {4K,1080,1080} reads 0.60 and is still fine, a bad take reads ~2:1 with the
  program at 170 ms latency. `powermetrics` has no media-engine sampler and
  `AMC Stats | AVE0/AVE1` byte counters stay zero. The AVAssetWriter session
  is created inside the first `appendPixelBuffer` (synchronously, on the
  writer's thread; the trace's create→first encode is ~5 ms) and torn down
  when the writer is finished/released, ~10–20 ms after `markAsFinished`.
  The pressure controller's recovery (+1%/500 ms, 43 s from 13%) made a
  single bad split cost the following take 150–700 frames too (its share
  was still recovering); unchanged in this fix, measured for later. The
  stream encoder shares the engines: start it apart from recordings too
  (done through the same gate).

- 2026-10-14 (#41, iso-drops, BLOCKED): the presumed stale ISO-share toggle
  is not the cause of bad independent takes. Temporary guards log new
  outputs=1/pressure=0/share=1000 every time and outputs=0/pressure=0 on Drop.
  Before a bad take's first admission cut, a VT interposer sees 4K encode
  submissions taking ~18–24 ms, eight frames in flight, callbacks ~170–180 ms
  behind. Three pending raw program pictures trigger cuts from 1000 to 132
  in ~0.53 s; all ISOs follow that common fraction. Recovery is +10 per
  500 ms per guard (1.2 fps/s at 60 fps for one guard), not independently
  measured 2.4 fps/s. Split guards overlap and can carry pressure forward.
  The upstream reason identical native sessions sometimes slow down is
  still unknown. Relative AVAssetWriter media times failed the split test;
  late explicit look-ahead=0, power-efficiency=false and max real-time fps=60
  each still had bad takes. After three native-setting trials, stop that
  hypothesis. All trials/logging were removed, overload behavior unchanged.
  Next useful control: identical prerecorded inputs and controlled native
  session initialization/ordering, tracing actual submitted/completed PTS.
  Full commands, raw traces and reports are in
  `/Volumes/IsocastBuild/iso-drops/scratch/REPORT.md`. Keep the soak's failing
  files on exonbook until #41 is genuinely fixed; do not weaken admission
  or move its globals and infer success from a few lucky clean takes.

- 2026-10-14 (8.2g review): Ryan's interactive zsh has `interactivecomments`
  off (zsh's default; `[[ -o interactivecomments ]]` in `zsh -i` on his
  rc files says off), so a `# comment` at the end of a pasted command line
  becomes arguments (`gh secret set … # paste` → "accepts at most 1 arg",
  `sed … Cargo.toml # [workspace.package]` → "no matches found", a line
  that is only a comment → "command not found: #"). `bash -n`, `zsh -n`
  and `zsh -ic '…'` all accept comments and miss this; only typed or
  pasted input into `zsh -i` on a pty shows it. Rule for docs: no comments
  inside command blocks, explanations in prose, and chain the lines of a
  block with `&&` so a failed guard stops the rest. The harness that
  pastes blocks into a real interactive zsh (Ryan's rc files, scratch HOME,
  stub gh/git) is `/Volumes/IsocastBuild/release-secrets/scratch/pty_paste.py`.
- 2026-10-14 (release-secrets): `gh secret set` reads stdin with
  `bytes.TrimRight(body, "\r\n")` (cli v2.96.0 `pkg/cmd/secret/set/set.go`),
  so an editor's trailing newline is harmless; with a TTY and no redirect it
  shows a hidden "Paste your secret:" prompt, which keeps the .p12 password
  and both API ids out of files and history. `GET releases/tags/<tag>`
  returns published releases only (404 for a draft or none), which is what
  makes a read-only preflight safe. The macos-15 arm64 image is published
  as `macos-15-arm64/<date>` in actions/runner-images; the plain `macos-15`
  readme on `main` describes the Intel image and lags behind. On macOS 27
  `hdiutil create -srcfolder` prints a deprecation warning (use `diskutil
  image create`); the macos-15 runner (15.7.9) does not, and the dmg-design
  lane is replacing that step anyway. `xcrun notarytool store-credentials
  <profile>` updates an existing profile in place and validates first.
- 2026-10-14 (DMG window): Finder's .DS_Store WindowBounds is the whole
  window frame, so a 640x400 background needs 640x432 (32 pt title bar of a
  toolbar-less window on macOS 26.7/27.0; older macOS may differ by a few
  points). dmgbuild treats a background path that starts with a colour word
  (`grey/…`, `gray…`, `rgb…`) as a colour; pass absolute paths. hdiutil on
  macOS 27 prints deprecation warnings (create/attach/convert); harmless.
  `uv run --with-requirements` ignores `--hash` lines (a zeroed hash
  installs fine, even with a cold cache); use `uv pip install
  --require-hashes` into a venv. `mv dir existing-dir` nests rather than
  replaces: move an invalid cache entry aside before publishing. Package
  metadata can outlive a deleted package, so the cache check also starts
  `python -m dmgbuild --help` and imports ds_store/mac_alias. Enlarging the installer window shows white
  beyond the 640x400 picture: Finder ignores icvp backgroundColor* when a
  picture background is set (black values tested on isocast-m2-a, Light
  and Dark), so there is no .DS_Store fix.
  Over SSH, a test Mac's appearance can be switched without osascript via
  SkyLight's SLSSetAppearanceThemeLegacy (dlsym; tiny Swift tool in
  ~/isocast-scratch/dmg-design/appearance on isocast-m2-a/-b).
- 2026-10-14 (resize-lag): **never drag by a TouchArea's own
  `mouse-x - pressed-x` when the drag moves that TouchArea** (splitters,
  dialog headers, scrollbar thumbs): the next reading already includes the
  last step, so the thing follows about half the pointer and shakes.
  Record the press as `absolute-position + mouse-x` and measure against
  it. In Slint, setting a layout's preferred widths one by one re-lays the
  row between assignments: read every width first (`let`), then set; and a
  layout change under a pressed TouchArea in its press handler can lose
  the drag. Measuring drags: window captures leave the pointer out, so
  `--drag-test` marks it pure cyan, in the native layers for canvas drags
  (Slint's own frame reaches the screen a frame after them), and
  drag-lag.py picks the narrow cyan group nearest the last one (scaled
  test patterns show cyan columns). On isocast-m2-b (1024pt window) the
  docks are squeezed below their set widths; the window recorder can start
  after the drag began on a cold app, so rest offsets are also taken after
  the drag. Sporadic single 50-80 ms window frames show in every mode in
  both builds there (not caused by these drags). isocastd's frame with a
  gesture's final layout comes ~5 ms after the release on m2-b: a
  "select right after letting go" check must select on the next turn of
  the event loop (`--drag-mode resize-select`), or it can't see a held box
  bug. Every dock layout is checked offscreen by sweeping the 26
  combinations of shown docks (`--state hide:DOCK`, `drag:`), reading the
  splitters from the snapshot.

- 2026-10-14 (report worker): a Worker module may export only handlers;
  `export const` beside `export default` fails at startup ("Incorrect type
  for map entry"). Local `wrangler dev` honors a client-sent
  `CF-Connecting-IP`, so the tests can use one IP per case. Rate limits are
  simulated locally (3/min per install shows [201,201,201,429]). The
  isocast-diagnostics crate isn't in default-members: run
  `cargo test -p isocast-diagnostics` for its scan test.

- 2026-10-14 (QA-3 R1): filling an off-air ISO's entire audio gap only when
  its next sample arrives (or at Stop) bursts 10 ms silence blocks into the
  native writer's bounded 64-buffer queue. Rejections advanced the logical
  sample count anyway, and AAC concatenated the remaining audio: 48 s video
  became 28.91 s audio after a 32 s gap. An 18 s take with shorter gaps did
  not shorten audio, only falsely counted gaps as dropouts. Pad progressively;
  extending only the MOV end time does not restore the missing audio/sync.

- 2026-10-14 (adaptive verification): don't reuse workspace artifacts blindly
  after `git archive main` into a different source folder with the same
  CARGO_TARGET_DIR. The first baseline reused merged-branch engine/protocol
  artifacts (its status wrongly contained compositor wall/wake fields absent
  from main). That trial was discarded; clean only the owned workspace
  packages, rebuild and verify the runtime shape and hash. Final main is
  `d915ca73...`, candidate `10e6ca00...`; hashes and full paths are in REPORT.md.

- 2026-10-14 (pacing review finding 2): residual launchd loss is **whole-thread
  blackouts, not timer leeway**. An engine-free 60 Hz NOTE_CRITICAL probe on
  M2 stops running for ~50–70 ms every 625–628 slots (~10.43 s); its overdue
  deadlines then wake together. M4 has sporadic similar stalls, M2b stronger
  ones (a two-hour daemon take lost 1.6%, longest gap 1.9 s). The existing
  >3-frame skip rule turns each blackout into aligned 1–3-slot losses at
  60 fps; 30 fps can absorb M2's ~70 ms event. NOTE_CRITICAL cannot override
  this scheduling blackout. Do not tune leeway or relax integrity/drop
  tolerance to chase it. Heavy Background ISO loss is a second limitation:
  all daemon CPU is clamped to background QoS, so ISO prepare/encode/write
  can fall behind while the program's wake p99 looks healthy. Report maxima
  and >1-period counts for program **and ISO** clocks; p99 misses rare stalls.

- 2026-10-13: **A.6 redaction verification found two real edge cases:**
  `Authorization: Bearer VALUE` initially hid only "Bearer", and uppercase
  `RTMPS://` initially kept its path. Both are fixed, as are window titles
  embedded in ScreenCaptureKit's `no window ...; windows: ...` error. No
  real credentials were used. Redaction runs before persistence and again
  on report collection; a bounded warning summary preserves method/cause
  rather than replacing a large API error with just "request completed".

- 2026-10-13: **A.6 killed-encoder check.** Killing the VT encoder whose
  `launchctl procinfo` responsible PID/path matched this isolated daemon
  produced AVAssetWriter errors **AVFoundation -11883 / OSStatus -12912**
  in the stopped event, final status and report bundle. A status only three
  seconds after the kill was too early: finalization reported the failure
  later. The stream stalled and reported 439 lost audio chunks, but no
  explicit `stream.last_error` (existing engine behavior; no recovery fix
  claimed here). Logs/metrics expose the stopped VT PID, in-flight queue,
  stalled callbacks, drops and writer error. Also: libproc's NULL-buffer
  FD query is capacity, not count (45 slots for a three-FD Python process);
  diagnostics now counts the rows actually returned by PROC_PIDLISTFDS.

- 2026-10-13: **A.6 native crash reports.** A background signed isocastd
  panic on isocast-m2-b produced its `.ips` in
  `/Library/Logs/DiagnosticReports` (readable to ec2-user), about 34 seconds
  after abort, not in `~/Library/Logs/DiagnosticReports`. Report collection
  checks both, parses the `.ips` header/body as separate JSON values, and
  records omitted/unreadable files. Generate a report again after macOS
  finishes the native crash report if only the immediate Rust crash file
  appears. GPU user clients are registry children of AGXAccelerator, not
  matches from IOServiceGetMatchingServices("AGXDeviceUserClient").
- 2026-10-13 (G.3 R1): a pause at **exactly** 2.000 s removes the sample
  needed to close the first fragment; 3/3 outputs can be unreadable. At
  4.000 s only 2 s survive. Immediate SIGKILL after a successful 2.050 s
  pause under 4K load likewise left all three unreadable in three trials;
  at 4.050 s each retained only 2 s. Even after 350 ms outputs can disagree
  (one ISO 2 s, others ~4 s). A recovered 2 s video fragment had 1.961333 s
  AAC, not a common exact A/V bound. Successful Resume at 3.7 s still left
  only 2 s on immediate kill. Request replies, metadata checkpoints and
  input progress must never be described as media durability evidence.

- 2026-10-14: An accessible Text inside a button makes an accessibility
  hit test (cua-driver's background click, VoiceOver) land on the text,
  whose press does nothing; give such labels `accessible-role: none`.
  Inside a `for` element, `parent.foo` doesn't reach a property declared
  on the repeated element: name it (`tick := Rectangle`). A dragged
  element's own `mouse-x` moves with it: measure drags in window
  coordinates (`absolute-position.x + mouse-x`). On the test-Mac bridge,
  a job whose children inherit its stdout (a lock holder, the app) keeps
  the queue busy until they exit: send their output elsewhere.
- 2026-10-14: README screenshots at 1440x900 on isocast-m2-b: its screen
  is only 960x720 points at 2x, and cua-driver's set_window_frame can't
  make a window bigger than the screen ("did not settle"). take.sh now adds
  a temporary 1440x960-point HiDPI virtual display
  (scripts/screenshots/virtual-display.m, CoreGraphics' undeclared
  CGVirtualDisplay classes; it lasts while the process runs) and puts the
  app window there, giving 2880x1800 captures. cua-driver also ends idle
  sessions mid-run, so every call now gets a fresh session label. Images
  are quantized with libimagequant (via uv) to about a third of their size
  without visible banding.
- 2026-10-13 (G.3 review): AVAssetWriter writes a movie fragment only when
  a sample at or past the fragment's end (on media time) is appended, never
  on wall time. Native probe on exonbook, H.264, 2 s interval, real-time
  input: appending through 3.7 s or exactly 4.0 s then idling 8 s leaves
  120 frames recoverable; appending one frame at 4.0 s writes the 2–4 s
  fragment (240 frames in that probe). This does not establish a durable
  lower bound: queued encoders can lag, and AAC's fragment timing differs
  (see R1 above). Probe source:
  /Volumes/IsocastBuild/g3-pause/scratch/f1probe.
- 2026-10-13 (G.3 review, F6, owner: iso_frames): a 30 fps ISO on a 60 fps
  engine sometimes alternates duplicated and skipped source frames (counter
  steps 0, 2, 0, 2…) while reporting 0 dropped and 0 repeated. Reproduced on
  frozen main ace7e0a without pauses (reviewer: 2 of 4 takes); likely a
  phase tie in ISO frame selection. It also blurs pause-join counter checks
  on 30 fps files (a join can read expected+1), so G.3 join checks use the
  60 fps files.
- 2026-10-13 (G.3 verification): a looping 997 Hz WAV with `offset_ms =
  -80` has short silences in the **program mix**, although both ISO audio
  tracks are continuous and reports say zero dropouts. Reproduced with no
  pauses on frozen main ace7e0a and G.3: roughly six 15–57 ms quiet spans/s
  in each program, e.g. 4.016–4.062 s in both. This is upstream of the new
  recording cut/fade (which begins only 5 ms before the cut), not introduced
  or repaired by G.3. Don't interpret zero reported dropouts as proof of
  continuous media audio with a negative offset. Baseline/candidate files
  and results: exonbook `~/isocast-scratch/g3-pause/offset-control/`;
  summary `/Volumes/IsocastBuild/g3-pause/scratch/offset-control.log`.

- 2026-10-13 (engine-e2e): `start`/`split` return media paths, not the
  manifest; match its files. A split reply precedes `begins_at`: a Stop
  before that boundary cancels the pending take. Pixel averages need FFmpeg
  `scale=1:1:flags=area` (the default scaler is not a full-frame mean).
  Explicitly demux generated beep WAVs with `-f wav`: FFmpeg can mistake
  their periodic bytes for MPEG-TS. macOS sometimes returns EPERM when
  signaling a group of dead native children; the harness checks for live
  non-zombie members before treating that as harmless, and cleanup errors
  cannot suppress the JSON report. Three signal-race repetitions pass.

- 2026-10-13: Screenshots on the test Macs: the bridge can't run
  `screencapture` ("could not create image from display": no Screen
  Recording grant); cua-driver's `get_window_state` with
  `include_accessibility_tree:false, max_image_dimension:0,
  screenshot_out_file` gives the window at native scale, but with
  transparent margins and sometimes the pointer in them, so
  scripts/screenshots/take.sh trims to the mostly-opaque rows and columns.
  isocast-m2-b's display is 1024x768 at 1x; its largest 2x mode is 960x720
  (mode 2; 1x is mode 3), which scripts/screenshots/display-mode.swift
  switches for the session. Terminal's `.command` files don't get the
  caller's environment (pass it in a file), and zsh's PROMPT shows the
  host, so the demo prints its own prompt.
- 2026-10-13: **Unset look-ahead is not an explicit zero.** On the M4 Max,
  `SuggestedLookAheadFrameCount` reads back as zero even when automatic
  mode stalls simultaneous HEVC sessions. Four deterministically ordered
  native A/Bs: twelve seconds of input took 14.722/14.759/11.989/15.398 s
  by default, versus 11.987–11.991 s after explicitly setting zero.
  `ReferenceBufferCount` was unsupported (-12900); delaying session
  creation and isolating ISO writers into processes did not fix the real
  flagship. A 100 ms delay before publishing pressure lost protected M2
  frames and was removed. Recording-only eager mixing did not fix throughput
  or Stop and was also removed; no mixer/filter changes ship from this round.

- 2026-10-13: **Overloaded AVAssetWriter crash fragments and B-frames.**
  Normally finalized files can be fine while their unfinished fragments
  present pictures out of order: an overloaded ISO moved from 2.116667 to
  2.033333 s after SIGKILL. Packet DTS stayed monotonic, but decoded PTS
  did not; changing FFmpeg's time base/passthrough options did not fix it.
  Disabling frame reordering fixed a matched four-file overload control
  and the final six killed files. Do not dismiss the null-muxer's warnings
  without checking decoded PTS. A raw-input end marker failed finalization
  (-12780); a long duration on the last raw picture was not reliably kept
  through HEVC reordering. Both approaches were rejected before counted
  tail repetition was qualified.
- 2026-10-13 (C.1 fixes): frame times are whole nanoseconds rounded down
  (`Time::add_ticks`), and the program's frames count from the live
  engine's clock, not the session's. So a split must fall on a moment a
  whole number of nanoseconds from the session's epoch that is also on
  every file's frame grid: whole seconds (`split`), or 1/gcd(1e9, 48000,
  every fps) (`retake`: 1/10 s for 60 and 30 fps, 1/8 s for 24). A first
  try on the 1/30 s grid made each ISO file of the next take a frame long
  and reported 1 dropped frame (file_end rounded up across the 1 ns), and
  the program's first frame 1 ns late.
- 2026-10-13 (C.1 fixes): a split asked for after the recording's loop
  has ended used to wait for the 30 s start timeout (its reply sat in the
  live engine's control, which outlives the session). The control now
  refuses splits once the loop ends (and answers a waiting one), and the
  loop takes a stop asked for at the same time before a split.
- 2026-10-13 (C.1 fixes): an ISO file whose last frames were dropped while
  recording (8K120 overload) ends a little short in its take (2.933 of
  3.0 s), and so in a joined file when its clip is last; the movie and its
  audio are the full length and players hold the last picture. Not
  padded: holding a passthrough frame longer needs its presentation-order
  last sample retimed through B-frame reordering, or encoding.

- 2026-10-13 (C.1): AVAssetWriter with non-real-time inputs keeps its
  inputs interleaved: an input stops being ready when it is ahead of the
  others. Feeding a join part by part (all of one file's video, then its
  audio) deadlocks at the first part's end; each track must be its own
  stream across all parts, appended whenever its input is ready.
- 2026-10-13 (C.1): `recording.iso.audio` goes into every ISO file, so
  "one ISO with audio, one without" can't be set up; the timeline checks
  joined ISO files with audio (both) and video-only ISO files in separate
  runs. A take ended by `stop` ends between frames: each file rounds up to
  its own grid (a 30 fps file can end 1/60 s after a 60 fps one), which is
  why clips use the program's length (Decisions).
- 2026-10-13 (C.1): FFmpeg reads a QuickTime audio track with one edit per
  joined AAC segment (priming skipped in each) ~20 ms off from where
  AVFoundation plays it (silencedetect vs an AVAssetReader decode of the
  same file). Check joined audio with both.
- 2026-10-13: **Hardened host capture (review #2).** The current dev app's
  signature has flags=0 (no hardened runtime); the new camera host enables
  runtime protection. It needs device.camera and device.audio-input, with
  capture usage strings. Screen capture uses TCC, not an invented entitlement.
  Wasmtime 49's actual mmap implementation publishes ordinary pages with
  mprotect(RX), so allow-jit alone is insufficient: the old hardened subset
  died at plugin add. Add allow-unsigned-executable-memory (retain allow-jit);
  no disable-library-validation exception was needed. With the corrected
  capture/runtime subset, the signed scratch host launches, runs wasm gain
  with 0 failures, and records 240/240 HEVC frames with A/V both 4.000 s,
  bt709 tags and full decode. This omits restricted install/group entitlements
  because real profiles are unavailable; do not call it a deployed-host test.
- 2026-10-13: **Review repro measurements**, M2 synthetic/isolated/common
  lock. Original reader-stall Stop 156.6 ms blocked status for 139.8 ms;
  fixed Stop 159.6 ms leaves status at 2.21 ms. Native child faults: open
  2.009 s, close 0.517 s, frame exchange 250 ms budget; concurrent status
  <=4.15 ms; recordings unaffected, child gone after parent kill. Reviewer
  42-cycle lifecycle and 3-pair churn repeated: all six MOVs 1800 frames,
  A/V 30.000 s, all MOV/FLV decodes clean, 0 recording/stream drops.
  Lifecycle vmmap phases still pause the process (~0.53 s); they are not
  latency/drop qualification. Evidence: REPORT.md → Review fixes under
  `/Volumes/IsocastBuild/g1-vcam/scratch/review-fixes/`.

- 2026-10-13: **G.1 Follow-up 1 build checks.** The XcodeGen output parent
  directory must exist before `generate --project`; use `--no-env` so it
  preserves the Rust-copy phase's shell variables. An application target
  with no compiled sources works: its script supplies isocastd before final
  signing. The wrapped and Cargo binaries' `__text` bytes match. Ad-hoc
  strict/deep signature verification does **not** prove deployability:
  the restricted-entitlement host receives SIGKILL on isocast-m2-a without
  profiles; the raw daemon's AVFoundation discovery runs and correctly
  reports Isocast absent. The normal verifier rejects the ad-hoc product.
  Xcode's ordinary build logs scratch-product Launch Services/execution-policy
  registration; no system-extension activation is involved. No Apple
  account update, real-identity signing or /Applications copy was performed.
  `AVCaptureSessionPresetInputPriority` is unavailable on macOS in the
  actual SDK (some indexed docs say otherwise); the probe requests the
  supported 1920x1080 preset. Full moving-camera capture and provisioning
  remain unverified until Ryan runs the installer.

- 2026-10-13: **G.1a verification limits.** All four targets, live edits,
  60 fps PTS, Rec.709 sample tags, portrait letterboxing, collection
  persistence, CLI/MCP/events and an intentionally stalled reader were
  exercised with a real separate-process Mach/IOSurface reader. CMIO's
  native queue ownership, sandbox and actual consumer retention still need
  installed-extension verification. Program-only 1080p recording + local
  720p RTMP + portrait camera: file **480/480 frames, A/V both 8.000 s**;
  stream **541 frames, 0 video/audio drops**; camera **289 frames, 60 fps**.
  MOV and received FLV fully decode and retain bt709 tags. A heavier
  flagship-1080 + extra stream run was overloaded: program **295/480**
  frames, video **7.966667 s** vs audio **8 s**, stream **190 video/221 audio**
  drops, camera **44.2 fps**. Preserved as a limit, not qualified as clean
  coexistence; S.2c/A.4 owns overload. Raw outputs under
  `~/isocast-scratch/g1-vcam/coexist-1791914609` (heavy) and
  `coexist-1791914842` (program-only) on isocast-m2-a; JSON/decode logs
  copied beside the report. `vcam-probe` is explicitly diagnostic and
  labels its status `diagnostic-iosurface`; never claim it registered a
  camera. A cached Mach import keeps a CVPixelBuffer pool surface in use:
  the probe drops each import before acknowledging it.
  Final lifecycle check: after an induced reader failure, retry without
  Stop reports the new missing-reader error; restoring the reader restarts
  cleanly (121 frames, no drops). Start joins the finished output before
  establishing new source demand, avoiding stale status/late cleanup.
- 2026-10-13: Fonts a cut lighter are narrower too: Mona Sans Light sets
  lines ~2% narrower than Regular (and than Paper's), Hubot Sans Medium
  ~4% narrower than SemiBold. Wrapped text can then break later than on
  a board (the kit gallery's intro now fits one line, so every band
  below sits 22px higher than the Components board's), and pixel scores
  against the boards (pairs.sh's MAD) rise from misregistered glyphs
  even as the weight converges: judge weight by ink
  (/Volumes/IsocastBuild/console-fix/tools/ink2.py), layout by rows.
- 2026-10-13: Slint 1.18: `accessible-role` must be a constant (no
  `cond ? a : b`); a Text's min-width is its preferred width even with
  `overflow: elide` (set `min-width: 0px` for a label that may shorten:
  Button `shrinks`); a binding that reads the width an element gets from
  its layout and changes what it contains is a loop even when the
  containing layout's min/preferred are set explicitly (a field sized
  from its band's width; Marker shown by the dock's width): let the
  layout share the room (min/preferred/max on the child) instead. Key
  events a focused item rejects go up to its ancestors' FocusScopes, so
  a FocusScope with `focus-on-tab-navigation: false` and
  `focus-on-click: false` wrapping a group takes keys for all of it (a
  mixer strip's ⇧F10).
- 2026-10-13: Isolated engines need socket names nobody else picks: the
  function critic's `/tmp/isocast-cfd.sock` and `-cfw` were also the
  names console-fix first chose (its own now: `-cfx*`). Check with
  `lsof /tmp/isocast-NAME.sock` before starting one.

- 2026-10-13: cua-driver's foreground `click` and `right_click` warp the
  pointer to the target without a mouse-moved event, and winit/Slint
  take the press at the last position they saw: a click right after
  another one on a different control lands on the first (on
  isocast-m2-b the Studio column's Transition, Fade and Cut "didn't
  work", a source row's right-click selected nothing, the dropdown
  "didn't reopen" after Escape; main's 9cda72d build the same). Real
  pointers move first, so the app is fine. Click with a 1px `drag`
  (from x,y to x+1,y: it posts the moves) or the accessibility press;
  /Volumes/IsocastBuild/console-integrate/remote/d.py. Right-clicks
  after other input stayed unreliable even after a 1px drag; each
  context menu opened as the first input after a start.
- 2026-10-13: A meter anywhere in the window costs a whole-window redraw
  per step (Slint has no partial redraw on wgpu; ~0.45% of a base M2
  core per redraw a second at 1440x900). The Console control bar's
  master level kept the window redrawing ~29 times a second while sound
  played (17-18% with the mixer hidden, main 4%), and quantising it to
  pixels at 15 Hz still left 10-11%: the meter's ballistic fall between
  level updates moves it every tick. Since N.5.2 it is a native layer
  under the window in layers mode (layers.rs `Meter`; the bar leaves a
  hole for it), 4.6%. Anything else that must move often belongs in a
  layer the same way.

- 2026-10-13: Slint fits a Path's viewbox into the element *less its
  stroke width* and offsets it by half the stroke (i-slint-core
  items/path.rs `fitted_path_events`), so a stroked 24-unit icon drew
  ~10% smaller than the SVG (16px icon, 1.5px stroke: the drawing spanned
  14.5px). The kit's `SvgPath` (ui/icons.slint) widens the viewbox by
  the stroke's share; draw any new stroked path with it, not `Path`.
- 2026-10-13: Out of a layout, a Slint element without a width takes its
  parent's, even one with a preferred width (the Sources dock's ISO badge
  was 40px, its lane's): set `width: self.preferred-width`.
- 2026-10-13: Offscreen snapshots run one process each: the undo history
  and the layout prefs (docks shown, Studio Mode) don't carry from one
  `--snapshot` to the next, but the isolated engine's state does (a
  recording started by `press:Start Recording` keeps running, a
  `transition:fade` stays set, Studio Mode's preview scene stays).
  Check undo or the pane toggles inside one run; stop or reset engine
  state after a run (/Volumes/IsocastBuild/console-main/scripts/beh.py).
- 2026-10-13: Paper's PNG exports carry a Display P3 profile (`danger`
  #FF4D43 reads (236,91,77)); convert them to sRGB before comparing
  pixels with the app's snapshots (PIL ImageCms; console-main's
  compare.py and crop.py and console-dialogs' compare.py do; the yellow
  #EDB93E reads #E5BB56 raw), then the colours match exactly.
- 2026-10-13: Slint 1.18 rounds a Text's layout height up to the next
  pixel (items/text.rs: `.ceil()`), and the line is font size × the
  face's natural ratio × `line-height-factor` in f32: 13 × 1.41 × 0.982
  is 18.0001, so the text was 19px, not Paper's 18 (Settings' page sat
  1px low under its header and gained 1px under every caption). The
  leading tokens are now the largest four-place factor whose line stays
  at or under the height (0.9819, 0.9456, 0.9026, 1.0606 for Martian
  Mono); a new one must be checked the same way (13 × 1.41 × f ≤ 18).
  A Text without a factor is its natural height rounded up (12px Mona
  Sans: 17, not Paper's 16): give single lines their `lh-*` height.
- 2026-10-13: A layout placed inside a 1px edge (`x: 1px; y: 1px; width:
  parent.width - 2px`) without `height: parent.height - 2px` is as tall
  as its parent, so it shares out the edge's 2px among its children:
  Welcome's buttons sat 1px low and LcdHero's digits half a pixel low.
  Padding inside such a layout is Paper's own (border-box), not less 1px.
- 2026-10-13: `--state press:NAME` can't reach anything in a dialog: the
  testing backend's search leaves out popups, and sheets live in one.
  Click by where `--a11y` says the control is instead (it lists popups):
  /Volumes/IsocastBuild/console-dialogs/scripts/behave.sh's `at`. A
  layout's numbers can be read the same way: give an element a
  temporary `accessible-description` built from `preferred-height`s.

- 2026-10-13: Slint 1.18's Skia text is heavier than Paper's (WebKit,
  `-webkit-font-smoothing: antialiased`) at the same weight and width:
  "Intro Clip, Desk Camera" (Mona Sans 13/400) is 142px wide in both, but
  its ink sums 324 in Paper's 1x and 2x exports, 497 in the 1x snapshot
  and 457 at 2x (hb-view's grayscale render of the same cut: 355). The
  weight is right (Medium would be 144px wide). `-AppleFontSmoothing 0`
  on the command line changed nothing; Slint draws glyphs with
  `skia_safe::Font::set_subpixel(true)` and the CoreText font manager,
  and offers no switch. Worked around since (N.5.2, Decisions): each
  Paper weight is drawn with the next lighter static cut (Theme's
  `weight-*`), which brings the ink to 1.02-1.28x Paper's; a patched
  renderer remains the exact fix.
- 2026-10-13: Slint layout traps met building the Console gallery: an
  item of fixed height in a HorizontalLayout sits at the top of a taller
  row (wrap it in `VerticalLayout { alignment: center; }`); a `for` of
  layouts in a HorizontalLayout stretches each one across the row
  (layouts stretch by default: set the group's `alignment`); a Text's
  preferred height is the face's own line (Mona Sans 1.41em: 15.5px at
  11px, not Paper's 14px), so stacked single-line labels need their
  `lh-*` heights or rows grow 1-2px each; equal columns need
  `preferred-width: 0px` as well as `min-width: 0px`; an element inside
  an `if` can't be read from outside it (the sheet's header height is
  computed from tokens); adding a public `shown` to NumberField collided
  with a private one of the same name ("Cannot override property"); an
  `Icon` (it inherits Path) can take `stroke-width` per instance; arrays
  of struct literals with different fields convert to `[MenuEntry]`.

- 2026-10-13: Never hand VideoToolbox's H.264/HEVC encoders BGRA. Its
  own BGRA to Y'CbCr transfer runs inside the encoder service, and with
  16+ such sessions (ISO files of test patterns, colours, text, images,
  or filtered ISOs) every encoder thread and the service's AVE thread
  wait on each other's synchronous XPC calls: the whole recording froze
  after 0.8 s (main 28ddad1: iso16 2/2, iso16-noprog, isofiltered16).
  Such a daemon can ignore shutdown and SIGTERM (1 of 4 here needed
  SIGKILL), and while it hangs another engine's VT sessions can fail
  (error -12903). ISO files now get 4:2:0 frames converted on the GPU
  (record/iso_frames.rs); colour within 1 code value of VT's own
  conversion; ProRes ISOs still get BGRA. 16 1080p60 HEVC ISOs plus the
  program (17 sessions) exceed the M5 Max's encoders: 8 files drop ~142
  frames (10%); 16 without the program are clean.
- 2026-10-13: A file that repeats the last picture hides lost frames, in
  Isocast too. On main, BGRA ISO sources whose writers hold their pool's
  frames stop producing, and the files repeat old pictures that never
  count as dropped (isoh264-8 on 28ddad1: 153 reported, 4 ISO files
  repeat 96-112 pictures each, ~565 lost). The converted ISO path drops
  and counts instead. Check repeats with distinct.py (YDIF < 0.1) and
  gaps with gaps.py; to compare colour of a scrolling test pattern,
  match frames by their FRAME counter (barmatch.py): tools in
  /Volumes/IsocastBuild/scale-port/tools (stress.py there SIGKILLs its
  own wedged daemon between runs).
- 2026-10-13: Slint app snapshots: a dialog field's auto-focus selects
  its text again on a 1 ms timer, which only fires when the snapshot's
  clock moves; `--state type:…` right after opening a dialog loses its
  first character (the re-select replaces it). Put a state that advances
  the clock first: `--state dialog:… --state hover:1,1 --state type:…`.
- 2026-10-13: The Slint app's open and save panels (Import…, Export…,
  Properties' file fields) are light: unlike the web app, it doesn't set
  the dark appearance on NSApp. For N.3. The first panel after launch
  takes ~3-4 s to appear on the M2s (the panel service starting); a menu
  command chosen meanwhile is dropped, not queued.
- 2026-10-13: On the test Macs, `ssh host 'nohup cmd &'` dies with the
  session ("nohup: can't detach from console"); `ssh host 'sh -c "cmd
  >/dev/null 2>&1 </dev/null &"'` keeps running (used to hold the bench
  lock while driving windows).
- 2026-10-13: Check a test Mac's busiest processes (`ps -Ao %cpu,etime,command
  -r | head`), not just the bench lock, before measuring: on isocast-m2-a two
  orphaned `ffmpeg -v error -i <recording>.mov -f null -` decodes (PPID 1,
  left by a finished check; one reading a file already deleted) had spun
  at ~95% of a core each for 35 and 73 minutes; SIGTERM didn't stop them,
  SIGKILL did (5.2a, 18:43 UTC). Put a time limit on decodes that check
  integrity.
- 2026-10-13: wgpu's `queue.write_buffer` makes a staging buffer per call
  (IOGPUResourceCreate) and copies into fresh memory: fine for a few
  records, but a profile of 1,000 animated records (272 KB a tick) had
  it at ~150 µs a tick on the M5 Max, more than the animation pass. A
  `wgpu::util::StagingBelt` written into the frame's encoder reuses
  mapped buffers (they come back on the device poll the compositor makes
  each frame). And `bytes.extend_from_slice` per float is slow:
  `as_chunks_mut::<4>()` over a sized buffer.
- 2026-10-13: Scene transitions start at the switch's time, between ticks,
  so their frames differ run to run; to compare builds, check each frame's
  progress against the formula. Item show/hide transitions start at the
  adopting tick, so their frames compare byte for byte across builds
  (trans_check.py in the 5.2a scratch). An apply's reply comes after the
  change is drawn (the setup is saved first): a marker sent after it lands
  ~3 frames after the first frame showing the change.

- 2026-10-13: Measuring when a change shows in a recording (5.5a): the
  program draws the frame for tick T one frame after T (`run_program`
  sleeps until `tick + frame`), so the first frame showing a change made
  at time m has pts ≈ m − 1 frame + resolve time: latencies against pts
  come out negative; add a frame for when it was drawn. Record the
  program as ProRes for such checks (intra-only: an unchanged region is
  the same bytes frame after frame, so `framemd5` of a crop changes
  exactly once per change), and give ffmpeg `-map 0:v:0 -an`, or
  framemd5 interleaves the audio track's packets with the frames. A
  marker sent right after the change (~0.1 ms later) puts the change on
  the session timeline.
- 2026-10-13: Content items aren't sources, so neither app lists or
  edits them as such yet: a content item shows in the Sources dock like a
  box (no source). P.2/N.x: show content items and edit their text.
- 2026-10-13: Don't read a recording engine's memory with `vmmap`: it
  suspends the process while it looks (the program missed ~30 ticks per
  call and dropped frames). `top -l 1 -pid PID -stats pid,mem` gives the
  footprint without stopping it. And a client pacing calls with a busy
  loop raises the Mac's clocks, which makes every other CPU time look
  lower (the compositor's per-tick time fell 584 → 63 µs): pace with
  sleeps.
- 2026-10-13: The live engine adopts new compiled scenes even while it
  draws nothing (no recording, stream or monitor), and the GPU then gets
  no submissions: queue writes and released textures pile up until one.
  Anything an adopt does on the GPU must submit (an empty submit and a
  non-blocking poll, as `DrawList::adopt` does for content) or it grows
  without bound (5.5a: ~33 MB a second with bound text flooded).
- 2026-10-13: Drawing a 1200×160 text picture (96 px bold, black
  background) takes ~5 ms on a base M2 at rest (both M2s, median 4.6–5.1
  ms, bimodal 2.4/5.4 ms: low clocks or an efficiency core) and 0.7 ms
  when something keeps the clocks up (a client spinning a core): over
  the design's 2 ms for a line. Not page faults (keeping the float canvas
  between pictures: no change) and not the scene thread's QoS
  (user-interactive: no change); both tried and undone. The cost follows
  the box's area, not the text: text draws on a float canvas (16 bytes a
  pixel), filled, then converted pixel by pixel with three divisions to
  straight 8-bit RGBA. Next (S.3, or 5.5b with chat): draw in 8-bit
  premultiplied and convert only where glyphs landed.

- 2026-10-13: A Rust guest for wasm32-wasip2 built with std imports
  fourteen WASI interfaces (clocks, streams, stdio, environment, exit)
  even if it never uses them, so Isocast refuses it. Build `no_std`: then
  wit-bindgen (without its `std` feature) doesn't link `cabi_realloc` on
  p2 (it expects wasi-libc's), and string compares need `memcmp`; the
  example's `support.rs` defines both, a global allocator and a trapping
  panic handler. The target: `rustup target add wasm32-wasip2` (7.4a used
  a scratch `RUSTUP_HOME` on /Volumes/IsocastBuild rather than changing
  Ryan's toolchain).
- 2026-10-13: `ffmpeg -ac 1` on stereo sums the channels at 0.707 each, so
  identical L and R read 3 dB louder; to read a level, take one channel
  (`-af pan=mono|c0=c0`).
- 2026-10-13: Items placed by `layout` (5.1) have no `rect`. Both apps
  read a missing rect as an empty one at the top left and won't drag,
  nudge or resize them. The web app's Edit Transform says "placed by its
  layout"; the Slint app's Edit Transform and Transform menu still offer
  edits that validation refuses ("give rect or layout, not both"). N.3:
  show the web app's note there, and draw layout items' boxes from
  `get --resolved`.

- 2026-10-13: Rare stream drops on isocast-m2-a (macOS 27, base M2), in
  main and in candidates alike, about 1 run in 6-11: each bad run (6-11
  dropped frames, 12-29 audio dropouts) coincides with a VideoToolbox
  encoder/decoder service process starting or exiting during the window
  (bench.py prints "VT None"). No crash reports. Not caused by S.3's
  stop change (main shows it too). Round-2 work: find what makes the VT
  services churn mid-stream and ride through it.
- 2026-10-13: taffy (0.10): `Style::DEFAULT.display` is Flex, not Block;
  percentages are 0..1 (`Dimension::percent(0.5)`); `TaffyTree<Ctx>` has
  no `Default` unless `Ctx = ()`; a node made with children never gets
  its measure function called (a picture with children needs its size
  or aspect ratio set up front).
- 2026-10-13: Checking the program's picture (4:2:0) by RGB fails at
  edges: chroma is shared by pixel pairs, so the pixels either side of an
  edge get tinted. Compare luma (0.2126 R + 0.7152 G + 0.0722 B) bands,
  and pick colours whose luma bands don't meet the tints (blue's 18 is
  too close to black's tinted neighbours; white works).
- 2026-10-13: The scene thread's resolve time (`scenes.resolve_us`) is
  mostly running cold, not layout: timed by phase in the engine
  (exonbook, a 50-item flex scene, after each change) clone 18-80 us,
  layout 38-130, compile 23-72, every phase 5-10x its warm time (a loop
  on an M5 Max: 4, 12 and 6 us); 1,000 items run about 2x their warm
  times (clone 120, layout 390, compile 190 us). Medians on
  isocast-m2-a move between sessions (50 items 90, 114 and 205 us in
  three), so compare builds back to back (resolve_ab.sh).

- 2026-10-13: WGSL: a user function named `read` breaks
  `var<storage, read>` ("identifier `read` resolves to a declaration"); the
  draw shader's sampler helper is `picture_at`. Shader errors only show
  when isocastd creates the module (the engine panics at start: see
  isocastd.log), so start an engine after touching render.rs.
- 2026-10-13: Builds from `git archive` (main for an A/B) into a target
  folder that already has newer artifacts: the archive's mtimes are the
  commit's, so cargo may skip recompiling; touch the sources first.
- 2026-10-13: Seen while checking 5.0, the same on main (not a 5.0
  change, not investigated): an item without a source (a box with a
  background) hidden with a `slide` hide_transition disappears at once
  instead of staying for the slide; with `fade` it fades over its
  duration, and a colour source slides as it should (4.12d's check).
- 2026-10-13: Isolated engines whose socket is named after a folder's last
  component (`/tmp/x-<name>.sock`) collide when two runs use folders with
  the same name: the second run talks to the first daemon (and its setup).
- 2026-10-13: Seatbelt can't hold a process to serving on loopback: SBPL
  hosts can only be `*` or `localhost`, and `(allow network-bind
  network-inbound (local ip "localhost:*"))` also admits a bind to 0.0.0.0
  and connections arriving on the LAN address (tested on isocast-m2-a).
  Process plugins must bind 127.0.0.1 themselves (the deck does). Seatbelt
  also matches canonical paths only (`/private/tmp/...`, not `/tmp/...`).
  A plugin run by macOS's /usr/bin/python3 stub (an app-started isocastd's
  PATH finds it before Homebrew's) logs "couldn't create cache file
  .../T/xcrun_db-..." (writes only go to its data folder): harmless, the
  deck ran on Xcode's Python 3.9 on exonbook.
- 2026-10-13: On Apple silicon a busy core elsewhere lowers a process's
  measured CPU time: with a plugin spinning a core, isocastd's recording of
  the same 1080p60 synthetic setup measured ~10% of a core instead of
  ~54% (identical files: 1800/900/1800 frames, 0 dropped), as the clusters
  run at higher clocks. Compare CPU only under matched background load.
- 2026-10-13: `scripts/testmac.sh` syncs the files in the index: a new
  file must be added to it (intent-to-add is enough) to reach the test Mac.

- 2026-10-13: A slow disk can't be told from an overloaded encoder by
  frame or byte counts: AVAssetWriter encodes and writes internally, so
  either way its inputs stop being ready and frames drop, and in steady
  state the bytes reaching the disk match the frames accepted. (Seen on a
  throttled disk image: the files grew a steady ~16 MB/s each from the
  start while ProRes LT needed ~25 MB/s; ~250 MB piled up in memory, then
  drops began at ~20 s, and the backlog kept writing ~7 s past the take.)
  What does tell them apart is how long a small write takes there: a
  4 KB write + plain fsync took 0.15-0.2 ms on the idle disk and 30-150 ms
  while it was behind. The recording measures that at take start and
  again while frames drop (`LiveStatus.disk`), and drop warnings name the
  disk when it is ≥10× slower and ≥5 ms. Don't use Rust's `sync_data`/
  `sync_all` for such probes on macOS: both are F_FULLFSYNC (flush the
  drive's cache).
- 2026-10-13: One daemon per socket is enforced by a lock, not by probing
  the socket: isocastd holds `<socket>.lock` (flock, pid inside) for its
  lifetime. A stalled daemon (SIGSTOP, or a full accept queue: 129 queued
  connects on macOS, then ECONNREFUSED) looks exactly like a stale socket,
  and before the lock a CLI call launched a second daemon that replaced
  the socket, orphaning a recording nothing could stop (chaos test T14).
  The client now treats a refused/missing socket with the lock held as
  "busy": it retries for 10 s, then says which pid isn't answering.
- 2026-10-13: Counting decoded frames can't see an encoder that falls
  behind if the app keeps the frame rate by repeating pictures: OBS does
  (its log: "skipped frames due to encoding lag: N/M"), so its 4K60 files
  on the base M2 had every frame by ffprobe's count but 27% distinct
  pictures. obs-bench counts repeats (ffmpeg signalstats YDIF < 0.1 at
  960 px wide; new frames of the bench clips measure 0.5-2): compare
  distinct frames, and do the same in any frame-loss check.
- 2026-10-13: Measuring processes on macOS (obs-bench): VideoToolbox runs
  each client's hardware encode/decode sessions in VTEncoderXPCService /
  VTDecoderXPCService processes of its own (launchd's children, not the
  app's), so the ones that appear after an app launches are its share;
  others serve the aerial wallpaper and other engines (~3% of a core on
  exonbook at rest). powermetrics' plist output has no per-process GPU
  time even with --show-process-gpu: read the AGX driver's counters,
  `ioreg -r -c AGXDeviceUserClient -l -w0` ("AppUsage" accumulatedGPUTime
  per "IOUserClientCreator" pid). RSS misses GPU memory on Apple silicon:
  use the physical footprint (`top -l 1 -stats pid,mem`). SIGINT sent to
  a `sudo -n powermetrics` child doesn't reach powermetrics: kill its pid
  with `sudo -n kill`.
- 2026-10-13: An isolated engine whose ISOCAST_SETUP file is rewritten
  between daemon starts (drag-lag.py writes and applies `setup`) gets a new
  collection and profile in `<file>/` each time the content changes: the
  file is taken in as newer work. Harmless in scratch, but that library
  grows; to start from a fixed setup, apply it (as bench.py does).
- 2026-10-13: Renaming the collection in use is an apply of the live
  setup's `name` (a new revision); renaming a profile writes its file, and
  later saves keep the name the file has (they read it back), so a live
  save that was waiting can't undo a rename.
- 2026-10-13: AudioToolbox's AAC encoder, given only
  `kAudioConverterEncodeBitRate`, is variable: 6-byte frames (2.2 kb/s)
  for silence and ~21 kb/s for near silence at a 160 target, which X's
  ingest flagged ("audio bitrate 17 kbps"). The stream encoder now sets
  `kAudioCodecPropertyBitRateControlMode` = constant (before the bitrate)
  and holds 160.0/128.0 kb/s on silence, quiet and loud tones. Recordings
  (AVAssetWriter, 128 kb/s per channel) stay variable on purpose: a tone
  file measured 22 kb/s. Measuring script (local RTMP listener, per-second
  audio/video kb/s, packet sizes): /Volumes/IsocastBuild/audio-cbr/acbr.py
  and matrix.py (results-cbr.jsonl beside them).
- 2026-10-13 (fixed on stress/soak-repair): on macOS 27 the live engine
  leaked autoreleased objects on its long-lived threads (media readers,
  compositor, encoders), which have no autorelease pool of their own:
  releasing an AVAssetReader left an NSError, ~9
  AVCMNotificationDispatcherListenerKeys and ~4
  AVRetainReleaseWeakReferences per loop of a looping media source, and
  each CVMetalTextureCache texture made for a new IOSurface left an
  MTLTextureDescriptorInternal and CFStrings. They stayed until the engine
  stopped (the threads' implicit pools drain at thread exit): +33-41 MB/h
  of live heap with 2 s loops. macOS 26.7 (isocast-m2-b) doesn't
  autorelease these, so main stays flat there. Rule: any platform call made
  from an engine thread that can autorelease (AVFoundation, CoreVideo's
  Metal cache, ImageIO) goes inside `autoreleasepool`, and so does the
  *release* of AVFoundation objects (fields drop after `Drop::drop`
  returns, outside its pool: release them inside it, as MediaReader does
  with ManuallyDrop).
- 2026-10-13: Measure memory growth with live heap bytes (`heap -s
  --forkCorpse <pid>`: "All zones: N nodes (B bytes)"), not footprint's
  Malloc or RSS: dirty malloc pages stay counted after they are freed, so
  they read as growth (or as a step) after a burst. MallocStackLogging=lite
  in the daemon's environment (the CLI passes its environment on) makes
  `heap -s` name non-objects by allocation site, e.g. "malloc in
  CMBlockBufferAppendMemoryBlock".
- 2026-10-13: A recording's live heap still grows ~6 MB/h after the
  autorelease fix (3 files at 60 fps; ~0.1 blocks/s of ~20 KB "malloc in
  CMBlockBufferAppendMemoryBlock", the same with audio off): AVAssetWriter
  keeps each file's sample tables for the movie header it writes at the
  end, about 15 bytes per sample. Freed when the recording stops; the cost
  of MOV/MP4, not a leak. Also ~0.5/s 80-byte "calloc in
  dispatch_group_async_f" blocks while recording, partly kept after stop
  (~150 more after a 15 min take, ~12 KB): too small to chase.
- 2026-10-13: `ffprobe -count_frames` decodes HEVC single-threaded (~61
  fps for 4K on the M5, ~1 h per hour of 4K60): add `-threads 4` (~300
  fps). Over plain SSH, `nohup … &` on a test Mac dies with the session
  ("can't detach from console"); start long jobs through `testmac.sh gui`.
  exFAT folders get `._*.session.json` AppleDouble files: skip dot-files
  when globbing manifests (the engine already does).
- 2026-10-13: Parallel stress agents on Ryan's Mac don't all take
  /Volumes/IsocastBuild/.bench.lock: a 90 s soak trial with another
  agent's engines busy dropped 52/335/363 frames (desktop/camera/program,
  363 missed program ticks) while the clean hour after it dropped none.
  Log other engines' CPU next to any number measured there. The repair's
  bench.py A/B had one run of each build drop ~70/330/350 frames
  (desktop/camera/program) at the same time as another agent's stress
  engines: compare alternating runs, never a single one.

- 2026-10-13: Don't SendMessage an agent that a running workflow started:
  replying to N.2's area C agent resumed a second copy of it (same id and
  transcript) outside the workflow, and both wrote into the same worktree.
  Answer workflow agents by editing the workflow (or after it ends); if a
  copy exists, give it a separate worktree and branch.

- 2026-10-13: /Volumes/IsocastBuild (a 120 GiB sparse bundle on the Extreme
  SSD) fills up when several agents build at once (each Slint/Skia release
  target is several GB): it hit 64 MB free with five N.2 area builds, and
  builds failed with "No space left on device". After merging a branch,
  delete its agent's target folder (literal paths); give each agent one
  release-only target. Growing the bundle (`hdiutil resize`) needs it
  unmounted, so do it only when no agent is running.

- 2026-10-12: A stream's queue used to start a new connection at the
  first keyframe pushed after it connected, which could be one made before
  (VideoToolbox and the audio interleaver hold frames for a while), so
  streams began 0.000/0.017 s with the forced keyframe a frame later and
  the grid from that. The queue now waits for the encoder to say which
  frame it forced (`SendQueue::start_at`) and drops anything earlier.
  Streaming test script (keyframe times, optional SIGSTOP stalls, a
  reconnect): /Volumes/IsocastBuild/v18-stream-start-keyframe/streamkf.py,
  run from a checkout on a test Mac after `scripts/testmac.sh build`.
- 2026-10-12: A `remote = "Self"` type (Source, Filter, Transition,
  ApplyParams, InitParams, ...) has an inherent `deserialize`, the raw
  derived one, which wins over the trait's: call
  `<T as Deserialize>::deserialize` from outside, or field paths are lost
  (MCP's init_setup lost them that way at first).
- 2026-10-12: Every apply during a recording repeats "<id>: the recording
  keeps the source as it was" for each pinned source whose definition
  differs, even when the apply only renamed the setup (40000 applies, 40000
  warnings). get_events copes (bounded), but `isocast watch` and the app
  see the spam; the engine should warn once per changed definition.
  Found by mcp-events; not fixed there.
- 2026-10-13: `scripts/testmac.sh` syncs tracked files only: add a new
  file to the index (intent-to-add is enough) before building on a test
  Mac, or it isn't there. rsync keeps the local mtime, so an edit saved in
  the same minute as the remote's last compile can look older than its
  build output and cargo skips it: touch the changed files before
  `testmac.sh build`. Its build line pipes cargo into `tail`, so a failed
  build still signs the old daemon: look for "Finished".

- 2026-10-12: Throughput follow-up, still blocked. The actual AVAssetWriter
  VT sessions already use RealTime=true, ExpectedFrameRate=60, hardware
  encoding, power-efficiency=false and 420v inputs. Failing M2 4K flagship
  callbacks complete ~42 frames/s, ~210 ms after submission, with nine
  outstanding inside VT and the writer's 32-frame queue full. Disabling
  reordering, changing VT real-time/speed/maximum-rate hints, and replacing
  AVF compression with a bounded asynchronous VT worker did not fix it.
  EncodeFrame itself can block ~97 ms despite asynchronous callbacks: it
  must not run on a thread that also feeds audio. The worker removed that
  audio regression but not the video loss; all exploratory code was removed.
  A native test's 960 frames were initially mistaken for real-time success;
  explicit wall timing found 20.30 s for 16 s of media. A larger submission
  window is not a demonstrated fix. Do not infer physical media-engine
  capacity from these traces: service scheduling vs hardware throughput
  remains unresolved. Existing warning events report drops during the take
  (first M2 warning at 2 s); no automatic resolution/rate fallback was added.
  Evidence, experimental patch and rerun commands:
  `/Volumes/IsocastBuild/astra-throughput/scratch/REPORT.md`.
- 2026-10-12: CPU comparisons need clock and instruction evidence. On both
  M2s, identical source build and byte-identical flagship-1080 clips, four
  interleaved baseline/diagnostic pairs each: all 48 files have 1440 frames,
  no audio drops. Baseline daemon CPU 51.0% on 27.0 vs 49.95% on 26.7, not
  the earlier 54% vs 35%; VT services 28.55% vs 24.55% remains (~16%).
  Powermetrics: daemon ~593M instructions/s on both, mostly E cores around
  1 GHz; VT encoder/decoder retire more instructions and cycles on 27.
  Profiling raised effective execution clocks to ~2.6 GHz and changed CPU
  percentages and drops: don't use sampled runs as performance baselines.
  Spindump's compositor times were nearly equal; 27's AAC threads used
  ~30% more CPU (only ~0.7 percentage point overall). No extra pixel-format
  conversion or Isocast busy-poll loop was established. The original 50%
  claim is not reproduced, and the residual system-framework cost is not
  fixed. M2-b has Command Line Tools but no `xcrun xctrace`.
- 2026-10-13: Slint 1.18 popups and focus: `show_popup` takes the window's
  focus item *after* making the popup's tree (so after its fields'
  `init`), keeps it, and gives the focus back to it when that popup
  closes. A dialog field that focused itself in `init` lost the focus at
  once, and a menu whose popup closed after the dialog it opened gave the
  focus back behind the dialog. Fields with `auto-focus` focus again on a
  1ms Timer; MenuHost closes its PopupWindow before reporting the item;
  `Menus.activated` runs on the next event-loop turn (src/app.rs).
- 2026-10-13: Slint's TouchArea grabs the pointer on any button's press
  and lets go on the release. A context menu opened on the press left
  the grab with the row under it; when the release went astray
  (cua-driver's background right-clicks on isocast-m2-b), the menu and
  any dialog it opened ignored the pointer until another press. Context
  menus open on the release now (lists, mixer, stage, monitor).
- 2026-10-13: Slint counts a 0×0 item as hidden (`ItemRc::is_visible`), and
  the testing backend's element walk skips the popups of hidden items, so
  `--a11y` never listed dialogs or menus; their hosts (Dialog, MenuHost)
  are 1px. A layout's own `min-height` replaces its children's minimum:
  FormRow's 28px let a short window squeeze Edit Transform's two crop
  lines into one (the label column carries the 28px now). A scroll area
  that narrows its content when it scrolls (the 8px bar gutter) is a
  binding loop if the content wraps text; lists and the mixer decide it
  from their row counts instead.
- 2026-10-13: Slint's Skia text vs WebKit's (Ryan's "Adobe-level" bar,
  design critique): the system face drew 7–12% narrower and wrapped lines
  12–20% tighter. Theme's tracking tokens (+0.5px at 13/11/14px, +0.55px
  at 12px) put word widths within ~1px of the references ("Shown in the
  title bar." 71 vs 72px at 1x), and `line-height-factor` (Theme.leading*,
  honoured by the Skia renderer) gives the 16/18/21px line heights. Every
  kit Text and TextInput with a token size carries them (a new Text needs
  them too: `letter-spacing: Theme.tracking-sm;` and, if it wraps,
  `line-height-factor: Theme.leading-sm;`). Strokes stay a little heavier
  than WebKit's `-webkit-font-smoothing: antialiased`; that needs Slint.
- 2026-10-13: cua-driver on the test Macs: `type_text` into a Slint
  NumberField shows the digits without firing the field's `edited`, so
  nothing commits (press_key does); `click` without a delivery mode may
  press the element through accessibility instead of clicking, so check
  pointer behaviour with `delivery_mode: foreground`. A `press:LABEL`
  snapshot state does an accessibility press offscreen.
- 2026-10-13: Comparing the Slint app's offscreen snapshots with the web
  app's references (/Volumes/IsocastBuild/n2-integrate/views.sh, then
  tools/pairs.py): offscreen the preview is a stand-in test picture
  (src/picture.rs), not the daemon's; `menu:OWNER` opens a menu at the
  window's centre, the web references at the pointer; and the web set was
  taken in one session, so from 41 on most of its views have Clip Only on
  air. Judge layout, sizes and colours around the preview, not the
  preview. `--a11y` lists open dialogs and menus since the critique
  fixes (it draws once first, and their 1px hosts count as visible);
  Slint 1.18's AccessKit adapter puts in-window popups into the real
  tree too (accesskit.rs, `PopupWindowLocation::ChildWindow`): on
  isocast-m2-b cua-driver's
  `get_window_state` listed an open Properties dialog's Add files, Loop,
  Speed, Defaults, Cancel and OK (read the tree twice: the first read
  comes before AccessKit has built it). cua-driver's `move_cursor` only
  draws its cursor (no hover reaches the app); use the `hover:X,Y`
  snapshot state. Its sessions end with the run: `start_session` with a
  fresh label each time.
- 2026-10-13: The Slint window is redrawn whole for every change, so the
  mixer's meters (30 steps a second while anything sounds) set its cost
  with native pictures: 19.6% of a core on a base M2 before the docks
  were cached in layers mode too, 13–14% after; 2.9% with nothing
  metering. Partial redraws would need Slint's renderer to support them. CPU readings on
  the test Macs drift between sessions: later on 2026-10-13 isocast-m2-b
  read 20–22% metering and 23–24% for `--preview image` at rest for the
  907f4dd build and the next alike, so compare builds A/B in one session
  (/Volumes/IsocastBuild/n2-fix/remote/ab.sh), not against old numbers.

- 2026-10-12: The daemon's `plan`/`apply` diff (isocast_protocol::change
  `diff`) matches identified list items by id and ignores their order, so
  a change that only reorders scenes, scene items or filters is "No
  changes" and isn't applied (a whole-setup apply and a remove-and-re-add
  patch alike). Both apps' drag-to-reorder, Move Up/Down and Order menus
  show the new order, then snap back on the next read. Repro:
  `isocast apply --revision R FILE` with the scenes reversed. Fixed
  2026-10-13: `diff` now reports a reorder as a change of the list itself
  (`~ scenes: ["one","two"] → ["two","one"]`), so plan shows it and apply
  runs it.
- 2026-10-12: Slint (isocast-ui): a Flickable takes a vertical drag as a
  scroll, so rows inside it never see a drag to reorder; the kit's lists
  set `interactive: false` (wheel and trackpad still scroll). A row's
  drag must end on `changed pressed`, not on a pointer-up event (a
  scripted drag on the test Mac never delivered one). A field that
  auto-focuses in a dialog loses to the dialog's own FocusScope unless
  it is created after it (`if true:`).
- 2026-10-12: The Slint app's Properties/Filters forms are Slint models
  changed row by row (`form::FormModel`), never replaced: replacing the
  model rebuilt every row on each live edit and killed a scrub mid-drag.
- 2026-10-12: NSColorPanel (the Slint app's colour swatches) can't be
  seen on the test Macs: apps there run in background mode, never
  frontmost, and AppKit hides an inactive app's panels. The modal
  NSOpenPanel shows.
- 2026-10-12: Slint (N.2.B): giving a repeater a new model, or a row new
  data through a new model, rebuilds its instances, so a mixer strip
  rebuilt on every live gain edit ended its fader drag at the first step
  (and lost double-clicks). Keep one VecModel per list and change rows
  in place (`update_rows` in src/areas/mixer.rs). A `Tooltip` placed
  straight in a dialog's tree (inside its PopupWindow) panics the 1.18
  compiler ("Could not find component \"\" from component \"\""); wrap it
  in a small component. A `changed` handler on a global-bound property
  inside a repeated strip didn't fire for a snapshot state; the dock's
  (not repeated) does.
- 2026-10-12: Meter cost (N.2.B, isocast-m2-b, 1024x678 window, three
  tones metering, preview in layers mode): every meter change redraws
  the whole window (the Skia wgpu surface has no partial rendering), at
  about 0.6–1% of a core per redraw a second. 60 fps meters: 29–36%;
  30 fps: 20–29%; 20 fps: 20%; meters resting (all muted): 1.5–2.6%.
  The web app on the same Mac: 33–37% metering, 19% resting (with its
  WebKit processes). The meters draw at 30 fps (`TICK`), stepping only
  while some meter moves; offscreen they step on each levels event.
  Levels alternate with −120 dB reports (the engine's), which the
  smoothing hides.
- 2026-10-12: Slint layout traps (area D): `width: min(460px,
  parent.width)` inside a layout is a binding loop (use `max-width` with
  `preferred-width`); a child of a layout can't set `x`/`y` (pad a
  wrapping layout instead); equal columns need `preferred-width: 0px` and
  `min-width: 0px` on each; a fixed-height control (Switch) in a FormRow
  sits at the row's top unless wrapped in `VerticalLayout { alignment:
  center; }`; Segmented and buttons stretch unless `horizontal-stretch:
  0`. Skia's text lines are ~2px shorter than WebKit's 16/21px line
  heights (Slint 1.18 can't set one), so long pages drift up a few px.
- 2026-10-12: cua-driver on the test Macs sees only the Slint window's
  chrome and menu bar in its accessibility tree (background mode), and its
  element tokens last only within one `session`; press Slint controls by
  window-screenshot pixels (`click` with x, y; the title bar is 32px).
  Dialogs are popups, which `--a11y` doesn't list either.
- 2026-10-12: /Volumes/IsocastBuild was full (47 MB free) during N.2.D;
  that agent built in its worktree's target/n2-d-settings on the root
  disk. Over SSH (testmac.sh run) ScreenCaptureKit times out, so device
  lists and init_setup need `testmac.sh gui`.
- 2026-10-12: Slint/Skia: `cache-rendering-hint: true` draws an element
  into a layer once and reuses it until something in it changes. With the
  picture drawn by Slint (`--preview image`) every video frame redraws the
  window, so the docks carry it there (pixel-identical; base M2 with
  moving meters 36.7% → 21.3%, silent 27.3% → 13.6%; the web app
  44.7%/27.3%). Skia on wgpu reports a buffer age of 0, so
  SLINT_SKIA_PARTIAL_RENDERING can't help. `transform-rotation` can't be
  set on a component's root (wrap a child). A spinner (animation-tick)
  left visible redraws the whole window at the display's rate: the
  monitors' "no frame yet" spinner was re-shown on every setup change
  (39% of a core idle until fixed).
- 2026-10-12: Test Macs: a capture by a dev app at a new path (each
  worktree gets its own) asks macOS 26+ for "bypass the private window
  picker" consent; the dialog sits over the desktop until answered (it
  was approved on isocast-m2-a for wf_1a0bdae5-ee6-6's dev app and for
  CuaDriver, as the test Macs' "allow everything" setup intends).
  cua-driver's get_window_state screenshot (same session label for the
  snapshot and pixel clicks; a fresh label per run, an ended one is
  refused) is the simplest way to see a test window; drags need
  delivery_mode "foreground". A function in `sh` started with `&` makes
  $! the subshell's pid, not the app's: start apps with `env VAR=… app &`.
- 2026-10-12: drag-lag on isocast-m2-a, the same build of each, 6 s drags:
  the Slint app 46.8 picture moves/s median of 5 runs (45.5-50.0), gap
  0.7 px median, ≤1.4 px p95, lag 0, 0 box-off frames; the web app 47.3
  median of 4 (42.8-49.0), gap 0.7-4.1 px, p95 5.5-8.2. The drag is
  limited by the pointer steps (outline ~45/s); applies came back within a
  step (most in flight 1, none stale), so pipelining (two steps in flight)
  only matters on a slower daemon or window thread.

- 2026-10-12: Slint 1.18 traps met building the kit: `focus()` can't be
  redeclared (use `forward-focus`); custom properties can't be called
  padding-*, min-/max-/minimum-height or `color` on a Rectangle (its
  deprecated background alias); a property named `text` or a loop
  variable named `tab` shadows enum values (write `MouseCursor.text`,
  `AccessibleRole.tab`); `accessible-role` must be constant; `parent` can't
  be chained (use ids); a component must be defined before it is used in
  its file; nothing outside a PopupWindow may read its children; key
  events stop at popups (a dialog's capture handler never sees a
  dropdown's keys); a popup is modal for the mouse (a press outside the
  topmost non-menu popup is swallowed), so toasts are drawn in the window
  and would sit under an open dialog (Dialog and Popover now carry a
  ToastStack of their own); `changed` handlers run after the event,
  not at once; an `easing` in an exported global warns (Motion is apart);
  Flickable's `viewport-y` is `content-y` now.
- 2026-10-12: Area C (slint/n2-c-controls) on the Slint app:
  - An element can't set both `width` and `max-width` (NumberField has a
    max-width), so wrap it in a layout to stretch it.
  - Slint 1.18 has no tabular figures, so the status bar's numbers aren't
    tabular as they are in the web app. They are no wider than the web
    app's either.
  - Offscreen runs don't poll `status`, so a snapshot shows the stream's
    and the take's numbers as they stood when it connected (plus events).
  - A real stream can be checked offscreen without the Keychain. Run a
    loopback RTMP listener (mediamtx with only `rtmp: yes`,
    `rtmpAddress: 127.0.0.1:PORT` and `paths: all_others:`) and use a
    `[stream]` with `service = "custom"`. Restart the isolated isocastd
    with `ISOCAST_STREAM_KEY=anything isocast restart`, then use the
    states `go-live` and `stop-stream`. Killing the listener gives
    Reconnecting.
  - Stream drops are only warned about while live, not while
    reconnecting (warn_stream_drops), so no drops toast comes then. The
    web app behaves the same.
  - cua-driver CLI calls each start fresh unless they pass the same
    `"session"` label. Without it, a click's element token from the
    previous `get_window_state` is refused as stale.
- 2026-10-12: Skia draws SF at its default optical size, tighter than
  WebKit's SF Text at 12-13px (same words are ~10% narrower than in the
  web app's snapshots); Slint 1.18 has no font-variation setting.
- 2026-10-12: AccessKit maps Slint's `list-item` to ListBoxOption, which
  macOS exposes as AXStaticText (Slint 1.18 has no row role); the
  selected state reaches VoiceOver as AXSelected and the list's
  AXSelectedRows. A true AXRow needs a Slint change.
- 2026-10-12: macOS names an app after its executable unless it runs from
  a bundle (LaunchServices' name; an embedded Info.plist doesn't change
  it), and Slint's macOS menu bar always adds an app menu (About, Hide,
  Quit) titled with that name. Run the Slint app from
  scripts/ui-dev.sh's bundle to have it say Isocast.

- 2026-10-12: A wgpu validation error panics the calling thread. When
  that was the live engine starting (a 16385-wide canvas), `starting`
  never cleared and every later `start` hung until `isocast restart`;
  `ensure_engine` now catches the panic and reports it. Seen while
  checking: a media source's `received_fps` in `status` jumps (0, 5158,
  1685, 637…) for the first seconds after a start before settling at the
  file's 30; and its `fps` ("opened at") is the canvas rate, the rate its
  ISO files run at, not the file's (MediaInfo has no frame rate yet).
- 2026-10-12: BLOCKED V1.8 m4-throughput. ISO diagnostics on failed M4
  runs show zero late/missing ticks or full delivery channels; the drops
  occur when AVAssetWriter video readiness stays false and its bounded
  input queue fills. Video-only flagship can be clean, but even video-only
  ISOs drop when only program audio is enabled: local interleaving alone
  is not a sufficient explanation. Native completion makes this worse by
  holding up audio too. After repeated unsuccessful controls, stopped
  changing encoder policy: App Nap activity assertions, QoS, zero-based
  timestamps, offline inputs, fragment/chunk changes and readiness polling
  did not reliably fix it and are not retained. The wrong assumption is
  that the ~340 loss has one demonstrated cause (timer policy, timestamp
  magnitude or hardware capacity); it does not yet. Next diagnostic is an
  isolated AVFoundation/VideoToolbox trace of simultaneous video-only and
  A/V writers, including per-input readiness and service scheduling, not
  another queue-size guess. Reproduce: `scripts/testmac.sh gui 'python3
  scripts/bench.py --work "$ISOCAST_SCRATCH/astra-record/bench" --daemon
  target/release/isocastd --compare "$HOME/dev/isocast-main/target/release/isocastd"
  --scenario flagship --runs 4 --seconds 20'`. Full evidence and limitations:
  `/Volumes/IsocastBuild/astra-record/scratch/REPORT-M4.md`.
- 2026-10-12: Robustness hard test (V1.8), fixed on hardtest/robustness:
  a start's session is in the daemon state before the engine warms up
  (status `starting`, `stop` cancels it), `stop` returns its own
  session's outcome, and `status.last_error` keeps why the last session
  ended without a report. Recordings hold a shared lock on
  `<folder>/.isocast.lock`; a "recording" manifest in an unlocked folder
  was killed/crashed, and the next start there marks it "failed" (doctor
  warns). The disk-full stop now keeps at least 10 s of the files (64 MB
  minimum) free even with keep_free_gb = 0. A stop right after start
  keeps a 1 s take. Testing tricks: a FIFO named `.isocast-speed-test` in
  the target folder holds the disk test until something reads it; a RAM
  disk (`hdiutil attach -nomount ram://614400`, check `diskutil info`
  says Disk Image, then `diskutil erasevolume APFS NAME /dev/diskN`)
  fills fast enough to test disk-full, unlike a .dmg on the busy SSD
  (2-4 MB/s). Open: the folder is still created before the space check
  refuses; an interrupted manifest keeps null durations (files aren't
  re-read).
- 2026-10-12: V1.8 streaming fixes. mediamtx accepts `publish` for an
  unconfigured path (a wrong key) and drops it ~2 s later, so retries now
  reset only after a connection stays up 10 s (else it looped forever).
  VideoToolbox's MaxKeyFrameInterval(Duration) at the keyframe interval
  added keyframes of its own 1-3 frames before the engine's grid ones
  whenever frames reached it late; it is now twice the interval (a
  backstop) and the engine re-asks for a keyframe the encoder dropped.
  VideoToolbox H.264 refuses any side over 4096 (error -12903), so
  stream.width/height stop at 4096. A recording's live drop count raced
  its writer (newest frame published before the frame was counted) and
  now and then warned "dropped 1 frame(s)" with nothing lost. Plan
  notes now say [stream] edits wait for the next stream start, and
  `isocast watch` exits 1 when isocastd goes away. Local test server for
  a "wrong key": mediamtx with `paths: live/goodkey:` only.

- 2026-10-12: Studio NDI: STUDIO_RING_VERSION also stamps the audio
  packets the app sends to the separately installed Studio Audio driver;
  bumping it (5b88dc0) silenced Studio Audio until fixed (its next
  commit). Don't change shared constants the installed driver checks
  without reinstalling the driver; and when rebuilding the app, keep the
  active camera extension byte-identical (copy it into the staged app and
  re-sign the outer app) unless the extension itself changes.

- 2026-10-12: Decision (Ryan): the app moves to Slint (PLAN section N),
  chosen by a research workflow over gpui (runner-up: leanest, but its
  video surface is macOS-only), Qt, egui, iced and web + native layers.
  Ryan approved the "About Slint" notice (royalty-free licence). Reasons:
  one UI for macOS and Windows; Slint 1.18's wgpu 30 texture import
  matches the engine's pinned wgpu 30 for a zero-copy preview on both
  OSes. Known gaps agents must build: docks/splitters, modal dialogs, a
  colour picker; every video frame repaints the window.

- 2026-10-12: agent/perf-capture (demand-driven program drawing, 4:2:0
  program, VT-scaled JPEGs, viewer-rate drawing; engine idle 540 → 141 MB,
  flagship --real 29.1 → 25.2% CPU) was reverted (a0cd008) because the
  drag-lag window showed a black monitor — wrongly: Ryan's browser window
  covered the test window, macOS reported it occluded, WebKit set
  `document.hidden`, and the monitor released its stream by design while
  the recorder kept filming the covered window. Re-applied with
  agent/capture-integration (8b45567, 32d8cdc). drag-lag.py now refuses
  runs whose window gets hidden. Lesson: a real-window test on Ryan's
  machine depends on what's in front; read the tool's warning.

- 2026-10-12: Ryan's iPhone camera (Continuity) opened at 1920×1080@60
  delivers 30 fps: when the capture session starts, macOS swaps it to a
  30 fps 1080p format. Re-setting the frame rate after start is refused;
  `automaticallyConfiguresCaptureDeviceForWideColor`/input-priority
  settings aren't supported and broke format setting for every camera;
  re-setting the 60 fps format after start reports 60 but frames still
  arrive at 29.9. Likely the phone (low light or its camera settings)
  holds it at 30 — unconfirmed. `isocast status` shows it as "at 30.0 fps
  (opened at 60)" (received_fps).

- 2026-10-12: Measuring a camera's frame rate: `isocast frames <source>
  --fps 60` samples the newest frame on a 60 Hz tick, so a jittery 60 fps
  source reads ~50-55 (two frames in one tick count once) — that misled
  a Studio NDI investigation twice. Use `ISOCAST_CAMERA_STATS=1` on the
  daemon (logs frames/s actually delivered to the capture callback every
  5 s); the daemon also logs frames the capture output discards as late.
  And mpdecimate's fuzzy duplicate count reads a still subject as a low
  frame rate: hash frames exactly (`ffmpeg -f framemd5`) instead.
- 2026-10-12: Studio NDI (Ryan's NDI virtual camera, ~/dev/experiments/
  2026-08-29-ndi-cam, fixed in its 5b88dc0): its camera extension allowed
  only 5 outstanding pixel buffers, so a consumer holding frames (Isocast
  passing BGRA through unconverted) starved it (-6689, frames unsent);
  now 12, with frames keeping their enqueue time and a 2 ms idle wait.
  Delivered to Isocast: 60.0 fps. Keep requesting 420v from BGRA-only
  cameras: the conversion hands Isocast buffers from the capture output's
  own pool rather than holding the device's.

- 2026-10-12: Drag lag (Ryan: the picture trails the selection box). Use
  `scripts/drag-lag.py [--diagnose]` (real app window on a test engine, a
  scripted drag, the window recorded at 60 fps and measured). Traps that
  made early numbers wrong: anything magenta/green elsewhere in the window
  (the source toolbar's colour swatch, test-pattern bars) — the scene is
  now plain and the block is searched only in the outline's rows. Causes
  found and fixed: apply waited on two fsyncs (now `live` applies, saved
  when the gesture pauses), drags sent every 50 ms (now every frame), JPEG
  monitors (now H.264), monitors at 30 fps (60 while working on the
  canvas), and the box drawn at the pointer while the picture lagged (now
  drawn from the shown frame's layout). Measured: gap 6 px median / 17 p95
  and 28 picture moves/s before the last two; 0 px / 0.6 p95 and 45 moves/s
  (the box's rate) after. The page itself was never the bottleneck (60 fps,
  ~220 ms of rendering in a 6 s drag).

- 2026-10-12: OBS-layout app (merged from agent/ui-obs): a Preact re-render
  lands before the next `setTimeout`, so a dialog's Return handler must read
  its handlers through a ref; the `.app` grid needs `minmax(0, 1fr)`
  columns or the docks push the window wider than a 1024 pt screen; the
  dock row needs at least 224 px or Controls hides Exit. Decisions: ISO
  toggle is a third row icon (copper when on) plus "Record ISO File" in the
  source menu; removing a source's last item deletes the source with an
  Undo toast; one transition per setup, so the Scene Transitions dock has
  no + / −; Settings edits a draft applied on Apply/OK; item appearance
  lives in Edit Transform. Open: no flip, no media play/pause API, no
  projectors/multiview/virtual camera/pause, docks can't float, hotkeys
  are fixed; `stream_stop` reported `audio_kbps: 0.0` though the stream
  had AAC (check); errors for a missing window list every window title on
  the Mac (other apps' titles too) — consider listing fewer.

- 2026-10-12: Test VM (`scripts/vm.sh`), set up at Ryan's request with Cua
  Spaces: Lume VM `isocast-vm` (cloned copy-on-write from his Cua space
  `space-07824e5011`, macOS 26.5, 6 cores, 12 GB; clean copy
  `isocast-vm-clean`). The Cua image has SIP off and admin access
  with its default lume/lume account, so `scripts/vm-grant.sh` writes
  screen recording, camera and microphone grants straight into TCC (by the
  signature's designated requirement, so they survive rebuilds signed with
  Ryan's Apple Development identity) plus a 100-year replayd screen-capture
  approval: no prompt ever waits for a click. Verified: onboarding shows all
  three Allowed; the engine records the VM's display. Apple's hardware
  encoders work in the VM (paravirtualized H.264/HEVC/ProRes), but VM
  numbers aren't benchmarks. Cua's background service can't reach the VM's
  192.168.64.x address (Local Network permission), so `vm.sh up` forwards
  127.0.0.1:13211 to it and registers that as the Space. Over SSH use
  `isocast start` (engine), not `isocast record`, which captures in the
  CLI's own process (no screen permission for SSH logins). Never use the
  Cua space "basebook": that is Ryan's real Mac.

- 2026-10-12: `scripts/bench.py` is the shared performance benchmark (Ryan
  asked for a way for optimizers to test): its flagship scenario is his
  target — a 4K60 desktop and a 1080p60 camera, each recorded as its own
  file, plus a 4K60 program — synthetic clips by default, `--real` for his
  display, Studio Video and Studio Audio. `--compare OLD_ISOCASTD`
  alternates builds; a machine-wide lock keeps agents' runs apart. First
  numbers (synthetic, release): isocastd 16.5%, VT services 8.9%, 0 drops.
  First `--real` run, with five other engines running: desktop and camera
  ISO files dropped ~5% and their mic had dropouts while the program was
  clean — under investigation.

- 2026-10-12: Ryan wants the app to look and feel like OBS (same places,
  same words): preview on top with the source toolbar under it, docks along
  the bottom (Scenes | Sources | Audio Mixer | Scene Transitions |
  Controls), a status bar, Studio Mode as Preview | Transition | Program,
  and Properties/Filters/Edit Transform dialogs instead of an inspector.
  Our colours, type and icons stay. Clean-room: layout and behaviour from
  public docs only, never OBS's code, .ui/.qss files or icons.

- 2026-10-12: Ryan approved constant bitrate for YouTube: `stream.rate_control`
  (`average` | `constant`). Then made constant the default for every
  service: YouTube's guidance says CBR, Twitch's usual advice too, and
  OBS users expect CBR (OBS has the same two Apple-encoder modes). Measured on a static 1080p60 card at 6000: constant 6000 every
  second, average 147–255.

- 2026-10-12: QA round (V1.8), merged: each split leaked one VideoToolbox
  encoder session per file until the recording ended; after ~8 takes
  (32 sessions) VideoToolbox fell back to its software HEVC encoder
  (VTEncoderXPCService ~1400% CPU, "VCPHEVC") and everything dropped
  frames. Fixed (writers drain autorelease pools and free their encoder
  when their file finishes), and H.264/HEVC files and the stream now
  require the hardware encoder, so overload drops frames (counted) instead
  of starving the Mac. Also fixed: media sources leaked ~5 decode threads
  per on-air switch; a device change just before `start` recorded the old
  device; a deleted media file froze the program; a stream started on a
  cold engine could be idled out mid-connect; frames lost before the
  stream encoder were not counted; keyframes came in pairs at 30 fps and
  drifted off their 2 s grid when frames dropped; a stream key pasted
  into a preset's server sat in plain text in setup.toml.
- 2026-10-12: Stream bitrates (1080p60, target 6000): a static scene
  sends ~350 kb/s, a desktop 300–1400, a game-like scene 8–10 Mb/s and
  full-screen noise ~44 Mb/s: VideoToolbox's average-bitrate mode stops
  raising QP near 49 and overshoots busy scenes. Its ConstantBitRate mode
  held exactly 6000 on static scenes at a 7–17% overshoot cost on busy
  ones — a possible opt-in (`stream.rate_control = "cbr"`) for Ryan to
  decide. Open: overshoot has no warning; audio lost from the program tap
  becomes uncounted silence; `[stream]` edits while live apply at the next
  start without saying so; apply holds the daemon lock across two fsyncs
  (p95 ~125 ms); a stuck watcher's queue is unbounded.

- 2026-10-12: `screencapture` from an agent raises "… is requesting to
  bypass the system private window picker" on Ryan's screen; use
  `isocast-app --snapshot` (or cua-driver window states) instead. Any
  test that applies a setup with real cameras or mics to an unsigned debug
  daemon opens them and prompts; test first runs with camera and
  microphone "none", or use a signed copy (ISOCAST_DEV_APP). In
  `--snapshot` mode the off-screen page is hidden, so monitors don't
  fetch frames there; a visible window does.
- 2026-10-12: Follow-ups from the walkthrough: drop warnings come one
  toast per file per second and stack over the monitor (combine them);
  listing devices for the first run takes ~4 s (camera formats).

- 2026-10-12: A signed copy of the dev app at another path
  (`ISOCAST_DEV_APP=…/Isocast Dev.app`, own `ISOCAST_SOCKET`) gets the
  same camera, microphone and screen grants, so parallel test daemons can
  use real devices.
- 2026-10-12: Unsigned debug builds each have a new code signature;
  reading a Keychain item an older build stored pops a SecurityAgent
  prompt that blocks the caller. The daemon disables Keychain UI; the
  signed dev app keeps one identity.
- 2026-10-12: ffmpeg's `rtmps` listener won't take a certificate; test
  RTMPS through `socat OPENSSL-LISTEN:…,cert=…,key=…,fork TCP:127.0.0.1:PORT`.
  Local RTMP: `ffmpeg -listen 1 -i rtmp://127.0.0.1:19350/live/test -c
  copy out.flv` with `[stream] service = "custom"`, `server =
  "rtmp://127.0.0.1:19350/live"`, and the daemon started with
  `ISOCAST_STREAM_KEY=test`.
- 2026-10-12: CoreMediaIO virtual cameras report transport `'virt'`, but
  NDI Virtual Camera reports `'othr'` (so "virtual" in the name counts
  too). A MacBook in clamshell mode reports its built-in camera as
  suspended. Don't script System Settings with `osascript` from an agent
  (an Automation prompt appears). A clean `git merge` can still break the
  build; build before trusting it.
- 2026-10-12: Low-motion scenes stream well under the target bitrate
  (~250–600 kb/s of 6000 for a test pattern); YouTube may warn about a
  low bitrate. Not yet tested against a real service (needs Ryan).

- 2026-10-12: X (rtmps://ca.pscp.tv:443/x) live tests with a test card
  (moving bars, a caption with the time, a -30 dBFS tone), 9000 kb/s CBR
  video, 128 kb/s CBR audio (the audio-cbr build), 2 s keyframes, on the
  M5 Max: 1080p30 5 min, 1080p60 5 min, 1080p60 15 min while recording
  the program and two HEVC ISOs, and a stop/restart with scene switches
  (cut, fade, swipe) and text changes (apply and a watched file) live.
  Every run: 0 dropped frames, 0 audio dropouts, 0 reconnects, congestion
  at most 2%, 10 s samples 8800–9277 kb/s video and 124–132 kb/s audio,
  ~9110–9135 kb/s on the wire; RTMPS connect to live in 0.7–0.8 s;
  daemon ~16% of one core and ≤153 MB while streaming and recording. The
  recording's three files: 51453 frames each at 60/1, 857.55 s, 0
  dropped. `elapsed_seconds` starts at connecting, so frames/elapsed reads
  ~59.8 at 60 fps. Samples and scripts: /Volumes/IsocastBuild/x-live/.
  The X key is in the Keychain for the dev app at
  /Volumes/IsocastBuild/xtest; re-sign that path to reuse it.

- 2026-10-12: Builds on the exFAT SSD crawl: macOS runs exFAT through a
  single user-space process (UVFSService, ~100% CPU), and a Rust build
  writes thousands of small files. Builds now go to an APFS sparse bundle
  on the same SSD, `/Volumes/Extreme SSD/isocast-build.sparsebundle`,
  mounted at `/Volumes/IsocastBuild` (`target` → `/Volumes/IsocastBuild/main`,
  agents use `/Volumes/IsocastBuild/NAME`): a cold engine build went from
  10+ min to 78 s. After a reboot, mount it again with `hdiutil attach
  -nobrowse "/Volumes/Extreme SSD/isocast-build.sparsebundle"`. The old
  exFAT `isocast-target` and `isocast-wt` folders are no longer used
  (deleting `isocast-wt` was paused to keep the driver free).

- 2026-10-11: The signed daemon reading a file in ~/Downloads (or
  Documents, Desktop) waits in `open()` for a macOS privacy prompt; an OBS
  import hung on Rebase.json's media until reads got a 3 s limit. A
  recording whose media lives there would wait the same way at start, so
  the app's first run should ask for those folders up front.
- 2026-10-11: OBS colours are 32-bit 0xAABBGGRR (red in the low byte);
  filter colours' alpha is often 0, so it's ignored there. A collection's
  `resolution` can differ from its profile's canvas (Rebase.json is
  720×1280, its profile 1768×3840); the import uses the saved resolution.
  OBS's `display_uuid` equals Isocast's display id. Newer window captures
  save only a window number, so they import only while that window exists.

- 2026-10-11: Filters (video and audio) and source kinds are flattened,
  kind-tagged structs, so serde silently ignores unknown fields: a gain
  filter written `db = -6` (the field is `gain_db`) did nothing and passed
  `setup check`. Fixed by validation that reports unknown fields (see
  4.15).
- 2026-10-11: Under heavy load (several agents building), program-only
  recordings started up to 0.67 s late (`start_offset_seconds`), with or
  without filters; measure against the manifest offset.

- 2026-10-11: System audio excludes the daemon's own audio when the setup
  monitors anything at start, so monitoring isn't recorded twice; turning
  monitoring on mid-take when nothing was monitored at start puts the
  monitored sound into the system-audio track. Monitoring a
  `system_audio` source is refused (feedback). The monitor hears sources
  before 4.10's audio filters (which run at mix time); OBS monitors after
  filters. A media source's monitoring delay follows its decoder's chunks
  (53–170 ms for a WAV); tones run 14–25 ms behind plus device latency.

- 2026-10-11: Edge antialiasing divides by `fwidth(d)`, which goes wrong
  at an item's corners: an odd-sized item at an even position draws its
  bottom-right corner pixel at 75%, and the pixel diagonally outside a
  corner gets about 10% of the item. Dividing by the pixel size from the
  local coordinates (`length(dpdx(local))`) would likely fix it, slightly
  changing antialiasing on rotated edges. Point sampling exactly on texel
  edges (4× decimation) is a GPU tie between two texels.

- 2026-10-11: Media sources (4.5a) ignore a video's rotation metadata
  (phone videos shot upright play sideways), decode to 4:2:0 (alpha in
  ProRes 4444 or HEVC-with-alpha is dropped; stingers in 4.11 will need
  BGRA), and an ISO file of a playlist whose files differ in size is
  untested. Speed changes audio pitch, as in OBS.

- 2026-10-11: Right after deleting 4.2 GB from the exFAT SSD, the daemon's
  `mkdir` for a new recording folder blocked for over 30 s; `start` timed
  out and the recording began later on its own. Big deletes on exFAT keep
  the volume busy for a while. The start timeout now says the recording
  is still starting and how to check or cancel it.

- 2026-10-11: The soak's only drops (2 frames in every file at once) came
  while a debug build wrote to the same SSD the recording was writing to.
  Heavy writes to the recording disk can cost frames; record to a
  different disk than builds or other big copies when it matters.

- 2026-10-11: Don't launch the unsigned debug `isocastd` through the CLI
  (`ISOCAST_DAEMON=target/debug/isocastd`): the CLI disclaims
  responsibility, so the daemon is its own TCC client, and since `target/`
  is on the removable Extreme SSD, dyld blocks opening the binary until
  removable-volume access is granted (a new prompt per build, since
  ad-hoc builds change identity). Test against the signed dev app (after
  `scripts/sign-dev.sh` and `isocast restart`), or run a debug daemon
  directly in a terminal.

- 2026-10-11: Items now default their id to their source, so two items
  showing the same source in one scene need explicit ids (validation says
  so). A group's opacity multiplies into its children, which differs
  from CSS where overlapping children inside a faded group would not show
  through each other; offscreen group rendering can fix that if it shows.

- 2026-10-11: Encoders shared by four files are briefly not ready now and
  then; dropping video at once lost ~10% of frames in 5-frame bursts on the
  real rig. `append_video` now waits up to 100 ms (the per-file packet queue
  absorbs it): 0 drops over 60 s.
- 2026-10-11: The iPhone screen reported 1206×2622 in one session and
  1920×1440 in another. A file's size is fixed from the first frame, so a
  device screen that changes size mid-take (rotation) is untested; check
  what AVAssetWriter does with differently sized frames.

- 2026-10-11: Chroma is sampled center-sited from the half-size plane;
  cameras and VideoToolbox use left-sited 4:2:0, so colour edges can be
  half a chroma pixel off in snapshots. Revisit if it shows in composites.
- 2026-10-11: wgpu 30: `BufferSlice::get_mapped_range` returns a `Result`,
  `PipelineLayoutDescriptor::bind_group_layouts` takes `Option`s, and
  render pipelines have `multiview_mask`. Debug builds of the workspace take
  about 75 s now that wgpu is in.

- 2026-10-11: Audio past a file's end used to be able to add silence beyond
  it: a chunk starting after the end, following a gap, was padded up to its
  start before the end limit applied (and counted as a dropout). Audio from
  at or after the end is now ignored; finishing pads to the end. This
  mattered once a file stays open after a split.

- 2026-10-11: `tune` results vary between runs on this machine (HEVC 8-bit
  on the rig: 1.5× to 2.2× real time), since it shares the encoders with
  whatever else runs; the recommendation stayed the same. Benchmarking with
  the encoder in offline mode (`expectsMediaDataInRealTime = false`) read
  slower than real-time mode, so it uses real-time mode like a recording.
  Right after a benchmark the exFAT SSD's burst write test can read 43 MB/s
  instead of 500+, so `tune` measures the disk before and after and keeps
  the better reading.

- 2026-10-11: macOS kept nothing across rebuilds when the signed daemon was
  a bare binary in `target/` (cause not confirmed; bare binaries may be keyed
  by path and hash); as a signed app bundle it keeps everything. Claude Code now runs as
  `ClaudeCode.app`, which has no camera usage string, so anything capturing
  in-process under the agent (like `isocast record`) can't use cameras; go
  through the daemon.
- 2026-10-11: The scratchpad from the first session is gone. Test sockets
  (`ISOCAST_SOCKET`) need a short path (Unix socket paths are limited to 104
  bytes), like `$TMPDIR/isocast-test.sock`. Without it, tests use the real
  daemon at the default socket.

- 2026-10-11: In the 10-minute take, both 60 fps files lost one frame at the
  same moment (592.5 s) with nothing else running: a system hiccup. Live
  warnings would show when; they are off in `--json` mode. The Insta360
  delivers 29.97 fps (17983 frames in 600 s), so its file repeats a frame
  about every 33 s; the mic (Wave:3) showed no drift beyond 2 ms in 10
  minutes (0 dropouts).

- 2026-10-11: ScreenCaptureKit's `excludesCurrentProcessAudio = true`
  silenced `afplay` started from the same terminal as isocast (it seems to
  exclude by responsible app, not process). It is off; Isocast plays no
  audio. Revisit when the app gets audio monitoring.

- 2026-10-11: The frame clock ticks at whole frame rates. A 59.94 fps device
  runs on a 60 fps grid, so a frame repeats about every 16.7 s. Fix with
  rational rates if a real device needs it (capture cards often run 59.94).
- 2026-10-11: Don't switch on the Continuity iPhone camera for tests: it
  takes over Ryan's phone. The Insta360 X3 (UVC, 1080p30) is a safe camera
  to test with.

- 2026-10-11: Not yet verified: 60 new frames per second from a display.
  With ordinary desktop activity ScreenCaptureKit delivered ~31 new frames/s
  (the content changed at ~30 Hz), and the file showed 300 distinct frames
  for 315 delivered. Check with 60 fps motion on screen: `isocast record
  --source screen --seconds 10` should report ~600 new frames.

- 2026-10-11: Device ids. Displays use the ColorSync display UUID (stable
  across reboots). Cameras and mics use `AVCaptureDevice.uniqueID` (the Core
  Audio UID for mics); for some UVC devices (Insta360: `0x1320002e1a00c1`)
  it encodes the USB location and changes if the device moves ports, so
  setups should match devices by name and use ids only to tell identical
  devices apart.
- 2026-10-11: AppKit (`NSScreen`) only answers on the main thread. Display
  names come from it, so off the main thread (a future daemon) they fall back
  to "Display N" until 2.1 routes the call to the main thread.
- 2026-10-11: For a scaled HiDPI mode, CoreGraphics reports the backing size
  (5120×2880), not the panel (3840×2160). Task 1.3 must choose the capture
  size; OBS recorded the canvas at 3840×2160.

- 2026-10-11: WebKit treats an offscreen window as occluded and freezes the
  page (one distinct frame in 6 s). The private
  `_setWindowOcclusionDetectionEnabled:NO` on the WKWebView fixes it; it is
  guarded by `respondsToSelector:`, so if a macOS update removes it, browser
  sources will freeze rather than crash. Try an on-screen window with
  `alphaValue` 0 then.
- 2026-10-11: The hidden window's backing scale is the main screen's (2 on
  Retina), so snapshots came back at 2× (3840×2160 for 1080p, 13 ms plus
  10 ms to scale down). Setting the snapshot width to `width / scale`
  points gives exactly one pixel per frame pixel (3.4 ms).
- 2026-10-11: ScreenCaptureKit lists the daemon's own windows (a browser
  source's hidden page); `devices::is_ours` leaves them out of window lists
  and window lookups.
- 2026-10-11: On the exFAT SSD, cargo warns "hard linking files in the
  incremental compilation cache failed"; it's harmless (it copies instead).
  macOS also writes `._*` AppleDouble files next to recordings there; ignore.
- 2026-10-11: AAC smears a hard beep onset up to ~1.5 ms early (silencedetect
  reports 0.9995 s for a beep at 1.000 s). Allow a couple of ms, or compare
  video flashes, when checking sync with the tone.
- 2026-10-11: objc2 0.3 renamed many C functions to methods (for example
  `CMClock::host_time_clock().time()`); the free functions are deprecated.

- 2026-10-15: README screenshots (scripts/screenshots/take.sh): take them on
  isocast-m2-b, not exonbook. On exonbook display-mode.swift has no HiDPI
  mode, the app window comes out 1440x867, and Finder's sidebar shows
  Ryan's favourites. On m2-b the first run builds Isocast.app there
  (ui-dev.sh --release) for more than 10 minutes, so `testmac.sh gui` gives
  up ("no answer from the desktop-session bridge") while remote.sh carries
  on: wait for it, then `scp 'isocast-m2-b:isocast-scratch/docs-shots/out/*.png'`
  and run take.sh's trim step by hand. Hold /Volumes/IsocastBuild/.bench.lock
  (`lockf`) so the hardware queue doesn't start on it meanwhile. Since
  recordings go into project folders, shoot.sh opens the take's project
  folder itself. Dialogs that need a fake OBS folder (the first-launch
  sheet) or a benchmark (the Optimize summary) are drawn offscreen with
  scripts/snapshot.sh: `ISOCAST_OBS_FOLDER=… --state wizard-first-launch`,
  and `--state wizard:goal --state wizard-sample:OPTIMIZED.json` (the
  sample needs the wizard open first).
- 2026-10-17 (README 0.0.6): shoot.sh now also takes multiview.png (top-8
  projector window over the app, Studio Mode on, so it has both tallies)
  and vertical.png (a [vertical] canvas patched in with the Screen scene
  arranged for 9:16, removed again). The vertical canvas is added only
  after the take stops: apply refuses it while recording
  (`vertical.follow_main: change scene linking once the recording stops`).
  Taken through `hw-batch.py now cmd --gui --mac m2-b --bin-dir …` after
  copying scripts/screenshots/* to m2-b's ~/isocast-scratch/docs-shots, with
  a no-op `cargo` first on PATH so ui-dev.sh bundles the pushed isocast-ui
  instead of building on the test Mac; then take.sh's trim and quantize
  step by hand. The Terminal window opened fine on m2-b (#85 was m2-a).
  editor.png stays offscreen (snapshot.sh: three takes, `editor`,
  `create-angle:slides`, `create-crop:9:16`, `create-crop-mode:on`, 1000x625@2).

- 2026-10-18 (#124): hw-batch takes scripts from a commit: for
  uncommitted harness changes use `--scripts worktree --bin-dir BIN`.
- 2026-10-18 (#124): the VM's VideoToolbox can wedge after ~15 h up and
  many engine runs (even a 128x128 ffmpeg *_videotoolbox encode hangs;
  e2e then fails with -12903 or "did not open within 8 s"). Killing
  VTEncoderXPCService didn't help; `scripts/vm.sh down && scripts/vm.sh up`
  did.
- 2026-10-18 (#124): PowerShell 5 on exon-desk writes `>` redirects as
  UTF-16 (oracle-windows.sh converts them); an f-string argument to
  `require(cond, f"...{xs[0]}")` is evaluated even when cond holds, so a
  message that indexes a list raises IndexError on success: build it only
  when failing.

- 2026-10-18 (README 0.0.8, #122): README shots retaken from a 0.0.8
  build (main 7df7983b, version bumped only in the build copy) on m2-b with
  `SHOTS_PREBUILT=1` (take.sh/remote.sh bundle the pushed isocast-ui
  instead of building on the Mac), after `scripts/testmac.sh push`. remote.sh
  runs `machine calibrate` before the take so status shows a profile; the
  CLI shot uses a 620 pt tall Terminal (status outgrew 32 rows), put back to
  504 pt for the Screen scene. New shots (masks, canvases, Automatic Quality)
  were driven by hand with cua-driver drags (a 1 px drag is the click) and
  `isocast apply --patch`; governor.png needs real load (16 busy loops, 6
  recorded 4K canvases on an M2 → "preview limited to 30 fps"). Offscreen
  (snapshot.sh, 0.0.8 binaries): editor, stream (with canvases/destinations
  in a scratch offscreen.toml), optimize (measured on m2-b after calibrate;
  `tune` needed CYCLE 8 for the 0.0.8 frame budget: "could not draw
  benchmark frames" with 16, see Questions). obs-import is unchanged.
- 2026-10-18 (#124 follow-up): windows-remote.sh refuses to sync a src
  folder with files it didn't put there; a run once left
  `src\Microsoft\Windows\PowerShell\ModuleAnalysisCache` in our lane's
  src (a PowerShell started with src as its working directory), and the
  next build stopped. e2e.py now starts engines and workers in their own
  folders, and oracle-windows.sh removes that one stray folder after a run.

## Verification recipes

- The sheets against Paper's Screens boards (N.5.1b): in
  /Volumes/IsocastBuild/console-dialogs, `scripts/boards.sh` snapshots
  Properties, Settings and Welcome at 1x and 2x (Welcome from a second
  engine with no setup) and scores each sheet's region against the
  board exports in board/ (scripts/compare.py: board | app | difference,
  mean absolute difference and the share of pixels off by more than
  32); `scripts/views.sh` snapshots every dialog and state, and
  `scripts/final.sh` all of it at 1x and 2x with the pairs, scores
  (pairs/final/scores.tsv) and a contact sheet;
  `scripts/behave.sh` drives the sheets' controls and checks the
  engine's setup; `scripts/bands.py` and `edges.py` find text rows and
  edges for pixel-exact checks. Run the app with BIN=… for a debug build
  (2-3 minutes a rebuild against 5 for release).

- The kit against Paper's Components board (N.5): export the board with
  Paper's `export` tool (node 1K-0 on page p-2-0 of the Console file; it
  writes to ~/Downloads, move the PNGs to scratch), snapshot the gallery
  with `isocast-ui --snapshot kit.png --size 1440x3060 [--scale 2]
  --state kit`, and set them side by side band by band
  (/Volumes/IsocastBuild/console-foundation/scripts/pair.py and
  allpairs.sh; rules.py finds the bands' top rules, ink.py measures a
  label's width and weight). Values come from get_jsx/get_computed_styles,
  never from these pictures.
- The main window and Studio Mode against their boards (N.5.1a): in
  /Volumes/IsocastBuild/console-main, `scripts/shots.sh` (offscreen 1x
  and 2x against the isolated engine of env.sh), `scripts/pairs.sh`
  (board left, app right, whole and band by band, scores.jsonl; the
  pictures masked), `scripts/views.sh 1|2` (every state the area owns),
  `scripts/contact.py`, `scripts/crop.sh main|studio OUT x0,y0,x1,y1 ZOOM`
  (a 1x box of both, stacked), and `scripts/beh.py` (behaviours by
  accessibility presses and drags, checked with `isocast status/get`).

- Test the daemon without touching `~/Library`: `export ISOCAST_SOCKET=<a
  short path in the scratchpad>` (Unix socket paths max out at 103 bytes;
  the exFAT SSD can't hold sockets). Raw calls: `printf
  '{"jsonrpc":"2.0","id":1,"method":"status"}\n' | nc -U "$ISOCAST_SOCKET"`.
  Stop it with the `shutdown` method.

- Simulate a stall (drops) without loading the machine: start a recording
  in the background, then `kill -STOP <pid>; sleep 1; kill -CONT <pid>`.

- Sync across files: record `--source test-pattern:1280x720@30 --source
  test-pattern:1920x1080@60 --source tone`, then for each file in the
  manifest find flash onsets (below) and beep onsets, add the file's
  `start_offset_seconds`, and compare. On 2026-10-11 flashes matched exactly
  across files and beeps were within 1.2 ms of flashes.

- Flash frames of a test pattern (bottom quarter goes white for 100 ms each
  second): `ffmpeg -v error -i F.mov -vf "crop=iw:ih/4:0:ih*3/4,signalstats,metadata=print:key=lavfi.signalstats.YAVG:file=-" -f null -`
  then look for YAVG > 128.
- Beeps: `ffmpeg -i F.mov -af silencedetect=n=-40dB:d=0.05 -f null - 2>&1 | grep silence_`

## References

- `docs/prior-art.md`: research on existing Rust/OBS projects, crates,
  licensing, and agent-control design.
- OBS backend design (behaviour reference only, no code):
  https://docs.obsproject.com/backend-design
