// Settings with a known set of choices, as dropdowns: a source's camera,
// microphone, display, app or window (from the engine's `devices`), a
// camera's resolution and frame rate (the formats the chosen camera
// offers), a compressor's sidechain source and a scaling filter's shape.
// What a source names but isn't there now stays chosen, marked.

import { useEffect, useState } from "preact/hooks";
import { html } from "../lib/html.js";
import { useStore } from "../lib/store.js";
import { loadDevices, refreshStatus } from "../lib/session.js";
import {
  appOptions,
  cameraChange,
  cameraOptions,
  cameraRates,
  cameraSizes,
  cameraSupports,
  displayOptions,
  matchApp,
  matchDevice,
  matchWindow,
  microphoneOptions,
  rateRanges,
  TITLE_PART,
  titleOptions,
} from "../lib/devices.js";
import { KINDS, sourceName } from "../lib/model.js";
import { Prop, RefreshDevices, Select, TextField } from "./controls.js";

/** The devices, asked for again once when the form opens (something may
 * have been plugged in since they were last read). */
export function useDevices() {
  const devices = useStore((s) => s.devices);
  useEffect(() => {
    loadDevices(true);
  }, []);
  return devices;
}

/** A Select with a refresh button beside it. */
function WithRefresh({ children }) {
  const [busy, setBusy] = useState(false);
  const refresh = async () => {
    setBusy(true);
    await loadDevices(true);
    setBusy(false);
  };
  return html`${children}<${RefreshDevices} busy=${busy} onClick=${refresh} />`;
}

/** Why a list of choices is empty, if the engine said. */
function emptyNote(devices, what) {
  if (!devices) return "Looking…";
  if (what === "windows" && devices.permissions?.screen_recording !== "granted") return "Screen recording isn’t allowed, so no windows are listed.";
  if (what === "cameras" && devices.permissions?.camera !== "granted") return "Camera access isn’t allowed, so no cameras are listed.";
  return null;
}

/** The control for a setting with known choices, or null if `name` of
 * `kind` has none. `all` is every setting of the source or filter;
 * `onPatch` sets several at once; `owner` is a filter's source. */
export function choiceControl({ name, kind, value, all, owner, onPatch }) {
  if (kind === "camera" && name === "device") return html`<${CameraDevice} value=${value} all=${all} onPatch=${onPatch} />`;
  if (kind === "microphone" && name === "device") return html`<${DeviceChoice} what="microphones" value=${value} onPatch=${(v) => onPatch({ device: v })} />`;
  if (name === "display" && (kind === "display" || kind === "application")) {
    return html`<${DeviceChoice} what="displays" value=${value} onPatch=${(v) => onPatch({ display: v })} />`;
  }
  if (name === "app" && (kind === "window" || kind === "application" || kind === "application_audio")) {
    return html`<${AppChoice} value=${value} all=${all} kind=${kind} onPatch=${onPatch} />`;
  }
  if (kind === "window" && name === "title") return html`<${TitleChoice} value=${value} app=${all?.app} onPatch=${onPatch} />`;
  if (kind === "filter" && name === "sidechain_source") return html`<${SidechainChoice} value=${value} owner=${owner} onPatch=${onPatch} />`;
  if (kind === "filter" && name === "aspect") return html`<${AspectChoice} value=${value} onPatch=${onPatch} />`;
  return null;
}

const OPTIONS = { microphones: microphoneOptions, displays: displayOptions };

function DeviceChoice({ what, value, onPatch }) {
  const devices = useDevices();
  return html`<${WithRefresh}><${Select} value=${value} options=${OPTIONS[what](devices, value)} onChange=${onPatch} /><//>`;
}

/** A camera: changing it keeps the resolution and frame rate only if the
 * new one offers them, else they go back to Auto. */
function CameraDevice({ value, all, onPatch }) {
  const devices = useDevices();
  const options = cameraOptions(devices, value);
  const change = (device) => onPatch(cameraChange(devices, all, device));
  const none = !(devices?.cameras ?? []).length;
  const note = none ? emptyNote(devices, "cameras") ?? "No cameras found." : null;
  return html`<${WithRefresh}>
    <${Select} value=${value ?? ""} options=${none && !value ? [["", note]] : options} onChange=${change} />
  <//>`;
}

/** What Auto picks on this canvas, as the engine does: the highest rate up
 * to the canvas's (or exactly `fps`, if chosen), then the largest size
 * that fits on the canvas either way round (else the smallest larger
 * one), turned as the canvas is if it can be. */
