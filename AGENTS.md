# Isocast agent guide

Isocast is an open-source Rust recorder in the style of OBS: scenes and
sources, per-source ISO recording, and full control from a CLI and MCP.
It's built by coding agents in **lanes**: implementers open pull requests,
and one or two QA agents review, test and merge them; a coordinator assigns
work and routes review comments back to implementers.

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
- **Done means a green, ready pull request.** Commit focused commits on
  your `agent/<lane>` branch, run `scripts/remote-build.sh gates`, write
  your REPORT (below), then `scripts/pr.sh --ready`. It publishes your
  branch merged with main to `ci/<lane>` and opens or updates the PR; CI,
  Engine E2E and Windows run from that push; you needn't wait for them. The
  coordinator sends red runs and review comments back to your session (the
  PR carries its id). Rerun `pr.sh` after every new commit: a PR whose
  branch moved since its snapshot can't merge.
- **Never touch scarce hardware** (exon-desk, the test Macs, the GPU Linux
  host). Say what your change needs in REPORT's "Hardware needed"; the QA
  agent alone qualifies it through `scripts/hw-batch.py`.
- **Keep your card on the board current** (Ryan reads the board, not
  chat): `~/dev/isocast-status/board`. Use `board mv` when you start,
  `board note` at each milestone (one line: what's proven, with numbers),
  `board set --blocked "reason"` when stuck, and `board mv … review` when
  handing back. `BOARD_BY=<model · lane>` names you.
- **REPORT.md** (`/Volumes/IsocastBuild/<lane>/scratch/REPORT.md`) becomes
  the PR description: `# Title`, then `## What and why`, `## How it was
  verified` (commands and what they printed), `## Hardware needed` (`none`,
  `windows-gpu`, `mac` or `linux-gpu`, then what to run), and optionally
  `## STATE notes`, `## Area`, `## Linked issue`, `## Screenshots`. No
  placeholders, no AI attribution: both are refused.
- **Don't edit `docs/STATE.md`** (every lane conflicts there). Put durable
  facts in `## STATE notes`; the merger applies them. Decisions go in the
  feature's design doc.
- **Review comments** are PR comments starting `QA: approved` or `QA:
  changes requested`, then one numbered line per point
  (`path:line: what's wrong. Fix: what to do.`, `[nit]` for optional ones)
  and a `Checked:` line with what QA ran. Answer each point with a commit
  and `pr.sh`, not a reply.
- **QA merges** with `scripts/merge-pr.sh N [N…]` (several PRs are tested as
  one candidate, bisected if red; `--hw N="evidence"` for hardware PRs).
  The PRs' STATE notes go in as `--notes REF` (a docs-only commit based on
  main, not yet on it), in the same candidate and publish. Never commit to
  main between merges: every open PR goes stale and needs a full CI rerun.
  A hosted e2e or motion failure in a job no merged path can affect may be
  accepted with `--known-flake N="e2e: exact failure line"`; it goes into
  the merge message.
- **Ask Ryan only through the board inbox, and only when you genuinely
  need him:** a taste call, a UI check, an approval, something only he can
  do. `board ask "…" --kind check --image shot.png --card ISO-6` (or
  `decision` with `--option`s); keep working on whatever doesn't depend on
  the answer, and read it with `board inbox` or `board wait ASK-n`. Never
  ask in chat. Make ordinary design decisions yourself and record them.
- **Never end your turn while your own builds or benchmarks are in
  flight** (a PR's CI is tracked on the PR). Wait on them in the
  background and keep working on what doesn't depend on them; ending the
  turn stops the lane until someone notices.
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
- **Only the QA agent uses scarce machines** (exon-desk, the test Macs, the
  GPU Linux host), through `scripts/hw-batch.py`. Builds on exon-desk take
  `C:\isocast-dev\build.lock`; measurements take `gpu.lock`.
- Details: `docs/STATE.md`, "Machines".

## Commands

```sh
scripts/remote-build.sh gates      # fmt, clippy --locked -D warnings, tests
scripts/remote-build.sh build --to DIR isocastd isocast isocast-ui
scripts/sign-dev.sh <isocastd>     # after building the daemon for real runs
python3 scripts/reference-docs.py <isocast> --check
scripts/pr.sh --ready              # publish the lane and open/update its PR
scripts/prs.sh [--json]            # open PRs: CI, review, hardware, session
scripts/merge-pr.sh N [N…]         # QA: land green PRs into main and publish
ffprobe -v error -show_entries stream=codec_name,width,height,r_frame_rate,pix_fmt -of compact <file>
```

Scratch recordings go in your scratch folder, never Ryan's recordings
folder.

## Commits

One focused commit per verified piece. The message says what changed, why,
and what was verified, and calls out removed or replaced behaviour. Never
commit recordings, credentials or private logs and screenshots. GitHub
writes go only through `pr.sh`, `ci.sh` (a lane's ci/<lane> snapshot) and
`merge-pr.sh` (snapshots, never private history). No other push, amend
or history rewrite unless Ryan asks.
