# Themes (design)

Status: design, 2026-10-13, revised the same day after a taste critique
and a technical critique (section 14 lists what changed and which
findings were rejected). Covers PLAN section T. Nothing here is built
yet. It answers Ryan's question: he likes four of the five Paper
directions (A Workbench, C Console, D Daylight, E Blender), can't pick
one, and likes each for its signature pieces, not its colours. He wants
to swap between them, signature pieces included, and asked whether that
can be part of the plugin system. Switching may restart the app.

It rests on four pieces of work done the same day: an inventory of every
signature piece on the four Paper files, a reading of Slint 1.18.1's
source, a fit with the plugin system, and a spike that built the mechanism
inside the real app and measured it (commit c72831d on `spike/themes`,
not merged). Numbers quoted here come from that spike unless they say
otherwise. The decisions are listed at the end with the alternatives that
were rejected, so Ryan can overrule any of them.

## Summary

- **A theme is** tokens (colours, sizes, radii, the canvas's selection
  handles), fonts, a choice of piece for each slot (the meter, the number
  field, the dock header, the LCD, the dialog header…), layout tokens
  that shape the shared containers (docks, dialogs, where Settings'
  sidebar sits, form rows, sections), and whether it is dark or light.
- **Console is the default.** N.5 is rebuilding the kit in the Console
  direction, so every slot has a Console piece. A plugin theme names a
  `base` and inherits what it leaves out. The four built-in themes set
  every token and every slot themselves, so a change to Console never
  shifts Workbench, Daylight or Blender.
- **Chosen at launch.** The app reads the choice before it makes its
  window. Switching relaunches the app: it saves its layout and window
  frame, then exec's itself. That took 0.5 s on a base M2. Recording and
  streaming run in isocastd, so they carry on: 24 relaunches during a
  recording and a stream lost 0 frames. A relaunch never takes focus from
  the app Ryan is in. A theme that crashes the app is skipped at the next
  launch, and `isocast-ui --safe` always starts in Console.
- **Compiled pieces.** All four directions' pieces are compiled into the
  one app. A `Look` global, set once before the window shows, picks one
  piece per slot with `if`. Only the chosen piece is ever built. The
  dispatchers are generated from the slot registry. The spike measured no
  CPU cost for the dispatch and +2.1 MB of binary for two pieces and two
  themes' fonts; its baseline was the old copper look, so T.3 measures
  again on Console.
- **Containers stay shared.** Slint 1.18 can't put `@children` inside an
  `if`, so a container (Dock, Dialog, form row, section) can't be swapped.
  Its look comes from layout tokens, and its non-container parts (headers,
  footers, rows) are slots.
- **One method table.** The theme is an engine-wide app setting kept by
  isocastd in `app.toml`: `theme_list`, `theme_get`, `theme_set`, event
  `theme_changed`; `isocast theme list|show|set|check`; MCP `list_themes`,
  `get_theme`, `set_theme`. Each app reports what it actually shows
  (fallbacks, a restart it is holding back), so the CLI and agents see
  the same thing Ryan does. The app also offers Settings › General ›
  Appearance and View › Theme.
- **Themes are a plugin kind.** A `[[themes]]` section in
  `isocast-plugin.toml` carries data only: a tokens file, fonts, previews,
  a `base`, and for each slot the name of a compiled piece. So a 15-line
  theme can mix Daylight's colours with Workbench's number fields and
  Console's LCD, with no code. A theme-only plugin asks for no
  permissions and installs without `--allow`.
- **Ready means every signature piece draws.** Each built-in theme has a
  checklist of the pieces Ryan's pitch for it names (section 4.2). Until
  all of them draw, the theme is a **preview**: usable with `--preview`,
  shown only under "Previews" in the picker, never under "Ready for
  Ryan". Some pieces need data the engine doesn't have yet (loudness,
  free disk while idle, media position, form sections, the file-name
  template, the action table), so each theme's "ready" step waits for
  its data slices.
- **Plugin code pieces wait.** Loading a theme's own `.slint` files at
  launch works (spike: slint-interpreter in a ComponentContainer), but it
  rests on experimental and hidden Slint APIs, costs 8 MB and about 2
  points of CPU for three meters and a field, and is no sandbox. New
  pieces come into Isocast as source, as built-in themes do.
- **Plan:** T.1–T.3 build the mechanism with Console only and change no
  pixel. T.4–T.6 add Workbench, Daylight and Blender area by area as
  previews, each checked board by board against its Paper file, then a
  "ready" step once its data has landed. T.7 adds the data. T.8 lists
  what waits (tabbed docks, workspaces, real windows, code pieces). T
  starts after A (A lists "other themes" as waiting).

## Rules this design keeps

- **One method table.** The theme choice is a method, a CLI command with
  `--json`, an MCP tool and an event. No theme adds a GUI-only
  capability. A theme can't add, remove or rename a menu item or a
  command; it only changes how the same things look.
- **Ids, not names.** Themes are `console`, `workbench`, or
  `<plugin id>/<theme id>` for plugins, as plugin kinds are named.
- **Agent-friendly errors.** Every check names its field path, and a file
  and line where there is one, and lists the valid options.
- **Never lose a recording.** A theme switch never touches isocastd. A
  theme can't hide the recording state: the colours that say "recording",
  "live" and "warning" keep their contrast, the controls that show them
  keep a minimum size, and each state has a word or shape besides its
  colour.
- **No focus stealing.** A relaunch leaves the frontmost app frontmost.
- **Clean-room.** Values come from our own Paper files, read with
  `get_tokens`, `get_jsx` and `get_computed_styles`, never from
  screenshots. Nothing comes from OBS's source, `.ui`, `.qss` or theme
  files. The Blender direction is our drawing of Blender's style: nothing
  is taken from Blender's source, theme XML or icon sheets.
- **Small changes.** No runtime embedding machinery, no new crate in the
  build beyond Slint's `unstable-fontique-011` feature (Slint is pinned at
  `=1.18.1`); the font and image-header checks use crates already in
  `Cargo.lock` through Slint.

## 1. What a theme is

```text
theme = tokens        colours, sizes, radii, type, motion, canvas handles (Theme global)
      + fonts         .ttf/.otf files, registered before the window exists
      + slot pieces   one compiled piece per slot (Look global)
      + layout tokens how the shared containers are shaped
      + appearance    dark or light (native menus, panels, title bar)
```

- **Tokens** are every property of the `Theme` global. On
  `ui/console-main` (N.5) that is 226 properties: Paper's 97 token names
  (minus the `--`) and the literals the Console boards repeat (radii, LCD
  type sizes, sheet padding, capsule sizes). The layout tokens and the
  canvas tokens below are added to them. The
  registry `TOKENS` lists each with its type and allowed range, and
  `build.rs` generates the `Theme` global from it (section 2.2).
- **Fonts** are files the theme carries. Tokens name the families
  (`font`, `font-ui`, `font-brand`, `font-mono`, and a new
  `font-display`). Every live number uses `font-mono`, because Slint 1.18
  has no tabular figures (all four directions do this).
- **Slots** are the kit's signature components. Each has one interface
  and one piece per direction. A theme names a piece per slot.
- **Layout tokens** shape the shared containers: the gap between docks,
  their radius and seams, where a dialog sits and whether it has a scrim,
  whether a sidebar dialog's sidebar runs the full height, a form row's
  label alignment and value chrome, a section's background, the mat
  around a picture, the canvas's selection box.
- **The base.** A plugin theme names a `base` (default `console`).
  Anything it doesn't set comes from the base, so the chain always ends
  in a complete theme. The built-in themes don't lean on it: each sets
  every token and every slot, and a test fails if one is missing
  (section 8).

What a theme never changes: the OBS layout and its words, menus,
shortcuts and commands, the set of actions (every action is reachable in
every theme, section 9), behaviour (keys, scrubbing maths, drafts and
apply rules), what screen readers are told, the arrangement of the frame
and docks (that is layout, later workspaces, section 2.5), and anything
in isocastd: the program picture (including a text source's font: the
boards' lower thirds are drawn in each direction's face, but that is
scene content), recordings, streams.

## 2. How it works

### 2.1 Chosen once, at launch

The app resolves its theme before it makes the window:

1. The choice: `--safe` (Console, and nothing else is read), else
   `--theme ID`, else `$ISOCAST_THEME`, else `$ISOCAST_RELAUNCH_THEME`
   (set by a relaunch, section 5.1), else `app.toml`'s
   `appearance.theme`, else `console`. The first three are per run and
   never saved (for snapshots, test Macs and side-by-side comparisons).
2. **The launch marker.** If `isocast-ui.launch` (beside
   `isocast-ui.json`) names the chosen theme, the last launch with it
   never drew a frame. The app uses Console instead, keeps the marker,
   and says so: "Isocast didn't start with Daylight desk last time, so it
   opened in Console. `isocast theme set` chooses again." The marker is
   dropped when the choice changes or `--theme` asks for that theme
   explicitly. Otherwise the app writes "starting X" to it now.
3. `isocast_protocol::theme::resolve()` reads the theme's manifest (built
   in, or from the plugins folder), follows its `base` chain, merges the
   tokens, checks each value, picks each slot's piece, and lists every
   fallback it had to make. It applies every limit itself (no cycle, at
   most four deep, file types and sizes, value ranges, section 7.2):
   nothing relies on the checks `plugin add` ran, because a linked theme
   is edited after install and a `base` in another plugin can change
   when that plugin is upgraded.
4. The app registers the theme's fonts with fontique and compares the
   families registered with the font tokens.
5. It makes the window (`AppWindow::new()`), sets every `Theme` token and
   every `Look` slot, sets the native appearance, gives the native canvas
   layers their look, loads the saved layout and window frame, and shows
   the window (without activating after a background relaunch, 5.1).
6. At the first frame it clears the marker. Once connected it reports
   what it shows (`theme_report`, section 6.2).
7. Once connected it asks `theme_get`. If isocastd's choice differs from
   the theme it launched with (and no `--theme`, `$ISOCAST_THEME` or
   `--safe` was given), it relaunches once, passing isocastd's choice.
8. If anything fell back, it shows one toast, logs one line that lists
   it all, and includes it in the report.

The app reads `app.toml` and the plugins folder itself, so the very first
frame (the "connecting" screen) is drawn in the right theme and launch
never waits for isocastd. The spike measured the theme step (fonts,
tokens, pieces) at about 6 ms, and warm launches to the first frame with
live meters at 177–186 ms in every theme.

Rust: `crates/isocast-ui/src/theme.rs` (the spike's `skin.rs` without the
interpreter), plus code generated by `build.rs`.

### 2.2 Tokens and fonts are data

- **One source of truth.** `isocast_protocol::theme::TOKENS` names every
  token with its type and range. `build.rs` generates
  `ui/theme-tokens.slint`, the `Theme` global's `in` properties, with
  Console's package values as their defaults, and a typed
  `set_token(window, name, value)` for each. `theme.slint` keeps only
  values derived from tokens. Nothing has to be kept in step by hand.
  The spike made the tokens `in` properties this way: its default theme
  was pixel-identical to main (0 of 4,096,000 pixels differed at
  1280×800, 2x) and cost no CPU (17.8% against 18.1%).
- A theme's tokens come from `tokens.css`: Paper's `get_tokens` output,
  pasted verbatim (`--name: value;`, one per line, comments allowed).
  Paper is where the directions are designed, so its output is the
  exchange format; no hand translation. Values beyond Paper's 97 go in
  the manifest's `[themes.set]` table, which also overrides `tokens.css`
  (later wins).
- Values: colours as `#rgb`, `#rrggbb`, `#rrggbbaa`, `rgb(r g b / a%)`;
  lengths in `px`; `em` letter-spacing turned into px against the font
  size the registry pairs with that token; weights as numbers; durations
  in `ms`; booleans; enums by name; font families as strings. Numbers
  must be finite and inside the token's range: `NaN`, `inf`, a negative
  size or a zero font size is refused (the spike's parser accepted all
  three).
- Fonts are registered at launch with
  `slint::fontique_011::shared_collection().register_fonts()` (feature
  `unstable-fontique-011`), before the window exists. A Text measured
  before its font is registered keeps the fallback face, which is why
  this must happen first (seen in the Slint study: 327 px stayed 327 px
  until a new instance measured 338 px). `register_fonts` returns the
  families it registered; a font token naming a family that isn't among
  them falls back (7.5). Only the chosen theme's fonts are registered,
  so a misspelt family can't silently land on another theme's face.
  Built-in fonts are bytes in the binary (`include_bytes!`); plugin fonts
  are read from the plugin's folder within the limits of 7.2. The
  `.slint` `import "*.ttf"` lines leave `theme.slint`.

### 2.3 Slots: compiled pieces, picked with `if`

Each slot has three parts:

- **The behaviour base**, `ui/slots/<slot>-base.slint`, hand-written:
  focus (the FocusScope), keys, pointer maths (scrubbing, dragging),
  every `accessible-*` property, and the slot's actions. This is how
  Slint's own styles share `SliderBase`, except that our pieces never
  declare accessibility themselves.
- **The dispatcher**, `ui/slots/<slot>.slint`, generated by `build.rs`
  from `SLOTS`. It inherits the behaviour base, so it is the focusable
  element, and holds one `if` branch per piece compiled into this build.
  `kit.slint` exports it under the kit's existing name (`NumberField`,
  `Meter`, `PanelHeader`…), so the areas don't change.
- **The pieces**, `ui/themes/<theme>/<slot>.slint`: visual shells
  (`ConsoleMeter`, `WorkbenchMeter`…). They draw what the base tells
  them.

```slint
// ui/slots/meter.slint (generated from SLOTS; abridged)
export component Meter inherits MeterBehaviour {
    // rms, peak, hold, muted, show-scale, label, accessible-*: MeterBehaviour
    if Look.meter == MeterPiece.console: ConsoleMeter {
        rms: root.rms; peak: root.peak; hold: root.hold;
        muted: root.muted; show-scale: root.show-scale; label: root.label;
    }
    if Look.meter == MeterPiece.workbench: WorkbenchMeter { /* the same bindings */ }
    if Look.meter == MeterPiece.daylight: DaylightMeter { /* … */ }
    if Look.meter == MeterPiece.blender: BlenderMeter { /* … */ }
}
```

- `Look` (`ui/look.slint`, generated) has one enum property per slot
  (`MeterPiece { unset, console, workbench, daylight, blender }`). Its
  default is `unset`, which matches no branch. That avoids a trap the
  Slint study found: `AppWindow::new()` builds every `if` branch that is
  true at that moment, before Rust can set anything, and runs its `init`.
  With `unset` nothing is built until Rust sets the slots, which it
  always does before `show()` (it checks no slot is left `unset`).
- **A branch tests `Look` and nothing else.** A condition on data (a
  range, `empty`, a state) would destroy the piece and build another
  whenever that value changed, ending a drag, an edit or the focus (the
  notes record a rebuilt mixer strip ending its fader drag, 2026-10-12).
  So a piece handles all of its states itself: Workbench's number field
  fills only when its range is bounded, inside the one piece. The
  generator writes only `Look` conditions; the build check rejects a
  dispatcher edited by hand.
- A condition on a runtime value keeps every piece in the binary; only
  the chosen one is ever instantiated, so unused pieces cost disk, not
  memory or CPU.
- **The interface can't drift.** The dispatcher is generated from
  `SLOTS`, so every branch binds every property (a hand-written branch
  that forgot one would compile and silently use the default); a piece
  that lacks a property is a compile error; a misspelt piece is a compile
  error (an enum, not a string).
