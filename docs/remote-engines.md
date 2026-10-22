# Remote engines: any Isocast app on any Isocast engine

Ryan, 2026-10-22: any client should be able to drive any server. Scope now is
**macOS ↔ Windows** (Mac app on a Windows engine, Windows app on a Mac
engine, and each on its own kind). Linux comes later, so nothing here may
assume only two operating systems. Tasks are in `PLAN.md` section R.

## Where we start (main 3e2ae8e4)

Already in place:

- **A separate engine.** `isocastd` captures, mixes, records and streams;
  the app, CLI and MCP are thin clients of one method table
  (`isocast-protocol::api::Method`, newline JSON-RPC).
- **Stable ids instead of names,** and device lists come from the engine.
- **`status.daemon`** carries the engine's version and build. Clients
  already fall back when an engine answers `METHOD_NOT_FOUND`.
- **An encoded preview stream** (`frames`, H.264 per monitor; built for the
  retired web app) and single JPEG stills (`frame`).
- **An obs-websocket server** on TCP with a password, so OBS tools can
  already drive an engine over the network.

Missing:

- **A network connection:** a Unix socket or named pipe only, no
  encryption, no identity, no discovery. The app launches its own engine
  (`connect_or_launch`).
- **Previews over the network:** previews use shared GPU surfaces
  (`shared_frames`), which only work on the same machine.
- **File access:** the app reads some files itself (waveform cache, setup
  import, file pickers through native dialogs, "Show in Finder").
- **Wording:** `platform_text::select` chooses Mac or Windows words by the
  *app's* OS at compile time.
- **Monitoring and hotkeys** only happen on the engine machine.

## The model

**An engine is a place; the app is a window onto it.** Every install
contains both an app and an engine. "This Mac" is just the nearest engine.
Everything the app shows comes from the engine it's connected to, and
nothing in the app assumes the engine is local.

The same connection model covers every case:

| | Same machine | Another machine |
|---|---|---|
| Control (method table, events) | local socket / pipe | encrypted network connection, same messages |
| Pictures | shared GPU surfaces (today) | hardware-encoded video, hardware-decoded |
| Sound | engine's monitoring device | Opus stream to the app's speakers, or the engine's device |
| Files | engine paths (happen to be local) | engine paths, browsed through the engine; nothing copied |

The app and CLI pick the picture and sound paths automatically: same
machine → zero copy; otherwise → encoded streams. Callers of
`isocast-client` don't choose.

## Decisions

Made by the delegator on 2026-10-22, then revised the same night after an
independent design review
(`/Volumes/IsocastBuild/delegator/scratch/remote-engines-REVIEW.md`) and
prior-art research (Sunshine/Moonlight, Syncthing, HomeKit, Matter, NDI,
rustls; `/Volumes/IsocastBuild/delegator/scratch/remote-research.md`).

Ryan confirmed on 2026-10-22: the standalone default and the two switches,
local network only, several clients with full control, control only,
the same version, pairing over SSH, remote settings only on the machine
itself, a window per engine, and agents allowed. He can overrule the rest.

### Product

1. **Standalone by default, like OBS.** Out of the box Isocast is one app
   with its own engine, and nothing about remote control shows anywhere.
   Settings → **Remote** has two independent switches, both off:
   - **"Control other computers":** adds the engine picker and lets you
     pair with and manage other engines.
   - **"Let other computers control this one":** starts the listener,
     advertises it and allows pairing windows.

   Turning the second switch off closes every remote connection at once
   (`RemoteDisabled` event).
2. **Local network only.**
   - This is a guard against mistakes. The security boundary is the pinned
     keys (decision 8).
   - An address counts as local when it's:
     - inside a prefix of one of the engine's own interfaces (which covers
       IPv6 global addresses on the same LAN);
     - RFC 1918, 169.254/16, fc00::/7 or fe80::/10;
     - 100.64/10, labelled "Tailscale" in the UI.

     IPv4-mapped IPv6 is unmapped first.
   - Pairing is accepted only from the same network or Tailscale.
   - Adding an engine by a public address, or allowing connections from
     one, shows "Not recommended over the internet (for now)" and needs
     confirming.
   - No internet relay.
