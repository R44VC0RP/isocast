# Coming

These are being built or planned and are not in a release yet. Each will
be on the API, the CLI and MCP, as well as in the app.

## Next

- **Windows,** in 0.0.11, once its installer is code-signed.
- **Linux window sources,** through the desktop's screen-sharing picker.
- **Linux automatic updates.** Today a Linux update is a manual install of
  the newer `.deb`.
- **Faster.** More rounds of [benchmarks against OBS](benchmarks.md) and
  the fixes they point to.
- **More remote control.** Live video for every monitor, not just the
  program, and a choice of where its sound plays.
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
- Later: SRT and WHIP streaming, NDI, Syphon, Audio Unit plugins.

In 0.0.10: Isocast on Linux (Ubuntu 24.04 and Debian 12), flips and
previews that give way to the program. In 0.0.9: per-take NLE export replaces the in-app editor; remote control
with program video and sound, OpenRPC and screen colour-depth settings.
Shipped in 0.0.8: a canvas for every destination, advanced masks and
automatic quality. Earlier: projectors and
multiview, the vertical canvas, OBS import, the replay buffer,
the virtual camera and obs-websocket. See the [README](../README.md#features).
