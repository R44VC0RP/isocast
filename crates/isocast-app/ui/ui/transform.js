// Edit Transform (⌘E), as in OBS: the selected item's position, rotation,
// size, alignment, bounding box and crop, with Reset at the bottom left.
// It follows the selection and changes show at once. Isocast's item
// style (opacity, corners, border, shadow, background) is at the end.

import { useState } from "preact/hooks";
import { html } from "../lib/html.js";
import { getState, useStore } from "../lib/store.js";
import { itemLabel } from "../lib/model.js";
import { humanize } from "../lib/schema.js";
import { selected, updateItem } from "../lib/items.js";
import { resetTransform } from "../lib/transform.js";
import { ColorField, NumberField, Select, Slider } from "./controls.js";
import { Dialog, closeDialog, registerDialog } from "./dialog.js";
import { Icon } from "./icons.js";

const ALIGNS = ["top_left", "top", "top_right", "left", "center", "right", "bottom_left", "bottom", "bottom_right"];
const ALIGN_OPTIONS = ALIGNS.map((a) => [a, humanize(a)]);
// Where an alignment sits in a box, as fractions of its width and height.
const ANCHOR = {
  top_left: [0, 0], top: [0.5, 0], top_right: [1, 0],
  left: [0, 0.5], center: [0.5, 0.5], right: [1, 0.5],
  bottom_left: [0, 1], bottom: [0.5, 1], bottom_right: [1, 1],
};
// OBS's bounding box types and Isocast's fits.
const BOUNDS = [
  ["stretch", "Stretch to bounds"],
  ["contain", "Scale to inner bounds"],
  ["cover", "Scale to outer bounds"],
  ["width", "Scale to width of bounds"],
  ["height", "Scale to height of bounds"],
  ["scale_down", "Maximum size only"],
  ["none", "No scaling (original size)"],
];

function Row({ label, children }) {
  return html`<div class="form-row"><span class="form-label">${label}</span><div class="form-value">${children}</div></div>`;
}

