# Crash-safe takes (G.3b, GitHub #3)

Design, options with evidence, and the default recording path for 0.0.5
(branch `agent/g3b-crash-safe`, 2026-10-15). Clean-room: ISO/IEC 14496-12
(ISO BMFF), Apple's QuickTime File Format specification and AVFoundation /
VideoToolbox / AudioToolbox documentation, plus inspection of files
AVAssetWriter wrote; no OBS source.

## 0.0.5 policy and fallback

Crash-safe recording is on by default, with a **500 ms** fragment interval.
Recovery runs automatically on the next engine launch (including remembered
project folders); no separate `isocast recover` command is needed. Recovery
keeps every complete sample it finds, even when audio and video end at
different times. The common recoverable duration is a conservative watermark,
not an instruction to trim the longer tracks.

For this release only, launch the engine with `ISOCAST_CRASH_SAFE=0` to use
the legacy AVAssetWriter path. `false`, `no` and `off` also disable it. Restart
the engine after changing its environment; setting a variable on a CLI that
connects to an already-running engine does not change its writer. This fallback
restores the old ~2 s crash-loss window and unknown pause recovery length.
It does not disable repair of crash-safe files recorded earlier. There is no
new setup field or UI-only setting.

Recording encoders use the same process-wide first-picture gate as the legacy
writer and streaming: protected program first, then largest ISO first, at
least 25 ms apart (#41). Queued startup pictures drain before the smaller ISO
queue limit applies. With our direct VT encoder, the initial keyframe can
return before the following pictures: startup ends only after two pictures
have returned and the pending queue is below the ISO limit. ProRes takes no
start-gate turn. The shared stream/replay encoder is
still placed and parked **before** recording writers are created, and retained
across splits (#68). The 200 ms closing window and stream pressure feedback
(#60) are unchanged. The recording's encoder is not shared with replay yet.

## The problem, precisely

The legacy recording path hands raw `CVPixelBuffer`s to `AVAssetWriterInputPixelBufferAdaptor`
and float PCM to AAC `AVAssetWriterInput`s (`crates/isocast-platform-macos/src/writer.rs`),
with `movieFragmentInterval = 2 s`. AVAssetWriter writes a movie fragment only
once a sample at or past the boundary is appended, writes the fragmented
`moov` itself only with the first fragment (so a take under ~2 s has no movie
header at all), cannot flush on demand (`flushSegment` is refused outside
manual delegate segmentation, which is passthrough-only and excludes MOV and
ProRes: 15 native probes, `g3b-segments` report), and acknowledges queued
inputs, not bytes on disk. So a crash or `kill -9` loses up to the last
2 s fragment plus whatever the encoder still held (worst observed common
A/V loss 2.25 s; 15 of 60 files from random kills had no header), a take
under 2 s entirely, and a pause cannot report a recoverable length
(`recoverable_seconds_at_pause: null`, R1 contract).

Anything that fixes this has to own the encode and the mux: the samples
must be ours to index and write when *we* decide.

## What a finished Isocast file looks like today

An AVAssetWriter movie with fragments, after `finishWriting`, is
`ftyp`, `wide`, one `mdat` spanning the whole middle, `moov` at the end: the
fragmented `moov`, every `moof` and the intermediate `mdat` headers are still
in the file, as unused bytes *inside* that single `mdat` (seen at offsets
89620 (`mvex`), 177880, 268392, … of an 8 s HEVC take: the final `mdat`
header at offset 28 was patched to cover 361484 bytes). Tracks: `hvc1` with
`hvcC` + `colr` (nclc 1/1/1), `mp4a` (QuickTime version 1 sound description
with a `wave`/`esds` atom), movie timescale 48000, audio edit list
`media_time = 2112` (the AAC encoder's priming) and the audio media three
packets longer than the movie. This is the compatibility target: editors
already accept exactly this layout from Isocast.

## Options

All three options use the same encoders: a `VTCompressionSession` per file
(H.264, HEVC 8/10-bit, every ProRes flavour: VideoToolbox encodes ProRes on
Apple silicon, the same engine AVAssetWriter uses) with the settings
`writer.rs` gives AVAssetWriter (constant quality, keyframe ≤ 1 s, no
reordering, hardware only for H.264/HEVC, Rec. 709 tags, letterbox scaling
for a smaller program file, the M4 Max look-ahead quirk), and the existing
AudioToolbox AAC-LC encoder from `stream.rs` per audio track. They differ in
what hits the disk when.

### (a) Fragmented MOV/MP4 written by us, finished in place — **recommended, prototyped**

While recording the file is `ftyp`, a 16-byte `free` placeholder, a
fragmented `moov` (empty sample tables, `mvex`/`trex`), then `moof`+`mdat`
pairs. Each `moof` (`mfhd`, one `traf` per track with `tfhd`
default-base-is-moof, `tfdt`, `trun` with per-sample sizes, durations and
sync flags) is a complete index of the `mdat` after it, so the file plays in
QuickTime Player, AVFoundation and ffmpeg up to the last complete fragment
at every moment (the g3b-segments kill matrix already showed AVFoundation
and ffmpeg decode such files; QuickTime fragments in `.mov` are what
AVAssetWriter itself writes). A fragment is written when its media span
reaches the interval (`ISOCAST_CRASH_SAFE_INTERVAL_MS`, 500 ms), or it
holds 24 MB (ProRes), or on demand: the first encoded picture goes out
when the second fixes its duration; a pause asks every
file to drain its encoder (`VTCompressionSessionCompleteFrames`), write and
`fsync`, and only then replies with the number.

Timing: a picture lasts until the next one. The writer holds its newest
encoded picture until the next arrives (or a commit or Stop releases it
with its nominal duration), so every `trun` carries the true duration and
a gap where frames were dropped stays in the file wherever it fell,
including right after the first picture (the first fragment is written
when the second picture arrives, not before); the final sample table is
rebuilt from the sample times as well (the same code recovery uses), so
Stop and recovery agree and nothing after a gap moves earlier against the
sound. (The first review caught the earlier version compressing gaps at
fragment boundaries by up to a second; the re-review caught the first
picture being released early.)

At Stop the complete sample tables (`stts`, `stss`, `stsc`, `stsz`, `stco`
or `co64`, the audio edit list for the 2112-sample priming plus the `roll`
groups) are appended as an ordinary `moov`, the data is flushed through,
and the placeholder is overwritten with one 16-byte `mdat` header spanning
everything up to that `moov`: the same plain `ftyp`/`mdat`/`moov` movie
AVAssetWriter produces, with the old indexes as unused bytes inside the
`mdat`. Every intermediate state is recoverable: before the patch the file
is a fragmented movie with a trailing duplicate `moov` (readers take the
first); and recovery never reads the 16 header bytes to decide anything,
since a write can tear after any number of bytes: the file is one of ours
only if its first box is this writer's `ftyp` byte for byte (it carries an
identifying compatible brand, `isoc`, from creation; AVAssetWriter movies
and look-alikes with another muxer's `ftyp` are never touched) and the
fragmented `moov` sits right behind the header bytes; it is finished only
if a complete final `moov` sits exactly where the header's length points
and the header is that `mdat` header; every other state (0 to 16 bytes
written, or garbage) is rescanned and patched again (unit test over all
17 states; the reviewer's syscall-level tears 0–16 on real media).

Recovery at the next launch (`Platform::recover_file`, called by
`recover_interrupted` for every file of a manifest that says recording,
interrupted *or failed*, so a file a write error failed, or a repair that
failed once, is retried; the manifest keeps its original error and gains
a note of what was finished, and the rebuilt movie's picture count
replaces the writer's accepted-input counters). Recovery only ever
modifies a regular file inside the folder whose recording lock it holds
for the whole pass: the file is opened through the folder with
`openat(O_NOFOLLOW)`, symbolic links and hard links (`st_nlink > 1`) are
refused, the manifest entry must be a plain file name, and the same
descriptor is used for every read and write (no path is re-resolved), so
a staged alias cannot make recovery write into a movie another engine is
still writing elsewhere. The recorder also holds an advisory lock
(`flock`) on its movie for as long as it writes it, and repair try-locks
the file it opened: a file still being written, even one renamed into
another folder, is left alone; the lock dies with a crashed process, so
its take stays repairable. Only a lock held elsewhere (`EWOULDBLOCK`)
refuses: on a share that refuses `flock` altogether (some SMB and NFS
mounts) the take records without the lock, with a warning in the log, and
repair, which needs the folder's lock, doesn't run there. If a complete final `moov` follows the
fragments the crash was between the `moov` append and the patch: patch
only. Else the tracks come from the fragmented `moov` (its `stsd` entries
are copied verbatim, so recovery never needs an encoder), the samples from
every `moof` whose `mdat` is complete, and the file is finished exactly as
Stop would have, in ~0.2 s including the daemon restart. Each kept track
presents all the media it has and the movie is as long as the longest of
them: sound recorded after the last saved picture, or with no picture at
all (the video stalled after its first picture, which is held until the
second), stays presented; a track without any sample is left out of the
movie, and the note says which ("audio 0.937 s; left out: video (no
picture)"). A partial trailing fragment stays in the file as
`mdat` bytes; nothing is truncated. A writer that fails with a write
error also tries this repair at Stop and says in its error whether it
worked.

AAC timing: the fragmented state delays the video by the priming duration
in `tfdt` (what AVAssetWriter does in its fragmented MP4 output: 0.044 s in
the probes), the finished state uses the audio edit list instead, like
AVAssetWriter's movies; recovery knows both. Several AAC tracks are just
several `traf`s and `trak`s. Pause works through the existing cut clock
(`file.rs` maps times; the writer never sees paused media) plus the commit
barrier; split/retake create a new writer per take as today, so a split
is a normal Stop of the old file; timeline export and `join.rs` read
finished files through AVFoundation as before.

Cost: the same hardware encodes (VideoToolbox does the work either way), a
copy of each compressed sample out of its `CMBlockBuffer` into the fragment
buffer (AVAssetWriter does the equivalent internally), two `write` calls
per fragment, ~250 bytes of `moof` per fragment, and memory for one
fragment per file (≤ 24 MB; a few MB for HEVC). The file is flushed
through to the drive (`sync_data`, which is `fcntl(F_FULLFSYNC)` on Apple
platforms: a drive-cache flush, tens of milliseconds on some drives) on
pause and Stop by default; `ISOCAST_CRASH_SAFE_FSYNC_SECONDS` adds a
cadence, checked whenever a fragment is written (so it is a cadence
between fragments, not a wall-clock heartbeat). A process crash loses
nothing the kernel already has, so the kill bound does not depend on
flushing; power loss does, hence the knob.

### (b) Short sealed segments joined at Stop — rejected

Rotate complete files every 1–2 s and concatenate at Stop. Measured in the
g3b-segments lane with the existing split/join: +62 % / +29 % retired
instructions at 1 s / 2 s rotation, a second AAC encode in the join (so no
bit-preserving audio), seconds of join work after Stop that grows with the
take, one lossy run, and still no commit on pause. Native AVAssetWriter
segment output is the same idea with worse limits (MP4 only, no ProRes, one
audio track, no manual flush with compression). It solves assembly, not
durability, and its cost is not "unchanged".

### (c) Write-ahead sample log rebuilt at the next launch — rejected as the main path

Append `(track, time, size, bytes)` records to a log with a periodic
`fsync`, and build the movie from it at Stop (a full second copy of every
byte, so Stop takes seconds per GB and disk writes double) or at the next
launch (the take is unplayable by anything but Isocast until then). Writing
the samples straight into the movie's `mdat` with the `moof` index
alongside, option (a), is this log with the index in the same file: same
write pattern, no copy, and self-describing to any player. A side-car
*index* journal would add sample-level recovery of the partial trailing
fragment to (a) at ~3 KB/s; not worth a second file today, listed under
"later".

## Durability guarantee

Let *I* be the fragment interval (500 ms), *E* the input-to-encoded-output
latency (including the bounded raw-picture queue and ≤ 8 pictures submitted
to VideoToolbox, typically 1–3 native frames with `RealTime`), *P*
the AAC priming plus the partial packet (2112 + ≤ 1023 samples = 44–65 ms),
*A* an audio track's own delivery delay behind the video (the program mix
runs a jitter buffer; ISO sources deliver within a chunk).
The raw queue is bounded at 32 pictures during startup and for the protected
program, and four for an ISO after startup. Queued or encoder-held pictures
are in flight, not part of `committed_seconds`; under overload their delay
must be added to the 500 ms fragment interval, not hidden by that number.

- While recording, a crash keeps every file up to its last fragment: video
  within *I* + *E* + one frame (the held picture) of the newest frame,
  audio within *I* + *P* + *A*.
- The first fragment is on disk within *E* + one frame + one write of the
  first frame (it waits for the second picture, which fixes the first
  one's duration): no take is lost entirely once two frames reached its
  file. (The Start reply itself does not promise a picture: the first
  frame is due 200 ms later.)
- Pictures the encoder accepts and then discards on its own (VideoToolbox
  dropped-frame callbacks, also those delivered while finishing) and AAC
  packets short of the input are counted as dropped frames / dropouts in
  live status, the report and the manifest; they are not in the file.
- At a pause, the reply's `recoverable_seconds` (and the manifest's
  `recoverable_seconds_at_pause`, written and flushed with its folder) is
  the saved-timeline position every file is on disk and flushed up to,
  read back from the writers after they drained their queues, completed
  their encoders and wrote their fragments. When every input before the
  cut had arrived in time (within 500 ms of the cut) that is the video up
  to the cut and the audio up to cut − *P*; a track still delivering
  behind the cut (the program mix under overload) makes the number
  smaller, never the number wrong. A file whose commit fails, or that
  does not answer within 3 s, leaves the pause with no number, its error
  in the reply and the manifest, and the file failed: nothing durable is
  promised for it. A kill at any time during the pause recovers exactly
  the stated amount.
- During a split, the ending take's pictures are committed at the split
  (a commit failure fails that file, loudly) and its tail audio (≤ 500 ms)
  when it arrives; the new take's files start with their first frame
  (same as a start).
- Stop and recovery produce the same file; a crash during Stop's finish,
  including in the middle of the 16-byte header patch, is finished by
  recovery with no loss.
- Not a bound on anything VideoToolbox had not returned when the process
  died, nor on power loss between flushes (a `kill -9` keeps what the
  kernel has; a power cut keeps what the drive has): with the default
  cadence there is no bounded power-loss window while recording, only at
  pause and Stop. No power cut was tested.

## Measured (test Mac isocast-m2-a, base M2, macOS 27.0, synthetic sources)

Kill matrix, flag on (15 seeded runs, `kill -9` at random media times,
during a pause, right after start, 160 ms into a split's new take, after
resume; HEVC/H.264/ProRes 422, MOV/MP4): 36/36 files decode fully after
recovery (`ffmpeg -xerror`), 36/36 unfinished files read by AVAssetReader
before recovery; video loss ≤ 0.499 s (median 0.146 s), program audio
≤ 0.681 s (median 0.209 s); a kill during a pause keeps the video exactly
to the cut and the audio exactly to the stated recoverable length; recovery
135–157 ms including the daemon restart. The same cases with the flag off
(AVAssetWriter) lose up to 2.16 s and 30 of 60 files entirely.

A/B (`scripts/bench.py` flagship-1080, 3 interleaved 20 s runs): main vs
this branch with the flag off is identical (562.3 vs 562.3 M instructions/s,
49.8 vs 50.0 % CPU; after the fix round 562.9 vs 562.3); flag on retires
11 % fewer instructions (499.9 M/s; 498.2 after the fix round), uses 14 %
less daemon CPU, +24 MB RSS, the same VT load, the same file sizes within
3 %, no drops. Full tables and the raw rows:
`/Volumes/IsocastBuild/g3b-crash-safe/scratch/REPORT.md`; summary in
`docs/notes.md` (Benchmarks, 2026-10-14).

Fix round 1 (after the adversarial review): the reviewer's SIGSTOP stall
cases finish with `mdhd` = `stts` = edit = ffprobe video end =
AVFoundation decoded end (3.983 / 4.333 / 5.683 / 5.100 s, stall gaps of
0.65–0.98 s kept in the table; before the fix the video ended 0.6–1.0 s
early against the sound); their 28-run matrix: 48/48 picture-bearing
files decode with every AVFoundation track complete; EIO on a write, on
the pause's flush and on the split's flush fail the file loudly (the
Pause reply's `errors`, the manifest) and the file is finished from its
fragments at Stop or at the next launch; a header patch torn after eight
bytes is recovered; kills during Stop and during recovery (the third
launch finishes the job) keep every file.

## Remaining qualification and future work

- Remove the temporary legacy-writer environment fallback after one release;
  `committed_seconds` in the app's REC details is a separate UI follow-up.
- QuickTime Player / Final Cut / Premiere import checks of both the finished
  files and an unfinished (fragmented) file on a Mac with a desktop; so far
  ffprobe/ffmpeg full decodes and AVFoundation's `media_info` probe.
- The AAC bitrate-control mode of the recording encoder (constant, as the
  stream encoder) versus AVAssetWriter's default; audio file sizes.
- Optional sync/dependency groups (`sdtp`) beyond the AAC roll groups already
  written for AVFoundation.
- B-frames stay off (as in writer.rs); this muxer has no composition
  offsets, so turning them on would need `ctts` in the final table and
  composition offsets in the fragments.
- A side-car sample index for sample-level recovery of the trailing
  fragment, if 500 ms is not enough.
- The M4 Max / M5 Max real-rig flagship qualification, and a 2-hour soak
  (fragment count ~14400 per file at 500 ms; `moov` size stays small: the
  sample tables are run-length coded).
