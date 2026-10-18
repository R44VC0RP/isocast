# Self-tuning (#110)

This is the target design. T.1a's implemented coverage and measured limits
are in the lane's scratch `REPORT.md`; T.1b remains open. Calibration prioritizes
the three concurrent HEVC fit cells, 4K60 HEVC 10-bit and live constant-bitrate
H.264, then diagnostic recording codecs including all supported ProRes
variants. The deadline may skip diagnostic cells: the profile records each
one and its reason. Live H.264 is diagnostic, not a fitted resource-pool model.
Automatic runs belong only to the default daemon: setting `ISOCAST_SOCKET`
or `ISOCAST_SETUP` disables them unless `ISOCAST_CALIBRATION=on` opts in.
The snapshot, visual/layout, e2e, benchmark, stress, soak and motion harnesses
explicitly set `ISOCAST_CALIBRATION=off`; calibration must never silently
compete with a different daemon's recording. Explicit `machine calibrate`
still works, and intentionally exercises shared hardware.

## Contract and slices

T.1 measures this machine; it does not change a running encoder, admission,
or a user's setup. T.2 is the live governor. T.3 uses the evidence for new
setups and adds pins. T.4 qualifies deliberately constrained machines.
Existing recording priority (main, vertical, ISO) remains authoritative.

## The silent measurement

The daemon runs one disposable, low-priority helper, with synthetic pictures
only: no camera, microphone, screen, window, audio playback, network upload,
or permission requests. A parent-enforced 30-second deadline includes helper
startup and teardown. Native encoder/GPU calls can block, so a thread-only
timeout is not sufficient. Cancellation kills and reaps the helper before
allowing an output to start. Temporary files belong to this invocation and
are removed on success, cancellation and failure. On launch, sweep only
well-formed `.calibration-<pid>-<timestamp>` directories whose owner pid is
definitely dead; keep live/unknown owners and symlinks. A failed build is
remembered beside the profile, so it retries only on a new build or an
explicit `machine calibrate`, not every launch. Cancellation is not failure.

On first launch and after a measurement-version/app update, schedule the
default daemon's helper only while idle, with no live engine previewing or
starting. An incoming preview cancels an automatic run before opening its
sources; explicitly requested calibration remains available while previewing.
Refuse an explicit request while recording,
streaming, buffering replay, feeding a virtual camera, tuning or exporting.
An output request always wins over a calibration, including concurrent
starts: the check and reservation share the daemon's output-start gate.
Do not wait 30 seconds to let someone record. A cancelled automatic run is
deferred until the next idle opportunity, not retried in a tight loop.

Measure through `Platform` and the ordinary portable GPU path:

- Hardware recording codecs at 1920×1080 and 3840×2160, 30 and 60 fps,
  including preparation, warm-up and completed frames, not just submissions.
  Name backend, depth, quality, rate control and concurrency. Unsupported,
  timed-out and unmeasured cells are not zero-cost cells.
- Concurrent HEVC/H.264 sessions and live bitrate-limited H.264 separately:
  a single session does not reveal engine count, placement or ISO recovery.
  Session startup follows the existing #41 spacing, not simultaneous opens.
- Offscreen composition and BGRA-to-NV12 conversion at HD/4K, synchronized
  GPU completion, excluding shader startup from steady-state timing.
- Physical memory, bounded copy throughput (never pressure the whole RAM),
  and bounded sequential scratch writes including sync. Disk results name
  their volume; they never replace the recording destination's space check.

Budget: approximately 20 seconds for codecs/concurrency, 4 for GPU,
2 for memory/disk and 4 for startup/cleanup. Stop starting new cells before
the deadline; preserve explicit partial coverage. Do not certify unmeasured
codecs, depths, sizes or mixed loads by scaling a convenient single number.
Low priority protects interactive work but also lowers the measurements:
record this condition; measurements are an observation, not a guarantee.

## Profile and validity

Versioned JSON lives beside the daemon's legacy setup as
`setup.machine.json`, independent of named recording profiles/collections.
An isolated `ISOCAST_SETUP` therefore isolates it too. The envelope holds
schema/measurement/app versions, timestamp, platform/OS, CPU and memory,
adapter/backend identities, elapsed time, complete/partial state, individual
measurements with units/sample counts, and any rejected cells with reasons.
No serial number, device/capture names, stream key or content is stored.

Validate version, machine/OS/backend fingerprint, finite positive timings,
dimensions and sample bounds before use. Missing, corrupt, stale or foreign
profiles use the current per-chip table unchanged. Save by writing and
syncing a sibling temporary file then replacing the old file atomically;
cancelled/failed runs never overwrite a useful profile. No profile is sent.

## Consumers

`machine_calibrate` and `machine_profile` on the one method table serve CLI
`isocast machine calibrate|show|cancel`, MCP and the app. Calibration returns
promptly with state; status reports progress, age, coverage and fallback
reason. Settings shows the same summary and offers recalibration/cancel.

