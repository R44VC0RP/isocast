# Install Isocast on macOS

Isocast requires an Apple-silicon Mac and macOS 14 or later.

1. Open the signed Isocast DMG and drag **Isocast** onto **Applications**.
2. Eject the DMG and open **Isocast** from Applications. macOS may ask you
   to confirm opening an app downloaded from the internet.
3. Follow the welcome screen to choose your sources. **Isocast Engine**
   requests camera, microphone and Screen & System Audio Recording access
   when you choose to grant it. Allow only the inputs you want to use.
   Screen-recording access may require restarting the engine (the app
   offers this; the CLI command is `isocast restart`).

The engine ships inside the app. No Rust, Homebrew, developer tools, or
separate daemon installation is needed. Permission status and next steps
are also available with `isocast permissions --json`. Synthetic sources
work without capture permissions.

## Command line

The command is `/Applications/Isocast.app/Contents/MacOS/isocast`. To put
it on your PATH without administrator access:

```sh
mkdir -p "$HOME/.local/bin"
ln -s /Applications/Isocast.app/Contents/MacOS/isocast "$HOME/.local/bin/isocast"
export PATH="$HOME/.local/bin:$PATH"
isocast --version
```

Add the `export PATH=…` line to `~/.zshrc` to keep it for new terminals.
The symlink command refuses to replace an existing command; inspect an
existing installation before changing it. Keep the app in Applications:
the symlink follows app updates and the CLI launches its bundled engine.

Run `isocast --help` for commands, `isocast mcp` for the MCP server. Normal
app, CLI and MCP sessions share the same engine and setup.

Isocast updates itself (from 0.0.2 on): it checks once a day and asks
before installing; **Isocast › Check for Updates…** checks now, and
Settings › General › Updates turns the daily check off. An update never
installs while isocastd records (or is paused), streams, exports or feeds
the virtual camera: it waits, then stops the engine and relaunches the
app on the new version. If you quit with **Keep Recording** while an update
is downloaded, it installs as the app closes and the engine finishes on
the old version; the next launch restarts it once it is idle.
`isocast status` shows which engine runs (`Engine: isocastd 0.0.3 (build …)`).

## Delete Isocast

