# Scenes as code and plugins (design)

Status: design, 2026-10-13. Covers PLAN sections 5 (5.1–5.7) and 7
(7.1–7.5). Slices 5.0 (compiled scenes), 5.1 (layout), 5.5a (data store,
content items, bound text), 5.2a (keyframe animations) and 7.1 are built
(PLAN log, and docs/archive/2026-10-22/notes.md for where they went beyond this text); the
rest is not yet. It turns Ryan's proposal in
[scene-language.md](scene-language.md) into one buildable design: the data
model, the method table, the engine changes, a performance budget with
numbers, failure modes, and an implementation plan in slices. The
decisions are listed at the end with the alternatives that were rejected,
so Ryan can overrule any of them. Its plan replaces the "Order" section of
scene-language.md (written when this was phase 4).

## Summary

- **Setups stay data.** Everything here is new, optional fields in the
  setup (TOML for people, JSON for the API, one JSON Schema). TSX is one
  way to write that data. Agents and the app keep patching plain data by
  id, and existing setups load and save unchanged.
- **One scene pipeline.** A new *scene thread* resolves the setup (plus
  live component output, bound data and source sizes) into an immutable
  **compiled scene**: every item placed, colours parsed, a flat draw list,
  GPU draw records uploaded once. The compositor adopts it with one atomic
  load per tick. For a static scene nothing is parsed, laid out, allocated
  or created on the GPU per tick, and a frame in which nothing changed is
  not drawn at all.
- **Layout (5.1)** is CSS flexbox, grid, block and absolute positioning
  (`px`, `%`, `auto`, `fr`, inset, aspect ratio, min/max sizes) through
  `taffy` 0.10, which is already in the lock file through Slint. It runs
  when something changes, never per frame. `rect` keeps working and means
  "absolute, in pixels".
- **Animation (5.2)** is keyframe sets (`@keyframes`), CSS `animation` on
  items on the session clock, CSS `transition` of changes (including layout
  moves, FLIP-style), enter and exit animations, keyframe scene transitions
  and per-pair transition overrides. It is evaluated per tick in a tiny CPU
  pass over animated properties only (≤ 0.2 µs each), rewriting only the
  draw records that changed.
- **WGSL (5.3)** shaders work as filters on sources, filters on items (the
  item and its children, like CSS `filter`), scene transitions and
  generated content, against a small fixed contract ("shader ABI 1") with
  named, typed params read from the shader by naga. They are validated at
  `plan` with file:line:col errors, compiled off the hot path, loop-guarded
  and GPU-time-budgeted.
- **TSX (5.4)**: `isocast plan|apply scenes.tsx`, `isocast scene
  build|watch|types`. The client sends the files; a sandboxed **scene
  host** process (oxc to strip TypeScript and JSX, QuickJS-ng through
  rquickjs to run it) returns setup data. Static components expand at
  build time. No JavaScript runs per frame, ever.
- **Live data (5.5)** is a data store in the engine (`data_get`,
  `data_set`, `data_delete`; CLI, MCP and `data_changed` events), text
  bound to keys, and **live components**: TSX functions of their props and
  data, re-rendered in the scene host when data they read changes. Their
  output is merged by id into the scene (with enter, exit and layout
  animations) and is never written into the setup.
- **External data (5.6)** comes from **feeds**: long-running script
  functions that fetch and open WebSockets for the origins listed in
  `[permissions] network`, and write data keys. Components only read data.
  Worked example: Twitch chat, read anonymously.
- **Plugins (7)** are folders with an `isocast-plugin.toml`, installed with
  `isocast plugin add`. Three tiers: external processes on the method table
  (any language; sandboxed, scoped by the connection's process), WGSL
  shaders (filters, transitions, content), and WebAssembly components on
  wasmtime (audio filters inline in the mixer under hard deadlines, custom
  sources). Plugin kinds appear in setups as `plugin/kind` with flat params
  checked against the manifest, and in `isocast setup schema`. A missing
  plugin is bypassed with a warning; it never breaks or rewrites a setup.
  Plugin node types (5.7) come from the same manifests.
- **Budget**: on a base M2, compositor CPU per tick ≤ 100 µs + 3 µs per
  draw for a static scene, 0 objects created per tick by Isocast code,
  ≤ 0.2 µs per animated property, change to screen within 2 ticks. Every
  slice proves its numbers on the test Macs.
- **Plan**: 16 slices in two tracks (scenes 5.x, plugins 7.x) that can run
  side by side. The first, 5.0, is a compositor refactor with no new
  features that makes the budget possible and is worth having on its own.

## Rules this design keeps

- **One method table.** Every capability below is a method in
  `isocast_protocol::api`, a CLI command (with `--json` when it prints
  data) and, where an agent would use it, an MCP tool. The app gets
  nothing the others don't.
- **Ids, not names.** Keyframe sets, scripts, feeds, data keys, plugins,
  plugin kinds and rendered nodes all have stable ids, and patches and
  diffs address them by id.
- **Live applies.** Every new field takes effect at the next tick without
  reopening sources. Work that can't finish in a tick (text rasterising,
  shader compiling, TSX building) happens elsewhere, and the program shows
  the old state until the new one is ready, then switches in one frame.
- **Agent-friendly errors.** Every validation error names the field path
  and lists the valid options, including errors inside TSX (file, line,
  column) and WGSL (file, line, column, plus the setup path that uses it).
- **Never lose a recording or a setup.** Nothing in this design can stop a
  recording: failures in scripts, shaders, plugins or feeds degrade the
  program picture (and say so), never the files. ISO files don't go
  through any of it. Setups that name missing plugins or scripts load, run
  everything else, and save back unchanged.
- **Pay for what you use.** A setup with no layout, animation, scripts,
  bound data or plugins compiles to the same draw list as today, never
  starts the scene host and loads no plugin runtime.
- **Clean-room.** Nothing here comes from OBS code; shader conventions
  follow public ones (gl-transitions' uniforms, CSS specs).

## Where we start

What exists (PLAN 3.x, 4.x, V1): scene items with `rect`, crop, fit,
align, scale filter, blend mode, style (opacity, background, corner
radius, border, shadow, transform), children (groups), nested scenes,
show/hide transitions (fade, slide) and `locked`; 15 source kinds, 11
video filter kinds (GPU passes per source), 9 audio filter kinds (in the
mixer), 8 transition kinds including Move; `plan`/`apply` with an
id-keyed merge patch, revisions and events; a compositor
(`record/program.rs`) that draws the scene on air at every canvas tick,
converts it to 4:2:0 on the GPU and waits for the GPU before handing the
frame on.

The per-tick work today, from reading `crates/isocast-engine`:

| Where | Done every tick |
|---|---|
| `scene::layers_with` | Walks the setup's item tree; formats a `String` path per item (`item_path`); parses every colour from hex (`Color::rgba` builds two `Vec`s); finds frames by linear search. |
| `program.rs` `frames`, `visible`, `item_looks` | `Scene::video_sources` builds `Vec`s; each filtered source's `Vec<Filter>` is cloned under a mutex; show/hide looks go into a new `HashMap`. |
| `render.rs` `draw_mode` | Per layer: a `Vec<u8>` of uniforms, a new GPU buffer (`create_buffer_init`), two texture views and a bind group. |
| `filter.rs` `apply` | Per filtered source: `Vec`s of sizes and passes, a GPU buffer and bind group per pass, and its own queue submission. |
| `program.rs` `draw` | Blocks on the GPU (`poll(Wait)`) before handing the frame to the outputs. |

None of this hurts at today's scale (a handful of items; recordings are
clean), but all of it grows with every item, style and animation, which
is exactly what scenes as code adds. Slice 5.0 removes it from the tick
before anything else lands. The GPU wait and the filter submissions are
S.3's to tackle (see "Fit with P and S").

## Architecture

```text
 scenes.tsx ─┐                      ┌──────────── isocastd ────────────────────────────┐
 setup.toml ─┼─ plan/apply ────────▶│ setup (revisioned) ── validate, save              │
 agent patch ┘   (client sends       │      │                                            │
                  script sources)    │      │  build ◀──────▶ scene host (child process,  │
                                     │      │                  sandboxed: oxc, QuickJS)   │
 data set ───────────────────────────▶ data store ──changed keys──▶ host re-renders     │
                                     │      │                       live components    │
                                     │      │         render_nodes ◀──┘   feeds ─▶ data │
                                     │      ▼                                            │
                                     │ scene thread: layout (taffy), merge rendered nodes │
                                     │   by id, change detection, content jobs           │
                                     │      │ Arc<CompiledScenes> + version              │
                                     │      ▼                                            │
                                     │ compositor tick: 1 atomic load; animate (tiny CPU │
                                     │   pass); write dirty draw records; import frames; │
                                     │   encode draws (cached bind groups); submit       │
                                     │      ▼                                            │
                                     │ program · preview · snapshots · shared frames    │
                                     │                                                   │
                                     │ plugin registry: WGSL → compositor pipelines;     │
                                     │   wasm → mixer and source threads;                │
                                     │   processes → supervised clients of the socket    │
                                     └───────────────────────────────────────────────────┘
```

- **The daemon** keeps its role: one revisioned setup, `plan` and `apply`,
  saving, events. It gains the data store, the plugin registry and the
  scene host's supervisor.
- **The scene thread** (new, in the engine, next to the source
  reconciler) owns resolving: it takes the setup's scenes, rendered nodes,
  bound data, source frame sizes and finished content jobs, and publishes
  `Arc<CompiledScenes>` with a version number. It never touches a frame.
- **Content workers** rasterise text, decode images and build shader
  pipelines, and upload the results as textures on the compositor's GPU
  device.
- **The compositor** (the existing program thread) adopts a new compiled
  scene when the version changes and otherwise only animates, imports
  source frames and encodes draws.
- **The scene host** is a separate, sandboxed child process, started only
  while the setup has scripts or a TSX change is being built.
- **Plugin runtimes**: WGSL pipelines live in the compositor; wasm
  instances in the mixer (audio filters) and on their own threads
  (sources); process plugins are supervised clients of the socket.

## Performance budget

Measured on the slowest Mac we support (base M2: isocast-m2-a/-b),
release builds, a 1080p60 canvas unless noted, isolated engines, holding
the benchmark lock. The reference scene ("scenes" benchmark): 2 live
sources, 40 items of which 10 are text, 10 animated properties, one item
filter, and a chat component fed 20 messages a second.

