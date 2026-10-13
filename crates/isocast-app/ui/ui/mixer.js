// The Audio Mixer dock, as in OBS: a strip per audio source with its name
// and volume on top, a meter (dBFS, −60 to 0, green, amber and red zones,
// peak hold), and under it the fader, mute and a menu. Meters draw on
// canvases from the latest `levels` event with smooth decay, outside
// Preact: new levels never re-render anything. Silence (or no levels)
// leaves them resting, with no animation frames at all.

import { useEffect, useRef } from "preact/hooks";
import { html } from "../lib/html.js";
import { getState, useStore } from "../lib/store.js";
import { hasAudio, hasVideo, sourceName } from "../lib/model.js";
import { levelOf, onLevels } from "../lib/levels.js";
import { edit } from "../lib/session.js";
import { Slider } from "./controls.js";
import { Icon } from "./icons.js";
import { showMenu } from "./popover.js";
import { SourceState } from "./sources.js";
import { Dock, setLayout } from "./docks.js";
import { openDialog } from "./dialog.js";

const FLOOR = -60;
const position = (db) => Math.min(1, Math.max(0, (db - FLOOR) / -FLOOR));
const TICKS = [-60, -48, -36, -24, -18, -12, -6, 0];
// Labels that make way on narrow meters (see app.css): first these…
const MINOR_TICKS = [-18, -6];
// …then these.
const MID_TICKS = [-48, -24];

// Every meter on screen, drawn by one animation loop.
const meters = new Set();
let running = false;

function frame(now) {
  for (const meter of meters) meter.draw(now);
  // Keep going while any meter shows something (or is falling back);
  // audible levels start the loop again.
  if ([...meters].some((m) => m.active)) requestAnimationFrame(frame);
  else running = false;
}

function wake() {
  if (!running && meters.size) {
    running = true;
    requestAnimationFrame(frame);
  }
}

// New levels start the loop only if some meter would show them (after its
// fader: a muted source still reports what it hears).
onLevels(() => !running && [...meters].some((m) => m.audible()) && wake());

function makeMeter(canvas, source, gain) {
  const state = { rms: FLOOR, peak: FLOOR, hold: FLOOR, holdAt: 0, last: performance.now(), active: false };
  const meter = {
    source,
    gain,
    get active() {
      return state.active;
    },
    audible() {
      const level = levelOf(meter.source);
      return !!level && level.peak + meter.gain() > FLOOR;
    },
    draw(now) {
      const dt = Math.min(0.1, (now - state.last) / 1000);
      state.last = now;
      const level = levelOf(meter.source);
      const offset = meter.gain();
      const targetRms = level ? Math.max(FLOOR, level.rms + offset) : FLOOR;
      const targetPeak = level ? Math.max(FLOOR, level.peak + offset) : FLOOR;
      // Rise at once, fall smoothly (about 24 dB a second).
      state.rms = targetRms > state.rms ? targetRms : Math.max(targetRms, state.rms - 24 * dt);
      state.peak = targetPeak > state.peak ? targetPeak : Math.max(targetPeak, state.peak - 32 * dt);
      if (state.peak >= state.hold || now - state.holdAt > 1500) {
        state.hold = state.peak;
        state.holdAt = now;
      }
      state.active = state.peak > FLOOR + 0.5 || state.hold > FLOOR + 0.5 || targetPeak > FLOOR;
      // Repaint only when a bar or the hold mark moves by a pixel.
      const look = [state.rms, state.peak, state.hold].map((db) => Math.round(position(db) * canvas.clientWidth * devicePixelRatio)).join();
      if (look !== state.look || canvas.width !== Math.round(canvas.clientWidth * devicePixelRatio)) {
        state.look = look;
        paint(canvas, state);
      }
    },
  };
  return meter;
}

// Zone colours and the unlit track come from the CSS tokens (--meter-*),
// read once on the first paint, never per frame.
let zones = null;
let track = "#22262b";
function meterColors() {
  const css = getComputedStyle(document.documentElement);
  const token = (name, fallback) => css.getPropertyValue(name).trim() || fallback;
  track = token("--meter-track", track);
  const ok = token("--meter-ok", "#3e8e7e");
  const warn = token("--meter-warn", "#d6a03f");
  const clip = token("--meter-clip", "#e5484d");
  // Each zone: [from, to, solid colour, peak colour (half alpha)].
  const half = (hex) => hex + "80";
  return [
    [FLOOR, -18, ok, half(ok)],
    [-18, -6, warn, half(warn)],
    [-6, 0, clip, half(clip)],
  ];
}

