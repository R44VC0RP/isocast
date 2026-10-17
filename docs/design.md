# Isocast design language

> **Superseded for the native app (2026-10-13): the Console direction.**
> Ryan picked Console (Paper file "C. Console", Final Cut Pro x Logic Pro
> x Tower): pure neutral blacks in a lighter frame, Mona Sans, Hubot Sans
> and Martian Mono, and only three colours that light up (yellow for
> selected/active and the one main button, green for the preview and
> the stream, red for the program and recording: the broadcast tally,
> Ryan on #95, 2026-10-16). Its values live in
> `crates/isocast-ui/ui/theme.slint` (named as Paper's tokens) and its
> pieces in the kit (`ui/kit.slint` lists them; `--state kit` draws them
> as the Components board). Read values from Paper, never from this file;
> the layout and naming rules below (OBS's layout and words, flat panes,
> clean-room) still hold. PLAN.md N.5.

Status: spec, 2026-10-12. Written from snapshots of every view of the web
app (`crates/isocast-app/ui`) at commit a24edf5. It describes the target
look, not the current one. The P1/P2/P3 change list in section 5 is the
work order.

Ryan's direction: "refine the UI so it looks really clean, (like an adobe
product type thing)". That means the discipline of a professional creative
app: quiet neutral greys, one accent, a strict spacing grid, compact
controls of consistent height, a crisp type hierarchy, consistent icons,
tidy panel headers, and clear hover, selected and focus states. The
earlier rules still hold:

- **Layout and words follow OBS Studio.** Things stay in the same places
  with the same names.
- **Flat.** Panes meet at 1 px hairlines and lists run edge to edge.
  There are no cards inside cards, and corners are barely rounded.
- **The palette and the brand face stay.** Ryan likes the mineral palette
  (copper, verdigris, slate, bone) and Instrument Sans.

Clean-room: nothing here comes from OBS's source, `.ui`, `.qss` or theme
files, or from Adobe's assets. No Adobe branding, icons or fonts (Adobe
Clean) are used. The values below are our own.

Every value is a plain token (colour, length, weight, duration) with no
`calc()` chains, so the Slint rewrite (PLAN.md N) can copy them into a
`global Theme` as-is.

---

## 1. Principles

1. **Structure is grey, meaning is colour.** Surfaces, lines and text use
   the neutral ramp. Colour appears only for one of these:
   - the accent: selection, focus, primary action, or a control that is on;
   - a status: on air, recording or live, warning, error.
2. **One accent: copper.** It is used for:
   - the canvas selection box;
   - the selected row's marker;
   - focus rings;
   - the primary button;
   - toggles, checkboxes and radios that are on;
   - the ISO toggle and the drop line.

   Copper is not used to fill a whole row, a menu highlight, a volume
   fader or a slider.
3. **Quiet chrome, bright content.** Panel headers, labels and units are
   dimmer than the things the user works on (scene and source names,
   values).
4. **A 4 px grid.** Heights, pane paddings and gaps between controls are
   multiples of 4. The exceptions are 1 px hairlines, the 2 px gap between
   icon buttons, 2 px and 6 px padding inside small elements (kbd, status
   items, the drag readout, textarea text) for optical centring, and the
   10 px meter scale.
5. **One height per control size.** Controls are 24 (small), 28 (default)
   or 32 (large) px. List rows are 28 px and menu items 24 px.
6. **Flat, with only floating things lifted.** Only menus, popovers,
   dialogs and toasts have a shadow and the 3 px radius. Everything inside
   a pane is separated by lines, not boxes.
7. **Hot paths stay cheap.** Nothing on the monitor, canvas overlay, meters
   or drag path uses `backdrop-filter`, blur, large shadows or
   layout-changing hover effects. Hover changes colour only.

---

## 2. Tokens

All tokens live in `:root` in `crates/isocast-app/ui/app.css`. Section 4
maps every current token to its replacement. New names are given first,
with the old name in brackets.

### 2.1 Neutral ramp (surfaces and lines)

These are slate-tinted neutral greys (hue about 210°, low saturation), the
mineral palette's slate family. Each step is visibly lighter than the one
below it, so hover, selected and pressed never look alike.

| Ramp | Hex | Token(s) | Used for |
|---|---|---|---|
| g0 | `#0b0c0d` | `--surface-monitor` [`--bg-monitor`] | the monitor behind the picture; handle borders |
| g1 | `#111315` | `--surface-stage` [`--bg-stage`] | the preview's surround; source previews in dialogs; full-window states |
| g2 | `#141619` | `--surface-chrome` [`--bg-app`]; `--surface-inset` [`--bg-field`, was `#15171a`] | status bar, dialog title bars, settings nav, the native title bar (`BACKGROUND` in `src/mac/mod.rs`, unchanged); fields, pick lists |
| g3 | `#17191d` | `--surface-header` (new) | dock header band |
| g4 | `#1b1e22` | `--surface-panel` [`--bg-panel`] | docks, source toolbar, dialog bodies |
| g5 | `#22262b` | `--surface-overlay` [`--bg-popover`] | menus, popovers, toasts |
| g6 | `#262a30` | `--hover` [`--bg-hover`]; `--control` [`--bg-raised`, was `#24282d`] | row hover and quiet-button hover on a panel; secondary button at rest |
| g7 | `#2b3035` | `--line` | every structural hairline |
| g8 | `#30353b` | `--control-hover` [`--bg-active`, was `#2c3137`]; `--overlay-hover` (new) | button hover; menu item hover |
| g9 | `#383e45` | `--line-strong`; `--control-active`; `--selected` and `--selected-hover` [`--sel`, `--sel-hover`]; `--field-border` | control borders, pressed buttons, a selected row (same on hover), field borders |
| g10 | `#4a5058` | `--field-border-hover` (new); `--scroll-thumb-hover` | field border on hover; scroll thumb on hover |

`--line-soft: rgba(238, 234, 226, 0.06)` stays, but only for separators
between items of one list: audio table rows, hotkey rows and mixer strips.
It is never used for a pane's edge.

### 2.2 Text

Text is warm (the limestone and bone family), on the slate surfaces.

| Token | Hex | Role | Contrast (panel / overlay / selected) |
|---|---|---|---|
| `--text` | `#eeeae2` | primary content: names, values, titles | 13.9 / 12.7 / 10.3 |
| `--text-2` | `#b0aba3` (was `#a39e95`) | labels, panel titles, secondary values, idle icons | 7.3 / 6.7 / 5.4 |
| `--text-3` | `#928e87` (was `#6f6b65`, which was 3.2:1) | hints, units, placeholders, meter scale, empty-list text | 5.1 / 4.7 / 3.8 ✗ |
| `--text-disabled` | `#66625c` (new) | disabled labels only (exempt from the contrast rule) | 2.8 |
| `--outline` | `#807c75` (new) | outlines of checkboxes, radios and the quality slider's ticks (non-text, needs 3:1) | 4.0 / 3.7 / 3.0 |

Rule: inside a selected row (`--selected`), or on a control's hover
(`--control-hover`), anything that would be `--text-3` uses `--text-2`
instead. `--text-3` measures only 3.8:1 there.

### 2.3 Accent (copper)

| Token | Value | Use |
|---|---|---|
| `--accent` [`--copper`] | `#b4683f` | strokes and fills without text: selection box, handles, row marker, focus ring, switch on, checkbox and radio on, drop line, guides. 4.0:1 on panel; non-text, so 3:1 is enough |
| `--accent-hover` [`--copper-hi`] | `#c4774c` | hover of the strokes above; the ISO toggle when on |
| `--accent-text` | `#d38d63` (new) | copper text and icons that mean "on" or "chosen": PREVIEW tag, the selected row's kind icon, the settings tab icon, menu check marks, Studio Mode checked, links. ≥4.5:1 on every surface up to `--selected` |
| `--accent-fill` | `#a65c35` (new) | primary button background, the Transition button, the active onboarding step dot |
| `--accent-fill-hover` | `#ae623a` | its hover |
| `--accent-fill-active` [`--copper-lo`] | `#9a5430` | its pressed state |
| `--on-accent` [`--text-on-accent`] | `#ffffff` (was `#fff9f2`) | text on `--accent-fill`: 5.0 / 4.6 / 5.7:1. The old `#fff9f2` on `#b4683f` was 4.0:1 and failed |
| `--accent-soft` [`--copper-soft`] | `rgba(180, 104, 63, 0.16)` | the background of a checked toggle button (Studio Mode), the info callout |

`--copper-glow` is removed. Focus has no halo (see 2.10).

### 2.4 Status colours

