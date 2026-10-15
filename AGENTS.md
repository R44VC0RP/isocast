# Isocast agent guide

Isocast is an open-source Rust recorder in the style of OBS: scenes and
sources, per-source ISO recording, and full control from a CLI and MCP.
It is built by coding agents working continuously through the task queue in
`PLAN.md`.

## The loop

Work autonomously. There are no natural stopping points: finishing a task
means starting the next one. Keep looping until the queue is empty, every
remaining task is blocked, or you run out of budget.

When starting (or resuming after a context reset):

1. Read `PLAN.md` (goals, rules, task queue, log) and `docs/notes.md`
   (decisions, gotchas, environment facts, what's waiting on Ryan).
2. Run `git log --oneline -10` and `git status`. If there are uncommitted
   changes, finish and verify that work, or revert your own partial work,
   before starting anything new.

For each task:

1. Pick the first unchecked task in `PLAN.md` whose prerequisites are met
   and that isn't blocked.
2. If it's large, split it into smaller checkboxes in `PLAN.md` and do the
   first one. Each commit should leave the project building and working.
3. Implement it, then verify its "done" condition (see Verification).
4. Check the box and add a Log line:
   `- <task id> (YYYY-MM-DD): what now works; how it was verified.`
5. Add decisions, gotchas and device quirks to `docs/notes.md`, short and
   dated, so a fresh session can continue without this conversation.
6. Commit (see Commits). Then go straight to the next task. Don't stop to
   summarize or ask whether to continue.

## When something needs Ryan

- **Permission prompts, devices, credentials, taste calls:** try once. If
  blocked, add an entry under "Waiting on Ryan" in `docs/notes.md` (what's
  needed, which tasks it blocks, the exact command to retry), then continue
  with tasks that don't depend on it. Synthetic sources exist so most work
  can continue without hardware.
- **Design decisions:** make them yourself. Pick the option that best fits
  the goals, record the choice and the reason in `docs/notes.md`, and keep
  going. Ryan can overrule later.
- **Stop and wait only** for actions that are destructive or irreversible,
  touch things outside this repository (other than scratch output), spend
  money, or publish anything (pushing, releases, creating repos).
- When a milestone is reached, add a "Ready for Ryan" entry with the one
  command to try, and keep going.

## Ground rules

- **macOS first, portable core.** Platform code goes in
  `isocast-platform-macos` behind traits defined in portable crates.
- **One method table.** GUI, CLI and MCP are thin clients of the same engine
  API. Never add a GUI-only or CLI-only capability.
- **Ids, not names.** Address sources and scenes by stable ids.
- **Agent-friendly errors.** Validation errors name the field path and list
  the valid options. Every CLI command that prints data supports `--json`.
- **Never lose a recording.** Prefer crash-safe file writing, check disk
  space before starting, and surface dropped frames.
- **Small changes.** No unrelated refactors, new dependencies only when the
  task needs them, no speculative abstractions.

## Commands

```sh
cargo build
scripts/sign-dev.sh        # after every build of isocastd; see below
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
cargo run -p isocast-cli -- setup check examples/recording.toml
```

Recording runs in the daemon, `isocastd`, which the CLI launches from the
signed development app that `scripts/sign-dev.sh` builds
(`~/Library/Application Support/Isocast/Isocast Dev.app`). macOS grants
camera, microphone and screen recording to that app once, and the grants
survive rebuilds as long as the app is re-signed. After rebuilding, run the
script and `isocast restart`; the CLI warns when the app is older than the
build.

`fmt` must be clean on every commit, with `clippy` and `test` for the
crates changed. The full workspace `fmt`, `clippy` and `test` must be clean
before a branch is handed off for review or merged; CI runs them too.

Benchmarks and anything that opens a window run on the test Mac, never on
Ryan's: `scripts/testmac.sh build`, then `scripts/testmac.sh run '<command>'`
(over SSH) or `scripts/testmac.sh gui '<command>'` (in its desktop session).
See "Test Mac" in `docs/notes.md`.

## Verification

- Prove the task's claim directly: run the real command, open or `ffprobe` the
  real output file, check durations, frame rates, codecs and A/V alignment.
- Use `ffprobe -v error -show_entries stream=codec_name,width,height,r_frame_rate,pix_fmt -of compact <file>`
  to inspect recordings. Put scratch recordings in `target/scratch/`, never in
  Ryan's recordings folder.
- Don't create new test files, fixtures or harnesses without Ryan's
  approval. If one would clearly help, add the proposal under "Questions for
  Ryan" in `docs/notes.md` and keep going. Real runs are the default way to
  verify.
- After three failed attempts at the same problem, stop changing code for
  it, write down the most likely wrong assumption in `docs/notes.md`, mark
  the task blocked, and move on.

## Commits

- One focused commit per completed, verified task (or per coherent part).
  Commit often: a context reset should never lose more than one task.
- The message explains what changed, why, and what was verified. Call out
  removed or replaced behavior and why.
- Never commit recordings, credentials, or private logs/screenshots.
- Do not push, amend, or rewrite history unless Ryan asks.
