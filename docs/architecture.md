# How Isocast works

```text
   Isocast.app     isocast (CLI)     isocast mcp     plugins
        │                │                │              │
        └────────────────┴───── JSON-RPC ─┴──────────────┘
                      over a local Unix socket
                                 │
   ┌─────────────────────────────┴──────────────────────────────┐
   │ isocastd (the engine)                                      │
   │   setup document (revisioned)  ·  one clock  ·  scenes     │
   │   GPU compositor (wgpu)  ·  audio mixer  ·  plugin host    │
   └──────────┬───────────────────────────────────┬─────────────┘
     capture: ScreenCaptureKit,          encode and write: VideoToolbox,
     AVFoundation, Core Audio            AVAssetWriter, RTMP(S) out
```

- **One engine, many clients.** `isocastd` does all the work: capture,
  compositing, mixing, encoding, writing files and streaming. The app, the
  `isocast` command, MCP and plugins are thin clients of one method table.
  No feature exists only in the app.
- **One document.** A setup is plain data (TOML for people, JSON for the
  API) with one JSON Schema (`isocast setup schema`). Clients read it, plan
  a change and apply it at a revision, so the app, a script and an agent
  can change the same setup without overwriting each other.
- **Ids, not names.** Sources, scenes, items and clips have stable ids.
- **One clock.** The program and every ISO file share one clock; each
  recording writes a `session.json` that says where every file starts, plus
  scene switches and markers.
- **Crash-tolerant files.** Recordings are fragmented movies (a fragment
  every 2 s), so if isocastd or the app crashes or is killed, the fragments
  already completed stay playable: the last seconds are lost, and a file
  killed before its first fragment may not open. Power loss and kernel
  panics are not covered: nothing forces the files (or the manifest) to
  the disk, so what macOS still held in memory is lost too, and how much
  survives isn't guaranteed (#3). The engine checks disk space before it
  starts and reports every dropped frame.

## Crates

| Crate | Job |
|---|---|
| `isocast-protocol` | The setup document, validation, JSON Schema and API types |
| `isocast-engine` | Sessions, clock, recording, scenes, compositor, mixer, plugins |
| `isocast-platform-macos` | macOS capture, encoding, file writing, devices |
| `isocast-daemon` | `isocastd`: the engine process and its control socket |
| `isocast-client` | The client side of the socket, used by the CLI and apps |
| `isocast-cli` | The `isocast` command and its MCP server |
| `isocast-ui` | The app, Isocast.app, in Slint |
| `isocast-app` | The retired web-view app (not built by default; removed after the alpha week) |
| `isocast-surfaces` | Shared GPU surfaces between processes |

Platform code lives in `isocast-platform-macos` behind traits defined in
the portable crates, so other platforms can follow.
