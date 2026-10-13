// The Profile and Scene Collection menus, as OBS has them: New, Duplicate,
// Rename and Remove act on the one in use, Import adds one from a file,
// Export writes the one in use to a file, and below them every profile or
// scene collection in the library, the one in use checked; choosing one
// switches to it. The native menu bar shows them (`menu_items`); each
// command is a call on the method table (library_create, library_rename,
// library_switch, library_delete, library_import, library_import_obs,
// library_get), and what the engine refuses (a switch while recording)
// shows in its own words.

import { useEffect, useState } from "preact/hooks";
import { html } from "../lib/html.js";
import { call, native } from "../lib/bridge.js";
import { getState, useStore } from "../lib/store.js";
import { failed, onMenu, toast } from "../lib/session.js";
import { Dialog, closeDialog, openDialog, registerDialog } from "./dialog.js";
import { chooseFiles } from "./fields.js";
import { showMenu } from "./popover.js";

const KINDS = {
  collection: {
    label: "scene collection",
    plural: "scene collections",
    title: "Scene Collection",
    list: "collections",
    // Isocast's exports and whole setups, or OBS's scene collections.
    types: ["toml", "json"],
  },
  profile: {
    label: "profile",
    plural: "profiles",
    title: "Profile",
    list: "profiles",
    // …or an OBS profile's basic.ini.
    types: ["toml", "json", "ini"],
  },
};

/** The library's scene collections or profiles, sorted by name. */
export const entries = (library, kind) => library?.[KINDS[kind].list] ?? [];

/** The one in use, if any. */
export const current = (library, kind) => entries(library, kind).find((e) => e.id === library?.[kind]) ?? null;

const label = (entry) => entry?.name || entry?.id || "";

/** What a menu's list shows: the name, and the id too when another has the
 * same name (the engine allows that; ids tell them apart). */
const menuLabel = (entry, all) =>
  all.some((other) => other.id !== entry.id && label(other) === label(entry)) ? `${label(entry)} (${entry.id})` : label(entry);

/** `base`, or `base 2`, `base 3`… if it is taken. */
function unique(base, names) {
  const used = new Set(names);
  if (!used.has(base)) return base;
  for (let n = 2; ; n++) if (!used.has(`${base} ${n}`)) return `${base} ${n}`;
}

/** The menu's items: "-" or {title, action, id, checked, enabled}. */
export function menuItems(kind, library) {
  const list = entries(library, kind);
  const now = current(library, kind);
  const usable = list.filter((e) => !e.problem);
  return [
    { title: "New…", action: `${kind}_new`, enabled: !!library },
    { title: "Duplicate…", action: `${kind}_duplicate`, enabled: !!now },
    { title: "Rename…", action: `${kind}_rename`, enabled: !!now },
    { title: "Remove…", action: `${kind}_remove`, enabled: !!now && usable.some((e) => e.id !== now.id) },
    "-",
    { title: "Import…", action: `${kind}_import`, enabled: !!library },
    { title: "Export…", action: `${kind}_export`, enabled: !!now },
    ...(list.length ? ["-"] : []),
    ...list.map((e) => ({ title: menuLabel(e, list), action: `${kind}_switch`, id: e.id, checked: e.current, enabled: !e.problem })),
  ];
}

/** Keep the menu bar's Profile and Scene Collection menus in step with the
 * library. */
export function useLibraryMenus() {
  const library = useStore((s) => s.library);
  useEffect(() => {
    native("menu_items", { menu: "Profile", items: menuItems("profile", library) }).catch(() => {});
    native("menu_items", { menu: "Scene Collection", items: menuItems("collection", library) }).catch(() => {});
  }, [library]);
}

// ---------- Commands ----------

/** Use another scene collection or profile (by id). */
async function switchTo(kind, id) {
  if (!id || id === getState().library?.[kind]) return;
  closeDialog();
  try {
    await call("library_switch", { [kind]: id });
  } catch (error) {
    failed(`Couldn’t switch ${KINDS[kind].plural}`, error);
  }
}