function TransformDialog() {
  const setup = useStore((s) => s.setup);
  const selection = useStore((s) => s.selection);
  const [anchor, setAnchor] = useState("top_left");
  const found = setup && selection ? selected(getState()) : null;
  const close = closeDialog;
  if (!found) {
    return html`<${Dialog} title="Edit Transform" width=${520} onCancel=${close} onAccept=${close}
      buttons=${html`<button class="btn primary" onClick=${close}>Close</button>`}>
      <div class="form"><div class="faint">Select a source to change its transform.</div></div>
    <//>`;
  }
  const { scene, chain, item } = found;
  const set = (fields, live) => updateItem(scene.id, chain, fields, live ? { key: `transform:${live}:${scene.id}`, interval: 50 } : undefined);
  const t = { translate: [0, 0], scale: 1, rotate: 0, ...(item.style?.transform ?? {}) };
  const crop = { left: 0, top: 0, right: 0, bottom: 0, ...(item.crop ?? {}) };
  const [ax, ay] = ANCHOR[anchor];
  if (!item.rect) {
    return html`<${Dialog} title="Edit Transform" width=${520} onCancel=${close} onAccept=${close}
      buttons=${html`<button class="btn primary" onClick=${close}>Close</button>`}>
      <div class="form"><div class="faint">This item is placed by its layout. Change its layout in the setup (isocast plan --patch, or an agent).</div></div>
    <//>`;
  }
  const { x, y, width, height } = item.rect;
  const fit = item.fit ?? "stretch";
  const style = item.style ?? {};
  const radius = typeof style.radius === "number" ? style.radius : Array.isArray(style.radius) ? style.radius[0] : 0;
  const locked = !!item.locked;
  return html`<${Dialog}
    title=${`Edit Transform for “${itemLabel(setup, item)}”`}
    width=${560}
    class="transform-dialog"
    onCancel=${close}
    onAccept=${close}
    extra=${html`<button class="btn" disabled=${locked} onClick=${resetTransform}>Reset</button>`}
    buttons=${html`<button class="btn primary" onClick=${close}>Close</button>`}
  >
    <div class=${"form" + (locked ? " locked" : "")}>
      ${locked && html`<div class="callout info"><${Icon} name="lock" size=${14} /><div>This source is locked. Unlock it in the Sources list to move it.</div></div>`}
      <${Row} label="Position">
        <${NumberField} label="X" value=${x + ax * width} onChange=${(v) => set({ rect: { x: Math.round(v - ax * width) } })} onInput=${(v) => set({ rect: { x: Math.round(v - ax * width) } }, "x")} />
        <${NumberField} label="Y" value=${y + ay * height} onChange=${(v) => set({ rect: { y: Math.round(v - ay * height) } })} onInput=${(v) => set({ rect: { y: Math.round(v - ay * height) } }, "y")} />
      <//>
      <${Row} label="Rotation">
        <${NumberField} value=${t.rotate} unit="°" min=${-360} max=${360} step=${0.1}
          onChange=${(rotate) => set({ style: { transform: { rotate } } })} onInput=${(rotate) => set({ style: { transform: { rotate } } }, "rotate")} />
        <span />
      <//>
      <${Row} label="Size">
        <${NumberField} label="W" min=${1} value=${width} onChange=${(w) => set({ rect: { width: w } })} onInput=${(w) => set({ rect: { width: w } }, "w")} />
        <${NumberField} label="H" min=${1} value=${height} onChange=${(h) => set({ rect: { height: h } })} onInput=${(h) => set({ rect: { height: h } }, "h")} />
      <//>
      <${Row} label="Scale">
        <${NumberField} value=${Math.round(t.scale * 1000) / 10} unit="%" min=${1} max=${1000} step=${0.1}
          onChange=${(s) => set({ style: { transform: { scale: s / 100 } } })} onInput=${(s) => set({ style: { transform: { scale: s / 100 } } }, "scale")} />
        <span />
      <//>
      <${Row} label="Positional Alignment">
        <${Select} value=${anchor} options=${ALIGN_OPTIONS} onChange=${setAnchor} title="Which point of the source the position is measured to" />
      <//>
      <div class="form-sep" />
      <${Row} label="Bounding Box Type">
        <${Select} value=${fit} options=${BOUNDS} onChange=${(f) => set({ fit: f === "stretch" ? null : f })} />
      <//>
      <${Row} label="Alignment in Bounding Box">
        <${Select} value=${item.align ?? "center"} options=${ALIGN_OPTIONS} disabled=${fit === "stretch"}
          onChange=${(a) => set({ align: a === "center" ? null : a })} />
      <//>
      <div class="form-sep" />
      <${Row} label="Crop">
        <div class="crop-grid">
          ${[["left", "Left"], ["right", "Right"], ["top", "Top"], ["bottom", "Bottom"]].map(
            ([side, name]) => html`<${NumberField} key=${side} label=${name} min=${0} value=${crop[side]} unit="px"
              onChange=${(v) => set({ crop: { ...crop, [side]: v } })} />`,
          )}
        </div>
      <//>
      <div class="form-section">Appearance</div>
      <${Row} label="Opacity">
        <${Slider} value=${style.opacity ?? 1} min=${0} max=${1} label="Opacity"
          onInput=${(opacity) => set({ style: { opacity } }, "opacity")} onChange=${(opacity) => set({ style: { opacity } })} />
        <span class="tabular dim value-note">${Math.round((style.opacity ?? 1) * 100)}%</span>
      <//>
      <${Row} label="Corner Radius">
        <${NumberField} value=${radius} min=${0} max=${2000} unit="px"
          onChange=${(r) => set({ style: { radius: r || null } })} onInput=${(r) => set({ style: { radius: r } }, "radius")} />
        <span />
      <//>
      <${Row} label="Border">
        ${style.border
          ? html`<${NumberField} value=${style.border.width} min=${0} max=${200} unit="px" onChange=${(w) => set({ style: { border: { width: w } } })} />
              <${ColorField} value=${style.border.color} onChange=${(color) => set({ style: { border: { color } } })} />
              <button class="icon-btn small" title="No border" onClick=${() => set({ style: { border: null } })}><${Icon} name="x" size=${14} /></button>`
          : html`<button class="btn" onClick=${() => set({ style: { border: { width: 4, color: "#eeeae2" } } })}>Add a border</button>`}
      <//>
      <${Row} label="Shadow">
        ${style.shadow
          ? html`<${NumberField} label="Blur" value=${style.shadow.blur ?? 0} min=${0} max=${500} unit="px" onChange=${(blur) => set({ style: { shadow: { blur } } })} />
              <${ColorField} value=${style.shadow.color} onChange=${(color) => set({ style: { shadow: { color } } })} />
              <button class="icon-btn small" title="No shadow" onClick=${() => set({ style: { shadow: null } })}><${Icon} name="x" size=${14} /></button>`
          : html`<button class="btn" onClick=${() => set({ style: { shadow: { x: 0, y: 8, blur: 24, color: "#00000099" } } })}>Add a shadow</button>`}
      <//>
      <${Row} label="Background">
        ${style.background
          ? html`<${ColorField} value=${style.background} onChange=${(background) => set({ style: { background } })} />
              <button class="icon-btn small" title="No background" onClick=${() => set({ style: { background: null } })}><${Icon} name="x" size=${14} /></button>`
          : html`<button class="btn" onClick=${() => set({ style: { background: "#000000" } })}>Add a background</button>`}
      <//>
    </div>
  <//>`;
}

registerDialog("transform", TransformDialog);
