// The Sources dock: the items of the scene being edited, the front one at
// the top, as in OBS. Each row has the source's type icon and name, then
// ISO (records the source to its own file), eye (visible) and lock
// toggles. Click to select, double-click for Properties, drag to restack;
// the toolbar adds (+), removes (−), opens Properties (gear) and moves the
// selected item up and down. The right-click menu is OBS's.

import { useRef, useState } from "preact/hooks";
import { html } from "../lib/html.js";
import { getState, setState, useStore } from "../lib/store.js";
import { KINDS, findScene, findSource, hasVideo, itemIcon, itemLabel, key, sourceName, takenIds, newId, withoutSource } from "../lib/model.js";
import { edit, editingSceneId, replaceSetup, select, toast } from "../lib/session.js";
import { removeItem, reorder, restack, selected, updateItem } from "../lib/items.js";
import { centerToScreen, fitToScreen, resetTransform, rotateBy, stretchToScreen } from "../lib/transform.js";
import { humanize } from "../lib/schema.js";
import { Icon } from "./icons.js";
import { showMenu } from "./popover.js";
import { moved, rowKeys, rowTabIndex, useReorder } from "./reorder.js";
import { RenameInput } from "./scenes.js";
import { Dock, setLayout } from "./docks.js";
import { openDialog } from "./dialog.js";

// ---------- Adding ----------

/** Add a source: the menu of types (OBS's words), each opening the
 * Create/Select Source dialog. */
export function addMenu(at) {
  if (!getState().setup?.scenes?.length) {
    toast("info", "Add a scene first", "Sources live in scenes: add one with + under Scenes.");
    return;
  }
  const items = Object.entries(KINDS).map(([kind, info]) => ({
    label: info.label,
    icon: info.icon,
    run: () => openDialog("create-source", { kind }),
  }));
  items.push({ label: "Scene", icon: "scene", run: () => openDialog("create-source", { kind: "scene" }) });
  items.sort((a, b) => a.label.localeCompare(b.label));
  showMenu(at, items);
}

// ---------- ISO ----------

/** Whether source `id` records to its own file. */
export function isIso(setup, id) {
  const iso = setup?.recording?.iso;
  return !!iso?.enabled && (iso.sources ?? []).includes(id);
}

/** Record source `id` to its own file, or stop. */
export function setIso(id, on) {
  const iso = getState().setup.recording.iso ?? {};
  const list = iso.sources ?? [];
  const sources = on ? [...list.filter((s) => s !== id), id] : list.filter((s) => s !== id);
  return edit({ recording: { iso: { sources, ...(on && !iso.enabled ? { enabled: true } : {}) } } });
}

// ---------- Copy and paste ----------

/** Copy the selected item (⌘C, Edit → Copy). */
export function copySelected() {
  const found = selected(getState());
  if (!found) return false;
  setState({ clipboard: { item: structuredClone(found.item) } });
  return true;
}

/** Paste the copied item into the scene being edited: showing the same
 * source (a reference), or a copy of the source (`duplicate`). */
export function paste(duplicate = false) {
  const { clipboard, setup } = getState();
  const sceneId = editingSceneId();
  const scene = findScene(setup, sceneId);
  if (!clipboard || !scene) return false;
  const item = structuredClone(clipboard.item);
  const patch = {};
  if (duplicate && item.source) {
    const source = findSource(setup, item.source);
    if (source) {
      const name = `${sourceName(source)} 2`;
      const id = newId(name, takenIds(setup));
      patch.sources = [{ ...structuredClone(source), id, name }];
      item.source = id;
      delete item.id;
    }
  }
  // Two items showing one source in a scene need ids of their own.
  const keys = new Set((scene.items ?? []).map(key));
  if (keys.has(key(item))) item.id = newId(itemLabel(setup, item), [...keys, ...takenIds(setup)]);
  patch.scenes = [{ id: sceneId, items: [item] }];
  edit(patch).then((ok) => ok && select({ scene: sceneId, path: [key(item)] }));
  return true;
}

// ---------- Removing ----------

/** How many items in every scene show source `id`. */
function uses(setup, id) {
  let n = 0;
  const walk = (items) => items.forEach((i) => (i.source === id && n++, walk(i.children ?? [])));
  setup.scenes.forEach((s) => walk(s.items ?? []));
  return n;
}

/** Remove an item from its scene. As in OBS, a source no other item
 * shows goes with it (Undo brings it back). */
export function removeFromScene(sceneId, chain) {
  const setup = getState().setup;
  const item = chain[chain.length - 1];
  const source = item.source ? findSource(setup, item.source) : null;
  if (source && hasVideo(source) && uses(setup, source.id) <= 1) {
    setState({ selection: null });
    replaceSetup(withoutSource(setup, source.id));
    toast("info", `Removed “${sourceName(source)}”`, "", { label: "Undo", run: () => replaceSetup(setup) });
    return;
  }
  removeItem(sceneId, chain);
}

