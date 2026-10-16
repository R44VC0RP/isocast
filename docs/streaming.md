# Streaming

Isocast streams the program (the canvas and the program mix) live over
RTMP or RTMPS as H.264 and AAC, with its own RTMP client. A stream runs on
the live engine next to any recording: starting or stopping a recording
doesn't touch it, and a slow connection never costs the program or a
recording a frame.

## Setting it up

Add a `[stream]` section to the setup and keep the stream key in the
Keychain:

```toml
[stream]
service = "twitch"            # custom, twitch, youtube, kick or facebook
# server = "rtmps://…/app"    # required for custom and kick; overrides a preset
video_bitrate_kbps = 6000     # default 6000
# rate_control = "constant"   # constant (default, CBR) or average
keyframe_interval_seconds = 2 # default 2, as Twitch and YouTube ask
audio_bitrate_kbps = 160      # 64, 96, 128, 160 (default), 192, 256 or 320
# width = 1280                # the size sent; default: the canvas, scaled
# height = 720                #   down to fit 1920×1080 (1080×1920 upright)
# reconnect = { enabled = true, max_retries = 25, delay_seconds = 2 }
```

```sh
isocast stream key set        # paste the key; it isn't echoed (or pipe it in)
isocast stream start          # returns once live, or says what to do
isocast stream status [--json]
isocast stream stop
isocast stream key clear
```

MCP has the same: `start_stream`, `stop_stream`, `set_stream_key`,
`clear_stream_key`, and the stream's health in `get_status`. Watchers get
a `stream` event at every state change (connecting, live, reconnecting,
failed, offline), bitrate-warning changes, and warnings when the stream
drops frames or audio chunks. `audio_dropouts` counts mix chunks lost at
the encoder tap and AAC packets discarded by the send queue. A
`bitrate_warning` appears after sent video stays above 130% of the video
target for five seconds; it clears when the rate recovers or the connection
leaves live. Warnings do not change the configured bitrate or rate control.