3. **Every paired client gets full control of recording, at the same
   time.**
   - Several apps (and CLIs and agents) can drive one engine; the last
     writer wins.
   - `SetupChanged` and the recording and stream events carry the client's
     name, so the other apps can say who changed what.
   - Each app shows who else is connected.
   - "Full control" means the show, not the machine (decision 4).
4. **Machine control stays on the machine.** The engine refuses
   these from any network caller with `NOT_PERMITTED` and "change this on
   exon-desk":
   - `remote_*` changes (pair, unpair, revoke, enable, disable);
   - `websocket_enable`, `websocket_disable`;
   - `plugin_add`, `plugin_remove`, `plugin_set_enabled`;
   - `shutdown`, `shutdown_if_idle`;
   - `debug_set`, `bug_report` with `send`;
   - `machine_calibrate`;
   - `speed_test` with a server.

   The read-only `remote_status` and `remote_clients` are allowed. One list,
   `isocast_protocol::remote::local_only_reason(method)`, lives beside
   `plugin::ungrantable_reason`: the engine enforces it and the app hides
   the same controls.
5. **One window = one engine.**
   - With "Control other computers" on, the engine picker lives in the
     title area ("This Mac", "exon-desk", "Add engine…").
   - The window title always names the engine it controls.
   - Opening another engine opens a new window. The first window stays on
     this machine's engine, so both are visible side by side.
6. **Control only; everything lands on the engine.**
   - Recordings, projects and media stay on the engine's disk; paths in the
     API are always engine paths. Nothing is copied to the app's machine.
   - The app never reads engine files directly, even when local.
   - Browsing another engine's recordings and projects is a future
     **remote library**, not this plan.
7. **The app and engine must be the same version.**
   - "Same" means equal product `version` and source `commit`; OS
     packaging build numbers differ by platform and aren't compared. A dev
     build on either side downgrades a commit difference to a warning.
   - These parts must never change across versions, so any two builds can
     talk well enough to say what's wrong:
     - the TLS setup and ALPN `isocast/1`;
     - `hello`;
     - the pairing exchange;
     - the mismatch error.
   - Under a mismatch, a connection may call only `hello`, read-only
     `status` and `remote_update_request` (R6).

### Security

8. **Mutual TLS 1.3 over TCP with pinned raw public keys.**
   - **Library:** `rustls` (0.23.46 or later) with the `ring` provider.
     `native-tls` (already used for RTMPS) can't be a TLS 1.3 server on
     macOS or pin raw keys. `ring` builds on Windows MSVC without cmake or
     NASM.
   - **Keys:** each engine and each client has an Ed25519 key pair in
     Keychain or Credential Manager, used as an RFC 7250 raw public key
     (no X.509, no expiry, no clocks). The identity is the key's SHA-256,
     and the pin is the SubjectPublicKeyInfo (SPKI).
   - **Verifiers:** custom, and they check handshake signatures, not only
     pins.
   - **No session resumption:** resumed sessions skip the client-key
     check, so a revoked client could get back in.
   - **ALPN `isocast/1`,** so a wrong peer is refused in the handshake.
   - **What gets parsed before authentication:** a connection whose key
     isn't pinned is closed before any JSON is parsed. The one exception is
     an open pairing window (decision 9).
   - **QUIC isn't used.** Its whole state machine runs before
     authentication. 2026 brought two remote code execution bugs in MsQuic
     (CVE-2026-62815, CVE-2026-105016) and about eight pre-auth DoS
     advisories in `quinn` (three on October 12, RUSTSEC-2026-0340, -0341
     and -0343). What it offers matters on lossy internet links, not this
     LAN.
