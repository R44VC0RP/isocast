// Popovers and menus: one at a time, anchored to an element or a point,
// kept inside the window, closed by a click outside or Escape. Menu items
// can carry a check mark and open submenus (hover, or → on the keyboard;
// ↑ and ↓ move between items, ← leaves a submenu).

import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import { html } from "../lib/html.js";
import { setState, useStore } from "../lib/store.js";
import { Icon } from "./icons.js";

/** Open a menu of `items` at an element or a mouse event. An item is
 * {label, icon, hint, danger, disabled, checked, run} or {label, items}
 * for a submenu, "-" for a separator, or {heading}. */
export function showMenu(at, items) {
  // A button that opened the menu looks pressed while it is open.
  setState({ menu: { anchor: anchorOf(at), items, owner: at instanceof Element ? at : null } });
}

export function closeMenu() {
  setState({ menu: null });
}

function anchorOf(at) {
  if (at instanceof Element) {
    const r = at.getBoundingClientRect();
    return { left: r.left, top: r.top, right: r.right, bottom: r.bottom };
  }
  if (at && "clientX" in at) return { left: at.clientX, top: at.clientY, right: at.clientX, bottom: at.clientY };
  return at;
}

/** Place `el` below (or above) `anchor`, aligned to its left edge unless
 * `align` is "right"; beside it with "side" (submenus, dialogs' helpers). */
function place(el, anchor, align) {
  const box = el.getBoundingClientRect();
  const margin = 8;
  let left = align === "right" ? anchor.right - box.width : anchor.left;
  let top = anchor.bottom + 6;
  if (align === "side" || align === "submenu") {
    const gap = align === "submenu" ? 2 : 12;
    left = anchor.right + gap;
    if (left + box.width > innerWidth - margin) left = anchor.left - box.width - gap;
    top = Math.max(margin, Math.min(anchor.top - (align === "submenu" ? 5 : 8), innerHeight - box.height - margin));
  } else if (top + box.height > innerHeight - margin) {
    top = Math.max(margin, anchor.top - box.height - 6);
  }
  left = Math.min(Math.max(margin, left), innerWidth - box.width - margin);
  el.style.left = `${Math.round(left)}px`;
  el.style.top = `${Math.round(top)}px`;
}

/** A floating panel at `anchor`. */
export function Popover({ anchor, align = "left", onClose, class: cls = "", children }) {
  const ref = useRef();
  useLayoutEffect(() => {
    if (ref.current && anchor) place(ref.current, anchor, align);
  });
  useEffect(() => {
    const key = (event) => {
      if (event.key === "Escape") {
        event.stopPropagation();
        onClose();
      }
    };
    addEventListener("keydown", key, true);
    return () => removeEventListener("keydown", key, true);
  }, [onClose]);
  return html`<div
      class="scrim"
      onPointerDown=${onClose}
      onContextMenu=${(e) => {
        e.preventDefault();
        onClose();
      }}
    />
    <div class=${"popover " + cls} ref=${ref} style="left: -9999px; top: -9999px">${children}</div>`;
}

/** Focus the next enabled item (`by` 1 or -1) in `popover`, wrapping. */
function step(popover, by) {
  const items = [...popover.querySelectorAll(":scope > .menu-item:not([disabled])")];
  if (!items.length) return;
  const at = items.indexOf(document.activeElement);
  items[at < 0 ? (by > 0 ? 0 : items.length - 1) : (at + by + items.length) % items.length].focus();
}

function Submenu({ anchor, items, lane, onDone, onBack }) {
  const ref = useRef();
  useLayoutEffect(() => {
    if (ref.current) place(ref.current, anchor, "submenu");
  });
  return html`<div class="popover submenu" ref=${ref} style="left: -9999px; top: -9999px">
    <${MenuItems} items=${items} lane=${lane} onDone=${onDone} onBack=${onBack} />
  </div>`;
}

