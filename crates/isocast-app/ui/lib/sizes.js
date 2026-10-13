// The native size of a source's picture: from its settings when they say
// it, else measured once from a snapshot (cached per source and settings).

import { call } from "./bridge.js";
import { declaredSize } from "./model.js";
import { getState } from "./store.js";

const cache = new Map();

/** The size if it is known without asking: from the live engine, which
 * runs every source, or from the settings. */
export function knownSize(setup, source) {
  if (!source) return null;
  const live = getState().running?.[source.id];
  if (live?.width && live?.height) return { width: live.width, height: live.height };
  return declaredSize(setup, source);
}

/** Resolves to {width, height}, or null if it can't be known. */
export function sourceSize(setup, source) {
  if (!source) return Promise.resolve(null);
  const known = knownSize(setup, source);
  if (known) return Promise.resolve(known);
  const { name: _name, audio: _audio, filters: _filters, audio_filters: _af, ...settings } = source;
  const cacheKey = JSON.stringify(settings);
  if (!cache.has(cacheKey)) {
    const probe = call("snapshot", { source: source.id })
      .then((snapshot) => ({ width: snapshot.width, height: snapshot.height }))
      .catch(() => {
        cache.delete(cacheKey);
        return null;
      });
    cache.set(cacheKey, probe);
  }
  return cache.get(cacheKey);
}
