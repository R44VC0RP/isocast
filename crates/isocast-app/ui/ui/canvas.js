// Editing the scene on the monitor: click to select, drag to move,
// handles to resize (Shift keeps the shape), Option-drag an edge to crop,
// with snapping to the canvas and other items (hold ⌘ to place freely).

import { useContext, useEffect, useRef, useState } from "preact/hooks";
import { html } from "../lib/html.js";
import { getState, useStore } from "../lib/store.js";
import { bounds, findSource, itemLabel, key, originOf, placement } from "../lib/model.js";
import { select, setupAt } from "../lib/session.js";
import { updateItem } from "../lib/items.js";
import { knownSize, sourceSize } from "../lib/sizes.js";
import { Icon } from "./icons.js";
import { addMenu, itemMenu } from "./sources.js";
import { snapping } from "./settings.js";
import { ShownRevision } from "./monitor.js";

// Drag steps go to the engine once per display frame, so the picture
// follows the pointer as smoothly as the selection frame does.
const FRAME_MS = 16;

const HANDLES = [
  ["nw", -1, -1],
  ["n", 0, -1],
  ["ne", 1, -1],
  ["e", 1, 0],
  ["se", 1, 1],
  ["s", 0, 1],
  ["sw", -1, 1],
  ["w", -1, 0],
];

const rotateBy = (x, y, degrees) => {
  const r = (degrees * Math.PI) / 180;
  const c = Math.cos(r);
  const s = Math.sin(r);
  return [x * c - y * s, x * s + y * c];
};

/** The candidate lines an edge or centre can snap to: the canvas's edges
 * and middle, and the other visible items' edges and centres. */
function snapTargets(setup, scene, skipKey) {
  const xs = [0, setup.canvas.width / 2, setup.canvas.width];
  const ys = [0, setup.canvas.height / 2, setup.canvas.height];
  for (const item of scene.items ?? []) {
    if (item.visible === false || key(item) === skipKey) continue;
    const b = bounds(placement(item));
    xs.push(b.left, (b.left + b.right) / 2, b.right);
    ys.push(b.top, (b.top + b.bottom) / 2, b.bottom);
  }
  return { xs, ys };
}

/** The best snap of any of `values` to `targets` within `limit`: the
 * offset to add, and the line snapped to. */
function snap(values, targets, limit) {
  let best = null;
  for (const v of values) {
    for (const t of targets) {
      const d = t - v;
      if (Math.abs(d) <= limit && (!best || Math.abs(d) < Math.abs(best.offset))) best = { offset: d, line: t };
    }
  }
  return best;
}

/** The items along `path` (keys from the top) in `scene`, or null. */
function chainAt(scene, path) {
  let items = scene?.items ?? [];
  const chain = [];
  for (const k of path) {
    const item = items.find((i) => key(i) === k);
    if (!item) return null;
    chain.push(item);
    items = item.children ?? [];
  }
  return chain;
}

