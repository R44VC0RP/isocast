# Isocast on the Anomaly release service

Plan, 2026-11-02. Nothing here is implemented yet. It covers what moving the
repository to `anomalyco/isocast` broke, how to publish macOS and Linux
builds to the Anomaly release service (`anomalyco/release`,
`https://anoma.ly/release`) the way Hex does, and how that gives us daily
active users. Windows is out of scope for now.

The steps are ordered so each one ships on its own:

```ts
0  unblock releases on anomalyco/isocast      this repo, small PR          releases work again
1  register isocast with the release service   anomalyco/release PR         publishing is allowed
2  publish every release to the service        release.yml                  files + manifests on anoma.ly
3  poll the manifest from the app              update.rs (Mac + Linux)      DAU on the analytics page
4  replace Sparkle with the service's updater  update.rs, release scripts   one update path, Linux updates
5  (optional) sign as Anomaly, not Ryan        signing, camera extension    separate decision
```

## Today

- **Trigger:** pushing a `vX.Y.Z` tag (or dispatching Release from `main`
  with an existing tag). `preflight` checks the version, tag, `Cargo.toml`
  and `docs/releases/X.Y.Z.md` without secrets.
- **Build and sign:** one `macos-26` job in the `release` environment (Ryan
  is the required reviewer) builds, Developer ID signs (team `9G68SMNHEU`),
  notarizes and staples `Isocast-<v>-arm64.dmg`, then writes an
  EdDSA-signed Sparkle `appcast.xml`. Linux tarballs and `.deb`s
  (`isocast-linux-{x86_64,aarch64}.tar.gz`,
  `isocast-<v>-{debian12,ubuntu24.04}-{amd64,arm64}.deb`) come from
  `windows.yml` and are attached by `attach-linux`.
- **Publish:** everything goes into a GitHub **draft** release, and Ryan
  publishing the draft is what ships the update.
- **Updates:** macOS runs Sparkle about once a day against
  `SUFeedURL=https://github.com/R44VC0RP/isocast/releases/latest/download/appcast.xml`,
  baked in by `scripts/release.sh`. Linux has no updater: `update.rs`'s
  `other` module is a stub.
- **Analytics:** none. Sparkle talks only to GitHub.

## 0. Unblock releases on anomalyco/isocast

The repository now lives at `anomalyco/isocast`, but the release path still
names `R44VC0RP/isocast`:

- `.github/workflows/release.yml` (`preflight`) and
  `.github/workflows/refresh-appcast.yml` (`preflight`) only run when
  `github.repository == 'R44VC0RP/isocast'`, so **every release run now skips
  everything**. Change both guards to `anomalyco/isocast`.
- The feed URL baked into the app: `scripts/release.sh` (`feed_url=`),
  `crates/isocast-ui/src/areas/developer.rs` and
  `crates/isocast-ui/src/update_windows.rs`. Point new builds at
  `https://github.com/anomalyco/isocast/releases/latest/download/appcast.xml`.
- `REPO` in `scripts/release-finalize.py`, which creates the draft and
  uploads its assets through the GitHub API. API writes to a transferred
  repository's old name aren't reliably redirected, so don't count on that.
- Repository defaults in the tooling: `scripts/ci.sh` (`repo=`),
  `scripts/pr_flow.py` (`REPO`), `ISOCAST_PUBLIC_REPO` in
  `scripts/publish-snapshot.sh`, `scripts/tests/train_sim.py`, and the
  Windows feed and publisher URL in `scripts/windows/appcast.ps1` and
  `scripts/windows/installer.iss`.
- Links: `Cargo.toml` `repository`, `crates/isocast-ui/src/areas/report.rs`
  (bug reports), `README.md`, `docs/getting-started.md`, `docs/install.md`,
  `docs/releasing.md`, `docs/STATE.md`, `.github/CONTRIBUTING.md`,
  `.github/SECURITY.md`, `.github/SAVED_REPLIES.md` and
  `.github/ISSUE_TEMPLATE/config.yml`.
- `git grep R44VC0RP/isocast` finds them all. The ones in `changelog/` and
  `docs/archive/` are history and can stay.

