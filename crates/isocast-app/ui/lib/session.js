// The interface's link to the live engine: the live setup with optimistic
// edits, the scenes on air, recording, and the daemon's events. Every
// change goes through the daemon's methods (`apply`, `switch_scene`,
// `start`…), the same ones the CLI and MCP use.

import { ROUTE, SNAPSHOT, WRONG_STATE, call, isUnknownMethod, native, onEvent } from "./bridge.js";
import { mergePatch } from "./patch.js";
import { getState, preference, savePreference, setState } from "./store.js";
import { pushLevels } from "./levels.js";

// ---------- Toasts ----------

let nextToast = 1;
const toastTimers = new Map();

/** Show a message: kind is "info", "success", "warning" or "error".
 * Errors stay until dismissed; the rest fade after a few seconds. A toast
 * with the same `key` (or, without one, the same words) is updated in
 * place and its time starts again, rather than stacking another. `rows`
 * ([label, value, class] triples) show as a small table. */
export function toast(kind, title, message = "", action = null, { key = null, rows = null } = {}) {
  const same = (t) => (key ? t.key === key : !t.key && t.kind === kind && t.title === title && t.message === message);
  const existing = getState().toasts.find(same);
  const id = existing?.id ?? nextToast++;
  const next = { id, key, kind, title, message, action, rows };
  setState((s) => ({
    toasts: s.toasts.some((t) => t.id === id) ? s.toasts.map((t) => (t.id === id ? next : t)) : [...s.toasts.slice(-2), next],
  }));
  clearTimeout(toastTimers.get(id));
  if (kind !== "error") toastTimers.set(id, setTimeout(() => dismiss(id), kind === "warning" ? 8000 : 4000));
  return id;
}

export function dismiss(id) {
  clearTimeout(toastTimers.get(id));
  toastTimers.delete(id);
  setState((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) }));
}

/** Report a failed daemon call in the daemon's own words. */
export function failed(title, error) {
  toast("error", title, error?.message ?? String(error));
}

// ---------- The live setup, with optimistic edits ----------

// The daemon's setup and revision, and edits not yet confirmed: one in
// flight at a time, in order.
let confirmed = null;
let revision = 0;
const queue = [];
let inflight = null;
let lastSent = 0;
let waiting = null;
let syncTimer = null;

// The confirmed setup at recent revisions: monitors' frames say which
// revision they were drawn from, and the canvas editor draws its boxes at
// that layout, so they move with the picture.
const history = new Map();
const HISTORY = 240;

function confirm(setup, rev) {
  confirmed = setup;
  revision = rev;
  if (!setup) return;
  history.set(rev, setup);
  if (history.size > HISTORY) history.delete(history.keys().next().value);
}

/** The confirmed live setup at revision `rev`, if this window knows it. */
export function setupAt(rev) {
  return history.get(rev) ?? null;
}

function recompute() {
  let setup = confirmed;
  for (const edit of [inflight, ...queue]) {
    if (!edit) continue;
    if (edit.setup) setup = edit.setup;
    else if (setup) setup = mergePatch(setup, edit.patch);
  }
  setState({ setup, revision });
}

/** Read the live setup from the daemon, dropping nothing queued. */
export async function reload() {
  try {
    const live = await call("get");
    confirm(live.setup, live.revision);
    recompute();
    setState({ loaded: true });
    // An apply that removed the scene on air puts another one up.
    const ids = (live.setup?.scenes ?? []).map((s) => s.id);
    const { scene, preview } = getState();
    if ((scene && !ids.includes(scene)) || (preview && !ids.includes(preview))) refreshStatus();
  } catch (error) {
    failed("Couldn’t read the live setup", error);
  }
}

// Undo and redo: the setup as it looked before each change (a drag or a
// stream of edits with the same key counts once).
const undoStack = [];
const redoStack = [];
let lastUndoKey = null;
let lastUndoAt = 0;

function remember(key) {
  const now = performance.now();
  const same = key && key === lastUndoKey && now - lastUndoAt < 1500;
  lastUndoKey = key;
  lastUndoAt = now;
  const current = getState().setup;
  if (same || !current) return;
  undoStack.push(current);
  if (undoStack.length > 100) undoStack.shift();
  redoStack.length = 0;
}

