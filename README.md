# Isocast

Scene-based screen and camera recording, written in Rust, where every source
can also be saved as its own high-quality file (ISO recording), and where
everything is controllable from a CLI and by AI agents over MCP.

Status: version one in progress, macOS on Apple silicon only. A desktop
app and the `isocast` command drive one engine that runs scenes of
displays, windows, cameras (including capture cards and iPhone screens over
USB), microphones, system audio, pictures, media, text and web pages, with
transitions, filters, an audio mixer and studio mode; it records the
program and every ISO source on one clock (with a `session.json` manifest
that lines the files up), streams over RTMP/RTMPS, and imports OBS scene
collections. See [docs/getting-started.md](docs/getting-started.md) and
[PLAN.md](PLAN.md).

## Try it

```sh
cargo build --release
scripts/sign-dev.sh target/release/isocastd   # the engine, signed so permissions stick
scripts/app-dev.sh --release && open ~/Applications/Isocast.app

alias isocast=target/release/isocast
isocast setup init --apply                    # scenes from this Mac's devices
isocast start --seconds 10                    # record in the background
isocast status
isocast frame -o program.jpg                  # what the program shows now
```

Synthetic sources (`--source test-pattern --source tone`) need no
permissions. Every command that prints data takes `--json`, and
`isocast mcp` serves the same methods to AI agents.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT](LICENSE-MIT), at your option.