| What | Budget | Runs on | Measured by |
|---|---|---|---|
| Compositor CPU per tick, static scene | ≤ 100 µs + 3 µs per draw (p50); p99 ≤ 2× p50 | compositor | new `engine.compositor.cpu_us` (p50, p99) |
| Objects Isocast code creates per tick, static scene (strings, `Vec`s, maps, GPU buffers, views, bind groups, pipelines) | 0 (wgpu's own command recording is the only allocation left) | compositor | allocation counter build (`--features alloc-count`) |
| Each animated property | ≤ 0.2 µs CPU plus rewriting its 272-byte draw record | compositor | `cpu_us` with 0, 100 and 1,000 animated properties |
| A tick where nothing changed (no new source frame, nothing animating, no change) | nothing drawn: the last program frame is sent again | compositor | `engine.compositor.reused` |
| Resolve (layout, merge, flatten) | ≤ 100 µs for 50 items, ≤ 1 ms for 1,000 | scene thread | `engine.scenes.resolve_us` |
| A change reaching the screen (apply, data, render) | ≤ 2 ticks after the engine has it, plus text rasterising or component rendering | | frame `generation` against the change's time |
| Text rasterising | ≤ 2 ms per 1080p-wide line | content worker | `engine.scenes.raster_us` |
| Live component render | typically ≤ 1 ms; interrupted at 50 ms | scene host | host status |
| Scene host while idle | 0% CPU (no timers unless a feed sets them) | scene host | `ps` |
| `data_set` | ≤ 20 µs plus JSON parsing; `data_changed` events coalesced to ≤ 20 a second | daemon | status counters |
| Item filters and shaders | their GPU time, reported per filter; plugin shaders ≤ 2 ms GPU per frame by default | GPU | wgpu timestamp queries |
| Wasm audio filter | ≤ 1 ms per 10 ms block; bypassed past a 3 ms deadline | mixer | per-filter counters |
| Content textures | ≤ 256 MB by default, least recently shown off-air content dropped first | GPU memory | `engine.scenes.content_mb` |

The gate for every slice besides its own numbers: the flagship benchmark
(`scripts/bench.py`, flagship and flagship-1080) on isocast-m2-a, main
against the branch, 3 runs each: no more dropped frames and no more
daemon CPU than main.

Why these numbers: a 60 fps tick is 16.7 ms, most of which belongs to
capture, the GPU and the encoders (the base M2's media engine is the
flagship's limit, see notes). 100 µs is 0.6% of a tick; a 40-draw scene at
≤ 220 µs costs about 1.3% of one core at 60 fps. The scene host and
feeds run outside the engine process entirely.

## 5.0 Compiled scenes

The foundation for everything else, and useful alone (a faster
compositor with the same pictures).

**Resolve** (scene thread, when anything changes):

1. Give every item a `NodeId` (a `u32` interned from its item path, which
   stays stable across changes so animations and transitions can follow an
   item).
2. Lay out (5.1), merge rendered component nodes (5.5), bind data.
3. Parse colours, radii, borders and shadows into floats; work out every
   item's affine transform and opacity down the tree.
4. Flatten into a **draw list** per scene: each entry is a node id, a
   pipeline (mode × blend, the 21 made up front today), the index of its
   **draw record** and what it samples (a source slot, a content texture,
   a nested scene canvas, or nothing).
5. Write the draw records (today's 17-`vec4` `Draw` struct) into one
   array, and note which records any animation or transition can change.

**Adopt** (compositor, at the first tick after the version changes):
upload the draw record array into one GPU storage buffer (one
`write_buffer`, ≤ 64 KB for 240 draws, ~10 µs), and make bind groups for
new content textures. The vertex and fragment shaders read
`draws[instance_index]`, so no per-draw uniform buffer exists any more.

**Each tick**:

1. Load the version (one atomic); adopt if it changed.
2. Evaluate running animations and transitions (5.2) into the CPU copy of
   their draw records, and write the changed ranges.
3. For each source the draw list samples, take its newest frame. If no
   source has a new frame, nothing animates and nothing changed, send the
   last program frame again and stop here: a static desktop, a "starting
   soon" card or a paused slide costs nothing.
4. Import the frames. Bind groups are cached by texture: content and
   canvases keep theirs; captured frames get theirs from a small cache
   keyed by IOSurface id, which wraps each surface as a texture once, as
   shared frames already do (capture pools recycle a fixed set of
   surfaces, so after the first second every lookup hits).
5. Encode the draw list (set the pipeline only when it changes, one bind
   group per texture change, `draw(0..4, record..record + 1)`), submit.

Every buffer the tick uses (`Vec`s of frames, dirty ranges) is kept
between ticks and cleared, never rebuilt. Snapshots, studio mode's
preview and shared frames use the same compiled scenes, so they always
match the program.

## 5.1 Layout

Items and scenes gain `layout`, CSS's box model in setup terms. An item
gives either `rect` (as today: absolute, in pixels, relative to its
parent) or `layout`, not both.

```toml
[[scenes]]
id = "panel"
layout = { display = "flex", direction = "row", gap = 24, padding = 48, align_items = "center" }

[[scenes.items]]
id = "facecam"
source = "facecam"
fit = "cover"
layout = { flex_grow = 1, flex_basis = 0, aspect_ratio = 1.7778 }

[[scenes.items]]
id = "sidebar"
layout = { width = "30%", height = "100%", display = "flex", direction = "column", gap = 12 }
style = { background = "#000000aa", radius = 16 }
```

**Fields** (all optional; CSS names in snake case):

- The item in its parent: `position` (`relative`, in the parent's flow,
  the default inside a flex or grid parent; or `absolute`), `left`,
  `top`, `right`, `bottom`, `inset`, `width`, `height`, `min_width`,
  `min_height`, `max_width`, `max_height`, `aspect_ratio`, `margin`,
  `flex_grow`, `flex_shrink`, `flex_basis`, `align_self`, `justify_self`,
  `grid_row`, `grid_column` (`"1 / 3"`, `"span 2"`).
- Its children: `display` (`block`, the default, `flex` or `grid`),
  `direction` (`row`, `column`, `row_reverse`, `column_reverse`), `wrap`,
  `gap` (one length or `[row, column]`), `padding`, `justify_content`,
  `align_items`, `align_content`, `grid_template_columns`,
  `grid_template_rows` (`"1fr 2fr 200px"`, `"repeat(3, 1fr)"`),
  `grid_auto_flow`, `overflow` (`visible` or `hidden`, which clips the
  children to the item's rounded rect).
- Lengths: a number is pixels; `"50%"` is of the parent's content box (CSS
  rules); `"auto"`; `fr` only in grid templates. `"12px"` is accepted.
  No `em`, `rem` or `vw`: there is no document font, and TSX can compute
  them.
- Sides (`margin`, `padding`): one length, `[vertical, horizontal]`, or
  `[top, right, bottom, left]`, as in CSS.

**Semantics.**

- A scene is the root container, the size of the canvas.
- `rect = { x, y, width, height }` is exactly `layout = { position =
  "absolute", left = x, top = y, width, height }`, so old and new items mix
  freely and resolve to the same rects.
- `visible = false` takes an item out of layout (CSS `display: none`), as
  hiding does in OBS. An item leaving with an exit animation leaves the
  flow at once and finishes its animation at its last rect, so its
  siblings can move up while it fades (with `animate_changes`, 5.2).
- `fit` and `align` keep their meaning (CSS `object-fit` and
  `object-position`) for the picture inside the box layout gives.
- **Intrinsic sizes** for `auto`: a source's current frame size (until
  its first frame, the size its settings name, else 16:9 of the box); an
  image's size; text measured by cosmic-text at the width layout offers
  (so text wraps at the width its box gets). A source whose frame size
  changes triggers a new resolve. (As built in 5.1: pictures are CSS
  replaced elements whose natural size is the picture as drawn, after
  filters, but whose least size is 0, as with `min-width: 0`, so they
  shrink to fit; `flex_grow` is CSS's longhand, so share a row by
  `flex_grow` with `flex_basis = 0`, CSS's `flex: 1`. Text measuring
  comes with content items in 5.5a; a text source's size is its
  picture's. See docs/archive/2026-10-22/notes.md.)
- `transform` stays a visual transform after layout (CSS's `transform`):
  it never moves siblings.
- `taffy` 0.10 does the work, with its default features and not its
  `parse` feature (which pulls in `cssparser`, MPL-2.0); grid track lists
  are parsed by our own small parser.

**Output.** Resolved rects are part of the compiled scene, and readable:
`get` with `resolved: true` (CLI `isocast get --resolved`, MCP `get_setup`
with `resolved`) returns every item's canvas rect and transform, including
rendered component nodes, with a `generation` number. Frames (H.264 and
shared) carry that generation beside the setup revision, so the app draws
selection boxes where the picture has them. (As built: H.264 revision
records grow a second u64; shared and JPEG frames carry it as
`layout_generation`, since a shared frame's `generation` already numbers
its ring.)

**Errors**, for example: `scenes[2].items[0].layout.width: "50 %" is not a
length; use a number of pixels, "50%", or "auto"`; `scenes[0].items[3]:
give rect or layout, not both`; `scenes[0].items[3].rect: an item needs a
rect, or a layout to place it`.

**Cost.** Only on change, on the scene thread. Taffy's flexbox and grid
on a few dozen nodes take microseconds; the budget is ≤ 1 ms for 1,000
items on an M2. Text measuring is cached by (text, settings, width).

## Content items

Scenes as code wants text and images inline in a scene, without a
global source for every chat line. Items gain `content`, a picture made
by the engine from settings, used instead of `source` or `scene`:

```toml
[[scenes.items]]
id = "title"
content = { kind = "text", text = "Building Isocast", font = "Inter", size = 48, weight = 600 }
layout = { position = "absolute", left = 64, bottom = 64 }
```

- Kinds: `text` (the text source's settings: `text`, `file`, or new
  `data`), `image` (the image source's settings), `shader` (5.3), and
  plugin node kinds (5.7). The settings structs are the source kinds'
  own, so validation and the schema are shared.
- Content is not a source: it has no ISO file, no audio, opens nothing,
  and costs nothing per tick. A content worker rasterises it when its
  settings change and uploads a texture; textures are cached by content
  hash and shared by every item that shows the same content.
- **Bound text.** The text source gains `data = "key"` as a third
  alternative to `text` and `file`: the text is the data key's value
  (strings as they are, numbers formatted, anything else as JSON). This
  works for global text sources too, so an OBS-style text source can be
  fed by `isocast data set` with no scripts at all.
- A change shows when its texture is ready; the scene thread publishes a
  compiled scene only once the content it needs is ready (waiting at most
  50 ms, then publishing with the old content and swapping it in later),
  so one apply never shows its new layout with old text.

As built (5.5a; docs/archive/2026-10-22/notes.md has the details):

- The scene thread draws content's pictures itself while it resolves
  (text ≈0.5–1 ms, images once per file), keyed by everything that
  decides them (settings, the text shown, the box it wraps in, an image
  file's time) and shared by items showing the same; it publishes scenes
  only with their pictures, so there is no 50 ms timeout yet (a worker
  pool with it can come with heavier content, 5.3b). The compositor makes
  a texture of each picture when it adopts the scenes (RGBA, sampled as a
  captured BGRA frame is, so content and its equivalent source draw
  identically) and keeps it while scenes show it; nothing per tick.
- Text placed by `layout` is measured by cosmic-text (its widest word is
  its least width) and drawn the size of its box, wrapped to fit (so it
  is never stretched); text placed by a `rect` is drawn as its source
  would be and fitted into the rect like one. An animated GIF shows its
  first frame. Content items can be cropped, fitted and aligned.
- Bound content resolves again when its key changes, at most once a
  canvas frame however often it changes; a text file content reads is
  looked at twice a second. Global text sources with `data` draw a new
  frame on change (at most once a frame) without reopening; their size
  follows the text unless `width`/`height` are set (an ISO file of one
  keeps its first size and scales).

## 5.2 Animation

### Keyframes and animations

Keyframe sets are top-level, by id:

```toml
[[keyframes]]
id = "slide-in"
frames = [
  { at = 0.0, opacity = 0.0, translate = [-40, 0] },
  { at = 1.0, opacity = 1.0, translate = [0, 0] },
]
```

Items play them with `animations` (CSS `animation`):

```toml
[[scenes.items]]
id = "title"
animations = [{ keyframes = "slide-in", duration_ms = 400, easing = { cubic_bezier = [0.0, 0.0, 0.58, 1.0] } }]
```

- **Animation fields:** `keyframes` (an id), `duration_ms`, `delay_ms`
  (negative starts part-way, as in CSS), `easing`, `iterations` (a number
  or `"infinite"`), `direction` (`normal`, `reverse`, `alternate`,
  `alternate_reverse`), `fill` (`both`, the default, `forwards`,
  `backwards`, `none`), `start` (below).
- **Animatable properties** are the ones the compositor can change without
  laying out or rasterising: `opacity`, `translate` (pixels or `%` of the
  item's own size), `scale`, `rotate`, `background`, `border_color`,
  `border_width`, `radius`, `shadow`, `crop`, and numeric params of the
  item's filters (`params = { "glow.amount" = 0.8 }`). Each stop may set
  `easing` for the stretch to the next stop.
- **Composition:** animated `translate`, `scale` and `rotate` compose with
  the item's own `style.transform` (like CSS's individual transform
  properties); animated opacity multiplies; the other properties replace.
  Later animations win over earlier ones on the same property.
- **Clock (`start`).** Every animation runs on the session clock, sampled
  at each canvas tick:
  - `on_air` (default): from when the item's scene starts being drawn by
    the program or by studio mode's preview (each keeps its own start; a
    nested scene starts with the scene showing it), restarting each time,
    like media's restart when shown.
  - `shown`: from the tick an item first appears or becomes visible
    through a change.
  - `clock`: from the engine's epoch, so loops never restart and stay in
    step across scene switches and across files.

### Easing

The four existing names (`linear`, `ease_in`, `ease_out`, `ease_in_out`)
keep their current cubic formulas, so existing transitions record the
same frames. New forms: `{ cubic_bezier = [x1, y1, x2, y2] }` and
`{ steps = n, position = "end" }` (`start`, `end`, `both`, `none`). The
TSX compiler maps CSS's keywords (`ease`, `ease-in`, `ease-out`,
`ease-in-out`) to CSS's exact cubic-béziers, so TSX authored with CSS
names looks as it would in a browser.

### Animated changes (CSS transitions)

```toml
animate_changes = { duration_ms = 250, easing = "ease_out", properties = ["layout", "opacity"] }
```

When a change (an apply, a data change, a component render, a layout
change caused by something else) alters one of the listed properties of an
item, the item animates from what is on screen at that tick to the new
value. A change mid-way retargets from the current value, so nothing
jumps. `layout` animates the resolved rect: the compositor interpolates
the old and new rects (FLIP), so layout itself never runs per frame.
Children of a box that moves move with it; children whose own rect
changes need their own `animate_changes`. Properties: `layout`, `opacity`,
`transform`, `background`, `border`, `radius`, `shadow`, `crop`,
`params`; all of them if `properties` is absent.

### Enter and exit

`enter` and `exit` are animations played when an item appears or
disappears: added or removed by an apply or a component render, or made
visible or hidden. An exiting item keeps drawing (and its source keeps
delivering) until its exit ends, as hiding items do today. The existing
`show_transition`/`hide_transition` (fade, slide) stay, for OBS parity and
the import, and become built-in keyframe sets with the same values:
`fade-in`, `fade-out`, `slide-in-<side>` and `slide-out-<side>` (`left`,
`right`, `up`, `down`), which setups can't reuse as ids. An item may set
one of `show_transition` and `enter`, and one of `hide_transition` and
`exit`.

### Scene transitions

- New transition kind `keyframes`: keyframe sets played on the outgoing
  and incoming scenes' canvases, `{ kind = "keyframes", exit =
  "slide-out-left", enter = "slide-in-right", duration_ms = 600 }`.
- `move` (Magic Move) keeps matching items by key; unmatched items play
  their own `enter`/`exit` if they have them, else fade as now.
- Per-pair overrides: `scenes[].transition_from` maps a scene id to the
  transition used when switching from that scene. Precedence: the switch's
  own > `transition_from.<from>` > the scene's `transition` > the setup's.

```toml
[[scenes]]
id = "closeup"
transition_from = { desktop = { kind = "move", duration_ms = 600, easing = { cubic_bezier = [0.2, 0.8, 0.2, 1.0] } } }
```

### Evaluation

The compiled scene lists the draw records that have a running animation or
transition, each with its tracks (stops as floats, easing, timing). At
each tick the compositor walks only that list: for each, the elapsed time
from its start (ticks are exact `Time` values, so the result is
deterministic), the eased value, composed into the record, the record
marked dirty. A parent whose transform animates rewrites its descendants'
records (one affine multiply each). Finished animations drop off the list.
With nothing animating the pass is one empty loop. Every program frame is
a pure function of the compiled scene and the tick time, so a program
could later be re-rendered from the ISO files, the setup and the
manifest (and a data log, see "Later").

As built (5.2a; docs/archive/2026-10-22/notes.md has the details):

- Keyframe sets travel with the scene collection. A property no stop at 0
  or 1 sets runs from or to the value underneath it (the item's own, or
  an earlier animation's), per property, as in CSS; the animation's
  `easing` is each stretch's unless a stop gives its own (CSS's
  `animation-timing-function`), and runs backwards with `reverse`.
  `easing` defaults to `ease_in_out`, Isocast's default for transitions
  (TSX writes CSS's béziers out). `iterations` may end part of the way
  through a run (`2.5`). Animated translate, rotate and scale go around
  the item's own `style.transform` (place = translate(centre + t) ·
  rotate · scale · own); colours interpolate premultiplied. `scale` may
  reach 0 in keyframes (a pop-in), unlike `style.transform.scale`.
  Not yet: `params` (item filters come with 5.3a; a source's filters
  also draw its filtered ISO file, so items don't animate them). The
  ids of 5.2b's built-in sets (`fade-in`, `slide-in-left`, …) are kept
  now, so setups written meanwhile keep loading.
- Clocks: the program notes when each scene it shows comes on air at
  every tick, drawn or not; studio mode's preview from its first frame
  after it is staged (or wanted again); a scene drawn by both is
  evaluated before each draw with its own start. `shown` starts when an
  item and the items it is inside become visible (or appear) in an
  adopted change, and keeps its time while hiding. `clock` is the
  engine's epoch. Animations are evaluated at the tick's time (the
  frame's pts), so a run of N ms spans exactly N ms of frames.
- Items animating background or border get a fill draw, and those
  animating a shadow a shadow draw as big as it gets, at compile; items
  inside a moving or fading one are rewritten with it, and clips move
  with their item. A record that comes out the same isn't rewritten, so
  a finished animation lets frames be sent again. Changed records go to
  the GPU through a staging belt in the frame's command encoder (no
  buffer made per frame). Move transitions draw animated items as their
  animations have them. Snapshots show animations at the start of their
  clocks (what the audience first sees); `get --resolved` gives rects as
  laid out. `engine.compositor.animated` counts the properties evaluated
  for the last frame.

## 5.3 Shader effects and transitions (WGSL)

### Where shaders run

| Use | Setup | Runs |
|---|---|---|
| Source filter | `sources[].filters`: `{ kind = "shader", file, params }` | in the source's filter chain, wherever it is shown; also in filtered ISO files |
| Item filter | `scenes[].items[].filters` (new): same `Filter` type | on the item as drawn, with its children (CSS `filter`): drawn into an offscreen texture in the item's own space, filtered, then drawn with the item's transform, opacity and blend |
| Transition | `{ kind = "shader", file, params, easing, duration_ms }` | one pass over both scenes' canvases, like the built-in kinds |
| Content | `content = { kind = "shader", file, params }` | draws the item's picture; drawn once and cached if it doesn't read time or frame |

Item filters use the same kinds as source filters, so built-in filters
(colour correction, LUT, sharpen, keys) work on items too; `render_delay`
stays source-only.

### The contract (shader ABI 1)

You write one function; Isocast adds a prelude and the entry points. For
a filter:

```wgsl
// Added by Isocast before your code:
struct Globals {
    size: vec2<f32>,   // output size in pixels
    texel: vec2<f32>,  // 1 / input size
    time: f32,         // seconds since this filter started showing
    frame: u32,        // canvas ticks since then
    progress: f32,     // transitions: 0 to 1, eased
    pad: f32,
}
@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var input: texture_2d<f32>;  // premultiplied RGBA
@group(0) @binding(2) var input_sampler: sampler;  // bilinear, clamped to the edges
fn sample_input(uv: vec2<f32>) -> vec4<f32> {
    return textureSampleLevel(input, input_sampler, uv, 0.0);
}

// You write (Params is optional):
struct Params {
    amount: f32,
    tint: vec4<f32>,
}
@group(1) @binding(0) var<uniform> params: Params;

fn effect(uv: vec2<f32>) -> vec4<f32> {
    let c = sample_input(uv);
    return mix(c, c * params.tint, params.amount);
}
```

- Transitions get `before` and `after` textures with `sample_before(uv)`
  and `sample_after(uv)`, and you write `fn transition(uv: vec2<f32>) ->
  vec4<f32>` using `globals.progress` (the same idea as gl-transitions'
  `getFromColor`/`getToColor`/`progress`, so those port by renaming).
- Content gets no input; you write `fn content(uv: vec2<f32>) ->
  vec4<f32>`.
- Results are premultiplied RGBA; Isocast clamps them.
- A filter may grow its picture (glows) with `margin` (pixels) on the
  filter.

### Params

Params are read from the shader: naga parses the module, and the fields of
`struct Params` (`f32`, `i32`, `u32`, `bool`, `vec2/3/4<f32>`) are the
params, laid out by WGSL's uniform rules. In the setup they are named and
type-checked (`params = { amount = 0.3, tint = "#ff8800" }`; colours
become `vec4<f32>`); an unknown name lists the struct's fields, a wrong
type names the expected one, missing ones are zero. Plugins add ranges,
defaults and docs to the same names in their manifest. Numeric params can
be animated (`params = { "glow.amount" = 0.8 }` in a keyframe).

### Validation, compiling, safety

- `plan` parses and validates the shader with naga (the version wgpu 30
  uses, already in the lock file) and reports errors as `scenes[0].items[2]
  .filters[0].file: shaders/wobble.wgsl:12:5: unknown identifier
  'colr'`.
- **Files are read when the change is planned.** The client sends the
  shader and script files a change names along with it (`files`, as for
  TSX in 5.4); the daemon reads any it wasn't sent itself, with the 3 s
  limit the OBS import uses, so a privacy prompt can't hang it. What runs
  is the copy read at apply, until the next apply: editing a shader file
  changes nothing until it is applied again (`isocast scene watch` does
  that on save). After a restart the daemon reads the files again; one it
  can't read is bypassed with a warning, like a missing LUT.
- Pipelines compile on a content worker after apply. Until one is ready
  its filter is skipped (or, for a transition, the switch waits for it,
  up to 1 s, like a loading stinger). Every shader the setup names is
  compiled at apply, so a switch never waits in practice.
- **Loop guard.** Before compiling, Isocast rewrites the naga module so
  every loop counts its iterations and breaks after 4,096 per entry (WGSL
  has no recursion, so this bounds every shader). The module goes to wgpu
  as naga IR (wgpu's `naga-ir` feature). naga's bounds checks stay on, so
  shaders can't read outside their textures and buffers.
- **GPU budget.** Each shader pass is timed with timestamp queries. A
  shader over its budget for 30 frames in a row is switched off with a
  warning naming its setup path and its measured time. A plugin shader's
  budget is 2 ms per frame at 60 fps by default; a shader from the setup
  itself is only switched off when it makes the program miss ticks, and
  otherwise gets a warning. (The loop guard bounds a runaway loop, but
  4,096 iterations per pixel can still take longer than a frame; the
  budget is what ends it.)
- A GPU error during a frame (device lost) restarts the compositor's GPU
  (as a failed engine start is handled today) without the shader that was
  running; the program drops the frames in between (counted), ISO files
  don't notice.

## 5.4 TSX authoring

### What it looks like

```tsx
// scenes.tsx
import { Scene, Video, Text, Box, keyframes } from "isocast";

const slideIn = keyframes("slide-in", {
  from: { opacity: 0, translate: [-40, 0] },
  to: { opacity: 1, translate: [0, 0] },
});

const Facecam = (p: { id: string; style?: object }) => (
  <Video id={p.id} source="facecam" crop={{ left: 120 }}
         style={{ borderRadius: 24, boxShadow: "0 8px 32px #0008", objectFit: "cover", ...p.style }} />
);

export const desktop = (
  <Scene id="desktop" style={{ background: "#111" }}>
    <Video id="screen" source="screen" style={{ position: "absolute", inset: 0 }} />
    <Facecam id="facecam" style={{ position: "absolute", right: 48, bottom: 48, width: 640, aspectRatio: 16 / 9 }} />
    <Text id="title" style={{ font: "600 48px Inter", color: "white", position: "absolute", left: 64, bottom: 64,
                              animation: `${slideIn} 400ms ease-out` }}>
      Building Isocast
    </Text>
  </Scene>
);

export const closeup = (
  <Scene id="closeup">
    <Facecam id="facecam" style={{ position: "absolute", inset: 0, borderRadius: 0 }} />
  </Scene>
);

export const transitions = {
  "desktop -> closeup": { kind: "move", duration: 600, easing: "cubic-bezier(.2,.8,.2,1)" },
};
```

`isocast apply scenes.tsx --revision N` replaces the scenes `desktop` and
`closeup` and the keyframe set `slide-in`, and sets
`scenes[closeup].transition_from.desktop`. The result is ordinary setup
data; `isocast scene build scenes.tsx` prints it as TOML (or JSON with
`--json`).

### How TSX becomes setup data

1. **The client collects the files.** The CLI (or MCP server, or app)
   reads the entry file and its relative imports (`./`, `../`), and sends
   them with the change: `Change::Script { entry, files }`. Images and
   shaders the script names by relative path are made absolute against the
   entry's folder, as `Setup::resolve_paths` does for setup files (and
   shader files are sent along, 5.3). The daemon never reads script files,
   so it never waits on a privacy prompt for `~/Documents`; `isocast apply
   setup.toml` sends the scripts a hand-written setup names in the same
   way.
2. **The daemon asks the scene host to build**, before taking its state
   lock (a build can take tens of milliseconds; status, switches and
   frames must not wait for it).
3. **The scene host compiles each file** with oxc: TypeScript stripped,
   JSX turned into calls to Isocast's JSX runtime in development mode (so
   every element knows its file, line and column), ES modules kept. Source
   maps let every error point at the TSX. Imports resolve only to the sent
   files, `isocast`, and `plugin:<id>` (plugin node components); there is
   no npm or `node_modules`.
4. **It runs the module** in a fresh QuickJS context with a build context,
   and collects the exports: `<Scene>` elements (or arrays of them),
   keyframe sets, feeds, `transitions`, `permissions`, and optionally
   `sources`.
5. **It expands components.** A component that doesn't read data or keep
   state is called at build time and its output inlined, so it leaves no
   trace in the setup. One that calls `useData` or `useState` becomes a
   `component` item (5.5); it must be exported (the host finds it by name
   at run time) and its props must be JSON.
6. **It maps elements and CSS** to setup fields (tables below), checking
   every property; an unsupported property or value is an error at its
   TSX position listing what is supported.
7. **The daemon replaces by id** what the module defines: each scene,
   keyframe set, script, feed and (if exported) source with the module's
   id replaces the setup's in place, new ones are added at the end, and
   nothing else changes. The resulting setup is planned and applied like
   any other: same diff, same problems, same revision check.
8. **The compiled modules are kept** by content hash in the daemon's
   support folder; `[[scripts]]` records the file and the hash. Live
   components and feeds run that compiled copy until the next apply, so
   saving the file mid-recording changes nothing until it is applied.

Scenes made by a script carry `origin = { script, hash }`. A patch to such
a scene gets a plan note ("scene desktop comes from scenes.tsx; the next
apply of scenes.tsx replaces it"), so app edits are never lost silently:
`isocast plan scenes.tsx` shows exactly what applying it would undo.

### Elements

| TSX | Setup data |
|---|---|
| `<Scene id style transition>` | a scene; `style.background` → `background`, layout props → `layout` |
| `<Video id source crop>` | an item with `source` (any video source) |
| `<Nested id scene>` | an item with `scene` |
| `<Box id>` | an item without a picture: a box, or a group of its children |
| `<Text id>…</Text>` | an item with `content = { kind = "text", … }` (children are the text) |
| `<Image id src>` | an item with `content = { kind = "image", path }` |
| `<Shader id file params>` | an item with `content = { kind = "shader", … }` |
| `<Node kind="plugin/node" …props>` | an item with plugin content (5.7); `plugin:<id>` imports give typed components for these |
| your component | inlined when static; a `component` item when live |
| `keyframes(id, stops)` | a `[[keyframes]]` set; its value stringifies to its id for CSS `animation` strings |
| `feed({ id, writes, props }, fn)` | a `[[feeds]]` entry (5.6) |
| `export const transitions` | `transition_from` on the target scenes |
| `export const permissions` | `[permissions]` |
| `export const sources` | sources, replaced by id (optional: devices usually stay in TOML) |

Every element takes `id` (or React's `key`), `style`, `visible`,
`locked`, `filters`, `enter`, `exit` and `blend`.

### Style

| CSS (camelCase in TSX) | Setup |
|---|---|
| `position`, `left`, `top`, `right`, `bottom`, `inset`, `width`, `height`, `minWidth`…`maxHeight`, `aspectRatio`, `margin`, `padding`, `display`, `flexDirection`, `flexWrap`, `gap`, `rowGap`, `columnGap`, `justifyContent`, `alignItems`, `alignSelf`, `alignContent`, `flexGrow`, `flexShrink`, `flexBasis`, `gridTemplateColumns`, `gridTemplateRows`, `gridColumn`, `gridRow`, `overflow` | `layout.*` |
| `opacity`, `background`, `borderRadius`, `border` (`"2px solid #fff"`), `boxShadow` (`"0 8px 32px #0008"`, no spread, one shadow), `transform` (`translate()`, `scale()`, `rotate()`) | `style.*` |
| `objectFit`, `objectPosition` (nine positions), `mixBlendMode`, `imageRendering` | `fit`, `align`, `blend`, `scale_filter` |
| `animation` (`"slide-in 400ms ease-out 100ms infinite alternate both"`) | `animations` |
| `transition` (`"layout 200ms ease-out, opacity 150ms"`) | `animate_changes` |
| `font` (`"600 48px Inter"`), `fontFamily`, `fontSize`, `fontWeight`, `fontStyle`, `color`, `textAlign`, `lineHeight`, `textShadow`, `WebkitTextStroke` | the text content's settings |

Colours may be any CSS colour (`#rgb`…`#rrggbbaa`, `rgb()`, `rgba()`,
`hsl()`, named); the runtime turns them into `#rrggbbaa`. Sides and pairs
(`padding`, `margin`, `gap`, `translate`) take CSS strings (`"6px 10px"`)
or arrays (`[6, 10]`).

### Static and live components

Components are plain functions of props returning elements. The JSX
runtime is Isocast's own, about 400 lines of JavaScript embedded in the
host: `jsx`, `Fragment`, and the hooks `useData(key)`, `useState`,
`useMemo` and `useRef`. There is no virtual DOM diffing in JavaScript: a
render returns a plain node tree, and the engine merges it by id (5.5).
There is deliberately no `useEffect`, no timers and no
`requestAnimationFrame` in components: side effects belong to feeds, and
motion to keyframes. A ticking clock is a feed writing a key once a
second (or, later, a `timer` content kind), never per-frame JavaScript.

### The scene host

`isocast-scene-host` is a separate binary in the signed bundle, started by
isocastd as a child only while it is needed (a build, or a setup with
scripts), speaking JSON-RPC lines over its stdin and stdout.

- **Sandboxed.** It is spawned disclaiming responsibility (like isocastd
  itself), so it has none of Isocast's camera, microphone or screen
  grants, and under a Seatbelt profile: no file reads outside its own
  cache folder and system libraries, no writes, no process launching, and
  outbound network only when the setup lists origins (which the host then
  checks per connection; Seatbelt can't filter by host name).
- **Bounded.** One QuickJS runtime per script module, each with a memory
  limit (32 MB by default) and an interrupt handler that stops any render
  after 50 ms and any build after 5 s.
- **Supervised.** The daemon restarts it after a crash or a hang (no reply
  to a ping in 2 s), with backoff (1, 2, 4… up to 60 s); the scene keeps
  the last rendered nodes meanwhile, and `status` shows the host's state,
  restarts and last error.
- **Cheap.** Idle, it does nothing; one render of a small component is
  well under a millisecond.

### The dependencies, and why

- **oxc** (MIT, pure Rust) for TSX: one parser and transformer for
  TypeScript and JSX, fast, with source maps. It strips types and does not
  type-check; `isocast scene types` writes `isocast.d.ts` for editors and
  `tsc` to check against.
- **rquickjs** (MIT) with the QuickJS-ng it bundles: a small (~1 MB),
  fast-starting, standards-complete engine with memory limits and an
  interrupt handler, which is what a sandboxed host needs. Per-frame speed
  doesn't matter because nothing runs per frame.
- Both live only in the scene host binary: isocastd doesn't link a JS
  engine or a TypeScript compiler.

### CLI and MCP

- `isocast plan|apply FILE.tsx` (also `.ts`, `.jsx`, `.js`).
- `isocast scene build FILE.tsx [--json] [-o OUT]`: the setup data the
  module makes, without planning.
- `isocast scene watch FILE.tsx`: plans and applies on every save,
  printing problems; the dev loop, purely a client of plan and apply.
- `isocast scene types [-o isocast.d.ts]`: TypeScript declarations for the
  `isocast` module, this setup's source, scene and keyframe ids, data keys
  seen so far, and installed plugin nodes, so editors and agents complete
  and check against the real setup.
- MCP `plan_change` and `apply_change` take `script = { entry, files }`
  (or a path, which the MCP server reads).

## 5.5 Live data

### The data store

Runtime values that scenes show, kept in the engine, separate from the
setup.

- **Keys** are dotted ids (`score.home`, `twitch.chat`): lowercase
  letters, digits, dashes and underscores between dots, at most 128
  characters. **Values** are JSON, at most 64 KB each and 16 MB in all;
  errors name the key and the limit.
- **Methods:** `data_get { keys?, prefix? }` → `{ revision, values }`;
  `data_set { values: { key: value, … }, persist? }` (several keys at
  once, atomically) → `{ revision }`; `data_delete { keys }`. Event
  `data_changed { revision, keys }`, coalesced to at most 20 a second.
- **CLI:** `isocast data get [KEY…] [--prefix P] [--json]`, `isocast data
  set KEY VALUE [--persist]` (VALUE is JSON, or a bare string), `isocast
  data delete KEY…`. **MCP:** `get_data`, `set_data`.
- **Persistence:** keys set with `persist` are saved (debounced, atomic)
  to `<setup>.data.json`, beside the setup file like `.tune.json`, and
  come back after a restart; others live as long as the daemon.
- **Bound content** (text `data = "key"`) is updated by the scene thread
  directly: no scripts involved.

As built (5.5a): `data_get` also says which keys are kept (`persisted`)
and which asked for aren't set (`missing`); `data_set` and `data_delete`
return `{ revision, changed }` (setting a key to the value it has changes
nothing, and doesn't count). `persist` is three-way: `true` keeps,
`false` stops keeping, absent leaves a key as it was (new keys aren't
kept). A prefix covers whole parts (`score` is `score` and `score.home`,
not `scoreboard`). Kept keys belong to the scene collection
(`collections/<id>.data.json`, as "Fit with P and S" says, which is the
setup file of this section since P.1): a switch saves the old
collection's and loads the new one's, a deleted collection's go with it
to `deleted/`, a duplicate gets a copy; a file that can't be read is moved
aside, never overwritten. Revisions start at the time, like setup
revisions, so they never repeat across restarts. Plugins' `data_read` and
`data_write` grants hold their data calls to their prefixes. `status`
gains `data` (keys, bytes, kept, writes, events, `set_us`, file, save
error) and `engine.scenes` gains `raster_us`, `content` and `content_mb`.

### Live components

A `component` item is filled at run time by its script:

```toml
[[scenes.items]]
id = "chat"
component = { script = "overlays", export = "Chat", props = { max = 8 } }
layout = { position = "absolute", right = 48, top = 48, width = 480, height = 600 }
```

1. After an apply, the daemon tells the scene host which component
   instances exist (instance path, script hash, export, props). Instances
   that didn't change keep their state.
2. The host renders each one, recording which keys it read through
   `useData`, and subscribes to them; the daemon forwards `data_changed`
   for subscribed keys only, with their values.
3. When a key changes, the host re-renders the instances that read it,
   at most once per canvas tick each, and sends the result with
   `render_nodes { instance, generation, nodes }`.
4. The scene thread merges the nodes by id under the component item:
   new ids enter (their `enter` animation), missing ids exit (their `exit`
   animation, out of the flow), the rest take their new properties (with
   `animate_changes`), then re-resolves layout and publishes. Node keys
   are namespaced by the instance (`chat/9f2c…`), so they never collide
   with the scene's own items and Move transitions can match them across
   scenes.

Rendered nodes are plain items (boxes, content, references to existing
sources and scenes). They can't open sources or contain components (the
host expands nested components itself), and they are runtime state: not
in the setup, not in diffs, not saved. `get --resolved` shows them.
`render_nodes` is on the method table for the scene host and for process
plugins that render nodes (5.7); a debugging CLI command exists
(`isocast nodes set INSTANCE FILE.json`), but no MCP tool: agents change
what a component shows through data.

Component state (`useState`) lives in the host; a host restart loses it.
Anything that must survive belongs in the data store.

## 5.6 External data: feeds, fetch and WebSockets

**Feeds** are the only scene code with side effects: long-running
functions in the scene host that fetch, open WebSockets, and write data
keys. They run while the setup lists them, whether or not any scene is on
air (so chat history is ready when the overlay comes on).

```toml
[permissions]
network = ["wss://irc-ws.chat.twitch.tv"]

[[feeds]]
id = "twitch"
script = "overlays"
export = "twitch"
writes = ["twitch"]
props = { channel = "ryanvogel", keep = 50 }
```

- **Permissions.** `[permissions] network` lists origins (scheme, host,
  optional port; `*.example.com` allowed) that scene code may reach. The
  host checks every connection before making it; anything else fails with
  `network: wss://evil.example is not allowed; add its origin to
  [permissions] network`. A change that adds origins gets a plan note
  ("scene code gains network access to …"), so neither a person nor an
  agent adds one without seeing it.
- **Writes.** A feed may only set keys under its `writes` prefixes.
- **APIs.** `fetch` (method, headers, body; text and JSON responses up to
  4 MB; 4 at a time), `WebSocket` (open, message, close, error, `send`,
  `close`; 8 at a time), `setTimeout`/`setInterval` (50 ms minimum), and
  the feed's context: `props`, `data.set` (coalesced to one write per key
  per 50 ms), `data.get`, `log`, and `restart(reason)`. The networking is
  Rust inside the host (tungstenite for WebSockets, native-tls as the
  RTMPS client already uses), feeding QuickJS's event loop.
- **Lifecycle.** A feed that throws or calls `restart` is started again
  with backoff (1 s doubling to 60 s); its keys keep their last values.
  `status` lists each feed's state, last error and last write.

### Worked example: Twitch chat

Reading Twitch chat needs no account or key: an anonymous IRC connection
over WebSocket (nickname `justinfan` plus a number) is read-only.

```tsx
// overlays.tsx
import { feed, keyframes, useData, Box, Text } from "isocast";

type Line = { id: string; user: string; color: string; text: string };

export const permissions = { network: ["wss://irc-ws.chat.twitch.tv"] };

export const twitch = feed(
  { id: "twitch", writes: ["twitch"], props: { channel: "ryanvogel", keep: 50 } },
  ({ props, data, restart }) => {
    const lines: Line[] = [];
    const ws = new WebSocket("wss://irc-ws.chat.twitch.tv:443");
    ws.onopen = () => {
      ws.send("CAP REQ :twitch.tv/tags");
      ws.send(`NICK justinfan${10000 + Math.floor(Math.random() * 80000)}`);
      ws.send(`JOIN #${props.channel}`);
    };
    ws.onmessage = (event) => {
      for (const raw of String(event.data).split("\r\n")) {
        if (raw.startsWith("PING")) ws.send("PONG :tmi.twitch.tv");
        const m = /^@(\S+) :(\w+)!\S+ PRIVMSG #\w+ :(.*)$/.exec(raw);
        if (!m) continue;
        const tags = Object.fromEntries(m[1].split(";").map((t) => t.split("=")));
        lines.push({ id: tags["id"], user: tags["display-name"] || m[2], color: tags["color"] || "#a0a0ff", text: m[3] });
        if (lines.length > props.keep) lines.shift();
        data.set("twitch.chat", lines);
      }
    };
    ws.onclose = () => restart("chat connection closed");
  },
);