9. **Pairing: a one-time code with SPAKE2, opened only on the engine.**
   - **Opening the window:** only a local caller opens it (Settings →
     Remote → "Pair a computer", or `isocast remote pair --show` over SSH).
     An unsolicited connection never shows anything on the engine's
     screen.
   - **The code:** 6 digits from the OS random generator, valid 2 minutes.
     The window closes after one successful pairing. Each failed
     confirmation uses up the code; 3 failures in a window, or 20 in a day,
     close pairing for 60 s or until it's reopened locally. Only one window
     is open at a time.
   - **The exchange:**
     - The client connects with its own (new) raw key.
     - A connection with an unpinned key is accepted only while a window
       is open, and may call only `remote_pair_finish`.
     - SPAKE2 (RustCrypto `spake2`) runs with the identities engine id and
       client id. Explicit key-confirmation MACs cover the transcript,
       **both TLS raw public keys** and the TLS exporter, so a
       man-in-the-middle fails outright.
     - On success, each side pins the other's key.
   - **The crate:** `spake2` is unaudited and not RFC 9382, which is
     acceptable with the binding and confirmation above. R7's security
     review checks the wrapper.
   - **Never logged:** the code and the SPAKE2 material.
10. **Agents and scripts are clients like any other.**
    - The CLI or MCP makes its own key (`isocast remote identity`, kept in
      the client's credential store) and pairs once with the code, read
      over SSH if nobody is at the engine.
    - The engine stores only the public key, a name and when it was last
      seen. Revoking unpins the key.
    - There are no bearer tokens: one mechanism, and no secrets at rest on
      the engine.
11. **Discovery through the operating system.**
    - DNS-SD `_isocast._tcp` goes through the OS's own responder:
      Bonjour `DNSServiceRegister`/`DNSServiceBrowse` on macOS, the Windows
      `DnsServiceRegister`/`DnsServiceBrowse` APIs (no firewall rule for
      5353), Avahi later on Linux. The engine never parses multicast
      itself.
    - The TXT record holds only `id`, `proto` and `name`. The version and
      OS are given in `hello`, after authentication.
    - The macOS bundles declare `NSBonjourServices` (`_isocast._tcp`).
12. **Bounded before authentication.**
    - TLS handshakes run in a small pool (8 at a time, 2 s deadline).
    - A rate limit applies per source IP.
    - At most 16 connections per paired client and 64 in total.
    - Remote requests are capped at 1 MB per line; the local cap stays
      16 MB.
13. **Secrets stay on the engine.**
    - Clients can set stream keys and passwords but never read them.
    - When a network caller changes a destination's `server` or `service`,
      that destination's stored key is cleared and must be set again.
      Otherwise a client could point the stream at its own server and
      receive the key in the RTMP publish.
14. **The obs-websocket server stops being the weak spot.**
    - It binds to loopback by default; a LAN bind shows a warning.
    - Failed logins are limited.
    - It follows decision 4's policy, since it's a network caller too.
    - It remains a compatibility layer, not the native remote path.
15. **Every remote action is attributed.**
    - The audit `peer` is `{transport: "tls", client_id, name, addr}`.
    - Pairing, revocation, connecting, disconnecting and refused local-only
      calls are logged and published as events.

25. **Network callers act on the show, inside engine-approved folders.**
    Added 2026-10-22 after the policy review: method-level rules miss
    effects reached through allowed methods, so the engine checks the
    **effective operation** for every network caller (remote and
    obs-websocket).
    - **Folder roots move into R1:**
      - every caller-chosen path, read or write, must resolve inside an
        engine-side root list, symlinks included: recording and setup
        folders, replay, screenshots, exports, project open and relink,
        reports, text, image, media, slideshow and browser files, OBS
        import paths;
      - the roots are set only on the machine itself (Settings → Remote →
        "Folders other computers can use"). By default: Movies/Videos,
        Pictures, Music, Desktop, Downloads, the configured recording
        folders, and fonts (read-only);
      - changing `recording.folder` never widens the roots.
    - **Browser sources from a network caller:**
      - http(s) to the internet and the LAN is allowed, since that's what
        overlays are for;
      - loopback, link-local and `file://` outside the roots are refused,
        checked on the literal host and its resolution at apply time.
        DNS rebinding after that is an accepted residual risk.
    - **Setup changes that alter the machine are refused:**
      `developer.*`, and installing or updating the virtual camera
      extension. A remote caller may only feed an already-installed,
      compatible camera.
    - **Credentials are never sent to a server a network caller chose:**
      - `optimize` and `tune` from a network caller can't name a server
        and run only against configured destinations;
      - key clearing fails closed, covering `ISOCAST_STREAM_KEY` and legacy
        stored keys.
    - **Benchmarks and disk tests** use operation-owned temporary files
      inside the roots, never fixed names.
    - **Hotkeys fired remotely keep the caller's origin** all the way
      through the hotkey worker.
    - **`shared_frames`** (same-machine GPU surfaces) is refused for
      network callers.
    - **Refusals are published as events,** as well as audited.

26. **The threat model: hard boundaries and guard rails.** Added
    2026-10-22 after the policy re-review showed that full filesystem
    confinement would mean handle-relative I/O throughout the engine,
    browser-identical URL parsing and redirect control. That's a sandbox
    project, not R1.
    - **Paired clients are trusted operators of the show.** Pairing needs
      a code read on the engine machine, so pair only computers you trust.
      This matches Syncthing, AirPlay and obs-websocket.
    - **Hard boundaries (Tier 1, must be airtight):**
      1. who can connect: pairing, pinned mutual TLS, local-only pairing
         windows, accept limits (decisions 8, 9, 12);
      2. machine control stays local: decision 4's methods, developer
         settings (including through library switches), plugins, remote
         settings, virtual camera install or update, and
         `shared_frames`;
      3. credentials never reach a server a network caller chose:
         - key clearing;
         - imports that would bring a stored key into a moved
           destination are local-only;
         - key provisioning serialized with destination changes;
         - all decided before the live engine changes.
    - **Guard rails (best effort, documented, not a sandbox):**
      - **Folder roots:** checked on explicit and derived paths; recording
        history never becomes a root.
      - **The browser rule:** checked on the WHATWG-parsed host.
      - **Accepted residuals:** symlinks swapped after a check, browser
        redirects, DNS rebinding, and references nested inside
        project or import files. Also: a browser URL naming the engine
        machine's own LAN address (only loopback and link-local are
        refused), and setup files (images, text, media) are opened later
        by the path given, not the one checked (request paths are handed
        on resolved).
      - Network callers can't give UNC paths (`\\server\share`):
        resolving one would connect to that server.
    - **The obs-websocket server** gets the same treatment, so local OBS
      tools follow network-caller rules (a documented behaviour change).

