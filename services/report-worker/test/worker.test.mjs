// `npm test`: the worker under `wrangler dev` (local R2 and rate limits)
// against a fake GitHub API.

import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";
import { after, before, test } from "node:test";
import { accessJwt, makeZip, start } from "./harness.mjs";

let h;
before(async () => {
  h = await start({ port: 8797 });
});
after(async () => {
  await h?.stop();
});

let ipCounter = 0;
const newIp = () => `198.51.100.${++ipCounter}`;
const newInstall = () => randomBytes(16).toString("hex");

function issueText({ panic = null, description = "The program froze when I pressed Stop." } = {}) {
  const triage = {
    schema: 1,
    versions: { version: "0.0.1", commit: "abc1234", profile: "release", arch: "aarch64" },
    hardware: { chip: "Apple M4", model: "Mac16,1" },
    crash_signature: panic ? { launch: "isocastd-20261014T0900-123", panic, thread: "recording writer" } : null,
    stream: { key: "[redacted]", url: "rtmps://live.example.com/[redacted]" },
  };
  return `# Isocast bug report\n\nState: "idle". 0 prior abnormal launch(es); 0 recent warning/error entries.\n\n## Description\n\n> ${description}\n\n## Agent triage\n\n\`\`\`json\n${JSON.stringify(triage)}\n\`\`\`\n\nReview metrics.jsonl, logs/, status.json, manifests/ and crash reports in the report's zip. No media is included.\n`;
}

function report({ issue = issueText(), files = {}, fields = {}, zipIssue } = {}) {
  const zip = makeZip({
    "ISSUE.md": zipIssue ?? issue,
    "status.json": JSON.stringify({ state: "idle", stream: { stream_key: "[redacted]", url: "https://a.example/[redacted]" } }, null, 2),
    "logs/isocastd-1.jsonl": '{"level":"info","message":"token: [redacted] Bearer [redacted]","fields":{"password":"[redacted]"}}\n',
    ...files,
  });
  return {
    zip,
    fields: { schema: 1, title: "The program froze when I pressed Stop.", issue, version: "0.0.1 (abc1234)", mac: "Apple M4 (Mac16,1), macOS 26.1 (25B78)", ...fields },
  };
}

async function post({ zip, fields, ip = newIp(), install = newInstall(), extra }) {
  const form = new FormData();
  form.set("fields", JSON.stringify(fields));
  form.set("zip", new Blob([zip], { type: "application/zip" }), "report.zip");
  if (extra) form.set(extra, "x");
  const response = await fetch(`${h.workerUrl}/reports`, {
    method: "POST",
    headers: { "CF-Connecting-IP": ip, "Isocast-Install-Id": install },
    body: form,
  });
  return { status: response.status, body: await response.json() };
}

async function getZip(id, headers = {}) {
  return fetch(`${h.workerUrl}/reports/${id}/zip`, { headers });
}

test("happy path: issue filed with the previewed text, zip kept privately", async () => {
  const r = report();
  const { status, body } = await post(r);
  assert.equal(status, 201, JSON.stringify(body));
  assert.match(body.report_id, /^[0-9a-f]{32}$/);
  assert.equal(body.deduplicated, false);
  const state = await h.state();
  const issue = state.issues.at(-1);
  assert.equal(body.issue_url, issue.html_url);
  assert.equal(issue.title, r.fields.title);
  assert.deepEqual(issue.labels, ["bug", "from-app"]);
  assert.ok(issue.body.startsWith(r.fields.issue), "issue body starts with exactly the previewed ISSUE.md");
  const footer = issue.body.slice(r.fields.issue.length);
  assert.ok(footer.includes(`Report \`${body.report_id}\``));
  assert.ok(footer.includes("0.0.1 (abc1234)") && footer.includes("Apple M4 (Mac16,1)"));
  assert.ok(!issue.body.includes(h.workerUrl), "never a link to the zip");
  const zip = await getZip(body.report_id, { Authorization: `Bearer ${h.maintainerToken}` });
  assert.equal(zip.status, 200);
  assert.deepEqual(Buffer.from(await zip.arrayBuffer()), r.zip);
  assert.ok((await h.objects()).includes(`reports/${body.report_id}.zip`), "zip in local R2");
});

test("maintainer fetch: bearer token or Access JWT only", async () => {
  const { body } = await post(report());
  const id = body.report_id;
  assert.equal((await getZip(id)).status, 401);
  assert.equal((await getZip(id, { Authorization: "Bearer wrong" })).status, 401);
  assert.equal((await getZip(id, { "Cf-Access-Jwt-Assertion": accessJwt(h) })).status, 200);
  assert.equal((await getZip(id, { "Cf-Access-Jwt-Assertion": accessJwt(h, { aud: "other" }) })).status, 401);
  assert.equal((await getZip(id, { "Cf-Access-Jwt-Assertion": accessJwt(h, { exp: 1 }) })).status, 401);
  assert.equal((await getZip("0".repeat(32), { Authorization: `Bearer ${h.maintainerToken}` })).status, 404);
});

