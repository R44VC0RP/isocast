// The main window, laid out as OBS Studio's: the preview at the top, the
// source toolbar under it, the docks along the bottom (Scenes, Sources,
// Audio Mixer, Scene Transitions, Controls) and the status bar under
// those. Properties, Filters, Edit Transform and Settings are dialogs.

import { useEffect } from "preact/hooks";
import { html } from "../lib/html.js";
import { native } from "../lib/bridge.js";
import { getState, setState, useStore } from "../lib/store.js";
import { findScene, findSource, key } from "../lib/model.js";
import { chooseScene, edit, editingSceneId, onMenu, redo, select, setStudio, take, undo } from "../lib/session.js";
import { selected, updateItem } from "../lib/items.js";
import { centerToScreen, fitToScreen, resetTransform, rotateBy, stretchToScreen } from "../lib/transform.js";
import { DockRow, RowSplitter, defaultDocksHeight, loadLayout, resetDocks, resetLayout, setLayout, toggleDock } from "./docks.js";
import { DialogHost, closeDialog, openDialog } from "./dialog.js";
import { MenuHost } from "./popover.js";
import { ScenesDock, addScene } from "./scenes.js";
import { SourcesDock, addMenu, copySelected, orderSelected, paste, removeSelected } from "./sources.js";
import { MixerDock } from "./mixer.js";
import { TransitionsDock } from "./transitions.js";
import { ControlsDock } from "./controls-dock.js";
import { Stage } from "./stage.js";
import { SourceToolbar } from "./source-toolbar.js";
import { RecordingDetails, StatusBar, StreamDetails } from "./statusbar.js";
import { Onboarding, startOnboarding } from "./onboarding.js";
import { Connecting, Toasts, Welcome } from "./states.js";
import { COMMANDS as LIBRARY, exportCurrent, importFile, showLibraryMenu, useLibraryMenus } from "./library.js";
import { openSettings } from "./settings.js";
// Dialogs register themselves.
import "./add-source.js";
import "./properties.js";
import "./filters.js";
import "./transform.js";
import "./audio-props.js";

setState({ layout: loadLayout() });

const typing = (event) => event.target.closest?.("input, textarea, select, [contenteditable]");

/** Keyboard shortcuts for the preview, scenes and sources. */
function onKey(event) {
  if (typing(event) || getState().menu) return;
  const state = getState();
  const setup = state.setup;
  if (!setup) return;
  const plain = !event.metaKey && !event.ctrlKey && !event.altKey;
  const command = event.metaKey && !event.ctrlKey && !event.altKey;
  const letter = event.key.length === 1 ? event.key.toLowerCase() : event.key;
  // ⌘Z and ⇧⌘Z undo and redo setup changes (text fields keep their own).
  if (command && letter === "z") {
    event.preventDefault();
    if (event.shiftKey) redo();
    else undo();
    return;
  }
  // The rest act on the main window, not over a dialog.
  if (state.dialog || state.onboarding) return;
  if (command && !event.shiftKey) {
    const actions = {
      e: () => openDialog("transform"),
      r: resetTransform,
      f: fitToScreen,
      s: stretchToScreen,
      d: () => centerToScreen(),
      c: copySelected,
      v: () => paste(false),
    };
    if (actions[letter]) {
      event.preventDefault();
      actions[letter]();
      return;
    }
  }
  // 1–9: switch to (or preview) that scene.
  if (plain && /^[1-9]$/.test(event.key)) {
    const scene = setup.scenes[Number(event.key) - 1];
    if (scene) chooseScene(scene.id);
    event.preventDefault();
    return;
  }
  if (event.key === "Escape") {
    select(null);
    return;
  }
  // Return and Space press a focused button instead.
  const onButton = event.target.closest?.("button, a, [role=button], [role=switch]");
  if (state.studio && plain && !onButton && (event.key === "Enter" || event.key === " ") && state.preview) {
    event.preventDefault();
    take();
    return;
  }
  const found = selected(state);
  if (!found) return;
  const { scene, chain, item } = found;
  if (plain && (event.key === "Backspace" || event.key === "Delete")) {
    event.preventDefault();
    removeSelected();
    return;
  }
  const arrows = { ArrowLeft: [-1, 0], ArrowRight: [1, 0], ArrowUp: [0, -1], ArrowDown: [0, 1] };
  if (arrows[event.key] && !event.metaKey && !item.locked && item.rect && !state.layout.previewLocked) {
    event.preventDefault();
    const step = event.shiftKey ? 10 : 1;
    const [dx, dy] = arrows[event.key];
    updateItem(scene.id, chain, { rect: { x: item.rect.x + dx * step, y: item.rect.y + dy * step } }, { key: `nudge:${scene.id}:${chain.map(key).join("/")}`, interval: 50 });
  }
}