function paint(canvas, state) {
  const width = Math.round(canvas.clientWidth * devicePixelRatio);
  const height = Math.round(canvas.clientHeight * devicePixelRatio);
  if (!width || !height) return;
  if (canvas.width !== width || canvas.height !== height) {
    canvas.width = width;
    canvas.height = height;
  }
  zones ??= meterColors();
  const c = canvas.getContext("2d");
  c.clearRect(0, 0, width, height);
  const r = devicePixelRatio;
  const bar = (from, to, color) => {
    const x0 = Math.round(position(from) * width);
    const x1 = Math.round(position(to) * width);
    if (x1 > x0) {
      c.fillStyle = color;
      c.fillRect(x0, 0, x1 - x0, height);
    }
  };
  // A plain neutral track.
  c.save();
  c.beginPath();
  c.roundRect(0, 0, width, height, r);
  c.clip();
  bar(FLOOR, 0, track);
  // Peak (lighter), then RMS (solid), each in its zone's colour.
  for (const [from, to, solid, peak] of zones) {
    bar(from, Math.min(to, state.peak), peak);
    bar(from, Math.min(to, state.rms), solid);
  }
  if (state.hold > FLOOR + 0.5) {
    const x = Math.round(position(state.hold) * width);
    c.fillStyle = (zones.find(([from, to]) => state.hold >= from && state.hold <= to + 0.01) ?? zones[0])[2];
    c.fillRect(Math.max(0, x - devicePixelRatio), 0, 2 * devicePixelRatio, height);
  }
  c.restore();
}


/** A meter for `source` after `gain` (a function of the moment's gain in
 * dB), drawn by the shared animation loop. */
function Meter({ source, gain, muted }) {
  const canvasRef = useRef();
  const gainRef = useRef(gain);
  gainRef.current = muted ? -Infinity : gain;
  useEffect(() => {
    const meter = makeMeter(canvasRef.current, source, () => (Number.isFinite(gainRef.current) ? gainRef.current : -120));
    meters.add(meter);
    meter.draw(performance.now());
    wake();
    // A resting meter still redraws at its new size.
    const resized = new ResizeObserver(() => meter.draw(performance.now()));
    resized.observe(canvasRef.current);
    return () => {
      resized.disconnect();
      meters.delete(meter);
    };
  }, [source]);
  return html`<div class="meter">
    <canvas ref=${canvasRef} />
    <div class="scale">
      ${TICKS.map((t) => html`<span key=${t} class=${MINOR_TICKS.includes(t) ? "minor" : MID_TICKS.includes(t) ? "mid" : ""} style=${{ left: `${position(t) * 100}%`, transform: t === FLOOR ? "none" : t === 0 ? "translateX(-100%)" : "" }}>${minus(String(t))}</span>`)}
    </div>
  </div>`;
}

// A true minus sign (U+2212), as in Settings, not a hyphen.
const minus = (text) => text.replace("-", "\u2212");
export const gainText = (db) => (db <= -100 ? "\u2212\u221e dB" : minus(`${db > 0 ? "+" : ""}${db.toFixed(1)} dB`));

export function setAudio(source, fields, options) {
  return edit({ sources: [{ id: source.id, audio: fields }] }, options);
}

/** Whether source `id` is in the program mix (OBS's track 1). */
export const inProgramMix = (setup, id) => (setup.recording.program?.audio ?? []).includes(id);

export function setProgramMix(id, on) {
  const list = getState().setup.recording.program?.audio ?? [];
  return edit({ recording: { program: { audio: on ? [...list.filter((s) => s !== id), id] : list.filter((s) => s !== id) } } });
}

export const MONITORING = [
  ["off", "Monitor Off"],
  ["monitor_only", "Monitor Only (mute output)"],
  ["monitor_and_output", "Monitor and Output"],
];

const hide = (id) => setLayout({ mixerHidden: [...new Set([...(getState().layout.mixerHidden ?? []), id])] });
const unhideAll = () => setLayout({ mixerHidden: [] });

function stripMenu(at, source, audio) {
  const setup = getState().setup;
  const mixed = inProgramMix(setup, source.id);
  showMenu(at, [
    { label: "Hide", run: () => hide(source.id) },
    { label: "Unhide All", disabled: !(getState().layout.mixerHidden ?? []).length, run: unhideAll },
    "-",
    { label: "Rename", run: () => openDialog("rename", { source: source.id }) },
    {
      label: "Audio Monitoring",
      items: MONITORING.map(([value, label]) => ({ label, checked: audio.monitor === value, run: () => setAudio(source, { monitor: value }) })),
    },
    { label: "Include in Program Mix", checked: mixed, run: () => setProgramMix(source.id, !mixed) },
    "-",
    { label: "Filters", run: () => openDialog("filters", { source: source.id }) },
    { label: "Properties", run: () => openDialog("properties", { source: source.id }) },
    { label: "Advanced Audio Properties", run: () => openDialog("advanced-audio") },
  ]);
}

