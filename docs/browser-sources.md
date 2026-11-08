# Browser sources

Browser sources (`kind = "browser"`) render a local file or URL at a configured
`width`, `height` and `fps` with transparent background, optional custom CSS,
and optional isolated page audio (`control_audio = true`, the default).

## Process model per platform

- **macOS (`crates/isocast-platform-macos/src/browser/`):** Each browser source
  owns an offscreen `NSWindow` and `WKWebView` on `isocastd`'s main thread
  (WebKit's WebContent, GPU and Networking work runs in Apple's own XPC helper
  processes).
- **Windows (`crates/isocast-browser/src/win.rs`, `crates/isocast-browser-host`):**
  Each browser source runs in a dedicated `isocast-browser.exe` helper process
  inside a kill-on-close Job Object, hosting a `CoreWebView2CompositionController`
  whose root `Visual` is captured offscreen with `Windows.Graphics.Capture`
  (`GraphicsCaptureItem::CreateFromVisual`).
- **Linux (`crates/isocast-browser/src/linux.rs`, `crates/isocast-platform-linux/src/browser.rs`):**
  Each browser source runs in a dedicated `isocast-browser` helper process on a
  private `gtk4-broadwayd` display with `webkit6::WebView`, supervised with
  memory and hang watchdogs.

## Video capture reference points and pacing

Every browser video frame is stamped on the engine's host clock (`Time`) at its
capture reference point rather than when pixels finish readback or arrive over
the helper pipe:

- **macOS:** A host-clock deadline pacer (`pacing::sleep_until`, backed by an
  absolute `NOTE_CRITICAL | NOTE_MACHTIME` kqueue timer) ticks at the source's
  configured `fps` (`30 Hz` for a `30 fps` source) and dispatches
  `takeSnapshotWithConfiguration` on the main queue when no snapshot is in
  flight. Frames are never retried back-to-back on slow hosts, avoiding
  WebContent main-thread starvation. Each snapshot's reference timestamp is
  `host_time() - 1 / (2 * fps)` (`16.67 ms` at `30 fps`, `8.33 ms` at `60 fps`,
  the midpoint of the snapshot polling interval) captured when the snapshot is
  requested, before `NSImage` rasterization.
- **Windows:** `isocast-browser.exe` subscribes to
  `Direct3D11CaptureFramePool::FrameArrived`. When a pacer tick precedes DWM's
  composition vblank (`TryGetNextFrame` empty), the helper wakes immediately on
  `FrameArrived` and phase-aligns its `fps` pacer to DWM's vblank instead of
  waiting another full `1 / fps` interval. After staging readback and
  unpremultiply, the helper emits a `FRMT` record carrying
  `age_us = (now_qpc - frame.SystemRelativeTime())`, and `isocast-browser-host`
  stamps the frame at `arrived - min(age_us, 500 ms)`.
- **Linux:** `isocast-browser` paces `webkit6::WebView::snapshot` on a monotonic
  `1 / fps` schedule (skipping ticks while a snapshot is in flight rather than
  running snapshots back-to-back) and emits `FRMT` with
  `age_us = 1 / (2 * max(fps, 60)) + snapshot_and_download_elapsed`.
  `isocast-platform-linux` stamps the frame at `arrived - min(age_us, 500 ms)`.

On every platform, computed frame timestamps are clamped to at least
`previous_frame + 1 ns` so variable snapshot or readback durations can never
reorder frame timestamps.

### Why macOS polls at `fps` instead of 60 Hz

Polling `takeSnapshotWithConfiguration` at `max(fps, 60)` was measured against
polling at the source's configured `fps` (`30 fps`, 20 s steady-state H.264
program + ISO recording on Apple M2, macOS 26.7):