| Meaning | Strokes and dots | Text and icons | Filled (white text) | Soft background |
|---|---|---|---|---|
| On air / program / success | `--onair` [`--verdigris`] `#3e8e7e` | `--onair-text` [`--verdigris-hi`] `#56a996` (6.0:1) | `--onair-fill` `#2f7365` (5.6:1) | `--onair-soft` [`--verdigris-soft`] `rgba(62,142,126,.16)` |
| Recording / live / error / destructive | `--danger` [`--red`] `#e5484d` | `--danger-text` [`--red-hi`] `#ec5d61` (5.0:1) | `--danger-fill` `#cf3a3f` (4.9:1), hover `#d63d42`, pressed `#c4363b` | `--danger-soft` [`--red-soft`] `rgba(229,72,77,.14)` |
| Warning / lagging / reconnecting | `--warning` [`--amber`] `#d6a03f` | `--warning` (7.1:1) | `--warning` with `--on-warning` `#1b1408` text (7.8:1) | `--warning-soft` `rgba(214,160,63,.14)` |

Today's red-filled Stop buttons put white on `#e5484d`, which is 3.9:1 and
fails; they move to `--danger-fill`.

The brand constants `--slate #2e3338`, `--limestone #c9c2b6` and
`--bone #eeeae2` stay for the brand mark, the onboarding pictures and the
slider thumbs.

### 2.5 Type

The system font (SF Pro) is used everywhere. Instrument Sans is used only
for the two brand titles: Welcome and the first-run step titles. Number
columns, timers, dB values and FPS get `font-variant-numeric:
tabular-nums`.

| Role | Size / line | Weight | Colour | Where |
|---|---|---|---|---|
| Display (brand) | 24 / 30, `letter-spacing: -0.01em`, `--font-brand` | 600 | `--text` | `.state-card h1`, `.ob-body h1` |
| Lead | 14 / 21 | 400 | `--text-2` | `.state-card p`, `.ob-lead` |
| Heading | 13 / 18 | 600 | `--text` | `.dialog-heading`, `.ob-row-title`, `.toast .title`, `.callout .title`, `.service .name`, `.choice .name` |
| Body | 13 / 18 | 400 | `--text` | list rows, menu items, mixer strip names, dialog prose |
| Control | 12 / 16 | 500 buttons, 400 fields | `--text` | every `.btn` (all sizes except large), field values, select text, segmented, the source toolbar |
| Label | 12 / 16 | 400 | `--text-2` | `.form-label`, `.prop-label`, `.set-label`, `.dock-form-label`, `.tb-label` |
| Panel title | 12 / 16 | 600 | `--text-2` | `.dock-title`, `.dialog-title`, `.set-group-title`, `.filter-group-title`, `.form-section`, `.ob-label` |
| Caption | 12 / 16 | 400 | `--text-3` | hints (`.set-hint`, `.prop-hint`, `.field-note`, `.ob-row-hint`), `.list-empty`, `.dialog-note` |
| Meta | 11 / 14 | 400 (600 for column heads) | `--text-2` | status bar, `.unit`, `.prefix`, `kbd`, `.audio-row.head`, `.menu-item .hint`, `.ob-meta`, `.quality-label` |
| Tag | 11 / 14, uppercase, `letter-spacing: 0.06em` | 600 | role colour | `.monitor-head .tag` (was 700) |
| Meter scale | 10 / 10 | 400 | `--text-3` | `.meter .scale` only |

Tokens: `--fs: 13px; --lh: 18px; --fs-sm: 12px; --lh-sm: 16px; --fs-xs:
11px; --lh-xs: 14px; --fs-micro: 10px; --fs-lead: 14px; --lh-lead: 21px;
--fs-display: 24px; --lh-display: 30px`. Only weights 400, 500 and 600
are used; no 700. Text is sentence case except the tags.

### 2.6 Spacing

`--space-1: 4px; --space-2: 8px; --space-3: 12px; --space-4: 16px;
--space-5: 20px; --space-6: 24px; --space-8: 32px`.

| Rule | Value |
|---|---|
| `--pane-x`: the horizontal inset of everything inside a dock, the source toolbar and the status bar | 12px (was 10) |
| Icon to text, inside rows, buttons and menu items | 8px (was 6) |
| Between controls on one line | 8px |
| Between icon buttons in a toolbar | 2px (was 1) |
| Between form rows | 8px |
| Between settings groups | 24px |
| Dialog content padding | 16px top and bottom, 20px sides |
| Settings page padding | 16px top, 24px sides and bottom |
| Label column | `--label-col: 168px`, gap `--label-gap: 12px` (was 170/180/120 with gaps of 12/14) |
| A hint under a control | 4px below it, indented `--label-col + --label-gap` = 180px |

Values to remove (each becomes the nearest step): 5, 7, 9, 10, 14, 18 and
22 px, in `gap`, `padding` and `margin` across `app.css`. A 6 px value
stays only inside small elements (principle 4); elsewhere it becomes 4 or
8, as listed in P2.3.

### 2.7 Sizes

| Token | Value | What |
|---|---|---|
| `--control-h` (renamed from `--control`) | 28px | default button, field, select, segmented |
| `--control-h-sm` | 24px (new) | toolbar buttons and fields (source toolbar, dock toolbars, toast actions), quick transitions, trailing icon buttons in rows |
| `--control-h-lg` | 32px (new; `.btn.large` was 36) | onboarding Continue/Create, Welcome actions, the Transition button |
| `--row` | 28px (was 26) | dock lists, filter lists, pick lists, settings nav |
| `--menu-item` | 24px (was 26) | menu and submenu items |
| `--header` | 28px (was 26) | dock title band; dialog title bar (was 30) |
| `--toolbar` | 32px (was 30) | dock toolbars |
| `--source-toolbar` | 40px (was 38) | source toolbar |
| `--statusbar` | 24px | status bar |
| `--icon` | 16px | every icon, by default |
| `--icon-sm` | 14px | icons inside `.btn` labels, select chevrons, submenu chevrons, status-bar icons (was 13), the mixer's headphones flag (was 13) |
| `--icon-xs` | 12px | check marks in step dots and checkboxes |
| `--icon-lg` | 20px | Welcome choice glyphs, onboarding row glyphs |
| `--dot` | 8px | REC dot, the stream health dot (was a 10px rounded square), the program dot in a scene row (was 7), the state dot in a source row |
| icon buttons | 28 (`.icon-btn`), 24 (`.icon-btn.small`) | the 22 px `.icon-btn.tiny` is removed; rows use `.small` |
| field widths | `--field-xs: 96px; --field-s: 120px; --field-m: 240px; --field-l: 320px` | replace the inline `style="width: 96/100/120/140/220/240/300px"` in `settings.js` (96, 100 → xs; 120, 140 → s; 220, 240 → m; 300 → l) |

Icons are Lucide, drawn by `Icon` at a constant 1.5 px stroke at every
size (already true, `ui/ui/icons.js`). Allowed sizes are 12, 14, 16 and 20
only. At rest an icon is `--text-2`; on hover `--text`; when on,
`--accent-text`; for a status, its status colour.

### 2.8 Radius, lines, elevation

- `--r: 2px` for controls (buttons, fields, checkboxes, kbd, handles 1px).
  `--r-panel: 3px` for floating surfaces (menus, popovers, dialogs, toasts,
  the onboarding and welcome cards). Round things stay round: switches,
  radios, dots, slider thumbs. This is unchanged from Ryan's approval.
- Hairlines are always 1px `--line`. They sit between docks, under a dock
  header, above a dock toolbar, above the docks, above the status bar,
  around the monitor, between dialog regions and between settings groups.
- `--shadow-pop: 0 12px 32px rgba(0,0,0,.45), 0 2px 6px rgba(0,0,0,.3)` is
  used only on menus, popovers, dialogs, toasts and the welcome and
  onboarding cards (unchanged). No other shadows: the slider thumbs lose
  theirs and get a 1px `--surface-monitor` ring instead.

### 2.9 States

| State | Rows / list items | Buttons (secondary) | Icon buttons | Fields | Menu items |
|---|---|---|---|---|---|
| Rest | transparent, `--text` | `--control` bg, 1px `--line-strong` border | transparent, `--text-2` | `--surface-inset` bg, 1px `--field-border` | transparent, `--text` |
| Hover | `--hover` | `--control-hover` | `--control-hover` bg, `--text` | border `--field-border-hover` | `--overlay-hover` |
| Pressed | `--control-hover` | `--control-active` | `--control-active` | — | `--control-active` |
| Selected | `--selected` (g9, 1.33:1 over hover) + `box-shadow: inset 3px 0 0 var(--accent-hover)` (3.1:1 on `--selected`); kind icon `--accent-text`; dim text goes to `--text-2` (4.7:1) | — | — | — | — |
| Selected + hover | unchanged (`--selected-hover` = `--selected`) | — | — | — | — |
| On / checked | — | `--accent-soft` bg, 1px `--accent` border, `--accent-text` label | `--accent-text` (or `--accent-hover` for ISO) | — | check icon `--accent-text` |
| Focus (keyboard) | `box-shadow: var(--focus-inset)` | `box-shadow: var(--focus)` | `box-shadow: var(--focus)` | border `--accent` + `0 0 0 1px var(--accent)` | `--overlay-hover` (same as hover) |
| Disabled | `opacity: .4` | `opacity: .4` | `opacity: .4` | `opacity: .4` | colour `--text-disabled`, icons too |
| Invalid | — | — | — | border `--danger` | — |

