# Scenes-as-code research (phase 4)

Research done 2026-10-11 by reading crate sources (nothing built). Pin
versions; these crates change APIs often. Verify as each task is done.

## Layout (4.2): `taffy = "0.14.0"` (MIT)

- No `parse` feature (it pulls MPL-2.0 `cssparser`); map props ourselves.
- `TaffyTree::new()`, `new_leaf(Style)`, `new_leaf_with_context(Style,
  ctx)`, `new_with_children(Style, &[NodeId])`, `set_style`,
  `compute_layout_with_measure(root, Size { width:
  AvailableSpace::Definite(w), .. }, |inputs, id, ctx, style|
  taffy::compute_leaf_layout(inputs, style, |_, _| 0.0, |known, avail|
  measure(ctx, known, avail)))`, `layout(id).location/size`.
- Prop mapping: `size: Size<Dimension>` (width/height), `inset:
  Rect<LengthPercentageAuto>`, `position: Position::{Relative, Absolute}`,
  `flex_direction`, `justify_content: Option<JustifyContent>` (consts like
  `JustifyContent::SPACE_BETWEEN`), `align_items`, `gap:
  Size<LengthPercentage>` (width = column gap), `padding`, `margin` (auto
  works), `aspect_ratio: Option<f32>` (width / height), grid via
  `display: Display::Grid`, `grid_template_columns: vec![fr(1.0), ..]`.
  `percent()` takes 0..1.
- Pitfalls: `location` is relative to the parent; absolute children are
  placed in their direct parent; default display is flex (decide whether
  scene children default to absolute); rounding is on by default
  (`disable_rounding()` for smooth morphs); flex items have automatic
  minimum sizes; lay out only on scene or data changes, never per frame.

## Text (4.3): `cosmic-text = "0.19.0"` (MIT OR Apache-2.0)

- glyphon 0.12 supports wgpu 30, but draws text in its own pipeline with no
  rotation, opacity, clipping or effects. Instead rasterise each text node
  once (cosmic-text `Buffer` + `SwashCache`) into its own RGBA texture,
  premultiplied, and draw it like an image; cache by (text, attrs, size,
  wrap width, raster scale).
- `FontSystem::new()` takes up to ~1 s (10× in debug): create one at
  daemon start, off the render thread. `Family::SansSerif` maps to "Open
  Sans", which macOS lacks: set `db_mut().set_sans_serif_family(..)`.
- Rasterise at the final pixel scale (the largest an animation reaches).

## Shapes (4.1)

- Rounded-box SDF (standard, as Inigo Quilez describes), per-corner radius
  by quadrant, antialiasing `clamp(0.5 - d / fwidth(d), 0, 1)`; border by
  `coverage(d + width)` (gives CSS's inner radius).
- Normalise radii on the CPU with CSS's overlap rule.
- Shadow: Gaussian with σ = blur / 2 via an erf approximation
  (Abramowitz & Stegun 7.1.27); not drawn under the box (CSS); quad margin
  ≥ 3σ + spread + |offset|.
- Premultiplied alpha throughout (`PREMULTIPLIED_ALPHA_BLENDING`).
- Keep `fwidth`/`textureSample` in uniform control flow (compute, then
  `select`). Pool uniform buffers (one dynamic-offset buffer per frame)
  once scenes have many nodes.

## TSX (4.8): `rquickjs = "0.14.0"` (features `loader`) + `oxc = "=0.153.0"` (features `transformer`, `semantic`, `codegen`)

- rquickjs: QuickJS-NG, 12 crates, pre-generated bindings for Apple
  targets (needs Xcode clang, not libclang). boa (145 crates) and
  deno_core (V8 download, 160 crates) are heavier.
- oxc: `Parser::new(&alloc, src, SourceType::tsx().with_module(true))`,
  `SemanticBuilder::new().build(&program).semantic.into_scoping()`,
  `TransformOptions` with `jsx.runtime = JsxRuntime::Classic`, `pragma =
  "h"`, `pragma_frag = "Fragment"` and the same pragmas under
  `typescript` (else the `h` import is elided), then
  `Transformer::new(..).build_with_scoping(..)` and `Codegen`.
- Evaluate in rquickjs with memory, stack and time limits
  (`set_interrupt_handler`); a prelude defines `h`, `Fragment` and the
  node components; local imports need a custom `Loader` that runs oxc;
  `Runtime` is `!Send` (dedicated thread). Return JSON via
  `ctx.json_stringify`.
- QuickJS has no fetch/WebSocket: expose Rust functions under the
  setup's permission list for 4.10.

## Images (4.4)

- `image = { version = "0.25.10", default-features = false, features =
  ["png", "jpeg"] }`: `ImageReader::open(p)?.with_guessed_format()?
  .into_decoder()?`, apply EXIF orientation, `into_rgba8()`, premultiply.
- SVG: `resvg = { version = "0.48.1", default-features = false, features
  = ["text", "system-fonts", "memmap-fonts"] }`; `usvg::Tree::from_data`,
  `resvg::render` into a `tiny_skia::Pixmap` (premultiplied) at the laid
  out size.
- usvg (fontdb 0.24) and cosmic-text (fontdb 0.23) can't share a font
  database.

## Licences

All permissive (MIT, Apache-2.0, BSD-3 for tiny-skia, Unicode-3.0 for ICU
data). Avoid taffy's `parse` feature and deno_core (MPL-2.0 pieces).

## OBS effect filters (from obsproject.com/kb filter pages, 2026-10-11)

Public knowledge-base pages give setting names and defaults (rarely
ranges); the formulas are ours. Doc quirks noted where a default looks
wrong.

- Color Correction: Gamma −3..3 (0), Contrast −4..4 (0), Brightness −1..1
  (0), Saturation −1..5 (0), Hue Shift −180..180 (0), Opacity 0..1 (1),
  Color Multiply (#ffffff, tints light colours), Color Add (#000000,
  changes dark colours).
- Chroma Key: Key Color Type (Green; also Blue, Magenta, custom),
  Similarity (400), Smoothness (80), Key Color Spill Reduction (100),
  Opacity (1.0), Contrast (0), Brightness (0), Gamma (0). The OBS UI
  ranges are 1–1000 for similarity, smoothness and spill.
- Color Key: Key Color Type (Green), Similarity (page says 400),
  Smoothness (page says 80), Opacity, Contrast, Brightness, Gamma.
- Luma Key: Luma Max (1.0), Luma Max Smooth (0.0), Luma Min (page says
  1.0; 0.0 is the only sensible default), Luma Min Smooth (0.0).
- Crop/Pad: Relative (on); Left/Top/Right/Bottom px (negative pads);
  absolute mode X/Y/Width/Height. Applies to every use of the source.
- Scaling/Aspect Ratio: Scale Filtering (Bicubic), Resolution (None, or a
  size or aspect ratio), Undistort centre (off).
- Scroll: Horizontal Speed (0; positive right), Vertical Speed (0;
  positive up), Limit Width/Height (off) with Width/Height (100), Loop
  (on).
- Sharpen: Sharpness 0..1 (0.08).
- Image Mask/Blend: Type (Alpha Mask (Color Channel) default, Alpha Mask
  (Alpha Channel); the UI also offers Blend Multiply/Addition/
  Subtraction), Path, Color (#ffffff), Opacity 0..1 (1), Stretch Image
  (off).
- Apply LUT: Path (.png or .cube), Amount 0..1 (1).
- Render Delay: Delay in ms (0).
