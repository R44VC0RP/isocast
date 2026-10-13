// The Scene Transitions dock: the transition scene switches use (a
// dropdown of every kind), its properties (gear) and its Duration. The
// setup holds one transition, so there is no list to add to; scenes can
// override it (Scenes → right-click → Transition Override).

import { useEffect, useRef, useState } from "preact/hooks";
import { html } from "../lib/html.js";
import { getState, useStore } from "../lib/store.js";
import { objectPatch } from "../lib/patch.js";
import { kindFields, loadSchema, schemaReady } from "../lib/schema.js";
import { edit } from "../lib/session.js";
import { NumberField, Select } from "./controls.js";
import { Dialog, closeDialog, openDialog, registerDialog } from "./dialog.js";
import { Dock } from "./docks.js";
import { SettingsForm } from "./fields.js";
import { Icon } from "./icons.js";

export const TRANSITIONS = [
  ["cut", "Cut"],
  ["fade", "Fade"],
  ["swipe", "Swipe"],
  ["slide", "Slide"],
  ["fade_to_color", "Fade to Colour"],
  ["luma_wipe", "Luma Wipe"],
  ["stinger", "Stinger"],
  ["move", "Move"],
];

const label = (kind) => TRANSITIONS.find(([k]) => k === kind)?.[1] ?? kind;

/** A transition of `kind`, keeping the duration; kinds that need a file
 * (a stinger) open their properties first. */
function choose(setup, kind) {
  const transition = setup.transition ?? { kind: "cut" };
  if (kind === (transition.kind ?? "cut")) return;
  if (kind === "stinger") {
    openDialog("transition", { kind });
    return;
  }
  edit({ transition: objectPatch(transition, { kind, duration_ms: transition.duration_ms ?? 300 }) });
}

export function TransitionsDock() {
  const setup = useStore((s) => s.setup);
  if (!setup) return html`<${Dock} title="Scene Transitions" />`;
  const transition = setup.transition ?? { kind: "cut" };
  const kind = transition.kind ?? "cut";
  const options = TRANSITIONS.some(([k]) => k === kind) ? TRANSITIONS : [...TRANSITIONS, [kind, kind]];
  const hasSettings = !["cut", "fade"].includes(kind);
  const fixedLength = kind === "cut" || kind === "stinger";
  return html`<${Dock} title="Scene Transitions">
    <div class="dock-form">
      <div class="dock-form-row">
        <div class="grow"><${Select} value=${kind} options=${options} onChange=${(k) => choose(setup, k)} /></div>
        <button class="icon-btn small" title="Transition properties" disabled=${!hasSettings} onClick=${() => openDialog("transition")}>
          <${Icon} name="settings" />
        </button>
      </div>
      <div class="dock-form-row">
        <span class=${"dock-form-label" + (fixedLength ? " disabled" : "")}>Duration</span>
        <div class="grow">
          <${NumberField}
            value=${transition.duration_ms ?? 300}
            min=${0}
            max=${10000}
            step=${50}
            unit="ms"
            disabled=${fixedLength}
            title=${kind === "stinger" ? "A stinger lasts as long as its video" : "How long the transition takes"}
            onChange=${(ms) => edit({ transition: { duration_ms: ms } }, { key: "transition-ms" })}
          />
        </div>
      </div>
    </div>
  <//>`;
}

/** Transition properties: the settings of the setup's transition (or of
 * `kind`, to switch to one that needs settings first). Changes show at
 * once; Cancel puts them back. */
function TransitionDialog({ kind: wanted }) {
  const setup = useStore((s) => s.setup);
  const [, setReady] = useState(schemaReady());
  useEffect(() => {
    if (!schemaReady()) loadSchema().then(() => setReady(true));
  }, []);
  const original = useRef(getState().setup?.transition ?? { kind: "cut" });
  const [draft, setDraft] = useState(() =>
    wanted && wanted !== original.current.kind ? { kind: wanted, duration_ms: original.current.duration_ms ?? 300 } : null,
  );
  if (!setup) return null;
  // Switching to a kind that can't be applied without settings (a stinger
  // needs its file): edit a draft, applied on OK.
  const current = draft ?? setup.transition ?? { kind: "cut" };
  const fields = kindFields("Transition", current.kind);
  const change = (name, value, live) => {
    if (draft) setDraft({ ...draft, [name]: value });
    else edit({ transition: { [name]: value } }, live ? { key: `transition:${name}`, interval: 80 } : undefined);
  };
  const cancel = () => {
    if (!draft) {
      const patch = objectPatch(getState().setup.transition ?? {}, original.current);
      if (Object.keys(patch).length) edit({ transition: patch });
    }
    closeDialog();
  };
  const accept = async () => {
    if (draft && !(await edit({ transition: objectPatch(getState().setup.transition ?? {}, draft) }))) return;
    closeDialog();
  };
  const defaults = () => {
    const reset = Object.fromEntries((fields ?? []).filter((f) => f.optional && f.name !== "duration_ms").map((f) => [f.name, null]));
    if (draft) setDraft({ kind: draft.kind, duration_ms: draft.duration_ms });
    else if (Object.keys(reset).length) edit({ transition: reset });
  };
  // Nothing to set (Fade, Cut): just Close, as in Filters.
  const empty = !draft && fields?.length === 0;
  return html`<${Dialog}
    title=${`Properties for “${label(current.kind)}”`}
    width=${520}
    onCancel=${empty ? closeDialog : cancel}
    onAccept=${empty ? closeDialog : accept}
    extra=${fields?.length > 0 && html`<button class="btn" onClick=${defaults}>Defaults</button>`}
    buttons=${empty
      ? html`<button class="btn primary" onClick=${closeDialog}>Close</button>`
      : html`<button class="btn" onClick=${cancel}>Cancel</button><button class="btn primary" onClick=${accept}>OK</button>`}
  >
    <div class="form">
      <${SettingsForm} fields=${fields} value=${current} kind="transition" onChange=${change} />
    </div>
  <//>`;
}

registerDialog("transition", TransitionDialog);
