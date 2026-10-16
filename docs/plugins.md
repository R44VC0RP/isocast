# Plugins

A plugin is a folder with an `isocast-plugin.toml` manifest. It says what
the plugin provides and which permissions it needs. Isocast never gives a
plugin more than it asked for and you granted.

```sh
isocast plugin add examples/plugins/deck          # lists what it asks for
isocast plugin add examples/plugins/deck --allow  # grants exactly that, installs it
isocast plugin list                               # state, grant, where its log is
isocast plugin remove deck
```

`--link` runs a plugin from its folder instead of a copy, for developing
it. Agents use `add_plugin`, `list_plugins` and `remove_plugin`.

There are two tiers today.

## Process plugins (any language)

A process plugin is a program Isocast starts, keeps running and restarts
if it crashes. It talks to the engine with the same JSON-RPC method table
as the CLI, one message per line, on the Unix socket in its
`ISOCAST_SOCKET` environment variable.

```toml
id = "deck"
name = "Deck"
version = "0.1.0"
api = 1
description = "Stream Deck-style buttons for scenes, Record, Marker and Split."
license = "MIT OR Apache-2.0"

[permissions]
methods = ["get", "status", "watch", "switch_scene", "marker", "split", "start", "stop"]

[process]
command = ["python3", "deck.py"]
autostart = true
```

- It may call only the methods in `permissions.methods`; anything else is
  refused.
- It runs in a macOS sandbox: it can write only to its own data folder, has
  no network unless granted, cannot open cameras or microphones, and
  cannot read recordings unless granted (below).
- If it exits five times in five minutes, it is turned off until you enable
  it again.

Example: [examples/plugins/deck](../examples/plugins/deck), a page on
`127.0.0.1` with a button per scene plus Record, Marker and Split, in plain
Python with only the standard library.

### Recordings

A plugin that works with recorded files asks for them:

```toml
[permissions]
methods = ["watch", "list_projects", "rename_project"]
recordings = "write"   # or "read"
```

| Grant | It may |
|---|---|
| none | nothing: the recordings folders are unreadable, wherever they are |
| `read` | read every file in the recordings folders |
| `write` | read, and **create new files** inside a project folder (a transcript, an export) |

