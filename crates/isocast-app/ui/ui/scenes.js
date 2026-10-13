// The Scenes dock: click a scene to put it on air (in Studio Mode, into
// preview), double-click to rename, drag to reorder, 1–9 to switch. The
// toolbar adds (+), removes (−) and moves the current scene up and down;
// the right-click menu is OBS's.

import { useRef, useState } from "preact/hooks";
import { html } from "../lib/html.js";
import { getState, useStore } from "../lib/store.js";
import { findScene, newId, nextName, sceneName, takenIds, withoutScene } from "../lib/model.js";
import { objectPatch } from "../lib/patch.js";
import { chooseScene, edit, editingSceneId, replaceSetup, toast } from "../lib/session.js";
import { Icon } from "./icons.js";
import { showMenu } from "./popover.js";
import { moved, rowKeys, rowTabIndex, useReorder } from "./reorder.js";
import { Dock } from "./docks.js";
import { TRANSITIONS } from "./transitions.js";

/** Add an empty scene after the others and put it up. */
export function addScene() {
  const setup = getState().setup;
  if (!setup) return null;
  const name = nextName("Scene", setup.scenes.map(sceneName));
  const id = newId(name, takenIds(setup));
  const added = edit({ scenes: [{ id, name, items: [] }] });
  chooseScene(id, added);
  return id;
}

function duplicateScene(scene) {
  const setup = getState().setup;
  const name = nextName(sceneName(scene), setup.scenes.map(sceneName));
  const id = newId(name, takenIds(setup));
  const copy = { ...structuredClone(scene), id, name };
  const index = setup.scenes.findIndex((s) => s.id === scene.id);
  const scenes = setup.scenes.slice();
  scenes.splice(index + 1, 0, copy);
  chooseScene(id, replaceSetup({ ...setup, scenes }));
  return id;
}

export function removeScene(scene) {
  const setup = getState().setup;
  if (!scene) return;
  replaceSetup(withoutScene(setup, scene.id));
  toast("info", `Removed “${sceneName(scene)}”`, "", {
    label: "Undo",
    run: () => replaceSetup(setup),
  });
}

/** Move a scene in the list: "up", "down", "top" or "bottom". */
export function moveScene(scene, where) {
  const setup = getState().setup;
  const scenes = setup.scenes;
  const from = scenes.findIndex((s) => s.id === scene.id);
  const to = { up: from - 1, down: from + 2, top: 0, bottom: scenes.length }[where];
  if (from < 0 || to < 0 || to > scenes.length || to === from || to === from + 1) return;
  replaceSetup({ ...setup, scenes: moved(scenes, from, to) });
}

function overrideItems(scene) {
  const current = scene.transition?.kind ?? null;
  const set = (kind) =>
    edit({
      scenes: [{ id: scene.id, transition: kind ? objectPatch(scene.transition ?? {}, { kind, duration_ms: scene.transition?.duration_ms ?? getState().setup.transition?.duration_ms ?? 300 }) : null }],
    });
  return [
    { label: "None", checked: !current, run: () => set(null) },
    "-",
    ...TRANSITIONS.filter(([k]) => k !== "stinger").map(([kind, label]) => ({ label, checked: current === kind, run: () => set(kind) })),
  ];
}