/** Remove the selected item (− and ⌫). */
export function removeSelected() {
  const found = selected(getState());
  if (found) removeFromScene(found.scene.id, found.chain);
}

/** Move the selected item: "forward" (up the list), "backward", "front"
 * (top) or "back" (bottom). */
export function orderSelected(where) {
  const found = selected(getState());
  if (found) restack(found.scene.id, found.chain, where);
}

// ---------- Menus ----------

const SCALE_FILTERS = [
  ["point", "Point"],
  ["bilinear", "Bilinear"],
  ["bicubic", "Bicubic"],
  ["lanczos", "Lanczos"],
  ["area", "Area"],
];
const BLENDS = ["normal", "additive", "subtract", "screen", "multiply", "lighten", "darken"];

/** Edit → Order, and the menus' Order submenu. */
export const orderItems = () => [
  { label: "Move Up", run: () => orderSelected("forward") },
  { label: "Move Down", run: () => orderSelected("backward") },
  "-",
  { label: "Move to Top", run: () => orderSelected("front") },
  { label: "Move to Bottom", run: () => orderSelected("back") },
];

/** Edit → Transform, and the menus' Transform submenu. */
export const transformItems = () => [
  { label: "Edit Transform…", hint: "⌘E", run: () => openDialog("transform") },
  { label: "Reset Transform", hint: "⌘R", run: resetTransform },
  "-",
  { label: "Rotate 90° Clockwise", run: () => rotateBy(90) },
  { label: "Rotate 90° Counterclockwise", run: () => rotateBy(-90) },
  { label: "Rotate 180°", run: () => rotateBy(180) },
  "-",
  { label: "Fit to Screen", hint: "⌘F", run: fitToScreen },
  { label: "Stretch to Screen", hint: "⌘S", run: stretchToScreen },
  { label: "Center to Screen", hint: "⌘D", run: () => centerToScreen() },
  { label: "Center Vertically", run: () => centerToScreen("y") },
  { label: "Center Horizontally", run: () => centerToScreen("x") },
];

function itemTransitionItems(item, field, set) {
  const current = item[field]?.kind ?? null;
  const choose = (kind) => set({ [field]: kind ? { kind, duration_ms: item[field]?.duration_ms ?? 300 } : null });
  return [
    { label: "None", checked: !current, run: () => choose(null) },
    { label: "Fade", checked: current === "fade", run: () => choose("fade") },
    { label: "Slide", checked: current === "slide", run: () => choose("slide") },
  ];
}

const pasteItems = () => {
  const has = !!getState().clipboard;
  return [
    { label: "Paste (Reference)", hint: "⌘V", disabled: !has, run: () => paste(false) },
    { label: "Paste (Duplicate)", disabled: !has, run: () => paste(true) },
  ];
};

const addItems = () => {
  const items = Object.entries(KINDS).map(([kind, info]) => ({ label: info.label, run: () => openDialog("create-source", { kind }) }));
  items.push({ label: "Scene", run: () => openDialog("create-source", { kind: "scene" }) });
  return items.sort((a, b) => a.label.localeCompare(b.label));
};

/** The preview's own items (Lock Preview, Preview Scaling), at the top
 * of its right-click menus as in OBS. */
export function previewItems() {
  const { previewLocked, previewScale } = getState().layout;
  const scaleTo = (previewScale) => setLayout({ previewScale });
  return [
    { label: "Lock Preview", checked: previewLocked, run: () => setLayout({ previewLocked: !previewLocked }) },
    {
      label: "Preview Scaling",
      items: [
        { label: "Scale to Window", checked: previewScale !== "canvas", run: () => scaleTo("window") },
        { label: "Canvas (100%)", checked: previewScale === "canvas", run: () => scaleTo("canvas") },
      ],
    },
    "-",
  ];
}