/** New (blank) or Duplicate (a copy of the one in use), switched to. */
async function create(kind, name, copyOf) {
  const k = KINDS[kind];
  try {
    await call("library_create", { kind, name, ...(copyOf ? { copy_of: copyOf } : {}), switch: true });
  } catch (error) {
    failed(copyOf ? `Couldn’t duplicate the ${k.label}` : `Couldn’t add the ${k.label}`, error);
  }
}

async function rename(kind, id, name) {
  try {
    await call("library_rename", { kind, id, name });
  } catch (error) {
    failed(`Couldn’t rename the ${KINDS[kind].label}`, error);
  }
}

/** Remove the one in use, as OBS does: switch to `next`, then delete it
 * (the engine keeps its file in the library's deleted folder). */
async function remove(kind, entry, next) {
  try {
    await call("library_switch", { [kind]: next.id });
    await call("library_delete", { kind, id: entry.id });
  } catch (error) {
    failed(`Couldn’t remove the ${KINDS[kind].label}`, error);
  }
}

/** Ask for a file (unless given `chosen`, as snapshots do) and add what it
 * holds to the library: an exported scene collection or profile, a whole
 * setup (both parts), or OBS's. */
export async function importFile(kind, chosen = null) {
  const k = KINDS[kind];
  closeDialog();
  const path = chosen ?? (await chooseFiles({ types: k.types, prompt: "Import" }))[0];
  if (!path) return;
  let change;
  try {
    const { method, params } = await native("library_import_file", { kind, path });
    change = await call(method, params);
  } catch (error) {
    failed(`Couldn’t import the ${k.label}`, error);
    return;
  }
  const made = [change.collection, change.profile].filter(Boolean);
  // A whole setup brings both: say which is which.
  const names = made.map((e) => (made.length > 1 ? `${KINDS[e.kind].label} “${label(e)}”` : `“${label(e)}”`)).join(" and ");
  const lines = [];
  const skipped = change.unsupported ?? [];
  if (skipped.length) lines.push(`${skipped.length} thing${skipped.length === 1 ? "" : "s"} from OBS couldn’t come over: ${skipped.map((u) => u.obs).join(", ")}.`);
  for (const problem of change.problems ?? []) lines.push(`${problem.path}: ${problem.message}`);
  const pair = { collection: change.collection?.id, profile: change.profile?.id };
  toast(lines.length ? "warning" : "success", `Imported ${names}`, lines.join("\n"), {
    label: "Switch to it",
    run: async () => {
      try {
        await call("library_switch", pair);
      } catch (error) {
        failed(`Couldn’t switch to ${names}`, error);
      }
    },
  });
}

/** Ask where (unless given `chosen`, as snapshots do), and write the one in
 * use there (TOML, or JSON for .json). */
export async function exportCurrent(kind, chosen = null) {
  const k = KINDS[kind];
  const now = current(getState().library, kind);
  if (!now) return;
  closeDialog();
  try {
    const document = await call("library_get", { kind, id: now.id });
    const name = `${label(now).replace(/[/:]/g, "-")}.toml`;
    const path = chosen ?? (await native("save_file", { name, types: ["toml", "json"], prompt: "Export" }));
    if (!path) return;
    await native("library_export_file", { path, document });
    toast("success", `Exported “${label(now)}”`, path, { label: "Show in Finder", run: () => native("reveal", { path }) });
  } catch (error) {
    failed(`Couldn’t export the ${k.label}`, error);
  }
}

/** What the menus' commands do, by action (the native menu sends the
 * entry's id with `_switch`). */
export const COMMANDS = {};
for (const kind of Object.keys(KINDS)) {
  Object.assign(COMMANDS, {
    [`${kind}_new`]: () => openDialog("library-name", { kind, mode: "new" }),
    [`${kind}_duplicate`]: () => openDialog("library-name", { kind, mode: "duplicate" }),
    [`${kind}_rename`]: () => openDialog("library-name", { kind, mode: "rename" }),
    [`${kind}_remove`]: () => openDialog("library-remove", { kind }),
    [`${kind}_import`]: () => importFile(kind),
    [`${kind}_export`]: () => exportCurrent(kind),
    [`${kind}_switch`]: (event) => switchTo(kind, event?.id),
  });
}
for (const [action, run] of Object.entries(COMMANDS)) onMenu(action, run);

