# Recording encoders

The program and ISO files each have an `encoder` under `recording.program`
and `recording.iso`. The app, CLI and MCP use these same settings.

```toml
[recording.iso]
enabled = true
sources = ["screen", "camera"]
audio = ["mic"]
encoder = { codec = "hevc", quality = "high", container = "mov" }
```

## Program size

The program is drawn at the canvas size. `recording.program.width` and
`height` (both or neither, even, at most the canvas's) write the program
file smaller, like OBS's Output (Scaled) Resolution: the encoder scales each
frame (keeping its shape; another shape is letterboxed). ISO files keep their
sources' own sizes, and monitors, snapshots and the stream still get the
canvas. A 4K canvas with a 1080p program keeps the 4K screen in its ISO file
for editing while the hardware encoders do a quarter of the program's work:

```sh
isocast plan --patch '{"recording":{"program":{"width":1920,"height":1080}}}'
```

On a base M2 (isocast-m2-a, macOS 27.0, 2026-10-13) the synthetic flagship
(4K60 desktop and 1080p60 camera ISOs, 4K60 program) kept 1044–1054 of
1440 frames per file in 24 s; with a 1920×1080 program file from the same
4K canvas, all three files had 1440/1440.

## Will this Mac keep up? (`isocast tune`)

`isocast tune` (MCP `tune_encoders`) encodes hard, noisy content for every
file of the setup at once, as fast as the hardware encoders take it, and
judges the speed:

| `capacity` | Real time | Meaning |
|---|---|---|
| `fits` | 1.25× or more | Keeps up with room for a busy Mac |
| `no_margin` | 1× to 1.25× | Keeps up; a stream, a call or an export may cost frames |
| `cannot_keep_up` | under 1× | A recording drops about the share it falls short by |

If the setup doesn't fit, tune tries changes closest first and stops at the
first that fits: the program file at 1440p, then 1080p; one quality preset
lower; half the frame rate; one ISO file fewer; then combinations. Every
file and frame rate is kept before anything else, and dropping a file comes
last. `variants` lists what was tried, each with its patch; `change` is the
closest that fits (or, if none does, the closest that keeps up), ready for
`isocast plan --patch`. Changes the pixel rate says can neither fit nor be
the closest that keeps up are skipped; tune takes up to about a minute.

Tune keeps what it measured beside the live setup's file
(`setup.toml` → `setup.tune.json`), per Mac and macOS version. `start` then
warns (a note in its result and a warning event) when the recording's files
are ones tune measured as `cannot_keep_up` on this Mac, or ask at least as
much file for file, naming the fix tune recommended.

On a base M2 (macOS 27.0), the synthetic flagship measured 0.71×
(`cannot_keep_up`); a 1080p program file 1.04× (`no_margin`, and 1440/1440
frames in a 24 s take); 30 fps 1.75× (`fits`, recommended, 720/720). A lower
quality preset barely changes VideoToolbox HEVC's speed (1.02× vs 1.04×), so
it helps the disk, not the encoders.

## A quick answer without benchmarking (`isocast capacity`)

`isocast capacity [SETUP] [--stream|--no-stream] [--add-iso SOURCE] [--json]`
(MCP `estimate_capacity`, method `estimate_capacity`) predicts instantly
from per-chip data (`crates/isocast-engine/src/media_engines.json`): each
H.264/HEVC encoder (program, stream, every ISO file) runs on one of the
chip's encode engines for its whole life, the largest first on the least
busy engine, and `load` is the busiest engine's pixel rate over what one
engine encodes in real time. Up to 0.85 `fits`, up to 1 `no_margin`, over 1
`cannot_keep_up` (ISO files drop first; `iso_drop_percent` is a floor).
ProRes runs on engines of its own and isn't counted. A tune measurement
that couldn't keep up overrules it.

`suggestions` lists the closest changes that fit (a camera or test pattern
captured smaller or slower, the program or stream smaller, half the frame
rate, the stream stopped, an ISO file off), each a patch for `plan` or a
method; `after_take` ones wait for the recording to end. `--add-iso`
answers "how do I turn this ISO file on so it still fits": as is, at a
smaller size or rate, or not at all, with the reason. `setup check` and
`doctor` print it as the "encoder capacity" check (a warning, never a
failure), `start` notes it when nothing was measured and it can't keep
up, and while recording the engine sends `encoder_overload` once per take
when files drop because the encoders fell behind.

On a base M2 (isocast-m2-b, 25 s synthetic takes, 2026-10-15): a 4K60 ISO
plus a 1080p60 program, 0.78 `fits`, dropped nothing; adding a 1080p60
ISO, 0.93 `no_margin`, dropped nothing; adding a 1080p60 stream, 1.23
`cannot_keep_up`, dropped half the ISO frames (and `encoder_overload`
came); the first suggestion (the 4K ISO at 1920×1080, 0.76) dropped
nothing. H.264 counts 1.9× HEVC per picture: `tune` on the same Mac ran
the same ISO files about 1.8× slower as H.264, so H.264 files are offered
HEVC, never the reverse.

## Quality

`quality` accepts exactly these five names, or an integer from **1–100**.
Names stay names in saved TOML, JSON, `get`, `plan` and MCP replies; numbers
stay numbers. Omitted quality now defaults to `"high"`, the same constant
quality 70 used previously.

| Name | VideoToolbox quality | HEVC depth |
|---|---:|---|
| `low` | 30 | Configured depth (8 by default) |
| `medium` | 50 | Configured depth |
| `high` (default) | 70 | Configured depth |
| `very_high` | 85 | Configured depth |
| `incredible` | 100 | 10-bit Main 10 |

These are constant-quality settings: resolution and frame rate stay the
same, and busy pictures use more space. `incredible` promotes HEVC
`bit_depth = 8` to 10 in the setup before capture opens. Returning to a lower
preset keeps that explicit 10-bit setting; set `bit_depth = 8` to return to
8-bit. Numeric quality remains the manual option and never changes depth.

For example, this preserves the old encoder configuration exactly:

```toml
encoder = { codec = "hevc", quality = 70, bit_depth = 8 }
```

And the same change through the shared engine API:

```sh
isocast plan --patch '{"recording":{"iso":{"encoder":{"quality":"incredible"}}}}' --json
isocast apply --revision N --patch '{"recording":{"iso":{"encoder":{"quality":"incredible"}}}}' --json
```

Encoder changes apply to the next recording. A screen's native capture depth
currently follows the **ISO** encoder and is chosen when its capture opens;
a program-only depth change does not select the capture format. A running
engine keeps an already-open screen's capture format across encoder edits.
Restart the engine while
idle to open that screen in the new depth. Cameras keep their device's native
format; writing a 10-bit file cannot restore precision absent from the source.
The current program compositor also produces 8-bit canvases; Main 10 does
not turn that composition into native 10-bit processing.

### Measured tradeoffs

Calibrated on an M5 Max with AVAssetWriter's recording settings,
hardware-only HEVC with the real-time hint, and offline 4-second 60 fps input
samples. Whole-flagship recording is checked separately in real time. The real desktop
and Studio Video camera were captured as ProRes 422 HQ references. The
synthetic clips are the benchmark's 4K test pattern and noisy 1080p camera
stand-in. Sizes are **video-only decimal MB/minute**; audio adds about
1.92 MB/minute per stereo track. These are examples, not size guarantees.

| Preset (default depth) | Synthetic 4K desktop | Real 4K desktop | Synthetic noisy 1080p | Real 1080p camera |
|---|---:|---:|---:|---:|
| low (8-bit) | 33 | 4.7 | 13 | 1.9 |
| medium (8-bit) | 106 | 6.8 | 42 | 2.7 |
| high (8-bit) | 469 | 12 | 207 | 9.7 |
| very_high (8-bit) | 835 | 22 | 864 | 103 |
| incredible (10-bit) | 2532 | 148 | 6290 | 3080 |

| Preset | Real desktop SSIM / PSNR dB | Real camera SSIM / PSNR dB |
|---|---|---|
| low | .981773 / 38.65 | .974584 / 41.76 |
| medium | .992673 / 43.65 | .982709 / 44.87 |
| high | .997933 / 50.34 | .990632 / 48.75 |
| very_high | .998963 / 54.13 | .993940 / 51.24 |
| incredible | .999687 / 59.99 | .999648 / 63.87 |

All five numeric values were also measured at both depths. At Q85, Main 10
improved the real camera to .998417 SSIM / 57.22 dB, but used 446 MB/minute.
Q100 Main 10 preserves still more fine detail and is the near-transparent
stop; it is not mathematically lossless. On the synthetic 8-bit source,
Q100 Main was pixel-exact, whereas Main 10 had measurable error concentrated
in chroma (.998808 desktop SSIM and .995460 noisy-camera SSIM).

Full-resolution VMAF, sampled every sixth frame, rose from 76.00 to 99.36
for the synthetic desktop and 82.34 to 99.95 for the noisy camera. Real
desktop/camera scores rose from 79.68/68.38 to 97.38/97.61. A VMAF score is
not a percentage of quality or a guarantee of invisibility; even the
pixel-exact synthetic Q100 Main encode scored 98.76.

HEVC Main 10 was retained for `incredible`: the real desktop reference used
about 7099 MB/minute in ProRes 422 HQ versus 148 in HEVC. The real camera
used 2782 versus 3080 respectively, so ProRes can be smaller for noisy camera
footage at the highest setting, but the combined desktop-and-camera workload
strongly favors HEVC's size. Noise is exceptionally expensive at Q100.

Each preset also completed a 12-second full flagship run with both the
synthetic clips and the real rig: 4K60 desktop ISO, 1080p60 camera ISO and
4K60 program, 720 frames per file, zero reported video/audio drops, with
ffprobe confirming counts and profiles. These short checks do not resolve
the separately reproduced, intermittent raw-ISO video stalls under contention.

## Codecs and containers

- `hevc`: hardware HEVC, 8-bit Main or 10-bit Main 10; the default.
- `h264`: hardware H.264, 8-bit only. The quality names select the same
  numeric values; the HEVC depth promotion does not apply.
- `prores422_proxy`, `prores422_lt`, `prores422`, `prores422_hq`,
  `prores4444`: ProRes quality is selected by the codec; `quality` is ignored.
  ProRes uses substantially more disk bandwidth.

`container` is `mov` (default) or `mp4`. Video is tagged SDR Rec.709.
Files use 2-second movie fragments so completed fragments survive a crash
or kill of the recording process (the last seconds are lost; see
[Getting started](getting-started.md) for the limits). Power loss is not
covered: the writer doesn't force data to the disk, so it can lose more
than the last fragment.
H.264 and HEVC require the hardware encoder instead of falling back to a
CPU-heavy software encoder under contention. All recording outputs use
AVAssetWriter's asynchronous encoding path. The earlier direct HEVC ISO path
was removed: synchronous `VTCompressionSessionCompleteFrames` calls could
stall an ISO file's audio and video together on the M4 Max.
Independent bounded video/audio queues let ready audio pass stalled video.
Capture pixel buffers are retained, not copied; constant-quality settings,
the shared clock and movie-fragment policy are unchanged.

The M4 Max synthetic flagship is not yet qualified for zero-drop recording:
video-input back-pressure can still fill these queues when recording two
4K60 outputs and a 1080p60 ISO together. The original M5 Max qualification
predates the direct-path removal and needs a new hardware run.
The calibration tables are representative measurements, not promised rates.

The recording disk check is a capacity/speed budget, not a prediction of
typical file size. Constant-quality rates depend strongly on motion, noise,
fine detail, codec, depth and the Mac's encoder. Measure a representative
take before budgeting a long recording.

## Replay buffer

Like OBS's: while it runs, the buffer keeps the last
`recording.replay.seconds` (30 by default, 5–300) of the program, its
picture and the program mix, in memory as compressed H.264 and AAC, apart
from any recording or stream. A save writes them, up to the moment it was
asked for, to a new file without encoding again.

```sh
isocast replay start          # Controls dock: Replay Buffer
isocast replay save --json    # Save Replay, or the replay.save hotkey
isocast replay status
isocast replay stop
```

MCP has `start_replay`, `save_replay`, `replay_status` and `stop_replay`;
hotkey actions are `replay.start`, `replay.stop`, `replay.toggle` and
`replay.save` (suggested: ctrl+alt+cmd+Y saves).

- **The file** is `Replay <date> <time>.mov` (or `.mp4`, as
  `recording.program.encoder.container`) in the running recording's project
  folder, or else the recordings folder. It starts on a keyframe at or
  before `seconds` ago, so it is up to a keyframe interval longer, and ends
  at the save. It is written under a hidden temporary name and named when
  complete, and needs room for itself plus `recording.keep_free_gb`.
  Saving never touches a recording or a stream; saves can follow each
  other at once.
- **The encoder.** While the setup has a `[stream]`, the buffer encodes as
  the stream does (its size, bitrate and 2 s keyframes), so a running
  stream and the buffer share one hardware encoder session. Otherwise it
  encodes on its own: the program file's size, 1 s keyframes, about 0.1
  bit per pixel (12 Mb/s at 1080p60, at most 40 Mb/s) unless
  `recording.replay.video_bitrate_kbps` says. Recordings encode inside
  their file writers, so the buffer can't share theirs yet.
- **Memory** is the bitrate times `seconds`, plus up to 3 s and a
  keyframe interval; `recording.replay.max_memory_mb` (1024 by default)
  caps it, and `replay_status` reports `memory_bytes` and says
  `memory_limited` when the cap keeps it short. Frames the buffer missed
  are counted in `dropped_frames` (warned about like the stream's), and a
  saved file reports its own.

```toml
[recording.replay]
seconds = 60
max_memory_mb = 512
```
