## Linked issue

<!-- Closes #123. Features, new dependencies, refactors and UI changes beyond a fix need an issue with the `accepted` label first. Small fixes and docs can skip this. -->

## What and why

<!-- What this changes, and the problem it solves. -->

## How it was verified

<!-- Commands you ran and what they printed: cargo test, the real isocast commands, ffprobe output for recordings, scripts/bench.py numbers before and after for performance. CI fails a pull request with this section empty. -->

## Screenshots

<!-- Required for any change to the app's interface (crates/isocast-ui, crates/isocast-app/ui): before and after, from scripts/ui-screenshots.sh. Drag the PNGs in here. CI fails a UI pull request without an image. -->

## Checklist

- [ ] `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test` pass.
- [ ] No OBS Studio source, shader, theme or UI files were read or copied (clean-room).
- [ ] New capabilities are on the API, the CLI (with `--json`) and MCP, not only in the app.
- [ ] I (or my agent) ran and verified this change, and the evidence is above.