The stream key is never part of a setup, `get`, `status`, logs, errors or
`--json` output (errors have it replaced by `<stream key>`). It is a
generic password in the login Keychain: service `<app bundle id>.stream`
(`dev.isocast.release.stream` for Isocast.app, `dev.isocast.isocastd.stream`
for the dev app, `dev.isocast.unbundled.stream` for a bare `isocastd`),
account the service name (`twitch`), or the server address for `custom`.
Each build has its own service because macOS lets only the build that saved
an item replace it. Up to 0.0.3 every build used `dev.isocast.stream`; a
build moves a key from there to its own service once, when it may read it
without a prompt, and ignores one another build saved (#63).
For local testing the daemon also takes a key from `ISOCAST_STREAM_KEY`.

A server on the local network (an RTMP relay or another Mac) needs macOS's
Local Network permission: the first stream to it asks to allow "Isocast
Engine". Turned off, a `.local` name can't be looked up and an address
fails with "No route to host"; the error says where to turn it on (System Settings → Privacy & Security → Local
Network). Isocast can't read that setting, so `isocast permissions` doesn't
list it (#57).

## Several destinations at once

The `[stream]` section is the main destination (id `main`). More go in
`[[stream.destinations]]`, and `isocast stream start` streams the same
program to every one that is on, each with its own key, connection,
state, reconnects and counters: one failing (a wrong key, an ingest that
drops) never stops the others or a recording.

```toml
[stream]
service = "youtube"
video_bitrate_kbps = 6000

[[stream.destinations]]
id = "twitch"
service = "twitch"

[[stream.destinations]]
id = "backup"
service = "custom"
server = "rtmp://relay.local/live"
video_bitrate_kbps = 3000     # its own bitrate and size need an encoder
width = 1280                  #   of their own
height = 720
# enabled = false             # `stream start` skips it unless --only names it
```

A destination sets its service, server, `enabled`, and optionally its own
`video_bitrate_kbps` and `width`/`height`; everything else (rate control,
keyframes, audio, reconnecting) is the main destination's, which stays
the default for the others even while the main one is turned off.
Destinations that send the same size at the same bitrate share one H.264
encoder (one hardware session, encoded once and sent to each connection);
one with its own bitrate or size gets an encoder of its own only if this
Mac's encoders have room by the capacity estimate (#66, counting the
recording only while one runs); otherwise `stream start` refuses, naming
those destinations and how to share an encoder instead.

```sh
isocast stream destination add twitch            # id "twitch"
isocast stream destination add custom --id backup --server rtmp://relay.local/live \
    --video-bitrate-kbps 3000 --width 1280 --height 720
isocast stream destination set backup --enabled false
isocast stream destination set backup --clear video_bitrate_kbps --clear size
isocast stream destination remove backup         # not main, not while it streams
isocast stream key set --destination twitch      # each has its own key
isocast stream start [--only twitch]…            # every enabled one, or these
isocast stream stop [--only twitch]…             # the others keep going
isocast stream status [--json]                   # every destination
```

Each destination's key is its own Keychain item under the build's service
(see above): the main one keeps the account it always had (`twitch`, or a
custom server), an added one's is that account with its id (`twitch
[alt]`), so two channels on one service keep two keys. MCP has
`stream_destination_add`, `stream_destination_set`,
`stream_destination_remove`, `only` on `start_stream` and `stop_stream`,
and `destination` on the key tools. `stream_start` and `stream_stop`
return the first destination's status (the main one's if it took part)
with every destination's in `destinations`; `status.stream` stays the
main destination's and `status.streams` lists them all; `stream` events
carry the destination's `id`. Settings › Stream lists the destinations
(on/off, state, service, server, key, own bitrate and size), and the
status bar's LIVE details show each one's state.

## Service presets

Each preset fills in the service's documented ingest server, RTMPS where
the service supports it. A `server` in the setup replaces it (for example
a nearer Twitch ingest from the list below).

| Service | Server | Source |
|---|---|---|
| `twitch` | `rtmps://ingest.global-contribute.live-video.net/app` | Twitch's ingest list, entry "Default" (`url_template_secure`): https://ingest.twitch.tv/ingests; URL format: https://dev.twitch.tv/docs/video-broadcast/ |
| `youtube` | `rtmps://a.rtmps.youtube.com/live2` (port 443) | YouTube's RTMPS ingestion guide (port 443, SNI): https://developers.google.com/youtube/v3/live/guides/rtmps-ingestion and https://support.google.com/youtube/answer/10364924; YouTube Studio shows this address as the RTMPS stream URL |
| `facebook` | `rtmps://rtmp-api.facebook.com/rtmp` | Live Video API, getting started (RTMPS only; the key is everything after `/rtmp/`): https://developers.facebook.com/documentation/live-video-api/getting-started |
| `kick` | none: each channel has its own | Copy the Stream URL from Kick's Creator Dashboard (Channel → Stream URL and Key); Isocast adds the `app` path if it's missing |
| `custom` | `server` | Any RTMP or RTMPS server |

Kick and Facebook only take RTMPS; the setup check says so for an
`rtmp://` server.

## How it works

- **Encoding.** VideoToolbox H.264, High profile, real time, no B-frames
  (decode order is presentation order, so FLV needs no composition
  offsets), Rec. 709 tags, speed over quality. The engine asks for every
  keyframe itself, on a grid that starts at each connection's first frame
  (a keyframe), so they come exactly every `keyframe_interval_seconds`
  from the start.
  Rate control: `average` is AverageBitRate plus a one-second
  DataRateLimit at the target, so still scenes send far less (a static
  1080p60 card: 150–350 kb/s of 6000); `constant` is VideoToolbox's
  ConstantBitRate, which pads still scenes to exactly the target every
  second, at the cost of overshooting very busy scenes by ~7–17% more.
  `constant` is the default for every service: YouTube's encoder guidance
  asks for CBR (and warns about streams far under their bitrate), and so
  does the usual advice for Twitch; OBS offers the same two modes for the
  Apple encoder (CBR on Apple silicon since OBS 28). Very busy scenes (full-screen noise, fast
  particles) overshoot the target in both modes: the encoder stops raising
  its quantizer near 49. AAC-LC via AudioToolbox at 48 kHz stereo, in its
  constant bitrate mode: every second carries `audio_bitrate_kbps`, even
  silence (the default mode sent ~2 kb/s of a 160 target for silence and
  ingests flagged it); frame times are
  moved back by the encoder's priming, so sound and picture line up.
  Recordings keep the variable mode (it saves space and isn't checked by
  anyone).
  When the stream's size isn't the canvas's, the encoder scales.
- **Protocol.** FLV tag bodies (AVC and AAC sequence headers, `onMetaData`)
  over Isocast's RTMP client, written from Adobe's RTMP specification:
  plain handshake, 4096-byte chunks, `connect` (app, tcUrl, flashVer,
  type nonprivate), `releaseStream`, `FCPublish`, `createStream`,
  `publish` (live), and answers to window acknowledgements, peer
  bandwidth and pings. RTMPS uses native-tls (Security.framework's TLS and
  the system's trusted certificates); `ISOCAST_STREAM_CA_FILE` can add one
  more certificate authority for testing against a local server.
- **Queueing.** Encoding and sending run on their own threads; audio and
  video are put in time order. If sending falls more than a second
  behind, queued video other than keyframes is dropped, and new video
  until the next keyframe (so what follows still decodes); keyframes and
  audio stay, up to five seconds. `congestion` in the status is how close
  the queue is to that second. If encoding itself falls behind (the
  media engines are busy, for example with recordings), program frames it
  can't take are skipped; they count in `dropped_frames` too. The tap holds
  at most eight packets, and the H.264 encoder admits at most four frames
  awaiting callbacks, so a stalled stream cannot retain the whole program
  frame pool. Audio lost at the tap is counted immediately, even while the
  encoder is blocked; gaps are filled with bounded chunks of silence.
- **Reconnecting.** After a dropped connection the stream reconnects
  every `delay_seconds`, up to `max_retries` times, and resumes with the
  metadata, sequence headers and a fresh keyframe, its timestamps
  starting again from zero. A first connection that fails is reported at
  once (it is usually the server or key). Each outage gets all of
  `max_retries`: the count starts again whenever the server accepts the
  stream. Connections that drop within 10 s are counted too, and
  `max_retries` of them in a row also end in `failed`, so a server that
  accepts the stream and drops it straight away (as some do for a wrong
  key) isn't retried forever; when the server accepted every connection
  and dropped each one, the error adds a hint to check the key.
- **Stopping.** A stop says goodbye to the server (`FCUnpublish`,
  `deleteStream`); if the server has stopped reading, the connection is
  cut after 0.5 s rather than waiting out the write timeout.

## Testing locally

Never stream to a real service while testing. An ffmpeg listener stands
in for one:

```sh
ffmpeg -listen 1 -i rtmp://127.0.0.1:19350/live/KEY -c copy out.flv
```

with `server = "rtmp://127.0.0.1:19350/live"` and the key `KEY`. For
RTMPS, put a TLS terminator in front (ffmpeg's `rtmps` listener doesn't
take a certificate), for example
`socat OPENSSL-LISTEN:19351,cert=server.pem,key=server.key,verify=0,fork TCP:127.0.0.1:19350`,
use `rtmps://localhost:19351/live`, and start the daemon with
`ISOCAST_STREAM_CA_FILE` naming the CA that signed `server.pem`.
