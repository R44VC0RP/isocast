// Isocast bug reports, filed in one click: POST /reports takes the report's
// public issue text and its private zip, refuses anything that looks like a
// secret, keeps the zip in a private R2 bucket and files (or adds to) a
// GitHub issue. GET /reports/:id/zip hands a zip to maintainers.

import { maintainer } from "./access";
import { GitHub, GitHubError } from "./github";
import { StreamScan, findSecret } from "./scan";
import { ZipError, entries, readText, text } from "./zip";

export interface Env {
  REPORTS: R2Bucket;
  IP_LIMIT: RateLimit;
  INSTALL_LIMIT: RateLimit;
  GITHUB_API: string;
  GITHUB_REPO: string;
  GITHUB_APP_ID: string;
  GITHUB_APP_PRIVATE_KEY?: string;
  MAINTAINER_TOKEN?: string;
  ACCESS_TEAM_DOMAIN: string;
  ACCESS_AUD: string;
}

const MAX_ZIP = 50 << 20;
const MAX_FIELDS = 128 << 10;
const MAX_BODY = MAX_ZIP + MAX_FIELDS + (64 << 10);

/** The JSON fields part: every field required, nothing else accepted. */
const FIELDS: Record<string, { min: number; max: number }> = {
  title: { min: 1, max: 200 },
  issue: { min: 1, max: 60000 },
  version: { min: 1, max: 64 },
  mac: { min: 0, max: 200 },
};

interface Fields {
  schema: 1;
  title: string;
  issue: string;
  version: string;
  mac: string;
}

class Refused extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}

function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json", "Cache-Control": "no-store" },
  });
}

function parseFields(raw: string): Fields {
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch {
    throw new Refused(400, "fields: not valid JSON");
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Refused(400, "fields: expected a JSON object");
  }
  const object = value as Record<string, unknown>;
  const valid = ["schema", ...Object.keys(FIELDS)];
  for (const key of Object.keys(object)) {
    if (!valid.includes(key)) throw new Refused(400, `fields.${key}: unknown field; valid fields: ${valid.join(", ")}`);
  }
  if (object.schema !== 1) throw new Refused(400, "fields.schema: must be 1");
  for (const [key, { min, max }] of Object.entries(FIELDS)) {
    const v = object[key];
    if (typeof v !== "string") throw new Refused(400, `fields.${key}: required string`);
    const n = [...v].length;
    if (n < min || n > max) throw new Refused(400, `fields.${key}: length must be ${min}..${max} characters`);
  }
  return object as unknown as Fields;
}

/** The fenced JSON triage block in ISSUE.md. */
function triage(issue: string): Record<string, unknown> {
  const match = /```json\n([\s\S]*?)\n```/.exec(issue);
  if (!match) throw new Refused(400, "fields.issue: no triage block (```json … ```)");
  try {
    return JSON.parse(match[1]) as Record<string, unknown>;
  } catch {
    throw new Refused(400, "fields.issue: the triage block is not valid JSON");
  }
}

/**
 * A crash's identity across launches and versions: its panic message and
 * thread with numbers (line numbers, ids, indexes) folded away.
 */
async function crashSignature(triageBlock: Record<string, unknown>): Promise<string | null> {
  const crash = triageBlock.crash_signature as { panic?: unknown; thread?: unknown } | null | undefined;
  if (!crash || typeof crash.panic !== "string" || !crash.panic) return null;
  const fold = (s: unknown) => (typeof s === "string" ? s : JSON.stringify(s ?? "")).replace(/[0-9]+/g, "#");
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(`${fold(crash.panic)}|${fold(crash.thread)}`));
  return [...new Uint8Array(digest).slice(0, 8)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

async function scanZip(zip: Blob, issue: string): Promise<void> {
  let list;
  try {
    list = await entries(zip);
  } catch (e) {
    throw new Refused(400, e instanceof ZipError ? e.message : "zip: unreadable");
  }
  const issueEntry = list.find((e) => e.name === "ISSUE.md");
  if (!issueEntry) throw new Refused(400, "zip: no ISSUE.md");
  try {
    if ((await readText(zip, issueEntry)) !== issue) {
      throw new Refused(400, "fields.issue: differs from the zip's ISSUE.md (send the issue text the user previewed)");
    }
    for (const entry of list) {
      const scan = new StreamScan();
      const reader = (await text(zip, entry)).getReader();
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        scan.push(value);
        if (scan.finding) {
          await reader.cancel();
          break;
        }
      }
      const finding = scan.end();
      if (finding) {
        throw new Refused(422, `zip: ${entry.name} contains ${finding}; nothing was stored. Remove it and make the report again`);
      }
    }
  } catch (e) {
    if (e instanceof Refused) throw e;
    throw new Refused(400, e instanceof ZipError ? e.message : `zip: unreadable (${(e as Error).message})`);
  }
}

