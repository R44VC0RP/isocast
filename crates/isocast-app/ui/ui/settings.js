// Settings, as in OBS: categories on the left (General, Stream, Output,
// Audio, Video, Hotkeys, Advanced), a page of settings on the right, and
// OK / Cancel / Apply at the bottom right. Pages change a draft; Apply
// (or OK) sends what changed to the live setup as one patch through the
// daemon (`apply`). The stream key goes straight to the Keychain through
// `stream_key_set` and is never shown again.

import { useEffect, useRef, useState } from "preact/hooks";
import { html } from "../lib/html.js";
import { call } from "../lib/bridge.js";
import { preference, savePreference, useStore } from "../lib/store.js";
import { hasVideo, sourceName } from "../lib/model.js";
import { mergePatch, objectPatch } from "../lib/patch.js";
import { edit, failed, loadDevices, refreshStatus, toast } from "../lib/session.js";
import { microphoneOptions, outputOptions } from "../lib/devices.js";
import { NumberField, RefreshDevices, Segmented, Select, Switch, TextField } from "./controls.js";
import { chooseFiles } from "./fields.js";
import { Icon } from "./icons.js";
import { Dialog, closeDialog, openDialog, registerDialog } from "./dialog.js";
import { SHORTCUTS } from "./shortcuts.js";
import { PREVIEW_RATES, previewRate, setPreviewRate } from "./monitor.js";

const CATEGORIES = [
  ["general", "General", "settings"],
  ["stream", "Stream", "live"],
  ["output", "Output", "disk"],
  ["audio", "Audio", "speaker"],
  ["video", "Video", "display"],
  ["hotkeys", "Hotkeys", "keyboard"],
  ["advanced", "Advanced", "wand"],
];

/** Open Settings, at a category, optionally saying why. */
export function openSettings(tab = null, notice = null) {
  openDialog("settings", { tab, notice });
}

/** Canvas snapping, a per-viewer preference (Settings → General). */
export const snapping = () => ({ enabled: true, distance: 8, ...preference("snapping", {}) });

export const SERVICES = [
  ["twitch", "Twitch"],
  ["youtube", "YouTube"],
  ["kick", "Kick"],
  ["facebook", "Facebook"],
  ["custom", "Custom server"],
];

// Where each service's key (and for Kick, server) comes from.
const SERVICE_HINTS = {
  twitch: "Key from the Creator Dashboard",
  youtube: "Key from YouTube Studio",
  kick: "Server and key from the Creator Dashboard",
  facebook: "Key from Live Producer",
  custom: "Any RTMP or RTMPS server",
};

const PRESET_SERVERS = {
  twitch: "rtmps://ingest.global-contribute.live-video.net/app",
  youtube: "rtmps://a.rtmps.youtube.com/live2",
  facebook: "rtmps://rtmp-api.facebook.com/rtmp",
};

const AUDIO_BITRATES = [64, 96, 128, 160, 192, 256, 320];

const CODECS = [
  ["hevc", "HEVC (H.265)"],
  ["h264", "H.264"],
  ["prores422_proxy", "ProRes 422 Proxy"],
  ["prores422_lt", "ProRes 422 LT"],
  ["prores422", "ProRes 422"],
  ["prores422_hq", "ProRes 422 HQ"],
  ["prores4444", "ProRes 4444"],
];

const CANVASES = [
  ["1920x1080", "1920×1080"],
  ["2560x1440", "2560×1440"],
  ["3840x2160", "3840×2160"],
  ["1280x720", "1280×720"],
  ["1080x1920", "1080×1920 (vertical)"],
];

const FPS = [24, 25, 30, 50, 60].map((f) => [String(f), String(f)]);

/** A row: a label and its control, with an optional hint under it. */
function Row({ label, hint, children }) {
  return html`<div class="set-row">
    <div class="set-label">${label}</div>
    <div class="set-control">
      <div class="set-inline">${children}</div>
      ${hint && html`<div class="set-hint">${hint}</div>`}
    </div>
  </div>`;
}

function Group({ title, children }) {
  return html`<fieldset class="set-group">
    ${title && html`<legend class="set-group-title">${title}</legend>`}
    ${children}
  </fieldset>`;
}

