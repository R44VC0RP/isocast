// Runs the worker locally (`wrangler dev`, local R2 and rate limiting) against
// a fake GitHub API and a fake Cloudflare Access key set. Used by
// worker.test.mjs and by serve.mjs (the engine's end-to-end check).

import { spawn } from "node:child_process";
import { generateKeyPairSync, createSign, createVerify, randomBytes } from "node:crypto";
import { mkdtempSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { crc32, deflateRawSync } from "node:zlib";

const here = dirname(fileURLToPath(import.meta.url));
const b64url = (b) => Buffer.from(b).toString("base64url");

/** The fake GitHub: checks the App JWT and installation token, keeps issues. */
export function fakeGitHub(appPublicKey, accessJwk) {
  const state = { issues: [], comments: [], calls: [], failNext: false };
  const installationToken = `fake-installation-${randomBytes(8).toString("hex")}`;
  const verifyJwt = (auth) => {
    const [head, claims, sig] = (auth ?? "").replace(/^Bearer /, "").split(".");
    if (!sig) return false;
    const v = createVerify("RSA-SHA256");
    v.update(`${head}.${claims}`);
    const ok = v.verify(appPublicKey, Buffer.from(sig, "base64url"));
    const body = JSON.parse(Buffer.from(claims, "base64url"));
    return ok && body.iss === "12345" && body.exp > Date.now() / 1000;
  };
  const server = createServer(async (req, res) => {
    let body = "";
    for await (const chunk of req) body += chunk;
    const url = new URL(req.url, "http://fake");
    const send = (status, value) => {
      res.writeHead(status, { "Content-Type": "application/json" });
      res.end(JSON.stringify(value));
    };
    state.calls.push(`${req.method} ${url.pathname}`);
    if (url.pathname === "/_state") return send(200, state);
    if (url.pathname === "/_fail") {
      state.failNext = true;
      return send(200, {});
    }
    if (url.pathname === "/cdn-cgi/access/certs") return send(200, { keys: [accessJwk] });
    if (url.pathname === "/repos/R44VC0RP/isocast/installation" && req.method === "GET") {
      return verifyJwt(req.headers.authorization) ? send(200, { id: 777 }) : send(401, { message: "Bad credentials" });
    }
    if (url.pathname === "/app/installations/777/access_tokens" && req.method === "POST") {
      if (!verifyJwt(req.headers.authorization)) return send(401, { message: "Bad credentials" });
      const request = JSON.parse(body);
      if (request.permissions?.issues !== "write") return send(422, { message: "permissions" });
      return send(201, { token: installationToken });
    }
    if (req.headers.authorization !== `Bearer ${installationToken}`) return send(401, { message: "Bad credentials" });
    if (url.pathname === "/repos/R44VC0RP/isocast/issues" && req.method === "POST") {
      if (state.failNext) {
        state.failNext = false;
        return send(503, { message: "Service Unavailable" });
      }
      const issue = JSON.parse(body);
      const number = state.issues.length + 1;
      state.issues.push({ number, state: "open", ...issue, html_url: `https://github.com/R44VC0RP/isocast/issues/${number}` });
      return send(201, state.issues.at(-1));
    }
    if (url.pathname === "/repos/R44VC0RP/isocast/issues" && req.method === "GET") {
      const label = url.searchParams.get("labels");
      const page = Number(url.searchParams.get("page") ?? 1);
      const open = state.issues.filter((i) => i.state === "open" && (!label || i.labels.includes(label))).reverse();
      return send(200, open.slice((page - 1) * 100, page * 100));
    }
    const comment = /^\/repos\/R44VC0RP\/isocast\/issues\/(\d+)\/comments$/.exec(url.pathname);
    if (comment && req.method === "POST") {
      const number = Number(comment[1]);
      state.comments.push({ issue: number, ...JSON.parse(body) });
      return send(201, { html_url: `https://github.com/R44VC0RP/isocast/issues/${number}#issuecomment-${state.comments.length}` });
    }
    send(404, { message: "Not Found" });
  });
  return { server, state };
}

/** A zip like the engine's: deflated entries. */
export function makeZip(files) {
  const locals = [];
  const central = [];
  let offset = 0;
  for (const [name, text] of Object.entries(files)) {
    const data = Buffer.from(text);
    const packed = deflateRawSync(data);
    const nameBytes = Buffer.from(name);
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4);
    local.writeUInt16LE(8, 8);
    local.writeUInt32LE(crc32(data), 14);
    local.writeUInt32LE(packed.length, 18);
    local.writeUInt32LE(data.length, 22);
    local.writeUInt16LE(nameBytes.length, 26);
    const cd = Buffer.alloc(46);
    cd.writeUInt32LE(0x02014b50, 0);
    cd.writeUInt16LE(20, 4);
    cd.writeUInt16LE(20, 6);
    cd.writeUInt16LE(8, 10);
    cd.writeUInt32LE(crc32(data), 16);
    cd.writeUInt32LE(packed.length, 20);
    cd.writeUInt32LE(data.length, 24);
    cd.writeUInt16LE(nameBytes.length, 28);
    cd.writeUInt32LE(offset, 42);
    locals.push(local, nameBytes, packed);
    central.push(cd, nameBytes);
    offset += 30 + nameBytes.length + packed.length;
  }
  const cdBytes = Buffer.concat(central);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(Object.keys(files).length, 8);
  end.writeUInt16LE(Object.keys(files).length, 10);
  end.writeUInt32LE(cdBytes.length, 12);
  end.writeUInt32LE(offset, 16);
  return Buffer.concat([...locals, cdBytes, end]);
}

