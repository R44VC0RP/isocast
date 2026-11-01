# Report a bug

On macOS, Windows or Linux, open **Help → Report a Bug…**, describe what you
did and what you expected, and update the preview. **Send Report** makes the
previewed issue text public on GitHub and sends the zip privately to maintainers.
**Save Zip Only** does not send anything. You can open a prefilled issue instead;
attaching a zip to GitHub yourself makes it **public**. The triage block is
copied for pasting when the desktop clipboard is available; otherwise use `ISSUE.md`.

If the local engine won't start, the app can still preview, save and send a
reduced report with your description, app version, OS, and recent local launch
markers. It omits raw historical logs (the engine owns profile-key redaction),
devices and live engine checks. Update Preview after reconnecting for a full report.

From the CLI:

1. Preview locally. `isocast report --describe "What happened, and what you expected" --preview --json` returns a compact file
   index with sizes, ISSUE.md, redaction policy and a `preview_id`, without
   creating a zip. Read selected contents with
   `isocast report-file PREVIEW_ID FILE --offset 0 --limit 4096 --json`.
2. Send it: `isocast report --submit --preview-id PREVIEW_ID` (or
   `isocast report --describe "…" --submit` in one step; MCP `bug_report`
   with `submit: true`; the app's Send Report). The engine writes the zip,
   checks it for secrets, and posts it to the report service
   (services/report-worker). ISSUE.md becomes a public GitHub issue (or a
   comment on an open issue with the same crash), and the zip is kept
   privately for the maintainers under a report id. The issue URL is
   returned. Sending happens only with this explicit flag. If it fails
   (offline, refused, rate limited, no service configured), `submit_error`
   says why, the command exits nonzero, and the zip is kept: attach it to a
    new public issue only if you're comfortable sharing it. The built-in endpoint
    is configured. For testing, set `ISOCAST_REPORT_ENDPOINT` in both the daemon
    and app environments to a loopback receiver. Submission uses the OS trust
    store and does not need a command-line HTTP tool.

`--out DIR` chooses a directory, `--launches 1..10` selects the most recent
launches of each process (default 3). `--private` (alias `--hash-paths`)
pseudonymizes paths, titles, device names and device IDs. Reports default to
`~/Downloads/Isocast Reports`; isolated engines keep reports beside their logs.
The API methods are `bug_report`, `report_file` and `logs`; MCP exposes
`bug_report`, `read_bug_report_file` and `get_logs`.

## Bundle layout

| File | Meaning |
|---|---|
| `ISSUE.md` | Description, short summary, valid fenced JSON triage block; under 60,000 characters |
| `environment.json` | OS version, CPU, cores, RAM, GPU and driver when available. macOS also includes displays, system/recording volumes, locale, power, signing and capture process names/PIDs. |
| `doctor.json` | Machine-only doctor checks, including video encoder availability. Does not probe setup sources, request permissions or test-write to recording folders. |
| `build.json` | Version, compiled commit, release/debug profile, architecture |
| `status.json`, `diagnostics.json` | Current outputs, last recording/error, prior abnormal launches and logger health |
| `setup.json`, `profile.json`, `library.json` | Current revisioned setup, current profile document and library inventory |
| `devices.json`, `permissions.json`, `plugins.json` | Device formats/rates, non-prompting OS permission states, plugin versions/grants/state |
| `logs/` | Last N launches per process, JSONL segments, launch markers, Rust crash files |
| `bootstrap.log` | Redacted startup stderr, preserved across launches when available |
| `plugins/` | Bounded, redacted current/previous process-plugin output |
| `metrics.jsonl` | Extracted one-second metric records (also present in logs) |
| `macos-crashes/` | Up to ten user/system macOS DiagnosticReports whose names identify Isocast |
| `manifests/` | Up to ten recent session manifests in current/recent recording folders; no media |
| `REDACTION.json` | Redaction categories/counts, privacy policy and files omitted by bounds |

Preview content is cached, already redacted, for five idle minutes (at most
two previews/128 MiB). `report_file` accepts only indexed names and UTF-8 byte
offsets, up to 16 KiB per page; `next_offset` continues. MCP sends the preview
once as structured content, with a short text summary rather than a second
copy. `logs` responses have a 32 KiB record budget; large records have a compact
summary pointing to the report file.
Archives use a new, private filename (0600), never overwrite a previous report,
and are finalized and synced before the API returns. Individual collected files
are capped at 4 MiB, collection at 32 MiB; omissions are explicit. Symlinks and
non-regular files are excluded. Reports remain until the user removes them.

## Logs and lifecycle

Defaults: `~/Library/Logs/Isocast` (macOS), `%LOCALAPPDATA%\Isocast\logs` (Windows),
or `$XDG_DATA_HOME/isocast/logs` (Linux, normally `~/.local/share/isocast/logs`). Setting `ISOCAST_SOCKET=/path/engine.sock`
uses `/path/engine.logs`, shared by clients of that isolated engine. Each process
launch has its own timestamp/pid/name prefix. JSON lines have UTC `time`,
`level`, `process`, `pid`, `launch`, `thread`, `component`, `message`, `fields`.
On Unix an independent native-stderr relay drains the pipe even after immediate
producer death, including a final line without newline. Its credential seed
channel is private and polled even when stderr is silent, with a 256 KiB
receive buffer. Backpressured seeds retry off the caller's thread; native
lines are withheld only between the pending/ready markers for that backlog.
Delivery recovery restores text logging automatically. A supervisor reports
unexpected relay exits as warning events and in text status, then respawns the
helper (one attempt per second on failure). During an outage seed messages are
dropped, not accumulated; the replacement is seeded from the bounded registry
before it can disclose native text. Native output emitted while the helper is
unavailable cannot be recovered. Relay restart/pending/drop counters are in
`status.diagnostics.native_relay`. Explicit credentials use the same scoped
rotation cap in the relay and producer. Native lines have `info` severity rather
than guessing severity from arbitrary text. Bootstrap stderr appends, so
pre-init failures survive the next launch. Camera diagnostic callbacks enqueue
bounded messages for a worker; they perform neither file writes nor pruning.
Plugin stdout/stderr is redacted before its 1 MiB current/previous logs are
written, and both logs can be included in the report.

The shared logger uses `serde_json` and whole-line `File` writes. Initialization
is infallible: failures switch to stderr, and status exposes the cause and
mode. Health history is kept beside the setup, independently of the log disk.
Mutations and errors are logged; successful polling is summarized per peer and
method per minute (counts, response bytes, mean/max duration). There is no
per-tick Levels logging and no duplicate request-received line. Media bytes
and diagnostic responses are omitted
to avoid copying video or recursively logging a report of the logs. Ordinary
lines reach the OS immediately (`File::flush` does not imply durable storage);
panic/exit markers use `sync_all`.
Disk logging failures are counted in `status.diagnostics.write_errors`.

Segments rotate at 4 MiB. Budgets are 40 MiB for the daemon and 8 MiB each
for CLI/native-app/web-app logs, seven days and twenty ordinary launches.
Background maintenance handles rotation requests and periodic pruning; camera
callbacks never prune directories. Active markers/current segments are kept.
The last three daemon fault launches (one per client family) retain their
marker, final segment and three bounded crash files regardless of size/launch
pruning, for up to seven days. Client traffic cannot evict daemon history.
Mutation/error log writes are capped per peer with explicit omission counts;
the engine operations themselves are not rate-limited.
The limit can temporarily exceed the budget by the current active segments.

`isocast logs --follow --level warn --component api` is human readable;
`--json` emits JSONL. It also works with the daemon stopped. The logs API's
cursor tracks offsets per file so concurrent daemon/app/CLI launches are not
missed just because another file has a later name.

An open, locked `.run` marker identifies a live process. Normal completion
appends a clean-exit marker; a stale unlocked marker without it is abnormal,
even after SIGKILL or machine reboot. The next daemon launch exposes this in
`status.diagnostics.previous_abnormal` and warns subscribers. A subsequent
clean daemon run acknowledges earlier abnormal launches; they remain retained
as evidence but no longer replay as fresh startup warnings. Failed concurrent
startup attempts do not acknowledge a real daemon run. A panic writes its thread, location/message and forced
backtrace to JSONL and a numbered `.crash.json`, syncs, and **unwinds without
aborting**. The hook uses non-blocking locks; if redaction is busy it withholds
the payload instead of leaking it or deadlocking. Connection, source, scene,
stream and recording workers contain failures; a contained panic becomes a
warning/failed worker rather than killing independent outputs. Core Audio's
monitor-only catch, source-open joins, engine-start and tune recovery remain
effective. A clean shutdown after a contained panic gets a clean marker.

A poisoned control/media state triggers an emergency stop: signal recording
stop, stop the stream, wait for writers/manifests, mark the take `interrupted`,
then exit 70 without invoking poisoned destructors. A ten-second watchdog
exits if shutdown hangs; that exceptional path can preserve only committed
fragments. Startup scans known recording folders under their exclusive lock,
marks abandoned `recording` manifests `interrupted`, reads media-duration
metadata from macOS, and exposes `status.interrupted` and the manifests in reports.
Recovery text states that recoverable duration is unknown: pause/resume
checkpoints are not proof of committed media. Both recovery paths annotate
paused takes; the incident sample includes `paused`. An open pause stays open
when the crash time is unknown. `recoverable_seconds_at_pause` stays null.
Unknown decoded-frame/drop counts remain null. SIGKILL/power loss cannot be
made lossless; no missing clean marker is claimed to identify its exact cause.
Triage identifies the last pre-exit/pre-panic sample separately from current
report-time state, retains compact incident context when other data is
shortened, and includes interrupted takes and durable logging-failure history.
Descriptions are blockquoted so an unclosed code fence cannot swallow triage.
Background daemons' native `.ips` reports can land in
`/Library/Logs/DiagnosticReports`, not the user's `~/Library/Logs/DiagnosticReports`;
both locations are collected when readable. macOS can write these tens of
seconds after the crash; generating another report later includes them.

For an isolated fault-injection run, set `ISOCAST_DEBUG_PANIC_AFTER=5` on the
daemon. Release builds honor it **only with an explicit `ISOCAST_SOCKET`**.
A named diagnostic worker panics and is contained five seconds after startup.
`ISOCAST_DEBUG_PANIC_MODE=connection` instead faults the next connection;
`=state` panics mid-change to the control state, marking it broken, to exercise
emergency finalization; `=state_read` panics while only reading it, which must
leave the recording and stream running.
`=native_exit` writes `ISOCAST_DEBUG_NATIVE_LINE` to native stderr and exits
immediately (71), testing relay durability; it does not invoke a Rust panic.
Unset the variables before restarting. Use a test Mac for fault injection.

## Developer mode

Developer mode (Settings › Advanced, `isocast debug on`, MCP
`developer_mode`; see docs/developer-mode.md) adds `level: "debug"`
records, every API call, and a frame timing `trace` record each second
while the engine runs, in each launch's `<launch>-developer-NNNN.jsonl`
segments. They have their own 200 MiB budget, so they never evict the
ordinary history, and pass through the same redaction. A report takes
only each launch's newest developer segment; `isocast logs` reads them
all. It turns itself off after 24 hours unless set otherwise.

## Metrics and availability

The macOS sampler writes one record per second while the engine runs (one per five seconds while idle):
daemon CPU as percent of **one core** (Mach rusage ticks converted using
`mach_timebase_info`), physical footprint/RSS, threads, open
files, process disk-write rate, recording volume/free space/filesystem,
thermal state/Low Power Mode/power source, engine compositor timings, per-file
video drops/audio dropouts, stream rates/congestion/errors, and H.264 encoder
in-flight/ready queues and last submission-to-callback time. The sampling
thread reports its previous cycle's CPU microseconds, including its own logging
and periodic device inventory. Cumulative logging CPU is measured separately;
those totals overlap for logs written by the sampling thread.

VT service measurements are explicitly **system-wide**, with PID/start identity;
libproc does not reliably attribute these XPC services to one client. Service
discovery is every five seconds. GPU time is the AGX driver's cumulative
`accumulatedGPUTime` for this PID, via IOKit without a subprocess; unavailable
counters are `null`. AVAssetWriter's private encoder queues are `null` with a
reason, not invented zeroes. No sudo,
powermetrics or synchronous GPU wait is added to the one-second loop.
The native relay is measured separately and in the daemon-total CPU field;
`scripts/bench.py` includes descendant helpers in CPU, instructions and RSS.

Permissions are checked every second; native camera/audio/display notifications
trigger an inventory comparison at the next one-second sample (including
notification counts when a transient change leaves the same final inventory).
A sixty-second fallback catches drivers' unnotified format changes. This never opens a capture or
enumerates windows through ScreenCaptureKit. Accessibility is the OS's
`AXIsProcessTrusted` for the daemon, not a claim about another app's grant.

## Privacy

Redaction happens before logging and again before preview/archive creation:
exact credential names and credential suffixes (`api_key`, `access_token`,
etc.), not substrings such as `hockey.clock`, `keyboard` or chroma-key enums.
Short/enum-like values are not learned from arbitrary fields. Environment,
Keychain and explicitly set stream keys keep the current and previous two
values per service/account; ordinary API traffic cannot evict them. Stream
keys are pinned only after a successful Keychain write, or after retrieval.
Failed writes only enter the ordinary bounded learned-value cache.
`status.diagnostics.credential_pinned_count` exposes the number of unique
pinned values without exposing credentials or account names.
The process also remembers the 512 most recently supplied API-learned values (least recently supplied
evicted first, counted in `status.diagnostics.credential_cache_evictions`);
API-learned values over 4 KiB are replaced in place and never remembered, so no request can
blind later logs or reports. An evicted API-learned value is still removed from any field
named as a credential; only its later echo in free text could survive. Each
report uses a fresh redactor seeded with the configured profile keys from the
Keychain (even before a stream starts) and `ISOCAST_STREAM_KEY`; no key
inventory is written or returned. Strict mode also pseudonymizes device
model strings.
Client logs omit arbitrary params/prose and server results, leaving the daemon
as the redaction authority.

`isocast frames` retains its partial capture and reports the ending reason,
but exits nonzero on a failed stream (including with `--json`). A compositor
failure makes both Start and Resume report `program output has failed`;
Resume leaves a paused take paused. Stop can still finalize its files.

All HTTP(S)/RTMP(S) capability URL paths, userinfo, queries and fragments are
removed by default. HOME becomes `~`; window titles and serial-number fields
are omitted. Only system/recording mounts are enumerated. Peer/capture processes
use executable basenames; signing identity names and TeamIdentifier are omitted.
Strict mode uses HMAC-SHA256 with a fresh random per-report key, not unsalted
hashes; the key is never exported, so low-entropy paths resist dictionary lookup.
Known device names/IDs are consistently pseudonymized within that report.

Default device names can contain an owner's name; strict mode hides them.
Text-source content and data-store values are retained by design unless they
look like credentials. No arbitrary environment dump, Keychain export, media,
screenshot or window list is collected. Review unlabelled prose before sharing.