/** The parts of a setup Settings changes. */
const pick = (setup) => {
  const out = { name: setup.name, canvas: setup.canvas, recording: setup.recording, audio: setup.audio ?? {} };
  if (setup.stream) out.stream = setup.stream;
  return structuredClone(out);
};

function SettingsDialog({ tab: initialTab, notice }) {
  const setup = useStore((s) => s.setup);
  const [tab, setTab] = useState(initialTab ?? "general");
  const base = useRef(setup ? pick(setup) : null);
  const [draft, setDraft] = useState(() => base.current);
  const prefsBase = useRef(snapping());
  const [prefs, setPrefs] = useState(() => prefsBase.current);
  const rateBase = useRef(previewRate());
  const [rate, setRate] = useState(() => rateBase.current);
  const [applying, setApplying] = useState(false);
  // Microphone sources' devices (Audio → Audio Devices), by source id.
  const [mics, setMics] = useState({});
  const patch = draft && base.current ? objectPatch(base.current, draft) : {};
  const micChanges = Object.entries(mics).filter(([id, device]) => setup?.sources.find((s) => s.id === id)?.device !== device);
  if (micChanges.length) patch.sources = micChanges.map(([id, device]) => ({ id, device }));
  const prefsChanged = JSON.stringify(prefs) !== JSON.stringify(prefsBase.current) || rate !== rateBase.current;
  const changed = Object.keys(patch).length > 0 || prefsChanged;
  const set = (change) => setDraft((d) => mergePatch(d, change));
  const apply = async () => {
    if (prefsChanged) {
      savePreference("snapping", prefs);
      prefsBase.current = prefs;
      setPreviewRate(rate);
      rateBase.current = rate;
    }
    if (!Object.keys(patch).length) return true;
    setApplying(true);
    const ok = await edit(patch);
    setApplying(false);
    if (ok) {
      base.current = draft;
      setDraft({ ...draft });
      setMics({});
      refreshStatus(true);
    }
    return ok;
  };
  const accept = async () => {
    if (await apply()) closeDialog();
  };
  const page = () => {
    if (!draft) return html`<p class="dim">Set up your first scene to change its settings.</p>`;
    switch (tab) {
      case "general":
        return html`<${GeneralPage} draft=${draft} set=${set} prefs=${prefs} setPrefs=${setPrefs} rate=${rate} setRate=${setRate} />`;
      case "stream":
        return html`<${StreamPage} draft=${draft} set=${set} notice=${notice} />`;
      case "output":
        return html`<${OutputPage} draft=${draft} set=${set} sources=${setup.sources} />`;
      case "audio":
        return html`<${AudioPage} draft=${draft} set=${set} mics=${mics} setMic=${(id, device) => setMics((m) => ({ ...m, [id]: device }))} />`;
      case "video":
        return html`<${VideoPage} draft=${draft} set=${set} />`;
      case "hotkeys":
        return html`<${HotkeysPage} />`;
      default:
        return html`<${AdvancedPage} draft=${draft} set=${set} />`;
    }
  };
  return html`<${Dialog}
    title="Settings"
    width=${Math.min(940, innerWidth - 40)}
    height=${Math.min(680, innerHeight - 40)}
    class="settings-dialog"
    onCancel=${closeDialog}
    onAccept=${accept}
    buttons=${html`<button class="btn" onClick=${closeDialog}>Cancel</button>
      <button class="btn" disabled=${!changed || applying} onClick=${apply}>Apply</button>
      <button class="btn primary" disabled=${applying} onClick=${accept}>OK</button>`}
  >
    <div class="settings-layout">
      <nav class="set-nav">
        ${CATEGORIES.map(
          ([id, label, icon]) => html`<button key=${id} class=${"set-tab" + (tab === id ? " on" : "")} onClick=${() => setTab(id)}>
            <${Icon} name=${icon} />${label}
          </button>`,
        )}
      </nav>
      <section class="set-page">
        <div class="set-body">${page()}</div>
      </section>
    </div>
  <//>`;
}

registerDialog("settings", SettingsDialog);

// ---------- General ----------