/** A Cloudflare Access JWT signed with the fake team key. */
export function accessJwt(h, { aud = "test-aud", exp = Date.now() / 1000 + 300 } = {}) {
  const head = b64url(JSON.stringify({ alg: "RS256", kid: "test-kid" }));
  const claims = b64url(JSON.stringify({ aud: [aud], iss: h.fakeUrl, exp: Math.floor(exp), email: "ryan@example.com" }));
  const sign = createSign("RSA-SHA256");
  sign.update(`${head}.${claims}`);
  return `${head}.${claims}.${b64url(sign.sign(h.accessKey))}`;
}

export async function start({ port = 8787, persist } = {}) {
  // GitHub hands out PKCS#1 PEM files; use the same format here.
  const app = generateKeyPairSync("rsa", { modulusLength: 2048 });
  const access = generateKeyPairSync("rsa", { modulusLength: 2048 });
  const accessJwk = { ...access.publicKey.export({ format: "jwk" }), kid: "test-kid", alg: "RS256", use: "sig" };
  const fake = fakeGitHub(app.publicKey, accessJwk);
  await new Promise((r) => fake.server.listen(0, "127.0.0.1", r));
  const fakeUrl = `http://127.0.0.1:${fake.server.address().port}`;
  const dir = mkdtempSync(join(tmpdir(), "isocast-report-worker-"));
  const maintainerToken = randomBytes(24).toString("hex");
  const pem = app.privateKey.export({ format: "pem", type: "pkcs1" });
  const envFile = join(dir, "test.env");
  writeFileSync(
    envFile,
    `GITHUB_APP_PRIVATE_KEY="${pem.trim().replace(/\n/g, "\\n")}"\nMAINTAINER_TOKEN="${maintainerToken}"\n`,
    { mode: 0o600 },
  );
  const wrangler = spawn(
    join(here, "../node_modules/.bin/wrangler"),
    [
      "dev",
      "--ip", "127.0.0.1",
      "--port", String(port),
      "--persist-to", persist ?? join(dir, "state"),
      "--env-file", envFile,
      "--var", `GITHUB_API:${fakeUrl}`,
      "--var", "GITHUB_APP_ID:12345",
      "--var", `ACCESS_TEAM_DOMAIN:${fakeUrl}`,
      "--var", "ACCESS_AUD:test-aud",
      "--show-interactive-dev-session=false",
    ],
    { cwd: join(here, ".."), env: { ...process.env, WRANGLER_SEND_METRICS: "false", CI: "1" }, stdio: ["ignore", "pipe", "pipe"] },
  );
  let log = "";
  wrangler.stdout.on("data", (d) => (log += d));
  wrangler.stderr.on("data", (d) => (log += d));
  const workerUrl = `http://127.0.0.1:${port}`;
  for (let i = 0; ; i++) {
    try {
      if ((await fetch(workerUrl)).ok) break;
    } catch {
      /* not up yet */
    }
    if (i > 120 || wrangler.exitCode !== null) throw new Error(`wrangler dev did not start:\n${log}`);
    await new Promise((r) => setTimeout(r, 500));
  }
  return {
    workerUrl,
    fakeUrl,
    accessKey: access.privateKey,
    maintainerToken,
    persist: persist ?? join(dir, "state"),
    log: () => log,
    state: async () => (await fetch(`${fakeUrl}/_state`)).json(),
    failNext: () => fetch(`${fakeUrl}/_fail`),
    /** Keys in the local R2 bucket. */
    objects: async () => {
      const r = await (await fetch(`${workerUrl}/cdn-cgi/local/explorer/api/r2/buckets/isocast-bug-reports/objects`)).json();
      return (r.result?.objects ?? r.result ?? []).map((o) => o.key);
    },
    stop: async () => {
      wrangler.kill("SIGTERM");
      fake.server.close();
      await new Promise((r) => wrangler.once("exit", r));
    },
  };
}
