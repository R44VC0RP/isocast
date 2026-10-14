// Keeps the worker and fake GitHub running for an end-to-end check with a
// real engine: `node test/serve.mjs <info.json> [port] [persist-dir]` writes
// the worker URL, fake GitHub URL and maintainer token (mode 0600) and runs
// until SIGTERM/SIGINT.

import { writeFileSync } from "node:fs";
import { start } from "./harness.mjs";

const [info, port = "8790", persist] = process.argv.slice(2);
if (!info) {
  console.error("usage: node test/serve.mjs <info.json> [port] [persist-dir]");
  process.exit(2);
}
const h = await start({ port: Number(port), persist });
writeFileSync(
  info,
  JSON.stringify({ workerUrl: h.workerUrl, fakeUrl: h.fakeUrl, maintainerToken: h.maintainerToken, persist: h.persist }),
  { mode: 0o600 },
);
console.log(`worker ${h.workerUrl}, fake GitHub ${h.fakeUrl}/_state`);
const stop = async () => {
  await h.stop();
  process.exit(0);
};
process.on("SIGTERM", stop);
process.on("SIGINT", stop);