Every disabled control uses one value, 0.4. Today it is 0.4 on icon
buttons, 0.45 on buttons, quality and onboarding choices, and 0.5 on
fields, pick lists and locked forms.

### 2.10 Focus

- `--focus: 0 0 0 1px var(--surface-panel), 0 0 0 3px var(--focus-ring)`
  is an outer ring with a 1 px gap on buttons, icon buttons, switches,
  checkboxes, radios, the quality thumb and links. `--focus-ring` is
  `--bone` (13.9:1 on the panel). A copper ring with no gap vanished on
  copper and red fills (1.2:1), so the ring is neutral and the gap
  separates it from any fill. (Revised after review, 2026-10-12.)
- `--focus-inset: inset 0 0 0 2px var(--focus-ring)` is for things that
  fill their container's width or are clipped by it: rows, settings tabs,
  pick lists, onboarding choices, services, segmented buttons.
- A selected item that has focus shows both: `inset 3px 0 0
  var(--accent-hover), var(--focus-inset)`.
- The focus rules come last in `app.css`, so they win over selected
  states that also set `box-shadow`.
- Fields show focus on the field: `border-color: var(--accent);
  box-shadow: 0 0 0 1px var(--accent)`; the input inside never gets its
  own ring (WebKit marks text inputs `:focus-visible` even on a click).
  This copper box is "editing", not keyboard focus.
- Menu items show focus as hover (`--overlay-hover`).
- Only `:focus-visible` shows a ring; a mouse click doesn't.

### 2.11 Motion

`--ease: cubic-bezier(0.2, 0.8, 0.2, 1)`, `--fast: 120ms` (hover colour)
and `--med: 160ms` (was 180). These cover menus, popovers and dialogs
fading in, a switch's knob, and toasts.

Menus and dialogs fade only; they don't move, so a submenu lands where it
is placed. Popovers and toasts fade and slide 4px. Nothing animates size or
layout.

Add a reduced-motion rule:

```css
@media (prefers-reduced-motion: reduce) {
  *:not(.spinner):not(.opening-dot), *::before, *::after {
    animation-duration: 1ms !important;
    transition-duration: 1ms !important;
  }
}
```

---

## 3. Components

Each component lists its anatomy, exact values and the selectors that
implement it.

### 3.1 The window

From top to bottom:

1. preview area, `--surface-stage`, padding 8px;
2. source toolbar, 40px, `--surface-panel`, `border-top: 1px --line`;
3. row splitter (0 px, invisible 7 px grab area);
4. docks, `border-top: 1px --line`;
5. status bar, 24px, `--surface-chrome`, `border-top: 1px --line`.

Docks are separated by `.dock-splitter` (1px `--line`). Nothing else has a
border.

### 3.2 Dock header ("panel header")

`.dock-title` is 28px tall:

- background `--surface-header` (`#17191d`);
- padding `0 var(--pane-x)`;
- 12/16 weight 600 `--text-2`, in OBS's words ("Scenes", "Sources", "Audio
  Mixer", "Scene Transitions", "Controls");
- `border-bottom: 1px solid var(--line)` (was `--line-soft`, which barely
  showed).

Across the dock row the header bands line up into one continuous strip, cut
only by the 1px dock splitters. That gives the tidy, tabbed-panel feel
without tabs or boxes.

Alignment rule: the header title, the first icon or letter of every row,
the mixer strip names, the transitions form and the Controls buttons all
start at `--pane-x` (12px) from the dock's left edge.

### 3.3 Dock toolbar

`.dock-toolbar` is 32px tall:

- padding `0 8px`, so a 24px `.icon-btn.small` with its 16px glyph inset by
  4px puts the glyph at 12px, in line with the rows;
- `gap: 2px`;
- `border-top: 1px solid var(--line)` (was `--line-soft`);
- background `--surface-panel`;
- `.toolbar-gap` is 8px.

This applies to the Scenes, Sources, Audio Mixer and Filters dialog lists.

### 3.4 List rows

`.row` is `--row` (28px) tall:

- padding `0 4px 0 var(--pane-x)`, `gap: 8px`;
- 13/18 `--text`;
- kind icon 16px `--text-2`;
- trailing buttons `.icon-btn.small` (24px), their icons `--text-2`.

States are in 2.9: hover `--hover`; selected `--selected` with the 2px
inset `--accent-hover` bar and the kind icon in `--accent-text`; hidden items' label
and icon in `--text-3`, or `--text-2` when selected; dragging `opacity:
.4`.

- `.row input.rename` is 24px tall, `margin-left: -5px`, `padding: 0 4px`,
  with a 1px `--accent` border and `--surface-inset` background.
- `.program-dot` is `--dot` (8px) in `--onair-text`.
- `.iso-toggle` is hidden until row hover; when on it is `--accent-hover`.
- `.drop-line` is 2px `--accent` (unchanged).
- Group indent in `sources.js` (inline `paddingLeft`) is
  `12 + depth * 16` px (was `10 + depth * 16`).
- `.list-empty` is padding `8px var(--pane-x)`, caption type.

The same row rules cover:

- `.filter-list .row` in Filters;
- `.pick` in Create/Select Source (28px, padding `0 var(--space-2)`);
- `.set-tab` in Settings: 32px, kept taller as a navigation list, padding
  `0 16px`, icon 16px, selected `--selected` + bar + icon in
  `--accent-text`;
- `.service` rows in Stream settings, which get no bar since they are
  actions, not a selection.

### 3.5 Buttons

There are four variants, all `--r` radius, 12/16 weight 500, icon
`--icon-sm` (14px) with an 8px gap, `white-space: nowrap`.

| Variant | Class | Height / padding | Rest | Hover | Pressed |
|---|---|---|---|---|---|
| Secondary (default) | `.btn` | 28 / `0 12px`; `min-width: 80px` in dialog footers | `--control`, 1px `--line-strong`, `--text` | `--control-hover` | `--control-active` |
| Primary | `.btn.primary` | same, no border (1px `transparent`) | `--accent-fill`, `--on-accent` | `--accent-fill-hover` | `--accent-fill-active` |
| Quiet | `.btn.ghost` (keep the name) | same | transparent, no border, `--text-2` | `--hover` bg, `--text` | `--control-hover` |
| Destructive / live | `.btn.control.active` | same | `--danger-fill`, 1px `--danger-fill`, `#fff` | `#d63d42` | `#c4363b` |

- `.btn.small` is 24px tall, padding `0 8px`, icon 14px. Use it in the
  source toolbar, toast actions and dialog helper buttons.
- `.btn.large` is 32px tall, padding `0 16px`, 13/18 weight 500. Use it in
  onboarding, Welcome and the full-window states.
- The checked toggle button (`.btn.control.checked`, Studio Mode on) uses
  `--accent-soft` bg, 1px `--accent` border and `--accent-text` label.
- Warning-live (`.btn.control.active.warn`, reconnecting) uses `--warning`
  bg with `--on-warning` text.

There is one primary button per surface: OK, Close, Continue, Create
setup, Transition or Try again. Everything else is secondary or quiet.

Today some secondary buttons have borders and some don't:

- bordered: the dialog footer (`.dialog-extra .btn, .dialog-buttons
  .btn:not(.primary)`) and `.btn.control`;
- borderless: `.btn` in Browse, "Add a border", "Add a shadow", "Add a
  background", Mark/Split, `.btn.quick` and `.btn.small` in the source
  toolbar.

Fix this by giving the border to `.btn` itself and deleting the
dialog-only rule. The source toolbar's Properties and Filters become
`.btn.ghost.small` (quiet, icon + label), as pro-app toolbars do. Inline
button styles go:

- `fields.js:58` and `fields.js:154` (`style="padding: 0 8px"`) become
  `class="btn ghost small"`;
- `states.js:126` (`style="height:24px; padding:0 8px"`) becomes
  `class="btn ghost small"`.

`.icon-btn` is 28×28 and `.icon-btn.small` 24×24, both transparent with
`--text-2` icons. Hover is `--control-hover` with a `--text` icon; pressed
is `--control-active`; on is `--accent-text`. Remove `.icon-btn.tiny`
(22px) and use `.small` in its places: `sources.js` row trailing buttons,
`filters.js:74` and `.row .group-toggle` (which becomes 16×24 in the icon
lane).

### 3.6 Fields

`.field` is 28px (24px in the source toolbar):

- `--surface-inset` bg, 1px `--field-border` (`#383e45`, was `--line`),
  `--r`;
- input padding `0 8px`, value 12/16 `--text`, placeholder `--text-3`;
- states as in 2.9;
- `.unit` and `.prefix` use meta type in `--text-3`; the prefix is 600 with
  `padding-left: 8px`;
- the select chevron is 14px `--text-2` (was `--text-3`) at `right: 8px`,
  and the select's `padding-right` is 28px;
