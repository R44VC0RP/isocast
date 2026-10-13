// Adding a source, OBS's way: pick a type from + (or Add in a menu), then
// Create/Select Source: a new one with a name, or one already in the
// setup, and whether it starts visible. A new source's Properties open
// next; types that can't exist without a file or an app (an image, a
// media file) are added once Properties says what.

import { useEffect, useState } from "preact/hooks";
import { html } from "../lib/html.js";
import { getState, useStore } from "../lib/store.js";
import { KINDS, findScene, findSource, hasVideo, key, newId, sceneName, showsScene, sourceName, takenIds } from "../lib/model.js";
import { edit, editingSceneId, loadDevices, select, toast } from "../lib/session.js";
import { updateItem } from "../lib/items.js";
import { sourceSize } from "../lib/sizes.js";
import { Dialog, closeDialog, openDialog, registerDialog } from "./dialog.js";

/** The settings a new source of `kind` starts with. */
export function starting(kind, devices) {
  switch (kind) {
    case "display":
      return { display: devices?.displays?.find((d) => d.main)?.id ?? "main" };
    case "camera":
      return { device: devices?.cameras?.find((c) => !c.suspended)?.name ?? "" };
    case "microphone":
      return { device: "default" };
    case "window":
    case "application":
    case "application_audio":
      return { app: "" };
    case "image":
      return { path: "" };
    case "slideshow":
    case "media":
      return { files: [] };
    case "text":
      return { text: "Text", size: 96, weight: 600, color: "#eeeae2" };
    case "browser":
      return { url: "https://" };
    case "color":
      return { color: "#2e3338" };
    default:
      return {};
  }
}

/** What has to be filled in before a source of `kind` can be added. */
export function missing(kind, settings) {
  if (kind === "image" && !settings.path) return "Choose an image file.";
  if ((kind === "slideshow" || kind === "media") && !settings.files?.length) return "Choose at least one file.";
  if (kind === "camera" && !settings.device) return "Choose a device.";
  if ((kind === "window" || kind === "application" || kind === "application_audio") && !settings.app) return "Choose an app.";
  if (kind === "text" && !settings.text?.trim() && !settings.file) return "Write the text.";
  if (kind === "browser" && !/^(https?|file):\/\/./.test(settings.url ?? "") && !settings.file) return "Enter the page’s URL.";
  return null;
}

/** The size the live engine reports once it has opened `id` (it says so
 * with a `source_changed` event), waiting up to 3 s. */
