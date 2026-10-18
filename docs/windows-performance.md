# How OBS gets its Windows performance, and how Isocast beats it in Rust

Research brief for the Isocast Windows lanes (W.5 onward). Written 2026-10-18.

- **Target machine:** RTX 5070 (GB205, **one** 9th-gen NVENC, one NVDEC), Ryzen 7 5700 (Zen 3, 8C/16T, x86-64-v3 capable), Windows 10 Pro.
- **Goal:** at most half of OBS's CPU at 1080p60 and 4K60 (program plus per-source ISO files), lower memory, faster startup, stop in under 20 ms, zero drops.
- **Today:** one window is about 30% below OBS's CPU, and 4K misses real-time deadlines in the first full pipeline.

**Sources read**
- OBS Studio `master` at **`f1c0a116441bdb7a1de7896f810009ea487b8508`** (2026-10-16). Permalink base: `https://github.com/obsproject/obs-studio/blob/f1c0a116441bdb7a1de7896f810009ea487b8508/<path>#L<line>`.
- exeldro/obs-source-record at **`9b5fe689e09d7ddce283ef66bec56a491956114c`**. This is the de facto ISO plugin for OBS, so it is OBS's real ISO baseline.
- Both clones are in `/private/var/folders/2j/6mslx1715gx8frsyn66sf1sh0000gn/T/opencode/obs-research/`.
- The UI tree is now `frontend/`, not `UI/obs-app`.

Throughout, "estimate" means an engineering estimate that has not been measured on exon-desk. Every item says how to measure it.

---

## Part 1 — The 15 changes most likely to cut CPU, memory and latency (ranked)

The ranking weighs expected impact on the stated targets: CPU at most half of OBS's, real-time 4K, stop in under 20 ms and zero drops. Effort comes second.

### 1. Respect the single-NVENC pixel budget: an admission controller with per-session presets

**What**
- The RTX 5070 has **one** NVENC engine, so split-frame encoding cannot apply. SFE needs two or more NVENCs, and the 5070 Ti and up have them.
- One engine encodes about **521 fps of 1080p with HEVC P5/HQ**, **947 fps with P3/HQ**, **181 fps with P7/HQ**, and **317 fps with H.264 P5/HQ**. Throughput scales with the video clock.
- In 1080p60-equivalents, a 4K60 program plus a 4K60 screen ISO plus a 1080p60 camera ISO is 240 + 240 + 60 = **540 fps**. That **exceeds P5 HEVC** (about 104%) and is impossible at H.264 P5 (about 170%).
- Compute the budget before starting. Default ISOs to P1–P3 with no two-pass, no look-ahead and no B-frames. Keep the program on the higher preset only if the budget allows, and refuse or downgrade clearly otherwise.

**Why it is #1:** the "4K misses deadlines" symptom is probably encoder-throughput-bound, not CPU-bound. No amount of CPU work fixes an over-subscribed engine.

**OBS evidence**
- OBS defaults (`plugins/obs-nvenc/nvenc-properties.c:41-61`): P5, `tune=hq`, `multipass=qres` (two-pass), look-ahead when supported, 2 B-frames, adaptive quantisation.
- OBS has no admission control. It just reports "encoding overloaded".