function autoFormat(camera, canvas, size, fps = null) {
  const greater = (a, b) => {
    const i = a.findIndex((x, j) => x !== b[j]);
    return i >= 0 && a[i] > b[i];
  };
  const target = fps ?? canvas.fps;
  let best = null;
  for (const f of camera?.formats ?? []) {
    if (size && (f.width !== size.width || f.height !== size.height)) continue;
    for (const range of rateRanges(f)) {
      // At the target if the range holds it, else as near as it gets.
      const rate = target >= range.min - 0.01 && target <= range.max + 0.01 ? target : range.max < target ? range.max : range.min;
      if (fps != null && Math.abs(rate - target) > 0.5) continue;
      const fits =
        Math.max(f.width, f.height) <= Math.max(canvas.width, canvas.height) &&
        Math.min(f.width, f.height) <= Math.min(canvas.width, canvas.height);
      const area = f.width * f.height;
      const turned = f.width >= f.height === canvas.width >= canvas.height;
      const key = [Math.min(Math.round(rate), canvas.fps), fits ? 1 : 0, fits ? area : -area, turned ? 1 : 0];
      if (!best || greater(key, best.key)) best = { key, width: f.width, height: f.height, fps: Math.round(rate) };
    }
  }
  return best;
}

const sizeKey = (s) => `${s.width}x${s.height}`;
const fpsText = (n) => (Math.abs(n - Math.round(n)) < 0.05 ? String(Math.round(n)) : n.toFixed(1));

/** A camera's Resolution and Frame rate, one dropdown each, from the
 * formats the chosen camera offers, and what the engine receives now. */
export function CameraFormat({ id, all, onPatch }) {
  const devices = useDevices();
  const canvas = useStore((s) => s.setup?.canvas);
  const camera = matchDevice(devices?.cameras, all.device);
  const size = all.width && all.height ? { width: all.width, height: all.height } : null;
  const sizes = cameraSizes(camera);
  const auto = canvas && camera ? autoFormat(camera, canvas, null, all.fps ?? null) : null;

  const sizeOptions = [
    ["auto", "Auto", auto ? `${auto.width}×${auto.height} on this canvas` : ""],
    ...sizes.map((s) => [sizeKey(s), `${s.width}×${s.height}`, aspectName(s)]),
  ];
  if (size && !sizes.some((s) => sizeKey(s) === sizeKey(size))) {
    sizeOptions.push([sizeKey(size), `${size.width}×${size.height}`, camera ? "not offered" : "not connected"]);
  }
  const setSize = (v) => {
    const next = v === "auto" ? null : { width: Number(v.split("x")[0]), height: Number(v.split("x")[1]) };
    const keepFps = all.fps == null || cameraSupports(camera, next, all.fps);
    onPatch({ width: next?.width ?? null, height: next?.height ?? null, ...(keepFps ? {} : { fps: null }) });
  };

  const rates = cameraRates(camera, size);
  const autoRate = canvas && camera ? autoFormat(camera, canvas, size) : null;
  const rateOptions = [
    ["auto", "Auto", autoRate ? `${autoRate.fps} fps${size ? "" : " on this canvas"}` : ""],
    ...rates.map((r) => [String(r.value), r.label]),
  ];
  if (all.fps != null && !rates.some((r) => r.value === all.fps)) {
    rateOptions.push([String(all.fps), `${all.fps} fps`, camera ? "not offered" : "not connected"]);
  }
  const setFps = (v) => onPatch({ fps: v === "auto" ? null : Number(v) });

  return html`
    <${Prop} label="Resolution" title="The size the camera sends. Auto: the largest that fits on the canvas at the best frame rate.">
      <${Select} value=${size ? sizeKey(size) : "auto"} options=${sizeOptions} onChange=${setSize} /><span class="refresh-gap" />
    <//>
    <${Prop} label="Frame rate" title="Frames per second the camera sends. Auto: the canvas’s rate, or the nearest the camera offers.">
      <${Select} value=${all.fps != null ? String(all.fps) : "auto"} options=${rateOptions} onChange=${setFps} /><span class="refresh-gap" />
    <//>
    ${id && html`<${Receiving} id=${id} />`}
  `;
}

const ASPECTS = [
  [16, 9],
  [4, 3],
  [3, 2],
  [1, 1],
  [9, 16],
  [21, 9],
];

function aspectName({ width, height }) {
  const found = ASPECTS.find(([w, h]) => Math.abs(width / height - w / h) < 0.01);
  return found ? `${found[0]}:${found[1]}` : "";
}