function GeneralPage({ draft, set, prefs, setPrefs, rate, setRate }) {
  const file = useStore((s) => s.status?.setup_file?.path);
  return html`
    <${Group} title="Setup">
      <${Row} label="Name" hint="Shown in the title bar.">
        <div class="grow"><${TextField} value=${draft.name} onChange=${(name) => name.trim() && set({ name: name.trim() })} /></div>
      <//>
      ${file &&
      html`<${Row} label="Kept in" hint="Isocast saves the live setup here after every change (the one before as .bak).">
        <span class="path dim mono" title=${file}><bdi>${file}</bdi></span>
      <//>`}
    <//>
    <${Group} title="Snapping">
      <${Row} label="Enable" hint="Edges and centres snap to the canvas and to other sources; hold ⌘ while dragging to place freely.">
        <${Switch} on=${prefs.enabled} onChange=${(enabled) => setPrefs({ ...prefs, enabled })} />
      <//>
      <${Row} label="Snap Sensitivity">
        <div class="w-s"><${NumberField} value=${prefs.distance} min=${1} max=${40} unit="px" onChange=${(distance) => setPrefs({ ...prefs, distance })} /></div>
      <//>
    <//>
    <${Group} title="Preview">
      <${Row} label="Preview Frame Rate" hint="Match canvas shows motion as recorded; 30 fps uses less power (monitors still follow the canvas rate while you drag).">
        <${Segmented} value=${rate} options=${PREVIEW_RATES} onChange=${setRate} />
      <//>
    <//>
  `;
}

// ---------- Stream ----------

function StreamPage({ draft, set, notice }) {
  const stream = draft.stream;
  const status = useStore((s) => s.stream);
  // Say why Start Streaming opened this page, until the stream is ready.
  const ready = stream && status?.key_set;
  const callout = notice && !ready && html`<div class="callout info"><${Icon} name="live" size=${14} /><div>${notice}</div></div>`;
  const setStream = (fields) => set({ stream: fields });
  if (!stream) return html`${callout}<${ChooseService} set=${set} />`;
  const preset = PRESET_SERVERS[stream.service];
  const live = ["live", "connecting", "reconnecting"].includes(status?.state);
  const sameSize = stream.width == null;
  return html`
    ${callout}
    ${live && html`<div class="callout info"><${Icon} name="info" size=${14} /><div>Changes apply the next time you start streaming.</div></div>`}
    <${Group}>
      <${Row} label="Service">
        <div class="w-m"><${Select} value=${stream.service} options=${SERVICES}
          onChange=${(service) => setStream({ service, server: service === stream.service ? stream.server : null })} /></div>
      <//>
      <${Row}
        label="Server"
        hint=${preset ? "Leave empty for the service’s recommended server." : stream.service === "kick" ? "Your channel’s Stream URL, from Kick’s Creator Dashboard." : "Like rtmp://host/app or rtmps://host/app (without the key)."}
      >
        <div class="grow"><${TextField} mono value=${stream.server ?? ""} placeholder=${preset ?? "rtmps://…"} onChange=${(server) => setStream({ server: server.trim() || null })} /></div>
      <//>
      <${StreamKeyRow} stream=${stream} status=${status} />
    <//>
    <${Group} title="Output">
      <${Row} label="Video Bitrate" hint="Twitch suggests up to 6000 kb/s; YouTube 4500–9000 for 1080p.">
        <div class="w-s"><${NumberField} value=${stream.video_bitrate_kbps ?? 6000} min=${200} max=${50000} step=${100} unit="kb/s"
          onChange=${(video_bitrate_kbps) => setStream({ video_bitrate_kbps })} /></div>
      <//>
      <${Row} label="Rate Control" hint="Constant (CBR) is what YouTube and Twitch ask for. Average sends less when the picture is still.">
        <${Segmented} value=${stream.rate_control ?? "constant"}
          options=${[["constant", "Constant (CBR)"], ["average", "Average (VBR)"]]} onChange=${(rate_control) => setStream({ rate_control })} />
      <//>
      <${Row} label="Audio Bitrate">
        <div class="w-s"><${Select} value=${String(stream.audio_bitrate_kbps ?? 160)}
          options=${AUDIO_BITRATES.map((b) => [String(b), `${b} kb/s`])} onChange=${(b) => setStream({ audio_bitrate_kbps: Number(b) })} /></div>
      <//>
      <${Row} label="Keyframe Interval" hint="Every 2 s is what Twitch and YouTube ask for.">
        <div class="w-s"><${NumberField} value=${stream.keyframe_interval_seconds ?? 2} min=${1} max=${10} unit="s"
          onChange=${(keyframe_interval_seconds) => setStream({ keyframe_interval_seconds })} /></div>
      <//>
      <${Row} label="Output Resolution" hint=${sameSize ? "The canvas, scaled down to fit 1080p if it is larger." : ""}>
        <${Segmented} value=${sameSize ? "canvas" : "custom"} options=${[["canvas", "Canvas"], ["custom", "Custom"]]}
          onChange=${(v) => setStream(v === "canvas" ? { width: null, height: null } : { width: 1280, height: 720 })} />
        ${!sameSize &&
        html`<div class="w-xs"><${NumberField} label="W" value=${stream.width} min=${16} max=${8192} step=${2} onChange=${(width) => setStream({ width })} /></div>
          <div class="w-xs"><${NumberField} label="H" value=${stream.height} min=${16} max=${8192} step=${2} onChange=${(height) => setStream({ height })} /></div>`}
      <//>
    <//>
    <div class="set-foot">
      <button class="btn danger" disabled=${live} onClick=${() => set({ stream: null })}>Turn off streaming</button>
    </div>
  `;
}

/** No stream yet: pick a service; custom servers and Kick need their
 * server address first (a setup can't name one without it). */
function ChooseService({ set }) {
  const [needsServer, setNeedsServer] = useState(null);
  const [server, setServer] = useState("");
  const choose = (service, address) => set({ stream: { service, ...(address ? { server: address } : {}) } });
  if (needsServer) {
    const valid = /^rtmps?:\/\/[^/]+\/.+/.test(server.trim());
    return html`<div class="set-empty">
      <p>${needsServer === "kick"
        ? "Kick gives each channel its own server: copy the Stream URL from the Creator Dashboard (Channel → Stream URL and Key)."
        : "The server’s address, without the stream key: like rtmp://host/app or rtmps://host/app."}</p>
      <div class="set-inline" style="max-width: 560px; margin-top: 12px">
        <div class="field grow"><input class="mono" value=${server} placeholder="rtmps://…" spellcheck="false"
          onInput=${(e) => setServer(e.currentTarget.value)} onKeyDown=${(e) => e.key === "Enter" && valid && (e.stopPropagation(), choose(needsServer, server.trim()))} /></div>
        <button class="btn" onClick=${() => setNeedsServer(null)}>Back</button>
        <button class="btn primary" disabled=${!valid} onClick=${() => choose(needsServer, server.trim())}>Continue</button>
      </div>
    </div>`;
  }
  return html`<div class="set-empty">
    <p>Stream the program live: the canvas and the program mix, sent as H.264 and AAC over RTMP or RTMPS. Choose where to:</p>
    <div class="service-grid">
      ${SERVICES.map(
        ([id, label]) => html`<button key=${id} class="service" onClick=${() => (id === "custom" || id === "kick" ? setNeedsServer(id) : choose(id))}>
          <span class="name">${label}</span>
          <span class="hint">${SERVICE_HINTS[id]}</span>
        </button>`,
      )}
    </div>
  </div>`;
}

function StreamKeyRow({ stream, status }) {
  const [key, setKey] = useState("");
  const [busy, setBusy] = useState(false);
  const target = { service: stream.service, ...(stream.service === "custom" && stream.server ? { server: stream.server } : {}) };
  // The engine says whether a key is stored for the live setup's service.
  const known = status?.service === stream.service;
  const save = async () => {
    if (!key.trim()) return;
    setBusy(true);
    try {
      await call("stream_key_set", { key: key.trim(), ...target });
      setKey("");
      toast("success", "Stream key saved", "It’s in your Keychain; Isocast never shows it again.");
      await refreshStatus(true);
    } catch (error) {
      failed("Couldn’t save the stream key", error);
    } finally {
      setBusy(false);
    }
  };
  const clear = async () => {
    try {
      await call("stream_key_clear", target);
      toast("info", "Stream key removed from the Keychain");
      await refreshStatus(true);
    } catch (error) {
      failed("Couldn’t remove the stream key", error);
    }
  };
  const saved = known && status?.key_set;
  return html`<${Row} label="Stream Key" hint=${saved ? "Saved in the Keychain. Paste a new one to replace it." : "From your service’s dashboard. It’s kept in the Keychain (saved at once), never in the setup or logs."}>
    <div class="field grow">
      <input type="password" autocomplete="off" spellcheck="false" placeholder=${saved ? "••••••••••••  saved" : "Paste the key"} value=${key}
        onInput=${(e) => setKey(e.currentTarget.value)} onKeyDown=${(e) => e.key === "Enter" && (e.stopPropagation(), save())} />
    </div>
    <button class="btn" disabled=${!key.trim() || busy} onClick=${save}>Save Key</button>
    ${saved && html`<button class="btn" onClick=${clear}>Remove</button>`}
  <//>`;
}