function Strip({ source, mixed, status }) {
  const audio = { gain_db: 0, muted: false, monitor: "off", ...(source.audio ?? {}) };
  const live = { key: `audio:${source.id}:gain`, interval: 50 };
  const monitoring = audio.monitor !== "off";
  return html`<div class=${"strip" + (audio.muted ? " muted" : "") + (mixed ? "" : " unmixed")} onContextMenu=${(e) => (e.preventDefault(), e.stopPropagation(), stripMenu(e, source, audio))}>
    <div class="strip-head">
      <span class="name ellipsis" title=${sourceName(source)}>${sourceName(source)}</span>
      <${SourceState} status=${status} />
      ${monitoring && html`<span class="strip-flag" title=${audio.monitor === "monitor_only" ? "Monitored only (not in the output)" : "Monitored"}><${Icon} name="headphones" size=${14} /></span>`}
      ${!mixed && html`<span class="strip-flag faint" title="Not in the program mix (⋮ → Include in Program Mix)">not in mix</span>`}
      <span class="db tabular">${gainText(audio.gain_db)}</span>
    </div>
    <${Meter} source=${source.id} gain=${audio.gain_db} muted=${audio.muted} />
    <div class="strip-controls">
      <div class="fader" onDblClick=${() => setAudio(source, { gain_db: 0 })} title="Volume: drag; double-click for 0 dB">
        <${Slider}
          value=${Math.max(FLOOR, Math.min(0, audio.gain_db))}
          min=${FLOOR}
          max=${0}
          step=${0.1}
          label=${`Volume of ${sourceName(source)}`}
          onInput=${(gain_db) => setAudio(source, { gain_db }, live)}
          onChange=${(gain_db) => setAudio(source, { gain_db })}
        />
      </div>
      <button class=${"icon-btn small" + (audio.muted ? " muted-btn" : "")} title=${audio.muted ? "Unmute" : "Mute"} onClick=${() => setAudio(source, { muted: !audio.muted })}>
        <${Icon} name=${audio.muted ? "muted" : "speaker"} />
      </button>
      <button class="icon-btn small" title="More" onClick=${(e) => stripMenu(e.currentTarget, source, audio)}>
        <${Icon} name="moreVertical" />
      </button>
    </div>
  </div>`;
}

function mixerMenu(at) {
  showMenu(at, [
    { label: "Unhide All", disabled: !(getState().layout.mixerHidden ?? []).length, run: unhideAll },
    { label: "Advanced Audio Properties", run: () => openDialog("advanced-audio") },
  ]);
}

/** The sources scene `id` shows, through groups and nested scenes. */
function shownBy(setup, id, out = new Set(), depth = 0) {
  const scene = setup?.scenes?.find((s) => s.id === id);
  if (!scene || depth > 8) return out;
  const walk = (items) =>
    items.forEach((item) => {
      if (item.source) out.add(item.source);
      if (item.scene) shownBy(setup, item.scene, out, depth + 1);
      walk(item.children ?? []);
    });
  walk(scene.items ?? []);
  return out;
}

/** The mixer's sources, as OBS has them: audio-only sources (they play in
 * every scene, like OBS's global audio), and sources with sound that the
 * scene on air or in preview shows. */
function mixerSources(setup, scene, preview, studio) {
  const shown = shownBy(setup, scene);
  if (studio && preview) shownBy(setup, preview, shown);
  return (setup?.sources ?? []).filter((s) => hasAudio(s) && (!hasVideo(s) || shown.has(s.id)));
}

export function MixerDock() {
  const setup = useStore((s) => s.setup);
  const running = useStore((s) => s.running);
  const hidden = useStore((s) => s.layout.mixerHidden ?? []);
  const { scene, preview, studio } = useStore((s) => ({ scene: s.scene, preview: s.preview, studio: s.studio }));
  const sources = mixerSources(setup, scene, preview, studio);
  const shown = sources.filter((s) => !hidden.includes(s.id));
  const toolbar = html`
    <button class="icon-btn small" title="Advanced Audio Properties" disabled=${!sources.length} onClick=${() => openDialog("advanced-audio")}>
      <${Icon} name="settings" />
    </button>
    <button class="icon-btn small" title="Mixer menu" onClick=${(e) => mixerMenu(e.currentTarget)}><${Icon} name="moreVertical" /></button>
  `;
  return html`<${Dock} title="Audio Mixer" toolbar=${toolbar}>
    <div class="mixer" onContextMenu=${(e) => (e.preventDefault(), mixerMenu(e))}>
      ${shown.map(
        (source) => html`<${Strip} key=${source.id} source=${source} mixed=${inProgramMix(setup, source.id)} status=${running[source.id]} />`,
      )}
      ${setup && sources.length === 0 && html`<div class="list-empty">No audio sources. Add an Audio Input Capture (a microphone) with + under Sources.</div>`}
      ${sources.length > 0 && shown.length === 0 && html`<div class="list-empty">Every audio source is hidden. Right-click → Unhide All.</div>`}
    </div>
  <//>`;
}