/** What the engine gets from a running source, every second while shown. */
function Receiving({ id }) {
  const status = useStore((s) => s.running?.[id]);
  useEffect(() => {
    const timer = setInterval(() => !document.hidden && refreshStatus(true), 1000);
    return () => clearInterval(timer);
  }, []);
  let text;
  let warn = false;
  if (!status) text = "Not running: no scene or output uses it now.";
  else if (status.state === "opening") text = "Opening…";
  else if (status.state === "failed") text = "Not running.";
  else if (!status.width) text = "Running.";
  else {
    const size = `${status.width}×${status.height}`;
    const got = status.received_fps;
    if (got == null) text = `Opened at ${size}, ${status.fps} fps.`;
    else if (got === 0) {
      text = `Opened at ${size}, ${status.fps} fps, but no frames are arriving.`;
      warn = true;
    } else {
      text = `Receiving ${size} at ${fpsText(got)} fps`;
      if (status.fps && got < status.fps - 1.5) {
        text += ` (set to ${status.fps}; the camera sends fewer)`;
        warn = true;
      }
    }
  }
  return html`<div class=${"prop-hint receiving" + (warn ? " warn" : "")}>${text}</div>`;
}

/** An app with windows; for a window source, changing it keeps the title
 * only if the new app has such a window. */
function AppChoice({ value, all, kind, onPatch }) {
  const devices = useDevices();
  const options = appOptions(devices, value);
  const change = (app) => {
    if (kind !== "window" || !all?.title) return onPatch({ app });
    onPatch({ app, ...(matchWindow(devices, app, all.title) ? {} : { title: null }) });
  };
  const none = !(devices?.windows ?? []).length;
  const note = none ? emptyNote(devices, "windows") ?? "No apps with windows found." : null;
  return html`<${WithRefresh}>
    <${Select} value=${value ?? ""} options=${none && !value ? [["", note]] : options} onChange=${change} />
  <//>`;
}

/** A window source's title: any window, one of the app's windows by its
 * whole title, or part of a title typed (one that lasts while the app
 * changes the rest, like an unread count). Choosing "Part of a title…"
 * starts from the title chosen, to trim. */
function TitleChoice({ value, app, onPatch }) {
  const devices = useDevices();
  const known = !!matchApp(devices, app);
  const options = titleOptions(devices, app, value);
  const whole = !!value && options.some(([v]) => v === value);
  const [typing, setTyping] = useState(false);
  const part = typing || (!!value && !whole);
  const choose = (v) => {
    if (v === TITLE_PART) return setTyping(true);
    setTyping(false);
    onPatch({ title: v || null });
  };
  return html`<div class="grow title-choice">
      <${Select} value=${part ? TITLE_PART : value ?? ""} options=${options} disabled=${!known && !value} onChange=${choose} />
      ${part &&
      html`<${TextField} value=${value ?? ""} placeholder="Part of the window’s title" onChange=${(v) => onPatch({ title: v.trim() || null })} />`}
    </div><span class="refresh-gap" />`;
}

/** A compressor's sidechain: another audio source of the setup, by id. */
function SidechainChoice({ value, owner, onPatch }) {
  const sources = useStore((s) => s.setup?.sources ?? []);
  const audio = sources.filter((s) => s.id !== owner && (KINDS[s.kind]?.audio || (s.kind === "browser" && s.control_audio)));
  const options = [["", "None", "its own sound"], ...audio.map((s) => [s.id, sourceName(s), KINDS[s.kind]?.label ?? s.kind])];
  if (value && !audio.some((s) => s.id === value)) options.push([value, value, "not in this setup"]);
  return html`<${Select} value=${value ?? ""} options=${options} onChange=${(v) => onPatch({ sidechain_source: v || null })} />`;
}

/** A scaling filter's new shape: a common one, or any "w:h" typed. A
 * shape replaces a size (the engine takes one or the other). */
function AspectChoice({ value, onPatch }) {
  const [typing, setTyping] = useState(false);
  const common = ASPECTS.map(([w, h]) => `${w}:${h}`);
  const custom = typing || (!!value && !common.includes(value));
  const options = [["", "Unchanged"], ...common.map((a) => [a, a]), ["custom", custom ? "Custom" : "Custom…"]];
  const set = (aspect) => onPatch({ aspect, ...(aspect ? { width: null, height: null } : {}) });
  const choose = (v) => {
    if (v === "custom") return setTyping(true);
    setTyping(false);
    set(v || null);
  };
  return html`<${Select} value=${custom ? "custom" : value ?? ""} options=${options} onChange=${choose} />
    ${custom && html`<div class="field-narrow"><${TextField} value=${value ?? ""} placeholder="w:h, like 5:4" onChange=${(v) => set(v.trim() || null)} /></div>`}`;
}
