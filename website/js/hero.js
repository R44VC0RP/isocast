// The default hero: hero 3 and 3a as one calm loop.
//   together  → the program frame sits as one thick slab
//   exploded  → it separates in depth into its layers, pierced by one copper playhead
//   fanned    → the layers turn into the four file windows, dealt like a deck,
//               each with its own content and the same ticking timecode
// then it eases back together and loops. Springs drive every move.
import { Spring, clamp, lerp } from "./spring.js";
import { FILES, fileWindow, h, svg, tcBadge, peakWave, brackets } from "./files.js";
import { extrude } from "./slab.js";
import { TAKE, LAYERS } from "./take.js";

const W = 480;
const BW = W - 16;
const BH = 261; // 16:9
const T = 12; // slab thickness (matches the Paper 3a slabs)
const R = 16; // corner radius of the slab; the picture inside is concentric (16 − 8 = 8)

// Loop timeline (seconds). Each state settles (about 1.3 s of spring), then
// holds: together ~1.6 s, exploded ~2.6 s, fanned ~3.6 s. Calm, not busy.
const TIMELINE = [
  { at: 0.0, sep: 0, fan: 0, tags: 0 },   // together
  { at: 2.6, sep: 1, fan: 0, tags: 1 },   // exploded: labels and the copper pin
  { at: 6.6, sep: 1, fan: 1, tags: 0 },   // fanned files
  { at: 11.8, sep: 0, fan: 0, tags: 0 },  // fold straight back into one slab
];
const LOOP = 14.6;

// Each layer shows its own file of the real take (js/take.js): the program
// composite, the screen, the camera, and the mic as a waveform of its audio.
function videoContent(kind, loop, extra) {
  return () => {
    const box = h("div", "content");
    box.style.background = "#0d0d0e";
    if (loop.els[kind]) box.appendChild(loop.els[kind]);
    if (extra) box.appendChild(extra());
    return box;
  };
}

function micContent(peaks) {
  return (w, hgt) => {
    const box = h("div", "content");
    box.style.background = "linear-gradient(180deg, #0d100f, #0a0b0b)";
    const wh = 132;
    const place = (wv) => {
      Object.assign(wv.style, { left: "16px", top: `${(hgt - wh) / 2}px`, width: `${w - 32}px`, height: `${wh}px` });
      box.appendChild(wv);
    };
    // The whole file's waveform; the loop is the stretch under the playhead.
    peaks.then((p) => { if (p) place(peakWave(p.overview, w - 32, wh)); });
    return box;
  };
}

const COLORS = {
  program: ["#2b2d30", "#1b1c1e", 0.16],
  screen: ["#28292c", "#19191b", 0.14],
  camera: ["#25262a", "#171819", 0.12],
  mic: ["#222326", "#151617", 0.1],
};

// z of the bottom layer in the stacked (isometric) arrangement.
const zIsoBottom = (sep, gap) => -1.5 * (T + sep * gap);