const pop = keyframes("chat-pop", {
  from: { opacity: 0, translate: [0, 24] },
  to: { opacity: 1, translate: [0, 0] },
});

export function Chat({ max = 8 }: { max?: number }) {
  const lines = useData<Line[]>("twitch.chat") ?? [];
  return (
    <Box style={{ display: "flex", flexDirection: "column", justifyContent: "flex-end", gap: 8,
                  height: "100%", overflow: "hidden", transition: "layout 200ms ease-out" }}>
      {lines.slice(-max).map((line) => (
        <Box id={line.id} enter={`${pop} 250ms ease-out`} exit="fade-out 200ms"
             style={{ background: "#000a", borderRadius: 10, padding: [6, 10] }}>
          <Text style={{ font: "600 22px Inter", color: line.color }}>{line.user}</Text>
          <Text style={{ font: "22px Inter", color: "white" }}>{line.text}</Text>
        </Box>
      ))}
    </Box>
  );
}
```

In `scenes.tsx`, `<Chat id="chat" max={8} style={{ position: "absolute",
right: 48, top: 48, width: 480, height: 600 }} />` becomes the
`component` item shown in 5.5, and the module adds the `[permissions]`,
`[[scripts]]` and `[[feeds]]` entries above. Each message is one
`data.set`, at most 20 renders a second, each a few hundred microseconds
in the host; new lines pop in, old ones fade out, the rest slide up with
the layout transition, and the compositor's per-tick work is the same
quads with a few animated records.

## 5.7 Plugin nodes

Plugins can declare node types: a props schema plus how they are drawn.
In setups they are content kinds named `plugin/node`, with flat props:

```toml
[[scenes.items]]
id = "chat"
content = { kind = "twitch-chat/chat-box", channel = "ryanvogel", max = 8 }
layout = { position = "absolute", right = 48, top = 48, width = 480, height = 600 }
```

Renderers, from cheapest to heaviest:

- **shader**: a WGSL `content` function; props are its params.
- **component**: a TSX component in the plugin, run by the scene host like
  a live component (props from the item, data through `useData`).
- **page**: an HTML page in the plugin, shown by an engine-owned browser
  source (WebKit, as 4.7) at the item's size, with props and subscribed
  data passed into the page (`window.isocast`). Heavy (a WebKit page each),
  so `status` shows its cost.
- **process**: the plugin's own process draws, either nodes (through
  `render_nodes`) or pixels: the daemon gives it a ring of IOSurfaces
  (shared frames, 4.x/N.1, in reverse) that it draws into with anything
  (Metal, Core Graphics) and hands back slot by slot; the engine samples
  them in place, no copies.

Node types are listed in `isocast setup schema` with their props' JSON
Schema and docs, so an agent can find and use them from the schema alone.

## 7. Plugins

### Tiers

| Tier | Provides | Runs | Isolation | Cost when unused |
|---|---|---|---|---|
| External process (7.2) | anything on the method table: controllers, bots, data, rendered nodes and frames | its own process, any language, a client of the socket | separate process, Seatbelt, no TCC grants, methods and data scoped by its process | none |
| WGSL (7.3) | video filters, transitions, content nodes | the compositor's GPU passes | no CPU code; validated, loop-guarded, GPU-time-budgeted | none |
| WebAssembly component (7.4) | audio filters, custom sources | wasmtime in isocastd: the mixer (filters), its own thread (sources) | wasm sandbox, no WASI capabilities unless granted, memory limit, epoch deadlines | none |

Native dynamic libraries are not a tier (see the decisions).

### The manifest

A plugin is a folder with `isocast-plugin.toml`:

```toml
# A plugin uses only the sections it needs; this one shows them all.
id = "twitch-chat"          # lowercase letters, digits, dashes; unique among installed plugins
name = "Twitch chat"
version = "0.3.1"           # semver
api = 1                     # the plugin API it was made for
description = "Anonymous Twitch chat as data and as an overlay."
license = "MIT"