/** The selected item's source, for Filters and Properties. */
const selectedSource = () => {
  const found = selected(getState());
  return found?.item.source ? findSource(getState().setup, found.item.source) : null;
};

/** In a text field, the menu's Undo and Redo are the field's own. */
const textUndo = (redoing) => {
  if (!document.activeElement?.closest?.("input, textarea, [contenteditable]")) return false;
  document.execCommand(redoing ? "redo" : "undo");
  return true;
};

/** A menu command that acts on the main window: not while a dialog is
 * open or a field is being typed in (its key, like ⌘S, is the field's). */
const onWindow = (run) => () => {
  if (getState().dialog || getState().onboarding || document.activeElement?.closest?.("input, textarea, select")) return;
  run();
};

/** What each native menu command does (src/mac: MENU_ACTIONS). */
const MENU = {
  settings: () => openSettings(),
  hotkeys: () => openSettings("hotkeys"),
  studio_mode: () => setStudio(!getState().studio),
  new_scene: () => addScene(),
  add_source: () => addMenu(document.querySelector(".dock-slot[data-dock=sources] .dock-toolbar .icon-btn") ?? { left: innerWidth / 2, top: innerHeight / 2, right: innerWidth / 2, bottom: innerHeight / 2 }),
  show_recordings: () => getState().setup && native("reveal", { path: getState().setup.recording.folder }),
  start_over: startOnboarding,
  undo: () => textUndo(false) || undo(),
  redo: () => textUndo(true) || redo(),
  paste_duplicate: onWindow(() => paste(true)),
  transform_edit: onWindow(() => openDialog("transform")),
  transform_reset: onWindow(resetTransform),
  rotate_cw: onWindow(() => rotateBy(90)),
  rotate_ccw: onWindow(() => rotateBy(-90)),
  rotate_180: onWindow(() => rotateBy(180)),
  fit_screen: onWindow(fitToScreen),
  stretch_screen: onWindow(stretchToScreen),
  center_screen: onWindow(() => centerToScreen()),
  center_vertical: onWindow(() => centerToScreen("y")),
  center_horizontal: onWindow(() => centerToScreen("x")),
  order_up: onWindow(() => orderSelected("forward")),
  order_down: onWindow(() => orderSelected("backward")),
  order_top: onWindow(() => orderSelected("front")),
  order_bottom: onWindow(() => orderSelected("back")),
  remove: onWindow(() => removeSelected()),
  filters: onWindow(() => selectedSource() && openDialog("filters", { source: selectedSource().id })),
  properties: onWindow(() => selectedSource() && openDialog("properties", { source: selectedSource().id })),
  lock_preview: () => setLayout({ previewLocked: !getState().layout.previewLocked }),
  advanced_audio: () => openDialog("advanced-audio"),
  source_toolbar: () => setLayout({ sourceToolbar: !getState().layout.sourceToolbar }),
  status_bar: () => setLayout({ statusBar: !getState().layout.statusBar }),
  reset_ui: resetLayout,
  lock_docks: () => setLayout({ locked: !getState().layout.locked }),
  reset_docks: resetDocks,
  dock_scenes: () => toggleDock("scenes"),
  dock_sources: () => toggleDock("sources"),
  dock_mixer: () => toggleDock("mixer"),
  dock_transitions: () => toggleDock("transitions"),
  dock_controls: () => toggleDock("controls"),
};

