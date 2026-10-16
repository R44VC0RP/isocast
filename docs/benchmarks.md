# Benchmarks

Isocast against OBS Studio 32.2.2 (its default OpenGL renderer) on the
same Mac with the same scene, measured on 2026-10-13 with
[scripts/obs-bench.py](../scripts/obs-bench.py).

## Conditions

- The scene: a looping 4K60 clip filling the canvas, a 1080p60 clip inset
  as a camera, and a short beep every second. Synthetic clips, not real
  capture, so both apps get exactly the same input.
- Hardware HEVC at the same quality, 1-second keyframes, Rec. 709,
  AAC 256 kb/s, fragmented MOV (OBS: Hybrid MOV). The stream rows send
  H.264 at 6000 kb/s to a local RTMP listener.
- OBS ran minimized with its preview off. Each app was launched fresh for
  each run with its own settings.
- Four interleaved rounds per row (the apps take turns), median shown.
  Nothing else measured ran at the same time.
- CPU is the app's own processes, in percent of one core. macOS runs
  hardware encoding in separate VideoToolbox processes; those are counted
  separately. Memory is physical footprint (what Activity Monitor shows,
  including GPU buffers).
- Macs: a MacBook Pro with M4 Max, and a base M2 (8 cores).

## Results

| Mac | Scenario | Isocast CPU | OBS CPU | Isocast memory | OBS memory |
|---|---|---|---|---|---|
| M4 Max | 1080p60 program | 26.6% | 58.5% | 278 MiB | 1158 MiB |
| M4 Max | 4K60 program | 28.6% | 70.6% | 356 MiB | 1344 MiB |
| M4 Max | 1080p60 stream | 25.1% | 64.1% | 276 MiB | 1168 MiB |
| M2 | 1080p60 program | 27.8% | 67.6% | 88 MiB | 630 MiB |
| M2 | 4K60 program | 27.5% | 68.5% | 165 MiB | 836 MiB |
| M2 | 1080p60 stream | 26.1% | 68.2% | 88 MiB | 619 MiB |

Counting the VideoToolbox processes too, Isocast used 47–53% of OBS's CPU
for the same work. In the 4K60 program row on the M2, Isocast's file held
6210 distinct pictures out of 6241. OBS reported 3,904 frames skipped, and
only 1,645 of its 6,292 frames were distinct pictures (it repeated frames
it couldn't draw in time).

## What this doesn't show

- The "flagship" scenario (the 4K60 program plus the screen and camera as
  ISO files) isn't a like-for-like comparison: Isocast writes three files,
  OBS one. On the base M2 that load is more than its encoder keeps up with,
  and Isocast reports the dropped ISO frames.
- These are synthetic sources. Real screen and camera capture adds the
  same macOS capture cost to both apps and hasn't been compared yet.
- OBS's Metal renderer (new in OBS 32) is not in this table.

## Running it

`scripts/obs-bench.py --help` explains the options. It needs OBS installed
and a desktop session, and it never reads your real OBS settings: each run
gets its own OBS home folder.
