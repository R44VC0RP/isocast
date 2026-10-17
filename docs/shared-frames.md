# Shared frames: monitors without copying

A native client on the same machine (the Slint app) shows the daemon's
pictures (the program, studio mode's preview, a source) without the
pixels ever being copied, encoded or decoded: `isocastd`'s GPU draws each
frame into a surface in GPU memory that the client's GPU then samples in
place. Method `shared_frames` (`isocast_protocol::api`); the H.264
`frames` stream stays for the web app, remote clients and the CLI.

## How it works

```text
client                                   isocastd
  │ {"method":"shared_frames", target, max_width/height, fps, slots}
  │ ───────────────────────────────────────▶ starts a viewer thread
  │ ◀── reply: transport "iosurface-mach", service, format "rgba8unorm"
  │ ◀── {"ring":{generation,width,height,slots,token}}
  │ ── Mach request (token) to `service` ──▶
  │ ◀── Mach reply: one IOSurface send right per slot
  │ ◀── {"frame":{generation,slot,time,revision}}   (per frame)
  │ ── {"release":{generation,slot}} ──────▶ (when its GPU is done)
  │ ── {"fps":30} / {"size":{max_width,max_height}} ─▶ (any time)
```

- **The ring.** Each viewer gets its own ring of 2 to 8 (default 4)
  `IOSurface`s, 8-bit RGBA, sized to fit the box it asked for (keeping the
  picture's shape, never larger than the target). The surfaces travel once
  per ring; a resize (`size` line) or a change in the target's size makes
  a new ring (`generation` + 1), and slots of an old ring are ignored.
- **A frame.** The viewer's thread picks frames like the H.264 monitors do
  (`Pace` in `frame_stream.rs`: the viewer's rate, on the canvas's frame
  grid, the newest again for a still picture), converts the program's
  BGRA canvas and scales it in one GPU pass (the compositor's `Renderer`,
  area filter when shrinking) into a free slot. Captured sources in Y'CbCr
  are converted in that pass. A command-buffer completion callback sends
  `{"frame"}` only after the GPU finishes; the viewer's thread does not
  wait for each draw. So whatever the client samples is
  complete; no cross-process GPU fence is needed.
- **Release.** A slot stays the client's until it sends `release`. The
  client must keep a frame until its own GPU has finished reading it (in
  practice: until the next frame has been drawn on screen), then release
  it. With every slot held, the daemon draws nothing and stops asking for
  the program, so a stuck or hidden client costs nothing; it only stalls
  itself (rings are per viewer).
- **One device.** Compositors, ISO conversions, scene feeds, viewers and
  the virtual camera share one process-lifetime `Gpu`. One completion
  thread services its callbacks; each producer has at most three pictures
  in flight. Native buffers (including cached imports) are retained until
  their GPU reads/writes finish. Producer-local fences preserve publication
  order even when wgpu runs callbacks on different submitting threads.
- **Revisions.** Program and preview frames carry the setup revision they
  were drawn from, as the H.264 stream's revision records do, so the app
  draws its selection box at the layout the picture shows: box and picture
  move in the same frame.
- **Demand.** Asking for frames keeps the program (or preview) drawn at
  the viewer's rate, through the same viewer-rate mechanism as `frames`
  (`slot_of` → `want_program`), and keeps the live engine running.

Code: `crates/isocast-surfaces` (surfaces, the Mach hand-over, wrapping a
surface as a wgpu texture), `crates/isocast-engine/src/record/shared_frames.rs`
(the viewer thread), `stream_shared_frames` in the daemon,
`isocast_client::shared` (the client API) and `isocast frames --shared`.

## Handing IOSurfaces to another process

A Unix socket can't carry an IOSurface (it has no file descriptor form),
so the daemon needs a Mach channel the client can find, even though the
client didn't start the daemon (the CLI usually does, via `posix_spawn`).

**Chosen: a Mach service checked in with the bootstrap server under the
daemon's own name**, `dev.isocast.isocastd.<pid>.surfaces`
(`bootstrap_check_in`, done lazily on the first `shared_frames`). launchd
lets an ordinary process check in a name nobody holds in its bootstrap
namespace (the user's login session) and drops the name when the process
exits. The daemon tells the client the name in the reply, and a random
128-bit token per ring over the control socket; the client sends the
service a request (token, reply port) and gets one send right per surface
(`IOSurfaceCreateMachPort`), which it opens with
`IOSurfaceLookupFromMachPort`. A token works once and is withdrawn when the
ring is replaced or the stream ends, so another process of the user can't
fetch the pictures by guessing.

Verified on this Mac (macOS 27.2, 2026-10-12): check-in of an undeclared
name from a terminal-launched process, and lookup from both a
terminal-launched process and an app launched through LaunchServices
(`open`), with the surface's pixels intact on the other side; and through
the real daemon launched by the CLI (`isocast frames --shared --check`).

Alternatives considered:

- **XPC** (`IOSurfaceCreateXPCObject` over an XPC connection): the clean,
  current API, but a client can only connect to a launchd-registered
  service (a LaunchAgent plist or `SMAppService`) or to an anonymous
  endpoint it was already sent over XPC. That means changing how the
  daemon is launched (launchd-managed rather than spawned by the CLI with
  its own TCC responsibility) and breaks per-socket private engines
  (`ISOCAST_SOCKET`) that agents and tests rely on. Worth revisiting when
  Isocast ships as an app that registers its daemon with `SMAppService`:
  the same ring/slot protocol carries over, only the hand-over changes.
- **Global IOSurface ids** (`kIOSurfaceIsGlobal` + `IOSurfaceLookup`):
  deprecated since macOS 10.11, and any process could look the pictures
  up. Not used.
- **A Mach port handed down at launch** (a task special port or an
  inherited bootstrap subset): only works when the client launched the
  daemon; the CLI usually does.
- **Shared memory and an upload per frame**: a copy each frame on both
  sides, which is what this replaces.

Risks:

- A daemon started in another login session (over SSH, or by launchd as a
  daemon) checks its name into that session's namespace; a GUI app can't
  see it. `fetch` says so, and the app can fall back to `frames` (H.264).
- A sandboxed app (Mac App Store) may look up only the Mach names its
  entitlements list: it would need
  `com.apple.security.temporary-exception.mach-lookup.global-name` for the
  `dev.isocast.isocastd.` prefix, which App Review discourages; the
  XPC/`SMAppService` route is the answer there.
- launchd's acceptance of undeclared check-ins is long-standing behaviour
  (Chromium's Mach rendezvous relies on it) but not a documented promise.
  If a future macOS refuses it, the error names the service and
  `shared_frames` fails cleanly; the H.264 path is unaffected.
- GPU memory per viewer: slots × width × height × 4 bytes (4 × 1600×900:
  23 MB).

## The client API

```toml
isocast-client = { path = "../isocast-client", features = ["wgpu"] }
```

```rust
use isocast_client::Client;
use isocast_protocol::api::SharedFramesParams;

let mut frames = Client::connect_or_launch()?.shared_frames(SharedFramesParams {
    target: "program".into(),
    max_width: Some(1600),
    max_height: Some(900),
    fps: Some(60),
    slots: None,
})?;
let control = frames.controller(); // set_fps / set_size from any thread
let mut showing = None;
loop {
    let Some(frame) = frames.newest(None)? else { continue };
    // An Rgba8Unorm texture of `device` over the shared surface, made once
    // per slot: TEXTURE_BINDING | RENDER_ATTACHMENT | COPY_SRC.
    let texture = frame.texture(&device)?;
    // frame.revision: the setup revision to draw marks at.
    // frame.time: host-clock ns (isocast_surfaces::host_now()).
    show(texture, frame.revision);
    // Dropping a frame releases its slot: keep the one on screen until
    // the next has been drawn.
    showing = Some(frame);
}
```

- `SharedFrames::next(timeout)` returns frames in order (`Some(ZERO)`
  polls); `newest(timeout)` skips to the newest one that has arrived,
  releasing the ones passed over. Rings are fetched inside them.
- `SharedFrame::surface()` is the `IOSurface` (`isocast_surfaces::Surface`,
  `io_surface()` for the raw `IOSurfaceRef`); `texture(&device)` wraps it
  for wgpu 30 (Metal) in place, cached per slot and device.
- When the window is hidden (occluded or minimized), close the stream, stop
  releasing, or ask for 1 fps (what `isocast-ui` does) so the daemon
  hardly draws for it.

## In the Slint app (`isocast-ui`)

`crates/isocast-ui/src/frames.rs` receives on a thread and hands each
frame to the window's thread (`slint::invoke_from_event_loop`);
`preview.rs` shows it with the selection box at the layout of the frame's
`revision` (layouts are kept per revision from `get` and from the app's
own live applies; a frame from a revision the app hasn't read yet waits,
at most 150 ms, for its layout). Two ways to put it on screen:

- **macOS (default): Core Animation layers** (`layers.rs`). The surface
  itself is the `contents` of a `CALayer` under Slint's Metal layer, and
  the box (outline and handles) is layers over it; picture and box change
  in one `CATransaction`, so they reach the screen together. Slint's
  window background is transparent over the preview (the `NSWindow` is
  kept opaque), so whatever Slint draws over the preview (a dialog, a
  popup) still covers it, and Slint redraws only when the interface
  changes rather than for every video frame. This is what keeps the app
  at about 1.5% of a core at 60 fps (Slint redrawing the whole window per
  frame costs about 9%).
- **Portable (`--preview image`)**: `slint::Image::try_from(texture)`
  (`Rgba8Unorm`, `TEXTURE_BINDING | RENDER_ATTACHMENT`, which these
  textures have), with the box drawn by Slint, set in the same event. On
  Windows the counterpart of the layers is a DirectComposition visual
  under a transparent (premultiplied) Slint swap chain; not built yet.

Either way a frame is kept until the next-but-one arrives (the window
server or the GPU may still be reading the one just replaced), so with the
default ring of 4 the daemon always has two free slots. Values are the
canvas's Rec. 709 RGB shown as they are; the layers don't colour-manage
them yet (the web app's video path does), see the notes.

## Checking it

```sh
isocast frames program --shared --check --seconds 5 \
  [--width 1600] [--height 900] [--fps 60] [--slots 4] \
  [--expect 0.885,0.185=#2050c0] [--touch] [--json]
```

receives frames through shared surfaces and reads every one back through
this process's own GPU, from the shared surface itself: 64 pixels in a
grid (fails on a black frame, or on alpha below 255, which is a slot
announced before it was drawn), the centre, and each `--expect` pixel
(`X,Y` as fractions of the picture), counted per setup revision. `--touch`
applies an empty live change halfway (the setup stays as it is, its
revision goes up) and checks that frames show the new revision. It reports
the rate, how late frames arrive after their time (host clock), rings and
revisions. Exit status 1 when a check fails.

## Measured (2026-10-12, M5 Max, release build)

A private engine with a synthetic test pattern and a colour swatch on a
1920×1080 @ 60 canvas, the viewer at 1600×900 and 60 fps:

| | shared frames | H.264 `frames` |
|---|---|---|
| rate | 60.1 fps | 56–60 fps |
| latency, a source (frame time → client) | 3.2 ms median, p99 6.8 ms | 27.8 ms median, before decoding |
| latency, the program | 20.3 ms median (the program pipeline; the canvas tick is its time) | 23.5 ms median, before decoding |
| daemon CPU, a source at 60 fps | 5.1% of a core (1.9% with no viewer) | 5.4% |
| daemon CPU, the program at 60 fps | 9.1% (includes drawing the program) | 10.0% |
| client CPU, receiving only | 0.2% | 0.2% (plus decoding) |

So the transport costs the daemon about 3% of a core per 60 fps viewer
(waking, the scale pass, waiting for the GPU, a JSON line) and the client
almost nothing before it draws. A 10-minute soak (debug build, every frame
read back): 35,984 frames at 59.97 fps, no black or undrawn frames, the
swatch's pixel in every frame, a live change's revision shown 6 ms after
it. Moving the swatch with a live apply mid-run: every frame labelled with
the old revision showed it in the old place, every frame with the new one
in the new place. 90 short sessions left the daemon's memory flat.

## Windows (design only)

The same protocol, with D3D12 shared resources instead of IOSurfaces and
the control connection on a named pipe:

- **Surfaces.** The daemon creates the ring as committed `ID3D12Resource`
  textures (`DXGI_FORMAT_R8G8B8A8_UNORM`, render target,
  `D3D12_HEAP_FLAG_SHARED`), and one shared `ID3D12Fence`
  (`D3D12_FENCE_FLAG_SHARED`). `ID3D12Device::CreateSharedHandle` makes an
  NT handle for each.
- **Hand-over.** The client sends its process id with `shared_frames`; the
  daemon opens the client (`OpenProcess(PROCESS_DUP_HANDLE)`, allowed for
  the same user at the same integrity level) and `DuplicateHandle`s each
  handle into it, then sends the duplicated handle values in the `ring`
  line (`transport: "d3d12-handle"`, no token or service needed). The
  client opens them with `ID3D12Device::OpenSharedHandle` and closes the
  handles. Both must use the same adapter (the reply names its LUID; the
  client creates its device, or Slint's, on that adapter).
- **Synchronisation on the GPU.** The daemon's queue signals the shared
  fence after drawing a slot, and the `frame` line carries that fence
  value; the client's queue `Wait`s for it before sampling, so neither CPU
  waits. Releases can stay as lines (the daemon then knows the slot is
  free once the client's own fence, also shared, passes the value the
  client signalled after its last read).
- **wgpu.** `wgpu::hal::dx12::Device::texture_from_raw` wraps the
  opened resource; the app must run Slint's wgpu 30 on the DX12 back end
  (not Vulkan) to share it. The fence wait needs raw `ID3D12CommandQueue`
  access through `as_hal`.
- **Code.** A `windows` module in `isocast-surfaces` beside `mac.rs` with
  the same `Surface`/`Exporter`/`fetch` shape (fetch becomes "open these
  handles"), and the platform crate's `shared_surface`; the engine's
  viewer thread and the protocol stay as they are.