Isocast Virtual Camera is a camera extension that stays in macOS when the
app goes to the Trash (macOS keeps listing it until it's removed). If you
started the virtual camera, remove it first:

1. In Isocast, open the gear beside **Virtual Camera** in the Controls dock
   and click **Remove Virtual Camera…** (or run `isocast vcam remove`).
2. Enter an administrator password when macOS asks.
3. Quit Isocast and move it to the Trash.

If macOS says a restart finishes the removal, keep Isocast until you have
restarted. Starting the virtual camera again installs it again. Your
recordings, setups and settings are not removed; they live in your
recordings folder and `~/Library/Application Support/Isocast`.

## Release maintainers

`scripts/release.sh --dry-run` prints the local build/sign/DMG workflow
(until the Sparkle public key is committed it says it skips that check;
real Developer ID builds stop on it, docs/releasing.md step 5).
Run without that flag to prepare a Developer ID-signed app and DMG. This
step requires the signing identity but no notarization credential or upload.
Gatekeeper should report **Unnotarized Developer ID** until finalization.
Use `--ad-hoc` for local packaging tests (not eligible for notarization/drafts).
Use a new `--output DIRECTORY` for each build; artifacts are never overwritten.
Version comes from `[workspace.package].version` in `Cargo.toml`.
Each build also stamps one numeric `CFBundleVersion` into the app, helper
and embedded daemon metadata (`ISOCAST_BUILD_VERSION`, else the current
Unix timestamp), independently of that product version.

Signing uses the login keychain's Developer ID Application identity and the
existing `isocast-notary` notarytool profile. First use of the signing key
may prompt its owner; approve it yourself, then retry if the 90-second
signing timeout was reached. Passwords are never read or written by the script.

Only the engine and CLI get hardened-runtime exceptions:

- `device.camera`: AVFoundation camera/capture-card recording.
- `device.audio-input`: microphone and audio-interface recording.
- `cs.allow-unsigned-executable-memory`: Wasmtime 49's compiled plugin code
  uses `mmap` then `mprotect(RX)`. The narrower `cs.allow-jit` applies to
  `MAP_JIT`, which this Wasmtime version does not use. It is not sufficient
  here, so it is not added. There is no library-validation bypass, debugger
  entitlement, sandbox, or separate screen-capture entitlement.

The UI has hardened runtime without exceptions. Camera/microphone usage
strings and screen-capture guidance are included in both app plists. The
production UI has ID `dev.isocast.release`. The helper retains the engine's
canonical `dev.isocast.isocastd` ID so the virtual-camera extension recognizes
its signed producer. Engine privacy grants may carry over from a development
build signed by the same Apple team.

The engine helper carries the virtual camera's system extension; the first
**Start Virtual Camera** asks macOS to activate it, and **Remove Virtual
Camera…** deactivates it (see `docs/virtual-camera.md`).

The native Slint UI is the default, controlled by the one `app_target`
assignment in `scripts/release.sh`. `ISOCAST_APP_TARGET=isocast-app` opts
into the retired web UI explicitly, matching `scripts/app-dev.sh`.
The three executables retain distinct names even on case-insensitive disks.

Once per signing Mac, the owner restores the notarytool profile interactively
(notarytool prompts for Apple ID and an app-specific password; do not put
the password in a command or file):

```sh
xcrun notarytool store-credentials isocast-notary --team-id 9G68SMNHEU \
  --keychain "$HOME/Library/Keychains/login.keychain-db"
```

After verifying the prepared app/DMG on a test Mac, finish those same binary
bytes and create the draft with one command:

```sh
scripts/release.sh --notarize --draft-release --output /path/to/verified/output
```

`--notarize` copies the prepared app into `OUTPUT/notarized/`, notarizes and
staples it, repacks the DMG containing that stapled app, and signs/notarizes/
staples the DMG. It never recompiles or re-signs the executables. The saved
product version, UI target and executable/plist hashes pin the build even
if the checkout changes. Strict signatures, both tickets, Gatekeeper and
the app mounted from the final DMG are rechecked. The final SHA-256 is
computed **after** stapling. Prepared originals remain in OUTPUT.

Submission ids and archive hashes are saved before waiting. Wait calls are
bounded to 60 seconds with progress every 20 seconds; a pending submission
resumes on the same command. If upload ended without an id, inspect the
notarytool history before removing the named `.submitting` marker and retrying;
the script refuses a blind duplicate upload. An interrupted signing step can
also resume. A stale `.release-lock` after a hard kill requires checking that
the previous process is gone before removing it.

`--draft-release` uploads only verified notarized output and its checksum
to a **draft** on `R44VC0RP/isocast`, with a short user-facing alpha summary
and that repo's main as its tag target. It reuses a matching draft and verifies both GitHub asset digests;
published releases or mismatched assets are refused. No git history is
pushed, release published, or repository setting changed. The draft URL is
saved in `OUTPUT/draft.json`. Use `--notarize` alone to stop before upload,
or `--draft-release` alone to resume after notarization. Both support
`--dry-run` against a prepared output directory.

Distribute `OUTPUT/notarized/Isocast-X.Y.Z-arm64.dmg` and its `.sha256` file.
Verify a download with `shasum -a 256 -c Isocast-X.Y.Z-arm64.dmg.sha256`
beside the DMG. App and DMG accepted receipts remain in `OUTPUT/notarized/`.

For GitHub Actions, API-key notarization, temporary signing keychains and
the protected release environment, see [Releasing Isocast](releasing.md).
The CI-specific runtime, generated-notes and draft-replacement options are
explicit; the local defaults above are unchanged.