/** The menu as the menu bar shows it, drawn in the window at its place
 * (snapshots, which have no menu bar: `app.libraryMenu("profile")`). */
export function showLibraryMenu(kind) {
  const left = kind === "profile" ? 300 : 360;
  const items = menuItems(kind, getState().library).map((item) =>
    item === "-"
      ? "-"
      : { label: item.title, checked: !!item.checked, disabled: item.enabled === false, run: () => COMMANDS[item.action]?.({ id: item.id }) },
  );
  showMenu({ left, top: 0, right: left, bottom: 0 }, items);
}

// ---------- Dialogs ----------

/** New, Duplicate and Rename: a name for the scene collection or profile. */
function LibraryNameDialog({ kind, mode }) {
  const k = KINDS[kind];
  const library = useStore((s) => s.library);
  const now = current(library, kind);
  const [name, setName] = useState(() => {
    const names = entries(library, kind).map(label);
    if (mode === "rename") return label(now);
    if (mode === "duplicate") return unique(label(now), names);
    return unique("Untitled", names);
  });
  // Nothing to name (the one in use went away): close.
  const gone = mode !== "new" && !now;
  useEffect(() => void (gone && closeDialog()), [gone]);
  if (gone) return null;
  const titles = { new: `Add ${k.title}`, duplicate: `Duplicate ${k.title}`, rename: `Rename ${k.title}` };
  const called = now && menuLabel(now, entries(library, kind));
  const headings = {
    new: `Name for the new ${k.label}`,
    duplicate: `Name for the copy of “${called}”`,
    rename: `Name for “${called}”`,
  };
  const accept = () => {
    const next = name.trim();
    if (!next) return;
    closeDialog();
    if (mode === "rename") {
      if (next !== now.name) rename(kind, now.id, next);
    } else {
      create(kind, next, mode === "duplicate" ? now.id : null);
    }
  };
  return html`<${Dialog}
    title=${titles[mode]}
    width=${380}
    onCancel=${closeDialog}
    onAccept=${accept}
    buttons=${html`<button class="btn" onClick=${closeDialog}>Cancel</button><button class="btn primary" disabled=${!name.trim()} onClick=${accept}>OK</button>`}
  >
    <div class="form">
      <div class="dialog-heading">${headings[mode]}</div>
      <div class="field">
        <input value=${name} spellcheck="false" aria-label="Name" ref=${(el) => el && !el.dataset.f && ((el.dataset.f = "1"), el.focus(), el.select())}
          onInput=${(e) => setName(e.currentTarget.value)} />
      </div>
    </div>
  <//>`;
}

registerDialog("library-name", LibraryNameDialog);

/** Remove: confirm, saying which one comes on instead. */
function LibraryRemoveDialog({ kind }) {
  const k = KINDS[kind];
  const library = useStore((s) => s.library);
  const now = current(library, kind);
  const next = entries(library, kind).find((e) => e.id !== now?.id && !e.problem);
  const gone = !now || !next;
  useEffect(() => void (gone && closeDialog()), [gone]);
  if (gone) return null;
  const accept = () => {
    closeDialog();
    remove(kind, now, next);
  };
  return html`<${Dialog}
    title=${`Remove ${k.title}`}
    width=${420}
    onCancel=${closeDialog}
    onAccept=${accept}
    buttons=${html`<button class="btn" onClick=${closeDialog}>Cancel</button><button class="btn primary" onClick=${accept}>Remove</button>`}
  >
    <div class="form">
      <div class="dialog-heading">Remove “${menuLabel(now, entries(library, kind))}”?</div>
      <div class="dim">Isocast switches to “${menuLabel(next, entries(library, kind))}” first. The ${k.label}’s file is kept in the library’s deleted folder.</div>
    </div>
  <//>`;
}

registerDialog("library-remove", LibraryRemoveDialog);
