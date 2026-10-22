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

- **At most three lanes at once.** Each scarce machine has one owner:
  exon-desk (Windows, GPU) gets **one** lane at a time; the test Mac gets
  one.
- **CI is QA.** A change merges only when the hosted Mac CI, Engine E2E and
  Windows CI are green on the exact candidate. Tests that run in CI are
  welcome (unit, integration, e2e scenarios, fuzz targets).
- **Finish before starting.** A lane ends merged, or paused with a
  checkpoint; nothing half-done is left unowned.
- **Ship on green.** Releases follow main when it's green, not big bangs.

## Now

1. **CI green and trusted** (in flight): fix main's red hosted CI, then run
   every workflow on each candidate before merge (`ci/<lane>` branches).
2. **Knowledge reset** (in flight): `docs/STATE.md`, this file, a short
   `AGENTS.md`; history archived.
3. **Windows beats OBS on CPU** (in flight, exon-desk's one lane):
   DirectComposition previews, the port of macOS `layers.rs`, so the app
   stops redrawing its whole window at 60 fps. #21, #105.

## Next

4. **Mac 0.0.9:** release what's on main once CI is green (needs Ryan's
   go; remote control stays off by default).
5. **Remote engines R2:** the app on a remote engine. Two paused branches
   (`agent/remote-r2-core`, `agent/remote-r2-ui`) with checkpoints; resume
   as one lane, Mac-first. Must include live program video in the remote
   window (low quality is fine). Design: [docs/remote-engines.md](docs/remote-engines.md).
6. **Windows alpha readiness:** code signing (#121), the alpha checklist,
   #116–#118, #128.

## Later

- Remote engines R3–R7 (encoded previews, sound, input, host mode,
  hardening).
- Performance and governor follow-ups (#27, #110, #127), editor, canvases,
  masks and media follow-ups (0.0.9 milestone).
- Linux (after remote engines; nothing above may block it).
- Playout (#129), scenes as code (#22), plugins (#23).