| Build / snapshot rate | Size @ 30 fps | `isocastd` CPU | `WebContent` CPU | `WebKit.GPU` CPU | Total CPU |
|---|---|---:|---:|---:|---:|
| `main` (`30 Hz` `NSTimer`, completion stamp) | `320x180` | `21.40%` | `10.38%` | `7.38%` | `39.71%` |
| `60 Hz` poll + request stamp | `320x180` | `23.09%` | `11.37%` | `9.03%` | `44.09%` |
| **`fps` (`30 Hz`) kqueue pacer + request stamp** | `320x180` | **`21.60%`** | **`10.48%`** | **`7.33%`** | **`40.01%`** |
| `main` (`30 Hz` `NSTimer`, completion stamp) | `1920x1080` | `17.27%` | `5.74%` | `16.62%` | `39.93%` |
| `60 Hz` poll + request stamp | `1920x1080` | `22.31%` | `5.74%` | `27.40%` | `55.75%` |
| **`fps` (`30 Hz`) kqueue pacer + request stamp** | `1920x1080` | **`17.36%`** | **`5.89%`** | **`16.72%`** | **`40.27%`** |

At `1920x1080@30`, polling at `60 Hz` cost `+15.8` percentage points of CPU
(`WebKit.GPU` `16.6% -> 27.4%`, `isocastd` `17.3% -> 22.3%`). Replacing
`NSTimer` (which lost 112 ticks over 62 s to RunLoop timer coalescing) with
`pacing::sleep_until` at exact `fps` (`30 Hz`) keeps CPU identical to `main`
(`40.27%` vs `39.93%`) while delivering `1858/1860` unique snapshots in 62 s
with `0.00 ms` A/V drift (`-28.77 ms` median).

## Page audio capture and the A/V sync model

When `control_audio = true`, `audio.js` redirects the page's WebAudio connections
to `AudioContext.destination` and same-origin `<video>`/`<audio>` elements
(`createMediaElementSource`) into a silent tap (`AudioWorkletNode`, with a
`ScriptProcessorNode` fallback for strict CSP pages) so page sound goes to the
recorder instead of the host's speakers.

### Run-anchored wall-clock timing and slew bound

1. `audio.js` buffers 1,024 stereo frames (`21.33 ms` at 48 kHz) and posts each
   chunk with `Date.now()` in microseconds (`endUs`) plus a `resumed` flag (`+`
   after an intentional `suspend()`/`resume()`, context handoff, or worklet gap;
   `@` for continuous chunks). An uncaptured keepalive oscillator feeds input 1
   of `AudioWorkletNode` (`process()` reads only `inputs[0]`) so WebKit never
   marks a silent offscreen `WKWebView` graph idle and throttles `AudioContext`
   rendering to `2 Hz` (`~520 ms` quanta).
2. To distinguish an involuntary `>= 30 ms` render hiccup right before
   `suspend()` from a late `pause` message arriving after the pause gap, the
   worklet records `gapEndMs` and only consumes `pause` immediately when
   `gapEndMs > pause.at`; otherwise it flushes pre-pause frames as `Continuous`
   and keeps `pauseArmed` set for the real suspend hole.
3. Output-latency compensation is locked once per continuous run (as soon as the
   backend reports a non-zero `outputLatency`, resetting only on `resumed`), and
   `BrowserAudioStamper` (`crates/isocast-engine/src/time.rs`) stitches
   `Continuous` chunks sample-accurately (`at = next`) from the run's origin, so
   a host default-output change mid-run cannot jump timestamps or synthesize a
   false gap.
4. Over long takes, `PageClock::observe_wall` runs a 64-chunk (`~1.37 s`)
   tumbling min-filter on `wall_us - end_us` (picking the lowest-IPC-delay chunk
   in each window) so NTP wall-clock slew relative to the monotonic host clock
   cannot accumulate past `MAX_LAG` (`195 ms`).

### Output-latency model: `<video>` elements vs WebAudio

A recording should match what a viewer of the page would see and hear, without
baking an unrelated Bluetooth headphone delay into WebAudio animations when the
captured audio never plays through those headphones. Hardware measurements
across `<video>` MP4 playback and WebAudio (`raw currentTime` vs
`getOutputTimestamp()`) revealed two distinct browser behaviors:

