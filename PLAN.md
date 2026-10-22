# Isocast plan

The task queue for an open-source, Rust, OBS-style recorder built for one
job first: Ryan's YouTube recording rig (screen + capture-card facecam +
iPhone, two scenes, every source also saved as its own ISO file), fully
controllable from a CLI and by agents over MCP.

Agents work through this queue continuously, task after task, until it is
empty or every remaining task is blocked. See `AGENTS.md` for the loop.

Research behind this plan: [docs/prior-art.md](docs/prior-art.md).

## Goals

1. **Record reliably.** Start and stop instantly, never lose a take, and
   produce high-quality files.
2. **ISO recording is built in.** Every source can be written to its own file
   at native resolution, alongside the program recording. All files share one
   clock, so they line up in the editor without audio syncing.
3. **Scenes and sources like OBS**, with a clean, flat interface.
4. **Agent-native.** Everything the app can do, the CLI and MCP can do. Setups
   are plain files an agent can read, check, change and tune for the machine.
5. **Extensible** through a plugin system.
6. **Open source** under MIT or Apache-2.0.

## Ground rules

- **Clean-room.** Never read or copy OBS source code, including its shader
  files. Use public docs, protocol specs and our own testing. OBS is GPL;
  copying it would make Isocast GPL.
- **Every task ends verified and committed**, then the next one starts.
- **Mac first, portable core.** macOS on Apple silicon only (no Intel
  builds). macOS code lives in `isocast-platform-macos` behind traits, so
  Windows and Linux can follow.
- **One method table.** The GUI, CLI and MCP are thin clients of the same
  engine API. No feature is GUI-only.
- **Never lose a recording.** Crash-safe files, disk-space checks before
  start, and loud warnings for dropped frames.

## Architecture

```text
            ┌────────── clients ──────────┐
            │  GUI    CLI    MCP   plugins │
            └──────────────┬──────────────┘
                 JSON-RPC over a local socket
            ┌──────────────┴──────────────┐
            │        isocast engine        │
            │  setup document (revisioned) │
            │  master clock · scene graph  │
            │  wgpu compositor · audio mix │
            └──────┬───────────────┬──────┘
          capture (per OS)    encode + write
   ScreenCaptureKit, AVFoundation, VideoToolbox, AVAssetWriter
   Core Audio                      (FFmpeg/GStreamer later for streaming)
```

Planned crates:

| Crate | Job | Tasks |
|---|---|---|
| `isocast-protocol` | Setup document, validation, JSON Schema, API types | 0.1 |
| `isocast-cli` | The `isocast` command | 0.1 |
| `isocast-platform-macos` | Device listing, capture, encode, file writing | 1.x |
| `isocast-engine` | Sessions, clock, recording, scenes, compositor, mixer | 1.x–3.x |
| `isocast-daemon` | Headless engine process with the control socket | 2.x |
| `isocast-mcp` | MCP server over the same API (`rmcp`) | 2.x |
| `isocast-app` | Desktop app | 6.x |
| `isocast-plugin` | Plugin host and SDK | 7.x |

Key decisions so far (details and later decisions in `docs/notes.md`):

- **Apple-native media path on Mac.** ScreenCaptureKit for screens and system
  audio, AVFoundation for cameras (capture cards and Continuity Camera are
  both camera devices), VideoToolbox for HEVC/H.264/ProRes, and AVAssetWriter
  for .mov/.mp4. Bindings: the `objc2-*` framework crates, or the
  `screencapturekit` crate if it is faster to work with (decided in 1.2).
- **Setups are TOML for people and JSON for the API.** Same types, same
  validation, one JSON Schema (`isocast setup schema`).
- **Stable ids, not names.** Sources and scenes are addressed by ids like
  `facecam`; names are only labels.
- **Every recording session writes a manifest** (`session.json`) with each
  file, its device, its start time on the shared clock and any markers, so
  editors (FilmCraft included) can line files up exactly.
- **Synthetic sources come first** (test pattern, tone, clock overlay), so
  the pipeline can be built and verified without a person granting
  permissions or plugging in devices.

## Task queue

Each task lists what "done" means. Tasks marked **needs Ryan** require a
permission prompt, a device or a judgement only Ryan can give; if he isn't
available, record it under "Waiting on Ryan" in `docs/notes.md` and move to
the next task that doesn't depend on it.

### 0. Foundation

- [x] **0.1 Project skeleton.** Cargo workspace, licences, setup document
  types with validation, `isocast setup check` and `isocast setup schema`,
  and `examples/recording.toml` ported from the OBS "Recording" collection.

### 1. Capture and ISO recording (replaces Source Record)

- [x] **1.1 Synthetic sources.** `test_pattern` (moving bars, frame counter,
  burnt-in timestamp) and `tone` (beeps at known times) source kinds in the
  setup format. Done when `isocast record` can write a test-pattern + tone
  .mov that `ffprobe` shows as the right length, size, fps and codecs.
  This builds the encode/write path (VideoToolbox → AVAssetWriter) without
  any capture permission.
- [x] **1.2 List devices.** `isocast devices [--json]` lists displays,
  windows, cameras (capture card, iPhone) and microphones with stable ids
  and supported formats. Record the binding-crate decision in notes.
- [x] **1.3 Record a display.** `isocast record --source screen --seconds 10`
  writes a playable HEVC .mov from ScreenCaptureKit. **Needs Ryan** once,
  for screen-recording permission.
- [x] **1.4 Record a camera.** Same for the capture card and iPhone at their
  native resolution and frame rate. **Needs Ryan** for camera permission
  and devices connected.
  - [x] 1.4a Camera capture through AVFoundation (cameras, capture cards,
    Continuity Camera, iPhone/iPad screens over USB), verified on a UVC
    webcam.
  - [x] 1.4b Verify Ryan's facecam and the iPhone screen over USB (what
    OBS's "iPhone Record" captured).
- [x] **1.5 Record audio.** Microphone and system audio into the file (AAC
  or PCM). **Needs Ryan** for microphone permission.
- [x] **1.6 One clock.** Every frame and audio buffer is stamped on the host
  clock. Each session writes `session.json` with file start offsets.
  Verify with two synthetic sources whose beeps and flashes line up.
- [x] **1.7 ISO recording from a setup.** `isocast record <setup>` writes one
  file per ISO source (`SCREEN-…`, `FACECAM-…`, `IPHONE-…`) with the
  configured audio in each, all aligned. Files survive a killed process
  (fragmented writing): verify with `kill -9` mid-recording.
- [x] **1.8 Recording quality and safety.** ProRes 422/HQ options, 10-bit,
  correct colour tags, free-space and disk-speed checks before start,
  dropped-frame counters. Verify a 10-minute take whose files line up
  within one frame.
  - [x] 1.8a Free-space check before start, and stop cleanly before the
    disk fills.
  - [x] 1.8b Disk-speed check before start (catches slow and network
    volumes).
  - [x] 1.8c Loud live warnings when frames or audio drop.
  - [x] 1.8d ProRes 422/HQ, 10-bit HEVC, correct colour tags.
  - [x] 1.8e A 10-minute take whose files line up within one frame.

### 2. Engine, CLI and MCP control

- [x] **2.1 Headless engine.** `isocastd` runs in the background and owns the
  session. `isocast start`, `stop` and `status` talk to it over a local
  socket.
- [x] **2.2 Live state.** The engine holds a revisioned setup document.
  `isocast get`, `isocast plan <change>` (diff plus problems) and
  `isocast apply` (atomic, with a revision check). `isocast watch` streams
  events.
- [x] **2.3 MCP server.** `isocast mcp` exposes intent-level tools: status,
  list devices, get state, plan and apply changes, start and stop
  recording, switch scene, add marker. Verify by connecting an MCP client
  and running a full start → switch → marker → stop session.
  - [x] 2.3a Markers and scene switching in the engine API and CLI
    (`isocast marker`, `isocast scene`), recorded in the manifest. Until
    the compositor exists, a scene switch is a timeline entry.
  - [x] 2.3b The MCP server.
- [x] **2.4 Markers and takes.** `isocast marker "intro"` writes markers to
  the manifest; split into a new take without stopping the devices.
- [x] **2.5 Tune to the machine.** `isocast doctor` checks permissions, chip,
  media engines, disk speed and space. `isocast tune` benchmarks encoders
  and proposes the best settings for this Mac as a setup change.
  - [x] 2.5a `isocast doctor`: permissions, chip and hardware encoders, the
    recording disk, and whether the setup's devices are connected.
  - [x] 2.5b `isocast tune`: benchmark encoders on the setup's streams and
    propose settings as a setup change.

### 3. Compositor and scenes

- [x] **3.1 GPU renderer.** wgpu on Metal, importing frames without copying
  (IOSurface). `isocast snapshot` saves a PNG.
  - [x] 3.1a The renderer, zero-copy import of BGRA, 8- and 10-bit 4:2:0
    frames, and `isocast snapshot` (MCP `preview_frame`) of any video
    source while idle, opening it briefly.
  - [x] 3.1b Snapshots while recording, from the session's own frames.
- [x] **3.2 Scene rendering.** Items with position, size, crop, visibility
  and order.
- [x] **3.3 Program recording.** The composited canvas is encoded at a steady
  canvas frame rate next to the ISO files.
- [x] **3.4 Audio mixer.** Per-source gain, mute, sync offset and level
  meters; the program mix.
- [x] **3.5 Live switching.** Cut and fade transitions, Preview and Program,
  and an explicit `take`.
- [x] **3.6 Agents can see.** The MCP `preview_frame` tool returns an image of
  the program, the preview or any source.
- [x] **3.7 Soak test.** A one-hour recording with CPU, GPU, thermal and sync
  numbers; fix what breaks. Use synthetic sources if real devices aren't
  available, and note which were used.

**Milestone: Isocast replaces OBS for Ryan's recordings.** When reached,
add a "Ready for Ryan" entry to `docs/notes.md` with the command to try,
then keep going.

### 4. Sources, filters and transitions (OBS parity)

Everything a first-time user expects from OBS on a Mac, under familiar
names: see `docs/obs-parity.md` for the full map. Each source kind and
filter is its own small module, so adding one is easy.

- [x] **4.1 Scene trees and styles.** Nested items (groups) and boxes with
  ids; opacity, rounded corners, borders, shadows and transforms
  (translate, scale, rotate). Flat items stay valid.
- [x] **4.2 Source modules.** Each source kind is a module behind a small
  trait (settings with a JSON Schema; open it to get video and/or audio),
  registered in one list; the existing kinds move over; `docs/obs-parity.md`
  explains adding one.
- [x] **4.3 Color, image and slide show sources.** PNG, JPEG, GIF and SVG
  images; slide shows with timing and transitions.
- [x] **4.4 Text source.** System and bundled fonts, size, colour, outline,
  background, alignment, word wrap, text from a file.
- [x] **4.5 Media source.** Video and audio files (and playlists) with loop,
  restart when shown, speed, hardware decoding; audio into the mixer.
  - [x] 4.5a Files and playlists with loop and speed, hardware-decoded,
    video and audio in step on the timeline.
  - [x] 4.5b Restart when shown (and stop when off air), as OBS offers.
- [x] **4.6 Window and application capture.** ScreenCaptureKit window and
  application modes (with cursor option), and application audio capture.
- [x] **4.7 Browser source.** An offscreen WKWebView rendering a URL or
  local file at a set size, with custom CSS, transparency and its audio.
  - [x] 4.7a The page's picture: URL or file, size, frame rate, custom CSS,
    transparent where the page is.
  - [x] 4.7b The page's audio into the mixer (OBS's "control audio via
    OBS"), not played aloud.
  - [x] 4.7c OBS's page options: shut down when not visible, refresh when
    the scene comes on air, and a refresh method in the API.
- [x] **4.8 Nested scenes.** A scene used as a source in another scene.
- [x] **4.9 Video filters.** A per-source GPU filter chain: Color
  Correction, Chroma Key, Color Key, Luma Key, Crop/Pad, Scaling/Aspect
  Ratio, Sharpen, Scroll, Image Mask/Blend, Apply LUT, Render Delay.
  - [x] 4.9a Filter chains: `filters` on sources (a registry like source
    kinds), run on the GPU before the program and snapshots draw the
    source; Color Correction.
  - [x] 4.9b Keys: Chroma Key, Color Key, Luma Key.
  - [x] 4.9c Geometry: Crop/Pad, Scaling/Aspect Ratio, Scroll.
  - [x] 4.9d Sharpen, Image Mask/Blend, Apply LUT.
  - [x] 4.9e Render Delay.
  - [x] 4.9f `recording.iso.filtered`: ISO files of filtered sources.
- [x] **4.10 Audio filters.** Per-source chains: Gain, Noise Gate, Noise
  Suppression, Compressor (with sidechain ducking), Limiter, Expander,
  Upward Compressor, 3-Band EQ, Invert Polarity.
  - [x] 4.10a Chains in the program mix: `audio_filters` (a registry like
    video filters); changes apply live.
  - [x] 4.10b `recording.iso.filtered` also records audio through its
    filters (no sidechain in ISO files).
- [x] **4.11 Transitions.** Swipe, Slide, Fade to Color, Luma Wipe, Stinger
  (with track matte), Move (items matched between scenes), and per-scene
  transition overrides.
  - [x] 4.11a Swipe, Slide, Fade to Color, Luma Wipe, Move; per-scene
    overrides (`scenes[].transition`); CLI `--transition`/`--duration`.
  - [x] 4.11b Stinger (cut point, track matte); media decoding keeps alpha.
- [x] **4.12 Item controls.** Bounding box types and alignment, scale
  filtering, blending modes, show/hide transitions.
  - [x] 4.12a Bounding boxes and alignment (`fit`, `align`).
  - [x] 4.12b Scale filtering (`scale_filter`).
  - [x] 4.12c Blending modes (`blend`).
  - [x] 4.12d Show/hide transitions.
- [x] **4.13 Audio monitoring and balance.** Hear a source through an
  output device; pan.
  - [x] 4.13a Balance with a setup-wide pan law (sine, square, linear).
  - [x] 4.13b Monitoring (Monitor Off / Monitor Only / Monitor and
    Output) through a chosen output device; outputs in `isocast devices`.
- [x] **4.14 Import OBS scene collections.** Read an OBS scene collection
  (JSON) into a setup, mapping sources, filters and transitions, and list
  anything not supported.
- [x] **4.15 Unknown fields.** Setup validation reports fields a source,
  filter, audio filter or transition kind doesn't have (serde's flatten
  ignores them, so `db = -6` on a gain filter was silently dropped),
  naming the field path and listing the kind's fields.

### V1. Version one (Ryan, 2026-10-11)

Ryan's direction: a very good, workable first version that can be set up
just like OBS (with a better interface if possible), with streaming that
works out of the box, recording, and a really good first experience;
polished, well spaced and fast. Then tested hard. These come before
phase 5; they replace phase 6's tasks.

- [x] **V1.1 Live engine.** While the daemon is live (the app is open, or a
  client asked), sources, the program and the mix run all the time, as in
  OBS. Recording and streaming attach and detach without restarting
  sources; layout, scene and source changes apply at once; clients get
  preview frames (`frame`) and audio levels many times a second
  (`levels` events). `isocast record` still works on its own.
- [x] **V1.2 Streaming.** RTMP and RTMPS output of the program: VideoToolbox
  H.264 and AAC in FLV over Isocast's own RTMP client; presets for Twitch,
  YouTube, Kick, Facebook and custom servers; the stream key kept in the
  Keychain, never in setups or logs; reconnects; bitrate, dropped frames
  and congestion in status; `isocast stream start|stop|status` and MCP.
  Tested only against a local server, never a real service.
- [x] **V1.3 Setup persistence and first run.** The daemon keeps the live
  setup in `~/Library/Application Support/Isocast/setup.toml` across
  restarts; `isocast setup init` makes a good starting setup from this
  Mac's displays, cameras and microphones; permissions are checked and
  requested with clear next steps.
- [x] **V1.4 App shell (was 6.1).** A native macOS window hosting the web
  interface, a thin client of isocastd over the socket; a signed dev
  bundle; a snapshot option so agents can see the interface.
- [x] **V1.5 App: preview and editing (was 6.2–6.4).** Live program (and
  studio-mode preview) monitors; scene and source lists; an inspector;
  drag, resize and crop on the canvas.
- [x] **V1.6 App: mixer and outputs (was 6.5–6.6).** Audio mixer with
  meters; record and go-live controls with timers, per-ISO status, disk
  space, stream health.
- [x] **V1.7 App: settings and first run (was 6.7).** Stream, recording,
  video and audio settings; onboarding that walks through permissions and
  builds the first setup.
- [ ] **V1.8 Hard testing.** Subagents run real scenarios through the app,
  CLI and MCP (setup from scratch, OBS import, record, stream to a local
  server, long runs, device loss) and the bugs they find are fixed.
  - [x] **V1.8 perf-audio/events.** Bound slow watcher queues and share
    serialized event bytes; verify memory under stalled readers, healthy
    event delivery, real-rig recording and synthetic A/V sync.
  - [x] **V1.8 setup-obs.** Fix the setup and OBS-import findings: canvas
    limits, a crashed engine start, off-air audio-only media, recording
    tracks, output size, the on-air scene and missing-file errors.
  - [x] **V1.8 m4-bridge.** Give desktop benchmark jobs Interactive launchd
    policy instead of inherited daemon scheduling; verify timer wakeups on
    exonbook before comparing recording builds.
  - [x] **V1.8 m4-iso.** Remove synchronous direct-HEVC ISO completion that
    blocks a file's audio; qualify the fallback and retain quality presets.
  - [x] **V1.8 m4-throughput.** Explicit zero HEVC look-ahead on the M4 Max
    removes the intermittent video-input back-pressure. Eight final
    interleaved pairs: all 35,040 candidate frames stored, zero program/ISO
    drops, matched A/V durations and clean decodes; main dropped 2,354.
    The hint is scoped to the qualified M4 Max: it regressed the base M2.
  - [ ] **V1.8 m5-retention (needs Ryan).** Requalify real capture-buffer
    retention on an approved M5 after the encoder changes. The M4 hint
    remains off on other chips; synthetic test-Mac results are not an M5
    real-device qualification. See Waiting on Ryan in notes.
  - [x] **V1.8 throughput-diagnostics.** Trace the simultaneous writers and
    compare CPU on both M2 OS versions. The 50% daemon CPU gap did not
    reproduce with matched clips (51.0% vs 49.95%); VT services retain a
    ~16% gap. No unqualified encoder replacement retained.
  - [x] **V1.8 agent-surface.** MCP/CLI/socket fixes from the hard test
    (12 of 14 findings).
  - [x] **V1.8 robustness.** "Never lose a recording" fixes from the hard
    test (all 11 findings).
  - [x] **V1.8 streaming.** Streaming fixes from the hard test (all 9
    findings).
  - [x] **V1.8 encoder-capacity.** Astra's traces show the flagship (4K60
    desktop + 1080p60 camera ISOs + 4K60 program, HEVC "high") exceeds the
    hardware encoders' real-time throughput on a base M2 (always) and an
    M4 Max (sometimes): even raw VideoToolbox took 20.3 s for 16 s of it.
    So degrade predictably and say so up front. `isocast tune` measures it
    accurately (isocast-m2-b, 2026-10-12: HEVC 0.7x real time for the 4K
    program flagship, matching the ~1045/1440 frames recorded; 1.0x with a
    1080p program, which recorded 0 drops in 18/18 files) but its verdict
    is too blunt: both get "nothing fits" (it wants 1.25x headroom) with
    generic advice. Make tune tell "can't keep up" from "keeps up, no
    margin", search concrete variants that fit (program at 1080p, lower
    preset, fewer ISOs, 30 fps) and recommend the closest one as a patch;
    use it at first run, and warn at start when a setup exceeds what tune
    last measured, as OBS's "encoding overloaded" does mid-take.
  - [x] **V1.8 stream-start-keyframe.** A stream starts with two keyframes
    one frame apart (0.000 and 0.017 s), then a clean 2 s grid: start the
    grid on the first frame.
  - [ ] **V1.8 soak.** A one-hour recording (synthetic flagship on a test
    Mac, then the real devices) with memory, CPU, drops and file integrity
    checked; device loss and return mid-recording.
    - [x] **Synthetic soak.** One hour of the synthetic flagship with a
      scene switch every 30 s on the M5 (4K program) and isocast-m2-b
      (1080p program): no drops, every file decodes fully.
    - [x] **Media-loop memory growth.** The live engine's memory grows
      ~22–30 MB/hour with looping media sources (objects autoreleased on
      long-lived engine threads, freed only when the engine stops); see
      Gotchas. Drain them per loop and per imported frame.
    - [ ] **Real devices.** The same hour with the approved devices, and
      device loss and return mid-recording.
  - [x] **V1.8 mcp-events.** MCP has no way to receive events (warnings
    that only go to the event stream): a bounded watch buffer in the MCP
    server and a `get_events` tool with a cursor, or MCP notifications.
  - [x] **V1.8 mcp-field-paths.** Some MCP parameter errors don't name
    the field path (serde `flatten` in InitSetupParams and the internally
    tagged Transition/TransitionKind buffer the error away).

### Priorities (Ryan, 2026-10-13)

C (Create timeline, below) was added later the same day: an editing MVP,
"fast and working", the editing itself later.

"Don't worry about release prep like CI distribution or docs ... We want to
work on scenes as code, plugins. We'll deal with Windows later. We need to
then do extreme stress testing on it to try to make it as performant as
possible ... so insanely optimized that if we were to put it up against OBS
... it would absolutely blow it out of the water ... And we need full
profile and scenes and all that stuff being able to be swapped and
customized too ... I just need to get it working locally." Order of work:
the design direction (Paper) and N.2/N.3, then P (profiles and scene
collections), 5 (scenes as code), 7 (plugins) and S (stress testing and
performance against OBS) in parallel where they don't collide. Section 8
and N.4 are parked under 9.

### P. Profiles and scene collections (Ryan, 2026-10-13)

- [x] **P.1 Model and API.** Named scene collections (scenes, sources,
  transitions) and named profiles (recording, stream, video, audio and
  hotkey settings), stored side by side, swappable and editable without
  restarting: list, create, duplicate, rename, delete, switch, import and
  export on the one method table (API, CLI with --json, MCP). The OBS
  import brings OBS's collections and profiles in as such. A switch while
  recording or streaming is refused or deferred with a clear reason.
- [x] **P.2 App.** Profile and Scene Collection menus as in OBS, in the
  Slint app (after N.2) and the web app until N.3.
- [x] **P.3 First launch finds OBS; Auto-Configuration (#46).** On the
  first launch (after the permissions step) OBS's scene collections are
  found and offered with their profiles' settings; then optimize for
  recording, streaming (upload test) or both, benchmarked with `tune`,
  with a one-line summary; also Tools › Auto-Configuration Wizard….
  `obs_discover`, `speed_test`, `optimize` on the method table.

### C. Create timeline (Ryan, 2026-10-13)

Ryan: "a simple MVP of the sort of editing workflow ... fast and working,
and then we can add the editing stuff later." Like TikTok's clip bar or
Audition: take 1, then take 2 lands after it; a bad take is retaken (the
last one thrown off and recorded again); takes can be removed or moved; at
the end, one joined file. Splitting a take, trimming and removing silence
come later (clips already carry in/out points on their take).

- [x] **C.1 Timeline MVP (engine).** Timelines: ordered clips, one whole
  take each (its manifest, program file and every ISO file), addressed by
  id. `timeline_new` (current from then on: every take that finishes, by
  `stop` or a split, is added), `timeline_get`/`list`/`open`/`close`,
  `retake`, `timeline_remove`/`move`, and `timeline_export` (one program
  file and one file per ISO source every clip has, video not re-encoded,
  A/V in sync at every join, in the background with `export_progress` and
  `export_finished` events) on the one method table: API, CLI
  (`isocast timeline new|list|open|close|show|remove|move|export`,
  `isocast retake`, `--json`), MCP tools, `timeline_changed` events.
  Taking a clip off never touches its take's files; timelines persist
  across restarts and crashes (docs/notes.md, Decisions, 2026-10-13).
- [x] **C.2 Create view in the Slint app.** The clip bar: takes in order
  (thumbnails, lengths), record and retake, remove, drag to reorder,
  export with progress; on the same methods and events. After the console
  redesign of crates/isocast-ui lands.
- [ ] **C.3 Edit view (#70).** Ryan, 2026-10-15: "Edit directly inside of
  Isocast, so all of your ISO sources are shown, and you can just do simple
  clipping with them." Simple and fast, not a full NLE.
  - [x] C.3a Edits on the method table: `timeline_trim`, `timeline_split`,
    `timeline_delete_range` (ripple), `timeline_angle` (per-range source of
    the edited video), `timeline_undo`/`redo` (each edit one revision);
    CLI `isocast timeline trim|split|delete|angle|undo|redo --json`, MCP
    tools. Never touches take files.
  - [x] C.3b Export cut to the edits: every file trimmed frame-accurately
    (smart render: copy whole keyframe intervals, encode again only from a
    cut to the next keyframe), an EDITED file with angle switches (angles
    in another format encoded again whole), audio cut to the sample with
    10 ms cross-fades at joins.
  - [x] C.3c The editor in its own window (Ryan: never change or crowd
    the studio): stacked tracks (program + each ISO) with thumbnails
    (waveforms: follow-up), playhead and scrubbing with a preview of the
    angle, trim handles, split (S), delete range, angle switching, J/K/L,
    I/O, Delete, ⌘Z, export.
  - [x] C.3d Real-window QA and performance on a test Mac: scrub a 30-min,
    3-ISO take; scrub latency and fps; export time against real time.
  - [x] C.3e Editor round 2 (#87): per-clip crops (`timeline_crop`,
    presets 16:9 9:16 1:1 4:5, handles on the viewer; export encodes
    only cropped clips again, in the EDITED file), rolls
    (`timeline_roll`, ⌥-drag), waveforms on every track, snapping (N),
    zoom (⌘+ ⌘− ⇧Z, ⌘-scroll, pinch), edit-point and marker navigation
    (↑ ↓, M ⇧M), Space; motion scenarios for trim, playhead and crop
    drags; EDITED files checked in ffmpeg, AVFoundation, Safari, Chrome.
- [x] **C.4 Project files and the editor after recording (#88).** A
  visible `<Project name>.isoprj` in every project folder (versioned JSON:
  id, takes, timelines with every edit, paths relative to it) that opens
  in the editor from the Finder (exported UTI `dev.isocast.project`), File
  › Open… and Open Recent, `isocast project open` and MCP `open_project`;
  offline takes relinked (`timeline_relink`); "Open Editor After
  Recording" (`recording.open_editor`). Written in the background,
  atomically, never into a folder being recorded.
  - [x] C.3f Editor follow-ups (#94): exports keep one video sample
    description (parameter sets in-band), so HEVC exports play in Chrome;
    full-rate forward playback (decoded ahead, stepped by drawn frames);
    `timeline_export --vertical` (a 9:16 edited file); angle changes
    re-snap crops; waveform pictures off the UI thread; caches keyed by
    path, size and time.
  - [x] C.3g Editor follow-ups 2 (#94): sound while playing (program
    audio on the monitoring device, in sync, pitch-corrected above 1x);
    Export menu with Vertical (9:16); playback paced by the display
    refresh; 10-bit angles encoded again in an 8-bit program's depth;
    VERTICAL naming edges; e2e `edit` checks VERTICAL frame by frame;
    caches evict old keys; offline clips show "Offline"; deterministic
    editor motion suite in CI.

- [x] **C.5 Export to Final Cut Pro, Premiere Pro and Resolve (#104).**
  `timeline_export_nle` (CLI `isocast timeline export --nle
  fcpxml|xmeml|otio [--app final_cut|premiere|resolve] [--output]`, MCP
  `export_timeline_nle`, the editor's File › Export › For Final Cut Pro /
  Premiere Pro / DaVinci Resolve… / OpenTimelineIO…): a project pointing
  at the takes' own files, every ISO on its own track with the program on
  top, the clip's angle the one on, crops, markers, audio per file.

- [x] **C.6 Where a recording goes (#114).** Ryan, 2026-10-17: three
  separate recordings landed as takes of one old "current" timeline.
  Each recording now starts a new project with its own timeline unless a
  record target was chosen (`record_target`, CLI `isocast record-target
  new|ID`, `start --new-project|--into ID`, MCP `record_target`,
  obs-websocket StartRecord and hotkeys follow it); splits and retakes
  stay with their recording; an old current timeline is migrated (shown,
  not used). App: the target next to Start Recording with its menu, the
  status bar, the editor's Record Next Take Here, "Saved as …" toasts.

### S. Stress testing and performance (Ryan, 2026-10-13)

- [ ] **S.1 Head-to-head with OBS.** A repeatable benchmark that runs the
  same scenarios in OBS (installed on a test Mac, driven through
  obs-websocket) and in Isocast on the same machine: CPU, GPU, memory,
  energy, dropped/late frames, encode latency, preview latency, start/stop
  time, with OBS's nearest equivalent settings. Results per Mac.
  - [x] S.1a `scripts/obs-bench.py`: program-1080, program-4k, flagship
    (OBS: its program) and a local stream in Isocast, OBS (OpenGL) and
    OBS (Metal), interleaved on exonbook and isocast-m2-a: CPU (app + its
    VT services), GPU time, footprint, power, distinct/decoded frames,
    the apps' own drop counters, launch/start/stop, file sizes and
    encoder checks. Results in docs/notes.md (Benchmarks).
  - [ ] S.1b Encode latency (frame drawn to packet out) and preview
    latency (frame drawn to on screen, Isocast's app vs OBS's preview),
    and a real-device run (display + camera) once a test Mac has them.
- [ ] **S.2 Extreme stress suite.** Many sources and scenes, many ISOs,
  4K/60 and higher rates, long runs, rapid scene switching and live
  edits, device churn, slow disks, stalled clients; find every limit and
  failure, and fix what breaks.
  - [x] S.2a Chaos-test repairs (findings 1-5, 8, 11, 6's wording): one
    daemon per socket, a stream stop that never waits on a frozen
    server, retries per outage, drop warnings that name a slow disk,
    honest split-vs-stop error, 16 MB request cap.
  - [x] S.2 repair-scale (ported onto main after 5.0 as
    stress/scale-port): the scale run's findings: BGRA ISO deadlock,
    failed files' loss report, filter pass cost, ISO memory under encoder
    overload (compositor re-imports: already fixed by 5.0).
  - [ ] S.2b Chaos-test leftovers (low): program file loses one more 2 s
    fragment than ISOs on a crash, `last_error` empty after a restart
    (6); doctor's disk estimate leaves out media/colour/text ISO files
    (7); `start` replies ~0.2 s before the take (9); every text edit
    reopens the source (10); f32 setup values come back with float noise
    through `serde_json::to_value` (12); a disk-bound take keeps writing
    its backlog ~7-15 s past its end (5).
  - [x] S.2d Stress suite: `scripts/stress.py` (hw-batch kind `stress`):
    many sources and scenes, ISOs at and past the capacity estimate, 100
    switches/s, hundreds of start/stop/split/pause cycles, plugins,
    obs-websocket and watch clients, replay saves, 4-destination
    multistream, long text and browser sources, a filling disk; every
    file decoded and accounted, latency and memory/thread/fd growth.
    Findings (at-limit ISO losses, rapid splits, refused-start state) in
    docs/benchmarks.md "Stress suite".
  - [x] S.2 rapid controls (#80, #81): keep one split handover in flight,
    honor its announced files on Stop, and clear a refused Start before
    answering it. M2-b: two full cycles/disk runs, 0 phantom files, split
    starts 504–1496 ms ahead, 0 stale-busy starts; concurrent controls,
    kill/ownership matrix, full strict e2e and workspace gates pass.
  - [x] S.2 #93 rapid-splits follow-ups: a stopped capture's file keeps
    its last frames, every split sent to a closing writer is answered,
    the split reply marks a file whose source delivers no frames, and
    `status` live.handover says when split/pause are busy. MIN_TAKE kept.
  - [x] S.2 #79 Capacity-limit isolation: recording-native submission
    pressure sheds ISO work before mixed A/V channels fill, including queued
    startup pictures; M2 capacity accounts for per-picture session overhead
    and admission recovery. At/past-limit media and #60/#68 controls verified.
  - [x] S.2c Overloaded ISO tails. Repeat the last accepted picture at the
    final slot when needed, retaining all audio. `repeated_frames` is
    explicit in reports/manifests; drops remain expected minus stored plus
    repeated frames. Round 3: all 72 candidate MOVs duration-matched,
    count-matched and decoded cleanly; main mismatched in 10/72. Disable
    recording B-frame reordering so overloaded crash fragments also retain
    presentation order; six final-byte killed files decoded cleanly.
- [ ] **S.3 Astra optimization rounds.** GPT-6 Astra (opencode, xhigh)
  sessions with S.1/S.2 as their measuring stick, one hot path at a time,
  until Isocast beats OBS on every measure we can control.
  - [ ] **S.3 #27 Frame-pool invariant.** Distinct-buffer ledger across
    native/GPU owners, independent recording/auxiliary reserves, capture
    isolation, status/doctor and governor feedback. Verify many-ISO/canvas/
    replay/multiview/mask stress, flagship A/B, gates and strict e2e.
    - [x] Buffer accounting and reservations implemented and qualified:
      baseline 461 program losses → zero across ten files; final gates
      244 unit tests + doctest, strict 18/18, flagship-1080 3-pair A/B.
    - [ ] Stronger native-stream-stall guarantee and device qualification:
      the existing native priority controller still sheds ISO frames when
      a live submit stalls, despite zero permit refusals; display permission
      and installed CMIO are unavailable. See frame-budget scratch REPORT.md.
  - [x] **S.3 #111 multiview and GPU follow-ups (perf-2), implementation.** Tile-sized
    scene rasters and completed program/preview reuse; contained GPU poll
    panics, snapshot/ISO loss recovery, virtual-camera pacing. #27's frame
    permit draft is removed and parked outside the tree (it double-counted
    shared deliveries). Reduced-tree ISO stress has zero protected losses;
    e2e 18/18, GPU/camera recovery checks and workspace gates pass. Tier-1
    review/merge remains the coordinator's; handoff
    `/Volumes/IsocastBuild/perf-2/scratch/REPORT.md`, "Split for merge".
  - [ ] **S.3 #26/#29 Shared GPU and upstream ISO admission.** One device
    per process, bounded asynchronous GPU hand-offs, BGRA native monitors,
    and ISO admission before conversion.
    - [x] Implementation and remote release/workspace gates.
    - [x] M2 paired A/B, strict e2e, cycles/disk/ISO stress, native surface
      and virtual-camera readback. Final program/stream/ISO drops are zero
      in all three benchmark workloads; memory trade-offs are measured.
    - [x] Tier-1 first review (3f327f7): merge with follow-ups.
    - [x] Fix round 1: timeout/lost-device recovery, coordinator-published
      ISO split demand, H.264 monitor readbacks; main 603c3ac integrated,
      remote gates and strict e2e 18/18 pass. Overloaded split cycles account
      all ticks in 152 files; quick A/B plus longer multiwindow confirmation
      keep every protected output whole, with no material performance change.
    - [ ] Coordinator re-review and completion of the uncommitted merge.
      Handoff: `perf-gpu/scratch/REPORT.md`, Fix round 1.
  - [x] **S.3a Recording-stop latency.** Finalize when the last video frame
    and all audio arrive, retaining the late-input allowance. Four paired
    rounds against main and OBS on M4 Max and M2; normal files complete,
    zero drops, equal A/V durations, crash fragments verified.
  - [x] **S.3a-f1 Restore live jitter tolerance.** Keep the 200 ms live
    budget; at Stop drain complete audio to recording routes only, pacing
    concurrent live taps at their original deadlines. Ten stream pairs
    on M2 and four on M4: zero candidate video/audio drops. Re-confirm
    stop/integrity, concurrent manual/timed stops, and induced VT stalls.
  - [x] **S.3b In-process profiles.** Profile 1080p, 4K and streaming on
    both Macs. The meter SIMD trial was removed in follow-up 1 because
    neutral-or-better process CPU was not established.
  - [x] **S.3d Bounded stream stall reserve.** A stream's encoder worker
    waits for outstanding submissions rather than discarding at eight;
    its tap reserves 96 packets without delaying normal delivery. M2:
    twelve interleaved pairs zero-drop, and ~110/210/310 ms VT freezes
    lose 0 video/audio versus main's 18/31. Recording integrity retained.
  - [ ] **S.3c Further CPU/graphics-residency optimization.** Round 2
    re-measured the merged renderer: M4's static 1 fps control falls from
    464 to 118 MiB; moving 4K content still has 24 non-purgeable 8 MiB
    graphics regions and startup peaks remain. Removing canvas metadata
    did not establish a CPU win; decoder-reader reuse introduced loop
    frame gaps. Both trials were removed. Direct instruction counts now
    work on both Macs. Further safe whole-process CPU reduction remains
    open; S.3d separately fixes the measured encoder-freeze losses.
  - [x] **S.3e Round-2 qualification.** Four interleaved main/candidate/OBS
    rounds per scenario on both Macs (96 rows), final-byte stream/stall
    and raw-wire checks, matched M4 memory controls, and file/crash checks.
    All zero-drop files match A/V durations; overloaded flagship tails
    differ in 9/80 files across both builds (documented, not claimed fixed).
    Full report: `/Volumes/IsocastBuild/astra-s3r2/scratch/REPORT.md`.
  - [x] **S.3f Graceful overload.** Program/stream admission precedes ISO
    work; sustained load reduces ISO admission instead of filling every
    queue. Stop waits for all accepted frames/audio and counts tail repeats.
    Against 85c5d6d: M4 flagship Stop 723 -> 127.5 ms (eight pairs), M2
    989 -> 150 ms (four); program drops zero. M2 simultaneous stream:
    program and stream zero video/audio drops in four pairs, at the expense
    of 10,345 ISO pictures. Healthy three-file controls, split/cancel,
    continuous/offset audio and crash fragments verified. Further M4
    zero-drop throughput work remains separate; round-three report in
    `/Volumes/IsocastBuild/astra-s3r3/scratch/REPORT.md`.
  - [x] **S.3g M4 Max throughput.** Native controls with deterministic
    property order and real VT traces distinguish unset (automatic)
    look-ahead from an explicit zero, although both read back as zero.
    Eight final M4 pairs: flagship Stop 747 -> 83 ms, zero candidate drops;
    four final M2 pairs per scenario retain zero protected program/stream
    drops, equal final A/V durations and truthful ISO losses. Final-byte
    codec/lifecycle/crash checks and release gates pass; full tables and
    rejected global-hint/sustained-pressure trials are in the round report.

### T. Self-tuning on every machine (#110, 2026-10-17)

Design and safety contract: [docs/self-tuning.md](docs/self-tuning.md).
Tier 1: build → review → fixes → re-review for each behavior-changing slice.

- [ ] **T.1 Measured machine profile.** Silent, cancellable, low-priority
  calibration within 30 s, first launch/updates/on demand, never alongside
  recording/streaming. Platform-neutral codec, compositor, conversion,
  memory/disk measurements; versioned isolated persistence; method table,
  CLI/MCP/status/Settings; measured capacity/Optimize with unchanged absent-
  profile fallback. AWS M2s and exonbook: compare existing capacity cases
  against real drops, cancel/start races, fresh-machine behavior.
  - [x] **T.1a Profile foundation (approved, with follow-ups).** Portable
    measurements, killable idle-only helper, persistence/build invalidation,
    API/CLI/MCP/Settings and covered HEVC capacity/Optimize integration.
    M2s/exonbook recordings, refusal/cancellation/deadline, fallback and full
    strict e2e verified; details and scope limits in the lane's scratch
    REPORT.md. Fix round 1 confines automatic calibration to the default
    daemon, unifies capacity checks and preserves exact Optimize passes;
    main integrated, 233 tests and strict e2e 18/18 pass.
    F4/F8: automatic calibration defers/preempts for preview; offline file
    checks explicitly say capacity is not checked and never launch a daemon.
    Both verified on exonbook and included in ca4cc010.
  - [ ] **T.1b Complete coverage and prediction qualification.** Other
    ProRes variants, live-H.264 resource costs, missed 4K60/10-bit cells,
    repeatability under outside load and post-GPU-lane integration. M4
    overload prediction improves; M2 percentage accuracy is within variation,
    not proven better than the table. Do not call all of T.1 complete yet.
    - [x] **T.1b.1 Bounded coverage and canvas predictions (merged6d6d58fd).** Separate
      recording/live paths and fit/diagnostic evidence, prioritize missing
      4K60/10-bit and ProRes variants, list skipped cells, qualify covered
      canvas predictions and repeated/outside-load calibration on base M2.
      Diagnostic timing alone does not qualify a mixed-resource model.
      M2: 22.26–22.86 s, all five ProRes variants/live H.264/UHD60 HEVC10;
      19–21/24 cells explicitly covered. Fitting cases lose no frames;
      overload prediction53.09%, actual44.39–53.67%, table52.62%: within
      variation, not improved M2 percentage accuracy. Full T.1b stays open.
    - [x] **T.1b.2 Hybrid mixed H26x model (ready for review).** Local HEVC
      costs plus independently normalized table H264 file/live costs, explicit
      mixed provenance. Re-evaluate coverage for each suggestion/admission;
      unknown source formats fall back, added ISO sources are probed. Real
      HEVC+H264 file pairs and two-live-hub+HEVC cases fit/overload as predicted;
      ISO loss percentages remain approximate, not improved accuracy. Exact
      H264-only/10bit/ProRes fallback and Optimize notes verified. Local H264
      coefficients and ProRes pool models remain open, not inferred from diagnostics.
    - [x] **T.1b.3 ProRes resources (ready for review).** Full concurrent variant/native HW-vs-SW
      observations, quantitative UHD bound, same-volume disk planning and
      separate CPU/GPU/frame-bandwidth accounting; no free-ProRes or unknown
      admission claims. Qualify noisy program/ISO/mixed workloads on AWS M2s,
      software fallback and governor consent/pins qualified. Final gates250,
      strict18/18 once, three ProRes A/B pairs0loss. Hardware variants and
      mixed HEVC/ProRes/H264 output decode/count correctly. Actual software
      EncoderID mapping and fresh per-CPU counters verified. SW4444 paired
      ISO startup still loses native pictures while program/audio stay whole;
      model explicitly refuses a Fits/admission claim for that unqualified
      recovery case (not hidden by changing native safety). Program-only SW
     4444 and other tested SW variants are loss-free. UHD scaling remains
      conservative, not an exact capacity/loss forecast. Evidence:
      self-tuning-2/scratch/prores/REPORT.md.
      Windows profiles wait for the Windows daemon switch.
- [ ] **T.2 Live governor.** Fixed reversible degradation order, hysteresis,
  visible reasons and protected-output integrity; qualify against outside
  CPU/GPU/encoder load without changing the established priority contract.
  - [x] First reversible slice (merged via f5b7d263; review approved with follow-ups):
    monitor/vertical ceilings, explicitly opted-in ISO ceilings/hold, upload bitrate,
    status/events/UI and pins/off. AWS ten-minute CPU/GPU A/B: zero losses
    and zero governor steps. Foreign-encoder A/B: six bounded changes,
    recovery without cycling, intact file accounting, but severe load still
    loses protected video and program audio on both builds. Main257ea057
    integrated via eaf0b6a7:240 tests, strict e2e18/18 and seven native
    media contracts pass; animated mask/filtered ISO, split during hold and
    native monitor readback verified. Scratch REPORT.md is authoritative.
    B1/B2/B3/F1 fixes: owner-local intentional refusal attribution, nonfatal
    preview retune refusal with per-feed errors, `allow_iso_thinning=false`
    by default, and pressure/lifecycle-triggered sampling. Real before/after
    vertical pin/recovery and retune refusal tests pass; idle CPU is about
    0.01% of one core, indistinguishable from disabled policy. Final gates240
    and strict e2e18/18 pass; fixes committed7c08ca3d and mergedf5b7d263.
  - [ ] Remaining qualification/steps: safe file-format/effort/software
    transitions, JPEG/vertical composition reductions, memory-pressure and
    per-encoder retune failure behavior. No zero-drop promise against foreign
    workloads that already exhaust protected encoder capacity.
  - [x] **T.2b Per-hub refusal/acknowledgement (ready for review).** A
    failed encoder no longer restores healthy siblings. Weak owner lifetime,
    active-hub aggregation and refusal suppression verified with three real
    hubs: healthy1500/failed4000/other-canvas1250; closing/recreating failed
    owner clears/retries only that hub. Congestion ladder remains canvas-scoped.
- [ ] **T.3 Low-end defaults and pins.** Measured 8 GB/base-chip defaults,
  persistent per-setting Automatic/pinned controls, backend-neutral mapping
  to NVENC/QSV/AMF/software with the Windows lane.
  - [x] **T.3a Conservative initialization (merged6d6d58fd).** New setups/blank profiles use
    30 fps on <=8-core or <=8-GB machines; explicit fps and existing profiles
    survive. Qualify persistent pins/ISO consent, base M2 defaults and native
    rate rollback. Physical 8-GB/backend mapping remains separate.
    API init/blank30 vs explicit/copy60 and restart persistence pass. Scoped
    native refusal reproduces and fixes skipped FPS rollback/false recovery;
    restored settings are actually acknowledged before clearing uncertainty.
  - [x] **T.3b Named secondary pins (ready for review).**
    `canvases.<id>.fps` protects just that secondary canvas; legacy
    `vertical.fps` protects all. Writers retain actual canvas ids. Real files
    prove pinned shorts/main/ISOs whole, square thinned before live pin and
    continuous30fps afterwards. Pins persist; native safety is not disabled.
- [ ] **T.4 Weak-machine matrix.** Prototype taskpolicy/CPU limits and a
  GPU-contention helper during T.1; then bounded memory pressure, full
  e2e/bench/soak, GPU-less Windows software encoding and governor recovery.
  - [x] **T.4a Bounded macOS contention (merged6d6d58fd).** Real touched-memory competition
    with verified readiness/cleanup, OS memory-pressure signal, base M2
    strict e2e/bench/short soak and recovery. This is contention, not a
    physical core/RAM limit or Windows software-encoder qualification.
    CPU4/Metal/2GiB: strict18/18, three interleaved A/B pairs and a288 s
    30-fps soak/crash checks pass, zero reported media losses. Injected
    memory warning/critical respects pins/ISO consent and restores fully.
    Gates242 pass. Tier-1 reviewer unavailable; no review approval claimed.
  - [x] **T.4b Real AWS memory pressure (ready for review).** Authorized
    `memory_pressure -p33` without `-S` produces7.92–8.40GiB available and
    actual warninglevel2. Under program/UHD+HD ISO/stream/preview load,
    files2841frames each and stream2793 lose nothing; preview restores fully
    59.91s after release. Watchdogs/guards/cleanup verified. This simulates
    an available-memory budget, not a physical8GiB chip. Gates243, strict18/18
    and stream A/B3pairs pass. Astra fallback review/re-review accepted;
    regular-model review remains required before release.

### A. Alpha: the daily driver (Ryan, 2026-10-13)

The stopping point: once these are done, features freeze and Ryan uses
Isocast for his daily content creation and streams for a stress-test week,
sending bug reports; then fixes, then the next phase. Everything not here
waits (full workspaces, other themes, scenes 5.3+, plugin tiers 7.3/7.4b/5.7,
layout editing, Windows, release prep).

- [x] **A.1 Native app by default.** The Console redesign (N.5) merged,
  then N.3: the Slint app is the app Ryan opens.
- [x] **A.2 OBS workflows in the native app.** Scenes, sources, filters,
  transitions, Studio Mode, mixer, program + ISO recording, streaming:
  one scripted pass in real windows on the test Macs, every finding fixed.
  - [x] **A.2 QA-3 R3 hotkeys:** accept focused input even when registered
    globally; pair matching native/focused press edges, preserve repeats and
    hold cleanup. Verify pause/marker on m2-a and identify Cua routing limits.
  - [x] **A.2 app #1 (QA-2 #2):** Quit, Close Window, Exit and the close
    button ask "You are currently recording. Stop and quit?" while
    recording or streaming (Stop and Quit / Keep Recording / Cancel).
  - [x] **A.2 app #2 (QA-1 #2):** a corner handle keeps the aspect ratio,
    Shift frees it; side handles stretch (Shift keeps the shape there).
  - [x] **A.2 app #3 (QA-1 #5):** after a click on a Scenes or Sources row,
    ↑/↓ move the selection (switching scenes) and F2 renames.
  - [x] **A.2 app #4 (QA-2 #3):** toasts time out (errors and warnings
    8 s, held while hovered), at most three, never over an open dialog.
  - [x] **A.2 app #5 (QA-1 #8):** the Filters (and Properties) preview
    shows the source through its filters (`shared_frames` `filtered`).
  - [x] **A.2 app #6 (QA-1 #7):** filter rows have a menu (Rename,
    Duplicate, Copy, Paste, Remove); a source's menu has Copy Filters and
    Paste Filters; every change is one setup edit.
  - [x] **A.2 app #7 (QA-1 #6):** Group Selected Items (⌘/Shift-click in
    Sources, right-click), Add → Group, Ungroup and Move to Group, on the
    engine's group model (items with children), one edit each.
  - [x] **A.2 app #8 (QA-1 #4, app part):** a size field set alone takes its
    pair from the source (one patch); refused changes name the source.
  - [x] **A.2 app #9 (QA-2 #6, polish):** menus as wide as their choices,
    whole names on hover in Sources and the mixer, a narrow Sources dock
    gives the lock's room to names, LIVE orange while reconnecting, Show
    Recordings opens the folder, hover outline cleared, Enter Full Screen
    (⌃⌘F), en-US "Color".
  - [x] **A.2 app #10 (after main):** items without bounds (`fit: none`)
    resize by `style.transform.scale` on the canvas and in Edit Transform;
    stretch items keep resizing the rect; Controls dock says "Stop" when
    "Stop Recording" would be cut beside Pause and Marker.
  - [x] **A.2 re-pass #5 (polish):** Exit always in the Controls dock (a
    square button beside Settings in a short dock); a refused start shows one
    toast, not two.
  - [x] **A.2 re-pass #3:** Settings → Output greys out MP4 for ProRes
    (from `Codec::containers`), and a refused Apply/OK says why in the sheet.
  - [x] **A.2 re-pass #2:** a stream key stored by another build shows as
    unusable in Settings → Stream, which takes it again.
  - [x] **A.2 re-pass #1:** Open GitHub Issue opens a short link; the full
    triage block goes on the clipboard (and stays in the zip).
  - [x] **A.2 re-pass R2:** the Scenes and Sources docks follow the program
    after the Create view, dock toggles, Studio Mode and refused starts.
  - [x] **A.2 QA-1 #1 engine:** native-size source items; preserve explicit
    bounds and custom legacy boxes; API/CLI, round-trip and offscreen checks.
  - [x] **A.2 QA-1 #3 engine:** publish pending opens and permission hints;
    native opens/snapshots time out, independent sources keep opening.
  - [x] **A.2 QA-1 #4 engine:** validation identifies source/field; atomic
    paired width/height source patch verified through CLI.
  - [x] **A.2 QA-2 #1 engine:** reject invalid codec/container pairs at
    apply/start; encoder/container catalogue on API, CLI, MCP and schema.
  - [x] **A.2 QA-2 #4 engine:** omitted ISO audio uses each source's own
    sound; explicit routing and per-source opt-outs persist.
  - [x] **A.2 QA-3 R1 engine:** clock silent ISO audio through off-air gaps;
    preserve end-of-take beep/flash sync and count only actual audio losses.
  - [x] **A.2 QA-2 #5 engine:** collection switches restore program and
    Studio preview; API/CLI/MCP can clear preview and app follows it.
  - [x] **A.2 QA-2 #7 engine:** imported names are distinguishable;
    source mono downmix is available on the shared audio control surface.
- [x] **A.3 OBS gaps for daily use:** custom hotkeys (G.5), pause
  recording (G.3), virtual camera (G.1). Replay buffer and projectors wait.
- [x] **A.4 Graceful overload (S.3 round 3):** the program and stream stay
  whole while ISOs drop first; A/V durations equal (S.2c); stop stays fast.
- [x] **A.4 ISO admission over-shedding (GitHub #68):** place the configured
  stream/replay encoder with one picture before the recording writers, park
  it until an output joins, and retain it through splits. M4 record-first:
  three short runs, 181 s and six split takes lose no ISO/program/stream
  pictures. M2 fitting load stays clean; overload sheds ISOs only, including
  cold startup. Admission and native writer policy unchanged.
- [x] **A.4 Parked program tap (GitHub #77):** detach after the warm
  picture; synchronously re-tap on the first real join, with canvas ownership
  serialized against edits. ISO-only parked recordings draw no program
  frames; M2-b interleaved CPU saves 4.48 points of one core. Heavy/fit
  record-first matrix keeps protected outputs clean; 150 s park, changed
  canvas then join, and ten join/edit races verified. Tier-1 re-review passed.
- [x] **A.4 Hung device opens (GitHub #82):** a source open given up after
  8 s no longer blocks every retry until macOS returns: opens are keyed by
  source and device, a retry starts a fresh open (at most 2 stuck per
  source, waiting within the 8 s for one to return), and the error says
  what to do. Root cause on Ryan's Mac: the Studio Audio HAL driver stopped
  answering Core Audio after the 7 h take (startRunning blocked 11.5 min).
- [x] **A.4 Replay flash/beep offset (GitHub #78):** not the save, the
  shared encoder or the file: the test pattern's generator thread (default
  QoS) woke up to 104 ms late and skipped the pulse's first frames, so the
  program (recording too) showed the flash two frames late. It now runs at
  the program thread's QoS. M2-b interleaved: base 2/15 runs at 33.31 ms,
  fixed 0/30, generator late wakes 1-7 per run to 0.
- [x] **A.4 Media previous-frame pictures (GitHub #126):** expose the
  already-decoded next frame to timestamp-based selection, without waiting
  for its producer's delivery wake. Five strict M2 1080p60 runs: 21,108
  oracle checks, zero mismatches; full strict e2e 26/26; A/B gate passes.
- [ ] **A.4 Split + stream losses (GitHub #60), qualification partial:**
  synchronous native stream submission stalls now signal pressure; final
  build's 16 post-split takes have zero protected losses. Cold stream startup
  still loses 9–12 program/stream pictures under heavy M2 load, so the strict
  zero-total-drop acceptance remains open. See split-drops scratch REPORT.md.
- [x] **A.4 LAN streaming (GitHub #57):** both bundles declare
  NSLocalNetworkUsageDescription (release.sh refuses one without it); the
  RTMP connect tries every address, IPv6 and IPv4 alternating; .local
  lookup and "no route" to a private address name the Local Network setting.
- [x] **A.4 Stream key per build (GitHub #63):** keys live under
  `<app bundle id>.stream`; a readable 0.0.3 key under `dev.isocast.stream`
  moves once, one another build saved is ignored, so Save Key works.
- [x] **A.4 ISO take loss (GitHub #41):** VideoToolbox placed sessions
  started within a few milliseconds of each other (or during another's
  teardown) three to one engine, each then at ~45 fps for the whole take,
  which tripped the pressure controller. Sessions now start in order
  (program, then largest first), 25 ms apart, and a split's old files stay
  open 200 ms past the split. Evidence: iso-drops-2 scratch REPORT.md.
- [x] **A.4 ISO capacity (GitHub #66):** `estimate_capacity` (API, CLI
  `isocast capacity`, MCP) predicts from per-chip data whether a setup's
  files fit this Mac's encoders, with fixes as patches and `add_iso`
  admission; setup check/doctor warn, start notes it, `encoder_overload`
  comes once per take, the app shows one notice with a fix (after the take
  while recording) and "don't show again for this setup", and turning an
  ISO file on picks a fitting size or leaves it off.
- [x] **A.4 Outside-load warnings (GitHub #90):** read-only detection,
  status/CLI/app visibility, and at most one warning event per cause per
  three minutes. Fix round 1 removes the unsuccessful early ISO hold:
  admission is main's, CPU-burner ISO fps matches main within run variation,
  protected outputs stay whole, and external-encoder warnings still precede
  stream drops. Recording/pause/connection text stays visible; the API names
  the five-second baseline window. Tier-1 re-review pending; no claim to
  prevent losses caused by uncooperative external encoders.
- [x] **A.5 Timeline MVP (C.1, C.2):** takes append, retake, remove,
  reorder, export one joined file; a simple Create view in the app.
- [x] **A.6 Log everything, report bugs in one click.**
  - [x] **A.6 dropped-file stop summaries (GitHub #42):** one warning per
    lossy file/take on the shared report, including percentage and path.
    API/CLI/MCP verified on six 30 s takes and a ten-minute split recording;
    13/13 strict e2e scenarios pass. The underlying ISO loss (#41) remains.
  - [x] **A.6 app:** Help > Report a Bug... in the Slint app: description,
    preview of exactly what is shared, the zip, the prefilled GitHub issue.
  - [x] **A.6 review #1:** contain panics, emergency-finalize poisoned state,
    and surface/repair interrupted manifests; reviewer repro on m2-a passes.
  - [x] **A.6 review #2–4:** infallible logging startup, compact polling,
    protected per-process crash retention, Mach CPU conversion. Reviewer
    volume/flood and full log-only disk repros pass; ps/log CPU ratio 1.0031.
  - [x] **A.6 review #5–12:** bounded fail-closed credential memory, exact
    credential fields, configured Keychain export seeding, HTTP capability
    path redaction, per-report HMAC/device pseudonyms, nonsticky warnings,
    incident-time triage, compact/paged MCP preview, explicit severities,
    deferred camera diagnostics and durable redacted native/plugin output.
  - [x] **A.6 re-review N1–N7 (fix round 2):** LRU credential memory with
    in-place oversized scrubbing and per-report Keychain-seeded redactors;
    compositor panics fail the program file in the manifest and degrade
    status; true failed-file durations; one contained spawn helper with owner
    cleanup; unthrottled errors and disconnect omission counts; quiet console
    fallback; no Debug text. Reviewer repros pass (m2-a + local).
  - [x] **A.6 fix round 3 (R2-1–R2-5):** continuously drained/retrying native
    credential delivery, lifetime-pinned actual credentials, pause-aware
    recovery and incident triage, failed-frame exit status, and failed-program
    Start/Resume refusal. Reviewer repros and 13/13 e2e cases pass on m2-a.
  - [x] **A.6 review long run:** two hours, program+ISO 216000 frames each,
    no drops, 67056 polls, 40.7 MB logs within the daemon budget, prior
    crash evidence retained throughout (isocast-m2-b).
  - [x] **A.6 hardening (M-1, M-2):** pin successful Keychain writes only;
    current plus previous two values per service/account. Supervise/reseed
    native relay restarts, report warnings, drop seeds during outages.
    3000 failed writes leave pinned count zero and logger/RSS near baseline;
    relay-kill and 12000-value unavailable-helper churn preserve recording,
    bounded memory and redacted native logging after recovery.
  - [x] **A.6a Diagnostics backend:** shared bounded JSONL, API/events,
    one-second metrics, panic/abnormal-launch detection, redacted offline
    report preview/zip on API, CLI and MCP (agent/a6-diagnostics).
  - [x] **A.6b Functional qualification:** real recording/stream/error/crash
    bundles, planted-secret/title audit, retention, native crash collection,
    CLI/MCP/app-launch hooks and available counters checked on isolated engines.
  - [x] **A.6 one-click reports (report worker):** `bug_report` `submit`
    (CLI `--submit`, MCP, the app's Send Report) posts the previewed
    ISSUE.md and zip to services/report-worker (GitHub App issue, private
    R2 zip, secret scan, dedupe, rate limits, maintainer fetch). Deployed
    and live since 2026-10-14.
  - [ ] **A.6 strict zero-overhead gate (follow-up, GitHub #7).** Review-fix 6+6 versus current
    main a619986, including descendant helper CPU/RSS: 48.9 → 49.0% of
    one core (+0.1 point), instructions 553.1 → 558.7 M/s; 18/18 complete
    files per build, zero video/audio drops. A literal zero-overhead claim
    is not made. Details in notes and
    `/Volumes/IsocastBuild/a6-diagnostics/scratch/REPORT.md`.
  - Remaining integration/availability: the redesigned app's Report a Bug
    menu is deferred by the A.6 brief; exact AVAssetWriter-private encode
    queue depth/latency have no public counter and are explicitly null,
    alongside measured H.264 queues/latency and recording drops/disk pressure.
  - Structured logs from the daemon, app and CLI on every launch, every API
    call with its result and timing, every event, warning, state and device
    change, and once a second while live: CPU, GPU, memory, drops per
    output, encoder queue depth, disk speed, thermal and power state.
  - Crash capture: panics with backtraces, macOS crash reports for our
    processes, "last run ended abnormally" at the next launch.
  - `bug_report` on the method table (Help > Report a Bug… in the app,
    `isocast report`, MCP): a GitHub issue body (summary plus a
    machine-readable triage block for the agent that reviews it) and a zip
    (logs, metrics, crash reports, manifests, setup, plugins, permissions,
    an environment snapshot of the Mac: macOS build, chip, memory,
    displays, cameras and mics with formats, disk).
  - Secrets (stream keys, tokens) always redacted; the user previews
    everything before it leaves the Mac; nothing is sent automatically
    (it opens a prefilled GitHub issue to post with the zip attached).
- [ ] **A.7 Exit test (GitHub #2).** A 2-hour soak with real camera, mic and display,
  program + ISOs + a test stream, 0 drops on Ryan's M5 Max; kill and crash
  tests recover every recording; the A.2 checklist passes; then hand over.
  - [x] **A.7 engine-e2e.** A single-file synthetic engine harness, isolated
    per scenario, with actual recording/stream/export/sync/scene/text/plugin
    checks and crash recovery; macOS arm64 CI plus interleaved base/head PR
    benchmarks and a trusted, data-only comment publisher. Eleven scenarios
    pass on the local Mac and isocast-m2-a; pause/bug_report explicitly skip
    until those methods land. Hosted first-run steps are in the e2e report.
    - [x] **Hosted VM policy.** Inventory CPU/RAM/GPU/VT and qualify 720p30
      H.264 with actual program+ISO/RTMP runs; retain zero drops when it fits,
      otherwise cap accounted hosted video loss at 25% per output without
      relaxing integrity/audio/sync checks. Local/test-Mac 1080p60 stays strict.
      Both CLI modes verified locally; the next published run qualifies the VM.
    - [x] **Hosted pacing, local reproduction and diagnostics.** Replace media
      sleeps subject to launchd coalescing with absolute kqueue deadlines and
      bounded leeway. One original M4 matrix stored 10,800 frames with no loss;
      review later reproduced loss on all test Macs in throttled contexts.
      Both Macs passed 11 E2E scenarios over SSH, where main also passed.
      Add native timer/Metal/process inventory and compositor wake/draw/wait
      timings before hosted qualification. See the hosted-pacing report.
    - [ ] **Hosted pacing, runner qualification (GitHub #5).** Confirm
      the cause and final bytes on GitHub's VM; do not infer a GPU limit from
      empty system_profiler output. M2 launch-context checks still lost nine
      frames in two 60 fps takes; universal zero-drop and universal CPU-neutral
      claims are not established. Retry commands are in notes and REPORT.md.
    - [x] **Adaptive pacing implementation and review follow-ups.** A fresh
      process starts with ordinary sleeps, escalating on measured stretching;
      virtual camera uses the same controller. Active program/source/ISO/camera
      modes, wake percentiles, maxima and late counts are on status. Diagnostics
      do not lock media workers. CI excludes app/probe binaries. Main merged
      first; normal E2E 13/13 on M4 and M2, diagnostic camera transport checked.
    - [x] **Adaptive non-regression qualification, process-wide evidence.**
      Normal CPU accepted within noise by the delegator. No recording-start
      wait: once any clock detects stretching, new clocks start critical and
      early-created clocks adopt the evidence at their next wait. The evidence
      survives live-engine restart, ending only with the process. Final M4/M2
      default/Background matrix: 162/162 files pass every integrity check;
      comparative drops equal/lower in all cells with the targeted M4
      Background30 control included (both builds have ISO-throughput outliers).
      Both strict normal E2E suites 13/13; fmt/clippy and 180 tests pass.
      See REPORT.md **Process-wide escalation**, including all outliers and the
      rejected constructor-only trial. No claim of loss-free Background operation.

- [x] **Developer mode (GitHub #47).** Settings › Advanced › Developer
  mode (the profile's, off by default, auto-off after 24 h), `isocast debug
  on|off|status [--json]`, MCP `developer_mode`, API `debug_status` and
  `debug_set`: every API call and debug messages from capture, compositor,
  encoders, writers, streaming and the updater, and per-second frame timing
  traces, in their own 200 MB log segments; the Developer menu and the live
  stats overlay. docs/developer-mode.md.

### G. OBS gaps (Ryan, 2026-10-13: "everything in OBS natively and fast")

What OBS has that Isocast doesn't yet (from docs/obs-parity.md, the N.2
notes and section 9). Each on the method table, CLI and MCP, then in the
native app; each verified against OBS's behaviour on a test Mac.

- [ ] **G.1 Virtual camera.** The program as a macOS camera (CoreMediaIO
  camera extension), selectable in other apps; start/stop from the API.
  - [x] **G.1a Engine and extension build.** Isocast-only CMIO extension,
    IOSurface output, program/preview/scene/source selection, API/CLI/MCP,
    events, waiting card and opt-in host embedding. Separate-process
    IOSurface reader, colour/timing, recording/stream coexistence and
    camera-off/on flagship measurements on isocast-m2-a.
  - [x] **G.1 installer (Follow-up 1).** One-command, confirmation-gated
    Xcode automatic-signing installer and uninstaller; exact-id profile
    verification and an isolated AVFoundation frame check. Project generation,
    unsigned/ad-hoc Xcode build, strict signature checks and dry-runs verified;
    account provisioning and activation intentionally remain Ryan's steps.
  - [x] **G.1 app.** Start/Stop Virtual Camera in the Slint app's
    Controls dock with its output and size; install help, never installs.
  - [ ] **G.1c Ship the extension in the release app (#67).**
    - [x] release.sh/workflow build, embed and Developer ID-sign it with
      both profiles, refusing builds without them; Start activates it;
      the app explains the System Settings approval.
    - [ ] Developer ID activation on a test Mac, camera clients,
      coexistence and removal (needs Ryan's two profiles).
    - [x] Follow-ups (#69): Start replaces an older active extension
      build after an update; Remove Virtual Camera (API/CLI/MCP/popover)
      deactivates it, replacing an older build first; the approval alert's
      name and the nonce idea are documented as not possible.
  - [ ] **G.1b Installed camera qualification (needs Ryan).** Apple-team
    signing and explicit activation/approval, then actual camera clients,
    multi-client buffer lifetime, extension CPU/GPU and client latency.
    Commands/entitlements/team in docs/virtual-camera.md; no extension was
    installed by G.1a. See Waiting on Ryan in docs/notes.md.
  - [x] **G.1 review fixes.** Reproduce and resolve the twelve findings:
    bounded, authenticated native sink subprocess; no lifecycle waits under
    daemon state; capture/JIT entitlements and build versions; producer-first
    format negotiation, signed-producer admission, API validation/defaults,
    off-state selection cleanup and developer-only diagnostics. Reviewer
    lifecycle/churn repros and installer dry-run rerun; actual installed
    camera clients, full provisioned-host capture and replacement stay in G.1b.
- [x] **G.2 Replay buffer.** Keep the last N seconds of the program (and
  ISOs) in memory; save on command or hotkey. Program only; ISOs in the
  buffer and sharing the recording's encoder are follow-ups.
- [x] **G.3 Pause recording.** Pause/resume a take without a new file;
  A/V stay in sync; the manifest records the pauses.
  - [x] **G.3 app.** Pause/Resume in the Slint app's Controls dock and
    control bar, PAUSED on the LCD, the pauses in the REC details.
  - [x] **G.3 R1/R2 recovery contract.** Unknown recoverable duration when
    writers cannot acknowledge committed fragments; no cut-derived durability
    claims or exact loss messages. Exact-boundary/immediate-kill/Resume matrix
    across codecs and 4K load; queued double-toggle behavior documented.
  - [ ] **G.3b Crash-safe pause (and crash-safe takes generally).** Today
    a fragment reaches disk only when a later sample is appended, so a crash
    during a pause can lose everything since the last 2 s fragment, and a
    take under 2 s entirely. Write segments ourselves (AVAssetWriter's
    delegate segment output with our own encoders, or an equivalent) so a
    pause, and every N seconds while recording, commits what's been
    recorded; report recoverable seconds from committed output only.
    Engine work for the record/* owner (Astra), with the reviewer's
    55-case kill matrix as the check. After the alpha unless it's small.
    - [x] **G.3b investigation.** Trace the actual raw-buffer recording path,
      probe native segment modes, and measure the existing split/join route
      at 1/2 s. Neither is a qualifying small replacement: manual delegate
      output needs compressed inputs and excludes MOV/ProRes; rolling takes
      add measured work. Repeat the 55 pause kills and 20 plain kills as
      negative controls. Evidence: g3b-segments scratch REPORT.md.
    - [x] **G.3b design and prototype (branch `agent/g3b-crash-safe`,
      not merged during the alpha week).** `docs/crash-safe-takes.md`:
      options with evidence and the recommendation (our own VideoToolbox
      and AAC encoders feeding a fragmented MOV/MP4 we write ourselves,
      finished in place into the plain movie AVAssetWriter produces, with
      recovery from the fragments at the next launch). Prototype behind
      `ISOCAST_CRASH_SAFE=1` for every file: first frame on disk once the second arrives,
      a fragment per 500 ms, commit barrier before the pause reply with a
      numeric `recoverable_seconds_at_pause`, `committed_seconds` in live
      status, recovery in `recover_interrupted`. Kill matrix and A/B in
      the lane report.
    - [x] **G.3b implementation and qualification (#3, 0.0.5).** Ryan's
      2026-10-15 decisions: default on, temporary `ISOCAST_CRASH_SAFE=0`
      fallback for one release, 500 ms commits, keep every frame of each
      track rather than trimming A/V, automatic next-launch repair, and
      retain `scripts/kill-matrix.py`. Integrate the #41 first-picture gate
      and #68 shared encoder placement; qualify kills, #60/#68 loads, CPU,
      timeline/Create/WebM, e2e and workspace gates before Tier-1 review.
      Qualified on M4/M2; coordinator Tier-1 review/re-review and merge remain.
- [x] **G.4 Projectors and multiview (#19).** Fullscreen or windowed preview,
  program, scene or source on any display; a multiview grid.
  - [x] G.4 follow-ups (#95): a remembered full-screen projector whose
    display is gone never covers another one; F/double-click full screen
    names its display; scene feeds follow the fastest current viewer and
    back off a failing draw thread. Multiview CPU remains.
  - [x] G.4 tally colours (#95, Ryan): red program, green preview
    everywhere (Studio Mode tags, program ring, scene badges, editor PGM).
- [x] **G.5 Custom hotkeys (engine side).** Bindable action catalogue,
  per-profile bindings, CLI/MCP, and global press/release registration where
  macOS allows. Timeline and pause actions are wired; timeline-clear remains
  reserved. Native Settings > Hotkeys and
  focused-key/Studio Mode presentation integration follow the app redesign.
  - [x] **G.5 review-quit.** Route AppKit quit events through graceful
    recording finalization, like signals and API shutdown; verify t_quit.py.
  - [x] **G.5 review-hardening.** Lenient/lossless hotkey loading with
    downgrade-safe profile storage, preserve bindings on setup replacement,
    protect typing/system shortcuts, bound diagnostics and complete CLI input.
  - [x] **G.5 app.** Settings > Hotkeys in the Slint app (groups, click to
    record, clear, the API's errors and warnings per row, `hotkeys_set`
    patch + revision) and the focused-key bridge (`hotkeys_input` on one
    connection, releases on blur).
  - [x] **G.5 follow-up 2.** Protect modified typing/common app shortcuts,
    archive orphaned sidecars before reusing profile ids, keep reads pure,
    and wire timeline hotkeys to the merged timeline/retake/export methods.
- [ ] **G.6 Small parity items.** Item flip (horizontal/vertical), stinger
  audio, media source play/pause/seek/restart on the API, floating docks
  in the native app.
- [ ] **G.7 Later items from section 9, when Ryan asks:** a second
  (vertical) canvas, SRT/WHIP streaming, NDI, Syphon, Audio Unit
  plugins.
  - [ ] **#86 vertical canvas.** A second, upright canvas with its own
    scene arrangements, recording and stream.
    - [x] **#86 model.** `Setup.vertical`, `Scene.vertical`, `CanvasId`,
      `VERTICAL_PROGRAM`, `Setup::for_canvas`, validation and the
      collection/profile split (protocol only; engine, daemon and UI
      separate).
    - [x] **#86 engine qualification.** Shared capture, two compositors,
      protected program files, independent stream groups and capacity/tune;
      real files/sync qualified after main's hardening; base-M2 full live
      workload needs ProRes programs to protect both streams (HEVC overload
      is reported, not promised loss-free).
    - [x] **#86 clients and app qualification.** Canvas-aware API/CLI/MCP,
      hotkeys, Studio/editing, two real layout alternatives and regression
      snapshots; full gates and strict e2e. Main-only decoded pixels and
      normalized manifests match; native byte streams vary even on main.
    - [ ] **#86 Tier-1 review/re-review and coordinator commits.**
    - [x] **#96 vertical in the app.** Settings › Stream for the vertical
      canvas's destinations and keys; Start Streaming starts both, per-canvas
      status bar; vertical projectors (program, preview, scene) and the
      multiview's vertical tile on the method table; Follow Main Scenes held
      (and explained) while recording; a contained vertical failure says so
      and offers Restart Engine.
    - [x] **#96 vertical failure follow-ups.** A vertical compositor
      failure ends only the vertical streams, with the reason; vertical
      starts are refused while the canvas is stopped; turning the canvas
      off and on clears the stop; no "add the key" toast when every
      vertical destination is turned off.
    - [x] **#96 / 0.0.7 remaining canvas outputs.** Main compositor failure
      leaves healthy vertical streams alone; atomic poison stop reason;
      quiet repeated vertical Stop and accurate recovery text; independent
      vertical replay (API/CLI/MCP/hotkeys), vertical virtual camera target;
      failed-file drops include the missing tail; approved `vertical` e2e.
      AWS M2 qualification complete; coordinator Tier-1 review remains.
    - [ ] **#96 live scene linking.** Keep `follow_main` fixed during a take
      until main/independent scene histories can be atomically spliced across
      transitions, pauses and split boundaries (the permitted 0.0.7 deferral).
- [x] **G.8 WebM and MKV media (#61).** Media sources and stingers play
  Matroska/WebM (sniffed by content): VP9 with alpha (OBS's stinger
  format), AV1 where the Mac decodes it, H.264/HEVC in MKV, Opus and AAC
  sound; clear errors for VP8, Vorbis and the rest.
- [x] **G.9 obs-websocket v5 compatibility (#71).** An obs-websocket v5
  server (crate isocast-obsws) on the method table, so Stream Deck/Touch
  Portal OBS actions, Streamer.bot, bots and scripts drive Isocast: auth,
  requests, batches, events, JSON and MessagePack; 77 requests mapped, the
  rest answer 204. Off by default; `isocast websocket status|enable|disable`.
- [x] **G.10 VP8, VP8 alpha and Vorbis in WebM/MKV (#64).** Older OBS
  stingers (VP8 with alpha) play without converting: libvpx's VP8 decoder,
  vendored in crates/isocast-vp8 and built statically; Vorbis sound
  through lewton.
- [x] **G.10 System and app audio through Core Audio process taps (#33).**
  `system_audio` and `application_audio` capture through a process tap
  and a private aggregate device on macOS 14.2+ (ScreenCaptureKit before
  14.2, while System Audio Recording isn't allowed, or forced with
  `ISOCAST_AUDIO_CAPTURE=screencapturekit`); `system_audio.exclude`
  leaves apps out; the `system_audio` permission; app pickers in the app.
- [x] **G.11 Advanced masks (#115).** `shape_mask` (rectangle with a
  radius per corner, circle, ellipse, regular polygon, star, heart;
  feather, invert, position, size, rotation), `gradient_mask` (linear or
  radial, up to 8 stops, feather) and `source_mask` (any video source or
  scene as a live mask by luminance, alpha or a channel; threshold,
  softness, fit, scale, position; frame-synchronised in the compositor,
  GPU only; the mask source runs only while what it masks does). Masks
  stack with `combine` (intersect, add, subtract) and animate with stops
  of their own. Setup schema/CLI/MCP, obs-websocket filter requests, OBS
  import of Advanced Masks filters, and handles on the Filters picture.
- [x] **G.12 Per-destination canvases (#84).** Canvases are first-class
  (`CanvasId` by id): `[vertical]` plus `[[canvases]]` (id, name, size,
  fps, follow_main or own scenes, `scenes[].canvases.<id>` arrangements,
  optional recording, own `stream` destinations with per-destination
  bitrate/size/rate control/keyframes/audio; equal formats share one hub).
  Engine/daemon generalise the vertical canvas to N outputs; capacity and
  Optimize count every canvas. `canvas_add|set|remove` on API/CLI/MCP,
  obs-websocket GetCanvasList and Aitum Vertical Canvas vendor requests,
  OBS import of Aitum Multistream and Vertical Canvas, Settings › Video ›
  Canvases and Stream › Streams of every canvas.
  - [ ] Tier-1 review and coordinator commit.
  - [x] **G.12a Extra canvases in the app (#120, #96, #95).** The canvas
    monitor shows the chosen extra canvas (its ⌄ menu switches canvases),
    Edit <name> Canvas edits any canvas's arrangements, projectors and
    multiview tiles for every canvas (`projector_open` `canvas: ID`,
    multiview `canvases`), replay per canvas from the monitor's menu,
    virtual camera `target = "canvas"`.
- [x] **G.10 Multistream (#73).** `[stream]` is the main destination,
  `[[stream.destinations]]` more (service, server, enabled, own key, own
  bitrate/size); one Stream each, matching formats share one encoder, a
  different one only if #66's estimate allows. API/CLI/MCP
  (`stream start --only`, `stream destination add|remove|set`),
  Settings › Stream as a destination list, per-destination LIVE details.

### T. Themes (Ryan, 2026-10-13)

**Parked until after the alpha (Ryan, 2026-10-13): "we'll probably want
switchable themes at some point, but let's just start with console for
right now."** Console (N.5) is the app's look; nothing below starts until
he asks, and the open questions in docs/notes.md wait with it.

Ryan likes four of the Paper directions (Workbench, Console, Daylight,
Blender) for their signature pieces, not their colours, and wants to swap
between them, pieces included; a switch may restart the app. Design:
[docs/themes.md](docs/themes.md) (revised after a taste and a technical
critique). A theme is tokens, fonts, one compiled piece per slot (picked
at launch with `if` on a `Look` global; dispatchers generated from the
slot registry) and layout tokens for the shared containers. Console
(N.5) is the default; plugin themes inherit from a base, the built-ins
set everything. Switching relaunches the app without taking focus;
recordings and streams carry on in isocastd. Theme plugins are data
only. A built-in theme is a **preview** until every signature piece in
Ryan's pitch draws (docs/themes.md 4.2); only then is it **ready** (in
the picker, "Ready for Ryan"). After A (A lists "other themes" as
waiting). T.3a waits for Ryan's OK to commit the board-check scripts
(notes, Questions for Ryan).

- [ ] **T.1 The setting and the theme kind** (needs N.5).
  `isocast_protocol::theme` (TOKENS with ranges, SLOTS with actions and
  extras, `resolve()` enforcing every limit), `[[themes]]` in plugin
  manifests with field-path and file:line checks (file types and sizes,
  fonts parsed, image headers), `app.toml` beside the socket,
  `theme_list`/`theme_get`/`theme_set` (restart only on a change,
  `--reload`, `--preview`)/`theme_report`/`plugin_check`,
  `theme_changed`/`theme_applied`, `isocast theme list|show|set|check`,
  MCP `list_themes`/`get_theme`/`set_theme`/`check_plugin`; a theme-only
  plugin installs without `--allow`.
- [ ] **T.2 Tokens, fonts, launch and relaunch.** The `Theme` global
  generated from TOKENS, fonts registered before the window, `--theme`,
  `--safe`, the launch marker, native light/dark appearance, canvas
  handle tokens, the window frame kept in prefs, relaunch (exec) on
  `theme_changed` after draining queued edits, without activating if
  the app was in the background, loop guard in the environment;
  Settings › General › Appearance and View › Theme. Console stays
  pixel-identical; a recording and stream through 20 relaunches lose 0
  frames; a relaunch leaves the frontmost app frontmost.
- [ ] **T.3 Slots** (each pixel- and `--a11y`-identical):
  - [ ] T.3a Generated dispatchers, behaviour bases, `Look` enums, theme
    features; the Console baseline (A/A, idle, metering); live
    scrubbing on the Transitions Duration (the one behaviour change).
  - [ ] T.3b Dock actions as `[DockAction]` models, changed row by row.
  - [ ] T.3c Layout and canvas tokens in the containers
    (`sidebar-dialog`, FlexboxLayout footer, `visible: false` folding).
  - [ ] T.3d Frame slots, `lcd` and `template-field` as slots.
- [ ] **T.4 Workbench:** T.4a controls, T.4b docks and mixer, T.4c
  frame and bars, T.4d dialogs and forms (each a preview, checked board
  by board); **ready** after T.7b, T.7c, T.7d and T.7f's focused dock.
- [ ] **T.5 Daylight:** the same four areas, plus the light appearance
  and the picture mat; **ready** after T.7a, T.7c, T.7d and T.7e.
- [ ] **T.6 Blender:** the same four areas, plus the top bar from the
  menu bar's one model, the outliner, panels and the tabbed Properties
  editor (in-window); **ready** after T.7a, T.7b, T.7d and T.7f's action
  table (so after G.5).
- [ ] **T.7 Data the pieces need**, each on the method table first
  (T.7a and T.7d first: Console needs them too):
  - [ ] T.7a Free space, volume and time left while idle; GB per hour.
  - [ ] T.7b Per-channel and program levels; EBU R128 loudness.
  - [ ] T.7c Media position, duration and codec (with G.6).
  - [ ] T.7d Form sections, per-field defaults and reset, live-locked
    fields.
  - [ ] T.7e ISO file-name template and a sample name.
  - [ ] T.7f App-side: canvas fps, quick-transition durations, item
    counts, Settings groups, preview zoom and pan, folded panels, focused
    dock, Welcome dismiss and links, menu filter, the action table (from
    G.5).
- [ ] **T.8 Later, when Ryan asks:** tabbed docks (Workbench's
  icon-over-label tabs, Daylight's tool tabs), workspaces (a layout
  feature, not a theme: Blender's "Beyond OBS" and Console's
  Live/Build/Create boards), real windows for Blender's Preferences and
  Properties, code pieces in theme plugins (once Slint stabilises
  ComponentContainer), SVG icon overrides, more-contrast variants.

### 5. Scenes as code

Ryan's scene language (see `docs/scene-language.md`): scenes authored in TSX
or plain setup data, laid out and animated with CSS ideas on the session
clock, and fed by live data and plugins. Every capability is on the method
table and in `isocast setup schema`.

- [x] **5.0 Compiled scenes.** The scene thread resolves the setup's
  scenes into compiled draw lists; the compositor draws them from one
  storage buffer of draw records with cached bind groups, sends the last
  frame again when nothing changed, and reports its timings in `engine`
  (docs/scenes-as-code.md, 5.0). Same pictures, no setup change.
- [x] **5.1 Layout.** Flexbox and grid with `taffy`; `px`, `%`, `inset`,
  absolute positioning, aspect ratio, object-fit. `layout` on items and
  scenes, `rect` optional, `overflow` clipping, `get --resolved` and the
  layout generation frames carry (docs/scenes-as-code.md, 5.1).
- [ ] **5.2 Keyframes.** `@keyframes`-style animations and transitions with
  CSS easing, evaluated at each canvas tick; enter and exit animations.
  - [x] **5.2a Keyframe animations.** `[[keyframes]]`, item `animations`
    (on_air, shown and clock clocks), `cubic_bezier` and `steps` easings,
    the per-tick evaluation pass (docs/scenes-as-code.md, 5.2a).
  - [ ] **5.2b Animated changes, enter and exit, scene transitions.**
    After 5.1 and 5.2a (docs/scenes-as-code.md, 5.2b).
- [ ] **5.3 Shader effects and transitions.** Custom WGSL effects on items
  and custom WGSL scene transitions.
- [ ] **5.4 TSX authoring.** `isocast scene build scenes.tsx` compiles
  components and props to setup data in an embedded JS runtime; `plan` and
  `apply` take it directly.
- [ ] **5.5 Live data.** A data store in the engine (`isocast data set`,
  MCP, events); components re-render when their data changes, merged by id
  like React, never per frame.
  - [x] **5.5a Data store, content items, bound text.** `data_*` methods,
    CLI `isocast data`, MCP get_data/set_data, `data_changed`, kept keys
    per scene collection; `content` (text, image) on items; text `data =
    "key"` (docs/scenes-as-code.md, 5.5a).
  - [ ] **5.5b Live components.** After 5.4a and 5.2b
    (docs/scenes-as-code.md, 5.5b).
- [ ] **5.6 External APIs.** Scene code can fetch and open WebSockets under
  a permission list; a Twitch chat overlay as the worked example
  (anonymous read-only chat).
- [ ] **5.7 Plugin nodes.** Plugins declare node types (props schema plus a
  shader, TSX component, web page or external process that renders them),
  listed in the schema so agents can use them.

### 6. The app (flat design)

Moved into V1 (V1.4–V1.7) on 2026-10-11.

### N. Native app in Slint (Ryan, 2026-10-12)

Ryan: "let's just go ahead and redesign the entire thing in slint". The
app is rebuilt as a native Slint (1.18, Skia renderer on wgpu 30) app, one
codebase for macOS now and Windows later, replacing the AppKit + WKWebView
app once it reaches parity. Same OBS layout and flat look; the engine,
CLI and MCP don't change (the app stays a client of isocastd). The shipped
app uses Slint's royalty-free licence: an "About Slint" notice in
Help → About (Ryan approved); Isocast's own code stays MIT OR Apache-2.0.

- [x] **N.1 Spike (de-risk).** A Slint window with OBS-style docks and
  splitters, a dropdown, a text field (IME), the native menu bar and the
  About notice; the daemon's frames shown zero-copy (shared GPU surfaces
  across processes, RGBA8 at the monitor's size) with a draggable
  selection box drawn in the same frame; an offscreen screenshot test
  (approved by Ryan). Pass: ≤5% of a core at 60 fps in a real window,
  box and picture in the same frame, no black frames in 10 minutes, the
  Scenes list exposed to screen readers.
- [x] **N.2 Parity.** Every view of today's app rebuilt (docks, dialogs,
  settings, dropdowns, first run, studio mode, mixer, status bar), in the
  design language of docs/design.md. Carry over from the spike: the
  Controls dock must fit at the minimum window width (cut off below ~1170
  px today), drags must keep up (pipeline live applies; 32 picture moves/s
  on an M2 vs the web app's 50), the app menu names Isocast, Instrument
  Sans bundled, colour management of the native layer (Rec. 709 tag), the
  portable `--preview image` path's cost (~21% on an M2). Split into
  areas built side by side (src/areas/mod.rs):
  - [x] **N.2.0 Foundation.** Every design.md token, the widget kit, the
    window split into five areas over a shared session, docks that fit
    the 900px minimum, a bundle that names the app Isocast
    (scripts/ui-dev.sh), snapshot states per area.
  - [x] **N.2.A Scenes and sources.** Scenes and Sources docks, source
    toolbar, adding sources, Properties, Filters, Edit Transform, Rename,
    their menus. Open: reordering scenes, items and filters isn't applied
    by the daemon (its plan ignores order: "No changes"; the web app has
    it too).
  - [x] **N.2.B Audio.** Audio Mixer, Advanced Audio Properties, meters
    (smoothing, peak hold, cost), the mixer's menus.
  - [x] **N.2.C Controls and status.** Controls, Scene Transitions,
    Studio Mode, status bar, recording and streaming states and details,
    toasts (dropped frames).
  - [x] **N.2.D Settings and first run.** Settings (every tab), first
    run, Welcome, connecting and failed states.
  - [x] **N.2.E The canvas.** Selection, move/resize/crop/snapping,
    locked items, pipelined live applies during drags, Studio Mode's two
    pictures, Rec. 709 on the native layer, `--preview image` cost.
  - [x] **N.2.F Integration.** The five areas merged on slint/n2, their
    duplicates made one (settings form, open panel, device choices,
    canvas prefs), their seams wired (row hover → canvas outline, the
    preview's item menu, Start Streaming → Settings), checked as a whole.
    Open: reordering isn't applied by the daemon (N.2.A); the checks on
    an M4 and a P3 display wait for exonbook's screen (Waiting on Ryan);
    with meters moving the window costs 13–14% of a core on a base M2
    (whole-window redraws); run outside its bundle the app menu says
    isocast-ui (scripts/ui-dev.sh's bundle says Isocast).
  - [x] **N.2.G Critique fixes.** The parity and design critics' findings
    on the integrated app: transform numbers sent as integers, lists
    changed row by row (renames and double-clicks survive the status
    poll), dialog fields keep the focus, context menus open on release
    (no stuck pointer grab), dialogs scroll in short windows, Escape
    commits nothing, ⇧⌘A/⇧⌘N/Return as in the web app, details close
    when recording or streaming stops, `--a11y` lists dialogs and menus,
    and the type spaced and leaded as the web app's (tracking and
    leading tokens). Left on purpose: notes.md Decisions, 2026-10-13.
- [x] **N.3 Switch.** The Slint app becomes Isocast.app; the web app is
  retired (kept out of the default build until after the alpha week).
- [ ] **N.4 Windows (parked by Ryan, 2026-10-13: later).** Shared D3D12 textures for the preview, packaging
  (with the engine's Windows platform crate, separate work).
- [x] **N.5 Console design (Ryan, 2026-10-13).** Ryan picked the Console
  direction (Paper file "C. Console", Final Cut Pro x Logic Pro x Tower:
  01M4SHX06Z4XBZDXMWN2YEB9EA, its Screens boards Main window, Studio Mode,
  Properties, Settings and Welcome, and its Components page) and wants
  the app redesigned in it now, keeping today's OBS layout (no workspaces
  or timeline). Themes become swappable later (Console first; Daylight,
  Workbench, Blender…), so every Console value is in the Theme global
  under Paper's token names and every signature piece is a kit component
  the areas use, never drawn inline. Exact values only from Paper.
  - [x] **N.5.0 Foundation** (branch ui/console). The tokens, the fonts
    (Mona Sans, Hubot Sans, Martian Mono with their OFL licences), the
    kit restyled to the Components board, the signature components (LCD,
    capsule clusters, transport buttons, sidebar list, sheets, meters,
    list rows, number fields, tabs, inspector bands, grouped rows, quick
    list, popovers, toasts, tooltips), and the kit gallery drawn as the
    Components board (`--state kit`, 1440×3060) with every state.
  - [x] **N.5.1 Areas.** Each area rebuilt from the Screens boards with
    the kit: the control bar (selected source, LCD, transport, master
    meter, capsules, pane toggles) and the status bar; the docks and
    their rows and toolbars; the mixer strips (green faders, M and
    headphones toggles); the sheets (Settings with its full-height
    sidebar, grouped rows and summary footer; Properties; Filters; the
    rest); Welcome; Studio Mode's column and viewer lines.
    - [x] **N.5.1a Main window and Studio Mode** (branch ui/console-main):
      the viewer line and preview frame, the control bar (selected
      source, LCD, transport, master level, Properties/Filters capsule,
      pane toggles), the Scenes, Sources, Audio Mixer, Scene Transitions
      and Controls docks, the status bar with REC/LIVE details and free
      space, toasts, Studio Mode's viewer lines and column, as the Main
      window and Studio Mode boards draw them.
    - [x] **N.5.1b Sheets, Settings and Welcome** (branch
      ui/console-dialogs). Every dialog a Console sheet (glyph, Hubot
      title, one line; grouped rows; Defaults left, the one yellow OK):
      Properties with its preview well, Filters and Settings with
      full-height sidebars (Settings' macOS list, summary footer),
      Create/Select Source, Rename, Edit Transform, Transition
      Properties, Advanced Audio, the profile and collection sheets,
      About, the first run and Welcome. Open: Settings' summary has no
      "~4.3 GB/h" (the engine exposes no estimate) and its disk badge
      waits on Status.recording_free_bytes (ui/console-main); the
      Properties well's time waits on a media position from the engine.
    - [x] **N.5.1c Integration** (branch ui/console-integrate, from
      ui/console with ui/console-main and ui/console-dialogs merged).
      Conflicts resolved (both branches' `broadcast`/`sliders` icons kept,
      the sheets' as `sheet-*`; the dropdown cap's SvgPath with
      `on-control`); Settings' disk badge wired to
      Status.recording_free_bytes; one `source_format`. Open: the master
      level keeps the window redrawing while sound plays, even with the
      mixer hidden (notes, Decisions 2026-10-13, Console integration).
  - [x] **N.5.2 Critique fixes** (branch ui/console-integrate). The
    fidelity and function critiques of 2f22005, checked against Paper
    and fixed (notes, Decisions 2026-10-13, Console critique fixes):
    - [x] Text weight: each Paper weight drawn with the next lighter
      official cut (Theme `weight-*`, a recorded renderer compensation).
    - [x] No setup and empty states as the Welcome board draws them
      (EmptyState with Paper's glyphs, IDLE, dimmed zero LCD, faded
      bands); menus' lane and red Remove; green only while streaming;
      the LCD's templates; the destructive Remove button; the muted
      strip; meter and streaming-glyph tokens; the 12-unit plus.
    - [x] The program file's GB/h (shared estimate, Status for the CLI
      and MCP), free space before a setup, stereo master level.
    - [x] Keyboard and screen readers: strip ⋯ and ⇧F10, quick
      transitions, Segmented from Custom, reset glyphs, the selected
      source's file, the path field, Welcome's names, LCD descriptions.
    - [x] Small windows: ISO lanes, fader minimum, band fields, Marker.
    - [x] Cost: cached chrome layers; the master level a native layer
      under the window (no redraw per step).
    Open: the Properties well's time (needs a media position from the
    engine); the contrast of two Paper values (Questions for Ryan).

### 7. Plugins

**Principle (Ryan, 2026-10-14):** features made for one person's workflow
are plugins, not core. A plugin uses what the CLI and MCP use (the one
method table and its events) plus files it is allowed to read; the core
adds a plugin capability only when a real plugin needs it, and nothing a
plugin does can cost a recording. In-pipeline tiers (shader filters,
WebAssembly audio and sources) exist only because a separate process
can't run inside the frame or audio loop. Not planned until a plugin needs
them: interceptor hooks, plugin UI contribution points, plugin settings
pages, background scheduling.

Shipped (v1): 7.1, 7.2, 7.4a, 7.5, 7.6. Next, in order:

- [x] **7.7 Plugin events and file access** (one feature, general, not
  built around one plugin). (a) **Every state change emits an event with
  full context**, so a plugin never has to poll or guess: recording and
  take started/paused/resumed/finished/failed, split, retake, marker,
  scene and preview switches, stream started/stopped/reconnecting, timeline
  exported, project created/renamed, device added/removed, setup applied,
  each carrying ids and, where files exist, the folder and the path of
  every file (program, each ISO, manifest, export). (b) **A folder per
  recording session** (`<recordings>/<date time> Untitled/` with every
  take's files, manifests and exports), renamed only after its files are
  finished, with `rename_project` and `list_projects` on the method table,
  so plugins and people have one stable place per recording. (c) **Scoped
  file grants** in the manifest, approved at install: `recordings =
  "read"` (read recording files only) and `recordings = "write"` (also
  add files to a session's folder, e.g. exports or transcripts; never
  modify or delete existing media). Done when the event catalogue is
  documented and covered by the e2e suite, a sandboxed plugin can read a
  finished take only with the grant and write only into the session folder
  with the write grant, and existing flat recordings are untouched.
- [ ] **7.8 Auto Name example plugin** (later; the first plugin built on
  7.7, not its design driver) (`examples/plugins/auto-name`): on
  `recording.finished`, transcribe the take's mic audio on-device (Apple
  Speech), name it with Apple's on-device model from the transcript, the
  scene names and markers plus the user's past renames (kept in its own
  data folder), and call `rename_project`. No network. Done when a real
  take is renamed within a few seconds of Stop and a rename by hand is
  used as an example next time.
- [ ] **7.3** (below) and **7.4b** stay planned but wait for a plugin that
  needs them; 5.7 waits for scenes as code.

- [x] **7.1 Plugin model.** Manifest format and three tiers, from safest to
  most powerful: external processes that use the control API (any
  language), WGSL shader filters, and WebAssembly components (wasmtime)
  for audio filters and custom sources. (Design slice "7.1 Plugin registry
  and process plugins": the manifest with every tier's sections, the
  registry, grants and the process tier; the WGSL and wasm runtimes are
  7.3 and 7.4.)
- [x] **7.2** External-process plugins, with a Stream Deck-style example.
- [ ] **7.3** Third-party shader filters (WGSL), next to the built-in ones
  from 4.9. Also: the `schema` method (left from 7.1), and plugin kinds in
  setups for filters, transitions and content (audio filters' came with
  7.4a, as did `plugin remove --force`).
- [ ] **7.4** Third-party WebAssembly audio filters and custom sources,
  next to the built-in ones from phase 4.
  - [x] **7.4a WebAssembly audio filters** (design slice 7.4a): wasmtime
    with the component model, the WIT package (`wit/plugin.wit`),
    precompiling and checking at `plugin add`, filters in the mixer with
    work and memory limits, bypass and fresh instances, plugin kinds for
    audio filters in setups, `examples/plugins/gain` (a gain and a
    sidechain ducker). Done when the wasm gain equals the built-in gain,
    the ducker matches the compressor within 0.5 dB, a trap, an endless
    loop and a memory overrun each bypass the filter with one warning and
    turn it off after three while the files keep every sample, per-block
    cost is measured (gain ≤ 0.05 ms), and the flagship A/B with a wasm
    filter on the tone shows no regression (isocast-m2-a).
  - [ ] **7.4b WebAssembly sources** (design slice 7.4b).
- [x] **7.5** `isocast plugin add`, `list` and `remove`.
- [x] **7.6 Events by kind.** `watch` takes `kinds` and `except` (the
  API, `isocast watch --kinds/--except`, and MCP's event thread, which
  asks for all but levels), so a client that shows no audio levels isn't
  woken 20 times a second; the deck asks only for what its page shows.
  Done when the idle deck receives no events and the 4K flagship with the
  deck installed drops no more than main (isocast-m2-a, 5+5 runs).

### 8. Open-source release prep (parked by Ryan, 2026-10-13: later)

- [x] **8.1** CI on GitHub Actions (macOS on Apple silicon) with formatting,
  lint and test gates.
  Write the workflow; don't create the GitHub repo or push.
- [ ] **8.2** Release build script for signed, notarized Apple silicon
  (arm64) builds and a Homebrew formula.
  - [x] **8.2a Production packaging.** `scripts/release.sh`: self-contained
    app/CLI/engine, Developer ID and hardened runtime, app and DMG separately
    notarized/stapled, checksums, dry-run/ad-hoc modes and draft-only upload
    of existing verified artifacts. Version from the workspace; one UI target.
  - [x] **8.2b First-install runtime and draft.** Rebuilt from main f6570b7:
    app and DMG notarized/stapled, strict signatures and Gatekeeper accepted.
    The mounted native app/engine/CLI and 4 s program+ISO recording pass on
    isocast-m2-b. One unpublished v0.0.1 Alpha draft with verified DMG and
    checksum assets; matching-draft reuse verified. URL/SHA-256 in the Draft
    section of `/Volumes/IsocastBuild/release/scratch/REPORT.md`.
  - [ ] **8.2c Homebrew distribution.** After a release is ready to publish.
  - [x] **8.2d Morning-ready native release.** Main 85e2210 merged; Slint
    default with its plist, bundled CLI/engine, consistent build metadata,
    Developer ID-signed app/DMG without notarization. Native window, PATH
    CLI/restart and bundled-engine program/ISO take pass on isocast-m2-b.
    Resume-only notarization and idempotent draft stages are ready.
  - [x] **8.2e GitHub release workflow.** Tag push/manual existing version,
    hosted macos-15 arm64, protected release environment, SHA-pinned actions,
    temporary keychain and API-key notarization, mounted bundled-runtime
    gate and one refreshed draft with generated notes. Actionlint and local
    dry/mock checks pass; real 720p30 bundle qualification passes on m2-b.
    One-time environment/secrets setup and first hosted run await Ryan;
    see docs/releasing.md and REPORT.md's Workflow section.
  - [x] **8.2g Release readiness (GitHub #11).** Workflow, scripts and docs
    checked against the current macos-15 arm64 image and today's hosted
    e2e evidence; a preflight job ends runs for published versions green
    without approval or secrets; Ryan's copy-paste checklist (secrets,
    first v0.0.2 run, app-specific password rotation) is docs/releasing.md
    steps 2–6. The six secrets and the first hosted run remain Ryan's.
  - [x] **8.2f Designed installer DMG** (Ryan, 2026-10-14: "ship it"). The
    Paper "G · Simple" background (black, amber arrow, cropped wordmark) via
    pinned dmgbuild in release.sh and release-finalize.py; no Install.txt.
  - [x] **8.2h Auto-update (GitHub #40).** Sparkle 2.10.0 (pinned, hash
    verified, not vendored): Isocast › Check for Updates…, daily check with
    a Settings › General toggle, EdDSA-signed appcast beside the notarized
    DMG in the draft; never installs during a recording/stream/export.
    Proved end to end on isocast-m2-a with a throwaway key; the real key is
    Ryan's (docs/releasing.md step 5).
  - [x] **8.2i Permissions onboarding for 0.0.3 (GitHub #51).** A
    permissions step before Welcome (and from Settings and a source's
    warning) with live statuses and one action each, the floating drag
    helper beside System Settings, Relaunch for a grant that waits for the
    engine, and plain words for a refusal without a prompt.
  - [x] **8.2j Permissions onboarding follow-ups (GitHub #58).** The helper
    stays beside System Settings' main window; Refused without asking
    offers Open Settings and isn't flagged outside a screen session; no
    restart for a bare isocastd; dead first-run plumbing gone; a disabled
    primary footer button looks disabled; Review… keeps Settings' edits;
    dev builds say "Isocast".
  - [x] **8.2k Follow-up sweep for 0.0.7 (GitHub #45, #55, #58, #65).**
    Restart and the updater refuse during `tune`; developer traces record
    a file's start without a lock; bug reports bring more than a tiny
    newest developer segment; the Pacing row and the auto-off switch;
    Settings isn't left disabled by an Apply behind Review…; the stream
    connect's Local Network hint and deadline.
  - [x] **8.2l Real release notes in the update window (GitHub #106).**
    Notes are `docs/releases/X.Y.Z.md`, committed before tagging: preflight
    and release.sh refuse a version without them; the appcast embeds them
    (markdown) and the GitHub body is them plus the Full Changelog link (no
    more generated notes). "Refresh appcast notes" (workflow_dispatch,
    release environment) re-signs only a published release's appcast.xml
    with its notes, keeping the DMG and its EdDSA signature; 0.0.6's run
    is Ryan's to approve.
- [x] **8.3** Docs: getting started, setup reference, MCP tools, plugins.
  (With the user-facing README and its screenshots, licences and
  THIRD_PARTY_NOTICES, and the .github templates.)
- [x] **8.4** Visual CI on pull requests (Ryan, 2026-10-13): build base and
  head, render the standard views at three sizes, diff them, comment
  before/after/diff, a restricted OpenCode judge per changed view, and the
  commit status `isocast/visual-check`. First real run: see "Waiting on
  Ryan" in docs/notes.md.

### W. Windows port (Ryan, 2026-10-17; GitHub #105)

This supersedes Windows being parked under N.4 and section 9. Use the native
exon-desk loop (exclusively assigned to this lane) and GitHub Actions as the
clean-machine milestone check. Stop for delegator review/merge after
each milestone. Shared recording changes are Tier 1 and must preserve
macOS behavior; Windows-only code is Tier 2. Merge current main between
slices, especially alongside the GPU, vertical-canvas and editor lanes.

- [x] **W.0 Portable CI.** `windows.yml` on `ci/**`, Windows and Ubuntu,
  cached Cargo build, clippy and existing tests, superseded runs cancelled.
  Start with protocol, diagnostics and obs-websocket (the engine still has
  Unix-only manifest I/O; client/daemon use Unix sockets). `windows-ci.sh`
  exports committed HEAD, uses the publishing secret/attribution scan,
  commits into its own public checkout, lease-force-pushes **ci/windows
  only**, waits for the exact snapshot's run and saves results/failed logs.
  Done when both real hosted jobs pass and their URLs are in the report;
  no main/tag/release writes. Add main to the workflow after qualification.
- [x] **W.1 Platform seams (#28).** Mechanical daemon generic over
  `Platform`; plugin, hotkey and diagnostics hooks through the trait;
  `isocast record` drives an isolated daemon; UI platform seam and
  target-specific wgpu backends. Extend portable CI to the engine once
  its OS I/O is behind a safe seam (Windows durability follows in W.2).
  Verify unchanged macOS API, recording, errors, timing and lifecycle;
  no no-op safety fallbacks merely to compile on Windows.
  - [x] **W.1a Engine build boundary.** Target-specific Metal/DX12 features;
    non-Unix exclusive manifest creation without changing the Unix path;
    build engine/test binaries on Windows without running them. Preserve
    all durability errors (Windows recording/flush qualification is W.2).
  - [x] **W.1b Daemon platform selection.** One concrete platform alias and
    a fail-closed stub behind `Platform`; replace per-call macOS shims
    mechanically, preserving dispatch and lifecycle on Mac.
  - [x] **W.1c OS services and IPC compile seam.** Platform hooks for
    diagnostics, hotkeys, plugins and process lifecycle. Real same-user
    named pipes and detached daemon/CLI on exon-desk (brought forward from
    W.6); macOS retains native Unix socket types and plugin sandbox policy.
    Windows process plugins/shared surfaces remain explicitly unsupported.
    Native build/clippy/tests, CLI/MCP and Mac gates/e2e/bench verified;
    clean-machine Actions run 39060641784 passes portable Windows/Ubuntu
    and the Windows daemon/CLI build.
  - [x] **W.1d CLI as a client.** Isolated-daemon `record`, no CLI native
    capture/recording/capacity calls. Parent-death finalization, real child
    reaping and old-daemon handshake; same API/MCP. Mac lifecycle/edge probes,
    strict e2e, gates and measured foreground A/B pass; Windows control path
    and owned-child lifetime pass (recording still refused by the stub).
    Actions 39065225187 passes portable jobs and the native build gate. Extra process
    costs about 44 ms warm startup and 45 MiB RSS on M2-a; no memory/timing
    equivalence claim. W.6 must port Windows XML NLE file URLs (OTIO works
    through its existing plain-path seam; XML is explicitly refused).
  - [x] **W.1e UI platform seam.** Mac window/media paths unchanged;
    Windows UI/daemon/CLI build, strict scoped clippy and UI tests pass.
    Slint compiles on an 8 MiB Windows build worker (main stack was 1 MiB).
    Real first-launch UI renders offscreen on RTX 5070/DX12; media, native
    panels, focused key hooks and editor playback remain explicit stubs.
    Pipe names include owner SID; native nonblocking I/O and shared-client
    deferred polling preserve buffered control bytes. Windows UI CI gate
    added; coordinator commit/clean-machine run next, not media qualification.
- [x] **W.2 Cross-platform writer.** Prefer extracting the existing
  crash-safe BMFF muxer without changing its bytes; qualify against the
  macOS writer before choosing it over ffmpeg. Windows file ownership,
  no-link/reparse-point checks, atomic manifests, flush and repair must
  preserve 500 ms fragments, actual commit watermarks, every complete
  track, true timestamp gaps, drop accounting and torn-finalize recovery.
  macOS encoders, admission/pressure and startup order stay unchanged.
  - [x] **W.2a Encoded-packet mux and ownership.** Qualify a separate
    portable BMFF writer accepting NVENC/MF compressed access units and
    AAC (explicit encoder priming, no raw pixels). Reuse our box/index
    algorithms without switching either Mac writer or its selection.
    Windows handle-anchored ownership/locks, 500 ms fragments, exact sample
    accounting, commit watermarks, safe finish and standalone repair.
    Native NVENC H.264/HEVC 8/10-bit input proof; 14 process-kill cases,
    exact paused commitments, torn publication/header and interrupted-repair
    regressions, aliases and rational packet timing pass. Mac production
    writers untouched; new APFS layer repairs Windows fragments identically.
    Standalone library proof; W.2b/W.2c below add launch integration before W.3.
  - [x] **W.2b Launch recovery and manifests.** Integrate Windows recovery
    under the same pinned folder lease as manifest I/O; atomic replacement
    and honest recovered counts/durations. Preserve the Unix paths.
    Real daemon startup repairs isw2 takes, retains historical facts when
    inspection is unavailable, preserves unknown fields and pauses, and
    reports persistence failure without deleting the old manifest. Pinned
    native rename, read-only inspection and bounded manifest enumeration;
    record-start skips historical media rather than rescanning it twice.
  - [x] **W.2c Native kill qualification.** On exon-desk, NVENC H.264/HEVC
    and AAC inputs, MP4/MOV, kill/commit/gap/empty-track/torn-finish/alias
    matrices, full decode and A/V checks, plus next-launch repair/idempotence.
    Distinguish encoded-packet writer proof from W.4/W.5 capture/encoder
    integration; no power-cut guarantee inferred from process kills.
    W.2a matrices plus eight writer kills repaired by actual daemon launch,
    seven daemon-recovery kills, four metadata-publication kills, readonly/
    alias/large-folder checks and byte-idempotent relaunches pass. Native
    MF/NVENC producer integration remains W.4/W.5, not claimed here.
- [ ] **W.3 Platform contract tests.** Shared executable contracts for
  clock/timestamp mapping, bounded frame pools and retention, GPU completion,
  capture teardown, encoder pressure/loss, A/V timing, commit/finalize,
  interrupted recovery and ownership. Every implemented platform must pass;
  extend existing checks first, obtain approval before new test files.
  - [x] **W.3a Shared native contract harness.** Dedicated harness/test files
    approved on2026-10-17. Identical generic Platform checks, one bounded
    worker per case: clock, pools/retention, GPU completion, writer pressure,
    A/V timing, commit/kill/recovery/ownership and capture teardown. Reports
    distinguish passed, failed, unavailable and not requested; a stub/core
    pass is never full native qualification. Verify the verifier rejects
    wrong counts, timing, shortened tracks and missing capabilities.
  - [ ] **W.3b Both native backends qualified.** Mac native suite on a
    reserved test machine; Windows native suite after the capture lane exposes
    a Platform implementation and writer. W.5c3 now exports WGC/DX12 and
    NVENC/MF video encoder pieces, but still selects StubPlatform. Do not mark this done from
    a Windows stub or packet-only writer run.
    Windows still needs its native Platform/FileWriter adapter. The Mac
    early-importer-drop failure was reproduced with CoreVideo alone and fixed
    by keeping each texture's cache alive until texture teardown (2026-10-18).
    Strict GPU contract and 256 BGRA/NV12 cross-thread cycles now pass; native
    M2-b core/media7/7 pass, capture honestly unavailable without permission.
    Independent lifetime was preserved, not weakened to obtain a green run.
- [ ] **W.4 Windows synthetic platform.** Add `isocast-platform-windows`:
  clock, bounded BGRA/YUV frames, importer, synthetic sources and W.2 writer.
  Prefer hardware MF/NVENC when available, software only as last resort for
  hosted recording; fail clearly
  for unsupported devices/codecs. A synthetic program and ISOs produce
  decodable, aligned files on windows-latest, with honest loss counters.
  - [ ] **W.4a Platform/FileWriter adapter (agent/windows).** Integrate the
    capture lane's committed native pieces, not duplicate their implementation.
    Production WindowsPlatform now integrates writable BGRA output, real
    Engine compositor/filtered ISOs, native recording-quality policy, MF AAC
    and WASAPI. Shared native media contracts7/7 pass WITHOUT factory overrides;
    actual Engine+loopback3takes/9files/540pictures fully decode with2s A/V
    tracks andzero countedlosses. NativeGPUdebug/blocked-queue lifetime passes.
    Original WGC GPU-copy stand-in proved1080p60, not full-app performance;
    real kill preservescommittedvideo/AAC andrepairisbyte-idempotent.4Kthree
    outputs runhalfrealtime; twofill8pending/refuseISOtick23. Targetsremainunmet.
    Device/permission/name resolution nowpassesactualown-WGC Engine3takes/
    9A/Vfiles/1080pictures,full-decode/colors/audio; unknownconsentisreported
    truthfully,32read-onlydiscoverycycleskeep151handles. WGC helper lifetime
    nowqualifies below; camera/full-performance/nativeparity gates remain.
    FinalbranchMacgates nowpass
    afterthecoordinator freedbuildspace(233unit+1doctest,strictClippy).
  - [x] **W.4 WGC helper lifetime gate (ready for CI).** ETW attributes retained
    ports to WindowsGraphicsCapture→COM→RPC, released only at process exit.
    WindowsPlatform uses a job-owned sibling helper with shared GPU textures,
    completion fences and acknowledged frame leases, without another pixel
    copy/readback.20reopens keep parentALPC at5; retained frames survive helper
    exit/crash/reopen/resize. BlockedGPU32-frame pressure and parent-crash
    cleanup pass. ActualEngine9A/Vfiles/1080frames and25s4K60program+ISO/AAC
    3000frames pass full decode withzero reported losses. NativeClippy/81unit,
    Mac233unit+1doctest and independent review pass. Uncommitted as requested.
  - [x] **W.4 daemon selection (ready for CI).** Deliberately mergefa061065;
    WindowsPlatform records real screen+CamLink4K+mic through CLI and app,
    program+twoISOs. FinalCLI900frames and actualapp1005frames fully decode
    withzero losses; realkill/restart preserves537video frames plus all
    observedAAC packets, second recovery byte-idempotent. Camera delivery is
    explicit sharedMJPG CPUdecode/upload; actualdefaultmic was silent.
    WGC helper now waits for original presentationPTS beforepublication:
    resolves UI-only future-frame starvation without rebasing/enlargingqueues.
    Sharedpreview/playback remainunsupported. Include isocast-capture.exe
    beside daemon/UI; new work remainsuncommitted forcoordinator.
  - [ ] **W.4/W.5 Stop and performance targets:** Windows Stop now wakes its
    coordinator immediately (measured0.06–0.08ms), retaining10msfallback and
    minimum-take/durablefinish semantics. WholeRPC54–134ms stillNOT<20ms;
    finalinput tail andmuxsync dominate. Headlessdaemon+helperCPU measured
    58.90%onecore at1080program /56.85%at4Kprogram, bothwith4Kscreen+1080camISOs.
    MatchedOBS baselineblockedbyCamLinkresource-busy atbothdefaultNV12and
    exactexistingMJPG1080p60; protectedbridgeunchanged. Nohalf-OBS/lower-memory
    claim fromunmatchedworkloads. Future performancework retainsdata/durability.
  - [x] **W.4 codec boundary gate:** tag/guard encoded codec at the engine
    boundary before wiring HEVC files. Existing live frame-stream/FLV paths
    assume H.264 parameter sets; the Windows live factory stays H.264, while
    the recording factory usesexplicitVideoEncoding andFileWriterchecksactual
    codec/configuration beforeAAC/video muxing. NativeH264/HEVC8/10full-decode
    cases passed; this doesnotclaimtrue10bitsourceprecisionfromBGRA8.
- [ ] **W.5 Capture, audio and encoders.** Windows.Graphics.Capture for
  screens/windows, Media Foundation cameras, WASAPI input/loopback/output,
  NVENC-first, hardware MF (NVENC/QSV/AMF) fallback, software last;
  permissions and reconnects. WGC D3D11 textures stay GPU-resident through
  D3D11/DX12 shared-surface interop with wgpu and DirectX-resource NVENC
  input: no hot-path CPU pixel readback. Async low-latency encode tuned for
  recording quality; per-ISO sessions within detected RTX 5070/driver limits,
  retaining program priority and ISO shedding. Exercise
  everything runners expose; list hardware paths unqualified rather than
  treating synthetic or software-encoder success as hardware evidence.
  From W.5 every milestone reports exon-desk versus OBS on matched scenes
  at 1080p60 and 4K60 with ISOs: launch-to-first-preview, ready-to-record,
  start-record latency, CPU/GPU/memory and dropped frames. Install isolated
  OBS as a permitted test dependency, never use Ryan's OBS profile.
  Standing Windows performance gates (Ryan,2026-10-18), matched full-engine
  program+ISOs at1080p60 and4K60: CPU at most half OBS, memory below OBS at
  every size, faster launch→first preview and launch→ready-to-record,
  stop<20ms and zero drops. Profile with WPR/ETW. Native MF AAC, GPU-resident
  NV12, exclusive-ownership direct NVENC registration where safe, fewer
  per-frame map/unmap/fences and MMCSS belong in W.5, not a later polish pass.
  Report timing/durability boundaries and matched settings; do not meet a
  metric by hiding helper CPU, deferring finalization or violating NVENC's
  mapped-resource ownership contract.
  - [x] **W.5a Native capture substrate (agent/windows-capture).** Commit
    8619c20, Actions39074561618 green. WGC display/HWND capture into bounded,
    fenced D3D11 BGRA textures; QPC stamps, cursor switch, resize, pressure
    counts and joined teardown. Six 1080p60/4K60 runs at 59.997–59.998 fps,
    zero observable pool/rate drops; 32 retained frames force counted drops
    and resume correctly, retained pixels survive resize. Two simultaneous
    captures share a device, window-close fallback verified on Windows 19045.
    ~230 ms device-open-to-first frame; not UI/record readiness. One GPU copy,
    no CPU readback; wgpu import, encoding and Platform selection still pending.
    OBS portable installed, matched record/ISO comparisons not yet possible.
  - [x] **W.5b Zero-copy DX12 input (agent/windows-capture).** Commit267a169,
    Actions39080080156 green. Same-adapter
    NT-handle imports and GPU-side waits on wgpu's rendering queue; no extra
    pixel copy/upload/readback. Cached surfaces and native frame leases keep
    the pool bounded through GPU work. Real renderer, held-texture pressure,
    blocked-GPU retention, resize and simultaneous imports verified under
    D3D12 GPU validation; native build/strict clippy,81 engine tests and full
    remote Mac gates pass. Final six10s runs:3606 completed renders at1080p/4K,
    no counted drops; callback→GPU completion0.575/0.878ms median.
    Reopening also exposed a WinRT factory lifetime crash, fixed by retaining
    the process MTA; Windows10 RPC handle retention across fresh capture
    workers remains a measured follow-up before Platform integration.
    Output pools/NVENC/writer/daemon selection remain separate work.
  - [x] **W.5c1 Native NVENC H.264 (agent/windows-capture).** Commit692314a,
    Actions39086165352 green. Asynchronous
    D3D11 P5/low-latency/High/709-limited encoding, eight bounded private GPU
    input/output slots, no CPU pixel readback. Native mapping is exclusive,
    so copy shared frames on GPU before mapping. H.264+mux files decode at
    1080p60/4K60; red-patch colour, repeated input plus concurrent compositor,
    rate changes, flush/drain and12-session driver admission verified.
    First matched single-window/silent-AAC OBS comparison:12 files decode,
    zero reported losses; prototypeCPU7.0/8.2% vsOBS9.8/12.7% of one core.
    Not full program/ISO parity: benchmark AAC is an external FFmpeg helper.
    MF hardware/software fallback, HEVC, GPU output pools, native AAC and
    engine priority/ISO shedding remain gates before Platform selection.
  - [x] **W.5c2 H.264 Media Foundation fallback (agent/windows-capture).**
    Commit7b17f64; Actions39089312953 portable jobs passed; native UI build
    failed with rustc-LLVM out-of-memory (platform crate compiled).
    NVENC-first provider selection, observable hardware-MFT/software-MFT
    fallback, private GPU NV12 conversion and tracked-sample retention.
    Explicit software download only; bounded admission and timestamp checks.
    Final low-latency hardware1080p60/4K60, software1080p60 and NVENC
    regression files decode2400/2400 frames with zero drops;
    12 occupied NVENC sessions force the software fallback, which records
    60/60 frames while existing sessions survive and capacity returns.
    16 real reopens per MF provider have stable native handles. Strict native
    clippy,81 engine tests, full remote Mac gates and independent review pass.
    Ready for CI; HEVC/10-bit follows this slice. No WindowsPlatform selection
    or full recording parity yet.
  - [x] **W.5c3 HEVC Main/Main10 (agent/windows-capture).** Extend native
    NVENC and hardware MF without codec/depth downgrade; length-prefixed HEVC
    packets and truthful hvcC, GPU-resident BGRA8 and actual RGB10 input,
    P010 conversion and matching DX12 import. Verify real1080p60/4K60 files,
    decoded10-bit precision, bounded retention/drain and H.264 regressions.
    NVIDIA HEVC MFT is installed on exon-desk; no software HEVC MFT is present.
    Final1980/1980-frame matrix, true10-bit precision, concurrency, lifetime
    and pressure checks pass; native strict clippy/81 engine tests, full
    remote Mac gates and independent review pass. Ready for CI.
    Engine codec-tag guard remains required when
    wiring HEVC to the writer, never the H.264 live/FLV path.
  - [x] **W.5c4 Recording-quality construction and nonterminal barriers.**
    Explicit native QP/Quality from VideoTrackSpec, no bitrate inferred from
    VideoToolbox quality. Packet-acknowledged barrier preserves prefix on
    failure without EOS; drained sessions start new files with forcedIDR and
    monotonic inputPTS. Native terminal errors preserve queued valid output.
    41files/1230decoded frames verify native quality ladder, two-file reuse,
    callback-error retry and liveCBR regression. Microsoft software quality
    rounding fails closed; default software recording remains unqualified.
    Native clippy/81engine tests and read-only review pass; full portable gates
    blocked on build-host disk floors, not bypassed. No full-engine/perf claim.
  - [x] **W.5c4a Nearest software MF quality.** Read actual after media-type
    negotiation; bounded nearest-value selection and truthful actual report.
    Production64×48 H264 defaultHigh70→72 now records; all101raw qualities
    and5factory presets pass.115files/711frames including NVIDIA regressions
    decode;101opens keep454handles. Native strict clippy/81tests/review pass.
    Uncommitted, ready for CI; full remote portable gates pass233unit+1doctest
    after m2-b space recovered, no cache cleanup or disk-floor bypass.
  - [x] **W.5d1 Native AAC and WASAPI capture primitives.** Microsoft raw48k
    AAC-LC with explicit1024-frame pre-roll, bounded admission, nonterminal
    polling and terminal trim; event-driven mic/endpoint loopback with native
    clock/loss accounting and joined teardown.56AAC edge cases plus actual
    24k/48k devices→AAC→MP4/full decode pass; repeated handles stable. WPR/ETW
    scoped evidence, not full-engine OBS parity. Endpoint exclusions/playback,
    absolute A/V alignment and reconnect qualification remain separate gates.
  - [ ] **W.5d2 GPU Media Foundation cameras.** Correct physical target is
    **Elgato Cam Link4K** (Ryan corrected the earlier4K S assumption). Prior
    MF-source4K S results are wrong-target diagnostics, not Cam Link proof.
    SharedReadOnly FrameServer now supplies realMJPG1080p60 without changing
    the bridge. The original WIC path is replaced by MF decoding in W.5d2b;
    native NV12 retains the GPU-only path.30QPC-matched nonuniform frames,
    248,832,000exact bytes and discriminatory field-swap/line-double controls
    pass;180-frame Renderer/NVENC run passes GPU validation and12reopens.
    Safe own-reader loss/stopped/reopen passes; physical unplug/replug remains
    blocked by protecting the bridge. Native165tests/strictClippy and portable
    full233unit+1doc gates pass; fallback Astra review has no blockers for
    this capture-library slice (ready forCI). Delegator chose
    sample-flag precedence/default weave: preserve both already-interleaved
    fields unchanged on GPU. Native capture, genuine NV12 plane-view rendering
    and source-keyed manifest metadata bridge are implemented;180-frame real
    capture/render/native-encode and bounded retention/reopen checks now pass
    on the realCam Link via sharedMJPG. Writer lane owns native Platform camera/metadata
    hooks and actual-frame ISO input selection. Shared GPU-only surfaces and
    single-field pairing remain unsupported; no other app terminated.
    Optional per-source off/weave/bob setting comes later. No RGB roundtrip
    for camera/ISO or fake plane textures. Regular reviewer unavailable.
    - [x] **W.5d2a Native NV12 input slice.** Committed42e2a922 by coordinator.
      GPU array-slice/crop copies,
      private NVENC input and MF NV12 copy/P010 code expansion pass exact
      component checks and7files/210decodedframes. BGRA regression37files/
      1110frames, native fmt/strict Clippy/81tests pass. Full portable gates
      now pass onm2-b:233unit+1doctest,1hardware-only ignored; no floor bypass.
    - [x] **W.5d2b Common camera formats.** Uncommitted, ready for CI.
      Missing SDR colour metadata no longer rejects Elgato Virtual Camera.
      Prefer NV12 at matching dimensions/nominal rate, then other raw formats,
      then MJPEG. YUY2/UYVY/RGB24/RGB32/ARGB32 use one upload plus GPU
      conversion; MJPEG uses the MF software decoder (replacing WIC) and the
      same upload. Manifest records subtype, decoder output, rate and colour.
      Exon-desk real CLI 10s program+ISO, all six files fully decoded: virtual
      600/600 frames, shared Cam Link597/600,4K S599/600 (pacing drops retained).
      Virtual delivered its Camera Hub placeholder;4K S was black, not a
      verified live HDMI feed. Cam Link delivered live imagery without touching
      the bridge. Real virtual-camera YUY2/ARGB32 GPU and NV12/YUY2/ARGB32 CPU
      samples passed60frames each; pixel/range/stride/8K-RGB24 checks pass.
      Native strict Clippy including UI, release CLI/daemon build,90 protocol
      tests and full portable250tests including doctest pass (1existing ignore).
      Final redundant recording attempt waited on writer's GPU lock and timed
      out without running; earlier recording receipts and final pixel/native
      build receipts are preserved separately in capture scratch REPORT.
    - [ ] **W.5d2c Attribute historical startup drops.** Original CamLink
      program+ISO lacked ticks6–8;4K S lackedtick6, not59.94drift. The exact
      historical pacing/queue/encoder cause remains unproven: it did not recur
      under ETW, without profiling, or with bounded stage timestamps. No
      speculative behavior change or drop-counter relaxation was retained.
      - [x] **60s zero-file-loss qualification.** Waited for shared GPU lock.
        Three60s series per camera (GeneralProfile+GPU, unprofiled production,
        temporary stage timing with CSwitch-only ETW);18program/ISO files,
        64,800frames fully decoded,3600/3600 each,0reported drops. Existing
        traces show0encoderrefusals/obsolete program ticks and low wake jitter.
        CamLink MFdecode mean9.08ms/max12.32ms, upload-submit max1.37ms; observed
        delivered cadence59.994Hz. Virtual placeholder31.960Hz and black4K S
        58.753Hz are source-adapter observations, not proven sensor rates;
        clocked picture reuse fills60fps outputs, not missing file samples.
        Temporary timing code removed, production matchesd18f2515 exactly;
        native daemon rebuilt without instrumentation. Verification ready for
        CI, NOT a claim that the old intermittent startup failure was fixed.
        In-take metrics exclude measurablepre-roll stalls (CamLinkupload24.8ms,
        4K Sdevice→worker46.9ms); these precedeepoch and do not root-causeold
        epoch+0.1s gaps. Unprofiledreceipt syncedtemporarysource but explicitly
        SHA-checked/rantheoldproductionbinary withoutbuilding; seeREPORT.
  - [ ] **W.5s Windows streaming.** Reuse portable RTMP/RTMPS, destination/
    canvas hubs, queues and reconnects. Full feature remains open until native
    budget/scaling integration and desktop Credential Manager proof are done.
    - [x] **W.5s1 Native AAC + matching-size transport slice — ready for CI,
      uncommitted.** MF AAC requested at 160 kbps produced 107/105 kbps in the
      unchanged oracle; native CBR/VBR properties returned E_NOTIMPL. Live AAC
      fill prefixes restore the bitrate without changing any native payload or
      recording packet. All 24 mono/stereo/rate/content cases decode exactly
      like their unpadded counterparts. Main and portrait stream oracles pass.
      The 615-second 1080p60 program + two ISOs kept 36,900 frames per file.
      Two RTMP destinations each received 36,746 frames over 612.434 seconds:
      zero drops, full decode and relative frame checks. Receiver kill/reconnect
      preserved recording and the other destination. RTMPS used a scoped test
      CA; untrusted-certificate failure preserved 900/900 recording frames and
      the trusted case 1800/1800. The native NVENC governor acknowledged
      6000→3000→1500 kbps; program and ISO kept 2400/2400 frames. The congested
      stream's 151 intentional drops remain reported. No aggregate admission
      or scaled-input claim is made from these legacy-factory runs.
    - [x] **W.5s2 Hidden Windows CLI secret entry — ready for CI, uncommitted.**
      Disable console echo before the prompt; fail closed if that fails; restore
      the mode afterwards. Headless ConPTY verified hidden input, restored echo,
      no key in output/private files, and the expected SSH logon refusal.
    - [ ] **W.5s3 Credential Manager persistence.** The native generic-user store
      is implemented and reviewed: bounded UTF-8 blobs, case-safe target names,
      no file fallback. Positive roundtrip needs a legitimate desktop logon.
      SSH returns 1312; the desktop launcher refused active use and has an
      Isocast-only allowlist. No guard bypass or real-key read was attempted.
    - [ ] **W.5s4 Shared admission and scaled destinations.** Integrate the
      writer's reviewed live/fixed-active ledger; no separate stream budget or
      wholesale NVENC replacement. Windows live encoders currently need matching
      input dimensions. GPU scaling remains next. No new test files or CI jobs.
  - [ ] **W.5p Windows performance capture/encoder lane (#105).** Follow
    main's docs/windows-performance.md; all changes remain uncommitted until
    coordinator handoff. No silent codec/quality substitutions.
    - [x] **UI DX12 heap sizing (2026-10-19).** Windows UI uses wgpu's
      MemoryUsage allocator hint, keeping preview size/rate unchanged.
      Matched n=3 main/candidate cohorts: private commit1513.85→1335.06MiB
      at1080,1755.75→1574.85MiB at4K; resident memory essentially unchanged.
      No CPU-speedup claim.48files fully decode, native zero losses;
      Windows oracle9pass/2partial-pending, zero losses; UI strict Clippy,
      fmt and13tests pass. Ready for CI, uncommitted; broader UI CPU open.
    - [x] **Windows adapter scheduling hooks (2026-10-19).** Cached deadline
      clock from first tick; Audio MMCSS for capture/AAC/mixer, AboveNormal
      video workers, recording-scoped power guard with warning-only failure.
      Native strict Clippy/85engine tests, reviewed; matched native n=3 at
      both sizes and Windows oracle zero losses. CPU not improved consistently
      (1080higher,4Klower); performance target remains open. Ready for CI,
      uncommitted. Win11 power-policy runtime proof remains outstanding.
    - [x] **DX12 descriptor heaps (2026-10-22).** UI/compositor request65,536
      live non-sampler bindings instead of1million. Quick paired measurements
      cut another~55MiB private commit; resident memory unchanged. Reviewed;
      final matched n=3 and Windows oracle zero losses, strict native Clippy
      and188tests pass. Ready for CI, uncommitted; not a CPU/RAM-goal win.
    - [x] **First-packet status (2026-10-22).** Optional output_active marks
      an actual muxed video fragment, not raw admission. Early publication
      lowers observed all-active median1162→473ms(1080),1188→502ms(4K),n=3;
      no claim of accelerated encoding or fsync. Same API for all clients,
      unknown backends omit it. Reviewed, native checks/oracle pass; ready
      for CI, uncommitted. Full CPU/preview parity remains open.
    - [x] **Native48k microphone de-jitter (2026-10-22).** Continuous device
      positions exposed short QPC spikes causing ISO silence/trim. Five-sample
      warmed phase median removes the exercised3ISO gaps; corrected4spikes in
      an80s4K take with0output losses. Microphone only; SRC/loopback/startup and
      real gaps retain prior timing. Six inline tests/native Clippy pass;
      final n=3/oracle and portable gates pass with documented partial cases.
      Ready for CI, uncommitted. Hardware proof preceded microphone-only scope
      narrowing (same mic path); historical QPC fatal/long soaks unqualified.
      Final Native CPU26.227/24.279 vsOBS18.356/17.726%onecore at1080/4K;
      resident299.204/375.903 vs216.044/276.175MiB. Overall goals remain open.
    - [ ] **#1 Aggregate encoder budget.** Native preset/driver-keyed planning
      API and explicit no-provider-fallback factory implemented/reviewed.
      Real2x4K60+1080p60,30swarm+60smeasure: H264P56938/10800 at100%NVENC;
      P3all10800/10800 at76.3%. HEVCP5allalreadyfits at88.05%; planned5/1/3
      fitsat78.80%.49files1470frames decoded. Writer aggregatepre-open wiring,
      selected-preset reporting and automatic calibration/cache integration
      remain open.
      - [x] **#1a Native planning/preset and calibration-data slice.**
        Finite positive per-preset measurements accepted without imposing
        monotonic hardware throughput; only beneficial ISO downgrades.
        Six actual driver-keyed measurements, independent load recomputation,
        invalid/stale/nonmonotone cases and native Clippy pass655fa805.
        Measured plans H2643/3/3 load0.638146; HEVC5/1/3 load0.759703.
        Short-window workload calibration, not an automatic daemon cache.
    - [x] **#3 Shared async completion.** Ready forCI, uncommitted. One worker/LUID,12sessions/two
      devices, bounded8pending/16total,32parallelreopens, stablehandles,
      retained-prefix/fatalquarantine and deterministic retirement-race tests
      pass. Independent review/recheck closes findings. MatchedWPRbothcodecs
      10800/10800/noMisses; CPUH26417.68→16.30%,HEVC15.96→15.64%onecore.
      Combinednative strictClippy/165unit tests pass; protocol-doc check
      separately passes aftersharedtimeout. Fullportable233unit+1docpass;
      no blanketlatency/engine-stopclaim.
    - [ ] **#6 Borderless DXGI monitors.** Additive dedicated-helper-device
      API functional slice readyforCI; actual4Kpixels/16GPUcursorcases/
      pressure/reopens and stale-final-update/burst fix proven/re-reviewed.
      PendingAcquire nominal1ms actuallyp9916ms; timer correction lowers
      DDA CPU6.635→5.699%,update-agep9916.35→2.879ms. MatchedWGC4.685%
      remainsbetterCPU; DDA23longgaps, no fullperformance/no-drop claim.
      Both workers nowAboveNormal; mechanics ofWGC remainunchanged.
      Helper routing/performance gates open; final strictClippycc0287d8pass.
      - [x] **#6a Opt-in functional capture slice.** ReadyforCI, uncommitted.
        Final704dd37c and independent two-hunk re-review pass; all own
        fixtures/probes/guards ended, private ETLs retained remotely.
    - [ ] **#7 Waitable deadline clock.** ExplicitQPC-based high-resolution
      timer, engineopt-in and no-obsolete-renderburst policy implemented and
      reviewed. ThreepairedWPRruns:0misses, p99~1.05ms, NOT<1mstarget.
      No sleep-spin existed here; no claimedCPUwin. Adapterhook remains open.
      - [x] **#7a Cached native timer and portable opt-in slice.** Actual
        MediaClock first-tick selection/default preservation, no false
        escalation/global contamination and Stop hand-off verified b503a2a6.
        Runtime1.99.1 already uses a fresh high-resolution timer per stdsleep;
        new TLS reuse avoids Create/Close churn, not an existing spin loop.
    - [ ] **#11 Scheduling/power.** AudioMMCSS/AboveNormalvideo helpers and
      recording-lifetimeguard hooks reviewed.32threadcycles174handlesstable;
      ownsingle-corecontention288/600misses→0forvideo/audio. Win10policyquery
      unsupported: guardNoOp, no guessedrestore. Win11save/restore and native
      adapterwiring remain open; no timeBeginPeriod request shipped.
      - [x] **#11a Thread roles and recording-lifetime hook slice.** Real
        WaveXLR48k AB/BA contention14e4908e: first normal loses2304steady
        frames/MMCSS0, second steady pair ties0 (normal warm-up loses1824).
        0timestamp errors,0lostETW events, processNormal unchanged. Helpers
        and supported-policy guard source reviewed; Win11runtime gate open.
    - [x] **#12 Event-only WASAPI.** Ready forCI, uncommitted. Remove250msidlepoll, add session-format/
      service-disconnectioneventwakeup.24realdevicecycles175handlesstable;
      registered-handlerfailure/reopen passes, no actualdevice/service reset.
      FinalWPRidlewakeups4.02/s→0,active100.87→100.82/s;720000frames/15s,
      0gaps/timestamp errors,NVENC0%,0lostETWevents. NativeClippy/165tests
      andfullportable233unit+1docpass (nativeprotocol-doc rerun aftertimeout).
      Unattributedprobe APIs/export preserved untouched and excluded from
      owned-only audio-event-only.patch in capture scratch.
    - [x] **#13a Startup overlap slice (2026-10-19).** Ready for CI,
      uncommitted. WPR identified eager shader preparation on the startup
      path; Windows compositor shader families now prepare concurrently and
      join before use. UI starts a bounded existing-API preview warm-up while
      opening its GPU. Five qualifying app-only process-cold pairs: median
      first-positive picture observation1.471s versus OBS1.857s, same720p30
      colour scene, warm filesystem/driver caches. Two Isocast cases1.58s;
      not an every-start guarantee or exact presentation-time speedup.
      Native build/strict Clippy, portable250 tests, oracle7pass/2pending,
      early-death idle cleanup and900/900-frame app-kill/resize take verified.
      Cua's desktop-wide UIA discovery delay excluded by owned-PID native
      window discovery; raw prior attempts and sampling bounds retained in
      windows-capture/scratch/REPORT.md. No encoder probe/cache changes:
      none occur on this path; wgpu30 persistent DX12 cache is unsupported.
- [ ] **W.6 App, IPC and shared surfaces.** Slint Windows window and native
  integration, same-user named pipes with isolated engines, D3D11/D3D12
  surface hand-over/fences with adapter identity and bounded per-viewer
  lifetime. Verify CLI/MCP/app use the one API, client death/resize/stalls
  cannot corrupt frames or stop recording, and shutdown cleans resources.
  Measure first preview and ready-to-record before setting latency targets;
  lazy initialization, parallel device opens and pre-warm must keep blocking
  work off the UI thread. Performance is a design requirement, not a later
  optimization pass.
  Platform-aware wording pass: preserve Mac copy, but Windows says "this PC",
  "Credential Manager" and "Explorer", uses Ctrl shortcuts, and names the
  real Windows executable/privacy controls instead of Mac/Keychain/Finder/
  Command glyphs or "Isocast Engine" macOS privacy instructions. Cover
  onboarding, capacity/admission, settings, errors and shortcut labels.
  - [x] **W.6 wording qualification.** Windows vocabulary/privacy/executable
    instructions and shortcut labels verified in ten actual VM offscreen
    screenshots, native help and the first-launch window. Mac239 unit+
    1doctest/gates and nine unchanged snapshot text regions pass. No native
    integration/capture claim from labels; hosted UI memory work is separate.
  - [x] **Shared GPU preview slice (2026-10-18).** Ready for CI, uncommitted.
    Same-session/LUID DX12 handover, producer completion and renderer-queue
    fences; bounded current/retired rings. Real Windows studio/program,
    source Properties target changes and projectors show live pixels.
    Held-GPU/resize probe passes; app killed during a 10s take leaves all
    three files complete, 300 frames each, no losses, full decode passes.
    Oracle 7 PASS/0 FAIL/2 PENDING (not shared-preview coverage). Portable
    250 tests incl doctest/one existing ignore; native strict checks and
    release UI pass. Startup and per-viewer CPU/GPU samples, limitations
    and receipts: windows-capture/scratch/REPORT.md. Broader W.6 stays open.
  - [x] **W.6 first run on Windows (Ryan's 0.0.7.801 report).** No console
    window (GUI-subsystem app; engine started with CREATE_NO_WINDOW); no
    permissions step unless a Windows privacy switch blocks the camera or
    microphone, then one Access step opening its exact Settings page; OBS,
    `~` and app folders from USERPROFILE/AppData/known folders instead of
    HOME; "Preview not available" instead of retried `shared_frames`
    errors; Optimize skips a source it can't open with a per-source
    warning; doctor lists NVENC/Media Foundation encoders as recording
    would open them. Verified in the clean VM and on exon-desk (REPORT).
  - [x] **W.6 Windows chrome.** Dark title bar, border and menu bar in
    Console tokens on every app window (Windows light and dark modes); app
    icon in Start, taskbar, Task View/Alt-Tab, title bars and the installer;
    the first window sized and centred in the work area (1024×768, 150%);
    Calibrate disabled on Windows with a tooltip.
  - [x] **W.6 Windows menus.** Exit Alt+F4, Redo Ctrl+Y, Maximize/Restore,
    Full Screen F11 (menu bar hidden in full screen), no Bring All to Front;
    Hotkeys page matches. Mac menus unchanged.
- [ ] **W.7 Packaging and updates.** Choose MSIX or installer; reproducible
  install/upgrade/uninstall, correct paths and per-user state, verified
  update authenticity and rollback. Build/test packages on runners; signing
  credentials, releases and publication require the delegator/Ryan.
  - [ ] **W.7a Per-user installer.** Pinned Inno Setup compiler, adjacent
    app/CLI/daemon and app-local CRT, no elevation or forced process exits,
    install/uninstall gate and downloadable unsigned payload in windows.yml.
    Clean Windows 11 evaluation VM under `C:\isocast-dev\vm`.
    - [x] **W.7a1 Test VM.** Official hash-verified Windows 11 Enterprise
      Evaluation, Gen2/Secure Boot/vTPM, 8 vCPU/32 GiB/200 GiB dynamic disk.
      Key-only admin and standard-user SSH; offline `clean` checkpoint and
      up/ssh/ps/down/revert wrapper verified, including actual clean restore.
  - [x] **W.7b Authentic updates.** Windows appcast alongside Sparkle's,
    pinned update verifier and embedded public key, idle-engine interlock,
    explicit trusted rollback; real WinSparkle download/signature rejection
    and successful update verified with a private test key. Feed-supplied
    installer arguments are ignored. Authenticode/publication remain gated.
  - [x] **W.7c Clean-machine qualification.** Fresh install/first launch,
    real older-build upgrade, rollback, uninstall, preserved settings and
    recordings, tampered-update refusal: native0.0.7.1→.2→.1→.2 in the clean
    Windows11 VM as a non-admin. Exact binary hashes and retained files
    verified; after-uninstall movie full decode passes. VM uses WARP/CPU;
    no GPU/NVENC or production capture qualification. Runner packaging is
    still open under W.7a pending the hosted UI-memory fix.
- [ ] **W.8 Windows e2e and stress.** Port existing synthetic e2e/stress
  checks: recording/ISOs/streaming, pause/split/retake/export, killed writers,
  ownership, rapid controls, overload and stalled clients. Run short gates
  on pushes; bounded explicit long soaks, not two hours for every push.
  Preserve integrity/audio/sync gates and record any runner limits honestly.
- [ ] **W.9 Windows alpha readiness.** Map every `docs/alpha-test.md`
  item to Windows evidence, using the newer crash-safe contract where that
  run sheet is stale: install, doctor/tune, two-hour program+ISO+local-stream
  soak, UI workflows, exact crash recovery and private report preview.
  Record loss and full-decode/A/V results; distinguish hosted qualification
  from the real camera/mic/display/GPU/permissions/signing checks still
  needed. Hand Ryan the checklist and one install command for his machine.

### R. Remote engines: any app on any engine (Ryan, 2026-10-22)

Scope: macOS ↔ Windows, Linux later. The design, decisions and done
conditions are in [docs/remote-engines.md](docs/remote-engines.md); read
it before any R task. Transport, pairing and remote Start/Stop are
Tier 1. A remote library (other engines' recordings and projects) is
future work, outside R. Order follows Windows quality work (W) unless Ryan reorders.

- [x] **R1 Foundation:** `isocast-remote` (rustls mutual TLS 1.3, raw
  public keys, no resumption), `Stream` enum with a duplex TLS driver,
  keepalive and timeouts, engine identity and `hello`, code + SPAKE2
  pairing opened only locally, client identities (no bearer tokens),
  caller-origin policy (machine control stays local, obs-websocket
  included), OS DNS-SD, accept limits, recording session ids, stream-key
  clearing, attribution, `--engine` for CLI and MCP, loopback mode in the
  gates. Done when each OS's CLI records on the other OS's engine,
  survives a pulled network, and a revoked client is cut off.
  - [x] **R1.d Discovery seam:** `isocast-discovery` (`register`,
    `browse`, `watch`) over Bonjour and Windows `DnsService*`, TXT only
    `id`/`proto`/`name`, `NSBonjourServices` in the bundles,
    `isocast engines --discover [--watch]` and `isocast debug advertise`.
    The transport lane calls `register` from `remote_enable`.
- [ ] **R2 App on a remote engine:** Settings → Remote (two switches, off
  by default, standalone like OBS), engine picker and pairing, the
  `Engine` handle (local-only flows gated), presence, connection states,
  wording by the engine's OS, remote permissions, engine file pickers
  (rooted, listing only), projector ownership, JPEG interim previews.
- [ ] **R3 Pictures:** shared, capped preview encoders on a size ladder,
  frames skipped before encoding (viewer-paced), hardware decode into the
  preview renderer, governor sheds previews first.
- [ ] **R4 Sound:** monitoring on the app's speakers via Opus, in sync with
  previews.
- [ ] **R5 Input:** hotkeys scoped to the engine machine or the app.
- [ ] **R6 Host mode:** engine at login without the app, updating the
  other side on a version mismatch.
- [ ] **R7 Hardening:** security review, fuzzing the pre-auth surface,
  impairment matrix, user docs.
- [ ] **R-L Linux (later):** Linux app (decoder, packaging), then the Linux
  engine backend (PipeWire, V4L2, VAAPI/NVENC).

### 9. Later (pull into the queue once the above is done)

- Streaming: RTMP and SRT through FFmpeg or GStreamer, WHIP through `str0m`.
- Syphon client source; Audio Unit plugins as audio filters.
- A second canvas (vertical video) and a replay buffer.
- Linux backend (Windows now tracked in W).
- NDI input and output.
- FilmCraft reads `session.json` to line up ISO files without audio sync.

## Log

- R1 (2026-10-22): remote control of an engine on another machine works: mutual TLS 1.3 with pinned keys, local-only code + SPAKE2 pairing, caller policy (machine control stays local, keys never to caller-chosen servers, folder roots and browser rule as guard rails), OS DNS-SD discovery, session ids, `isocast remote`/`engines`/`--engine` for CLI and MCP. Verified basebook <-> exon-desk both ways (30 s program + ISO recordings, zero loss; Reconnecting 3.97 s after a link block, same session; revocation 0.50 s); four, three and two review rounds; gates 269 tests.

- R1 transport/integration (2026-10-22, ready for review): pinned TLS,
  single-use PAKE, OS-backed identities, caller origins, discovery, CLI/MCP
  engine selection and liveness. Mac CLI → Windows engine: program + ISO
  905 frames / 30.166667 s; Windows CLI → Mac engine: 929 / 30.966667 s,
  H.264/AAC, equal A/V, no reported losses, full decodes clean. A 20 s
  port block: Reconnecting in 3.971 s, fresh snapshot and same session;
  revocation closes the watcher in 0.496 s and refuses reconnect. Loopback
  TLS is in cargo test; fmt/strict Clippy, 268 unit tests + 1 doctest and
  native Windows checks pass. Main 6619f88a integrated, uncommitted;
  /Volumes/IsocastBuild/remote-r1/scratch/REPORT.md has evidence and risks.

- W.6 integration blockers (2026-10-19, #105, uncommitted): one pinned
  DX12 adapter LUID now serves capture, compositor and viewers; software MF
  BGRA8 no longer requires a video-processor interface. Recording admission
  absorbs short encoder stalls with bounded shared wait/cooldown windows.
  Controlled120ms stalls:11/540 lost before,0/540 three times after;300ms
  still reports30/540 lost, no audio dropouts. Native full oracle9/0/2,
  zero lost; macOS gates and native strict checks pass. Corrected the
  oracle's exact0.15s flash/beep cutoff against its retained failing movie.
  VM CLI without UI:720p30 H264,10s,300frames/0lost. Live previews now show
  pixels, but VM recording with UI open still drops under an exclusive
  host reservation; W.6 GUI qualification remains open. Full evidence and
  proposed commit messages: windows-integrate/scratch/REPORT.md.

- W.6 Windows menus (2026-10-18, #105): the menus follow Windows
  conventions.
  - File ends with Exit (Alt+F4); Redo is Ctrl+Y.
  - The Window menu has Maximize/Restore, and Full Screen / Exit Full
    Screen on F11 (which hides the menu bar while full screen). Bring All
    to Front is gone.
  - The Hotkeys page matches.

  Verified with windows-gui.sh screenshots of all nine menus in the clean
  VM, of Restore / Exit Full Screen after maximizing and pressing F11, and
  of F11 full screen (no menu bar) and back (bar #1e1e1e again).
  15 Mac snapshot pairs are unchanged, including every menu bar menu and
  Hotkeys. Windows UI clippy passes.

- W.6 Windows chrome (2026-10-18, #105): Ryan saw a white title bar and
  menu bar over the dark app. Now:
  - The title bar, border and menu bar are drawn in Console's tokens:
    winit's dark theme, DWM colours, and a menu-bar subclass with dark
    dropdowns.
  - The app icon comes from Isocast.icon's design, as a resource and in the
    installer.
  - The first window fits the work area.
  - Calibrate is off on Windows, with a tooltip.

  Verified in the clean VM (0.0.7.903, Start menu): 1024×768 in Windows
  light and dark mode, and 1920×1080 at 150% in both modes. Title and menu
  bars measure #1e1e1e, the line #242424. Mac gates pass (250 tests); 9 Mac
  snapshot pairs are unchanged.

- W.6 first run on Windows (2026-10-18, #105): Ryan's 0.0.7.801 report.
  isocast-ui.exe is a GUI program (subsystem 2; it was 3) and the engine
  starts with CREATE_NO_WINDOW. Onboarding asks nothing unless a Windows
  privacy switch blocks the camera or mic (consent store), then one Access
  step opens its Settings page. OBS, `~` and app folders come from
  USERPROFILE/AppData/known folders, not HOME. "Preview not available"
  (UNSUPPORTED, one request) replaces shared_frames retries. Optimize skips
  an unopenable source with a warning, and doctor lists NVENC/MF encoders.
  Verified in the clean VM from the Start menu (no console, both onboarding
  paths, OBS found, Optimize with a missing camera) and on exon-desk (doctor
  NVENC, Ryan's Elgato case skipped). Mac gates 250 pass; 8 Mac snapshot
  pairs unchanged. windows-firstrun/scratch/REPORT.md.

- W.5p main integration (2026-10-18, #105): merged main565729db into capture
  1b9fc578, staged without commit. Preserved main's named canvases/governor and
  capture metadata/pacing hooks; main's UI memory workflow replaces the older
  global-jobs workaround. Portable fmt/strictClippy/248unit+1doc pass; native
  services/all-target build and separate NO_GALLERY/-j1 UI+test build pass on
  tree91ec7b7a. Review clear; native API/cache/completion handoff to writer.

- W integration (2026-10-18, ready for CI): main1dbb44c9 merged into
  agent/windows5e2d496b withoutcommit. Retained WindowsBGRA/NV12/helper/camera/
  rawISO/Stopwake paths with main's canvases, framebudget, masks, self-tuning
  andfollowups. Windows CI uses main's splitUI/no-gallery memory fix, plus
  explicit nativepackage forhelper compilation. Macgates pass249unit+1doc,
  strictClippy/fmt (1existinghardwareignore); Windowsnativeall-targetcheck
  passesdc58f10e. Independentfallback audit clear; ordinaryreview unavailable.
  Merge staysuncommitted forcoordinator WindowsCI, no runtime/performanceclaim.

- W.4 (2026-10-18, ready for CI: Windows daemon recording): committedcamera
  snapshotfa061065 merged deliberately (pendingMERGE_HEAD); native daemon
  selects sharedWindowsPlatform, rawISO input matchesactualFrameformat.
  CLI58/58checks, actualapp53/53checks, fullA/Vdecode/zero losses. App's
  initiallymissing screenISO traced to WGC PTS26msahead; due-time publication
  changed missing600→0, preservingtimestamps andbounds. NativecleanUIexit
  now0 afterexplicitcleanup (previousnativeexit-phaseoverride2170).
  Realdaemonkill/restart37recoverychecks preserveallobservedpackets and
  exactsecond-recoverybytes. Native190tests/strictClippy andMac233unit+1doc
  pass; finalnativefixes have explicitfallbackaudits afterreviewservice
  outages. Stopwakecomponentimproved, butunder20ms/fullperformancetargetsopen.

- W.4 (2026-10-18, ready for CI: WGC helper isolation and lifetime): isolate
  Windows-owned RPC retention in one disposable helper per capture; preserve
  GPU allocation ownership with shared textures/fences and per-picture leases.
  FlatALPC20reopens, post-exit/crash retained-frame encoding, parent-death
  cleanup,32-frame blockedGPU pressure and640→800→640resize all pass. Real
  Engine9A/Vfiles/1080frames and4K60program+ISO25s/3000frames fully decode with
  zero losses. NativeClippy/81unit and Mac233unit+1doctest pass; ordinary review
  cleared the geometry-cache fix. Helper shutdown max33.05ms measured, not a
  <20ms record-stop win; post-isolation CPU comparisons must include helpers.

- W.4/W.5 (2026-10-18, scoped memory reduction; uncommitted): NVENC private
  ring8→4 matches the documented4+B minimum without changing presets/quality
  or shared-frame ownership. Three actual4K60HEVC program+ISO/AAC takes pass
  9,000pictures/full decode/zero reported losses. Matched no-preview recorder
  CPUmedian6.172%onecore versusOBS17.734%; private memory~1,085 versus~1,160MiB.
  RSS~368 still exceedsOBS~349MiB: overall memory/full-app gates remain open.
  ModerateCUDAcontention preserves1,200packets, max polled latency30.55ms;
  single4KH264 remains88.9fps, not an overload fix. Native strictClippy,81unit
  tests,7native contracts and1080p/HEVC8/10 regression pass; capture unavailable
  is not full parity. WGC lifetime isolated to method-call ALPC retention;
  allocation-stack trace precedes user-approved GPU-shared helper containment.

- W.4/W.5 (2026-10-18, profile before tuning): branch4a2063e8 finalMacgates
  pass(m2-b,233unit+1doctest,oneexistinghardwareignore). Exactold4Kfailure
  reproducedunderWPRwithzero lostevents. Native-boundarytimingandNVMLshow
  H264/P5encoderbackpressure,notcopy/map/fileIO:9.223sof10.175sinEncodePicture;
  isolatedNV12/P5control~86.6fps. RealEngine2×4K60H264alsofails; explicit
  HEVC/P5comparisonpasses600/600pictures,zero countedlosses,fullA/Vdecode.
  Noqueueenlargement,presetdowngradeorcodec-switchmasqueradingasH264fix.
  OBSmatchedconfigurationqualificationandWGCfresh-vs-reusedthreadlifetime
  experiments are active; allnewwork remainsuncommitted as requested.

- W.4a (2026-10-18, real WGC engine/discovery; still in progress): actual
  productionWindowsPlatform+Engine resolvesownwindowsbyapp/title andrecords
  program+2ISOs+WASAPI/MF AAC,3takes9files1080pictures/full-decode/colors/tone
  checks,zero countedlosses. No copied-compositor/test-policyoverride. Native
  display/window/micdiscovery, physicalsizes, stablemonitorIDs andUnknown
  permissions implemented; explicitcaptureworkswhiledoctorwarns/initusesreal
  devices.32readonlydiscoverycycles151handlesconstant. FixedinitialUnknownUI
  reportflow afterreview; fullnative/UIstrictClippyand178unittests pass.
  Finalfullnativecontractprofileis7passed/captureunavailable(Unknownconsent),
  neverfullparity. FinalMacgatesstilldisk-floorblocked. Daemon
  remainsstub pendingcapturelifetime/performance/camera/remainingPlatformgates.

- W.4a (2026-10-18, production A/V integration; still in progress): imported
  qualified native AAC/WASAPI/recording-policy snapshots. ProductionWindows
  media7/7passesnormal/pressure/ownership/realcommit-killrepairwithtwoAACtracks,
  measuredsync0 andidempotentrepair. ActualEngine+WASAPI997Hzstimulus+MF AAC+
  filteredprogram/2ISO3takes9files540picturesfull-decodewith2s tracks,zero
  countedlosses.20AACedges243checkscovertinyfirst/lastsamples,audio-only
  cancel/drop,gaps,invalidinput,reservationvalidationfailure. Reviewfound
  advancing16kcaller starvation; boundedtwo-batchgapfill nowoutpacesit and
  nativefalse,false,true/tone-placementregressionpasses. Reviewresolved.
  Windowsdiskprobe no longerdeletesunownedpathname; realcanarypreserved.
  Nativechecks pass; finalMacgatesblockedbyallbuildhostsbelow40GBfloor.
  EarlierMacoutputbatch233unit+doctest and19/19e2e+9filteredISO filespass.
  Daemonstillstub; capture-lifetime/4K performance anddiscovery gatesremain.

- W.4a (2026-10-18, real recording proof; still in progress): retained
  Session1 probes now record native WGC→NVENC→FileWriter program+2ISOs.
  640/1080 files each hold300frames/5s; full decode, shared timeline, distinct
  program/ISO pixels and native HEVC8/10/gap-tail cases pass. Real kill after
  2.5s leaves150verified frames in each of3files; repair/full decode and byte-
  idempotent second repair pass.1080CPU single sample4.293%ofonecore, not a
  matched OBS comparison.4Kthree-output5s media takes10.155s; two outputs
  reproducibly refuseISO tick23 with8pending/15completed. Their24/23accepted
  prefixes still finish and full-decode. No queue enlargement, hidden retry,
  quality downgrade or native-encoder patch. Corrected probe's busy-yield and
  added real-time verification so file counts cannot hide missed deadlines.
  All probe/driver sources, tree and executable hashes retained; nativeAPI
  ownership/AAC coordination remains open. No daemon switch or CI claim.

- W.4a (2026-10-18, in progress): integrated capture lane5d05c5ce locally
  after GPU fixea67be57; merge remains uncommitted. Added a video-only native
  FileWriter taking a caller-configured encoder, deferred codec configuration
  on an owned mux reservation, and identity-checked empty cancellation.
  Native release build/strict clippy,81 existing engine tests and12 contract
  verifier tests pass. Empty cancellation, refusal with a hard link or
  deny-delete reader, nonempty finish/full decode and pinned junction rebound
  pass. Read-only system queries and64 MMCSS thread lifetimes pass. Native
  writer recording has NOT run: shared GPU lock timed out before launch.
  Capture-lane writable-frame/quality/AAC coordination and independent review
  remain open; two reviewer launches failed at the service endpoint. Not
  ready for CI/native parity/performance claims. Evidence: windows/scratch/REPORT.md.

- W.7b hosted CI fix and B1 follow-ups (2026-10-18, #105): run
  39161376599 failed because the hosted step's job forbids breakaway and the
  CLI deliberately won't start a daemon its caller's job could kill. The
  wording and install steps now start their own isocastd. Both pass natively
  inside a fresh no-breakaway job (the old step reproduced "Access is
  denied"). WinSparkle now shows a distinct offline message and sweeps
  staging folders left by interrupted downloads. Clean VM: a killed
  mid-download folder was removed at the next start, decoys were kept, the
  offline wording showed, and all 23 hostile names were still refused.

- W.7b B1 updater confinement (2026-10-18, #105): WinSparkle 0.9.4 now
  builds from pinned source with `winsparkle-download.patch`. Each update
  downloads only to `Isocast-update.exe` in a private per-user staging
  folder. A server name that isn't a strict plain `.exe` is refused with
  "This update couldn't be verified and wasn't installed", and its staging
  folder is removed. The app refuses the unpatched DLL. Clean VM, non-admin:
  all 23 hostile names refused with no new profile entries (one Windows
  TokenBroker cache file aside), no payload copy outside staging and
  unchanged sentinel files and streams. The real app refused a signed
  traversal name with that message, then a normal update took 0.0.7.4 to
  0.0.7.5 and relaunched. Also fixed CI's wording step: debug `isocast.exe`
  overflowed Windows' 1 MiB main stack, now 8 MiB; the step passes natively
  in 43 s. Mac gates pass. Evidence in `windows-pkg/scratch/security`.

- Windows packaging merge-main (2026-10-18, #105): main565729db merged with
  `--no-commit` over095385e0. Canvas/settings/self-tuning behavior retained,
  W.6 words reapplied; main's debug/no-gallery/isolated-j1 Windows CI memory
  fix retained with installer and updater gates. Mac gates248unit+1doctest
  pass;14 main/merge offscreen UI-region comparisons are pixel-identical.
  Native merged release/installer0.0.7.3 builds on exon-desk; exact hash in
  notes/REPORT.md. Merge left staged/uncommitted; hosted CI and requested
  Tier1 release review remain coordinator-owned.

- W.7b/W.7c and W.6 native qualification (2026-10-18, #105): built in the
  packaging lane's own exon-desk folder after explicit approval. Fixed native
  callback return type, allowed Slint's existing CPU/WARP fallback before any
  threads, and decoded appcast notes as UTF8 on PS5. Non-admin clean install/
  Start launch, .1→.2 upgrade, authenticated rollback, tampered WinSparkle
  refusal, valid update/idle exit, ignored unsigned arguments and uninstall
  pass. Both real movies/custom data and3setup files retained byte-identical;
  full decode after uninstall passes. Ten Windows wording snapshots inspected,
  native CLI help/capacity words pass. Native strict Clippy and14unit+1doctest
  pass; fallback review has no blockers. Evidence/hashes in
  `windows-pkg/scratch/qualified` and REPORT.md. Test key only, no release or
  production certificate used; all qualification tasks stopped and VM off.

- W.6 wording preparation (2026-10-18, #105): target-aware PC/Credential
  Manager/Explorer/Ctrl copy, correct desktop-app privacy instructions,
  isocastd.exe errors, generated CLI/MCP help and fresh Windows Videos default.
  Mac full gates239 unit+1doctest pass;9 snapshot pairs preserve authored UI
  pixels in explicit text regions. Review gaps fixed. Native Windows build/
  snapshots queued through coordinator CI; not yet verified or marked done.

- W.7b preparation (2026-10-18, #105): bounded atomic idle shutdown on
  client/CLI/MCP, incoming-CLI installer interlock, pinned WinSparkle/manual
  checks, authenticated explicit rollback, Windows appcast/signing hooks and
  preservation of adjacent Mac/Windows assets. Source-review blockers fixed;
  strict Mac client/CLI/UI Clippy,15 unit tests+1doctest and release build pass;
  development Mac Updates pixels unchanged. Native verifier rejects tamper/
  wrong key. W.7a hosted build passed but CRLF version parsing prevented any
  package artifact; W.7b replaces that parser and awaits two-build native CI.
  Actual install/update/rollback/uninstall qualification is not yet claimed.

- W.7a1 / W.7a preparation (2026-10-18, #105): `isocast-pkg` installed
  unattended on exon-desk-lan with no host reboot or host-wide setting change.
  Official ISO hash, Windows11 EnterpriseEval/build26300, Secure Boot, ready
  vTPM, standard-user SSH, exact exit-code propagation and checkpoint restore
  pass. Inno7.1.0 compiles the installer source as a non-admin (compiler-only
  check, not an Isocast install); shellcheck, actionlint and native PS5 parse
  pass. Unsigned Actions installer gate awaits delegator CI and payload before
  lifecycle qualification. Authentic updates/rollback and W.6 wording remain
  open. Evidence: `windows-pkg/scratch/REPORT.md`. No GPU/NVENC claim.

- 0.0.8 follow-ups (2026-10-18, #94 #102 #64): HEVC exports with cuts,
  the .isoprj shutdown flush/"file wins" warning and Vorbis were already on
  main and were re-checked (headless Chrome 154 plays EDITED/VERTICAL/crop
  to the end, pre-fix file fails at 0.33 s; trim+shutdown file revision 3 =
  engine 3, one warning; Vorbis lag 0). New: HE-AAC with implicit SBR in
  MKV on time (probe lag 2112 -> 0 samples; engine bursts 145-156 ms ->
  104-111 ms), reserved source ids renamed on collection switch (was
  refused), system audio always leaves out the Isocast app, editor caches
  drop off-timeline files and don't store failed waveform reads, editor
  kept in front when it has the keyboard. Evidence:
  /Volumes/IsocastBuild/followups-008/scratch/REPORT.md.
- T.1b.3 (#110, 2026-10-18): ProRes no longer counts as free H.26x capacity;
  native HW/SW resource cells, CPU/GPU/frame/disk accounting and live-backend
  invalidation share estimate/admission/Optimize. Governor handles secondary
  ProRes resource pressure while preserving main/pins/ISO consent. AWS actual
  HW/SW files, mixed stream, controlled governor/CPU signals, strict18/18,
  ProRes3-pair A/B and full remote gates250 verified. Native SW4444 ISO startup
  remains explicitly unqualified; no numeric loss forecast or exact UHD
  capacity promise. Uncommitted, ready for review; Windows waits for switch.

- T.1b.2/T.2b/T.3b/T.4b (#110, 2026-10-18): main6d6d58fd fast-forwarded;
  hybrid mixed H26x model/coverage fixes, independent hub failures and named
  secondary pins implemented. Real mixed file/live boundaries, refused hub
  isolation/recreation, exact pin-file PTS, persistence and genuine AWS memory
  pressure/recovery verified. Full remote gates243 and strict18/18 once,
  stream A/B3pairs0loss. Fallback review/re-review accepted (regular reviewer
  unavailable>1h); uncommitted, ready for review. Explicit model/simulation
  limits and job IDs: self-tuning-2/scratch/remaining/REPORT.md.
- A.4 / #126 (2026-10-18): media lookahead removes the decode-to-delivery
  scheduling race. QoS alone still failed; async traces caught a decoded
  frame waiting 20 ms for delivery. Five strict record/scenes/takes runs
  passed with zero previous pictures; full strict 26/26 and three-pair
  A/B gate passed. Evidence: media-pacing/scratch/REPORT.md.

- T.1b.1/T.3a/T.4a (#110, 2026-10-18): explicit diagnostic/fit calibration
  coverage, measured multi-canvas HEVC predictions with independent stream
  assumptions, new-only30fps defaults, OS memory signal/bounded competition,
  late-canvas floor and native rollback fixes. M2 real files, four calibrations,
  strict18/18, three A/B pairs, short zero-drop soak/crashes, pins and failure
  injection pass; full remote gates242. Uncommitted, ready for review (service
  unavailable). Full T.1b/T.3/T.4 remain open for model/8GB/Windows scope.
  Evidence: /Volumes/IsocastBuild/self-tuning-2/scratch/REPORT.md.

- T.2 review fixes (2026-10-18, #110, uncommitted): intentional admission
  drops stay attached to their writer/encoder across restoration; delayed
  totals no longer trigger false ISO holds. Refused preview retunes preserve
  the feed and last acknowledged settings, with per-feed error ownership.
  ISO thinning requires explicit opt-in (off by default), surfaced in
  Settings/Optimize/capacity. Healthy/idle sampling avoids expensive work.
  Before035640 reproduced both blockers; after043111 passed pin and natural
  restoration/refusal/lifecycle checks; consent/choice043746+043938 verified.
  Final full gates240 and strict20261018-05573918/18 pass, zero reported loss.
  Neither improvement nor non-inferiority proven for foreign encoder load.

- T.2 qualification/main integration (2026-10-18, #110): interrupted
  jobs162024/195658/195715 never reached hardware. Detached replacements
  all passed; CPU/GPU competitors caused zero losses/steps, while four UHD60
  external demands still caused protected loss on main and governor. Six
  changes without cycling; full preview restored61 s after load ended.
  Merged main257ea057 with --no-commit, retaining governor admission/caps,
  mask presentation times and GPU recovery; platform alias corrected.
  Merged240 tests, strict18/18, seven native contracts and mask/hold/split/
  recovery job20261018-032243 pass. Merge staged; follow-up notes uncommitted.

- T.2 first slice (2026-10-17, #110, uncommitted): runtime ceilings and
  pins/off visible through setup/status/events/UI. Review-driven correction:
  upload losses do not blank ISOs; emergency holds last at most3 seconds,
  then native admission resumes, rather than a second sticky encoder cap.
  Exonbook:233 tests, strict e2e18/18 zero drops; ten-minute fitting
  HD30 program/twoISOs/stream/monitor zero steps and zero losses; real CBR
  6000→1500→6000 with replay decode intact; encoder-contention A/B and
  pin/split/off checks. AWS jobs195658/195715 wait behind the release soak;
  T.2 is not yet qualified/complete. Full evidence is in scratch/REPORT.md.

- T.1a F4/F8 (2026-10-17, #110): file checking never starts a daemon and
  says capacity is not checked if unavailable. Automatic calibration waits
  for preview to end; an incoming preview preempts it, but manual calibration
  remains available. Exonbook: missing/stale socket, 66 preview observations,
  manual/automatic distinction, 33 ms preemption, 90/90-frame intact recording;
  full remote gates 233 pass. T.2's M2 baseline was blocked by the release
  soak; the user subsequently approved exonbook first with AWS still queued.

- W.3 GPU lifetime (2026-10-18, #105): real cache-order retention bug,
  not premature pixel reuse. Pure CoreVideo control: cache-first teardown
  loses one pool slot/round, texture-first returns all32 even with identical
  autorelease scopes. Uncached Metal textures now retain their cache through
  GPU retirement and release it last, with a retirement autorelease scope.
  Unchanged strict GPU contract passes;256 cross-thread BGRA/NV12 import/
  target cycles fully recycle. Cached paths unchanged; three A/B CPU pairs
  measure+20–29ns per uncached plane, cached medians within 0.3%. Candidate
  core/media7/7, hosted e2e19/19 and Mac gates 233 unit + 1 doctest pass. 36 ordinary
  filtered-ISO stop cycles pass on both baseline and candidate; no claim that
  ordinary recording already failed. Separate Tier1 change before adapter work.

- W.5p#1a/#6a/#7a/#11a (2026-10-18, #105, uncommitted): native API slices
  readyforCI with explicit remaining gates. Six unmodified per-preset
  measurements feed reviewed budget math; actual API cases655fa805pass.
  Borderless DDA pixel/cursor/lifetime/final-update proofs pass; bounded
  timer correction improves pending age/CPU but does not beat matchedWGC.
  MediaClock opt-in/default/Stop behavior and real48kMMCSS contention tested.
  Native/portable checks pass as recorded; optional final consolidated
  rerun62688d6d did not execute because writer heldGPUlock. Adapter/split/
  cache integration, Win11power/sub1ms/fullpipeline targets remain open.
  All own jobs cleaned up; shared local volume below20GiB, no local builds.

- W.5p#3/#12 (2026-10-18, #105, uncommitted): sharedNVENCcompletionworker
  andevent-onlyWASAPI are readyforCI. Deterministicretirementrace fixedand
  re-reviewed;12sessionsoneworker,49files1470frames decoded, matchedWPR
  10800/10800eachcodec/noMisses. WASAPIidle4.02wakeups/s→0 withregistered
  session-disconnectionwake;24devicecyclesstable175handles. NativeClippy/
  165unit+remainingdocpass, fullportable233unit+1docpass. #1budget,#7timer,
  #11scheduling APIs qualified as primitives; adapter/Win11/precision gates
  remain, #6DDAstillunderreview. No commits orunownedprobehunksaltered.

- W integration (2026-10-18, #105 #28 #110 #114): main49e7732 merged into
  agent/windows with profile-aware capacity checks retained, standalone
  record explicitly starting a new project, workspace lock versions0.0.7,
  and unsupported non-mac calibration skipped before any worker launch.
  Merged full hosted e2e19/19 passes; real record Ctrl-C exits0 with3complete
  H.264 files, no drops, private workspace removed and sentinel target intact.
  Its .isoprj opens via the real editor's project-open path after private
  timeline cleanup, with decoded preview and3file lanes. Mac full gates
  pass233unit+1doctest; Windows scoped strict clippy and190tests pass.
  GPU-lifetime task remains separate; no such fix is included in the merge.

- W.3a (2026-10-17, #105 #28): shared development-only Platform contract
  workers and bounded runner implemented; new files explicitly approved.
  Same generic clock/pool/GPU/writer/capture checks, real decode/A-V and
  commit-kill-repair verification,12 falsification/selftests on both hosts.
  Mac/Windows strict scoped clippy and build pass; existing Mac full gates
  pass232unit+1doctest. Windows stub/core passes; native/full is explicitly
  unavailable. Mac full suite has7passes/1GPU failure: dropping the importer
  before submitted work completes retains one pool slot per round. Strict
  test preserved and independently reviewed, no production backend changes.
  W.3b and whole W.3 remain blocked on Mac lifetime investigation and the
  capture lane's Windows Platform/FileWriter adapter. CI additions run only
  the supported core profiles, not a false full-native qualification.
- W.5s1/W.5s2 (2026-10-19, uncommitted): native AAC live fill fixed the strict
  stream bitrate oracle; 24 cases preserve payloads and decoded PCM exactly.
  Main/portrait oracles, the 615-second program + two ISOs + two destinations,
  receiver kill/reconnect, positive/negative TLS trust, and governor bitrate
  steps were verified on exon-desk under the GPU lock. Healthy outputs had zero
  drops; intentional congestion loss remained visible while recordings stayed
  complete. ConPTY verified hidden CLI input, restoration and redaction. Native
  Clippy/build and 250 portable tests passed. Credential desktop roundtrip,
  shared admission and live GPU scaling remain open; this is not full Windows
  streaming completion. Exact receipts and reviews are in capture REPORT.
- W.5d2c (2026-10-19, verification only): ETW GeneralProfile+GPU, CSwitch-only
  timing, and unprofiled production60s takes pass for all3cameras:18files,
  64,800decodedframes,0file drops. Historical3/1drop events were matching
  startupPTS gaps inprogram/ISO, not a fractional-rate pattern, but their
  exactcause did not reproduce. Measured MFJPEGdecode/upload and delivered
  sourcecadence separately; no speculative scheduling/writer change. Temporary
  instrumentation removed, production source byte-identical tod18f2515 and
  native daemon rebuilt. Root-cause checkbox remains open; captureREPORT has
  receipts, failed analysis attempts, clock counters and interpretation limits.
- W.5d2b (2026-10-19, #105, uncommitted): common camera formats now negotiate
  by size/nominal rate/cost; absent SDR metadata uses explicit defaults.
  NV12 stays native, packed YUV/RGB converts on GPU after one upload, and MF
  MJPEG decoding replaces WIC. Actual10s program+ISO recordings for all three
  exposed cameras fully decode with manifest formats; virtual600, CamLink597,
  4K S599frames/file. Preserve placeholder/black-feed and pacing-drop qualifiers.
  Actual GPU/CPU raw-format captures, exact/tolerance pixel checks, MFJPEG
  full-range oracle, native build/strictClippy/90tests, and portable250tests
  pass. Shared bridge untouched. Final duplicate take blocked by writer GPU
  lock; no claim of a final-snapshot six-file rerun.
- W.5d2 (2026-10-18, #105, uncommitted shared camera slice): realCamLink4K
  concurrent FrameServer MJPG1080p60 capture preserves bridge's mode/process;
  explicit CPUJPEG decode/GPUupload path, nativeNV12 staysGPU.30QPC-matched
  frames/248,832,000exactbytes with swap/double-line controls;180frames fully
  rendered/encoded/decoded,12reopens and safe reader-loss/stopped/reopen pass.
  Native165tests/strictClippy, fullportable233unit+1doc and fallback review
  pass. Physical unplug/engineISO integration remain separate gates.

- W.5d2 (2026-10-18, #105, uncommitted): default GPU weave honors sample
  flags without changing NV12 field rows; true multi-planar GPU import and
  renderer views, bounded camera metadata reader and optional manifest map.
  Real180-frame capture/render/native encode passes D3D12 GPU validation;
  native strict Clippy/165tests and full m2-b portable gates233unit+1doctest
  pass. Physical nonuniform-content proof is NOT claimed from uniform black;
  native Platform/ISO integration remains writer-owned. No recording-stop/OBS
  performance claim from standalone capture Drop or GPU-validation timings.

- W.5d2a (2026-10-18, #105, native qualification): inherited NV12-input slice
  rechecked on exon-desk under bounded GPU lock: exact345,600component copies/
  expansion, array/crop/pressure/lease checks;7files210frames full-decode,
  BGRA regression37files1110frames full-decode. Windows fmt/strict all-target
  Clippy/81engine tests pass. New portable gates exit69 at38/39/39GiB remote
  disk floors; reviewer endpoint unavailable. No camera success claimed:
  actual public API still rejects contradictory interlaced samples. No
  production source changed during this verification takeover; no commits.

- W.5c4a (2026-10-18, #105, ready for CI): replace exact softwareMF quality
  rejection with bounded nearest actual-value negotiation AFTER media types;
  SetValue alone only echoes the request. Preserve native choice on ties,
  re-pin/recheck and expose requested+actual metadata; factory reports actual.
  All101software inputs and5production presets at64×48 pass, High70→72;
  NVIDIA3codecs×3qualities unchanged.115files/711decodedframes, stable454handles,
  native strict clippy/81tests/read-only fallback review pass. Uncommitted.
  AAC/WASAPI already committed in59f36cd4; no audio internals changed.

- W.5c4 (2026-10-18, #105, ready for CI): recording factory now selects native
  CONSTQP/MFQuality, preserving codec/depth and live bitrate behavior. New
  nonterminal barrier individually acknowledges accepted output; failed
  callback keeps its packet, segment reuse forcesIDR without resettingPTS.
  Native finish no longer discards valid prefix on ordinary errors.41native
  files decode1230/1230frames; ordered quality, duration, reuse and liveCBR
  checks pass. NVIDIA MF all3codecs qualified at30/70/100; Microsoft software
  rounded quality30/70 rejected,100verified. Native strict clippy/81engine
  tests/review pass. Full portable gates externally blocked below40GB on all
  3Mac hosts. Uncommitted; no adapter/daemon selection or full-engine/OBS claim.

- W.5d1 (2026-10-18, #105): native MF AAC and WASAPI mic/endpoint loopback
  implemented, uncommitted for coordinator. Real sample-zero loss requires
  deliberate1024-frame pre-roll; native24k capture positions require SRC-aware
  clock accounting.56AAC edgecases, real24k/48k capture→AAC→mux/decode, atomic
  admission, nonterminal continuation and24reopens pass; no growing handles.
  Explicit stall reports48k loss or stops uncertain24k conversion. WPT26100
  trace0lostevents, isolated steady2.7%one-core including tone playback; not
  full-engine/OBS. Native strict clippy/build pass; existing scoped test
  commands pass with0tests; read-only fallback review clears fixes. Ready for
  CI with disclosed portable-gate blocker: two attempts exit69 because all
  Mac build hosts are below40GB disk floor. No bypass or adapter/daemon
  selection; recording CQ/QP/barriers follow as a separate slice.

- Windows CI memory controls (2026-10-18, #105, ready for CI): merged
  main49a98729 without commit, retaining W.3a and native Windows package.
  Stable Cargo timings/PowerShell samples isolate the~69MB generated UI:
  one compiler peaks~13.5GiB WS/~15GiB private, before the much smaller linker.
  Native-job-only jobs1/debug-info0 keeps bin/test compilers from overlapping;
  assertions, overflow checks, compiled Slint and every target remain. Rejected
  opt1/opt-s/CGU overrides as no useful memory improvement. Exact all-targets
  nine-package build passes20m19s; strict contract clippy,12verifier tests,
  stub clock/pool contracts and13UI tests pass. Full remote Mac gates pass
  233unit+1doctest. Hosted GitHub rerun still required; no native Windows
  Platform qualification claimed. Standing full-engine OBS CPU/memory/start/
  stop/drop targets recorded above for the remaining W.5 integration.

- Windows CI investigation (2026-10-18, #105, blocked): Actions39092440023
  confirms isocast-ui LLVM OOM, not capture/encoder compile failure. Merged
  W.3a c668d98b without commit, retaining native Windows package and all
  contract checks. Three bounded12GiB native attempts failed even with one
  compiler/no debug symbols:8CGUs,256CGUs,256CGUs+UIopt-level1. Stopped at
  the debugging limit; unqualified workflow edits reverted. Need actual
  uncapped per-UI compiler peak/phase diagnosis, not more guessed profile
  changes. Evidence and coordinator question in capture REPORT.md/notes.

- W.5c3 (2026-10-17, #105, ready for CI): HEVC Main/Main10 now encodes
  through NVENC/MF from GPU BGRA8 or realRGB10; no codec/depth downgrade.
  hvcC is SPS-derived, hvc1 removes parameter sets and rejects changes after
  configuration. Final11-file matrix fully decodes1980/1980 frames at1080p60/
  4K60 with zero observed pool/rate/callback/encoder drops; mux/decode keys
  match[0,60,90,150]. RGB10 pool/DX12 preserves1024 levels; NVENC/MF decode
  877limitedY levels with correct endpoints/chroma. Replaced incorrect NVIDIA
  P010 video-processor output with explicit GPU quantization; four simultaneous
  colour encoders remain exact. 20MFMain10 opens keep521handles; bounded
  overload and12-session refusal/recovery pass, no software HEVC installed.
  Native strict clippy/81 engine tests, full Mac gates232tests+1doctest and
  review/re-review pass. No writer edits or Platform selection. W.5c2 Actions
  portable jobs pass but UI compile hit LLVM OOM; coordinator owns CI action.
  Camera/WASAPI/nativeAAC and codec-safe Platform/writer/priority integration
  follow, retaining WGC handle and other-GPU qualification gates.

- W.5c2 (2026-10-17, #105, ready for CI): NVENC-first H.264 selection now
  falls through same-adapter hardware MF to explicitly labelled software MF.
  Hardware uses GPU BGRA→NV12 conversion and tracked-sample private slots;
  software alone downloads pixels. Final eight low-latency files fully decode
  2400/2400 frames at1080p60/4K60 (software1080p), zero counted losses;
  redYUV63/102/240, keyframes, invalid-PTS recovery and bounded overload pass.
  12 occupied NVENCs force software60/60; old session survives and capacity
  returns. Fixed per-open MF runtime14-handle retention and async-credit lag;
  16 real opens each provider keep stable464/481 handles after warmup. Native
  strict clippy/81 engine tests and full remote Mac gates232tests+1doctest
  pass; independent review/re-review approve. Detailed evidence in capture
  lane REPORT.md. W.5c1 Actions39086165352 green; isolated OBS/credential
  cleaned. HEVC/10-bit, camera/WASAPI, nativeAAC and Platform/priority wiring
  follow; Intel/AMD and GPU-less software remain unqualified.

- W.5c1 (2026-10-17, #105, ready for CI): native NVENC now
  consumes GPU-resident frames and produces aligned H.264 through the W.2
  packet mux. Driver617.42 admits12 sessions, refuses13 with NVENC21;
  existing encoding survives and capacity returns after teardown. Real red
  patch decodes709-limited YUV63/103/240. Three interleaved8s OBS pairs per
  resolution produce12 decodable files, zero reported losses; scoped results
  and limitations in capture REPORT. Native strict clippy/81 tests and full
  remote Mac gates232 tests+1 doctest pass; two read-only reviews clear.
  Writer d54df54 merged without commit, staged separately from this feature;
  final integrated1080p/4K records fully decode120frames each. Coordinator
  owns merge/feature commits and CI. Isolated OBS cleanup needs a human exit
  after Cua refused process termination; no recording remains active.

- W.5b (2026-10-17, #105, ready for CI): native read-only
  D3D11→wgpu DX12 import now renders captured textures in place. Shared
  handle/fence lifetime, GPU completion retention and cache reuse verified
  on RTX5070; no shared engine/macOS edits. W.5a Actions39074561618 confirmed
  green. Native build, strict clippy and81 engine tests pass; full remote
  Mac gates232 unit tests+1 doctest pass. Independent review/re-review clear
  for this unselected substrate; RPC handle retention gates later Platform
  selection. Coordinator commits/CI next, no commits made by this lane.
  Writer e38160a integrated without commit, staged separately; combined
  capture/mux Windows clippy and full remote Mac gates also pass.

- W.5a (2026-10-17, #105, native qualification): separate Windows capture
  lane/root and shared GPU lock; new crate's WGC/QPC/D3D11 pool works on RTX
  5070. Real frames, known pixels, bounded retention/resize, simultaneous
  capture and closed-window callbacks checked. Windows strict clippy and
  release build pass; full remote Mac gates pass. Work remains uncommitted
  for coordinator CI. No daemon platform switch, writer edits, CPU capture
  fallback or recording/OBS-parity claim; W.4/W.5 remain open.

- W integration (2026-10-18, #105 #28 #110 #114): main49e7732 merged into
  agent/windows with profile-aware capacity checks retained, standalone
  record explicitly starting a new project, workspace lock versions0.0.7,
  and unsupported non-mac calibration skipped before any worker launch.
  Merged full hosted e2e19/19 passes; real record Ctrl-C exits0 with3complete
  H.264 files, no drops, private workspace removed and sentinel target intact.
  Its .isoprj opens via the real editor's project-open path after private
  timeline cleanup, with decoded preview and3file lanes. Mac full gates
  pass233unit+1doctest; Windows scoped strict clippy and190tests pass.
  GPU-lifetime task remains separate; no such fix is included in the merge.

- W.3a (2026-10-17, #105 #28): shared development-only Platform contract
  workers and bounded runner implemented; new files explicitly approved.
  Same generic clock/pool/GPU/writer/capture checks, real decode/A-V and
  commit-kill-repair verification,12 falsification/selftests on both hosts.
  Mac/Windows strict scoped clippy and build pass; existing Mac full gates
  pass232unit+1doctest. Windows stub/core passes; native/full is explicitly
  unavailable. Mac full suite has7passes/1GPU failure: dropping the importer
  before submitted work completes retains one pool slot per round. Strict
  test preserved and independently reviewed, no production backend changes.
  W.3b and whole W.3 remain blocked on Mac lifetime investigation and the
  capture lane's Windows Platform/FileWriter adapter. CI additions run only
  the supported core profiles, not a false full-native qualification.

- W.2b/W.2c (2026-10-17, #105 #28): Windows startup now repairs encoded
  takes and writes truthful manifests through one pinned folder lease.
  Fixed stale999999-frame/999s claims in a real daemon reproduction; counts
  now match verified packets. Historical finalized counts survive readonly,
  held, foreign and missing files; unknown fields/pauses retained. Native
  NtSetInformationFile replaces the Win32 rename wrapper that returned87 on
  Win10. Eight writer-kill/daemon-relaunch cases preserve all published packet
  bytes (four pauses exactly3.2s); seven daemon-recovery kills and four
  metadata-publication kills converge safely.12 manifest edge cases,10020
  unrelated files and concurrent reads pass; second launch is byte-idempotent.
  Record-start scans100 historical manifests in9ms instead of234ms in a
  bounded fixture, by leaving their media to launch recovery. Windows strict
  clippy and105 existing tests pass; Mac full gates232 tests+doctest pass.
  Mac writer/encoder paths unchanged. W.3 contracts follow this storage slice.

- W.2a (2026-10-17, #105 #28): new encoder-neutral isocast-mux accepts
  compressed access units, not CPU pixels. Native H.264/HEVC MOV/MP4 and
  Annex B/length-prefix outputs preserve packet payloads and timing; Main10
  also decodes. 14 process kills: 13 recoverable movies plus one explicit
  no-media early start; active loss at most 0.495 s, four pauses recover
  their exact 3.2 s promise. Fixed a reproduced interrupted-repair bug where
  an appended zero byte completed a torn commit; one-byte tail sealing makes
  retries stable. Alias/pinned-directory, 1.5M-sample admission, jitter/gap,
  empty/unequal-track and torn-header checks pass. Windows-created fragments
  repair on APFS byte-identically to normal finish. Full Mac gates 232 unit
  + 1 doctest pass (1 existing hardware ignore); Mac writer paths unchanged.
  New crate added to portable CI; W.2b next-launch manifests remain next.

- W.1e (2026-10-17, #105 #28): Windows UI builds alongside daemon/CLI;
  fixed the observed Slint build-script 1 MiB stack overflow using an 8 MiB
  Windows-only worker. Mac compiler/rendering paths remain unchanged.
  Native release build, strict UI/client/IPC clippy, 12 UI tests + 1 doctest
  pass; Mac full gates 232 unit + 1 doc pass (1 existing hardware ignore).
  RTX 5070/DX12 renders real first-launch UI to 1280x800 PNG. Actual process
  counters replace Windows zero stubs. SID namespaces and 2000 empty polls,
  full 64 KiB/partial writes, busy mode transitions, EOF and actual shared
  parser deferral/partial-line recovery pass on both pipe endpoints.
  Main 24bea09 RC merge is separately staged; feature uncommitted for
  coordinator. W.2 writer next; no Windows capture/recording/shared-surface
  or full visual/DPI parity claim.

- W.1d (2026-10-17, #105 #28): `record` now uses a private, parent-owned
  daemon; the CLI no longer depends on MacPlatform. Same report/exit behavior,
  killed-owner finalization, concurrent isolated runs and untouched default
  daemon verified on M2-a. Twenty auto-stop/SIGINT races return 20 reports;
  a forced cleanup failure still prints the completed report; private
  engines register no global hotkeys, old daemons are refused before Start.
  Full remote gates: 232 unit + 1 doctest pass, 1 existing hardware ignore;
  Windows: 190 unit tests and strict scoped clippy pass; strict e2e18/18.
  Three interleaved 1080p60 program+2ISO pairs: CPU44.8->44.2%, all18 files
  whole, no video/audio loss; warm start callback264->308ms, RSS128->173MiB.
  Startup retry/exit waiting reduced new callback353->308ms and setupcheck
  155->70ms (base40ms), capacity JSON identical. Main a9b661f merge staged
  separately; feature unstaged for coordinator. W.1c Actions39060641784 green.
- C.6 #114 (2026-10-17): recordings start a new project each unless a
  timeline is chosen as the record target; the app shows and changes the
  choice next to Start Recording, in the status bar and in the editor.
  Verified with e2e `timeline` (three default recordings → three projects
  and timelines; a chosen timeline gets take 2 in its folder; split and
  retake stay together; an old current.json migrated), `projects`, `edit`,
  offscreen snapshots and the layout audit.

- S.3 #111 fix round 1 (2026-10-17): keep scene compositors across reuse,
  clear an old scene's reuse when its publisher switches, debounce raster
  rebuilds, skip undersized one-off scene pictures, and back off lost ISO
  replacements. Reviewer's cut probe max/p99: 176/149 ms → 65/61 ms (main
  67/63); on-air edits 41/40 ms (main 66/60). Six injected device losses:
  ISO stage opens spaced 1.001–1.015 s, every tick counted, next take whole.
  Main 215db9e merged without committing; strict e2e 18/18 and full gates
  pass. One-pair multiview GPU 545→262 ms/s, footprint 363→217 MiB, zero
  losses. Fix-only re-review: perf-2/scratch/REPORT.md, "Fix round 1".

- S.3 #111 split (2026-10-17): removed and preserved #27's entire draft;
  integrated main a9b661ff. Reduced-tree full ISO stress passes with zero
  program/stream/audio losses (18 files), strict e2e 18/18 and workspace
  gates pass. GPU loss/poll panic, cached snapshots, ISO recovery and camera
  pacing rechecked. One M2-a multiview A/B pair: GPU 536.3→302.9 ms/s,
  footprint 416.3→204.1 MiB, CPU 65.8→64.8%, zero recording losses.
  Initial stress noise also reproduced on baseline; both preserved.
  Uncommitted for coordinator Tier-1 review: perf-2/scratch/REPORT.md.
- T.1a fix round 1 (2026-10-17, #110, awaiting re-review): isolated engines
  never auto-calibrate by default and harnesses force off; snapshot.sh
  rendered after a 10 s wait with no calibration child. Failed-build retry
  suppression, dead-owner cleanup, fit guards, unified capacity checks and
  exact Optimize-pass preservation verified on M2-b. Merged main a9b661f;
  remote gates 233 pass, one strict e2e run 18/18 pass. Evidence:
  `/Volumes/IsocastBuild/machine-profile/scratch/REPORT.md`, Fix round 1.

- T.1a (2026-10-17, #110, awaiting Tier-1 review): measured machine profile,
  bounded idle-only calibration and shared API/CLI/MCP/Settings; covered HEVC
  estimates/Optimize use local numbers. AWS M2s/exonbook media checks, race/
  refusal/deadline/fallback, remote workspace gates (230 pass) and strict
  e2e 18/18 pass. T.1b remains open; REPORT.md states coverage/accuracy limits.
  T.4 contention prototype preserves failures (86.1%/96.2% drops vs clean
  controls), not a governor or a true CPU-core-limit simulator.

- #108/#109 (2026-10-17): the first layout audit's 11 issues fixed (narrow
  Controls/source toolbar/mixer/vertical head adapt, source child names
  use the empty ISO lane, ruler labels skip the playhead chip and the
  edge, wider encoder popup, middle-elided paths, multiview outer margin);
  re-audit of 482 pictures: none of the 11 left except an ellipsised child
  name at the 900px minimum; visual diff changes only at 900 wide and the
  encoder row. A scene without a vertical arrangement is now letterboxed
  (fitted, centred): real M2-a recording, cropdetect 1080:608:0:656 on all
  three vertical takes; e2e vertical PASS, 0/578 drops.
- W.1c (2026-10-17, #105 #28): native Windows loop syncs tracked worktree
  bytes to exon-desk with warm caches and returns exit codes/logs/binaries.
  Owner-only local pipes, kernel peer identity, overlapped cancellation and
  detached handle-allowlisted launch work; daemon/CLI build and clippy pass.
  Windows: 104 existing unit tests pass (one Unix-root fixture corrected),
  40 concurrent clients, watch/restart/persistence and MCP's 100 tools/status;
  recording fails explicitly without media. Mac full gates: 229 unit tests
  plus 1 doctest, 1 existing hardware test ignored; strict e2e 18/18.
  Three interleaved M2-a pairs versus W.1b + main f21a4c0: CPU 41.6 -> 41.7%,
  instructions 488.7 -> 488.9 M/s, 18/18 complete files, no video/audio drops.
  W.1d isolated-daemon `record` and W.1e UI remain. Coordinator owns pending
  main merge, feature commit and the next Actions daemon/CLI build.

- W.1a qualification / W.1b (2026-10-17, #105 #28): run 39047095306
  builds the engine on Windows and passes both portable jobs. Next slice
  selects the daemon platform once and routes media helpers through the
  trait; a bounded CPU-synthetic stub explicitly refuses native services.
  Windows daemon IPC/process hooks remain W.1c; no runnable Windows
  recorder is claimed. Main c8cf497's release-only changes integrated
  without a merge commit; coordinator owns the staged merge and slice.
  Remote gates pass (228 unit tests + 1 doctest, 1 hardware test ignored),
  stub clippy/build and direct CPU-frame probe pass; strict M2-a e2e 18/18.
  Three interleaved pairs: CPU 41.5% both, 18/18 complete files and no
  drops; RSS 153 -> 169 MB, not claimed identical. Actions stub build next.

- W.0 qualification / W.1a start (2026-10-17, #105 #28): delegator run
  39043755843 passes Windows (4m7s) and Ubuntu (1m32s), all portable tests;
  W.0 merged by the coordinator in main 603c3ac. Initially integrated
  a8276a1; fast-forwarded to 603c3ac (including security-103) before handoff.
  W.1 starts with the shared engine build boundary; daemon/plugin/UI seams
  follow separately. Five strict smoke scenarios pass; three A/B pairs on
  M2-a: 18/18 complete files, zero drops, CPU 42.0 -> 41.5%, instructions
  489.4 -> 488.8 M/s (before the security-103 integration).
  After integration, remote daemon/CLI/UI build and fmt/clippy/228 unit
  tests + 1 doctest pass (1 existing hardware test ignored). Windows engine
  build-only qualification remains the next Actions run.

- 8.2l / #106 (2026-10-17): release notes from docs/releases/X.Y.Z.md
  (0.0.5 and 0.0.6 added; body + Full Changelog reproduces both published
  bodies byte for byte); preflight fails on a missing/empty/Changelog-only
  file (stubbed gh); Refresh appcast notes proved with a throwaway key
  against the published 0.0.6 DMG re-keyed: same item, enclosure and
  signature, notes embedded, only appcast.xml uploaded; tampered feeds,
  drafts, wrong version/key/checksum and extra assets refused.
  actionlint and shellcheck clean.
- S.3 #26/#29 implementation (2026-10-17): one process GPU, completion
  hand-offs and pre-conversion ISO admission; final remote gates clean,
  strict e2e 17/17, cycles/disk/ISO stress and native readbacks pass. Final
  three-pair multiwindow A/B: daemon 71.6→66.2% CPU, GPU 580.7→483.3 ms/s,
  0 program/ISO drops in 3×64 s takes; live footprint 305.6→438.1 MiB.
  Flagship and dual CPU roughly flat/slightly higher, all protected outputs
  and audio whole. Tier-1 review/commit remain with the coordinator; see
  `/Volumes/IsocastBuild/perf-gpu/scratch/REPORT.md`.

- #96 / 0.0.7 (2026-10-17): canvas-scoped stream failures, atomic poison
  recovery state, quiet ended-stream Stop, missing-tail frame counts,
  vertical replay and virtual camera. M2-a: fault/poison healthy outputs
  0 loss; 4 replay saves decode/sync; strict e2e 17 existing scenarios pass,
  then the corrected vertical fixture passes (6 movies/578 frames plus
  287 stream frames, 0 drops). M2-b: portrait camera IOSurface probe
  1080×1920/24 fps, correct canvas/color and failure; default replay start
  mid-take keeps both 8 s/240-frame files and the replay loss-free.
  Remote gates 228 pass/1 existing ignored; final warm-up hook has targeted
  engine/daemon clippy/tests and full fmt. Working diff and review handoff:
  /Volumes/IsocastBuild/vertical-007/scratch/REPORT.md; live linking deferred.
- 0.0.7 follow-ups (2026-10-17, #102 #95 #74 #64 #100 #85): .isoprj
  writer warns once per folder when the file's edits are newer, re-checks
  the recording gate right before writing, flushes on graceful shutdown
  (trim then shutdown at once: file revision 3 = engine 3), probes cold
  folders at start; relink-by-folder moves only offline takes; mid-
  recording open says the take joins when it stops. obs-websocket
  GetMonitorList, OpenVideoMixProjector, OpenSourceProjector (VM: 11
  requests, errors 400/402/600 as OBS). A full-screen projector whose
  display is absent at start waits for it and reopens when it attaches; a
  window on a gone display drops its frame. HE-AAC and HE-AAC v2 in MKV on
  time (cross-correlated lag 0 at 48 and 44.1 kHz, was 44-48 ms; v2 failed
  '!dat' before). hw-batch and e2e.py robustness (#100). Full hosted e2e
  17/17 in the VM.
- W.0 CI round 3 (2026-10-17, #105): run 39042460186 passes diagnostics
  and obs-websocket on Windows; protocol 80/81 (test used a Unix-only
  absolute root). Project-file tests now use native roots and exercise the
  actual file boundary; Windows writes `/`-separated relative take paths,
  leaving absolute references and macOS bytes alone. CI keeps going through
  clippy/tests. Audit also fixes Windows home/data roots, diagnostic path
  redaction, timeline randomness, plugin rooted paths and OBS wipe paths.
  Remote fmt/clippy, 228 unit tests + 1 doctest (1 hardware test ignored),
  diagnostics' separate test and Actionlint pass; Unix project bytes match.
  Next hosted run required; W.0 remains unchecked.

- W.0 CI round 2 (2026-10-17, #105): hosted run 39042059588 passes
  Windows build/clippy and diagnostics' test; three obs-websocket tests fail
  because production password/salt generation opens `/dev/urandom` on
  Windows. Protocol tests were not reached; Ubuntu passes throughout.
  Use the existing getrandom crate on non-Unix targets only, retaining the
  Unix random source and all tests. Remote macOS fmt/clippy and 227 unit
  tests + 1 doctest pass (1 hardware test ignored); locked Windows dependency
  graph resolves. Next hosted run still required.

- W.0 CI round 1 (2026-10-17, #105): hosted run 39041442122 builds on
  Windows and Ubuntu; Ubuntu clippy/tests pass. Windows clippy found three
  Unix-only diagnostics declarations compiled without their callers. Gate
  those imports/helpers with `cfg(unix)`, leaving macOS code unchanged.
  Remote fmt/clippy and 228 unit tests + 1 doctest pass (1 hardware test
  ignored; diagnostics' test run separately).
  Windows tests were skipped; W.0 remains open pending the next hosted run.

- W.0 preparation (2026-10-17, #105): added the Windows/Ubuntu portable
  workflow and branch-restricted snapshot runner; shared Rust code is
  unchanged. Remote Bash/ShellCheck/Actionlint, snapshot scan and argument
  refusals pass; remote gates pass (227 unit tests + 1 doctest, 1 hardware
  test ignored), plus diagnostics' 1 test. Real Actions qualification remains
  pending the delegator's commit/push (this implementer cannot commit or
  push). W.0 stays unchecked.

- A.4 #90 fix round 1 (2026-10-16): removed the early ISO hold and share
  cap after review found lost ISO content without protected-output gain.
  Three CPU pairs: ISO medians 30.54 -> 31.57 fps (base range 30.39–35.36),
  all protected video/audio drops zero. Three foreign-encoder pairs warn
  210–428 ms before sampled stream loss. Four CPU episodes produce four
  status episodes but only two events 186 s apart; real app recording,
  paused and reconnecting lines retain their state and warning. Full strict
  e2e 17/17 and remote fmt/clippy/227 tests pass on main 12c6bad integration.

- A.4 #90 (2026-10-16, partial): early external-pressure warnings verified
  on both AWS Macs, one event per episode, real status/CLI/app visibility;
  30 min without a headroom warning, 17/17 strict e2e, 221 workspace tests.
  Protected-loss reduction is not established; normal soak loses one frame
  on program and stream. Tier-1 review and stronger protection remain open.

- C.4 / #88 (2026-10-16): project files (`.isoprj`) in every project
  folder open in the editor from the Finder, File › Open/Open Recent, the
  CLI and MCP; copies open with the same edits; offline takes relink; the
  editor can open after a recording stops. Verified with isolated engines
  in the VM (copy opened by a second engine: identical clips; 25 kill -9s
  during edits: file always valid; kill -9 while recording: playable
  files), offscreen snapshots, and GUI checks on isocast-m2-a.
- #96 vertical UI (2026-10-16): Settings › Stream "Streams of: Main Canvas /
  Vertical Canvas" (keys under `canvas:vertical:…`), Start Streaming starts
  both with per-canvas status-bar words, vertical projectors (`canvas:
  vertical`, engine `vertical-scene:ID` feeds) and a multiview VERT tile,
  vertical monitor right-click Open Projector, Follow Main Scenes disabled
  with a reason while recording, `engine.vertical.stopped` with a toast,
  status words and Restart Engine. Offscreen snapshots of each view, visual
  diff 0 px on all 15 standard views, hosted e2e 17/17; real window on an M2:
  vertical-ui/scratch/REPORT.md.

- #96 vertical stop (2026-10-16): a vertical compositor failure fails the
  `vertical:` stream slots ("the vertical canvas stopped: …") and leaves
  main's alone; `stream_start --canvas vertical` is refused while
  `engine.vertical.stopped`; a new vertical compositor (canvas off/on,
  resize) clears it unless the canvas's scene state is broken; the app
  stays quiet when every vertical destination is off. Fault hook on
  isocast-m2-a with two local receivers: vertical receiver ends at the
  fault, main stream 1088 frames 0 drops/0 audio dropouts, largest gap
  34 ms; main take 26.0 s 0 drops; off/on → streams 173 frames again;
  scene-poison stays stopped. vertical-stop/scratch/REPORT.md.

- #96 engine follow-ups (2026-10-16): main-over-vertical encoder priority,
  local vertical failure/on-air cleanup, per-canvas live estimates and measured
  M2 stream cost. Compositor fault injection found/fixed timed failed-file
  duration; real movies decode with aligned A/V. Three overloads keep main
  loss-free; vertical cycles/disk and strict e2e pass. Final evidence/review
  status: vertical-followups/scratch/REPORT.md; remaining #96 UI/linking deferred.

- G.7 / #86 model (2026-10-16): optional vertical canvas and per-scene
  arrangements, shared sources/mix, validation and collection/profile
  round-trip. Remote 77 protocol tests passed; main-integrated engine/protocol
  clippy passed; live offscreen apply and vertical-only editing verified.
  The full feature remains unqualified, not resolved.
- G.7 / #86 qualification (2026-10-16): main 79bf8c7 integrated; 214 workspace
  tests/doctests and strict AWS e2e 17/17 pass. Three 14 s dual-program + two
  ISO runs: zero loss, 12 files decode, sync ≤0.021 ms. Full HD60 + portrait60
  ProRes programs and two H.264 streams: both protected programs/streams
  loss-free; shared HEVC ISOs shed at the no-margin M2 load. CPU increment
  9.35 points of one core. Offscreen vertical drag/lock/removal preserve main;
  CLI/MCP and link/restart verified. Review/commits remain coordinator-owned;
  evidence in vertical-canvas/scratch/REPORT.md.
- #92 (2026-10-16): isocastd raises its open-file soft limit at launch
  (256 -> min(hard, 10240); status.diagnostics.open_files); encoder creation
  that runs out of descriptors says "too many open files" (errno EMFILE
  after VideoToolbox's -12908, or within 8 of the limit) instead of "no
  hardware encoder is free"; plugin sandboxes cover a recordings folder
  before it is made, so the first take into it no longer restarts every
  plugin. m2-b (hw-batch): stress clients main 2dccb6f FAIL x2 (encoder
  error), branch PASS x4 (0 plugin restarts; main with ulimit 4096 still
  restarts all 16); forced hard limit 64: main misleading, branch truthful;
  strict e2e 17/17.

- A.4 / #62 (2026-10-16): cold stream start under heavy M2 load no longer
  loses protected frames on main (79bf8c7; #68's placed-and-parked encoder,
  #77, #79). No code change. 4K60 + 1080p60 HEVC ISOs, 1080p60 program and
  stream: 10 cold starts each of record-first, stream-first and both at once
  on m2-a (22f7574) and m2-b (79bf8c7), 60 fresh engines, program/stream/audio
  0/0/0, ISOs shedding; 180 files ffprobe + full decode. 24 min hardware
  soak (m2-a, hw-batch) and 16 min (m2-b) 0 protected loss; #60 light stream
  restarts and 4K+HD record-only splits, #68 fit/heavy short and split runs
  clean; strict e2e 17/17; gates 214 tests. The two soaks' early losses were
  external load (another lane's engine; softwareupdated): see notes.

- S.2 rapid controls / #80 / #81 (2026-10-16): splits stay busy until the
  new writers start and the previous ones finalize; normal Stop preserves
  the newest announced take. A startup refusal follows session cleanup.
  Final M2-b binaries: 200 split attempts (78 accepted, 122 busy), no phantom
  files or VT errors, 388 stress files checked, eight disk refusals with
  the correct reason, 40 concurrent-control files checked; seven kill modes
  and 11 ownership controls, strict e2e 16/16, fmt/clippy and 212 tests pass.
  Tier-1 review remains with the coordinator; evidence in rapid-splits
  scratch REPORT.md.
- G.4 (#19, 2026-10-16): projectors (program, preview, scene, source,
  multiview top-8/3x3/2x2) on the method table (projector_open/close/
  list/place, `isocast projector`, MCP), kept and placed by the daemon,
  shown by the app full screen (simple full screen above the menu bar) or
  windowed; scene:ID monitor targets on an engine scene-feeds thread.
  Verified on m2-a: 0 dropped frames recording program + 2 ISOs with a
  full-screen program projector, a scene projector and a multiview open
  (isocastd +24/+27% of a core, A/B x2); projector frames reach the window
  as soon as the CLI's shared stream gets them (p50 45.7 vs 46.3 ms after
  the program tick); a virtual display unplugged closes its full-screen
  projector in 1 s with the reason; offscreen snapshots; visual-shots base
  vs head unchanged.
- #93 (2026-10-16): with the developer fault hook (bars2 capture stops
  before frame 90) BARS2 ends on source frame 89 on head in every run, 88
  on base 3470602 in 2 of 3; live.handover true after a split, false
  1.6 s after its start; e2e split/pause/record, kill matrix 18/18 and
  stress cycles+disk pass on m2-a (38 splits accepted, 62 busy, 0 files
  acknowledged without a file).
- #95 tally (2026-10-16): Studio Mode's PROGRAM tag, the live program
  ring, the scenes' PGM/LIVE badge and the editor's PGM are red
  (`danger`), PREVIEW and a new Studio Mode PVW scene badge green
  (`onair`), the multiview's tokens; offscreen base/head shots and
  visual_diff: only those pixels changed.
- #95 (2026-10-16): virtual display removed before launch, app windows
  polled every 100 ms on m2-a: base covers the main display for 0.9 s on
  app start and on `projector open program`, head never (0/75, 0/56); F
  on the second display records it and unplugging closes the projector
  in 1 s.

- Engine hardening (#27/#30/#31/#35, 2026-10-16): explicit critical-state
  recovery replaces lock poisoning; the monitor callback is lock/heap-free;
  AVAssetWriter append exceptions are contained and both VT session types
  drain before invalidation. Live tap video is bounded separately from
  audio, with counted oldest-picture eviction; aggregate pool/capture-ISO
  isolation (#27) remains open. Hosted e2e 16/16 and M2-b strict smoke 5/5,
  zero drops in three interleaved flagship A/Bs, CPU 26.3% main / 26.1% head.
  Panic and native teardown evidence in hardening/scratch/REPORT.md; review
  and re-review found no blocker. Final merged-main gates and monitor-hour
  result are recorded in that report.

- A.4 / #82 (2026-10-16): a microphone whose driver stops answering no
  longer leaves "previous open is still pending" until macOS returns. Test
  VM with coreaudiod stopped 40 s, base vs head: base repeats Ryan's
  errors and refuses the first start after coreaudiod resumes; head gives
  the actionable message, keeps 2 stuck opens, logs both returning, and
  opens the mic on the first start after resuming. e2e setup, record,
  scenes, shutdown pass on m2-b.
- S.2 / #79 (2026-10-16): native recording submission pressure protects
  program and audio at the M2 limit. Three interleaved A/B pairs: six baseline
  20 s takes lost 48–55 program pictures and 494–561 audio chunks each; final
  program/audio 0/0, ISOs shed instead. Final 4/5/6/8-ISO sweeps and 7/8/9/11
  with stream have zero protected video/audio loss; #60 light restarts,
  recording-only six splits, #68 fit/heavy 181 s and six splits, overloaded
  pause/split, and full strict e2e 16/16 pass. M2 predicts five HD ISOs over
  capacity (53% ISO loss), eight plus stream ~82%, instead of 0%/56%.
  Final stock stress isos PASS (new limit four); workspace fmt/clippy and
  212 tests/doctests pass. Tier-1 review and commits remain the coordinator's.

- A.4 / #78 (2026-10-16): the e2e replay's intermittent 33.31 ms
  flash/beep offset is the test pattern generator waking late (37-104 ms,
  default QoS) and skipping the pulse's first frames; burnt-in FRAME labels
  in both failing saves show the stale frame repeated and frames 1139-1141 /
  1200-1201 missing, and the same take's recording has the same late flash.
  The generator now asks for real-time QoS. M2-b, no load, interleaved:
  base 2/15 FAIL (33.31 ms), fix 0/30 (all 0.021 ms), generator max wake
  ≤12.4 ms with 0 late wakes, compositor reused frames 3-7 to 0.
- A.4 / #77 (2026-10-16): a warm reservation no longer keeps a program
  tap; first real join restores it synchronously without recreating the
  native encoder or resetting counters/timestamps. M2-b final CPU baseline /
  fixed / no-stream 2.919 / 2.359 / 2.432% whole-machine; program draws
  1219–1220 / 0 / 0 per 20 s. Fit/heavy short, 181 s and six split takes:
  program/stream/audio losses 0, fitting ISO losses 0. 150 s park starts
  with 5,406 B; 64 files probe/decode, ten immediate join/canvas edits
  refused, changed-canvas heavy join clean. Review B1 reproduced and fixed;
  re-review approves. Workspace gates 206 tests; full e2e once 15/16
  (intermittent replay 33.31 ms sync failure), baseline/head replay controls
  2/2 each and final stream/replay 2/2 pass. Follow-up retained in report.
- #76 flock (2026-10-16): crash-safe takes refuse a file only when its
  lock is held elsewhere (WouldBlock); a share that refuses flock records
  with a logged warning. Verified on an NFS `nolocks` mount (ENOTSUP):
  main refused the take, head records it complete (program 120 frames, ISO
  108: it starts 0.4 s late on this share, NFS file creation); exFAT and
  APFS lock as before (kill-matrix ownership 11/11 incl. live-renamed).
- #76 creation time (2026-10-16): crash-safe movies and their tracks carry
  the take's start (mvhd/tkhd/mdhd), after Stop and after kill -9 + repair;
  ffprobe creation_time and AVFoundation creationDate match the file name
  on program and ISO files (APFS, exFAT, NFS; kill-matrix 6/6, ownership
  11/11); main's files had none.
- G.10 / #73 (2026-10-15): multistream. Isolated engine recording 1080p60
  plus 3 local ffmpeg receivers: all decode clean, no gaps, A/V within a
  frame; killing one receiver reconnected it (1 reconnect) with 0 drops on
  the others and the recording; mixed 6000/6000/3000 kb/s measured as set;
  an overloaded own-format start is refused with the estimate's reason.
  Settings/status bar checked offscreen. Evidence and m2-b numbers:
  /Volumes/IsocastBuild/multistream/scratch/REPORT.md.

- G.3b / #3 (2026-10-15, default-on integration, awaiting coordinator review):
  preserved the reviewed fragment/repair implementation while integrating
  main 505145d; new writers take the shared first-picture gate after the
  parked stream encoder, and keep startup backlog until two native pictures
  return (fixed measured 3/3/2 ISO startup losses to 0/0/0). 500 ms default,
  one-release legacy fallback, keep every track's samples, automatic repair,
  retained kill-matrix. Final M4 #68 matrix 35/35 movies, 0/0/0 drops; M2
  fitting/overload split checks 39/39, protected outputs 0. Final 7 kill +
  11 ownership cases pass; prior 21 kills pass on retained evidence after
  correcting the empty-track checker. Full e2e 16/16 once, final scoped 5/5;
  full workspace fmt/clippy/201 tests pass. M2 CPU 31.1 → 25.7%, instructions
  557.6 → 486.1 M/s, 0 drops, all benchmark movies decode. Evidence and
  uncommitted merge handoff: g3b-crash-safe scratch REPORT.md.

- G.10 / #33 (2026-10-15): system and app audio through Core Audio process
  taps with SCK fallback, `exclude`, `system_audio` permission. Verified on
  isocast-m2-a (macOS 27) and m2-b (26.7): tone tracks right (excluded and
  other apps at -107 dB, also for apps that start mid-take), beep stamps
  0.6 ms (27) / 21.9 ms (26.7) from AVAudioEngine's schedule with 0 spread
  (SCK -0.6 / 37.7 ms), 0.1-0.2 s less CPU per 18 s than SCK, output switch
  mid-take recorded like SCK, forced and not-allowed fallbacks; e2e hosted
  16/16.
- A.4 / #68 review B1 (2026-10-15): keep the pre-placed encoder's native
  clock contiguous across parking; restore host PTS/DTS before muxing so
  VT cannot spend the idle gap as a bitrate burst. 150 s park: first packet
  4,935 B, memory +0.84 MiB, no stream loss; stream e2e now checks startup
  packet size and bytes/second as well as drops. Re-review evidence in REPORT.md.

- A.4 / #68 (2026-10-15): pre-place and park the shared stream/replay encoder
  before recording writers; M4 record-first short/181 s/split runs have zero
  ISO drops, M2 fitting load has zero drops and overload protects program and
  stream. Real media probe/decode and idle CPU evidence in iso-overshed REPORT.md.

- A.4 / #66 (2026-10-15): capacity estimate, overload notice, safe ISO
  defaults. On isocast-m2-b (25 s synthetic takes) the estimate matched
  three setups: 4K60 ISO + 1080p60 program 0.78 fits -> 0 drops; + 1080p60
  ISO 0.93 no margin -> 0; + 1080p60 local stream 1.23 can't keep up ->
  49% ISO drops and one encoder_overload event; its first fix (4K ISO at
  1920x1080, 0.76) -> 0 drops in every file and the stream. H.264 costs
  1.9x HEVC (tune on m2-b). App notice verified offscreen in every state,
  including the real m2-b event; hosted e2e 16/16 in the VM.
- P.3 (2026-10-15, #46): first launch finds OBS and optimizes. Engine:
  `obs_discover` (ISOCAST_OBS_FOLDER for isolated engines),
  `library_import_obs` `stream_key`, `speed_test` (RTMP filler publish,
  server acknowledgements), `optimize` (rules in
  isocast_protocol::optimize, then `tune`), on the API, CLI and MCP. App:
  area I's wizard sheet (From OBS, Goal, Summary), Tools ›
  Auto-Configuration Wizard…. Verified: scratch OBS folders (several,
  none, errors, m2-a's copy) imported in an isolated engine and a 4 s
  take ffprobed (HEVC 1080p60, 4.000 s); speed test 7988 kb/s through an
  8000 kb/s throttle, 3995 through 4000 on m2-a; optimize on m2-a: a 4K
  H.264 setup became "1440p60, HEVC High, 3 ISO files" (tune: program
  2560×1440, 1.73×; a separate tune of the applied setup 1.74×, fits),
  its take HEVC 2560×1440 + 3 ISO files at 60 fps, 4.000 s; streaming at
  4 Mb/s became 30 fps with a 2.5 Mbps 720p stream; stream key moved to
  the Keychain in m2-a's desktop session (and cleared). Offscreen
  snapshots of every step; visual-shots base vs head unchanged.

- G.8 (2026-10-15, #61): WebM/MKV media sources and stingers through our
  own EBML reader and VideoToolbox/AudioToolbox; AVFoundation keeps every
  other file. Verified with an isolated engine on an M5 Max: a 1080p60
  VP9-alpha stinger composites transparent (scene pixels exact) for its
  120 frames; 709/601/full-range colours within 1 code; flash/beep clips
  as VP9/Opus, AV1/Opus and H.264/AAC MKV loop in sync (<= 0.04 ms); 120 s
  looping alpha media: 0 drops, flat RSS, 11.7% of a core.

- #57 / #63 stream fixes (2026-10-15): LAN streaming and per-build
  stream keys work. Verified on isocast-m2-a (macOS 27) with Developer ID
  builds: main's build fails rtmp://Mac-mini.local with "could not look up"
  once Local Network is denied; the fixed build names the setting, and
  streams 5.7 s of H.264+AAC to it once allowed. A key a different app
  owns under the old service blocked main's Save Key (-25244); the fixed
  build ignores it, saves under dev.isocast.release.stream, and moves a
  key main's release build saved. Settings › Stream banner checked offscreen.
- Motion harness fix round 2 (2026-10-14, re-review): no path outside
  the run folder is touched (plain names, no recursive removal, symlinks
  refused, state allowlist, isolated engine required); an flock for the
  work folder and a crashed run's engine ended before its folder goes;
  release expectations at the release's position; window stalls of a
  frozen edge; gain reported, lag only when the edge follows. Verified:
  controls 11/11, window --selfcheck 6/6, three lockstep suites + a
  CPU-loaded one identical, reviewer's repros, lock and leak checks.
- Motion harness fix round (2026-10-14, review "fix first"): no false
  passes (coverage gate, invalid on engine timeout), pixel-grid stalls,
  true rest frame and per-gesture expectations, app time through
  src/clock.rs so lockstep is deterministic, no leaked engines, stale
  results rejected, GPU time in frame time, controls with injected faults.
  Verified: controls 9/9 as expected; three lockstep suites plus a
  SIGSTOP-perturbed and a CPU-loaded one compared with
  scripts/motion/compare.py; pgrep/lsof clean after normal, failed,
  SIGINT and SIGTERM runs.
- Motion harness (2026-10-14, GitHub #24): `scripts/motion.sh` replays
  scripted pointer input into the real interface offscreen (virtual clock,
  real engine, every frame read back) and in a real window on a test Mac
  (`--window`), and checks the edge against the pointer every frame
  (docs/motion-testing.md). It reproduces #24 on main e9753d4: every
  splitter moves at half the pointer's distance (headless: 59 pt p95 gap
  on a 120 pt drag; isocast-m2-b window: 50 pt for 120, gain 0.42, idle
  and recording); canvas resize realtime a frame behind with 27-31 stalled
  frames, 6.3 pt p95 while recording. Verified by two identical lockstep
  runs and the window recordings.
- A.4 / #60 (2026-10-15, partial qualification): VideoToolbox blocks inside a stream submission
  below our eight-frame limit, hiding pressure and exhausting the program
  pool. Publish pressure from the native call's frame-time budget too;
  retain the start gate and bounded wait. Review found first-frame setup
  could cut healthy ISOs (51 frames each); exclude that first submission,
  verified with three loss-free stream starts. Final build: 16 post-split
  takes loss-free, but 9/12 pictures lost at stream startup in two runs.
  Stop after three investigated variants; the initial native call can block
  beyond the pool's reserve before feedback can act. 249/249 files pass
  ffprobe/full decode, final e2e 15/15, workspace fmt/clippy/191 tests pass.
  Review blocker cleared; strict zero-drop bar remains open. See REPORT.md.

- A.4 / #41 (2026-10-14, fixed): the cause is VideoToolbox's placement of
  hardware sessions on the M4 Max's two encode engines, decided at each
  session's first picture; a standalone VT benchmark reproduced the slow
  mode (three sessions on one engine at ~45 fps) in 3–30% of together-started
  trials and 0 of 240 started ≥ 17 ms apart. The macOS writer and the stream
  encoder now take turns starting (program first, then largest first, 25 ms
  apart) and a split's ending files stay open 200 ms. Exonbook six 30 s takes
  + 10-minute split session: 0 drops in all files, three repeats (main lost
  1,011–1,068 per ISO in half the takes); 20-take split loop 0 lossy files,
  engine ratio 1.00 every take; m2-a interleaved A/B unchanged (healthy 0
  drops, overload ISO losses retained); e2e 13/13 on both.
- A.4 / #41 (2026-10-14, blocked, superseded above): baseline six-take repro lost 9,416 ISO
- 8.2j / #58 (2026-10-15): on isocast-m2-b (Developer ID build) the
  helper stays put while System Settings' password sheet (a 260x373 window
  of its own, in front) shows; a bare isocastd's `permissions` call drops
  from 23 to 9.6 ms (no preflight child), the bundled one still checks
  (60 ms); ssh sessions lack sessionHasGraphicAccess (0x5020 vs 0x6030 in
  the desktop). Offscreen before/after for the rest; visual-shots 15/15
  views unchanged.
- 8.2i / #51 (2026-10-15): the permissions step and the drag helper work
  end to end on a Developer ID build on isocast-m2-a (macOS 27) from fresh
  grants: camera and microphone prompts from Allow…, rows flipping live,
  Open Settings + helper + a real cua drag of Isocast.app into the list,
  Needs relaunch → Relaunch → Allowed, then Start with this Mac and a
  recording with screen, synthetic camera and tone ISOs (HEVC 60 fps, AAC,
  5.83 s each, 0 dropped). Offscreen 1440x900@2 shots of every state.
- A.4 / #41 (2026-10-14, blocked): baseline six-take repro lost 9,416 ISO
  frames, zero program frames. Native submission stalls precede the shared
  controller's 1000→132 cut; independent takes reset all priority counters.
  Relative timestamps, late look-ahead=0, power-efficiency=false and explicit
  maximum real-time fps=60 did not solve it; all trials/instrumentation
  removed. #42-only build still loses ISO frames; do not call this fixed.
- A.6 / #42 M2 qualification (2026-10-14): three interleaved baseline/head
  pairs per scenario. Healthy flagship-1080: 0 drops both builds. Overloaded
  flagship+stream: protected program/stream video/audio drops 0 both; median
  total ISO drops per run unchanged at 2,562. All 36 MOVs ffprobe matched;
  strict M2 e2e 13/13, zero drops. No recording-path/admission changes.

- A.6 / #42 (2026-10-14): stop and status.last now summarize each file's
  dropped pictures; tail repeats do not inflate the percentage denominator.
  Six quiet takes plus four ten-minute-session splits: 9 affected files all
  warned, 31 clean files not warned; CLI text and MCP stop/status checked.
  Release fmt/clippy, 182 unit tests + one doctest and 13/13 zero-drop e2e
  pass on exonbook. All 204 investigation files ffprobe count/duration matched.
- resize-lag, GitHub #24 (2026-10-14): dock splitters, dialog headers and
  scrollbar thumbs follow the pointer exactly (window coordinates; column
  splitters fit the docks on press), a dragged handle's box is drawn at
  once, and monitors ask for new sizes at most twice a second. Verified with
  scripts/drag-lag.py --mode resize|columns|rows [--record] on isocast-m2-b,
  median of 3 interleaved runs: gap p95 4.5/25.4/46.8 -> 1.0/0.5/1.0 points,
  lag 16.7 -> 0 ms, 0 glitch frames, 0 dropped frames while recording.
  Fix round 1: the held resize box snaps to a new selection at once (500
  -> 0 ms of wrong outline), and column splitters work with any docks
  hidden (147/147 offscreen cases in all 26 dock layouts, against 112).

- A.2 QA-3 R1 (2026-10-14): fill ISO audio gaps progressively on the
  pause-mapped video clock, retaining the late-input/negative-offset allowance;
  media/browser silence no longer counts as loss. Reproduced 48 s video with
  28.91/28.93 s audio and 1894/1892 dropouts; fixed files are 48 s A/V,
  zero drops, final beep/flash error 0.021 ms. Browser unload/reload and 18
  leading/trailing/EOF/pause/split/multitrack/offset files also pass. Program
  retains its audio, strict E2E 13/13, fmt/release clippy and 178 tests pass.
  Evidence: `/Volumes/IsocastBuild/qa-engine/scratch/R1-REPORT.md`.

- A.7 process-wide escalation (2026-10-14): share measured timer stretching
  across clock creation and subsequent waits, without an admission barrier or
  extra startup wait. Live-engine restart and new ISO clocks inherit critical
  mode with zero local escalations. Final matrix: all 162 files fully decoded,
  10 s A/V and correct accounting; no short ISO. M4 drops by default30/
  Background30/default60/Background60: 0/331/0/380 -> 0/295/0/199 (Background30
  includes three targeted control pairs); M2 0/6/12/25 -> 0/3/7/22. Normal
  E2E 13/13 each; fmt/release all-target clippy and 180 tests pass. Previous
  CPU accepted within noise by decision; existing Background limits retained.

- A.7 adaptive-pacing (2026-10-14): merged main 18b8d4c as 25a3238; ordinary
  pacing selected on every clock in all final normal-context CPU rows, critical
  pacing selected in every sampled throttled recording clock. Program/ISO/source/
  virtual-camera modes and lock-free wake diagnostics verified; 49.9 MB of
  app/probe payload excluded from CI diagnostics. Both Macs strict E2E 13/13;
  fmt/release clippy and 180 tests pass. Powermetrics and throttled comparisons
  do not prove the full requested non-regression gate; retained all failures,
  including a short ISO, and marked that qualification blocked instead of
  relaxing checks. See REPORT.md Adaptive pacing for all per-cell numbers.

- A.7 hosted-pacing (2026-10-13): reproduced 10 ms sleeps stretched to ~80 ms
  by default launchd and ~200 ms by Background, unaffected by interactive QoS.
  Absolute critical kqueue timers cap intentional slack at 5 ms. Final M4
  launch matrix 10,800/10,800 frames, M2 10,791/10,800, all 48 files decoded and
  duration-matched; normal E2E 11/11 on each. Three final locked M4 A/B pairs:
  median CPU 32.6 -> 30.1%, no drops; M2 CPU results contaminated by concurrent
  builds/UI and excluded. fmt/release clippy/test (180 tests) clean. Hosted
  diagnosis still awaits publication; no integrity or loss gate was relaxed.

One line per finished task, newest last.

- A.2 QA engine integration (2026-10-14): seven focused finding commits;
  existing e2e assertion follows stable source paths and collection CLI help
  describes restored selections. Final m2-a run 13 PASS / 0 FAIL / 0 SKIP,
  strict zero drops. Report: /Volumes/IsocastBuild/qa-engine/scratch/REPORT.md.

- A.2 QA-2 #7 (2026-10-14): repeated imports produce QA Show / (2) / (3),
  verified in CLI and offscreen menu across restart. Stereo/mono/stereo
  recordings: channel-difference RMS 0.149967 / 0 / 0.149995, each 90 frames,
  3 s and zero drops; monitor render checked before balance. Release gates
  pass (180 tests); final engine e2e 13/13, no skips or drops on m2-a.

- A.2 QA-2 #5 (2026-10-14): QA Show returns to Main/BRB preview instead
  of Bars/no preview; profile switches, restart, removed-scene fallback
  verified. Offscreen Studio restore and real Studio-off button pass;
  CLI/MCP clear_preview, fmt/clippy and 180 tests pass.

- A.2 QA-2 #4 (2026-10-14): QA clip on m2-a now has its own AAC in its
  default ISO; six before/after/explicit routing cases, 18 decoded files,
  90 frames and 3.000 s each, zero drops/dropouts, nonzero audio, restart
  persistence. Release gates and Advanced Audio snapshot pass.

- A.2 QA-2 #1 (2026-10-14): 14 codec/container combinations checked;
  ProRes/MP4 apply/start errors name encoder.container and MOV. Seven
  encoder options agree across API/CLI/MCP/schema; release gates pass.

- A.2 QA-1 #4 (2026-10-14): QA source index 7 now reports
  sources.colour.height and "Colour Source" rather than sources[7];
  one CLI apply sets 400×300 atomically. Typed error, offscreen app,
  fmt/clippy and 180 tests pass.

- A.2 QA-1 #3 (2026-10-14): blocked native read is present in status and
  source_changed, snapshot errors after 8 s (8.718 s CLI wall time), duplicate
  request fails promptly, healthy edits and shutdown continue. Actual QA
  Documents clip opens (access already granted). Release gates: 180 tests.

- A.2 QA-1 #1 (2026-10-14): exact Headline edit grows 178×116 to 741×87,
  pattern 1920×1080 to 1280×720; explicit stretch/custom boxes preserved.
  New API item uses no bounds; 0.5 scale resolves 640×360. TOML/JSON
  round-trip, offscreen selection boxes, fmt/clippy and 180 tests pass.

- 0.1 (2026-10-11): skeleton, setup format and checker, example rig.
- 1.1 (2026-10-11): `isocast record --source test-pattern --source tone
  --seconds 10` writes an HEVC 1920×1080 60 fps .mov with an AAC 48 kHz
  stereo track through AVAssetWriter/VideoToolbox. ffprobe: both streams
  10.000000 s, 600 frames, r_frame_rate 60/1, Rec. 709 tags; signalstats
  shows exactly 6 flash frames at each whole second and silencedetect shows
  the beeps there too. Also checked: 4K60 and 720p30 sources, Ctrl-C stop
  (file ends on a frame boundary), validation errors.
- 1.2 (2026-10-11): `isocast devices [--json]` lists displays (stable UUIDs,
  pixel size, refresh), windows (with screen permission), cameras with their
  formats merged by size and pixel format, microphones with sample rates
  and channels, and the three permissions, without prompting. Ran it on this
  Mac: 2 displays, 8 cameras incl. the iPhone as Continuity Camera, 8 mics,
  all permissions granted; JSON parses. Binding decision (objc2 family) is
  in notes.
- 1.3 (2026-10-11): `isocast record --source screen --seconds 10` writes HEVC
  3840×2160 (panel native, not the 5120×2880 backing) 60/1 fps, 600 frames,
  exactly 10.000000 s, Rec. 709, 0 dropped, at ~4% of one core.
  ScreenCaptureKit frames go through the engine's frame clock; it reported
  315 new frames and the file has 300 distinct ones (content changed at
  ~30 Hz). Also: second display by name with a tone track, unknown display
  error listing valid ones. Screen permission was already granted.
- 1.4a (2026-10-11): `isocast record --source camera:Insta360 --source tone
  --seconds 10` writes HEVC 1920×1080 30/1 fps (the camera's best format),
  300 frames, both streams exactly 10.000000 s, start offset 0 (the session
  waits for the camera's first frame); 301 frames captured. Errors list the
  device's formats or the available cameras. Found that OBS's "iPhone"
  source is the iPhone screen over USB; support for it is in, untested.
- 1.5 (2026-10-11): `isocast record --source test-pattern --source mic:Wave
  --source system-audio --seconds 8` writes the video plus a mono AAC track
  from the Elgato Wave:3 (peak −23.6 dB) and a stereo system audio track, all
  three exactly 8.000000 s, 0 dropouts. A quiet system sound played during
  the take shows in the system track at −36.6 dB, matching a reference
  capture. Mic and screen permission were already granted.
- 1.6 (2026-10-11): every session writes `<date> <time>.session.json` with each
  file's start offset on the session timeline (written at start, once all
  files start, and at the end). Recorded a 30 fps and a 60 fps test pattern
  with the tone in both: flashes fall on exactly 0, 1, … 9 s in both files
  (0.00 ms apart), beeps match across files, flash vs beep ≤ 1.2 ms (AAC
  onset). The manifest reads as `recording` mid-take and `complete` after.
- 1.7 (2026-10-11): `isocast record <setup>` with Ryan's rig layout (stand-ins:
  Insta360 X3 for the capture card, a 1152×2504@60 test pattern for the
  iPhone screen; neither device is connected) wrote SCREEN- (3840×2160@60),
  FACECAM- (1920×1080@30) and IPHONE- files, each with the mic (mono) and
  desktop audio (stereo) tracks, all streams exactly 10.000000 s. With 2 s
  movie fragments, `kill -9` at ~7 s left all three files decodable with 0
  errors (6 s each: 360/180/360 frames) plus a manifest with offsets; a
  normal finish is a standard indexed movie.
- 1.8a (2026-10-11): a 4K60 recording of 2,000,000 s is refused with the free
  space, the estimated need and the rate per minute. With `keep_free_gb` set
  1.5 GB under the SSD's free space and `dd` writing 2 GB of filler
  mid-take, a 30 s recording stopped cleanly at 4.84 s: the file plays
  (4.87 s, both streams) and the manifest says `failed` with the reason.
- 1.8b (2026-10-11): each recording first writes and fsyncs a 64 MB probe: the
  SSD measured 854–877 MB/s; an 8K60 ProRes 4444 setup (estimated 1.4 GB/s)
  is refused before any file is created. Network volumes are flagged from
  `MNT_LOCAL` (checked against `mount`: the SMB share lacks `local`).
- 1.8c (2026-10-11): the engine reports live counters every second
  (`RecordOptions::on_status`) and the CLI warns as soon as a file drops
  frames or audio. Freezing the recorder for 1 s with SIGSTOP produced
  "dropped 3 frame(s) at 3 s" then "58 … (61 so far)", matching the final
  61; an 8 s screen + camera + mic + system audio take printed no warnings.
- 1.8d (2026-10-11): new `encoder.bit_depth` (8/10). ffprobe on real takes:
  ProRes 422 and 422 HQ (profiles Standard/HQ, yuv422p10le), HEVC Main and
  Main 10 (yuv420p10le), H.264 High; all tagged bt709 primaries, transfer
  and matrix, tv range. The screen in 10-bit HEVC (10-bit capture) and in
  ProRes 422 HQ at 4K60 (~1.5 Gb/s) both decode with 0 errors. H.264 10-bit
  is rejected by setup validation with the alternatives.
- 1.8e (2026-10-11): 10-minute take of the stand-in rig (4K60 screen,
  Insta360 facecam, 1152×2504@60 and 1280×720@30 test patterns; mic, system
  audio and the tone in every file): all 16 streams exactly 600.000000 s,
  offsets 0, 0 audio dropouts; 18000/18000 frames at 30 fps and
  35999/36000 at 60 fps (one shared hiccup at 592.5 s). All 600 flashes on
  exact whole seconds in both pattern files; tone beeps within 1.17 ms of
  whole seconds in all four files to the end, so the files line up within
  1.2 ms (a frame is 16.7 ms). Debug build: 57% of one core, 239 MB.
- 2.1 (2026-10-11): `isocastd` owns the session and answers JSON-RPC 2.0
  (status, devices, start, stop, shutdown) on a Unix socket; `isocast start`
  launches it if needed, `status` shows live counters, `stop` returns the
  report. Verified start → status → stop, "already recording"/"not
  recording" errors, precise errors for unknown methods and bad params, a
  timed take ending by itself, a display matched by name inside the daemon
  (AppKit on its main thread), and SIGTERM mid-take finishing the file
  (fully indexed, manifest `complete`) and removing the socket.
- 2.2 (2026-10-11): the daemon holds a revisioned live setup: `isocast get`,
  `plan FILE|--patch JSON` (diff with id paths plus problems), `apply …
  --revision N` (atomic; refuses stale revisions and setups with problems)
  and `watch` (event stream). Verified on Ryan's example setup: a patch
  changing fps, retargeting the facecam and removing the iPhone planned as
  `canvas.fps 60 → 30`, `sources.facecam.device …`, `- sources.iphone`;
  removing a referenced source was refused with both problems; a stale
  revision was refused; `start` without a setup recorded the live one (30
  fps screen + Insta360, scratch folder); `watch` showed setup_changed,
  started, status ticks and stopped.
- 2.3a (2026-10-11): `isocast marker LABEL` and `isocast scene ID` (API
  `marker`, `switch_scene`, events, scene in status). The manifest gains
  `markers` and a `scenes` timeline, rewritten as they arrive. Verified: the
  first scene goes on air when a setup is applied; a switch before recording
  sets the starting scene (0.0 s); during a take a marker at 1.05 s, a switch
  at 1.91 s and a marker at 1.93 s were in the manifest mid-take and after
  stop; marker while idle and unknown scenes are refused with the options.
- 2.3b (2026-10-11): `isocast mcp` serves 9 intent-level tools over stdio
  (rmcp 3.5): get_status, list_devices, get_setup, plan_change,
  apply_change, start_recording, stop_recording, switch_scene, add_marker,
  each a call on the daemon's method table, with annotations and output
  schemas. Verified with a scratch stdio MCP client: initialize (protocol
  2025-06-18) → tools/list → plan → apply → start → switch scene → marker
  → switch → marker → stop; the report and manifest hold both markers and
  the scene timeline; a bad scene and a second stop come back as tool
  errors with the reason.
- 2.4 (2026-10-11): markers came in 2.3a; `isocast split` (API `split`, MCP
  `split_take`) now ends the current files at the next whole second (≥0.5 s
  ahead) and continues into a new take's files and manifest, without
  stopping any source. Verified with 30 and 60 fps patterns + tone and two
  splits: takes of exactly 3.0 s (90/180 frames), 2.0 s (60/120) and the
  rest (31/61), 181 frames in all at 30 fps for a 6.01 s session (none lost
  or doubled); takes 2 and 3 open on a whole-second flash and beep; each
  manifest has its own markers; streams in every file are equal length.
- 2.5a (2026-10-11): `isocast doctor [SETUP] [--folder]` (API `doctor`, MCP
  `check_setup`) checks permissions against what the setup needs, the chip
  and hardware encoders (VideoToolbox), the setup's validity, each source
  resolved to a device and the format it would record (without opening it),
  and the recording disk's hours of room, speed and network status. On
  Ryan's setup it flagged the missing capture card (listing the cameras),
  resolved the screen to 3840×2160@60, `iphone` to the Continuity camera and
  `mic` to the MacBook mic, and passed the SSD (17 h, 874 MB/s). Through the
  daemon with no live setup it checks the machine only.
- 1.4b (2026-10-11): with Ryan's devices through the signed daemon: "Studio
  Video" (his facecam) 1920×1080 60/1, 600 frames, 0 dropped; "Ryan iPhone"
  (screen over USB) 1206×2622 60/1, 0 dropped, size taken from its first
  frame; "Studio Audio" stereo in both; all streams 10.000000 s. A Studio
  Video frame looked right (orientation, colour). The capture card is not
  in use now (question in notes).
- 2.5b (2026-10-11): `isocast tune [setup]` (MCP `tune_encoders`) encodes
  every ISO stream at once with each candidate codec on busy camera-like
  frames, judges speed (1.25× real time after a 1 s warm-up), disk speed
  and an hour on disk, and returns a recommendation plus a patch for `plan`.
  On Ryan's rig (screen 3840×2160@60, Studio Video 1920×1080@60, iPhone
  screen 1206×2622@60, its size learned by opening it) HEVC 8-bit ran
  1.5–2.2× and was recommended; ProRes ran 10–15× but only 0.4–0.7 h fit.
  Verified the refusals (start while tuning, tune while tuning) and that an
  invalid setup is rejected with its field path.
- 2.4 follow-up (2026-10-11): fixes from a code review of splits. A file a
  split ends is finished on its own thread and kept open up to 500 ms for
  its last audio; the split time is picked after the new files exist and at
  least 1 s after the current take's start; splits that never begin are
  cancelled even with no packet after them; marker/scene replies give the
  take and its timeline; status lists the new take's files. Verified on the
  rig: a 60 s take then a split, 0 dropped frames in both takes, take 1's
  audio is real mic sound up to its last sample (no padded zeros), audio and
  video both 61.000 s; markers replied 59.90 s/take 1 and 1.92 s/take 2,
  exactly as in the manifests; a double split gave a 1.000 s take; split
  then stop left no orphan file or manifest.
- 3.1a (2026-10-11): wgpu 30 on Metal; CoreVideo frames become wgpu
  textures through a CVMetalTextureCache (no copies), drawn by one shader
  that converts Y'CbCr with the frame's matrix and range. `isocast snapshot
  SOURCE [-o FILE] [--width N]` and MCP `preview_frame` return a PNG.
  Verified: test-pattern pixels exactly equal the drawn bar values (BGRA);
  a 4K screen snapshot (420v) against an ffmpeg BT.709 decode of a
  recording of the same screen: median difference 0, per-channel bias
  under 0.15 levels on unchanged pixels; 10-bit (x420) agrees with 8-bit
  (bias under 0.3); Studio Video and the iPhone screen look right. Audio
  sources and unknown names give errors listing the valid choices.
- 3.1b (2026-10-11): while recording, `snapshot` of a recorded source uses
  the newest frame the recording captured, without taking it from the
  frame clock; test patterns and displays still open on their own; other
  cameras are refused (opening one could change its format under the
  recording). Verified during a rig recording: facecam, iphone and screen
  snapshots in 0.08–0.3 s, `camera:Studio Video` refused naming the
  recorded sources, and every file kept 0 dropped frames.
- 3.2 (2026-10-11): scene items gain `crop` (pixels off each edge) and
  must have a non-empty rect; the renderer draws a scene's visible items
  back to front, each cropped and scaled into its rect, clipped at the
  canvas edges. `isocast snapshot --scene ID` (MCP `preview_frame` with
  `scene`) draws a scene, and with no argument the scene on air. Verified
  with two test patterns: 7 pixel checks exact (position and scaling, a
  1-bar crop, a hidden item not drawn, draw order, an item half off the
  canvas); Ryan's desktop and closeup scenes drawn idle, and the on-air
  scene drawn from a running recording's frames in 0.07 s with 0 dropped
  frames.
- 3.3 (2026-10-11): with `recording.program` enabled, every scene's sources
  open with the session and a compositor draws the scene on air on the GPU
  at each canvas tick into an IOSurface canvas that goes straight to the
  program file (`{date} {time}.mov`, its own encoder, one audio track per
  `program.audio` source until 3.4 mixes them); it splits into takes like
  the ISO files, and `tune` counts it. Verified with test patterns: program
  and ISO files 187 frames each, 0 dropped, same 6.233 s; whole-second
  flashes on the same frames in both; a scene switch at 3.157 s took effect
  on frame 95 (3.167 s), the first tick after it. On Ryan's real rig (4K60
  screen, Studio Video, iPhone screen, 4K60 program with two audio tracks)
  a 60 s take with a switch halfway: all four files 3592 frames, 0 dropped,
  59.867 s; desktop and closeup frames look right. The first real-rig run
  dropped ~10% in 5-frame bursts because a busy encoder dropped frames at
  once; video appends now wait up to 100 ms.
- 3.4 (2026-10-11): sources gain `audio = { gain_db, muted, offset_ms }`.
  The program's audio is one stereo mix of `program.audio` (mono sources
  centred), mixed 200 ms behind real time on the session timeline; ISO
  files keep raw audio shifted by the offset. Gain and mute change a
  running mix at once (`isocast audio SOURCE --gain/--mute/--unmute
  --offset`, MCP `set_audio`, or any apply); live status carries peak and
  RMS levels per source and for the mix. Verified with a 1 kHz tone (+100
  ms) and a 440 Hz tone (−6 dB) muting the first mid-take: ISO onsets
  exactly +100.0 and 0.0 ms, the mix's 440 Hz at exactly −6 dB, the 1 kHz
  at full level then 0.000 from the second after the mute; meters read −6
  dBFS for amplitude 0.5. Ryan's rig: one stereo mix track, 0 drops.
- 3.5 (2026-10-11): transitions (`cut`, `fade` with `duration_ms`; a
  setup-wide `[transition]`, cut by default, and per switch), a preview
  scene (`isocast preview ID`, MCP `preview_scene`) and `isocast take`
  (MCP `take_preview`), which puts preview on air and moves the old
  program to preview. The compositor draws both scenes of a fade on their
  own canvases and blends them (layers now have opacity); the manifest's
  scene timeline records each transition. Verified with a black scene and
  a test-pattern scene: `take --fade 1000` at 1.372 s ramped linearly from
  frame 42 (1.400 s) to full on frame 72, exactly 30 frames; `scene dark
  --cut` at 3.918 s went black on frame 118 (3.933 s); 0 dropped frames.
- 3.6 (2026-10-11): MCP `preview_frame` (and `isocast snapshot`) shows
  the program by default, the preview (`--preview`), any scene or any
  source. While recording, the program image is the exact canvas the
  compositor recorded last (it publishes each canvas to a frame slot), so
  transitions show as recorded. Verified during a recording: a snapshot
  1.0 s into a 2 s fade was 0.49 of full brightness; `--preview` showed
  the staged scene while another was on air. Source and scene snapshots
  came in 3.1–3.2.
- 3.7 (2026-10-11): one-hour soak of Ryan's rig without the iPhone (4K60
  display, Studio Video, Studio Audio, Elgato Wave, system audio, a 4K60
  program with a 3-source mix, fades every 5 min, markers every 10 min)
  plus a synthetic sync reference (test pattern + tone ISO), release
  build: every file 3599.95 s and 215,995 frames, 2 dropped per file (all
  at once, at 36 min, while a debug build wrote to the same SSD), 0 audio
  dropouts, 12 scene switches and 5 markers in the manifest. Sync: flashes
  on exact whole-second frames and beeps within 0.02 ms of whole seconds
  in all three ISO files at the start, middle and end (no drift). Daemon
  CPU median 49% of one core (max 56%), 135 MB, GPU ~43% busy, no thermal
  warnings (M5 Max).
- 4.1 (2026-10-11): scene items gain an `id` (default: their source),
  optional `source` (boxes and groups have none), `style` (opacity,
  background, radius, border, shadow, transform) and `children` placed in
  their parent's rect and moved, rotated, scaled and faded with it. The
  renderer draws every item as a transformed quad with an SDF rounded
  rect, border, CSS-style Gaussian shadow, premultiplied alpha. Patches
  and diffs match items by id, else source. Verified with a styled test
  scene: 10 pixel checks (background box, rounded corner, border colour,
  shadow falloff (9,17,26) vs computed (8,17,25), a 50% group and its
  child, a 90° rotation); the 3.2 plain-scene checks still exact; a patch
  by item id changed one border colour; a recorded program shows the
  styled scene. 19 new unit tests (clock, patches, validation, radii,
  transforms, audio tracks, mixer) pass.
- 4.2 (2026-10-11): each source kind has its own settings struct and
  checks (`isocast_protocol::source`, registered in one `source_kinds!`
  list) and an opener (`isocast_engine::source::open`) that delivers
  frames into a frame slot and/or audio into a feed; test patterns and
  tones became generator threads on the session timeline, so the session
  has one video path (frame clock) and one audio path, and snapshots,
  doctor and tune open sources the same way. `docs/obs-parity.md` explains
  adding a kind. Verified: flashes on frames 0, 30, ... 150 and beeps 0.02
  ms after whole seconds in both ISO and program files; the display,
  Studio Video, Studio Audio and Elgato Wave recorded 8.000 s with 0 drops
  and their own audio tracks; snapshots of a pattern (bar exact) and the
  camera; unit tests pass. Setup files are unchanged.
- 4.3 (2026-10-11): `color` (any size, canvas by default), `image` (PNG,
  JPEG upright by EXIF, GIF with animation, WebP, BMP, TGA, SVG at a set
  size) and `slideshow` (files, slide time, cut or fade, loop, shuffle,
  fitted and centred) sources; image paths in a setup file are relative to
  it; RGB frames keep their alpha. Verified with a scene of all of them:
  colour (32,64,96) exact, a 50%-alpha PNG blended to (143,32,48) vs
  (144,32,48) computed, an SVG circle with transparent corners; recorded:
  the GIF changes every 6th frame (200 ms at 30 fps), the slide show holds
  15 frames and fades over 15 (500 ms) each way; 0 dropped frames.
- 4.4 (2026-10-11): `text` sources (text or a file re-read when it
  changes; font, size, weight, italic, colour, outline, shadow,
  background, alignment, wrap width, padding, line height, fixed size),
  shaped and rasterised with cosmic-text into a picture drawn like an
  image. Verified: a bold title with outline and shadow, centred lines and
  a file-fed chat box with a translucent background look right; editing
  the file mid-recording changed the program at 1.57 s (written at ~1.5
  s); fonts load in well under a second even in debug.
- 4.5a (2026-10-11): `media` sources play video and audio files (a
  playlist, looping or not, at 0.25-4x speed) through AVAssetReader
  (hardware decode to IOSurface 4:2:0 frames, stereo float audio), paced
  onto the session timeline by two generators on one schedule. Verified
  with a 3 s clip that flashes and beeps on whole seconds, looped over 7 s:
  flashes on frames 0, 30, ... 180 across the loops, beeps where the clip
  has them (1.0027/2.0053 s within it) in ISO and program; at 2x speed
  flashes every 15 frames and beeps every 0.5 s; 0 drops.
- 4.5b (2026-10-11): the compositor tells the session timeline which
  sources are on air since when; media with `restart_on_activate` (the
  default, as in OBS) plays from the start each time it comes on air and
  not while off air, video and audio restarting from the same moment.
  Verified: cutting to the clip's scene at 2.252 s, the program shows the
  clip's first flash on frame 68 (the next tick, 2.267 s), then every 30
  frames; its beeps at 2.267, 3.269, 4.272 and 5.267 s (the clip's own
  offsets from the restart, across a loop); no clip audio before.
- 4.6 (2026-10-11): `window` (an app's largest on-screen window, by name
  or bundle id and optional title, scaled to keep its size if resized),
  `application` (all of an app's windows on a display) and
  `application_audio` sources through ScreenCaptureKit, a `cursor` option
  on screen captures, quick specs `window:APP` and `app-audio:APP`, and
  doctor probes for them. Verified: a Finder window recorded at its Retina
  pixel size 1840×872 (920×436 points) showing only that window, the
  Finder app at the display's 3840×2160, 0 drops; Music's audio captured
  (silent: nothing was playing, so not verified with sound); unknown
  windows and apps fail listing what is open.
- 4.7a (2026-10-11): `browser` sources render a URL or local HTML file in
  an offscreen WKWebView on the daemon's main thread (which now runs a real
  run loop), snapshotted at the source's frame rate once the page has
  loaded, with custom CSS (OBS's transparent-body default) and light
  appearance. Verified: an animated page with a half-transparent box over
  a colour source recorded with the box moving and the background showing
  through; 1080p60 delivered 363 frames in 6 s with 348 of 360 recorded
  frames distinct (daemon 54% of a core in a debug build); custom CSS, a
  file in a folder with a space, https://example.com, and validation
  errors for a bad scheme or no url/file; the hidden page window is left
  out of window lists.
- 4.7b (2026-10-11): `control_audio = true` sends a browser source's sound
  (Web Audio and media elements) to the mixer and ISO files as its own
  stereo audio instead of the speakers; without it pages play aloud, as in
  OBS. Media autoplays; local pages can fetch the files next to them.
  Verified: a page beeping every second recorded beeps exactly 1.000 s
  apart at the page's level (0.5), its flashes within one frame (audio
  24–57 ms later); two looping <audio> files (one created by script) at
  their file levels; system audio recorded alongside had none of it, and
  with control_audio off had the beeps at the page's −26 dB; fetch() of a
  neighbouring JSON works (fails without the preference); unit tests for
  message decoding, timestamps and resampling.
- 4.7c (2026-10-11): browser sources take `shutdown_when_hidden` (unload
  while the program doesn't show the page, leaving an empty frame, and
  load it afresh when shown) and `refresh_when_shown`; `refresh_browser`
  (CLI `isocast refresh`, MCP `refresh_browser`) reloads a page while
  recording, bypassing the cache. Verified with a page whose bar shows its
  age: switching away at 2.8 s and back at 5.9 s, the ISO file showed the
  page running on (refresh_when_shown) or empty (shutdown_when_hidden)
  while hidden, then 0.1 s old at 6.0 s; `isocast refresh` reset it to
  0.1 s; no reload at the start; errors when idle and for a non-browser id
  (listing the browser sources); `--json`; the MCP tool is listed.
- 4.8 (2026-10-11): a scene item can show another scene (`scene = "id"`
  instead of `source`), drawn on its own transparent canvas-sized texture
  and then like a source, so crop, radius, border, shadow, transform and
  opacity apply to it as a whole; nesting can go 8 deep; cycles (through
  hidden items too) are refused naming the chain. Sources inside nested
  scenes are on air when the outer scene is. Verified: snapshots of a
  three-level setup; at full size the nested red at 80% read exactly
  (186, 51, 58) over blue, empty parts of the nested scene the blue
  (32, 64, 96) and its border (210, 217, 223); a program recording with
  the outer scene on air; a browser source three scenes down with
  shutdown_when_hidden stayed loaded while it was on air, emptied when cut
  away and reloaded when cut back; unit tests for references, cycles and
  the drawing order.
- 4.9a (2026-10-11): video sources take `filters` (a kind registry like
  sources'), run on the GPU in order wherever the source is shown (the
  program, scene and source snapshots; ISO files stay as captured), and
  changes apply to a running recording at once; Color Correction is the
  first. Verified by snapshot pixels against hand-computed values:
  brightness, gamma, contrast, saturation, hue shift, multiply, add,
  opacity, a disabled filter and two chained filters all exact; the same
  in a scene snapshot and (within HEVC rounding) the program; a patch
  mid-recording turned the program from green to blue; the 5K display
  through a filter at 1080p60 dropped 0 of 480 frames; validation names
  the field and range.
- 4.9b (2026-10-11): `chroma_key` (key colour type green/blue/magenta/
  custom, similarity, smoothness, spill, opacity, contrast, brightness,
  gamma; OBS's defaults 400/80/100), `color_key` (80/50) and `luma_key`.
  Verified over a magenta background: a dull generated green screen was
  removed completely with OBS's defaults while skin and grey patches kept
  their colours and a skin/green fringe went partly transparent with its
  green reduced; exact green was removed by the colour key and a near
  green kept at 0.674 opacity (0.677 predicted for its actual pixel);
  luma max/min removed white and black, kept grey, and a soft edge gave
  the predicted 62 of 128.
- 4.9c (2026-10-11): `crop_pad` (edges, negative pads, or an area),
  `scaling` (a size or an aspect; point, bilinear, bicubic, lanczos or
  area; undistort) and `scroll` (speeds, a window, loop). Filters can now
  change the picture's size and move with the frame's time. Verified with
  generated images: crop, pad and area exact in size and pixels; point
  scaling kept the quadrants exact; aspect 2:1 stretched to 200×100;
  undistort put 137 at x=125 against 137.2 computed (160 without); a
  scrolling line in the program moved exactly 2 px per frame at 60 px/s,
  wrapped at 200 px, scrolled off once without loop, and a 50 px window
  scrolling up showed the image's lower half after 2 s and wrapped at 4 s.
- 4.9d (2026-10-11): `sharpen`, `image_mask` (colour or alpha mask,
  multiply/add/subtract blends; the image covers the picture keeping its
  shape, or stretches) and `lut` (.cube, or .png as 512×512 tiles or a
  strip; amount). Files load once per path; a missing one skips its
  filter with one log line. Verified with generated images and tables:
  a step edge sharpened to exactly 0 and 255 beside it with flat areas
  unchanged; each mask and blend mode gave its expected colour; cover and
  stretch differed as designed; identity .cube and .png LUTs returned
  (64, 128, 192) exactly, inverting ones (191, 128/127, 63), half amount
  127; a missing LUT left the picture as it was.
- 4.9e (2026-10-11): `render_delay` (0–500 ms) holds a source's picture
  back in the program, at its place in the chain, keeping GPU copies of
  recent pictures (snapshots skip it). Verified with the beeping, flashing
  page and its audio in the program: without a delay its flashes led the
  beeps by 21 ms every second; with 200 ms they trailed by 174–208 ms,
  200 ms later within one 30 fps frame.
- 4.9f (2026-10-11): `recording.iso.filtered = true` records sources with
  filters as filtered: each such ISO source gets a GPU stage that runs its
  live filters and draws into frames of the chain's output size (fixed
  when the file starts; later size changes are scaled to fit). Verified:
  a source with crop (left 50) and a 120° hue shift recorded a 50×100 ISO
  file whose top-left was blue (the green quadrant, turned) and bottom-left
  white, alongside an unfiltered source and the program, 90 frames each,
  0 dropped; with `filtered = false` the same file was 100×100 and red at
  the top-left, as captured.
- 4.12a (2026-10-11): scene items take `fit` (stretch by default, contain,
  cover, width, height, scale_down, none: OBS's bounding box types) and
  `align` (nine positions); the rect clips the picture and the item's
  background shows behind it. Verified by snapshot: 14 items (every fit,
  six alignments, a crop) matched a per-pixel model on all 140,000 pixels
  within 0.5 levels (native size exactly); a nested scene letterboxed in
  the item's colour; a 50% PNG over navy read (128, 128, 192) as computed;
  a 4:2:0 clip with cover put its edge at x≈469.9 as computed; the program
  frame matched the snapshot within HEVC error. (Built in parallel by an
  agent in a worktree; merged and re-checked on main: a fixed scene of an
  image, a video file, text and a colour snapshotted identically, max
  difference 0, before and after the merge, and display capture renders
  correctly.)
- 4.12b (2026-10-11): `scale_filter` per item (point, bilinear by default,
  bicubic, lanczos, area), with kernels shared with the Scaling filter.
  Verified: every method enlarging 10× and shrinking 4× matched its kernel
  model within 0.5 levels (point exactly, away from texel-edge ties); point
  keeps a 4:2:0 clip's red/blue edge hard; a 1080p60 program drawing a 4K
  pattern with lanczos, area and bicubic dropped 0 of 300 frames.
- 4.12c (2026-10-11): `blend` per item: normal, additive, subtract,
  screen, multiply, lighten, darken, as GPU blend states on premultiplied
  colour. Verified: 28 pixels (opaque, opacity 0.5, a box, inside a nested
  scene, all seven modes) within 0.5 levels of the formulas; the program
  recordings matched within HEVC error with 0 dropped frames.
- 4.13a (2026-10-11): sources take `audio.balance` (−1 left … 1 right) and
  the setup `[audio] pan_law` (`sine` by default, `square`, `linear`: the
  classic laws relative to the centre and capped there, so 0 keeps both
  sides as captured and ±1 silences the far side); it changes a running
  mix at once (`isocast audio SOURCE --balance X`, MCP `set_audio`).
  Verified per channel with 0.5 tones: at ±0.5 the far side read 0.2707
  (sine, 0.2706 expected), 0.3536 (square), 0.2501 (linear); a mono file at
  −0.25 read 0.3930/0.4330/0.3750; full right left −84 dB; live changes
  took effect from the next beep. Re-checked on main through the signed
  app: left/right ratio 0.5417 at sine 0.5 (0.5412 expected).
- 4.13b (2026-10-11): `audio.monitor` (`off`, `monitor_only`,
  `monitor_and_output`) and `[audio] monitoring_device` (`default` follows
  the system output, or an output's id or name); outputs in `isocast
  devices`; `isocast audio SOURCE --monitor MODE` and MCP change it live;
  status shows the monitor. Each source's audio goes to the monitor as
  captured, after gain, mute and balance, and a Core Audio output unit
  plays it. Verified silently (`ISOCAST_MONITOR_SILENT=1`): a −6 dB tone at
  sine 0.5 read L −17.354 / R −12.021 dB in the monitor (−17.353/−12.020
  expected); 48.0k frames/s with 0 underruns over 22 s, 14–25 ms
  buffered; a missing device is a status error and recording continues.
  Re-checked on main through the signed app: a monitor-only tone was absent
  from the program and kept in its ISO track, the monitor ran on the
  headphone output with 0 underruns. The audible check is Ryan's (Ready
  for Ryan).
- 4.10a (2026-10-11): sources take `audio_filters` (registry
  `audio_filter_kinds!`): gain, invert_polarity, noise_gate,
  noise_suppression (RNNoise via nnnoiseless), compressor with
  `sidechain_source`, limiter, expander, upward_compressor, equalizer, run
  by the mixer on each input just before it is mixed; sidechains hear the
  other source aligned (after its filters), a sidechain-only source is
  opened but not mixed, noise suppression's 20 ms delay is fed ahead, and
  changes reach a running mix at once. Verified (agent, in-process) with a
  1 kHz tone stepped −40/−20/−6/−30/silence/−30 dBFS: gain −6 →
  −46/−26/−12/−36; invert correlation −1.00000; gate cut −40, passed
  −20/−6, held −30 open after loud and shut after silence; compressor
  4:1@−18 −6 → −15.00; limiter −12.00; expander 3:1@−25 −76.02/−46.02;
  upward 2:1@−20 −30/−25; EQ −12/0/+6 → −11.38/+0.01/+5.93 dB at
  100 Hz/1 kHz/10 kHz; `say` speech plus pink noise: pauses −42.10 →
  −68.61 dBFS, speech −18.88 → −18.94, aligned at 0 samples; music −12
  ducked to −37.19 (25.19 computed) under a −10 dB voice. Re-checked on
  main through the signed app after merging with 4.13: gain −6 gave 0.2507
  (0.2506), compressor 4:1@−18 −14.97 dBFS with balance kept.
- 4.10b (2026-10-11): `recording.iso.filtered` also records audio through
  its filters (per-source chain in the audio runner, stamped back by its
  delay); sidechains are left out of ISO files. Verified: every filter's
  ISO levels equal the mix's; noise suppression in an ISO track −41.95 →
  −67.65 dBFS, aligned with the program; ducked music stayed −12.00 in its
  ISO track while the program had −37.19.
- 4.11a (2026-10-11): transitions are a kind registry (swipe, slide,
  fade_to_color, luma_wipe with 12 procedural patterns or an image, move)
  drawn by one GPU pass over both scenes' canvases (Move interpolates the
  scenes' layers); scenes take `transition` overrides (switch > scene >
  setup); `duration_ms` defaults to 300. Verified (agent) on gradient
  scenes read back per frame: slides and swipes within 1 px of the eased
  offsets, fade linear, fade to white peaking at its 25% switch point,
  luma softness within 0.02 of the formula, an inverted circle on the
  computed frames, all 12 patterns by eye; Move's box centroid within
  0.6 px and area within 1%. Re-checked on main through the signed app
  with the real display, camera and mic at 1080p60: slide (scene
  override), circle wipe, move and fade in one 12 s recording, 720/720
  frames and 0 dropped in the program and both ISO files.
- 4.11b (2026-10-11): stingers (cut point, or a side-by-side/stacked track
  matte) play their video over the switch, opened with the session and
  started a frame ahead; media with alpha decodes to BGRA; restart-on-show
  media waits with its first frame decoded. Verified (agent) with ProRes
  4444 stingers: the video's time correct on every frame (±0.001 s), the
  cut on exactly the 500 ms frame, straight alpha, the matte edge on every
  frame, 7 reuses on schedule. Stinger audio is not played yet.
- 4.14 (2026-10-11): `isocast setup import-obs COLLECTION [-o OUT]
  [--canvas WxH[@FPS]] [--profile P] [--json]` (daemon `import_obs`, MCP
  `import_obs_collection`) reads an OBS scene collection into a setup:
  sources with ids from their names and devices matched to this Mac,
  scenes and items (OBS's anchor-and-rotate placement, all bounds types,
  crop, groups as children, nested scenes, scale filter, blend mode),
  video and audio filters, balance and monitoring, Source Record as ISO
  sources, the current transition (cut, fade, swipe, slide, fade to
  colour, luma wipe, stinger, move), and the canvas, frame rate and
  recording settings from the collection and its profile; it lists
  everything else with the reason. Verified (agent): a synthetic
  collection's snapshot matched 25/25 hand-computed OBS pixel positions
  (12/12 rescaled to 1920×1080) and recorded 60 frames, 0 dropped.
  Re-checked on main through the signed app: Ryan's Recording collection
  imported 8 sources and 2 scenes, Rebase 11 sources and 7 scenes with its
  noise suppression as an audio filter, both passing `setup check`.
- V1.1 (2026-10-12): the live engine (`record::Engine`): sources, the
  program compositor and the program mix run while the daemon is live;
  recordings attach and detach on it (and `tap_program` gives a stream
  the program's frames and mix); `apply` changes scenes at the next frame
  and opens/closes sources in the background; `frame` serves JPEG frames
  of the program, studio preview or a source (long-poll), `engine`
  holds it, `levels`/`source_changed`/`engine_changed` events; CLI `frame`,
  `engine start|stop`; MCP `live_frame`, `live_engine`. Verified through
  the signed app with the real screen (3840×2160), Studio Video, Studio
  Audio, a test pattern and a tone: engine start 1.8 s, preview 60.2 fps
  at 1280×720 over the socket, a recording started in 2.1 s on the running
  engine while switching scenes and moving the camera: program, SCREEN
  and FACECAM files 480/480 frames at 60 fps, 8.000 s, 0 dropped, 0 audio
  dropouts, scene timeline in the manifest; program flashes and beeps
  aligned exactly; `isocast record` (in-process) still records two test
  patterns + tone with flashes aligned across files and beeps within
  0.1 ms; a source added and removed while running with 0 missed program
  frames; idle stop after 15 s and a cold restart in 0.44 s; ~10% of a
  core while running (debug build).
- 4.12d (2026-10-12): scene items' `show_transition` / `hide_transition`
  (`fade`, or `slide` in from / out to a side within the item's rect, with
  duration and easing) play when `visible` changes in the live engine; the
  OBS import maps fade, slide and swipe. Verified through the signed app:
  a red box hidden with a 1 s linear slide right covered its rect 1.0 →
  0 over exactly 1.0 s (clipped to the rect mid-way) and shown with a 1 s
  fade rose 0 → 1 over 1.0 s, in a recorded program file.
- V1.1 soak (2026-10-12): 10 min recording on the live engine of the real
  screen (4K ISO), Studio Video and the program with Studio Audio and a
  tone: 36000/36000 frames in each file, 0 dropped, 0 audio dropouts,
  600.000 s audio and video, beeps at the same sub-frame offset at the
  start and the end (no drift), daemon RSS steady at 200–235 MB, 30–55%
  of a core (debug build).
- V1.3 (2026-10-12): the live setup persists in `setup.toml` (atomic save
  with `.bak`; a file that won't load is kept, reported in status/get,
  and moved aside on the next apply). `isocast setup init` (method,
  MCP) builds Screen + Camera / Camera / Screen from this Mac's devices,
  ranking real cameras over virtual ones, with refused devices as
  placeholders; `isocast permissions [--request] [--open X]` gives each
  permission's state, next step and System Settings link. Verified (agent)
  with real-device `setup check`/`doctor` and an 8 s synthetic recording;
  on main through a signed copy: `setup init --camera "Studio Video"
  --microphone "Studio Audio" --apply` then a 10 s recording of the real
  screen and camera: 600/600 frames per file, 0 dropped.
- 4.15 (2026-10-12): unknown fields on sources, filters, audio filters and
  transitions are reported with their path, a guess and the kind's
  fields, in TOML and JSON, plan, apply, MCP, scene switches and the saved
  file. Verified (agent) with an eight-typo setup in both formats.
- V1.2 (2026-10-12): streaming of the program as VideoToolbox H.264 (High,
  no B-frames, keyframes every 2 s) and AAC over Isocast's own RTMP/RTMPS
  client; presets for Twitch, YouTube, Facebook, Kick and custom; the key
  in the Keychain (never in setups, status or logs); reconnects; drops
  video under congestion; bitrates, drops and congestion in status; CLI
  `isocast stream …`, MCP. It taps the live engine, independent of
  recordings, at most 1080p by default. Verified (agent) against local
  ffmpeg RTMP and an RTMPS terminator: A/V within 0.2 ms, keyframes every
  2.000 s, reconnect after the listener was killed, an 8 s stall with 0
  recording drops, 1440p60 sent as 1080p60. On main with real devices:
  12 s to a local listener (H.264 High 1080p60 + AAC-LC 48 kHz, keyframes
  every 2 s) while a 10 s recording of the screen and Studio Video wrote
  600/600 frames per file, 0 dropped, with a scene switch mid-stream.
- V1.4 (2026-10-12): the desktop app (`crates/isocast-app`, see
  docs/app.md): an AppKit window hosting the web interface (built in,
  served at isocast://app/), a thin client of isocastd through the shared
  `isocast-client` crate, with live frames, events, menus, a snapshot mode
  (`isocast-app --snapshot`) and `scripts/app-dev.sh [--release]` building
  a signed Isocast.app. Verified (agent) with snapshots and the CLI
  unchanged; on main in a real window through the signed engine.
- V1.5 (2026-10-12): scenes, sources, canvas editing (move, Shift-resize,
  Option-crop, snapping, nudging), inspector, studio mode, undo; every
  edit through `apply`. Verified (agent) by scripted pointer/key events
  and reading the live setup back.
- V1.6 (2026-10-12): mixer meters and faders; Record and Go live with
  timers and health popovers, marker and split. Verified (agent) by a
  recording through the app and a stream to a local ffmpeg listener
  (H.264/AAC, 14 s, 0 dropped).
- V1.7 (2026-10-12): settings (stream, recording, video, audio) and the
  first run (permissions, devices, layout → `init_setup`). Verified on
  main by clicking through the real window against a fresh signed engine:
  permissions showed allowed, Studio Video and Studio Audio chosen, a
  scratch recordings folder typed in, "Create setup" landed in the main
  window with the screen and the camera inset live; Record, key 2 to the
  Camera scene, Stop wrote program, SCREEN and CAMERA files with the
  switch in the manifest (with three test daemons recording at once the
  4K screen and program dropped frames; repeated 15 s recordings under
  the same load dropped none).
- V1.2 real service (2026-10-12): with Ryan's go-ahead and his key (set by
  him in the app, kept in the Keychain), a separate test engine streamed
  a synthetic test card (test pattern, tone, text; no screen, camera or
  mic) over RTMPS to X's ingest (rtmps://ca.pscp.tv:443/x) for 61 s:
  live throughout, 1802 frames at 30 fps, 0 dropped, 0 reconnects, a clean
  stop. The near-static card encoded at ~150–250 kb/s against a 6000 kb/s
  target (see the low-bitrate gotcha).
- V1.2 X live tests (2026-10-12): 1080p30 and 1080p60 over RTMPS to X at
  9000 + 128 kb/s CBR with a test card: 5 min each, 15 min at 60 fps while
  recording the program and two ISOs, and a stop/restart with live scene
  and text changes; all with 0 dropped frames, 0 audio dropouts and 0
  reconnects, 10 s video samples 8800–9277 kb/s, audio 124–132; the
  recording's three files had 51453 frames each at 60 fps (ffprobe), and
  the program file decoded without errors. Also fixed: center/right text
  alignment in a fixed-size text box, and `stream stop` reporting 0 kb/s.
- V1.8 perf-audio/events (2026-10-12): bounded, shared event fan-out
  stopped memory growth under 64 stalled readers (late sample 572→543 MB);
  all stalled clients disconnected, eight healthy clients each received
  285 levels, 12 status, started and stopped events. The real rig wrote
  three 12 s files with 720 frames each, zero dropped frames/audio;
  synthetic program/ISO sync stayed within 1.167 ms (AAC onset tolerance),
  and silent monitoring reported zero underruns.
- V1.5–V1.7 redo, OBS layout (2026-10-12): the app now has OBS Studio's
  places and words — preview with the source toolbar, Scenes | Sources |
  Audio Mixer | Scene Transitions | Controls docks, status bar (LIVE/REC,
  dropped frames, CPU, FPS from new `status` fields), Studio Mode with the
  transition column, Properties/Filters/Edit Transform/Advanced Audio
  dialogs instead of the inspector, OBS-style Settings and menus, and the
  Low→Incredible quality slider. Verified by snapshots of every view at
  1024–1920 px, scripted behaviour checks, a recording and a local stream;
  the real window, title bar and menus checked in the test VM.
- Smooth canvas drags (2026-10-12): the picture moves with the selection
  box: live applies (no fsync per step), steps every display frame, H.264
  monitors at 60 fps while working on the canvas, and the box drawn from
  the displayed frame's layout. Verified with scripts/drag-lag.py: gap
  between box and picture 0 px median (0.6 p95), both moving 45 times/s
  in a 60 fps recording of the window; decoder 60 frames/s during drags.
- Device dropdowns (2026-10-12, Ryan's request): every setting with known
  choices is a dropdown of what the hardware offers — camera device,
  Resolution (one control for width/height) and Frame rate per size,
  microphones, displays (saved by id), windows (app, then title, with
  "part of a title" free entry), application capture, sidechain and
  aspect, the monitoring device; unplugged values stay visible; a refresh
  re-reads devices. Camera Auto picks the highest rate up to the canvas
  rate, then the largest size that fits the canvas (either orientation).
  Each running source reports its measured `received_fps` in `status`.
  Verified by an implementer, three independent reviewers (12 issues, all
  fixed) and read-back of applied values on a test engine.
- V1.8 setup-obs (2026-10-12): canvases must be even, 16–8192 (a 16385-wide
  canvas crashed the engine start and hung every later `start`; a 1281×721
  one recorded 1280×720 silently); a panicking engine start is reported
  instead of hanging. Audio-only media in an off-air scene stays silent
  (880 Hz band -91 dB off air, -21 dB from the switch at 1.95 s; with no
  item it still plays). OBS import uses the recorded track (RecTracks=2 →
  track 2's source), lists extra tracks, the scaled output size and the
  scene on air, and missing files/folders get their own errors. Verified
  on release builds against the finder's repros on an isolated daemon.
- V1.8 m4-bridge (2026-10-12): desktop-session bridge uses Interactive
  process policy. A standalone 10 ms sleep fell from ~75–85 ms to ~13–14 ms;
  launchctl confirmed interactive spawn type after reloading an idle bridge.
  Release build, fmt, clippy -D warnings and 94 tests passed on exonbook.
- V1.8 m4-iso (2026-10-12): all recording outputs use asynchronous
  AVAssetWriter with independent bounded input queues; removed the direct
  HEVC session and its ISO-only flag. Four interleaved synthetic flagship
  runs per build on M4: old branch 1930 frame drops/966 audio dropouts,
  fallback 1065/0, main 1116/265. All 36 decoded frame counts matched the
  manifests; MP4 splits had zero drops and flash/beep sync within 0.021 ms;
  SIGKILL left three decodable 6.016 s MOVs. All five presets record with
  the expected HEVC depth. Release, fmt, clippy and 94 tests passed.
  Remaining video back-pressure and M5 requalification are explicitly blocked.
- N.1 (2026-10-12): the Slint app (crates/isocast-ui) shows the daemon's
  program zero-copy (shared_frames: an IOSurface ring per viewer over a
  Mach service) as a native layer with the selection box drawn from each
  frame's revision. Two independent verifiers on an M2 test Mac, real
  window, interactive scheduling: 2.0-2.9% of a core at 60 fps (web app
  plus WebKit ~35%), 0 ms box/picture lag and 0 mismatches over thousands
  of frames (real pointer drags too), 10-minute soak 36,069 frames with 0
  black, AXList "Scenes" with on-air state. Merged as d39c3e4; recording
  A/B before/after the merge on exonbook identical (0 drops, 44.9% vs
  45.2% CPU); 99 tests, clippy and fmt clean.
- V1.8 robustness (2026-10-12): stop during an engine warm-up cancels the
  start (status `starting`, `last_error`), stop never returns an older
  report, killed recordings are marked failed and reported (start note,
  doctor), keep_free_gb = 0 still stops cleanly before the disk fills, a
  stop right after start keeps a 1 s take, live drop counts no longer
  count a frame still being written, a slow disk test isn't cached, and
  clearer space/filename/missing-source errors. Each reproduced on main
  and re-run on the fix with synthetic sources on an isolated daemon
  (FIFO-held disk tests, kill -9, a 300 MB RAM disk, 300 s loaded runs).
- V1.8 agent-surface (2026-10-12): merged as fc39214 (12 of 14 fixed, each
  reproduced on main first). Re-checked on the merged build on exonbook:
  all 30 MCP tools' outputSchemas are objects (the official SDK rejected
  the list before).
- V1.8 robustness (2026-10-12): merged as 43c48cc. Re-checked on the
  merged build on exonbook with synthetic sources: a stop 0.3 s into a
  start keeps a 1 s take (60 frames, 0 dropped) instead of recording on;
  after kill -9 mid-take, doctor warns and the next start notes the
  interrupted session and marks its manifest "failed"; a daemon shutdown
  mid-take finalizes all three files (60 frames each, ffprobe-counted).
- V1.8 streaming (2026-10-12): merged as 4b9e677 (all 9 fixed). Re-checked
  on the merged build on exonbook: 20 s to a local RTMP listener, 1080p60
  H.264 + AAC received, 1191 frames sent, 0 dropped, 0 reconnects, 6239
  kb/s for a 6000 kb/s constant-bitrate target, keyframes on the 2 s grid
  after a start pair (0.000/0.017 s; follow-up queued). All four V1.8 hard
  test areas merged: 37 of 40 findings fixed; fmt, clippy -D warnings and
  99 tests clean on exonbook.
- UI polish (2026-10-12): merged as ab51aae. docs/design.md is the design
  language (tokens for the web app now and the Slint app in N.2). 49
  views snapshotted before/after on an M2 (pairs under
  /Volumes/IsocastBuild/ui-polish/build/pairs and refine/half). Drag A/B
  on isocast-m2-b, old vs polished UI, 2 runs each: lag 0 ms in all,
  picture moves 39.5/47.8 vs 38.7/40.0 a second, median gap 0.0/0.7 vs a
  constant 1.4 px (the selection box's new 2 px outline ring shifts the
  detected outline).
- V1.8 throughput-diagnostics (2026-10-12): 48 locked synthetic recorder
  runs on the M4 and both M2s; all 144 file counts matched manifests (42
  decoded, 102 indexed). Traced hardware VT completions at ~42 fps in
  failing M2 flagship runs and EncodeFrame calls up to 97 ms. A bounded,
  separate encoder worker preserved audio but still dropped video; it and
  the other policy experiments were removed. Byte-identical flagship-1080
  clips, four interleaved pairs per M2: zero drops in all 48 files, baseline
  daemon CPU 51.0% (27.0) vs 49.95% (26.7), VT 28.55% vs 24.55%. Sample,
  spindump and powermetrics evidence plus unresolved limits are in
  `/Volumes/IsocastBuild/astra-throughput/scratch/REPORT.md`. Local fmt,
  clippy --all-targets -D warnings and existing tests passed.
- V1.8 stream-start-keyframe (2026-10-12): a stream (and each reconnect)
  starts with the keyframe asked for at connecting, and the 2 s grid from
  it: a keyframe the encoder made before the connection no longer slips in
  a frame earlier. 20 s 1080p streams to a local ffmpeg RTMP listener on
  isocast-m2-b, keyframes by ffprobe: main had 0.000/0.017 then 2.017…
  at 60 fps and 0.000/0.033 then 2.033… at 30; now 0.000, 2.000 … 18.000
  at both. With the daemon SIGSTOPped 30-80 ms every 0.1-0.4 s (seeds 3
  and 4, 12-121 frames dropped) no pairs; off-grid keyframes only where the
  grid frame was missed (12.033, 14.05, 16.05), back on the grid next. A
  reconnect mid-stream: the second connection's file also 0.000, 2.000….
- V1.8 mcp-field-paths (2026-10-12): every parameter error names its field
  path and lists the valid options, on the socket and in MCP, wire format
  unchanged. Sent 125 wrong params (50 MCP tool calls, 75 socket requests)
  to an isolated daemon: all name the field (before: no socket error had a
  path; MCP stopped at `transition`, `setup.sources[0]` or nothing for
  init_setup's options), e.g. `params.setup.sources[0].filters[0].gamma`,
  `transition.direction`, `layout`. A valid patch with a chroma key, gain
  filter and swipe applied, saved and reloaded from TOML; a 4 s synthetic
  recording with two switches: 120/120 frames in both files, 0 dropped.
- V1.8 mcp-events (2026-10-12): MCP's `get_events` reads the newest 500
  of isocastd's events (cursor, kinds, limit, wait), kept by a watch in
  the MCP server that reconnects after restarts. Real stdio sessions
  (initialize, tools/list: 31 tools): locally a pinned-source warning
  arrived at once; on isocast-m2-a the synthetic flagship's first drop
  warning came 2.0 s into the take ("program dropped 1 frame(s) at 2 s")
  and the report had 523-547 drops per file. A reader that never polled
  through 40000 applies + 40000 warnings + 20 s of recording held 500
  events and RSS 33.1-33.5 MB (M2: 4000 applies, 23.8 MB flat, all 73
  drop warnings kept). restart_isocastd, SIGTERM and SIGKILL mid-take
  each gave connection lost/back events (SIGKILL's says the take didn't
  stop), with the watch back before the next tool returned.
- V1.8 encoder-capacity (2026-10-13): `isocast tune` reports `capacity`
  (fits >= 1.25x, no_margin, cannot_keep_up < 1x); when the setup doesn't
  fit it benchmarks changes closest first (program file at 1440p/1080p via
  the new recording.program.width/height, a lower preset, half the frame
  rate, one ISO fewer, combinations) and recommends the closest that fits
  as a patch; its measurements are kept in <setup>.tune.json and `start`
  notes (CLI, MCP, warning event) when a recording can't keep up by them,
  naming the fix. A canvas fps change now reopens the sources that follow
  it. isocast-m2-a: flagship 0.70-0.71x cannot_keep_up (recorded
  1044-1054/1440), 1080p program 1.04x no_margin (1440/1440 x3), 30 fps
  1.75-1.79x recommended: start of the flagship got the note with the
  patch; after the patch no note and 720/720 x3, 0 dropped; MCP
  start_recording returned the note; a heavier unmeasured setup got the
  "asks more than tune measured" note; flagship-1080 (the 1080p-program
  files, no_margin) none.
  exonbook (M4 Max): flagship fits 1.28x twice, no note, 24 s 1440/1440
  x3; at 120 fps 0.70x, a 1080p program didn't help (0.70x), 60 fps
  recommended. fmt, clippy -D warnings and tests clean.
- V1.8 encoder-capacity, re-check (2026-10-13): merged as 7abe7bc; on
  isocast-m2-b (macOS 26.7) the merged build's tune gives the same
  verdicts as on m2-a (4K program 0.71x "can't keep up", 1080p program
  1.05x "no margin", 30 fps 1.83x recommended) and start prints the
  warning with the patch. All V1.8 follow-ups merged; fmt, clippy and 99
  tests clean on exonbook.
- S.1a (2026-10-13): `scripts/obs-bench.py` runs the same synthetic
  scenes in Isocast and OBS 32.2.2 (OpenGL and Metal), driven through
  obs-websocket from an isolated OBS home, interleaved 3 rounds x 4
  scenarios on exonbook (M4 Max) and isocast-m2-a (M2), twice. Same work:
  Isocast at 0.47-0.56x OBS's CPU (app + its VT services: 43-48% vs
  80-91% of a core), 27-66% less GPU time, 2.7-8x less memory, 30-49% less
  machine power, launch 0.06 vs 0.4-0.7 s; on the M2 at 4K60 OBS kept 27%
  distinct frames (its log: 82% skipped for encoding lag), Isocast 99.5%.
  Isocast's flagship (3 files) used 67-69% CPU vs OBS's 89% for 1 file but
  dropped 3-4% (M4 Max) and 27% (M2) of frames; recording stop 0.34 vs
  0.14 s. Verified with ffprobe (decoded frames, codecs, 1 s keyframes,
  B-frames, Rec. 709 tags, bitrates), OBS's own log (CRF 0.70, keyint 1 s,
  AAC 256 CBR, Hybrid MOV, hardware decoding) and repeated-frame counts.
- P.1 (2026-10-13): named scene collections (scenes, sources,
  transitions, canvas size) and profiles (frame rate, recording, stream,
  audio) stored side by side in `setup/` and swapped live: library,
  library_get/create (blank or copy)/rename/delete/switch/import/import_obs
  on the method table, `isocast collection|profile …` (--json), eight MCP
  tools, a library_changed event; switches refused while recording or
  streaming. Isolated engine (synthetic sources for the engine):
  examples/recording.toml saved by main's isocastd migrated to the same
  live setup (1598 bytes of JSON), backup byte-identical, setup.toml
  untouched; collection+profile switched 1080p60 → 720p30 with the engine
  running, then a 6 s take: program 1280×720 HEVC 60 fps 360 frames, ISO
  files 180 frames each at 30 fps, AAC 282 frames, 0 dropped; refusals
  while recording (3) and streaming to a local RTMP listener (2); Ryan's
  OBS Recording collection and his three profiles imported (combined
  `setup import-obs` unchanged from main); main's isocastd on the migrated
  folder left all 25 library files byte-identical and its change was taken
  in as a new pair at the next start; MCP 39 tools exercised.
- stream audio CBR (2026-10-13): stream AAC now uses AudioToolbox's
  constant bitrate mode (X's ingest had flagged "audio bitrate 17 kbps,
  recommended 128"). Local RTMP listener, 20 s each, 1080p30 and 1080p60,
  silence / quiet tone (-51 dBFS) / loud tone (-6 dBFS): every run
  averaged exactly 160.0 kb/s (target 160) or 128.0 (target 128),
  per-second 157.0–161.2 / 125.4–129.0 (46 or 47 AAC frames per second);
  before, silence sent 2.2 kb/s (6-byte frames), a quiet tone ~21 then
  142, a loud tone 84–130. Video held 6000.0–6000.2 and 9000.0–9000.2
  kb/s per second; no drops. Recordings keep the variable mode.
- N.2.0 (2026-10-12): the Slint app's foundation on slint/n2: all
  design.md tokens, a widget kit (buttons, fields, dropdown with
  duplicate-name details, choices, slider/fader, meter, lists, docks,
  forms, menus with submenus, popover, modal dialogs, tabs, toasts,
  tooltips) with keyboard focus and accessibility, and the window split
  into five areas over an App/Session core (any API call, the web app's
  edit queue). Verified offscreen against an isolated engine: 1440x900
  matches 01-main's layout, every dock is whole at 900x600, Studio Mode
  matches 03-studio, the gallery's dialog/menu/dropdown match
  08-create-source and 04-source-menu, an 8 s drag soak drew 0 black and
  0 drifted frames, --a11y lists rows with index, count and selected.
- N.2.A (2026-10-12): the Slint app's scenes and sources area on
  slint/n2-a-sources: Scenes and Sources docks (rename inline, drag to
  reorder, groups fold, ISO/eye/lock, state badges, OBS's scene, item,
  list and preview menus), source toolbar quick settings for every kind,
  Create/Select Source (drafts for sources that need a file), Properties
  and Filters drawn from the setup schema with the source's live picture
  (shared_frames with the source as target), Edit Transform, Rename,
  copy/paste, the Edit menu's Order and Transform, system open and colour
  panels. Verified offscreen at 1440x900 against an isolated engine beside
  18 of the web app's snapshots (pairs in /Volumes/IsocastBuild/
  n2-a-sources/pairs), and on isocast-m2-a in a real window (right-click
  menus, a row drag, Add files' open panel).
- N.2.B (2026-10-12): the Slint app's Audio Mixer and Advanced Audio
  Properties on slint/n2-b-audio, to the web app's behaviour: the mixer's
  sources (audio-only plus the scene's), hiding (kept per viewer), strip
  flags, fader (live, 0 dB on double-click), mute, both menus, smoothed
  meters with peak hold at 30 fps that rest in silence, and the table
  with balance, offset, monitoring and Program/ISO tracks. Verified
  offscreen on isocast-m2-b against the web app's 01-main, 43-mixer-menu
  and 15-adv-audio (same layout to a point), plus strip states, hidden,
  dock menu and 900x600; in a real window there (cua-driver): right-click
  menu, fader drag to −36.2 dB, double-click to 0, mute, Advanced Audio's
  balance drag (0.74) and ISO box all reached the live setup. CPU with
  three tones metering: 20–29% of a core (web app 33–37%), resting 2.2%
  (web app 19%).
- N.2.C (2026-10-12): area C on slint/n2-c-controls.
  - Status bar: the stream's dropped frames, rate and health, LIVE/REC
    timers that don't drift, CPU and FPS.
  - REC and LIVE details popovers.
  - The dropped-frames toast, updated in place, with Details.
  - "Recording saved" with Show in Finder.
  - Toasts over dialogs.
  - Scene Transitions as transitions.js (a stinger asks for its video
    first).
  - Transition Properties on the shared schema form (live edits, Cancel
    restores, drafts applied on OK).
  - Studio column and take.
  Verified:
  - Offscreen on isocast-m2-a, side by side with 13 of the web app's
    snapshots (01, 03, 16, 16b, 42, 44, 45, 46, 50, 51, 53, 63, 81).
  - A real stream to a loopback RTMP listener: live, Reconnecting when
    killed, then stopped.
  - A real window on the M2 driven through accessibility (cua-driver):
    record; REC details; stop; Fade to Colour from the dock; a live
    switch-point edit read back by `isocast get`; Cancel back to 50;
    Studio Mode; Bars into preview; Transition swapped preview and
    program.
- N.2.D (2026-10-12): Settings (all seven pages, the draft applied as one
  undoable patch, stream key to the Keychain, the quality slider), the
  first run (permissions, devices, layout, init_setup), Welcome (open a
  setup, import from OBS) and the connecting/failed states, on
  slint/n2-d-settings. Verified offscreen against isolated engines (every
  page and state side by side with the web app's references; OK saved the
  name, FPS and ISO to the daemon's file; import and open made the live
  setup on an empty engine) and on isocast-m2-b (devices listed, Create
  setup made "My setup"; the open panel came up from Browse in a real
  background window and cancelled cleanly).
- N.2.E (2026-10-12): the Slint canvas does what the web app's does:
  click/hover/move/resize/crop with snapping, guides and readout, rotated
  and locked items, right-click menus, the empty scene, Preview Scaling →
  Canvas, Studio Mode's two live pictures; gesture steps pipelined; native
  pictures tagged sRGB (BT.709 darkened them); --preview image caches the
  docks. Verified offscreen (snapshot states canvas-hover, canvas-drag,
  canvas-hold, preview-scale; side by side with 02/03/06/40) and in real
  windows on isocast-m2-a: clicks, drags and an Option-crop through
  cua-driver, Studio Mode in both modes, colours within 1/255 of the web
  app, drag-lag 46.8 picture moves/s median vs the web app's 47.3 with
  0.7 px gap, CPU image 21.3%/13.6% vs web 44.7%/27.3% (meters
  moving/silent), layers 2.5% silent. exonbook was locked (Waiting on
  Ryan), so nothing ran there.
- N.2.F (2026-10-13): the five areas merged on slint/n2 and made one app:
  one settings form (area A's, now src/form/ and the kit's SettingsForm,
  also under Transition Properties), one open panel, one device list,
  one set of snapping/preview-rate prefs; Sources row hover outlines the
  item on the canvas; the preview's item menu as the web app's; docks
  cached in layers mode too. Verified: fmt, clippy -D warnings, 110
  workspace tests; all 49 reference views snapshotted offscreen beside
  the web app's (contact sheets in /Volumes/IsocastBuild/n2-integrate/
  compare and compare2x); on isocast-m2-b (exonbook locked) in real
  windows: 60 fps silent 2.9% of a core layers / 14.6% image, metering
  13.2–14.4% / 20.4–20.9% (web app 45.2%), drag-lag 44.2 picture
  moves/s median, gap ≤1.4 px median / ≤2 px p95 (web app 48.8, p95
  5.5–8.2 px), a 10-minute --check run 36025 frames, 0 black, 0 lost;
  the real accessibility tree (cua-driver) lists the docks and an open
  Properties dialog's controls.
- N.2.G (2026-10-13): the critics' high-priority findings fixed on
  slint/n2 (b879255 behaviour and layout, 9462e64 type). Verified: fmt,
  clippy -D warnings, 111 workspace tests; the 49 reference views at 1x
  and 2x beside the web app's, every difference score down or level
  (mean −0.24; contact sheets in /Volumes/IsocastBuild/n2-fix/compare
  and compare2x); on isocast-m2-b in real windows with cua-driver:
  renames typed across a poll, transforms applied as integers, Create
  Source's field takes typing and its Cancel works by real click, a real
  click works in a menu opened by a background right-click, Escape
  applies nothing; drag-lag 44.2 picture moves/s median (43.3–46.5),
  gap 0.7 px median, ≤1.4 px p95 (web app 42.8, 38.0–50.5, p95 6.8–8.2
  px); a 10-minute --check run 36030 frames, 0 black, 0 lost, longest
  repeat 1. CPU that session read higher on m2-b for both builds alike
  (A/B with 907f4dd: metering 20.3–21.6% vs 20.3–21.0%; --preview image
  at rest 23.7% vs 23.2%; web app 47.3%).
- S.3a (2026-10-13): recording stop waits for delivered media and actual
  finalization instead of an unconditional 300 ms tail. Complete mixer
  inputs can run immediately; missing inputs retain the 200 ms allowance.
  The early-finish predicate compares frame ticks (separate nanosecond
  rounding caused spurious waits). Four interleaved main/candidate/OBS
  rounds: M4 1080p/4K stop 346/356.5 → 64.5/83.5 ms, OBS 127.5/144;
  M2 364.5/386.5 → 84.5/93 ms, OBS 151/2104 (OBS overloaded at 4K).
  All 16 candidate benchmark MOVs plus nine qualification files complete,
  zero drops, equal A/V durations; split/cancel and killed-fragment checks
  pass. Release fmt/clippy/tests clean. Full evidence and limitations:
  `/Volumes/IsocastBuild/astra-s3/scratch/REPORT.md`.
- S.3b (2026-10-13): meter peak/power reductions use independent SIMD
  lanes outside the lock, with odd-chunk parity against the monitor path.
  Profiles of all three scenarios on both Macs show a smaller meter
  hotspot; NEON confirmed in disassembly. Whole-process CPU is flat/mixed:
  M4 main/candidate medians 31.95/31.6, 33.55/33.5, 29.3/29.25%; M2
  34.8/34.0, 35.65/35.5, 29.55/33.35%. No broad CPU or memory win claimed.
  M4 IPC differences only 0.04–0.23%; memory investigation localized 336
  MiB to GPU activation, with a native trace proving renderer buffer
  churn for the compiled-scenes owner. M2 stream had one 11-video/29-audio
  drop run, then eight clean follow-up runs. Release gates and real-output
  qualification pass; report has all A/B tables and remaining limits.
- S.3a-f1 (2026-10-13): restored the unconditional 200 ms live mix budget.
  Stop requests bounded early delivery to recording routes; concurrent
  streams retain paced delivery of the same audio. This also fixes an
  intermediate Stop flush that caused three stream audio dropouts.
  Meter SIMD removed. Final normal streaming A/B: M2 10/10 and M4 4/4
  candidate runs zero video/audio drops, main also clean. Four stop pairs
  per resolution: M4 1080p/4K 363.5/363 → 71/65.5 ms; M2 340.5/372 →
  79/93.5 ms. All candidate MOVs complete with expected frames and equal
  A/V durations. Four concurrent lifecycle pairs stayed zero-drop; hard
  100/200/300 ms encoder suspensions leave recording intact and streaming
  no worse than main (over-budget drops remain visible). Seven safety
  cases rechecked. Release fmt/clippy, 104 unit tests and one doc test pass;
  REPORT.md Follow-up 1 has the complete evidence and bounded-stall limits.
- 5.0 (2026-10-13): compiled scenes. A scene thread compiles the live
  setup's scenes on every change into flat draw lists of 272-byte draw
  records (item node ids kept across changes); the compositor adopts a
  version with one atomic load per tick, keeps the records in one storage
  buffer read as `draws[instance_index]`, rewrites only the records a
  picture's size or format or a show/hide transition changes, caches bind
  groups by texture (captured frames get stable textures per IOSurface),
  sends the last frame again when nothing changed, and reports
  `compositor.cpu_us` (p50, p99), `reused`, `draws` and
  `scenes.resolve_us` in `status`/`engine` (CLI, --json, MCP; bench.py
  keeps them); scene snapshots and studio mode's preview draw the same
  lists. Verified on isocast-m2-a (pictures on exonbook too), against
  main: a fixed scene with every item feature (all fits and alignments,
  the 5 scale filters, the 7 blend modes, opacity, backgrounds, radii,
  borders, shadows, transforms, a group with a grandchild, a nested scene
  with crop/border/shadow, hidden and off-canvas items; colour, RGBA image,
  text and 4:2:0 video) snapshotted at full size and 640 wide, its nested
  scene, the program's canvas and studio mode's preview are identical
  (max difference 0; JPEGs byte for byte); 4.8's nested values rebuilt
  read (186, 51, 58), (32, 64, 96) and border (210, 217, 223) in both;
  4.12d's show/hide: slide hide 1.0 -> 0 over 61 frames and fade show 0 ->
  1 over 60, curves identical to main's. Compositor CPU per tick p50
  (5.0a -> 5.0, 3 runs each): flagship-1080 1173 -> 579 us (0.49x),
  flagship 4K 1580 -> 873 us (0.55x), a 40-item static scene 4784 -> 42 us
  with 1246-1247 of 1247-1248 program frames sent again; 0 dropped
  (flagship-1080, static), the 4K flagship drops what main drops (the M2's
  encoder limit). The static scene's HEVC program decodes to the same
  framemd5 sequence as 5.0a's in all 3 runs, and in ProRes 422 HQ to one
  identical picture in all 1380 frames in both. Allocations per tick
  (uncommitted counting allocator): 0 on sent-again ticks for 10, 40 and
  160 items; 66/72/77 on drawn ticks for 13/51/201 draws (wgpu's command
  recording). Flagship gate vs main (3 runs): flagship-1080 daemon CPU
  49.9 -> 42.4%, 0/0 dropped; flagship 45.7 -> 41.7%, 3474 -> 3470
  dropped. Scene transitions (fade, move, swipe, luma wipe) recorded 540
  of 540 frames on exonbook.
- 5.1 (2026-10-13): layout. Items and scenes take `layout` (CSS flexbox,
  grid, block and absolute positioning in snake case, laid out by taffy
  0.10 on the scene thread when the scenes or their pictures' sizes
  change, never per frame); `rect` is optional (rect or layout, not both;
  a rect is absolute at its pixels, and scenes without layout skip
  taffy); pictures size themselves from the picture as drawn (through
  filters; a 16:9 shape until known), with a least size of 0;
  `overflow = "hidden"` clips children to the rounded rect (clip records
  in a second storage buffer); `get --resolved` (CLI, --json, MCP
  get_setup `resolved`) returns every item's rect, canvas box and
  transform with a layout generation that program frames carry (H.264
  revision records, shared and JPEG frames); field-path errors from
  check, plan, the socket and MCP. Verified on isocast-m2-a and exonbook
  (same results): snapshot pixels for a flex row of three 320x180
  pictures (padding 40, gap 20) at x = 40, 380, 720, centring at
  800,450, a flex_grow 1:2 column (flex_basis 0) split at 360, a 1fr 2fr
  grid at 640, inset/%/aspect_ratio 2 with fit contain at 480,108
  960x480, an overflow clip cut at the card's edge and corner, the same
  on air (by luma), Ryan's facecam by right/bottom = by rect (max
  difference 0 at 3840x2160); get --resolved = the pixels (difference
  0); live edits relay out with a new generation that frames carry, a
  rename keeps it, a filter or a playlist changing a picture's size
  relays out; hiding with a fade moves siblings up at once while the item
  fades where it was; Move between laid-out scenes; 5.0's fixed scene
  with every feature max difference 0 against 5.0 (JPEGs byte for
  byte); examples/recording.toml, a copy of Ryan's live setup and a
  migrated library load and save byte for byte against 5.0. On
  isocast-m2-a: a 1,000-item flex scene resolves in 857-861 us median
  (50 items 90-205 us across sessions: cold execution, over the 100 us
  budget in two of three); 100 live layout applies at 30/s while
  recording: 0 missed program frames, 420/420 frames. Flagship gate
  against 5.0 (6 and 9 runs): flagship-1080 daemon CPU 43.2 -> 43.2%, 0
  dropped; 4K flagship 41.9 -> 42.3%, dropped 10,423 -> 10,420; within
  the spread of two near-identical builds (control).
- 7.1, 7.2, 7.5 (2026-10-13): plugins: the isocast-plugin.toml manifest
  (every tier's sections, checked with field paths), the registry in the
  plugins folder (an isolated engine's own), `plugin_list|add|remove|
  set_enabled` on the method table, `isocast plugin add|list|remove|
  enable|disable` (--json), MCP list_plugins/add_plugin/remove_plugin;
  grants equal to what a manifest asks for, enforced per call on each
  plugin's own socket (and by peer process on the main one); process
  plugins sandboxed (Seatbelt, no isocastd privacy grants), restarted with
  backoff and turned off after 5 crashes in 5 minutes; the deck example
  (Python, standard library). Isolated engines on isocast-m2-a and
  exonbook: add without --allow refused listing the 8 methods; the deck's
  page switched scenes, recorded, marked and split (2 takes, the marker
  and the scene timeline in the manifests, 0 dropped); `methods.shutdown:
  probe may call: get`; the sandbox denied ~/Documents, home, writes
  outside its data folder, /usr/bin/open, /bin/sh, security, the main
  socket, outbound network, signalling isocastd and screen capture; kill
  -9 restarted the deck in 0.01 s; a crash-looping plugin restarted after
  1, 2, 4, 8 s and was off at 15.4 s; remove stopped it; a 30 s recording
  with plugins crash-looping, hanging, spinning a core, stopped (SIGSTOP)
  and killed every 3 s had every file complete (1800/900/1800 frames, 0
  dropped, as without plugins). Flagship benchmark with the deck idle vs
  main: CPU and memory the same, 0 drops at 1080p; the M2's saturated 4K
  flagship 0.8–1.4% more of its drops (~0.2% of frames, within the spread
  between rounds); the idle deck itself 0.40% of a core, isocastd 13.3%
  with it vs 13.2% without (notes, Benchmarks).
- V1.8 synthetic soak (2026-10-13): one-hour synthetic flagship soaks
  (4K60 desktop + 1080p60 camera ISOs + program, HEVC high, fade every
  30 s) on the M5 (4K program) and isocast-m2-b (1080p program) recorded
  216000/216000 frames in every file with 0 dropped frames, 0 audio
  dropouts and 0 missed program ticks; `ffprobe -count_frames` decoded
  all 216000 frames of all 6 files with no errors and video = audio =
  3600.000 s; isocastd CPU 15.0% (M5) / 57.2% (M2) median, fds and
  threads flat, no warnings in the log or event stream. Memory grows
  ~22–30 MB/hour (new task above).
- V1.8 media-loop memory growth (2026-10-13): media readers release their
  AVAssetReader inside an autorelease pool and frame imports drain one of
  their own. Isolated engine on the M5 (macOS 27.2), soak flagship with
  2 s loops, `heap -s --forkCorpse` live heap: 30 min recording (1080p
  program + camera ISO) main 12.88 → 29.90 MB (+41 MB/h; NSError +1/s,
  MTLTextureDescriptorInternal +29/s) vs fix 10.87 → 13.27 MB (+5.7 MB/h,
  every autoreleased class flat); the rest is the MOV sample tables
  (CMBlockBuffer blocks, also without audio), freed at stop. Live engine
  alone: NSError 58 → 120 in 61 s on main, 18 → 18 fixed. On
  isocast-m2-b (macOS 26.7) main doesn't autorelease these (NSError
  18 → 18) and both builds recorded 5 min with 0 drops. bench.py
  flagship A/B (4 alternating 60 s runs): isocastd CPU 13.6% fix vs
  13.7% main median, RSS 111 vs 112 MB.
- S.2a (2026-10-13): chaos-test repairs on isolated release daemons on
  Ryan's M5 Max, each reproduced on main df7055f first. Instance lock:
  stalled daemon + full accept queue + `isocast get` gave 2 daemons and
  an unstoppable recording on main, now 1 daemon, `get` names the busy
  pid after 10 s, stop works; 8 parallel launches -> 1 daemon. Stream
  stop against a SIGSTOPped server 17.5 s/4.4 s -> 0.51 s; flapping
  server (3 s up/3 s down, 20 retries) failed after 39.5 s on main,
  survived 52.7 s/8 reconnects; key hint only when every connection was
  accepted and dropped. Slow disk (throttled, contended disk image,
  ProRes LT): 0 of 39 warnings named the disk on main, 38 of 39 now
  (test write 0.17 ms at start, 31-147 ms while dropping); a 3 s daemon
  freeze isn't blamed on the disk. split racing stop: 6 timeouts on main
  -> 5 "stopped before it could split" (wrong state). 64 MB request line
  -> -32600 at 16 MB. A/B 40 s 1080p60 take: CPU 17.2/17.4% main vs
  17.5/17.3%, 0 drops; start p50 41 vs 40 ms; healthy stream stop
  4.9/3.8 vs 5.6/5.2 ms.
- S.2 repair-scale port (2026-10-13): stress/scale-port brings the scale
  repairs onto main after 5.0: c1e3d40, 9f40df8 and 8db69d7 ported,
  b3751cf not needed (5.0 already keeps and reuses imports). Re-verified
  with the scale agent's stress.py on the M5 Max, main 28ddad1 vs port,
  interleaved: iso16 froze main 2/2 (0/17 files; iso16-noprog and
  isofiltered16 too), the port wrote every file in all of them (8 of 17
  files drop ~142 frames: encoder capacity); a failed file says "49 of
  1440 frames due over 24.0 s, 1391 dropped" with manifest counts (main:
  "49 frames over 0.8 s, 0 dropped", null); src192 163-167% vs 158-163%
  CPU, ~57 fps min on both (old main: 230-239%, 33 fps); filters4
  25.2-25.5 -> 22.1-22.5%, filters16 65-66 -> 61-63%, filters4k16 69-73
  -> 64-65%, stillfilters4-mixed 17-18 -> 15.5-16%; iso4k8 RSS 8.3 ->
  1.2 GB, CPU 111-120 -> 77%. Pictures: static128 and stillfilters4
  program files identical to main's (psnr inf), plain and filtered ISO
  colour within 1 code value of VideoToolbox's on the same frame,
  isoh264-8 has no frame missing at any file start and every loss
  counted (main hides ~410 as repeats). Flagship gate on isocast-m2-b
  (bench.py, 4+4 alternating): flagship-1080 46.2 vs 46.4% CPU, 0
  dropped, 12/12 files on both; flagship 4K 44.5 vs 44.7%, 4693 vs 4706
  dropped (the M2's 4K limit). fmt, clippy, 141 tests clean.
- S.3d (2026-10-13): stream-only bounded encoder waiting and tap reserve;
  12 M2 A/B pairs clean, induced ~110/210/310 ms stalls 18 video/31 audio
  drops on main versus 0/0, received video gaps removed, both recordings
  complete at 1308/1308 frames with equal A/V durations. Release fmt,
  all-target clippy and tests passed; no recording path or queue changed.
- S.3 counters (2026-10-13): both benchmark scripts measure whole-process
  retired instructions with public macOS counters, including the M2 whose
  powermetrics IPC sampler fails. Verified on real M2 streams and an M4
  program recording, plus missing/decreasing-counter handling; no new
  dependency or test file. Final comparison results remain in S.3's report.
- S.3 churn attribution (2026-10-13): unified logs plus generated clip
  timestamps tie a mid-stream VT churn episode to another benchmark's
  FFmpeg clip preparation outside the common lock. Both entry points now
  prepare under that lock; verified a real preparation request blocks
  without starting an encoder while an existing stream benchmark owns it.
- S.3e (2026-10-13): 96 final main/candidate/OBS rows complete, retired
  instructions measured on both Macs. Final-byte M2 streams: 12 pairs all
  zero-drop; induced freezes main 18 video/30 audio versus candidate 0/0,
  with four further raw-wire pairs gap-free on the candidate. All 80 MOVs
  finalized and decoded, all 42 zero-drop files duration-matched; nine
  overloaded tails mismatch on both builds, preserved as failures. Four
  killed recordings retain 360 decodable frames each. M4 matched static
  memory control 464 -> 118 MiB on current main; no additional runtime CPU
  optimization accepted. Release fmt/clippy/tests clean; report has tables,
  hashes, rejected patches and remaining limits.
- P.2 (2026-10-13): the Profile and Scene Collection menus in both apps,
  laid out as OBS's: New…, Duplicate…, Rename…, Remove… (on the one in
  use), Import…, Export…, then every profile or scene collection, the one
  in use checked (same-named ones show their id); choosing one switches.
  Name and Remove dialogs in each app's dialog style; every command a
  `library_*` call, refusals in the engine's words; Import takes Isocast
  exports, whole setups and OBS's collections and profiles (the CLI's file
  code, now isocast_client::library); undo history is forgotten after any
  switch. Web app: native NSMenus filled by the interface (`menu_items`);
  Slint app: MenuBar repeaters over a new area F. Verified offscreen
  against an isolated engine (snapshots of both menus and all four
  dialogs in each app; create, duplicate, rename, switch by menu click,
  remove, import of an export/a whole setup/an OBS collection, export to
  TOML and JSON, the toasts' Switch to it, undo after a switch leaving
  the new collection alone (1 scene before and after), refusals while
  recording, each checked with `isocast collection|profile list`), and
  in real windows on isocast-m2-b through cua-driver with both apps open
  on one isolated engine: native menus captured open in both apps, New
  (web), Duplicate and Rename (Slint), switch from each app's list,
  Remove from each (file kept in deleted/), Export and Import through the
  real save and open panels in each app, a CLI `collection new`, and
  switches while recording refused in both (the 20 s take: 607 frames, 0
  dropped); every change appeared in the other app's menu at once. Slint
  app with meters moving: 12.4% of a core on the M2 (N.2: 13-14%).
- 5.5a (2026-10-13): data store, content items, bound text. `data_get`,
  `data_set` (atomic, `persist` true/false/as was), `data_delete` and
  `data_changed` (≤ 20 a second) on the method table, `isocast data
  get|set|delete` (--json), MCP get_data/set_data, `status.data`;
  dotted keys ≤ 128 characters, values ≤ 64 KB, 16 MB in all, errors
  naming the param and rule; kept keys saved beside the scene collection
  (`collections/<id>.data.json`, debounced, atomic, following switches,
  deletes and duplicates); plugins' data grants enforced. Items take
  `content` (text or image, the sources' settings and checks) and text
  takes `data = "key"` (content and global text sources); content is drawn
  on the scene thread, shared by key, made a texture at adopt (nothing
  per tick), laid-out text measured and drawn its box's size. Verified on
  base M2s (isolated, synthetic, bench lock): 60 changes during a ProRes
  recording each show in exactly one new frame, its pts ≤ +0.4 ms after
  the change (+4.3 ms on m2-a), drawn ≤ 17.1 ms after (20.9 on m2-a); a
  1,000/s flood on unshown keys while recording: 20 data_changed a
  second, compositor p99 745 µs vs 695 idle (idle windows 648–876), RSS
  and footprint within 1.5 MB, 0 dropped or missed in 83 s; flooding a
  shown title: 0 dropped, p99 1,494 µs; 60,000 shown sets with nothing
  drawn: RSS flat at 84 MB (fixed a 33 MB/s growth on the way); a kept
  key survives `isocast restart`, an unkept one doesn't; bad, long keys
  and a 70 KB value refused naming the rule; MCP round trip; inline text
  and image content snapshot identical to the equivalent sources (max
  difference 0, M5 and m2-b); examples/recording.toml, a copy of Ryan's
  setup and a migrated library load and save byte for byte against main;
  flagship gate on isocast-m2-a, alternating, 3 runs each: flagship-1080
  CPU 42.9% main vs 42.9%, 0 drops both; flagship 43.2 vs 42.2%, dropped
  3,483 vs 3,483 (the M2's 4K limit); and on isocast-m2-b over two rounds
  (46.3/46.6 vs 45.5/46.8%, 0 drops; 44.7/44.6 vs 44.8/44.5%, drops
  within 0.3%). Left: text pictures take ~5 ms on a base M2 at rest (over
  the design's 2 ms; notes, Gotchas), the apps don't list or edit content
  items, `watch` doesn't hold plugins' data events to their grants.
- 7.6 (2026-10-13): `watch` takes `kinds`/`except` (API, `isocast watch
  --kinds/--except`, MCP's event thread asks for all but levels, the deck
  for 4 kinds). isocast-m2-a, 1080p60 synthetic engine idle-running, 3
  alternating 45 s rounds per build: a watcher with no filter gets 17.7
  events/s, the deck's and MCP's filters 0; the idle deck's CPU 0.311% of
  a core on main vs 0.022%; isocastd 10.8-11.5% either way. 4K flagship
  with the deck installed (main with main's deck vs branch), 10+10 runs in
  two interleaved rounds: dropped frames per run median 1159 vs 1158.5
  (the M2's encoder limit), isocastd CPU 41.85 vs 42.2%, 0 audio dropouts.
- 7.4a (2026-10-13): WebAssembly audio filters (wasmtime 49, the WIT
  package, plugin kinds for audio filters in setups, examples/plugins/gain
  with a gain and a sidechain ducker). Unit tests on the example: the wasm
  gain is bit for bit the built-in gain (peak 0.2506 for 0.5 at -6 dB), the
  ducker 0 dB from the built-in compressor. isocast-m2-a (release, lock):
  gain 9.3-10.8 µs a block (p99 21-30), ducker with a sidechain 52-64 µs
  (p99 183-190); trap, endless loop (out of 5,000,000 fuel 2.1-4.9 ms into
  the call) and memory set mid-recording: 3 warnings per chain then off,
  both files 14.00 s A/V, every beep, beeps 0.02 ms from flashes, 840/840
  frames, 0 dropped, 0 audio dropouts. Main vs branch saved setups (every
  built-in audio filter; examples/recording.toml) byte for byte equal.
  Flagship A/B (main's built-in gain vs the wasm gain on the tone): 4K 5+5
  drops/run median 1162 vs 1165, CPU 42.5 vs 42.4%, RSS 124 vs 128 MB;
  1080 3+3: 0 vs 0 dropped, 9/9 files complete, CPU 44.4 vs 43.6%.
- A.6a/b (2026-10-13): shared redacted JSONL, crash markers/backtraces,
  available one-second metrics and offline bug_report/logs on API, CLI and
  MCP. Verified real synthetic recording/local RTMP, missing camera, bad
  setup, full disk image, owned VT service kill (-11883/-12912), panic and
  SIGKILL; six interrupted files decode. Native .ips reports, 11-value
  redaction audit, 80 MB retention flood, relative output/strict preview,
  46-tool MCP round trip and both Rust app launch hooks pass; 165 existing
  tests, release build/fmt/clippy clean. Three 4+4 flagship-1080 rounds all
  zero-drop; sampler 0.214–0.235% of one core. Strict total CPU gate stays
  blocked (+0.70 points in the final round); see A.6 and the scratch report.
- A.6 review #1 (2026-10-13): panic hooks unwind instead of aborting;
  connection/worker guards, emergency writer finalization and interrupted
  manifest recovery. Reviewer m2-a repro: contained worker/connection
  1184/1184 frames, poisoned state 845/845 frames, zero drops; SIGKILL
  recovers 420 frames and an interrupted manifest. Release clippy/fmt and
  165 existing tests pass. Shared recording files contain recovery hooks only.
- A.6 review #2–12 (2026-10-13): infallible/compact logging and protected
  crash retention, correct Mach CPU units, bounded fail-closed redaction,
  configured-Keychain export seeding, HTTP capability-path removal,
  per-report HMAC privacy, nonsticky warnings, preserved incident triage,
  compact/paged MCP preview, callback-safe/native/plugin output. Reviewer
  repros and end-to-end checks pass; two-hour streamless program+ISO run
  with 67056 polls stays at 40.7 MB logs and retains crash evidence, both
  files 216000 frames / 7200 s / zero losses. Whole-daemon 6+6 vs current
  main a619986: 48.9→49.0% of a core, 18/18 complete files per build, zero
  drops; +1.0% instructions, +13 MB summed RSS including native relay.
  Release build/fmt/clippy and 165 tests pass. Details and limits in notes
  and the scratch REPORT.md's Review fixes section.
- S.2c / S.3f (2026-10-13): overloaded files keep complete audio and equal
  final A/V duration with explicitly counted tail repeats; program/stream
  outputs have admission priority. Eight M4 and four M2 flagship pairs:
  program drops zero; Stop medians 723/989 -> 127.5/150 ms. All 72 candidate
  matrix files and 22 lifecycle files pass duration/count/decode checks;
  six killed files retain decodable, presentation-ordered fragments after
  disabling recording B-frame reordering. Release fmt/clippy/tests pass.
- V1.8 m4-throughput / S.3g (2026-10-13): the M4 Max explicitly disables
  automatic HEVC look-ahead. Eight final paired flagships store 35,040 of
  35,040 frames, zero drops; main loses 2,354. Stop medians M4 747 -> 83 ms,
  overloaded M2 984.5 -> 153 ms. Four M2 concurrent-stream pairs have zero
  program/stream video/audio drops. All 60 candidate matrix files and 28
  final-byte lifecycle/codec files are duration/count matched and decode
  cleanly; six killed files preserve decodable fragments. The M5 real-device
  check remains explicitly unqualified. Release fmt/clippy/tests pass.
- 5.2a (2026-10-13): keyframe animations. Setups take `[[keyframes]]`
  (stops by id, CSS's @keyframes) and items `animations` (keyframes,
  duration_ms, delay_ms, easing, iterations, direction, fill, start =
  on_air | shown | clock); animatable: opacity, translate (px or %),
  scale, rotate, background, border colour and width, radius, shadow,
  crop (`params` waits for item filters, 5.3a); easings gain `{
  cubic_bezier = [...] }` and `{ steps = n, position }` wherever one is
  taken (the four names bit for bit); errors name the field and list the
  sets; 5.2b's built-in ids are kept. The scene thread compiles tracks of
  floats; the compositor evaluates them at each tick's time from a compact
  per-scene list, patches moving items' records in place, rewrites
  restyled ones, uploads through a staging belt; program, preview, nested
  scenes and Move each on their own clocks; `engine.compositor.animated`.
  Verified on isocast-m2-a (60 fps ProRes program): linear 0 → 300 px
  moves exactly 5.000 px a frame (0.0 px error), cubic_bezier within
  0.034 px, steps = 4 jumps 4 times (frames 15/30/45/60), an infinite
  alternating loop 0.0 px off schedule over 62 s, on_air restarts on the
  first frame after a switch and clock doesn't; shown, nested, Move and
  preview checks; item transitions' frames identical to 5.5a's (61/61
  each way), scene transitions within the same residuals; per animated
  property 0.056 µs (1,000 translates) and 0.036 (1,000 on 100 boxes)
  with the clocks up, 0.24 at 100 (a fixed ~20 µs once anything
  animates), ~0.44 µs at rest where the whole tick runs ~8× slower;
  0 drops in every cost run. Flagship gate on isocast-m2-a (alternating,
  10 runs each at 1080, 6 at 4K): CPU per pair median −0.25 points both,
  4K dropped 6,970 vs 6,994, 1080 0 vs 0 except one disturbed 5.2a run
  (14). Local: the every-feature scene snapshots identical, existing
  setups load and save byte for byte, keyframe sets patch by id. Left:
  the apps don't list or edit animations; `params`; finished animations
  are still evaluated each tick; at rest the per-property cost is over
  0.2 µs.
- C.1 (2026-10-13): timelines on the one method table (API, `isocast
  timeline …`/`isocast retake` with --json, 9 MCP tools, timeline_changed,
  export_progress, export_finished). Isolated engine, synthetic sources
  (program + 60 fps desktop + 30 fps camera ISOs, tone in each): 4 takes
  (stop, split, stop), retake of the 4th, retake while recording (the
  running take dropped, the next one followed), remove c2, move c3 first,
  export: c3+c1+c5 = 112+169+114 frames, every joined file 6.583 s = the
  clips' lengths, video packets and decoded frames identical to the
  parts', 316,000 audio samples, flash-to-beep 1.2 ms at 60 fps (the
  30 fps file 16.7 ms, the same as in its takes), no decode errors; the
  16 take files recorded before the edits unchanged (sha256, mtime); the
  timeline byte for byte the same after `isocast restart`; kill -9 mid-take
  left the split's finished take on the timeline and the killed one off.
  Edge cases: video-only ISO files joined; a source missing from a clip
  skipped, naming it; errors name the field and list ids. isocast-m2-b:
  flagship-1080 3+3 vs main with a timeline open: CPU 45.8 vs 45.8%, 0
  drops both, 9/9 files; a 40 s 4K/1080 export in 5.4 s while recording
  with 0 drops (docs/notes.md, Benchmarks).
- C.1 fixes (2026-10-13): the adversarial review's findings, each
  reproduced on 9d1b42c first. `retake` during a stop now waits for it and
  retakes the stopped take (was: "OK" for a take never recorded, or a 30 s
  hang, and the take dropped); retakes are one at a time and a second
  before the new take begins drops nothing more (was: "dropped" but kept);
  a retake while recording begins the next take 0.25–0.35 s later (was
  0.5–1.5 s) on a 1/10 s grid and says when (`begins_at`; the CLI returns
  once it has begun); one export at a time, enforced, with unique ids;
  exports never take a same-disk recording's reserve (refused, or
  stopped); no program, no export; clearer errors for unfinished takes;
  interrupted takes listed under `removed`; restart refused and shutdown
  clean during an export, and a killed export cleaned up at the next
  start; dropped frames on clips; names fit 255 bytes; a broken current
  timeline is said. Verified on an isolated engine: retake 0–200 ms after
  a stop 6/6 consistent, two concurrent retakes 3/3 (one take dropped, the
  new take the clip), begins_at = the take's started_at to the ms, takes
  split by retake exact (0 dropped, durations whole frames); main flow
  export ALL OK (verify.py: packets and frames identical, 9.65 s each,
  flash-to-beep ≤ 1.17 ms); a 50-clip export (49 whole-second splits)
  ALL OK; two CLI exports at once 6/6 one refused; kill -9 mid-export
  then restart: manifest failed, partial removed, 201 take files
  unchanged; isocast-m2-b: an export at 965 MiB free beside a recording
  keeping 1 GB refused, the recording ran its full 30.0 s; with room, an
  export to the same disk while recording, 0 drops.
- 8.2a (2026-10-13): production app/CLI/engine and DMG built with Developer
  ID, hardened runtime, separate Accepted notarizations and stapled tickets;
  strict codesign/spctl passed on the build Mac and installed test Mac.
  Dry-run/ad-hoc/draft-only modes; workspace version and bundled daemon
  precedence. fmt, release clippy and 179 tests passed. Ad-hoc hardened
  runtime: two 4 s 640x360/30 HEVC/AAC files, 120 frames each, matching A/V,
  no drops, clean decodes; Wasm -6 dB measured -5.9994 dB. Signed runtime
  and the draft wait for Ryan's normal first-open confirmation (8.2b).
- G.3 (2026-10-13): pause/resume on the method table, CLI, MCP and events;
  one sample/frame-aligned cut clock for every recording file, manifest
  pauses and compact marker/timeline times, stop during pause, frozen REC
  status/timer. M4 synthetic verification: 39 finalized lifecycle MOVs and
  3 killed fragments decode; frame counters exact at every join (including
  100 pause/resume pairs in 9.965 s), beep/flash within 0.667 ms, concurrent
  stream zero drops. Four flagship A/B pairs vs ace7e0a: CPU 27.9/27.9%,
  zero drops both, all 24 files duration/count/decode matched. fmt, release
  clippy all-targets and 179 existing tests pass. G.5 reserved action hooks
  documented; report at /Volumes/IsocastBuild/g3-pause/scratch/REPORT.md.
- G.3 review fixes (2026-10-13, after merging main): pause hotkeys wired
  (recording.pause/resume/pause_toggle, fired through real focused input on
  exonbook); first pause allowed from the take's start (cut at 0.1 s, was
  refused then 1.0 s); daemon state not held while waiting for the cut lock
  (status p99 3.3 ms during 4K-overload cuts). Crash-while-paused keeps the
  2 s fragment bound (AVAssetWriter can't flush or change its interval in
  file mode; probed), now stated in the manifest (`crash_safe_seconds`) and
  the interruption note: kill -9 10 s into a pause at 3.7 s recovers 2.0 s,
  as the manifest says. Overhead A/B vs the merge, 3 runs: CPU 29.7 → 28.8%,
  same MB/min. e2e 12 pass (pause included), 1 skip (bug_report not on main).
- G.5 engine (2026-10-13): 35 typed actions, per-profile hotkeys, Carbon
  global press/release and permission-aware media fallback, CLI/MCP and
  events. Test Mac: recording/split (60+5 frames), PTT audio (110 frames;
  silent → −16.18 dBFS → silent), profile/collection switches, two local
  RTMP streams, no drops in the tested recordings; 680 focus samples stayed
  on System Settings. Initial idle numbers missed the Mach timebase conversion
  and are superseded by review-hardening's measurements. Pre-change profile byte identity,
  field-path/duplicate/native-conflict errors, MCP and disconnect recovery
  verified; fmt, clippy and 165 existing tests pass. docs/hotkeys.md;
  `/Volumes/IsocastBuild/g5-hotkeys/scratch/REPORT.md` records limitations.
- G.5 review-quit (2026-10-13): quit Apple Events now defer until the shared
  graceful stop finishes. Reviewer's t_quit.py on exonbook: before 60 frames
  / 2 s with manifest still recording; after 117 frames / 3.9 s, complete,
  fully decodable, disk probe removed. SIGTERM control complete too;
  fmt/release clippy and 165 existing tests pass.
- G.5 review-hardening (2026-10-13): invalid/future/duplicate hotkeys warn and
  stay preserved while setups load and record. Sidecars let d6c8aec read/write
  profiles without parking them; omitted setup fields preserve bindings.
  Bare typing/navigation and common Command shortcuts refused; TextEdit gets
  `jjj` after rejection (before: 3 hotkey fires, no `j`). CLI input with timed
  holds, merge-by-default defaults, missing-unset errors, bounded/pathful
  diagnostics (300 bindings: 525 KB -> 68 bytes), cancelled late registration,
  transactional holds, removed-source pruning and typed preview clearing.
  Reviewer store/validate/hold/global/quit repros rerun; idle (exonbook,
  3x20 s per configuration, timebase-correct): main 0.00302%, empty 0.00306%,
  35 keys 0.00277% of one core, all 1 interrupt wakeup/s and 0 idle wakeups.
  fmt, release all-target clippy and 165 existing tests pass.
- 8.1 (2026-10-13): .github/workflows/ci.yml runs fmt --check, clippy
  -D warnings, test (--locked) and the notices check on macos-15 arm64,
  workflow_dispatch only while the repo is private; actionlint clean, the
  same commands pass locally (agent/docs-release).
- 8.3 (2026-10-13): README for people downloading the app (hero + four
  screenshots from ui/console-integrate on a test Mac at 2x, benchmark
  card, download, first steps, FAQ); docs/ references generated from the
  binary (scripts/reference-docs.py: CLI, setup schema, 53 MCP tools),
  scenes, timelines, plugins, coming, architecture, benchmarks;
  THIRD_PARTY_NOTICES from cargo metadata (496 crates) plus Slint, fonts,
  Wasmtime; issue/PR templates, SECURITY, CONTRIBUTING, CODE_OF_CONDUCT.
  Verified: --check modes pass, links resolve, setup examples pass
  `setup check`, publish-snapshot.sh passes on the branch.
- 8.1, 8.3 follow-up (2026-10-13): the repo is public, so CI runs on push
  to main and pull requests (GitHub-hosted, rust-cache, concurrency);
  contribution policy: CONTRIBUTING (agents encouraged, evidence required,
  issue-first with `accepted`), PR template with evidence sections,
  pr-checks.yml (screenshots for UI changes, non-empty verification;
  pr_checks_test.py 15/15), scripts/ui-screenshots.sh (5 views at
  2880x1800 in ~9 s), CODEOWNERS, SAVED_REPLIES; repository URL, private
  reporting links and the benchmark caption fixed. actionlint clean.
- G.1a (2026-10-13): virtual-camera API/CLI/MCP, events and persisted
  program/preview/scene/source selection; bounded IOSurface output, CMIO
  extension with waiting card and opt-in host embedding. Separate-process
  reader verified pixels, Rec.709 and 60 fps; recording + local stream +
  camera verified (480-frame/8 s MOV, 541 stream frames, no A/V drops).
  Flagship base/off/on: 18 runs plus 8 off-state confirmation runs on M2;
  no consistent camera-off regression, 1080p probe adds ~8.2 CPU points.
  fmt/clippy clean, 164 unit tests + 1 doctest pass, Swift and ad-hoc nested
  bundle verify. G.1b remains blocked on Ryan's Apple signing/approval;
  native camera-client latency/cost is not yet measured. Full evidence:
  `/Volumes/IsocastBuild/g1-vcam/scratch/REPORT.md`.
- G.1 installer / Follow-up 1 (2026-10-13): `scripts/vcam-install.sh`
  prepares exact-id Xcode automatic signing/profiles, verifies the envelope,
  confirms account/copy/activation separately, then checks a private
  test-pattern feed through AVFoundation; matching confirmed uninstaller.
  XcodeGen + unsigned/ad-hoc Xcode build and deep/strict verification pass;
  wrapped executable code matches Cargo, dry-runs work with no executable
  PATH, shellcheck/fmt/clippy and 165 existing tests pass. Actual account
  updates, real signing, installation and moving-camera checks remain Ryan's
  step. Details and build-tool registration caveat in REPORT.md Follow-up 1.
- G.1 review fixes (2026-10-13): reproduce reviewer findings before changes;
  bound/authenticate native sink work in an isolated child, release state
  before lifecycle waits, restore hardened capture/JIT permissions, accept
  consumer negotiation, stamp package versions, clear removed inactive
  selections, reject MCP extras and gate diagnostics. Reviewer stall's
  concurrent status 139.8 → 2.21 ms; child open/close faults 2.009/0.517 s
  with RPC <=4.15 ms. All 6 churn MOVs 1800 frames/30 s A/V, 0 recording or
  stream drops; hardened wasm take 240 frames/4 s. fmt/clippy/default tests
  and diagnostic clippy pass, Swift builds and installer dry-run verified.
  Full provisioned-host/device/installed-client acceptance still needs Ryan.
- G.5 follow-up 2 (2026-10-13, after merging ff5ea77): reject modified typing
  and common app shortcuts; reviewer t_type now delivers A/Option-L/! with no
  hotkey fires. Old-daemon orphan sidecars are archived before id reuse;
  reads leave profile/backup bytes and mtimes unchanged, migration waits for
  a write. Reviewer t_store2: 10 no-hotkey files byte-identical to d6c8aec.
  Eight timeline actions call real methods (including idle/live retake,
  marker and export); pause/clear remain unavailable. Global input: both
  exports complete and decode (57 + 30 frames), removed takes retained,
  188 focus samples all Terminal. MCP schema/strict args and old id aliases
  verified; fmt, release all-target clippy and 179 tests pass.
- 8.2 follow-up (2026-10-13): main 905d360 merged with product/build-version
  metadata preserved, third-party notices bundled, and canonical engine
  signing ID retained for virtual-camera producer admission. Original signed
  first launch/CLI/recording passed after clicking Open; merged local-mode
  program/ISO/timeline smoke passed (3 files, 120 frames/4 s each, 0 drops,
  equal A/V and clean decodes). fmt, release clippy, 179 tests, notices and
  ShellCheck passed. Re-notarization/draft blocked: isocast-notary is absent
  from the keychain; the previous build is not drafted as the current one.
- 8.4 (2026-10-13): .github/workflows/visual.yml (pull_request, read-only,
  no secrets) renders 5 views x 3 sizes (1440x900@2, 1920x1080@1,
  900x600@2) for merge base and head with scripts/visual-shots.sh and diffs
  them with scripts/visual_diff.py; visual-report.yml (workflow_run,
  trusted) validates the artifact, pushes images to `ci-screenshots`, runs
  the judge (scripts/visual_judge.py + .github/visual-judge: Sonnet 5.5,
  read/glob/grep + deliver_verdict only), upserts one comment and sets
  `isocast/visual-check`. Verified locally (M5 Max): the same build twice
  0 px changed in 15/15 views (ISOCAST_SNAPSHOT_STILL); a surface-panel
  token change 26-62% changed in 15/15; a too-long button label
  0.04-0.10% in 14/15 (elided, settings-900x600 hides it); judge: identical
  -> no_change, token -> possible_regression (colour), clipped label ->
  possible_regression (clipped_text) at every size, also with no code diff
  (images only) and with opencode/gpt-6-luna; planted opencode.json,
  .opencode/{agents,tools,plugins}, MCP server and AGENTS.md in the input
  folder: none loaded (4 tools offered, no marker files), reads outside
  the input folder denied. 14-view judge run $0.29, ~$0.02/view.
  actionlint clean; nothing pushed.
- A.7 engine-e2e (2026-10-13): stdlib-only isolated engine checks and arm64
  PR CI/benchmarks. Local M5 Max and isocast-m2-a: 11 pass, 2 optional-method
  skips each; 300/300 frames per 5 s program/ISO file, matched A/V durations,
  clean decodes, <=1.167 ms pulse sync, zero RTMP drops, timeline/export,
  text, Wasm gain and crash recovery verified. ProRes checks pass; timeout
  and SIGTERM leave no child processes. Three-pair A/A benchmark automation
  reports noise correctly. actionlint, fmt, release all-target clippy and
  all 179 existing Rust tests pass. Hosted first-run steps in the e2e report.
- A.6 fix round 2 (2026-10-14): merged main; re-review N1-N7 fixed. One
  5,000-byte token or 700 distinct tokens no longer withhold anything (0
  "withheld", bad-apply error and versions present); late secret still
  redacted. Injected compositor panic: manifest failed with the cause,
  status "degraded"; writer-site BARS manifest 14.433 s = ffprobe; later
  `frames` after a frame-stream panic deliver (was 0×0) with a warning;
  error after 40 calls logged, omission flushed at disconnect; unwritable
  logs: CLI prints one warning, no JSONL. Clean worker/state/SIGKILL cases
  unchanged. The first start after a launch that repaired a killed take
  names it again (e2e shutdown expected that note; it accepts the A.6
  "interrupted" status). Main merged twice more (Console app, pause);
  after the last: fmt, clippy -D warnings, 180 tests, crash cases rerun,
  e2e on isocast-m2-a 13/13 pass incl. bug_report, shutdown and pause.
- N.5.0 (2026-10-13): the Console foundation on ui/console. Every Console
  token from Paper in ui/theme.slint under Paper's names (97, plus the
  literals the boards use: LCD, sheet, capsule, meter, shadow sizes);
  Mona Sans 400/500/600/700, Hubot Sans 600 and Martian Mono 300 bundled
  as their projects' static cuts with their OFL licences (their shaped
  widths of the boards' labels are within a pixel of Paper's); the kit
  restyled to the Components board, with new components (Lcd, LcdCell,
  LcdTime, LcdHero, Capsule, CapsuleButton, TransportButton, MiniToggle,
  MasterMeter, MeterBar, Badge, Tag, Rule, TextMenuButton, EmptyState,
  SheetGlyph, SheetHeader, SheetFrame, RowGroup, GroupRow, GroupHeading,
  GroupHint, Band, Inspector, QuickList, QuickRow, MenuView, ShownState),
  the sheets hung 44px from the top with glyph, title and one line, and
  NavList as the grouped sidebar. Verified offscreen: the gallery
  (`--state kit`) drawn as the Components board, snapshot at 1440×3060
  at 1x and 2x beside Paper's 1x and 2x exports (pairs in
  /Volumes/IsocastBuild/console-foundation/pairs): every band starts on
  the board's pixel row (146, 683, 1264, 1685, 2158, 2543) and the label
  widths match; text draws heavier than Paper's (Skia, see notes). The
  app's views (main, Studio Mode, menus, Properties, Filters, Transform,
  Settings, toasts, a 1024×700 window, the kit's menu, dialog and
  dropdown) drawn against an isolated engine; `--a11y` lists the docks,
  the sheet, its sidebar tabs and buttons. Areas not restyled (N.5.1).
- N.5.1a (2026-10-13): the main window and Studio Mode in Console on
  ui/console-main (the WIP 7f210de checked against Paper and fixed, kit
  fixes b823452: icons drew ~10% small through Slint's stroke-inset path
  fit, now Paper's size and stroke; a field's disabled fade; the band
  stepper). Verified offscreen at 1440x900, 1x and 2x, beside the Main
  window and Studio Mode exports (pictures masked: 0.76-0.92% mean
  difference; pairs, scores and contact sheets in
  /Volumes/IsocastBuild/console-main/pairs), 20 views and states
  (recording, REC/LIVE details, toasts, menus, hover, no docks, 1024 and
  900 px windows); `--a11y` lists the rows, controls, menus and dialogs;
  18 behaviours pass against an isolated engine (record, marker, split,
  stop, scene switch, mute, ISO toggle, transition, Studio Mode preview
  and takes, drag and undo, pane toggle); on isocast-m2-b idle 3.6% and
  meters moving 21.5% of a core vs main's 3.8% and 21.2%.
- N.5.1b (2026-10-13): the sheets, Settings and Welcome in Console on
  ui/console-dialogs (WIP 42d2955 checked against Paper and finished).
  Kit fixes: leading tokens a hair under their line heights (Slint
  rounds text heights up, which made each such text 1px taller than
  Paper's), toasts with Paper's edge and 2px gaps, NumberField `suffix`
  and `fixed` ("1.00×"), Dialog `accessible-name`. Region scores against
  the boards (mean absolute difference, 0-255, boards converted from
  Display P3): Properties 1.65 (1x) / 1.46 (2x), Settings 2.77 / 2.70,
  Welcome 3.14 / 2.99; the rows land on the boards' rows, what is left
  is Skia's heavier text and data. Behaviours driven offscreen against
  an isolated engine (OK, Cancel/Escape, Return, Apply, switches,
  segments, steppers, typing, the sidebar, filters' eyes, a new
  profile, dragging a sheet); `--a11y` lists every sheet and control.
- N.5.1c (2026-10-13): the Console main window, Studio Mode and sheets
  together on ui/console-integrate. fmt, clippy --all-targets -D warnings
  and the 165 release tests clean. Offscreen, 52 views and states at
  1440x900 (1x, 2x) and 900x600 beside the Paper exports
  (/Volumes/IsocastBuild/console-integrate/compare): main window 0.86% /
  0.79% (1x/2x, pictures masked), Studio Mode 0.92% / 0.75%, Properties
  1.58 / 1.39, Settings › Output 2.66 / 2.59 (2.77 / 2.70 before the
  disk badge), Welcome 3.10 / 2.95; the kit gallery against the
  Components board 1.48% / 1.22% (the foundation's 1.57% / 1.32%); the
  main window pixel-identical to ui/console-main's but for live content.
  Both areas' behaviour runs pass (18/18 main, 19/19 sheets); `--a11y`
  lists the sheets, their rows and controls. Real windows on
  isocast-m2-b (cua-driver, benchmark lock): every in-window menu, the
  dropdown, every menu-bar dialog (13), a canvas drag and Edit › Undo,
  scene switches, Studio Mode takes by Transition, Fade, Cut and Return,
  a pointer-started 7.6 s recording (ffprobe: H.264 1920x1080 60 fps +
  AAC). CPU (3 runs each, alternating with main's 9cda72d): at rest
  3.8% vs 3.5%; three tones 21.4% vs 19.9% (6 runs; ui/console-main's
  own build 22.4%); with the mixer hidden 16.6% vs 3.7%, as the control
  bar's master level redraws the window with the sound.
- N.5.2 (2026-10-13): the Console critiques fixed on ui/console-integrate
  (37c28c9, b71274c, ba7dbb2, 708e98c). Text a cut lighter than Paper's
  weights (ink against the boards 1.48-1.54x → 1.24-1.28x for 400,
  Welcome's title 1.20x → 1.04x); Welcome's window and every empty state
  as the boards draw them (the whole Welcome window 0.77% → 0.49% from
  its board); menus, signals, the LCD, the destructive button, the muted
  strip, tokens; the program file's GB/h through Status; stereo master;
  keyboard and screen-reader paths (strip ⋯ and ⇧F10, quick
  transitions, Segmented, reset glyphs); small windows; the master level
  as a native layer. Offscreen: 52 views at 1x/2x/900x600, behaviours
  18/18 and 19/19, Tab orders, --a11y. isocast-m2-b (real window,
  benchmark lock): menus, sheets, a strip's menu by keyboard, a recording
  by the transport (ffprobe H.264 1080p60 + AAC), a Studio take, a drag
  and Undo; CPU vs main 9cda72d (medians) at rest 3.3 vs 3.1%, tones
  with the mixer shown 20.2 vs 20.0%, hidden 4.6 vs 4.0% (2f22005:
  18.0%). fmt, clippy --workspace -D warnings, 166 release tests clean.
- N.3, A.1 (2026-10-13): scripts/app-dev.sh builds the Slint app as
  Isocast.app (dev.isocast.app, Isocast.icon; `app_target` the one-line
  switch, ui-dev.sh a wrapper); the web app's crate is out of
  default-members (not built, tested or listed in THIRD_PARTY_NOTICES,
  which now names the Slint app's fonts) but kept for the alpha week;
  open/save panels dark. Verified: the bundle's plist, signature and
  icon locally; offscreen 1440x900@2 and 900x600@2; on isocast-m2-b the
  bundle built by app-dev.sh and opened with `open` shows "Isocast" in
  the app menu, cua-driver lists it as dev.isocast.app, Profile ›
  Import… opens a dark panel; fmt, release clippy, 178 tests (the web
  app's own tests no longer run by default), e2e 11 pass / 2 skip.
- G.1 app (2026-10-13): the Controls dock's Virtual Camera toggle and its
  output popover (program, preview, a scene, a video source; canvas or
  an advertised size; size and fps from virtual_camera_status, counts
  while running), state from virtual_camera events, and the install
  explanation with `scripts/vcam-install.sh` and Copy when a start finds
  no extension. Verified: offscreen 1440x900@2 and 900x600@2 (stopped,
  missing, running, failed; no clipping, Exit gives way under 240px),
  `--a11y` names; on isocast-m2-b (real window, no extension installed)
  Start got the daemon's real "not installed" error and opened the
  explanation, Copy put the command on the pasteboard (cua-driver
  clipboard_read), choosing "A scene" wrote virtual_camera {target:
  scene, scene: main} (isocast get), Tab reaches the toggle and gear and
  Return opens the popover; app CPU at rest (3 x 20 s, alternating with
  main's build, benchmark lock) median 2.7% vs main 2.8%.
- A.7 engine-e2e hosted follow-up (2026-10-13): inspected Actions run
  38493967068 and all ten retained MOVs (clean decode/equal A/V, 19–38%
  losses, some short files). Added real hardware/throughput tables, a
  720p30 H.264 qualification and explicit hosted-only loss accounting with
  a 25% ceiling; zero-drop qualification keeps the whole suite strict.
  Local strict mode 11 PASS/2 optional skips; hosted mode 12 PASS including
  qualification/2 skips, all zero drops. Actual hosted artifacts prove the
  bounded acceptance/rejection and unchanged duration/frame-count gates.
  actionlint, fmt, release all-target clippy and 179 Rust tests pass. A
  hosted re-run after publication is still required; exact commands in REPORT.
- G.3 R1/R2 (2026-10-13): replace cut-derived `crash_safe_seconds` with
  explicit unknown `recoverable_seconds_at_pause: null`; legacy values are
  discarded on recovery, and both manifest errors and Start notes avoid
  invented saved/lost seconds. Explain that Pause/Resume acknowledge metadata,
  not durable media; hotkey double toggles queue and execute serially.
  Test Mac: legacy 2.000 s false promise reproduced, 55-case/165-file matrix
  (111 readable and cleanly decoded; 54 without a movie header), all actual
  cut targets exact and unknown metadata preserved through recovery. Includes
  H.264/HEVC/ProRes, exact/adjacent boundaries, immediate/delayed kills,
  during/after Resume, and 13 three-4K60 cases. Double toggle 0.283 ms apart
  succeeds twice. e2e 12 PASS, 0 FAIL, bug_report unavailable; full fmt,
  release all-target clippy and 180 tests pass. Full per-output comparison:
  /Volumes/IsocastBuild/g3-pause/scratch/REPORT.md, "Re-review fixes".
- G.3b investigation (2026-10-13): no runtime replacement retained. Fifteen
  native configuration probes distinguish automatic raw compression from
  manual passthrough-only segmentation; MOV, ProRes and multi-audio limits
  prevent a drop-in switch. Three interleaved flagship rounds of existing
  split/join add 62%/29% median instructions at 1/2 s. Baseline pause kills:
  111/165 files decode, 54 unreadable; plain kills: 45/60 decode, worst
  common A/V loss 2.250 s. All 27 joined outputs decode and 54 tracks import
  into AVMutableComposition. E2E 12 pass/1 unavailable; fmt/clippy and 180
  tests pass. G.3b remains unchecked; full evidence and remaining work in
  /Volumes/IsocastBuild/g3b-segments/scratch/REPORT.md.
- G.3b design and prototype (2026-10-14, branch `agent/g3b-crash-safe`,
  unmerged): `docs/crash-safe-takes.md` and a working `ISOCAST_CRASH_SAFE=1`
  writer (own VideoToolbox/AAC encoders, fragmented MOV/MP4 finished in
  place, commit barrier before the pause reply with a numeric
  `recoverable_seconds_at_pause`, `committed_seconds` in status, recovery
  from fragments at the next launch). Test Mac kill matrix: 36/36 files
  decode after recovery, video loss ≤ 0.5 s, pause kills exact; flag off
  identical to main (562.3 M instructions/s both); flag on −11 %
  instructions, +23 MB RSS. Report: g3b-crash-safe scratch REPORT.md.
- G.3b fix round 1 (2026-10-14, after the adversarial review): pictures
  are held until the next one fixes their duration (gaps at fragment
  boundaries stayed out of the finished table: up to 1 s of A/V drift),
  failed files are finished from their fragments at Stop and at every
  launch, commit errors fail the file and reach the Pause reply
  (`recoverable_seconds`/`errors`), CLI and manifest, a pause drains the
  writer's queue first, a torn header patch is recovered, per-track edits,
  empty tracks left out, encoder drops counted. Reviewer's repros re-run:
  stall cases `mdhd` = `stts` = decoded ends, 48/48 matrix files decode
  with all AVFoundation tracks, EIO write/fsync cases loud and repaired,
  torn patch recovered; own matrix incl. kills during Stop and recovery
  48/48; flag off still equals main (562.3 vs 562.9 M instructions/s).
- G.3b fix round 2 (2026-10-14, after the re-review): `MacWriter` forwards
  `lost_frames` and `finish` returns the drops it found (the VT drop
  interposer's 6 and 1 injected drops now show as 84/6 and 89/1 in the
  manifest), recovery decides nothing from the 16 header bytes (all 17
  torn states recover on real media), repair opens only regular files in
  its own folder through `openat(O_NOFOLLOW)` and refuses links (the
  live-alias probe no longer touches the owner's file), the first fragment
  waits for the second picture (no raw `tfdt` gap after a stall at the
  start), repaired files report the movie's own picture count. Flag off
  equals main on `bench.py camera-1080` (449.2 vs 448.2 M instructions/s;
  no 4K ISO because of #41).
- G.3b fix round 3 (2026-10-14, after re-review 2, "merge behind the
  flag"): a repaired take with audio but no picture presents its audio
  (movie length = longest kept track; the reviewer's encoder-stall repro:
  0.959 s playable in AVFoundation, was 0 s), the `ftyp` carries an
  `isoc` brand and repair requires it byte for byte (an Isocast-shaped
  ffmpeg file is left unchanged), the recorder `flock`s its movie and
  repair skips a locked file (a live file renamed into another folder is
  left alone). Reviewer's suite: 68/68 torn-header cases, all main and
  ffmpeg files unchanged, drops 84/6 and 89/1.
- G.5 app (2026-10-14): Settings › Hotkeys lists the profile's bindings by
  group (record, stream, scenes, Studio Mode, sources, timeline, other)
  with click-to-record chords from AppKit key codes, ✕ to clear, the
  API's refusals and OS warnings on the row; one `hotkeys_set` patch at
  the shown revision per change; the focused-key bridge forwards edges
  with `hotkeys_input` on one connection and releases on blur. Verified:
  offscreen 1440x900@2 and 900x600@2 (sample bindings with a refusal and
  a warning, a row recording), `--a11y` names; isocast-m2-b real window:
  clicking "Start or stop recording" and pressing ⌃⌥⌘R wrote
  recording-toggle (ctrl+alt+cmd+r, global) through `hotkeys_set`; ⌘S on
  "Split the take" showed the API's refusal on the row; with cmd+F6 held
  by another app (registration warning, focused fallback) ⌘F6 in the
  window fired studio.toggle with source "focused" and the app followed
  into Studio Mode.
- C.2 (2026-10-14, branch ui/create-view): View → Create (⌘⇧C, or the
  Build/Create switch) puts Paper's clip bar where the docks are: takes in
  order on a Program track with thumbnail strips (AVFoundation, off the UI
  thread, cached), names and lengths; ruler, playhead, record head; Record,
  Retake, Remove (and Delete), drag to reorder, New/Open/Close, Export with
  progress and Show in Finder; it reads timelines only on connect and
  `timeline_changed`. Verified: offscreen snapshots (no timeline, empty,
  three takes, recording, retake, export; 1440x900@2 and 900x600@2);
  real window on isocast-m2-b (cua-driver, bench lock): three takes,
  retake idle (`retaken`) and while recording (`dropped`), drag reorder,
  Remove, Export: 15.300 s program/ISO files = 4.08 + 5.75 + 5.47 s, video
  and audio 15.300 s each; CPU at rest medians main 2.7%, Build 2.5%,
  Create 2.8% (3 runs, 20 s); 51 clips: zoom, scroll and drags at most
  29% of one core in any second; e2e.py 11 PASS (2 skips, not present on
  main).
- A.6 fix round 3 (2026-10-14): R2-1–R2-5 fixed after merging main's pause
  durability correction. Relay fatal-tail checks with 10/40/120/600 learned
  values all retain readable redacted text; actual stalled-seed delivery
  withholds only the pending line and recovers. Env and rotated/Keychain
  stream credentials survive 600-value churn with no raw/zip leaks. Reviewer
  paused SIGKILL: manifest explains unknown recoverability and triage paused
  true; compositor fault refuses Resume/Start (exit 1), keeps the take paused;
  frame-stream panic exits 1, next client gets 61 frames. m2-a regressions:
  contained worker 393 frames/file complete, poisoned state 177 interrupted,
  post-resume writer file 177 frames/5.9 s, native exit retains 360 frames.
  All decoded files clean. fmt, release clippy/tests (180), e2e 13/13 pass.
- A.2 app #1 (2026-10-14): leaving while recording or streaming asks first
  (⌘Q and the Dock through applicationShouldTerminate:, Close Window,
  Controls → Exit, the close button); Stop and Quit stops then quits. m2-a
  real window: each way showed the sheet, Cancel kept recording, Stop and
  Quit left a complete 160.5 s take (h264+aac) and no app process, Keep
  Recording quit with isocastd still recording.
- A.2 app #2 (2026-10-14): corner resize keeps the shape by default, Shift
  frees it, sides stay free, snapping kept (one edge snaps, the other
  follows the shape). Offscreen 1440 and 900: 1120x630 dragged to 1031x580
  (16:9), Shift 780x580, side 1078x630, snapped to the canvas edge
  1800x1013. m2-a real window: 1209x680 (1.778), Shift 936x680.
- A.2 app #3 (2026-10-14): a click on a row gives its list the keyboard
  (↑ ↓ Home End, F2, Delete); a press on the canvas gives it back to the
  window (arrows nudge again). Offscreen: click Bars, ↓ → brb on air; F2
  fields in both lists; canvas click then → nudged x 0 → 1. m2-a real
  window: ↓ brb, ↑ bars-full, F2 rename field in Scenes and Sources.
- A.2 app #4 (2026-10-14): errors fade after 8 s unless hovered, three at
  most; a dialog shows them beside or under its sheet, or keeps them
  behind it. Offscreen 1440/900 with the quit sheet and Settings: toasts
  right of or under the sheet, four sent → three shown. m2-a real window:
  two error toasts at 6 s, gone at 10 s; a hovered one still up at 11 s,
  gone 9.5 s after the pointer left.
- A.2 app #5 (2026-10-14): `shared_frames` takes `filtered` (CLI
  `frames --shared --filtered`): a source drawn through its live filters,
  redrawn when they change; the Filters and Properties previews use it.
  Offscreen and m2-a real window: hue shift 90 on a test pattern and the
  media clip shows shifted bars in the dialog (two filters stack: 180).
- A.2 app #6 (2026-10-14): filter row menu and inline rename (double-click
  too), source Copy Filters / Paste Filters. Offscreen: duplicate gave
  color-correction-2, rename typed into the field became its id, Copy
  Filters on Intro Clip then Paste Filters on Test Pattern added both
  (ids made unique), row Paste appended one more. m2-a real window: the
  right-click menu, Duplicate added color-correction-2.
- A.2 app #7 (2026-10-14): groups from the app. Offscreen: Intro Clip +
  Test Pattern grouped into `group` at 120,120 1680x630 with children at
  0,0 and 1200,0 (nothing moved on the canvas), Ungroup restored the exact
  rects, Add → Group then Move to Group (Logo, Lower Third) grew it to
  120,800 1636x256. m2-a real window: ⌘-click, right-click → Group Selected
  Items made the same group.
- A.2 app #8 (2026-10-14): Properties of a Color source: Width 400 then
  Height 300 gave 400x1080 then 400x300 (isocast get), no refusal; Width
  9000 was refused with the toast saying "“Backdrop”: width and height
  must be from 1 to 8192" instead of `sources[0]`. m2-a real window.
- A.2 app #9 (2026-10-14): offscreen 1024x681 Encoder menu shows "ProRes
  422 Proxy · Apple VideoToolbox" whole, resolution menu "1080×1920
  (vertical)", "Custom (1920×1080)"; 900x600 Sources names with ISO
  badges get the lock's room; LIVE pill orange (stream:reconnecting);
  hover outline gone after the pointer leaves the canvas and while a
  dialog is open. m2-a: ⌃⌘F filled the 1024x768 screen and back, File →
  Show Recordings opened a Finder window titled "rec". App CPU at rest
  (m2-a, real window, --bench 20, alternating under the bench lock): main
  18.0/16.9/19.2% vs branch 17.0/18.2/16.8% (median 18.0 vs 17.0), 29.4
  redraws/s both.
- A.6 hardening (2026-10-14): M-1 pins only successfully stored/read keys,
  capped at current + two previous per service/account. 3000 failed writes:
  pin count 0 -> 0, logger CPU/error 19.84 -> 21.22 us, warmed daemon RSS
  47.58 -> 48.12 MB; two accounts/eight rotations cap at 3/6 pins in both
  processes. M-2 warns on native-relay exit, supervises/reseeds replacement,
  and drops seed transport messages while unavailable. SIGKILL plus 12000
  credentials during blocked respawn: 0 pending bytes; recovery keeps native
  text redacted, concurrent 240-frame program/ISO files complete without drops.
  Reviewer 100000-line relay harness retains order and redaction; release
  fmt/clippy, 180 tests and all 13 engine-e2e scenarios pass.
- G.3 app (2026-10-14): Pause/Resume in the Controls dock and the control
  bar's transport, the LCD's PAUSED with the frozen take time, the status
  line's pause length, the REC details' list of pauses. Verified:
  offscreen 1440x900@2 and 900x600@2 (recording, paused, details with two
  pauses), `--a11y` names (Pause/Resume Recording, PAUSED); isocast-m2-b
  real window: started by the transport, paused by it (LCD PAUSED), resumed
  by the dock's toggle, paused again by a recording.pause_toggle hotkey
  through the focused-key bridge, stopped: program and camera ISO 53.3 s
  with equal A/V (1599 and 3198 frames), manifest pauses 5.1-20.7 s and
  65.3-89.8 s of the uncut clock.
- A.6 app (2026-10-14): Help › Report a Bug… previews the report
  (`bug_report` preview: ISSUE.md and every file with its size), writes the
  zip, opens the prefilled new issue on R44VC0RP/isocast (bug_report.yml
  fields) and shows the zip in the Finder; nothing is sent. Verified:
  offscreen 1440x900@2 and 900x600@2 (preview with a typed description;
  saved), `--a11y`; isocast-m2-b real window: Help › Report a Bug…, Create
  Report wrote isocast-report-…zip beside the isolated engine's logs, Open
  GitHub Issue opened Safari at GitHub's sign-in with return_to
  …/R44VC0RP/isocast/issues/new?template=bug_report.yml&labels=bug&version=…
  &mac=Apple M2 (Mac14,3), macOS 26.7 (25G229)&attachments=Attach isocast-
  report-….zip…, and the Finder showed the reports folder.
- Dark window chrome (2026-10-14): every Isocast window takes the dark
  appearance (NSApp DarkAqua, set at launch since N.3), so title bar,
  traffic lights, menus and panels match on a light Mac. Verified on
  isocast-m2-b in light appearance: the real window's title bar is dark.
- A.2 app #10 (2026-10-14): after merging main (425c990). Canvas on a
  fit-none item (rect 1920x1080, scale 0.5): corner 836x470 at scale
  0.4352, side 700x394 (vertical centre kept), Shift still one scale, left
  handle keeps the right edge; stretch items: corner 409x230, Shift
  380x180, side 380x270, scale 1; Reset Transform scale 1. m2-a real
  window: the same on a 1280x720 clip (scale 0.75 → 0.6655, 0.7669,
  0.6865, rect unchanged), stretch Test Pattern rect changed (Shift
  ratio 2.281); Edit Transform Width 640 → scale 0.5, Height shows 360;
  Controls dock "Stop" at 1024 px while recording.
- 8.2d (2026-10-14): main 85e2210 merged, signed native Slint app/DMG prepared
  at release/scratch/morning-native. Strict signatures and team/entitlement
  checks pass; spctl reports Unnotarized Developer ID as expected. Native
  window, bundled daemon after CLI restart, 4 s program+ISO: 120 frames each,
  0 drops/repeats/audio dropouts, equal A/V and clean decodes, timeline c1.
  fmt, release Clippy, 178 default-workspace tests and notices pass; the
  retired web app is excluded. Resumable --notarize plus --draft-release
  finalizes saved bytes and reuses only a matching draft. No notary or GitHub
  write attempted; exact morning commands in REPORT.md.
- 8.2b (2026-10-14): rebuilt current main f6570b7, notarized/stapled app and
  DMG with the restored login-keychain profile; both Accepted. Strict code,
  tickets, Gatekeeper and mounted binary hashes verified. Mounted native UI,
  bundled CLI/restart/engine on m2-b: 2x120 frames/4 s HEVC/AAC program+ISO,
  no drops/repeats/audio dropouts, equal A/V, clean decodes, timeline c1.
  One unpublished draft (412748947), both uploaded digests match; repeat
  lookup reuses it. Finalizer uses the OS-reported mount point after custom
  nested mounting failed on macOS 27.2; short user-facing alpha notes.
  fmt, release Clippy, 183 tests, notices and ShellCheck pass. Report: Draft.
- 8.2e (2026-10-14): release.yml and opt-in finalizer CI modes ready; local
  profile defaults retained. Protected release environment, pinned actions,
  macos-15/Xcode 26.3, ephemeral signing keychain, API-key notarization and
  generated-note draft replacement; no publication path. Actionlint/ShellCheck,
  fmt/release Clippy/183 tests/notices pass. Exact version and signing/cleanup
  blocks dry-run/mocked; API auth/redaction and draft update refusal checked
  without service writes. Mounted signed bundle on m2-b: 3x5 s program+2 ISO,
  9 files/1350 frames, 0 loss, full decodes, equal A/V, sync <=0.021 ms; final
  take tightened to zero-drop after qualification. Setup/first hosted run
  remain Ryan's steps; existing draft unchanged.
- A.2 QA-3 R3 hotkeys (2026-10-14): focused input no longer disappears merely
  because native registration succeeded. Global/focused presses pair within
  100 ms in either order; repeats, rapid distinct presses and hold release/
  disconnect are preserved. m2-a: four focused and four Finder-frontmost
  pause/resume pairs, exact markers, eight dual-path pairs with no double fire;
  final binary also verifies both UI/global paths (116 frames, equal A/V,
  complete, zero drops). Unchanged QA daemon proves desktop Cua events reach
  Carbon; window-targeted foreground ones bypass it. 13/13 E2E cases pass on
  exonbook (GUI locked); hotkey E2E also passes on m2-a. fmt, release clippy
  and 183 tests pass; report: g5-hotkeys/scratch/R3-REPORT.md on build volume.
- A.2 re-pass R2 (2026-10-14): cause: Slint 1.18's skia layer cache isn't
  freed with a component, so a cached dock made again (`if`) at the same
  address showed the old dock's picture over the right rows (the AX tree
  had the right items). The docks, status bar, viewer line and source
  toolbar now stay made and hide by size. m2-a real window: Create → 2 →
  Create shows Bars and Test Pattern (main showed Main's items); hiding
  Sources, 3, showing it: BRB items; Studio Mode twice; a refused Start
  with two toasts then 2, 3: right each time. App CPU at rest (bench lock,
  3 alternating runs): main 17.9/17.4/15.6 vs 17.1/17.4/16.6 (uncached it
  was 35%).
- A.2 re-pass #1 (2026-10-14): the new-issue link carries title, summary,
  version, Mac and a pointer to paste (2000 characters at most, the
  description cut if it must be); the triage block (6960 characters on
  m2-a) is copied for the Triage field, and the sheet says to paste it.
  m2-a: Safari opened github.com/…/issues/new (842-character address,
  sign-in redirect) and the clipboard held the ```json block.
- A.2 re-pass #2 (2026-10-14): `StreamStatus.key_problem` says when the
  saved key can't be read without a prompt; Settings → Stream shows a red
  callout and "Paste the key again", Start Streaming opens it there. m2-a:
  a key stored with main's a2-qa build → callout; pasted a new key and
  Save Key → key_problem none, "•••• saved".
- A.2 re-pass #3 (2026-10-14): MP4 greyed for ProRes, choosing ProRes on
  MP4 switches to MOV; a refused Apply/OK shows the engine's words in a
  callout. Offscreen 1440/900: ProRes 422 HQ with MOV, MP4 grey. m2-a:
  H.264 + MP4 → choose ProRes 422 HQ → OK saved prores422_hq/mov; canvas
  1280x1080 while recording → callout "canvas: the canvas stays
  1920×1080 at 30 fps while recording…", dialog stays open.
- A.2 re-pass #5 (2026-10-14): m2-a 1024x681: Exit button beside Settings
  (AX "Exit", pressing it quit the app); keep_free_gb 100000 → Start
  Recording → one toast "Couldn't start recording" (was two).
- A.6 report worker (2026-10-14): `isocast report --submit` against
  `wrangler dev` with a fake GitHub filed the issue. Its body starts with the
  exact preview ISSUE.md, the R2 zip is byte-identical to the local zip, and
  the planted stream key and password are absent from the issue and zip. The
  worker's 8 local tests pass. Offline, 429, no-endpoint and invalid-param
  paths keep the zip. Offscreen snapshots show the sheet sent and offline.
  The e2e bug_report case passes.
- A.2–A.6 (2026-10-14): ticked at the alpha handover. Every item is on
  main and published (A.2 QA passes and fixes, A.3 hotkeys/pause/virtual
  camera, A.4 graceful overload, A.5 timeline and Create view, A.6 logs and
  one-click reports through the deployed report service); `cargo test`
  183/183 and `scripts/e2e.py` 13/13 with zero drops, locally and on
  GitHub's runner. Follow-ups moved to GitHub issues: A.6 overhead #7,
  hosted pacing #5. A.7 (Ryan's soak, #2) stays open.
- 8.2 (2026-10-14): Isocast 0.0.1 Alpha published by Ryan's request as a
  prerelease, tag v0.0.1 on the public snapshot 77ceb13, DMG SHA-256
  16a86d30…7d917 (notarized, stapled, Gatekeeper accepted; downloaded
  asset re-hashed to match).
- 8.2g (2026-10-14): release workflow readiness for #11. Secret names
  match docs/workflow (6/6; environment has reviewer R44VC0RP, main + v*,
  0 secrets); Xcode 26.3 present on macos-15-arm64 image 20260907.0337.1;
  hosted e2e run 38585167425 on that image: 0 drops in 2 qualification
  takes + 2 streams and 13/13 scenarios at 720p30, inside the release
  gate's 25%-then-zero bound. A run for a published version used to go red
  after a full build and two notarizations; a preflight job now ends it
  green without approval (12 local cases against the real repository and
  canned API replies). Dry runs: release.sh default and CI-override, the
  workflow's exact finalizer command (API-key mode), and a real ad-hoc
  build at e9753d4 (6 m 11 s warm, 4 Mach-Os arm64/system-only, DMG
  verified, checksum OK); actionlint clean. Rust tree untouched. Review
  fix round: the doc's command blocks lost their inline comments (Ryan's
  interactive zsh passes them as arguments) and gained `&&` guards that
  check 0.0.2 before any commit, publish or tag; all nine blocks were
  pasted into a real interactive zsh (typed lines and bracketed paste,
  stub gh/git, scratch copies) with the guards stopping on 0.0.1; the
  macOS job's re-check now fails closed (38 preflight/re-check cases).
- 8.2f (2026-10-14): release DMG has the designed 640x400 window (dmgbuild
  1.6.7, scripts/dmg/): Isocast.app and the Applications link either side of
  the arrow, HiDPI .background.tiff (640x400@72 + 1280x800@144), volume icon,
  no Install.txt. Verified with a Developer ID-signed (not notarized)
  release.sh build: hdiutil verify, mounted listing, .DS_Store dump, and real
  Finder windows on isocast-m2-a in Light and Dark (content 640x400, pixels
  match the art, icons centred at 180/460, y 200). Finder labels are black
  on the black art in both appearances (unreadable): waiting on Ryan.
- 8.2f (2026-10-14): labels drawn in the art (#BBBBBB, under Finder's
  invisible black ones); rebuilt with release.sh and checked in Finder on
  isocast-m2-a, Light and Dark: centred under each icon (x 180/459, y
  293-304), no overlap with Finder's labels, selection pill or wordmark.
- 8.2h (2026-10-14): Sparkle 2 auto-update. On isocast-m2-a, update-test
  builds 0.0.90 -> 0.0.91 (Developer ID, throwaway key, local feed):
  offered, downloaded, EdDSA-verified, installed and relaunched; the new
  app's engine reports 0.0.91 build 91. A scheduled check waited while
  recording and ran 1 s after stop; Install and Relaunch waited 30-78 s of
  recording and installed within 1 s of stop; Keep Recording quit replaced
  the bundle under a recording engine and the next launch restarted it once
  idle. All 5 takes decode fully with frame counts matching duration at 30
  fps. Tampered feed and tampered DMG refused; user-defaults feed override
  cleared and never fetched. Screenshots and REPORT in
  /Volumes/IsocastBuild/auto-update/scratch.
- 8.2h fix round 1 (2026-10-14): review findings fixed: engine method
  shutdown_if_idle (atomic; restart and updates use it; race repro 3 cuts in
  122 restarts before, 0 in 220 after), abort recovery (induced installer
  death recovered the engine in 60-63 s), virtual camera counts as busy, no
  error toast on the stale-engine restart, quit question names a staged
  update, docs: EdDSA key as sole root of trust, full releases only, dry run
  without the key. Re-checked on isocast-m2-a; 6 takes intact.
- #47 developer mode (2026-10-14): developer mode on the one method table
  (debug_status/debug_set, isocast debug, MCP developer_mode, Settings >
  Advanced), verbose logs and per-second traces in separate 200 MB
  segments, Developer menu and stats overlay. Off: flagship-1080 A/B on
  isocast-m2-a, median of 3 interleaved, isocastd 50.2% main vs 49.9%
  branch, 559.2 vs 559.0 M instructions/s, 9/9 files 0 drops each. On:
  50.0% vs 49.7% off, 560.0 vs 559.0 M instr/s, 2.25 KB/s logged; 4K60 + 3
  ISOs 2.9 KB/s. Toggling 8 times during a 20 s recording: 3 files,
  600/600 frames, 0 dropped (ffprobe). e2e 13/13 on isocast-m2-a.
- 7.7 (2026-10-14): every recording session writes into its own project
  folder (`<date> <time> Untitled/`, `.isocast-project.json` id) with
  `list_projects`/`rename_project` on API, CLI (`isocast project`) and MCP;
  new events take_finished, project_created, project_renamed,
  device_changed and project/take/manifest/folder/cause context on
  started/stopped/marker/recording_paused (catalogue in docs/plugins.md);
  `permissions.recordings = "read" | "write"` enforced by Seatbelt.
  Verified on isocast-m2-a: e2e 15/15 PASS with zero drops (new
  `projects`: events once with paths, rename refused while recording,
  files/manifests/timeline/export resolve after rename, flat recordings
  untouched and recovered; `plugin_files`: denials per grant; shutdown:
  kill -9 take recovered in its project folder).
- UI fast loop (2026-10-15): .slint edits show without a rebuild
  (`--features live-slint`, Slint's interpreter, dev only), a watcher
  (`--snapshot-watch DIR --crop AREA`) redraws in ~0.9 s, `--snapshot`
  repeats for several pictures per run, the `ui-dev` profile rebuilds a
  Rust edit in ~3-4 s, and visual-shots renders 4 views at once with
  VIEWS/SIZES subsets. Verified: 86900ef's fix redone in watch mode
  (0.87 s to the clipped picture, 0.91 s to the fixed one); 0 changed
  pixels base vs head and live vs compiled across all 15 views.
- G.2 (2026-10-15, #18): the replay buffer keeps the last
  `recording.replay.seconds` of the encoded program (H.264 + AAC GOPs,
  memory-capped) as a sink of a new shared program encoder (the stream's,
  when the setup has a `[stream]`: one hardware session for both); saves
  end at the request, start on a keyframe, are muxed by our own MOV/MP4
  writer and named atomically; API, CLI `isocast replay`, MCP, hotkeys,
  Controls dock and Settings. Verified: isolated-engine saves (keyframe at
  0, >= N s, A/V equal, ffmpeg and AVFoundation decode, end = request
  within one frame), e2e `replay` (record + stream with and without the
  buffer) PASS locally and on m2-b, m2-b A/B program + 2 ISOs with/without
  the buffer's own encoder 3 pairs all 1200/1200, memory cap 16 MB held,
  30-min RSS flat.
- G.1c (2026-10-15, #67): release builds embed dev.isocast.isocastd.camera
  in Isocast Engine.app, Developer ID-signed with both Developer ID profiles
  (checked before the build and after signing; builds refuse without
  them); Start asks macOS to activate it, and the app explains the
  approval. Verified: release.sh --dry-run, ad-hoc release.sh build on
  exonbook, crafted-profile and tampered-bundle refusals, on isocast-m2-b
  the activation request reaching sysextd (ad-hoc: "Missing entitlement")
  and the DMG-path refusal; offscreen vcam:approval. Real activation waits
  on the profiles.
- C.3a (2026-10-15, branch ui/edit-view, #70): timeline edits on the one
  method table (trim, split, ripple delete, angle, undo/redo; CLI
  `isocast timeline trim|split|delete|angle|undo|redo`, 6 MCP tools), one
  revision each, never touching take files. Verified: unit tests of the
  edit maths; isolated engine, three synthetic takes edited from the CLI,
  undo/redo, the timeline byte for byte the same after `isocast restart`,
  the 15 take files' sha256 unchanged.
- C.3b (2026-10-15, branch ui/edit-view, #70): exports cut to the edits.
  Verified on an isolated engine, synthetic program + 60 fps desktop +
  30 fps camera ISOs: HEVC/.mov (trims at 1.25 s etc., split, ripple
  delete, camera and desktop angles) and H.264/.mp4; every frame of every
  file (4 files, 3,734 + 1,722 frames) matched by the test pattern's bars
  to the source frame on screen at that moment of the take, 0 mismatches,
  MSE ≤ 0.8 for re-encoded frames; re-encoded frames = exactly the partial
  keyframe intervals (45/15 at 1 s keyframes) plus the camera angle (120),
  all other packets bit for bit the takes'; no decode errors; audio
  samples = duration × 48000; flash-to-beep 0 ms (30 fps files ≤ 16.6 ms,
  as in their takes); a join from mid-beep to silence and from silence to
  mid-beep stepped at most 0.092 (the tone's own largest step 0.098; a
  hard cut 0.7); take files' sha256 unchanged.
- C.3c (2026-10-15, branch ui/edit-view, #70): the Edit view in the Create
  view: an ISO track per source under Program (each clip's stretch of the
  file, the angle shown lit), an exact-frame preview of the angle at the
  playhead (right of the tracks, chosen over left), trim handles, Split,
  Delete span, Undo/Redo, S, I/O, Delete, J/K/L, arrows, 1–9 angles, ⌘Z.
  Verified offscreen (scripts/snapshot.sh, 3 recorded takes): both layouts,
  1440x900 and 900x600; S, I/O + Delete, 3 and ⌘Z through real key and
  menu events, each a method call the engine confirmed.
- 0.0.5 fixes (2026-10-15, #49 #59 #25): the app restarts an engine older
  than its bundled one (notice; after the take while recording or
  streaming), the CLI warns; ended streams report the live setup's key;
  the docs no longer claim power-loss safety. Verified on isocast-m2-b with
  two Developer ID update-test builds (1000 running and recording, 2000
  launched; idle; reverse), and a failed-stream key_set run there.
- G.9 (2026-10-15, #71): obs-websocket v5 server in isocastd
  (`websocket_*` methods, CLI, MCP; settings beside the setup, password in
  the Keychain). Verified on an isolated engine with obs-websocket-js
  (msgpack 82/82, JSON 77/77 checks), obsws-python (9/9) and obs-cmd/obws;
  Get responses match protocol.json; GetVersion p50 0.07 ms, SetInputMute
  19 ms.
- #72 (2026-10-15): each ISO file can carry its own audio tracks
  (`recording.iso.source_audio`, CLI `isocast iso-audio`, MCP
  `set_iso_audio`, Settings › Output checkboxes, the Sources dock's ISO
  badge menu, OBS Source Record "different audio"). Verified with pulse
  sources (tones 700/1700 Hz): camera ISO only the mic, screen ISO only
  desktop audio, program the mix, every track in sync (0.021 ms), across a
  split, pause/resume and a timeline export; setups without the field give
  the same documents, manifests and tracks as main.
- small fixes (2026-10-15, #75 #74 #69 #49): an ISO audio source with no
  sound is skipped with a warning instead of panicking the start (isolated
  engine, video-only media clip: before "recording panicked", after both
  fields complete with 180 frames and the tone track per file);
  obs-websocket refuses browsers and non-loopback binds without a password,
  caps clients at 32 and answers Sleep outside a batch with 206 (raw
  WebSocket client, base vs head); the camera extension pins the engine's
  exact paths (10 path cases); `isocast mcp` warns about an older engine and
  the app's couldn't-restart notice stays (fake distribution run).
- C.3c rework (2026-10-15, #70): the editor moved into its own window,
  "Isocast Editor — <timeline>" (Window › Editor, ⌘E), per Ryan; the
  studio's Create view is C.2's clip bar again. Verified offscreen: the
  editor window and a full-window-mode trial drawn side by side
  (snaps2/), keys S, I/O+Delete, 2, 3 in the editor through real key
  events; studio pictures unchanged (visual-shots, below).
- C.3d (2026-10-16, #70): on isocast-m2-b (base M2), a 30-min take with
  program + 3 HEVC ISOs (0 drops in 108,000/54,000/54,000/108,000 frames),
  six CLI edits (13-21 ms each), export of the 28:55 timeline (5 files,
  400 s of angles encoded again) in 157.9 s, 11x real time, every file
  1734.75 s. Editor window opened from Window › Editor in a real window:
  ruler drag scrub on the 30-min take 16.7 ms median preview latency (p95
  31.5); on a 2-min take J/L playback 27-30 pictures/s at 18-25 ms median,
  frame steps 38-41 ms; the studio's clip bar unchanged beside it.
- e2e `edit` (2026-10-16, branch agent/e2e-edit, #70; Ryan approved):
  `scripts/e2e.py --only edit` makes a timeline of two program + 2 ISO
  takes, trims, splits, ripple-deletes, sets an angle, undoes and redoes,
  exports, and checks every exported picture against its take frame by
  the test pattern, flash/beep at every cut, clicks at joins, and the
  takes' hashes. isocast-m2-b: 5/5 PASS strict (1080p60 HEVC, ~35 s each)
  and PASS --hosted (720p30 H.264, 18 s); FAILs on an export cut one
  frame late (298 of 433 pictures) and on an in point one frame late.
- S.2d (2026-10-16): `scripts/stress.py`, nine scenarios on isolated
  engines, and hw-batch's `stress` kind; hw-batch now runs one job per
  test Mac (pinned jobs hold `.bench.lock.<mac>`). Full run on
  isocast-m2-b with main d66fbb8 (job 20261016-032046-stress-suite-stress,
  13 min): 224 takes, 244 files and 40 replay saves all decode with frames
  accounted; sources, switching (100.0/s, p99 1.9 ms), clients (16
  plugins, 32+1 obs-websocket, 32 watchers), replay, multistream and
  text_web pass with zero program/stream drops and threads/fds back to
  baseline. isos, cycles and disk fail on engine bugs (scratch
  /Volumes/IsocastBuild/stress-suite/scratch/REPORT.md).
- G.1 follow-ups (2026-10-16, #69): after an update Start asks macOS to
  activate the app's extension build when another is active, and
  `virtual_camera_remove` / `isocast vcam remove` / MCP / the camera
  popover's Remove Virtual Camera… deactivate it. Verified on isocast-m2-b
  with two notarized Developer ID builds: build A approved and live
  (149/149 distinct frames), app replaced by B → Start replaced the active
  build in 1.3 s with no prompt (151/151 distinct); B active, app downgraded
  to A → remove replaced then deactivated it (one password), camera gone
  from AVFoundation. The approval alert names the executable and the
  extension runs as _cmiodalassistants, so neither the rename nor a nonce
  check is possible (docs/virtual-camera.md).
- #4 (2026-10-16): `isocast scene X --cut` back to ~8.8 ms from 13.4 ms
  (QA-2 build 7.7 ms) on Ryan's Mac: the CLI's diagnostics no longer
  F_FULLFSYNC their run marker or prune before the command. isocastd's own
  switch is 0.02-0.05 ms and unchanged; program latency of a cut unchanged
  (under one frame); obs-websocket SetCurrentProgramScene 0.31 ms both.
  Verified by interleaved A/B against one isolated engine (80 runs each)
  and idle/recording/streaming runs; m2-b A/B queued (hw-batch).
- #83 (2026-10-16): release engine executable renamed to "Isocast Engine",
  so the camera-extension alert says "Isocast Engine" (screenshot on
  isocast-m2-b). Grants made under the old `isocastd` build survived a
  bundle swap and a Sparkle update (TCC rows unchanged, no prompts); the
  old running isocastd was detected (#49) and replaced; recording (ffprobe
  5 s/8 s takes, 0 drops), the virtual camera (149/149 distinct frames) and
  `isocast permissions` (incl. the fresh-process restart check) work.
- #56 (2026-10-16): plugin follow-ups: the hard-link check warns instead of
  refusing on volumes without hard links; clips healed by name get their
  project id; export manifests heal after a Finder rename; plugin sandboxes
  restart with new recordings folders and not-a-project folders; standalone
  `isocast record` projects are listed. Verified: e2e projects and
  plugin_files pass on head and fail on main (export heal, id backfill,
  write probe restart); standalone record listed (head 1, main 0); exFAT
  and FAT (LINK_MAX 1) takes record in both writers; an APFS hard link
  still stops repair.
- 8.2k / #45 #55 #58 #65 (2026-10-16): restart refused in 0.06 s during
  `tune` (was: waited for it), tune unharmed; encoder_start_order intact
  across a split with the lock-free start; a report holds both developer
  segments after a toggle (23.9 kB + 392 B); offscreen snapshots for the
  Pacing row, the 6-hour auto-off and Settings after Review… (before: OK
  and Apply disabled); rtmp unit test extended; gates clean on
  isocast-m2-b.
- #74 (2026-10-16): obs-websocket follow-ups: one Get + diff per pump for
  every queued setup_changed (300 live applies: 256 session Gets on main,
  11 here, same events); identify closes 4000 after two retries when the
  tracker can't start; refused web pages logged (peer + Origin, one line
  per 10 s); `websocket enable --port/--bind` keeps no-password instead of
  generating a password; `enable` warns when ISOCAST_WEBSOCKET_PASSWORD
  overrides the stored password; GetStreamStatus/Start/Stop and
  StreamStateChanged treat every destination as one stream (main off,
  backup live: active, was inactive with 604/501). Verified on an
  isolated engine with obs-websocket-js (msgpack 80/80, JSON 77/77),
  obsws-python 9/9, local mediamtx; latency unchanged (3 interleaved
  rounds).
- C.3g (2026-10-17, branch agent/editor-007, #94): editor sound in a real
  window on isocast-m2-b within 2 ms of the picture at 1x (5 ms at 2x),
  pictures held exactly 2 refreshes 91% at 1x; e2e edit (hosted) checks
  VERTICAL's 214 pictures; 8-bit program + 10-bit ISO angle exports one
  Main description (HEVC, exonbook); editor motion suite identical across
  two lockstep runs.
- C.3f (2026-10-16, branch agent/editor-followups, #94): HEVC/H.264/ProRes
  exports have one sample description (Chrome, Safari, QuickTime, ffmpeg
  play them; AVFoundation's decode bit-identical to ffmpeg's); forward
  playback 29.6 pictures/s, 84% held exactly 2 refreshes (main 24.0, 52%)
  in a real window on isocast-m2; VERTICAL 9:16 export; angle re-snaps
  crops. e2e edit (HEVC on exonbook, H.264 hosted) and the full hosted
  suite pass; take files unchanged.
- C.3e (2026-10-16, branch agent/editor-round2, #87): crops, rolls,
  waveforms, snapping, zoom and editor keys; e2e edit scenario crops a
  program clip and an angle and checks every cropped picture (bar edges
  through the crop, band, bars) against its source frame; motion suite
  scripts/motion/editor.toml passes realtime and lockstep; studio views
  pixel-identical to main (visual-shots, 15 pictures); real window on
  isocast-m2-b (keys, zoom, play, crop mode).
- #89 (2026-10-16, branch agent/audio-channels): audio interface inputs.
  Microphone sources take `channels` (1-based; one is centred mono at full
  level, two stereo; the ISO track holds exactly those), validated against
  the device with its channel names; `isocast devices` lists channel
  names; the app has a Mono switch (Advanced Audio Properties, strip menu)
  and a Channels dropdown in a microphone's Properties; OBS imports bring
  Mono over. Verified in the test VM on BlackHole 16ch with a tone per
  channel: [1], [3], [16] and [3, 4] land exactly in program, ISO and
  monitoring (levels within 0.001 dB of the source, other tones below
  -64 dB, ISO vs program 0.00 ms, 0 audio dropouts); default setups match
  main (setup JSON, manifest and stream structure, the same mixdown).
- #99 (2026-10-16, branch agent/audio-followups): picked microphone
  `channels` are checked against the device's current input count (Core
  Audio), not the most any AVFoundation format lists, and a picked input
  that disappears while recording stops that source with the reason (like
  a disconnect) while the take records on; a count change that keeps the
  picked inputs records on. Verified in the test VM with an aggregate
  device switched between 2, 16 and 18 inputs: main accepted input 5 on a
  2-input device and recorded silence, and recorded silence without an
  error after a drop; this branch refuses the first (field path, valid
  list) and stops the source after the drop (600 Hz exact before, other
  source unaffected); 16 <-> 18 changes record input 3 exact throughout;
  default setups byte-identical `setup check --json` and the same mixdown.
- #101 (2026-10-16, branch agent/audio-stop-manifest): an audio source
  that stops mid-take (unplugged, a picked input gone, a tap ended) is in
  the take's manifest per affected file (`audio_stops`: source, track,
  take time, reason; `resumed_seconds` when the program mix gets it
  back), in the stop reply and `status.last`, and in `errors`; split takes
  after it say it stopped before they began; the app shows a notice on
  stop. Verified in the test VM: an aggregate input dropped and the whole
  device removed mid-take give entries in the program, vertical and the
  ISO whose policy includes it (not the other ISO) at the time the tone
  ends (within 10 ms); files decode; normal take's manifest identical to
  main's apart from times and counts.
- #99 default (2026-10-16, branch agent/audio-default, Ryan's call): a
  microphone source without `channels` on a device with more than two
  inputs records inputs 1 + 2 (stereo) instead of macOS's mix of every
  input; 1- and 2-input devices unchanged; the default never stops when
  the device shrinks (fewer than two inputs: input 1 in both channels,
  with a diagnostic). Release notes for 0.0.6: "changes recorded audio for
  interfaces with more than two inputs". Verified in the test VM:
  BlackHole 16ch with a tone per channel records 200 Hz left / 400 Hz
  right at -15.05 dB (the source level), every other tone below -100 dB
  (main: the 16-tone mix, -7.65 dB RMS); `setup check --json` and
  `devices --json` identical to main for all six setups; the 2-input
  Apple Virtual Sound Device records the same PCM as main; an aggregate
  going 18 -> 2 mid-take keeps the source running (logged), 18 -> 16
  keeps tones 1 and 2 exact.
- C.5 (2026-10-17, #104): `timeline_export_nle` writes FCPXML 1.10, xmeml 5
  and OTIO pointing at the takes' files. e2e edit: every track of all five
  variants matches Isocast's export frame by frame (788 frames each);
  FCPXML valid against the 1.10 DTD; OTIO's adapters read the xmeml, FCPXML
  and OTIO alike. DaVinci Resolve 21.1.1 (free, isocast-m2-b) renders the
  Resolve xmeml frame for frame as the EDITED file (0/212 off, >= 46 dB,
  same level), angles of other sizes and crops included; markers land.
- #103 (2026-10-17): a folder a plugin's request gives (`start{folder}`,
  `recording.folder` via `apply`/`start{setup}`/`init_setup`, an imported
  profile's) must already be a recordings folder (or `~/Movies/Isocast`),
  otherwise -32002. Also #56 F3 (a 5 s sandbox poll), #56 F4 (export healing
  rewrites only paths that exist where the export is now) and #77 (a join
  after the worker ended on its own starts a fresh hub). Verified with a
  real sandboxed attacker plugin: on main, apply recording.folder=secret
  then let it read, list and write the secret folder; on head all three
  attacks are refused and it stays denied. A client `start` into a new
  folder still works and restarts the plugin. Standalone record reaches a
  running read plugin in 0.3 s on head and not at all in 20 s on main.
  Export-only healing is left alone on head and wrongly rewritten on main.
- G.11 (2026-10-18): mask filters (#115): shape, gradient and source/scene
  masks with combine and keyframes; 25/25 offscreen scene snapshots match
  numpy-made expected images within 1 LSB; a 6 s take on isocast-m2-a with
  a video mask: 180/180 frames show the masked bar exactly where the mask
  video's bar is in the same frame, moving one mask frame per program
  frame; Filters handles drag/size/turn verified offscreen.
- G.11 review fixes (2026-10-18): mask-only media play and restart (on
  air with what they mask), missing masks fail closed, clock animations
  agree in filtered ISOs/previews, scene masks drawn at the tick, fit =
  none scene masks keep their size in small monitors; each reproduced on
  the reviewed build and fixed, in recorded takes on isocast-m2-a; 28/28
  pixel checks.
- G.12 / #84 (2026-10-18): per-destination canvases. Real take on
  isocast-m2-a: main 1920×1080@30 and `[[canvases]]` shorts 1080×1920@60
  streamed to two local RTMP receivers (751/1501 frames, 0 dropped, 0
  audio dropouts; ffprobe 30/1 and 60/1, largest packet gap one frame),
  program 1920×1080@30 and square 1080×1080@24 HEVC recorded, 0 drops;
  pixel checks of each received layout (arrangement, then letterbox after
  a main switch) and the square's own scene. Bench A/B single canvas
  41.9% vs 41.8% CPU, 0 drops; layout audit 40/40.
  canvases/scratch/REPORT.md.
- G.12a / #120 (2026-10-18): extra canvases in the app. Real take on
  isocast-m2-b (hw-batch 20261018-092435, desktop session): the app opened
  square and shorts program projectors and a multiview with both canvases
  (all three through `projector_open`); a display capture read back 100%
  green in the square projector, 100% red over 100% blue in the shorts
  one, and the multiview's last tiles green / red+blue; square 1080×1080
  and main 1920×1080 files 900 frames / 30 s. Offscreen design A/B, layout
  audit, gates, e2e. canvas-ui/scratch/REPORT.md.
- G.12 / #84 review fixes (2026-10-18): merged main (#110 governor);
  governor per canvas (congested `[[canvases]]` stream retuned 5000 →
  1250 kb/s while main held 6000 with 0 drops, isocast-m2-a), inline
  `CanvasId`, `recording = {}` named by canvas id (TOML take with three
  recorded extra canvases), Aitum vendor requests in the plugin's shapes,
  extra-canvas replay warm, engine canvas cap refused at apply. Gates, VM
  e2e 19/19, strict e2e 18/18. canvases/scratch/REPORT.md.
- #124 frame-oracle harness (2026-10-18): e2e.py's oracle_* scenarios
  check every picture and second marker of program, ISOs, vertical and
  extra canvases, splits, pauses, scene switches, masks, replay saves,
  edited MP4/MOV exports and FCPXML/xmeml/OTIO projects against an
  independent model; --quick (VM 1.5 min) and --full; --selftest and
  --pattern-oracle on Windows (exon-desk: NVENC + contract takes);
  bench.py --gate A/B. Verified: quick and full pass on main (VM hosted
  26 pass + the governor's hardware skip, strict M2-b 26/26); caught masks B1-B5 on a79c147c,
  double warm on a68b6f3f, the governor restore on 7705e90c and an
  injected off-by-one cut; A/B no-op PASS, 1.5 ms/frame slower FAIL.
- #124 review fixes (2026-10-18): the oracle is strict (#126's pattern
  only with --tolerate-issue-126, exact and isolated); edits, exports and
  NLE projects checked against the oracle's own timeline model; export and
  track inventories required; transitions modelled (fade share, slide
  offsets); Windows runner exits on any failed stage; bench gate
  INCONCLUSIVE on missing or unfinished measurements, zero-loss when the
  base lost nothing, alternating order; old queued bench jobs run ungated;
  merge checks use the merged scripts (`hw-batch --scripts worktree`).
  Verified: each blocker's case fails in a real run or the rules
  self-test; pre-fix commits and the injected cut still fail; quick and
  full pass on main; A/A false-fail rate measured (REPORT).
- #124 follow-up, the oracle on Windows (2026-10-18): e2e.py drives
  isocastd.exe over its named pipe; win_record/win_takes/win_scenes apply
  the oracle's model to the test pattern (no media files on Windows yet);
  unsupported parts report PENDING; `scripts/oracle-windows.sh --engine`
  builds agent/windows natively in its own lane root and runs record, iso,
  split, scenes, win_* and the canvases/masks oracle scenarios under the GPU
  lock. Verified at agent/windows 0c47af4c: 7 PASS, 2 PENDING, 0 FAIL;
  the pattern oracle fails a Windows program shifted one frame or with one
  frame missing; macOS VM quick and win_* PASS.
- W.6 shared previews (2026-10-18): GPU-only DX12 daemon→app/projectors,
  adapter/session checks, renderer-queue fenced releases and two-ring viewer
  bounds. Live studio/source dialogs/projectors verified on exon-desk;
  app-kill recording900/900 pictures decodes, oracle7 pass/2 pending,
  native strict checks/release build and portable250 tests pass. Startup,
  per-viewer CPU/GPU and app-arrival samples retained in capture REPORT;
  no cold-engine or photon-latency claim. Ready for CI, left uncommitted.
- W.5/#13a startup overlap (2026-10-19): WPR-driven parallel Windows shader
  preparation (joined before use) plus bounded UI-requested engine pre-warm.
  Five qualifying app-only cold-process pairs: median first-positive picture
  observation1.471s vs OBS1.857s; warm caches, fixed order, sampling intervals
  overlap, two native cases1.58s. No exact presentation/before-after claim.
  Native build/strict Clippy, portable250 tests, oracle7pass/2pending and
  900/900-frame resize/app-kill recording proof pass. Ready for CI; uncommitted.
- R1.d (2026-10-22): engines advertise and are found through the OS's own
  DNS-SD (`isocast-discovery`: Bonjour on macOS, `DnsService*` on Windows,
  unsupported elsewhere); `isocast engines --discover [--watch] [--json]`,
  `isocast debug advertise`. Verified basebook ↔ exon-desk both ways with
  exonbook observing: right addresses and port, found in 0.5–2 s, lost
  within ~1 s of Ctrl-C, kill -9 (Mac) and Stop-Process (Windows); name
  conflicts renamed on both; no Isocast UDP 5353 rule on Windows.
