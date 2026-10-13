// The source toolbar under the preview, as in OBS: the selected source's
// type and name, its Properties and Filters, and the one or two settings
// people change most for its type (a display, a device, the text, the
// file). With nothing selected it says so.

import { html } from "../lib/html.js";
import { call } from "../lib/bridge.js";
import { getState, useStore } from "../lib/store.js";
import { findSource, itemIcon, itemLabel, kindOf } from "../lib/model.js";
import { edit, failed, loadDevices, toast } from "../lib/session.js";
import { selected } from "../lib/items.js";
import { appOptions, cameraChange, cameraOptions, displayOptions, microphoneOptions } from "../lib/devices.js";
import { ColorField, Select, TextField } from "./controls.js";
import { chooseFiles } from "./fields.js";
import { Icon } from "./icons.js";
import { openDialog } from "./dialog.js";

const set = (source, fields) => edit({ sources: [{ id: source.id, ...fields }] });

function DeviceSelect({ source, field, options, onChange }) {
  const value = source[field] ?? "";
  return html`<${Select} value=${value} options=${options.length ? options : [[value, value]]}
    onChange=${onChange ?? ((v) => set(source, { [field]: v }))} />`;
}


/** The quick controls for a source's type. */
function Quick({ source }) {
  const devices = useStore((s) => s.devices);
  const needsDevices = ["display", "camera", "microphone", "window", "application", "application_audio"].includes(source.kind);
  if (needsDevices && !devices) loadDevices();
  switch (source.kind) {
    case "display":
      return html`<span class="tb-label">Display</span>
        <div class="tb-field wide">
          <${DeviceSelect} source=${source} field="display" options=${displayOptions(devices, source.display)} />
        </div>`;
    case "camera":
      return html`<span class="tb-label">Device</span>
        <div class="tb-field wide"><${DeviceSelect} source=${source} field="device" options=${cameraOptions(devices, source.device)}
          onChange=${(device) => set(source, cameraChange(devices, source, device))} /></div>`;
    case "microphone":
      return html`<span class="tb-label">Device</span>
        <div class="tb-field wide">
          <${DeviceSelect} source=${source} field="device" options=${microphoneOptions(devices, source.device)} />
        </div>`;
    case "window":
    case "application":
    case "application_audio":
      return html`<span class="tb-label">Application</span>
        <div class="tb-field wide"><${DeviceSelect} source=${source} field="app" options=${appOptions(devices, source.app)}
          onChange=${(app) => set(source, source.kind === "window" && app !== source.app ? { app, title: null } : { app })} /></div>`;
    case "text":
      return source.file
        ? html`<span class="tb-label">From</span><span class="tb-path ellipsis" title=${source.file}>${source.file.split("/").pop()}</span>`
        : html`<span class="tb-label">Text</span>
            <div class="tb-field grow"><${TextField} value=${source.text ?? ""} onChange=${(text) => set(source, { text })} /></div>`;
    case "image":
      return html`<span class="tb-label">Image File</span>
        <span class="tb-path ellipsis" title=${source.path}>${source.path?.split("/").pop() || "None"}</span>
        <button class="btn small" onClick=${async () => {
          const [path] = await chooseFiles({ types: ["png", "jpg", "jpeg", "gif", "webp", "bmp", "tga", "svg"] });
          if (path) set(source, { path });
        }}>Browse</button>`;
    case "color":
      return html`<span class="tb-label">Colour</span>
        <div class="tb-field"><${ColorField} value=${source.color} onChange=${(color) => set(source, { color })} /></div>`;
    case "media":
    case "slideshow": {
      const files = source.files ?? [];
      return html`<span class="tb-label">${source.kind === "media" ? "Media" : "Images"}</span>
        <span class="tb-path ellipsis" title=${files.join("\n")}>
          ${files.length ? `${files[0].split("/").pop()}${files.length > 1 ? ` and ${files.length - 1} more` : ""}` : "None"}
        </span>`;
    }
    case "browser":
      return html`<span class="tb-label">URL</span>
        <span class="tb-path ellipsis" title=${source.url ?? source.file ?? ""}>${source.url ?? source.file ?? ""}</span>
        <button class="btn small" title="Reload the page, bypassing the cache" onClick=${async () => {
          try {
            await call("refresh_browser", { source: source.id });
            toast("info", "Reloading the page");
          } catch (error) {
            failed("Couldn’t reload the page", error);
          }
        }}>Refresh</button>`;
    default:
      return null;
  }
}

export function SourceToolbar() {
  const setup = useStore((s) => s.setup);
  const selection = useStore((s) => s.selection);
  const found = selection ? selected(getState()) : null;
  const item = found?.item;
  const source = item?.source ? findSource(setup, item.source) : null;
  if (!item) {
    return html`<div class="source-toolbar empty">
      <span class="faint">No source selected</span>
    </div>`;
  }
  return html`<div class="source-toolbar">
    <span class="tb-source" title=${source ? kindOf(source).label : ""}>
      <${Icon} name=${itemIcon(setup, item)} />
      <span class="ellipsis">${itemLabel(setup, item)}</span>
    </span>
    <button class="btn small" disabled=${!source} onClick=${() => openDialog("properties", { source: source.id })}>
      <${Icon} name="settings" />Properties
    </button>
    <button class="btn small" disabled=${!source} onClick=${() => openDialog("filters", { source: source.id })}>
      <${Icon} name="filters" />Filters
    </button>
    ${source && html`<span class="tb-sep" /><${Quick} source=${source} />`}
  </div>`;
}
