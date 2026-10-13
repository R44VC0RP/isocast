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
  no network unless granted, and cannot open cameras or microphones.
- If it exits five times in five minutes, it is turned off until you enable
  it again.

Example: [examples/plugins/deck](../examples/plugins/deck), a page on
`127.0.0.1` with a button per scene plus Record, Marker and Split, in plain
Python with only the standard library.

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
