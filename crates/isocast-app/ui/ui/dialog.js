// Dialogs, as OBS has them: window-like panels over the main window with a
// title bar (drag it to move the dialog), a body, and buttons along the
// bottom: a reset (Defaults) on the left, OK and Cancel (or Close) on the
// right. One is open at a time (`state.dialog`); Escape cancels, Return
// accepts (outside multi-line text). Focus moves into the dialog, Tab
// stays inside it, and focus goes back where it was when it closes.

import { useEffect, useRef, useState } from "preact/hooks";
import { html } from "../lib/html.js";
import { getState, setState, useStore } from "../lib/store.js";

/** Open dialog `type` with its props, replacing any open one. */
export function openDialog(type, props = {}) {
  setState({ dialog: { type, ...props }, menu: null, adding: null });
}

export function closeDialog() {
  setState({ dialog: null });
}

/** The open dialog's props if it is of `type`. */
export const dialogOf = (type) => {
  const dialog = getState().dialog;
  return dialog?.type === type ? dialog : null;
};

const FOCUSABLE = "button, input, select, textarea, [tabindex]:not([tabindex='-1'])";

/** A dialog frame. `onAccept` runs on Return; `onCancel` on Escape, the
 * scrim and Cancel. `buttons` and `extra` (bottom left) are the footer. */
export function Dialog({ title, width = 560, height, onCancel, onAccept, extra, buttons, class: cls = "", children }) {
  const [offset, setOffset] = useState([0, 0]);
  const ref = useRef();
  // The newest handlers: Return lets a field commit what was typed (a
  // re-render), then accepts with what that render made.
  const handlers = useRef();
  handlers.current = { onCancel, onAccept };
  useEffect(() => {
    const before = document.activeElement;
    // A field that focused itself (a new source's name) keeps focus.
    if (ref.current && !ref.current.contains(document.activeElement)) ref.current.focus({ preventScroll: true });
    const key = (event) => {
      // A menu or popover over the dialog takes Escape first.
      if (getState().menu) return;
      if (event.key === "Tab" && ref.current) {
        const focusable = [...ref.current.querySelectorAll(FOCUSABLE)].filter((el) => !el.disabled && el.offsetParent !== null);
        if (!focusable.length) return;
        const first = focusable[0];
        const last = focusable[focusable.length - 1];
        const inside = ref.current.contains(document.activeElement);
        if (event.shiftKey && (!inside || document.activeElement === first || document.activeElement === ref.current)) {
          event.preventDefault();
          last.focus();
        } else if (!event.shiftKey && (!inside || document.activeElement === last)) {
          event.preventDefault();
          first.focus();
        }
        return;
      }
      if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        handlers.current.onCancel?.();
      } else if (event.key === "Enter" && handlers.current.onAccept && !event.target.closest?.("textarea, button, select")) {
        event.target.blur?.();
        event.preventDefault();
        setTimeout(() => handlers.current.onAccept?.());
      }
    };
    addEventListener("keydown", key, true);
    return () => {
      removeEventListener("keydown", key, true);
      if (before instanceof HTMLElement && before.isConnected) before.focus({ preventScroll: true });
    };
  }, []);
  const drag = (event) => {
    if (event.button !== 0 || event.target.closest("button")) return;
    event.preventDefault();
    const start = [event.clientX - offset[0], event.clientY - offset[1]];
    const box = ref.current.getBoundingClientRect();
    const move = (e) => {
      // Keep the title bar on screen.
      const x = Math.min(Math.max(e.clientX - start[0], -box.left - box.width + 80), innerWidth - box.left - 80);
      const y = Math.min(Math.max(e.clientY - start[1], -box.top), innerHeight - box.top - 40);
      setOffset([x, y]);
    };
    const up = () => {
      removeEventListener("pointermove", move);
      removeEventListener("pointerup", up);
    };
    addEventListener("pointermove", move);
    addEventListener("pointerup", up);
  };
  return html`<div class="dialog-scrim" onPointerDown=${(e) => e.target === e.currentTarget && e.preventDefault()}>
    <div
      class=${"dialog " + cls}
      role="dialog"
      aria-modal="true"
      aria-label=${title}
      tabindex="-1"
      ref=${ref}
      style=${{ width: `${width}px`, height: height ? `${height}px` : undefined, transform: `translate(${offset[0]}px, ${offset[1]}px)` }}
    >
      <div class="dialog-title" onPointerDown=${drag} onDblClick=${() => setOffset([0, 0])}>${title}</div>
      <div class="dialog-body">${children}</div>
      ${(buttons || extra) &&
      html`<div class="dialog-foot">
        <div class="dialog-extra">${extra}</div>
        <div class="dialog-buttons">${buttons}</div>
      </div>`}
    </div>
  </div>`;
}

const hosts = new Map();

/** Make `Component` the dialog shown for `type`. */
export function registerDialog(type, Component) {
  hosts.set(type, Component);
}

/** The open dialog, if any. */
export function DialogHost() {
  const dialog = useStore((s) => s.dialog);
  const Component = dialog && hosts.get(dialog.type);
  if (!Component) return null;
  return html`<${Component} key=${dialog.type + (dialog.key ?? "")} ...${dialog} />`;
}
