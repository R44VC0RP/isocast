# Coming

These are being built or planned and are not in a release yet. Each will
be on the API, the CLI and MCP, as well as in the app.

## Next

- **Steadier under heavy load.** An engine hardening pass (frame pools,
  locks that can't poison, a lock-free monitor, writers that drain on
  errors), one GPU device per process, and a stream that starts without
  drops on a busy Mac.
- **Faster.** More rounds of [benchmarks against OBS](benchmarks.md) and
  the fixes they point to.
- **0.0.5 follow-ups.** Fixes for [plugins](plugins.md),
  [streaming](streaming.md), the [virtual camera](virtual-camera.md),
  developer mode and automatic updates.

## Ideas

- **Run real OBS plugins:** OBS filters and sources in a helper process
  next to the engine.
- Elgato Stream Deck's own protocol, so its official actions work (today
  Stream Deck works through obs-websocket community plugins).
- [Scenes as code](scenes-as-code.md): keyframes, shaders, TSX and live
  data; WGSL shader filters and WebAssembly sources for
  [plugins](plugins.md).
- A second (vertical) canvas for shorts.
- Projectors and a multiview grid, themes.
- Later: SRT and WHIP streaming, NDI, Syphon, Audio Unit plugins, Windows.

Shipped in 0.0.5: OBS import and Optimize on first launch, the replay
buffer, the virtual camera in the app, ISO capacity warnings, crash-safe
takes on by default, the editor window, obs-websocket v5, several stream
destinations at once, per-ISO audio, per-app audio and VP8 stingers. See
the [README](../README.md#features).
