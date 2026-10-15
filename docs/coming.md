# Coming

These are being built and are not in a release yet. Each will be on the
API, the CLI and MCP, as well as in the app.

## In 0.0.5 (on main now)

- **First launch finds OBS.** OBS's scene collections are offered for
  import, then an Optimize step picks settings this Mac keeps up with,
  for recording, streaming or both (also Tools › Auto-Configuration
  Wizard…).
- **Replay buffer.** Keep the last seconds of the program in memory and
  save them on command or hotkey ([recording](recording.md#replay-buffer)).
- **ISO capacity warnings.** `isocast capacity`, and warnings before and
  during a take when the encoders can't keep up with every file
  ([recording](recording.md)).

## Virtual camera

The program as a macOS camera, so Zoom, Meet, FaceTime and other apps can
use it. Start and stop it from the app, the CLI or an agent. The controls
are in the app today; the camera extension shipped inside the app is
being verified.

## Also planned

- Projectors and a multiview grid.
- WGSL shader filters and WebAssembly sources for [plugins](plugins.md).
- Later: a second (vertical) canvas, SRT and WHIP streaming, NDI, Syphon,
  Audio Unit plugins, Windows.

Hotkeys and the Create view (View › Create, a clip bar for
[timelines](timelines.md)) shipped in 0.0.4: see [hotkeys](hotkeys.md).
