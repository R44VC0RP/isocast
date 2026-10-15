# Developer mode

For testing a build on your own Mac (GitHub #47): verbose logs of
everything the engine does, timing traces while recording, a live stats
overlay and a Developer menu. Off by default; while off it costs a single
atomic load where it would log or trace.

## Turning it on

- **App:** Settings › Advanced › Developer › Developer Mode, then Apply.
- **CLI:** `isocast debug on [--auto-off-hours N]`, `isocast debug off`,
  `isocast debug status [--json]`.
- **MCP:** `developer_mode` (no arguments reads it; `enabled`,
  `auto_off_hours` change it).
- **API:** `debug_status`, `debug_set {"enabled": true}`.

It is the profile's setting (`[developer]` in the profile, `developer` in
the live setup), so every client sees the same switch, and switching to a
profile without it turns it off. It turns itself off 24 hours after it was
turned on (Settings: "Turn Off After 24 Hours"; `auto_off_hours = 0` keeps
it on). The time it was turned on is kept beside the logs
(`developer-mode.json`), so restarting isocastd doesn't restart the clock;
turning it on again does. Changing it never touches a running recording.

## What it writes

Everything goes to the logs folder (`~/Library/Logs/Isocast`, or
`<socket>.logs` for an isolated engine) in segments of its own,
`<launch>-developer-NNNN.jsonl`, with their own 200 MiB budget, so the
ordinary 40 MB history is never evicted by them. The usual redaction
applies (stream keys, tokens, passwords). `isocast logs` reads them with
the rest (`--level debug`, `--component trace`).

- **Every API call.** Successful polling (`status`, `get`, `frame`…) is
  summarised per peer and minute in the ordinary log; in developer mode
  each call is also logged on its own (method, duration, response size,
  client), and calls over the ordinary log's per-peer rate are kept.
- **Debug messages** (`level: "debug"`) from capture (source opens),
  the compositor (scene resolves after a setup change), encoders (H.264
  sessions), writers (files created and finished), streaming (RTMP
  connect and publish) and the app's updater. None are per frame.
- **Timing traces** (`component: "trace"`), one record a second while the
  live engine runs, rather than one per frame:
  - `outputs`: each recording file's frames written, `dropped` (the
    encoder refused them), `late` (reached the writer more than two frame
    periods after their time), `age_us` (a frame's time, captured or the
    program's canvas tick, to its writer taking it: queues included),
    `submit_us` (the encoder append, including waiting for a busy
    encoder) and `writer_queue_max` (frames waiting inside the writer);
  - `encoder_start_order`: the files' first frames in the order their
    writers started them;
  - `program`: frame rate, missed ticks, composite wall time, GPU wait and
    CPU per tick (p50/p99 over the last 600 ticks);
  - `pacing`: every media clock's wake lateness (p50/p99, maximum) and late
    wakes;
  - `stream_encoders`: the H.264 stream and monitor encoders' frames in
    flight and submit-to-callback latency;
  - the recording's per-file counters, isocastd's CPU and memory.

  Volume: about 2.3 KB a second (8 MB an hour) for a 1080p60 program
  with two ISO files, 2.9 KB a second (10.4 MB an hour) for a 4K60
  program with three, and about 1.5 KB a second while the engine runs
  without recording; the 200 MiB budget holds about 19 hours of traces.

## In the app

While it is on, the menu bar has a **Developer** menu:

- **Stats Overlay**: frame rate and missed ticks, frame time, pacing,
  encoder queues, engine and app CPU and memory, and each recording
  output's last second (frames, drops, late, submit time, writer queue),
  at the stage's bottom left. It reads `debug_status` once a second only
  while shown.
- **Open Logs Folder**, **Copy Diagnostics** (a short redacted summary on
  the clipboard), **Show Update Feed** (the appcast this build reads, or
  the released one), **Restart Engine** (`shutdown_if_idle`: refused while
  anything records, streams, exports or feeds the camera; the app starts
  it again).
