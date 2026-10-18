// The first run: permissions, then the devices to use, then the layout,
// then `init_setup` makes the setup (scenes "Screen + Camera", "Camera"
// and "Screen", recording the program and ISO files) and the main window
// opens on it.

import { useEffect, useState } from "preact/hooks";
import { html } from "../lib/html.js";
import { call, native } from "../lib/bridge.js";
import { setState, useStore } from "../lib/store.js";
import { failed, loadDevices, refreshStatus, reload, toast } from "../lib/session.js";
import { Segmented, Switch, TextField } from "./controls.js";
import { chooseFiles } from "./fields.js";
import { BrandMark, Icon } from "./icons.js";

const STEPS = ["Permissions", "Devices", "Layout"];

const PERMISSIONS = {
  screen_recording: { icon: "display", title: "Screen recording", why: "Displays, windows and system audio", pane: "Screen & System Audio Recording" },
  camera: { icon: "camera", title: "Camera", why: "Cameras, capture cards and iPhones", pane: "Camera" },
  microphone: { icon: "mic", title: "Microphone", why: "Microphones and audio interfaces", pane: "Microphone" },
};

const sentence = (text) => (text ? text.charAt(0).toUpperCase() + text.slice(1) : "");

const STATE_LABEL = {
  granted: "Allowed",
  denied: "Turned off",
  not_determined: "Not asked yet",
  restricted: "Blocked by a policy",
  not_granted: "Not allowed",
  unknown: "Unknown",
};

const LAYOUTS = [
  ["bottom_right", "Bottom right"],
  ["bottom_left", "Bottom left"],
  ["top_right", "Top right"],
  ["top_left", "Top left"],
  ["side_by_side", "Side by side"],
];

const CANVASES = [
  ["1920x1080", "1080p"],
  ["2560x1440", "1440p"],
  ["3840x2160", "4K"],
  ["1080x1920", "Vertical"],
];

/** Open the first-run flow (from the welcome screen or the setup menu). */
export function startOnboarding() {
  setState({ onboarding: true });
}

export function Onboarding() {
  const [step, setStep] = useState(0);
  const [choices, setChoices] = useState({
    display: "main",
    camera: null,
    microphone: "default",
    system_audio: true,
    layout: "bottom_right",
    canvas: "1920x1080",
    fps: 60,
    folder: "~/Movies/Isocast",
    name: "My setup",
  });
  const [creating, setCreating] = useState(false);
  const hasSetup = useStore((s) => !!s.setup);
  // Ask for the devices now, so they're there by the second step.
  useEffect(() => {
    loadDevices(true);
  }, []);
  const set = (fields) => setChoices((current) => ({ ...current, ...fields }));
  const close = () => setState({ onboarding: false });

  const create = async () => {
    setCreating(true);
    const [width, height] = choices.canvas.split("x").map(Number);
    const options = {
      display: choices.display,
      microphone: choices.microphone,
      system_audio: choices.system_audio,
      layout: choices.layout,
      width,
      height,
      fps: choices.fps,
      folder: choices.folder,
      name: choices.name.trim() || "My setup",
      ...(choices.camera ? { camera: choices.camera } : {}),
    };
    try {
      const made = await call("init_setup", { ...options, apply: true });
      await reload();
      await refreshStatus();
      setState({ onboarding: false });
      const scenes = made.setup?.scenes?.length ?? 0;
      toast(
        "success",
        `Set up “${options.name}”`,
        scenes > 1 ? `${scenes} scenes, ready to record. Switch between them with 1–${scenes}.` : "Ready to record.",
      );
    } catch (error) {
      failed("Couldn’t make the setup", error);
    } finally {
      setCreating(false);
    }
  };

  const pages = [
    html`<${PermissionsStep} />`,
    html`<${DevicesStep} choices=${choices} set=${set} />`,
    html`<${LayoutStep} choices=${choices} set=${set} />`,
  ];
  const last = step === STEPS.length - 1;
  return html`<div class="dialog-scrim onboarding">
    <div class="ob-card">
      <div class="ob-head">
        <${BrandMark} size=${28} />
        <ol class="ob-steps">
          ${STEPS.map(
            (name, i) => html`<li key=${name} class=${i === step ? "on" : i < step ? "done" : ""}>
              <span class="ob-step-dot">${i < step ? html`<${Icon} name="check" size=${12} />` : i + 1}</span>${name}
            </li>`,
          )}
        </ol>
        ${hasSetup && html`<button class="icon-btn" title="Close" onClick=${close}><${Icon} name="x" /></button>`}
      </div>
      <div class="ob-body">${pages[step]}</div>
      <div class="ob-foot">
        ${step === 0
          ? html`<button class="btn ghost" onClick=${close}>${hasSetup ? "Cancel" : "Back"}</button>`
          : html`<button class="btn ghost" onClick=${() => setStep(step - 1)}>Back</button>`}
        <span class="grow" />
        ${last
          ? html`<button class="btn primary large" disabled=${creating} onClick=${create}>
              ${creating ? html`<div class="spinner small" />` : null}Create setup
            </button>`
          : html`<button class="btn primary large" onClick=${() => setStep(step + 1)}>Continue</button>`}
      </div>
    </div>
  </div>`;
}

