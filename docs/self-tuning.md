# Self-tuning (#110)

This is the target design. T.1a's implemented coverage and measured limits
are in the lane's scratch `REPORT.md`; T.1b remains open. Calibration currently measures HEVC,
H.264 and ProRes 422, not every ProRes variant or live rate-control mode.
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
default daemon's helper only while idle. Refuse an explicit request while recording,
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
profile-aware estimate, including its tune memory. Calibration alone
never applies a setup patch. New-machine behavior is unchanged until a valid
profile exists; low-end defaults are T.3, not an unreviewed T.1 side effect.

Real sessions may later refine conservative bounds only when their workload,
output counters and outside-load state are known. A clean short take proves
that workload kept up, not the machine's maximum capacity. Contention and
thermal episodes never permanently lower an otherwise healthy profile.

## Live governor (T.2)

Diagnose the bottleneck before selecting a step. CPU starvation is not
automatically encoder overload: check capture/conversion placement and
thread QoS, and ensure the governor itself can run. Preview/ISO changes
below target GPU/encoder work; bitrate reductions target upload/disk, not
assumed VideoToolbox savings. Read the same pressure signal as ISO admission
(#41/#60/#68); its intentional dropped ISO frames are not a second feedback
signal, or the controllers can oscillate.

Proposed reversible step order, each requiring evidence of its bottleneck:
1. Reduce preview/projector refresh (60 → 30 → 15), never recorded fps.
2. Reduce unpinned ISO work (fps before size); keep audio and frame/drop
   accounting. A size change requires a new file/segment, never an in-place
   track-description mutation.
3. Lower encoder effort; lower bitrate for measured upload/disk pressure,
   not as an assumed cure for hardware saturation.
4. Reduce unpinned vertical output work, preserving main program/stream.
5. Consider software encoding only when measured CPU headroom demonstrates
   it helps; otherwise explain the remaining overload rather than worsening it.

Use queue age, completed-frame latency, compositor deadlines, memory/thermal
state, and #90's outside-load detector together. Its five-second baseline
blind period is not evidence of spare capacity. Require sustained pressure
for 2 seconds, wait at least 5 seconds between ordinary downward steps.
Protected program loss bypasses this ladder: immediately shed all unpinned
ISO video work, retaining audio/accounting and the saved intended settings.
Impending queue/resource exhaustion also bypasses ordinary delays.
Restore one step after 30 seconds below 70% measured budget with no losses;
wait 30 seconds again before restoring another. A failed restoration backs
off for 2 minutes. These are initial values to qualify, not a zero-drop
promise against uncooperative external encoders.

Every step emits old/new values, reason, scope and restoration state, shown
in status and a plain-language app notice. Pins are explicit field paths in
the recording profile, persist across launches, and prohibit automatic
changes to those fields. Manual edits pin the edited setting until the user
chooses Automatic. Never silently overwrite a pinned setting; explain when
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