export function CanvasEditor({ sceneId, locked: previewLocked = false }) {
  const latest = useStore((s) => s.setup);
  // The picture under the boxes shows the layout of a slightly older setup
  // revision (the newest edits are still on their way through the engine):
  // the boxes are drawn at that layout, so they move with the picture, as
  // in OBS. Without a live picture, at the newest setup.
  const shown = useContext(ShownRevision);
  const setup = (shown != null && setupAt(shown)) || latest;
  const selection = useStore((s) => s.selection);
  const hover = useStore((s) => (s.hover?.scene === sceneId ? s.hover.path : null));
  const ref = useRef();
  const [width, setWidth] = useState(0);
  const [gesture, setGesture] = useState(null);

  useEffect(() => {
    const el = ref.current;
    if (!el) return undefined;
    const observer = new ResizeObserver(() => setWidth(el.clientWidth));
    observer.observe(el);
    setWidth(el.clientWidth);
    return () => observer.disconnect();
  }, []);

  const scene = setup?.scenes?.find((s) => s.id === sceneId);
  if (!setup || !scene) return html`<div class="overlay" ref=${ref} />`;
  const k = width / setup.canvas.width;
  const selectedPath = selection?.scene === sceneId ? selection.path : null;

  // The boxes: top-level items, and a selected child (with its parents).
  const boxes = [];
  for (const item of scene.items ?? []) boxes.push({ chain: [item] });
  if (selectedPath?.length > 1) {
    let items = scene.items ?? [];
    const chain = [];
    for (const k2 of selectedPath) {
      const item = items.find((i) => key(i) === k2);
      if (!item) break;
      chain.push(item);
      items = item.children ?? [];
    }
    if (chain.length === selectedPath.length) boxes.push({ chain });
  }

  const toCanvas = (event) => {
    const r = ref.current.getBoundingClientRect();
    return [(event.clientX - r.left) / k, (event.clientY - r.top) / k];
  };

  const begin = (event, drawnChain, handle) => {
    if (event.button !== 0) return;
    event.stopPropagation();
    // The gesture works on the newest setup, not the one drawn.
    const chain =
      chainAt(
        getState().setup?.scenes?.find((s) => s.id === sceneId),
        drawnChain.map(key),
      ) ?? drawnChain;
    const item = chain[chain.length - 1];
    const path = chain.map(key);
    select({ scene: sceneId, path });
    // A locked source, or a locked preview (Edit → Lock Preview), stays
    // put; so does one placed by its layout (no rect).
    if (item.locked || previewLocked || !item.rect) return;
    const current = getState().setup;
    const currentScene = current.scenes.find((s) => s.id === sceneId);
    const p0 = placement(item, originOf(chain));
    const origin = originOf(chain);
    const start = toCanvas(event);
    const rect0 = { ...item.rect };
    const crop0 = { left: 0, top: 0, right: 0, bottom: 0, ...(item.crop ?? {}) };
    const targets = snapTargets(current, currentScene, chain.length === 1 ? key(item) : null);
    const cropping = !!handle && event.altKey && !!item.source;
    const editKey = `${cropping ? "crop" : handle ? "size" : "move"}:${sceneId}:${path.join("/")}`;
    const target = event.currentTarget;
    try {
      target.setPointerCapture(event.pointerId);
    } catch {
      // A synthetic event (scripts) has no pointer to capture.
    }
    let moved = false;
    let size = null;
    const snapPrefs = snapping();
    if (cropping) {
      const source = findSource(current, item.source);
      size = knownSize(current, source);
      if (!size) {
        sourceSize(current, source).then((s) => {
          size = s;
        });
      }
    }

    const move = (e) => {
      const [x, y] = toCanvas(e);
      let dx = x - start[0];
      let dy = y - start[1];
      if (!moved && Math.hypot(dx * k, dy * k) < 3) return;
      moved = true;
      const free = e.metaKey || !snapPrefs.enabled;
      const limit = snapPrefs.distance / k;
      const guides = [];
      if (!handle) {
        if (e.shiftKey) {
          if (Math.abs(dx) > Math.abs(dy)) dy = 0;
          else dx = 0;
        }
        if (!free) {
          const b = bounds({ ...p0, cx: p0.cx + dx, cy: p0.cy + dy });
          const sx = snap([b.left, (b.left + b.right) / 2, b.right], targets.xs, limit);
          const sy = snap([b.top, (b.top + b.bottom) / 2, b.bottom], targets.ys, limit);
          if (sx) {
            dx += sx.offset;
            guides.push({ v: sx.line });
          }
          if (sy) {
            dy += sy.offset;
            guides.push({ h: sy.line });
          }
        }
        const rect = { x: Math.round(rect0.x + dx), y: Math.round(rect0.y + dy) };
        updateItem(sceneId, chain, { rect }, { key: editKey, interval: FRAME_MS });
        setGesture({ guides, readout: `${rect.x}, ${rect.y}`, at: [p0.cx + dx - p0.w / 2, p0.cy + dy - p0.h / 2] });
        return;
      }
      const [, ax, ay] = handle;
      const [lx, ly] = rotateBy(x - p0.cx, y - p0.cy, -p0.rotate);
      const hw = p0.w / 2;
      const hh = p0.h / 2;
      if (cropping) {
        crop(lx, ly, ax, ay, hw, hh);
        return;
      }
      // The opposite edge stays put; the dragged one follows the pointer.
      const fx = -ax * hw;
      const fy = -ay * hh;
      let w = ax ? Math.max(1, ax * (lx - fx)) : p0.w;
      let h = ay ? Math.max(1, ay * (ly - fy)) : p0.h;
      if (!free && !e.shiftKey && p0.rotate === 0) {
        const left = p0.cx + (ax ? Math.min(fx, fx + ax * w) : -hw);
        const right = p0.cx + (ax ? Math.max(fx, fx + ax * w) : hw);
        const top = p0.cy + (ay ? Math.min(fy, fy + ay * h) : -hh);
        const bottom = p0.cy + (ay ? Math.max(fy, fy + ay * h) : hh);
        if (ax) {
          const s = snap([ax > 0 ? right : left], targets.xs, limit);
          if (s) {
            w += ax * s.offset;
            guides.push({ v: s.line });
          }
        }
        if (ay) {
          const s = snap([ay > 0 ? bottom : top], targets.ys, limit);
          if (s) {
            h += ay * s.offset;
            guides.push({ h: s.line });
          }
        }
      }
      if (e.shiftKey) {
        const ratio = p0.w / p0.h;
        if (ax && ay) {
          if (w / ratio > h) h = w / ratio;
          else w = h * ratio;
        } else if (ax) h = w / ratio;
        else w = h * ratio;
      }
      const clx = ax ? fx + (ax * w) / 2 : 0;
      const cly = ay ? fy + (ay * h) / 2 : 0;
      const [ox, oy] = rotateBy(clx, cly, p0.rotate);
      const cx = p0.cx + ox;
      const cy = p0.cy + oy;
      const width = Math.max(1, Math.round(w / p0.scale));
      const height = Math.max(1, Math.round(h / p0.scale));
      const translate = item.style?.transform?.translate ?? [0, 0];
      const rect = {
        x: Math.round(cx - origin[0] - translate[0] - width / 2),
        y: Math.round(cy - origin[1] - translate[1] - height / 2),
        width,
        height,
      };
      updateItem(sceneId, chain, { rect }, { key: editKey, interval: FRAME_MS });
      setGesture({ guides, readout: `${width} × ${height}`, at: [cx - w / 2, cy - h / 2] });
    };

    // Option-drag: cut the source's edge, shrinking the item with it so the
    // rest of the picture stays where it is.
    const crop = (lx, ly, ax, ay, hw, hh) => {
      const natural = size ?? { width: rect0.width + crop0.left + crop0.right, height: rect0.height + crop0.top + crop0.bottom };
      const perX = (natural.width - crop0.left - crop0.right) / rect0.width;
      const perY = (natural.height - crop0.top - crop0.bottom) / rect0.height;
      const next = { ...crop0 };
      let width = rect0.width;
      let height = rect0.height;
      let shiftX = 0;
      let shiftY = 0;
      const limitIn = (visible, per) => (visible - 1) / per;
      if (ax) {
        // How far the edge moved inwards, in rect pixels.
        let d = (ax > 0 ? hw - lx : lx + hw) / p0.scale;
        const side = ax > 0 ? "right" : "left";
        d = Math.min(limitIn(natural.width - crop0.left - crop0.right, perX), Math.max(-crop0[side] / perX, d));
        next[side] = Math.round(crop0[side] + d * perX);
        width = rect0.width - d;
        shiftX = (ax > 0 ? -d : d) / 2;
      }
      if (ay) {
        let d = (ay > 0 ? hh - ly : ly + hh) / p0.scale;
        const side = ay > 0 ? "bottom" : "top";
        d = Math.min(limitIn(natural.height - crop0.top - crop0.bottom, perY), Math.max(-crop0[side] / perY, d));
        next[side] = Math.round(crop0[side] + d * perY);
        height = rect0.height - d;
        shiftY = (ay > 0 ? -d : d) / 2;
      }
      const [ox, oy] = rotateBy(shiftX * p0.scale, shiftY * p0.scale, p0.rotate);
      const w = Math.max(1, Math.round(width));
      const h = Math.max(1, Math.round(height));
      const cx = p0.cx + ox;
      const cy = p0.cy + oy;
      const translate = item.style?.transform?.translate ?? [0, 0];
      const rect = {
        x: Math.round(cx - origin[0] - translate[0] - w / 2),
        y: Math.round(cy - origin[1] - translate[1] - h / 2),
        width: w,
        height: h,
      };
      updateItem(sceneId, chain, { rect, crop: next }, { key: editKey, interval: FRAME_MS });
      setGesture({
        guides: [],
        readout: `Crop ${next.left} ${next.top} ${next.right} ${next.bottom}`,
        at: [cx - (w * p0.scale) / 2, cy - (h * p0.scale) / 2],
        crop: true,
      });
    };

    const up = () => {
      target.removeEventListener("pointermove", move);
      target.removeEventListener("pointerup", up);
      target.removeEventListener("pointercancel", up);
      setGesture(null);
    };
    target.addEventListener("pointermove", move);
    target.addEventListener("pointerup", up);
    target.addEventListener("pointercancel", up);
  };

  return html`<div
    class=${"overlay" + (gesture?.crop ? " crop" : "")}
    ref=${ref}
    onPointerDown=${(e) => e.button === 0 && select(null)}
  >
    ${k > 0 &&
    boxes.map(({ chain }) => {
      const item = chain[chain.length - 1];
      const path = chain.map(key);
      const isSelected = selectedPath && path.join("/") === selectedPath.join("/");
      if (item.visible === false && !isSelected) return null;
      const p = placement(item, originOf(chain));
      const style = {
        left: `${(p.cx - p.w / 2) * k}px`,
        top: `${(p.cy - p.h / 2) * k}px`,
        width: `${p.w * k}px`,
        height: `${p.h * k}px`,
        transform: p.rotate ? `rotate(${p.rotate}deg)` : "",
      };
      return html`<div
        key=${path.join("/")}
        class=${"item-box" + (isSelected ? " selected" : " hoverable") + (item.locked ? " locked" : "") + (hover === path.join("/") && !isSelected ? " hovered" : "")}
        style=${style}
        title=${itemLabel(setup, item)}
        onPointerDown=${(e) => begin(e, chain, null)}
        onContextMenu=${(e) => itemMenu(e, sceneId, chain, null, true)}
      >
        ${isSelected &&
        !item.locked &&
        !previewLocked &&
        HANDLES.map(
          (h) => html`<div
            key=${h[0]}
            class=${"handle " + h[0]}
            style=${{ left: `${((h[1] + 1) / 2) * 100}%`, top: `${((h[2] + 1) / 2) * 100}%` }}
            onPointerDown=${(e) => begin(e, chain, h)}
          />`,
        )}
      </div>`;
    })}
    ${!(scene.items ?? []).length &&
    html`<div class="canvas-empty">
      <span>Nothing in this scene yet</span>
      <button class="btn" onPointerDown=${(e) => e.stopPropagation()} onClick=${(e) => addMenu(e.currentTarget)}>
        <${Icon} name="plus" />Add a source
      </button>
    </div>`}
    ${gesture?.guides.map((g, i) =>
      g.v != null
        ? html`<div key=${"g" + i} class="guide v" style=${{ left: `${Math.round(g.v * k)}px` }} />`
        : html`<div key=${"g" + i} class="guide h" style=${{ top: `${Math.round(g.h * k)}px` }} />`,
    )}
    ${gesture?.readout &&
    html`<div
      class="drag-readout"
      style=${{ left: `${Math.max(4, gesture.at[0] * k)}px`, top: `${Math.max(4, gesture.at[1] * k - 24)}px` }}
    >
      ${gesture.readout}
    </div>`}
  </div>`;
}
