// The preview at the top of the window: the canvas, letterboxed on a dark
// surround, where the scene on air is edited. In Studio Mode it splits into
// Preview (left, edited) and Program (right), with the Transition button
// and quick transitions in a column between them, as in OBS.

import { html } from "../lib/html.js";
import { useStore } from "../lib/store.js";
import { findScene, sceneName } from "../lib/model.js";
import { editingSceneId, take } from "../lib/session.js";
import { CanvasEditor } from "./canvas.js";
import { Icon } from "./icons.js";
import { Monitor } from "./monitor.js";
import { openDialog } from "./dialog.js";
import { canvasMenu } from "./sources.js";

/** Studio Mode's quick transitions: a cut, the setup's own (a fade
 * unless it says otherwise), and a fade through black. */
function quickTransitions(setup) {
  const ms = setup.transition?.duration_ms ?? 300;
  return [
    { label: "Cut", transition: { kind: "cut" } },
    { label: `Fade (${ms} ms)`, transition: { kind: "fade", duration_ms: ms } },
    { label: `Fade to Black (${ms} ms)`, transition: { kind: "fade_to_color", color: "#000000", duration_ms: ms } },
  ];
}

function StudioColumn({ setup, canTake }) {
  return html`<div class="studio-column">
    <button class="btn primary transition-btn" disabled=${!canTake} title="Put the preview on air with the current transition (Return)" onClick=${() => take()}>
      Transition
    </button>
    <div class="quick-transitions">
      ${quickTransitions(setup).map(
        (q) => html`<button key=${q.label} class="btn quick" disabled=${!canTake} title=${`Put the preview on air: ${q.label}`} onClick=${() => take(q.transition)}>
          <span class="ellipsis">${q.label}</span>
        </button>`,
      )}
    </div>
    <button class="icon-btn" title="Transition properties" onClick=${() => openDialog("transition")}>
      <${Icon} name="settings" />
    </button>
  </div>`;
}

export function Stage() {
  const setup = useStore((s) => s.setup);
  const { scene, preview, studio, live, locked, actual } = useStore((s) => ({
    scene: s.scene,
    preview: s.preview,
    studio: s.studio,
    live: s.status?.state === "recording" || ["live", "reconnecting"].includes(s.stream?.state),
    locked: s.layout.previewLocked,
    actual: s.layout.previewScale === "canvas",
  }));
  const editing = useStore((s) => editingSceneId(s));
  if (!setup) return html`<div class="preview-area empty" />`;
  const onAir = findScene(setup, scene);
  const inPreview = findScene(setup, preview);
  const noScenes = !setup.scenes?.length;
  const emptyProgram = noScenes ? "Add a scene with + under Scenes." : !onAir ? "Nothing on air." : null;
  const aspect = setup.canvas.width / setup.canvas.height;
  const menu = (event) => {
    if (event.target.closest(".item-box")) return;
    event.preventDefault();
    canvasMenu(event, editing, true);
  };
  // Preview Scaling → Canvas: one canvas pixel to a screen pixel.
  const scale = actual ? { "--actual": `${setup.canvas.width / devicePixelRatio}px` } : {};
  const editor = html`<${CanvasEditor} sceneId=${editing} locked=${locked} />`;
  if (!studio) {
    return html`<div class=${"preview-area" + (actual ? " actual" : "")} style=${{ "--aspect": aspect, ...scale }} onContextMenu=${menu}>
      <${Monitor} target="program" name=${onAir ? sceneName(onAir) : ""} live=${live} empty=${emptyProgram}>
        ${!emptyProgram && editor}
      <//>
    </div>`;
  }
  return html`<div class=${"preview-area studio" + (actual ? " actual" : "")} style=${{ "--aspect": aspect, ...scale }} onContextMenu=${menu}>
    <div class="studio-side">
      <${Monitor}
        target="preview"
        tag="Preview"
        tagClass="preview"
        name=${inPreview ? sceneName(inPreview) : ""}
        empty=${inPreview ? null : "Click a scene to put it in preview."}
      >
        ${inPreview && editor}
      <//>
    </div>
    <${StudioColumn} setup=${setup} canTake=${!!inPreview} />
    <div class="studio-side">
      <${Monitor} target="program" tag="Program" tagClass="program" name=${onAir ? sceneName(onAir) : ""} live=${live} empty=${emptyProgram} />
    </div>
  </div>`;
}