1. **`<video>` and `<audio>` media elements (the common case):**
   - **Apple WebKit (`WKWebView` on macOS):** `createMediaElementSource` attaches
     a CoreMedia `MTAudioProcessingTap` to `AVPlayerItem`. Direct probing inside
     `WKWebView` showed that `MTAudioProcessingTap` delivers decoded PCM **`99 ms`
     ahead of `video.currentTime`**, while `AVPlayer` simultaneously delays
     `<video>` frames on screen by the default CoreAudio output device's full
     `outputLatency` (`10 ms` on built-in speakers, `90 ms` on a 90 ms virtual
     device, `200 ms` on a 200 ms virtual device). `audio.js` therefore inserts
     a `100 ms` `DelayNode` on `createMediaElementSource` on Apple WebKit and
     compensates media routes by `baseLatency + outputLatency` up to `250 ms`.
   - **Chromium (`WebView2` on Windows) and WebKitGTK (Linux):** Calling
     `createMediaElementSource` detaches the media element from direct hardware
     output and clocks video presentation from WebAudio's pull clock (`baseLatency`
     only, ignoring `outputLatency`). Adding WASAPI's `40 ms` `outputLatency` on
     Windows pushed `<video>` audio `+54.29 ms` late, whereas compensating by
     `baseLatency` (`10 ms`) aligns `<video>` at **`-7.81 ms`**.
2. **Pure WebAudio (`rAF` + `AudioContext`, no media elements):**
   - **Apple WebKit (`macOS`):** `AudioContext.getOutputTimestamp().contextTime`
     equals `currentTime` (`0.0 ms` lag regardless of `outputLatency`), and
     offscreen `WKWebView` snapshots have `~10 ms` visual latency. Applying
     unscoped `90 ms` or `200 ms` output latency shifted WebAudio `+65.6 ms` to
     `+98.4 ms` late; capping pure-WebAudio compensation on Apple WebKit at
     `25 ms` keeps built-in speakers (`12.7 ms`) and high-latency virtual
     outputs (`90 ms`, `200 ms`) within one frame (`-3.46 ms` to `-28.77 ms`).
   - **Chromium (`Windows`) and WebKitGTK (`Linux`):** Offscreen Chromium + DWM
     composition has `~66.7 ms` visual pipeline latency, matching WASAPI's
     `10 + 40 = 50.0 ms` `baseLatency + outputLatency` (`55 ms` cap). Pages
     keying `rAF` to `ctx.currentTime` align at **`-16.15 ms`** (`-17.40 ms`
     over 60 s; `-12.19 ms` Take 3 median on `exon-desk`). Pages on Chromium
     that additionally delay their own `rAF` draw
     by `ctx.getOutputTimestamp()` (`53.5 ms` behind `currentTime`) draw
     `~70–83 ms` after the audio render time (`-82.81 ms`), matching what a
     viewer on the local monitor sees because the page starts painting only
     after the sample has already reached the speakers.

### Measured A/V offsets (`beep_pts - flash_pts`, 30 fps)

