# Getting started

Isocast records and streams like OBS, from scenes of screens, cameras,
microphones, pictures, text and web pages, and also saves every source as
a file of its own (ISO recording) on one clock. The app, the `isocast`
command and AI agents over MCP all drive the same engine, `isocastd`.

macOS on Apple silicon only, for now.

## Install

Download the latest `Isocast.dmg` from
[Releases](https://github.com/R44VC0RP/isocast/releases), open it and drag
Isocast to Applications. You need macOS 14 or later on Apple silicon.

The first time you record, macOS asks for screen recording, camera and
microphone access. Allow them in System Settings → Privacy & Security; after
granting screen recording, quit and reopen Isocast (or run
`isocast restart`). `isocast permissions` shows what is granted and what to
do about the rest.

Building from source instead? See
[CONTRIBUTING.md](../.github/CONTRIBUTING.md).

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

Press **Record** (or `isocast start`, and `isocast stop`). Each recording
gets its own project folder in the setup's recordings folder
(`~/Movies/Isocast` for a setup made by "Start with this Mac"), named
`2026-10-12 09-30-00 Untitled`, holding every take of that recording (a
`split` adds a take to the same folder) and the timeline exports made from
them:

- `2026-10-12 09-30-00.mov`: the program, what the scenes show;
- `SCREEN-….mov`, `CAMERA-….mov`: each ISO source at its own resolution,
  with the microphone;
- `2026-10-12 09-30-00.session.json`: the manifest, with where each file
  starts on the shared clock, every scene switch and marker.

Completed movie fragments can survive a crash; see the recovery limitations
below. Power loss is not covered: the files aren't forced to the disk, so
more than the last fragment can be lost. `isocast split` starts a new take without stopping anything;
`isocast marker "intro done"` marks a moment. `isocast project list` shows
the projects and `isocast project rename <id> "Rust talk"` names one once it
has stopped (the folder becomes `2026-10-12 09-30-00 Rust talk`; manifests
and timelines keep finding its files). Recordings from before project
folders stay where they are.

`isocast pause` and `isocast resume` keep the same take and files. The paused
span is removed from the program and every ISO, including their audio;
streaming and preview continue. `isocast status --json` reports `live.paused`,
`live.paused_seconds` and recorded `live.elapsed_seconds` (excluding pauses).
Both commands support `--json`; MCP exposes `pause_recording` and
`resume_recording`. Watchers receive `recording_paused` on either change.

A marker during a pause lands at the frozen recording playhead. Resume
before splitting or retaking; stopping while paused finalizes the existing
files. An explicit `start --seconds N` remains a wall-clock deadline, so it
can stop a paused recording. A paused/resumed take adds one timeline clip,
with only its recorded duration. The manifest's `pauses` list gives each
pause's uncut `start_seconds` / `end_seconds` and its `time_seconds` join on
the saved take; an unfinished pause has a null end until resume or stop.

Cuts share an exact frame/sample boundary across all files: the smallest
step every file's frame rate and the 48 kHz audio share, so 50 ms when all
files are 60 fps, 100 ms with a 30 fps file, 250 ms with 24 fps and up to
1 s with unusual rates. Pause and resume return after that boundary and the
manifest checkpoint, so a pause can take up to one step to land and 100
pause/resume pairs take about 10 s at 60 fps (20 s with a 30 fps file). A
concurrent direct pause/resume request during a pending cut is refused
("a pause/resume boundary is pending"); retry after the reply. Hotkey actions
are queued and executed in order instead. Pausing works from the take's first
frames: one asked for right as `start` returns cuts at
about 0.1 s. Audio fades to and from silence over 5 ms at the join to avoid
clicks, without shifting samples or changing the duration.

**Crash recovery while paused.** The writer requests 2-second fragments of
recorded time, but Pause only confirms the cut clock and manifest checkpoint.
It does not confirm that any fragment has reached disk. Each manifest pause
therefore records `recoverable_seconds_at_pause: null` (unknown, not zero),
including after Resume. The former cut-time-derived `crash_safe_seconds`
field was unreliable and is no longer emitted or trusted when reading older
manifests.

A cut exactly at 2.000 s removes the sample needed to close the first
fragment: after a crash the entire file can be unreadable. At 4.000 s only
2 s may survive. Even a cut just past a boundary can lose the preceding
fragment when killed immediately after the reply, while writers are still
draining. Audio and video need separate inspection: a recovered 2 s video
fragment can contain only 1.961333 s of AAC. Neither the cut time nor a frame
counter establishes a common recoverable duration across outputs and tracks.
Resume restarts recording; its reply does not make the pre-pause tail
durable. A successful Stop finalizes the files. After interruption, recovery
messages report unknown durations rather than claiming exact saved or lost
seconds.

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
Drag sources on the canvas to move them (a corner keeps their shape and
Shift frees it, Option crops, ⌘ ignores snapping); the inspector on the right has their exact
position, fit, crop, blending, filters and settings.

## Agents

Every command takes `--json`. For an agent over MCP:

```sh
claude mcp add isocast -- isocast mcp
```

The tools (`get_setup`, `plan_change`, `apply_change`, `start_recording`,
`live_frame`, `start_stream`, …) are the same methods the app uses. See
[mcp-tools.md](mcp-tools.md) for all of them.

## Going further

- [Setup reference](setup-reference.md): every field of a setup file.
- [CLI reference](cli.md): every command and option.
- [Scenes as code](scenes.md): layout, live data and keyframe animations.
- [Timelines](timelines.md): record takes in order and export one video.
- [Plugins](plugins.md): process and WebAssembly plugins.

## When something is wrong

- `isocast doctor` checks permissions, encoders, devices and the disk.
- `isocast status` shows the live engine, each source's state and any
  error; `isocast watch` prints events as they happen.
- The engine's log is `~/Library/Application Support/Isocast/isocastd.log`.
- Reading pictures or videos in Downloads, Documents or Desktop makes macOS
  ask Isocast for access to that folder; allow it, or keep media
  elsewhere.