// ---------- Output ----------

// `autoDepth` (ISO files): the depth can be left out, for the engine to
// pick each file's ("Auto"); H.264 then gets 8 bits without a set depth.
function EncoderRows({ encoder = {}, onChange, autoDepth = false }) {
  const codec = encoder.codec ?? "hevc";
  const prores = codec.startsWith("prores");
  const depths = autoDepth
    ? [["auto", "Auto"], [8, "8-bit"], [10, "10-bit"]]
    : [[8, "8-bit"], [10, "10-bit"]];
  const depth = encoder.bit_depth ?? (autoDepth ? "auto" : 8);
  const toH264 = !autoDepth || (encoder.bit_depth != null && encoder.bit_depth !== 8);
  return html`
    <${Row} label="Encoder">
      <div class="w-m"><${Select} value=${codec} options=${CODECS} onChange=${(c) => onChange({ codec: c, ...(c === "h264" && toH264 ? { bit_depth: 8 } : {}) })} /></div>
    <//>
    <${Row} label="Recording Format">
      <${Segmented} value=${encoder.container ?? "mov"} options=${[["mov", "MOV"], ["mp4", "MP4"]]} onChange=${(container) => onChange({ container })} />
      ${!prores && codec !== "h264" &&
      html`<${Segmented} value=${depth} options=${depths} onChange=${(bit_depth) => onChange({ bit_depth: bit_depth === "auto" ? null : bit_depth })} />`}
    <//>
  `;
}