Capacity estimates, their suggestions and ISO admission must use the same
model. Prefer qualified measured service costs for covered workloads;
retain the table for uncovered cells, and label mixed measured/assumed
predictions. Preserve per-picture overhead and separate live H.264 cost
(#79/#96). Never infer multiple-engine capacity from one encoder's speed.
The fit requires full (at least 0.5 s) windows and HD60×4 throughput no lower
than HD60×2. These three cells precede diagnostic codec cells in calibration.
Existing exact `tune` failures remain a safety override. Optimize uses the
profile to choose/rank candidates but still measures the actual setup when
coverage is incomplete; `speed_test` remains an explicit, separate network
operation with its existing no-test-mode restrictions. A recording-only
Optimize keeps an exact tune pass; a more conservative model gives a note,
not another setup change. `setup check` and doctor render the daemon's same
profile-aware estimate, including its tune memory. A file's `setup check`
connects only to an already running daemon: when none is reachable it says
capacity was **not checked**, without starting an engine; file validation
still completes locally. Calibration alone
never applies a setup patch. Missing/invalid profiles preserve table-based
capacity estimates. New-setup defaults are a separate T.3 policy below.

## New setups and pins (T.3)

The daemon starts new `setup init` setups and newly created blank profiles at
30 fps on machines with at most eight CPU cores or eight GB of physical RAM.
The initialized canvas remains 1080p unless a size was explicitly supplied;
a display below 30 Hz keeps its slower default. An explicit `setup init`
fps wins. Existing setups, imported profiles and copies are never changed.
The heuristic uses portable machine facts, not a guessed GPU vendor/session
limit. It leaves more headroom on the measured base M2, but is not proof that
every source, eight-GB machine or software encoder will keep up. Capacity
checks and explicit Optimize still evaluate the actual workload.

Persistent `governor.pins` are the per-setting opt-out for the supported
runtime controls, listed below. Selecting Automatic removes the relevant
pin; pinning restores its requested value where the backend accepts that
restoration. Refusal remains visible. No default enables ISO thinning and
no pin disables the native hardware safety controller.

Real sessions may later refine conservative bounds only when their workload,
output counters and outside-load state are known. A clean short take proves
that workload kept up, not the machine's maximum capacity. Contention and
thermal episodes never permanently lower an otherwise healthy profile.

## Live governor (T.2)

The first runtime slice implements monitor refresh, vertical encoder
admission (only while main is protected), shared-stream bitrate ceilings
and, only when the user opts in, ISO admission. The governor never
implicitly lowers main-program or ISO recording quality: codec, size,
compression quality, audio and frame rate stay as set. File
size/codec/quality changes and software-backend transitions remain
unqualified and are not attempted inside a take. The governor says
when no further safe live reduction is available; it cannot make an external
application leave enough hardware for the protected outputs.

`[governor] enabled = false` turns off these overrides, not the pre-existing
native encoder safety controller. ISO temporal shedding by the governor is
an explicit user choice: `allow_iso_thinning = true` (Settings → Advanced →
Automatic Quality → "Allow ISO Frame Thinning"), **off by default**. Off,
every ISO counts as pinned. On, skipped ISO pictures are permanently missing
from those files; the switch's hint says so. `pins` accepts `preview.fps`,
`recording.iso.fps`, `recording.iso.sources.<source-id>.fps`, `vertical.fps`
and `stream.video_bitrate_kbps`; the ISO pins remain overrides when thinning
is allowed (and are kept, but have no extra effect, while it is off).
Settings edits the same profile fields through the normal draft/Apply path.
Pins and the default prevent governor overrides, not physical frame loss or
native overload protection: the pre-existing native encoder safety
controller (#41/#60/#68) still sheds ISO pictures on real encoder overload,
before the program, whatever these settings say.

Optimize, `capacity` suggestions, ISO admission estimates, `setup check` and
doctor never turn thinning on or count it toward a fit. Optimize's notes
mention the choice when ISO files are enabled; a strained setup's capacity
check line also mentions it. Runtime limits are reported in
`status.governor` and `governor_changed` events, never written over the setup.

Diagnose the bottleneck before selecting a step. CPU starvation is not
automatically encoder overload: check capture/conversion placement and
thread QoS, and ensure the governor itself can run. Preview/ISO changes
below target GPU/encoder work; bitrate reductions target upload/disk, not
assumed VideoToolbox savings. Read the same pressure signal as ISO admission
(#41/#60/#68); its intentional dropped ISO frames are not a second feedback
signal, or the controllers can oscillate.

Proposed reversible step order, each requiring evidence of its bottleneck:
1. Reduce preview/projector refresh (60 → 30 → 15), never recorded fps.
2. Only with `allow_iso_thinning` opted in: reduce unpinned ISO work (fps
   before size); keep audio and frame/drop accounting. A size change
   requires a new file/segment, never an in-place track-description
   mutation. Without the opt-in this step is skipped.
3. Lower encoder effort; lower bitrate for measured upload/disk pressure,
   not as an assumed cure for hardware saturation.
4. Reduce unpinned vertical output work, preserving main program/stream.
5. Consider software encoding only when measured CPU headroom demonstrates
   it helps; otherwise explain the remaining overload rather than worsening it.

Use queue age, completed-frame latency, compositor deadlines, memory/thermal
state, and #90's outside-load detector together. Its five-second baseline
blind period is not evidence of spare capacity. Require sustained pressure
for 2 seconds, wait at least 5 seconds between ordinary downward steps.
With the opt-in, protected program loss bypasses this ladder: immediately
shed all unpinned ISO video work, retaining audio/accounting and the saved
intended settings. Impending queue/resource exhaustion also bypasses
ordinary delays. The emergency ISO hold is a **non-renewable three-second
overlay**, rearmed after five quiet seconds. It is not a permanent zero-FPS
setting. A helpful CPU/GPU/disk hold may leave a proportional ISO ceiling
(at least 25%) for the slower recovery policy. Without the opt-in neither
the hold nor the ceiling applies to any ISO. Encoder-only steady shedding
remains owned by the native controller in either case; a second sticky ISO
limit was measured to waste pictures without improving protected losses
and was removed. Required first/split pictures and all audio remain exempt
from admission thinning.

Transport shedding is not video-production loss. Upload-only pressure never
holds recording ISOs: it lowers bitrate on congested canvases, including an
urgent reduction when the one-second send queue is half full. Encoder owners
apply bitrate changes between frames without replacing the session or FPS;
all consumers sharing a session see its acknowledged bitrate. A refused
change is reported and does not terminate the stream/recording.
An H.264 monitor likewise keeps running after a refused optional retune,
using its last acknowledged encoder settings while frame selection follows
the requested ceiling. An unchanged refused request is not retried per frame.
Errors belong to the affected monitor: another healthy monitor cannot clear
them, and closing that feed removes its error.
If native rollback itself fails, the backend retains an uncertainty flag.
Returning to the last acknowledged rate still makes a full native request;
the preview cannot silently clear the warning by comparing equal numbers.
FPS rollback is attempted even when bitrate rollback fails.
Restore one step after 30 seconds below 70% measured budget with no losses;
wait 30 seconds again before restoring another. A failed restoration backs
off for 2 minutes. These are initial values to qualify, not a zero-drop
promise against uncooperative external encoders.
Upload-only pressure does not block recovery of video-work ceilings. The
native fast admission share remains authoritative beneath every ceiling.

The active-output sampler also reads macOS's kernel VM-pressure level.
Warning/critical memory pressure wakes policy, permits preview reduction
and blocks restoration; ISO intervention still requires consent. This is
the OS pressure state, not a threshold on free RAM. Unsupported/read failure
is unknown. No memory syscall is added to media callbacks or idle ticks.
Rate ceilings reduce work/churn; they do not promise to release resident
pools or solve pressure from other applications.

A newly congested canvas joins the stream bitrate targets even when another
has reached the shared 25% floor. This does not make independent bitrate
hubs on one canvas independently recoverable: acknowledgement/failure is
still canvas-scoped and requires further qualification.

Intentional admission refusals are attributed by the writer/encoder that
made them, not by inspecting the ceiling when delayed counters arrive.
They still count as lost pictures in the file/stream report, but restoring
or pinning vertical cannot turn them into fresh protected-loss events.
On unchanged, pressure-free ticks the daemon only probes lifecycle/pressure;
it does not clone setups/status strings or gather full engine statistics.
Idle native headroom sampling is also skipped. Recovery and settings changes
still wake the policy; this is effectively zero incremental idle work, not
a claim that the daemon has no timers or CPU cost.

Every step emits old/new values, reason, scope and restoration state, shown
in status and a plain-language app notice. Pins and `allow_iso_thinning`
are explicit fields in the recording profile, persist across launches, and
pins prohibit automatic changes to those fields. Pins are explicit; a manual
edit does not currently add one implicitly. Never silently overwrite a pinned setting; explain when
the remaining load cannot fit. Governor state is runtime state, separate
from the saved user's intended setup.

## Windows and constrained machines

Profiles describe backend/resource pools rather than Apple chip names:
VideoToolbox today; NVENC, QSV, AMF and software when those Platform backends
land. Record driver/backend identity and session limits. A discrete GPU's
copy/upload costs differ from unified memory; measure the real conversion
and transfer path. No Apple engine count or ProRes independence is assumed
on Windows. Unsupported calibration leaves today's fallback intact.

T.4 runs existing e2e/bench/soak through the hardware queue's lock with an
isolated daemon and bounded taskpolicy/CPU duty-cycle constraints, plus a
separate offscreen GPU-contention helper. Record requested and observed
constraints: taskpolicy is scheduling pressure, not a simulated slower chip;
SIGSTOP duty cycling introduces stalls, not fewer physical cores. Memory
pressure is bounded to scratch helpers and released on every exit. Compare
interleaved unconstrained/constrained runs, predicted vs actual drops,
protected-output integrity and the governor's visible decisions. Windows
GPU-less software encoding is qualified in the Windows lane, not inferred
from a throttled Mac.

Before T.2, measure competitors only, with Isocast at normal priority.
`weak-machine.py` now defaults to this; `--background` explicitly recreates
the earlier taskpolicy-on-Isocast experiment. The 86%/96% losses in that
experiment include scheduler clamping of Isocast's real-time threads and
must not be used to choose a governor step order for ordinary machines.
