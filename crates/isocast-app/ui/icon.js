// The app icon (`--route icon`), drawn through snapshot mode where the
// Liquid Glass icon (bundle/Isocast.icon) can't be compiled: the brand
// mark, at the icon's own proportions, on a slate rounded square on
// Apple's 1024-point icon grid (an 824-point square, 100 points in).

import { html } from "./lib/html.js";
import { MarkShapes } from "./ui/icons.js";

export function AppIcon() {
  return html`<svg width="1024" height="1024" viewBox="0 0 1024 1024" style="display:block">
    <defs>
      <linearGradient id="slab" x1="0" y1="0" x2="0" y2="1">
        <stop offset="0" stop-color="#3B4249" />
        <stop offset="1" stop-color="#22262A" />
      </linearGradient>
      <filter id="shadow" x="-20%" y="-20%" width="140%" height="140%">
        <feDropShadow dx="0" dy="12" stdDeviation="14" flood-color="#000" flood-opacity="0.35" />
      </filter>
    </defs>
    <rect x="100" y="100" width="824" height="824" rx="186" fill="url(#slab)" filter="url(#shadow)" />
    <rect x="100.5" y="100.5" width="823" height="823" rx="185.5" fill="none" stroke="#FFFFFF" stroke-opacity="0.08" />
    <${MarkShapes} />
  </svg>`;
}