// Recording quality, as OBS's simple output mode names it: five stops the
// setup's `encoder.quality` accepts by name. Numbers (1–100) still work;
// these are the numbers each name stands for here, to place a number on
// the slider (an assumption until the engine says otherwise: "high" is
// the default, 70).
export const QUALITIES = [
  ["low", "Low", 40],
  ["medium", "Medium", 55],
  ["high", "High", 70],
  ["very_high", "Very high", 80],
  ["incredible", "Incredible", 90],
];

/** A quality (a name, a number or nothing) as a number. */
const qualityNumber = (q) => (typeof q === "number" ? q : QUALITIES.find(([name]) => name === q)?.[2] ?? 70);
/** The stop a quality is exactly, if it is one. */
const qualityStop = (q) => QUALITIES.findIndex(([name, , n]) => q === name || q === n || (q == null && name === "high"));
/** The stop nearest a quality. */
const nearestStop = (q) => {
  const n = qualityNumber(q);
  let best = 0;
  QUALITIES.forEach(([, , v], i) => Math.abs(v - n) < Math.abs(QUALITIES[best][2] - n) && (best = i));
  return best;
};

/** About how many GB an hour of recording takes, if the engine says
 * (status `recording_estimate.mb_per_minute`, all files together). */
function useEstimate() {
  const mbPerMinute = useStore((s) => s.status?.recording_estimate?.mb_per_minute ?? null);
  return mbPerMinute == null ? null : (mbPerMinute * 60) / 1000;
}

/** Five stops from Low to Incredible; a number that isn't one of them
 * shows as Custom at the nearest stop. */
