# Virtual camera (G.1)

`isocast vcam start|stop|status [--json]` controls **Isocast Virtual Camera**.
The method table and MCP use `virtual_camera_start`, `virtual_camera_stop`,
and `virtual_camera_status`. `status.virtual_camera` and `virtual_camera`
events report state, format, frame counts and failures. The release app
carries the camera extension and activates it on the first Start (below); a
development engine never installs anything (use `scripts/vcam-install.sh`).
Real activation and camera-client qualification wait on Ryan's Developer ID
provisioning profiles; see `docs/notes.md`.

## In the release app (#67)

`Isocast.app/Contents/Helpers/Isocast Engine.app` (bundle id
`dev.isocast.isocastd`) embeds `Contents/Library/SystemExtensions/
dev.isocast.isocastd.camera.systemextension`. Both are signed with Developer
ID and carry Developer ID provisioning profiles; the engine's grants
`com.apple.developer.system-extension.install`. The app group is
`9G68SMNHEU.dev.isocast.isocastd` (Team-ID prefixed: no registration for
Developer ID) and the Mach service `9G68SMNHEU.dev.isocast.isocastd.camera`.
`scripts/release.sh` builds, embeds, signs and checks them
(docs/releasing.md step 4a).

When a Start finds no Isocast camera and the engine carries the extension,
the engine asks macOS to activate it (`<engine> --install-camera-extension
--wait-for-approval`, a child with its own run loop):

- **Already allowed** (for example after a reinstall): Start retries until
  CoreMediaIO publishes the device, up to 5 s, and runs.
- **First time:** macOS asks the user once. Start fails with "allow Isocast
  Virtual Camera in System Settings › General › Login Items & Extensions ›
  Camera Extensions"; the app's popover says the same, with an Open System
  Settings button. The request stays alive for 15 minutes; when the user
  allows it the engine starts the camera itself, unless Stop was pressed.
  A later Start while it waits doesn't ask again. macOS's alert says
  **"Isocast Engine" would like to use a new camera extension**: it names
  the requesting process's executable, not its bundle (#69), so since #83
  the engine's executable is `Contents/MacOS/Isocast Engine` (it was
  `isocastd`, and the alert said "isocastd"; checked on isocast-m2-b,
  2026-10-16). The password sheets that follow use the bundle name
  ("Isocast Engine is trying to modify a System Extension").
- **Not in /Applications** (run from the DMG): Start says to move Isocast
  into Applications. **Refused** by macOS: Start reports macOS's reason.

