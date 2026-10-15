# Isocast

**A free, open-source OBS alternative for Mac that records every source as its own file.**

Isocast is a screen recorder and streaming app for macOS on Apple silicon.
It works like OBS, with scenes, sources, transitions and an audio mixer,
and it also saves each screen, window and camera as a separate
full-quality file (ISO recording), all on one clock. It uses about half
the CPU of OBS for the same recording, and it imports your OBS scene
collections. Written in Rust, MIT or Apache 2.0.
[isocast.xyz](https://isocast.xyz)

![Isocast recording: the Screen scene shows a terminal with a picture-in-picture photo and a title, above the recording timer, frame rate, dropped frames and CPU; below are the scenes, the sources with their ISO badges, the audio mixer and the Controls dock with Stop, Pause, Marker, Replay Buffer, Virtual Camera and Studio Mode](docs/images/app.png)

[Download for Mac](https://github.com/R44VC0RP/isocast/releases) ·
[Getting started](docs/getting-started.md) ·
[Isocast vs OBS](#isocast-vs-obs) · [FAQ](#faq)

## Why switch from OBS

- **Every source in its own file.** Fix a shot in your editor without
  re-recording: the program video plus one file per screen, window and
  camera, lined up by a session file. No plugin, no syncing by hand.
- **Lighter on your Mac.** In our benchmarks Isocast used 25–29% of a core
  where OBS used 58–71%, and 88–356 MiB of memory where OBS used
  619–1344 MiB ([how it was measured](docs/benchmarks.md)).
- **Bring your OBS setup.** Import OBS scene collections and profiles;
  anything that can't come over is listed with the reason.
- **Scriptable.** The app, the `isocast` command and AI agents (over MCP)
  drive the same engine, so anything you can click you can automate.

## Isocast vs OBS

| | Isocast | OBS Studio |
|---|---|---|
| Price and licence | Free, MIT or Apache 2.0 | Free, GPL |
| Platforms | macOS 14+, Apple silicon | macOS, Windows, Linux |
| Scenes, sources, transitions, Studio Mode | Yes | Yes |
| Each source as its own file (ISO), on one clock | Built in | Not built in (third-party Source Record plugin) |
| CPU for the same recording ([benchmarks](docs/benchmarks.md)) | About half of OBS's | Baseline |
| Memory, 1080p60 on a base M2 | 88 MiB | 630 MiB |
| Control | App, `isocast` CLI and MCP, one engine | App, obs-websocket and scripts |
| Replay buffer | Coming in 0.0.5 | Yes |
| Virtual camera | In testing | Yes, mature |
| Plugins | Early ([process plugins, WebAssembly audio filters](docs/plugins.md)) | Large ecosystem |
| Track record | New, in alpha | Years of polish |

OBS is the better choice today if you need Windows or Linux, a specific
OBS plugin, or a proven virtual camera. Isocast is for Mac users who want
separate files per source, a lighter app, or a recorder they can script.

![Isocast compared with OBS 32.2.2: less CPU, less memory, faster stop and launch, and no skipped frames on a base M2](docs/images/benchmark.png)

Isocast and OBS Studio 32.2.2 recording the same synthetic 1080p60 scene
(left: MacBook Pro, M4 Max) and 4K60 scene (right: base M2 Mac mini) with
hardware HEVC, median of 4 interleaved runs, October 2026. On the M2 4K
run, OBS reported 3,904 frames skipped; only 1,645 of 6,292 were distinct.
Synthetic sources, not real capture: see [docs/benchmarks.md](docs/benchmarks.md).

## Features

**In Isocast 0.0.4 (the current download):**

- Scenes and sources: screens, windows, cameras, microphones, desktop
  audio, pictures, videos (including WebM and MKV), text and web pages,
  with filters and an audio mixer.
- Transitions, including stingers with track mattes, and Studio Mode.
- ISO recording: any source as its own file, plus the program, with a
  `.session.json` that says where each file starts and where every scene
  switch and marker falls.
- Pause and resume in the same file, markers, crash-safe fragmented files.
- Streaming to Twitch, YouTube, Kick, Facebook or any RTMP/RTMPS server,
  with stream keys kept in your Keychain.
- Profiles and scene collections, as in OBS, and OBS import.
- Global hotkeys, timelines (record takes in order, export one video), the
  `isocast` CLI and MCP tools for agents.
- Automatic updates.

**Coming in 0.0.5 (on the main branch, not in a release yet):**

- First launch finds your OBS scene collections and offers to import them,
  then an **Optimize** step picks settings this Mac can keep up with, for
  recording, streaming or both.
- **Replay buffer:** keep the last 5–300 seconds of the program in memory
  and save them on command or with a hotkey.
- **ISO capacity warnings:** Isocast tells you before and during a
  recording when this Mac's encoders can't keep up with all the files,
  and suggests the closest setup that fits.
- The virtual camera shipped inside the app (still being verified).

## A closer look

These pictures are from the main branch, so some show 0.0.5 features.

**Every source in its own file.** One take: the program video, an ISO file
per source and the session file that lines them up.

![One take in Finder: the program video, an ISO file per source and the session file that lines them up](docs/images/recordings.png)

**Studio Mode.** Line up the next scene in the preview while the program
stays on air, then take it live with a cut, a fade or a stinger.

![Studio Mode while recording: the Starting soon intro in preview on the left, the live Screen scene in the program on the right, and the Transition button with Cut, Fade and Fade to Black between them](docs/images/studio.png)

**Choose what gets its own file** in Settings › Output.

![Settings, Output page: the recording path, the quality preset, the program encoder and format, and the ISO Recording switches that give the Terminal and Slides sources their own files](docs/images/settings.png)

**Bring your OBS scenes** (0.0.5): on first launch Isocast offers the
scene collections it finds, then **Optimize** picks settings this Mac can
keep up with.

![The first-launch sheet, Bring your scenes from OBS: three OBS scene collections found on this Mac, each with its profile's size, frame rate, codec and stream settings, ready to import](docs/images/obs-import.png)

![Optimize summary, ready to apply: 1440p60, HEVC High, 3 ISO files, stepped down to fit a base M2, with each change from the current setup](docs/images/optimize.png)

**Script it.** The `isocast` command drives the same engine as the app.

![The isocast command in a terminal: adding a marker, then isocast status showing the recording, its frame counts with nothing dropped, and the three files it is writing](docs/images/cli.png)

## Download

1. Get the latest **Isocast.dmg** from
   [Releases](https://github.com/R44VC0RP/isocast/releases).
2. Open it and drag Isocast to Applications.
3. Isocast updates itself: it checks daily and asks before installing, or
   use **Isocast › Check for Updates…**.

You need a Mac with Apple silicon (M1 or later) and macOS 14 Sonoma or
later. Intel Macs aren't supported.

Isocast is in alpha: expect rough edges, and keep a backup recorder for
anything you can't redo.

## First steps

1. **Open Isocast and allow access.** The permissions step asks for
   screen recording, camera and microphone access; macOS may ask you to
   reopen Isocast after granting screen recording.
2. **Pick a start.** *Start with this Mac* builds ready-made scenes from
   your screen, camera and microphone. *Import from OBS* brings an OBS
   scene collection over.
3. **Record.** Press **Start Recording**, then **Stop**. **File › Show
   Recordings** opens the folder.

More in [Getting started](docs/getting-started.md).

## FAQ

**Can I import my OBS scenes?**
Yes. *Import from OBS* (or `isocast setup import-obs`) reads OBS scene
collections and profiles from `~/Library/Application Support/obs-studio`
or an exported `.json`, and lists anything without an Isocast equivalent.
The [OBS parity table](docs/obs-parity.md) shows how each OBS source,
filter and transition maps.

**Does it support stingers and WebM?**
Yes, since 0.0.4: stinger transitions (with track mattes), and WebM and
MKV files in media sources and stingers. Stinger audio isn't played yet.

**Can it stream to Twitch or YouTube?**
Yes: Twitch, YouTube, Kick, Facebook or any RTMP/RTMPS server, while
recording if you like. See [Streaming](docs/streaming.md).

**Where do my recordings go?**
To `~/Movies/Isocast` unless you choose another folder in Settings ›
Output, one folder per recording. Each take holds the program video, one
file per ISO source (`SCREEN-…`, `CAMERA-…`) and a `.session.json`.
Files are written in fragments, so a crash leaves the completed
fragments playable.

**Is it free? Is it open source?**
Yes and yes: free, MIT or Apache 2.0. It's a new app written in Rust, not
a fork of OBS, and no OBS code was read or copied.

**Does it work on Windows or Linux?**
Not yet. Mac first; the engine is built so other platforms can follow.

## Docs

- [Getting started](docs/getting-started.md),
  [recording](docs/recording.md), [streaming](docs/streaming.md)
- [Timelines](docs/timelines.md): record takes in order and export one video
- [Hotkeys](docs/hotkeys.md), [scenes as code](docs/scenes.md),
  [plugins](docs/plugins.md)
- References: [CLI](docs/cli.md), [setup files](docs/setup-reference.md),
  [MCP tools for agents](docs/mcp-tools.md)
- [Benchmarks against OBS](docs/benchmarks.md),
  [OBS parity](docs/obs-parity.md), [coming soon](docs/coming.md)

---

Building from source, how it works inside, and how to contribute:
[CONTRIBUTING.md](.github/CONTRIBUTING.md). Licensed under
[MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE); third-party licences
are in [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES).