/** Put the setup back as it was before the last change. */
export function undo() {
  const previous = undoStack.pop();
  if (!previous) return false;
  redoStack.push(getState().setup);
  lastUndoKey = null;
  replaceSetup(previous, { undoable: false });
  return true;
}

/** Forget the undo history: after a switch the live setup is another
 * scene collection or profile, which an undo must not overwrite with the
 * one before. */
function forgetUndo() {
  undoStack.length = 0;
  redoStack.length = 0;
  lastUndoKey = null;
}

export function redo() {
  const next = redoStack.pop();
  if (!next) return false;
  undoStack.push(getState().setup);
  lastUndoKey = null;
  replaceSetup(next, { undoable: false });
  return true;
}

/** Change the live setup by a merge patch. Shown at once; sent in order.
 * Edits with the same `key` in a row (a drag) are merged, and sent at most
 * every `interval` ms, as `live` changes (the daemon applies them at once
 * and saves the file when the gesture pauses). Resolves to whether the
 * daemon applied it. */
export function edit(patch, { key = null, interval = 0, undoable = true } = {}) {
  if (undoable) remember(key);
  const last = queue[queue.length - 1];
  let entry;
  if (key && last && last.key === key && last.patch) {
    last.patch = mergePatch(last.patch, patch);
    entry = last;
  } else {
    entry = { patch, key, interval, done: [] };
    queue.push(entry);
  }
  const result = new Promise((resolve) => entry.done.push(resolve));
  recompute();
  pump();
  return result;
}

/** Replace the whole live setup (structural changes made locally).
 * Resolves to whether the daemon applied it. */
export function replaceSetup(setup, { undoable = true } = {}) {
  if (undoable) remember(null);
  const entry = { setup, done: [] };
  const result = new Promise((resolve) => entry.done.push(resolve));
  queue.push(entry);
  recompute();
  pump();
  return result;
}

/** Whether edits are waiting to reach the daemon. */
export const editing = () => !!inflight || queue.length > 0;

async function pump() {
  if (inflight || waiting || !queue.length) return;
  const wait = (queue[0].interval || 0) - (performance.now() - lastSent);
  if (wait > 0) {
    waiting = setTimeout(() => {
      waiting = null;
      pump();
    }, wait);
    return;
  }
  inflight = queue.shift();
  lastSent = performance.now();
  const change = inflight.setup ? { setup: inflight.setup } : { patch: inflight.patch };
  try {
    const applied = await call("apply", { revision, ...change, ...(inflight.key ? { live: true } : {}) });
    confirm(inflight.setup ?? mergePatch(confirmed, inflight.patch), applied.revision);
    for (const resolve of inflight.done) resolve(true);
  } catch (error) {
    const stale = error.code === WRONG_STATE && /revision/.test(error.message);
    if (stale && inflight.patch && !inflight.retried) {
      // Changed elsewhere meanwhile: take that, and make this edit on top.
      await reload();
      queue.unshift({ ...inflight, retried: true });
    } else {
      failed(stale ? "The setup changed elsewhere; your change wasn’t applied" : "Couldn’t apply the change", error);
      for (const resolve of inflight.done) resolve(false);
      await reload();
    }
  } finally {
    inflight = null;
    recompute();
    if (queue.length) pump();
    else scheduleSync();
  }
}

// Once edits settle, read the daemon's own copy (it fills in defaults).
function scheduleSync() {
  clearTimeout(syncTimer);
  syncTimer = setTimeout(() => {
    if (!editing()) reload();
  }, 500);
}

// ---------- Scene collections and profiles ----------

/** Read the library: the scene collections and profiles, and which are in
 * use (the Profile and Scene Collection menus, ui/library.js). */
export async function loadLibrary() {
  try {
    setState({ library: await call("library") });
  } catch (error) {
    // An engine from before P.1 has none; the menus then offer nothing.
    if (!isUnknownMethod(error)) native("log", { message: `library: ${error.message}` }).catch(() => {});
    setState({ library: null });
  }
}

// ---------- Engine state ----------

// When this window last switched scenes itself; a status read just after
// may predate the switch.
let switchedAt = -Infinity;

