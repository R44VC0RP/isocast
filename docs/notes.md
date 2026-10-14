# Working notes

Short, dated facts a future session needs. Newest entries go at the top of
each section. Delete entries that stop being true.

## Environment

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
- 2026-10-12: exonbook's screen is locked: ScreenSaverEngine runs and
  `sysadminctl -screenLock status` says the lock is immediate, so every
  window there is hidden (captures show only the aerial wallpaper; a Slint
  window is reported occluded and stops drawing). Real-window checks for
  N.2.E (drag-lag web vs ui, colours, CPU) ran on isocast-m2-a instead.
  Needs: unlock exonbook's desktop session (or turn its screen saver and
  lock off). Still locked on 2026-10-13, so the N.2 integration's
  real-window checks (CPU, drag-lag, soak) ran on isocast-m2-b too.
  Blocks: the same checks on an M4 and on a P3 display. Retry:
  `ssh exonbook 'pgrep -fl ScreenSaverEngine'` prints nothing, then
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

- 2026-10-13 (themes, docs/themes.md section 15, revised): (1) **May
  N.5's board-check scripts and the A/B CPU script go into `scripts/`?**
  `pair.py`, `rules.py`, `ink.py` are in
  `/Volumes/IsocastBuild/console-foundation/scripts/` and `ab.sh` in
  `/Volumes/IsocastBuild/n2-fix/remote/`, not in git, so a fresh session
  or a test Mac can't run the theme slices' checks; AGENTS.md asks for
  approval before adding harnesses. T.3a waits on this. (2) Blender's
  font: the boards use Inter Tight (Paper has no upright Inter), real
  Blender uses Inter (wider); the design ships Inter Tight unless you
  say Inter. (3) Blender's Preferences and Properties stay in-window,
  drawn with the boards' title bar (only the close dot works); real
  windows later (T.8)? (4) If the four themes cost more than +5 MB of
  binary each (projected 5–9 MB; T.4a measures), is that fine? (5) One
  theme for the whole app, not one per profile. (6) "Blender" is the
  Blender Foundation's trademark: fine locally, maybe renamed
  ("Editor") before any release. (7) The old copper look retires with
  N.5 rather than becoming a fifth theme.

- 2026-10-13 (7.4a): three things to overrule if you'd rather. (1) Wasm
  calls are bounded by fuel (work), not the design's 1 ms epoch ticker
  (wall time); see Decisions. (2) The example plugin's built components
  (gain.wasm 16 KB, ducker.wasm 9 KB) are committed beside their source,
  so `plugin add examples/plugins/gain` works without the wasm32-wasip2
  target. (3) wasmtime makes isocastd 29 -> 43 MB and the CLI 32 -> 46 MB
  (it links the engine); a cargo feature could keep it out of the CLI.
  Nothing waits on these.

- 2026-10-13: May the `alloc-count` cargo feature (a counting global
  allocator in isocastd, off by default) be committed, as
  docs/scenes-as-code.md proposes? 5.0 measured allocations per tick with
  an uncommitted patch instead:
  `/Volumes/IsocastBuild/scenes-5.0/scratch/alloc_patch.py REPO` applies
  it (`--revert` takes it back); build with `--features
  isocast-daemon/alloc-count`, and the daemon's log gets a line every 600
  program ticks.

- 2026-10-13: Taste call: the migration names your setup's collection after
  the setup ("Recording", id `default`) and its profile "Default". Rename
  either (`isocast collection rename default NAME`, `isocast profile rename
  default NAME`) if you'd rather. Nothing waits on it.
- 2026-10-13: The source toolbar's Properties and Filters: docs/design.md
  (3.5, 3.18) says quiet buttons, which the Slint app follows; the web
  app's final polish uses bordered small buttons
  (crates/isocast-app/ui/ui/source-toolbar.js). Which should both use?
  Nothing waits on it.

- 2026-10-11: Your recordings folder (`~/dev/obs-recordings`) is on the
  internal disk, which has ~31 GB free. Isocast estimates a full rig take
  (4K60 screen + 1080p60 facecam + iPhone) at up to ~0.75 GB/minute in HEVC,
  so the free-space check may warn or stop long takes. Record to the Extreme
  SSD instead (`folder = "/Volumes/Extreme SSD/…"`), or set `keep_free_gb`?

- 2026-10-11: Ryan's primary camera is "Studio Video" and his mic "Studio
  Audio" (XLR, network); `examples/recording.toml` now uses them instead of
  the OBS collection's "Capture Card" facecam. Asked whether a capture card
  (HDMI capture device) is still used for anything, e.g. a second camera or
  a console, or can be dropped from the rig. Nothing waits on it.

## Benchmarks

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
- 2026-10-12: The Slint app uses the system face ("System Font", SF) via
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
  channels (multi-channel interfaces: first two inputs for now), matched like
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

## Gotchas

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

## Verification recipes

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
