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

### Desktop timing contract (monitor-audio slice, 2026-10-23)

The early monitor mix is WHEP-only. Recordings, streams and replays retain
their existing delivery policy; this is not a blanket latency guarantee for
every capture device or filter.

- Video RTP is the engine capture/composition clock at **90,000 Hz**; Opus
  RTP is that same clock at **48,000 Hz**. Do not rebase either track to its
  first received packet. They wrap independently at 32 bits.
- Use each track's RTCP **Sender Report**, not Receiver Report, to map its
  samples to common sender time:
  `capture_NTP = SR.NTP + signed_wrapping_delta(packet_RTP, SR.RTP) / rate`.
  str0m gets one shared `(engine Time, Instant)` anchor for the two tracks;
  encode/delivery delay must never replace capture time in that mapping.
  Opus packet PTS accounts for the encoder's queried lookahead, so the
  client must not subtract that codec delay a second time.
- Pair the negotiated program audio/video **MIDs of this WHEP session**
  (and common `program` MSID) as one sync group. str0m 0.24.1 currently emits
  different per-track CNAMEs; CNAME equality alone cannot discover this
  group. The actual SR mappings still share one wall-clock domain; no
  Isocast per-frame timestamps or synchronized client/server system clocks
  are needed to calculate relative A/V timing.
- Choose **one** sender-time-to-local-playout mapping and adaptive delay for
  both tracks, not a separate arrival-based epoch per track. Buffer the
  faster stream for the slower one. A 50–60 ms starting target is appropriate
  for fast inputs; include decoder/audio-device output latency and increase it
  for late capture/lookahead or network jitter. Apply drift correction to the
  audio device clock without
  changing the common video/audio capture timeline. Start at the first common
  available capture interval and discard older prefixes, rather than playing
  old audio alongside new video. Configured source sync offsets and filter
  latency compensation are already reflected in the timestamps. Reoffer after EOF.

The low-latency path is a separate monitor-only mixer fed non-blockingly from
the same timestamped raw inputs, with independent filter state and meters.
It may commit complete 10 ms blocks early, including all sidechains and
filter lookahead; each input gets a **40 ms capture allowance plus its filter
lookahead** before missing samples become silence, never the recording mix's
200 ms hold. For example, a 20 ms denoiser gets 60 ms after the block's end,
so ordinary device delay cannot make it commit not-yet-arrived lookahead as
silence. An unfiltered input still expires after 40 ms, even beside a denoiser.
This caps capture slack, not DSP latency: a chain declaring one second of
lookahead can put the monitor about a second behind.
WASM audio plugins can declare up to one second of lookahead each; a chain's
lookahead is the sum of its enabled filters.
Recording, streaming and replay continue using the original mixer, with its
unchanged samples, settings-at-commit semantics and unconditional live hold.
A source-graph change or reopened input triggers a fresh monitor tap without
replacing viewers, the Opus encoder or RTP sessions. Reattachment tries up to
five times with 100 ms backoff between failures while the engine/viewers remain;
exhaustion closes the feed. Brief audio gaps retain true capture timestamps. Live
gain/mute changes are heard sooner in the monitor than in the held recording
mix; this does not retroactively alter recorded audio.
Monitor plugin warnings are diagnostics-only, not surfaced as engine events or status.

Every offer passes `Caller`, `local_only_reason` and effective-operation
policy as an ordinary remote caller. A loopback TCP peer is **not** local
authority. Revoking a paired identity or disabling remote control closes
its signaling connections and media; recordings remain independent.

## Safety and implementation gates

### Monitor resolution and GPU ownership

`frames` and WHEP use the same `frame_stream::Streams` entry keyed by target
and requested width. Clients snapping to 480/640/960 share that rung's encoder;
WHEP's current program request is 960 wide. Rates are coalesced to the highest
viewer demand, not encoded once per protocol or viewer.

