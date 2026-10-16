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

## Stress suite

[scripts/stress.py](../scripts/stress.py) pushes the engine past normal
use and checks that it stays correct, not fast (PLAN S.2). Each scenario
runs on its own isolated engine with synthetic sources, and every take is
checked the same way: the manifest is complete, every file a start or
split returned exists, and every file passes ffprobe, decodes fully and
accounts for its frames (decoded + dropped − repeated = frame slots). It
reports drops per output, API latency (p50/p99/max per method), errors,
and the engine's memory footprint, threads and file descriptors before,
at peak and after the load. Growth past 300 MB, 24 threads or 32 file
descriptors fails, as do program or stream drops, audio dropouts, crashes,
panics, a slow shutdown and any API error that isn't a documented refusal.

| Scenario | What it does |
|---|---|
| sources | 42 sources (test patterns, colours, text) in 20 scenes; a scene switch every 0.25 s and a live setup edit every 2 s while recording |
| isos | asks `estimate_capacity` how many 1080p60 ISOs fit, records at that limit twice, then 3 past it while streaming: the ISOs shed pictures, the program and stream must not |
| switching | 100 scene switches a second through the API while recording: 20 s of cuts, then 10 s of fades interrupting each other |
| cycles | 100 held and 50 instant start/stop takes, 100 splits 0.6 s apart with a marker after each, 100 pause/resume pairs |
| clients | 16 process plugins polling `status` 20 times a second and watching events, 32 obs-websocket clients (the 33rd must get HTTP 503) and 32 `watch` connections, with scene switches while recording; afterwards the engine must be back to its threads and file descriptors |
| replay | the replay buffer beside a recording, saved 40 times (20 at 0.1 s, 20 at 0.5 s), every save checked |
| multistream | 4 stream destinations (one with its own size and bitrate) to local receivers while recording; one receiver dies and comes back, and only that destination may reconnect or drop |
| text_web | long text sources (up to 23,000 characters), four data-bound texts set 20 times a second, and 4 animated browser sources |
| disk | a 2 GB disk image with `keep_free_gb = 0.5`: too-long timed takes are refused (and an immediate retry is refused for the same reason), then another program fills the disk under a recording, which must stop by itself with every file finished |

Run it on a test Mac with `scripts/hw-batch.py submit stress [--scenario
isos,cycles]` (about 15 minutes for all nine), or directly with
`python3 scripts/stress.py --scenario NAME --out DIR`. `--profile quick`
records 720p30 with fewer clients and cycles, and reports drops without
failing on them, for slow machines.

On isocast-m2-b (base M2, macOS 26.7, main d66fbb8, 2026-10-16; hardware
job 20261016-032046-stress-suite-stress), every file the engine wrote
played and decoded with its frames accounted for (224 takes, 244 files,
40 replay saves), and no scenario left a thread or file descriptor
behind. Six scenarios passed; three failed on engine bugs: at the estimate's ISO limit, the program drops frames and
every file has audio dropouts; rapid splits promise files that never
appear; a start refused for disk space briefly leaves the recorder
"already recording".

| Scenario | Result | Numbers |
|---|---|---|
| sources | pass | 0/1201 program drops; 77 switches, all in the manifest; `apply` p99 126 ms |
| isos | fail | the estimate fits 5 ISOs (load 0.93); at 5 the program dropped 50 and 49 frames in two 20 s takes, with 502–527 audio dropouts, and the ISOs lost 4.3–4.5% (estimated 0%); 8 ISOs with a stream: program and stream 0 drops, ISOs lost 72% (estimated 56%) |
| switching | pass | 100.0 switches a second; `switch_scene` p50 0.35 ms, p99 1.9 ms; 0 program drops; 14 of 3,001 switches superseded within the same frame |
| cycles | fail | 214 takes verified; 100/100 pauses and markers in the manifests; of 93 acknowledged splits, 30 never produced their files and 7 more failed with VideoToolbox error −12903; `stop` of a sub-second take p50 878 ms, `pause` p50 258 ms |
| clients | pass | 8,927 plugin `status` calls (p99 3.7 ms), 6,690 obs-websocket requests (p99 4.4 ms), every client got all 40 scene events; peak 181 threads and 241 fds, back to 30 and 14 |
| replay | pass | 40 of 40 saves verified (5.03–6.0 s each); `replay_save` p50 284 ms |
| multistream | pass | main, d2 and d4 (its own encoder) 0/1200 drops; d3 reconnected once after its receiver died |
| text_web | pass | 0/1189 program drops; 350 `data_set` calls, p99 0.74 ms |
| disk | fail | the recording stopped by itself at 394 MB free with all 3 files playable; 2 of 5 immediate retries of a refused start were refused with "already recording" |
