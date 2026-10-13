// A source's Properties, as in OBS: a live preview of the source on top,
// its settings below (drawn from the setup schema), Defaults at the
// bottom left and OK / Cancel at the bottom right. Changes show at once;
// Cancel puts the source back as it was. A source that can't exist yet
// (a new image without its file) is edited as a draft and added on OK.
// Also the Rename dialog the mixer uses.

import { useEffect, useRef, useState } from "preact/hooks";
import { html } from "../lib/html.js";
import { getState, useStore } from "../lib/store.js";
import { KINDS, findSource, hasVideo, kindOf, sourceName } from "../lib/model.js";
import { objectPatch } from "../lib/patch.js";
import { kindFields, loadSchema, schemaReady } from "../lib/schema.js";
import { knownSize } from "../lib/sizes.js";
import { edit } from "../lib/session.js";
import { Dialog, closeDialog, registerDialog } from "./dialog.js";
import { SettingsForm } from "./fields.js";
import { Icon } from "./icons.js";
import { Monitor } from "./monitor.js";
import { addSource, missing } from "./add-source.js";

/** Re-render once the schema has loaded. */
export function useSchema() {
  const [, setReady] = useState(schemaReady());
  useEffect(() => {
    if (!schemaReady()) loadSchema().then(() => setReady(true));
  }, []);
}

/** The fields of a source that are its settings (not its name, audio
 * or filters). */
const OTHER = new Set(["id", "name", "kind", "audio", "filters", "audio_filters"]);
export const settingsOf = (source) => Object.fromEntries(Object.entries(source ?? {}).filter(([k]) => !OTHER.has(k)));

/** A live picture of a source, letterboxed in a box of its own. */
export function SourcePreview({ source, height = 260, note }) {
  const size = useStore((s) => (source ? knownSize(s.setup, source) : null));
  const failed = useStore((s) => s.running?.[source?.id]?.state === "failed");
  if (failed && !note) note = "No picture: the source isn’t running.";
  const canvas = useStore((s) => s.setup?.canvas);
  const aspect = size ? size.width / size.height : canvas ? canvas.width / canvas.height : 16 / 9;
  return html`<div class="source-preview" style=${{ height: `${height}px`, "--aspect": aspect }}>
    ${note
      ? html`<div class="source-preview-note">${note}</div>`
      : html`<${Monitor} target=${source.id} empty=${null} />`}
  </div>`;
}

/** Why the engine couldn't start a source, if it couldn't. */
function SourceProblem({ id }) {
  const status = useStore((s) => (id ? s.running?.[id] : null));
  if (status?.state !== "failed") return null;
  return html`<div class="callout">
    <${Icon} name="warning" size=${14} />
    <div><div class="title">This source isn’t running</div><div class="message">${status.error ?? "It couldn’t start."}</div></div>
  </div>`;
}

function PropertiesDialog({ source: id, pending, created }) {
  useSchema();
  const live = useStore((s) => (id ? findSource(s.setup, id) : null));
  const original = useRef(live ? structuredClone(live) : null);
  const [draft, setDraft] = useState(() => (pending ? { id: "new", kind: pending.kind, name: pending.name, ...pending.settings } : null));
  const source = draft ?? live;
  // The source went away (removed elsewhere): nothing to edit.
  useEffect(() => {
    if (!source) closeDialog();
  }, [!source]);
  if (!source) return null;
  const fields = kindFields("Source", source.kind);
  const change = (name, value, isLive) => {
    if (draft) setDraft((d) => ({ ...d, [name]: value }));
    else edit({ sources: [{ id: source.id, [name]: value }] }, isLive ? { key: `src:${source.id}:${name}`, interval: 80 } : undefined);
  };
  // Several settings in one change (a camera and its format), so the
  // engine never sees half of it.
  const patch = (fields) => {
    if (draft) setDraft((d) => Object.fromEntries(Object.entries({ ...d, ...fields }).filter(([, v]) => v != null)));
    else edit({ sources: [{ id: source.id, ...fields }] });
  };
  const cancel = () => {
    if (!draft && original.current) {
      const now = findSource(getState().setup, id);
      const patch = objectPatch(settingsOf(now), settingsOf(original.current));
      if (Object.keys(patch).length) edit({ sources: [{ id, ...patch }] });
    }
    closeDialog();
  };
  const need = draft ? missing(draft.kind, settingsOf(draft)) : null;
  const accept = async () => {
    if (draft) {
      if (need) return;
      const { id: _id, kind, name, ...settings } = draft;
      closeDialog();
      await addSource({ sceneId: pending.sceneId, kind, settings, name, visible: pending.visible });
      return;
    }
    closeDialog();
  };
  // Defaults: every setting that has one goes back to it; the ones a
  // source can't do without (its device, its file) stay.
  const defaults = () => {
    const optional = (fields ?? []).filter((f) => f.optional).map((f) => f.name);
    if (draft) {
      setDraft((d) => Object.fromEntries(Object.entries(d).filter(([k]) => !optional.includes(k))));
      return;
    }
    const patch = Object.fromEntries(optional.filter((name) => source[name] !== undefined).map((name) => [name, null]));
    if (Object.keys(patch).length) edit({ sources: [{ id: source.id, ...patch }] });
  };
  const video = hasVideo(source) || !!KINDS[source.kind]?.video;
  return html`<${Dialog}
    title=${`Properties for “${sourceName(source) || kindOf(source).label}”`}
    width=${720}
    class="properties-dialog"
    onCancel=${cancel}
    onAccept=${accept}
    extra=${html`<button class="btn" onClick=${defaults}>Defaults</button>
      ${need && html`<span class="dialog-note">${need}</span>`}`}
    buttons=${html`<button class="btn" onClick=${cancel}>Cancel</button>
      <button class="btn primary" disabled=${!!need} onClick=${accept}>OK</button>`}
  >
    ${video &&
    html`<${SourcePreview}
      source=${source}
      note=${draft ? "The preview shows once the source is added." : null}
    />`}
    <div class="form properties-form">
      <${SourceProblem} id=${draft ? null : id} />
      <${SettingsForm} fields=${fields} value=${source} kind=${source.kind} onChange=${change} onPatch=${patch} />
    </div>
    ${created && html`<div class="form-note">Added to the scene. Cancel keeps it with these settings; remove it with − under Sources.</div>`}
  <//>`;
}

registerDialog("properties", PropertiesDialog);

/** Rename a source (the mixer's Rename). */
function RenameDialog({ source: id }) {
  const source = useStore((s) => findSource(s.setup, id));
  const [name, setName] = useState(() => sourceName(source));
  if (!source) return null;
  const accept = () => {
    const next = name.trim();
    if (next && next !== sourceName(source)) edit({ sources: [{ id, name: next }] });
    closeDialog();
  };
  return html`<${Dialog}
    title="Rename"
    width=${380}
    onCancel=${closeDialog}
    onAccept=${accept}
    buttons=${html`<button class="btn" onClick=${closeDialog}>Cancel</button><button class="btn primary" disabled=${!name.trim()} onClick=${accept}>OK</button>`}
  >
    <div class="form">
      <div class="dialog-heading">Name for “${sourceName(source)}”</div>
      <div class="field">
        <input value=${name} spellcheck="false" ref=${(el) => el && !el.dataset.f && ((el.dataset.f = "1"), el.focus(), el.select())}
          onInput=${(e) => setName(e.currentTarget.value)} />
      </div>
    </div>
  <//>`;
}

registerDialog("rename", RenameDialog);