- **Cargo features** `theme-workbench`, `theme-daylight` and
  `theme-blender` choose which pieces are generated and compiled.
  Console's are always in. All three are default features, so `cargo
  build`, clippy and the tests cover every theme; the dev loop can build
  Console alone with `--no-default-features`. A theme whose pieces aren't
  in the build falls back like a piece the binary lacks (7.5).

Interface rules, from what Slint 1.18 allows (spike and Slint study):

- Values go in as `in` properties. Two-way values use `<=>` from the
  branch to the dispatcher's property.
- An element inside an `if` can't be read from outside. So a piece's
  outputs come back through callbacks: an edited text, a mixer strip's
  `height-changed(length)` that sets its `out` height.
- **Focus.** The dispatcher inherits the base's FocusScope, so
  `forward-focus`, `has-focus` and `.focus()` work on it as on today's
  kit components; the pieces sit inside it as children (an `if` child
  inside a base's `@children` is allowed; only the placeholder can't be
  in an `if`). A piece that holds its own TextInput gets focus through
  the base's `focus-pending` counter, which `.focus()` bumps: the piece
  reads it in its own `init` (so a piece built after the request still
  takes it; a `changed` handler wouldn't fire, because Slint's
  `ChangeTracker::init` reads the first value without notifying) and in
  `changed focus-pending` for later requests.
- No `@children` in a slot. A slot is a leaf. Where a kit component takes
  children today (PanelHeader's header buttons, DockToolbar's buttons,
  ListRow's trailing toggles), they become data: a `[DockAction]` model
  per dock, and rows that know their own toggles. Each model is one
  VecModel changed row by row, never replaced (a replaced model rebuilds
  its rows and ends a drag; notes, 2026-10-12).

### 2.4 Containers: shared, shaped by layout tokens

Slint 1.18 rejects `@children` inside `if` or `for` ("cannot appear in a
conditional element"), and named slots need
`SLINT_ENABLE_EXPERIMENTAL_FEATURES`. So these stay one component each,
and read layout tokens:

| Container | Layout tokens (Console's value first) |
|---|---|
| Frame (`app.slint` Workspace) | `frame-gutter` 0 (Blender 2px, also around the window's edge); `frame-seam` colour; splitters stay 4px grab zones over the gutter |
| Dock | `dock-radius` 0 (Blender 6); `dock-gap` 0 (Workbench 1, Blender 2); `dock-highlight` none (Blender `#3c3c3c` top line); `dock-focus-ring` none (Workbench accent 35% inset ring); `dock-footer-side` false (Blender: the footer column sits right of the list) |
| Dialog | `dialog-placement` top (Console's sheet, 44px down) or centre (Workbench, Daylight, Blender); `dialog-scrim` colour (transparent for Blender); `dialog-radius`; `dialog-shadow-*`; `sidebar-dialog` full-height (Console, Daylight) or under-header (Workbench, Blender) |
| Section body | `section-bg`; `section-radius`; `section-row-gap` (0, 1px gaps, hairline); `section-hairline-inset` (Daylight 12); `section-collapsible` |
| Form row | `label-col`; `row-label-right` (Workbench and Blender true); `row-value-end` (Console and Daylight true); `row-height`; `row-band` colour (Console's inspector bands); `row-well` colour (Workbench's dark well behind the value column); `row-value-chrome` boxed or bare (Daylight: values as bare text with ⇕ or Browse); `row-reset-lane` 0 (Workbench 12px); `row-decorator-lane` 0 (Blender 20px); `row-hint-icon` false (Blender's ⓘ before a hint); `row-two-line` (Daylight's description under the label) |
| List | `list-stripe` none (Blender `#2b2b2b` stripes to the bottom); `list-well` false (Blender's scene list sits in a well); `row-radius`; `row-inset` |
| Picture frame (Stage, Monitor, SourcePreview) | `mat-pad` 0 (Daylight 12); `mat-colour`; `mat-radius`; `mat-shadow-*`; `head-height`; `caption-height`; `caption-gap` |
| Canvas box (native layers, `layers.rs`) | `handle-size`; `handle-radius`; `handle-fill`; `handle-ring`; `handle-ring-width`; `box-ring`; `box-outer-ring` |

- **Settings and other sidebar dialogs.** With `sidebar-dialog =
  full-height` the sidebar (the `nav-list` slot) runs the dialog's full
  height, and the page header and footer sit in the content column only;
  the dialog draws no `dialog-header`, the page is titled by
  `page-header`, and the nav list's own title names the window (Daylight's
  "Settings"). With `under-header` a full-width `dialog-header` (Workbench's
  "Settings" band, Blender's title bar) sits on top and the footer runs
  under the sidebar too. The Dialog places header, sidebar, content and
  footer by binding their x, y, width and height to the token: placement
  maths, not a second tree, so `@children` stays where it is.
- **`dock-footer-side`** changes the Dock's direction around its
  `@children`. The body and footer sit in a `FlexboxLayout` whose
  `flex-direction` is bound to the token (column, or row for Blender).
  FlexboxLayout is stable in 1.18.1 (only ComponentContainer is held back
  as experimental), and Slint documents it for "a `flex-direction` that
  changes at runtime".
- **`section-collapsible`.** A folded section body is `visible: false`
  (Slint lowers it to a visibility clip). Tab skips it (Slint's
  `move_focus` only stops on visible items) and AccessKit marks it
  hidden; T.3c checks that VoiceOver skips it too.
- **The canvas box.** `layers.rs` draws the selection box and its eight
  handles as Core Animation layers; today they are fixed (`HANDLE = 8.0`,
  accent fill, a 1px border in the monitor colour, radius 1). The boards
  all differ: Console 8px round handles in bone with a 1px `#00000099`
  ring; Workbench 6px squares in bone with a 1px accent ring; Daylight
  7px white squares with an orange edge; Blender small orange squares
  with a dark ring. The canvas tokens go to `Layers` with its colours
  (`Colours` becomes `CanvasLook`). Cropping and locked keep today's rules
  in the theme's colours.
- Picture tokens feed Stage's maths, so `picture-left/-top/-width/-height`
  (which `layers.rs` reads to place the native picture) stay exact.
  Daylight's mat is drawn around the transparent picture hole.
- Each container's parts that aren't containers are slots: the dock's
  header and footer, the dialog's header and footer, the section's
  header, the rows.

### 2.5 The frame

`app.slint`'s Workspace is a fixed column today. It becomes a sequence of
frame slots, each with a `none` piece for themes without it:

```text
top-bar        Blender's top bar (menus, workspace tabs, selectors)
viewer-line    Console's line at the window's top
stage          shared; monitor-head, monitor-caption, monitor-overlay inside
source-bar     the source toolbar row (Console: the control bar)
docks          shared Dock containers
status-bar
```

These are leaves, so they swap like any slot. The app's View menu still
shows or hides the source toolbar and status bar in every theme.

**The arrangement is the app's, not the theme's.** A theme picks a piece
for each frame slot; it can't reorder them or move docks. ABI 1 names
slots and pieces, never positions. Workspaces (A's "full workspaces",
waiting) will rearrange the window later without a new theme ABI: the
Blender file's "Beyond OBS: Layout" page (Live and Review workspaces, a
scene shelf, an outliner tree, a timeline, filters as a modifier stack)
and the Console file's "Workspace · Live/Build/Create" and "Workspaces ·
Customize & flow" boards are workspaces. Choosing the Blender theme gives
Blender's look on OBS's layout, not the Beyond-OBS layout.

### 2.6 What the spike measured

All on base M2 test Macs, release builds, an isolated engine with three
tone sources so the meters move (about 29 redraws a second).

**Read these as "the mechanism is cheap", not as the budget's
baseline.** The spike's "main" and default theme are the look before
N.5: the old copper look with the system font and Menlo. Workbench
registered Geist, so part of its −21 MB footprint and +1.2 points may be
fonts rather than pieces. Idle (connected, nothing changing) wasn't
measured. And CPU readings on the test Macs drift several points between
sessions (notes, 2026-10-13), more than some differences below. T.3a
measures again on Console, with an A/A pass for the noise (section 10).

CPU and memory (isocast-m2-a, four interleaved runs each, median; CPU in
% of one core):

| Build | CPU | Peak RSS | Footprint |
|---|---|---|---|
| main | 18.1% | 139 MB | 75 MB |
| compiled, default theme | 17.8% | 139 MB | 75 MB |
| compiled, Workbench's meter and number field | 19.0% | 112 MB | 54 MB |
| interpreter build, default theme | 18.3% | 140 MB | 76 MB |
| interpreter, the same two pieces as `.slint` files | 21.2% | 124 MB | 61 MB |
| whole UI interpreted (Slint live preview) | 24.2% | 357 MB | 298 MB |

Launch, spawn to the first frame with live meters (isocast-m2-b, median
of 5 warm and 3 cold runs):

| Build | Warm | Cold (after `purge`) |
|---|---|---|
| compiled: default, Workbench, Daylight | 186, 178, 177 ms | 4.4–5.5 s |
| interpreter build: default, Workbench | 187, 177 ms | 5.5–5.9 s |
| interpreter, Workbench pieces from a folder | 181 ms | 5.3 s |
| whole UI interpreted | 631 ms | 5.8 s |

The theme step takes about 6 ms. Cold launches are mostly paging in the
binary and system frameworks (2.3–3.2 s before the window exists).

Switching (isocast-m2-a), from `isocast theme X` to the relaunched
window's first frame with meters: a median of 483–587 ms per theme over
24 relaunches while recording and streaming (range 247–595 ms), 457–575
ms idle. Of that, exec to backend ready is 60–140 ms, the theme 6–16 ms,
window shown to first frame 130–400 ms.

Binary (stripped) and release build of `isocast-ui` (isocast-m2-b, clean /
after touching one `.slint` file):

| Build | Size | Clean | After a `.slint` edit |
|---|---|---|---|
| main | 45.2 MB | 260 s | 180 s |
| compiled pieces | 47.3 MB (+2.1) | 267 s | 206 s |
| with slint-interpreter | 55.3 MB (+8.0 more) | 284 s | 192 s |
| live preview (dev loop only) | 35.5 MB | 156 s | 23 s |

From the Slint study (a scratch crate, software renderer): `.slint` code
costs about 1.6–1.8 MB of binary per 1,000 lines; a second full copy of
Isocast's 11,964-line UI added 18.9 MB and took the release build from
2m37s to 3m51s; switching all seven conditional docks off and on cost
0.6–0.75 ms.

### 2.7 Mechanisms not chosen, and the weak spots

| Mechanism | Verdict |
|---|---|
| Compiled pieces picked at launch | **Chosen.** No measurable CPU; +2.1 MB for two pieces and fonts. Dispatchers generated from `SLOTS`. |
| `.slint` pieces loaded at launch (slint-interpreter, ComponentContainer) | Works in 1.18.1, but `ComponentContainer` and `component-factory` need `SLINT_ENABLE_EXPERIMENTAL_FEATURES`, `slint::ComponentFactory` is deprecated ("made public by mistake"), `create_embedded` needs the interpreter's `internal` feature, each piece has its own globals that Rust must fill and relay, and nothing is sandboxed (section 7.7). +8 MB, +2.2 points of CPU for three meters and a field. **Later** (T.8), for plugin code pieces, once Slint stabilises it (slint-ui/slint#2390, #5756). |
| Whole UI through the interpreter | **Rejected.** Live preview bakes absolute paths in at build time, so a theme can't override the kit; Slint says not to ship it. Using the interpreter directly means rewriting about 660 typed calls. +27% CPU, +245 MB, +450 ms per launch. |
| One build of the UI per theme (`with_library_paths` per theme) | **Rejected.** N copies of every generated type for the Rust glue, about +19 MB each. |
| Swapping themes live, without a restart | **Not needed** (Ryan: "We don't need hot reloading"). It would need state carried across swaps, reactive tokens, and re-measuring text after a font change. |

Where the chosen mechanism is weak, and what we do about it:

- **Binary size and build time grow with every piece, probably past the
  budget.** On `ui/console-main` the Console code that maps to slots is
  about 3.5–5k lines (fields 693, onboarding 757, dialog 479, statusbar
  384, lcd 249, control-bar 220…). If each further direction is
  similar, the study's 1.6–1.8 MB per 1,000 lines gives 5–9 MB per
  theme against a 5 MB budget, and the study's extra 11,964 lines made
  the release build 47% slower against a 30% budget. Pieces only draw
  (behaviour bases, icons and kit primitives are shared), which should
  keep them smaller, but that is unmeasured. The cargo features of 2.3
  keep the dev loop at Console's cost; T.4a measures the real cost per
  piece, and if the projection for three themes passes the budget, Ryan
  decides (section 15) rather than an agent quietly accepting it.
- **Runtime fonts** rest on `unstable-fontique-011`. If a Slint upgrade
  drops it, built-in fonts go back to `.slint` `import`s (stable, all
  registered at window creation) and plugin fonts stop working until a
  replacement exists. The upgrade checklist names this.
- **The relaunch** uses `exec` of `current_exe()` with the same arguments
  (same pid). exec runs no destructors and erases the process's memory
  (the spike's thread-local loop guard with it), and the new image
  activates like a fresh launch; that is why 5.1 drains edits first and
  passes the theme, the loop guard and activation through the
  environment. If a
  macOS version breaks exec inside the bundle, fall back to `open -n -g`
  of the bundle and quit (a new pid; slower).
- **Design drift between Paper and the app.** The board check (section
  13) uses N.5's pairing scripts, which aren't in the repository yet;
  committing them needs Ryan's approval (section 15).

## 3. The slot table

ABI 1 has 35 slots. "Replaces" is the Console kit component (N.5) the
dispatcher takes the name of, or "new". Pieces named after a theme are
that direction's drawing; a theme may use another's piece (Daylight's
switch is Console's switch in Daylight's colours). `none` draws nothing.

| Slot | Replaces | console | workbench | daylight | blender |
|---|---|---|---|---|---|
| `number-field` | NumberField | console | workbench | daylight | blender |
| `slider` | Slider | console | workbench | daylight | blender |
| `toggle` | Switch | console | console | console | blender |
| `segmented` | Segmented | console | workbench | console | blender |
| `quality` | QualitySlider | console | workbench | daylight | workbench |
| `template-field` | new | console | console | daylight | console |
| `meter` | Meter | console | workbench | daylight | blender |
| `tip` | Tip | console | console | console | blender |
| `menu-panel` | MenuPanel, MenuView | console | console | console | blender |
| `empty-state` | EmptyState, ListEmpty | console | workbench | daylight | blender |
| `dock-header` | PanelHeader | console | workbench | daylight | blender |
| `dock-footer` | DockToolbar | console | none | daylight | blender |
| `scene-row` | ListRow (Scenes) | console | workbench | daylight | blender |
| `source-row` | ListRow (Sources) | console | workbench | daylight | blender |
| `mixer-strip` | MixerStrip | console | workbench | daylight | blender |
| `mixer-extras` | new | none | workbench | daylight | blender |
| `top-bar` | new | none | none | none | blender |
| `viewer-line` | Console's viewer line | console | none | none | none |
| `monitor-head` | MonitorHead | console | workbench | daylight | blender |
| `monitor-caption` | new | none | none | daylight | none |
| `monitor-overlay` | new | none | none | none | blender |
| `source-bar` | SourceToolbar / control bar | console | workbench | daylight | blender |
| `lcd` | Lcd (N.5's `widgets/lcd.slint`) | console | none | none | none |
| `status-bar` | StatusBar | console | workbench | daylight | blender |
| `details` | RecordingDetails, StreamDetails | console | console | console | blender |
| `notices` | ToastStack | console | console | console | blender |
| `studio-column` | StudioColumn | console | workbench | daylight | blender |
| `controls` | the Controls dock's body | console | workbench | daylight | blender |
| `dialog-header` | SheetHeader | console | workbench | daylight | blender |
| `dialog-footer` | DialogFooter | console | workbench | daylight | blender |
| `section-header` | GroupHeading, FormSection's title | console | workbench | daylight | blender |
| `nav-list` | NavList | console | workbench | daylight | blender |
| `page-header` | new | console | none | daylight | none |
| `welcome` | Welcome | console | workbench | daylight | blender |
| `properties-view` | Properties, Filters, Edit Transform | sheets | sheets | sheets | tabbed |

Each slot below: its interface (in / out / callbacks; **new** marks what
today's kit lacks), then each direction's piece. Sizes and colours are
the boards'; the slices read exact values from Paper. The actions each
slot must make reachable, and the extras a piece may add, are in
section 9.

### 3.1 Controls

**`number-field`**
- In: value, minimum, maximum, step, prefix (scrub label), unit, label,
  placeholder, enabled, invalid, empty, small, submits, stepper; **new:**
  label-inside, locked (live-locked). Out: has-focus (the base's).
  Callbacks: scrubbing(float), changed(float).
- Each piece handles bounded and unbounded values itself (only bounded
  fields fill), so the piece never changes when a range or `empty`
  changes. Scrubbing applies live (pipelined, as the canvas's drags
  are), in every theme. That is one behaviour change for Console (the
  Transitions dock's Duration applies while you drag, as the spike did),
  made and checked in T.3a.
- console: 22px stepper field, Martian Mono value, a 16px ▲▼ column.
- workbench: 24px field that fills with accent from the left as you drag
  anywhere; ‹ › on hover step by one; unbounded numbers keep the scrub
  prefix and dim unit ("W 1920 px").
- daylight: a plain 56×20 mono field with the unit inside (paired with a
  slider where the range is bounded, see `slider`).
- blender: 20px control-grey field, label inside on the left, value on
  the right, blue fill; green fill when locked while live; editing turns
  it into a dark inset field.

**`slider`**
- In: value, minimum, maximum, step, origin, enabled, label, value-text,
  tooltip. Callbacks: moved(float), changed(float), double-clicked().
- console: 4px track with onair or accent fill, 12px thumb with a shadow.
- workbench: 2px track, 10px bone thumb with a 1px black ring.
- daylight: neutral 2px track, bordered thumb, and a 56×20 value field.
- blender: no track and thumb; drawn as the number field's fill bar.

**`toggle`**
- In: on, enabled, label, tooltip. Callback: toggled(bool).
- console (also workbench, daylight): 30×18 switch in the accent.
- blender: a checkbox (Blender has no switches). The role stays a switch.

**`segmented`**
- In: options [Segment], value, enabled, label, small. Callback:
  selected(string).
- console (also daylight): a capsule with an inset pill.
- workbench: a joined field with the chosen segment in accent fill.
- blender: joined enum buttons, the chosen one blue.

**`quality`** (Settings › Output's quality)
- In: labels, stop, at, enabled; **new:** estimate-text ("≈ 9.4 GB /
  hour", T.7a). Callback: chosen(int).
- console: capsule segmented plus "Quality value 60".
- workbench (also blender): joined segmented plus "Custom 60".
- daylight: a 344px rail with stop dots, labels under each stop, the
  estimate under the row's label.

**`template-field`** (new: the ISO file-name template, T.7e)
- In: segments [TemplateSegment{kind (token or text), value, label}],
  available tokens [TemplateToken{id, label}], sample (the file name the
  current setup would write), field path, enabled, invalid, error.
  Callbacks: add(token-id, index), remove(index), move(from, to),
  edited(text) (the plain-text form, for keyboards and pastes).
- console (also workbench, blender): a mono text field holding the
  template, and the sample on the line under it.
- daylight: the tokens as pills with the separators between them and a
  `+` that lists the available tokens, then a "Sample" row with the file
  name in mono ("Main - Camera - 2026-10-12 - 001.mov").
- The base keeps the plain-text form and the pills in step and validates
  through the same setup check as the CLI, so an invalid template shows
  the CLI's message.

**`meter`** (a mixer strip's level)
- In: rms, peak, hold (dB), muted, show-scale, label; **new:** channels
  [Level] (stereo, T.7b; one bar repeated until then).
- console: a 3px line under the fader.
- workbench: 6px black well, lit segments 2px on and 1px off (a tiled
  image, as the spike's `seg-gap.svg`, not one element per segment),
  green to −18, amber to −6, red above, a 1px bone hold line.
- daylight: a 4px bar indented 26px, onair fill, a 1px hold line.
- blender: two 4px channel bars in an 11px well; unlit zones faintly
  tinted; a 1px hold line 9px tall.

**`tip`**
- In: text; **new:** title, shortcut, cli (from the action table, T.7f).
- console (also workbench, daylight): one text box in the theme's
  tokens.
- blender: 300px wide; title, description, "Shortcut: …", and the CLI
  line in mono ("CLI: isocast start"), taken from the action table so it
  is always a real command.

**`menu-panel`**
- In: the menu's MenuList and entries; **new:** searchable. Callbacks as
  MenuHost's.
- console (also workbench, daylight): today's list with checks.
- blender: checkbox marks, headings, shortcuts on the right, a search
  field when searchable (T.7f adds type-to-filter for every theme; only
  the field is Blender's).

**`empty-state`**
- In: text; **new:** hint, glyph.
- console: a 40px outlined glyph and one line, centred.
- workbench: a caption.
- daylight: a title over a hint ("Click + below to add one.").
- blender: text inside the list's well.

### 3.2 Docks and lists

**`dock-header`**
- In: dock (id), title, glyph, count-text, actions [DockAction{id, icon,
  label, enabled, on, menu-open, group}], focused, menu-open. Callbacks:
  action(id, x, y, w, h), dock-menu(x, y).
- console: the title, an optional gear.
- workbench: 16px dock glyph and ⌄ (the dock's menu), title, 22px action
  buttons (+ and ⋯), and the focused dock's inset accent ring.
- daylight: the title with "· 4" or "· 4 live" in text-3, + and ⋯.
- blender: an editor-type dropdown (glyph and chevron; opens the dock's
  menu), header menus as text items, joined icon toggle groups and
  popover chevrons.

**`dock-footer`**
- In: dock, actions [DockAction], vertical. Callback: action(id, x, y, w,
  h).
- console: capsule clusters at both ends (Tower).
- workbench: none (its actions are in the header and the ⋯ menu).
- daylight: 24px icon buttons with 1px separators.
- blender: for Scenes, a 20px side column (+ − ⌄ ^ ⌄) beside the list
  (`dock-footer-side`); nothing for other docks.

Today each dock passes its buttons as children of DockToolbar and
PanelHeader. T.3b turns them into `[DockAction]` models in each area
(`SourcesArea.actions` and so on) and routes `action(id)` to the
existing handlers. Each header and footer piece picks which actions it
shows as buttons by `group`; the dock's menu (⋯, Blender's editor-type
menu) lists every action neither piece shows, computed in the base, so a
theme can't drop one (section 9).

**`scene-row`**
- In: scene (SceneRow, **new** item count), index, count, selected,
  focus-shown, studio, on-air, live, dragging. Callbacks: clicked,
  double-clicked, context-menu, drag-moved, drag-ended. Renaming stays a
  TextField the dock lays over the row.
- console: 30px pill row, yellow-tinted selection, a red PGM or LIVE
  capsule, and in Studio Mode a green PVW capsule (the tally, #95).
- workbench: 26px row, a 3px accent bar, a 6px onair dot.
- daylight: the program dot on the icon, the item count on the right.
- blender: a UI-list row in a well, a rounded blue band, an 8px green dot.

**`source-row`**
- In: item (ItemRow), index, count, selected, focus-shown, dragging,
  renaming. Callbacks: select, open-properties, toggle-visible,
  toggle-locked, toggle-iso, toggle-group, context-menu, drag-moved,
  drag-ended, **new** hover-changed(bool) (row hover → canvas outline).
- console: pill row, an ISO capsule, eye and lock always shown.
- workbench: a 16px status lane (ISO as a 6px red ring), eye and lock on
  hover or when not default; hidden items dimmed.
- daylight: a 6px state dot on the kind icon, eye and lock.
- blender: an outliner row: disclosure, kind icon in its kind colour, the
  active source's name in orange, and eye, lock and camera (ISO) columns
  always shown.

**`mixer-strip`**
- In: strip (Strip, **new** icon, collapsed), level (Level or stereo),
  first. Callbacks: gain-moved, gain-changed, gain-reset, toggle-mute,
  **new** toggle-monitor, toggle-collapsed, menu(x, y),
  height-changed(length). The dock reads the height to place menus and
  the scroll gutter (it replaces the fixed `StripSize` global).
- console: a 44px Logic-style track header (name, M, headphones, green
  fader with the 3px meter under it, value).
- workbench: about 76px: head with a 52×20 value box, segmented meter,
  scale, fader.
- daylight: about 71px: kind icon, level bar, fader and value field; the
  scale moves to the dock's top (`mixer-extras`).
- blender: a collapsible panel: header with the value, stereo meter,
  scale, a Volume number field with a joined mute button and the strip's
  ⌄ menu (Monitor is in that menu). Collapsed it is a 24px header.

**`mixer-extras`**
- In: reads MixerArea; **new:** loudness (T.7b).
- workbench: "S −14.2 I −16.0 LUFS" in the dock header.
- daylight: the one shared dB scale row at the top of the mixer.
- blender: the "View" header menu.

### 3.3 Frame and bars

**`top-bar`** (Blender only)
- In: the menu bar's model (T.6 gives it one source, see section 12),
  studio, the scene collection in use and the list, canvas size and fps.
  Callbacks: menu(id), set-studio(bool), switch-collection(id),
  open-video-settings.
- blender: logo, in-window menus, "Layout" and "Studio" workspace tabs
  (Studio Mode off and on), the collection selector, the canvas selector.

**`viewer-line`** (Console only)
- In: canvas size and fps, scene name, zoom text. Callbacks: zoom-menu,
  view-menu.
- console: 28px at the window's top: format left, scene centred, "Fit
  64% ⌄" and "View ⌄" right.

**`monitor-head`**
- In: tag (PREVIEW, PROGRAM or none), tag tone, name, facts, zoom-text,
  live facts. Callbacks: zoom-menu, view-menu.
- console: Studio Mode only, PREVIEW (green) and PROGRAM (red) tags.
- workbench: always: "Main 1920×1080 30 fps … Fit 50% ⌄" in mono.
- daylight: Studio Mode tags (facts go under the picture).
- blender: an area header: editor type, Preview/View/Select/Source/Canvas
  menus, snapping and overlay toggles, Fit.

**`monitor-caption`** (Daylight only)
- In: canvas facts, the selection's name and box, media facts (T.7c),
  live facts. Daylight: mono 11px under the mat, canvas facts left, the
  selection right in accent text.

**`monitor-overlay`** (Blender only)
- In: title lines; **new:** zoom and pan actions (T.7f). Blender: "Program
  Preview / Main | Intro Clip" top left, and a 28×88 gizmo column (zoom,
  pan, frame all). Until zoom and pan exist the gizmo offers fit and 1:1.

**`source-bar`**
- In: reads `Ui.selected-*` and SourcesArea; **new:** facts [Chip{icon,
  text, mono}], filter-count, kind name. Callbacks: properties, filters,
  rename, quick-*, chip(id), and the transport and pane toggles for
  Console's control bar.
- console: the control bar: selected source and facts, the `lcd` slot,
  transport buttons, master level capsule, toolbar capsule, pane
  toggles.
- workbench: name plus property chips (resolution, fps, codec, file,
  ISO), quiet Properties and Filters.
- daylight: name and kind, chips, Properties, "Filters 2".
- blender: a header-only area: kind icon, an editable name field,
  Properties and Filters buttons, kind and file, "Options ⌄".
- Every source-bar and status-bar piece has a place for the `lcd` slot;
  the layout token `lcd-place` (source-bar or status-bar) says which one
  draws it. So a plugin can put Console's LCD into Workbench's source
  bar.

**`lcd`** (new as a slot; N.5 draws it inside the control bar)
- In: rec-time, live-time, recording, live, fps, target fps, dropped,
  cpu, size (bar or hero). Callbacks: none (it is a readout; the REC and
  LIVE details open from the controls and the status bar).
- console: Logic's LCD: REC or LIVE time, FPS, DROPPED, CPU in Martian
  Mono with unused leading zeros dimmed (separate Text runs); the
  dropped cell turns amber when frames drop. Console's control bar and
  Welcome hero use it.
- workbench, daylight, blender: none (their readouts are in the status
  bar).

**`status-bar`**
- In: reads ControlsArea and Ui; **new:** free-text, iso-armed, sentence,
  reports (with `notices`).
- console: a state capsule (READY, REC, LIVE), a sentence, ruled mono
  readouts.
- workbench: REC, LIVE, CPU, fps, dropped (amber) … free space, Mb/s.
- daylight: REC, LIVE, "ISO 4 armed" … CPU, fps, dropped, free.
- blender: mouse hints, reports, "|" stats with the version.

**`details`** (REC and LIVE popovers)
- In: ControlsArea files and stream rows. Callbacks: marker, split; **new:**
  stop, show-in-finder.
- console (also workbench, daylight): today's popovers in the theme's
  tokens.
- blender: files with dots and times, "Free on disk 212 GB · about 9 h"
  (T.7a), joined [Show in Finder | Stop Recording].

**`notices`**
- In: the Toasts global.
- console (also workbench, daylight): the toast stack, top right.
- blender: info and success go to the status bar as reports. Warnings and
  errors (dropped frames, disk full, a failed file) show as toasts too:
  no theme may hide a recording problem.
- In every theme the base announces each notice with
  `accessible-live-region` (polite for info and success, assertive for
  warnings and errors), whether a piece draws it as a toast or a report.

**`studio-column`**
- In: can-take, **new** quick-transitions [QuickTransition{label,
  duration-text, current}] (T.7f). Callbacks: take(int),
  open-transition-properties.
- console: a yellow Transition button, a grouped quick list, a round gear.
- workbench: 24px rows with mono durations.
- daylight: an orange Transition button, a "QUICK" group with ✓, "Edit
  transitions".
- blender: its own area with a header, a joined list, "Transition
  Properties".

**`controls`**
- In: ControlsArea record-state, stream-state, busy-*, times. Callbacks:
  toggle-stream, toggle-record, marker, split, set-studio, open-settings,
  quit.
- console: N.5's buttons (yellow on, red stop).
- workbench: 30px buttons with leading icons, the timer inside Stop.
- daylight: two groups 16px apart.
- blender: joined toggles; Settings | Exit side by side.

### 3.4 Dialogs, forms and views

**`dialog-header`**
- In: title, subtitle, icon, icon-accent. Out (through a callback): drag
  offset. Callback: close.
- console: an icon tile, a bold title, one line of explanation.
- workbench: a 30px band, title centred.
- daylight: an icon tile, an Instrument Serif title, a subtitle.
- blender: a 28px window title bar with the title centred and three
  window-control dots at the left, as the Properties and Preferences
  boards draw them: grey while idle, the first turns red with × on hover
  and closes the dialog (Escape does the same); the other two stay
  inactive, because these are in-window dialogs, not OS windows (Ryan may
  prefer real windows, section 15).

**`dialog-footer`**
- In: ok, cancel, apply and extra text and enabled, note; **new:**
  note-tone (a dot), summary, menu. Callbacks: ok, cancel, apply, extra,
  menu.
- console: summary line, buttons bottom right inside the body.
- workbench: 48px band with "• 1 unsaved change".
- daylight: 56px band with a note on the left.
- blender: a 34px strip with the ≡ menu.

**`section-header`**
- In: title, open, enable-state (none, on, off), changed, active,
  removable, value-text. Callbacks: toggle-open, toggle-enabled, reset,
  remove, grip-drag.
- console: a 32px band, the title and a reset arrow.
- workbench: a 28px band: chevron, enable box, title, reset, grip.
- daylight: a small-caps caption above the group.
- blender: a 24px Blender panel header: chevron, checkbox, title, value,
  grip, × on the active one.

**`nav-list`** (Settings' sidebar)
- In: tabs [NavTab{id, label, icon, **new** group}], current, title.
  Callback: selected(id).
- console: headed groups (Recording, Broadcast, System), rounded rows.
- workbench: grouped blocks on 1px seams, a 3px accent bar.
- daylight: a "Settings" title, small-caps groups, a white selected row
  with an orange bar.
- blender: grouped blocks, the selected tab a full blue band
  (`#4772B3`), no bar.

**`page-header`** (a Settings page's top, with `sidebar-dialog =
full-height`)
- In: title, icon, description, meta (T.7a's free space).
- console: icon tile, title, one line.
- daylight: an Instrument Serif title with mono meta at the right ("412
  GB free on Scratch").

**`welcome`**
- In: SettingsArea start-with-this-mac, import-obs, open-setup-file,
  welcome-busy; **new:** dismiss, open-link(id), version.
- console: an LCD hero (the `lcd` slot), title, capsule buttons with
  hints.
- workbench: two-line rows with key hints.
- daylight: a serif title, an inset group of rows, a docs link, "esc Skip
  for now".
- blender: a splash: artwork band (a bitmap from Paper, since Slint can't
  blur), Start and Getting Started columns; a click outside dismisses.
- Escape dismisses Welcome in every theme (the base handles it), whether
  or not the piece shows "Skip".

**`properties-view`**
- sheets: Properties, Filters and Edit Transform as separate dialogs
  (today's views in the theme's dialog pieces).
- tabbed: Blender's Properties editor: a source preview area and a
  properties area with a 28px icon tab column (Source, Transform, │,
  Filters, Audio, ISO/Video), Defaults, Cancel and OK. The same menu
  commands open it on the matching tab. Both read the same SourcesArea
  data.
- The tabbed view is modal like the sheets and keeps their rules: each
  tab applies and previews exactly as its sheet does, and Cancel reverts,
  and OK keeps, every tab that was touched. T.6 checks that the same
  edits made through the sheets and through the tabs leave the same
  setup.

### 3.5 Tokens only, no slot

- Accents and status colours, dark text on yellow and orange, Daylight's
  light palette, Workbench's dark seams: the 97 tokens.
- Control and row heights, header heights, radii, pill radii, dot sizes.
- Button variants (primary, danger, Studio Mode on), the program ring.
- **Kit widgets that differ only in small ways** stay one component each
  with tokens, not slots: Dropdown, TextField, Checkbox, Button,
  IconButton, Hint and Callout. The boards differ in:
  - `dropdown-glyph`: Console's macOS popup ⇕ ("Cut ⇕", "H.264 Apple
    VideoToolbox ⇕"), Daylight's ⇕, Workbench's and Blender's ⌄;
  - `field-chrome`: the field's well and border (Workbench's dark well,
    Blender's `#282828` field), with `row-value-chrome = bare` for
    Daylight's settings rows, where the encoder and the path are bare
    right-aligned text (the boxed "Cut ⇕" in Transitions stays boxed);
  - `checkbox-radius`: Console's checkboxes are round, the others square.
  The Components-page board check covers each.
- **New tokens** with Console's look as their default: `font-display`
  (big titles: Hubot Sans, Geist, Instrument Serif, Inter Tight),
  `icon-stroke` (Workbench 1.5px), `kind-*` colours (Blender colours
  icons by source kind), `active-name` (Blender's #FFAF29),
  `tooltip-border`, `tab-separator`, `lcd-place`, the canvas tokens and
  the layout tokens of 2.4.
- **Icons** stay one Lucide set looked up by name (`Icons.named`).
  Workbench draws them at 1.5px; Blender colours them by kind. Glyphs a
  board needs that the set lacks (mouse hints, editor types, the gizmo)
  are added to the one set, drawn by us or taken from Lucide (ISC).

## 4. Data the pieces need, and when a theme is ready

### 4.1 Data

Some pieces show things the engine or the app doesn't have yet. Under the
one-method-table rule these become methods or fields first (slice T.7).
Until then a piece leaves that part out; it never invents a value. Some
of these Console needs too; whatever N.5.1 adds is reused.

| Need | Used by | Where it goes |
|---|---|---|
| Free space and volume name while idle, time left at the current rate | Workbench and Daylight status bars, Console's sheet summary and status sentence, Daylight's page meta, Blender's REC popover | T.7a: `status.disk` always, not only while recording |
| GB per hour estimate | Console's sheet summary, Daylight's quality stops | T.7a: from the setup's outputs, in `status` and `setup check` |
| Per-channel (stereo) levels, the program mix's level | Blender's meter, Console's master capsule | T.7b: `levels` events carry channels and the program |
| Loudness: short-term, integrated, true peak | Workbench's mixer header | T.7b: EBU R128 on the program, per source on request |
| Media position, duration, codec | Workbench, Daylight and Console Properties, Daylight's caption, the codec chip | T.7c: `SourceStatus` (with G.6's play/pause/seek) |
| Sections and titles in schema forms, per-field default and reset, fields locked while live | Workbench's panels and reset arrows, Console's and Daylight's Properties sections, Console's reset arrows, Blender's panels, decorator dots and locked fields | T.7d: in `setup schema` and the form data |
| ISO file-name template and a sample name | Daylight's token pills, every theme's `template-field` | T.7e: a setup field, checked with field paths, and a pure function in isocast-protocol for the sample |
| Canvas fps in `Ui`, quick transitions with duration and "current", item count per scene, Settings tab groups and descriptions, preview zoom % and pan, folded sections and strips, list row counts, the focused dock, Welcome dismiss and links, menu type-to-filter, an action table (action id → name, shortcut and CLI command) | many; the action table is Blender's tooltip line and section 9's action set | T.7f: app-side or derived; per-viewer things go in `isocast-ui.json`; the action table comes from G.5's action ids and is generated from the CLI so it stays true |

### 4.2 The signature pieces, theme by theme

These are the pieces Ryan named in his pitch for each direction (plus
the two the boards repeat everywhere: the selection box and the
Settings arrangement). A theme is **ready** when every row draws: only
then does it appear in the picker's main list, get a "Ready for Ryan"
entry, and lose its `preview` status. Rows marked "T.8" wait by design
and are listed in section 15 so Ryan sees them; they don't hold back
"ready".

**Workbench** (Blender × Linear × DaVinci Resolve 20)

| Piece | Drawn by | Waits for |
|---|---|---|
| Graphite panels on a darker frame, 1px near-black seams | tokens; `dock-gap`, `frame-seam` | — |
| Geist and Geist Mono | fonts | — |
| Every control 24px; one right-aligned label column | tokens; `row-label-right`, `label-col` 148px | — |
| Number fields that fill as you drag | `number-field` | — |
| Folding panels (Properties' Media and Playback; Settings' sections) | `section-header`, `section-collapsible` | T.7d |
| Settings rows in dark wells with a reset arrow | `row-well`, `row-reset-lane`, `section-header` | T.7d |
| One indigo accent; status as a tiny dot before a grey name | tokens; `scene-row`, `source-row` | — |
| Segmented green/amber/red meters | `meter` | — |
| LUFS readout in the mixer header | `mixer-extras` | T.7b |
| The focused dock's ring | `dock-header`, `dock-focus-ring` | T.7f (the focused dock) |
| Media facts in Properties, the codec chip | `source-bar`, Properties | T.7c |
| Centred dialogs; Settings with a full-width title band and footer | `dialog-placement`, `sidebar-dialog` | — |
| 6px square handles with an accent ring | canvas tokens | — |
| Icon-over-label dock tabs (Resolve) | — | T.8 (tabbed docks) |

Ready needs T.4a–d, T.7b, T.7c, T.7d and T.7f's focused dock.

**Console** (Final Cut Pro × Logic Pro × Tower) is the default and is
drawn by N.5; it is ready when N.5 is, except for two pieces that wait
here: the sheet summary "~4.3 GB/h · 212 GB free" (T.7a) and the
section reset arrows (T.7d). T.7a and T.7d come first in T.7 for that
reason.

**Daylight** (Capture One × OrbStack × Linear, light)

| Piece | Drawn by | Waits for |
|---|---|---|
| White and pale-grey chrome; light native menus and panels | tokens; `appearance = "light"` | — |
| Every picture on a dark mat | `mat-*` tokens (Stage, Monitor, SourcePreview) | — |
| One safety-orange accent; state dots | tokens; `source-row`, `scene-row` | — |
| Flat sections and grouped settings | `section-*`, `row-value-chrome = bare`, `section-header` | T.7d (Properties' Files and Playback sections) |
| Sliders with value fields | `slider` | — |
| Metadata under the picture | `monitor-caption` | T.7c |
| Naming templates as token pills with a live sample | `template-field` | T.7e |
| "≈ 9.4 GB / hour" under Quality; "412 GB free on Scratch" | `quality`, `page-header` | T.7a |
| Instrument Sans, Instrument Serif titles, Fragment Mono | fonts, `font-display` | — |
| Settings with a full-height sidebar titled "Settings" | `sidebar-dialog`, `nav-list` | — |
| 7px white handles with an orange edge | canvas tokens | — |
| Small-caps tool tabs, the active one tinted (Capture One) | — | T.8 (tabbed docks) |

Ready needs T.5a–d, T.7a, T.7c, T.7d and T.7e.

**Blender** (Isocast as a Blender 4.x editor)

| Piece | Drawn by | Waits for |
|---|---|---|
| Every region an area with rounded corners on a 2px near-black gutter | `frame-gutter`, `dock-radius`, `dock-highlight` | — |
| An editor-type dropdown in each header | `dock-header` | — |
| Collapsible panels with grips | `section-header`, `mixer-strip` | T.7d (form sections), T.7f (folded state kept) |
| Striped Sources outliner with eye, lock and camera (ISO) | `source-row`, `list-stripe` | — |
| Number fields that fill with blue | `number-field` | — |
| Blender's greys, one blue, orange for the active source, green and red for LIVE and REC | tokens, `active-name` | — |
| Stereo meters | `meter` | T.7b |
| Settings as Preferences (title bar, blue nav band, ⓘ hints, decorator dots) | `dialog-header`, `nav-list`, `sidebar-dialog`, `row-hint-icon`, `row-decorator-lane` | T.7d (the dot shows a value that isn't its default) |
| Properties as the Properties editor with icon tabs | `properties-view = tabbed` | T.7d (its Media, Transform, Visibility panels) |
| Welcome as the splash | `welcome` | — |
| Tooltips that end with "CLI: isocast …" | `tip` | T.7f's action table, which comes from G.5 |
| "Free on disk 212 GB · about 9 h" in the REC popover | `details` | T.7a |
| The top bar with in-window menus and workspace tabs | `top-bar` | the menu bar's one model (T.6c) |
| Inter Tight (as the boards; or Inter, section 15) | fonts | Ryan's call |
| Small orange handles with a dark ring | canvas tokens | — |

Ready needs T.6a–d, T.7a, T.7b, T.7d and T.7f's action table, so
Blender's ready step waits for G.5 (custom hotkeys, still in review).

## 5. Switching: the app relaunches

### 5.1 What happens

1. Something calls `theme_set` (the app's own picker, `isocast theme set`,
   MCP `set_theme`, a plugin). If the id is the current one and `reload`
   isn't asked for, nothing happens and the reply says so. Otherwise
   isocastd writes `app.toml` and sends `theme_changed {theme, previous,
   reload}`.
2. Each running app compares `theme` with the theme it launched with. If
   they differ, or `reload` is true, it relaunches, unless it is busy
   (5.4).
3. To relaunch it lets queued edits finish (5.4), saves `isocast-ui.json`
   (the layout, plus two new keys, below), sets three environment
   variables, and `exec`s `current_exe()` with its own arguments. Same
   pid, same bundle. The spike measured 247–595 ms from the command to
   the new window's first frame with meters.
   - `ISOCAST_RELAUNCH_THEME`: isocastd's choice. The new process uses it
     instead of reading `app.toml` again, so an app and a daemon that
     read different `app.toml` files (different environments) agree
     after one relaunch instead of looping.
   - `ISOCAST_RELAUNCH`: a count and a time, the loop guard. A thread-local
     guard (the spike's) is erased by exec. At most three relaunches in
     10 s; then the app stays as it is and shows "Isocast keeps being
     asked to restart (theme X here, Y in isocastd)".
   - `ISOCAST_RELAUNCH_ACTIVE`: whether the app was the active app.
4. The new process launches as in 2.1. **If it wasn't the active app, it
   doesn't become one:** it starts with activation prohibited (as
   background mode does: `background.rs` notes that winit's launch
   activation brings the app forward even when asked not to), shows its
   window at the saved frame, then becomes a regular app again (Dock
   icon, menu bar) without activating. A full-screen window is put back
   into full screen when Isocast is next activated, so macOS doesn't
   switch Spaces under Ryan. If it was active, it activates as normal.
5. It connects, reports what it shows (`theme_report`), and carries on.

Offscreen snapshots, `--bench`, `--soak` and `--drag-test` never relaunch:
they use the theme they started with.

### 5.2 What the relaunch restores

- **Everything a normal launch restores:** dock sizes, which docks and
  bars show, locks, Studio Mode, preview scaling, snapping, the mixer's
  hidden strips.
- **New: the window frame.** Position, size, full screen and zoomed, saved
  on quit and before a relaunch, restored at every launch. It is put back
  on the main display if its display is gone. (Today the window opens at
  the default place; a relaunch would jump.)
- **New: a one-shot `restore` entry.** The selected source (by its item
  path) and, when the switch came from Settings, the open Settings tab.
  It is used only within 10 s of being written, then deleted.
- **From isocastd, as always:** the scene on air, the preview scene, the
  setup, levels, recording and stream state. These were never the app's.

Not restored: undo history (as after a collection switch), scroll
positions, open menus and popovers, and dialogs other than Settings. The
picker says so: "Isocast restarts to show Workbench. Recording and
streaming carry on. Undo history is cleared."

### 5.3 Recording and streaming carry on

The app sends nothing to isocastd when it quits: `on_quit` only ends the
event loop (`crates/isocast-ui/src/app.rs`, `quit_event_loop`), and exec
drops the old process's connections the same way (Rust opens sockets
close-on-exec, and the frames arrive over IOSurface Mach ports, which
exec doesn't inherit). isocastd keeps its engine while a session or
stream exists (`stop_idle_engine_now`,
`crates/isocast-daemon/src/main.rs`), and while idle it keeps the live
engine for 15 s after a client last asked for frames (`ENGINE_IDLE`), so
sources (cameras) stay open across a relaunch that takes under a second.

Verified twice on 2026-10-13:

- **Spike** (isocast-m2-a): a 60.7 s take and a stream to a local server,
  with 24 relaunches across themes. Stream live throughout: 1818 frames
  sent, 0 dropped, 0 audio dropouts, 0 reconnects. Program file
  H.264+AAC, 1817 frames; bars ISO file HEVC+AAC, 3634 frames; both 0
  dropped, 0 audio dropouts, video and audio 60.566667 s each (ffprobe).
- **Plugin-fit check** (Ryan's Mac, isolated engine on `/tmp/itv.sock`):
  `isocast start --source test-pattern:1280x720@30 --source tone`, then
  the real app launched offscreen against it three times (two normal
  exits after connecting, one `kill -9`). Status after each: still
  recording, 0 dropped. The take stopped at 28.23 s: HEVC 1280×720 at
  30/1, 847 frames (exactly 28.23 × 30), AAC 28.23 s; manifest
  `complete`, 0 dropped, 0 audio dropouts. Files in
  `target/scratch/theme-verify/`.

Each theme slice repeats this check (section 13).

### 5.4 When the app waits

exec runs no destructors, so anything the app hasn't handed to isocastd
is lost. The app doesn't relaunch while it would lose work:

- a dialog has unsaved edits (a dirty Settings draft, a Properties form
  with changes);
- a text field holds an uncommitted edit;
- a pointer drag is in progress (canvas, fader, list reorder);
- edits are on their way: the edit pump's queue, in-flight steps or
  retries aren't empty (`session.queue`, `inflight`, `retry` in
  `app.rs`). The app waits up to 2 s for them to drain, then relaunches;
  if they haven't drained it waits as for the cases above.

It shows a toast instead: "Theme changes to Workbench when Isocast
restarts · Restart now". "Restart now" relaunches (after the same
drain). The app's own picker applies on OK or Apply, so it never waits on
itself. The app's report (6.2) says why it is waiting.

Two apps open (two windows, or the web app until N.3): each follows the
event on its own. The web app ignores it.

## 6. The setting, the method table, CLI, MCP and events

### 6.1 Where the choice lives

An engine-wide app setting, kept by isocastd:

- **Not in a profile.** Profiles hold output settings, and switching one
  is refused while recording. A theme has nothing to do with output and
  may change at any time. (Ryan can overrule: a theme per profile would
  be a profile field, and a profile switch would then relaunch the app.)
- **Not only in the app.** CLI and MCP must set it.
- **File:** `app.toml` at `api::app_settings_path()`:
  `$ISOCAST_APP_SETTINGS`; else, for a daemon on a socket of its own,
  beside it (`/tmp/x.sock` → `/tmp/x.app.toml`), so test engines never
  touch the real one; else `<support>/app.toml`. Written whole and
  renamed into place. isocastd is the only writer; the app only reads.
  A file that can't be read is moved to `app.broken.toml`, never
  overwritten (as the data store does).

```toml
version = 1
[appearance]
theme = "console"          # a built-in id, or "<plugin id>/<theme id>"
```

### 6.2 Methods

| Method | CLI | MCP | Event |
|---|---|---|---|
| `theme_list` | `isocast theme list [--json]` (also bare `isocast theme`) | `list_themes` | — |
| `theme_get {id?}` (default: the current one, with what each app shows) | `isocast theme show [ID] [--tokens] [--json]` | `get_theme` | — |
| `theme_set {id, restart?, reload?, preview?}` | `isocast theme set ID [--no-restart] [--reload] [--preview] [--json]` | `set_theme` | `theme_changed` |
| `theme_report {chosen, shown, fallbacks, pending_restart}` (each app, after launch and when it changes) | read with `isocast theme show` | read with `get_theme` | `theme_applied` |
| `plugin_check {path}` | `isocast plugin check PATH [--json]`, also `isocast theme check PATH` | `check_plugin` | — |

- `theme_set` names the field and the valid ids when it refuses:
  `id: no theme "consol"; did you mean "console"? The themes are
  console, workbench, daylight, blender, ryan-mix/daylight-desk`
  (over the socket: `params.id: …`). It refuses a theme whose state is
  `broken`, `incompatible` or `disabled`, and lists why.
- A built-in theme that isn't ready yet is refused unless `preview` is
  given: `id: workbench is a preview; missing: LUFS readout (T.7b),
  reset arrows (T.7d); pass --preview to use it anyway`. The picker
  lists previews under "Previews", which passes it.
- `theme_set` is allowed while recording and streaming.
- **Restarts only when the theme changes.** Setting the current theme
  again does nothing, so an agent that "makes sure" Console is set never
  restarts Ryan's app. `reload` relaunches the apps even when the theme
  is the same, which is how a theme author sees an edit to a linked
  theme. `restart: false` (`--no-restart`) only saves; apps show the
  "Restart now" toast.
- **What the apps show.** `theme_get` returns, beside the setting, one
  entry per connected app: `{pid, chosen, shown, fallbacks[],
  pending_restart}` (for example "a Settings draft is open"), as each app
  last reported it; an entry goes when its connection closes. The event
  `theme_applied` announces each report. So a fallback (a missing font,
  a faint safety colour, a piece this app lacks), a deferred restart or
  a failed exec is visible to the CLI and agents, not only in a toast.
  `isocast theme show` prints it: "app 4312: showing Console (chose
  ryan-mix/daylight-desk; font Inter missing → Console's fonts)".
- `plugin_check` runs `plugin add`'s checks without installing, for
  authors.
- Themes are installed only with `plugin add` (event `plugin_changed`).
  There is no second install path.
- The event kinds are `theme_changed` and `theme_applied` for `watch
  --kinds`.

### 6.3 Types (`crates/isocast-protocol/src/theme.rs`)

```rust
pub const THEME_ABIS: &[u32] = &[1];
pub const DEFAULT_THEME: &str = "console";
pub const BUILTIN_THEMES: &[&str] = &["console", "workbench", "daylight", "blender"];

pub struct TokenSpec {
    pub name: &'static str, pub ty: TokenType,
    pub range: Option<(f32, f32)>,      // finite bounds for lengths and numbers
    pub em_of: Option<&'static str>,    // the size an em letter-spacing value is taken against
    pub guard: Option<Guard>,           // a safety colour, or a size that shows REC/LIVE
}
pub enum TokenType { Color, Length, Float, Int, Bool, Weight, Duration, Font, Enum(&'static [&'static str]) }
pub static TOKENS: &[TokenSpec];        // build.rs generates the Theme global from it

pub struct SlotSpec {
    pub name: &'static str,             // "number-field"
    pub component: &'static str,        // the kit name, "NumberField"
    pub pieces: &'static [&'static str],
    pub properties: &'static [(&'static str, &'static str)], // name, Slint type
    pub callbacks: &'static [&'static str],
    pub actions: &'static [&'static str], // what every piece must make reachable (section 9)
    pub extras: &'static [&'static str],  // accessible elements a piece may add
}
pub static SLOTS: &[SlotSpec];          // build.rs generates the dispatchers and Look from it

pub struct ThemeSpec {                  // a [[themes]] entry
    pub id: String, pub name: String, pub description: String,
    pub abi: u32, pub base: Option<String>, pub appearance: Option<Appearance>,
    pub tokens: Option<String>,         // tokens.css, in the plugin's folder
    pub set: BTreeMap<String, toml::Value>,
    pub fonts: Vec<String>, pub previews: Vec<String>,
    pub slots: BTreeMap<String, String>,
    pub missing: Vec<String>,           // built-ins only: pieces not drawn yet; non-empty = a preview
}
pub enum Appearance { Dark, Light }

pub struct Themes { pub current: String, pub themes: Vec<ThemeInfo>, pub apps: Vec<AppTheme> }
pub struct ThemeInfo {
    pub id: String, pub name: String, pub description: String,
    pub source: ThemeSource,            // Builtin | Plugin { id, version }
    pub abi: u32, pub appearance: Appearance,
    pub state: ThemeState,              // Ok | Broken | Incompatible | Disabled
    pub status: ThemeStatus,            // Ready | Preview { missing }
    pub problems: Vec<Issue>,
    pub slots: BTreeMap<String, String>,
    pub previews: Vec<Preview>,         // Builtin("workbench/main") | File(PathBuf)
    pub current: bool,
}
pub struct AppTheme {
    pub pid: u32, pub chosen: String, pub shown: String,
    pub fallbacks: Vec<String>, pub pending_restart: Option<String>,
}
pub struct ResolvedTheme {
    pub id: String, pub chosen: String, pub appearance: Appearance,
    pub tokens: BTreeMap<String, String>, pub fonts: Vec<PathBuf>,
    pub slots: BTreeMap<String, ResolvedSlot>, // { piece, fallback: Option<String> }
    pub warnings: Vec<String>,
}
pub struct ThemeSetParams {
    pub id: String,
    #[serde(default = "yes")] pub restart: bool,
    #[serde(default)] pub reload: bool,
    #[serde(default)] pub preview: bool,
}
pub struct ThemeChange { pub theme: String, pub previous: String, pub reload: bool, pub notes: Vec<String> }

pub fn resolve(app_settings: &Path, plugins: &Path, choice: Option<&str>) -> ResolvedTheme;
```

Other changes:

- `plugin::Manifest` gets `themes: Vec<ThemeSpec>` (`[[themes]]`), and
  `KindClass::Theme`, so `plugin list` shows a plugin's themes. Its
  "the plugin provides nothing" check counts `[[themes]]`. The plugin
  `api` stays 1.
- The installed plugin's record (`install.toml`, today a private type in
  `crates/isocast-daemon/src/plugins.rs`) moves to `isocast-protocol`
  with a reader, so the app's `resolve()` and isocastd agree on which
  plugins are installed, linked and enabled.
- `api::Method`: `ThemeList`, `ThemeGet(ThemeTarget)`,
  `ThemeSet(ThemeSetParams)`, `ThemeReport(AppTheme)`,
  `PluginCheck(PluginCheckParams)`; `api::app_settings_path()`;
  `Event::ThemeChanged { theme, previous, reload }`,
  `Event::ThemeApplied(AppTheme)`.
- `theme_get` returns resolved tokens, so a web client could map them to
  CSS variables later. Nothing is built for that now.

## 7. The theme plugin kind

### 7.1 The manifest

A `[[themes]]` section in `isocast-plugin.toml`, beside any other
sections a plugin has:

```toml
id = "ryan-mix"
name = "Ryan's mix"
version = "0.1.0"
api = 1

[[themes]]
id = "daylight-desk"            # chosen as "ryan-mix/daylight-desk"
name = "Daylight desk"
description = "Daylight's colours with Workbench's fields and Console's LCD."
abi = 1                         # theme ABI: token registry + slots + pieces
base = "daylight"               # anything not given comes from here (default "console")
appearance = "light"            # dark | light (default: the base's): native menus, panels
tokens = "tokens.css"           # optional; Paper's get_tokens CSS
fonts = ["fonts/Inter-Regular.ttf"]
previews = ["previews/main.jpg"]

[themes.set]                    # single tokens, beyond or instead of tokens.css
accent = "#e05a1c"
label-col = "160px"
lcd-place = "source-bar"

[themes.slots]                  # slot = a piece compiled into Isocast
number-field = "workbench"
lcd = "console"                 # Console's LCD in Daylight's source bar
meter = "workbench"
```

This is what Ryan asked for: mixing signature pieces needs no code. A
theme names pieces; it doesn't carry them.

### 7.2 Checks

`Manifest::check` runs them in isocastd, without Slint, at `plugin add`
and `plugin check`, and `resolve()` runs the ones that matter for safety
again at every launch (2.1). Each names its field path and, for files,
the line:

- An unknown token: `themes[0].tokens: tokens.css:12: unknown token
  "accent-fil"; did you mean "accent-fill"? (all of them: isocast theme
  show console --tokens)`.
- A value of the wrong type or out of range: `themes[0].tokens:
  tokens.css:3: surface-panel: "12px" isn't a colour (#rgb, #rrggbb,
  #rrggbbaa, rgb(r g b / a%))`; `themes[0].set.row: "NaNpx" isn't a
  length between 16px and 64px`.
- A token given twice: `tokens.css:40: "accent" again (first at line 31)`.
- An unknown slot: `themes[0].slots.metre: unknown slot; valid:
  number-field, slider, toggle, …`.
- An unknown piece: `themes[0].slots.meter: no piece "logic"; valid:
  console, workbench, daylight, blender`. `none` only where the slot
  allows it.
- Files (`tokens`, `fonts`, `previews`): relative, inside the plugin's
  folder (resolved once), each opened without following a link
  (`O_NOFOLLOW`) and checked on the open file to be a regular file (not
  a FIFO, device or directory) of at most its size limit before it is
  read, and read with that limit, so a file that grows can't run the app
  out of memory: the manifest and `tokens.css` 64 KB each; fonts at most
  20 and 12 MB in all; previews PNG or JPEG, at most 6, 2 MB each and
  1024 px a side, read from the header (`imagesize`, already in
  `Cargo.lock`) before anything is decoded.
- Fonts parse: each face is read with `ttf-parser` (already in
  `Cargo.lock` through Slint's text stack), which rejects a damaged file
  and gives its family names, so `check` can say that a font token names
  a family no file provides.
- `base`: a built-in, another theme in this plugin, or `<plugin>/<theme>`;
  no cycle; at most four deep.
- `abi` not in `THEME_ABIS`: the plugin is added but the theme is listed
  `incompatible`, as plugins with another `api` are.
- `id`: lowercase letters, digits and dashes, unique in the plugin.
- `missing` (what makes a theme a preview): allowed in built-in
  manifests only.
- Contrast, as **warnings**: text, text-2 and text-3 on the panel,
  header, chrome and overlay surfaces (4.5:1, 4.5:1, 3:1); the accent's
  text on its fill (4.5:1); focus ring, danger, onair and warning against
  the surfaces they sit on (3:1). The built-ins must pass with no
  warning.

A `cargo test` in `plugin.rs`'s existing tests runs `Manifest::check` on
every built-in package, and fails if a built-in leaves any token or slot
to its base.

### 7.3 Permissions and what a theme can reach

A theme carries data. It can't run a process, call a method, reach the
network or read outside its folder: it can only set token values and name
pieces that ship in Isocast.

- A plugin made only of themes asks for nothing. `check_grant`
  (`crates/isocast-daemon/src/plugins.rs`) gains one arm,
  `(None, None) if asked.is_empty() => Ok(asked)`, so `isocast plugin add
  PATH` installs it without `--allow`. Today such a plugin is refused
  until `--allow`.
- **What gets parsed.** The manifest (TOML), `tokens.css`, the preview
  headers and images, and the fonts. Fonts are the riskiest: the app
  hands them to fontique and to Skia's font manager (CoreText on macOS,
  `FontMgr::new_from_data`), which parse them inside the app's process,
  without the checks macOS's font daemon makes when a font is
  installed. The sizes above and the `ttf-parser` check narrow that; they
  don't remove it. A theme plugin is something Ryan installs himself on
  his own Mac, which is why that is acceptable for now.
- **Previews** are decoded by the app with explicit limits (`image`'s
  `Limits`, already in the tree through Slint: 1024 px a side, 8 MB),
  not through `Image::load_from_path`, whose decoder allows 512 MiB.
- **Safety colours.** At launch, if `danger`, `onair` or `warning` fails
  3:1 against the surfaces it sits on, or `text` fails 4.5:1, that token
  falls back: first to its base's value if that passes on this theme's
  surfaces; else to the theme's own value with its lightness moved until
  it passes; else to a fixed safety palette per appearance (in `TOKENS`,
  checked on every built-in surface). A light theme on a dark base thus
  never gets a dark theme's yellow on white.
- **Safety sizes.** The tokens that size the REC and LIVE controls and
  the status readouts (status bar height, control heights, their font
  sizes, the dot) have minimums in `TOKENS`; gutters and gaps have
  maximums. A theme can't set `statusbar` to 0 or `frame-gutter` to a
  million pixels.

### 7.4 Versioning

- `abi` is the theme ABI's major version, separate from the plugin
  `api`. ABI 1 is: the token registry (`TOKENS`), the slot names, and the
  piece names. It says nothing about where slots sit in the window
  (2.5).
- Adding a token, slot or piece doesn't bump it: an older theme gets the
  base's value or piece.
- Renaming a token keeps the old name as an alias for at least one
  release (`TOKEN_ALIASES`; `check` warns "renamed to …").
- Removing a piece or slot doesn't bump it either: a theme naming it
  falls back to its base's piece with a warning.
- Changing a token's type or a slot's meaning bumps the major.
- ABI 2 is kept for code pieces (T.8): a theme's own `.slint` per slot,
  checked against `SLOTS`' interfaces.

### 7.5 Fallbacks

At launch the app resolves as narrowly as it can, and says what it did in
one toast, one log line and its report to isocastd:

- a bad, unknown or out-of-range token value → the base's value;
- a font that can't be read or parsed, or a family no registered font
  provides → the base's font tokens;
- a piece this binary doesn't have (an app older than the daemon, or a
  build without that theme's feature) → the base's piece;
- a safety colour that fails contrast → as in 7.3;
- a base chain with a cycle or more than four links (possible after an
  upgrade or an edit) → cut there, Console fills the rest;
- an unknown, removed, disabled or incompatible theme → `console`;
- a theme that didn't reach its first frame last time → `console` (the
  launch marker, 2.1).

`theme_list` and `theme_show` report a theme as `broken` with its
problems.

### 7.6 Removal, disabling, upgrading

- `plugin remove` of the plugin that provides the current theme is
  allowed. The setting is kept as it is (as setups keep missing plugin
  kinds). The reply notes: "the current theme ryan-mix/daylight-desk falls
  back to Console at the app's next launch; `isocast theme set` chooses
  another".
- Disabling it does the same.
- Upgrading it (`plugin add` of a newer version) leaves running apps as
  they are; they show "Daylight desk was updated · Restart now".
- A linked theme (`plugin add --link`) is read from its source folder at
  every launch, with every limit of 7.2, so an author edits `tokens.css`
  and runs `isocast theme set ID --reload` to see it.

### 7.7 Why plugin code pieces wait

A plugin that ships its own pieces would load `.slint` files at launch
through slint-interpreter into ComponentContainers. The spike did it:
Workbench's meter and number field from a folder, 4.7–5.0 ms each to
compile warm, drawing the same pixels as compiled code. It stays out of
ABI 1 because:

- it rests on APIs Slint marks experimental or hidden (above), which may
  change in any release;
- each piece has its own globals, so Rust must copy every token in and
  relay every value and callback;
- it isn't a sandbox: a piece can read any image or font by absolute path,
  import any `.slint` file, and pass the values it is given to
  `Platform.open-url`; a `Timer` or an endlessly repeating animation
  redraws the whole window for good; a `for` over a huge range freezes
  the app or exhausts its memory; a binding loop panics, which at launch
  is a crash on every start;
- it costs 8 MB and about 2 points of CPU for three meters and a field.

New pieces come into Isocast as source, in a theme folder, as the four
directions do. For a local-first app that Ryan builds himself, that is
enough. T.8 revisits code pieces when Slint makes ComponentContainer
stable; it would need more than a manifest grant: a check of the
compiled tree (no Timer, only finite animations, no absolute paths or
imports outside the theme's folder, size caps), a frame-time watchdog,
and the launch marker.

## 8. The four built-in themes as packages

The four directions ship in the plugin format, so the loader and the
validator run on real themes every build. B (Session Grid) isn't built.

```text
crates/isocast-protocol/themes/<id>/    what isocastd needs
  isocast-plugin.toml                   the manifest: one [[themes]] entry, every token and slot set
  tokens.css                            Paper's get_tokens output for that direction
crates/isocast-ui/ui/themes/<id>/       what only the app needs
  number-field.slint …                  one file per piece (WorkbenchNumberField…)
  fonts/                                the static cuts the boards use, with LICENSE-* files
  previews/                             main.jpg, studio.jpg, settings.jpg: 480 px, from the app
  splash.png                            Blender only: the splash's artwork, exported from Paper
```

| Theme | Paper file | Fonts (all SIL OFL 1.1) | Appearance |
|---|---|---|---|
| console | c-console, 01M4SHX06Z4XBZDXMWN2YEB9EA | Mona Sans 400–700, Hubot Sans 600, Martian Mono 300 | dark |
| workbench | a-workbench, 01M4SHWYV43XWS5KHXE9DET29W | Geist, Geist Mono | dark |
| daylight | d-daylight, 01M4SHX0S01G9PHZJF0H2SYGXC | Instrument Sans, Instrument Serif, Fragment Mono | light |
| blender | e-blender, 01M4SJSDJTPJZ0Q76EEJ9BRT96 | Inter Tight 400 and 500, Noto Sans Mono | dark |

- **Every built-in sets every token and every slot.** `tokens.css` is
  Paper's 97, verbatim, and `[themes.set]` sets every other token (the
  literals each board repeats, read with `get_jsx` and
  `get_computed_styles`, every layout token and every canvas token);
  `[themes.slots]` names a piece for every slot. Nothing is inherited, so
  Console can keep changing (its dialogs are still landing) without
  moving the others, and the test in 7.2 fails on a gap. `isocast theme
  show console --tokens --toml` prints a complete table to start a new
  one from.
- **Console is a package like the others.** Its `tokens.css` and
  `[themes.set]` are where Console's values live; `build.rs` makes them
  the generated `Theme` global's defaults.
- **Blender's font.** Paper's e-blender tokens name Inter Tight
  (`--font`, `--font-brand`: "Mockup stand-in: Blender 4.x uses Inter,
  but Paper only has Inter's italic styles … Ship Inter (OFL) in
  Slint"). The boards Ryan liked are drawn in Inter Tight, so T.6 ships
  Inter Tight 400 and 500 and sets `font`, `font-brand` and the weights
  (regular 400, medium 400, semibold 500) explicitly in `[themes.set]`.
  That keeps the board check's 2 px text tolerance. Real Blender's Inter
  is wider; if Ryan prefers it (section 15), T.6 sets `font = "Inter"`
  instead and the board check stops comparing text widths for Blender
  (edges and sizes of everything else still count).
- **Fonts** are each project's own static cuts, unmodified, only the
  weights the boards use, each with its licence file beside it. An
  instance we cut ourselves would be a Modified Version under the OFL and
  couldn't keep a Reserved Font Name (N.5 found this for Mona and Hubot).
- **Previews come from the app, not from Paper.** Each theme slice
  generates them from offscreen snapshots of the theme against the
  isolated engine and setup the board check uses (`isocast-ui --snapshot
  … --theme ID --state …`, then `sips -Z 480`), so the picker shows what
  the build draws, not a picture of pieces it may not draw yet. They are
  regenerated whenever the theme's pieces change.
- **Compiled in.** The generated dispatchers import every piece of the
  enabled themes, so `cargo build` compiles them; the fonts are
  `include_bytes!`; the previews are images in the app.
  `isocast-protocol` embeds each manifest and `tokens.css` from its own
  `themes/` folder with `include_str!` (`theme::BUILTIN`), so isocastd
  lists and resolves them without the app, and no crate reaches into
  another's folder. A built-in's manifest lists no fonts or previews (they
  are the app's); `ThemeInfo.previews` names them as
  `Preview::Builtin("workbench/main")`.
- **Listed** with `source: builtin` and their bare ids. They can't be
  removed or disabled. A plugin's themes are always `<plugin>/<theme>`,
  so they never clash with a built-in.

Workbench's manifest, abridged (the real one sets every token and slot):

```toml
id = "workbench"
name = "Workbench"
version = "1.0.0"
api = 1
license = "MIT OR Apache-2.0 (fonts: OFL-1.1)"

[[themes]]
id = "workbench"
name = "Workbench"
description = "Blender x Linear x DaVinci Resolve 20: graphite panels on near-black seams, one indigo accent."
abi = 1
appearance = "dark"
tokens = "tokens.css"
missing = ["LUFS readout (T.7b)", "folding panels and reset arrows (T.7d)", "…"]  # emptied by T.4's ready step

[themes.set]
label-col = "148px"
meter-warn-db = -18
meter-clip-db = -6
dock-gap = "1px"
dock-focus-ring = "#6c7bf459"
dialog-placement = "centre"
sidebar-dialog = "under-header"
row-label-right = true
row-value-end = false
row-band = "transparent"
row-well = "…"                  # read from Paper with get_computed_styles in T.4d
row-reset-lane = "12px"
handle-size = "6px"
handle-radius = "0px"
icon-stroke = "1.5px"
dropdown-glyph = "chevron-down"
checkbox-radius = "2px"
# … and every other token

[themes.slots]
number-field = "workbench"
dock-footer = "none"
viewer-line = "none"
lcd = "none"
page-header = "none"
nav-list = "workbench"
toggle = "console"
# … and every other slot
```

## 9. Accessibility and keyboard parity

Behaviour and accessibility live in each slot's behaviour base, never in
a piece, so every theme says and does the same things. (Slint's own
styles declare `accessible-*` per style; a style that forgets loses it.
We don't copy that.) The base also owns roles: a number field is a text
input in every theme (the spike's fill field declared itself a slider;
under this rule a piece can't).

**Parity is over actions, not elements.** Themes differ in which
controls they show: Workbench has no dock footer and shows eye and lock
only on hover; Blender's mixer strip has no headphones button. Forcing
Console's visible buttons into every theme would flatten the restrained
directions into Console. So:

- **An action** is anything the user can do: every `DockAction` in each
  dock's model, every entry of the action table (T.7f, from G.5's ids),
  and each slot's `actions` in `SLOTS` (a mixer strip: mute, monitor,
  gain, gain-reset, menu; a source row: visible, locked, iso,
  properties, rename…).
- **Every theme makes every action reachable by keyboard and announced**
  with the same name, value and state: as a button, or as an item in the
  dock's menu (⋯, Blender's editor-type menu), the strip's menu or the
  row's context menu. The dock's menu lists every action its header and
  footer pieces don't show as buttons; the base computes it, so a piece
  can't drop one.
- **Tab follows each piece's visual order.** Workbench's eye and lock
  aren't Tab stops (they show on hover); Visible and Locked are in the
  row's context menu and on the list's keys. Blender's strip tabs
  through Volume, mute and its menu; Monitor is in that menu. A
  collapsed Blender strip still exposes its name, value and menu:
  collapsing changes the order, not what is reachable.
- **Extras** are allowed only where `SLOTS` lists them for the slot:
  Workbench's section enable boxes, chevrons and grips; Blender's
  collapse controls, top bar and Properties tabs. Decorative extras
  (Blender's mouse hints, grips, Workbench's dock glyph) are
  `accessible-role: none`.
- **The same behaviour in every theme:** Escape always dismisses
  Welcome; notices keep their live region whether drawn as toasts or as
  Blender's status-bar reports (3.3); the tabbed Properties editor keeps
  the sheets' modality, drafts and apply rules (3.4).
- **Not by colour alone.** REC, LIVE, ISO and program each have a word
  or a shape besides their colour in every theme: REC and LIVE carry
  their words (all four boards do); ISO is a ring or an "ISO" capsule,
  program a filled dot or a "PGM" capsule. Workbench's ISO ring and
  Daylight's state dot keep their shapes; Blender's green and red LIVE
  and REC keep their words. Red and green alone look the same to
  deuteranopes, so each slice checks a greyscale snapshot.

| Slot | Role | Name, value | Keys |
|---|---|---|---|
| number-field | text input (as the kit's today) | label; value with unit | ↑ ↓ step (⇧ ×10), Return commits, Escape reverts, typing edits, Tab moves on |
| slider | slider | label; value-text | ← → ↑ ↓ step, Page Up/Down ×10, Home and End |
| toggle | switch (also Blender's checkbox look) | label; on or off | Space |
| segmented | tab list of tabs | label; the chosen one | ← → |
| quality | as today's QualitySlider | "Quality"; the stop's label | ← → |
| template-field | text input, with the pills as a list | "File name"; the template; the sample in the description | typing edits the plain-text form; ← → move between pills, Delete removes one |
| meter | progress indicator | label; "−12 dB" | not focusable |
| dock-header, dock-footer | buttons, and the dock's menu | each action's label | Tab between the buttons shown, Space or Return; the menu holds the rest |
| scene-row, source-row | list item, selected | name; index and count; hidden or locked in the description | today's list keys (N.2), unchanged; the context menu has every row action |
| mixer-strip | group of slider and buttons | source name | Tab in the piece's visual order; every strip action reachable (button or the strip's menu) |
| lcd | group of labels | "Recording 00:12:41", "29.93 fps", "0 dropped", "CPU 2.4%" | not focusable |
| section-header | button, expandable | title; expanded | Space or Return folds; the enable box and reset are their own buttons |
| nav-list | tab list of tabs | tab labels; the current one | ↑ ↓, Return |
| dialog-header, dialog-footer | (header not focusable except Blender's close dot, a button); buttons | — | Return is OK, Escape is Cancel (the Dialog container) |
| status-bar, details | buttons | REC, LIVE | Space opens details |
| notices | alert (live region) | title; text | as today's toasts |
| welcome | buttons | the three starts | Tab, Return; Escape always dismisses |
| properties-view (tabbed) | tab list, in a modal dialog | Source, Transform, Filters, Audio, ISO/Video | ↑ ↓ on the tab column; the menu commands open the matching tab; Return and Escape as the sheets |
| top-bar | menu bar items, tabs | menu titles; Layout, Studio | as the native menu bar |

Rules:

- Every focusable piece shows the shared `FocusRing` (tokens
  `focus-ring`, `focus-gap`, `focus-width`).
- Every piece honours `Theme.reduce-motion`.
- A behaviour added for one theme lands in the base, so every theme gets
  it (type-to-filter in menus and the notices' live region are the
  first).

**Check** (each theme slice): `isocast-ui --a11y --theme X --state S` for
every snapshot state; the action set (action id with name, value and
state) equals Console's; extras appear only where `SLOTS` lists them.
Then a keyboard pass with the `key:` and `type:` states: for every
action, a key sequence reaches and runs it in this theme and leaves the
same daemon state as in Console (the sequences may differ; the end state
can't). Then the greyscale snapshot: REC, LIVE, ISO and program still
tell apart. And every theme opens Rename, New scene, Add source and
Settings with the first field focused.

## 10. Performance budget

What a theme may cost, against Console in the same session on
isocast-m2-a (base M2), release builds:

- **Baseline:** Console after N.5, measured in T.3a, not the spike's
  numbers (2.6).
- **Noise first:** each measurement session starts with an A/A pass (the
  same build against itself, four interleaved runs each) and reports its
  interquartile range. A difference smaller than that range is reported
  as "within noise", not as a number. Builds are compared A/B in one
  session (the notes' `ab.sh`), never against old numbers.

| Measure | Budget | Spike (indicative only) |
|---|---|---|
| CPU, window shown, three tones metering | ≤ +1.5 points of one core | Workbench's two pieces +1.2 (19.0 vs 17.8), against the old look |
| CPU idle: connected, not recording, status updating | ≤ +0.3 points or within the A/A range, whichever is larger; at most one redraw a second | not measured |
| Memory footprint | ≤ +10 MB | Workbench −21 MB (fonts differ) |
| Launch to the first frame with meters, warm | ≤ +20 ms; the theme step ≤ 20 ms | 177–186 ms; step ~6 ms |
| Switch, `theme_set` to the first frame | ≤ 1 s median | 483–587 ms |
| Recording and stream through switches | 0 dropped, 0 audio dropouts, 0 reconnects | 24 relaunches, 0 |
| Binary, stripped, release (all themes) | target ≤ +5 MB per theme; over it, Ryan decides (2.7) | +2.1 MB for two pieces and fonts |
| Release build of isocast-ui, all themes | ≤ +30% clean; the Console-only dev build unchanged | +3% clean, +14% after a `.slint` edit |

Rules a piece follows:

- No timer or animation runs while nothing changes. Slint's Skia
  surface on wgpu redraws the whole window for any change (it never
  renders partially), so a pulsing dot costs a full redraw at its rate.
  Animations run only on interaction or state change.
- **Live readouts update together, at most once a second.** The LCD,
  the status bar's CPU, fps, dropped, free space and Mb/s, and the REC
  and LIVE times all change on one app timer tick, so they cost one
  full-window redraw a second between them, not one each. (The status
  poll stays at 3 s; the times tick from the tick.)
- Meters draw a lit zone as one element (a tiled image for segments),
  not one element per segment.
- A piece binds only the values it shows.

## 11. Failure modes

| What goes wrong | What happens | What the user sees | Recording |
|---|---|---|---|
| `app.toml` missing | Console | nothing | unaffected |
| `app.toml` unreadable | Console; isocastd moves it to `app.broken.toml` at its next write | one toast | unaffected |
| The theme's plugin removed or disabled | Console | "Theme ryan-mix/daylight-desk isn't installed; showing Console" | unaffected |
| The theme's ABI not supported | Console | toast naming the ABI | unaffected |
| The app crashes or hangs before its first frame with theme X | the next launch starts in Console (launch marker) until the choice changes | "Isocast didn't start with X last time, so it opened in Console" | unaffected |
| Ryan needs a known-good start | `isocast-ui --safe` (or `ISOCAST_THEME=console`) | Console, no plugin theme read | unaffected |
| A bad or out-of-range token (a linked folder edited) | the base's value | one toast listing all fallbacks; in `theme show` | unaffected |
| A font missing, unreadable or not parsing | the base's font tokens | the same toast | unaffected |
| A piece this binary lacks | the base's piece | the same toast | unaffected |
| A safety colour too faint | base, adjusted, or the safety palette (7.3) | the same toast | unaffected |
| A base cycle or chain deeper than four (after an upgrade or edit) | cut there; Console fills the rest | the same toast | unaffected |
| A theme file is a FIFO, device, link out of the folder, or too big | not read; that file's fallback | the same toast | unaffected |
| A preview declares huge dimensions | not decoded | the card shows no picture | unaffected |
| `theme_set` with an unknown, broken or (without `--preview`) preview theme | refused | the error lists the valid ids, the problems or what is missing | unaffected |
| `theme_set` with the current theme | nothing | the reply says it is already the theme | unaffected |
| The app is busy when the event comes | no relaunch; reported to isocastd | "Restart now" toast; `theme show` says why | unaffected |
| Edits still on their way | drained (up to 2 s), then relaunch | nothing, or the toast | unaffected |
| exec fails (the binary moved or was replaced badly) | the app keeps running in the old theme; reported | "Couldn't restart: … Quit and reopen Isocast." | unaffected |
| The app and isocastd read different `app.toml` files | one relaunch with isocastd's choice; at most three in 10 s | toast naming both choices if it repeats | unaffected |
| A relaunch while Ryan works in another app | the new process doesn't activate | nothing comes forward | unaffected |
| The app crashes during the relaunch | isocastd carries on | reopen the app | unaffected (verified with `kill -9`) |
| Switch while idle | the live engine waits 15 s for a client | cameras don't reopen | — |
| A theme makes a piece look wrong with another theme's sizes (a mix) | drawn as asked | — | unaffected |

## 12. How N.3 and P.2 change

**N.3 (the Slint app becomes Isocast.app, the web app retired):**

- Themes are Slint-only. Until N.3 the web app ignores `theme_changed`
  and keeps `app.css`; N.3 removes the last client that ignores it.
- The relaunch exec's the bundle's own executable, so the app keeps its
  name, icon and bundle. N.3's checks add: a switch from the bundle,
  window frame restored, native appearance applied.
- The two new prefs keys (window frame, `restore`) help every launch,
  not only switches.
- N.3 itself doesn't wait for T. T waits for N.3 (and A).

**P.2 (Profile and Scene Collection menus, done):**

- The menus stay in the native menu bar in every theme. A theme can't
  add, remove or rename menu items.
- Blender's top bar draws the scene collection selector from
  `LibraryArea.collections` and switches with the same `library_switch`
  call as the menu; its canvas selector shows the profile's video size
  and fps and opens Settings › Video. T.6c gives the menu bar one source
  (a model built by `MenuBuilder`, which the native MenuBar repeats over,
  as P.2's library menus already do), so the in-window menus can't drift
  from the native ones. Slint 1.18 flips a checkable item's `checked` by
  itself, so, as in P.2, the models are replaced after every choice.
- A theme is not part of a profile or a scene collection; switching
  either never changes the theme.
- P.2's dialogs (New…, Rename…, Remove…) use the Dialog container, so
  they take each theme's header and footer pieces with no change.

## 13. Implementation plan

T.1–T.3 build the mechanism with Console only and change no pixel.
T.4–T.6 each add one direction in four area slices (controls and
fields; docks, lists and mixer; frame and bars; dialogs, forms and
views), each leaving the theme a usable **preview**, then a **ready**
step once its data has landed (section 4.2). Because the dispatchers are
generated, the three directions don't collide in shared files and can
be built side by side. T.7's data slices are method-table features that
stand on their own; T.7a and T.7d come first (Console needs them too).
T starts after A ("other themes" wait for the alpha).

| # | Slice | Needs | Usable on its own as |
|---|---|---|---|
| T.1 | The setting and the theme kind | N.5 (the token registry mirrors its `theme.slint`) | `isocast theme`, theme plugins checked and installed |
| T.2 | Tokens, fonts, launch and relaunch | T.1, N.3 | the app follows the setting; window frame kept; no focus stealing |
| T.3a | Generated dispatchers, behaviour bases, `Look`; the Console baseline | T.2; the board-check scripts in git (Ryan, section 15) | Console's pieces through slots; the budget's baseline |
| T.3b | Dock actions as data | T.3a | `[DockAction]` models; dock menus hold the actions not shown |
| T.3c | Layout and canvas tokens in the containers | T.3a | Dialog (with `sidebar-dialog`), Dock, sections, form rows, lists, pictures and the canvas box read tokens |
| T.3d | Frame slots | T.3c | top bar, viewer line, source bar, LCD, status bar as slots |
| T.4a–d | Workbench by area | T.3d | `workbench` as a preview |
| T.4 ready | Workbench ready | T.4a–d, T.7b, T.7c, T.7d, T.7f (focused dock) | in the picker; "Ready for Ryan" |
| T.5a–d | Daylight by area | T.3d | `daylight` as a preview |
| T.5 ready | Daylight ready | T.5a–d, T.7a, T.7c, T.7d, T.7e | in the picker; "Ready for Ryan" |
| T.6a–d | Blender by area | T.3d | `blender` as a preview |
| T.6 ready | Blender ready | T.6a–d, T.7a, T.7b, T.7d, T.7f (action table, so G.5) | in the picker; "Ready for Ryan" |
| T.7a–f | Data the pieces need | — (T.7f's action table needs G.5) | each is a method-table feature |
| T.8 | What waits | Ryan | tabbed docks, workspaces, real windows, code pieces |

How every slice verifies: isolated engines (`ISOCAST_SOCKET` in scratch),
synthetic sources, offscreen snapshots on Ryan's Mac (no windows), real
windows only on the test Macs through `scripts/testmac.sh` (isocast-m2-a
preferred; hold the benchmark lock when measuring), scratch output in
`target/scratch/` or the test Mac's scratch folder. Paper values come
from `get_tokens`, `get_jsx` and `get_computed_styles`.

**Board-by-board check** (T.3 for Console; each T.4–T.6 area slice for
its boards; all six boards in each ready step). Export the theme's six
boards from its Paper file with Paper's `export` at 1x and 2x (main
window, Studio Mode, Properties, Settings › Output, Welcome, and the
Components page); snapshot the app with `--theme ID` at the board's size
and state against the isolated engine and setup N.5 used for the same
boards (`--state studio`, the Properties dialog's `dialog:` state,
`settings:output`, an empty setup for Welcome, `--state kit` for the
Components page); and pair them band by band with N.5's scripts
(`pair.py`, `rules.py`, `ink.py`; the "Verification recipes" entry in
docs/notes.md, on `ui/console` until N.5 merges).

- **The program picture is masked.** Its content is the scene, drawn by
  isocastd, which no theme changes: the boards' lower third "Isocast
  design review" is set in each direction's display face (Geist, Hubot
  Sans, Instrument Serif, Inter Tight), and Daylight's serif lower third
  is scene content, not theme. Inside the picture only the selection box
  and its handles are compared (a 4 px band around the box's edges and
  each handle).
- For every piece in section 3 and every row of 4.2: edges within 1 px
  at 1x, sizes exact, colours exact on flat areas, label widths within
  2 px (Skia draws text heavier than Paper; known; Blender with Inter,
  if chosen, compares no text widths). The Settings board checks the
  `sidebar-dialog` arrangement; the Components board checks the dropdown
  glyph, field chrome and checkbox radius.
- Differences are listed by name with their cause: missing data (T.7), a
  Slint limit (no blur, one shadow layer, no tabular figures), or an
  in-window stand-in for a real window. In a ready step, only Slint
  limits and in-window stand-ins may remain.
- **Every built-in theme is re-checked** in each T slice, and in any
  later change to `ui/kit.slint`, `ui/slots/` or a container, so a kit
  change can't silently move a theme that isn't being worked on.
- **The scripts aren't in git.** `pair.py`, `rules.py` and `ink.py` live
  in `/Volumes/IsocastBuild/console-foundation/scripts/` and the A/B CPU
  script in `/Volumes/IsocastBuild/n2-fix/remote/ab.sh`; a fresh session
  or a test Mac can't run them. AGENTS.md asks for Ryan's approval before
  adding harnesses, so "Questions for Ryan" asks to commit them under
  `scripts/`; T.3a waits for that.

### T.1 The setting and the theme kind

`isocast_protocol::theme` (types, `TOKENS` with ranges, `SLOTS` with
Console's pieces, actions and extras, `resolve()` with every limit of
7.2), the install record moved into the protocol crate, `[[themes]]` in
the manifest with its checks (file types and sizes, `ttf-parser`, image
headers), `KindClass::Theme`, `app_settings_path()` and `app.toml`,
`theme_list`, `theme_get` (with apps), `theme_set` (restart only on a
change, `reload`, `preview`), `theme_report`, `plugin_check`,
`theme_changed`, `theme_applied`, the CLI (`isocast theme
list|show|set|check`, `isocast plugin check`) and MCP (`list_themes`,
`get_theme`, `set_theme`, `check_plugin`), the `check_grant` arm, and the
built-in `console` package (manifest and Paper's `tokens.css`) in
`crates/isocast-protocol/themes/`.

Verify: against an isolated engine, `isocast theme --json` lists
`console`; `isocast theme set consol` names `id` and the valid ids;
`theme_set` writes `app.toml` beside the isolated socket only (the real
support folder untouched) and `watch --kinds theme_changed` shows the
event; setting the current theme again writes nothing and sends no
event, and `--reload` does; a test theme plugin in scratch with a
misspelt token, a wrong type, `NaNpx`, `infpx`, a negative size, an
unknown slot, an unknown piece, a missing font, a font that doesn't
parse, a 3000×3000 preview header, a FIFO named as `tokens.css` and a
base cycle gives each error with its field path and `file:line`, and
`resolve()` gives the same results when the linked folder is changed
after install (a cycle added by editing a linked theme doesn't recurse);
the fixed plugin installs with no `--allow`; `plugin remove` of the
current theme's plugin notes the fallback; MCP's tools round-trip
through a real client; `cargo test` runs `Manifest::check` on the
built-in manifest and fails when a token or slot is removed from it.

### T.2 Tokens, fonts, launch and relaunch

`build.rs` generates the `Theme` global from `TOKENS` with Console's
package values as defaults, and the setters; `tokens.css` and
`[themes.set]` loading with em conversion and ranges; fonts registered
with fontique before the window, with the family check (Console's fonts
move from `.slint` imports into the package); `--theme`, `--safe`,
`$ISOCAST_THEME`; the launch marker; the native appearance (NSApp, so the
title bar, menus, open panel and colour panel follow `appearance`); the
canvas tokens in `layers.rs`; the window frame and `restore` in prefs;
relaunch on `theme_changed` with the busy rules, the edit drain, the
environment (theme, loop guard, activation) and the inactive start;
`theme_report`; one fallback toast; Settings › General › Appearance
(theme cards with previews, a "Previews" disclosure, the restart note)
and View › Theme.

Verify: Console's snapshots identical to the commit before, pixel for
pixel, at 1280×800 1x and 2x, in every snapshot state; `--a11y` output
identical. On isocast-m2-a with a window: `isocast theme set console
--reload` relaunches in ≤ 1 s and the window comes back in the same
place and size (also on a second display where the test Mac has one, and
full screen); with another app frontmost, a relaunch leaves that app
frontmost (checked with System Events before and after); a relaunch of
the bundle opened from Finder keeps one Dock tile and the menu bar; the
open file count (`lsof -p`) is the same after 24 relaunches as after
one; the accessibility tree is present after a relaunch (cua-driver
reads the Scenes list); a fader released and `theme set` run within
100 ms leaves the daemon's setup at the final value; a hand-written
launch marker naming the chosen theme starts the app in Console with
the toast, and a normal launch removes the marker; `--safe` starts in
Console with a broken plugin theme chosen; a recording plus a local RTMP
stream through 20 relaunches (and one `kill -9` of the app) keep 0
dropped frames, 0 audio dropouts, 0 reconnects, with ffprobe durations
equal across files; a dirty Settings draft defers the relaunch with the
toast and `theme show` reports why; CPU with meters within the A/A range
of the commit before; binary and release build time recorded.

### T.3 Slots (four slices, each pixel-identical)

- **T.3a Dispatchers.** `build.rs` generates `ui/look.slint` (enums) and
  the 35 dispatchers in `ui/slots/` from `SLOTS`, behind the theme
  features; the hand-written behaviour bases; Console's pieces moved to
  `ui/themes/console/`, exported under the kit's names; the focus
  pattern of 2.3 in place of `.focus()` on pieces; the strip height out
  of `StripSize`; the one behaviour change, live scrubbing on the
  Transitions dock's Duration; `--state kit` drawing the dispatchers. The
  Console baseline for section 10: an A/A pass, idle, metering, launch,
  binary and build time.
- **T.3b Dock actions.** Docks' header and toolbar children turned into
  `[DockAction]` models, changed row by row; each dock's menu lists the
  actions its pieces don't show.
- **T.3c Containers.** The layout tokens of 2.4 read by the Dialog (with
  `sidebar-dialog`'s placement), the Dock (FlexboxLayout for
  `dock-footer-side`), sections (`visible: false` folding), form rows,
  lists, Stage and the picture frames; the canvas box from tokens; the
  kit widgets' tokens of 3.5.
- **T.3d Frame.** The frame as frame slots, `lcd` as a slot with
  `lcd-place`, `template-field` with Console's piece (drawn once T.7e
  gives it data).

Verify each: as T.2, pixel-identical and `--a11y`-identical in every
state, and the board-by-board check against Console's Paper file still
passes as N.5 left it; the keyboard pass (Tab order in every dock and
dialog, list keys, field keys) matches the commit before; Rename, New
scene, Add source and Settings open with the field focused; T.3a: a hand
edit adding a non-`Look` condition to a dispatcher fails the build (tried
once in scratch, not committed), and the Duration scrub applies live in
a real window on the test Mac; T.3b: a fader drag isn't ended by a mute
toggled from the CLI mid-drag; T.3c: a folded section's fields are
skipped by Tab and by VoiceOver; CPU and launch within the A/A range.

### T.4 Workbench

- **T.4a Controls and fields:** number field, slider, segmented,
  quality, the kit widgets' tokens, the canvas handles.
- **T.4b Docks, lists and mixer:** dock header (with the focused-dock
  ring, drawn once T.7f says which dock is focused), scene and source
  rows, mixer strip, LUFS header (drawn once T.7b lands), empty state.
- **T.4c Frame and bars:** monitor head, source bar chips (the codec
  chip once T.7c lands), status bar, studio column, controls; 1px dock
  gap on near-black seams.
- **T.4d Dialogs, forms and views:** dialog header and footer, centred
  placement, `sidebar-dialog = under-header`, section header (collapsible
  panels with reset arrows once T.7d lands), right-aligned 148px label
  column, dark wells, 12px reset lane, nav list, welcome.
- **T.4 ready:** every row of 4.2 draws; all six boards pass; previews
  generated from the app; `missing` emptied in the manifest; a "Ready
  for Ryan" entry.

Package: `tokens.css` from Paper's a-workbench file, every other token
in `[themes.set]`, Geist and Geist Mono with their OFL licence.

Verify (each area slice for its pieces, the ready step for all): the
board-by-board check against Workbench's Paper file
(01M4SHWYV43XWS5KHXE9DET29W); the fill field drags 300 → 5050 ms on the
Transitions dock's Duration in a real window on the test Mac with live
applies during the drag (as the spike); section 9's action parity,
keyboard pass and greyscale check; the budget table, A/B against Console
in one session; T.4a also records the binary and build cost of its
pieces and projects it for three themes (2.7); 10 switches between
Console and Workbench during a recording and stream, 0 dropped.

### T.5 Daylight

- **T.5a Controls and fields:** number field and slider with value
  fields, quality stops (the estimate once T.7a lands), the template
  field's pills and sample (with data once T.7e lands), the kit widgets'
  tokens (bare values, ⇕), the canvas handles.
- **T.5b Docks, lists and mixer:** dock header with counts, dock footer,
  scene and source rows with state dots, mixer strip and the shared
  scale, empty state.
- **T.5c Frame and bars:** monitor head and caption (media facts once
  T.7c lands), the mat (`mat-*` tokens; the native layer's opaque colour
  and the stage are the light `#e6e6e9`, and the picture hole sits inside
  the mat), source bar, status bar, studio column, controls.
- **T.5d Dialogs, forms and views:** dialog header and footer with serif
  titles, centred placement, `sidebar-dialog = full-height` with the nav
  list titled "Settings", small-caps section headers and grouped inset
  sections, page header (its meta once T.7a lands), nav list, welcome.
- **T.5 ready:** as T.4's.

Package: Instrument Sans, Instrument Serif, Fragment Mono;
`appearance = "light"`.

Verify: the board-by-board check against Daylight's Paper file
(01M4SHX0S01G9PHZJF0H2SYGXC); the native title bar, menus, open panel and
colour panel are light (snapshot and a real window on the test Mac); the
picture lines up with the canvas box in the same frame (N.1's frame
check, `--check`), with the mat; the contrast checks pass with no
warning; with T.7e, editing the pills changes the setup's template and a
recording's file names equal the sample; parity and budget as T.4.

### T.6 Blender

- **T.6a Controls and fields:** number field (label inside, blue fill),
  slider as a fill bar, checkbox toggle, joined enums, tooltip with the
  CLI line (from T.7f's action table when it lands; plain until then),
  menu panel, the kit widgets' tokens, the canvas handles.
- **T.6b Docks, lists and mixer:** dock header with editor type and
  header menus, the Scenes side column, outliner rows with stripes and
  the list well, mixer strip panels (stereo once T.7b lands), empty
  state.
- **T.6c Frame and bars:** top bar (the menu bar's one model, section
  12), the 2px gutter and rounded areas with their top highlight,
  monitor head and overlay, source bar, status bar with reports, notices,
  REC details (free on disk once T.7a lands), studio column as an area,
  controls.
- **T.6d Dialogs, forms and views:** dialog header as the board's title
  bar with no scrim, dialog footer with ≡, `sidebar-dialog =
  under-header`, panel headers, the blue-band nav list, hint icons and
  the decorator lane (its dot once T.7d lands), the splash,
  `properties-view = "tabbed"`. Preferences and Properties stay in-window
  (T.8).
- **T.6 ready:** as T.4's, after G.5 has merged and T.7f's action table
  exists.

Package: Inter Tight 400 and 500 (or Inter, section 15), Noto Sans Mono,
the splash bitmap exported from Paper; `font`, `font-brand` and weights
set explicitly.

Verify: the board-by-board check against Blender's Paper file
(01M4SJSDJTPJZ0Q76EEJ9BRT96); the window still fits at 900 px wide with
the gutters (every dock at its minimum); the in-window menus list exactly
the native menu bar's items (compared as data); Properties, Filters and
Edit Transform open the tabbed view on the right tab from every menu and
shortcut, and the same edits through sheets and tabs leave the same
setup; warnings and errors still raise toasts and every notice is
announced; every tooltip's CLI line runs (each command checked with
`--help`); parity and budget as T.4.

### T.7 Data the pieces need

Each is a method-table feature first (API, CLI with `--json`, MCP where an
agent would use it), then shown by the pieces that wait for it. Order:
T.7a and T.7d first (Console needs them too), then T.7c, T.7b, T.7e,
T.7f.

- **T.7a Disk and size.** Free space, volume name and time left while
  idle; a GB-per-hour estimate. Verify against `df` and a measured take
  on the test Mac; `status --json` while idle.
- **T.7b Levels.** Per-channel levels and the program mix's level in
  `levels`; EBU R128 loudness on the program (and per source on request).
  Verify with tone sources of known level: a 1 kHz tone at −23 dBFS on
  both channels reads −23.0 LUFS within 0.1 LU (EBU Tech 3341's own
  check); a left-only tone lights only the left channel.
- **T.7c Media.** Position, duration and codec in `SourceStatus`, shared
  with G.6. Verify against ffprobe of the clip.
- **T.7d Forms.** Sections and titles in `setup schema` forms; each
  field's default, "is default" and a reset; fields locked while live.
  Verify a reset through `plan` and `apply` from the CLI, then the app.
- **T.7e ISO file names.** A template setup field with field-path errors,
  and the sample name. Verify a recording's file names match the sample,
  and that the template field edits it in every theme.
- **T.7f App-side data.** Canvas fps in `Ui`, quick transitions with
  durations, item counts, Settings groups and descriptions, preview zoom
  and pan, folded sections and strips, list row counts, the focused dock,
  Welcome dismiss and links, menu type-to-filter, and the action table
  (from G.5's action ids, generated from the CLI). Verify each in
  snapshots, and that per-viewer state survives a relaunch.

### T.8 What waits, until Ryan asks

- **Tabbed docks:** Workbench's icon-over-label tabs (Resolve) and
  Daylight's small-caps tool tabs with the active one tinted (Capture
  One), both on their Components pages; Blender's area-type switching.
  Layout editing, which A lists as waiting.
- **Workspaces:** Blender's "Beyond OBS: Layout" (Live and Review
  workspaces, scene shelf, outliner tree, timeline, filters as a
  modifier stack, REC/LIVE in the top bar) and Console's "Workspace ·
  Live/Build/Create" and "Customize & flow" boards. A's "full
  workspaces"; a layout feature, not a theme (2.5).
- Real separate windows for Blender's Preferences and Properties. Slint
  supports several windows, but each window component has its own
  globals, so every area global would be relayed from Rust.
- Code pieces in theme plugins (ABI 2), with the checks of 7.7.
- SVG icon overrides in theme plugins.
- A "more contrast" variant per theme.

## 14. Decisions and rejected alternatives

- **Console is the default, today's copper look retires with N.5.** N.5
  rebuilds the kit as Console. Keeping the old look as a fifth theme
  would cost a fifth set of pieces nobody asked for. (Overrule: it is in
  git history and could become a package.)
- **Compiled pieces for the built-ins.** Measured free at run time. Not
  interpreted pieces (experimental, +8 MB, +2 points), not the whole UI
  interpreted (+27% CPU, +245 MB, +450 ms), not one build per theme.
- **Relaunch, not live switching.** Ryan allowed it; it removes every
  carry-over problem; 0.5 s.
- **exec, not `open -n`.** Same pid, same bundle, measured. `open -n -g`
  is the fallback.
- **The app resolves the theme itself** from `app.toml` and the plugins
  folder, with the same code isocastd uses. The first frame is right and
  launch never waits for isocastd. Rejected: asking isocastd at launch
  (a "connecting" screen in the wrong theme, and no theme without a
  daemon), or a resolved cache written by isocastd (stale while an
  author edits a linked theme).
- **Engine-wide, not per profile, not per viewer.** One method table;
  profiles are output settings. Per run: `--theme`.
- **`tokens.css` from Paper verbatim**, not a TOML token file. Paper is
  where the themes are drawn; no hand translation. Single values go in
  `[themes.set]`, which wins.
- **Data-only theme plugins.** The prior art says the theme systems that
  last expose named values and stable component interfaces (Firefox,
  VS Code, Zed, libadwaita, Qt Quick Controls), and the ones that
  exposed internals broke on updates (Firefox complete themes, Kodi
  skins, Winamp modern skins). ABI 1 exposes named values and a choice of
  pieces; the interfaces stay ours.
- **Generated dispatchers** (changed from hand-written): `build.rs`
  writes them from `SLOTS`, so a branch can't forget a binding, a piece
  can't be misspelt (enums), conditions are only on `Look`, themes can be
  left out of a build, and the three directions don't collide in shared
  files.
- **One icon set**, with stroke and kind-colour tokens, not a set per
  theme.
- **Blender's windows in-window.** A real window means relaying every
  area global from Rust. Its title bar is drawn as the boards show it.
- **Missing data hides, never invents**, and a theme with missing pieces
  is a preview, not ready.

### Revisions after the critiques (2026-10-13)

Two critiques read the first version: one for taste (against the
boards), one technical (against the code and Slint's source). Each
finding was checked before it was taken: the boards and Paper values
(Settings arrangements, dropdowns, checkboxes, handles, Blender's
tokens, the workspace pages), the app (`layers.rs`, `background.rs`,
`app.rs`'s edit pump), the spike (`skin.rs`, `slots.slint`, its
`theme.slint`) and Slint 1.18.1's source (`ChangeTracker::init`,
FlexboxLayout's stability, Skia's partial rendering, `image`'s 512 MiB
limit, Skia's font manager, `move_focus`, AccessKit's hidden nodes,
`accessible-live-region`). Every finding held up on its facts. A few
fixes were changed from what was suggested, and one conclusion is
rejected; both are listed below.

Taken (taste): theme slices gated on their data, with a preview/ready
split and the per-theme checklist (4.2, 13); parity over actions with
Tab in visual order (9); `sidebar-dialog` (2.4); the `template-field`
slot (3.1); built-ins set every token and slot (8); Blender's font set
explicitly, Inter Tight by default, Ryan to choose (8, 15); canvas
tokens and the selection box in the board check (2.4, 13); Blender's own
nav-list piece, its title bar as drawn, hint-icon and decorator-lane
tokens (3.4, 2.4); dropdown, field-chrome and checkbox tokens (3.5); a
"what waits" list and workspaces kept out of the theme ABI (2.5, 13,
15); previews from the app (8); `lcd` as its own slot (3.3); the
picture masked in the board check (13).

Taken (technical): the launch marker, `--safe` and limits enforced at
launch (2.1, 7.2); no focus stealing on relaunch (5.1); queued edits
drained before exec (5.4); conditions only on `Look` (2.3); focus through
the base and `focus-pending` (2.3); `theme_report`, `theme_applied` and
restart only on a change (6.2); file, size, number and image limits,
honest wording about font parsing, `ttf-parser` (7.2, 7.3); action-based
parity, Escape, live regions, modal tabbed view, not colour alone, the
safety-colour chain (9, 7.3); the re-baseline, idle row, 1 Hz readouts
and A/A pass (10); generated dispatchers with features and enums (2.3);
the scripts question (13, 15); T.1 after N.5, T.3 and T.4–T.6 split, the
Duration scrub assigned to T.3a, models changed row by row (13);
FlexboxLayout and `visible: false` named as mechanisms (2.4); the loop
guard and the daemon's choice through the environment, and the lsof,
Finder and VoiceOver checks (5.1, T.2); the code-piece hazards (7.7);
the built-in data in the protocol crate, the `Theme` global generated
from `TOKENS`, and `Preview::Builtin` (8, 6.3).

Changed from what was suggested:

- **Ready prerequisites are wider than proposed.** The taste critique
  suggested T.4 needs T.7b and T.7d, T.5 needs T.7a, T.7c and T.7e. The
  boards also show Workbench's media facts and codec chip (T.7c),
  Workbench's focused-dock ring (T.7f), Daylight's Properties sections
  (T.7d) and Blender's Properties panels and decorator dots (T.7d). Each
  ready step lists them all (4.2).
- **Theme features default on.** The technical critique suggested
  Console-only builds for the dev loop and all themes for release.
  Default features include every theme so `cargo build`, clippy and the
  tests (which must be clean before every commit) cover all of them;
  Console-only is opt-in with `--no-default-features`.
- **Tokens, not a dropdown slot.** For the kit widgets that differ by a
  glyph, a radius or the field's chrome, tokens suffice; a slot would
  add a dispatcher, a behaviour base and four pieces for a glyph.
- **The binary budget stays, as a target.** The projected 5–9 MB per
  theme is plausible but unmeasured; T.4a measures it, and passing the
  budget goes to Ryan rather than being quietly accepted or designed
  around now.

Rejected:

- **A collapsed Blender strip changing the Tab order is not a parity
  failure.** The technical critique listed it among per-theme behaviour
  changes. Under action parity, order follows what is drawn; what must
  not change is that every action stays reachable, which a collapsed
  strip keeps (its name, value and menu).
- **Taste finding 14** (the design keeps OBS's layout without OBS's
  style) asked for no change, and none was made.

## 15. Questions for Ryan

Nothing in T.1–T.3 waits on these except the first.

- **May the board-check and A/B scripts go into the repository?**
  `pair.py`, `rules.py`, `ink.py` (N.5's Paper comparison) and `ab.sh`
  (the A/B CPU runs) are on the build volume, not in git, so a fresh
  session or a test Mac can't run T.3–T.6's checks. AGENTS.md asks for
  your approval before adding harnesses. **T.3a waits on this.**
- **Blender's font.** The boards you liked use Inter Tight (Paper has no
  upright Inter); real Blender uses Inter, which is wider. The design
  ships Inter Tight, matching the boards. Say "Inter" and T.6 ships
  Inter instead (the board check then stops comparing Blender's text
  widths).
- **Blender's Preferences and Properties as in-window dialogs.** They
  are drawn as the boards show them, with a title bar and three
  window-control dots of which only close works, inside the main window
  and without a scrim. Real OS windows are possible later (T.8) at the
  cost of relaying every area's state between windows. Is in-window
  fine?
- **Binary size.** If the four themes push the release binary past +5
  MB per theme (the projection is 5–9 MB), is that acceptable for a
  local-first app, or should some pieces be cut? T.4a will have the real
  number.
- **Theme per profile?** This design keeps one theme for the whole app.
  A per-profile theme is possible; a profile switch would then restart
  the app.
- **The "Blender" name.** Blender is the Blender Foundation's trademark.
  Fine for a local build; before any release, a name like "Editor" may
  be safer. Release prep is parked.
- **The old copper look.** It retires with N.5 unless you want it kept
  as a fifth theme.

**What waits** (so you know what choosing a theme won't give you yet):
Workbench's icon-over-label dock tabs and Daylight's small-caps tool
tabs (tabbed docks); the Blender file's "Beyond OBS" layout and the
Console file's Live, Build and Create workspaces (workspaces, a layout
feature: choosing Blender gives Blender's look on OBS's layout); real
windows for Blender's Preferences and Properties; code pieces in theme
plugins. And one thing that never comes from a theme: the lower third's
font in the program picture (Daylight's serif) is scene content, set on
the text source.
