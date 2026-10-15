# Motion testing

How Isocast checks that what you drag follows the mouse, every frame,
without glitches, and without a person watching. It came from GitHub #24
(dragging dock splitters and resizing canvas items lags behind the mouse
and glitches, worse while recording).

```sh
scripts/motion.sh                         # the standard suite, headless, on this Mac
scripts/motion.sh --scenario canvas-corner --variant recording
scripts/motion.sh --clock lockstep --no-timing   # what CI runs
scripts/motion.sh --suite scripts/motion/controls.toml   # the harness's own controls
scripts/motion/compare.py OUT_A OUT_B            # two lockstep runs must be identical
ISOCAST_TESTMAC=isocast-m2-b scripts/testmac.sh gui 'scripts/motion.sh --window --bin target/release'
```

Results go to `target/scratch/motion/<time>/` (or `--out DIR`):
`summary.md` and `summary.json` for the suite, and per run
`<scenario>@<variant>/result.json`, `frames.jsonl` (one line per frame) and
`sheet.png` (the frames around the worst moment). `scripts/motion.sh` exits
0 when every run passed (a control: came out as its `expect` says), 3 when
one failed, 1 when one couldn't run. Only this invocation's runs count
(`runs.tsv`, and results stamped with its `MOTION_INVOCATION`): a result
left in the folder by an earlier run never stands in for one that failed
to run. One run at a time: the script holds an OS lock (flock) on
`$TMPDIR/isocast-motion.lock` for its whole life, so a crash frees it; the
next run first ends every process a crashed run left (its UIs, recorded
in the work folder's marker, then its engine and log relay), then empties
the folder. Outputs in `--out` (runs.tsv, each run's json and log, the
summaries) are written to a new temporary file and renamed into place;
a target that is a symlink or not a plain file is refused. Every engine the script starts is shut down (and TERM/KILLed if
it lingers) on normal exit, failure, Ctrl-C and SIGTERM; only SIGKILL of
the script itself can leave one.

## Three layers

| Layer | Where | What it drives | What it sees | Catches |
|---|---|---|---|---|
| Headless, lockstep | CI, any Mac | the real Slint interface, offscreen, Slint pointer events, each at its own instant on a virtual clock the whole interface reads; after each input every call is answered and the picture shows the newest setup at the asked size before the frame is drawn (an engine infinitely fast in virtual time) | the interface's state, every frame's pixels (GPU readback), calls to the engine | anything structural: an edge that doesn't follow the pointer, frames of lag built into the design, jumps back, blank frames, flicker, too many engine calls. Deterministic (same pixels every run, checked), so it can gate a PR. A wait over 2 s makes the run invalid (it fails), never a drawn frame. |
| Headless, realtime | any Mac | the same, frames paced on the wall clock, the engine running as it does | the same, plus the engine's real round trips | lag and stalls from the engine (edits in flight, frames late), idle and while recording. Depends on the machine: a report, not a gate. |
| Real window | test Macs | `isocast-ui` in a window, real macOS mouse events | a second engine recording the window at 60 fps, cursor included | end to end: macOS events → winit → Slint → Core Animation layers → the screen. What a person sees. |

The headless layers run the real app code: `isocast-ui --motion` builds the
same `AppWindow` and `App` as the window does and talks to a real
`isocastd`. Only the platform is different: an offscreen window adapter
whose clock moves only when the runner says so (Slint's timers and
animations follow it, and so does everything the app times itself through
`src/clock.rs`: edit pacing, gesture tails, held frames, the LCD, meters), and the preview in its
portable mode (`--preview image`: the engine's frames as Slint images) since
there are no Core Animation layers offscreen. The real-window layer covers
the layers path.

## Scenarios

Suites are trusted developer input, but the harness still keeps them in
their box: scenario names are plain (letters, digits, `-`, `_`) because
they name the run's folder, which must be a plain child of `--out`; the
harness removes only the files it writes there (never recursively); `state`
steps come from an allowlist that only changes what the window shows or
the isolated engine's live setup (no file import/export, settings,
streaming or bug reports); and `isocast-ui --motion` refuses to run unless
`ISOCAST_SOCKET` and `ISOCAST_SETUP` point it at an isolated engine
(scripts/motion.sh sets them). The work folder is only emptied if its name
starts with `isocast-motion`, it isn't a symlink, and it carries the
marker the script writes.

A suite is TOML (`scripts/motion/suite.toml`, the standard one):

```toml
[defaults]
size = "1280x800"        # window points
scale = 1
fps = 60                 # the virtual display
picture = "engine"       # engine: the daemon's frames; test: a stand-in drawn from the box
clock = "realtime"       # or lockstep
variants = ["idle", "recording"]

[defaults.thresholds]
gap_pt_p95 = 2.0
lag_frames = 1
jump_back_frames = 0
stall_frames = 2
black_frames = 0
flicker_frames = 0
frame_ms_max = 16.7

[[scenario]]
name = "splitter-scenes"
target = "splitter:0"
before = []              # --state steps first, e.g. select-first-item, snap:off
steps = [
  { press = "target", ms = 50 },
  { move = [120, 0], ms = 600, easing = "ease-in-out" },
  { move = [-120, 0], ms = 600, easing = "ease-in-out" },
  { release = true, ms = 100 },
]
```

Steps: `press` (`"target"`: the target's grab point, or `[x, y]`), `move`
(by `[dx, dy]`) or `to` (`[x, y]`) over `ms` at `hz` events a second
(default 120, like a mouse) with `easing` (`linear`, `ease-in-out`,
`ease-out`), `release`, `wait`, `wheel` (`[dx, dy]` over `ms`), `key`, and
`state` (any `--state` the snapshot mode knows, for menus, dialogs,
toasts, Studio Mode's take…). Variants: `idle`, and `recording` (the
program and two ISO files of synthetic sources, started through the
Controls dock as a person would).

Targets, and how each is found in a frame:

- `splitter:N`: the Nth column splitter, left to right. State: the
  `ColumnSplitter` element's position (Slint's element search). Pixels: its
  1 px hairline (`Theme.line`), followed from frame to frame along a row
  through the docks.
- `row-splitter`: above the docks. State: the `RowSplitter` element. Pixels:
  the edge's profile down a column, taken at rest and matched each frame.
- `canvas-corner`, `canvas-move`: the selected item (the setup's magenta
  block, or the test picture's verdigris item with `picture = "test"`).
  State: the newest setup's box; the box drawn for the frame shown; the
  drag-test outline. Pixels: the item's right (or left) edge in the
  picture, along a row through the item where the frame shows it (found
  again each frame).
- `none`: no tracking, for scenarios that only check flicker and blank
  frames.

The standard suite: the Scenes|Sources splitter, the Mixer|Transitions
splitter (it resizes the dock after it), the splitter above the docks (it
resizes the preview, and with it the engine's frames), a canvas
bottom-right corner resize, and a canvas move (fixed in October, the
control). Each idle and while recording.

## What a run records and checks

Per frame (`frames.jsonl`): the pointer and whether it is pressed; the
edge in the interface's state, in the box drawn and in the pixels; the
interface's own time for the frame (inputs, timers, updates) and its
drawing time; how long it waited for the engine; redraws Slint asked for;
calls to the engine made in the frame and their size; engine frames
received and the setup revision they show; blank and black frames; tiles
(16 points) that went back to how they were two frames before.

Frame 0 is drawn before any input. Where the edge should be: at each
press, wherever it was in the frame before; from then to the frame with
the release (at the release's own position) moved as far as the pointer
has moved since the press; held after. Each gesture is measured from its
press to a third of a second after its release. Checks (`result.json`,
each against the suite's threshold):

- **coverage** (always, no threshold): a tracked target missing from any
  frame of a gesture, an unknown state, no gesture at all, or (lockstep)
  the engine not catching up fails the run. A measurement that couldn't
  be made is never a pass;
- **gap** (window points, `gap_pt_*`) between that and the edge on
  screen: median, p95, max;
- **gain** (`gain_off_by` against `gain_tolerance`): the edge's travel
  over the pointer's travel through the gesture, from the rest frame (1
  follows, late or not; 0.5 is the #24 splitter);
- **lag** (frames): the delay that best lines the edge up with the pointer,
  and the gap left after it (`residual_pt_p95`: wrong place, not late).
  Always reported and checked, whatever the gain;
- **jump-backs**: frames where the edge stepped back against the pointer
  by more than a device pixel;
- **stalls**: frames where the pointer (after the lag) has moved more than
  1.5 device pixels since the edge last moved and the edge didn't move. An
  edge drawn on the pixel grid moves in whole pixels, so a slow drag
  isn't a stall;
- **black/blank frames**: the picture black, or the window one colour;
- **flicker**: A-B-A tiles while the pointer goes one way (where it turns,
  positions repeat by design; within two frames of a press or release the
  interface changes state). Tiles that do it at rest (a blinking light) are
  found in 1.5 s of calibration first and left out, and so is the dragged
  object's own area (its texture moving can repeat a tile; whether it
  keeps up is the gap's business);
- **frame time** (`frame_ms_max`, `late_frames`): the frame finished: the
  interface's work, its drawing commands, the GPU's drawing and the read
  back (`cpu_ms` without the GPU is reported apart). Wall time, so only on
  local runs; CI passes `--no-timing`. Wall numbers live under
  `result.json`'s `wall`.

The same checks run on the interface's state (`in_state`) and the drawn box
(`drawn_box`), so a failure says where the lag comes from: the state
following the pointer but the pixels not (drawing, engine), or the state
itself wrong (the interface's logic). On failure, `sheet.png` shows twelve
frames around the worst one, cropped where the pointer went, with the
pointer (red cross), where the edge should be (cyan) and where it was
found (yellow). `--frames` writes every frame's crop (for a video), and
`--dump-frames 10,11` whole frames.

## Controls

`scripts/motion/controls.toml` checks the harness itself: runs whose
verdict is known. A good drag, a slow drag (under a pixel a frame), a
diagonal drag and a press with motion batched into its first instant, on
the offscreen test picture (drawn from the box, so they follow by
construction), must pass; the good drag with an injected fault must fail:
`inject = "lag:3"` (inputs reach the app three frames late), `"jump:20"`
(one frame's pointer 30 points off), `"blank:20"` (one frame not drawn),
`"freeze:15"` (ten frames of moves never reach the app). A control
expected to fail names the checks it must fail (`expect_failing`), so it
can't pass by failing for another reason. A release followed at once by
hovering far away must pass (the expectation holds at the release's own
position). The real-window analysis has its own controls on known rows:
`scripts/motion/window.py --selfcheck` (follows, 4-frame lag, frozen edge,
half speed, missing frames).
The splitter on main (#24) must fail too, until it's fixed. Run it after
any change to the harness.

## Determinism

Lockstep runs of the same build must draw the same pixels.
`scripts/motion/compare.py` compares two or more output folders frame by
frame (pixels hashed, positions, revisions, calls) and result by result,
leaving out only wall-clock numbers. What it took: the virtual clock
through the app (src/clock.rs), each input at its own instant, waiting for
every call and for the engine frame of the newest setup at the asked size,
replies applied before events of the same batch (two connections race),
the engine's live readouts (status, CPU) stilled, revisions counted from
the replay's start, and a fixed work folder (its path shows in the status
bar).

## Real window (test Macs)

`scripts/motion.sh --window` (scripts/motion/window.py), through
`scripts/testmac.sh gui`, holding the benchmark lock:

1. a test engine with `scripts/motion/setup.toml` and the app in a window
   (background mode, its own layout file);
2. the scenario run headless at the window's size, for the pointer's path
   and the grab point;
3. a second engine recording the window (window source, cursor shown,
   60 fps, ISO HEVC at high quality); with `recording`, the test engine
   records too;
4. the drag, as real macOS mouse events: `scripts/motion/pointer.swift`
   posts the whole path at 120 Hz with each event's host time when the
   process may post events (Accessibility); otherwise cua-driver (which has
   Accessibility on the test Macs) makes one straight drag of the
   scenario's first move;
5. the recording read frame by frame: the edge against the pointer (a
   frame of the drag where either isn't found makes the measurement
   invalid; frames keep their places, so lag is in capture frames). With
   cua-driver the pointer is the cursor in the same captured frame, so no
   clocks need lining up (end-to-end gap and **gain**: how far the edge
   moves per point the pointer moves); with pointer.swift, against the
   posted events through the host clock in the recording's manifest
   (`host_time_ns`), good to about a frame.

Measured so far: column splitters. Not yet: the row splitter (no tracker in
the window recording) and the canvas (the captured cursor sits on the
white corner handle, which the cursor finder would take for it; with
pointer.swift's event log it wouldn't need the cursor).

## CI

The `motion` job in `.github/workflows/ci.yml` (macOS 15 arm64, GitHub-hosted,
read-only token) runs on pushes to main and on pull requests that touch
`crates/isocast-ui/`, `scripts/motion.sh`, `scripts/motion/` or ci.yml
(other pull requests skip its steps and pass, so it can be required). It
builds `isocast`, `isocastd` and `isocast-ui` (release), then

```sh
scripts/motion.sh --suite scripts/motion/controls.toml --no-timing   # the harness's own controls
scripts/motion.sh --clock lockstep --no-timing                       # the standard suite
```

and uploads both output folders as `motion-results`. It is a real gate:
lockstep draws the same frames on any machine (the virtual clock drives
the interface, the engine is waited for after each input, a 2 s wait makes
the run fail as invalid) and frame times aren't checked. The standard suite
has open issues, recorded in `scripts/motion/suite.toml` as `expect =
"fail"` with exactly the checks each fails; the job fails if any run's
failures differ from that (a regression, or a fix whose line wasn't
updated).

## Prior art, and what was taken

- **Slint's testing backend** (`i-slint-backend-testing`): element search by
  type and accessible name, mocked time. Taken: the element search, to
  find splitters by type (debug info is already on for `--a11y`), and a
  platform whose clock only moves when told. Not its window adapter: it
  doesn't draw, and the pixels are half of what's checked.
- **Chromium's input latency and frame pacing** (event-to-frame latency
  traced per input, dropped/janky frames in frame sequences, smoothness
  metrics): the per-frame record of which input a frame shows, latency in
  frames rather than milliseconds, and checking dropped frames apart from
  latency.
- **Apple Instruments, Animation Hitches** (hitch time ratio; a commit
  hitch vs a render hitch): the split between the interface's own work per
  frame and its drawing, and the idea of naming where a late frame came
  from (state vs pixels vs engine).
- **Flutter `integration_test` and `FrameTiming`** (build and raster time
  per frame, `traceAction` around a scripted gesture, percentiles in a
  summary): scripted gestures over a real app with per-frame timings
  summarised as p50/p95/max, and timings as a report rather than a gate on
  shared runners.
- **Playwright tracing** (an action timeline with snapshots before and
  after each action): every run keeps its frames' facts and a picture of
  the moment it went wrong, so a failure can be understood from the
  artifact alone.

## Gotchas

- Slint's `TouchArea` reports `mouse-x` relative to where the area is
  *now*. A splitter that moves with the drag and computes its size from
  `mouse-x - pressed-x` feeds its own movement back: it settles at half
  the pointer's distance (#24, measured both headless and in a real window).
- 4:2:0 video can't be cropped to an odd height (`ffmpeg crop` fails
  quietly in a pipe); the window recording's levels differ from the
  theme's (`#242424` on `#121212` came out `#141414` on `#080808`), so lines
  are found by contrast.
- The test Macs' desktop bridge may not post events (no Accessibility):
  pointer.swift says so and window.py falls back to cua-driver.
- A relative recording folder resolves against the engine's working
  directory: motion.sh writes the work folder's absolute path into the
  setup.