Installed copies (0.0.2 to 0.0.10) still poll the old URL. They only keep
working through GitHub's redirect for transferred repositories, so **never
create a new repository named `R44VC0RP/isocast`**: that would replace the
redirect and strand every installed copy on its current version.

The `release` environment came across with the transfer (Ryan as required
reviewer, branch policy). Confirm the nine secrets did too:
`gh secret list --repo anomalyco/isocast --env release` should list
`DEVELOPER_ID_P12_BASE64`, `DEVELOPER_ID_P12_PASSWORD`,
`APPLE_API_KEY_P8_BASE64`, `APPLE_API_KEY_ID`, `APPLE_API_ISSUER_ID`,
`APPLE_TEAM_ID`, `ENGINE_PROVISIONING_PROFILE_B64`,
`CAMERA_PROVISIONING_PROFILE_B64` and `SPARKLE_ED_PRIVATE_KEY`.

## 1. Register isocast with the release service

The service authorizes publishing with the workflow's GitHub OIDC token,
checked against `registry/<app>.json` in `anomalyco/release`; there are no
keys to store. Add `registry/isocast.json`, list it in `registry/index.ts`,
and have a release-service maintainer (Dax or Kit) merge and deploy it:

```json
{
  "app": "isocast",
  "description": "Isocast, the open-source recorder (anomalyco/isocast). Its Release workflow publishes stable from a v* tag in environment release (Ryan approves), and dev from every push to main in environment release-dev.",
  "stages": ["production"],
  "artifacts": {
    "desktop": { "channels": ["stable", "beta", "dev"] }
  },
  "publishers": [
    {
      "repository": "anomalyco/isocast",
      "repository_id": "1417989450",
      "repository_owner_id": "66570915",
      "workflow": ".github/workflows/release.yml",
      "refs": ["refs/tags/v*", "refs/heads/main"],
      "environment": "release",
      "artifacts": ["desktop"],
      "channels": ["stable", "beta"]
    },
    {
      "repository": "anomalyco/isocast",
      "repository_id": "1417989450",
      "repository_owner_id": "66570915",
      "workflow": ".github/workflows/release.yml",
      "refs": ["refs/heads/main"],
      "environment": "release-dev",
      "artifacts": ["desktop"],
      "channels": ["dev"]
    }
  ]
}
```

- `refs/heads/main` is on the stable publisher because a manual dispatch runs
  from `main` (it still builds the requested tag).
- The dev channel is optional. It needs its own environment (`release-dev`,
  no reviewer, limited to `main`) because `release` asks Ryan to approve every
  run. Leave that publisher out until dev builds are wanted.
- Rehearse on the production service's `dev` channel.
  `https://dev.anoma.ly/release` only tests the service itself.

## 2. Publish every release to the service

Add a `publish` job to `release.yml`, after the macOS release job and
`linux-packages`:

- It runs in `release` (or `release-dev`) with `permissions: id-token: write`
  and `contents: read`. It **only downloads the finished files and runs the
  client**: it never builds, installs dependencies or runs repository code,
  because any code in a job with `id-token: write` can mint a publish token.
  It sparse-checks out the release scripts only, as Hex's jobs do.
- It uses the service's client, vendored as `.github/release/release-client.sh`
  (copy `client/release.sh` from `anomalyco/release`, as Hex does), so nothing
  fetched at run time executes:

  ```sh
  bash .github/release/release-client.sh publish --app isocast --artifact desktop \
    --release release.json --out manifest.json --verify-downloads \
    --target macos-arm64=dist/Isocast-<v>-arm64.zip=Isocast.zip \
    --target macos-arm64-dmg=dist/Isocast-<v>-arm64.dmg=Isocast.dmg \
    --target linux-x64=dist/isocast-linux-x86_64.tar.gz=isocast-linux-x64.tar.gz \
    --target linux-arm64=dist/isocast-linux-aarch64.tar.gz=isocast-linux-arm64.tar.gz \
    --target linux-x64-deb=dist/isocast-<v>-ubuntu24.04-amd64.deb=isocast.deb \
    --target linux-arm64-deb=dist/isocast-<v>-ubuntu24.04-arm64.deb=isocast.deb
  ```

  `release.json` is `{"channel": "stable", "version": "<v>", "notes_url":
  "https://github.com/anomalyco/isocast/releases/tag/v<v>", "distributions":
  [{"kind": "direct"}]}`. Dev builds use `0.0.0-dev-<run number>`; versions are
  display text and never compared. Published names allow no spaces, and the
  sha256 in each file's URL keeps them apart. The Debian 12 `.deb`s need
  their own targets (for example `linux-x64-deb_debian12`) if they stay
  separate from the Ubuntu ones.
