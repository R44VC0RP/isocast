# The web app (retired)

Since N.3 (2026-10-13), Isocast.app is the Slint app, `crates/isocast-ui`
(docs/design.md, src/areas/mod.rs). This page describes the web app it
replaced. Its crate stays in the workspace, outside the default build,
until Ryan has used the Slint app for the alpha week; build it with
`ISOCAST_APP_TARGET=isocast-app scripts/app-dev.sh`.

`crates/isocast-app` was the desktop app: a native macOS window whose whole
interface is a web page, a thin client of `isocastd`. It can do nothing
the CLI and MCP can't: every change goes through the daemon's methods
(`apply`, `switch_scene`, `preview`, `take`, `start`, `stop`, `frame`…).

## Shape

- **Shell** (`src/mac`): an AppKit window with a plain title bar
  ("Isocast — <setup name>", in the interface's colour) over a
  `WKWebView`. Menus, after OBS's where Isocast has the function: the app
  menu (About, Settings… ⌘,, Hide, Quit), File (Show Recordings, Add
  Scene ⇧⌘N, Add Source… ⇧⌘A, Settings), Edit (Undo/Redo, the text
  field's Cut/Copy/Paste, Paste (Duplicate), Transform ▸ with ⌘E ⌘R ⌘F ⌘S
  ⌘D, Order ▸, Filters…, Properties…, Lock Preview, Advanced Audio
  Properties), View (Studio Mode ⇧⌘S, Source Toolbar, Status Bar, Reset
  UI, Full Screen), Docks (Lock Docks, Reset Docks, each dock), Profile
  and Scene Collection (as OBS has them: New…, Duplicate…, Rename…,
  Remove… on the one in use, Import… and Export…, then every profile or
  scene collection in the library, the one in use checked; choosing one
  switches), Tools (Auto-Configuration Wizard…: the first run), Window,
  Help (Keyboard Shortcuts ⌘/: Settings → Hotkeys). Items the interface
  handles reach it as `menu` events by name (`MENU_ACTIONS`); it keeps
  their check marks in step with `menu_state`. Profile and Scene
  Collection change with the library, so the interface fills them
  (`ui/ui/library.js`, `menu_items`), and their entries' `menu` events
  carry the entry's `id`.
- **Files**: `ui/` is built into the binary (`build.rs`) and served at
  `isocast://app/` by a `WKURLSchemeHandler`, so ES modules load normally.
  `ISOCAST_UI_DIR=crates/isocast-app/ui` serves it from disk instead:
  edit, then reload or take a new snapshot, no rebuild.
- **Calls**: `window.webkit.messageHandlers.isocast.postMessage(json)`
  with `{"method", "params"}` is relayed to the daemon on a worker thread
  (`isocast-client`, pooled connections) and answered through
  `WKScriptMessageHandlerWithReply` as `{"result"}` or
  `{"error": {"code", "message"}}` (code −32099: the daemon can't be
  reached). `{"native": …}` runs a window command: `title`,
  `menu_state`, `menu_items`, `quit`, `window`, `open_file`, `save_file`,
  `reveal`, `open_url`, `read_setup_file`, `library_import_file` (a file
  to import as the `library_import`/`library_import_obs` call that adds
  it), `library_export_file` (`library_get`'s document written as TOML or
  JSON; both with the CLI's code, `isocast_client::library`),
  `connection`, `reconnect`, `ready`, `log`.
- **Events**: one more connection runs `watch`; every event (and the
  shell's own `connection`, `watching`, `menu` and `window` events) reaches
  `window.__isocastEvent`. The app launches the daemon like the CLI does
  and reconnects with back-off.
- **Pictures**: each monitor reads `isocast://app/_frames/<target>?w=<px>&fps=30&id=<n>`
  (`program`, `preview` or a source id) as a stream: the shell opens the
  daemon's `frames` connection and passes its binary records (low-latency
  hardware H.264, see `frames` in `isocast_protocol::api`) to the page as
  they arrive. A worker per monitor (`ui/ui/monitor-worker.js` running
  `monitor-core.js`) reads and decodes them with WebCodecs and writes the
  frames to a `VideoTrackGenerator` whose track a `<video>` over the
  canvas plays: the media player puts frames on screen without the page's
  main thread, which a drag keeps busy (a canvas, even an OffscreenCanvas
  drawn in the worker, only updated as often as the main thread renders).
  Without that, frames are drawn with WebGL (`ui/ui/painter.js`; a 2D
  canvas converted them on the CPU) in the canvas handed to the worker,
  or by the page where a canvas can't move to a worker. Monitors run at
  the canvas's rate (at most 60), as OBS's preview does. A viewer can
  choose 30 fps to save power (Settings → General → Preview Frame Rate, a
  localStorage preference); then, while the user drags, resizes, crops or
  nudges on the canvas editor over a monitor (and 500 ms after), that
  monitor asks for the canvas's rate with `{"native": "frames_rate", "id",
  "fps"}`, which the shell sends down the stream's connection: same
  stream, no new keyframe. Program and preview frames say which setup
  revision they were drawn from (a `revision` record before a frame that
  shows a new one); the monitor tells the canvas editor over it, which
  draws its boxes at that revision's layout (`setupAt` in
  `lib/session.js` keeps recent confirmed setups), so boxes and picture
  move together, as in OBS (on the video route a display frame after the
  frame is handed over, when the player shows it); without a live picture
  the boxes follow the newest setup. A resize asks for the new width; a
  hidden window (covered by another, minimized) lets go, and the picture
  comes back when it shows again. Daemons without `frames` (501), or a stream that can't start
  (no hardware encoder free), fall back to `/_frame/<target>?after=<seq>&w=<px>`:
  JPEG bytes from the `frame` long poll, one request at a time, at most 30
  a second (204 when nothing new came), or PNG snapshots from daemons
  without `frame`. `/_schema.json` is the setup's JSON Schema, from which
  source and filter settings forms are drawn. The native Slint app
  (PLAN N) shows pictures without copying instead: `shared_frames`, GPU
  surfaces shared with the daemon (docs/shared-frames.md).

## Interface

Plain ES modules, no build step: Preact 10.29.8 and htm 3.1.1 vendored in
`ui/vendor` (with their licences), Lucide icons and the brand mark in
`ui/ui/icons.js`, and one stylesheet of tokens (`ui/app.css`; the design
language, its tokens and per-component rules are in `docs/design.md`).
Type is the system font, with Instrument Sans (SIL OFL,
the brand's secondary face, also in `ui/vendor`) for brand moments only:
the welcome and first-run titles.

The window is laid out as OBS Studio's default, so someone who knows OBS
finds everything where they expect it (`ui/ui/app.js`):

- **Preview** (`stage.js`): the canvas letterboxed on a dark surround,
  where the scene on air is edited. Studio Mode splits it into Preview
  (edited) and Program, with Transition, quick transitions (Cut, Fade,
  Fade to Black) and the transition's gear in a column between them.
- **Source toolbar** (`source-toolbar.js`): the selected source,
  Properties, Filters, and its type's quick setting.
- **Docks** (`docks.js`), resizable, sizes per viewer (localStorage),
  View → Reset UI and Docks → Reset Docks restore them: Scenes
  (`scenes.js`), Sources (`sources.js`: eye, lock and ISO per row; OBS's
  right-click menu with Order, Transform, Show/Hide Transition, Scale
  Filtering, Blending Mode, Record ISO File, Lock, Filters, Properties),
  Audio Mixer (`mixer.js`: audio-only sources, which play in every
  scene, and the sources with sound the scene on air or in preview shows;
  meters drawn outside Preact, so levels never re-render anything), Scene Transitions (`transitions.js`) and Controls
  (`controls-dock.js`: Start Streaming, Start Recording with marker and
  split while recording, Studio Mode, Settings, Exit).
- **Status bar** (`statusbar.js`): dropped frames, bitrate and health
  while live, LIVE and REC timers (they open the stream's and the
  recording's details), CPU (`status.cpu_percent`) and FPS
  (`status.engine.fps` of the canvas's).
- **Dialogs** (`dialog.js`, one at a time): Properties (`properties.js`:
  a live preview of the source, its settings from the schema, Defaults,
  OK / Cancel that puts it back; settings with known choices are
  dropdowns, `device-fields.js` and `lib/devices.js`: cameras,
  microphones, displays, apps and their windows from `devices`, read
  again when the dialog opens and with ↻; a camera's Resolution and
  Frame rate from the chosen camera's formats, with what Auto picks on
  this canvas and the engine's measured rate under them,
  `received_fps`; a value that isn't there now stays chosen, marked
  "not connected"), Filters (`filters.js`), Edit Transform
  (`transform.js`), Advanced Audio Properties (`audio-props.js`),
  Create/Select Source (`add-source.js`), transition properties, Rename,
  and Settings (`settings.js`: General, Stream, Output, Audio, Video,
  Hotkeys, Advanced over a draft, sent as one patch on Apply or OK).
- The first run (`onboarding.js`, `states.js`) is a dialog over the
  empty window.

- `lib/session.js`: the live setup with optimistic edits. `edit(patch)`
  shows a merge patch at once and sends `apply` patches in order, one at a
  time (a drag's patches with the same key merge, sent at most every
  50 ms); a stale revision is reloaded and the edit retried once; refused
  changes show the daemon's message and revert. ⌘Z/⇧⌘Z undo whole-setup
  states. `lib/patch.js` mirrors the daemon's id-merging patches.
- `lib/transform.js`: Fit, Stretch, Center, Reset and Rotate.
- `ui/canvas.js`: selection, move, resize (Shift keeps the shape),
  Option-drag to crop (in source pixels, from the engine's source sizes),
  snapping to the canvas and other items (⌘ to place freely; Settings →
  General), arrow keys; Edit → Lock Preview stops moves.

## Seeing it

```sh
isocast-app --snapshot out.png [--size 1440x900] [--scale 1|2] \
  [--wait-ms 1500] [--route settings|onboarding|icon] [--eval JS]
```

renders the interface off screen (a private, throwaway web data store),
waits for it to say it is ready plus `--wait-ms`, and saves a PNG. `--eval`
runs a script first; `window.app` has helpers (`app.select(scene, key)`,
`app.studio()`, `app.scene(id)`, `app.menu(action)` for any menu
command, `app.dialog(type, props)`, `app.properties(id)`,
`app.filters(id)`, `app.settings(tab)`, `app.addSource()`,
`app.edit(patch)`, `app.undo()`, `app.state()`). Synthetic
`PointerEvent`s drive the canvas and lists. Errors in the page go to the
app's stderr.

Don't use `screencapture` to check the real window: it shows a macOS
privacy prompt on the user's screen.

## Packaging

`ISOCAST_APP_TARGET=isocast-app scripts/app-dev.sh` builds the web app as
`Isocast.app` (`dev.isocast.app`, the binary renamed from `isocast-app`,
which can't be `Isocast` beside the CLI's `isocast` on a case-insensitive
disk) with the icon compiled by Xcode's actool, signed with the Apple Development identity (else ad hoc).
`ISOCAST_APP` says where; `~/Applications/Isocast.app` by default. The
daemon keeps its own signed bundle (`scripts/sign-dev.sh`).
