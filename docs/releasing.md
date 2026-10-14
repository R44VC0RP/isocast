# Releasing Isocast

The **Release** workflow builds an existing `vX.Y.Z` tag on GitHub-hosted
`macos-15` (arm64), signs with Developer ID, notarizes/staples the app and
DMG, runs the mounted CLI/engine recording checks, and creates **one draft**.
Rerunning a version replaces that draft's assets and generated notes.
Published releases are refused. Only Ryan publishes a draft.

No pull requests or self-hosted machines run this job. Actions are pinned
to commit SHAs. The job has `contents: write`; workflow defaults are read-only.
It waits at the **release** environment before receiving its secrets. A
workflow-wide concurrency group serializes tag pushes and manual reruns.
The camera extension remains a separate, one-time installation.

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
in YAML alone does not configure its protection rules.

### 2. Export the Developer ID certificate

In **Keychain Access → login → My Certificates**, expand
**Developer ID Application: Ryan Vogel (9G68SMNHEU)**. Export the certificate
**and its private key** as a password-protected `.p12`. A `.cer` alone cannot
sign. Save the `.p12` and its export password in a private folder outside
the repository. Put the password in a protected text file without a trailing
newline, using your password manager/editor; never put it in a command.

### 3. Create an App Store Connect API key

In **App Store Connect → Users and Access → Integrations → App Store
Connect API → Team Keys**, create a key with the **Developer** role. An
Account Holder/Admin may first need to enable API access. Download its `.p8`
private key (available once), and save its **Key ID** and **Issuer ID** into
separate protected text files. Use a **team** key: this workflow passes both
`--key-id` and `--issuer`. It does not need an Apple ID password or local
notarytool profile. Team ID is **9G68SMNHEU**.

### 4. Add the six environment secrets from files

Example private folder: `$HOME/.config/isocast-release-secrets` (directory
mode 700, files 600). The following assumes the exports/text files are
already there; adjust filenames to your exports. Run with shell tracing off.
None of these commands print the contents:

```sh
umask 077
secrets="$HOME/.config/isocast-release-secrets"
chmod 700 "$secrets"
chmod 600 "$secrets/developer-id.p12" "$secrets/p12-password.txt" \
  "$secrets/AuthKey.p8" "$secrets/key-id.txt" "$secrets/issuer-id.txt"
base64 < "$secrets/developer-id.p12" | tr -d '\n' > "$secrets/p12.base64"
base64 < "$secrets/AuthKey.p8" | tr -d '\n' > "$secrets/p8.base64"
printf '%s' '9G68SMNHEU' > "$secrets/team-id.txt"

gh secret set DEVELOPER_ID_P12_BASE64 --repo R44VC0RP/isocast --env release < "$secrets/p12.base64"
gh secret set DEVELOPER_ID_P12_PASSWORD --repo R44VC0RP/isocast --env release < "$secrets/p12-password.txt"
gh secret set APPLE_API_KEY_P8_BASE64 --repo R44VC0RP/isocast --env release < "$secrets/p8.base64"
gh secret set APPLE_API_KEY_ID --repo R44VC0RP/isocast --env release < "$secrets/key-id.txt"
gh secret set APPLE_API_ISSUER_ID --repo R44VC0RP/isocast --env release < "$secrets/issuer-id.txt"
gh secret set APPLE_TEAM_ID --repo R44VC0RP/isocast --env release < "$secrets/team-id.txt"
gh secret list --repo R44VC0RP/isocast --env release
```

Keep the exports in your credential store and remove transient plaintext
copies when finished. Do not upload them as workflow artifacts or commit
them. The workflow decodes them only under RUNNER_TEMP with restrictive
permissions, masks its random keychain password/identity, and imports into
a temporary keychain. `always()` restores the original search list, deletes
that keychain, and removes the certificate/API-key files, including after
failures. Credentials are not placed in Cargo/Homebrew caches or passed to
the recording daemon. Rotate/revoke the certificate and API key in Apple's
portals and replace the environment secrets when necessary.

## Cut a release

1. Set `[workspace.package].version` in Cargo.toml (and refresh Cargo.lock)
   in the source repository, verify/commit it, and publish a clean snapshot.
2. In the **public snapshot checkout**, tag the matching version and push
   only that tag. For example, after updating Cargo.toml to `0.0.2`:

   ```sh
   cd "$HOME/dev/isocast-public"
   git pull --ff-only
   git tag -a v0.0.2 -m 'Isocast 0.0.2 alpha'
   git push origin v0.0.2
   ```

3. In **Actions → Release**, review the tag/commit and approve the waiting
   **release** environment job. The version must match Cargo.toml exactly;
   a malformed/missing tag or mismatch fails before secrets are imported.
4. Inspect the job summary's draft link, checksum and results. Download the
   DMG for the final human check. Publication is manual, outside the workflow.

Manual retry of an **existing tag** (also requires environment approval):

```sh
gh workflow run release.yml --repo R44VC0RP/isocast --ref main -f version=0.0.2
```

Both `0.0.2` and `v0.0.2` are accepted. This builds the tag's code, not
whatever main happens to contain later. It refreshes the same draft, never
makes another or replaces a published release. Do not publish it while its
workflow is still updating assets.

## Verification and local use

The workflow selects Xcode 26.3 from the hosted image (its default Xcode 16
cannot compile the Icon Composer asset), installs stable Rust and Python
3.12, and caches Rust/Homebrew separately from ordinary CI. If GitHub removes
that Xcode version, update DEVELOPER_DIR to an installed Xcode 26+ version.

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
