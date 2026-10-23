# The engine is the server

Decision, 2026-10-22. **Desktop first; browsers later.** `isocastd` owns
capture, composition, recording and streaming. The desktop app is a client,
not a server with a UI attached. This extends, rather than replaces,
[remote engines decisions 1–26](remote-engines.md).

## Contract and transport

- **Control:** the existing JSON-RPC 2.0 method table and events on the
  owner-only local socket/pipe or R1's pinned mutual TLS 1.3 connection.
  OpenRPC is generated from that table and its JSON schemas by
  `isocast api spec --json`; `docs/openrpc.json` is checked by the existing
  reference-docs CI gate. No HTTP/WebSocket listener or second authentication
  system is introduced. obs-websocket remains the OBS compatibility path.
  R1's current wire profile is one object per line with an unsigned integer
  request id; no request batches or client-originated notifications. OpenRPC
  publishes the method contract; it does not change those framing limits.
- **Picture and sound:** WebRTC, with WHEP's offer/answer and session
  lifetime semantics carried by authenticated JSON-RPC. This is **not an
  HTTP WHEP endpoint** and generic HTTP WHEP players cannot connect directly.
  RFC 9725 is **WHIP (ingestion)**, not WHEP. The current egress document is
  [draft-ietf-wish-whep-05](https://datatracker.ietf.org/doc/draft-ietf-wish-whep/),
  still a work in progress as of 2026-10-22.
- **Discovery and identity:** keep OS DNS-SD `_isocast._tcp` and R1 pairing.
  No new public port is advertised. No bearer tokens, certificates or
  HTTPS deployment work in this slice.
- **Local desktop:** socket/pipe plus `shared_frames` stays zero-copy.
  The remote desktop can use existing `frames` H.264 immediately; the media
  contract below is the planned replacement once its verification passes.

## Desktop media API (native loopback verified; LAN/UI qualification pending)

Use a **separate ordinary authenticated R1 connection** for each viewer;
keep the main control and `watch` connections independent.

The Rust entry point is the existing `isocast_client::Client::call`:
`call::<WhepAnswer>(Method::WhepOffer(WhepOfferParams { sdp }))`.
Retain that `Client` for the viewer's lifetime; call
`call::<serde_json::Value>(Method::WhepClose)` or drop it to stop watching.
The three types are in `isocast_protocol::api`. The receiver example is
`crates/isocast-daemon/examples/webrtc-receive.rs`; `--pair HOST` reads the
pairing code from stdin and creates that executable's own native identity.
Local-socket diagnostics use IPv4 loopback. An R1 viewer's UDP address must
match its authenticated TCP peer address; multihomed clients bind accordingly.

1. Create a WebRTC receiver with **no STUN or TURN servers**, `max-bundle`,
   RTCP multiplexing, one recvonly video transceiver and one recvonly audio
   transceiver. Gather host candidates, then send
   `whep_offer {"sdp": "v=0…"}`. The target is program; the initial rung
   is at most 960 pixels wide and requests 30 fps. As on `frames`, viewers
   at the same size share the highest requested encoder rate (up to 60 fps);
   actual cadence also follows the source and governor.
2. The result is `{"sdp":"v=0…","session":"…"}`: a sendonly SDP answer,
   equivalent to WHEP's 201 answer and Location resource. Set it as the
   remote description. Native offers should give both tracks the same
   stream id (`program`); both tracks use the engine clock.
3. Negotiate H.264 packetization-mode 1 with baseline/constrained-baseline
   or constrained-high/High profile; **the SPS must agree with the SDP profile**.
   Mac monitors prefer constrained-high, falling back to High if the encoder
   refuses it; the answer always negotiates the actual SPS profile. Audio is Opus,
   48 kHz stereo, initially 10 ms packets at 128 kb/s.
   RTP clocks are 90 kHz video and 48 kHz audio. str0m handles RTP/RTCP,
   DTLS-SRTP, retransmission, feedback and ICE consent.
   str0m's received frame data is **Annex B H.264** (parameter sets on IDRs)
   and raw Opus packets, not the old `frames` stream's avcC/AVCC records.
   Adapt that framing before feeding the existing native H.264 decoder.
4. Keep the signaling connection open. Closing it terminates the viewer,
   just as DELETE terminates a WHEP resource. No renegotiation, trickle or
   ICE restart in the initial profile; gather before offering, reconnect
   on an address/codec change. An explicit `whep_close` on that connection
   is the DELETE equivalent. A failed offer retains no session.
   An encoder reconfigure (for example a canvas size change) ends the
   viewer: the client must expect EOF and open a new connection to reoffer.

Every offer passes `Caller`, `local_only_reason` and effective-operation
policy as an ordinary remote caller. A loopback TCP peer is **not** local
authority. Revoking a paired identity or disabling remote control closes
its signaling connections and media; recordings remain independent.

## Safety and implementation gates

Prefer **str0m 0.24.1**, MIT/Apache-2.0, released 2026-10-03: a sans-I/O
implementation fits the engine's blocking-thread model, makes the packet
and deadline budget explicit, and avoids another async runtime. Its
[upstream](https://github.com/algesten/str0m) documents the single-mutation /
drain-output invariant. This is an actively developed pre-1.0 dependency,
not an audited security guarantee; pin it, inspect its crypto backend and
audit the resolved lockfile before merging. BitWHIP is useful deployment
precedent, not proof of our integration's correctness.

Prefer **opus2 0.4.0** with bundled `libopus_sys` for a small safe binding
to libopus, rather than a new codec implementation. It was updated in
March 2026; libopus_sys in April. Do not use `audiopus_sys`, marked
unmaintained in [RUSTSEC-2026-0150](https://rustsec.org/advisories/RUSTSEC-2026-0150.html).
The resolved binding uses `libopus_sys` 0.3.3. An OSV batch query on
2026-10-22 found no advisories for the 75 newly resolved registry packages,
including str0m 0.24.1, dimpl 0.7.6 and str0m-rust-crypto 0.6.0. This is
baseline-limited to the added packages and that database/date, not an audit
or a claim that all existing dependencies are advisory-free. The RustCrypto
feature still pulls AWS-LC transitively for certificate support; both it
and bundled libopus need CMake on the build host.

Reuse `record/frame_stream.rs`'s shared hardware H.264 encoder, not one
encoder per viewer. Add a bounded **audio-only program-mix tap**, not a
second mixer and not a video tap that retains GPU surfaces. Share Opus
encoding between viewers. PLI/FIR requests force a keyframe with a global
per-encoder one-second rate limit. The existing governor reduces preview
rates before encoding; viewer count and media demand must reach it, and
viewers must be evictable before recording loses frames.
When a recording, stream or replay is active, a continuous pressure episode
sheds just the newest viewer. Survivors are not cascaded out when it leaves;
another viewer is selected only after five quiet seconds and new pressure.
Per-frame encoder-stall flapping stays in the same episode. New
admissions wait for that active episode to clear. Idle auxiliary refusals
neither evict viewers nor block reconnects (the three-viewer cap still applies).

Bound offers to 64 KiB, sessions to 3 total per engine (therefore at most 3 per identity),
handshake lifetime to 10 seconds, and media queues explicitly. Allocate
UDP sockets only after authentication and validated SDP. Advertise host
candidates only on the chosen loopback/LAN interface; enforce allowed
destinations on **every outgoing datagram**, not just the advertised SDP.
No public candidates, relay servers or arbitrary remote UDP destinations.
No SDP or ICE credentials in logs. Losing a viewer never stops recording.
Source-IP filtering is not authentication: while a viewer exists, str0m's
ICE/DTLS parsers still process candidate UDP traffic. ICE credentials and
the DTLS fingerprint exchanged over R1 provide the media identity binding;
the three-session limit and timeouts bound that additional attack surface.

Verification before enabling the capability: a Rust receiver using normal
R1 pairing counts decoded/received H.264 and Opus; exercise malformed SDP,
ownership, revocation, disable, timeouts, disconnect and capacity in CI.
Measure timecode latency and matched 0/1/3-viewer CPU; inspect each recorded
file and require zero recording loss on hardware. A successful SDP answer
alone is not a working-media claim.

### Latency limits of this prototype

The existing program mixer deliberately holds audio for **200 ms**
(`record/mix.rs::MIX_DELAY`) to tolerate capture jitter. The Opus tap
preserves that recording-safe behavior. Fast video packet arrival therefore
does **not** prove synchronized sub-100-ms playback: the desktop must align
picture/sound by their RTP/RTCP clock mapping, or accept delayed sound.
Measure video and audio separately; a future low-latency mix tap must not
reduce the recording mix's jitter protection. The headless receiver does
not implement desktop playout or measure glass-to-glass latency.

### Verification, 2026-10-22

Normal R1 pairing on an isolated signed basebook engine: H.264 received and
independently decoded, Opus decoded, 1 and 3 viewers. Three 720p60 recordings
totaled **2,177 frames, zero dropped/repeated frames and zero audio dropouts**.
Revocation and remote-disable closed media in about 59 ms while recording
continued; a fourth viewer was refused. PLI and FIR produced recovery IDRs.

Without recording pressure, each viewer received **30 fps**. During recording,
the host's memory pressure made the existing governor cap previews at 15 fps;
no encoder pacing change was needed. In short, steady-30-fps idle-media
cohorts, daemon CPU was **2.00 / 6.71 / 7.36% of one core** for 0 / 1 / 3
viewers; this is observational, not a controlled performance qualification.
Video packet age was about **25 ms median / 28 ms p95**; audio about
**214 / 217 ms**, consistent with the mix hold above.

Remote workspace fmt/Clippy/tests passed (280 tests, one existing hardware
teardown test ignored), plus current-tree targeted checks and schema validation.
Hosted Mac/Engine E2E/Windows candidate CI remains the delegator's gate.
Windows hardware, LAN media and desktop playout are not qualified here.

## Versioning

Keep R1's frozen hello/pairing/mismatch exchange and `isocast/1` ALPN.
The desktop still requires matching product version and source commit
(dev commit differences warn), per decision 7. OpenRPC's `info.version`
is the stable protocol version from `hello::PROTOCOL` (also exposed as
`x-isocast-protocol`), not the crate/product version. Release-only version
bumps therefore do not change the generated contract; regenerate it when
the method table or schemas change. The OpenRPC format version is separate.
Publish new methods/capabilities additively; unknown methods fail with
`METHOD_NOT_FOUND`, never silently change meaning. An incompatible method
or media profile needs a new capability/version and an explicit migration,
not reinterpretation of existing fields. The generated document describes
the build, not a promise that every platform supports every capability.

## Browser later

Browser JavaScript cannot pin R1 raw keys or self-signed certificates. A
future HTTP/WHEP + WebSocket gateway therefore needs a separate decision:
Plex-style per-engine public certificates (issuance/renewal infrastructure),
a locally trusted HTTPS certificate (installation on every client), or
plaintext LAN HTTP plus token (interception risk; secure-context and mixed
content restrictions). Recommend keeping R1 for native clients and only
shipping browser access with trusted HTTPS. Loopback HTTP experiments do
not establish LAN browser compatibility. No browser work is required now.
