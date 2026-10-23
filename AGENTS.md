# Isocast agent guide

Isocast is an open-source Rust recorder in the style of OBS: scenes and
sources, per-source ISO recording, and full control from a CLI and MCP.
It's built by coding agents in **lanes**, coordinated by a delegator that
assigns work, reviews, merges and releases.

## Start here

1. Read [docs/STATE.md](docs/STATE.md) (current facts, machines,
   gotchas) and [PLAN.md](PLAN.md) (priorities).
2. Read the GitHub issue you were assigned and the design doc for your
   area (for example `docs/remote-engines.md`).
3. Run `git log --oneline -10` and `git status`.

History (the old task queue, 11,000 lines of notes) is archived in
`docs/archive/2026-10-22/`. Grep it when you need a past decision; don't
read it front to back.

## How a lane works

- **One assignment, finished.** Do the issue you were given. If it's
  large, split it into commits that each build and work.
- **Verify the claim directly** with real runs (the real command, the real
  file, `ffprobe`), and add tests that run in CI for anything that could
  regress: unit, integration, e2e scenarios in `scripts/e2e.py`, fuzz
  targets for parsers of untrusted input. Test behaviour, not source text.
- **Done means CI green.** The hosted Mac CI, Engine E2E and Windows CI
  must pass on your candidate before merge. The delegator runs them.
- **Keep your card on the board current** (Ryan reads the board, not
  chat): `~/dev/isocast-status/board`. Use `board mv` when you start,
  `board note` at each milestone (one line: what's proven, with numbers),
  `board set --blocked "reason"` when stuck, and `board mv … review` when
  handing back. `BOARD_BY=<model · lane>` names you.
- **Leave work uncommitted** unless told otherwise, with `REPORT.md` in your
  scratch folder: what changed, how it was verified, a suggested commit
  message. The delegator commits and merges.
- **Record durable facts** in `docs/STATE.md` (short, dated, only what's
  still true) and decisions in the feature's design doc. Delete what stops
  being true.
- **Ask Ryan only through the board inbox, and only when you genuinely
  need him:** a taste call, a UI check, an approval, something only he can
  do. `board ask "…" --kind check --image shot.png --card ISO-6` (or
  `decision` with `--option`s); keep working on whatever doesn't depend on
  the answer, and read it with `board inbox` or `board wait ASK-n`. Never
  ask in chat. Make ordinary design decisions yourself and record them.
- **After three failed attempts** at the same problem, stop, write the most
  likely wrong assumption in REPORT, and hand back.

## Ground rules

- **Portable core, platform code behind traits:** `isocast-platform-macos`,
  `isocast-platform-windows`; nothing OS-specific in portable crates.
- **One method table.** GUI, CLI and MCP are thin clients of the same
  engine API; no GUI-only or CLI-only capability.
- **Ids, not names.** Address sources and scenes by stable ids.
- **Agent-friendly errors:** name the field path and list the valid
  options. Every CLI command that prints data supports `--json`.
- **Never lose a recording.** Crash-safe writing, disk checks before start,
  and dropped frames surfaced.
- **Small changes.** No unrelated refactors; new dependencies only when the
  task needs them, named in REPORT.
- **Clean-room.** Never read or copy OBS source code.
- **Never touch Ryan's personal data** (his OBS profile, recordings, app
  settings). Use scratch copies.

## Machines

- **Never build on Ryan's Mac;** use `scripts/remote-build.sh`.
- **One lane owns each scarce machine** at a time: exon-desk (Windows) and
  the test Mac. The delegator says which lane. Builds on exon-desk take
  `C:\isocast-dev\build.lock`; measurements take `gpu.lock`.
- Details: `docs/STATE.md`, "Machines".

## Commands

```sh
scripts/remote-build.sh gates      # fmt, clippy --locked -D warnings, tests
scripts/remote-build.sh build --to DIR isocastd isocast isocast-ui
scripts/sign-dev.sh <isocastd>     # after building the daemon for real runs
python3 scripts/reference-docs.py <isocast> --check
scripts/windows-ci.sh              # hosted Windows CI on the committed HEAD
ffprobe -v error -show_entries stream=codec_name,width,height,r_frame_rate,pix_fmt -of compact <file>
```

Scratch recordings go in your scratch folder, never Ryan's recordings
folder.

## Commits (delegator)

One focused commit per verified piece. The message says what changed, why,
and what was verified, and calls out removed or replaced behaviour. Never
commit recordings, credentials or private logs and screenshots. No
push, amend or history rewrite unless Ryan asks.