[permissions]               # asked for at install; nothing beyond these is possible
network = ["wss://irc-ws.chat.twitch.tv"]
data_read = ["twitch"]      # data key prefixes
data_write = ["twitch"]
methods = []                # process tier: methods it may call
files = []                  # extra paths it may read

[[feeds]]
id = "chat"
script = "chat.ts"
props = { channel = { type = "string", doc = "The channel to read." } }

[[nodes]]
id = "chat-box"
component = "ChatBox.tsx"
props.channel = { type = "string" }
props.max = { type = "i32", default = 8, min = 1, max = 50, doc = "Lines shown." }

[[filters]]
id = "glow"
shader = "glow.wgsl"
params.amount = { type = "f32", default = 0.5, min = 0.0, max = 1.0, doc = "How strong the glow is." }

[[transitions]]
id = "ripple"
shader = "ripple.wgsl"

[[audio_filters]]
id = "deesser"
component = "deesser.wasm"
sidechain = false

[[sources]]
id = "spectrum"
component = "spectrum.wasm"

[process]
command = ["python3", "bot.py"]
autostart = true
```

Param and prop types: `f32`, `i32`, `bool`, `string`, `color`, `enum`
(with `values`), `file`, `vec2`, `vec4`, each with `default`, `min`,
`max`, `doc`. They become JSON Schema for `setup schema`, the field
checks behind setup errors, and (for shaders) are matched against the
shader's `Params` struct when the plugin is added.

### Kinds in setups

- Plugin kinds are `<plugin id>/<kind id>` wherever built-in kinds go:
  `sources[].kind`, `filters[].kind` (sources and items), `audio_filters
  [].kind`, `transition.kind`, `content.kind`, and `feeds[].kind` (a
  plugin's feed, used instead of `script` and `export`). Their params are
  flat, exactly like built-in kinds' (`{ kind = "twitch-chat/chat-box",
  max = 8 }`), and are checked against the manifest with the same
  field-path and unknown-field errors.
- **A missing plugin never breaks a setup.** A kind whose plugin isn't
  installed (or is disabled or incompatible) is kept verbatim, round-trips
  through `get`, `apply` and saving untouched, and is bypassed at run time
  with one warning: a filter passes its picture or sound through, a
  transition becomes a fade, content and sources draw nothing. Plan lists
  it as a note with the command to install it, not as a problem, so a
  setup from another Mac still applies.
- A setup can say which plugins it needs: `[plugins] twitch-chat =
  "^0.3"`; `setup check`, `doctor` and `plan` report a missing or
  mismatched one.
- `isocast setup schema` (and MCP `get_setup_schema`) asks the daemon,
  which adds the installed plugins' kinds to the static schema; with no
  daemon running, the CLI reads the plugins folder itself.

### Permissions

Asked for at install (`plugin add` prints them and needs `--allow`),
stored with the installed plugin, enforced at run time:

- `methods` (process tier): the method table calls it may make; `watch`
  only delivers events it may see.
- `data_read`, `data_write`: key prefixes, for every tier that touches data.
- `network`: origins, for feeds, page and component nodes (checked by the
  scene host or WebKit's content rules) and for wasm (only through the
  outgoing HTTP we provide, checked per request). Process plugins get a
  coarse on/off from Seatbelt.
- `files`: paths a process or wasm plugin may read, besides its own folder.
- An upgrade that asks for more lists the difference and needs `--allow`
  again.

### Isolation and crash containment

- **Process plugins.** isocastd starts them (when `autostart`, or when a
  setup uses something they provide), disclaiming responsibility (no
  camera, microphone or screen grants) under a Seatbelt profile: the home
  folder unreadable except the plugin's folders and granted files, writes
  only in its data folder, exec only of its own command, network only if
  granted, and the socket. Each process plugin reaches isocastd on a
  socket of its own (`plugins/<id>/socket`, its `ISOCAST_SOCKET`), the
  only socket its sandbox may connect to; every call there is checked
  against its grant (`methods.apply: deck may call: get, marker, split,
  start, status, stop, switch_scene, watch`). A connection on the main
  socket is that plugin's too when its peer process (`LOCAL_PEERPID`) or a
  parent is one the daemon started (the case without a sandbox). A crash
  or exit restarts it with backoff (1 s doubling to 60 s, counted from its
  last start, so a plugin that ran a while comes back at once; disabled
  after 5 in 5 minutes, with the reason in `plugin list`). Output goes to
  `plugins/<id>/log`. (Built in 7.1; see notes for the profile. Seatbelt
  can't hold serving to loopback, so a plugin binds 127.0.0.1 itself.)
- **WGSL.** As 5.3: validated at add and at plan, loop-guarded, compiled
  off the hot path, timed, switched off when over budget.
- **WebAssembly.** One wasmtime store per instance, a memory limit (64 MB
  by default), no WASI capabilities unless granted (no files, no network,
  no clocks beyond what we pass), and epoch interruption: an engine-wide
  ticker advances the epoch every millisecond and each call gets a
  deadline. An audio filter call past its deadline (3 ms) or a trap
  bypasses the filter for that block (the dry block is mixed, so the mix
  never has a gap), drops the instance and instantiates a fresh one off
  the mixer thread; three failures in a minute disable it with a warning
  naming `sources[2].audio_filters[1]`. A source's call past its frame
  period drops that frame (the frame clock repeats the last, as for any
  late source). (As built in 7.4a, a call's limit is a budget of work,
  fuel, rather than an epoch deadline: about 2 ms of a tight loop on a
  base M2, the same work however loaded the Mac is; see notes.)
- In every tier the recording keeps going: the worst case is a bypassed
  effect or a frozen overlay, reported in `status` and as warnings.

### Versioning

- `version` is the plugin's semver; one version of a plugin is installed
  at a time. `plugin add` of a newer version checks the live setup's use
  of its kinds against the new schemas and refuses (listing field paths)
  if it would leave problems, unless `--force`.
- `api` is the plugin API's major version (1). The engine lists the
  versions it supports; others are installed as `incompatible` (their
  kinds count as missing).
- Each tier's contract has its own version: shader ABI 1 (the prelude),
  and the WIT package `isocast:plugin@1.0.0` (minor versions only add).
  wasm components are checked against the WIT world at add time.

### `isocast plugin`

| CLI | Method | MCP |
|---|---|---|
| `isocast plugin add PATH [--link] [--allow] [--force] [--json]` | `plugin_add` | `add_plugin` (takes the permissions it grants, which must equal the manifest's) |
| `isocast plugin list [--json]` | `plugin_list` | `list_plugins` |
| `isocast plugin remove ID [--force] [--json]` | `plugin_remove` | `remove_plugin` |
| `isocast plugin enable ID`, `isocast plugin disable ID` | `plugin_set_enabled` | — |

- `add` reads and checks the manifest, prints the permissions, copies the
  folder into the plugins folder (`--link` keeps a link to it instead, for
  development, and re-checks it at each load), validates every shader,
  precompiles every wasm component (wasmtime's precompile, so loading
  later takes milliseconds), and registers its kinds live (event
  `plugin_changed`), with no restart.
- `list` shows id, version, kinds, state (`ok`, `disabled`,
  `incompatible`, `failed: …`, process `running`/`restarting`), restarts
  and granted permissions.
- `remove` refuses while the live setup uses the plugin's kinds, listing
  the field paths; `--force` removes it anyway, and the setup keeps those
  entries verbatim (bypassed, as missing).
- The plugins folder is `~/Library/Application Support/Isocast/plugins`;
  an isolated engine uses `$ISOCAST_PLUGINS`, else a folder beside its
  socket (`/tmp/x.sock` → `/tmp/x.plugins`), as setup files do. Plugins
  come from local folders only for now; downloading from a URL or a
  registry is release work, parked.

### 7.2 Process tier: the example

`examples/plugins/deck`, a Stream Deck-style controller in plain Python
(standard library only, to show "any language"): a page on
`127.0.0.1:<port>` with a button per scene, Record, Marker and Split; it
reads `get` and `status` (what is on air and whether it records when it
starts), follows `watch` to light the scene on air, and calls
`switch_scene`, `marker`, `split`, `start` and `stop`. Its grant is
exactly those eight methods. A real Stream Deck would add a `devices =
["hid"]` permission later.

### 7.3 WGSL tier

Plugin filters, transitions and content are shader files written to ABI 1
(5.3); the manifest names them and adds ranges, defaults and docs to their
params.

### 7.4 WebAssembly tier

```wit
package isocast:plugin@1.0.0;

