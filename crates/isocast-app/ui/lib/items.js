// Changes to scene items, as patches through `edit`.

import { getState, setState } from "./store.js";
import { findScene, findSource, itemChain, itemLabel, key } from "./model.js";
import { itemPatch, replaceList } from "./patch.js";
import { edit, toast } from "./session.js";

/** The selected item's scene and chain, if it still exists. */
export function selected(state = getState()) {
  const sel = state.selection;
  if (!sel) return null;
  const scene = findScene(state.setup, sel.scene);
  const chain = itemChain(scene, sel.path);
  return chain ? { scene, chain, item: chain[chain.length - 1] } : null;
}

/** Change fields of an item. `key` and `interval` make a stream of
 * changes (a drag) coalesce. */
export function updateItem(sceneId, chain, fields, options) {
  return edit(itemPatch(sceneId, chain, fields), options);
}

export function removeItem(sceneId, chain) {
  const setup = getState().setup;
  const scene = findScene(setup, sceneId);
  const before = scene?.items;
  const item = chain[chain.length - 1];
  setState({ selection: null });
  edit(itemPatch(sceneId, chain, { remove: true }));
  toast("info", `Removed “${itemLabel(setup, item)}” from ${scene?.name ?? sceneId}`, "", {
    label: "Undo",
    run: () => {
      const now = findScene(getState().setup, sceneId)?.items ?? [];
      edit({ scenes: [{ id: sceneId, items: replaceList(now, before) }] });
    },
  });
}

/** Put the items of a level (the scene's top level, or a group's
 * children) in a new order. */
export function reorder(sceneId, parentChain, items) {
  if (!parentChain.length) {
    const old = findScene(getState().setup, sceneId)?.items ?? [];
    return edit({ scenes: [{ id: sceneId, items: replaceList(old, items) }] });
  }
  const parent = parentChain[parentChain.length - 1];
  return edit(itemPatch(sceneId, parentChain, { children: replaceList(parent.children ?? [], items) }));
}

/** Move an item to the front (end of its list) or the back (start). */
export function restack(sceneId, chain, where) {
  const scene = findScene(getState().setup, sceneId);
  const parentChain = chain.slice(0, -1);
  const siblings = parentChain.length ? parentChain[parentChain.length - 1].children ?? [] : scene.items;
  const item = chain[chain.length - 1];
  const others = siblings.filter((i) => key(i) !== key(item));
  const index = siblings.findIndex((i) => key(i) === key(item));
  let list;
  if (where === "front") list = [...others, item];
  else if (where === "back") list = [item, ...others];
  else {
    list = others.slice();
    const to = where === "forward" ? Math.min(others.length, index + 1) : Math.max(0, index - 1);
    list.splice(to, 0, item);
  }
  return reorder(sceneId, parentChain, list);
}

/** Whether an item can be moved on the canvas. */
export const isLocked = (item) => !!item?.locked;

/** The source an item shows, if any. */
export function sourceOf(item) {
  return item?.source ? findSource(getState().setup, item.source) : null;
}