- the colour field's `.color-swatch` is 16×16 (was 20) with `margin-left:
  6px` and the same inset ring;
- `.field.textarea textarea` has padding `6px 8px`, 12/16 type and
  `min-height: 64px`.

Number fields fill the value column only up to `--field-m`:
`.prop > .prop-value > .field.number` becomes `flex: 0 1
var(--field-m)` (240px, was 220).

### 3.7 Checkbox, radio, switch

These three share one look. All native checkboxes become custom:

- `add-source.js:213`;
- `audio-props.js:53`, `:62`, `:64`;
- `settings.js:543`.

One rule, `input[type="checkbox"]`, covers them all:

```css
input[type="checkbox"] {
  -webkit-appearance: none; appearance: none; margin: 0;
  width: 14px; height: 14px; flex: none; border-radius: var(--r);
  border: 1px solid var(--outline); background: var(--surface-inset);
}
input[type="checkbox"]:checked {
  border-color: var(--accent); background: var(--accent)
    url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24' fill='none' stroke='white' stroke-width='3' stroke-linecap='round' stroke-linejoin='round'%3E%3Cpath d='M20 6 9 17l-5-5'/%3E%3C/svg%3E")
    center / 10px no-repeat;
}
input[type="checkbox"]:focus-visible { box-shadow: var(--focus); }
input[type="checkbox"]:disabled { opacity: .4; }
```

WebKit doesn't draw `::after` on inputs, hence the data-URI check, which
is inline and adds no dependency.

- **Radio.** `.radio input[type=radio]` and `.ob-radio` are 14px circles
  with a 1px `--outline` border (was 1.5px `--line-strong`, about 1.6:1
  and hard to see). Checked: `border: 4px solid var(--accent); background:
  var(--bone)` (unchanged).
- **Switch.** `.switch` stays 28×16. Off: `--line-strong` track with a
  `--bone` knob. On: `--accent` track. The knob is 12px and moves over
  `--med`.

### 3.8 Segmented control

`.segmented` is 28px:

- `--surface-inset` bg, 1px `--field-border`;
- buttons 12/16 `--text-2`, padding `0 12px`;
- 1px `--line` dividers;
- (revised after review: the on segment is `--accent-soft` with `inset 0 0 0 1px var(--accent)`, like Studio Mode on, and the button has `aria-pressed`; a grey step was too close to hover) the on segment gets `--segment-on` (`#383e45`, was `#2c3137`, which was
  barely different from off) with a `--text` label;
- hover on an off segment turns its label `--text`.

### 3.9 Sliders

Sliders are neutral. Copper is kept for state, and a slider's value is
not a state.

- `.slider` is 20px tall.
- `.track` is 2px (was 4) `--line-strong`, `top: 9px`.
- `.fill` is 2px `--text-3`.
- `.thumb` is a 12px `--bone` circle with `box-shadow: 0 0 0 1px
  var(--surface-monitor)` and no drop shadow.
- **Bipolar sliders** (Balance in Advanced Audio): the fill runs from the
  centre to the thumb. Add `origin` to `Slider` in `ui/ui/controls.js`
  (`origin = min`). The fill's `left` and `width` are
  `min(f0, f)` and `|f − f0|`, where `f0` is the fraction for `origin`;
  `audio-props.js:55` passes `origin={0}`.
- **The mixer fader** (`.strip-controls .slider`) is the same component
  with the same neutral look, so the mixer no longer shows a row of copper
  bars.
- **The quality slider** (`.quality-*`) keeps its five stops:
  - rail 2px `--line-strong`, fill 2px `--text-3`;
  - ticks 8px; off ticks `--outline`, ticks at or below the value
    `--text-2`;
  - thumb 14px `--bone` with a 1px `--surface-monitor` ring;
  - Custom: a hollow thumb, 2px `--bone` border;
  - the chosen label in `--text` weight 600.

### 3.10 Menus and popovers

`.popover` uses:

- `--surface-overlay`, 1px `--line-strong`, `--r-panel`;
- padding 4px (was 6);
- `--shadow-pop`;
- `min-width: 200px` for popovers, 180px for menus.

`.menu-item` is 24px:

- padding `0 12px 0 8px`, `gap: 8px`, 13/18 `--text`, `--r` corners;
- the mark lane (check or icon) is 16px; the check icon is
  `--accent-text`;
- other icons `--text-2`;
- `.hint` (shortcut) is meta type `--text-3` with `padding-left: 24px`;
- the submenu chevron is 14px `--text-3`.

Hover and the active (submenu open) state are `--overlay-hover` with
`--text`. This replaces the full copper fill, the loudest repeated element
today: every hovered menu row turned solid copper. Icons and hints go
`--text` on hover.

- Danger items use `--danger-text`; on hover, `--danger-soft` background
  with `--danger-text`, not a red fill with white.
- Disabled items use `--text-disabled`.
- `.menu-sep` is 1px `--line`, `margin: 4px 0` (was `6px 4px`).
- `.menu-label` is meta 600 `--text-3`, padding `8px 8px 4px`.

Details popovers (`.details`, REC/LIVE) are 420px wide, with `.kv` and
`.file-row` rows 24px tall, 12/16 type, and dim labels in `--text-2`.

### 3.11 Dialogs and forms

`.dialog` uses `--surface-panel`, 1px `--line-strong`, `--r-panel` and
`--shadow-pop`.

- **Title bar.** `.dialog-title` is 28px (was 30), `--surface-chrome`,
  centred panel-title type, `border-bottom: 1px --line`. Centred, like a
  macOS window title, as OBS's dialogs are.
- **Footer.** `.dialog-foot` is 52px with padding `0 16px` (was 14),
  `border-top: 1px --line`. Helper buttons (Defaults, Reset) go on the
  left as `.btn`; the actions go on the right, `.btn` then `.btn.primary`,
  `gap: 8px`, `min-width: 80px`.
- **Scrim.** `.dialog-scrim` is `rgba(8,9,10,.5)` and fades in.
- **Forms.** `.form` has padding `16px 20px` and `gap: 8px`.
  - `.form-row` and `.prop` use `grid-template-columns: var(--label-col)
    minmax(0, 1fr)` with `gap: var(--label-gap)`.
  - Labels are right-aligned, label type `--text-2`.
  - `.prop-hint` sits under its control with `margin: 0 0 0 180px` (was
    `-2px 0 0 182px`).
  - `.sub-form` uses `margin-left: 180px`, `padding-left: 12px`, a 1px
    `--line` left border, columns `120px 1fr`, and hints at
    `margin-left: 132px`.
  - `.form-sep` is a 1px `--line`, `margin: 8px 0`.
  - `.form-section` ("Appearance" in Edit Transform) uses the
    settings-group pattern (3.12): panel-title type on the left, then a
    1px `--line` filling the rest of the line, with 8px above it. It
    replaces the `.form-sep` before it.
- **Callouts.** `.callout` is a full-width band, `padding: 12px 20px`,
  12/16 type, icon 16px. The error type uses `--danger-soft` with a
  `--danger-text` icon; info uses `--accent-soft` with an `--accent-text`
  icon.
- **Properties.** The source preview sits on `--surface-stage` with a
  12px inset and a `--line` hairline under it; only the settings scroll
  (unchanged).
- **Filters.** The left column is 248px (was 250) with `--line` borders.
  `.filter-group-title` uses panel-title type with padding `12px 12px
  8px`. Each list has its dock toolbar (3.3).

### 3.12 Settings

`.set-nav` is 180px on `--surface-chrome`, padding 4px 0. Its tabs are
described in 3.4.

`.set-body` has padding `16px 24px 24px` and a 24px gap between groups.

`.set-group` is a legend in panel-title type followed by a 1px `--line`
filling the rest of the line (unchanged pattern), with `padding-top: 8px`.

`.set-row` uses columns `var(--label-col) 1fr` with a 12px gap and
padding `4px 0`. Two changes:

- `.set-label` changes from `--text` to `--text-2` to match the dialogs'
  labels. Today Settings labels are bright and Properties labels dim.
- `.set-hint` uses caption type.

Inline widths become classes (`.w-xs`, `.w-s`, `.w-m`, `.w-l` →
`--field-xs` … `--field-l`) in `settings.js`.

`kbd` is 20px tall with a min-width of 20, padding `0 6px`, meta type 500,
`--control` bg, 1px `--line-strong` and `--r`. It has no inset shadow, so
it is flat.

### 3.13 Toasts

`.toast` uses `--surface-overlay`, 1px `--line-strong`, `--r-panel`,
`--shadow-pop` and padding 12px. They stack top right with a 12px margin
and 8px gap, 360px wide.

- Icon 16px in the status colour (info `--text-2`).
- Title in heading type; message 12/16 `--text-2`.
- Rows in 12/16 tabular, values right-aligned and coloured by status.
- Actions are `.btn.ghost.small` and `.icon-btn.small`.

### 3.14 Tooltips

Tooltips are native: `title` attributes, drawn by AppKit, so they follow
macOS and cost nothing. Don't build custom tooltips; they would need
positioning work on hot paths. Write them in sentence case, under about 80
characters, and say what the control does.

### 3.15 Scroll bars

The bar is 8px with a transparent track. The thumb is
`rgba(238,234,226,.14)` with a 2px transparent border (an effective 4px
thumb), fully rounded, and `--field-border-hover` (`#4a5058`) on hover.
Today it is 0.10 and 0.18.

