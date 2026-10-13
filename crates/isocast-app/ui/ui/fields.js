// Forms from the setup schema: one control per field of a source's or a
// filter's settings, with dropdowns where the choices are known (devices,
// apps, windows, a camera's formats: device-fields.js) and pickers for
// files.

import { html } from "../lib/html.js";
import { native } from "../lib/bridge.js";
import { humanize, startValue } from "../lib/schema.js";
import { ColorField, NumberField, Prop, Select, Switch, TextField } from "./controls.js";
import { CameraFormat, choiceControl } from "./device-fields.js";
import { Icon } from "./icons.js";

const IMAGE_TYPES = ["png", "jpg", "jpeg", "gif", "webp", "bmp", "tga", "svg"];
const MEDIA_TYPES = ["mov", "mp4", "m4v", "m4a", "mp3", "wav", "aif", "aiff", "caf", "mkv", "webm"];

/** Pick files with the system panel; their paths (none if cancelled). */
export async function chooseFiles({ types, multiple = false, folders = false, prompt = "Choose" } = {}) {
  try {
    return await native("open_file", { types, multiple, folders, prompt });
  } catch {
    return [];
  }
}

function fileTypes(kind, name) {
  if (kind === "image" || kind === "slideshow" || name === "image" || name === "path") return IMAGE_TYPES;
  if (kind === "media" || kind === "stinger") return MEDIA_TYPES;
  if (name === "file" && kind === "text") return ["txt", "md"];
  if (name === "file" && kind === "browser") return ["html", "htm"];
  return undefined;
}

function PathField({ value, kind, name, onChange }) {
  const pick = async () => {
    const [path] = await chooseFiles({ types: fileTypes(kind, name) });
    if (path) onChange(path);
  };
  return html`<div class="grow"><${TextField} value=${value ?? ""} placeholder="A file path" showEnd onChange=${(v) => onChange(v || null)} /></div>
    <button class="icon-btn small" title="Choose a file…" onClick=${pick}><${Icon} name="folderOpen" /></button>`;
}

function FilesField({ value = [], kind, onChange }) {
  const add = async () => {
    const paths = await chooseFiles({ types: fileTypes(kind, "files"), multiple: true, prompt: "Add" });
    if (paths.length) onChange([...value, ...paths]);
  };
  return html`<div class="file-field">
    ${value.length > 0 &&
    html`<div class="file-list">
      ${value.map(
        (path, i) => html`<div class="file-list-row" key=${i} title=${path}>
          <span class="grow ellipsis">${path.split("/").pop()}</span>
          <button class="icon-btn small" title="Remove" onClick=${() => onChange(value.filter((_, j) => j !== i))}>
            <${Icon} name="x" size=${14} />
          </button>
        </div>`,
      )}
    </div>`}
    <button class="btn small" onClick=${add}><${Icon} name="plus" />Add files</button>
  </div>`;
}

/** The control for one field. `all` is every setting; `onPatch` sets
 * several at once. A setting left out shows its default, which is what
 * the engine uses. */
function Control({ f, value, kind, all, owner, onChange, onPatch }) {
  const set = (v) => onChange(f.name, v);
  const choice = choiceControl({ name: f.name, kind, value: value ?? f.default, all, owner, onPatch });
  if (choice) return choice;
  switch (f.type) {
    case "boolean":
      return html`<${Switch} on=${!!(value ?? f.default)} onChange=${set} />`;
    case "color":
      return html`<${ColorField} value=${value} onChange=${set} />`;
    case "enum":
      return html`<${Select}
        value=${value ?? f.default ?? f.options[0].value}
        options=${f.options.map((o) => [o.value, humanize(String(o.value))])}
        onChange=${set}
      />`;
    case "number":
      return html`<${NumberField}
        value=${value ?? f.default}
        min=${f.min}
        max=${f.max}
        step=${f.integer ? 1 : Math.abs(f.max - f.min) <= 10 ? 0.01 : 0.1}
        unit=${f.unit}
        placeholder=${f.optional && value == null ? "Auto" : ""}
        onChange=${set}
        onInput=${(v) => onChange(f.name, v, true)}
      />`;
    case "strings":
      return html`<${FilesField} value=${value} kind=${kind} onChange=${set} />`;
    case "string":
      if (PATH_FIELDS.includes(f.name)) {
        return html`<${PathField} value=${value} kind=${kind} name=${f.name} onChange=${set} />`;
      }
      return html`<div class="grow"><${TextField}
        value=${value ?? ""}
        multiline=${f.name === "text" || f.name === "css"}
        mono=${f.name === "css"}
        placeholder=${f.optional ? "None" : ""}
        onChange=${(v) => set(v === "" && f.optional ? null : v)}
      /></div>`;
    default:
      return html`<span class="faint mono">${JSON.stringify(value)}</span>`;
  }
}