- **macOS needs a ZIP of the stapled app** (ditto, root `Isocast.app`), not
  only the DMG: that is what an in-app updater downloads (step 4).
  `release.sh` already makes the stapled app, so this is one extra
  `ditto -c -k --keepParent` after notarization.
- **When to publish:** today a draft ships nothing until Ryan publishes it.
  To keep that gate, publish to the service from a separate dispatch (or a
  `release: published` trigger) that downloads the published GitHub assets
  and checks their digests. The simpler option, which Hex uses, is to
  publish in the same run once the environment approval passes.
- Keep the GitHub release and `appcast.xml` until step 4 is done.
- Stable download links then exist for the site and README, for example
  `https://anoma.ly/release/isocast/desktop/stable/macos-arm64-dmg` (302 to
  the current file).

Recommended while in there (Hex's pattern, not required): split building from
signing so repository code never runs in a job that holds secrets, with a
`SHA256SUMS` digest checked between jobs (`.github/release/verify-artifacts.sh`
in Hex).

## 3. DAU: poll the manifest from the app

The service's analytics count **update checks**. Every
`GET https://anoma.ly/release/isocast/desktop/<channel>.json` is logged
(`type = 'check'`, with IP, User-Agent and Cloudflare country) to the
production R2 lake (`default.event`, and the typed `release.request_v1`). A
user is a distinct IP that checked. Once the app is in the registry and
copies start polling, it appears automatically on
`https://anoma.ly/release/admin/?app=isocast` (Cloudflare Access): users per
day, week and month, by channel, version and country.

What the app has to do (`docs/protocol.md` in `anomalyco/release`):

- Send `User-Agent: isocast/<channel>/<version>`, where the channel is the
  one the build was published on, stamped at build time (Hex uses
  `HEX_BUILD_CHANNEL`). Add a fourth segment for a different tool, for example
  `isocast/stable/0.0.11/cli` if the CLI ever checks. Send nothing else that
  identifies the install: no query string, no install id.
- Check at launch and then **at least once an hour**, failures included
  (back off, but never past an hour), with `If-None-Match`. Sparkle's daily
  check is not enough for the protocol, and it never reaches the service
  anyway.

So DAU arrives as soon as the app polls the manifest, before the updater is
replaced. The quick version is a small poller in `update.rs` for macOS and
Linux that fetches the manifest on that schedule and, for now, only reports
"Isocast <v> is available" with a link to the DMG or `.deb`. Sparkle keeps
installing macOS updates until step 4.

Caveats: an IP is not a person (NAT, VPNs and offices undercount; a laptop
that moves networks overcounts). Copies released before the poller never
show up. Turning off update checks in Settings should also stop the poll,
which hides those users.

OpenCode Agent posts a daily Hex DAU chart in #abattoir from the same data
(an R2 SQL query on `default.event` with `app = 'hex'`, split by channel).
Ask it to post one for Isocast once checks arrive. That job lives outside
this repository.

## 4. Replace Sparkle with the service's updater

Once releases are on the service, one updater can serve macOS and Linux:

- **Client:** port Hex's `src/updater.rs` and `updater/protocol.rs`. The
  protocol's client rules must pass the service's `test-vectors/` (Hex
  copies them into `tests/fixtures/release-protocol`). Installs follow the
  channel's current release: anything that is not the running build is an
  update, so rollback is just publishing an older build.
- **Integrity:** manifests are unsigned. Files are checked against the
  manifest's size and sha256 over HTTPS. Beyond that:
  - macOS: before swapping the bundle, require a code-signing requirement
    pinned in the app (Hex: `app/update-requirement.txt`, anchor apple
    generic, identifier, Developer ID team, notarized). For Isocast that is
    `dev.isocast.app` and its team. This replaces the Sparkle EdDSA key as
    the root of trust.
  - Linux: an ed25519 signature over each file's sha256, published as a
    `linux-<arch>-sig` target and checked against a public key built into
    the app (Hex: `sign-linux.sh`, `HEX_RELEASE_SIGNING_KEY` in the release
    environment).
- **What Isocast must keep:** the current `update.rs` defers while
  recording, stops and recovers the engine helper, and finishes an update
  after **Keep Recording**. The new updater has to keep all of that.
- **Linux installs:** self-update only tarball installs from
  `packaging/linux/install.sh`, which then writes an `install.json` receipt
  (Hex: `~/.local/share/hex/install.json`, owned by the user). `.deb`
  installs show the download or apt command instead. Record package managers
  as `distributions` so the app can show the right command.
- **Moving Sparkle users across:** ship the new updater in a normal Sparkle
  release first, so every copy that updates lands on the new path. After
  that, keep putting an `appcast.xml` on every GitHub release that is
  "latest", pointing at the newest DMG: the feed is
  `releases/latest/download/appcast.xml`, so a latest release without it is
  a 404 for anyone who hasn't updated yet. Retire it once the admin page
  shows no old versions checking.

## 5. Optional: sign as Anomaly instead of Ryan's team

Hex moved its Developer ID from a personal team to Anomaly's
(`5NZ4Q7NXJ4`). Isocast is signed by Ryan's team `9G68SMNHEU`. The release
service doesn't need this, but if it moves:

- The team is hard-coded in `camera-extension/Camera.swift`
  (`producerRule`), `camera-extension/project.yml`,
  `scripts/build-camera-extension.sh`, `scripts/vcam-common.sh`,
  `scripts/vcam-package.py`, `scripts/release.sh`,
  `scripts/release-finalize.py`, and the team-prefixed app group and Mach
  service (`9G68SMNHEU.dev.isocast.isocastd[.camera]`,
  `docs/virtual-camera.md`).
- Both Developer ID provisioning profiles (engine with System Extension, and
  camera) must be regenerated under the new team, and the bundle ids
  registered there.
- Users re-grant Screen Recording, Camera and Microphone because the
  designated requirement changes. A system extension from a different team
  is a different extension to macOS, so the old camera extension probably
  has to be deactivated. Test that on a real Mac before shipping.
- Do it **while Sparkle still installs updates**: Sparkle accepts a new
  signing team when the EdDSA signature is valid (`docs/releasing.md`,
  step 5). An updater that pins a team (step 4) would need to accept both
  teams for one release instead.
- Secrets become Anomaly's (Hex's names: `APPLE_SIGNING_CERTIFICATE`,
  `APPLE_NOTARY_KEY`, variables `APPLE_NOTARY_KEY_ID` and
  `APPLE_NOTARY_ISSUER_ID`), re-entered from 1Password by an admin.

## Who does what

- **Ryan:** merge step 0 and confirm the secrets; decide on a dev channel
  (and `release-dev`), whether publishing stays gated on publishing the
  draft, and step 5.
- **Dax or Kit:** review and deploy `registry/isocast.json` in
  `anomalyco/release`; they are also the only `mutate` admins for rollout,
  halt and promote through the admin API.
- **Lanes:** steps 2 to 4 as ordinary PRs. Step 4 needs a Mac and a Linux
  machine to qualify the update paths.

## References

- `anomalyco/release`: `docs/publishing.md`, `docs/protocol.md`,
  `docs/operations.md` ("Analytics"), `registry/hex.json`,
  `client/release.sh`.
- `anomalyco/hex`: `.github/workflows/release.yml`, `.github/release/`,
  `docs/releasing.md`, `docs/plans/release-service.md`, `src/updater.rs`.
