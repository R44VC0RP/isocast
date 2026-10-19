# Coming

These are being built or planned and are not in a release yet. Each will
be on the API, the CLI and MCP, as well as in the app.

## Next

- **Windows.** Recording already works end to end in testing: screen,
  camera and microphone to the program and ISO files, with previews,
  crash-safe files and an installer with signed updates. Next: streaming,
  media playback, a faster start, the matched OBS comparison and an alpha.
- **Faster.** More rounds of [benchmarks against OBS](benchmarks.md) and
  the fixes they point to.
- **Follow-ups** for canvases (a separate audio mix per canvas, importing
  OBS 31 canvases), masks and automatic quality.

## Ideas

- **Run real OBS plugins:** OBS filters and sources in a helper process
  next to the engine.
- Elgato Stream Deck's own protocol, so its official actions work (today
  Stream Deck works through obs-websocket community plugins).
- [Scenes as code](scenes-as-code.md): keyframes, shaders, TSX and live
  data; WGSL shader filters and WebAssembly sources for
  [plugins](plugins.md).
- Later: SRT and WHIP streaming, NDI, Syphon, Audio Unit plugins, Linux.

Shipped in 0.0.8: a canvas for every destination, advanced masks,
automatic quality and recording targets. Earlier: projectors and
multiview, the vertical canvas, the editor, OBS import, the replay buffer,
the virtual camera and obs-websocket. See the [README](../README.md#features).