### Recording safety

16. **A recording never depends on a client.** Losing the network, closing
    the app or sleeping the client never changes what the engine records or
    streams.
17. **Recording sessions have ids.**
    - `Started`, `Status` and the recording events carry `session`.
    - `stop { session }` and `request_stop { session }` answer `WRONG_STATE`
      when that session isn't the one running, so a retried Stop can never
      end a recording someone else started in the meantime.
    - `start` with identical parameters while recording returns the
      current session with `already_recording: true`.
    - Request ids are kept only for audit correlation; there's no replay
      cache.
18. **Dead links are noticed in seconds.**
    - Both ends use TCP keepalive (idle 10 s, 3 probes) and `TCP_NODELAY`.
    - The client's control calls have a read timeout: 30 s, longer for
      start and stop.
    - `watch` sends a heartbeat every 2 s.
    - After a pull, the app shows "Reconnecting…" within 10 s.
    - On reconnect the client re-`watch`es and gets a fresh snapshot. Event
      sequence numbers only detect gaps; there's no replay.

### Architecture

19. **One stream type, local or remote.**
    - `isocast_ipc::Stream` becomes an enum, `Local(..)` or `Tls(Duplex)`,
      on every platform. It supports `read`, `write`, `try_clone`,
      `shutdown`, read and write timeouts, `hung_up` and `peer()`.
    - `Tls` is one driver thread per connection that owns the rustls
      connection and socket and passes plaintext through bounded pipes, so
      the reader and writer halves work independently. A write timeout is a
      bounded queue plus a deadline, and `hung_up` reads TLS state, not
      raw-socket peeks.
    - The daemon and client keep their blocking-thread style, with no async
      runtime.