function QualitySlider({ value, onChange, disabled, custom = false }) {
  const exact = custom ? -1 : qualityStop(value);
  const at = exact >= 0 ? exact : nearestStop(value);
  const track = useRef();
  const pick = (event) => {
    const box = track.current.getBoundingClientRect();
    const f = Math.min(1, Math.max(0, (event.clientX - box.left) / box.width));
    const i = Math.round(f * (QUALITIES.length - 1));
    if (i !== exact) onChange(QUALITIES[i][0]);
  };
  const down = (event) => {
    if (disabled || event.button !== 0) return;
    pick(event);
    const move = (e) => pick(e);
    const up = () => {
      removeEventListener("pointermove", move);
      removeEventListener("pointerup", up);
    };
    addEventListener("pointermove", move);
    addEventListener("pointerup", up);
  };
  const key = (event) => {
    const by = { ArrowLeft: -1, ArrowDown: -1, ArrowRight: 1, ArrowUp: 1 }[event.key];
    if (!by || disabled) return;
    event.preventDefault();
    const i = Math.min(QUALITIES.length - 1, Math.max(0, at + by));
    if (i !== exact) onChange(QUALITIES[i][0]);
  };
  const percent = (i) => (i / (QUALITIES.length - 1)) * 100;
  return html`<div class=${"quality" + (disabled ? " disabled" : "") + (exact < 0 ? " custom" : "")}>
    <div class="quality-track" ref=${track} tabindex=${disabled ? -1 : 0} role="slider" aria-valuetext=${exact >= 0 ? QUALITIES[exact][1] : "Custom"}
      onPointerDown=${down} onKeyDown=${key}>
      <div class="quality-rail" />
      <div class="quality-fill" style=${{ width: `${percent(at)}%` }} />
      ${QUALITIES.map((_, i) => html`<span key=${i} class=${"quality-tick" + (i <= at ? " on" : "")} style=${{ left: `${percent(i)}%` }} />`)}
      <span class="quality-thumb" style=${{ left: `${percent(at)}%` }} />
    </div>
    <div class="quality-labels">
      ${QUALITIES.map(
        ([name, label], i) => html`<button key=${name} class=${"quality-label" + (i === exact ? " on" : "")} style=${{ left: `${percent(i)}%` }}
          disabled=${disabled} onClick=${() => onChange(name)}>${label}</button>`,
      )}
    </div>
  </div>`;
}

/** Recording Quality: one slider for the program and the ISO files, and
 * under Advanced, each one's number. */