/** The menu for a scene item, from its row or (`onPreview`) the canvas. */
export function itemMenu(event, sceneId, chain, rename, onPreview = false) {
  event.preventDefault();
  event.stopPropagation();
  const item = chain[chain.length - 1];
  select({ scene: sceneId, path: chain.map(key) });
  const setup = getState().setup;
  const source = item.source ? findSource(setup, item.source) : null;
  const set = (fields) => updateItem(sceneId, chain, fields);
  const scaling = item.scale_filter ?? "bilinear";
  const blend = item.blend ?? "normal";
  showMenu(event.type === "contextmenu" ? event : event.currentTarget, [
    ...(onPreview ? previewItems() : []),
    { label: "Add", items: addItems() },
    "-",
    { label: "Copy", hint: "⌘C", run: copySelected },
    ...pasteItems(),
    "-",
    rename && (item.source || item.scene) && { label: "Rename", run: rename },
    { label: "Remove", hint: "⌫", run: () => removeFromScene(sceneId, chain) },
    "-",
    { label: "Order", items: orderItems() },
    { label: "Transform", items: transformItems() },
    "-",
    { label: "Show Transition", items: itemTransitionItems(item, "show_transition", set) },
    { label: "Hide Transition", items: itemTransitionItems(item, "hide_transition", set) },
    "-",
    {
      label: "Scale Filtering",
      items: SCALE_FILTERS.map(([v, label]) => ({ label, checked: scaling === v, run: () => set({ scale_filter: v === "bilinear" ? null : v }) })),
    },
    {
      label: "Blending Mode",
      items: BLENDS.map((v) => ({ label: humanize(v), checked: blend === v, run: () => set({ blend: v === "normal" ? null : v }) })),
    },
    "-",
    source && hasVideo(source) && { label: "Record ISO File", checked: isIso(setup, source.id), run: () => setIso(source.id, !isIso(setup, source.id)) },
    { label: "Lock", checked: !!item.locked, run: () => set({ locked: !item.locked }) },
    "-",
    source && { label: "Filters", run: () => openDialog("filters", { source: source.id }) },
    source && { label: "Properties", run: () => openDialog("properties", { source: source.id }) },
  ]);
}

/** The menu for the preview (`onPreview`) or the Sources list away from
 * any item. */
export function canvasMenu(event, sceneId, onPreview = false) {
  event.preventDefault();
  showMenu(event, [
    ...(onPreview ? previewItems() : []),
    findScene(getState().setup, sceneId) && { label: "Add", items: addItems() },
    ...pasteItems(),
  ]);
}

// ---------- The dock ----------

/** A failed (or still opening) source, flagged in its row. */
export function SourceState({ status }) {
  if (status?.state === "failed") {
    return html`<span class="state-badge failed" title=${status.error ?? "This source couldn’t start."}>
      <${Icon} name="warning" size=${14} />
    </span>`;
  }
  if (status?.state === "opening") {
    return html`<span class="state-badge" title="Starting…"><span class="opening-dot" /></span>`;
  }
  return null;
}

/** Rows to show: items front first, children under their group. */
function rowsOf(items, collapsed, depth = 0, chain = []) {
  const rows = [];
  for (const item of items.slice().reverse()) {
    const itemChain = [...chain, item];
    const path = itemChain.map(key).join("/");
    rows.push({ item, chain: itemChain, depth, path, parent: chain.map(key).join("/") });
    if (item.children?.length && !collapsed.has(path)) {
      rows.push(...rowsOf(item.children, collapsed, depth + 1, itemChain));
    }
  }
  return rows;
}

function rename(item, name) {
  if (item.source) edit({ sources: [{ id: item.source, name }] });
  else if (item.scene) edit({ scenes: [{ id: item.scene, name }] });
}

