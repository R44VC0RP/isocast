# Scenes as code

A scene is plain data in the setup. You can write it by hand, have an agent
patch it, or build it in the app; all three produce the same document.
This page covers what works today: layout, live data and keyframe
animations. Every field is in the [setup reference](setup-reference.md).

The longer design, including what is not built yet (TSX authoring, animated
changes and enter/exit animations, WGSL effects, external APIs, plugin
nodes), is in [scenes-as-code.md](scenes-as-code.md).

## Items

A scene is a list of items, drawn back to front. An item shows one of:

- a video `source`, by id;
- another `scene`, by id (a nested scene);
- `content`: text or an image the engine draws from settings, without a
  source (no ISO file, opens nothing);
- nothing: a box that is just its `style` (a background, border, radius,
  shadow).

Any item can hold `children`, which move, scale, rotate and fade with it.

`style.transform` moves, scales, turns and flips an item about its centre,
after layout. A flip mirrors everything the item draws (its picture,
children, border and shadow), so a camera that comes in mirrored is fixed
with `transform = { flip_horizontal = true }`. ISO files keep the source
as captured.

## Layout

Place an item with `rect` (canvas pixels), or with `layout`: CSS flexbox,
grid, block and absolute positioning, in snake case. Lengths are pixels or
percentages. Layout runs when something changes, never per frame.

```toml
[[scenes]]
id = "talk"
name = "Talk"
layout = { display = "flex", padding = 48, gap = 48 }

[[scenes.items]]
source = "screen"
layout = { flex_grow = 3, flex_basis = 0 }
fit = "contain"

[[scenes.items]]
source = "camera"
layout = { flex_grow = 1, flex_basis = 0, aspect_ratio = 0.5625 }
fit = "cover"
style = { radius = 24 }
```

`isocast get --resolved` prints where layout put every item.

## Live data

The engine keeps a data store: values by dotted key, like `score.home`.
Text shows a key with `data = "key"` and changes as soon as the value does.
Setting data never changes the setup.

```toml
[[scenes.items]]
id = "score"
rect = { x = 64, y = 64, width = 400, height = 80 }
content = { kind = "text", data = "score.home", size = 64 }
```

```sh
isocast data set score.home 3
isocast data set score.home 4 --persist   # keep it with the scene collection
isocast data get                          # every key
```

Agents use `get_data` and `set_data`; plugins and scripts call the same
methods. Watchers get a `data_changed` event.

## Keyframes

Define keyframe sets once, at the top level, then play them on items. This
is CSS's `@keyframes` and `animation`: stops at 0 to 1, easing, iterations,
direction and fill.

```toml
[[keyframes]]
id = "slide-in"
frames = [
  { at = 0, translate = ["-120%", 0], opacity = 0 },
  { at = 1, translate = [0, 0], opacity = 1 },
]

[[scenes.items]]
id = "lower-third"
rect = { x = 64, y = 900, width = 900, height = 120 }
style = { background = "#000000cc", radius = 16 }
animations = [{ keyframes = "slide-in", duration_ms = 400, easing = "ease_out" }]
```

Animatable properties: `translate`, `scale`, `rotate`, `opacity`,
`background`, `border_color`, `border_width`, `radius`, `crop` and
`shadow`.

An animation runs on a clock, set by `start`:

- `on_air` (the default): from when the item's scene comes on air (or into
  studio mode's preview), again each time;
- `shown`: from when the item appears or is made visible;
- `clock`: on the engine's clock, which never restarts, so loops stay in
  step across scene switches and recordings.

Easing is a name (`linear`, `ease_in`, `ease_out`, `ease_in_out`), a
`cubic_bezier`, or `steps`. Animations are evaluated once per canvas
tick, for animated properties only.

## Masks

Three filter kinds mask a source's picture (#115): `shape_mask`
(rectangle with a radius per corner, circle, ellipse, polygon, star,
heart), `gradient_mask` (linear or radial, with stops) and `source_mask`
(another source's or scene's live picture, by its luminance, alpha or one
channel, in step with the program). Positions and sizes are fractions of
the picture, radii and feathers its pixels. Stack them; `combine` says
how each joins those before it. Their numbers animate with stops of
their own:

```toml
[[sources]]
id = "camera"
kind = "camera"
filters = [
  { id = "round", kind = "shape_mask", shape = "circle", width = 0.6, height = 0.9, feather = 4 },
  { id = "wipe", kind = "source_mask", mask_source = "wipe-video", channel = "luminance",
    animations = [{ duration_ms = 2000, iterations = "infinite", frames = [{ at = 1, scale = 1.2 }] }] },
]
```

A source mask's source runs while what it masks does. In Filters, a
shape mask's box has handles on the picture to move, size and turn it.

## Changing scenes from code

Read the setup and its revision, plan a change, then apply it at that
revision. Lists of sources, scenes and items merge by id, so a patch names
only what changes:

```sh
isocast get --json                        # the setup and its revision
isocast plan --patch '{"scenes":[{"id":"talk","items":[{"id":"score","visible":false}]}]}'
isocast apply --revision 12 --patch '...'
```

Over MCP the same steps are `get_setup`, `plan_change` and `apply_change`.