function PermissionsStep() {
  const [report, setReport] = useState(null);
  const [asking, setAsking] = useState(false);
  const check = () =>
    call("permissions")
      .then((next) => {
        setReport((before) => {
          const granted = (r) => (r?.details ?? []).filter((d) => d.state === "granted").length;
          // Newly allowed: what can be listed changed.
          if (before && granted(next) > granted(before)) loadDevices(true);
          return next;
        });
      })
      .catch((error) => failed("Couldn’t check permissions", error));
  useEffect(() => {
    check();
    // Granted in System Settings while this is open: notice it.
    const timer = setInterval(() => !document.hidden && check(), 4000);
    return () => clearInterval(timer);
  }, []);
  const ask = async () => {
    setAsking(true);
    try {
      setReport(await call("request_permissions"));
    } catch (error) {
      failed("Couldn’t ask for permissions", error);
    } finally {
      setAsking(false);
    }
  };
  const restart = async () => {
    try {
      await call("shutdown");
    } catch {
      // It went away mid-answer; that's the point.
    }
    native("reconnect");
    toast("info", "Restarting the engine", "It picks up the new permission when it starts again.");
    setTimeout(check, 3000);
  };
  const details = report?.details ?? [];
  const anyToAsk = details.some((d) => d.action === "request");
  const app = report?.app ?? "Isocast";
  // What to do, in the app's words (the daemon's next steps are for the
  // command line).
  const guidance = (d, info) => {
    if (d.state === "unknown") return d.next_step ?? "Access is checked when capture starts.";
    if (d.action === "open_settings") {
      return html`Turn on “${app}” in System Settings → Privacy & Security → ${info.pane}.${d.restart_after_granting
        ? html` Then <button class="link" onClick=${restart}>restart the engine</button>.`
        : ""}`;
    }
    if (d.action === "ask_administrator") return "A policy on this Mac blocks it; ask its administrator.";
    return null;
  };
  return html`
    <h1>Let Isocast see and hear</h1>
    <p class="ob-lead">${details.some((d) => d.state === "unknown")
      ? "Access is checked when capture starts; no per-app permission prompt is available. See each item for guidance."
      : "macOS asks you once for each. You can skip any of them now; your setup leaves out what it can’t use, and you can allow it later."}</p>
    <div class="ob-list">
      ${!report && html`<div class="ob-row"><div class="spinner" /><span class="dim">Checking…</span></div>`}
      ${details.map((d) => {
        const info = PERMISSIONS[d.permission] ?? { icon: "info", title: d.permission, why: "" };
        const granted = d.state === "granted";
        return html`<div class="ob-row" key=${d.permission}>
          <span class="ob-glyph"><${Icon} name=${info.icon} size=${20} /></span>
          <div class="grow">
            <div class="ob-row-title">${info.title}</div>
            <div class="ob-row-hint">${sentence(d.needed_for) || info.why}</div>
            ${!granted && guidance(d, info) && html`<div class="ob-row-hint ob-todo">${guidance(d, info)}</div>`}
          </div>
          <span class=${"ob-state" + (granted ? " ok" : d.state === "denied" ? " bad" : "")}>
            ${granted && html`<${Icon} name="check" size=${14} />`}${STATE_LABEL[d.state] ?? d.state}
          </span>
          ${d.action === "open_settings" &&
          html`<button class="btn" onClick=${() => call("open_permission_settings", { permission: d.permission }).catch((e) => failed("Couldn’t open System Settings", e))}>
            Open Settings
          </button>`}
        </div>`;
      })}
    </div>
    <div class="ob-actions">
      ${anyToAsk &&
      html`<button class="btn primary" disabled=${asking} onClick=${ask}>
        ${asking ? html`<div class="spinner small" />` : null}${asking ? "Waiting for your answers…" : "Allow access…"}
      </button>`}
      <button class="btn" onClick=${check}>Check again</button>
    </div>
  `;
}

