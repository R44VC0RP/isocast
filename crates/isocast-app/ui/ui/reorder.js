// Drag to reorder rows in a list: press, move a few pixels, and a line
// shows where the row will land. A press that doesn't move is a click.

import { useRef, useState } from "preact/hooks";

/** Returns `start(event, index)` for a row's pointerdown, the index being
 * dragged and the drop line's position (`{index, top}`), and calls
 * `onMove(from, to)` (to: the index before which it lands, in 0..n) when
 * a row is dropped somewhere else. `onClick(index, event)` is called
 * for a press without a drag. */
export function useReorder(listRef, { onMove, onClick }) {
  const [drag, setDrag] = useState(null);
  const dragRef = useRef(null);

  const start = (event, index) => {
    if (event.button !== 0) return;
    if (event.target.closest("button, input")) return;
    const startY = event.clientY;
    const row = event.currentTarget;
    let moved = false;
    const rows = () => [...listRef.current.querySelectorAll("[data-row]")];
    const move = (e) => {
      if (!moved && Math.abs(e.clientY - startY) < 4) return;
      if (!moved) {
        moved = true;
        try {
          row.setPointerCapture(e.pointerId);
        } catch {
          // A synthetic event (scripts) has no pointer to capture.
        }
      }
      const list = listRef.current.getBoundingClientRect();
      const boxes = rows().map((r) => r.getBoundingClientRect());
      let to = boxes.length;
      for (let i = 0; i < boxes.length; i++) {
        if (e.clientY < boxes[i].top + boxes[i].height / 2) {
          to = i;
          break;
        }
      }
      const edge = to < boxes.length ? boxes[to].top : boxes[boxes.length - 1]?.bottom ?? list.top;
      const state = { from: index, to, top: edge - list.top + listRef.current.scrollTop };
      dragRef.current = state;
      setDrag(state);
    };
    const up = (e) => {
      removeEventListener("pointermove", move);
      removeEventListener("pointerup", up);
      const state = dragRef.current;
      dragRef.current = null;
      setDrag(null);
      if (!moved) {
        onClick?.(index, e);
        return;
      }
      if (state && state.to !== state.from && state.to !== state.from + 1) onMove(state.from, state.to);
    };
    addEventListener("pointermove", move);
    addEventListener("pointerup", up);
  };

  return { start, dragging: drag?.from ?? null, line: drag ? { top: drag.top } : null };
}

/** `list` with the item at `from` moved to land before index `to`. */
export function moved(list, from, to) {
  const out = list.slice();
  const [item] = out.splice(from, 1);
  out.splice(to > from ? to - 1 : to, 0, item);
  return out;
}

/** Keyboard for a list's rows, while a row has keyboard focus (a row
 * focused by a click leaves the arrows to the preview, which nudges the
 * selection): ↑ and ↓ (Home, End) pick the next row and focus it; F2
 * renames. Rows carry `data-row`; only the picked one is in the Tab
 * order (see `rowTabIndex`). */
export function rowKeys(event, listRef, index, count, { pick, rename }) {
  if (!event.currentTarget.matches(":focus-visible") || event.metaKey || event.ctrlKey || event.altKey) return;
  if (event.key === "F2" && rename) {
    event.preventDefault();
    event.stopPropagation();
    rename();
    return;
  }
  const by = { ArrowUp: -1, ArrowDown: 1, Home: -count, End: count }[event.key];
  if (by === undefined) return;
  event.preventDefault();
  event.stopPropagation();
  const next = Math.max(0, Math.min(count - 1, index + by));
  if (next !== index) pick(next);
  requestAnimationFrame(() => listRef.current?.querySelectorAll("[data-row]")[next]?.focus());
}

/** The selected row is the one in the Tab order; with none, the first. */
export const rowTabIndex = (index, selectedIndex) => (index === Math.max(0, selectedIndex) ? 0 : -1);
