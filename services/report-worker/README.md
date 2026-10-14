# Isocast report worker

A Cloudflare Worker that turns a bug report sent from Isocast into a GitHub
issue, so users don't have to paste a triage block or attach a zip.

- `POST /reports` takes two multipart parts: `fields` (JSON: `schema` 1,
  `title`, `issue`, `version`, `mac`; nothing else is accepted) and `zip`
  (the report zip, at most 50 MB), plus an `Isocast-Install-Id` header (32
  hex characters; a random id the engine keeps, used only for rate limiting).
  The worker checks the schema and sizes, checks that the zip's `ISSUE.md`
  is exactly the `issue` text, and scans the fields and every file in the zip
  for secrets. The scan uses `scripts/publish-snapshot.sh`'s patterns plus
  anything the engine's redactor should have replaced: a credential field or
  credential in text that isn't `[redacted]`, or an HTTP/RTMP URL with a path,
  query or user name. If the scan finds anything, the report is refused with
  a `422` that names the file and the rule (never the text), and nothing is
  stored. Otherwise the zip goes into the private R2 bucket as
  `reports/<id>.zip`, and the worker files an issue labeled `bug` and
  `from-app`. The issue holds the title, `ISSUE.md` exactly as the user saw
  it (the description, the state and the triage block), then a footer with
  the report id, version, Mac and crash signature. It never links to the zip.
  If the triage block has a crash signature (the panic message and thread
  with numbers folded) that matches an open `from-app` issue, the report is
  added to that issue as a comment instead. Response: `201
  {"issue_url","report_id","deduplicated"}`.
  Errors are `{"error": "<field path>: <why>"}`. Status codes: 400 schema,
  413 size, 415 type, 422 secret found, 429 rate limited, 502 GitHub failed
  (the zip is deleted again).
- `GET /reports/<id>/zip` is for maintainers only. Send
  `Authorization: Bearer $MAINTAINER_TOKEN`, or set up Cloudflare Access
  (below).
- Rate limits are per IP (10 a minute) and per install id (3 a minute),
  using Workers rate limiting.

The engine sends a report with `isocast report --submit`, the MCP tool
`bug_report` with `submit: true`, or the app's **Send Report** button. It
posts to `ISOCAST_REPORT_ENDPOINT` or, when that isn't set, to
`REPORT_ENDPOINT` in `crates/isocast-protocol/src/diagnostics.rs`.

## Local development and tests

```sh
cd services/report-worker
npm ci
npm run check            # TypeScript
npm test                 # wrangler dev + fake GitHub: happy path, oversize,
                         # planted secrets, schema, dedupe, GitHub failure,
                         # rate limits, maintainer fetch (token and Access)
node test/serve.mjs /tmp/worker.json 8790   # keep it running for an engine
```

`test/harness.mjs` makes a throwaway GitHub App key (PKCS#1, like GitHub's)
and an Access signing key, starts a fake GitHub API that verifies the App
JWT and installation token, and runs `wrangler dev` with local R2 and rate
limiting. Nothing leaves the machine.

## Ryan's setup (one time)

Nothing here has been created or deployed yet. You need a Cloudflare account
on **Workers Paid** (the scan needs more than the free plan's CPU time;
`limits.cpu_ms` is 60000) and admin on `R44VC0RP/isocast`.

1. **Install and log in.**
   `cd services/report-worker && npm ci && npx wrangler login`
2. **Create the GitHub App** from `app-manifest.json`. The App is private,
   with Issues: write and Metadata: read, no webhook.
   `node register-app.mjs ~/.config/isocast/report-app.pem`
   This opens a page with one button. Approve on GitHub, and the script
   prints `GITHUB_APP_ID=…` and writes the private key to that path (0600).
   When GitHub shows the install page, choose **Only select repositories →
   R44VC0RP/isocast**.
   (By hand instead: GitHub → Settings → Developer settings → New GitHub App,
   with the same permissions and no webhook. Then generate a private key and
   install the App on the repository.)
3. **Make the labels** (if they don't exist yet):
   `gh label create from-app --repo R44VC0RP/isocast --description "Sent from the Isocast app" --color 5319e7`
   (`bug` exists by default.)
4. **Create the private R2 bucket** (no public access, no custom domain):
   `npx wrangler r2 bucket create isocast-bug-reports`
5. **Configure and add the secrets.** Set `"GITHUB_APP_ID"` in
   `wrangler.jsonc` to the printed id. Then:
   `npx wrangler secret put GITHUB_APP_PRIVATE_KEY < ~/.config/isocast/report-app.pem`
   `openssl rand -hex 32 | tee /dev/stderr | npx wrangler secret put MAINTAINER_TOKEN`
   Keep the token in 1Password (`2password`) as "Isocast report worker
   maintainer token". The rate-limit `namespace_id`s (4401, 4402) only need
   to be unique in your account.
6. **Deploy:** `npx wrangler deploy`. It prints
   `https://isocast-report-worker.<subdomain>.workers.dev`. Check it with
   `curl -s https://…workers.dev/` (`{"ok":true}`). To use your own domain,
   add `"routes": [{"pattern": "reports.example.com", "custom_domain": true}]`
   and deploy again.
7. **Point Isocast at it.** In `crates/isocast-protocol/src/diagnostics.rs`,
   set `REPORT_ENDPOINT` to `Some("https://…/reports")`. Until then, the app's
   Send Report button says reports can't be sent yet, and
   `ISOCAST_REPORT_ENDPOINT=https://…/reports` works for testing.
8. **Try it:** `isocast report --describe "Test report, please close" --submit`.
   It prints the issue URL. Fetch the zip:
   `curl -fH "Authorization: Bearer $TOKEN" -o r.zip https://…/reports/<report id>/zip`

### Cloudflare Access instead of (or as well as) the token

Make a Zero Trust → Access → Applications → Self-hosted application for
`<worker host>/reports/*/zip`, with a policy for your email (and a service
token for the triage agent). Then copy the application's **AUD tag** into
`ACCESS_AUD`, and set `ACCESS_TEAM_DOMAIN` to
`https://<team>.cloudflareaccess.com` in `wrangler.jsonc`. The worker then
accepts a valid `Cf-Access-Jwt-Assertion`, checking the signature against
the team's keys, the audience, the issuer and the expiry. The bearer token
keeps working unless you delete the `MAINTAINER_TOKEN` secret. Access needs a
hostname it can protect: the custom domain from step 6, or workers.dev with
Access turned on in the Worker's settings.

## Limits and choices

- The zip is never public. R2 has no public bucket or custom domain, and the
  issue names only the report id.
- The worker stores no IP address. The install id is used only as a rate-limit
  key and is not stored.
- Zips are kept until a maintainer deletes them
  (`npx wrangler r2 object delete isocast-bug-reports/reports/<id>.zip`).
  An R2 lifecycle rule (for example, delete after 180 days) can be added in
  the dashboard.
- Dedupe looks at the newest 300 open `from-app` issues.