function Choice({ on, title, meta, hint, disabled, onClick }) {
  return html`<button class=${"ob-choice" + (on ? " on" : "")} disabled=${disabled} onClick=${onClick} title=${hint ?? title}>
    <span class="ob-radio" />
    <span class="ob-choice-text">
      <span class="ellipsis">${title}</span>
      ${meta && html`<span class="ob-meta ellipsis">${meta}</span>`}
    </span>
  </button>`;
}

const CAMERA_KINDS = {
  external: "USB or capture card",
  continuity: "iPhone (Continuity Camera)",
  built_in: "Built-in",
  virtual: "Virtual camera",
  device_screen: "iPhone or iPad screen",
  desk_view: "Desk View",
};

function DevicesStep({ choices, set }) {
  const devices = useStore((s) => s.devices);
  useEffect(() => {
    loadDevices(true);
  }, []);
  const lead = html`<h1>Choose your devices</h1>
    <p class="ob-lead">What to capture. You can add more sources, and change these, once you’re in.</p>`;
  if (!devices) {
    return html`${lead}
      <div class="ob-columns">
        ${["Screen", "Camera", "Microphone"].map(
          (label) => html`<div class="ob-group" key=${label}>
            <div class="ob-label">${label}</div>
            <div class="ob-looking"><div class="spinner small" />Looking…</div>
          </div>`,
        )}
      </div>`;
  }
  const cameras = devices.cameras.filter((c) => c.kind !== "desk_view");
  return html`
    ${lead}
    <div class="ob-columns">
      <div class="ob-group">
        <div class="ob-label">Screen</div>
        ${devices.displays.map(
          (d) => html`<${Choice} key=${d.id} on=${choices.display === d.name || (choices.display === "main" && d.main)}
            title=${d.name} meta=${`${d.main ? "Main display · " : ""}${d.width}×${d.height}`} onClick=${() => set({ display: d.main ? "main" : d.name })} />`,
        )}
        ${devices.displays.length === 0 && html`<div class="ob-row-hint">No displays found${!["granted", "unknown"].includes(devices.permissions?.screen_recording) ? " (screen recording isn’t allowed yet)" : ""}.</div>`}
      </div>
      <div class="ob-group">
        <div class="ob-label">Camera</div>
        <${Choice} on=${choices.camera == null} title="Automatic" meta=${bestCamera(cameras)?.name ?? "None found"}
          hint="The best real camera: USB first, then iPhone, then built-in" onClick=${() => set({ camera: null })} />
        ${cameras.map((c) => {
          // Two devices with one name: tell them apart, and choose by id.
          const twins = cameras.filter((o) => o.name === c.name);
          const twin = twins.length > 1;
          const kind = c.suspended ? "Unavailable right now" : CAMERA_KINDS[c.kind] ?? "";
          const short = c.suspended ? "Unavailable" : CAMERA_SHORT[c.kind] ?? "";
          const value = twin ? c.id : c.name;
          return html`<${Choice} key=${c.id} on=${choices.camera === value} title=${c.name}
            meta=${twin ? `${short} · ${twins.indexOf(c) + 1} of ${twins.length}` : kind} disabled=${c.suspended}
            onClick=${() => set({ camera: value })} />`;
        })}
        <${Choice} on=${choices.camera === "none"} title="No camera" onClick=${() => set({ camera: "none" })} />
      </div>
      <div class="ob-group">
        <div class="ob-label">Microphone</div>
        <${Choice} on=${choices.microphone === "default"} title="System default" meta=${devices.microphones.find((m) => m.default)?.name ?? "Follows System Settings"}
          onClick=${() => set({ microphone: "default" })} />
        ${devices.microphones.map((m) => {
          const twins = devices.microphones.filter((o) => o.name === m.name);
          const twin = twins.length > 1;
          const value = twin ? m.id : m.name;
          return html`<${Choice} key=${m.id} on=${choices.microphone === value} title=${m.name}
            meta=${twin ? `${twins.indexOf(m) + 1} of ${twins.length}` : ""} onClick=${() => set({ microphone: value })} />`;
        })}
        <${Choice} on=${choices.microphone === "none"} title="No microphone" onClick=${() => set({ microphone: "none" })} />
        <div class="ob-switch-row">
          <${Switch} on=${choices.system_audio} onChange=${(system_audio) => set({ system_audio })} />
          <span>Include what this Mac plays</span>
        </div>
      </div>
    </div>
  `;
}