export async function refreshStatus(quiet = false) {
  try {
    const asked = performance.now();
    const status = await call("status");
    const running = Object.fromEntries((status.engine?.sources ?? []).map((s) => [s.source, s]));
    const scenes = asked > switchedAt + 300 ? { scene: status.scene ?? null, preview: status.preview ?? null } : {};
    const streaming = getState().busy.stream ? {} : { stream: status.stream ?? null };
    setState({ status, running, ...scenes, ...streaming });
  } catch (error) {
    if (!quiet && getState().connection.state === "connected") failed("Couldn’t read the engine’s status", error);
  }
}

let devicesLoading = null;

/** The devices on this Mac (cached; `fresh` asks again). */
export function loadDevices(fresh = false) {
  if (getState().devices && !fresh) return Promise.resolve(getState().devices);
  devicesLoading ??= call("devices")
    .then((devices) => {
      setState({ devices });
      return devices;
    })
    .catch((error) => {
      failed("Couldn’t list this Mac’s devices", error);
      return null;
    })
    .finally(() => {
      devicesLoading = null;
    });
  return devicesLoading;
}

/** The scene whose items are being edited: the one in preview in studio
 * mode, else the one on air. */
export function editingSceneId(state = getState()) {
  const scenes = state.setup?.scenes ?? [];
  const id = state.studio ? state.preview ?? state.scene : state.scene;
  return scenes.some((s) => s.id === id) ? id : scenes[0]?.id ?? null;
}

/** Click on a scene: on air, or into preview in studio mode. With
 * `applied` (a pending edit adding the scene), wait for it first. */
export async function chooseScene(id, applied = null) {
  switchedAt = performance.now();
  const state = getState();
  setState({ selection: null });
  if (applied) {
    setState(state.studio ? { preview: id } : { scene: id });
    if (!(await applied)) return;
  }
  if (state.studio) {
    setState({ preview: id });
    try {
      await call("preview", { scene: id });
    } catch (error) {
      failed("Couldn’t put the scene in preview", error);
      refreshStatus();
    }
    return;
  }
  setState({ scene: id });
  try {
    await call("switch_scene", { scene: id });
  } catch (error) {
    failed("Couldn’t switch scenes", error);
    refreshStatus();
  }
}

/** Studio mode: put the preview on air, with `transition` (a quick
 * transition) or the setup's. */
export async function take(transition = null) {
  switchedAt = performance.now();
  const { scene, preview } = getState();
  if (!preview) return;
  setState({ scene: preview, preview: scene });
  try {
    await call("take", transition ? { transition } : {});
  } catch (error) {
    failed("Couldn’t take the preview to air", error);
    refreshStatus();
  }
}

export async function setStudio(on) {
  setState({ studio: on, selection: null });
  savePreference("studio", on);
  const { preview, scene } = getState();
  if (on && !preview && scene) {
    setState({ preview: scene });
    try {
      await call("preview", { scene });
    } catch (error) {
      failed("Couldn’t start studio mode", error);
    }
  }
}

export function select(selection) {
  setState({ selection });
}

// ---------- Outputs ----------

export async function toggleRecording() {
  const { status, busy } = getState();
  if (busy.record) return;
  const recording = status?.state === "recording" || status?.state === "starting";
  setState({ busy: { ...busy, record: true } });
  try {
    if (recording) {
      const report = await call("stop");
      const files = report.files?.length ?? 0;
      toast("success", "Recording saved", `${files} file${files === 1 ? "" : "s"} in ${report.folder}`, {
        label: "Show in Finder",
        run: () => native("reveal", { path: report.manifest ?? report.folder }),
      });
    } else {
      setState((s) => ({ status: { ...(s.status ?? {}), state: "starting" } }));
      await call("start", {});
    }
  } catch (error) {
    failed(recording ? "Couldn’t stop recording" : "Couldn’t start recording", error);
  } finally {
    setState((s) => ({ busy: { ...s.busy, record: false } }));
    refreshStatus();
  }
}

/** Whether the live setup can stream now: a `[stream]` section and a
 * stored key. If not, the stream settings open instead. */
function streamReady() {
  const { setup, stream } = getState();
  if (!setup?.stream) return "Choose where to stream first.";
  if (stream && !stream.key_set) return "Add your stream key first. It’s kept in the Keychain, never in the setup.";
  return null;
}