| Platform & default output | `base + output` | `<video>` MP4 (`pulse22.mp4`) | WebAudio (`currentTime` / `rAF`) | WebAudio (`getOutputTimestamp` / `rAF`) |
|---|---:|---:|---:|---:|
| macOS M2 — built-in speakers | `2.7 + 10.0 = 12.7 ms` | **`-2.19 ms`** (was `-114.3 ms`) | **`-28.77 ms`** 60 s / **`+7.85 ms`** 16 s (was `-44.52 ms`) | **`-25.48 ms`** |
| macOS M2 — virtual 90 ms output | `2.7 + 90.0 = 92.7 ms` | **`-15.7 ms`** (unscoped/no-delay `-115.7 ms`; uncomp `-208.4 ms`) | **`-3.46 ms`** (unscoped `+65.60 ms`) | **`-3.46 ms`** |
| macOS M2 — virtual 200 ms output | `2.7 + 200.0 = 202.7 ms` | **`+2.2 ms`** (120 ms cap/no-delay `-180.5 ms`; uncomp `-300.5 ms`) | **`<= +26 ms`** (120 ms cap `+98.44 ms`) | **`<= +26 ms`** |
| Windows RTX 5070 — USB WASAPI | `10.0 + 40.0 = 50.0 ms` | **`-7.81 ms`** (`+54.29 ms` with `outputLatency`) | **`-16.15 ms`** 16 s / **`-17.40 ms`** 60 s (was `-87.73 ms`) | `-82.81 ms` (page-delayed draw + 67 ms DWM) |
| Linux arm64 VM — PipeWire null | `2.7 + 0.0 = 2.7 ms` | **`+17.19 ms`** | **`-5.81 ms`** 16 s / **`-21.17 ms`** 60 s (was `-40.54 ms`) | **`-5.81 ms`** |

### End-to-end test coverage (`scenario_browser_audio_gap` Take 3)

`scenario_browser_audio_gap` Take 3 (`scripts/e2e.py`) checks simultaneous
canvas flash + WebAudio beep pulses on both `web` and `program` using
`browser_sync_check`:

- **Why `browser_sync_check` differs from `test_pattern` + `tone`:** Engine-internal
  `test_pattern` + `tone` share one sample/slot clock, so a flash is never split
  across adjacent slots. An asynchronous browser page (`requestAnimationFrame`
  at `~60 Hz`) captured at `30 fps` (`33.33 ms` slots) can land a flash in either
  adjacent slot even at zero mean offset (`e.g. +33.85 ms` when a `0.52 ms` beep
  lands just past slot `N` while `rAF` painted into slot `N - 1`). Take 3
  therefore uses a per-pulse bound of **`1 / fps + 2 ms`** (`35.33 ms` at
  `30 fps`) to cover adjacent-slot quantization plus jitter, steps its pulse
  period by **`26 / 27 s`** (`28 + 8/9` frames at `30 fps`) so 9 interior pulses
  sample all 9 sub-frame phases rather than aliasing to one fixed phase, and
  enforces **`|median(beep - flash)| <= 0.5 / fps`** (`16.67 ms` at `30 fps`) so
  any constant shift of half a frame or more fails.
- **Hardware vs hosted runners:** Test Macs and Windows GPU hardware (`exon-desk`)
  run Take 3 strictly (`lost = None`, zero writer drops, zero audio dropouts,
  `1 / fps + 2 ms`, and `|median| <= 0.5 / fps`). Hosted Linux (`--hosted` on
  software `gtk4-broadwayd` + `llvmpipe` + PipeWire `auto_null`) uses a
  hold-accounted per-pulse bound of **`1 / fps + 1 / 60 + 2 ms`** (`52.0 ms` at
  `30 fps`, with no median gate) because two free-running `30 Hz` timers
  (`isocast-browser`'s `glib::timeout` snapshot loop and `isocastd`'s
  `IsoRunner`) lock to a random phase in `[0, 33.3 ms)` per run and combine with
  `60 Hz` `rAF` (`16.7 ms`) for a `50.0 ms` physical span, while still catching
  any shift of `>= 2` frames (`66.7 ms`). Hosted `macos-15` VMs (no CoreAudio
  hardware, leaving WebKit's dummy `AudioWorklet` `nanosleep` timer subject to
  `> 150 ms` timer-coalescing stalls, #203) and Windows software-WARP VMs
  (`windows-latest` and `aws-win`, which have no GPU and no audio output device,
  so `AudioContext` reports `0 ms` WASAPI latency against `32–66 ms` CPU
  Chromium + WARP DWM composition) skip Take 3 while keeping `win_browser`
  strict on `windows-latest`.
