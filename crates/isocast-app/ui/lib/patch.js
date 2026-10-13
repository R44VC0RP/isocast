// Merge patches with the daemon's semantics (isocast_protocol::change): a
// JSON merge patch, except that lists of identified items (sources and
// scenes by `id`, scene items by `id`, else `source`, else `scene`, filters
// by `id`, else `kind`) merge by id, and `{"id": "x", "remove": true}`
// removes one. Other lists are replaced whole.

const isObject = (value) => value !== null && typeof value === "object" && !Array.isArray(value);

/** What identifies an item of a list. */
export function itemKey(item) {
  if (!isObject(item)) return null;
  return item.id ?? item.source ?? item.scene ?? item.kind ?? null;
}

const keyed = (list) => list.length > 0 && list.every((item) => itemKey(item) != null);

function stripNulls(value) {
  if (!isObject(value)) return value;
  const out = {};
  for (const [key, inner] of Object.entries(value)) {
    if (inner !== null) out[key] = stripNulls(inner);
  }
  return out;
}

function mergeById(list, patch) {
  const out = list.slice();
  for (const item of patch) {
    const key = itemKey(item);
    const index = out.findIndex((existing) => itemKey(existing) === key);
    if (item.remove === true) {
      if (index >= 0) out.splice(index, 1);
    } else if (index >= 0) {
      out[index] = mergePatch(out[index], item);
    } else {
      out.push(stripNulls(item));
    }
  }
  return out;
}

/** `patch` applied to `doc`, as the daemon applies it. */
export function mergePatch(doc, patch) {
  if (isObject(doc) && isObject(patch)) {
    const out = { ...doc };
    for (const [key, value] of Object.entries(patch)) {
      if (value === null) delete out[key];
      else out[key] = key in out ? mergePatch(out[key], value) : stripNulls(value);
    }
    return out;
  }
  if (Array.isArray(doc) && Array.isArray(patch) && keyed(patch) && (doc.length === 0 || keyed(doc))) {
    return mergeById(doc, patch);
  }
  return stripNulls(patch);
}

/** The fields that identify a scene item in a patch. */
export function itemIdentity(item) {
  if (item.id != null) return { id: item.id };
  if (item.source != null) return { source: item.source };
  return { scene: item.scene };
}

/** A patch changing `fields` of the item at the end of `chain` (the items
 * from the top level down) in scene `sceneId`. */
export function itemPatch(sceneId, chain, fields) {
  let node = { ...itemIdentity(chain[chain.length - 1]), ...fields };
  for (let i = chain.length - 2; i >= 0; i--) {
    node = { ...itemIdentity(chain[i]), children: [node] };
  }
  return { scenes: [{ id: sceneId, items: [node] }] };
}

/** A patch putting a list of identified items in exactly this order and
 * content: every old item removed, then every new one added. */
export function replaceList(oldList, newList) {
  return [
    ...oldList.map((item) => ({ ...identityOf(item), remove: true })),
    ...newList,
  ];
}

function identityOf(item) {
  if (item.id != null) return { id: item.id };
  if (item.source != null) return { source: item.source };
  if (item.scene != null) return { scene: item.scene };
  return { kind: item.kind };
}

/** A patch turning object `from` into `to`: changed fields set, missing
 * ones nulled (nested objects recursively; lists replaced whole). */
export function objectPatch(from = {}, to = {}) {
  const out = {};
  for (const key of Object.keys(from)) {
    if (!(key in to)) out[key] = null;
  }
  for (const [key, value] of Object.entries(to)) {
    const old = from[key];
    if (isObject(old) && isObject(value)) {
      const inner = objectPatch(old, value);
      if (Object.keys(inner).length) out[key] = inner;
    } else if (JSON.stringify(old) !== JSON.stringify(value)) {
      // Identified lists merge by id, so replacing one takes removals.
      const merges = Array.isArray(old) && Array.isArray(value) && keyed(old) && (value.length === 0 || keyed(value));
      out[key] = merges ? replaceList(old, value) : value;
    }
  }
  return out;
}