### 3.16 Audio Mixer and meters

`.strip` has padding `8px var(--pane-x) 4px`, `gap: 4px`, with strips
separated by 1px `--line-soft`.

- `.strip-head` is 20px: name in body type `--text` weight 400 (was 500),
  dB in 12/16 tabular `--text-2`, "not in mix" in meta `--text-3`, the
  headphones flag 14px `--accent-text`, and the failed badge 16px
  `--danger-text`.
- `.meter` is 22px. The bar is 8px tall with a 1px radius (was 2).
- The scale sits under the bar (meter-scale type) with ticks every 12 dB.
  The minor and mid ticks hide in narrow strips (unchanged).
- Meter drawing in `ui/ui/mixer.js` `paint()`:
  - The unlit track is neutral `#22262b` (`--surface-overlay`), not the
    three zone tints at 14%, which drew a muddy olive/brown/red band under
    every idle meter.
  - Lit colours are unchanged: `rgb(62,142,126)` below −18, `rgb(214,160,63)`
    from −18 to −6, `rgb(229,72,77)` above −6.
  - Peak is the zone colour at 0.5 alpha; the hold line is 2px.
  - Read the colours from CSS once (`getComputedStyle(document.documentElement)`
    at start-up, not per frame), so they come from tokens:
    `--meter-ok`, `--meter-warn`, `--meter-clip`, `--meter-track`.
- `.strip-controls` is 24px: the fader (3.9), mute `.icon-btn.small`
  (`--danger-text` when muted) and the ⋮ menu.
- A muted strip shows its name and dB in `--text-3` and the meter at 0.35
  opacity (unchanged).

### 3.17 Preview, monitors and canvas overlay

The preview area is `--surface-stage` with 8px padding. The monitor has a
1px `--line` ring. It gets an `--onair` ring when it shows the program
while live (unchanged).

Studio Mode:

- `.monitor-head` is 26px: the tag in tag type (PREVIEW `--accent-text`,
  PROGRAM `--onair-text`) and the name in 12/16 weight 600 `--text`.
- `.studio-column` is 148px with a gap of 8px.
- The Transition button is `.btn.primary` at `--control-h-lg` (32px, was
  34) with 12/16 weight 600 type.
- `.quick-transitions .quick` are `.btn.small` (24px), 12/16 (was 11px),
  with a gap of 4px.
- The gear is an `.icon-btn`.

Canvas overlay (`ui/ui/canvas.js`, hot path, CSS only):

- `.item-box.selected` uses `box-shadow: 0 0 0 1px var(--accent), 0 0 0
  2px rgba(0,0,0,.5)`. The outer dark ring keeps the copper edge visible
  on bright pictures, where copper on red or orange vanished. The inset
  dark line goes.
- `.handle` is 8×8, `--r` 1px (was 2), `--accent` fill, 1px
  `--surface-monitor` border. In crop mode the fill is `--bone`
  (unchanged).
- Hover outline: `inset 0 0 0 1px rgba(238,234,226,.55)` (unchanged).
- Guides are 1px `--accent` (unchanged).
- `.drag-readout` uses `rgba(11,12,13,.85)`, `--r`, padding `2px 6px`,
  meta type, tabular.
- **Empty scene (`.canvas-empty`).** Give it `background:
  var(--surface-monitor)` so the message never sits on an old frame. The
  monitor can keep showing the last picture after a switch to an empty
  scene; snapshot 64 shows "Nothing in this scene yet" unreadable over the
  test pattern. Text is caption type in `--text-2`, and the button is
  `.btn` (secondary).

### 3.18 Source toolbar

`.source-toolbar` is 40px, `--surface-panel`, `border-top: 1px --line`,
padding `0 var(--pane-x)`, `gap: 8px`, control type.

- The name (`.tb-source`) is 13/18 weight 600 `--text` with a 16px
  `--text-2` icon.
- Properties and Filters are `.btn.ghost.small`.
- `.tb-sep` is 1px × 16px `--line-strong` with `margin: 0 4px`.
- The quick setting has a label `.tb-label` (label type) and its field at
  24px.
- With nothing selected, "No source selected" is caption type `--text-3`.

### 3.19 Controls dock

`.controls` has padding `8px var(--pane-x)` and a gap of 4px (was 5).

`.btn.control` is the secondary button: 28px, 12/16 weight 500, full
width. Its active (Stop Streaming, Stop Recording) and checked (Studio
Mode) variants are in 3.5. Marker and Split are `.btn.control.icon-only`,
28×28.

At the 224 px minimum dock height (docs/notes.md) there are 224 − 1 −
28 = 195 px under the header. The buttons need 8 + 5×28 + 4×4 + 8 = 172
px, so Exit stays visible. Marker and Split share Stop Recording's row,
so recording adds no height.

### 3.20 Status bar

`.statusbar` is 24px, `--surface-chrome`, `border-top: 1px --line`,
padding `0 6px`, meta type `--text-2`, `gap: 16px` between the groups
(stream, REC, CPU, FPS). Items (`.status-item`) are 20px tall with padding
`0 6px`, `gap: 6px`, `--r`; clickable ones hover to `--hover` and `--text`.

- `.status-dot` and `.health` are both `--dot` (8px) circles. Health
  changes from a 10px rounded square to a dot: `--onair-text` ok,
  `--warning` warn, `--danger` bad, `--text-3` off.
- REC on shows a `--danger` dot with `--text` label and time. LIVE on shows
  a `--danger-text` icon with `--text` label and time.
- Dropped counts use `--danger-text`; lagging FPS uses `--warning`.

### 3.21 Scene Transitions dock

`.dock-form` has padding `8px var(--pane-x)` and a gap of 8px.
`.dock-form-label` is label type, 64px wide (was 58). The select and the
gear sit on one line with an 8px gap. Duration is a number field with an
"ms" unit; it is disabled (0.4) for Cut.

### 3.22 First run, Welcome, full-window states

These are brand moments: Instrument Sans display titles, the brand mark,
and `--surface-panel` cards with `--r-panel` and a shadow over a dim
scrim.

- **`.ob-card`** is 760px. `.ob-head` is 64px with padding `0 16px 0
  24px`. `.ob-body` has padding `24px 32px 8px`. `.ob-foot` has padding
  `16px 24px`.
- **Step dots.** Step dots are 20px. Active: `--accent-fill` with
  `--on-accent`. Done: `--onair-soft` with an `--onair-text` 12px check.
  Upcoming: 1px `--outline`, `--text-3`.
- **`.ob-row`** is 64px minimum with a 1px `--line` between rows. Glyphs
  are 20px `--accent-text`. Allowed is `--onair-text`; Denied is
  `--danger-text`.
- **"Check again"** becomes `.btn.ghost.small`, aligned with the row text
  (`margin-left: 34px`, the glyph lane). Today it floats indented.
- **`.ob-choice`** (devices) is 36px minimum, padding `6px 12px`. Selected
  is `--selected` without a bar (the radio carries the state). Hover is
  `--hover`.
- **`.ob-layout`** tiles: selected is `--selected` with `box-shadow: inset
  0 0 0 1px var(--accent)`. The SVG pictures in `onboarding.js:349-353`
  read `--control`, `--line-strong`, `--text-3` and `--accent` through
  `var()` in `fill`/`stroke` instead of hex.
- **`.welcome-card`** is 680px. `.choice` tiles are 132px minimum with 1px
  `--line` between them. The primary choice name is `--accent-text`; glyphs
  are 20px `--accent-text`; hover is `--hover`.
- **`.state` (connecting, failed).** `.detail` uses `--surface-panel`, 1px
  `--line`, `--r`, mono 11/16. "Try again" is `.btn.primary.large`. The
  footnote is caption type, with the inline style in `states.js:27`
  removed.

---

## 4. Custom property map

Put the new block in `:root`. During migration, alias the old names to the
new ones (`--bg-panel: var(--surface-panel);`) so P1 can land as one
reviewable token change. Delete the aliases in P3.

