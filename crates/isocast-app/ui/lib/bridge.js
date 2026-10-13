// The link to the native shell: daemon calls (the same method table the CLI
// and MCP use), window commands, and the daemon's events.

const handler = window.webkit?.messageHandlers?.isocast;

/** JSON-RPC error codes the daemon uses, plus the shell's own. */
export const INVALID_REQUEST = -32600;
export const METHOD_NOT_FOUND = -32601;
export const FAILED = -32000;
export const WRONG_STATE = -32001;
export const UNREACHABLE = -32099;

export class RpcError extends Error {
  constructor(code, message) {
    super(message);
    this.code = code;
  }
}

async function post(message) {
  if (!handler) throw new RpcError(UNREACHABLE, "The interface is running outside the Isocast app.");
  const text = await handler.postMessage(JSON.stringify(message));
  const reply = JSON.parse(text);
  if (reply.error) throw new RpcError(reply.error.code, reply.error.message);
  return reply.result;
}

/** Call a daemon method, like `call("switch_scene", {scene: "main"})`. */
export function call(method, params) {
  return post(params === undefined ? { method } : { method, params });
}

/** Run a window command in the shell, like `native("drag")`. */
export function native(command, args = {}) {
  return post({ native: command, ...args });
}

/** Whether the daemon refused a method it doesn't have (an older build;
 * builds before METHOD_NOT_FOUND said so with INVALID_REQUEST). */
export function isUnknownMethod(error) {
  return (
    error?.code === METHOD_NOT_FOUND ||
    (error?.code === INVALID_REQUEST && /unknown variant/.test(error.message))
  );
}

const listeners = new Set();

/** Listen to daemon and shell events; returns a function that stops. */
export function onEvent(listener) {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

window.__isocastEvent = (event) => {
  for (const listener of listeners) {
    try {
      listener(event);
    } catch (error) {
      console.error("event listener failed", error);
    }
  }
};

// Errors in the interface go to the app's stderr too.
const report = (message) => handler?.postMessage(JSON.stringify({ native: "log", message })).catch(() => {});
addEventListener("error", (e) => report(`${e.message} (${e.filename}:${e.lineno})`));
addEventListener("unhandledrejection", (e) => report(`unhandled: ${e.reason?.stack ?? e.reason}`));

/** Snapshot mode (`Isocast --snapshot`) and the view it asked for. */
const query = new URLSearchParams(location.search);
export const SNAPSHOT = query.has("snapshot");
export const ROUTE = query.get("route");