export async function toggleStream() {
  const { stream, busy } = getState();
  if (busy.stream) return;
  const on = ["live", "connecting", "reconnecting"].includes(stream?.state);
  if (!on) {
    const missing = streamReady();
    if (missing) {
      setState({ dialog: { type: "settings", tab: "stream", notice: missing } });
      return;
    }
  }
  setState({ busy: { ...busy, stream: true } });
  if (!on) setState((s) => ({ stream: { ...(s.stream ?? {}), state: "connecting" } }));
  try {
    const status = await call(on ? "stream_stop" : "stream_start");
    setState({ stream: status });
  } catch (error) {
    if (isUnknownMethod(error)) {
      toast("info", "Streaming isn’t in this engine yet", "Go live arrives with the streaming update to isocastd.");
    } else {
      failed(on ? "Couldn’t stop the stream" : "Couldn’t go live", error);
    }
    refreshStatus(true);
  } finally {
    setState((s) => ({ busy: { ...s.busy, stream: false } }));
  }
}

// ---------- Dropped frames ----------

// While frames drop, the daemon warns once a second for each file (and
// for the stream), and each `status` event carries the same counts. They
// show as one toast, kept up to date while drops continue, rather than a
// stack of them.
const FILE_DROPS = /^\S+ (dropped \d+ frame\(s\)|had \d+ audio dropout\(s\)) at \d+ s \(\d+ so far\)$/;
const STREAM_DROPS = /^the stream dropped \d+ frame\(s\) \((\d+) so far/;
const drops = { files: new Map(), stream: 0, rows: null };

const plural = (n, word) => `${n} ${word}${n === 1 ? "" : "s"}`;

function fileLabel(source) {
  if (source === "program") return "Program";
  return getState().setup?.sources?.find((s) => s.id === source)?.name || source;
}

/** The recording's (or the stream's) details popover, over its item in
 * the status bar. */
function openDetails(which) {
  const button = document.querySelector(which === "recording" ? ".status-item.rec" : ".status-item.live");
  if (!button) return;
  const r = button.getBoundingClientRect();
  const anchor = { left: r.left, top: r.top, right: r.right, bottom: r.bottom };
  setState(which === "recording" ? { details: anchor } : { streamDetails: anchor });
}

/** Counts from a recording's status (`files`) or the stream's total so
 * far; the toast shows (or updates) when any of them went up. */
function noteDrops(files, streamTotal = null) {
  let grew = false;
  for (const f of files ?? []) {
    const seen = drops.files.get(f.source) ?? { frames: 0, audio: 0 };
    const now = { frames: f.dropped_frames ?? 0, audio: f.audio_dropouts ?? 0 };
    if (now.frames > seen.frames || now.audio > seen.audio) grew = true;
    drops.files.set(f.source, now);
  }
  if (streamTotal != null) {
    if (streamTotal > drops.stream) grew = true;
    drops.stream = streamTotal;
  }
  if (!grew) return;
  const rows = [];
  let frames = drops.stream;
  for (const [source, n] of drops.files) {
    frames += n.frames;
    if (n.frames) rows.push([fileLabel(source), `${n.frames} dropped`, "bad"]);
    if (n.audio) rows.push([`${fileLabel(source)} audio`, plural(n.audio, "gap"), "warn"]);
  }
  if (drops.stream) rows.push(["Stream", `${drops.stream} dropped`, "bad"]);
  const recording = getState().status?.state === "recording";
  drops.rows = rows;
  toast(
    "warning",
    frames ? "Dropping frames" : "Audio dropouts",
    recording ? "The recording carries on." : "The stream carries on; recordings keep every frame.",
    { label: "Details", run: () => openDetails(recording ? "recording" : "stream") },
    { key: "drops", rows },
  );
}

/** The recording stopped: a drops toast still up says so. */
function dropsStopped() {
  if (!getState().toasts.some((t) => t.key === "drops") || !drops.rows) return;
  toast("warning", "The recording dropped frames", "In the files it just finished:", null, { key: "drops", rows: drops.rows });
}

const sentence = (text) => (text ? text.charAt(0).toUpperCase() + text.slice(1) : "");

// ---------- Events ----------

const menuActions = new Map();

/** Handle a menu command from the native menu bar (`run` gets the event:
 * {action, id} for the Profile and Scene Collection menus' entries). */
export function onMenu(action, run) {
  menuActions.set(action, run);
}

function handle(event) {
  switch (event.type) {
    case "connection":
      setState({ connection: event });
      break;
    case "watching":
      // A daemon that started again counts revisions from the start.
      history.clear();
      confirm(event.live?.setup ?? null, event.live?.revision ?? 0);
      queue.length = 0;
      recompute();
      setState({ loaded: true });
      refreshStatus();
      loadLibrary();
      break;
    case "library_changed":
      // Made, renamed, deleted, imported or switched, by this window or
      // any other client.
      if (event.action === "switched" || event.action === "migrated") forgetUndo();
      loadLibrary();
      break;
    case "setup_changed":
      if (event.revision > revision && !editing()) reload();
      break;
    case "scene_changed":
      setState({ scene: event.scene });
      break;
    case "preview_changed":
      setState({ preview: event.scene });
      break;
    case "started":
      drops.files.clear();
      refreshStatus();
      break;
    case "status":
      setState((s) => ({ status: { ...(s.status ?? {}), state: "recording", live: event.live } }));
      if (event.live?.levels) pushLevels(event.live.levels);
      noteDrops(event.live?.files);
      break;
    case "levels":
      pushLevels(event.levels);
      break;
    case "source_changed":
      if (event.status?.source) setState((s) => ({ running: { ...s.running, [event.status.source]: event.status } }));
      break;
    case "stream":
      if (event.stream) {
        const before = getState().stream?.state;
        if (!["live", "connecting", "reconnecting"].includes(before)) drops.stream = 0;
        setState({ stream: event.stream });
        if (event.stream.state === "failed" && before !== "failed") {
          toast("error", "The stream stopped", event.stream.last_error ?? "It couldn’t stay connected.");
        } else if (event.stream.state === "reconnecting" && before === "live") {
          toast("warning", "Reconnecting the stream", event.stream.last_error ?? "The connection dropped.");
        }
      }
      break;
    case "engine_changed":
      if (!event.running) setState({ running: {} });
      else refreshStatus();
      break;
    case "stopped":
      dropsStopped();
      refreshStatus();
      if (event.error) toast("error", "The recording stopped", event.error);
      break;
    case "warning": {
      // Drops are counted from `status` (see noteDrops).
      if (FILE_DROPS.test(event.message)) break;
      const stream = STREAM_DROPS.exec(event.message);
      if (stream) noteDrops(null, Number(stream[1]));
      else toast("warning", "Warning", sentence(event.message));
      break;
    }
    case "menu":
      menuActions.get(event.action)?.(event);
      break;
    case "window":
      setState({ fullScreen: !!event.full_screen });
      break;
    default:
      break;
  }
}

/** Start listening, and read where things stand. */
export function boot() {
  setState({
    studio: preference("studio", false),
    dialog: ROUTE === "settings" ? { type: "settings" } : null,
    onboarding: ROUTE === "onboarding",
  });
  onEvent(handle);
  native("connection")
    .then((event) => {
      if (event) handle(event);
      if (event?.state === "connected") {
        reload();
        refreshStatus();
        loadLibrary();
      }
    })
    .catch(() => {});
  native("window")
    .then((w) => setState({ fullScreen: !!w?.full_screen }))
    .catch(() => {});
  if (SNAPSHOT) signalReady();
  // The engine's health (missed frames, sources) every few seconds while
  // the window shows.
  setInterval(() => {
    if (!document.hidden && getState().connection.state === "connected" && getState().loaded) refreshStatus(true);
  }, 3000);
}

// In snapshot mode, tell the shell once the first real screen is up: the
// setup (or the welcome screen) is loaded, the engine can't be reached, or
// it is still starting after a few seconds (the screen says so).
function signalReady() {
  const started = performance.now();
  const check = () => {
    const { connection, loaded } = getState();
    const settled = loaded || connection.state === "failed" || performance.now() - started > 6000;
    if (settled) {
      requestAnimationFrame(() => requestAnimationFrame(() => native("ready").catch(() => {})));
      return true;
    }
    return false;
  };
  if (check()) return;
  const timer = setInterval(() => check() && clearInterval(timer), 50);
}