// A camera's width, height and fps are one Resolution and one Frame rate
// dropdown, drawn after its device.
const CAMERA_FORMAT = ["width", "height", "fps"];

const PATH_FIELDS = ["path", "file", "image", "matte"];
/** Whether a field's row ends in a button (choose a file, remove). */
const trailing = (f, current) =>
  (f.type === "string" && PATH_FIELDS.includes(f.name)) || (f.optional && current != null && (f.type === "color" || f.type === "enum"));
const mayTrail = (f) => (f.type === "string" && PATH_FIELDS.includes(f.name)) || (f.optional && (f.type === "color" || f.type === "enum"));

/** Every field of `fields` for settings `value`. `onChange(name, value,
 * live)` sets one (null removes an optional one; `live` while dragging);
 * `onPatch(fields)` sets several at once (by default one at a time).
 * `owner` is the source a filter's settings belong to. */
export function SettingsForm({ fields, value, kind, owner, onChange, onPatch, skip = [] }) {
  if (!fields) return html`<div class="faint">Loading…</div>`;
  const patch = onPatch ?? ((changes) => Object.entries(changes).forEach(([name, v]) => onChange(name, v)));
  const camera = kind === "camera";
  const shown = fields.filter((f) => !skip.includes(f.name) && !(camera && CAMERA_FORMAT.includes(f.name)));
  if (!shown.length) return html`<div class="faint" style="font-size: var(--fs-sm)">No settings.</div>`;
  // When any row ends in a button, every row keeps that lane, so the
  // fields share one right edge.
  const lane = shown.some(mayTrail);
  return shown.map((f) => {
    const current = value?.[f.name];
    if (camera && f.name === "device") {
      return html`<div key=${f.name} class="form-group">
        <${Prop} label=${f.label} title=${f.description}>
          <${Control} f=${f} value=${current} kind=${kind} all=${value} onChange=${onChange} onPatch=${patch} />
        <//>
        <${CameraFormat} id=${value?.id !== "new" ? value?.id : null} all=${value} onPatch=${patch} />
      </div>`;
    }
    if (f.type === "object") {
      const on = current != null;
      return html`<div key=${f.name}>
        <${Prop} label=${f.label} title=${f.description}>
          <${Switch} on=${on} onChange=${(v) => onChange(f.name, v ? startValue(f) : null)} />
        <//>
        ${on &&
        html`<div class="sub-form">
          <${SettingsForm}
            fields=${f.fields}
            value=${current}
            kind=${kind}
            onChange=${(name, v, live) => onChange(f.name, { ...current, [name]: v }, live)}
            onPatch=${(changes) => onChange(f.name, { ...current, ...changes })}
          />
        </div>`}
      </div>`;
    }
    const optionalOff = f.optional && current == null && (f.type === "color" || f.type === "enum");
    return html`<${Prop} key=${f.name} label=${f.label} title=${f.description}>
      ${optionalOff
        ? html`<button class="btn ghost small" onClick=${() => onChange(f.name, startValue(f))}><${Icon} name="plus" />Add</button>`
        : html`<${Control} f=${f} value=${current} kind=${kind} all=${value} owner=${owner} onChange=${onChange} onPatch=${patch} />
            ${f.optional && current != null && (f.type === "color" || f.type === "enum") &&
            html`<button class="icon-btn small" title="Remove" onClick=${() => onChange(f.name, null)}><${Icon} name="x" size=${14} /></button>`}`}
      ${lane && !trailing(f, current) && html`<span class="refresh-gap" />`}
    <//>`;
  });
}