/** `lane`: keep a 16px lane before the labels even without marks (a
 * submenu of a menu that has one, so its text doesn't jump). */
export function MenuItems({ items, lane = false, onDone, onBack }) {
  const [open, setOpen] = useState(null);
  const timer = useRef();
  const opener = useRef();
  const keys = (event, sub, enter) => {
    const by = { ArrowDown: 1, ArrowUp: -1 }[event.key];
    if (by) {
      event.preventDefault();
      step(event.currentTarget.parentElement, by);
    } else if (sub && event.key === "ArrowRight") {
      event.preventDefault();
      opener.current = event.currentTarget;
      enter(event);
      requestAnimationFrame(() => {
        const submenu = document.querySelector(".popover.submenu");
        if (submenu) step(submenu, 1);
      });
    } else if (onBack && event.key === "ArrowLeft") {
      event.preventDefault();
      onBack();
    }
  };
  const list = items.filter(Boolean);
  const hasMarks = lane || list.some((item) => item !== "-" && (item.icon || item.checked !== undefined));
  useEffect(() => () => clearTimeout(timer.current), []);
  return list.map((item, i) => {
    if (item === "-") return html`<div class="menu-sep" key=${i} />`;
    if (item.heading) return html`<div class="menu-label" key=${i}>${item.heading}</div>`;
    const sub = item.items?.filter(Boolean);
    const enter = (event) => {
      clearTimeout(timer.current);
      if (!sub) {
        timer.current = setTimeout(() => setOpen(null), 150);
        return;
      }
      const r = event.currentTarget.getBoundingClientRect();
      setOpen({ index: i, anchor: { left: r.left, top: r.top, right: r.right, bottom: r.bottom } });
    };
    const mark = item.checked
      ? html`<${Icon} name="check" class="menu-check" />`
      : item.icon
        ? html`<${Icon} name=${item.icon} />`
        : hasMarks
          ? html`<span class="menu-lane" />`
          : null;
    return html`<button
        key=${i}
        class=${"menu-item" + (item.danger ? " danger" : "") + (open?.index === i ? " active" : "")}
        disabled=${item.disabled || (sub && !sub.length)}
        onPointerEnter=${enter}
        onKeyDown=${(e) => keys(e, sub, enter)}
        onClick=${(e) => {
          if (sub) {
            enter(e);
            return;
          }
          onDone?.();
          item.run?.();
        }}
      >
        ${mark}
        <span class="menu-text">${item.label}</span>
        ${item.hint && html`<span class="hint">${item.hint}</span>`}
        ${sub && html`<${Icon} name="chevronRight" size=${14} class="submenu-chev" />`}
      </button>
      ${open?.index === i && sub && html`<${Submenu}
        key=${"sub" + i}
        anchor=${open.anchor}
        items=${sub}
        lane=${hasMarks}
        onDone=${onDone}
        onBack=${() => {
          setOpen(null);
          opener.current?.focus();
        }}
      />`}`;
  });
}

/** The one open menu, if any. */
export function MenuHost() {
  const menu = useStore((s) => s.menu);
  useEffect(() => {
    const owner = menu?.owner;
    owner?.classList.add("menu-open");
    // ↑ or ↓ before any item has focus enters the menu.
    const key = (event) => {
      if ((event.key === "ArrowDown" || event.key === "ArrowUp") && !document.activeElement?.closest?.(".popover")) {
        const popover = document.querySelector(".popover.menu");
        if (!popover) return;
        event.preventDefault();
        event.stopPropagation();
        step(popover, event.key === "ArrowDown" ? 1 : -1);
      }
    };
    if (menu) addEventListener("keydown", key, true);
    return () => {
      owner?.classList.remove("menu-open");
      removeEventListener("keydown", key, true);
    };
  }, [menu]);
  if (!menu) return null;
  return html`<${Popover} anchor=${menu.anchor} onClose=${closeMenu} class="menu">
    <${MenuItems} items=${menu.items} onDone=${closeMenu} />
  <//>`;
}