export function SourcesDock() {
  const setup = useStore((s) => s.setup);
  const sceneId = useStore((s) => editingSceneId(s));
  const selection = useStore((s) => s.selection);
  const running = useStore((s) => s.running);
  const [collapsed, setCollapsed] = useState(() => new Set());
  const [renaming, setRenaming] = useState(null);
  const listRef = useRef();
  const scene = findScene(setup, sceneId);
  const rows = scene ? rowsOf(scene.items ?? [], collapsed) : [];
  const selectedPath = selection?.scene === sceneId ? selection.path.join("/") : null;
  const selectedRow = rows.find((r) => r.path === selectedPath);
  const selectedSource = selectedRow?.item.source ? findSource(setup, selectedRow.item.source) : null;

  const reorderRows = useReorder(listRef, {
    onClick: (index) => {
      const row = rows[index];
      if (row) select({ scene: sceneId, path: row.chain.map(key) });
    },
    onMove: (from, to) => {
      const row = rows[from];
      const siblings = rows.filter((r) => r.parent === row.parent);
      const before = rows.slice(0, to).filter((r) => r.parent === row.parent).length;
      const fromIndex = siblings.indexOf(row);
      // Dropped among another group's rows: keep it where it was.
      const near = rows[Math.min(to, rows.length - 1)];
      const above = rows[to - 1];
      if (near?.parent !== row.parent && above?.parent !== row.parent && !(above && above.path === row.parent)) return;
      const display = moved(siblings, fromIndex, before);
      reorder(sceneId, row.chain.slice(0, -1), display.reverse().map((r) => r.item));
    },
  });

  const toggleGroup = (path) => {
    const next = new Set(collapsed);
    if (next.has(path)) next.delete(path);
    else next.add(path);
    setCollapsed(next);
  };

  const toolbar = html`
    <button class="icon-btn small" title="Add a source" disabled=${!scene} onClick=${(e) => addMenu(e.currentTarget)}><${Icon} name="plus" /></button>
    <button class="icon-btn small" title="Remove the selected source" disabled=${!selectedRow} onClick=${removeSelected}><${Icon} name="minus" /></button>
    <button class="icon-btn small" title="Properties of the selected source" disabled=${!selectedSource}
      onClick=${() => openDialog("properties", { source: selectedSource.id })}><${Icon} name="settings" /></button>
    <span class="toolbar-gap" />
    <button class="icon-btn small" title="Move up" disabled=${!selectedRow} onClick=${() => orderSelected("forward")}><${Icon} name="chevronUp" /></button>
    <button class="icon-btn small" title="Move down" disabled=${!selectedRow} onClick=${() => orderSelected("backward")}><${Icon} name="chevronDown" /></button>
  `;

  return html`<${Dock} title="Sources" toolbar=${toolbar}>
    <div
      class="dock-list"
      ref=${listRef}
      onPointerDown=${(e) => e.target === listRef.current && select(null)}
      onContextMenu=${(e) => e.target === listRef.current && canvasMenu(e, sceneId)}
    >
      ${scene && rows.length === 0 &&
      html`<div class="list-empty">No sources in this scene. Click + below, or right-click here, to add one.</div>`}
      ${!scene && html`<div class="list-empty">No scene.</div>`}
      ${rows.map((row, index) => {
        const { item, depth, path } = row;
        const hidden = item.visible === false;
        const group = item.children?.length > 0;
        const missing = item.source && !findSource(setup, item.source);
        const source = item.source ? findSource(setup, item.source) : null;
        const iso = source && hasVideo(source) ? isIso(setup, source.id) : null;
        return html`<div
          key=${path}
          data-row
          class=${"row" + (path === selectedPath ? " selected" : "") + (hidden ? " hidden-item" : "") + (reorderRows.dragging === index ? " dragging" : "")}
          style=${{ paddingLeft: `${12 + depth * 16}px` }}
          tabindex=${rowTabIndex(index, rows.indexOf(selectedRow))}
          onKeyDown=${(e) =>
            rowKeys(e, listRef, index, rows.length, {
              pick: (j) => select({ scene: sceneId, path: rows[j].chain.map(key) }),
              rename: () => setRenaming(path),
            })}
          onPointerDown=${(e) => reorderRows.start(e, index)}
          onPointerEnter=${() => setState({ hover: { scene: sceneId, path } })}
          onPointerLeave=${() => setState({ hover: null })}
          onDblClick=${(e) => !e.target.closest("button") && source && openDialog("properties", { source: source.id })}
          onContextMenu=${(e) => itemMenu(e, sceneId, row.chain, () => setRenaming(path))}
        >
          ${group
            ? html`<button class="icon-btn small group-toggle" title=${collapsed.has(path) ? "Show what’s in it" : "Fold it away"} onClick=${() => toggleGroup(path)}>
                <${Icon} name=${collapsed.has(path) ? "chevronRight" : "chevronDown"} size=${14} />
              </button>`
            : html`<span class="kind-icon"><${Icon} name=${itemIcon(setup, item)} /></span>`}
          ${renaming === path
            ? html`<${RenameInput}
                value=${itemLabel(setup, item)}
                onDone=${(name) => {
                  setRenaming(null);
                  if (name && name !== itemLabel(setup, item)) rename(item, name);
                }}
              />`
            : html`<span class="label" title=${missing ? `No source “${item.source}” in the setup` : ""}>
                ${itemLabel(setup, item)}${missing && html`<span class="faint"> · missing</span>`}
              </span>`}
          <span class="trail">
            <${SourceState} status=${item.source ? running[item.source] : null} />
            ${iso != null &&
            html`<button
              class=${"icon-btn small iso-toggle" + (iso ? " on" : "")}
              title=${iso ? "Records its own ISO file; click to stop" : "Record its own ISO file"}
              onClick=${() => setIso(source.id, !iso)}
            >
              <${Icon} name="record" size=${14} />
            </button>`}
            <button class="icon-btn small" title=${hidden ? "Show" : "Hide"} onClick=${() => updateItem(sceneId, row.chain, { visible: hidden })}>
              <${Icon} name=${hidden ? "eyeOff" : "eye"} size=${14} />
            </button>
            <button
              class=${"icon-btn small" + (item.locked ? " on-lock" : "")}
              title=${item.locked ? "Unlock" : "Lock"}
              onClick=${() => updateItem(sceneId, row.chain, { locked: !item.locked })}
            >
              <${Icon} name=${item.locked ? "lock" : "unlock"} size=${14} />
            </button>
          </span>
        </div>`;
      })}
      ${reorderRows.line && html`<div class="drop-line" style=${{ top: `${reorderRows.line.top}px` }} />`}
    </div>
  <//>`;
}