20. **`hello` is an ordinary method.**
    - `isocast-client` calls it automatically after a remote connect and
      caches the answer: `os`, `engine_id`, `name`, `DaemonBuild`,
      `protocol`, `capabilities`.
    - Local connections may call it but don't have to.
21. **The app holds an `Engine` handle.**
    - A window owns `Engine::Local` or `Engine::Remote { id }`. Every
      static `Client::connect*`, `stale_engine`, `log_path` and
      `running_daemon` path takes it.
    - These run only for `Engine::Local`:
      - stale-engine restart;
      - "Restart Engine";
      - `shutdown_if_idle`;
      - both updater paths;
      - `connect_or_launch`'s launch.
    - CLI commands that are local by nature (`restart`, `logs`, `setup
      check <file>`, isolated `record`) fail clearly with `--engine`
      instead of acting on the wrong machine.
22. **Wording follows the machine the words are about.**
    - Engine words (Keychain/Credential Manager, Finder/Explorer,
      permissions, recording folders, "this Mac/PC") come from `hello.os`.
    - App words (menus, shortcuts) come from the app's OS.
    - `platform_text` takes an OS value instead of using `cfg!`, with
      Linux as a third value.
23. **Previews: share encoders, cap them, and skip frames before
    encoding.**
    - Preview sizes snap to a short ladder (e.g. 640, 960, 1280, 1920
      wide), so viewers share encoders.
    - At most 3 preview encoders per engine, and a fair share per client.
    - The recorder's encoders start first, and the governor evicts previews
      when it needs a session (#128's rule).
    - Each frame depends on the previous one, so frames are skipped
      **before encoding**, never after: the encoder is paced by viewer
      acks, with a small socket queue (`TCP_NOTSENT_LOWAT`, small
      `SO_SNDBUF`, 1 s write timeout).
    - Keyframes are forced only to recover a lagging viewer, rate-limited
      to one per second per encoder.
    - A careful pipeline reaches 20–50 ms on a wired LAN; the target is
      under 100 ms.
24. **Projectors belong to the app that opened them.** `projector_open`
    records the client, each app shows only its own, and a projector whose
    owner disconnects closes. "Full-screen on the engine's display" is
    owned by the engine.

## The work

Each milestone is done only when its "done when" passes on real machines:
basebook (Mac engine) ↔ exon-desk (Windows engine), with exonbook as an
SSH-run Mac client. R1 adds a loopback mode, so most work can also be
checked on one machine and in CI.

### R1. Foundation: the CLI drives an engine on another machine

Tier 1.

- **`isocast-remote` crate** (portable, blocking threads):
  - rustls mutual TLS with raw public keys, ALPN, no resumption;
  - keys in the platform credential store;
  - the listener with handshake limits (decision 12) and the duplex driver
    (decision 19);
  - keepalive and timeouts (decision 18).
- **`isocast_ipc::Stream` as an enum,** with the daemon and client moved
  onto it: the local paths behave exactly as before, and the hang-up check
  and timeouts work for both.
- **Engine identity and `hello`** (decision 20). The version rule and the
  frozen parts (decision 7).
- **Pairing** (decision 9) and client identities (decision 10).
- **Methods:**
  - `remote_status`, `remote_enable`, `remote_disable`;
  - `remote_pair_begin`, `remote_pair_finish`;
  - `remote_clients`, `remote_revoke`.

  CLI: `isocast remote …` and `isocast engines`.
- **Caller-origin policy** (decision 4): a `Caller` origin in the daemon's
  `handle`, with the obs-websocket server included.
- **Discovery through the OS** (decision 11).
- **Engine selection:** `isocast --engine <name|id|host[:port]>` and
  `ISOCAST_ENGINE` for the CLI and MCP. A loopback mode
  (`ISOCAST_ENGINE=tls://127.0.0.1:47470` with a pre-paired dev identity)
  for single-machine checks and CI.
- **Recording session ids** (decision 17), stream-key clearing (decision 13)
  and attribution (decision 15).
- **Default port TCP 47470.** A second user's engine on the same machine
  picks the next free port, which DNS-SD advertises.
- **Firewalls:**
  - **Windows:** the engine lives at a stable installed path. When it's
    listening but nothing ever connects, "Allow on network…" asks for
    elevation once to add a Private-profile rule (a refused first-listen
    prompt silently creates block rules).
  - **macOS:** accepting connections needs no Local Network permission,
    but advertising does. The engine runs from the app bundle so the
    permission belongs to Isocast.

**Done when:**
- the Mac CLI pairs with exon-desk and runs `status`, `devices` and a 30 s
  recording with start and stop, and the Windows CLI does the same against
  basebook;
- pulling the network mid-recording leaves the file whole, the client says
  "Reconnecting…" within 10 s and then shows the true state;
- a revoked client is cut off at once and can't reconnect;
- a local-only method from a remote caller is refused;
- the loopback mode passes in the gates.

### R2. The app on a remote engine (control, no live pictures yet)

- **Settings → Remote:** the two switches, "Pair a computer" (shows the
  code), the paired engines and clients, revoking, and the internet warning.
  These controls are hidden from remote windows (decision 4).
- **Engine picker and pairing:**
  - discovered engines, code entry, "Forget engine";
  - "This engine's identity changed: pair again" when a reinstalled engine
    presents a new key.
- **The `Engine` handle** through the app (decision 21).
- **Connection states:** Connecting…, Reconnecting… (controls disabled,
  state kept), Engine unavailable, Version mismatch (with the update
  action).
- **Presence:** who's connected, and toasts naming who started or stopped a
  recording.
- **Wording by the engine's OS** (decision 22).
- **Permissions are the engine machine's:** the app shows their status and
  "Grant on exon-desk", and the engine machine shows its own prompt.
- **App-side file reads move to methods:** waveforms (`waves.rs`),
  thumbnails, setup import and export.
- **Engine file pickers** (`files_roots`, `files_list`):
  - the roots are decision 25's list (enforced since R1);
  - every path is checked to stay inside a root after resolving symlinks;
  - hidden entries are skipped;
  - listing only, never file contents;
  - "Show in Finder" becomes "Show on exon-desk".
- **Projector ownership** (decision 24).
- **Live program video (required, Ryan 2026-10-22):** a remote window shows
  the program moving, even at low quality.
  - The engine's existing low-latency H.264 monitor stream (`frames`) runs
    through each platform's hardware encoder, at about 960 px wide and
    30 fps.
  - It's hardware-decoded in the app (VideoToolbox on macOS, Media
    Foundation on Windows) into the preview.
  - JPEG stills are only the fallback when decoding isn't available.
  - R3 adds the rest: every monitor, adaptive bitrate, under 100 ms.