export function mountHero(ctx, loop, peaksP) {
  // Each layer shows what js/take.js's LAYERS says: a video, a still or peaks.
  const layer = (kind, extra) => (LAYERS[kind] && LAYERS[kind].peaks ? micContent(peaksP) : videoContent(kind, loop, extra));
  const CONTENT = {
    program: layer("program"),
    screen: layer("screen"),
    camera: layer("camera", brackets),
    mic: layer("mic"),
  };
  let peaks = null;
  peaksP.then((p) => { peaks = p; });
  // Small screens get taller window headers so names stay readable when scaled down.
  // The header only exists for the fanned files; stacked, each layer is an even
  // bezel around its picture (as in the Paper 3a slabs).
  const HEAD = ctx.compact ? 46 : 34;
  const world = h("div", "world merge");
  Object.assign(world.style, { position: "absolute", inset: "0", perspective: "2600px", perspectiveOrigin: "50% 46%" });
  const group = h("div", "merge-group");
  Object.assign(group.style, { position: "absolute", left: "0", top: "0", width: "0", height: "0", transformStyle: "preserve-3d", transformOrigin: "0 0" });
  world.appendChild(group);

  const units = FILES.map((f, i) => {
    const unit = h("div", "merge-unit");
    unit.style.zIndex = String(10 - i);
    const win = fileWindow(f, { w: W, bodyH: BH, headH: HEAD, colors: COLORS[f.kind], content: CONTENT[f.kind] });
    unit.append(win.el);
    const slab = extrude(unit, { r: R, t: T });
    group.appendChild(unit);
    return { ...win, unit, slab, i };
  });

  // Contact shadow under the stack (drawn first, so it sits beneath the slabs).
  const floor = h("div", "floor");
  Object.assign(floor.style, { width: `${W}px`, height: `${8 + HEAD + BH + 8}px`, transformOrigin: "50% 50%" });
  group.prepend(floor);

  // One copper playhead through every layer while they're stacked.
  const rods = [0, 1, 2, 3, 4].map(() => {
    const a = h("div", "rod");
    const b = h("div", "rod rod-x");
    group.append(a, b);
    return [a, b];
  });
  const dots = units.map((u) => {
    const d = h("div", "slab-dot");
    u.body.appendChild(d);
    return d;
  });

  // 2D overlay: labels with leader lines, and the timecode badge on the rod.
  const overlay = h("div", "iso-overlay");
  Object.assign(overlay.style, { position: "absolute", inset: "0", pointerEvents: "none", opacity: "0" });
  const lines = svg("svg", { class: "iso-leaders", width: ctx.w, height: ctx.h, viewBox: `0 0 ${ctx.w} ${ctx.h}` });
  Object.assign(lines.style, { position: "absolute", left: "0", top: "0", overflow: "visible" });
  const paths = units.map(() => {
    const p = svg("path", { fill: "none", stroke: "#3a3836", "stroke-width": "1" });
    lines.appendChild(p);
    return p;
  });
  overlay.appendChild(lines);
  const labels = FILES.map((f) => {
    const l = h("div", "iso-label");
    l.textContent = f.name;
    if (f.kind === "program") l.classList.add("is-program");
    overlay.appendChild(l);
    return l;
  });
  const tcb = tcBadge();
  overlay.appendChild(tcb.el);
  ctx.inner.append(world, overlay);
  // Label widths, so every label keeps the page gutter even when the object
  // runs edge to edge on phones.
  let labelW = labels.map((l) => l.offsetWidth);
  document.fonts?.ready.then(() => { labelW = labels.map((l) => l.offsetWidth); });

  // Phase springs.
  const sepS = new Spring(0, 1.25, 0);
  const fanS = new Spring(0, 1.45, 0);
  const tagS = new Spring(0, 0.6, 0); // labels, pin and badge fade on their own
  let clock = 0;
  let hold = 0; // seconds the loop stays paused (after a scrub)
  let step = -1;
  if (ctx.reduced) { sepS.snap(1); fanS.snap(1); tagS.snap(0); }

  const tmp = new DOMPoint();
  const project = (M, x, y, z) => {
    tmp.x = x; tmp.y = y; tmp.z = z; tmp.w = 1;
    const p = M.transformPoint(tmp);
    return { x: p.x / p.w, y: p.y / p.w };
  };
  let lastHeadA = -1, lastRodA = -1, lastLabelA = 1, lastHH = -1, lastFloor = "", lastSh = -1;

  return {
    // Dragging moves the playhead with the finger (the loop spans
    // length/total of the file's width).
    scrubSpan: () => BW * lerp(0.84, 1, fanS.x) * (TAKE.length / TAKE.total),
    onRelease() { hold = 1.4; },
    phase: () => ({ clock: +clock.toFixed(2), sep: +sepS.x.toFixed(3), fan: +fanS.x.toFixed(3) }),
    // Verification: jump to a point in the loop and settle there.
    seek(t) {
      clock = ((t % LOOP) + LOOP) % LOOP;
      step = -1;
      let k = 0;
      for (let j = 0; j < TIMELINE.length; j++) if (clock >= TIMELINE[j].at) k = j;
      sepS.snap(TIMELINE[k].sep);
      fanS.snap(TIMELINE[k].fan);
      tagS.snap(TIMELINE[k].tags);
      hold = 0;
    },
    update(ctx, dt) {
      const { s, take } = ctx;
      // ── timeline ──
      if (ctx.reduced) {
        sepS.snap(1); fanS.snap(1); tagS.snap(0);
      } else {
        if (ctx.scrubbing) hold = Math.max(hold, 0.6);
        else if (hold > 0) hold -= dt;
        else clock = (clock + dt) % LOOP;
        let k = 0;
        for (let j = 0; j < TIMELINE.length; j++) if (clock >= TIMELINE[j].at) k = j;
        if (k !== step) {
          step = k;
          sepS.target = TIMELINE[k].sep;
          fanS.target = TIMELINE[k].fan;
          tagS.target = TIMELINE[k].tags;
        }
        sepS.step(dt);
        fanS.step(dt);
        tagS.step(dt);
      }
      const e = s.enter.x;
      const sep = clamp(sepS.x, -0.1, 1.2);
      const f = clamp(fanS.x, -0.05, 1.1);
      const boost = s.fan.x; // hover: a little extra air between the files

      // ── camera: isometric when stacked, facing you when fanned ──
      // On small screens the stack slides left while its labels are showing.
      const labelPhase = clamp(tagS.x, 0, 1) * clamp(1 - f * 2, 0, 1);
      const cx = ctx.w * (ctx.compact ? 0.5 - 0.09 * labelPhase : 0.5);
      const cy = ctx.h * lerp(0.5, 0.5, f) + (1 - e) * 26;
      const rx = lerp(55, 7, f) - s.tiltY.x * 5 + s.press.x * 2 + (1 - e) * 6;
      const rz = lerp(45, 0, f) + s.tiltX.x * 4 * (1 - f);
      const ry = lerp(0, -10, f) + s.tiltX.x * 7 * f;
      // One slab reads big; it shrinks a little to make room as it explodes.
      // On phones the object runs edge to edge, so the fanned deck is a touch
      // smaller to keep a margin on both sides.
      const sc = lerp(lerp(0.96, ctx.compact ? 0.8 : 0.84, clamp(sep, 0, 1)), ctx.compact ? 0.92 : 0.98, f);
      const lift = -s.press.x * 14 - (1 - e) * 40;
      const tf = `translate3d(${cx.toFixed(2)}px, ${cy.toFixed(2)}px, ${lift.toFixed(2)}px) rotateX(${rx.toFixed(3)}deg) rotateY(${ry.toFixed(3)}deg) rotateZ(${rz.toFixed(3)}deg) scale(${sc.toFixed(4)})`;
      group.style.transform = tf;
      const op = Math.min(1, e * 1.4).toFixed(3);
      world.style.opacity = op;

      // Window chrome (names, timecodes) belongs to the fanned files: the header
      // opens as they turn to face you, and closes back into the bezel.
      const open = clamp((f - 0.1) / 0.7, 0, 1);
      const hh = Math.round(HEAD * open * (2 - open) * 4) / 4; // ease-out, quarter pixels
      const H = 8 + hh + BH + 8;
      if (hh !== lastHH) {
        for (const u of units) {
          u.head.style.height = `${hh}px`;
          u.slab.layout(W, H);
        }
        lastHH = hh;
      }

      // ── the four files ──
      const gap = (ctx.compact ? 102 : 146) * (1 + boost * 0.35);
      const stepX = (ctx.compact ? 44 : 54) * (1 + boost * 0.35);
      const stepY = (ctx.compact ? 58 : 52) * (1 + boost * 0.25);
      const depth = 60 * (1 + boost * 0.6);
      const offs = [];
      for (let i = 0; i < 4; i++) {
        const u = units[i];
        const k = i - 1.5;
        const zIso = -k * (T + sep * gap);
        const zFan = -k * depth;
        const x = lerp(0, k * stepX, f);
        const y = lerp(0, -k * stepY, f);
        const z = lerp(zIso, zFan, f);
        offs.push([x, y, z]);
        u.unit.style.transform = `translate3d(${(x - W / 2).toFixed(2)}px, ${(y - H / 2).toFixed(2)}px, ${z.toFixed(2)}px)`;
        // Each body's playhead at the same instant.
        const px = take.frac * BW;
        u.playhead.style.transform = `translate3d(${px.toFixed(2)}px,0,0)`;
        if (u.dim) {
          u.dim.style.transform = `translate3d(${px.toFixed(2)}px,0,0)`;
          // The mic's level right now, from the same peaks the bars are drawn from.
          let lv = 0;
          if (peaks) {
            const p = peaks.peaks, x = (take.pos / TAKE.length) * p.length;
            const i = Math.min(p.length - 1, Math.max(0, Math.floor(x)));
            lv = Math.max(p[i], p[Math.min(p.length - 1, i + 1)]) / 100;
          }
          u.level.style.transform = `translate3d(${px.toFixed(2)}px,0,0) scaleY(${Math.max(0.04, lv).toFixed(3)})`;
        }
        u.ticker.set(ctx.tc.mmss);
        u.glare.style.setProperty("--gx", `${(-s.tiltX.x * 60).toFixed(1)}px`);
        u.glare.style.setProperty("--gy", `${(-s.tiltY.x * 40).toFixed(1)}px`);
        dots[i].style.transform = `translate3d(${(px - 6).toFixed(2)}px, ${(BH * 0.5 - 6).toFixed(2)}px, 0)`;
      }

      // Names and timecodes only show once the files have spread apart, so a
      // header never peeks out half-covered while the deck turns or folds.
      const headA = clamp((f - 0.72) / 0.22, 0, 1);
      if (Math.abs(headA - lastHeadA) > 0.002) {
        const a = (headA * headA * (3 - 2 * headA)).toFixed(3);
        for (const u of units) for (const c of u.head.children) c.style.opacity = a;
        lastHeadA = headA;
      }

      // Contact shadow: tight under one slab, soft and wide as the layers lift,
      // gone once the files face you.
      const floorA = (1 - clamp(f * 1.6, 0, 1)) * lerp(0.95, 0.55, clamp(sep, 0, 1));
      const shA = lerp(0.25, 1, clamp(f, 0, 1));
      if (Math.abs(shA - lastSh) > 0.003) { for (const u of units) u.slab.setShadow(shA); lastSh = shA; }
      const spread = lerp(1.04, 1.16, clamp(sep, 0, 1));
      const fz = (zIsoBottom(sep, gap) - T - 1).toFixed(2);
      const fl = `${floorA.toFixed(3)}|${spread.toFixed(3)}|${fz}`;
      if (fl !== lastFloor) {
        floor.style.transform = `translate3d(${-W / 2}px, ${-H / 2 + 10}px, ${fz}px) scale(${spread.toFixed(3)})`;
        floor.style.opacity = floorA.toFixed(3);
        lastFloor = fl;
      }

      // ── the rod, labels and badge belong to the exploded stack ──
      const rodA = clamp(tagS.x, 0, 1) * clamp(sep * 1.6 - 0.4, 0, 1) * clamp(1 - f * 3, 0, 1);
      const labelA = rodA;
      const px = -W / 2 + 8 + take.frac * BW;
      const py = -H / 2 + 8 + hh + BH * 0.5;
      if (Math.abs(rodA - lastRodA) > 0.002) {
        for (const r of rods) { r[0].style.opacity = r[1].style.opacity = rodA.toFixed(3); }
        for (const d of dots) d.style.opacity = rodA.toFixed(3);
        lastRodA = rodA;
      }
      const zs = offs.map((o) => o[2]); // program (top) … mic (bottom)
      const ztop = zs[0] + (ctx.compact ? 64 : 110);
      // Above the top layer to the badge, between every pair, and a tail below the stack.
      const segs = [[zs[0], ztop], [zs[1], zs[0] - T], [zs[2], zs[1] - T], [zs[3], zs[2] - T], [zs[3] - T - 70, zs[3] - T]];
      for (let j = 0; j < 5; j++) {
        const [a, b] = segs[j];
        const L = Math.max(0.01, b - a).toFixed(2);
        rods[j][0].style.transform = `translate3d(${(px - 1.5).toFixed(2)}px, ${py.toFixed(2)}px, ${a.toFixed(2)}px) rotateX(90deg) scaleY(${L})`;
        rods[j][1].style.transform = `translate3d(${px.toFixed(2)}px, ${(py - 1.5).toFixed(2)}px, ${a.toFixed(2)}px) rotateY(-90deg) scaleX(${L})`;
      }

      if (labelA > 0.001 || lastLabelA > 0.001) {
        const ox = ctx.w / 2, oy = ctx.h * 0.46;
        const M = new DOMMatrix(`translate(${ox}px, ${oy}px) perspective(2600px) translate(${-ox}px, ${-oy}px) ${tf}`);
        for (let i = 0; i < 4; i++) {
          const [x, y, z] = offs[i];
          let best = null;
          for (const [lx, ly] of [[W / 2, -H / 2], [W / 2, H / 2], [-W / 2, H / 2], [-W / 2, -H / 2]]) {
            const p = project(M, x + lx, y + ly, z);
            if (!best || p.x > best.x) best = p;
          }
          const edge = ctx.w - (ctx.compact ? 22 : 12) / ctx.k; // keep the page gutter
          const widest = Math.max(...labelW);
          const labelX = Math.min(Math.max(best.x + 26, edge - widest), edge - labelW[i]) ;
          paths[i].setAttribute("d", `M${(best.x + 8).toFixed(1)} ${best.y.toFixed(1)}H${(labelX - 10).toFixed(1)}`);
          labels[i].style.transform = `translate3d(${labelX.toFixed(1)}px, ${(best.y - (ctx.compact ? 14 : 10)).toFixed(1)}px, 0)`;
        }
        const top = project(M, px, py, ztop);
        tcb.el.style.transform = `translate3d(${top.x.toFixed(1)}px, ${(top.y - 30).toFixed(1)}px, 0) translateX(-50%)`;
        overlay.style.opacity = (labelA * e).toFixed(3);
        lastLabelA = labelA;
      }
      tcb.set(ctx.tc);
    },
    destroy() { world.remove(); overlay.remove(); },
  };
}