The monitor worker now resizes mismatched frames before handing them to a
native encoder. Previously macOS delegated this to VideoToolbox, whereas
Windows NVENC rejected a 1280×720 frame given to a 960×540 encoder. The
portable path reuses the existing filterless `IsoFrames` GPU draw/conversion
stage with its own bounded platform frame pool: Y'CbCr on macOS, BGRA on
Windows and the Linux bootstrap. No pixel readback is added by the resizer
(Linux's software encoder still needs its existing readback). Frames bypass
this stage only when both size and Y'CbCr/BGRA family match the encoder. Monitor
downscales use Area sampling; ISO stages explicitly retain their historical
Bilinear sampling. Recording routes, frames and encoder settings are unchanged.

The GPU completion callback only hands off a completed frame non-blockingly;
only the optional monitor worker waits, for at most the one-second GPU fence
budget. Device loss recreates the stage/encoder with up to three consecutive
recovery attempts and 100 ms backoff, retaining subscribers. Permanent failure
ends that monitor; it never blocks capture or a recording writer.

Qualification so far: Mac native/VM ladder decoding, concurrent frames plus
one/three WHEP viewers, unchanged recording timing/zero loss on basebook, and
injected-device-loss recovery pass. Windows builds pass; the desktop-client
lane owns fresh Windows runtime and bidirectional five-minute LAN qualification
with the real app after merge. Linux scaler runtime proof remains deferred;
matching its BGRA frame API is not a runtime qualification.

The hosted PLI/FIR test runs on an idle engine: recording pressure may legally
evict its viewer before the scheduled FIR (observed in run39926595275). It
still requires both requests and at least three IDRs. Recording safety runs
separately with the same shared960 monitor encoder/scaler and a WHEP viewer
active before the take. That viewer has no feedback-test requirement and may
be shed; only the take's zero-loss counters are required. Production shedding
and keyframe-rate limits are unchanged.

### LAN firewall requirements

- The engine needs inbound R1 TCP (47470 by default) and inbound UDP for its
  advertised host ICE candidate. Candidate UDP ports are allocated dynamically;
  allowing the TCP control port alone does not permit media.
- Windows Defender Firewall rules should be scoped to the engine executable
  and the trusted viewer address/subnet. A Windows receiver also needs inbound
  UDP from its engine for its host candidate. Apply rules to the actual LAN
  interface/profile; do not change a network profile or disable the firewall.
- Qualification uses only temporary, uniquely named rules scoped to the two
  test-machine LAN addresses, removed after all owned processes exit. No
  STUN/TURN, public port forwarding or host-wide firewall policy is needed.

### Transport libraries and bounds

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

Reuse `record/frame_stream.rs`'s shared platform H.264 encoder, not one
encoder per viewer. All WHEP viewers share one optional **monitor-only mix**
and Opus encoder. Its bounded raw-input fan-out retains no video surfaces;
filter histories, reports and meters are separate from the recording mix.
Its normal-priority worker cannot block the input producer. Queue overruns
become monitor-only gaps, not recording backpressure or a teardown of every
viewer. Graph changes and source reopens retap without reoffering. PLI/FIR requests
force a keyframe with a global
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

### Why the recording hold stays

`record/mix.rs::MIX_DELAY` is a **200 ms jitter/lookahead allowance**, not a
video timestamp correction. Capture PTS is already mapped to the host clock
(AVFoundation uses `camera::host_time` / `CMSyncConvertTime`; Windows uses
WASAPI's QPC timestamps). Delivery can still be late or bursty: microphone,
camera/system/loopback capture callbacks, decoded media and browser audio,
scheduler stalls, negative sync offsets, and dependencies on other sources'
sidechains. Noise suppression alone needs 960 future samples (20 ms);
plugins can declare further lookahead. This slice does not claim a measured
latency for each physical device.

The ordinary mixer retains the hold even when all input is ready. A previous
global early-complete change was rejected in `de5bfcd2` after real M2/M4
streaming/Stop experiments; recording-only early drain was retained instead.
`Input::filter` mutates filter history/buffered samples and `mix` consumes
them; advancing that original mixer also changes which live gain/filter
settings apply to recorded samples. Therefore it cannot be pulled early
without changing recording behavior.

The WHEP copy waits only for audible inputs and their sidechain/lookahead
dependencies. Complete blocks can leave immediately; each late dependency gets
40 ms of capture slack in addition to its own filter lookahead before missing
input becomes silence. Both the original
recording algorithm and its held RTMP/replay delivery are unchanged. The
monitor's filter histories start independently, so startup and live-setting
transients need not be sample-identical to the held program.

The monitor input uses the existing bounded capture-burst reserve; raw mixed
and encoded Opus queues cover one bounded catch-up pass (nine ten-ms blocks).
Lookahead is excluded from stall detection, but never expands that batch:
remaining ready blocks wait for the next pass rather than overflowing it.
Older backlog is skipped with true PTS, never retimestamped as current audio.
The headless receiver measures capture-to-receive age and decoded marker
alignment using actual SRs; it does not implement hardware playout or claim
glass-to-glass latency.

For a two-machine receiver run, `ISOCAST_WEBRTC_SAME_CLOCK` is invalid. Use
the measured receiver-minus-sender wall-clock offset in
`ISOCAST_WEBRTC_CLOCK_OFFSET_MS` and a whole-run uncertainty bound in
`ISOCAST_WEBRTC_CLOCK_UNCERTAINTY_MS`. Actual SRs then map capture time to the
first packet's receiver arrival time and, separately, the complete media
unit's delivery to the application before decoding. Calibration must include drift/clock
adjustments since the sender initialized str0m's frozen wall-clock anchor;
report every latency percentile with that uncertainty, not as playout delay.
The receiver reports delivered RTP sequence-range gaps and capture cadence
separately. These gaps include depacketizer omissions and exclude recovered
loss; they are not a count of raw pre-recovery network loss.

### Monitor-audio verification, 2026-10-23

On basebook with synthetic flash/beep sources over normal R1, a 14-second
receive run after the 40 ms/recovery fixes measured Opus **18.69 ms p50 /
22.50 ms p95** (previously ~214 ms). Actual RTCP SRs mapped 13 decoded
flash/beep pairs into one capture timeline: **2.43 ms worst absolute skew**,
within one 30-fps frame. Both tracks supplied SRs; their engine-clock anchor
discrepancy was 0.023 ms. This proves receiver
timing, not hardware speaker/display playout. Video arrival in this 30-fps
source run was 45.49/49.96 ms p50/p95, so the client still buffers the faster
audio to the common playout target.

A separate eight-second viewer survived adding a running audio source to the
program graph, receiving 373,920 decoded audio frames and 236 video frames
and closing the same signaling connection successfully. Its audio p50 was
27.27 ms. The previous binary failed this exact source-add check (zero viewers).

Five-second no-viewer and attached-viewer recordings each had **150 frames,
zero drops/repeats/audio dropouts**, identical per-track PTS/DTS/durations,
and matching recorded flash/beep alignment (0.021 ms worst). The recording
mix algorithm remains unchanged; deterministic tests compare every output
sample bit, meter bit and input/filter state with and without an early mix.
The separately captured movie files are not claimed byte-identical: their
wall-time metadata and burned-in source clocks naturally differ.

Full build-host gates passed (296 tests; one existing hardware-only ignore).
The hosted VM `webrtc` scenario also passed with actual SR/content checks:
six pulse pairs per viewer, at most 1.49 ms skew, and source-add continuity.
VM measurements are not a performance qualification. Physical device latency,
LAN jitter and final desktop playout still require their own qualification.

### Original transport verification, 2026-10-22

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