**Docs**
- Per-engine fps table and "single session cannot exceed one NVENC unless SFE": https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-application-note/index.html §4.
- SFE "Number of NVENCs on GPU: 2 or more"; strips are forced down to the NVENC count: https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-video-encoder-api-prog-guide/index.html §8.14.
- RTX 5070 has 1 encoder: NVIDIA Blackwell whitepaper table 6 (https://www.techpowerup.com/gpu-specs/docs/nvidia-blackwell-architecture.pdf) and Puget (https://www.pugetsystems.com/blog/2025/01/21/nvidia-geforce-rtx-50-series-features-for-content-creators/).
- GeForce cards are limited to 8 concurrent sessions per system (app note §3).

**Impact:** decides whether 4K60 with ISOs can be real-time at all. Zero drops depends on this.

**Effort:** S. Ship a static table plus a calibration run per driver, keyed by driver version.

**Measure**
- `nvidia-smi dmon -s u` (`enc` column) or the PDH counter `\GPU Engine(*engtype_VideoEncode)\Utilization Percentage`.
- Per-session submit→completion latency histograms.
- Rerun the failing 4K case at P1/P3 and see whether the misses disappear. That run alone confirms or refutes this hypothesis.

### 2. One device, zero cross-device sync: register the converter's NV12 pool with NVENC once

**What**
- Capture, compositing, NV12 conversion and NVENC share one GPU device, or at most a D3D11 device and the wgpu D3D12 device joined by shared NT handles plus **shared fences**.
- Allocate a small pool of NV12 textures per session (in-flight depth plus 1).
- `nvEncRegisterResource` each one once at session start. The converter renders straight into them.
- No keyed mutexes, no `OpenSharedResource`, no per-frame `CopyResource` into encoder-owned textures.

**OBS evidence (the waste we remove)**
- `plugins/obs-nvenc/nvenc-d3d11.c:25` `d3d11_init` creates a **second D3D11 device** on adapter 0 just for NVENC.
- `:173` `get_tex_from_handle` calls `OpenSharedResource` on libobs's legacy shared handle.
- `:210` `d3d11_encode` does this **every frame per encoder**:
  1. `IDXGIKeyedMutex::AcquireSync(INFINITE)`
  2. a full `CopyResource` into a registered NV12 texture
  3. `ReleaseSync`
  4. `nvEncMapInputResource`
  5. later `nvEncUnmapInputResource` (`nvenc.c:1165`)
- libobs's side: `libobs/obs-video.c:442` `queue_frame` calls `gs_texture_acquire_sync(..., GS_WAIT_INFINITE)` and `gs_texture_release_sync` on the graphics thread. Textures are created with `GS_SHARED_KM_TEX` (`libobs/obs.c:179`).
- One mix has a pool of 10 NV12 keyed-mutex textures (`NUM_ENCODE_TEXTURES`, `libobs/obs-internal.h:52`). Each encoder has `buf_count` more (`nvenc.c:318-324`, at least 4; 12 with 2 B-frames; up to 64 with look-ahead).

**Impact**
- CPU: removes two kernel-mode keyed-mutex transitions, a cross-device flush and a map/unmap pair per frame per session (estimate: tens to low hundreds of µs per frame per session).
- GPU: removes one full-frame copy per session per frame (about 25 MB of read+write per 4K NV12 frame).
- VRAM at 4K: OBS holds 22 NV12 surfaces of 12.4 MB, about 270 MB, per mix-plus-encoder pair. A pool of 4 is about 50 MB.

**Effort:** M. Choose either NVENC's D3D12 interface on the wgpu device (§7.3) or a D3D11 device on the same LUID with shared fences.

**Measure**
- WPR "GPU activity" + CPU sampling → WPA.
- GPUView: count queue packets per frame, look for `DxgkSubmitCommand` from keyed-mutex syncs, and check copy-engine time.
- VRAM: Task Manager "Dedicated GPU memory" or DXGI `QueryVideoMemoryInfo`.

### 3. NVENC async mode with one completion thread per GPU

**What**
- `enableEncodeAsync=1`, one Win32 event per bitstream buffer (`NvEncRegisterAsyncEvent`).
- One completion thread waits on all sessions' events (`WaitForMultipleObjects`, at most 64 handles; 8 sessions × 4–6 buffers fits).
- Lock with `doNotWait` and hand packets to the muxer through a lock-free ring.
- The render/submit thread only calls `nvEncEncodePicture`, which does not block.

**OBS evidence**
- `plugins/obs-nvenc/nvenc.c:166` sets `params->enableEncodeAsync = 0`.
- `get_encoded_packet` (`nvenc.c:1083`) calls `nvEncLockBitstream` with `doNotWait = false` and hides latency only by waiting `output_delay = buf_count - 1` frames (`nvenc.c:326`).
- All of a mix's encoders run **serially on one `gpu_encode_thread`** (`libobs/obs-video-gpu-encode.c:25`). A slow 4K session delays the 1080p one behind it.
- NVIDIA: "Whenever possible, NVIDIA recommends using the asynchronous mode" and "the main encoder thread should be used only … to submit work" (programming guide §6.1, §6.3).

**Impact**
- CPU: low to medium.
- Latency and stop time: high. Output arrives as soon as each frame is done, not after 11 queued frames.
- Removes head-of-line blocking between ISO sessions.

**Effort:** M.

**Measure**
- Encode latency per frame, from the QPC at submit to the QPC at the completion event.
- Thread wait chains in WPA (CPU Usage (Precise), "Ready Thread").

### 4. Encode once, mux many (video and audio)

**What**
- When an ISO's content and settings equal the program's (one full-canvas source) or another ISO's, reuse that session's packets.
- Encode each audio track once and fan it out to every file that wants it.

**OBS evidence**
- OBS shares an encoder between outputs only within one mix (`obs_encoder` with multiple outputs).
- Source Record creates **its own view and video mix per ISO** (`source-record.c:1424-1426`, `obs_view_create` + `obs_view_add2`) and its own encoder per filter, so identical content is encoded twice.

**Impact:** proportional to the sessions removed. One fewer 4K session is −240 fps of 1080p-equivalent NVENC budget plus its CPU.

**Effort:** S–M.

**Measure:** session count in `nvidia-smi` (`nvidia-smi encodersessions` or `--query-gpu=encoder.stats.sessionCount`), plus encoder utilisation.

### 5. Render ISOs without compositing, and skip work when a source did not change

**What**
- An ISO of one source runs one fused "sample, scale, convert to NV12" pass straight from the capture texture into that session's registered NV12 surface. No scene graph and no RGBA intermediate.
- When the source delivered no new frame this tick (WGC delivers only on change), **skip conversion entirely** and re-submit the previous NV12 surface. NVENC turns it into a near-free skip/P frame.
- Apply the same to the program when no input changed.

**OBS evidence**
- Every mix re-renders every tick: `output_frames` → `output_frame` → `render_video` → `render_main_texture` → `obs_view_render` (`libobs/obs-video.c:172, 539, 868, 916`).
- All mixes, including every Source Record ISO, are rendered **serially on the single graphics thread** (`output_frames` loops over `obs->video.mixes`).
- Only same-view, same-size mixes reuse the texture (`can_reuse_mix_texture`, `:136`).

**Impact**
- High for desktops, which are mostly static: estimate 30–70% fewer GPU passes and draw submissions.
- CPU drops with them.

**Effort:** M.

**Measure:** passes per frame (PIX timing capture), GPU 3D engine utilisation, CPU of the render thread. Measure static and moving desktops separately.

### 6. Event-driven capture on its own thread; DXGI duplication for monitors on Windows 10

**What**
- **Windows:** WGC `Direct3D11CaptureFramePool::CreateFreeThreaded` with 2–3 buffers. Handle `FrameArrived` on a capture thread. Do one GPU copy into a pooled shared texture (or sample directly while holding the frame, see §1.1). Release the frame promptly.
- **Monitors on Windows 10:** use DXGI Desktop Duplication on a dedicated thread with a blocking `AcquireNextFrame(timeout)`. WGC on Windows 10 cannot hide the yellow border, because `IsBorderRequired` needs Windows 11. `MinUpdateInterval` and `DirtyRegionMode` exist only on 24H2 (build 26100).

**OBS evidence**
- `libobs-winrt/winrt-capture.cpp:311` uses the non-free-threaded `Create`.
- The DispatcherQueue is created on the **graphics thread** (`winrt-dispatch.cpp` `CreateDispatcherQueueController(DQTYPE_THREAD_CURRENT)`, called from `obs_graphics_thread` → `init_winrt_state`, `obs-video.c:1015`).
- Frames are therefore dispatched by the graphics thread's `PeekMessage` loop (`obs-video.c:1114`). That adds up to one tick of latency and puts the copy (`CopyResource`, `winrt-capture.cpp:173`; their comment says "if they gave an SRV, we could avoid this copy") on the render thread.
- Display capture defaults to DXGI (`plugins/win-capture/duplicator-monitor-capture.c:250` `choose_method`). It is polled with `AcquireNextFrame(0)` once per tick on the graphics thread, followed by a `CopyResource` (`libobs-d3d11/d3d11-duplicator.cpp:256-296`).

**Docs**
- `MinUpdateInterval`, `IncludeSecondaryWindows`, `DirtyRegionMode` (26100+): https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscapturesession
- robmikh: "the capture will go up to the refresh rate, but if content isn't changing you'll get a lower rate": https://github.com/microsoft/Windows.UI.Composition-Win32-Samples/issues/142

**Impact:** CPU low–medium. Latency one tick better. The render thread no longer stalls on capture.

**Effort:** M.

**Measure:** arrival→composite latency per frame in QPC, and render-thread CPU in WPA.

### 7. A frame clock that waits instead of spinning

**What**
- Pace the output tick with a `CreateWaitableTimerExW(CREATE_WAITABLE_TIMER_HIGH_RESOLUTION)` timer (Windows 10 1803+). Use an absolute QPC-derived due time and re-arm every tick.
- Do not spin and do not `Sleep(n-1)`. A missed tick produces an explicit "late" decision (duplicate the previous surface, count it), never a silent burst.

**OBS evidence**
- `libobs/util/platform-windows.c:338` `os_sleepto_ns` calls `Sleep(ms-1)` and then **busy-waits with `QueryPerformanceCounter` + `YieldProcessor()`** until the deadline.
- This runs every tick of the graphics thread (`video_sleep`, `obs-video.c:805`): roughly 1–2 ms of spinning per 16.7 ms frame, about 6–12% of one core at 60 fps (estimate).
- `timeBeginPeriod(1)` is set for the whole process lifetime in `DllMain` (`platform-windows.c:785`).

**Docs**
- Since Windows 10 2004, `timeBeginPeriod` is per-process. On Windows 11, a process that is occluded or windowless loses the raised resolution unless it opts out: https://learn.microsoft.com/en-us/windows/win32/api/timeapi/nf-timeapi-timebeginperiod and https://randomascii.wordpress.com/2020/10/04/windows-timer-resolution-the-great-rule-change/
- Waitable timer flag: https://learn.microsoft.com/en-us/windows/win32/api/synchapi/nf-synchapi-createwaitabletimerexw

**Impact:** CPU medium, the cheapest real win. Lower power.

**Effort:** S.

**Measure**
- Per-thread CPU (ETW sampled and Precise).
- Tick-jitter histogram (actual − due, in µs). Target p99 < 1 ms.

### 8. One submission per tick, nothing allocated per frame

**What**
- Record all of a tick's GPU work (program composite, ISO conversions, preview) into one command encoder or list. Submit once and signal one fence.
- Pre-create pipelines, bind groups, samplers and uniform ring buffers.
- No `Vec`/`Box`/`Arc` allocation, no `create_*` calls and no COM `AddRef`/`Release` churn on the hot path. Borrow interfaces (`&ID3D12Resource`) rather than cloning them; in windows-rs, `Clone` on an interface is an `AddRef`.

**OBS evidence**
- OBS is better than its reputation here: D3D11 state objects are cached and buffers use `D3D11_MAP_WRITE_DISCARD`.
- But each mix and display is its own `gs_begin_scene`/`gs_end_scene`, plus an explicit `gs_flush` per mix (`obs-video.c:896`).
- The NVENC device submits separately.
- Async sources copy every frame into a cache (`cache_video`, `libobs/obs-source.c:3542`) and upload it with `Map(WRITE_DISCARD)` (`gs_texture_set_image`).

**Impact:** CPU medium, mostly in the driver's user-mode and kernel submission paths.

**Effort:** M.

**Measure:** allocations per frame (a counting `GlobalAlloc` wrapper in debug, or ETW heap tracing), `ExecuteCommandLists` calls per frame (PIX), driver CPU in WPA (stacks in `nvwgf2umx.dll`/`d3d12.dll`).

### 9. Bounded frames in flight, and no blocking readback on the render thread

**What**
- 2–3 frames in flight, gated by fence values.
- GPU→CPU readback only for explicit raw consumers (thumbnails, CPU encoders, analysis). Do it on a worker thread that waits on the fence event and then maps.
- Never `Map` with an implicit wait on the render thread.

**OBS evidence**
- `IDXGIDevice1::SetMaximumFrameLatency(16)` (`libobs-d3d11/d3d11-subsystem.cpp:417`, "prevent stalls sometimes seen in Present calls") lets the CPU queue up to 16 frames ahead.
- Raw outputs do a blocking `Map(D3D11_MAP_READ)` (`d3d11-subsystem.cpp:2866`) on the graphics thread one frame later (`download_frame`, `obs-video.c:583`; `NUM_TEXTURES 2`).
- The raw frame is then **memcpy'd** into the `video-io` cache of up to 16 frames (`set_gpu_converted_data`, `obs-video.c:618`; `MAX_CACHE_SIZE 16`, `libobs/media-io/video-io.c:35`).
- OBS does skip staging when only GPU encoders are active (`raw_active` gating in `render_video`). Keep that property.

**Impact:** latency and memory; drops under load.

**Effort:** S.

**Measure:** queue depth (fence value delta), staging bytes per second, render-thread waits in WPA.

### 10. GPU scheduling priority without elevation where possible

**What**
- Create the compositor/encode command queue with `D3D12_COMMAND_QUEUE_PRIORITY_HIGH`, which needs no admin. This means a raw queue that we own, or a wgpu hal-level queue.
- Optionally call `D3DKMTSetProcessSchedulingPriorityClass(HIGH)` when allowed, as OBS does: `HIGH` under HAGS, `REALTIME` otherwise.
- Never require "Run as administrator".

**OBS evidence**
- `libobs-d3d11/d3d11-subsystem.cpp:436` `set_priority`: `D3DKMT_SCHEDULINGPRIORITYCLASS_HIGH` if HAGS is enabled, else `REALTIME`, plus `IDXGIDevice::SetGPUThreadPriority`.
- It is enabled only in official builds (`GPU_PRIORITY_VAL` comes from a CI secret, `libobs-d3d11/CMakeLists.txt:28-33`) and works only elevated: it logs "failed (not admin?)".
- OBS logs a warning when HAGS is on (`:651`).

**Docs**
- OBS KB, "Run OBS as Administrator" to reserve GPU: https://obsproject.com/kb/encoding-performance-troubleshooting
- 2019 GPU-priority fix thread: https://obsproject.com/forum/threads/obs-studio-24-0-3-gpu-priority-fix-testing.111669/
- Queue priority: https://learn.microsoft.com/en-us/windows/win32/api/d3d12/ne-d3d12-d3d12_command_queue_priority

**Impact:** zero drops while a game saturates the GPU. This is OBS's #1 field problem.

**Effort:** S.

**Measure:** "lagged/late" counters while running a GPU-saturating load (FurMark or a game uncapped), with HAGS on and off.

### 11. CPU scheduling and power knobs, scoped to recording

**What**
- MMCSS (`AvSetMmThreadCharacteristicsW(L"Pro Audio")` or `L"Audio"`) for WASAPI capture and the audio mixer.
- `THREAD_PRIORITY_ABOVE_NORMAL` (not time-critical) for the frame clock and NVENC completion threads.
- Opt out of EcoQoS/power throttling: `SetProcessInformation(ProcessPowerThrottling, EXECUTION_SPEED | IGNORE_TIMER_RESOLUTION, state 0)`. This matters because **isocastd is windowless**, which Windows 11 treats as "invisible".
- If a timer-resolution request is still needed, scope it to an active recording (`timeBeginPeriod`/`timeEndPeriod`).
- Leave the process priority class at Normal by default. OBS exposes it as a user setting only (`frontend/utility/platform-windows.cpp:191` `SetProcessPriority`).

**OBS evidence**
- MMCSS "Audio" on the libobs audio thread (`libobs/media-io/audio-io.c:209`) and on each WASAPI capture thread (`plugins/win-wasapi/win-wasapi.cpp:1105`).
- The RTMP send thread is `ABOVE_NORMAL` (`plugins/obs-outputs/rtmp-windows.c:227`).
- No video thread uses MMCSS.
- There is no power-throttling opt-out anywhere in the tree (grep for `POWER_THROTTLING` is empty).

**Docs**
- https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-setprocessinformation (`PROCESS_POWER_THROTTLING_*`)
- https://learn.microsoft.com/en-us/windows/win32/procthread/multimedia-class-scheduler-service

**Impact:** robustness against drops; idle power. It does not reduce CPU.

**Effort:** S.

**Measure:** drops under a CPU-saturating load (for example `prime95`, or `cargo build -j16`); ETW ReadyThread delays for these threads.

### 12. Lean audio: event-driven, device timestamps, minimal resampling, shared AAC

**What**
- Shared-mode WASAPI with `AUDCLNT_STREAMFLAGS_EVENTCALLBACK`, 10–20 ms buffers, and QPC device timestamps (`GetBuffer`'s `pu64QPCPosition`).
- Use process loopback (`AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`, Windows 10 2004+) for per-app ISOs.
- Mix in f32 with SIMD. Resample only when the device rate ≠ 48 kHz, with a fixed-ratio polyphase resampler.
- Encode AAC once per track (§4). Choose Media Foundation AAC (in the box, no FFmpeg) or FFmpeg's native AAC (OBS's Windows default).

**OBS evidence**
- `win-wasapi.cpp:739-743`: event callback with a **5 s** buffer (`BUFFER_TIME_100NS`).
- For non-input sources (loopback/process) the capture thread **also wakes every 10 ms** (`WaitForMultipleObjects(..., 10)`, `:1130-1135`, a Windows 7 workaround).
- By default it timestamps with `os_gettime_ns()` at drain, not the device QPC (`ProcessCaptureData`, `:1065`; `use_device_timing` is opt-in).
- The audio thread ticks every 1024 frames with a `Sleep` loop (`audio-io.c:205-233`, `os_sleepto_ns_fast`).
- Resampling uses libswresample (`libobs/media-io/audio-resampler-ffmpeg.c`).
- Windows AAC default is `ffmpeg_aac` (`frontend/settings/OBSBasicSettings_Stream.cpp:1542`).

**Impact:** CPU small (audio is under 1% of a core in both apps). Accuracy and A/V alignment improve.

**Effort:** S–M.

**Measure:** wakeups per second per audio thread (ETW context switches); drift between A and V over 1 hour (ffprobe the start times; Isocast's existing alignment checks).

### 13. Startup: probe lazily, cache everything, open devices in parallel

**What**
- Cache NVENC/MF capabilities keyed by adapter LUID plus driver version. No probe subprocess on a warm start.
- Ship precompiled DXIL, or warm a pipeline cache on a worker thread. wgpu on DX12 compiles WGSL → HLSL → DXIL/DXBC at pipeline creation.
- Create one device. Open MF cameras and WASAPI endpoints concurrently and asynchronously.
- Show the first preview from the first captured frame. Load no browser engine.

**OBS evidence (what makes OBS slow)**
- `OBSBasic::OBSInit` (`frontend/widgets/OBSBasic.cpp:873`) loads every module DLL.
- Probe subprocesses: `obs-nvenc-test.exe` (`plugins/obs-nvenc/nvenc-helpers.c:290` `nvenc_check`), `obs-amf-test.exe` (`plugins/obs-ffmpeg/texture-amf.cpp:2682`), `obs-qsv-test.exe` (`plugins/obs-qsv11/common_utils_windows.cpp:163`), and 32- plus 64-bit `get-graphics-offsets` (`plugins/win-capture/plugin-main.c:51-57`, cached by version).
- It initialises CEF (`obs_browser_init_panel`).
- It runs a D3D11 NV12 render-and-readback self-test (`HasBadNV12Output`, `d3d11-subsystem.cpp:335`).
- It compiles HLSL with `D3DCompile`, cached under `%ProgramData%\obs-studio\shader-cache` (`libobs-d3d11/d3d11-shader.cpp:252, 302`).
- It creates every source in the scene collection, including slow DirectShow device opens.

**Impact:** startup. Target: daemon ready under 300 ms warm, first preview under 1 s.

**Effort:** M.

**Measure:** ETW/Tracy spans from process start → device ready → first frame → ready-to-record, warm and cold (after reboot).

### 14. Stop under 20 ms by design

**What**
- Keep each session's output delay at 1–2 frames: async mode, no look-ahead, no B-frames for ISOs.
- On stop: pick the stop PTS, send EOS to all sessions **in parallel**, drain on the completion thread, and write the final fragment plus index of the crash-safe fragmented MP4 (Isocast's existing `isocast-mux` design).
- Acknowledge "stopped at PTS X" as soon as the last packet is queued. The file close and fsync finish on a worker; report "file sealed" separately.

**OBS evidence**
- `output_delay = buf_count - 1` (`nvenc.c:326`): 11 frames with OBS's default 2 B-frames, more with look-ahead.
- `get_encoded_packet(finalize=true)` drains all of them serially per encoder.
- ffmpeg-mux outputs run a **separate `obs-ffmpeg-mux.exe` process fed over a pipe** (`plugins/obs-ffmpeg/obs-ffmpeg-mux.c:310`). The default `hybrid_mp4` uses the in-process `mp4_output` (`frontend/widgets/OBSBasic.cpp:487`; `plugins/obs-outputs/mp4-output.c`).

**Impact:** stop latency, from OBS's typical 100s of ms to under 20 ms for the acknowledgement.

**Effort:** M.

**Measure:** QPC from the stop command to the acknowledgement, and to file sealed; ffprobe the last PTS against the requested stop.

### 15. Build and profiling discipline, applied after the structural wins

**What**
- `lto = "fat"`, `codegen-units = 1` and `panic = "abort"` for the daemon.
- PGO (cargo-pgo) on a recorded steady-state workload *after* items 1–9. Our own user-mode code is a minority of process CPU; the driver, kernel and DWM dominate.
- `target-cpu` baseline x86-64-v2 with runtime-dispatched AVX2 for audio mixing/resampling and any CPU colour fallback.
- An allocator change only if ETW heap tracing shows churn.
- Instrument with `tracing` → Tracy, plus ETW TraceLogging so WPA lines our frame IDs up with GPU queue packets.

**Evidence**
- Windows rustc PGO: 10–20% (https://nnethercote.github.io/2022/07/20/how-to-speed-up-the-rust-compiler-in-july-2022.html).
- cargo-pgo: https://kobzol.github.io/rust/cargo/2023/07/28/rust-cargo-pgo.html
- Typical service PGO: 0.5–10% (https://github.com/zamazan4ik/awesome-pgo).

**Impact:** low to medium (estimate 3–10% of *our* user-mode CPU).

**Effort:** S.

**Measure:** A/B the same 60 s workload with process CPU from `GetProcessTimes`, release vs release+PGO.

### How to compare against OBS (one harness for all 15)

1. Use matched scenes on exon-desk: a 1080p60 and a 4K60 canvas; one WGC window, one display and one camera; the program plus per-source ISOs.
2. Configure OBS to match: portable/isolated profile, same codec, preset and bitrate, `hybrid_mp4`, and Source Record for the ISOs. Run it once elevated and once not, because GPU priority only works elevated.
3. Warm up for 30 s, then measure for 60 s steady state.
4. CPU: process CPU time delta from `GetProcessTimes`, or `typeperf "\Process(isocastd)\% Processor Time"`.
5. Memory: private bytes and working set; dedicated VRAM from the PDH `GPU Process Memory` counters.
6. GPU engines: `\GPU Engine(*)\Utilization Percentage`, split into 3D, Copy and VideoEncode.
7. Drops: Isocast counters against OBS's "skipped/lagged" stats, plus ffprobe frame counts and timestamps.
8. Repeat under a GPU-saturating load and a CPU-saturating load.

---

## Part 2 — Detailed findings

### 1. OBS's Windows video path, end to end

#### 1.1 Capture

**Windows.Graphics.Capture** (`libobs-winrt/winrt-capture.cpp`)
- Session setup (`winrt_capture_init_*`, around `:300-340`):
  - A frame pool from `Direct3D11CaptureFramePool::Create(device, B8G8R8A8 or R16G16B16A16F, 2 buffers, size)`, the **non-free-threaded** variant.
  - `IsBorderRequired(false)` after `RequestAccessAsync(Borderless)` when supported (Windows 11 only).
  - `IsCursorCaptureEnabled` (Windows 10 2004+).
- Threading:
  - `libobs/obs-video.c:1015` `init_winrt_state` creates a DispatcherQueue **on the graphics thread** (`winrt-dispatch.cpp`, `DQTYPE_THREAD_CURRENT`).
  - The graphics loop pumps it with `PeekMessage` once per tick (`obs-video.c:1114`).
  - `on_frame_arrived` (`:128`) therefore runs on the graphics thread: `TryGetNextFrame`, `GetDesc`, then `obs_enter_graphics` and a `CopyResource`/`CopySubresourceRegion` into an OBS-owned texture (`:160-175`).
  - It calls `frame_pool.Recreate` when the size changes.
- Effect: at most one copy per new frame, but arrival latency is quantised to the graphics tick, and the copy plus WinRT projection overhead sit on the render thread.

**DXGI Desktop Duplication** (`libobs-d3d11/d3d11-duplicator.cpp`)
- `DuplicateOutput1` with supported formats (`:56`).
- `gs_duplicator_update_frame` (`:256`) polls `AcquireNextFrame(0, …)` once per tick on the graphics thread, then `CopyResource` and `ReleaseFrame`.
- Display capture "Automatic" picks DXGI unless the monitor is not on the rendering adapter or it is a laptop with two or more adapters (`plugins/win-capture/duplicator-monitor-capture.c:250-277`).
- This is the right choice on Windows 10: no border, cheap.

**Game capture hook** (`plugins/win-capture/graphics-hook/*`, `game-capture.c`)
- Injected into the game. On `Present`, the hook copies the back buffer into a **legacy shared texture** it owns (`D3D11_RESOURCE_MISC_SHARED`, `graphics-hook/d3d11-capture.cpp:98`, copy at `:247`).
- OBS opens it once (`gs_texture_open_shared`, `game-capture.c:1597` `init_shtex_capture`) and samples it directly: **no per-frame copy on OBS's side**.
- A shared-memory fallback (`shmem`, `d3d11-capture.cpp:163/266` `Map(READ)`) does CPU readback inside the game and is very expensive.
- Hooking needs injection: anti-cheat friction, admin for some games. WGC costs a DWM composition copy but is supported and needs no injection.
- Recommendation: Isocast stays WGC/DXGI-only. Hooking is a large, risky surface and is not needed for screen/window ISOs.

**Async sources (cameras)** (`plugins/win-dshow`)
- libdshowcapture delivers CPU frames. MJPEG is decoded with FFmpeg, optionally with hardware decode (`ffmpeg-decode.c:47`) but still downloaded to the CPU.
- `obs_source_output_video2` → `cache_video` memcpys into a cached frame (`obs-source.c:3542`, up to `MAX_ASYNC_FRAMES 30`).
- The graphics thread uploads with `gs_texture_set_image` (`Map(WRITE_DISCARD)`) and converts with a shader.
- Isocast (MF Source Reader) wins by:
  - asking for NV12/YUY2 directly and uploading into a ring of `UPLOAD`-heap buffers, then copying to a texture once;
  - or, for MJPEG, decoding with the MF hardware MJPEG decoder into D3D11 textures (`MF_SOURCE_READER_D3D_MANAGER`) so frames never touch the CPU.

#### 1.2 Shared textures and keyed mutexes

- libobs creates NV12 render targets with `GS_SHARED_KM_TEX` (`libobs/obs.c:179-185`), which means `D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX` (`libobs-d3d11/d3d11-texture2d.cpp:129`) with a legacy `GetSharedHandle` (`:85`).
- Encode hand-off (`queue_frame`, `obs-video.c:442-508`):
  1. Pop a free texture from `gpu_encoder_avail_queue` (10 deep).
  2. If it was released, `gs_texture_acquire_sync(key)`.
  3. **Swap** it with `convert_textures_encode` (no copy unless raw outputs are also active or the frame is duplicated).
  4. `gs_texture_release_sync(++key)` and post the semaphore.
- If the encoder is behind, frames are *duplicated* (`tf->count++`) rather than queued. This is OBS's back-pressure.
- The encoder side (`nvenc-d3d11.c:210`) acquires with the same key, copies, and releases with `next_key`. The key chain serialises multiple encoders on the same texture (`obs-video-gpu-encode.c`, `lock_key`/`next_key`).
- Cost:
  - Each `AcquireSync`/`ReleaseSync` is a kernel transition that can flush the immediate context.
  - On two different devices it also forces cross-device synchronisation.
  - Modern replacements are NT-handle sharing (`D3D11_RESOURCE_MISC_SHARED_NTHANDLE`, `ID3D12Device::OpenSharedHandle`) plus shared fences (`ID3D11Device5::CreateFence(D3D11_FENCE_FLAG_SHARED)` / `ID3D12Fence`, `ID3D11DeviceContext4::Wait/Signal`). They are GPU-side waits with no CPU stall and no implicit flush. Docs: https://learn.microsoft.com/en-us/windows/win32/api/d3d11_3/nf-d3d11_3-id3d11device5-opensharedfence

#### 1.3 Texture-based NVENC (`obs_nvenc_*_tex`, `plugins/obs-nvenc`)

- **Device:** its own D3D11 device on adapter 0 (`nvenc-d3d11.c:25-73`), so it is always cross-device from libobs's device.
- **Registration:**
  - `d3d11_init_textures` creates `buf_count` NV12/P010 textures, sets them to `DXGI_RESOURCE_PRIORITY_MAXIMUM`, and calls `nvEncRegisterResource` **once each** (`:94-149`). This part is correct.
  - Input shared handles are opened once and cached (`get_tex_from_handle`, `:173`).
- **Per frame:**
  1. Keyed-mutex acquire.
  2. `CopyResource` input → pool texture.
  3. Release.
  4. `nvEncMapInputResource`.
  5. `nvEncEncodePicture` (`nvenc_encode_base`, `nvenc.c:1284`).
  6. Later `nvEncLockBitstream` (blocking), `nvEncUnlockBitstream`, `nvEncUnmapInputResource` (`nvenc.c:1083-1190`).
  - Map/unmap per frame is the API's normal pattern for D3D11 registered resources and is cheap. The avoidable costs are the copy and the cross-device sync.
- **Mode:** synchronous (`enableEncodeAsync = 0`, `nvenc.c:166`). Output is polled by delay: a packet is only pulled once `buffers_queued >= output_delay` (`nvenc.c:1091`), with `buf_count = max(4, frameIntervalP*2*2)`, look-ahead-adjusted, capped at 64 (`:318-324`).
- **Threading:** one `gpu_encode_thread` per mix runs every encoder of that mix in sequence (`obs-video-gpu-encode.c:25-200`).
- **Split-frame encoding:** a user property (`nvenc-properties.c:238-251`) passed through as `splitEncodeMode` (`nvenc.c:171`). It is irrelevant on the RTX 5070.

#### 1.4 Colour conversion

- OBS uses **pixel-shader conversion**, not `ID3D11VideoProcessor` (no `VideoProcessor` symbols anywhere in libobs/libobs-d3d11).
- `render_convert_texture` (`obs-video.c:344`) draws a full-screen triangle per plane with techniques `NV12_Y` and `NV12_UV` (`libobs/obs.c:66-67`, effect `libobs/data/format_conversion.effect`).
- On D3D11 the Y (R8) and UV (R8G8) render targets are **views of one NV12 texture**. This needs `ExtendedResourceSharing` and passes a driver self-test (`HasBadNV12Output`, `d3d11-subsystem.cpp:335-415`, `nv12Supported` at `:686`).
- An output rescale is a separate pass first (`render_output_texture`, `:270`, bicubic/lanczos/area effects).
- Isocast:
  - On D3D12 an NV12 texture can be bound as two RTVs with `PlaneSlice` 0/1. Better still, one **compute** pass writes Y and UV together from a single sample of the source, fusing scale + matrix + chroma downsample.
  - The video processor (`VideoProcessorBlt`) is not a CPU win on NVIDIA. It runs on the 3D/compute engine through a heavier driver path and gives less control over chroma siting and matrices.
  - Alternatively, NVENC accepts **ARGB/ABGR input** directly (`NV_ENC_BUFFER_FORMAT_ARGB`) and converts internally on CUDA cores ("Encoding of RGB contents" in programming guide §6.4). For a window ISO this could remove our conversion pass entirely. Verify the colour-matrix and range control and the quality before relying on it.

#### 1.5 Frame pacing and stall avoidance

- **`obs_graphics_thread`** (`obs-video.c:1158`), loop body `obs_graphics_thread_loop` (`:1094`):
  1. `update_active_states`
  2. `tick_sources` (all sources' `video_tick`)
  3. pump the Win32/WinRT messages
  4. `output_frames` (each mix: render, convert, encode hand-off, optional stage)
  5. `render_displays` (every preview swapchain)
  6. `execute_graphics_tasks`
  7. `video_sleep`
- **`video_sleep`** (`:805`) sleeps to the next interval with `os_sleepto_ns`, which spins at the end.
  - On overrun it computes `count` intervals missed, adds `count-1` to `lagged_frames`, and pushes one `vframe_info` with `count`.
  - Encoders then **duplicate** that frame `count` times (`queue_frame`). Output stays CFR, and the drop shows up as "rendering lag".
- **Frames in flight:**
  - Raw path: `NUM_TEXTURES 2` staging sets, mapped one frame late.
  - GPU-encode path: 10 NV12 textures, then the encoder's `buf_count`.
  - D3D11 allows 16 queued presents (`SetMaximumFrameLatency(16)`).
- **Synchronisation:**
  - Keyed mutexes for encode.
  - Implicit (blocking `Map`) for raw.
  - Timestamp/disjoint queries only for the source profiler (`d3d11-rebuild.cpp:285-303`, `d3d11-subsystem.cpp:3057`).
  - No D3D11 fences.
- **`video_thread`** (`libobs/media-io/video-io.c`) consumes raw frames for CPU encoders and raw outputs. It is inactive in a pure NVENC setup.

### 2. Thread priorities and OS knobs OBS uses

| Knob | OBS | Where | Isocast recommendation |
|---|---|---|---|
| MMCSS | "Audio" on the audio mix thread and WASAPI capture threads only | `audio-io.c:209`, `win-wasapi.cpp:1105` | Same for audio ("Pro Audio" if we use 10 ms periods). Video threads: ABOVE_NORMAL, not MMCSS. MMCSS boosts to the realtime band and can starve the system if a bug spins. |
| Process priority | User setting, default Normal | `frontend/utility/platform-windows.cpp:191` | Normal. Expose it like OBS does. |
| GPU priority | `D3DKMTSetProcessSchedulingPriorityClass(HIGH if HAGS else REALTIME)` + `SetGPUThreadPriority(GPU_PRIORITY_VAL)`; admin only; official builds only | `d3d11-subsystem.cpp:436-460`, `CMakeLists.txt:28-33` | D3D12 queue priority HIGH (no admin); D3DKMT HIGH when permitted; never require elevation. |
| Timer resolution | `timeBeginPeriod(1)` for the life of the process | `util/platform-windows.c:785` | High-resolution waitable timer; scope any `timeBeginPeriod` to recording. |
| Power throttling / EcoQoS | none | — | Opt out of EXECUTION_SPEED and IGNORE_TIMER_RESOLUTION throttling in isocastd (windowless daemon). |
| HAGS | Detected and logged as a warning; changes the priority class | `d3d11-subsystem.cpp:646-653` | Detect and log (`D3DKMT_WDDM_2_7_CAPS`); test both. Overwolf (OBS-based) tells users to turn it off (https://support.overwolf.com/support/solutions/articles/9000181387). Users report Windows 10 HAGS as less stable for capture and Windows 11 as fine (anecdotal). |
| Game hook vs WGC | Hook: zero OBS-side copy, injection risks; WGC: DWM copy, supported | §1.1 | WGC/DXGI only. |
| Max frame latency | 16 | `d3d11-subsystem.cpp:417` | 2–3 frames in flight via fences. |
| RTMP thread | ABOVE_NORMAL | `rtmp-windows.c:227` | Same for any network sender. |

Windows 10 specifics:
- No WGC `MinUpdateInterval`, `IsBorderRequired` or dirty regions.
- `timeBeginPeriod` has been per-process since 2004.
- HAGS has been available since 2004.
- NVENC's D3D12 interface needs 20H1+ (programming guide §3.1.1.4).

### 3. Audio

- **Capture** (`plugins/win-wasapi/win-wasapi.cpp`):
  - One thread per source: `CaptureThread` (`:1091`) with COM MTA and MMCSS "Audio".
  - Event-driven via `SetEventHandle` (`:841`), shared mode, a 5 s buffer (`:638`).
  - Loopback/process sources also time out every 10 ms (`:1130-1135`).
  - `ProcessCaptureData` (`:995`) drains with `GetNextPacketSize`/`GetBuffer`, substitutes a reused silence buffer on `AUDCLNT_BUFFERFLAGS_SILENT`, and passes packets to `obs_source_output_audio` (which copies into the source's circular buffer and resamples with swresample when needed).
  - Process loopback: `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK` (`:696`).
  - Reconnect handling runs on a separate thread.
- **Mix:** `audio_thread` (`libobs/media-io/audio-io.c:205`) ticks every `AUDIO_OUTPUT_FRAMES = 1024` samples (21.3 ms at 48 kHz) with `os_sleepto_ns_fast` (a `Sleep` loop, no spin).
  - It walks the source tree, mixes into up to 6 mixes × 8 channels of f32, clamps, and calls encoders.
  - OBS audio buffering adds up to hundreds of ms of latency under timestamp jitter. This is the "audio buffering hit the maximum value" warning.
- **AAC:** the Windows default is `ffmpeg_aac` (FFmpeg's native encoder). `CoreAudio_AAC` is macOS only and `libfdk_aac` is optional (`OBSBasicSettings_Stream.cpp:1542-1546`).
  - Media Foundation's AAC encoder (`CLSID_AACMFTEncoder`) is in the box. It supports 44.1/48 kHz, 16-bit input and 96/128/160/192 kbps (https://learn.microsoft.com/en-us/windows/win32/medfound/aac-encoder).
  - Either costs well under 1% of a core per stereo track. The win is encoding each track **once** (Part 1 #4).
- **Isocast:**
  - Event-driven with QPC device timestamps.
  - No polling timeouts on Windows 10.
  - 10 ms periods for the mic.
  - A single mixer thread under MMCSS.
  - A preallocated ring per source.
  - Resampling only on mismatch.
  - Clock-domain drift correction by small resampling ratio adjustments rather than buffering.

### 4. Startup

What OBS does before the window is usable (`OBSBasic::OBSInit`, `frontend/widgets/OBSBasic.cpp:873-1000`):
1. `InitBasicConfig`, then `ResetAudio` (audio subsystem).
2. `ResetVideo`: the D3D11 device, the adapter log, the HAGS check, the NV12 self-test render plus blocking readback, and shader compiles (cached).
3. Load all modules: about 40 DLLs. Several run probes:
   - NVENC: `obs-nvenc-test.exe` subprocess, result cached in config (`nvenc-helpers.c:290-367`).
   - AMF: `obs-amf-test.exe`.
   - QSV: `obs-qsv-test.exe`.
   - win-capture: 32- and 64-bit `get-graphics-offsets` subprocesses unless the cache matches (`plugin-main.c:51-57`).
4. Initialise the CEF browser panel.
5. Log every encoder.
6. Create outputs and hotkeys.
7. Load the scene collection: create every source, open every camera (DirectShow graph build is often 0.5–2 s per device) and start every WGC session.

What Isocast can skip or defer:
- Every probe subprocess. Probe in-process once per driver version and cache.
- The browser engine.
- The NV12 driver self-test. D3D12 NV12 RTV/UAV support is queryable with `CheckFeatureSupport(D3D12_FEATURE_FORMAT_SUPPORT)`.
- Runtime HLSL compilation: ship DXIL or warm pipelines on a worker.
- Serial device opens: open cameras/endpoints in parallel and asynchronously.
- Capture sessions for sources not in the active scene or any armed ISO.

Measure with spans: process start → engine ready → first preview frame → ready to record.

### 5. Where OBS is wasteful (and the design that avoids it)

1. **Single-threaded rendering.** One graphics thread ticks sources, renders every mix serially (program, each Source Record ISO view, vertical canvases), renders every preview display, and handles WGC frame arrival.
   - Fix: a GPU-timeline design. One submit thread records everything into one or two command lists; capture, completion and mux run on their own threads.
   - The CPU work of recording is small, so a single submit thread is fine *if* it never blocks. The problem in OBS is blocking and serialisation, not the thread count.
2. **Extra copies per frame.** WGC/DXGI → OBS texture (1); scene → render texture (2); optional rescale (3); convert to NV12 (4); NVENC-side copy into its own device's texture (5).
   - Fix: (1) only when needed; (2)+(3)+(4) fused for ISOs; (5) removed.
3. **CPU readback** for raw outputs, CPU encoders, the virtual camera and some plugins: blocking `Map` on the graphics thread plus a memcpy into the `video-io` cache (§1.5). Fix: async readback on a worker, only on demand.
4. **Per-frame allocations.** `da_push_back` of `encoder_packet_time` per frame per encoder (`obs-video-gpu-encode.c:176`); `da_copy_array` of every packet into `packet_data` (`nvenc.c:1132`); output packet duplication into interleave queues. Fix: a preallocated packet arena per session; muxer reads by reference.
5. **One global video clock and thread for multiple outputs/ISOs.** All mixes share `obs->video.video_time` and one `video_sleep`. A slow mix makes *every* output lag-duplicate. Each mix has a single `gpu_encode_thread` running its encoders serially.
   - Fix: a per-output deadline and shed policy (Isocast already has ISO shedding on macOS). Program priority comes first; ISOs degrade first.
6. **Second NVENC device and keyed mutexes** (§1.2–1.3).
7. **Over-deep buffering:** 16 frames of present latency, 10 + `buf_count` NV12 surfaces per encode path, and a 5 s WASAPI buffer. All of it costs memory, and stop/start latency.
8. **Preview rendering every tick.** Displays render every frame. A 2017 user benchmark found that disabling the OBS preview cut its FPS impact on the game by 22–58% (https://www.reddit.com/r/letsplay/comments/5lqgol/). Fix: preview at a reduced rate, only when visible, shared to the UI process as a surface (W.6).

### 6. Rust specifics on Windows

- **windows-rs vs winapi:**
  - Both compile to direct vtable calls with no runtime cost difference. windows-rs is maintained, generated from metadata, has COM/WinRT support (needed for WGC) and HRESULT→`Result`.
  - Costs to watch:
    - `Interface::clone()` is `AddRef`, and drop is `Release`. Pass `&IFoo`/`Param` and keep long-lived interfaces in structs.
    - `.cast::<T>()` is `QueryInterface`. Do it once at setup.
    - WinRT event handlers allocate a delegate once, which is fine.
    - Avoid per-frame `HSTRING`/`BSTR`.
  - winapi is unmaintained and has no WinRT. Prefer `windows` or `windows-sys` for the raw C ABI pieces (https://github.com/microsoft/windows-rs).
- **LTO / codegen-units / PGO:**
  - Release profile: `lto = "fat"`, `codegen-units = 1`, `opt-level = 3`, `debug = "line-tables-only"` (keep PDBs for profilers), `panic = "abort"` for the daemon if unwinding is not relied on.
  - PGO through cargo-pgo on Windows works with MSVC targets; collect a profile from a scripted 60 s 1080p and 4K recording. Expect single-digit % on our code (Part 1 #15). Skip BOLT, which is ELF-focused.
- **target-cpu:**
  - Zen 3 supports x86-64-v3 (AVX2, FMA, BMI2). Build with the default or `-C target-cpu=x86-64-v2` so the binary runs everywhere in scope.
  - Use `#[target_feature(enable = "avx2,fma")]` functions selected via `is_x86_feature_detected!` (or the `multiversion` crate) for hot CPU kernels: audio mix/resample, peak meters, any CPU colour fallback.
  - `-C target-cpu=native` would break distribution.
- **Allocators:**
  - The Windows default heap is NT heap with LFH; the Segment Heap can be opted into via the manifest (`<heapType>SegmentHeap</heapType>`, Windows 10 2004+; https://learn.microsoft.com/en-us/windows/win32/sbscs/application-manifests).
  - mimalloc (https://github.com/microsoft/mimalloc) usually beats both on alloc-heavy multithreaded code and reduces fragmentation in long sessions.
  - With a zero-per-frame-allocation design the allocator barely matters for CPU. Decide by measured RSS growth over a 1 h recording, not by benchmarks.
- **Per-frame allocation and COM churn:**
  - Preallocate frame, packet and command arenas.
  - Use fixed-capacity SPSC/MPSC rings (`rtrb`, `crossbeam` `ArrayQueue`).
  - Keep a pooled `Vec<u8>` per bitstream buffer.
  - Store `windows::core` interfaces in long-lived structs.
  - Enforce it with a debug-only counting global allocator that panics on allocation inside a `no_alloc` span.
- **wgpu DX12 overhead and when to bypass:**
  - wgpu adds validation and resource-state tracking per pass and submit (about µs per command; negligible at tens of draws per frame), automatic barriers, and staging-belt or `write_buffer` allocations.
  - Keep wgpu for compositing and preview.
  - Disable validation flags in release (`InstanceFlags` without `VALIDATION`/`DEBUG`).
  - Pre-create bind groups and pipelines.
  - Use raw D3D12 via hal interop for the parts wgpu cannot express:
    1. Importing shared NT-handle textures from the D3D11 capture/MF side (`Device::as_hal::<Dx12>` + `hal::dx12::Device::texture_from_raw` + `create_texture_from_hal`).
    2. Shared fences and waits on the queue: get the raw `ID3D12CommandQueue` through hal and `Wait`/`Signal` a shared `ID3D12Fence` around wgpu submissions.
    3. NV12 plane-slice RTV/UAV for conversion (if the pinned wgpu cannot express NV12 storage writes).
    4. Registering NV12 outputs with NVENC's D3D12 interface (input/output fence points, `D3D12_HEAP_TYPE_READBACK` bitstream buffers; programming guide §4.1.3).
  - Verify these hal entry points against the wgpu version pinned in `Cargo.lock`; the hal API moves between releases.
- **D3D11 vs D3D12 for NVENC:**
  - D3D11 registration is simplest and most battle-tested (OBS, FFmpeg). It needs a D3D11 device; use the WGC device on the same LUID.
  - Hand-off with D3D12 then means shared textures and fences, one GPU-side wait per frame and no CPU wait.
  - NVENC on D3D12 removes the second API but is less trodden, and is documented with explicit fences rather than the classic async-event model. Spike both and measure.
- **Tokio vs dedicated threads:** dedicated OS threads for capture, the frame clock and submit, NVENC completion, the audio mixer and mux I/O, with bounded rings between them. Tokio only for IPC and control.
  - Tokio's work-stealing scheduler adds wakeup latency jitter (tens to hundreds of µs) and cannot carry MMCSS or priority per task.
  - Blocking Win32 waits (`WaitForMultipleObjects`) belong on plain threads.
  - File writes: overlapped I/O or a dedicated writer thread with large aligned writes.
- **Profiling:**
  - **WPR/WPA** (`wpr -start CPU -start GPU -start Video`): CPU sampling with stacks, ReadyThread, GPU queue packets.
  - **GPUView**, for the DX kernel queue and VideoEncode engine.
  - **ETW TraceLogging** from Rust (`tracelogging` crate, https://github.com/microsoft/tracelogging) to put frame IDs and stop events on the same WPA timeline.
  - **Tracy** (`tracing-tracy`) for live per-thread zones and allocation tracking.
  - **Superluminal**: excellent with Rust PDBs; `superluminal-perf` crate for markers.
  - **PIX on Windows**: D3D12 timing captures and GPU captures (`WinPixEventRuntime` markers).
  - **nvidia-smi dmon** for encoder and engine utilisation.
  - Always build with PDBs (`debug = "line-tables-only"` or `split-debuginfo`).

### 7. NVENC best practice for low CPU (Video Codec SDK 13)

1. **Async mode** (`enableEncodeAsync = 1`; events via `NvEncRegisterAsyncEvent`; `NvEncLockBitstream` after the event). Only on Windows with WDDM; RTX is WDDM. Submit on one thread and wait on another (§6.1, §6.3).
2. **Register once, map per use:**
   - `NvEncRegisterResource` once per pooled texture; unregister only at teardown (§4.1.2).
   - On D3D11, `NvEncMapInputResource`/`Unmap` around each use is the documented pattern and is cheap. The churn to avoid is *registering* per frame (for example registering whatever texture WGC handed us).
   - If feeding WGC pool textures directly, register each pool buffer once and re-register only on `Recreate`.
   - On D3D12 there is no map: pass `NV_ENC_INPUT_RESOURCE_D3D12` with fence points (§4.1.3, §4.3).
3. **Buffer counts:** inputs/outputs ≥ 1 + B-frames (§3.9); async I/O ≥ 4 + B-frames (§6.1). Keep it at exactly what the in-flight depth needs.
4. **Memory-lean settings** (§9): avoid B-frames, look-ahead, two-pass, temporal filter and UHQ for ISOs. Each allocates extra VRAM and adds latency.
5. **"Recording/Archiving" recommendation** (§9): HQ/UHQ tuning, VBR, large VBV, B-frames, look-ahead, AQ. Use it only for the **program** and only if the pixel budget allows. Otherwise use the CQ/VBR target-quality mode with P1–P4.
6. **Multiple sessions on GeForce:** at most 8 concurrent sessions per system (app note §3). Hardware time-slices sessions with "negligible context-switching penalty"; aggregate throughput is bounded by the per-engine table.
7. **Split-frame encoding:** HEVC/AV1 only, needs two or more NVENCs (5070 Ti and up), raises single-session speed but not total throughput, and costs quality. **Not available on RTX 5070.**
8. **Blackwell (9th gen) specifics:**
   - About 5% better BD-rate for HEVC/AV1 vs Ada.
   - 4:2:2 H.264/HEVC encode.
   - H.264 High10.
   - AV1 UHQ tuning.
   - Per-engine throughput at P1/P5 roughly equal to or above Ada (P5 HEVC HQ 521 vs 411 fps at 1080p).
   - Sources: Blackwell whitepaper (https://www.techpowerup.com/gpu-specs/docs/nvidia-blackwell-architecture.pdf), app note §2/§4.
9. **DXGI + NVENC threading caveat** (§6.3): calling `AcquireNextFrame` on one thread while `NvEncLockBitstream` runs on another with the same DirectX device can misbehave, because NVENC uses the app's device internally.
   - NVIDIA's recommended settings for this case: `enableEncodeAsync = 1`, `doNotWait = 0`, `enableOutputInVidmem = 0`.
   - Keep DXGI duplication on its own D3D11 device.
10. **ARGB input** is supported, with conversion on CUDA cores. This is an option to drop the conversion pass for window ISOs; validate the colour matrix and range.

### 8. How others beat OBS

- **NVIDIA ShadowPlay / NVIDIA App:**
  - Driver-integrated capture (historically NvFBC/NvIFR, now internal), straight into NVENC, with no compositor, scene graph or second device.
  - Recent independent tests show about 3% FPS loss vs OBS's 5–19% on an RTX 5060: LTT Labs, 2026 (https://www.lttlabs.com/articles/2026/08/24/your-screen-recorder-wants-to-steal-your-frames).
  - A 2017 user report: OBS used 15% more CPU than ShadowPlay at 4K60 with matched NVENC settings (https://obsproject.com/forum/threads/obs-using-15-more-cpu-than-nvidia-share-shadowplay-at-4k-but-configured-the-same.74818/).
  - Lesson: the composite-everything pipeline is the cost. ISOs that bypass composition (Part 1 #5) get Isocast most of ShadowPlay's advantage while keeping scenes.
- **Xbox Game Bar:** OS-integrated (Windows.Media.Capture / WGC + MF hardware encode). It loses about 1 FPS in LTT's 2026 test, but other tests show 10% (https://www.ascent.gg/blog/best-game-recorder-no-fps-drop, vendor blog, low confidence). Results vary by version and game.
- **Medal, Outplayed (Overwolf), Streamlabs:** all **libobs-based**.
  - Overwolf's recorder exposes OBS encoder IDs such as `obs_nvenc_h264_tex` (https://github.com/overwolf/ow-electron-packages-types/blob/main/types.d.ts), and Overwolf support calls it "based on OBS".
  - Streamlabs Desktop wraps libobs (obs-studio-node). Its overhead is *worse* than OBS: 11–20% FPS loss in LTT's test, plus Electron memory.
  - Medal is reported to use libobs with a trimmed Electron UI (blog-level evidence, unverified).
  - None of them beat OBS's engine. They inherit it and sometimes trim the UI.
- **Published benchmark quality is weak.** Most numbers are game-FPS deltas, not recorder CPU. Lift the comparison harness from Part 1 into W.5 reports, as PLAN.md already asks.

### 9. Open questions and assumptions (no action needed from Ryan)

- The RTX 5070's NVENC clock affects per-engine fps. Calibrate with a short encode run on exon-desk instead of trusting the 5090-class table.
- Whether NVIDIA's D3D12 NVENC path supports the async-event model or only fence plus `SetEventOnCompletion` completion. Spike it; either way the completion thread waits on events, not by spinning.
- NVENC RGB-input colour handling (matrix and range) on Blackwell. Validate it against our shader conversion with a test pattern before using it for ISOs.
- The exact OBS CPU breakdown on exon-desk. Capture one WPR trace of OBS (isolated profile) with 1080p60 and with 4K60 + Source Record before claiming per-item savings. Expect the busy-wait, the keyed-mutex/copy path and the serialised encode thread to show up near the top.
