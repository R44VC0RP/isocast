# OBS parity

Isocast aims to offer everything a first-time user expects from OBS Studio
on a Mac: the same kinds of sources, filters, transitions and scene item
controls, under familiar names. This page maps each OBS feature (from the
public OBS knowledge base, not its source code) to Isocast and the plan
task that brings it. Game Capture is Windows-only in OBS and has no Mac
counterpart.

Isocast runs on Apple silicon Macs only (aarch64-apple-darwin).

## Sources

| OBS source | Isocast `kind` | Status / task |
|---|---|---|
| macOS Screen Capture: display | `display` | done (1.3) |
| macOS Screen Capture: window | `window` | done (4.6) |
| macOS Screen Capture: application | `application` | done (4.6) |
| Display Capture, Window Capture (pre-macOS 13) | same as above | covered by ScreenCaptureKit |
| Video Capture Device (webcams, capture cards, iPhone) | `camera` | done (1.4) |
| Audio Input Capture | `microphone` | done (1.5) |
| Audio Output Capture (desktop audio) | `system_audio` | done (1.5) |
| Application Audio Capture | `application_audio` | done (4.6) |
| Color Source | `color` | done (4.3) |
| Image | `image` | done (4.3) |
| Image Slide Show | `slideshow` | done (4.3) |
| Text | `text` | done (4.4) |
| Media Source (video and audio files) | `media` | done (4.5) |
| Browser Source | `browser`: `control_audio`, `shutdown_when_hidden`, `refresh_when_shown`, and `refresh_browser` for "Refresh cache of current page" | done (4.7) |
| Scene (a scene inside another) | an item with `scene = "id"` | done (4.8) |
| Group | items with `children` | done (4.1) |
| VLC Video Source (playlists) | `media` with a playlist | done (4.5a) |
| Syphon Client | — | Later |
| NDI (plugin in OBS) | — | Later |
| Isocast only: test pattern, tone | `test_pattern`, `tone` | done (1.1) |

## Multistreaming and canvases (#84)