**Done when** the Mac app fully controls the Windows engine and vice versa:
scenes, sources, settings, recording, streaming, library and profiles,
**with the program playing as live video** in the remote window. The engine
is named in the window, every engine-side word matches its OS, and two
apps on one engine see each other and don't duplicate projectors.

### R3. Pictures

- **Encoded previews** per decision 23:
  - VideoToolbox and NVENC/Media Foundation low-latency settings: no
    B-frames, intra refresh where supported, viewer-paced;
  - viewer acks and keyframe recovery.
- **Hardware decode in the app:** VideoToolbox → Metal, Media Foundation →
  D3D11, into the preview renderer's textures. `isocast-client` gets one
  `Pictures::open(target, size, fps)` that returns textures, local or
  remote.
- **One bandwidth budget across a window's monitors.**
- **Projectors on the app's machine;** full-screen on the engine's display
  stays owned by the engine.

**Done when** each OS's app shows program, preview and source previews of
the other's engine at 60 fps on the wired LAN in under 100 ms, measured,
and the engine's recording loses nothing while they play.

### R4. Sound

- **"Listen on":** "This computer (the app's)" alongside the engine's
  output devices.
- **The monitor mix as Opus:**
  - low-delay mode, 10 ms frames, 128–256 kb/s, on its own connection;
  - a jitter buffer with clock-drift resampling;
  - delayed to the picture by the engine's clock.
