// Every keyboard shortcut, as Settings → Hotkeys lists them (Help →
// Keyboard Shortcuts opens that page). Keys that act on the preview and
// the lists never fire while typing in a field.

export const SHORTCUTS = [
  [
    "Scenes and Studio Mode",
    [
      [["1", "–", "9"], "Switch to that scene (in Studio Mode, put it in preview)"],
      [["Return"], "Transition: put the preview on air (Studio Mode; Space too)"],
      [["⇧", "⌘", "S"], "Studio Mode"],
      [["⇧", "⌘", "N"], "Add a scene"],
    ],
  ],
  [
    "Sources",
    [
      [["⇧", "⌘", "A"], "Add a source"],
      [["⌘", "E"], "Edit Transform"],
      [["⌘", "R"], "Reset Transform"],
      [["⌘", "F"], "Fit to Screen"],
      [["⌘", "S"], "Stretch to Screen"],
      [["⌘", "D"], "Center to Screen"],
      [["⌘", "C"], "Copy the selected source (⌘V: paste it as a reference)"],
      [["←", "↑", "→", "↓"], "Nudge the selected source 1 px (with ⇧, 10 px)"],
      [["⌫"], "Remove the selected source from the scene"],
      [["Esc"], "Deselect"],
    ],
  ],
  [
    "On the preview",
    [
      [["⇧", "Drag"], "Move along one axis; resize keeping the shape"],
      [["⌥", "Drag"], "Crop from an edge or corner"],
      [["⌘", "Drag"], "Place freely, without snapping"],
      [["⌘", "Z"], "Undo (⇧⌘Z: redo)"],
    ],
  ],
  [
    "Fields and the mixer",
    [
      [["Drag"], "A number’s label: scrub (with ⇧, ×10; with ⌥, ×0.1)"],
      [["↑", "↓"], "In a number: step (with ⇧, ×10)"],
      [["Return"], "Apply what you typed (Esc: put it back)"],
      [["Double-click"], "A fader: back to 0 dB"],
    ],
  ],
  [
    "From the keyboard",
    [
      [["Tab"], "Move between controls (in a dialog, it stays in the dialog)"],
      [["↑", "↓"], "In the Scenes or Sources list (reached with Tab): the next one"],
      [["F2"], "In the Scenes or Sources list: rename"],
      [["←", "→"], "On a slider: move it (with ⇧, ×10; Home and End: its ends)"],
      [["↑", "↓"], "In a menu: the next item (→ opens a submenu, ← leaves it)"],
    ],
  ],
  [
    "Window",
    [
      [["⌘", ","], "Settings"],
      [["⌃", "⌘", "F"], "Full screen"],
      [["⌘", "/"], "These shortcuts"],
    ],
  ],
];