function QualityRows({ program, iso, setRec }) {
  const [advanced, setAdvanced] = useState(false);
  const estimate = useEstimate();
  const encoders = [program.enabled !== false && program.encoder, iso.enabled && iso.encoder].filter(Boolean);
  const prores = encoders.length > 0 && encoders.every((e) => (e.codec ?? "hevc").startsWith("prores"));
  const pq = program.encoder?.quality;
  const iq = iso.encoder?.quality;
  // One value when both say the same (or only one records); else Custom.
  const same = qualityNumber(pq) === qualityNumber(iq) || !iso.enabled || program.enabled === false;
  const shown = same ? (iso.enabled && program.enabled === false ? iq : pq) : null;
  const stop = prores ? -1 : same ? qualityStop(shown) : -1;
  const setBoth = (quality) => setRec({ program: { encoder: { quality } }, iso: { encoder: { quality } } });
  const status = prores
    ? "ProRes files are made at a fixed quality"
    : same
        ? `Custom (${qualityNumber(shown)})`
        : `Custom (program ${qualityNumber(pq)}, ISO ${qualityNumber(iq)})`;
  return html`
    <${Row} label="Recording Quality">
      <div class="quality-row">
        <${QualitySlider} value=${same ? shown : pq} custom=${!same} disabled=${prores} onChange=${setBoth} />
        ${(stop < 0 || (estimate != null && !prores)) &&
        html`<div class="quality-status">
          ${stop < 0 && html`<span class="dim">${status}</span>`}
          ${estimate != null && !prores && html`<span class="dim">≈ ${estimate < 10 ? estimate.toFixed(1) : Math.round(estimate)} GB per hour</span>`}
        </div>`}
      </div>
    <//>
    <${Row} label="">
      <button class="disclosure" aria-expanded=${advanced} onClick=${() => setAdvanced(!advanced)}>
        <${Icon} name=${advanced ? "chevronDown" : "chevronRight"} size=${14} />Advanced
      </button>
    <//>
    ${advanced &&
    html`<${Row} label="Program Quality" hint="Constant quality, 1–100; higher makes bigger files.">
        <div class="w-s"><${NumberField} value=${qualityNumber(pq)} min=${1} max=${100} disabled=${program.enabled === false}
          onChange=${(quality) => setRec({ program: { encoder: { quality } } })} /></div>
      <//>
      <${Row} label="ISO Quality">
        <div class="w-s"><${NumberField} value=${qualityNumber(iq)} min=${1} max=${100} disabled=${!iso.enabled}
          onChange=${(quality) => setRec({ iso: { encoder: { quality } } })} /></div>
      <//>`}
  `;
}

function OutputPage({ draft, set, sources }) {
  const rec = draft.recording;
  const iso = rec.iso ?? {};
  const program = rec.program ?? {};
  const videoSources = sources.filter(hasVideo);
  const setRec = (fields) => set({ recording: fields });
  const chooseFolder = async () => {
    const [folder] = await chooseFiles({ folders: true, prompt: "Choose" });
    if (folder) setRec({ folder });
  };
  const isoSources = iso.sources ?? [];
  const toggleIso = (id, on) => setRec({ iso: { sources: on ? [...isoSources, id] : isoSources.filter((s) => s !== id) } });
  return html`
    <${Group} title="Recording">
      <${Row} label="Recording Path">
        <div class="grow"><${TextField} mono showEnd value=${rec.folder} onChange=${(folder) => folder.trim() && setRec({ folder: folder.trim() })} /></div>
        <button class="btn" onClick=${chooseFolder}>Browse</button>
      <//>
      <${QualityRows} program=${program} iso=${iso} setRec=${setRec} />
      <${Row} label="Keep Free" hint="A recording won’t start without room for it plus this, and stops cleanly when the disk gets this low.">
        <div class="w-s"><${NumberField} value=${rec.keep_free_gb ?? 1} min=${0} max=${10000} step=${0.5} unit="GB" onChange=${(keep_free_gb) => setRec({ keep_free_gb })} /></div>
      <//>
    <//>
    <${Group} title="Program">
      <${Row} label="Record Program" hint="The canvas as everyone sees it, with the program mix.">
        <${Switch} on=${program.enabled !== false} onChange=${(enabled) => setRec({ program: { enabled } })} />
      <//>
      ${program.enabled !== false && html`<${EncoderRows} encoder=${program.encoder} onChange=${(encoder) => setRec({ program: { encoder } })} />`}
    <//>
    <${Group} title="ISO Recording">
      <${Row} label="Record ISO Files" hint="Each source chosen below in its own file, at its own size, on the program’s clock. Also in each source’s right-click menu.">
        <${Switch} on=${!!iso.enabled} onChange=${(enabled) => setRec({ iso: { enabled } })} />
      <//>
      ${iso.enabled &&
      html`<${Row} label="Sources">
          <div class="check-list">
            ${videoSources.map(
              (s) => html`<label key=${s.id} class="check">
                <input type="checkbox" checked=${isoSources.includes(s.id)} onChange=${(e) => toggleIso(s.id, e.currentTarget.checked)} />
                <span>${sourceName(s)}</span>
              </label>`,
            )}
            ${videoSources.length === 0 && html`<span class="dim">No video sources yet.</span>`}
          </div>
        <//>
        <${EncoderRows} encoder=${iso.encoder} autoDepth=${true} onChange=${(encoder) => setRec({ iso: { encoder } })} />
        <${Row} label="With Filters" hint="Record sources as the program shows them (filters applied) instead of as captured.">
          <${Switch} on=${!!iso.filtered} onChange=${(filtered) => setRec({ iso: { filtered } })} />
        <//>`}
    <//>
  `;
}

// ---------- Audio ----------

const PAN_LAWS = [
  ["sine", "Sine (−5.3 dB at half)"],
  ["square", "Constant power (−3 dB at half)"],
  ["linear", "Linear (−6 dB at half)"],
];

function AudioPage({ draft, set, mics, setMic }) {
  const devices = useStore((s) => s.devices);
  const sources = useStore((s) => s.setup?.sources ?? []);
  useEffect(() => {
    loadDevices(true);
  }, []);
  const [looking, setLooking] = useState(false);
  const refresh = async () => {
    setLooking(true);
    await loadDevices(true);
    setLooking(false);
  };
  const audio = draft.audio ?? {};
  const device = audio.monitoring_device ?? "default";
  const micSources = sources.filter((s) => s.kind === "microphone");
  return html`
    ${micSources.length > 0 &&
    html`<${Group} title="Audio Devices">
      ${micSources.map((m, i) => {
        const value = mics[m.id] ?? m.device;
        return html`<${Row} key=${m.id} label=${sourceName(m)}>
          <div class="w-l"><${Select} value=${value} options=${microphoneOptions(devices, value)}
            onChange=${(device) => setMic(m.id, device)} /></div>
          ${i === 0 ? html`<${RefreshDevices} busy=${looking} onClick=${refresh} />` : null}
        <//>`;
      })}
      <div class="set-hint">Your Audio Input Capture sources. Add more with + under Sources.</div>
    <//>`}
    <${Group} title="Advanced">
      <${Row} label="Monitoring Device" hint="Where sources you monitor play (Audio Mixer → ⋮ → Audio Monitoring).">
        <div class="w-l"><${Select} value=${device} options=${outputOptions(devices, device)} onChange=${(monitoring_device) => set({ audio: { monitoring_device } })} /></div>
        <${RefreshDevices} busy=${looking} onClick=${refresh} />
      <//>
      <${Row} label="Balance Law" hint="How moving a source towards one side lowers the other.">
        <div class="w-l"><${Select} value=${audio.pan_law ?? "sine"} options=${PAN_LAWS} onChange=${(pan_law) => set({ audio: { pan_law } })} /></div>
      <//>
    <//>
  `;
}

// ---------- Video ----------

function VideoPage({ draft, set }) {
  const { width, height, fps } = draft.canvas;
  const size = `${width}x${height}`;
  const known = CANVASES.some(([v]) => v === size);
  const setSize = (value) => {
    if (value === "custom") return;
    const [w, h] = value.split("x").map(Number);
    set({ canvas: { width: w, height: h } });
  };
  return html`
    <${Group} title="Video">
      <${Row} label="Base (Canvas) Resolution" hint="Recordings and the program are made at this size. Sources keep their pixel positions when it changes.">
        <div class="w-m"><${Select} value=${known ? size : "custom"} options=${[...CANVASES, ["custom", `Custom (${width}×${height})`]]} onChange=${setSize} /></div>
        <div class="w-xs"><${NumberField} label="W" value=${width} min=${16} max=${8192} step=${2} onChange=${(w) => set({ canvas: { width: w } })} /></div>
        <div class="w-xs"><${NumberField} label="H" value=${height} min=${16} max=${8192} step=${2} onChange=${(h) => set({ canvas: { height: h } })} /></div>
      <//>
      <${Row} label="Common FPS Values">
        <div class="w-s"><${Select} value=${String(fps)} options=${FPS.some(([v]) => v === String(fps)) ? FPS : [[String(fps), String(fps)], ...FPS]}
          onChange=${(f) => set({ canvas: { fps: Number(f) } })} /></div>
      <//>
    <//>
  `;
}

// ---------- Hotkeys ----------

function HotkeysPage() {
  return html`<div class="hotkeys">
    <p class="set-hint" style="margin: 4px 0 12px">Isocast’s keys are fixed for now. They don’t fire while you type in a field.</p>
    ${SHORTCUTS.map(
      ([title, rows]) => html`<${Group} key=${title} title=${title}>
        ${rows.map(
          ([keys, what]) => html`<div class="shortcut">
            <span class="what">${what}</span>
            <span class="keys">${keys.map((k) => (k === "–" ? html`<span class="to">–</span>` : html`<kbd>${k}</kbd>`))}</span>
          </div>`,
        )}
      <//>`,
    )}
  </div>`;
}

// ---------- Advanced ----------

function AdvancedPage({ draft, set }) {
  const stream = draft.stream;
  const reconnect = stream?.reconnect ?? {};
  const setReconnect = (fields) => set({ stream: { reconnect: fields } });
  return html`
    <${Group} title="Automatically Reconnect">
      ${!stream && html`<div class="set-hint">Set up streaming (Stream) to change how it reconnects.</div>`}
      ${stream &&
      html`<${Row} label="Enable">
          <${Switch} on=${reconnect.enabled !== false} onChange=${(enabled) => setReconnect({ enabled })} />
        <//>
        <${Row} label="Retry Delay">
          <div class="w-s"><${NumberField} value=${reconnect.delay_seconds ?? 2} min=${1} max=${600} unit="s"
            disabled=${reconnect.enabled === false} onChange=${(delay_seconds) => setReconnect({ delay_seconds })} /></div>
        <//>
        <${Row} label="Maximum Retries">
          <div class="w-s"><${NumberField} value=${reconnect.max_retries ?? 25} min=${1} max=${1000}
            disabled=${reconnect.enabled === false} onChange=${(max_retries) => setReconnect({ max_retries })} /></div>
        <//>`}
    <//>
  `;
}