| Today | Value today | Becomes | Value |
|---|---|---|---|
| `--copper` | `#b4683f` | `--accent` (keep `--copper` as the brand constant) | `#b4683f` |
| `--copper-hi` | `#c4774c` | `--accent-hover` | `#c4774c` |
| `--copper-lo` | `#9f5a35` | `--accent-fill-active` | `#9a5430` |
| `--copper-soft` | `rgba(180,104,63,.16)` | `--accent-soft` | same |
| `--copper-glow` | `rgba(180,104,63,.32)` | removed (no focus glow) | — |
| — | — | `--accent-text` | `#d38d63` |
| — | — | `--accent-fill` / `-hover` | `#a65c35` / `#ae623a` |
| `--verdigris` / `-hi` / `-soft` | `#3e8e7e` / `#56a996` / `.16` | `--onair` / `--onair-text` / `--onair-soft` | same |
| — | — | `--onair-fill` | `#2f7365` |
| `--red` / `--red-hi` / `--red-soft` | `#e5484d` / `#ec5d61` / `.14` | `--danger` / `--danger-text` / `--danger-soft` | same |
| — | — | `--danger-fill` / `-hover` / `-active` | `#cf3a3f` / `#d63d42` / `#c4363b` |
| `--amber` | `#d6a03f` | `--warning` (+ `--warning-soft` `rgba(214,160,63,.14)`, `--on-warning` `#1b1408`) | same |
| `--slate`, `--limestone`, `--bone` | — | brand constants, unchanged | — |
| `--bg-monitor` | `#0b0c0d` | `--surface-monitor` | same |
| `--bg-stage` | `#111315` | `--surface-stage` | same |
| `--bg-app` | `#141619` | `--surface-chrome` | same (native `BACKGROUND` unchanged) |
| `--bg-field` | `#15171a` | `--surface-inset` | `#141619` |
| — | — | `--surface-header` | `#17191d` |
| `--bg-panel` | `#1b1e22` | `--surface-panel` | same |
| `--bg-popover` | `#22262b` | `--surface-overlay` | same |
| `--bg-raised` | `#24282d` | `--control` | `#262a30` |
| `--bg-hover` | `#262a30` | `--hover` | same |
| `--bg-active` | `#2c3137` | `--control-hover` (also `--overlay-hover`) | `#30353b` |
| — | — | `--control-active`, `--segment-on`, `--field-border` | `#383e45` |
| — | — | `--field-border-hover` | `#4a5058` |
| `--sel` | `rgba(180,104,63,.30)` (`#49342b` on panel) | `--selected` (+ the 2px accent bar) | `#30353b` |
| `--sel-hover` | `rgba(180,104,63,.38)` | `--selected-hover` | `#383e45` |
| `--line` / `--line-strong` / `--line-soft` | `#2b3035` / `#383e45` / `.06` | same names | same; `--line-soft` only inside lists |
| `--text` | `#eeeae2` | `--text` | same |
| `--text-2` | `#a39e95` | `--text-2` | `#b0aba3` |
| `--text-3` | `#6f6b65` | `--text-3` | `#928e87` |
| — | — | `--text-disabled` / `--outline` | `#66625c` / `#807c75` |
| `--text-on-accent` | `#fff9f2` | `--on-accent` | `#ffffff` |
| `--fs` `--lh` `--fs-sm` `--fs-xs` | 13 / 18 / 12 / 11 | same + `--lh-sm 16px`, `--lh-xs 14px`, `--fs-micro 10px`, `--fs-lead 14px`, `--lh-lead 21px`, `--fs-display 24px`, `--lh-display 30px` | — |
| `--r` / `--r-panel` | 2 / 3 | same | same |
| `--row` | 26px | `--row` | 28px |
| `--control` (height) | 28px | rename to `--control-h` (so `--control` can be the colour) + `--control-h-sm 24px`, `--control-h-lg 32px` | 28px |
| `--source-toolbar` | 38px | same | 40px |
| `--statusbar` | 24px | same | 24px |
| — | — | `--header 28px`, `--toolbar 32px`, `--menu-item 24px`, `--pane-x 12px`, `--label-col 168px`, `--label-gap 12px`, `--dot 8px`, `--space-1…8`, `--field-xs…l`, `--icon 16px`, `--icon-sm 14px`, `--icon-xs 12px`, `--icon-lg 20px` | — |
| `--fast` / `--med` / `--ease` | 120 / 180 / curve | same / 160ms / same | — |
| `--shadow-pop` | — | same | — |
| `--focus` | `0 0 0 1px copper, 0 0 0 3px glow` | `--focus` / `--focus-inset` | `0 0 0 2px var(--accent)` / `inset 0 0 0 2px var(--accent)` |
| `mixer.js` `ZONES` RGB | hard-coded | `--meter-ok` `#3e8e7e`, `--meter-warn` `#d6a03f`, `--meter-clip` `#e5484d`, `--meter-track` `#22262b` | — |

Note the rename: `--control` is a height (28px) today. Rename the height
to `--control-h` first, mechanically, then introduce the colour
`--control`.

For Slint, the same names become one `global Theme` with typed
properties. For example `out property <color> surface-panel: #1b1e22;`,
`out property <length> row: 28px;`, `out property <duration> fast: 120ms;`.
Shadows map to `drop-shadow-*` on the floating windows only.

---

## 5. Change list

All "before" snapshots are 1440×900 at 2×, unless noted, in
`/Volumes/IsocastBuild/ui-polish/spec/shots/` on Ryan's Mac. The same
files are in `~/isocast-scratch/spec/shots/` on `isocast-m2-a`. Zoomed
crops are in `/Volumes/IsocastBuild/ui-polish/spec/crops/`. Section 6
says how to retake them.

Each item says what to change, where, and how to check it. Work in this
order; each item is one small commit.

### P1: must

**P1.1 Token block.** Add every token from section 2 to `:root` in
`app.css`, rename the height `--control` to `--control-h`, and alias the
old names (section 4).

- No visual change except the retuned values: `--text-2`, `--text-3`,
  `--bg-field`, `--bg-raised`, `--bg-active`, `--text-on-accent`.
- Before: `01-main.png`.
- Check: every snapshot still renders. After the rename, `grep -n
  'var(--control)' app.css` returns nothing until the colour token
  arrives.

**P1.2 Text contrast.** Apply `--text-3 #928e87` and `--text-2 #b0aba3`.
In selected rows and hovered buttons, dim text uses `--text-2`. Add these
rules (`color: var(--text-2)`):

- `.row.selected .faint`;
- `.row.selected.hidden-item .label`, `.row.selected.hidden-item
  .kind-icon`;
- `.ob-choice.on .ob-meta`.

This affects hints, units, placeholders, the meter scale, empty lists,
"No source selected" and status icons.

- Before: `20-settings-general.png` (hints 3.2:1), `01-main.png` (meter
  scale, the `ms` unit, the toolbar), `08-create-source.png`.
- Check: hint text measures ≥4.5:1 on `--surface-panel`; `--text-3` no
  longer appears on `--selected`.

**P1.3 Accessible filled buttons.** Change these rules:

- `.btn.primary` (and its `:hover`, `:active`) to `--accent-fill` /
  `--accent-fill-hover` / `--accent-fill-active` with `--on-accent`
  `#ffffff`. This affects OK, Close, Continue, Create setup, Transition and
  Try again.
- `.btn.control.active` to `--danger-fill` (hover `#d63d42`, pressed
  `#c4363b`), `#fff`.
- `.ob-steps li.on .ob-step-dot` to `--accent-fill`.

Before:

- `08-create-source.png` (OK at 4.0:1);
- `03-studio.png` (Transition);
- `50-recording.png` and `crops/rec-controls.png` (Stop Recording, white
  on `#e5484d` at 3.9:1);
- `44-streaming.png` (Stop Streaming);
- `30-onboarding-1.png`.

Check: all fill and text pairs are ≥4.5:1 (table in 2.3/2.4).

**P1.4 Neutral selection with an accent marker.**

- `.row.selected` becomes `--selected` with `box-shadow: inset 2px 0 0
  var(--accent-hover)`, and `:hover` becomes `--selected-hover`. The kind icon
  is `--accent-text`.
- Apply the same to `.set-tab.on` (icon `--accent-text`) and `.pick.on`.
- `.ob-choice.on` is `--selected` without the bar.
- `.ob-layout.on` is `--selected` with `inset 0 0 0 1px var(--accent)`.
- Drop `--sel` and `--sel-hover`.

Before:

- `01-main.png` and `crops/main-docks-left.png` (Scenes "Main", muddy
  brown `#49342b`);
- `02-main-selected.png` and `crops/selected-toolbar.png` (source row);
- `20-settings-general.png` (nav);
- `12-filters-clip.png` (filter row);
- `08-create-source.png` (a disabled pick list still brown);
- `31-onboarding-2.png`, `32-onboarding-3.png`.

Check: no brown fills remain (search for `--sel` → 0), and the selected
row is clearly brighter than a hovered one (`54-hover-row.png` vs
`02-main-selected.png`).

**P1.5 Menus.**

- `.popover`: padding 4px.
- `.menu-item`: 24px, padding `0 12px 0 8px`, gap 8px.
- `.menu-item:hover, .menu-item.active`: `--overlay-hover` + `--text`;
  icons, hints and chevrons `--text`. No copper fill.
- The check mark is `--accent-text`. In `MenuItems` (`popover.js`), pass
  `class="check"` to the check `Icon` (`Icon` already takes `class`, as
  the submenu chevron does) and style `.menu-item .check`.
- `.menu-sep`: `margin: 4px 0`.
- Danger: `--danger-text`; on hover, `--danger-soft` bg.
- `.menu-item[disabled]`: `--text-disabled`.

Before:

- `04-source-menu.png` and `crops/source-menu.png` (a solid copper row);
- `05-scene-menu.png` and `crops/scene-menu.png`;
- `06-canvas-menu.png` and `crops/canvas-menu.png`;
- `07-add-source-menu.png` and `crops/add-menu.png`;
- `43-mixer-menu.png`.

Check: open the source menu with Transform expanded. The hovered row is
grey and the submenu sits beside it.

