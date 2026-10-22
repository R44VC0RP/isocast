// The thickness of a slab: the top face's rounded rectangle, extruded.
//
// The edge is real geometry: the perimeter is walked once (straight sides
// plus each corner arc split into short facets) and every piece becomes a
// wall standing at the boundary, folded down into -z. The walls follow the
// same radius as the top face, so the edge is continuous around every
// corner at any viewing angle. Each piece is shaded from its own outward
// normal (a fixed light), lit at the lip and darker towards the bottom, with
// a 1 px highlight where the edge meets the top. A core plane inside the slab
// sits behind every wall, so anti-aliased joins between facets never show
// the page through.

import { h } from "./files.js";

const FACETS = 6; // per quarter circle: chord error < 0.15 px at r = 16
const OVERLAP = 0.6; // px each facet overruns its neighbours
const LIGHT = (() => { const x = -0.45, y = 1, l = Math.hypot(x, y); return [x / l, y / l]; })();

// Edge colours: mixed between a shadowed and a lit graphite by the facet's light.
const DARK = [15, 16, 18];
const LIT = [52, 54, 58];
const mix = (t) => {
  const c = DARK.map((d, i) => Math.round(d + (LIT[i] - d) * Math.min(1, Math.max(0, t))));
  return `rgb(${c[0]} ${c[1]} ${c[2]})`;
};

// The boundary, walked counter-clockwise on screen (y down), so each piece's
// outward normal is its direction turned by +90°.
function perimeter(W, H, r) {
  const segs = [];
  const line = (x, y, phi, len) => segs.push({ x, y, phi, len });
  const arc = (cx, cy, a0, a1) => {
    for (let k = 0; k < FACETS; k++) {
      const p = (a) => [cx + r * Math.cos(a), cy + r * Math.sin(a)];
      const [x1, y1] = p(a0 + ((a1 - a0) * k) / FACETS);
      const [x2, y2] = p(a0 + ((a1 - a0) * (k + 1)) / FACETS);
      line(x1, y1, Math.atan2(y2 - y1, x2 - x1), Math.hypot(x2 - x1, y2 - y1));
    }
  };
  const d = Math.PI / 180;
  line(W - r, 0, 180 * d, W - 2 * r);           // top edge, right → left
  arc(r, r, -90 * d, -180 * d);                 // top-left corner
  line(0, r, 90 * d, H - 2 * r);                // left edge, down
  arc(r, H - r, 180 * d, 90 * d);               // bottom-left corner
  line(r, H, 0, W - 2 * r);                     // bottom edge, left → right
  arc(W - r, H - r, 90 * d, 0);                 // bottom-right corner
  line(W, H - r, -90 * d, H - 2 * r);           // right edge, up
  arc(W - r, r, 0, -90 * d);                    // top-right corner
  return segs;
}

export function extrude(unit, { r, t }) {
  const core = h("div", "slab-core");
  const shadow = h("div", "slab-shadow");
  unit.prepend(shadow, core);
  const n = 4 + 4 * FACETS;
  const walls = [];
  for (let i = 0; i < n; i++) {
    const w = h("div", "slab-wall");
    w.style.height = `${t}px`;
    unit.prepend(w);
    walls.push(w);
  }
  let shaded = false;
  return {
    // Lay the walls along the slab's current size (its height changes as the
    // window header opens).
    layout(W, H) {
      const segs = perimeter(W, H, r);
      segs.forEach((s, i) => {
        const w = walls[i];
        const len = s.len + OVERLAP * 2;
        const x = s.x - Math.cos(s.phi) * OVERLAP;
        const y = s.y - Math.sin(s.phi) * OVERLAP;
        w.style.width = `${len.toFixed(3)}px`;
        w.style.transform = `translate3d(${x.toFixed(3)}px, ${y.toFixed(3)}px, 0) rotateZ(${s.phi.toFixed(5)}rad) rotateX(-90deg)`;
        if (!shaded) {
          // Outward normal of this piece, and how much light it catches.
          const nx = -Math.sin(s.phi), ny = Math.cos(s.phi);
          const lit = 0.5 + 0.5 * (nx * LIGHT[0] + ny * LIGHT[1]);
          w.style.background = `linear-gradient(180deg, ${mix(lit * 0.9 + 0.12)}, ${mix(lit * 0.55 - 0.05)})`;
          w.style.boxShadow = `inset 0 1px 0 rgb(255 255 255 / ${(0.05 + 0.1 * lit).toFixed(3)})`;
        }
      });
      shaded = true;
      core.style.width = `${W - 2}px`;
      core.style.height = `${H - 2}px`;
      core.style.transform = `translate3d(1px, 1px, ${(-t * 0.5).toFixed(2)}px)`;
      shadow.style.width = `${W}px`;
      shadow.style.height = `${H}px`;
      shadow.style.transform = `translate3d(0, 0, ${(-t - 0.5).toFixed(2)}px)`;
    },
    // The soft drop shadow belongs to the fanned windows; stacked, the floor's
    // contact shadow grounds the slabs instead.
    setShadow(a) { shadow.style.opacity = a.toFixed(3); },
  };
}
