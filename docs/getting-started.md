# Getting started

Isocast records and streams like OBS, from scenes of screens, cameras,
microphones, pictures, text and web pages, and also saves every source as
a file of its own (ISO recording) on one clock. The app, the `isocast`
command and AI agents over MCP all drive the same engine, `isocastd`.

macOS on Apple silicon only, for now.

## Build and open the app

```sh
cargo build --release
scripts/sign-dev.sh target/release/isocastd   # the engine, signed so permissions stick
scripts/app-dev.sh --release                  # the app, as ~/Applications/Isocast.app
open ~/Applications/Isocast.app
```

`scripts/sign-dev.sh` puts the engine in
`~/Library/Application Support/Isocast/Isocast Dev.app`. macOS asks once
for screen recording, camera and microphone access for "Isocast Dev"; the
grants survive rebuilds as long as you sign again after each build. After
granting screen recording, restart the engine: `isocast restart`.

`scripts/app-dev.sh` takes `ISOCAST_APP=/some/where/Isocast.app` to put
the app elsewhere.

## The first setup

With no setup yet, the app offers three starts:

- **Start with this Mac** builds scenes from your devices: "Screen +
  Camera" (the camera in a corner), "Camera" and "Screen", recording the
  program and an ISO file of the screen and the camera, with your
  microphone and desktop audio in the mix.
- **Import from OBS** reads an OBS scene collection (Scene Collection →
  Export, or the files in `~/Library/Application Support/obs-studio/basic/scenes`)
  and lists anything that couldn't come over.
- **Open a setup** loads an Isocast setup file (`.toml` or `.json`).

The same from the command line:

```sh
alias isocast=target/release/isocast
isocast permissions                       # what's granted, and what to do if not
isocast devices                           # displays, windows, cameras, mics, outputs
isocast setup init --apply                # or --camera NAME --microphone NAME --layout side_by_side ...
isocast setup import-obs "My Scenes" -o my.toml && isocast apply --revision 0 my.toml
```

The live setup is kept as a **scene collection** (scenes, sources,
transitions, canvas size) and a **profile** (frame rate, recording, stream,
audio), as in OBS, in `~/Library/Application Support/Isocast/setup/`
(each with its previous version as `.bak`), so it is there next time. Keep
several and swap them from the **Profile** and **Scene Collection** menus
(New, Duplicate, Rename and Remove act on the one in use; Import adds an
exported one, a whole setup file or OBS's; Export writes the one in use;
the list below switches), or from the command line:

```sh
isocast collection list                   # and `isocast profile list`
isocast collection new "Gaming" --switch  # one empty scene; or `duplicate ID NAME`
isocast profile switch streaming          # refused while recording or streaming
isocast collection import --obs "My Scenes" --obs-profile "My Profile"
isocast profile export default -o studio.toml   # `import` adds it on another Mac
```

A `setup.toml` from an earlier version is taken in once as the "default"
collection and profile (a copy goes to `setup/backups/`) and left as it was.

## Recording

Press **Record** (or `isocast start`, and `isocast stop`). Recordings go to
the setup's folder (`~/Movies/Isocast` for a setup made by "Start with
this Mac"):

- `2026-10-12 09-30-00.mov`: the program, what the scenes show;
- `SCREEN-….mov`, `CAMERA-….mov`: each ISO source at its own resolution,
  with the microphone;
- `2026-10-12 09-30-00.session.json`: the manifest, with where each file
  starts on the shared clock, every scene switch and marker.

Files are written in 2-second fragments, so even a crash leaves them
playable. `isocast split` starts a new take without stopping anything;
`isocast marker "intro done"` marks a moment.

To build one video from takes, open a timeline first:
`isocast timeline new "My video"`. Every take that finishes (`isocast stop`,
or a split) then lands at its end. `isocast retake` records the last take
again (while recording, it drops the take in progress and returns once the
new take has begun, about a quarter second later);
`isocast timeline show`, `isocast timeline remove c2` and
`isocast timeline move c3 1` edit the order; `isocast timeline export`
joins the clips into `My video PROGRAM.mov` plus one file per ISO source,
next to the takes. Taking a clip off a timeline never deletes its files.

Recording quality defaults to **high**. Choose `low`, `medium`, `high`,
`very_high` or `incredible`, or keep using a numeric quality from 1–100.
See [recording encoders](recording.md) for settings and measured tradeoffs.

## Streaming

Streaming sends the program to Twitch, YouTube, Facebook, Kick or any RTMP
or RTMPS server. The stream key stays in the macOS Keychain; it is never
written into a setup or shown anywhere.

```sh
isocast stream key set                    # paste the key, then Enter
isocast apply --revision N --patch '{"stream": {"service": "twitch"}}'
isocast stream start                      # and `isocast stream stop`
isocast stream status
```

The app's **Go live** button does the same. See [streaming.md](streaming.md)
for services, bitrates and how it behaves on a bad connection.

## Studio mode, scenes and sources

Number keys switch scenes. **Studio** shows a preview next to the program:
edit the preview, then **Take** it on air with the chosen transition.
Drag sources on the canvas to move them (Shift keeps their shape, Option
crops, ⌘ ignores snapping); the inspector on the right has their exact
position, fit, crop, blending, filters and settings.

## Agents

Every command takes `--json`. For an agent over MCP:

```sh
claude mcp add isocast -- "$PWD/target/release/isocast" mcp
```

The tools (`get_setup`, `plan_change`, `apply_change`, `start_recording`,
`live_frame`, `start_stream`, …) are the same methods the app uses.

## When something is wrong

- `isocast doctor` checks permissions, encoders, devices and the disk.
- `isocast status` shows the live engine, each source's state and any
  error; `isocast watch` prints events as they happen.
- The engine's log is `~/Library/Application Support/Isocast/isocastd.log`.
- Reading pictures or videos in Downloads, Documents or Desktop makes macOS
  ask "Isocast Dev" for access to that folder; allow it, or keep media
  elsewhere.
