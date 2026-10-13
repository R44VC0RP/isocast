// A source's Filters, as in OBS: the filter lists on the left (Audio
// Filters and Effect Filters, each with + / − / ⌃ ⌄ and an eye per
// filter), and on the right a preview of the source and the selected
// filter's settings. Changes show at once; Close keeps them.

import { useState } from "preact/hooks";
import { html } from "../lib/html.js";
import { useStore } from "../lib/store.js";
import { findSource, hasAudio, hasVideo, kindOf, sourceName } from "../lib/model.js";
import { replaceList } from "../lib/patch.js";
import { humanize, kindFields, kindsOf } from "../lib/schema.js";
import { edit } from "../lib/session.js";
import { Dialog, closeDialog, registerDialog } from "./dialog.js";
import { SettingsForm } from "./fields.js";
import { Icon } from "./icons.js";
import { showMenu } from "./popover.js";
import { SourcePreview, useSchema } from "./properties.js";

const fkey = (f) => f.id ?? f.kind;
const identity = (f) => (f.id != null ? { id: f.id } : { kind: f.kind });
const filterName = (f) => (f.id ? humanize(f.id.replace(/-/g, "_")) : humanize(f.kind));

/** One list of filters (`list`: "audio_filters" or "filters"). */
function FilterList({ source, list, def, title, selected, onSelect }) {
  const filters = source[list] ?? [];
  const patch = (items) => edit({ sources: [{ id: source.id, [list]: items }] });
  const index = filters.findIndex((f) => selected?.list === list && fkey(f) === selected.key);
  const add = (event) => {
    showMenu(
      event.currentTarget,
      kindsOf(def).map((k) => ({
        label: humanize(k.kind),
        run: () => {
          const taken = new Set(filters.map(fkey));
          const filter = { kind: k.kind };
          if (taken.has(k.kind)) {
            // Ids are lowercase letters, digits and dashes: "noise-gate-2".
            const base = k.kind.replace(/_/g, "-");
            let n = 2;
            while (taken.has(`${base}-${n}`)) n++;
            filter.id = `${base}-${n}`;
          }
          patch([filter]);
          onSelect({ list, key: fkey(filter) });
        },
      })),
    );
  };
  const remove = () => {
    if (index < 0) return;
    patch([{ ...identity(filters[index]), remove: true }]);
    const next = filters[index + 1] ?? filters[index - 1];
    onSelect(next ? { list, key: fkey(next) } : null);
  };
  const move = (by) => {
    if (index < 0 || index + by < 0 || index + by >= filters.length) return;
    const next = filters.slice();
    const [f] = next.splice(index, 1);
    next.splice(index + by, 0, f);
    patch(replaceList(filters, next));
  };
  return html`<div class="filter-group">
    <div class="filter-group-title">${title}</div>
    <div class="dock-list filter-list">
      ${filters.length === 0 && html`<div class="list-empty">None. Click + to add one.</div>`}
      ${filters.map((filter) => {
        const k = fkey(filter);
        const on = filter.enabled !== false;
        return html`<div
          key=${k}
          class=${"row" + (selected?.list === list && selected.key === k ? " selected" : "") + (on ? "" : " hidden-item")}
          onClick=${() => onSelect({ list, key: k })}
        >
          <button class="icon-btn small" title=${on ? "Turn off" : "Turn on"} onClick=${(e) => {
            e.stopPropagation();
            patch([{ ...identity(filter), enabled: !on }]);
          }}>
            <${Icon} name=${on ? "eye" : "eyeOff"} size=${14} />
          </button>
          <span class="label">${filterName(filter)}</span>
        </div>`;
      })}
    </div>
    <div class="dock-toolbar">
      <button class="icon-btn small" title="Add a filter" onClick=${add}><${Icon} name="plus" /></button>
      <button class="icon-btn small" title="Remove the filter" disabled=${index < 0} onClick=${remove}><${Icon} name="minus" /></button>
      <span class="toolbar-gap" />
      <button class="icon-btn small" title="Move up" disabled=${index <= 0} onClick=${() => move(-1)}><${Icon} name="chevronUp" /></button>
      <button class="icon-btn small" title="Move down" disabled=${index < 0 || index === filters.length - 1} onClick=${() => move(1)}><${Icon} name="chevronDown" /></button>
    </div>
  </div>`;
}

function FiltersDialog({ source: id }) {
  useSchema();
  const source = useStore((s) => findSource(s.setup, id));
  const video = source && hasVideo(source);
  const audio = source && hasAudio(source);
  const [selected, setSelected] = useState(() => {
    const first = (video && source.filters?.[0] && { list: "filters", f: source.filters[0] }) || (audio && source.audio_filters?.[0] && { list: "audio_filters", f: source.audio_filters[0] });
    return first ? { list: first.list, key: fkey(first.f) } : null;
  });
  if (!source) return null;
  const filter = selected ? (source[selected.list] ?? []).find((f) => fkey(f) === selected.key) : null;
  const def = selected?.list === "audio_filters" ? "AudioFilter" : "Filter";
  const change = (name, value, isLive) =>
    edit(
      { sources: [{ id: source.id, [selected.list]: [{ ...identity(filter), [name]: value }] }] },
      isLive ? { key: `filter:${source.id}:${selected.key}:${name}`, interval: 80 } : undefined,
    );
  // Several settings in one change (a shape replacing a size), so the
  // engine never sees half of it.
  const patch = (fields) => edit({ sources: [{ id: source.id, [selected.list]: [{ ...identity(filter), ...fields }] }] });
  const defaults = () => {
    if (!filter) return;
    const reset = Object.fromEntries(Object.keys(filter).filter((k) => !["id", "kind", "enabled"].includes(k)).map((k) => [k, null]));
    if (Object.keys(reset).length) edit({ sources: [{ id: source.id, [selected.list]: [{ ...identity(filter), ...reset }] }] });
  };
  return html`<${Dialog}
    title=${`Filters for “${sourceName(source) || kindOf(source).label}”`}
    width=${880}
    height=${Math.min(640, innerHeight - 60)}
    class="filters-dialog"
    onCancel=${closeDialog}
    onAccept=${closeDialog}
    extra=${html`<button class="btn" disabled=${!filter} onClick=${defaults}>Defaults</button>`}
    buttons=${html`<button class="btn primary" onClick=${closeDialog}>Close</button>`}
  >
    <div class="filters-layout">
      <div class="filters-side">
        ${audio && html`<${FilterList} source=${source} list="audio_filters" def="AudioFilter" title="Audio Filters" selected=${selected} onSelect=${setSelected} />`}
        ${video && html`<${FilterList} source=${source} list="filters" def="Filter" title="Effect Filters" selected=${selected} onSelect=${setSelected} />`}
      </div>
      <div class="filters-main">
        ${video && html`<${SourcePreview} source=${source} height=${220} />`}
        <div class="form filters-form">
          ${filter
            ? html`<div class="dialog-heading">${filterName(filter)}${filter.enabled === false ? html`<span class="faint"> · off</span>` : ""}</div>
                <${SettingsForm} fields=${kindFields(def, filter.kind)} value=${filter} kind="filter" owner=${id} skip=${["id", "enabled"]} onChange=${change} onPatch=${patch} />`
            : html`<div class="faint">Select a filter to change its settings, or add one with +.</div>`}
        </div>
      </div>
    </div>
  <//>`;
}

registerDialog("filters", FiltersDialog);