- Meters stay as events.

**Done when** monitoring a source on the Windows engine plays on the Mac in
sync with its preview for 30 minutes without the buffer draining or
growing, and switching back to the engine's device works.

### R5. Input

- Hotkeys already have an app-input path (`HotkeysInput`), so this is
  mostly scoping each binding: "On the engine machine" (global, today's) or
  "In the app" (sent as input).
- The obs-websocket server keeps serving Stream Deck and similar tools
  (loopback by default, decision 14).

**Done when** a hotkey on the Mac switches scenes on the Windows engine
while the engine machine's own hotkeys keep working.

### R6. Host mode

- **"Keep the engine running for other computers":** the engine starts at
  login and runs without the app.
  - **macOS:** `SMAppService`, so the Local Network permission belongs to
    Isocast.
  - **Windows:** a per-user startup entry.
- **Updates on a version mismatch:**
  - `remote_update_request` asks the engine to run its own updater against
    its signed release channel;
  - the client never supplies a binary or URL;
  - the engine shows a local confirmation (toast or CLI) before
    installing;
  - the app updates itself when it's the older side.

**Done when** exon-desk boots, logs in and is reachable from the Mac app
with no app open there, and a version mismatch is resolved from the Mac.

### R7. Hardening

Tier 1.

- **Independent security review:**
  - pairing (the SPAKE2 wrapper, binding, limits, code lifetime);
  - verifiers and resumption;
  - the caller policy;
  - accept limits;
  - the file roots;
  - stream-key handling;
  - obs-websocket;
  - logs and audits;
  - the bug classes behind Sunshine's and Matter's 2025–2026 advisories
    (the list is in the research report).
- **Fuzzing** of the pre-auth surface: pairing messages and the first
  request line.
- **Impairment runs:** latency, loss and pulled links during recording,
  start and stop, verified with ffprobe and the frame oracle.
- **User docs** (`docs/remote.md`); CLI and MCP references regenerated.

**Done when** the review passes, the impairment matrix loses no recorded
frame, and fuzzing finds nothing.

## Later, outside this plan

- **Remote library:** browse, play and open another engine's recordings and
  projects (Ryan, 2026-10-22: "in the future").
- **Internet use** (a relay, or QUIC once implementations settle), after
  local-network control is solid.
- **A view-only role:** the caller policy already has the hook.

## Linux, later (nothing above may block it)

- `isocast-remote`, the protocol, pairing, file pickers and the picture and
  sound formats are portable; `os` is an enum with `linux`.
- **The Linux app** needs VAAPI or software decode behind the same decoder
  trait, a file dialog and packaging; the Slint app is otherwise portable.
- **The Linux engine** needs the platform backend: PipeWire screen capture
  through the portal, V4L2 cameras, PipeWire audio, VAAPI/NVENC encoding,
  and Avahi for discovery.

## Verification hosts

- **Mac ↔ Windows:**
  - basebook (Ryan's Mac, 10.0.10.26) is the Mac engine. It runs the signed
    dev engine, which already has LAN access; heavy builds run elsewhere.
  - exon-desk (10.0.10.149) is the Windows engine. TCP 47470 is open on
    any profile, from LocalSubnet and Tailscale (100.64/10).
  - exonbook (10.0.10.165) is the SSH-run Mac client only; its firewall
    would prompt for an engine.
- **Same-OS pairs and CI:** the loopback mode; exon-desk ↔ the
  `isocast-pkg` VM.
- **Real runs are the default.** Any new harness or test files go to
  "Questions for Ryan" first (AGENTS.md).