export function ScenesDock() {
  const setup = useStore((s) => s.setup);
  const { scene: onAir, studio } = useStore((s) => ({ scene: s.scene, studio: s.studio }));
  const editing = useStore((s) => editingSceneId(s));
  const [renaming, setRenaming] = useState(null);
  const listRef = useRef();
  const scenes = setup?.scenes ?? [];
  const current = findScene(setup, editing);

  const reorder = useReorder(listRef, {
    onMove: (from, to) => replaceSetup({ ...getState().setup, scenes: moved(scenes, from, to) }),
    onClick: (index) => {
      const scene = scenes[index];
      if (scene && scene.id !== editing) chooseScene(scene.id);
    },
  });

  const menu = (event, scene) => {
    event.preventDefault();
    event.stopPropagation();
    if (scene && scene.id !== editing) chooseScene(scene.id);
    const index = scene ? scenes.indexOf(scene) : -1;
    showMenu(event, [
      { label: "Add", run: () => setRenaming(addScene()) },
      scene && { label: "Duplicate", run: () => duplicateScene(scene) },
      scene && { label: "Rename", run: () => setRenaming(scene.id) },
      scene && { label: "Remove", run: () => removeScene(scene) },
      scene && "-",
      scene && {
        label: "Order",
        items: [
          { label: "Move Up", disabled: index === 0, run: () => moveScene(scene, "up") },
          { label: "Move Down", disabled: index === scenes.length - 1, run: () => moveScene(scene, "down") },
          "-",
          { label: "Move to Top", disabled: index === 0, run: () => moveScene(scene, "top") },
          { label: "Move to Bottom", disabled: index === scenes.length - 1, run: () => moveScene(scene, "bottom") },
        ],
      },
      scene && "-",
      scene && { label: "Transition Override", items: overrideItems(scene) },
    ]);
  };

  const index = current ? scenes.indexOf(current) : -1;
  const toolbar = html`
    <button class="icon-btn small" title="Add a scene" disabled=${!setup} onClick=${() => setRenaming(addScene())}><${Icon} name="plus" /></button>
    <button class="icon-btn small" title="Remove the scene" disabled=${!current} onClick=${() => removeScene(current)}><${Icon} name="minus" /></button>
    <span class="toolbar-gap" />
    <button class="icon-btn small" title="Move up" disabled=${index <= 0} onClick=${() => moveScene(current, "up")}><${Icon} name="chevronUp" /></button>
    <button class="icon-btn small" title="Move down" disabled=${index < 0 || index === scenes.length - 1} onClick=${() => moveScene(current, "down")}>
      <${Icon} name="chevronDown" />
    </button>
  `;

  return html`<${Dock} title="Scenes" toolbar=${toolbar}>
    <div class="dock-list" ref=${listRef} onContextMenu=${(e) => e.target === listRef.current && menu(e, null)}>
      ${scenes.length === 0 && html`<div class="list-empty">No scenes. Click + below to add one.</div>`}
      ${scenes.map((scene, i) => {
        const selected = scene.id === editing;
        const live = studio && scene.id === onAir;
        return html`<div
          key=${scene.id}
          data-row
          class=${"row" + (selected ? " selected" : "") + (reorder.dragging === i ? " dragging" : "")}
          title=${i < 9 ? `${sceneName(scene)} (${i + 1})` : sceneName(scene)}
          tabindex=${rowTabIndex(i, index)}
          onKeyDown=${(e) => rowKeys(e, listRef, i, scenes.length, { pick: (j) => chooseScene(scenes[j].id), rename: () => setRenaming(scene.id) })}
          onPointerDown=${(e) => reorder.start(e, i)}
          onDblClick=${() => setRenaming(scene.id)}
          onContextMenu=${(e) => menu(e, scene)}
        >
          ${renaming === scene.id
            ? html`<${RenameInput}
                value=${sceneName(scene)}
                onDone=${(name) => {
                  setRenaming(null);
                  if (name && name !== sceneName(scene)) edit({ scenes: [{ id: scene.id, name }] });
                }}
              />`
            : html`<span class="label">${sceneName(scene)}</span>`}
          ${live && html`<span class="program-dot" title="On air (Program)" />`}
        </div>`;
      })}
      ${reorder.line && html`<div class="drop-line" style=${{ top: `${reorder.line.top}px` }} />`}
    </div>
  <//>`;
}

/** An inline name editor: Enter or blur keeps, Escape cancels. */
export function RenameInput({ value, onDone }) {
  const done = useRef(false);
  const finish = (name) => {
    if (done.current) return;
    done.current = true;
    onDone(name);
  };
  return html`<input
    class="rename"
    value=${value}
    ref=${(el) => el && !el.dataset.focused && ((el.dataset.focused = "1"), el.focus(), el.select())}
    onPointerDown=${(e) => e.stopPropagation()}
    onKeyDown=${(e) => {
      e.stopPropagation();
      if (e.key === "Enter") finish(e.currentTarget.value.trim());
      if (e.key === "Escape") finish(null);
    }}
    onBlur=${(e) => finish(e.currentTarget.value.trim())}
  />`;
}
