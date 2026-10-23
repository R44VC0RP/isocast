# Isocast roadmap

An open-source, Rust, OBS-style recorder: scenes and sources, every source
also recorded as its own ISO file, and full control from a CLI and MCP.
Built first for Ryan's YouTube rig.

This file is the roadmap: priorities and what's in flight. The work
itself lives in **GitHub issues and milestones** (one issue per piece of
work); facts an agent needs are in [docs/STATE.md](docs/STATE.md); the
design of each feature is in its own doc under `docs/`. The old 5,800-line
task queue and its log are archived in `docs/archive/2026-10-22/`.

## Goals

1. **Record reliably.** Start and stop instantly, never lose a take.
2. **ISO recording built in,** every file on one clock.
3. **Scenes and sources like OBS,** with a clean, flat interface.
4. **Agent-native.** Anything the app does, the CLI and MCP do; setups are
   plain files.
5. **Extensible** through plugins.
6. **Open source** under MIT or Apache-2.0. Clean-room: never read OBS
   source.

## How we work (Ryan, 2026-10-22)

- **One owner per scarce machine** (Ryan, 2026-10-22: more lanes, as fast
  as possible): exon-desk (Windows, GPU) and the test Mac each have one
  owning lane; others use them only between that lane's measurements or
  through the hw-batch queue. Lanes otherwise scale with review capacity.
- **CI is QA.** A change merges only when the hosted Mac CI, Engine E2E and
  Windows CI are green on the exact candidate. Tests that run in CI are
  welcome (unit, integration, e2e scenarios, fuzz targets).
- **Finish before starting.** A lane ends merged, or paused with a
  checkpoint; nothing half-done is left unowned.
- **Ship on green.** Releases follow main when it's green, not big bangs.

## Direction (Ryan, 2026-10-22)

**Isocast is a server and its clients.**
- **The server is the engine (`isocastd`):** recording with per-source ISO files, mixing, streaming, CLI and MCP. It speaks standard protocols so **any client can connect**: WebSocket/HTTP JSON-RPC described by OpenRPC for control, and WebRTC (WHEP) for picture and sound ([docs/server-protocol.md](docs/server-protocol.md)).
- **The desktop app (the OBS-style view) is the client GUI.** Studio Mode (preview/program) stays.
- **The studio (in-app editor and timeline) is removed.**
- **Engine performance comes first.**

## Now

1. **Server performance on Windows** (owns exon-desk, ISO-6): a headless benchmark against OBS with its preview off, then engine slices.
2. **Server performance on Mac** (owns the test Mac, ISO-25): the same headless benchmark and profile; owns the portable audio and writer paths.
3. **Standard server protocol** (ISO-23): an OpenRPC contract for the method table; WebRTC media with WHEP semantics, signaled over R1's mutual TLS. **The desktop client is the only client that matters now; browsers come later.**
4. **Desktop client on a remote server** (agent/desktop-client): R2-core plus R2-ui, with live program video, Mac ↔ Windows.
5. **Remove the studio** (ISO-24).
6. **Linux server bootstrap** (ISO-26): headless isocastd on Ubuntu with test sources, software encoding, ISO files and R1.
7. **CI reliability** (ISO-22): hosted E2E flakes, so the merge gate is trustworthy.

## Next

- **Browsers as clients** (needs a certificate approach for the LAN).
- **Linux capture and hardware encoding** (needs a Linux PC with a GPU).
- **Mac 0.0.9** once main is green (Ryan's go).
- **Windows alpha readiness:** code signing (#121), DirectComposition release follow-ups, #116–#118, #128.

**Paused:** the PR visual judge (ISO-20), the website (copy pass finishing), remote engines R2 (superseded).

## Later

- Remote engines hardening (R7) on the standard protocol.
- Performance and governor follow-ups (#27, #110, #127), canvases,
  masks and media follow-ups (0.0.9 milestone).
- Linux (after remote engines; nothing above may block it).
- Playout (#129), scenes as code (#22), plugins (#23).
