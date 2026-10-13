// Small controls shared by the docks, the dialogs and Settings.

import { useEffect, useRef, useState } from "preact/hooks";
import { html } from "../lib/html.js";
import { Icon } from "./icons.js";

const round = (value, step) => {
  const digits = step < 1 ? Math.min(4, String(step).split(".")[1]?.length ?? 2) : 0;
  return Number(value.toFixed(digits));
};

/** A number with a label you can drag sideways to scrub (like a design
 * tool), typed values committed on Enter or blur. `onInput` fires while
 * scrubbing, `onChange` once a value is final. */
export function NumberField({
  label,
  value,
  onChange,
  onInput,
  min = -Infinity,
  max = Infinity,
  step = 1,
  unit,
  title,
  disabled,
  placeholder,
}) {
  const [text, setText] = useState(null);
  const shown = text ?? (value == null || Number.isNaN(value) ? "" : String(round(value, step)));
  const clamp = (v) => Math.min(max, Math.max(min, v));
  const commit = () => {
    if (text == null) return;
    setText(null);
    const parsed = Number(text.replace(",", "."));
    if (text.trim() !== "" && Number.isFinite(parsed) && parsed !== value) onChange(clamp(round(parsed, step)));
  };
  const scrub = (event) => {
    if (disabled || event.button !== 0) return;
    event.preventDefault();
    const start = event.clientX;
    const from = Number(value) || 0;
    const target = event.currentTarget;
    try {
      target.setPointerCapture(event.pointerId);
    } catch {
      // A synthetic event (scripts) has no pointer to capture.
    }
    let current = from;
    const move = (e) => {
      const factor = e.shiftKey ? 10 : e.altKey ? 0.1 : 1;
      current = clamp(round(from + Math.round((e.clientX - start) / 2) * step * factor, step));
      (onInput ?? onChange)(current);
    };
    const up = () => {
      target.removeEventListener("pointermove", move);
      target.removeEventListener("pointerup", up);
      target.removeEventListener("pointercancel", up);
      if (current !== from) onChange(current);
    };
    target.addEventListener("pointermove", move);
    target.addEventListener("pointerup", up);
    target.addEventListener("pointercancel", up);
  };
  const key = (event) => {
    if (event.key === "Enter") event.currentTarget.blur();
    else if (event.key === "Escape") {
      setText(null);
      event.currentTarget.blur();
    } else if (event.key === "ArrowUp" || event.key === "ArrowDown") {
      event.preventDefault();
      const delta = (event.key === "ArrowUp" ? 1 : -1) * step * (event.shiftKey ? 10 : 1);
      setText(null);
      onChange(clamp(round((Number(value) || 0) + delta, step)));
    }
  };
  return html`<label class=${"field number" + (disabled ? " disabled" : "")} title=${title}>
    ${label != null && html`<span class="prefix" onPointerDown=${scrub}>${label}</span>`}
    <input
      value=${shown}
      placeholder=${placeholder}
      inputmode="decimal"
      spellcheck="false"
      onFocus=${(e) => e.currentTarget.select()}
      onInput=${(e) => setText(e.currentTarget.value)}
      onBlur=${commit}
      onKeyDown=${key}
    />
    ${unit && html`<span class="unit">${unit}</span>`}
  </label>`;
}

/** Text committed on Enter or blur (Escape restores it). With `showEnd`
 * (paths), a long value shows its end while it isn't being edited. */
export function TextField({ value, onChange, placeholder, multiline, mono, disabled, autoFocus, showEnd }) {
  const [text, setText] = useState(null);
  const ref = useRef();
  useEffect(() => {
    if (autoFocus) ref.current?.focus();
  }, []);
  useEffect(() => {
    const el = ref.current;
    if (showEnd && el && document.activeElement !== el) el.scrollLeft = el.scrollWidth;
  });
  const commit = () => {
    if (text != null && text !== (value ?? "")) onChange(text);
    setText(null);
  };
  const key = (event) => {
    if (event.key === "Enter" && (!multiline || event.metaKey)) {
      event.preventDefault();
      event.currentTarget.blur();
    } else if (event.key === "Escape") {
      setText(null);
      event.currentTarget.blur();
    }
  };
  const props = {
    ref,
    value: text ?? value ?? "",
    placeholder,
    spellcheck: false,
    class: mono ? "mono" : "",
    onInput: (e) => setText(e.currentTarget.value),
    onBlur: commit,
    onKeyDown: key,
  };
  return multiline
    ? html`<div class=${"field textarea" + (disabled ? " disabled" : "")}><textarea rows="3" ...${props} /></div>`
    : html`<div class=${"field" + (disabled ? " disabled" : "")}><input ...${props} /></div>`;
}

/** A native select styled as a field; options are [value, label] pairs,
 * or [value, label, hint] with a hint shown dim after the chosen label
 * (and after each label in the menu). */
export function Select({ value, options, onChange, disabled, title }) {
  // Without a value the native select shows its first option; so does the label.
  const chosen = value == null ? options[0] : options.find(([v]) => String(v) === String(value));
  const hinted = options.some((o) => o[2]);
  return html`<label class=${"field select" + (hinted ? " hinted" : "") + (disabled ? " disabled" : "")} title=${title}>
    <select value=${value} onChange=${(e) => onChange(e.currentTarget.value)}>
      ${options.map(([v, label, hint]) => html`<option value=${v}>${hint ? `${label}  ·  ${hint}` : label}</option>`)}
    </select>
    ${hinted &&
    html`<span class="select-shown">
      <span class="ellipsis">${chosen?.[1] ?? ""}</span>
      ${chosen?.[2] && html`<span class="select-hint ellipsis">${chosen[2]}</span>`}
    </span>`}
    <${Icon} name="chevronDown" size=${14} class="chev" />
  </label>`;
}

