// One store for the interface's state. Components read slices with
// `useStore(select)` and re-render only when their slice changes; updates
// are batched to one notification per microtask.

import { useEffect, useReducer, useRef } from "preact/hooks";

let state = {
  // The daemon connection: connecting, connected, disconnected or failed.
  connection: { state: "connecting" },
  // Whether the live setup has been read at least once.
  loaded: false,
  // The live setup as shown: the daemon's, plus edits not yet confirmed.
  setup: null,
  revision: 0,
  // `status` from the daemon: engine state, scenes on air and in preview,
  // recording counters.
  status: null,
  scene: null,
  preview: null,
  studio: false,
  // The selected scene item: {scene, path: [key, ...]}.
  selection: null,
  // A copied scene item (Edit → Copy), to paste.
  clipboard: null,
  devices: null,
  // The live engine's sources by id: {state: opening|running|failed,
  // error, width, height, fps, channels}.
  running: {},
  // Dock sizes and what's shown: per-viewer preferences (ui/docks.js).
  layout: null,
  fullScreen: false,
  toasts: [],
  // The open dialog: {type, ...props} (ui/dialog.js).
  dialog: null,
  // Busy outputs: "record" or "stream" while starting or stopping.
  busy: {},
  stream: null,
  // The scene collections and profiles (`library`), and which are in use.
  library: null,
};

const subscribers = new Set();
let scheduled = false;

export function getState() {
  return state;
}

/** Merge `update` (an object, or a function of the state returning one). */
export function setState(update) {
  const changes = typeof update === "function" ? update(state) : update;
  if (!changes) return;
  state = { ...state, ...changes };
  if (!scheduled) {
    scheduled = true;
    queueMicrotask(() => {
      scheduled = false;
      for (const subscriber of subscribers) subscriber();
    });
  }
}

export function subscribe(subscriber) {
  subscribers.add(subscriber);
  return () => subscribers.delete(subscriber);
}

function shallowEqual(a, b) {
  if (Object.is(a, b)) return true;
  if (typeof a !== "object" || typeof b !== "object" || !a || !b) return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  const keys = Object.keys(a);
  if (keys.length !== Object.keys(b).length) return false;
  return keys.every((key) => Object.is(a[key], b[key]));
}

/** The slice `select(state)`, re-rendering when it changes (shallowly). */
export function useStore(select) {
  const [, rerender] = useReducer((n) => n + 1, 0);
  const selectRef = useRef(select);
  selectRef.current = select;
  const value = select(state);
  const valueRef = useRef(value);
  valueRef.current = value;
  useEffect(() => {
    const check = () => {
      const next = selectRef.current(state);
      if (!shallowEqual(next, valueRef.current)) rerender();
    };
    const unsubscribe = subscribe(check);
    // The state may have changed between rendering and subscribing.
    check();
    return unsubscribe;
  }, []);
  return value;
}

/** A per-viewer preference kept in localStorage (it may be unavailable). */
export function preference(key, fallback) {
  try {
    const value = localStorage.getItem(`isocast.${key}`);
    return value === null ? fallback : JSON.parse(value);
  } catch {
    return fallback;
  }
}

export function savePreference(key, value) {
  try {
    localStorage.setItem(`isocast.${key}`, JSON.stringify(value));
  } catch {
    // Not kept; fine.
  }
}