async function openedSize(id) {
  for (let i = 0; i < 30 && id; i++) {
    const status = getState().running?.[id];
    if (status?.width && status?.height) return { width: status.width, height: status.height };
    if (status?.state === "failed") return null;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  return null;
}

/** Size a new item from its source's picture: its own size if it fits on
 * the canvas, else as large as fits; centred. */
async function placeNew(sceneId, itemKey) {
  const setup = getState().setup;
  const item = findScene(setup, sceneId)?.items?.find((i) => key(i) === itemKey);
  const source = findSource(setup, item?.source);
  const size = (await openedSize(source?.id)) ?? (await sourceSize(setup, source));
  if (!item || !size?.width || !size?.height) return;
  const canvas = setup.canvas;
  const scale = Math.min(1, canvas.width / size.width, canvas.height / size.height);
  const width = Math.max(1, Math.round(size.width * scale));
  const height = Math.max(1, Math.round(size.height * scale));
  const rect = { x: Math.round((canvas.width - width) / 2), y: Math.round((canvas.height - height) / 2), width, height };
  const now = findScene(getState().setup, sceneId)?.items?.find((i) => key(i) === itemKey);
  if (now) updateItem(sceneId, [now], { rect });
}

/** An item for scene `sceneId` showing `fields` (a source or a scene),
 * the size of the canvas, with an id of its own if the scene shows that
 * already. */
function newItem(sceneId, fields, visible) {
  const setup = getState().setup;
  const canvas = setup.canvas;
  const item = { ...fields, rect: { x: 0, y: 0, width: canvas.width, height: canvas.height }, ...(visible ? {} : { visible: false }) };
  const keys = (findScene(setup, sceneId)?.items ?? []).map(key);
  if (keys.includes(key(item))) item.id = newId(sourceName(findSource(setup, item.source)) || item.scene, [...keys, ...takenIds(setup)]);
  return item;
}

/** Add a source of `kind` (and, for video, an item showing it in scene
 * `sceneId`). Resolves to its id, or null if the daemon refused. */
export async function addSource({ sceneId, kind, settings, name, visible = true }) {
  const setup = getState().setup;
  const id = newId(name || KINDS[kind]?.label || kind, takenIds(setup));
  const source = { id, name: name || KINDS[kind]?.label, kind, ...settings };
  const patch = { sources: [source] };
  const video = hasVideo(source);
  let item = null;
  if (video) {
    item = newItem(sceneId, { source: id }, visible);
    patch.scenes = [{ id: sceneId, items: [item] }];
  }
  if (KINDS[kind]?.audio) {
    patch.recording = { program: { audio: [...(setup.recording.program?.audio ?? []), id] } };
  }
  if (!(await edit(patch))) return null;
  if (video) {
    select({ scene: sceneId, path: [key(item)] });
    placeNew(sceneId, key(item));
  } else {
    toast("success", `Added “${source.name}”`, "It’s in the program mix: find it in the Audio Mixer.");
  }
  return id;
}

/** Name for a new source of `kind`: its type, numbered after the first. */
function defaultName(kind, setup) {
  const base = kind === "scene" ? "Scene" : KINDS[kind]?.label ?? kind;
  const names = new Set((setup?.sources ?? []).map(sourceName));
  if (!names.has(base)) return base;
  for (let n = 2; ; n++) if (!names.has(`${base} ${n}`)) return `${base} ${n}`;
}

/** Create/Select Source. */
function CreateSourceDialog({ kind }) {
  const setup = useStore((s) => s.setup);
  const sceneId = useStore((s) => editingSceneId(s));
  const devices = useStore((s) => s.devices);
  const scene = findScene(setup, sceneId);
  const isScene = kind === "scene";
  const audioOnly = !isScene && !KINDS[kind]?.video;
  const existing = isScene
    ? (setup?.scenes ?? []).filter((s) => s.id !== sceneId && !showsScene(setup, s.id, sceneId))
    : audioOnly
      ? []
      : (setup?.sources ?? []).filter((s) => s.kind === kind);
  const [mode, setMode] = useState(isScene ? "existing" : "new");
  const [name, setName] = useState(() => defaultName(kind, setup));
  const [pick, setPick] = useState(existing[0]?.id ?? null);
  const [visible, setVisible] = useState(true);
  useEffect(() => {
    loadDevices();
  }, []);
  if (!scene) return null;
  const title = isScene ? "Scene" : KINDS[kind]?.label ?? kind;
  const accept = async () => {
    if (mode === "existing") {
      if (!pick) return;
      closeDialog();
      const fields = isScene ? { scene: pick } : { source: pick };
      const item = newItem(sceneId, fields, visible);
      if (await edit({ scenes: [{ id: sceneId, items: [item] }] })) {
        select({ scene: sceneId, path: [key(item)] });
        if (!isScene) placeNew(sceneId, key(item));
      }
      return;
    }
    const settings = starting(kind, devices);
    const finalName = name.trim() || defaultName(kind, setup);
    if (missing(kind, settings)) {
      // Not valid yet: Properties collects what it needs, then adds it.
      openDialog("properties", { pending: { kind, settings, name: finalName, visible, sceneId } });
      return;
    }
    closeDialog();
    const id = await addSource({ sceneId, kind, settings, name: finalName, visible });
    if (id) openDialog("properties", { source: id, created: true });
  };
  const taken = mode === "new" && (setup?.sources ?? []).some((s) => sourceName(s) === name.trim());
  return html`<${Dialog}
    title="Create/Select Source"
    width=${440}
    onCancel=${closeDialog}
    onAccept=${accept}
    buttons=${html`<button class="btn" onClick=${closeDialog}>Cancel</button>
      <button class="btn primary" disabled=${mode === "existing" ? !pick : !name.trim() || taken} onClick=${accept}>OK</button>`}
  >
    <div class="create-source">
      <div class="dialog-heading">${title}${!isScene && html` · ${sceneName(scene)}`}</div>
      ${!isScene &&
      html`<label class="radio">
          <input type="radio" name="mode" checked=${mode === "new"} onChange=${() => setMode("new")} />
          <span>Create new</span>
        </label>
        <div class=${"field indent" + (mode === "new" ? "" : " disabled")}>
          <input value=${name} spellcheck="false" ref=${(el) => el && mode === "new" && !el.dataset.f && ((el.dataset.f = "1"), el.focus(), el.select())}
            onInput=${(e) => setName(e.currentTarget.value)} />
        </div>
        ${taken && html`<div class="field-note bad indent">A source is already called that.</div>`}`}
      ${!audioOnly &&
      html`<label class="radio">
          <input type="radio" name="mode" checked=${mode === "existing"} disabled=${!existing.length} onChange=${() => setMode("existing")} />
          <span>Add Existing</span>
        </label>
        <div class=${"pick-list indent" + (mode === "existing" ? "" : " disabled")}>
          ${existing.length === 0 && html`<div class="faint pick-empty">${isScene ? "No other scenes can go in this one." : "None in this setup yet."}</div>`}
          ${existing.map(
            (s) => html`<button key=${s.id} class=${"pick" + (pick === s.id ? " on" : "")} onClick=${() => (setMode("existing"), setPick(s.id))}
              onDblClick=${() => (setMode("existing"), setPick(s.id), setTimeout(accept))}>
              ${isScene ? sceneName(s) : sourceName(s)}
            </button>`,
          )}
        </div>`}
      ${!audioOnly &&
      html`<label class="check">
        <input type="checkbox" checked=${visible} onChange=${(e) => setVisible(e.currentTarget.checked)} />
        <span>Make source visible</span>
      </label>`}
      ${audioOnly && html`<div class="field-note">Audio sources play in every scene; it goes in the Audio Mixer and the program mix.</div>`}
    </div>
  <//>`;
}

registerDialog("create-source", CreateSourceDialog);

/** Helpers for scripts: add a source without dialogs. */
export const quickAdd = (kind, settings = {}, name = "") =>
  addSource({ sceneId: editingSceneId(), kind, settings: { ...starting(kind, getState().devices), ...settings }, name, visible: true });