| OBS (plugin) | Isocast | Status |
|---|---|---|
| Aitum Multistream: more outputs, own server/key/bitrate/resolution | `[[stream.destinations]]` (own bitrate, size, rate control, keyframes, audio) | done (G.10, G.12); imported from OBS |
| Aitum Vertical Canvas: a vertical canvas with its own scenes and outputs | `[vertical]` / `[[canvases]]` with `scenes[].canvases.<id>` and own `stream` | done (G.12); imported (its own scenes aren't) |
| Aitum Stream Suite / OBS 31 extra canvases | `[[canvases]]` (up to 8) | done (G.12); OBS's own canvases aren't imported |
| obs-websocket GetCanvasList; Vertical Canvas vendor requests | mapped in the plugin's shapes; per-canvas recording, pause, chapters and virtual camera answer `success: false` (one recording takes every canvas) | done (G.12) |

## Effect filters (video), task 4.9 (`filters` on a source)

Run on the GPU in each source's own filter chain, in order, before the
scene draws it. ISO files keep the unfiltered picture unless
`recording.iso.filtered` is set (done, 4.9f); the program and snapshots
are filtered.

| OBS filter | Isocast filter `kind` |
|---|---|
| Color Correction (gamma, contrast, brightness, saturation, hue, opacity, colour multiply/add) | `color_correction` (done, 4.9a) |
| Chroma Key | `chroma_key` (done, 4.9b) |
| Color Key | `color_key` (done, 4.9b) |
| Luma Key | `luma_key` (done, 4.9b) |
| Crop/Pad | `crop_pad` (done, 4.9c) |
| Scaling/Aspect Ratio | `scaling` (done, 4.9c) |
| Sharpen | `sharpen` (done, 4.9d) |
| Scroll | `scroll` (done, 4.9c) |
| Image Mask/Blend | `image_mask` (done, 4.9d) |
| Apply LUT (.cube, .png) | `lut` (done, 4.9d) |
| Render Delay | `render_delay` (done, 4.9e) |
| Advanced Masks plugin: shape masks (rectangle, circle, ellipse, regular polygon, star, heart) | `shape_mask` (done, #115) |
| Advanced Masks plugin: gradient masks | `gradient_mask`, linear or radial with up to 8 stops (done, #115) |
| Advanced Masks plugin: source masks (a source or scene as the mask) | `source_mask`, by luminance, alpha or one channel, with threshold and softness (done, #115) |
| Advanced Masks plugin: image masks | `image_mask` (the plugin's image masks aren't imported; add one with the file) |
| Advanced Masks plugin: adjustment masks, BSM alpha wipe, squircle/superformula, custom channel mix | not built; import leaves them out with a note |

Mask filters stack: each one's `combine` (`intersect`, `add`,
`subtract`) says how it joins the masks before it, and their numbers
animate with stops written in the filter (`animations`). They are
written from the plugin's public description, not its code. Importing an
OBS collection maps Advanced Masks filters' alpha shape, source and
gradient masks; positions are read against the masked source's size
saved with the filter. Its menus are numbered as its interface lists
them (the collections seen agree); polygon and star sizes, the gradient's
width, source-mask thresholds and Scene View Transformation aren't read
(a note says so). obs-websocket's filter requests (`GetSourceFilterList`,
`GetSourceFilter`, `SetSourceFilterSettings`, `SetSourceFilterEnabled`,
`CreateSourceFilter`, `RemoveSourceFilter`, `GetSourceFilterKindList`,
`GetSourceFilterDefaultSettings`) use Isocast's own kinds and settings.

## Audio filters, task 4.10

`audio_filters` on a source. Run on each source's audio in the mixer
before the mix (done, 4.10a), and in ISO files only with
`recording.iso.filtered` (done, 4.10b; no sidechain ducking there).

| OBS filter | Isocast filter `kind` |
|---|---|
| Gain | `gain` (done, 4.10a) |
| Noise Gate | `noise_gate` (done, 4.10a) |
| Noise Suppression | `noise_suppression` (RNNoise via `nnnoiseless`; OBS's Speex and NVIDIA methods not offered) (done, 4.10a) |
| Compressor (with sidechain ducking) | `compressor` with `sidechain_source` (done, 4.10a) |
| Limiter | `limiter` (done, 4.10a) |
| Expander | `expander` (done, 4.10a) |
| Upward Compressor | `upward_compressor` (done, 4.10a) |
| 3-Band Equalizer | `equalizer` (done, 4.10a) |
| Invert Polarity | `invert_polarity` (done, 4.10a) |
| Video Delay (Async) / sync offset | `audio.offset_ms` (done, 3.4) |
| VST 2.x Plugin | Audio Unit plugins (the Mac equivalent), Later |

## Transitions, task 4.11

| OBS transition | Isocast `kind` |
|---|---|
| Cut | `cut` (done, 3.5) |
| Fade | `fade` (done, 3.5) |
| Swipe | `swipe`  (done, 4.11a) |
| Slide | `slide`  (done, 4.11a) |
| Fade to Color | `fade_to_color`  (done, 4.11a) |
| Luma Wipe | `luma_wipe`  (done, 4.11a) |
| Stinger (incl. track matte) | `stinger` (done, 4.11b; side-by-side or stacked matte; no stinger audio yet) |
| Move (items matched between scenes) | `move`  (done, 4.11a) |
| Per-scene transition override | `scenes[].transition`  (done, 4.11a) |

## Scene items, tasks 4.1 and 4.12

| OBS | Isocast |
|---|---|
| Position, size, rotation, crop | `rect`, `style.transform`, `crop` (3.2, 4.1) |
| Flip horizontal and vertical | `style.transform.flip_horizontal` and `flip_vertical`; an OBS item scaled by a negative amount imports as flipped (done, G.6) |
| Visibility, order, lock | `visible`, list order (3.2); lock is an app concern |
| Bounding box types and alignment | `fit` (`stretch`, `contain`, `cover`, `width`, `height`, `scale_down`, `none`) and `align` (done, 4.12a) |
| Scale filtering | `scale_filter` (`point`, `bilinear`, `bicubic`, `lanczos`, `area`) (done, 4.12b) |
| Blending modes (blending method "sRGB off") | `blend` (`normal`, `additive`, `subtract`, `screen`, `multiply`, `lighten`, `darken`) (done, 4.12c) |
| Show/hide transitions | items' `show_transition` / `hide_transition`: `fade` or `slide` (a direction), duration, easing; OBS's swipe imports as a slide, others as a fade (done, 4.12d) |
| Groups | `children` (4.1) |

## Audio mixer, tasks 3.4 and 4.13

| OBS | Isocast |
|---|---|
| Volume, mute | `audio.gain_db`, `audio.muted` (3.4) |
| Sync offset | `audio.offset_ms` (3.4) |
| Meters | live status levels (3.4) |
| Balance | `audio.balance` (−1 left … 1 right) and `[audio] pan_law` (`sine`, `square`, `linear`) (done, 4.13a) |
| Audio monitoring | `audio.monitor` (`off`, `monitor_only`, `monitor_and_output`) and `[audio] monitoring_device` (done, 4.13b) |
| Tracks | the program mix plus per-source ISO tracks (3.4) |

## Importing OBS scene collections (4.14)

`isocast setup import-obs` (MCP `import_obs_collection`) converts a
collection using the mappings on this page and lists anything without an
Isocast equivalent with the reason.

## Adding a source kind

Every source, whatever it is, ends up as frames in a frame slot and/or
audio chunks in a feed, stamped on the host clock. The session's frame
clocks, the program compositor, the mixer, snapshots and ISO files all read
from there, so a new kind needs only:

1. **Settings** in `crates/isocast-protocol/src/source.rs`: a struct whose
   doc comments describe each field (they become the JSON Schema), an
   `impl Kind` saying whether it has video and/or audio and checking its
   settings, and a line in the `source_kinds!` list at the bottom
   (`"media" => Media(MediaSource),`).
2. **An opener** in `crates/isocast-engine/src/source.rs`: an arm in
   `open` that starts the source and returns an `Opened` with its
   `Video` (a `FrameSlot`, size and frame rate) and/or `Audio` (a feed,
   chunks and channel count). Device sources call a `Platform` method;
   generated ones run a `Generator` thread that waits for the timeline's
   `Epoch` and pushes frames or chunks.
3. **A platform method** in `crates/isocast-platform-macos` if it needs
   macOS (a capture, a decoder, a web view), behind a `Platform` trait
   method in the engine.

Optionally, a quick spec in `Setup::quick` (like `camera:NAME`) and a
`Platform::probe` arm so `doctor` can check it without opening it.

## Adding a filter

A video filter needs:

1. **Settings** in `crates/isocast-protocol/src/filter.rs`: a struct with
   doc comments, an `impl Check` for its ranges, and a line in
   `filter_kinds!` (`"color_correction" => ColorCorrection(...)`).
2. **A pass** in `crates/isocast-engine/src/filter.rs`: a fragment shader
   in `SHADER` that reads the premultiplied canvas `input` and returns
   premultiplied colour, its entry in `PASSES`, and an arm in `pass` that
   turns the settings into up to eight vec4 parameters.

The chain, its canvases, live changes and validation of ids come for
free. An audio filter needs settings in
`crates/isocast-protocol/src/audio_filter.rs` (struct, `impl Check`, a line
in `audio_filter_kinds!`) and a processor in
`crates/isocast-engine/src/audio_filter.rs` (arms in `Processor::new`, `set`
and `process`; `latency` if it delays the sound).