**P1.6 Neutral sliders; bipolar Balance.** Section 3.9:

- `.slider .track` and `.fill` become 2px, the fill `--text-3`;
- `.thumb` gets a ring and no shadow;
- `Slider` in `controls.js` gains `origin`; `audio-props.js:55` passes
  `origin=${0}`;
- quality slider colours as in 3.9.

Before:

- `01-main.png` (copper faders);
- `15-adv-audio.png` (Balance filled from the left to the centre);
- `14-transform.png` (Opacity);
- `20-settings-output.png` (quality).

Check: at Balance 0 there is no fill; at +0.5 the fill runs from the centre
to the right. The mixer shows no copper.

**P1.7 One secondary button style.**

- `.btn` gets the 1px `--line-strong` border and `--control` bg; delete
  the rule `.dialog-extra .btn, .dialog-buttons .btn:not(.primary)`.
- `.btn.primary` gets `border-color: transparent`.
- `.btn.ghost` stays borderless.
- Source toolbar Properties/Filters become `btn ghost small`
  (`source-toolbar.js:107,110`).
- Replace the inline styles in `fields.js:58,154` and `states.js:126`
  with `btn ghost small`.
- `.btn.small` is 24px, padding `0 8px`, 12px type, icon 14px.

Before:

- `14-transform.png` (border-less "Add a border/shadow/background" next
  to a bordered Reset);
- `20-settings-output.png` (Browse vs Cancel);
- `02-main-selected.png` (toolbar chips);
- `51-rec-details.png` (Mark/Split);
- `42-toasts.png`.

Check: every button in every snapshot is one of the four variants.

**P1.8 Custom checkboxes and stronger radios.** Section 3.7: the
`input[type="checkbox"]` rule (the data-URI check), and radios with a 1px
`--outline` border.

- Before: `08-create-source.png` and `crops/create-source.png` ("Make
  source visible" is a native box, the radio rings barely visible);
  `15-adv-audio.png` (Mute, Program, ISO); `62-settings-output-iso.png`
  (ISO sources).
- Check: checked boxes are copper with a white tick; unchecked outlines
  are visible (4.0:1).

**P1.9 Panel headers and dock dividers.**

- `.dock-title`: 28px, `--surface-header`, `0 var(--pane-x)`, 12/16 600
  `--text-2`, `border-bottom: 1px solid var(--line)`.
- `.dock-toolbar`: 32px, padding `0 8px`, gap 2px, `border-top: 1px solid
  var(--line)`.
- `.row`: padding-left `--pane-x`.
- `.strip`, `.dock-form`, `.controls`, `.list-empty`: horizontal padding
  `--pane-x`.
- `sources.js:371` becomes `12 + depth * 16`.
- `.source-toolbar`: 40px, padding `0 var(--pane-x)`.

Before: `crops/main-docks-left.png`, `crops/main-docks-right.png` (header
and toolbar lines at 6% are nearly invisible; padding 10px).

Check: header bands line up across all five docks, and the glyphs in
lists, headers and toolbars all start at x = 12 within each dock.

**P1.10 One label style in forms.**

- `.set-label` becomes `--text-2` (was `--text`).
- `.form-row`, `.prop` and `.set-row` use `var(--label-col)
  var(--label-gap)` (168/12); `.set-row` gap 12 (was 14).
- `.prop-hint` gets `margin-left: 180px`.
- `.sub-form` as in 3.11.

Before: `20-settings-general.png` (bright labels, 180px column) next to
`09-props-clip.png` (dim labels, 170px column).

Check: labels in Settings, Properties, Filters and Edit Transform share
colour, size and right edge.

### P2: should

**P2.1 Type scale.**

- Every `.btn`: 12/16 weight 500, including dialog buttons, which are 13
  today.
- `.btn.large`: 13/18 weight 500 at 32px.
- `.quick-transitions .quick`: 12px (was 11).
- `.monitor-head .tag`: weight 600 (was 700).
- `.strip-head .name`: weight 400.
- `.status-item .icon` and `.strip-flag` icons: 14px (was 13).
- `.dialog-title`: 12/16 600.
- No other sizes than in 2.5. Every `font-size` uses a type token, so
  `grep -nE 'font-size: [0-9]+px|font-weight: 700' app.css` finds
  nothing (the meter scale uses `--fs-micro`).

Before: `03-studio.png` (11px quick buttons), `01-main.png`. Check: grep
finds no sizes outside the scale.

**P2.2 Row and control metrics.**

- `--row` 28, `--menu-item` 24, `--header` 28, `--toolbar` 32.
- Remove `.icon-btn.tiny`; use `.icon-btn.small` in
  `sources.js:398,404,408` and `filters.js:74`.
- `.row .group-toggle` is 16×24.
- `.row input.rename` is 24px.
- `.program-dot` is 8px.
- `.state-badge` is 24×24.

Before: `crops/main-docks-left.png`. Check: the 224 px minimum dock height
still shows Exit in Controls (`53-small-window.png` is 1024×700).

**P2.3 Spacing pass.** Replace the off-grid values in `app.css`:

| Selector | Property | Today | Becomes |
|---|---|---|---|
| `.row` | gap | 6 | 8 |
| `.controls`, `.control-row` | gap | 5 | 4 |
| `.status-item` | padding | 7 | 6 |
| `.dialog-foot` | padding | 14 | 16 |
| `.form`, `.create-source` | padding | 18 | 20 |
| `.set-body` | padding | 14 / 22 | 16 / 24 |
| `.set-row` | padding / gap | 5 / 14 | 4 / 12 |
| `.btn` | gap | 6 | 8 |
| `.tb-sep` | margin | 6 | 4 |
| `.tb-source` | margin-right | 6 | 4 |
| `.dock-form-label` | width | 58 | 64 |
| `.filters-side` | width | 250 | 248 |
| `.strip` | padding | `6px 10px 2px` | `8px 12px 4px` |
| `.toast` | padding | `10px 8px 10px 12px` | 12px |

Before: any snapshot. Check: `grep -nE '(gap|padding|margin)[^;]*\b(5|7|9|10|14|18|22)px' app.css`
returns nothing, and the remaining 6px values are all inside small
elements.

**P2.4 Fields and segmented.** Section 3.6 and 3.8:

- `.field` border `--field-border`, hover `--field-border-hover`;
- `:focus-within` becomes `border-color: var(--accent); box-shadow: 0 0 0
  1px var(--accent)` (no glow);
- `.select .chev` `--text-2`, `right: 8px`;
- `.color-swatch` 16px;
- `.segmented button.on` `--segment-on`, label `--text`.

Before: `20-settings-output.png` and `32-onboarding-3.png` (MOV/MP4 and
1080p…: the chosen segment is hard to see), `10-props-text.png`.

Check: the chosen segment is obvious at a glance; a focused field shows a
crisp 2px copper edge. Focus can't be captured off screen, because the
snapshot window is never key; check it in a real window on a test Mac.

**P2.5 Meters from tokens, neutral track.** In `mixer.js` `paint()`, the
unlit track is one `--meter-track` fill (no zone tints) and the bar radius
is 1. The lit colours read from CSS custom properties once at module load.
Keep the hot path allocation-free: no `getComputedStyle` per frame.

- Before: `crops/main-docks-right.png` (olive and red bands under idle
  meters).
- Check: an idle strip shows a plain dark bar; a loud tone lights green,
  then amber, then red.

**P2.6 Status bar.**

- `gap: 16px` between groups; `.status-item` padding `0 6px`, gap 6px.
- `.health` becomes an 8px circle using `--onair-text`, `--warning` and
  `--danger`.
- `.status-dot` is 8px.
- Icons are 14px.

Before: `crops/status-both.png` (`44-streaming.png`, `50-recording.png`:
the square health light next to round dots). Check: streaming and
recording snapshots show dots only.

**P2.7 Canvas overlay and the empty canvas.**

- `.item-box.selected` gets an outer dark ring.
- `.handle` radius 1.
- `.canvas-empty` gets `background: var(--surface-monitor)`, its text
  `--text-2`, and its button `.btn` (secondary).

Before: `crops/selected-canvas.png` (`02-main-selected.png`: the copper
edge is lost on the red bar) and `64-empty-scene.png` and
`crops/empty-canvas.png` ("Nothing in this scene yet" drawn over a stale
frame of the previous scene, unreadable).

Check: select the clip over the colour bars; the box is visible on every
colour. Switch to "Empty Scene"; the message sits on black.

**P2.8 Dialog chrome.**

- `.dialog-title` 28px; `.dialog-foot` padding `0 16px`; `.dialog-buttons
  .btn` `min-width: 80px`.
- The transition dialog with no settings (`transitions.js`, `fields` empty
  or Fade without options) shows only a primary Close, as Filters does.
  Today it shows "No settings." with Defaults, Cancel and OK.

Before: `16-transition.png`, `08-create-source.png`, `14-transform.png`.

**P2.9 Studio column.** The Transition button is 32px; quick transitions
are `.btn.small` at 12px with a gap of 4. Before: `03-studio.png`,
`63-studio-live.png`.

**P2.10 One disabled look.** Every `opacity: .45` and `.5` used for
disabled becomes `.4`:

- `.btn[disabled]`, `.field.disabled`, `.pick-list.disabled`,
  `.quality.disabled`, `.ob-choice[disabled]`, `.form.locked .form-row`,
  `.radio input:disabled`;
- `.menu-item[disabled]` uses `--text-disabled`.

Before: `08-create-source.png` (Add Existing), `20-settings-general.png`
(Apply). Check: grep `opacity: 0.4[5]|opacity: 0.5` returns only
non-disabled uses (`.strip.muted .meter canvas` stays 0.35).

### P3: nice

**P3.1 Reduced motion.** Add the media query from 2.11. Check: enable
"Reduce motion" on a test Mac; menus and toasts appear without animating,
and spinners still turn.

**P3.2 Remove the aliases.** Replace the old token names in every selector
and delete the aliases from `:root`. Check: `grep -E -- '--(bg-|copper|verdigris|red|amber|sel\b|sel-)' app.css`
finds only the brand constants.

**P3.3 Settings field widths.** Replace the inline `style="width:
…px"` in `settings.js` (lines 217, 246, 259, 267, 271, 278–279, 368, 495,
499, 525, 587, 596, 600, 620–625, 664, 668) with `.w-xs`, `.w-s`, `.w-m`
and `.w-l` (96/120/240/320; the mapping is in 2.7). Check: `grep -c
'style="width' settings.js` is 0.

**P3.4 Flat `kbd`.** 20px, border 1px `--line-strong`, no inset shadow
(3.12). Before: `20-settings-hotkeys.png`.

**P3.5 Scroll bar.** Thumb `.14`, hover `#4a5058` (3.15). Before:
`01-main.png` (the mixer scroll bar).

**P3.6 Onboarding details.**

- "Check again" becomes a quiet button aligned with the row text.
- Layout pictures use tokens (3.22).
- Step dots follow 3.22.

Before: `30-onboarding-1.png` and `crops/onb1.png`,
`32-onboarding-3.png`.

**P3.7 Docs.** In `docs/app.md`, point "one stylesheet of tokens (… the
mineral palette)" at this file. In `docs/notes.md`, add the dated decision
(neutral selection, grey menus, neutral sliders).