interface types {
  variant value {
    number(f64),
    integer(s64),
    flag(bool),
    text(string),
    color(tuple<f32, f32, f32, f32>),
  }
  record param {
    name: string,
    value: value,
  }
  record source-info {
    width: u32,
    height: u32,
    fps: u32,
    channels: u32,
  }
}

interface host {
  log: func(level: u8, message: string);
  /// A data key's value as JSON, if the plugin may read it.
  data-get: func(key: string) -> option<string>;
}

world audio-filter {
  use types.{param};
  import host;
  /// At start and whenever its settings change.
  export configure: func(sample-rate: u32, channels: u32, params: list<param>) -> result<_, string>;
  /// Frames of delay it adds; that much audio is fed ahead.
  export latency: func() -> u32;
  /// One 10 ms block of interleaved samples (and the sidechain's, if the
  /// manifest asks for one); returns the filtered block, the same length.
  export process: func(samples: list<f32>, sidechain: option<list<f32>>) -> list<f32>;
  export reset: func();
}

world source {
  use types.{param, source-info};
  import host;
  export configure: func(params: list<param>) -> result<source-info, string>;
  /// The picture at `time-ns` on the session clock: width × height BGRA,
  /// premultiplied, rows packed.
  export video: func(time-ns: u64) -> list<u8>;
  /// `frames` frames of interleaved audio from `time-ns`.
  export audio: func(time-ns: u64, frames: u32) -> list<f32>;
}
```

- **Audio filters** run in the mixer, per input, in place in the chain,
  exactly where built-in filters run (so sidechains and timing work the
  same); a manifest's `sidechain = true` adds a `sidechain_source` param
  like the compressor's. The host reads results straight out of the
  guest's memory (wasmtime's `WasmList`), so a block costs no host
  allocation.
- **Sources** run on their own generator thread on the session timeline,
  like test patterns: each canvas tick asks for the picture and copies it
  once into a pooled IOSurface (1280×720 is ~3.7 MB, ~0.3 ms); audio goes
  to the mixer like any source's.
- Components are built with any language that targets the component model
  (Rust with `cargo component` or wit-bindgen, for the examples:
  `examples/plugins/gain` and `examples/plugins/plasma`).

## Method table changes

| Method | Change | CLI | MCP | Events |
|---|---|---|---|---|
| `get` | optional params; `resolved: true` adds resolved rects and rendered nodes, with `generation` (`{"method":"get"}` still works) | `get --resolved` | `get_setup` (`resolved`) | — |
| `plan`, `apply` | `Change::Script { entry, files }`; `files` (sources of the shaders and scripts a change names) on any change | `plan`/`apply FILE.tsx` | `plan_change`/`apply_change` (`script`) | `setup_changed` |
| `build_script` (new) | the setup data a module makes, unplanned | `scene build` | — (plan covers it) | — |
| `data_get`, `data_set`, `data_delete` (new) | the data store | `data get`, `data set`, `data delete` | `get_data`, `set_data` | `data_changed` |
| `render_nodes` (new) | a component's or process plugin's nodes | `nodes set` (debugging) | — | — |
| `plugin_list`, `plugin_add`, `plugin_remove`, `plugin_set_enabled` (new) | the plugin registry | `plugin …` | `list_plugins`, `add_plugin`, `remove_plugin` | `plugin_changed` |
| `schema` (new) | the setup schema with installed plugin kinds | `setup schema` | `get_setup_schema` | — |
| `plugin_frames` (new) | a ring of IOSurfaces a process plugin draws into | — | — | — |
| `status`, `engine` | scene host, scripts, feeds, plugins; compositor and scene timings | as now | as now | `status` |

`isocast scene watch` and `scene types` are client-side, built from `get`,
`plan`, `apply` and `schema`.

## Data model changes

Sketched as the Rust types in `isocast-protocol` (doc comments become the
schema, as now). Every new field is optional and skipped when empty.

```rust
pub struct Setup {
    // … existing fields …
    pub keyframes: Vec<Keyframes>,            // [[keyframes]], by id
    pub scripts: Vec<Script>,                 // [[scripts]] { id, file, hash }
    pub feeds: Vec<Feed>,                     // [[feeds]] { id, script, export, writes, props }
    pub permissions: Permissions,             // [permissions] { network: Vec<Origin> }
    pub plugins: BTreeMap<String, String>,    // [plugins] id = "semver requirement"
}

