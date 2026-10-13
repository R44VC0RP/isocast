// Whole-window states: reaching the engine, and the first run without a
// setup (a dialog over the empty main window, as OBS opens its
// Auto-Configuration Wizard). Also the toasts.

import { useState } from "preact/hooks";
import { html } from "../lib/html.js";
import { call, native } from "../lib/bridge.js";
import { getState, useStore } from "../lib/store.js";
import { dismiss, failed, refreshStatus, reload, replaceSetup, toast } from "../lib/session.js";
import { chooseFiles } from "./fields.js";
import { BrandMark, Icon } from "./icons.js";
import { startOnboarding } from "./onboarding.js";

export function Connecting() {
  const connection = useStore((s) => s.connection);
  const failedState = connection.state === "failed";
  return html`<div class="state">
    <div class="state-card">
      <${BrandMark} size=${56} />
      ${failedState
        ? html`<h1>Isocast can’t reach its engine</h1>
            <p>The app talks to isocastd, which does the capturing and recording. It didn’t start or answer.</p>
            ${connection.error && html`<div class="detail">${connection.error}</div>`}
            <div class="state-actions">
              <button class="btn primary large" onClick=${() => native("reconnect")}>Try again</button>
            </div>
            <p class="note">It keeps trying on its own, too.</p>`
        : html`<h1>${connection.state === "disconnected" ? "Reconnecting to the engine…" : "Starting the engine…"}</h1>
            <div class="spinner" style="margin-top: 8px" />`}
    </div>
  </div>`;
}

/** Make `setup` the live setup: through the undoable edit queue when one
 * is live already, else as the first. */
async function useSetup(setup, notes = []) {
  if (getState().setup) {
    if (!(await replaceSetup(setup))) return;
  } else {
    try {
      await call("apply", { revision: getState().revision, setup });
      await reload();
    } catch (error) {
      return failed("Couldn’t use that setup", error);
    }
  }
  await refreshStatus();
  if (notes.length) toast("warning", "Imported, with notes", notes.join("\n"));
}

/** Ask for an OBS scene collection and make it the live setup. */
export async function importObsCollection() {
  const [path] = await chooseFiles({ types: ["json"], prompt: "Import" });
  if (!path) return;
  try {
    const imported = await call("import_obs", { collection: path });
    await useSetup(imported.setup, imported.notes ?? []);
  } catch (error) {
    failed("Couldn’t import the scene collection", error);
  }
}

/** Ask for a setup file (TOML or JSON) and make it the live setup. */
export async function openSetupFile() {
  const [path] = await chooseFiles({ types: ["toml", "json"], prompt: "Open" });
  if (!path) return;
  try {
    await useSetup(await native("read_setup_file", { path }));
  } catch (error) {
    failed("Couldn’t read the setup file", error);
  }
}

export function Welcome() {
  const [busy, setBusy] = useState(null);
  const run = (name, work) => async () => {
    setBusy(name);
    try {
      await work();
    } finally {
      setBusy(null);
    }
  };
  const importObs = run("obs", importObsCollection);
  const open = run("file", openSetupFile);
  const choice = (name, icon, title, hint, action, primary) => html`<button class=${"choice" + (primary ? " primary" : "")} disabled=${!!busy} onClick=${action}>
    <span class="glyph">${busy === name ? html`<div class="spinner" />` : html`<${Icon} name=${icon} size=${20} />`}</span>
    <span class="name">${title}</span>
    <span class="hint">${hint}</span>
  </button>`;
  return html`<div class="dialog-scrim welcome">
    <div class="state-card welcome-card">
      <${BrandMark} size=${56} />
      <h1>Set up your first scene</h1>
      <p>Isocast records the program and every source to its own file, all on one clock. Start from this Mac, or bring what you have.</p>
      <div class="choice-grid">
        ${choice("start", "sparkles", "Start with this Mac", "Pick your screen, camera and mic; get three scenes, ready to record.", startOnboarding, true)}
        ${choice("obs", "fileUp", "Import from OBS", "A scene collection (.json) from OBS Studio.", importObs)}
        ${choice("file", "folderOpen", "Open a setup", "An Isocast setup file (.toml or .json).", open)}
      </div>
    </div>
  </div>`;
}

export function Toasts() {
  const all = useStore((s) => s.toasts);
  // The details popovers under Record and Go live show the drop counts
  // themselves, where the toast would cover them.
  const detailsOpen = useStore((s) => !!(s.details || s.streamDetails));
  const toasts = detailsOpen ? all.filter((t) => t.key !== "drops") : all;
  const icon = { error: "alert", warning: "warning", success: "check", info: "info" };
  return html`<div class="toasts">
    ${toasts.map(
      (t) => html`<div key=${t.id} class=${"toast " + t.kind} role=${t.kind === "error" ? "alert" : "status"}>
        <${Icon} name=${icon[t.kind] ?? "info"} />
        <div class="body">
          <div class="title">${t.title}</div>
          ${t.message && html`<div class="message">${t.message}</div>`}
          ${t.rows?.length > 0 &&
          html`<div class="toast-rows tabular">
            ${t.rows.map(([label, value, cls]) => html`<span class="ellipsis">${label}</span><span class=${cls ?? ""}>${value}</span>`)}
          </div>`}
        </div>
        <div class="actions">
          ${t.action &&
          html`<button class="btn ghost small" onClick=${() => {
            dismiss(t.id);
            t.action.run();
          }}>${t.action.label}</button>`}
          <button class="icon-btn small" title="Dismiss" onClick=${() => dismiss(t.id)}><${Icon} name="x" size=${14} /></button>
        </div>
      </div>`,
    )}
  </div>`;
}