/** A small button that asks for the devices again (one plugged in since). */
export function RefreshDevices({ onClick, busy }) {
  return html`<button class=${"icon-btn small" + (busy ? " spinning" : "")} title="Look for devices again" disabled=${busy} onClick=${onClick}>
    <${Icon} name="refresh" size=${14} />
  </button>`;
}

export function Switch({ on, onChange, title, disabled }) {
  return html`<button
    class=${"switch" + (on ? " on" : "")}
    role="switch"
    aria-checked=${on ? "true" : "false"}
    title=${title}
    disabled=${disabled}
    onClick=${() => onChange(!on)}
  />`;
}

export function Segmented({ value, options, onChange }) {
  return html`<div class="segmented">
    ${options.map(
      ([v, label]) =>
        html`<button class=${v === value ? "on" : ""} aria-pressed=${v === value ? "true" : "false"} onClick=${() => v !== value && onChange(v)}>${label}</button>`,
    )}
  </div>`;
}

/** A horizontal slider from `min` to `max`. Its fill runs from `origin`
 * (default `min`) to the value, so a bipolar one (Balance) fills from the
 * centre. From the keyboard, arrows move it a hundredth of its range
 * (with ⇧, a tenth), Home and End to its ends. */
export function Slider({ value, min, max, step = 0.01, origin = min, label, onInput, onChange }) {
  const ref = useRef();
  const fractionOf = (v) => Math.min(1, Math.max(0, (v - min) / (max - min)));
  const fraction = fractionOf(value);
  const from = fractionOf(origin);
  const valueAt = (event) => {
    const box = ref.current.getBoundingClientRect();
    const f = Math.min(1, Math.max(0, (event.clientX - box.left) / box.width));
    return Math.round((min + f * (max - min)) / step) * step;
  };
  const down = (event) => {
    if (event.button !== 0) return;
    const target = event.currentTarget;
    try {
      target.setPointerCapture(event.pointerId);
    } catch {
      // A synthetic event (scripts) has no pointer to capture.
    }
    let current = valueAt(event);
    (onInput ?? onChange)(current);
    const move = (e) => {
      current = valueAt(e);
      (onInput ?? onChange)(current);
    };
    const up = () => {
      target.removeEventListener("pointermove", move);
      target.removeEventListener("pointerup", up);
      onChange(current);
    };
    target.addEventListener("pointermove", move);
    target.addEventListener("pointerup", up);
  };
  const key = (event) => {
    const unit = Math.max(step, Math.round((max - min) / 100 / step) * step) * (event.shiftKey ? 10 : 1);
    const by = { ArrowLeft: -unit, ArrowDown: -unit, ArrowRight: unit, ArrowUp: unit, Home: -Infinity, End: Infinity }[event.key];
    if (by === undefined || event.metaKey || event.ctrlKey || event.altKey) return;
    event.preventDefault();
    event.stopPropagation();
    const next = Math.min(max, Math.max(min, Math.round(((Number(value) || 0) + by) / step) * step));
    if (Number.isFinite(next) && next !== value) onChange(next);
  };
  return html`<div class="slider" ref=${ref} tabindex="0" role="slider" aria-label=${label}
    aria-valuemin=${min} aria-valuemax=${max} aria-valuenow=${value} onPointerDown=${down} onKeyDown=${key}>
    <div class="track" />
    <div class="fill" style=${{ left: `${Math.min(from, fraction) * 100}%`, width: `${Math.abs(fraction - from) * 100}%` }} />
    <div class="thumb" style=${{ left: `${fraction * 100}%` }} />
  </div>`;
}

/** A colour as #rrggbb (with optional alpha kept), with a swatch that
 * opens the system colour picker. */
export function ColorField({ value, onChange }) {
  const hex = normalizeColor(value);
  return html`<div class="field">
    <span class="color-swatch" style=${{ background: value || "transparent" }}>
      <input type="color" value=${hex.slice(0, 7)} onChange=${(e) => onChange(e.currentTarget.value + hex.slice(7))} />
    </span>
    <input
      class="mono"
      value=${value ?? ""}
      spellcheck="false"
      onChange=${(e) => {
        const text = e.currentTarget.value.trim();
        if (/^#([0-9a-f]{3,4}|[0-9a-f]{6}|[0-9a-f]{8})$/i.test(text)) onChange(text);
        else e.currentTarget.value = value ?? "";
      }}
    />
  </div>`;
}

function normalizeColor(value) {
  const text = (value ?? "#000000").replace("#", "");
  if (text.length === 3 || text.length === 4) {
    return "#" + text.split("").map((c) => c + c).join("");
  }
  return "#" + text.padEnd(6, "0");
}

/** A labelled row: label on the left, control on the right. */
export function Prop({ label, hint, title, stack, children }) {
  return html`<div>
    <div class=${"prop" + (stack ? " stack" : "")}>
      <span class="prop-label" title=${title || label}>${label}</span>
      <div class="prop-value">${children}</div>
    </div>
    ${hint && html`<div class="prop-hint" style=${stack ? "margin-left: 0" : ""}>${hint}</div>`}
  </div>`;
}