/** The camera `init_setup` picks when none is named (the daemon decides;
 * this mirrors its order to show the name): USB, then iPhone, then
 * built-in, then others and virtual ones, available before suspended. */
function bestCamera(cameras) {
  const rank = { external: 0, continuity: 1, built_in: 2, other: 3, virtual: 4, desk_view: 5 };
  const ranked = cameras
    .filter((c) => c.kind in rank)
    .map((c) => [rank[c.kind] + (c.suspended ? 10 : 0), c])
    .sort((a, b) => a[0] - b[0]);
  return ranked[0]?.[1] ?? null;
}

const CAMERA_SHORT = {
  external: "USB",
  continuity: "Continuity",
  built_in: "Built-in",
  virtual: "Virtual",
  device_screen: "Device screen",
  desk_view: "Desk View",
};

/** A tiny picture of a layout: the screen in grey, the camera in copper. */
function LayoutPicture({ layout }) {
  const cam = {
    bottom_right: { x: 46, y: 25, w: 22, h: 12 },
    bottom_left: { x: 4, y: 25, w: 22, h: 12 },
    top_right: { x: 46, y: 4, w: 22, h: 12 },
    top_left: { x: 4, y: 4, w: 22, h: 12 },
  }[layout];
  return html`<svg width="72" height="41" viewBox="0 0 72 41" aria-hidden="true">
    <rect x="0.5" y="0.5" width="71" height="40" style="fill: var(--control); stroke: var(--line-strong)" />
    ${layout === "side_by_side"
      ? html`<rect x="4" y="4" width="44" height="33" style="fill: var(--text-3)" /><rect x="51" y="4" width="17" height="33" style="fill: var(--accent)" />`
      : html`<rect x="4" y="4" width="64" height="33" style="fill: var(--text-3)" />
          <rect x=${cam.x} y=${cam.y} width=${cam.w} height=${cam.h} style="fill: var(--accent); stroke: var(--surface-panel)" />`}
  </svg>`;
}

function LayoutStep({ choices, set }) {
  const pickFolder = async () => {
    const [folder] = await chooseFiles({ folders: true, prompt: "Choose" });
    if (folder) set({ folder });
  };
  return html`
    <h1>Lay it out</h1>
    <p class="ob-lead">Where the camera goes over your screen. You get three scenes to switch between: Screen + Camera, Camera, and Screen.</p>
    <div class="ob-layouts">
      ${LAYOUTS.map(
        ([id, label]) => html`<button key=${id} class=${"ob-layout" + (choices.layout === id ? " on" : "")} onClick=${() => set({ layout: id })}>
          <${LayoutPicture} layout=${id} />
          <span>${label}</span>
        </button>`,
      )}
    </div>
    <div class="ob-form">
      <div class="ob-form-row">
        <span class="ob-label">Canvas</span>
        <${Segmented} value=${choices.canvas} options=${CANVASES} onChange=${(canvas) => set({ canvas })} />
        <${Segmented} value=${choices.fps} options=${[[30, "30 fps"], [60, "60 fps"]]} onChange=${(fps) => set({ fps })} />
      </div>
      <div class="ob-form-row">
        <span class="ob-label">Recordings</span>
        <div class="grow"><${TextField} mono showEnd value=${choices.folder} onChange=${(folder) => folder.trim() && set({ folder: folder.trim() })} /></div>
        <button class="btn" onClick=${pickFolder}>Choose…</button>
      </div>
      <div class="ob-form-row">
        <span class="ob-label">Name</span>
        <div class="grow"><${TextField} value=${choices.name} onChange=${(name) => set({ name })} /></div>
      </div>
    </div>
  `;
}
