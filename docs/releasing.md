# Releasing Isocast

The **Release** workflow builds an existing `vX.Y.Z` tag on GitHub-hosted
`macos-15` (arm64), signs with Developer ID, notarizes/staples the app and
DMG, runs the mounted CLI/engine recording checks, and creates **one draft**
with the DMG, its checksum and Sparkle's `appcast.xml` (the self-update
feed, EdDSA-signed). The release notes are `docs/releases/X.Y.Z.md`,
committed before tagging: Sparkle's update window shows them (embedded in
the appcast as markdown) and they are the GitHub release body, followed by
a **Full Changelog** compare link. Rerunning a version replaces that
draft's assets and notes. Published releases are refused; a published
release's update notes are fixed with **Refresh appcast notes** (below). Only Ryan publishes a
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
whether that version is already published, and that `docs/releases/X.Y.Z.md`
exists at the tag and isn't empty. A published version ends the run green
with "nothing to build or draft" and never asks for an approval; a missing
tag, a mismatch or missing notes fail there with the reason.

## Windows packaging (W.7, qualification in progress)

Windows uses **Inno Setup 7.1.0**, pinned by version and SHA-256 in
`scripts/windows/package.ps1`. Its source licence permits open-source and
commercial use; the vendor requests commercial licences but its
[licensing FAQ](https://jrsoftware.org/isorder.php#required) says purchase is
not strictly required and is not expected during qualification. No purchase
has been made. Unlike MSIX, this keeps the existing native daemon/CLI layout
and ordinary user-data paths without package identity or filesystem
virtualization. It installs per user in `%LOCALAPPDATA%\Programs\Isocast`,
with the app, CLI and `isocastd.exe` beside each other and the x64 Visual C++
CRT app-local; it needs no administrator prompt or machine-wide runtime.
There is one Start menu shortcut and no service or PATH modification. WinGet
distribution is later work, not a requirement to install the alpha.

The Windows CI engine job builds debug binaries without debug symbols from the locked Cargo tree,
packages them with that compiler, exercises install/daemon-launch/uninstall,
and retains `windows-x64-unsigned` and `windows-x64-unsigned-next`. These are
distinct compiled `X.Y.Z.1` and `X.Y.Z.2` qualification builds, with an explicit
test-only public key and loopback feed. The private test key is not in GitHub
or its artifacts. `payload/package.json` records the compiler, payload hashes
and the actual executable's `--update-info` identity. This is a pinned recipe,
**not yet a claim of bit-identical Rust builds across runner images**. The
delegator runs it through `scripts/windows-ci.sh`; agents do not dispatch,
push, upload releases or publish. Do not distribute qualification builds as
production releases. The signing/appcast stages refuse their test-only identity.
The hosted UI is compiled separately at `-j 1`, with `ISOCAST_UI_NO_GALLERY=1`
also retained for Clippy, tests and the second candidate, following the
runner's measured memory limit. Installer/snapshot paths use `target/debug`.
Native and production release builds retain the full gallery; the native
package qualification uses release binaries, not this hosted debug recipe.

For an already-built Windows payload:

```powershell
./scripts/windows/package.ps1 -BinDir target/release `
  -OutputDir C:\isocast-dev\packages\candidate `
  -ToolsDir C:\isocast-dev\package-tools `
  -CrtDir '<Visual Studio VC\Redist\MSVC\version\x64\Microsoft.VC143.CRT>'
```

`OutputDir` must be new. The compiler is installed only inside `ToolsDir`.
Close the app and CLI commands first. The incoming CLI asks the daemon to
shut down through the same atomic `shutdown_if_idle` API used on Mac; active
recording/output work is refused, idle calibration is cancelled, and IPC waits
are bounded. Nothing is force-terminated. A remaining executable/DLL lock
blocks replacement. Uninstall removes only packaged files and the shortcut;
it does not remove `%LOCALAPPDATA%\Isocast` (the engine's existing state root),
recordings, credentials or unknown files inside the installation directory.
`isocast engine shutdown --json` and MCP's `shutdown_idle_daemon` expose that
same non-launching idle shutdown for maintenance.

**Windows updates are manual checks for now.** Settings → General → Check
for Updates uses pinned **WinSparkle 0.9.4**. Its Ed25519 verifier uses the
public key compiled into the app (the existing Sparkle release key), never a
key or feed chosen in user settings. Its automatic timer/startup prompt is
disabled before initialization, even if an old preference enabled it: an
unsolicited dialog must not enter a screen recording. The automatic switch
is disabled on Windows; Mac's daily-check behavior is unchanged. Installation
requires atomic idle shutdown and executes only fixed installer arguments,
never unsigned arguments supplied by an appcast.

Upstream 0.9.4 names the downloaded file after the server's
`Content-Disposition` header or the URL and writes it before checking the
signature, so a hostile response could write outside its temporary folder.
No upstream release fixes this, so `scripts/windows/winsparkle.ps1` builds
0.9.4 from its pinned source with `winsparkle-download.patch` applied and never
ships the prebuilt DLL. The patch:

- saves only as `Isocast-update.exe` (`CREATE_NEW`) in a fresh
  `%LOCALAPPDATA%\Isocast-Update-<uuid>` folder that only the user and
  SYSTEM can open. Any ancestor that isn't on ACL-capable fixed storage, or
  is a reparse point, fails closed;
- refuses the update unless the server's name is a plain
  `[A-Za-z0-9._-]+\.exe` with no `..` that isn't a device name;
- requires Ed25519 (no DSA or unsigned fallback) and caps the payload at
  1 GiB;
- deletes refused folders without recursion, through held handles. At
  startup it also sweeps folders left by an interrupted download (up to 64
  strictly named, private ones), never touching one in use or one it didn't
  create;
- tells the user "This update couldn't be verified and wasn't installed"
  for any refused download. An unreachable or unreadable feed instead shows
  "Couldn't check for updates" / "Isocast couldn't reach its update server.
  Check your internet connection and try again later."

The app refuses a WinSparkle DLL that lacks the patch's
`win_sparkle_isocast_confined_downloads` export (it must return `1`). The
build is cached by patch SHA-256, and the package's `WinSparkle-build.json`
records the patch, source and DLL hashes. Rebuild after any change to the
patch.

Preserve the previous installer and its public `.ed25519` sidecar. After
closing the app, deliberate authenticated rollback is:

```powershell
& "$env:LOCALAPPDATA\Programs\Isocast\isocast-ui.exe" `
  --rollback-update C:\path\Isocast-previous-windows-x64-setup.exe '<signature from its .ed25519 file>'
```

The embedded key verifies the locked installer before stopping the daemon or
launching anything. Only this explicit path adds `/ALLOWDOWNGRADE=1`; automatic
updates cannot supply it. This is a deliberate older-installer rollback, not
automatic recovery from an unsuccessful application launch. If the current
app cannot run, the release operator can use the pinned standalone
`winsparkle-tool verify --public-key ... --signature ... PREVIOUS.exe` and run
that verified previous installer with `/ALLOWDOWNGRADE=1`. Do not infer that
Inno's failure rollback restores overwritten application files. User state is
retained, not downgraded or deleted; older versions may reject newer settings.

**Production boundary (operator-owned):** clear all `ISOCAST_WINDOWS_UPDATE_*`
and `ISOCAST_WINDOWS_APPCAST_URL` qualification overrides when compiling the
release. The production build number defaults to the workspace version. Give
`package.ps1` all three signing options—`CertificateThumbprint`, `SignToolPath`
and an HTTPS `TimestampUrl`—only in the protected release environment. It
uses an already-installed certificate, SHA-256/RFC3161 signing, and validates
the expected timestamped signatures; Inno signs the uninstaller and Setup too.
It never imports credentials or publishes. The resulting installer is then
signed with the existing Sparkle Ed25519 key (`sign_update`; its signature is
compatible with WinSparkle), under the same protected signing authority.

`scripts/windows/appcast.ps1 -PackageDir ... -Signature ... -DownloadUrl ...
-ToolsDir ... -ExpectedCertificateThumbprint ...` verifies the checksum,
timestamped Authenticode identity, embedded release key/feed and Ed25519
signature before producing `appcast-windows.xml` and the public signature
sidecar. It uses the same committed `docs/releases/X.Y.Z.md`. For private
qualification only, `-AllowUnsignedTest` accepts a test-key build and produces
`appcast-windows-test.xml`; HTTP is allowed only on `127.0.0.1`.

Finish/verify the Mac draft **first**, then the operator attaches the Windows
installer, its `.sha256`, its `.ed25519` and `appcast-windows.xml` to that same
unpublished version. A Mac-only draft replacement now refuses to delete any
attached Windows assets. Published Mac-note refresh accepts an optional
complete Windows set, verifies and preserves every asset digest, and still
replaces only the Mac `appcast.xml`. No Windows-only latest release should
strand Mac users without their feed. Signing, release approval and publishing
remain the delegator/Ryan's. Actual package/update lifecycle qualification is
still required before W.7 is complete; source checks are not release sign-off.

Native qualification (2026-10-18): test-key builds0.0.7.1 and0.0.7.2 passed
non-admin clean-VM install/launch, upgrade, authenticated rollback, actual
WinSparkle tamper refusal and valid installation, and state/recording-preserving
uninstall. Windows automatically permits Slint's CPU/WARP adapter fallback
when no hardware adapter is available; it does not force CPU rendering.
This does not qualify capture/encoding or Authenticode/SmartScreen behavior.
Hosted package qualification still depends on the Windows runner memory fix.
Release-note generation explicitly reads UTF8 on Windows PowerShell5.1.

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
   **Write the release notes in the same commit or before it:**
   `docs/releases/X.Y.Z.md`, markdown, for testers (see `docs/releases/0.0.6.md`).
   Start with one line on who it's for and how to update, then `**Bold**`
   group headings with `- ` bullets. Sparkle's update window (macOS 12+)
   renders headings, bold/italic, lists, `code`, block quotes and http(s)
   links. Don't use tables (their text disappears there), `---` rules
   (blank space) or images (only the alt text). No **Full Changelog** line
   (the GitHub release adds it). They are
   read from the tag, so they can't be added after tagging without
   re-tagging: `python3 scripts/release-finalize.py --check-notes X.Y.Z`
   checks them, and preflight refuses a tag without them.
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
   The Mac job needs no Linux job. For a combined release, wait for the four
   `.deb` install tests and approve the separate Linux attachment job. It adds
   four `.deb`s, two Ubuntu 24.04-based tarballs and `SHA256SUMS` to the same
   draft. Download those seven together and require six OKs from `sha256sum
   -c SHA256SUMS` (on macOS, `shasum -a 256 -c SHA256SUMS`). The tarballs are
   inspected, not clean-installed by the workflow. If attachment fails, leave
   the draft unpublished and rerun the failed jobs; the Mac draft need not be
   rebuilt. A Mac-only publication requires **no** Linux assets, never a partial
   set. Never publish while a job might still upload.
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
6. **After publishing, bump the website's version and redeploy.** The site
   names the version in one place: `data-version` on the meta line in
   `website/public/index.html`. Both Download buttons are built from it
   (`releases/download/vX.Y.Z/Isocast-X.Y.Z-arm64.dmg`), so check that URL
   downloads and commit `website: X.Y.Z`. Then, on main, deploy from
   `website/` with the `cf` CLI's `work` profile
   ([docs/website.md](website.md)):
   ```sh
   npm ci && npm run check    # cf deploy --dry-run --profile work: uploads nothing
   npm run deploy             # cf deploy --profile work
   ```
   Also check the page's claims against the release notes, as for the
   README.

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

## Fix a published release's update notes

The appcast is signed (`SURequireSignedFeed`), so its notes can't be edited
by hand. The **Refresh appcast notes** workflow
(`.github/workflows/refresh-appcast.yml`) replaces only `appcast.xml` on a
published release with one that embeds `docs/releases/X.Y.Z.md` from main:

1. Commit and publish the corrected `docs/releases/X.Y.Z.md` on main.
2. Run it and approve the **release** environment when it waits:

   ```sh
   gh workflow run refresh-appcast.yml --repo R44VC0RP/isocast --ref main -f version=0.0.6
   ```

Preflight (no secrets) refuses drafts, unpublished or missing versions,
missing notes and unexpected/partial asset sets. Assets must be the DMG, its
`.sha256` and `appcast.xml`, optionally plus the complete seven-file Linux
set and/or the complete four-file Windows set. After approval the job downloads the assets,
checks their GitHub digests and the DMG's `.sha256`, reads the version, build
and `SUPublicEDKey` from the DMG's app (mounted read-only), verifies the
published appcast (one item, the DMG's EdDSA signature against that key,
feed signature), then runs `generate_appcast` on the same DMG with the notes.
The new feed must have the same item and enclosure (URL, length, EdDSA
signature; Ed25519 is deterministic, so the same key gives the same
signature) and pubDate, the notes as markdown and a valid feed signature.
Only then is `appcast.xml` uploaded with `--clobber`, and every asset digest
is checked again. The DMG is never rebuilt or re-signed. The job summary
keeps the old and new feeds; installed apps see the new notes on their next
check (GitHub may cache the download for a few minutes).

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

Local defaults remain the login-keychain identity/profile; notes always
come from `docs/releases/X.Y.Z.md` (a Developer ID build and `--draft-release`
stop without them). CI opts into the extra modes:

```sh
scripts/release.sh --notarize --verify-runtime --hosted \
  --draft-release --verify-tag --replace-draft \
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