`write` never lets it change, rename or delete a file that exists, its own
included: the sandbox (Seatbelt's `file-write-create`) lets a process make
a new plain file or folder and write the file through the handle that made
it, and nothing else. Write a file in one go (or write a new one to replace
it). It can only create inside a project folder (one named `<date> <time>
…` with Isocast's project file in it), never links (symbolic or hard),
manifests (`*.session.json`), `*.partial` files or Isocast's own `.isocast*`
files. No plugin can hard-link a recording, whatever its grant. The
recordings folders are the live setup's `recording.folder` and those
recorded into lately (by the engine or `isocast record`). A sandbox can't
change while its process runs, so when a recording starts in a folder its
sandbox doesn't have yet (or, for `write`, a folder named like a project
but without Isocast's project file has appeared since), Isocast restarts
the plugin's process with the folders as they are then
(`plugin_changed`, `restarted`); it may miss that recording's `started`
and `project_created` events, so read `list_projects` when it starts. A
folder made in between, or one only a standalone `isocast record` used,
gets its rules at the next such start.

### Projects

Every recording session writes into its own **project folder** in the
recordings folder, `<YYYY-MM-DD HH-MM-SS> Untitled/`: every take's program
and ISO files, their manifests (`….session.json`, listing files relative
to themselves) and the timeline exports made from them. A hidden
`.isocast-project.json` gives it a stable id (`20261014-153012`); its name
is the folder's name after the date and time. `list_projects` lists them;
`rename_project` (refused while a recording or an export writes into it)
renames the folder to `<date> <time> <name>` in one step (atomic on APFS
and HFS+; on exFAT a check and a rename) and moves timelines' references
(timelines also find a project renamed in the Finder by its id, and
`list_projects` points its export manifests at the new folder), so a plugin can name a take once `take_finished`
or `stopped` says it is done. Recordings from before project folders stay
where they are.

### Events

A plugin granted `watch` gets every state change as it happens, with ids
and, where there are files, their paths, so it never has to poll. Ask only
for what you need: `{"method": "watch", "params": {"kinds": ["take_finished",
"project_renamed"]}}` (`isocast watch --kinds/--except` on the CLI). Each
arrives once, as `{"jsonrpc":"2.0","method":"event","params":{"type":…}}`.

| Event (`type`) | When | Fields |
|---|---|---|
| `started` | a recording's first take began, or a split or retake began a new take | `files`, `take`, `manifest`, `folder`, `project`, `cause` (`start`, `split`, `retake`), `begins_at` (split/retake) |
| `take_finished` | a take's files are finished (stop, split, retake, or failure) | `take`, `manifest`, `files`, `folder`, `project`, `status` (`complete`, `failed`, `interrupted`), `duration_seconds`, `errors` |
| `stopped` | a recording ended, or failed to start | `report` (every take and file), `error`, `project`, `folder` |
| `recording_paused` | a recording paused or resumed | `recording` (`paused`, `elapsed_seconds`, `paused_seconds`), `project` |
| `marker` | a marker was added | `label`, `take`, `time_seconds`, `project` |
| `project_created` | a recording made a new project folder (as its first take began) | `project` (`id`, `name`, `folder`, `created_at`, `takes`, `files`) |
| `project_renamed` | a project was renamed | `project` (after), `old_name`, `old_folder` |
| `scene_changed` | a scene went on air | `scene`, `take`, `time_seconds`, `transition` |
| `preview_changed` / `preview_cleared` | studio mode's preview changed / was cleared | `scene` |
| `stream` | the stream connected, went live, is reconnecting, failed or stopped | `stream` (`state`, counters) |
| `timeline_changed` | a timeline was made, opened, closed or changed | `timeline`, `action`, `clip`, `clips`, `current` |
| `export_progress` | a timeline export moved on | `export`, `timeline`, `progress`, `file` |
| `export_finished` | a timeline export ended | `export`, `timeline`, `manifest`, `files`, `error` |
| `setup_changed` | a setup change was applied | `revision` (`get` reads it) |
| `library_changed` | a scene collection or profile was made, renamed, deleted, imported or switched | `action`, `kind`, `id`, `collection`, `profile` |
| `device_changed` | a camera, microphone, display or audio output appeared or went | `action` (`added`, `removed`), `kind`, `id`, `name` |
| `source_changed` | a live source started opening, runs, or failed | `status` |
| `engine_changed` | the live engine started or stopped | `running` |
| `plugin_changed` | a plugin was added, removed, enabled, disabled, crashed or restarted | `id`, `action`, `state`, `process` |
| `data_changed` | data keys were set or deleted | `revision`, `keys` |
| `hotkey_fired` | a hotkey ran its action | `binding`, `action`, `source`, `phase`, `error` |
| `virtual_camera` | the virtual camera's state changed | `camera` |
| `warning` | something needs attention (dropped frames, a failed save) | `message` |
| `status` | live counters, about once a second while recording | `live` |
| `levels` | audio levels, about 20 times a second | `levels` |

Fields added later are optional: a plugin should ignore fields it doesn't
know.

## WebAssembly audio filters

A WebAssembly plugin adds audio filters that run inside the engine's mixer,
next to the built-in ones. They are WebAssembly components (built for
`wasm32-wasip2`) for the interface in [wit/plugin.wit](../wit/plugin.wit),
run by Wasmtime.

```toml
[[audio_filters]]
id = "gain"
component = "gain.wasm"
params.gain_db = { type = "f32", default = 0.0, min = -30.0, max = 30.0, doc = "How much louder, in dB." }
```

Use one in a source's audio filters as `<plugin id>/<filter id>`:

```toml
[[sources]]
id = "music"
kind = "tone"
audio_filters = [{ kind = "gain/gain", gain_db = -6 }]
```

A component gets nothing but the plugin interface: no files, network or
clocks. Each call has a fixed budget of work and 64 MB of memory. A filter
that crashes, loops or runs out of memory is bypassed for that block and
restarted fresh, so the recording keeps every sample; after three failures
in a minute it is turned off until its settings change.

Example: [examples/plugins/gain](../examples/plugins/gain), a gain and a
sidechain ducker in Rust (`build.sh` rebuilds them; the built `.wasm` files
are included).

## Coming

- Shader filters written in WGSL.
- WebAssembly sources (video and audio made by a plugin).
- Plugin node types for scenes.

The full design is in [scenes-as-code.md](scenes-as-code.md), section 7.
