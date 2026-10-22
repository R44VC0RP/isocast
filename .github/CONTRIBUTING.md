# Contributing to Isocast

Thanks for helping. We want contributions that are easy to review and
clearly work, so this page is short, and its rules are firm. CI checks most
of them for you.

## Coding agents are encouraged

Isocast is built by coding agents, and we expect most contributions to be
written with one. Please do.

- **Use the latest state-of-the-art model, at its highest reasoning
  setting.** Older or smaller models make more mistakes. Agents are very
  good at this work when they're the best available.
- **Point your agent at [AGENTS.md](../AGENTS.md).** It holds the rules
  every agent working on Isocast follows: how to build, how to verify, and
  what never to do.
- **The evidence is the agent's job.** Have it run the real commands, take
  the screenshots, read the `ffprobe` output and measure before and after,
  then paste the results into the pull request.
- **You are responsible for what you submit.** Read it, and make sure the
  evidence is real. Pull requests with unverified machine output are
  closed.
- **No attribution needed.** Don't add AI co-author trailers or "generated
  with" lines to commits.

## What to send, and where

- **Small bug fixes and docs:** open a pull request directly.
- **Anything bigger** (features, new dependencies, refactors, UI changes
  beyond a fix): open an issue first and wait for a maintainer to add the
  `accepted` label. Bigger pull requests without an accepted issue are
  closed, so nobody's work goes to waste on something that won't land.
- **Questions and ideas:** use
  [Discussions](https://github.com/R44VC0RP/isocast/discussions).
- **Security problems:** report them privately, see
  [SECURITY.md](SECURITY.md).

## Evidence is required

Every pull request fills in the template's **How it was verified** section
with the commands you ran and what they printed. CI fails a pull request
when that section is empty.

- **UI changes** (`crates/isocast-ui`, `crates/isocast-app/ui`) need before
  and after screenshots in the description. CI fails a UI pull request that
  has no image. `scripts/ui-screenshots.sh` makes a consistent set in
  about a minute (see below).
- **Recording and streaming changes** need the real `isocast` commands you
  ran and the `ffprobe` output of the files: codec, size, frame rate,
  duration, and audio and video in step.
- **Performance claims** need `scripts/bench.py` numbers before and after,
  from alternating runs on the same Mac.
- **Everything** passes `cargo fmt`, `cargo clippy --all-targets -- -D
  warnings` and `cargo test`.

## Rules of the project

- **Clean-room.** Never read or copy OBS Studio source code, or its shader,
  theme or UI files. OBS is GPL and Isocast is MIT OR Apache-2.0, so
  copying would change that. OBS's public documentation is fine, as is
  testing how OBS behaves.
- **One method table.** The app, the CLI and MCP are thin clients of one
  engine API. A new capability goes on the API, the CLI (with `--json`) and
  MCP, not only in the app.
- **Ids, not names.** Sources, scenes and clips are addressed by stable ids.
- **Agent-friendly errors.** An error names the field path and lists the
  valid options.
- **Never lose a recording.** Crash-safe writing, a disk-space check before
  starting, and dropped frames reported, never hidden.

The full rules are in [AGENTS.md](../AGENTS.md).

## Build and check

You need a Mac with Apple silicon, macOS 14 or later, Rust stable and the
Xcode command line tools.

```sh
cargo build --release
scripts/sign-dev.sh target/release/isocastd   # the engine, as a signed dev app
scripts/app-dev.sh --release                  # the app, as ~/Applications/Isocast.app

cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

The CLI starts the engine from the signed dev app that `sign-dev.sh` makes.
macOS grants it screen recording, camera and microphone access once, and the
grants survive rebuilds as long as you sign again. After rebuilding the
engine, run `scripts/sign-dev.sh target/release/isocastd` and then
`target/release/isocast restart`.

To test without touching your real setup, give the engine a folder of its
own: `ISOCAST_SOCKET=/tmp/try/engine.sock`. Its setup and plugins then live
beside that socket. Synthetic sources need no permissions:
`isocast start --source test-pattern --source tone --seconds 5`.

If you change the CLI, the setup format or the MCP tools, regenerate the
reference docs. If you change dependencies, regenerate the notices:

```sh
cargo build --release -p isocast-cli && scripts/reference-docs.py
scripts/third-party-notices.sh
```

## Screenshots for UI changes

```sh
scripts/ui-screenshots.sh before/    # on main
scripts/ui-screenshots.sh after/     # on your branch
```

This builds the app and draws its standard views offscreen against a
throwaway engine with synthetic sources, as 1440x900 windows at 2x: the
main window, Studio Mode, Properties, Settings and the audio mixer's menu.
Nothing opens on screen, and your own setup isn't touched. The video area
shows a stand-in test picture, so judge the interface around it. Drag the
PNGs you need into the pull request.

## Visual checks

Pull requests that touch the interface also get an automatic visual check.
`.github/workflows/visual.yml` builds your branch and its merge base,
draws every standard view at 1440x900 (2x), 1920x1080 (1x) and 900x600
(2x) with `scripts/visual-shots.sh`, and compares them pixel by pixel. A
bot comment then shows before, after and a diff (changed pixels in
magenta) for each view that changed. A judge agent (Claude Sonnet 5.5,
running in OpenCode with read-only access to the images and your interface
diff, and nothing else) reviews each changed view for clipped text,
overlap, missing controls, misalignment and broken colours. Its verdict
sets the `isocast/visual-check` status: success when nothing changed or
every change looks intended, failure on a likely regression, error when
the check couldn't run. The judge is a reviewer, not the last word: if it
flags something you did on purpose, say so in the pull request.

Maintainers: make the check required under Settings → Branches → `main` →
Require status checks to pass → `isocast/visual-check` (it appears in the
list after it has reported once).

## How pull requests land

Until the alpha ends, Isocast's history is published as snapshots. An
accepted pull request is applied to the upstream branch with you kept as
the author. It appears in the next snapshot commit, and the pull request is
closed with a link to it.

## More

- [docs/architecture.md](../docs/architecture.md): how the engine and its
  clients fit together.
- [PLAN.md](../PLAN.md): the roadmap; the work itself is in GitHub issues.
- [docs/STATE.md](../docs/STATE.md): current facts, machines and gotchas.
  Older decisions and measurements are archived in `docs/archive/`.

By contributing, you agree that your contribution is licensed under MIT OR
Apache-2.0, like the rest of Isocast.
