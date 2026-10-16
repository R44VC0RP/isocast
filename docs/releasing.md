# Releasing Isocast

The **Release** workflow builds an existing `vX.Y.Z` tag on GitHub-hosted
`macos-15` (arm64), signs with Developer ID, notarizes/staples the app and
DMG, runs the mounted CLI/engine recording checks, and creates **one draft**
with the DMG, its checksum and Sparkle's `appcast.xml` (the self-update
feed, EdDSA-signed). Rerunning a version replaces that draft's assets and
generated notes. Published releases are refused. Only Ryan publishes a
draft, and publishing is what ships the update: installed copies read
`releases/latest/download/appcast.xml`, which only a published, non-
prerelease release serves.

No pull requests or self-hosted machines run this job. Actions are pinned
to commit SHAs. The job has `contents: write`; workflow defaults are read-only.
It waits at the **release** environment before receiving its secrets. A
workflow-wide concurrency group serializes tag pushes and manual reruns.
The DMG's app carries the virtual camera's CMIO extension
(`dev.isocast.isocastd.camera`) inside its engine helper, which activates it
on the first Start Virtual Camera; that needs two Developer ID provisioning
profiles (step 4a), and no build is made without them. The engine helper is
`Isocast.app/Contents/Helpers/Isocast Engine.app` (bundle id
`dev.isocast.isocastd`), and its executable is `Contents/MacOS/Isocast Engine`
(Cargo's `isocastd`, renamed when packaged; #83): macOS names that file in the
camera extension's approval alert. Releases before #83 called it `isocastd`;
the clients and the extension accept both.

A small **preflight** job runs first, without secrets or approval: it
checks the version, that the tag exists, that Cargo.toml at the tag matches,
and whether that version is already published. A published version ends
the run green with "nothing to build or draft" and never asks for an
approval; a missing tag or a mismatch fails there with the reason.

## One-time setup (Ryan)

### 1. Publish the workflow and protect its environment

Merge the release branch into the private working repository's main and use
`scripts/publish-snapshot.sh` to send a clean snapshot to
`R44VC0RP/isocast`, as usual. **Never push this checkout's history.** The tag
you release must include this workflow and its release scripts.

In GitHub: **R44VC0RP/isocast → Settings → Environments → New environment**:

- Name it **release**.
- Under required reviewers, select **Ryan's GitHub user** and save.
- Leave **Prevent self-review** off: Ryan is the only reviewer and must be
  able to approve a tag/dispatch he started himself. Keep administrator
  bypass off where that control is available.
- Limit deployment branches/tags to branch **main** and tags **v***. Manual
  dispatch is allowed only from main; it still builds the requested tag.

Configure the reviewer **before adding secrets**. Referencing an environment
in YAML alone does not configure its protection rules. Done on 2026-10-14
(reviewer R44VC0RP, branch main and tags v*); steps 2–6 remain.

### 2. Export the Developer ID certificate

First `mkdir -m 700 -p ~/.config/isocast-release-secrets`. In **Keychain
Access → login → My Certificates**, select the row
**Developer ID Application: Ryan Vogel (9G68SMNHEU)** (the certificate
itself, which carries its private key and chain), then **File → Export
Items…**, format **Personal Information Exchange (.p12)**. Save it as
`~/.config/isocast-release-secrets/developer-id.p12` (⇧⌘G in the save
dialog reaches the hidden folder) and give it a non-empty export password;
macOS then asks for the login keychain password to allow the export. A
`.cer` alone cannot sign. Keep the export password in your password
manager; never put it in a command line.

### 3. Create an App Store Connect API key

In **App Store Connect → Users and Access → Integrations → App Store
Connect API → Team Keys** (`https://appstoreconnect.apple.com/access/integrations/api`),
generate a key named `isocast-release` with the **Developer** role. An
Account Holder/Admin may first need to enable API access. Download its
`.p8` private key (offered once) to
`~/.config/isocast-release-secrets/AuthKey.p8`, and note the **Key ID**
(10 characters, on the key's row) and the **Issuer ID** (the UUID at the
top of the page). Use a **team** key: this workflow passes both `--key-id`
and `--issuer`. It does not need an Apple ID password or local notarytool
profile. Team ID is **9G68SMNHEU**.

### 4. Add the six environment secrets

Private folder: `$HOME/.config/isocast-release-secrets` (directory mode
700, files 600). The two files are encoded inside a subshell (so the
`umask` does not outlive the block) and read from disk. The three `gh
secret set` lines without a file show gh's hidden **Paste your secret:**
prompt; paste, in that order, the `.p12` export password, the Key ID and
the Issuer ID, so they never exist as files or in shell history. The last
command must list all six names. Run with shell tracing off; nothing here
prints a secret (`gh secret set` also trims a trailing newline from piped
input). The command blocks in this document have no comments and are
meant to be pasted whole into an interactive zsh with default options.

```sh
secrets="$HOME/.config/isocast-release-secrets"
( umask 077 && chmod 700 "$secrets" &&
  chmod 600 "$secrets/developer-id.p12" "$secrets/AuthKey.p8" &&
  base64 < "$secrets/developer-id.p12" | tr -d '\n' > "$secrets/p12.base64" &&
  base64 < "$secrets/AuthKey.p8" | tr -d '\n' > "$secrets/p8.base64" )
gh secret set DEVELOPER_ID_P12_BASE64 --repo R44VC0RP/isocast --env release < "$secrets/p12.base64"
gh secret set DEVELOPER_ID_P12_PASSWORD --repo R44VC0RP/isocast --env release
gh secret set APPLE_API_KEY_P8_BASE64 --repo R44VC0RP/isocast --env release < "$secrets/p8.base64"
gh secret set APPLE_API_KEY_ID --repo R44VC0RP/isocast --env release
gh secret set APPLE_API_ISSUER_ID --repo R44VC0RP/isocast --env release
gh secret set APPLE_TEAM_ID --repo R44VC0RP/isocast --env release --body 9G68SMNHEU
gh secret list --repo R44VC0RP/isocast --env release
rm "$secrets/p12.base64" "$secrets/p8.base64"
```

Then clean the folder: the two `.base64` files are already gone. Store
`developer-id.p12` (with its export password) and `AuthKey.p8` (with its
Key ID and Issuer ID; Apple offers the `.p8` for download only once) in
1Password, then delete the certificate export with
`rm "$HOME/.config/isocast-release-secrets/developer-id.p12"`; the login
keychain still holds the identity. Delete `AuthKey.p8` the same way,
unless you intend to take step 7's API-key alternative: then leave it at
`$HOME/.config/isocast-release-secrets/AuthKey.p8` (mode 600) until that
step has run; `notarytool store-credentials` copies the key's contents
into the keychain item, after which the file can go. Do not upload any of
them as workflow artifacts or commit them. The workflow decodes the
secrets only under RUNNER_TEMP with restrictive permissions, masks its
random keychain password/identity, and imports into a temporary keychain.
`always()` restores the original search list, deletes that keychain, and
removes the certificate/API-key files, including after failures.
Credentials are not placed in Cargo/Homebrew caches or passed to the
recording daemon. Rotate/revoke the certificate and API key in Apple's
portals and replace the environment secrets when necessary.

### 4a. The virtual camera's two provisioning profiles

Make them as the guide in `~/dev/isocast-status/guides/virtual-camera-profiles.html`
says (Developer ID profiles, Distribution → Developer ID), choosing **the
Developer ID Application certificate releases are signed with**, not the
newest one in the account. Identify it by serial number from a signed
release (the portal lists certificates by expiry date and serial):

```sh
d="$(mktemp -d)" && codesign -d --extract-certificates="$d/c" /Applications/Isocast.app &&
  openssl x509 -inform DER -in "$d/c0" -noout -subject -serial -enddate && rm -r "$d"
```

Today that is serial `05D0C582E2F1E2D8`, expiring 1 Feb 2027 (the
login keychain's "Developer ID Application: Ryan Vogel (9G68SMNHEU)").
A profile's own expiry (2044) is not the limit: the portal only lets you
download it until its certificate expires, so **renewing or replacing the
signing certificate means regenerating both profiles for the new one and
replacing both secrets** (`release-profiles --identity` refuses profiles
without the signing certificate). The profiles: **Isocast Engine Developer ID** for
`dev.isocast.isocastd` with the **System Extension** capability, and
**Isocast Virtual Camera Developer ID** for `dev.isocast.isocastd.camera`
(no capabilities; the Team-ID-prefixed app group
`9G68SMNHEU.dev.isocast.isocastd` needs no registration). Then keep them
where local builds look and add them as two more environment secrets:

```sh
secrets="$HOME/.config/isocast-release-secrets"
( umask 077 &&
  mv "$HOME/Downloads/Isocast_Engine_Developer_ID.provisionprofile" "$secrets/isocast-engine.provisionprofile" &&
  mv "$HOME/Downloads/Isocast_Virtual_Camera_Developer_ID.provisionprofile" "$secrets/isocast-camera.provisionprofile" &&
  chmod 600 "$secrets"/isocast-*.provisionprofile )
python3 scripts/vcam-package.py release-profiles "$secrets/isocast-engine.provisionprofile" "$secrets/isocast-camera.provisionprofile" --identity "Developer ID Application: Ryan Vogel (9G68SMNHEU)"
base64 < "$secrets/isocast-engine.provisionprofile" | tr -d '\n' | gh secret set ENGINE_PROVISIONING_PROFILE_B64 --repo R44VC0RP/isocast --env release
base64 < "$secrets/isocast-camera.provisionprofile" | tr -d '\n' | gh secret set CAMERA_PROVISIONING_PROFILE_B64 --repo R44VC0RP/isocast --env release
```

(Use the downloaded files' actual names in the two `mv` lines.) The check
line must print two "Provisioning profile checked" lines: Developer ID
(no device list), team 9G68SMNHEU, the exact app id, the System Extension
capability on the engine's, at least 30 days left, and your signing
certificate. Local builds read the two files from that folder (or
`ISOCAST_ENGINE_PROFILE` / `ISOCAST_CAMERA_PROFILE`); the workflow decodes
the secrets into its temporary signing folder and stops before building
while either is missing. `release.sh` checks them before the build and
again after signing (each bundle's embedded profile against its signature
and signed entitlements), and refuses the build otherwise. Profiles aren't
secret, but keep them only there and in 1Password. Done on 2026-10-15
(both secrets set, profiles tied to the 2027 certificate).

### 5. Sparkle update key

Updates are signed with an EdDSA key that only you hold: its public half is
built into every app, its private half signs each release's DMG and
appcast. **That private key is the sole root of trust for installed
copies**: Sparkle installs any archive with a valid EdDSA signature and
only checks that the app inside is validly code signed, not that it is
signed by team 9G68SMNHEU (the team is compared only when the EdDSA check
fails, Sparkle's key-rotation fallback). Developer ID signing and
notarization are what this pipeline guarantees for the releases it makes;
whoever holds the private key can ship anything to every tester. Keep it
only in the `release` environment (Ryan as required reviewer) and
1Password. Make it once, on your Mac, then never again (a lost key means
updates can only change keys through a Developer ID-signed DMG; keep the
1Password copy). `generate_keys` stores the key in your login keychain
(allow its prompt) and prints only the public key; the private key goes
from a mode-600 file straight into the secret and the file is deleted:

```sh
cd "$HOME/dev/isocast" &&
  dist="$(scripts/sparkle/sparkle.sh path)" &&
  "$dist/bin/generate_keys" >/dev/null &&
  key="$("$dist/bin/generate_keys" -p)" &&
  { grep '^#' scripts/sparkle/public-ed-key.txt; printf '%s\n' "$key"; } > scripts/sparkle/public-ed-key.new &&
  mv scripts/sparkle/public-ed-key.new scripts/sparkle/public-ed-key.txt &&
  scripts/sparkle/sparkle.sh public-key &&
  rm -f "$HOME/.config/isocast-release-secrets/sparkle-ed.key" &&
  ( umask 077 && "$dist/bin/generate_keys" -x "$HOME/.config/isocast-release-secrets/sparkle-ed.key" ) &&
  gh secret set SPARKLE_ED_PRIVATE_KEY --repo R44VC0RP/isocast --env release < "$HOME/.config/isocast-release-secrets/sparkle-ed.key" &&
  rm "$HOME/.config/isocast-release-secrets/sparkle-ed.key" &&
  git commit scripts/sparkle/public-ed-key.txt -m 'Sparkle: the release update key (public half)'
```

The printed line is the public key (fine to share). Save the keychain item
"Private key for signing Sparkle updates" to 1Password. Until the public
key is committed, Developer ID builds stop at once with "no Sparkle public
key"; `gh secret list` must now show nine names (step 4's six, 4a's two
and this one).

### 6. First hosted run

A published version (0.0.1 today) ends the run in preflight, so the first
real run needs the next version. Three blocks, each chained with `&&` so
that a failed line stops the rest of its block. First bump
`[workspace.package].version` in the source checkout on `main` (with this
workflow merged) and look at the result:

```sh
cd "$HOME/dev/isocast" &&
  sed -i '' 's/^version = "0.0.1"$/version = "0.0.2"/' Cargo.toml &&
  cargo update --workspace --offline &&
  grep -n '^version = ' Cargo.toml
```

The last line must print exactly one line, `20:version = "0.0.2"`, and
`git status --short` must show only `Cargo.toml` and `Cargo.lock`. If
not, stop here. The next block checks the version again before it
commits only those two files and publishes the snapshot:

```sh
cd "$HOME/dev/isocast" &&
  grep -q '^version = "0.0.2"$' Cargo.toml &&
  git commit Cargo.toml Cargo.lock -m 'Version 0.0.2' &&
  scripts/publish-snapshot.sh --push 'Isocast 0.0.2'
```

The tag is created in the **public snapshot checkout**, only after the
pulled snapshot itself says 0.0.2:

```sh
cd "$HOME/dev/isocast-public" &&
  git pull --ff-only &&
  grep -q '^version = "0.0.2"$' Cargo.toml &&
  git tag -a v0.0.2 -m 'Isocast 0.0.2 alpha' &&
  git push origin v0.0.2 &&
  sleep 20 &&
  gh run list --repo R44VC0RP/isocast --workflow release.yml -L 1
```

The tag push starts the run; the last line shows it with its id.
Preflight passes, then the release job waits for your approval under
**Actions → Release → Review deployments** (`gh run view <id> --repo
R44VC0RP/isocast --web` opens that page). Approve it; `gh run watch <id>
--repo R44VC0RP/isocast --exit-status` follows it. 30–60 minutes later
(cold Rust build, two notarizations, three recording takes), the job
summary links the draft. Check it like a user would:

```sh
gh release view v0.0.2 --repo R44VC0RP/isocast --json isDraft,assets --jq '{isDraft, assets: [.assets[]|{name,size}]}'
gh release download v0.0.2 --repo R44VC0RP/isocast --dir "$HOME/Downloads/isocast-0.0.2"
cd "$HOME/Downloads/isocast-0.0.2" && shasum -a 256 -c Isocast-0.0.2-arm64.dmg.sha256
spctl --assess --type open --context context:primary-signature -v Isocast-0.0.2-arm64.dmg
open Isocast-0.0.2-arm64.dmg
```

Expect `isDraft: true`, three assets (DMG, checksum, `appcast.xml`), `OK` from shasum and `accepted
source=Notarized Developer ID` from spctl. Publishing the draft stays
manual. **Publish it as a full release, never with "Set as a pre-release"**:
installed apps read `releases/latest/download/appcast.xml`, and GitHub's
`latest` skips drafts and pre-releases (v0.0.1 is a full release today, so
`latest` resolves; it has no appcast, so 0.0.1 finds no update until 0.0.2
is published, and 0.0.1 has no updater anyway). Every release meant to reach
testers must be published the same way. If you prefer not to cut 0.0.2 yet, a manual run for 0.0.1
(`gh workflow run release.yml --repo R44VC0RP/isocast --ref main -f version=0.0.1`)
only demonstrates the clean "already published" exit; it does not exercise
the secrets.

### 7. After the first green run: retire the app-specific password

Local notarizing (`scripts/release.sh --notarize` without the API-key
variables) uses the login keychain profile **isocast-notary**, which holds
an Apple ID app-specific password. Rotate it once the hosted run works:

1. At `https://account.apple.com/account/manage` → **Sign-In and Security →
   App-Specific Passwords**, revoke the password used for isocast-notary and
   generate a new one named `isocast-notary`. Copy it from that page only.
2. Update the profile in place (it prompts for the new password; nothing
   is typed on the command line) and prove it works with a read-only call:

   ```sh
   xcrun notarytool store-credentials isocast-notary --apple-id '<your Apple ID email>' \
     --team-id 9G68SMNHEU --keychain "$HOME/Library/Keychains/login.keychain-db"
   xcrun notarytool history --keychain-profile isocast-notary \
     --keychain "$HOME/Library/Keychains/login.keychain-db" | head -5
   ```

   `--validate` is the default, so a wrong password is refused before the
   old one is replaced. Alternatively store the same team API key locally
   and keep no app-specific password at all: revoke the app-specific
   password as above, then run step 2's commands with
   `--key "$HOME/.config/isocast-release-secrets/AuthKey.p8" --key-id <Key ID> --issuer <Issuer ID>`
   in place of `--apple-id … --team-id …`. notarytool copies the key's
   contents into the keychain item, so `AuthKey.p8` can be deleted from
   disk afterwards (it stays in 1Password).

## Cut a release

1. Set `[workspace.package].version` in Cargo.toml (and refresh Cargo.lock)
   in the source repository, verify/commit it, and publish a clean snapshot.
2. In the **public snapshot checkout**, tag the matching version and push
   only that tag. For example, after updating Cargo.toml to `0.0.2`:

   ```sh
   cd "$HOME/dev/isocast-public" &&
     git pull --ff-only &&
     grep -q '^version = "0.0.2"$' Cargo.toml &&
     git tag -a v0.0.2 -m 'Isocast 0.0.2 alpha' &&
     git push origin v0.0.2
   ```

3. In **Actions → Release**, review the tag/commit and approve the waiting
   **release** environment job. The version must match Cargo.toml exactly;
   a malformed/missing tag or mismatch fails in preflight, before the
   approval and before any secret is read.
4. Inspect the job summary's draft link, checksum and results. Download the
   DMG for the final human check. Publication is manual, outside the workflow.
5. **README once-over, before publishing (every release).** The README is
   the landing page for people looking for an OBS alternative, so when the
   release goes live it must describe exactly what this version ships:
   - every feature it lists as available is in this version's DMG; this
     version's new features are added, and anything still unreleased sits
     under "coming";
   - the screenshots show this version's app: retake any that changed (new
     buttons, dialogs, layout) from the release build, always through
     `scripts/snapshot.sh` or `scripts/screenshots/take.sh`, never against
     Ryan's own engine;
   - the Isocast vs OBS table, FAQ, requirements and benchmark numbers are
     still true; every image link resolves and has alt text.
   Commit it as `README: <version>` (or "README checked for <version>, no
   changes"), publish the snapshot, and only then publish the release.

Manual retry of an **existing tag** (also requires environment approval):

```sh
gh workflow run release.yml --repo R44VC0RP/isocast --ref main -f version=0.0.2
```

Both `0.0.2` and `v0.0.2` are accepted. This builds the tag's code, not
whatever main happens to contain later. It refreshes the same draft, never
makes another or replaces a published release. Do not publish it while its
workflow is still updating assets. Once a version is published, a tag push
or manual run for it stops in preflight (green, "nothing to build or
draft"); if it is published while the job waits for approval, the job stops
before importing secrets, and the finalizer refuses as a last resort.

## Verification and local use

The workflow selects Xcode 26.3 from the hosted image (its default Xcode 16
cannot compile the Icon Composer asset), installs stable Rust and Python
3.12, and caches Rust/Homebrew separately from ordinary CI. If GitHub removes
that Xcode version, update DEVELOPER_DIR to an installed Xcode 26+ version.
Checked 2026-10-14: the `macos-15` arm64 image (20260907.0337.1) ships Xcode
26.3 at `/Applications/Xcode_26.3.app` with 16.4 as default; the current
inventory is `images/macos/macos-15-arm64-Readme.md` in
`actions/runner-images`, under the latest `macos-15-arm64/*` release.

Self-update (Sparkle 2, #40): `release.sh` downloads Sparkle at a pinned
version and SHA-256 (`scripts/sparkle/sparkle.sh`), embeds it arm64-only
without the XPC services (non-sandboxed app), signs its helpers inside out,
and writes `SUFeedURL`/`SUPublicEDKey` plus the locked-down settings
(daily check, ask before every install, archive verified before extraction,
signed feed) into Info.plist. At draft time the finalizer runs Sparkle's
`generate_appcast` on the notarized DMG with the release notes embedded,
then checks the item's build, version, URL and length, verifies the DMG's
EdDSA signature against the app's own `SUPublicEDKey` (a wrong secret fails
here, before anything ships) and verifies the feed signature. The update
archive is the notarized, stapled DMG itself: Sparkle installs from DMGs,
and a Developer ID-signed DMG is the only way to rotate the EdDSA key later
with `SUVerifyUpdateBeforeExtraction` on.

The finalizer checks strict/deep signatures, Apple team, notarization
receipts and stapled tickets, Gatekeeper, the checksum, and executable/plist
hashes inside the mounted DMG. It invokes the **actual bundled CLI/engine**
without copying or re-signing them. The native GUI is not opened on the runner.

Mounted runtime qualification reuses `scripts/e2e.py`'s actual encoder
probes and file/loss/sync validators: hosted 720p30, H.264 hardware if
available (else HEVC/ProRes), two 5-second program+two-ISO takes, then a
third checked take. The existing per-output video-loss ceiling is 25%; if
both qualification takes have zero drops, the last requires zero too.
Audio loss is always zero; A/V duration, decoded counts, pulse sync and full
decode remain mandatory. The report records measured losses. Missing tools,
unusable recording/graphics backends or other runtime errors block drafting
rather than quietly passing a skipped check. All runs use private state,
bounded waits and the common recording lock; owned daemons are shut down.

Local defaults remain the login-keychain identity/profile and short alpha
notes. CI opts into the extra modes:

```sh
scripts/release.sh --notarize --verify-runtime --hosted \
  --draft-release --verify-tag --generate-notes --replace-draft \
  --output /path/to/prepared/release
```

`--verify-runtime` alone verifies an already finalized release. Without
`--hosted`, it runs strict 1080p60. `--replace-draft` is explicit: ordinary
local reruns still only reuse matching assets. CI sets
`ISOCAST_SIGN_KEYCHAIN`, `ISOCAST_SIGN_IDENTITY`, `APPLE_TEAM_ID`,
`APPLE_API_KEY_PATH`, `APPLE_API_KEY_ID` and `APPLE_API_ISSUER_ID`; without the
API-key trio, the existing local `isocast-notary` profile is used. Partial
API-key configuration is refused. API credentials are redacted from command
logs/errors; the `.p8` is read only by notarytool.

Use `--dry-run` to preview either stage. No secrets, environment settings,
workflow dispatch or public snapshot are created by checking in these files;
the first real Actions run is the final hosted signing/notarization check.