function footer(id: string, fields: Fields, signature: string | null): string {
  const plain = (s: string) => s.replace(/[`\r\n]/g, " ").trim();
  return [
    "",
    "---",
    "",
    `Report \`${id}\` · Isocast ${plain(fields.version)}${fields.mac ? ` · ${plain(fields.mac)}` : ""}`,
    ...(signature ? [`Crash signature: \`${signature}\``] : []),
    "Filed from Isocast. The diagnostics zip is private; maintainers fetch it by report id.",
    "",
  ].join("\n");
}

async function submit(request: Request, env: Env): Promise<Response> {
  const ip = request.headers.get("CF-Connecting-IP") ?? "unknown";
  if (!(await env.IP_LIMIT.limit({ key: `ip:${ip}` })).success) {
    return json(429, { error: "rate limited: too many reports from this network; try again in a minute" });
  }
  const install = request.headers.get("Isocast-Install-Id") ?? "";
  if (!/^[0-9a-f]{32}$/.test(install)) {
    return json(400, { error: "headers.Isocast-Install-Id: required, 32 lowercase hex characters" });
  }
  if (!(await env.INSTALL_LIMIT.limit({ key: `install:${install}` })).success) {
    return json(429, { error: "rate limited: too many reports from this Mac; try again in a minute" });
  }
  if (!(request.headers.get("Content-Type") ?? "").startsWith("multipart/form-data")) {
    return json(415, { error: "body: expected multipart/form-data with parts fields and zip" });
  }
  const length = Number(request.headers.get("Content-Length") ?? NaN);
  if (!Number.isFinite(length)) return json(411, { error: "headers.Content-Length: required" });
  if (length > MAX_BODY) return json(413, { error: `zip: larger than ${MAX_ZIP >> 20} MB` });

  const form = await request.formData().catch(() => null);
  if (!form) return json(400, { error: "body: malformed multipart" });
  const names = [...form.keys()];
  for (const name of names) {
    if (name !== "fields" && name !== "zip") return json(400, { error: `${name}: unknown part; valid parts: fields, zip` });
  }
  if (names.length !== new Set(names).size) return json(400, { error: "body: each part only once" });
  const rawFields = form.get("fields");
  const zip = form.get("zip");
  if (rawFields === null) return json(400, { error: "fields: required" });
  if (zip === null || typeof zip === "string") return json(400, { error: "zip: required file part" });
  const fieldsText = typeof rawFields === "string" ? rawFields : await rawFields.text();
  if (fieldsText.length > MAX_FIELDS) return json(413, { error: `fields: larger than ${MAX_FIELDS >> 10} KiB` });
  if (zip.size === 0) return json(400, { error: "zip: empty" });
  if (zip.size > MAX_ZIP) return json(413, { error: `zip: larger than ${MAX_ZIP >> 20} MB` });

  const fields = parseFields(fieldsText);
  for (const key of Object.keys(FIELDS) as (keyof typeof FIELDS)[]) {
    const finding = findSecret((fields as unknown as Record<string, string>)[key]);
    if (finding) throw new Refused(422, `fields.${key} contains ${finding}; nothing was stored`);
  }
  const signature = await crashSignature(triage(fields.issue));
  await scanZip(zip, fields.issue);

  const id = [...crypto.getRandomValues(new Uint8Array(16))].map((b) => b.toString(16).padStart(2, "0")).join("");
  const key = `reports/${id}.zip`;
  await env.REPORTS.put(key, zip, {
    httpMetadata: { contentType: "application/zip" },
    customMetadata: { created: new Date().toISOString(), version: fields.version, ...(signature ? { signature } : {}) },
  });
  try {
    const github = await GitHub.connect(env);
    const body = fields.issue + footer(id, fields, signature);
    if (signature) {
      const marker = `Crash signature: \`${signature}\``;
      const existing = (await github.openReports()).find((i) => i.body?.includes(marker));
      if (existing) {
        const comment = await github.comment(existing.number, `Another report with the same crash.\n\n${body}`);
        return json(201, { issue_url: comment.html_url, report_id: id, deduplicated: true });
      }
    }
    const issue = await github.createIssue(fields.title, body, ["bug", "from-app"]);
    return json(201, { issue_url: issue.html_url, report_id: id, deduplicated: false });
  } catch (e) {
    await env.REPORTS.delete(key);
    const message = e instanceof GitHubError ? e.message : "GitHub is unreachable";
    return json(502, { error: `${message}; nothing was stored. Keep the zip and file the issue by hand` });
  }
}

async function fetchZip(request: Request, env: Env, id: string): Promise<Response> {
  if (!(await maintainer(env, request))) {
    return json(401, { error: "maintainers only: Authorization: Bearer <MAINTAINER_TOKEN> or Cloudflare Access" });
  }
  const object = await env.REPORTS.get(`reports/${id}.zip`);
  if (!object) return json(404, { error: `report ${id}: not found` });
  return new Response(object.body, {
    headers: {
      "Content-Type": "application/zip",
      "Content-Disposition": `attachment; filename="isocast-report-${id}.zip"`,
      "Cache-Control": "private, no-store",
    },
  });
}

export default {
  async fetch(request, env): Promise<Response> {
    const url = new URL(request.url);
    try {
      if (url.pathname === "/reports") {
        if (request.method !== "POST") return json(405, { error: "method: use POST" });
        return await submit(request, env);
      }
      const zip = /^\/reports\/([0-9a-f]{32})\/zip$/.exec(url.pathname);
      if (zip) {
        if (request.method !== "GET") return json(405, { error: "method: use GET" });
        return await fetchZip(request, env, zip[1]);
      }
      if (url.pathname === "/") return json(200, { service: "isocast-report-worker", ok: true });
      return json(404, { error: "path: valid paths: POST /reports, GET /reports/:id/zip" });
    } catch (e) {
      if (e instanceof Refused) return json(e.status, { error: e.message });
      console.error("report worker failed", (e as Error).name);
      return json(500, { error: "internal error; nothing was stored" });
    }
  },
} satisfies ExportedHandler<Env>;
