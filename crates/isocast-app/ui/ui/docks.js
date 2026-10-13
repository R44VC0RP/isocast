// The dock row along the bottom of the window, in OBS's default
// arrangement: Scenes | Sources | Audio Mixer | Scene Transitions |
// Controls, each a titled panel. A splitter above the row and between
// docks resizes them; sizes, hidden docks and the lock are per-viewer
// preferences (View → Reset UI, or Docks → Reset Docks, restore them).

import { html } from "../lib/html.js";
import { getState, preference, savePreference, setState, useStore } from "../lib/store.js";

/** The docks, left to right. The mixer takes what's left. */
export const DOCKS = [
  { id: "scenes", title: "Scenes", width: 210, min: 140 },
  { id: "sources", title: "Sources", width: 250, min: 170 },
  { id: "mixer", title: "Audio Mixer", min: 240 },
  { id: "transitions", title: "Scene Transitions", width: 200, min: 160 },
  { id: "controls", title: "Controls", width: 200, min: 160 },
];

const DEFAULT_LAYOUT = {
  height: null,
  widths: Object.fromEntries(DOCKS.filter((d) => d.width).map((d) => [d.id, d.width])),
  hidden: [],
  locked: false,
  sourceToolbar: true,
  statusBar: true,
  previewLocked: false,
  // Preview Scaling: "window" (as large as fits) or "canvas" (one canvas
  // pixel to a screen pixel, scrolling if it doesn't fit).
  previewScale: "window",
};

/** The layout as saved, with defaults for anything missing. */
export function loadLayout() {
  const saved = preference("layout", {});
  return { ...DEFAULT_LAYOUT, ...saved, widths: { ...DEFAULT_LAYOUT.widths, ...(saved.widths ?? {}) } };
}

/** Change the layout and keep it. */
export function setLayout(fields) {
  const layout = { ...getState().layout, ...fields };
  setState({ layout });
  savePreference("layout", layout);
}

/** View → Reset UI: every dock back where it was, and the bars shown. */
export function resetLayout() {
  setLayout({ ...DEFAULT_LAYOUT });
}

/** Docks → Reset Docks: sizes and shown docks, not the bars. */
export function resetDocks() {
  setLayout({ height: null, widths: DEFAULT_LAYOUT.widths, hidden: [], locked: false });
}

export function toggleDock(id) {
  const hidden = getState().layout.hidden;
  setLayout({ hidden: hidden.includes(id) ? hidden.filter((h) => h !== id) : [...hidden, id] });
}

/** The dock row's height when none is saved: about a third of the window. */
export const defaultDocksHeight = () => Math.round(Math.min(320, Math.max(224, innerHeight * 0.3)));

/** Drag to resize: calls `set(start + delta)` while dragging and `done`
 * with the last value. */
export function dragResize(event, start, axis, set, done) {
  if (event.button !== 0) return;
  event.preventDefault();
  const origin = axis === "x" ? event.clientX : event.clientY;
  let value = start;
  const move = (e) => {
    value = set(start + (axis === "x" ? e.clientX : e.clientY) - origin);
  };
  const up = () => {
    removeEventListener("pointermove", move);
    removeEventListener("pointerup", up);
    document.body.style.cursor = "";
    done(value);
  };
  document.body.style.cursor = axis === "x" ? "col-resize" : "row-resize";
  addEventListener("pointermove", move);
  addEventListener("pointerup", up);
}

const clamp = (value, min, max) => Math.round(Math.min(max, Math.max(min, value)));

/** A dock: its title along the top, then its content and toolbar. */
export function Dock({ title, toolbar, onContextMenu, children, class: cls = "" }) {
  return html`<section class=${"dock " + cls} onContextMenu=${onContextMenu}>
    <div class="dock-title">${title}</div>
    <div class="dock-content">${children}</div>
    ${toolbar && html`<div class="dock-toolbar">${toolbar}</div>`}
  </section>`;
}

/** The row of docks: `docks` maps each dock's id to its content. */
export function DockRow({ docks }) {
  const layout = useStore((s) => s.layout);
  const shown = DOCKS.filter((d) => !layout.hidden.includes(d.id) && docks[d.id]);
  // A splitter between two docks resizes the one with a fixed width next
  // to it (the left one, unless that is the mixer).
  const resize = (event, left, right) => {
    if (layout.locked) return;
    const fixed = left.width ? left : right;
    const sign = fixed === left ? 1 : -1;
    const el = event.currentTarget.parentElement.querySelector(`[data-dock="${fixed.id}"]`);
    const start = el?.getBoundingClientRect().width ?? layout.widths[fixed.id];
    dragResize(
      event,
      0,
      "x",
      (delta) => {
        const width = clamp(start + sign * delta, fixed.min, 640);
        setState((s) => ({ layout: { ...s.layout, widths: { ...s.layout.widths, [fixed.id]: width } } }));
        return width;
      },
      () => setLayout({}),
    );
  };
  return html`<div class=${"dock-row" + (layout.locked ? " locked" : "")}>
    ${shown.map((dock, i) => {
      const width = dock.width ? layout.widths[dock.id] : null;
      const style = width ? { flex: `0 1 ${width}px`, minWidth: `${dock.min}px` } : { flex: "1 1 0", minWidth: `${dock.min}px` };
      return html`
        ${i > 0 &&
        html`<div class="dock-splitter" key=${"split-" + dock.id} onPointerDown=${(e) => resize(e, shown[i - 1], dock)} />`}
        <div class="dock-slot" key=${dock.id} data-dock=${dock.id} style=${style}>${docks[dock.id]}</div>
      `;
    })}
  </div>`;
}

/** The splitter between the preview and the docks. */
export function RowSplitter() {
  const layout = useStore((s) => s.layout);
  const down = (event) => {
    if (layout.locked) return;
    const start = layout.height ?? defaultDocksHeight();
    dragResize(
      event,
      start,
      "y",
      // Dragging up makes the docks taller.
      (v) => {
        const height = clamp(2 * start - v, 120, innerHeight - 220);
        setState((s) => ({ layout: { ...s.layout, height } }));
        return height;
      },
      () => setLayout({}),
    );
  };
  return html`<div class=${"row-splitter" + (layout.locked ? " locked" : "")} onPointerDown=${down} />`;
}
