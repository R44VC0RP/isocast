// OBS's Transform actions on the selected scene item (Edit → Transform,
// the source menus and ⌘R/⌘F/⌘S/⌘D), as patches through `edit`.

import { getState } from "./store.js";
import { findSource } from "./model.js";
import { selected, updateItem } from "./items.js";
import { sourceSize } from "./sizes.js";

const cropOf = (item) => ({ left: 0, top: 0, right: 0, bottom: 0, ...(item.crop ?? {}) });

/** A patch taking away an item's scale and offset (kept: its rotation),
 * so its rect is where it is drawn. */
const unscaled = (item) => (item.style?.transform ? { style: { transform: { scale: null, translate: null } } } : {});

/** The selected item, or null; `run(found)` with it. Items placed by
 * their `layout` (no rect) aren't moved here. */
function withSelected(run) {
  const found = selected(getState());
  if (!found || found.item.locked || !found.item.rect) return null;
  return run(found);
}

/** The picture's own size after cropping, if it can be known. */
async function pictureSize(setup, item) {
  const source = item.source ? findSource(setup, item.source) : null;
  const natural = source ? await sourceSize(setup, source) : null;
  if (!natural) return null;
  const crop = cropOf(item);
  return { width: Math.max(1, natural.width - crop.left - crop.right), height: Math.max(1, natural.height - crop.top - crop.bottom) };
}

/** Fit to Screen (⌘F): as large as fits on the canvas, keeping its
 * shape, centred. */
export function fitToScreen() {
  return withSelected(async ({ scene, chain, item }) => {
    const setup = getState().setup;
    const canvas = setup.canvas;
    const size = (await pictureSize(setup, item)) ?? { width: item.rect.width, height: item.rect.height };
    const scale = Math.min(canvas.width / size.width, canvas.height / size.height);
    const width = Math.max(1, Math.round(size.width * scale));
    const height = Math.max(1, Math.round(size.height * scale));
    updateItem(scene.id, chain, {
      rect: { x: Math.round((canvas.width - width) / 2), y: Math.round((canvas.height - height) / 2), width, height },
      ...unscaled(item),
    });
  });
}

/** Stretch to Screen (⌘S): cover the canvas exactly. */
export function stretchToScreen() {
  return withSelected(({ scene, chain, item }) => {
    const { width, height } = getState().setup.canvas;
    updateItem(scene.id, chain, { rect: { x: 0, y: 0, width, height }, ...unscaled(item) });
  });
}

/** Center to Screen (⌘D), or only along one axis ("x": horizontally,
 * "y": vertically). */
export function centerToScreen(axis = null) {
  return withSelected(({ scene, chain, item }) => {
    const { width, height } = getState().setup.canvas;
    const t = item.style?.transform?.translate ?? [0, 0];
    const rect = {};
    if (axis !== "y") rect.x = Math.round((width - item.rect.width) / 2 - t[0]);
    if (axis !== "x") rect.y = Math.round((height - item.rect.height) / 2 - t[1]);
    updateItem(scene.id, chain, { rect });
  });
}

/** Reset Transform (⌘R): its own size at the top left, no rotation,
 * scale or crop. */
export function resetTransform() {
  return withSelected(async ({ scene, chain, item }) => {
    const setup = getState().setup;
    const source = item.source ? findSource(setup, item.source) : null;
    const natural = source ? await sourceSize(setup, source) : null;
    const size = natural ?? { width: item.rect.width, height: item.rect.height };
    updateItem(scene.id, chain, {
      rect: { x: 0, y: 0, width: size.width, height: size.height },
      crop: null,
      fit: null,
      align: null,
      ...(item.style?.transform ? { style: { transform: null } } : {}),
    });
  });
}

/** Rotate by `degrees` (clockwise), about the centre. */
export function rotateBy(degrees) {
  return withSelected(({ scene, chain, item }) => {
    const now = item.style?.transform?.rotate ?? 0;
    let next = (((now + degrees) % 360) + 360) % 360;
    if (next > 180) next -= 360;
    if (!next && !item.style?.transform) return;
    updateItem(scene.id, chain, { style: { transform: { rotate: next || null } } });
  });
}