- **After an update** (#69): Sparkle replaces the app and the extension it
  carries, but macOS keeps running the build it activated. The first Start
  of each installed build asks macOS which build is active
  (`<engine> --camera-extension-state`, one JSON line); when it's another
  build, Start asks macOS to activate this one, which replaces it without
  asking the user again, then starts. Engine logs say
  `camera extension build check: Current` (or the replacement).

### Remove Virtual Camera

Moving Isocast.app to the Trash does **not** remove the camera (checked on
isocast-m2-b: it stays activated and listed). **Remove Virtual Camera…** in
the camera's popover (the gear beside Virtual Camera), `isocast vcam remove
[--json]`, MCP `virtual_camera_remove` and the API's `virtual_camera_remove`
stop the camera and ask macOS to deactivate the extension; macOS asks for an
administrator password and the call waits for it (about 4 minutes at most;
the app makes it on its own connection). It returns
`{restart_needed, message}`; a later Start installs the camera again. A
development engine refuses (use `scripts/vcam-uninstall.sh`). Removal stops
Start until macOS answers.

macOS only deactivates the build the requesting app carries ("not found"
otherwise). When an older build is still active (an update without a Start
since), removal first asks macOS to activate this app's build, which needs no
approval, and then deactivates it. The engine's own maintenance entry point
does the same from a terminal:

```sh
"/Applications/Isocast.app/Contents/Helpers/Isocast Engine.app/Contents/MacOS/Isocast Engine" --uninstall-camera-extension
```

To delete Isocast: Remove Virtual Camera, then move the app to the Trash
(docs/install.md, "Delete Isocast").

Checked on isocast-m2-b (macOS 26.7, notarized Developer ID build,
2026-10-15): Finder's Move to Trash leaves the camera listed (showing the
waiting card) until deactivated. Deactivation asks for an administrator
password.

## Picture and lifecycle

The live setup accepts:

```toml
[virtual_camera]
target = "program" # program | preview | scene | source
# scene = "interview" # required only for target = "scene"
# source = "facecam"  # required only for target = "source"
# width = 1920       # give both dimensions, or neither for the canvas
# height = 1080
```

These settings belong to the **scene collection**, preserving the meaning
of source/scene ids. The canvas supplies the frame rate (1–240 integer fps).
The extension advertises 1920×1080, 1280×720, 2560×1440, 3840×2160,
1080×1920 and 720×1280. For other canvases, configure an advertised output
size; mismatched aspect ratios are letterboxed. Invalid settings name the
field and valid choices. Preview requires a staged scene.

Stop the camera before changing its settings/canvas or switching collections
or profiles; scene contents, source properties, program cuts/fades and preview
selection still update live. Recording and streaming attach independently.
A camera-only source opens at Start and closes at Stop, with no capture work
while the camera is off. Camera backpressure never blocks a recording route.
Deleting its selected scene/video source while stopped falls back to program,
shown in the plan's diff and notes. Explicit invalid selections still fail.
Stopped status reports the next output's size and fps. MCP camera tools take
**no arguments** and reject unknown ones; configure through plan/apply first.

The output worker samples the existing program/preview frames, or draws a
named scene with the same compiled compositor. One GPU pass scales/converts
to an IOSurface-backed BGRA pool (bounded at 32 outstanding buffers).
An authenticated private child owns all native CoreMediaIO sink calls. The
worker transfers IOSurface references over Mach; CoreMediaIO's bounded sink
queue (4 samples) transfers them onward without a CPU image copy. CoreVideo
keeps surfaces out of the pool while downstream processes hold them. There
is no raw shared file or globally discoverable IOSurface-id transport.

Native open has a **2 s** deadline, frames **250 ms**, and close **500 ms**.
Timeout kills/reaps only the helper; its watchdog also detects parent death.
No daemon state lock is held during setup, native calls or worker joins. The
worker's own start/stop waits are bounded at 3 s; an unfinished driver worker
blocks further camera workers rather than accumulating abandoned threads.
The helper keeps a main run loop for framework callbacks. A full sink queue
fails after a second; the per-frame DeviceIsAlive IPC has been removed.

The control socket is created **0600** and Darwin peers must match the daemon's
effective uid. The anonymous helper socket verifies its kernel peer audit
token against the daemon's designated code requirement. The extension refuses
a producer whose CMIO signing id is another app's (CMIO reports the engine's
as "unknown"). It checks the live client against Apple anchor + identifier
**dev.isocast.isocastd** + team **9G68SMNHEU** when macOS allows the lookup;
its sandbox denies that by pid on macOS 26.7, so it then requires the
producer's executable to be at one of the engine's exact paths: the release
app's `/Applications/<app>.app/Contents/Helpers/Isocast Engine.app/Contents/MacOS/Isocast Engine`
or the development host's `/Applications/Isocast Camera Host.app/Contents/MacOS/isocastd`
(weaker than the team check). The release engine's name before #83,
`…/MacOS/isocastd` in the same helper, is also accepted, so an older
engine works until its build replaces the extension. Refusals are logged:
`log stream --predicate 'subsystem == "dev.isocast.isocastd.camera"'`.
Reservations are keyed by CMIO client id so a second authorization cannot
overwrite the first.

**Why not a nonce (#69).** The suggested stronger check was a secret the
engine writes into the Team-ID app group container, presented to the
extension as a custom stream property. It can't work: the extension runs as
**`_cmiodalassistants`** (checked on isocast-m2-b: `ps` shows
`_cmiodalassistants … IsocastCamera`), so its app group container is under
`/var/db/cmiodalassistants`, not the user's `~/Library/Group Containers`,
and the engine (the user) can't write there. Any secret the engine can store
for itself (a file, its Keychain) is readable by another process of the same
user or invisible to the extension's user, and CMIOExtensionClient (macOS 27
SDK) still gives only a client id, a signing id and a pid: no audit token to
verify the caller's code. So the producer check stays the path pin. What it
leaves open: a process running as the same user that installs its own app
at `/Applications/<name>.app/Contents/Helpers/Isocast Engine.app/Contents/MacOS/Isocast Engine`
(writable without a password for an admin user) can feed the camera. Such a
process can already do far more with the user's account; the camera shows
only what a producer sends and never reads anything back.

Samples have host-clock PTS, frame duration, Rec.709 primaries/transfer/matrix
and full-range RGB pixels. Program PTS is the engine's canvas timestamp,
approximately one frame before it is drawn. `enqueue_latency_us` measures
PTS-to-sink enqueue, **not** program-ready-to-camera-client latency. The
diagnostic transport includes its reader acknowledgement in this metric.

When no producer is sending, camera clients receive an immutable Isocast
waiting card. Stop/disconnect resets freshness immediately; a hung producer
expires after max(500 ms, three frame durations). Starting the camera in a
different format is allowed while clients show the card. Valid consumer
size/rate requests (including 30 and 29.97 fps) are accepted before and during
production. **The producer's format wins while connected**, reported/notified
as the effective size/rate; clients can scale/thin frames. The consumer's
preference resumes for the card after Stop. Bad requests get format/rate errors,
not installation advice.

Empty sink reads wait a frame period instead of polling every 2 ms. The first
live segment after a card is retimed without pixel copies, preserving monotonic
PTS rather than dropping its first pictures. Original program PTS remains in
the `dev.isocast.program-pts-ns` attachment. Actual camera-app compatibility
and consumer-held surface lifetime still need installed qualification.

## Install on your Mac

From this worktree, in your terminal:

```sh
scripts/vcam-install.sh
```

This uses Xcode's **automatic Apple Development signing**, team **9G68SMNHEU**,
the same provisioning approach as Studio NDI. XcodeGen and shellcheck are
already available on Ryan's Mac; the installer requires XcodeGen, Xcode's
command-line tools, Python 3, Cargo, an Xcode-signed-in Apple account and the
team's signing access. Default Cargo output is
`/Volumes/IsocastBuild/g1-vcam/target`; packaging is under
`/Volumes/IsocastBuild/g1-vcam/scratch/vcam-installer`. Keep the build volume
mounted with at least 20 GiB free. `CARGO_TARGET_DIR`, `ISOCAST_VCAM_BUILD` or
`--build-dir /absolute/path` can choose other build locations.

1. **Confirm the terminal's three system steps.** It shows the commands and
   asks before Xcode registers the two Isocast ids/creates profiles, before
   copying the verified host to `/Applications/Isocast Camera Host.app`, and
   before submitting activation. Copying uses sudo; Xcode may request
   Keychain/account access. Noninteractive execution cannot approve these.
2. **Click the macOS approval.** Enable **Isocast Virtual Camera** in System
   Settings → General → Login Items & Extensions → Camera Extensions. The
   activation command exits **2** when approval is pending; sysextd continues
   after approval. Complete approval or the requested reboot, then run
   `scripts/vcam-install.sh --check`. No callback for 30 s also exits 2.
3. **Read the check result.** The script lists system extensions, discovers
   only Isocast's device through AVFoundation, starts a private synthetic
   test-pattern engine, calls `isocast vcam start`, and captures four seconds
   through AVFoundation. Success requires `passed=true`, at least 30 received
   frames, changing pixels, monotonic PTS and 1920×1080 dimensions. It prints
   actual fps and stops its own test engine. The first AVFoundation check may
   also need ordinary camera-access permission; its error gives the next step.

Your live setup, recordings, plugins and default daemon/socket are not used by
this check. Normal use requires a G.1-capable Isocast engine/CLI and `isocast
vcam start`; the installer does not replace an already-running production
daemon. Select **Isocast Virtual Camera** in the camera app. With no producer
it shows the waiting card. Rerun just the installed-device check at any time:

```sh
scripts/vcam-install.sh --check
```

Preview every command/confirmation without running external commands or
creating files (including with spaces in the build path):

```sh
scripts/vcam-install.sh --dry-run
```

### Signing and collision checks

`camera-extension/project.yml` generates an app and system-extension target
in scratch. The host's build phase copies the Cargo-built **real isocastd** as
its main executable before Xcode's final code-sign step; there is no stub
launcher. `xcodebuild -allowProvisioningUpdates` is used **only after the first
confirmation**. It creates profiles for the exact bundle identifiers and
embeds them in both bundles. An existing signed Studio NDI profile cannot be
reused for Isocast.

We retain **`dev.isocast.isocastd`** and **`dev.isocast.isocastd.camera`** so the
engine's identity, activation code and extension handoff remain consistent.
They do not collide with `com.ryanvogel.studiondi` or `.camera`. The app group
is `9G68SMNHEU.dev.isocast.isocastd`; the extension's Mach service is
`9G68SMNHEU.dev.isocast.isocastd.camera`. Resolved Xcode ids/team/groups are
checked before any account operation. Copy/removal refuse a different bundle
id, symlink, unrelated embedded extension or running host at the destination.
Older host bundles lacking the maintenance-version marker are refused for
checks/uninstall, so an unrecognized flag cannot start their default daemon.

Before copying, and again after copying, verification checks strict/deep code
signatures, the Apple team, signed entitlements, and both embedded profiles'
team, expiry, **exact app id** and app-group grant. The host profile must grant
`com.apple.developer.system-extension.install`. Profile/certificate contents
are decoded in memory, never dumped. Developer ID builds additionally require
Gatekeeper acceptance; local Apple Development signing is the default and is
not a notarized distribution build. Account provisioning and real installed
camera checks have **not** been executed by this agent; they require Ryan.

Host entitlements include system-extension.install, application-groups,
**device.camera**, **device.audio-input**, allow-jit and
**allow-unsigned-executable-memory**. Wasmtime 49 uses ordinary mmap followed
by mprotect(RX), so allow-jit alone is insufficient. Camera, microphone, screen
and system-audio usage descriptions are included. Screen capture remains TCC
controlled; no invented screen entitlement or disabled library validation is
needed. The installer verifies these capture/runtime fields.
Extension entitlements: app-sandbox and the
same application group. The extension is embedded in
`Contents/Library/SystemExtensions/dev.isocast.isocastd.camera.systemextension`.
The earlier manual bare-codesign procedure is superseded: `sign-dev.sh` now
refuses real-identity camera embedding and directs you to this installer.
Its camera option also requires an explicit scratch app and refuses the daily
dev app. Host and extension receive the same fresh monotonic timestamp
**CFBundleVersion** on each packaging build; template `1` is not installed.
Scratch extension bundles are rebuilt cleanly. The installer replaces the
whole verified host with staging/rollback, avoiding stale files from merging.

### Remove it

```sh
scripts/vcam-uninstall.sh
```

Close camera clients and stop the Camera Host first. The script validates the
exact host, asks before deactivation, lists the extension state, then asks
before removing only `/Applications/Isocast Camera Host.app`. A reboot-pending
deactivation keeps the host and exits 2; rerun after restarting. Preview with
`scripts/vcam-uninstall.sh --dry-run`.

Profiles, Apple-account identifiers/app groups, recordings and setups are
retained. If no longer needed, manage only the two Isocast ids/profiles in Xcode
or your Developer account. Studio NDI's app, extension and profiles are never
targets of these scripts.

## Build without installing

```sh
scripts/vcam-install.sh --unsigned
```

This runs Cargo, project generation, resolved-setting checks and Xcode with
`CODE_SIGNING_ALLOWED=NO`, then ad-hoc signs and verifies the scratch bundles.
It never uses an Apple signing identity, `-allowProvisioningUpdates`, copies
to /Applications or requests activation. Xcode's ordinary build may register
the **scratch product** with Launch Services/execution policy; that is not
camera-extension activation. The resulting ad-hoc app has no provisioning
profiles and is explicitly rejected by the normal installation verifier.
The Swift extension is arm64/macOS 14+; no new Cargo dependencies are needed.

## Non-installing verification

The brief-authorized `vcam-probe` reader is an explicit diagnostic, not a fake
registered camera. In an isolated test environment:

```sh
cargo build --release -p isocast-daemon -p isocast-platform-macos \
  --features isocast-daemon/vcam-diagnostics --bin isocastd --example vcam-probe
# Terminal 1 (choose a fresh socket path):
"$CARGO_TARGET_DIR/release/examples/vcam-probe" "$ISOCAST_SCRATCH/camera-probe.sock" \
  "$ISOCAST_SCRATCH/camera-frame.ppm"
# Terminal 2: export this before starting the isolated daemon:
export ISOCAST_VCAM_PROBE="$ISOCAST_SCRATCH/camera-probe.sock"
# Start the daemon with ISOCAST_SOCKET, ISOCAST_SETUP, ISOCAST_DEV_APP isolated,
# apply a setup, then run isocast vcam start / status / stop --json.
```

This runs the production output worker/native sample construction and validates
IOSurface backing, actual sample colour tags, PTS and duration. A separate
process imports the surface through Mach rights, checks dimensions, monotonic
PTS and sampled pixels, optionally saves a PPM, then releases it before
acknowledging. A stalled reader times out and fails only the camera output.
Status explicitly says `diagnostic-iosurface`. It does **not** verify CMIO's
queue ownership, extension sandbox, activation or downstream camera apps.

The `vcam-diagnostics` feature is **off in normal release/installer builds**.
Those builds reject diagnostic environment overrides before a transport opens.
Diagnostic builds also support `ISOCAST_VCAM_CHILD_TEST=pass|open|send|close`
to exercise the real authenticated child transport/deadlines without a camera
installation; their status says `diagnostic-native-child`.

A SIGKILLed reader leaves its socket. Prefer a fresh private directory/path.
To reuse a path, first confirm the owning reader is gone, then remove **only
your known stale socket**:

```sh
rm -f "${PROBE_SOCKET:?set this to your own stale probe socket}"
```

The reader intentionally refuses to unlink an existing socket at startup.
`Connection refused` for ISOCAST_VCAM_PROBE means a missing/stale reader, not
that the system extension needs reinstalling.

References: Apple's [camera extension guide](https://developer.apple.com/documentation/coremediaio/creating-a-camera-extension-with-core-media-i-o),
[SystemExtensions](https://developer.apple.com/documentation/systemextensions),
[installation entitlement](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.developer.system-extension.install),
and the installed Xcode SDK's CMIOExtension/CMIOHardware/CoreVideo headers.