pub struct Scene {
    // … existing fields …
    pub background: Option<Color>,
    pub layout: Option<Layout>,               // container fields only
    pub transition_from: BTreeMap<String, Transition>,
    pub origin: Option<Origin>,               // { script, hash }: made by a script
}

pub struct SceneItem {
    // … existing fields; `rect` becomes Option<Rect>, required unless `layout` …
    pub content: Option<Content>,             // text | image | shader | "plugin/node"
    pub component: Option<ComponentUse>,      // { script, export, props }
    pub layout: Option<Layout>,
    pub filters: Vec<Filter>,                 // on the item as drawn
    pub animations: Vec<Animation>,
    pub animate_changes: Option<AnimateChanges>,
    pub enter: Option<Animation>,
    pub exit: Option<Animation>,
}

pub struct Keyframes { pub id: String, pub frames: Vec<Keyframe> }
pub struct Keyframe {
    pub at: f32,                              // 0 to 1
    pub opacity: Option<f32>,
    pub translate: Option<[Length; 2]>,
    pub scale: Option<f32>,
    pub rotate: Option<f32>,
    pub background: Option<Color>,
    pub border_color: Option<Color>,
    pub border_width: Option<f32>,
    pub radius: Option<Radius>,
    pub shadow: Option<Shadow>,
    pub crop: Option<Crop>,
    pub params: BTreeMap<String, f32>,        // "<filter id>.<param>"
    pub easing: Option<Easing>,               // towards the next stop
}

pub struct Animation {
    pub keyframes: String,
    pub duration_ms: u32,
    pub delay_ms: i32,
    pub easing: Easing,
    pub iterations: Iterations,               // n or "infinite"
    pub direction: PlayDirection,
    pub fill: Fill,                           // default Both
    pub start: AnimationStart,                // OnAir (default) | Shown | Clock
}

pub struct AnimateChanges {
    pub duration_ms: u32,
    pub easing: Easing,
    pub properties: Vec<AnimatedProperty>,    // empty: all
}

pub enum Easing {                             // untagged; the four names keep their math
    Linear, EaseIn, EaseOut, EaseInOut,
    CubicBezier { cubic_bezier: [f32; 4] },
    Steps { steps: u32, position: StepPosition },
}

pub struct Layout { /* the CSS fields listed in 5.1, each Option */ }
pub enum Length { Px(f32), Percent(f32), Auto }   // 12, "50%", "auto"
// (As built: Length is a number or a string kept as written, read by
// validation and the engine, so `"12px"` saves as `"12px"`.)