/** Tell the shell which menu items are checked, and what they say. */
function useMenuState() {
  const s = useStore((st) => ({
    studio: st.studio,
    layout: st.layout,
    name: st.setup?.name ?? null,
  }));
  useEffect(() => {
    const hidden = s.layout.hidden;
    native("menu_state", {
      checked: {
        studio_mode: s.studio,
        source_toolbar: s.layout.sourceToolbar,
        status_bar: s.layout.statusBar,
        lock_preview: s.layout.previewLocked,
        lock_docks: s.layout.locked,
        dock_scenes: !hidden.includes("scenes"),
        dock_sources: !hidden.includes("sources"),
        dock_mixer: !hidden.includes("mixer"),
        dock_transitions: !hidden.includes("transitions"),
        dock_controls: !hidden.includes("controls"),
      },
    }).catch(() => {});
  }, [s.studio, s.layout]);
  useEffect(() => {
    native("title", { title: s.name ? `Isocast — ${s.name}` : "Isocast" }).catch(() => {});
  }, [s.name]);
}

function Workspace() {
  const layout = useStore((s) => s.layout);
  const height = layout.height ?? defaultDocksHeight();
  return html`<div class="workspace">
    <${Stage} />
    ${layout.sourceToolbar && html`<${SourceToolbar} />`}
    <${RowSplitter} />
    <div class="docks" style=${{ height: `${height}px` }}>
      <${DockRow}
        docks=${{
          scenes: html`<${ScenesDock} />`,
          sources: html`<${SourcesDock} />`,
          mixer: html`<${MixerDock} />`,
          transitions: html`<${TransitionsDock} />`,
          controls: html`<${ControlsDock} />`,
        }}
      />
    </div>
  </div>`;
}

export function App() {
  const { connection, loaded, hasSetup, fullScreen, onboarding, statusBar } = useStore((s) => ({
    onboarding: s.onboarding,
    connection: s.connection.state,
    loaded: s.loaded,
    hasSetup: !!s.setup,
    fullScreen: s.fullScreen,
    statusBar: s.layout.statusBar,
  }));
  const setup = useStore((s) => s.setup);
  useMenuState();
  useLibraryMenus();

  useEffect(() => {
    addEventListener("keydown", onKey);
    // No browser context menu outside text fields.
    const context = (e) => !typing(e) && e.preventDefault();
    addEventListener("contextmenu", context);
    for (const [action, run] of Object.entries(MENU)) onMenu(action, run);
    return () => {
      removeEventListener("keydown", onKey);
      removeEventListener("contextmenu", context);
    };
  }, []);

  // Drop selections that no longer exist.
  useEffect(() => {
    const state = getState();
    if (state.selection && !selected(state)) setState({ selection: null });
    if (state.selection && state.selection.scene !== editingSceneId(state)) setState({ selection: null });
  }, [setup]);

  let body;
  if ((connection !== "connected" && !hasSetup) || !loaded) body = html`<${Connecting} />`;
  else body = html`<${Workspace} />`;

  return html`<div class=${"app" + (fullScreen ? " full-screen" : "") + (statusBar ? "" : " no-status")}>
    ${body}
    ${statusBar && html`<${StatusBar} />`}
    ${loaded && !hasSetup && !onboarding && html`<${Welcome} />`}
    ${loaded && onboarding && html`<${Onboarding} />`}
    <${DialogHost} />
    <${RecordingDetails} />
    <${StreamDetails} />
    <${MenuHost} />
    <${Toasts} />
  </div>`;
}

// Helpers for scripts (`isocast-app --snapshot … --eval "app.select('main', 'bars')"`).
window.app = {
  state: getState,
  select: (scene, ...path) => select({ scene, path }),
  studio: (on = true) => setStudio(on),
  scene: (id) => chooseScene(id),
  findScene: (id) => findScene(getState().setup, id),
  /** Run a native menu command by name (see MENU and library.js), with the
   * entry's id for the Profile and Scene Collection lists. */
  menu: (action, id) => (MENU[action] ?? LIBRARY[action])?.({ action, id }),
  /** The Profile or Scene Collection menu, drawn where the menu bar has it
   * ("profile" or "collection"). */
  libraryMenu: showLibraryMenu,
  /** Import… and Export… with the file the panel would give. */
  libraryImport: importFile,
  libraryExport: exportCurrent,
  dialog: openDialog,
  closeDialog,
  settings: (tab) => openSettings(tab),
  properties: (id) => openDialog("properties", { source: id }),
  filters: (id) => openDialog("filters", { source: id }),
  addSource: () => addMenu(document.querySelector(".dock-slot[data-dock=sources] .dock-toolbar .icon-btn")),
  layout: setLayout,
  undo,
  redo,
  edit,
};