test("oversize zip refused before storing", async () => {
  const before = (await h.state()).issues.length;
  const stored = (await h.objects()).length;
  const big = report();
  big.zip = Buffer.concat([randomBytes((50 << 20) + 1), big.zip]);
  const { status, body } = await post(big);
  assert.equal(status, 413, JSON.stringify(body));
  assert.match(body.error, /^zip: larger than 50 MB/);
  assert.equal((await h.state()).issues.length, before);
  assert.equal((await h.objects()).length, stored);
});

test("planted secrets refused, nothing stored", async () => {
  const before = await h.state();
  const stored = (await h.objects()).length;
  const cases = [
    [{ files: { "logs/isocastd-2.jsonl": '{"message":"pushed with ghp_' + "a".repeat(36) + '"}\n' } }, /zip: logs\/isocastd-2\.jsonl contains a GitHub token/],
    [{ files: { "setup.json": '{\n  "stream_key": "live_abcdef0123456789"\n}' } }, /zip: setup\.json contains a credential field/],
    [{ files: { "bootstrap.log": "connecting to rtmp://live.twitch.tv/app/" + "live_123456789_abcdefghijkl\n" } }, /bootstrap\.log contains a stream URL with a key/],
    [{ files: { "bootstrap.log": "GET https://hooks.example.com/services/T000/B000/XXXX\n" } }, /contains a URL with a path/],
    [{ fields: { title: "my password: hunter2" } }, /^fields\.title contains a credential in text/],
  ];
  for (const [options, error] of cases) {
    const { status, body } = await post(report(options));
    assert.equal(status, 422, JSON.stringify(body));
    assert.match(body.error, error);
    assert.ok(!JSON.stringify(body).includes("hunter2") && !JSON.stringify(body).includes("ghp_"), "the error never repeats the secret");
  }
  const after = await h.state();
  assert.equal(after.issues.length, before.issues.length);
  assert.equal((await h.objects()).length, stored);
});

test("schema: unknown fields and parts, mismatched ISSUE.md", async () => {
  let r = await post(report({ fields: { email: "a@b.c" } }));
  assert.equal(r.status, 400);
  assert.match(r.body.error, /^fields\.email: unknown field; valid fields: schema, title, issue, version, mac/);
  r = await post({ ...report(), extra: "screenshot" });
  assert.equal(r.status, 400);
  assert.match(r.body.error, /^screenshot: unknown part/);
  r = await post(report({ zipIssue: issueText({ description: "something else" }) }));
  assert.equal(r.status, 400);
  assert.match(r.body.error, /differs from the zip's ISSUE\.md/);
  r = await post({ ...report(), install: "not-an-id" });
  assert.equal(r.status, 400);
  assert.match(r.body.error, /^headers\.Isocast-Install-Id/);
});

test("dedupe: the same crash adds a comment to the open issue", async () => {
  const first = await post(report({ issue: issueText({ panic: "index 5 out of range at crates/x/src/a.rs:10:4" }) }));
  assert.equal(first.status, 201);
  const second = await post(report({ issue: issueText({ panic: "index 9 out of range at crates/x/src/a.rs:12:4", description: "Again" }) }));
  assert.equal(second.status, 201, JSON.stringify(second.body));
  assert.equal(second.body.deduplicated, true);
  const state = await h.state();
  const issue = state.issues.find((i) => i.html_url === first.body.issue_url);
  assert.ok(second.body.issue_url.startsWith(issue.html_url + "#issuecomment-"));
  const comment = state.comments.at(-1);
  assert.equal(comment.issue, issue.number);
  assert.ok(comment.body.includes(second.body.report_id) && comment.body.includes("> Again"));
  const other = await post(report({ issue: issueText({ panic: "a different panic" }) }));
  assert.equal(other.body.deduplicated, false);
});

test("GitHub failure: 502, zip removed again", async () => {
  const stored = (await h.objects()).length;
  await h.failNext();
  const { status, body } = await post(report());
  assert.equal((await h.objects()).length, stored);
  assert.equal(status, 502);
  assert.match(body.error, /503 Service Unavailable; nothing was stored/);
});

test("rate limits: per install id and per IP", async () => {
  const install = newInstall();
  const statuses = [];
  for (let i = 0; i < 4; i++) statuses.push((await post({ ...report(), install })).status);
  assert.deepEqual(statuses, [201, 201, 201, 429]);
  const ip = newIp();
  const byIp = [];
  for (let i = 0; i < 11; i++) {
    const r = await fetch(`${h.workerUrl}/reports`, { method: "POST", headers: { "CF-Connecting-IP": ip, "Isocast-Install-Id": newInstall() } });
    byIp.push(r.status);
  }
  assert.deepEqual(byIp, [...Array(10).fill(415), 429]);
});