// Kinds gain, beside built-ins:
//   FilterKind::Shader { file, params, margin }
//   TransitionKind::Shader { file, params, easing }, TransitionKind::Keyframes { enter, exit }
//   Content::{Text(TextSource), Image(ImageSource), Shader(ShaderContent), Plugin(PluginKind)}
//   SourceKind / FilterKind / AudioFilterKind / TransitionKind ::Plugin(PluginKind)
pub struct PluginKind { pub kind: String /* "plugin/kind" */, pub params: Map<String, Value> }
// TextSource gains `data: Option<String>` beside `text` and `file`.
```

`PluginKind` is what lets unknown kinds round-trip: the kind-tagged
enums' custom deserialisers (which already exist for field paths, see
`located`) fall back to it for any `kind` containing a `/`, keeping the
params as JSON. Patches and diffs need no new rules: the new lists have
`id`s and merge by id; `transition_from` and `plugins` are maps, merged by
key.

## How existing setups keep working

- The setup `version` stays 1: everything is additive and omitted when
  empty, so existing TOML and JSON round-trip byte for byte (5.1 checks
  this on `examples/recording.toml` and a copy of Ryan's live setup, and
  each later slice again).
- `rect` keeps its meaning and stays required for items without `layout`,
  with the same error (now mentioning `layout`).
- The four easing names keep their formulas; show/hide transitions keep
  their look (rebuilt on keyframes with the same values, checked against
  4.12d's measurements); filters, transitions and Move are unchanged.
- 5.0's compositor rewrite is checked pixel for pixel against today's
  (the 3.2, 4.1, 4.8, 4.12 snapshot checks, max difference 0).
- Unknown plugin kinds are kept and bypassed; scripts whose compiled copy
  is missing make their components draw nothing (with a warning saying to
  apply the script again), and everything else runs.
- Rendered nodes and data are never written into the setup, so `get`,
  diffs and the setup file look as they do now.
- The OBS import is unchanged. The app keeps editing `rect`s; items placed
  by layout come later in the app (P.2/N.x), using `get --resolved` for
  their boxes.

## Failure modes

| What goes wrong | What happens | What the user sees | Recording |
|---|---|---|---|
| TSX syntax or type-strip error | the change is refused | `plan`: `scenes.tsx:14:7: Unexpected token`, nothing applied | unaffected |
| A component throws or runs past 50 ms | that render is dropped; the last good nodes stay | warning with the instance path and TSX stack | unaffected |
| The scene host crashes or hangs | restarted with backoff; nodes stay as last rendered; feeds restart | `status.scene_host`, warning | unaffected |
| A feed's network fails | restarted with backoff; its keys keep their last values | feed state and error in `status` | unaffected |
| Scene code reaches an origin not listed | the connection fails in the script | the error names `[permissions] network` | unaffected |
| A data key over its limit, or a bad key name | `data_set` refused | the error names the key, the rule or the limit | unaffected |
| A client floods `data_set` | values apply; events and renders coalesce per tick | — | unaffected |
| Layout gives an item no size (0 × 0) | it isn't drawn | `get --resolved` shows the zero rect | unaffected |
| A keyframe set id doesn't exist | refused at plan | `…animations[0].keyframes: "slid-in" is not a keyframe set; valid: slide-in, chat-pop` | unaffected |
| A shader doesn't compile | refused at plan | file:line:col and the setup path | unaffected |
| A shader is slow or loops | loop guard ends loops; over budget it is switched off | warning with its GPU time | program may drop frames before it is switched off (counted) |
| A GPU device loss | the compositor's GPU restarts without the shader | warning | program drops the frames in between (counted); ISO files unaffected |
| A text or image can't be rendered (missing font, bad file) | the item draws its box only | warning once, as missing LUTs do | unaffected |
| A wasm filter traps or overruns | the dry block is mixed, a fresh instance made; disabled after 3 in a minute | warning with the filter's path | no audio gap |
| A process plugin crashes | restarted with backoff, disabled after 5 in 5 minutes | `plugin list`, warning | unaffected |
| A plugin calls a method it wasn't granted | refused | `methods.shutdown: deck may call: get, marker, split, start, status, stop, switch_scene, watch` | unaffected |
| A plugin is missing or incompatible | its kinds are bypassed, kept in the setup | a plan note with the install command | unaffected |
| Content textures exceed their budget | off-air content is dropped first and re-made when shown | `status` | unaffected |

## Implementation plan

Two tracks that can run side by side: scenes (5.x) and plugins (7.x).
Each slice is one task in `PLAN.md`, one or a few commits, building and
useful on its own, verified on the test Macs (isolated engines, synthetic
sources, release builds under the benchmark lock when measuring, scratch
in `target/scratch` or the test Mac's scratch folder).

| # | Slice | Needs | Usable on its own as |
|---|---|---|---|
| 1 | 5.0 Compiled scenes | — | a faster compositor, same pictures |
| 2 | 5.1 Layout | 5.0 | flex and grid scenes in TOML and patches |
| 3 | 5.5a Data store, content items, bound text | 5.0 (5.1 to lay text out) | `isocast data set` driving text, from agents too |
| 4 | 5.2a Keyframe animations | 5.0 | animated overlays in TOML |
| 5 | 5.2b Animated changes, enter/exit, scene transitions | 5.1, 5.2a | smooth edits and Keynote-style switches |
| 6 | 5.3a Item filters and WGSL filters | 5.0 | custom shaders on sources and items |
| 7 | 5.3b WGSL transitions and content, GPU budget | 5.3a | custom transitions and generated pictures |
| 8 | 5.4a Scene host and TSX build | 5.1, 5.5a | scenes written in TSX |
| 9 | 5.4b The TSX dev loop | 5.4a | watch, editor types |
| 10 | 5.5b Live components | 5.4a, 5.2b | data-driven overlays |
| 11 | 5.6 Feeds, network permissions, Twitch chat | 5.5b | live chat overlay |
| 12 | 7.1 Plugin registry and process plugins | — (5.5a for data permissions) | Stream Deck-style controllers in any language |
| 13 | 7.3 WGSL plugins | 7.1, 5.3b | installable filters and transitions |
| 14 | 7.4a WebAssembly audio filters | 7.1 | installable audio filters |
| 15 | 7.4b WebAssembly sources | 7.4a | installable generators |
| 16 | 5.7 Plugin nodes | 7.1, 7.3, 5.5b | installable overlay node types |

One agent works down the table; two can take the scenes track (1–11) and
the plugins track (12, 14, 15) side by side, meeting at 13 and 16. Slices
that both change the compiled scene (2, 3, 4, 6) should merge one at a
time.

### 5.0 Compiled scenes

The scene thread, `CompiledScenes`, the draw-record storage buffer,
bind-group caches (content, canvases, IOSurface-keyed frames), reusing the
last frame when nothing changed, and compositor timings in `engine`
(`compositor.cpu_us` p50/p99, `reused`, `draws`; `scenes.resolve_us`).
Snapshots, preview and shared frames use it too. No setup change.

Verify: today's snapshot checks give the same numbers (3.2's 7 pixel
checks, 4.1's 10, 4.8's nested values, 4.12a's 140,000-pixel fit model,
4.12c's 28 blend pixels) and a fixed scene with every item feature
snapshots with max difference 0 against main; 4.12d's show/hide timings
unchanged; on isocast-m2-a, compositor CPU per tick on the flagship and a
40-item static scene before and after (target: half or less), 0 dropped
frames; a static scene records 60 fps with `reused` ≈ ticks after the
first frame and identical frames (`ffmpeg -f framemd5`); allocations per
tick with `alloc-count` independent of the item count.

### 5.1 Layout

`layout` on items and scenes with taffy, optional `rect`, intrinsic sizes
from sources and images, `get --resolved`, frame `generation`.

Verify by snapshot pixels: a flex row of three 320×180 test patterns with
padding 40 and gap 20 lands at x = 40, 380, 720 exactly; `justify_content
= "center"`; a column with `flex_grow` 1 and 2 splits 1:2; a `1fr 2fr`
grid splits a third and two thirds (±0.5 px); `%`, `inset`, `aspect_ratio`
with `fit = "contain"`; Ryan's facecam placed with `right`/`bottom`
matches its `rect` version pixel for pixel. `get --resolved` agrees within
0.5 px. `examples/recording.toml` and a copy of Ryan's setup round-trip
byte for byte. A generated 1,000-item flex scene resolves in ≤ 1 ms on
isocast-m2-a; 100 live applies of layout changes at 30 a second: 0 missed
program frames.

### 5.5a Data store, content items, bound text

`data_*` methods, CLI, MCP, `data_changed`, persistence; `content` on items
(text, image); text `data = "key"`.

Verify: during a recording, `isocast data set title '"Hello"'` shows in
the program within 2 frames plus the measured raster time (frame of the
change by `framemd5`, against the reply's time on the session timeline);
a scratch loop setting 1,000 keys a second for 10 s: daemon memory flat
(±5 MB), compositor p99 unchanged, ≤ 20 `data_changed` events a second, 0
dropped frames; a persisted key survives `isocast restart` and an
unpersisted one doesn't; bad keys and oversized values give errors naming
the rule; MCP `set_data`/`get_data` round trip; inline text and image
content snapshot the same as the equivalent sources.

Built 2026-10-13 (PLAN log, 5.5a; numbers in docs/archive/2026-10-22/notes.md, Benchmarks).
Content pictures are drawn on the scene thread (no worker pool yet), kept
keys live with the scene collection, and laid-out text is drawn the size
of its box; the "As built" notes in "Content items" and "The data store"
say where it went beyond this text.

### 5.2a Keyframe animations

`[[keyframes]]`, item `animations`, the clocks (`on_air`, `shown`,
`clock`), new easings, the per-tick evaluation pass.

Verify in program recordings at 60 fps: a box translated 0 → 300 px
linearly over 1,000 ms moves exactly 5 px a frame (centroid ±0.5 px);
`cubic_bezier = [0.2, 0.8, 0.2, 1]` within 0.5 px of the computed curve;
`steps = 4` jumps exactly 4 times; an infinite alternating loop is on
schedule after 60 s; `on_air` restarts on the first frame after a switch
and `clock` doesn't; compositor CPU with 0, 100 and 1,000 animated
properties stays within 0.2 µs per property; existing transitions record
the same frames.

Built 2026-10-13 (PLAN log, 5.2a; numbers in docs/archive/2026-10-22/notes.md, Benchmarks).
Every check passed on isocast-m2-a; each animated property costs 0.04–0.06
µs with the clocks up (0.24 µs at 100 properties, where a fixed ~20 µs a
tick once anything animates dominates) and ~0.44 µs at rest, where the
whole tick runs ~8× slower. The "As built" notes under "Evaluation" say where it
went beyond this text.

### 5.2b Animated changes, enter and exit, scene transitions

`animate_changes` (including layout, FLIP), `enter`/`exit` (show/hide
transitions rebuilt on them), exiting items leaving the flow, the
`keyframes` transition kind, `transition_from`, Move using enter/exit.

Verify: a patch moving an item with a 500 ms `ease_in_out` layout
transition follows the eased path per frame (±0.5 px); a second patch
mid-way retargets without a jump (frame-to-frame step bounded by the
easing's maximum speed); removing an item with a 300 ms `exit` draws it
for exactly 18 more frames while its siblings slide up; 4.12d's show/hide
measurements unchanged; four switches exercise each level of transition
precedence and the manifest's timeline names the transition used; a
`keyframes` transition's per-frame offsets match its keyframes.

### 5.3a Item filters and WGSL filters

Item `filters` (offscreen in the item's space), the `shader` filter kind
for sources and items, ABI 1, naga validation and param reflection at
plan, the loop guard, compiling off the hot path.

Verify: an invert shader on a test pattern gives exactly 255 minus each
value, as a source filter and on an item with children (the group as a
whole); colour correction as an item filter equals it as a source filter
for an unstyled item; a shader with a typo fails plan with
`…filters[0].file: wobble.wgsl:12:5: …`; an unknown param lists the
struct's fields; a shader that loops forever is ended by the loop guard
and switched off within 30 frames with one warning, after which the
program misses no ticks (the misses before are counted and reported); 20
live param changes and a new shader file applied mid-recording: 0 missed
frames, its compile time logged.

### 5.3b WGSL transitions and content, GPU budget

The `shader` transition kind, `shader` content (cached when it doesn't
read time), timestamp queries and the plugin budget.

Verify: a WGSL left-to-right wipe's edge is at progress × width ±1 px per
frame; a WGSL crossfade equals the built-in fade within 1 level; a static
gradient content node costs no GPU pass after its first frame; a
deliberately heavy shader at 4K is reported with its GPU time and (as a
plugin, after 7.3) switched off after 30 frames with a warning naming its
path.

### 5.4a Scene host and TSX build

`isocast-scene-host` (oxc, rquickjs, the JSX runtime and `isocast`
module), its sandbox and supervision, `Change::Script`, replace-by-id,
`[[scripts]]` with compiled copies by hash, `origin`, `plan`/`apply
FILE.tsx`, `scene build`, MCP `script`. Static components only.

Verify: the example in 5.4 builds into setup data that plans to 0
differences against the same scenes written by hand in TOML and applied
first; `isocast apply scenes.tsx` then `get` shows plain data; a syntax
error is reported as `scenes.tsx:14:7`, a throw inside a component with
its TSX line, an unknown CSS property with the supported list; the host
can't read a file in `~/Documents` (Seatbelt denial logged) and has no
camera access; killing the host mid-build returns an error and isocastd
carries on; a warm build of the example takes ≤ 100 ms; a recording
during 20 applies of the TSX file: 0 missed frames.

### 5.4b The TSX dev loop

`isocast scene watch`, `isocast scene types`, the plan note for edits to
script-made scenes.

Verify: saving the file applies within 300 ms of the save and shows a
problem without applying when there is one; `scene types` lists every
source, scene and keyframe id and parses with oxc; patching a TSX-made
scene gives the note, and `plan scenes.tsx` shows the edit being undone.

### 5.5b Live components

`component` items, the hooks, instance management in the host,
`render_nodes`, merging by id with enter/exit/layout animations, coalesced
renders, rendered nodes in `get --resolved`.

Verify: the worked example's `Chat` component (5.6), fed by `isocast data
set` 20 times a second for 60 s while recording 1080p60: lines enter,
exit and slide as designed; change-to-screen latency p50 ≤ 2 frames plus
render time, p99 ≤ 4 frames (frame `generation` against the set's time);
host CPU per render ≤ 1 ms; compositor p99 within 10% of the same scene
without the chat; 0 dropped frames. `kill -9` of the host
mid-recording: the overlay holds, the host is back within 2 s, the
overlay resumes, 0 dropped frames in every file.

### 5.6 Feeds, network permissions, Twitch chat

`[[feeds]]`, `[permissions] network`, `fetch`, `WebSocket`, timers, feed
restarts, feed status, the Twitch example in `examples/scenes/`.

Verify against a local stand-in for Twitch's chat server (a scratch
WebSocket server on the test Mac speaking the IRC lines the example
uses): the feed joins, parses tags, answers PING, fills `twitch.chat`, and
reconnects with backoff after the server is killed; a connection to an
unlisted origin fails with the `[permissions]` error; `plan` notes added
origins. Reading a real channel anonymously is listed for Ryan.

### 7.1 Plugin registry and process plugins (7.1, 7.2, 7.5)

The manifest, the plugins folder, `plugin add|list|remove|enable|disable`
on the method table, CLI and MCP, permissions, versioning, the schema
method, the process supervisor with Seatbelt and peer-process scoping, and
the deck example.

Verify: `plugin add examples/plugins/deck --allow` lists it with its
permissions; the deck page switches scenes, adds markers and splits on an
isolated engine; a call outside its grant is refused naming the grant;
`kill -9` of the deck restarts it within 1 s and repeated crashes back off
and disable it; it can't read a test file in `~/Documents`; `plugin
remove` stops it; an isolated engine uses its own plugins folder.

Built 2026-10-13 (PLAN log, 7.1/7.2/7.5). Left for 7.3: the `schema`
method. (`plugin remove --force` came with 7.4a, whose audio filters were
the first plugin kinds setups could use.)

### 7.3 WGSL plugins

Plugin filters, transitions and content from manifests; params with
ranges, defaults and docs; plugin kinds in setups and in `setup schema`;
missing-plugin bypass; refusing removal while used.

Verify: an example plugin's filter and transition appear in the schema
with their ranges and docs, validate with field paths (`…filters[0]
.amount: 2 is over the maximum 1`), and draw in the program; `plugin
remove` while used lists the paths; with `--force` the setup still applies
with a note, the program bypasses the filter, and the saved setup is byte
for byte the same; adding the plugin back makes it work again with no
setup edit.

### 7.4a WebAssembly audio filters

wasmtime with the component model, the WIT package, precompiling at add,
filters in the mixer with epoch deadlines, memory limits, bypass and
re-instantiation; `examples/plugins/gain` (Rust to `wasm32-wasip2`).

Verify: the wasm gain at −6 dB gives exactly the built-in gain's level
(0.2506 for a 0.5 tone); a sidechain ducker matches the built-in
compressor within 0.5 dB; an injected trap and an infinite loop each give
a warning, 0 audio dropouts, a continuous mix, and disabling after three;
CPU per block measured (gain ≤ 0.05 ms).

Built 2026-10-13 (PLAN log, 7.4a; docs/archive/2026-10-22/notes.md Decisions for where it
differs): the WIT package is `wit/plugin.wit`; calls are bounded by fuel
(a budget of work) rather than epoch deadlines, results are lifted into a
host buffer (wasmtime 49 frees a `WasmList` before the call returns), and
plugin kinds in setups arrived here for audio filters, with `plan`'s
manifest checks and `plugin remove --force` (the `schema` method stays
7.3's). The example plugin (`examples/plugins/gain`) holds a gain and a
sidechain ducker, built `no_std` with no WASI.

### 7.4b WebAssembly sources

Sources from wasm components on generator threads, pictures copied into
pooled IOSurfaces, audio into the mixer; `examples/plugins/plasma`.

Verify: a 1280×720 30 fps plasma source records an ISO file with 300 of
300 frames in 10 s and 0 dropped; per-frame call time p99 measured; a
guest allocating past its memory limit traps, the source holds its last
frame, and a warning names it.

### 5.7 Plugin nodes

Node types from manifests: shader, component, page and process
renderers; `plugin_frames` (IOSurface rings drawn by a process); node
types in the schema and `scene types`.

Verify: one example plugin with a node of each renderer draws in a scene;
the schema lists them with props and docs; a scripted MCP session adds a
node using only `get_setup_schema` and `apply_change`; a process node
drawing 1080p60 into its ring costs the daemon no more than a capture
source of the same size (CPU measured).

### Later

- A data log: data changes during a take written beside the manifest, so
  a program can be re-rendered exactly.
- A `timer` content kind (clocks and countdowns rasterised once a second).
- Static subtree caching (draw unchanged groups once into a texture), if
  5.0's numbers show draw counts matter; it belongs with S.3.
- Plugins from URLs or a registry; HID access for real Stream Decks.
- The app's editors for layout, animations, data and plugins (P.2/N.x).

## Decisions and rejected alternatives

1. **The native compositor draws everything.** Rejected: a browser per
   scene (heavy, not frame-exact; `docs/scene-language.md` already chose
   this).
2. **Compile scenes once per change into a draw list with one storage
   buffer of draw records and cached bind groups.** Rejected: today's
   per-layer uniform buffers and bind groups (cost grows with every item);
   evaluating animation on the GPU in a compute pass (the CPU pass is
   microseconds, and the CPU must track starts and ends anyway); bindless
   texture arrays (captured frames are external IOSurface textures, and
   wgpu's binding arrays aren't portable).
3. **Repeat the last program frame when nothing changed.** Rejected:
   always redrawing (a static desktop or card would cost a full draw every
   tick for identical pixels).
4. **Layout with taffy 0.10**, already locked through Slint, MIT, flexbox,
   grid and block. Rejected: our own flexbox (years of edge cases), Yoga
   (C++), morphorm (not CSS). Its `parse` feature is off (cssparser is
   MPL-2.0).
5. **Layout runs on change, never per frame; layout changes animate as
   rect interpolation (FLIP).** Rejected: animating layout properties
   (width, flex-grow) by laying out every tick.
6. **Only compositor properties animate.** Rejected: animating text,
   fonts or content (a re-raster per frame).
7. **`rect` stays as the absolute form; `rect` or `layout`, not both.**
   Rejected: replacing `rect` (breaks every setup, the app and the
   import).
8. **`visible = false` takes items out of layout** (CSS `display: none`),
   matching OBS. Rejected: keeping their space (could become a
   `keep_space` flag later).
9. **Default `fill = "both"` and `start = "on_air"`** (taste calls: enter
   animations with a delay shouldn't flash the end state, and animations
   should play when the audience first sees the scene). Rejected: CSS's
   `none` default.
10. **Keep the four existing easing names' formulas; map CSS keywords to
    CSS's béziers in TSX.** Rejected: changing the existing names (would
    change recorded transitions of existing setups).
11. **Item filters reuse the filter registry** (two places to attach one
    set of kinds). Rejected: a separate "effects" registry.
12. **Shader params come from the shader's `struct Params` through naga;
    manifests only add ranges and docs.** Rejected: generating the struct
    from a manifest (ad-hoc shaders would need one); the built-ins'
    anonymous `array<vec4<f32>, 8>` (no names, no useful errors).
13. **Untrusted shaders are loop-guarded by rewriting naga IR, and
    timed.** Rejected: trusting them; a separate GPU process (a
    cross-process texture hand-off every frame).
14. **TSX applies as replace-by-id of what it defines, remembering its
    origin.** Rejected: two-way sync between TSX and the setup
    (unsolvable merges); TSX as the only source of truth (breaks the app
    and agent patches).
15. **Files a change names are read when it is planned, sent by the
    client where it can; what runs is the copy read at apply** (scripts
    compiled and kept by hash). Rejected: the daemon reading script files
    itself (privacy prompts block it, as the OBS import found) and running
    whatever is on disk now (a save mid-recording would change the
    program).
16. **Scripts run in a separate, sandboxed scene host.** Rejected: a JS
    engine inside isocastd (a crash or exploit in the process holding the
    camera, microphone and screen grants).
17. **oxc for TSX, QuickJS-ng (rquickjs) to run it.** Rejected: swc
    (a much larger dependency tree), sucrase inside QuickJS (weaker
    TypeScript support), Node or esbuild as a required tool (not on a
    fresh Mac), V8/deno_core (tens of MB, long builds), Boa (pure Rust but
    slower and a heavier build).
18. **No npm; relative imports, `isocast` and `plugin:` only.** Rejected:
    `node_modules` resolution (network, size, and a supply chain inside
    scene code).
19. **Our own small JSX runtime and hooks; the engine merges by id.**
    Rejected: React with react-reconciler (size, scheduler assumptions),
    Preact (DOM-bound).
20. **Component output is runtime state, not setup data.** Rejected:
    applying renders as setup changes (a revision, a diff and a file write
    per chat message).
21. **Feeds hold side effects; components are pure.** Rejected:
    `useEffect` in components (a connection would die when its scene went
    off air).
22. **The data store is separate from the setup, with opt-in
    persistence.** Rejected: data as setup fields (revisions and saves per
    value).
23. **Plugin kinds are `plugin/kind` with flat params; unknown ones are
    kept and bypassed.** Rejected: `kind = "plugin"` with nested params
    (verbose, unlike built-ins); refusing setups with unknown kinds (loses
    setups across Macs).
24. **Process plugins are scoped by their connection's process
    (`LOCAL_PEERPID` and its parents), sandboxed with Seatbelt and without
    TCC grants.** Rejected: tokens (leak through the environment and `ps`,
    and a child could connect without one as the user); XPC (a macOS-only
    API for plugin authors). Seatbelt (`sandbox-exec` profiles) is
    deprecated by Apple but still used by current tools; if it goes, the
    plugins run unsandboxed with a doctor warning.
25. **WebAssembly through the component model on wasmtime, in-process,
    precompiled at add, with epoch deadlines.** Rejected: native dylibs
    (no isolation, unstable Rust ABI); out-of-process wasm for audio (IPC
    jitter on 10 ms blocks); core-module ABIs (worse ergonomics, and
    `WasmList` already avoids copies).
26. **Plugins install from local folders only.** Rejected for now: URLs
    and a registry (release work, parked by Ryan).

## Questions for Ryan

- AGENTS.md asks before adding harnesses: may the slices add
  `examples/scenes/` (the TSX examples), `examples/plugins/` (deck, gain,
  plasma, a WGSL example), a "scenes" scenario in `scripts/bench.py`, and
  an `alloc-count` cargo feature on isocastd for the allocation check?
- May 5.6 read a real Twitch channel's chat anonymously from a test Mac
  (read-only, no account), or should that stay your check?
- Taste calls you may overrule: `fill = "both"` and `start = "on_air"` as
  defaults; `visible = false` collapsing layout; the 2 ms GPU budget for
  plugin shaders; an animation's `easing` defaulting to `ease_in_out` (as
  transitions do) rather than CSS's `ease` (5.2a).

## Fit with P and S

- **P (profiles and scene collections).** Everything new here except the
  data store's values and installed plugins belongs to the scene
  collection: keyframes, scripts, feeds, permissions and plugin
  requirements travel with scenes. Persisted data values are kept per
  collection (`<collection>.data.json`). Installed plugins and their
  grants are per machine, in neither. A TSX apply patches the active
  collection. A collection switch stops its feeds and components and
  starts the new one's.
- **S (stress and performance).** 5.0 is a compositor refactor that S.3
  would otherwise do; it should land first and be measured with S.1's
  harness. The GPU wait in `program.rs` and the per-source filter
  submissions stay S.3's. S.2's stress suite should add a scenes scenario
  (many items, many animations, a chat flood, rapid data and applies) once
  5.5b lands; the S.1 comparison with OBS can then include an overlay
  scene built both ways.