---

## 6. Snapshots: how they were made and how to retake them

The setup is `/Volumes/IsocastBuild/ui-polish/spec/setup.toml` (also
`~/isocast-scratch/spec/setup.toml` on `isocast-m2-a`): a 1920×1080 at 30
canvas with these sources:

- Backdrop (color);
- Test Pattern;
- Intro Clip (media, a generated `clip.mp4` with two filters);
- Old Intro (media with a missing file, the error state);
- Lower Third (text);
- Tone and Music Bed (tone, with a Gain filter on Tone).

Its scenes are Main, Bars, Clip Only and Empty Scene.

The run uses an isolated engine:
`ISOCAST_SOCKET=/tmp/isocast-spec.sock ISOCAST_SETUP=… ISOCAST_MONITOR_SILENT=1`.
The baseline UI is a `git archive` of a24edf5 served with `ISOCAST_UI_DIR`,
so other agents' edits don't leak into "before". The driver scripts are
`snap.sh`, `batch-a.sh`, `batch-b.sh` and `batch-c.sh`, with one `--eval`
file per view in `js/`, all in `/Volumes/IsocastBuild/ui-polish/spec/`. They are queued through the
test Mac's desktop bridge (`scripts/testmac-bridge.sh queue`).

Two caveats:

- Off-screen snapshot windows never become key, so focus rings don't show
  in them. Check focus in a real window on a test Mac.
- Program state persists between runs, so the views after
  `41-error-scene` show "Clip Only".

| Shot | View | How (`--eval` unless noted) |
|---|---|---|
| `01-main` | main window, idle | none |
| `02-main-selected` | canvas selection + source toolbar + selected row | `app.select("main","clip")` |
| `03-studio` | Studio Mode | `app.studio(); app.select("main","bars")` |
| `04-source-menu` | source context menu + Transform submenu | contextmenu on `.dock-slot[data-dock=sources] .row`, then `pointerenter` on "Transform" |
| `05-scene-menu` | scene context menu | contextmenu on the first `.dock-slot[data-dock=scenes] .row` |
| `06-canvas-menu` | preview menu + Preview Scaling | contextmenu on `.preview-area` |
| `07-add-source-menu` | Add menu | `app.addSource()` |
| `08-create-source`, `08b-…-media` | Create/Select Source | `app.dialog("create-source",{kind:"color"})` or `{kind:"media"}` |
| `09`/`10`/`11`/`11b-props-*` | Properties: media, text, test pattern, tone | `app.properties(id)` with `clip`, `title`, `bars`, `tone` |
| `12-filters-clip`, `13-filters-tone` | Filters | `app.filters(id)` with `clip`, `tone` |
| `14-transform` | Edit Transform | select, then `app.dialog("transform")` |
| `15-adv-audio` | Advanced Audio Properties | `app.dialog("advanced-audio")` |
| `16-transition` | transition properties | `app.dialog("transition",{kind:"fade"})` |
| `17-rename` | Rename | `app.dialog("rename",{source:"clip"})` |
| `20-settings-{general,stream,output,audio,video,hotkeys,advanced}` | Settings, every tab | `app.settings("<tab>")` |
| `62-settings-output-iso` | Output with ISO on, scrolled | `setIso("clip",true)` via `import("/ui/sources.js")`, then `app.settings("output")` + scroll `.set-body` |
| `30-onboarding-1`, `31-…-2`, `32-…-3` | first run, steps 1–3 | `--route onboarding`, then click `.ob-foot .btn.primary` 0/1/2 times |
| `35-welcome` | Welcome (no setup) | `import("/lib/store.js")` → `setState({setup:null})` |
| `36-connecting`, `37-failed` | engine states | `setState({setup:null, loaded:false, connection:{state, error}})` with state `connecting` or `failed` |
| `40-empty-scene`, `64-empty-scene` | empty scene on the canvas | `app.scene("empty")` (`WAIT=4000`) |
| `41-error-scene` | failed source in list, mixer and toolbar | `app.scene("clip-full"); app.select("clip-full","old-intro")` |
| `42-toasts` | toasts: error, warning, success, info + Undo | `import("/lib/session.js")` → `toast(...)` ×4 |
| `43-mixer-menu` | mixer strip ⋮ menu | click `.strip .icon-btn[title=More]` |
| `44-streaming`, `45-stream-details`, `46-reconnecting` | status bar live; LIVE details; reconnecting | `setState({stream:{state:"live", …}})`, then click `.status-item.live` |
| `50-recording`, `51-rec-details` | recording (real: `isocast start`), REC details | none / click `.status-item.rec` |
| `61-iso-on` | ISO toggle on in a row | `setIso("clip",true)` |
| `63-studio-live` | Studio Mode while live (program ring) | `app.studio()` + fake live stream |
| `53-small-window`, `53b-…-default` (1024×700) | smallest common window | `SIZE=1024x700` |
| `54-hover-row` | a hovered source row (forced) | sets `--bg-hover` on a row |

To take "after" shots, sync a worktree to a test Mac and use the same
`--eval` strings with `ISOCAST_UI_DIR=crates/isocast-app/ui`. CSS and JS
changes need no rebuild.

## 7. Revisions after review (2026-10-12)

Two reviews (polish and usability) of the first pass changed these rules;
the sections above are updated where they state a value.

- Focus: `--focus` is a bone ring with a 1px gap; focus rules come last in
  `app.css`; segmented buttons, rows and picks take the inset ring;
  inputs inside `.field` take none (2.10).
- Selection: `--selected` is g9 with a 3px `--accent-hover` bar and no
  hover change (2.9). The chosen segment is `--accent-soft` with a 1px
  copper edge and `aria-pressed`.
- Colour meanings: copper = state (selected, on, primary, focus field);
  teal = the program (PROGRAM tag, scene dot, the Program monitor's 2px
  frame while recording or streaming); red = a running Stop control,
  REC, errors and danger. `--danger-text` is `#f27a7d` (4.6:1 on
  `--control-hover`).
- Status bar: items fill its 24px height; stream health is an 8px square
  (REC is round) with a word when it isn't fine.
- Icons: `Icon` sets sizes other than 16 inline, so 12/14/20 icons keep
  the 1.5px stroke.
- Forms: a row that ends in a button reserves a 24px lane in every row of
  that form; a file list is one bordered list of 28px rows; the quality
  slider's end labels sit on the value column's edges; number values
  line up with text values; radio and checkbox labels start 24px in.
- First run: one 32px left edge for header, body and footer; one card
  height (560px) for every step; form labels like everywhere else.
- Copy: true minus signs in levels, curly apostrophes in copy.
- Keyboard: Scenes/Sources rows (↑ ↓ Home End F2 with keyboard focus),
  sliders (arrows, Home, End), menus (↑ ↓ → ←), dialogs (focus in, Tab
  trapped, focus restored).

