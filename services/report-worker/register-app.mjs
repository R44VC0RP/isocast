// Ryan runs this once to create the GitHub App from app-manifest.json
// (GitHub's manifest flow): `node register-app.mjs ~/isocast-report-app.pem`.
// It opens a local page with a button that sends the manifest to GitHub;
// after you approve, GitHub redirects back here with a one-time code, which
// is exchanged for the App's id and private key. The key is written to the
// given path (mode 0600, never into the repository) and nothing else is kept.

import { readFileSync, writeFileSync, existsSync } from "node:fs";
import { createServer } from "node:http";
import { execFile } from "node:child_process";

const out = process.argv[2];
if (!out || existsSync(out)) {
  console.error("usage: node register-app.mjs <new path for the private key .pem>");
  process.exit(2);
}
const manifest = readFileSync(new URL("./app-manifest.json", import.meta.url), "utf8");
const state = crypto.randomUUID();
const escape = (s) => s.replace(/&/g, "&amp;").replace(/"/g, "&quot;").replace(/</g, "&lt;");
const page = `<!doctype html><meta charset=utf-8><title>Isocast Bug Reports App</title>
<form action="https://github.com/settings/apps/new?state=${state}" method="post">
<p>Create the private GitHub App “Isocast Bug Reports” on your account (Issues: write).</p>
<input type="hidden" name="manifest" value="${escape(manifest)}">
<button type="submit">Create GitHub App</button></form>`;

const server = createServer(async (req, res) => {
  const url = new URL(req.url, "http://127.0.0.1:3999");
  if (url.pathname === "/") {
    res.writeHead(200, { "Content-Type": "text/html" });
    return res.end(page);
  }
  if (url.pathname !== "/callback" || url.searchParams.get("state") !== state) {
    res.writeHead(404);
    return res.end();
  }
  const code = url.searchParams.get("code");
  const response = await fetch(`https://api.github.com/app-manifests/${code}/conversions`, {
    method: "POST",
    headers: { Accept: "application/vnd.github+json", "User-Agent": "isocast-register-app" },
  });
  const app = await response.json();
  if (!response.ok) {
    res.writeHead(500);
    res.end("GitHub refused the code; see the terminal.");
    console.error(`conversion failed: ${response.status} ${app.message ?? ""}`);
    process.exit(1);
  }
  writeFileSync(out, app.pem, { mode: 0o600 });
  const install = `${app.html_url}/installations/new`;
  res.writeHead(302, { Location: install });
  res.end();
  console.log(`App created: ${app.slug} (id ${app.id}). Private key written to ${out}.`);
  console.log(`GITHUB_APP_ID=${app.id}`);
  console.log(`Next: install it on R44VC0RP/isocast only: ${install}`);
  server.close();
});
server.listen(3999, "127.0.0.1", () => {
  console.log("Opening http://127.0.0.1:3999/ …");
  execFile("open", ["http://127.0.0.1:3999/"]);
});
