// isocast.com: one render loop, one clock, and springs for everything that moves.
import { Spring, clamp, timecode } from "./spring.js";
import { mountHero } from "./hero.js";
import { TAKE, SIZES, createTake, loadPeaks } from "./take.js";

const root = document.documentElement;

// The release on offer: index.html writes the version once (.meta's
// data-version); both download links point at that version's DMG. Without
// this script they open the latest release's page.
const version = document.querySelector(".meta[data-version]")?.dataset.version;
if (version && /^\d+\.\d+\.\d+$/.test(version)) {
  const dmg = `https://github.com/R44VC0RP/isocast/releases/download/v${version}/Isocast-${version}-arm64.dmg`;
  for (const a of document.querySelectorAll("a[data-download]")) a.href = dmg;
}

// Scroll-in sections start hidden (site.css) and show themselves after 2.5 s
// if this module never runs. If it arrives that late, or the browser can't
// tell CSS that scripts run, they stay where they are, without the entrance.
const lateStart = performance.now() > 2300 || !matchMedia("(scripting: enabled)").matches;
if (lateStart) {
  for (const el of document.querySelectorAll("[data-reveal]")) el.removeAttribute("data-reveal");
  for (const b of document.querySelectorAll(".cpu-bar")) b.style.transform = "none";
}
root.classList.add("is-live");

const stage = document.getElementById("stage");
const inner = document.getElementById("stage-inner");
const hero = document.querySelector(".hero");
const light = document.querySelector(".hero-light");

const reducedMQ = matchMedia("(prefers-reduced-motion: reduce)");
const compactMQ = matchMedia("(max-width: 720px)");

// ───────── Springs ─────────
const s = {
  tiltX: new Spring(0, 0.9, 0),      // pointer parallax, -1..1
  tiltY: new Spring(0, 0.9, 0),
  fan: new Spring(0, 0.7, 0.1),      // hover: a little more air between the files
  press: new Spring(0, 0.18, 0),     // the object sinks under a press
  enter: new Spring(0, 0.9, 0.12),   // first appearance
  lx: new Spring(0, 1.2, 0),         // background light, px
  ly: new Spring(0, 1.2, 0),
};

// ───────── One clock for every file ─────────
// The real take's loop time, read from program.mp4 (js/take.js). Dragging scrubs it.
// Every file is the whole 26 s take; the loop is 00:20–00:23 of it, so the
// playheads creep through that stretch and spring back at the seam.
const take = { dur: TAKE.length, pos: 0, frac: (TAKE.start / TAKE.total) };
const head = new Spring(TAKE.start / TAKE.total, 0.32, 0);
const peaksP = loadPeaks();
let loop = null;

const ctx = {
  stage, inner, s, take,
  reduced: reducedMQ.matches,
  compact: compactMQ.matches,
  k: 1,
  w: 700, h: 560,
  hover: false,
  scrubbing: false,
  tc: timecode(TAKE.start),
};

let hero3d = null;

// ───────── Layout: scale the fixed-size stage to its box ─────────
let stageRect = stage.getBoundingClientRect();
let heroRect = hero.getBoundingClientRect();
let pointerSeen = false;
function measure() {
  const cs = getComputedStyle(stage);
  ctx.w = parseFloat(cs.getPropertyValue("--sw")) || 700;
  ctx.h = parseFloat(cs.getPropertyValue("--sh")) || 560;
  const box = stage.clientWidth || ctx.w;
  ctx.k = box / ctx.w;
  ctx.compact = compactMQ.matches;
  inner.classList.toggle("is-compact", ctx.compact);
  inner.style.setProperty("--k", ctx.k.toFixed(4));
  stageRect = stage.getBoundingClientRect();
  heroRect = hero.getBoundingClientRect();
  if (!pointerSeen) {
    // Until the pointer arrives, the light sits behind the object.
    s.lx.snap(stageRect.left + stageRect.width / 2 - heroRect.left);
    s.ly.snap(stageRect.top + stageRect.height * 0.45 - heroRect.top);
  }
}
const ro = new ResizeObserver(() => {
  const was = ctx.compact;
  measure();
  if (hero3d && was !== ctx.compact) mount({ intro: false });
  kick();
});
ro.observe(stage);
addEventListener("scroll", () => { stageRect = stage.getBoundingClientRect(); heroRect = hero.getBoundingClientRect(); }, { passive: true });

// ───────── Mounting ─────────
// The encoded size that covers the drawn picture at this screen's density.
function takeSize() {
  // Device pixels the picture is drawn at; allow ~10% softness before stepping up.
  const px = 464 * ctx.k * (window.devicePixelRatio || 1) * 0.9;
  const sizes = [...SIZES].sort((a, b) => a - b);
  return ctx.compact ? sizes[0] : sizes.find((s) => s >= px) || sizes[sizes.length - 1];
}
const saveData = !!(navigator.connection && navigator.connection.saveData);

function mount({ intro = true } = {}) {
  measure();
  if (hero3d) hero3d.destroy();
  if (loop) loop.destroy();
  inner.replaceChildren();
  // Reduced motion and data saver get the stills.
  loop = createTake({ size: takeSize(), still: ctx.reduced || saveData });
  hero3d = mountHero(ctx, loop, peaksP);
  loop.setVisible(visible && inView);
  if (intro && !ctx.reduced) {
    s.enter.snap(0);
    s.enter.target = 1;
  } else {
    s.enter.snap(1);
  }
  kick();
}

function updateClock() {
  if (loop) loop.sync();
  take.pos = loop ? loop.time() : 0;
  head.target = (TAKE.start + take.pos) / TAKE.total;
  // Playing forward (and while scrubbing) the head is exactly on the clock;
  // the jump back at the loop seam glides on its spring instead of popping.
  const d = head.target - head.x;
  if (ctx.reduced || ctx.scrubbing || (d >= 0 && d < 0.02)) head.snap();
  take.frac = clamp(head.x, 0, 1);
  ctx.tc = timecode(TAKE.start + take.pos);
}

// Scrub: move the one clock (and every file with it) by a fraction of the loop.
ctx.scrubBy = (df) => {
  if (!loop) return;
  loop.scrubTo(loop.time() + df * take.dur);
};
let keyScrubTimer = 0;

// ───────── Loop ─────────
let raf = 0;
let last = 0;
let visible = document.visibilityState === "visible";
let inView = true;
function kick() {
  if (!raf && visible && inView) {
    last = performance.now();
    raf = requestAnimationFrame(frame);
  }
}
function frame(now) {
  raf = 0;
  const dt = Math.min(0.05, Math.max(0, (now - last) / 1000));
  last = now;

  s.fan.target = ctx.hover && !ctx.scrubbing ? 1 : 0;
  if (ctx.reduced) {
    s.enter.snap(1);
    for (const k of ["fan", "press", "tiltX", "tiltY"]) s[k].snap(0);
    s.lx.snap(); s.ly.snap();
  } else {
    for (const k in s) s[k].step(dt);
    head.step(dt);
  }

  updateClock();
  light.style.transform = `translate3d(${s.lx.x.toFixed(1)}px, ${s.ly.x.toFixed(1)}px, 0)`;
  if (hero3d) hero3d.update(ctx, dt, now);

  // Reduced motion draws once per change and then idles.
  if (visible && inView && !ctx.reduced) raf = requestAnimationFrame(frame);
}

// ───────── Input: pointer, drag, keyboard ─────────
function pointerTilt(e) {
  const r = stageRect;
  const cx = r.left + r.width / 2;
  const cy = r.top + r.height / 2;
  s.tiltX.target = clamp((e.clientX - cx) / (r.width * 0.75), -1, 1);
  s.tiltY.target = clamp((e.clientY - cy) / (r.height * 0.75), -1, 1);
  s.lx.target = e.clientX - heroRect.left;
  s.ly.target = e.clientY - heroRect.top;
}
hero.addEventListener("pointermove", (e) => {
  if (e.pointerType !== "mouse" || ctx.reduced) return;
  pointerSeen = true;
  pointerTilt(e);
  kick();
}, { passive: true });
hero.addEventListener("pointerleave", (e) => {
  if (e.pointerType !== "mouse") return;
  s.tiltX.target = 0;
  s.tiltY.target = 0;
}, { passive: true });
stage.addEventListener("pointerenter", (e) => { if (e.pointerType === "mouse") { ctx.hover = true; kick(); } });
stage.addEventListener("pointerleave", (e) => { if (e.pointerType === "mouse") { ctx.hover = false; kick(); } });

// Drag (mouse or touch) scrubs the one clock: every file's playhead moves together.
let drag = null;
stage.addEventListener("pointerdown", (e) => {
  if (drag || (e.pointerType === "mouse" && e.button !== 0)) return; // one pointer at a time
  drag = { id: e.pointerId, x0: e.clientX, y0: e.clientY, lastX: e.clientX, active: false };
  s.press.target = 1;
  kick();
});
stage.addEventListener("pointermove", (e) => {
  if (!drag || e.pointerId !== drag.id) return;
  const dx = e.clientX - drag.x0;
  const dy = e.clientY - drag.y0;
  if (!drag.active && Math.abs(dx) > 5 && Math.abs(dx) > Math.abs(dy)) {
    drag.active = true;
    ctx.scrubbing = true;
    stage.classList.add("is-dragging");
    try { stage.setPointerCapture(drag.id); } catch {}
    drag.lastX = e.clientX;
  }
  if (drag.active) {
    const span = hero3d.scrubSpan(ctx) * ctx.k;
    ctx.scrubBy((e.clientX - drag.lastX) / span);
    drag.lastX = e.clientX;
  }
  kick();
});
function endDrag(e) {
  if (!drag || e.pointerId !== drag.id) return;
  if (drag.active) {
    ctx.scrubbing = false;
    stage.classList.remove("is-dragging");
    loop.endScrub();
    hero3d.onRelease(ctx);
  }
  s.press.target = 0;
  drag = null;
  kick();
}
// Listen on window: a drag can end anywhere, even outside the stage or the window.
// (Touch pointers also lose and regain capture as the drag starts, so capture
// events can't be trusted to mean "the drag ended".)
addEventListener("pointerup", endDrag);
addEventListener("pointercancel", endDrag);
addEventListener("blur", () => { if (drag) endDrag({ pointerId: drag.id }); });

// Keyboard: with the stage focused, arrows step the take a frame at a time
// (Shift: half a second); it plays on again shortly after the last press.
stage.tabIndex = 0;
stage.addEventListener("keydown", (e) => {
  if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
  e.preventDefault();
  const dir = e.key === "ArrowLeft" ? -1 : 1;
  ctx.scrubBy((dir * (e.shiftKey ? 0.5 : 1 / 30)) / take.dur);
  clearTimeout(keyScrubTimer);
  keyScrubTimer = setTimeout(() => loop.endScrub(), 900);
  hero3d.onRelease(ctx);
  kick();
});

// Visibility: run the loop only while the hero can be seen.
document.addEventListener("visibilitychange", () => {
  visible = document.visibilityState === "visible";
  loop?.setVisible(visible && inView);
  if (visible) kick();
});
// Videos and the render loop pause while the object is off-screen.
new IntersectionObserver((entries) => {
  inView = entries[entries.length - 1].isIntersecting;
  loop?.setVisible(visible && inView);
  if (inView) kick();
}, { rootMargin: "48px" }).observe(stage);

reducedMQ.addEventListener("change", () => {
  ctx.reduced = reducedMQ.matches;
  if (ctx.reduced) { s.tiltX.target = s.tiltY.target = 0; }
  mount({ intro: false });
});

// ───────── Below the hero ─────────
// Sections enter once, on springs, as they scroll in; the CPU bars then grow
// to their values. Reduced motion shows everything in place.
const revealItems = [...document.querySelectorAll("[data-reveal]")];
const bars = [...document.querySelectorAll(".cpu-bar")];
const moving = new Set();
let belowRaf = 0;
let belowLast = 0;
function belowKick() {
  if (!belowRaf) { belowLast = performance.now(); belowRaf = requestAnimationFrame(belowFrame); }
}
function settle(el) {
  el.style.opacity = "";
  el.style.transform = "";
  el.removeAttribute("data-reveal");
}
function belowFrame(now) {
  belowRaf = 0;
  const dt = Math.min(0.05, (now - belowLast) / 1000);
  belowLast = now;
  for (const m of moving) {
    if (now < m.start) continue;
    m.s.step(dt);
    if (m.kind === "reveal") {
      const x = Math.min(1, m.s.x);
      m.el.style.opacity = Math.min(1, x * 1.15).toFixed(3);
      m.el.style.transform = `translate3d(0, ${((1 - m.s.x) * 16).toFixed(2)}px, 0)`;
      if (m.s.resting) { settle(m.el); moving.delete(m); }
    } else {
      m.el.style.transform = `scaleX(${Math.max(0, m.s.x).toFixed(4)})`;
      if (m.s.resting) { m.el.style.transform = "none"; moving.delete(m); }
    }
  }
  const pulsing = pulseFrame(now);
  if (moving.size || pulsing) belowRaf = requestAnimationFrame(belowFrame);
}
const revealIO = new IntersectionObserver((entries) => {
  const now = performance.now();
  let i = 0;
  for (const en of entries) {
    if (!en.isIntersecting) continue;
    const el = en.target;
    revealIO.unobserve(el);
    if (ctx.reduced || lateStart) { settle(el); continue; }
    const start = now + i++ * 90; // stagger sections that arrive together
    const s = new Spring(0, 0.9, 0);
    s.target = 1;
    moving.add({ kind: "reveal", el, s, start });
    if (el.classList.contains("cpu")) {
      el.querySelectorAll(".cpu-bar").forEach((bar, j) => {
        const b = new Spring(0, 1.1, 0);
        b.target = 1;
        moving.add({ kind: "bar", el: bar, s: b, start: start + 220 + j * 110 });
      });
    }
  }
  belowKick();
}, { rootMargin: "0px 0px -10% 0px" });
revealItems.forEach((el) => revealIO.observe(el));
if (ctx.reduced || lateStart) bars.forEach((b) => (b.style.transform = "none"));

// The diagram: hover a tile and its wires light up with the files it feeds.
const flow = document.getElementById("flow");
const chips = [...flow.querySelectorAll(".chip")];
const wires = [...flow.querySelectorAll(".flow-in path, .flow-out path")];
function hot(kind) {
  const on = (el) => {
    const src = el.dataset.src, out = el.dataset.out;
    if (!kind) return false;
    if (kind === "program") return out === "program" || !!src;
    return src === kind || out === kind || out === "program";
  };
  for (const el of chips) el.classList.toggle("is-hot", on(el));
  for (const el of wires) el.classList.toggle("is-hot", on(el));
}
for (const chip of chips) {
  const kind = chip.dataset.src || chip.dataset.out;
  chip.addEventListener("pointerenter", (e) => { if (e.pointerType === "mouse") hot(kind); });
  chip.addEventListener("pointerleave", () => hot(null));
}

// One slow pulse travels the wires on one clock: from the sources into the
// mark, then out to all three files at once.
const PULSE = 4.8; // seconds per cycle
const hubGlow = flow.querySelector(".flow-hub-glow");
const pulses = wires.map((w) => {
  const p = w.cloneNode();
  p.removeAttribute("data-src");
  p.removeAttribute("data-out");
  p.classList.add("pulse");
  p.setAttribute("pathLength", "1");
  w.after(p);
  return { p, out: !!w.dataset.out };
});
let flowVisible = false;
let pulseT0 = 0;
function pulseFrame(now) {
  if (!flowVisible || ctx.reduced || document.visibilityState !== "visible") return false;
  const t = ((now - pulseT0) / 1000) % PULSE;
  const leg = (from, dur) => Math.min(1, Math.max(0, (t - from) / dur));
  const ease = (x) => x * x * (3 - 2 * x);
  for (const { p, out } of pulses) {
    const x = out ? leg(1.55, 1.25) : leg(0.2, 1.25);
    const d = ease(x);
    // A short dash slides along the wire; it fades in and out at the ends.
    p.style.strokeDashoffset = (0.14 - d * 1.14).toFixed(4);
    p.style.opacity = x > 0 && x < 1 ? (Math.sin(Math.PI * x) * 0.9).toFixed(3) : "0";
  }
  const g = leg(1.25, 0.9);
  hubGlow.style.opacity = g > 0 && g < 1 ? (Math.sin(Math.PI * g) * 0.8).toFixed(3) : "0";
  return true;
}
new IntersectionObserver(([en]) => {
  const was = flowVisible;
  flowVisible = en.isIntersecting;
  if (flowVisible && !was) { pulseT0 = performance.now(); belowKick(); }
}).observe(flow);
document.addEventListener("visibilitychange", () => { if (document.visibilityState === "visible") belowKick(); });

// Mobile dock: appears (spring) once the hero's Download scrolls away.
const dock = document.getElementById("dock");
const dockY = new Spring(1.2, 0.5, 0.12);
let dockRaf = 0;
let dockLast = 0;
function dockFrame(now) {
  const dt = Math.min(0.05, (now - (dockLast || now)) / 1000);
  dockLast = now;
  dockY.step(dt);
  dock.style.transform = `translate3d(0, ${(dockY.x * 100).toFixed(2)}%, 0)`;
  dockRaf = dockY.resting ? 0 : requestAnimationFrame(dockFrame);
}
new IntersectionObserver(([en]) => {
  const show = !en.isIntersecting && en.boundingClientRect.top < 0;
  dock.setAttribute("aria-hidden", String(!show));
  dock.querySelector("a").tabIndex = show ? 0 : -1;
  dockY.target = show ? 0 : 1.2;
  if (ctx.reduced) { dockY.snap(); dock.style.transform = `translate3d(0, ${dockY.x * 100}%, 0)`; return; }
  if (!dockRaf) { dockLast = 0; dockRaf = requestAnimationFrame(dockFrame); }
}).observe(document.getElementById("download"));

// Touch: give presses a held state on the download keys too.
document.querySelectorAll(".btn-download").forEach((a) => {
  a.addEventListener("pointerdown", () => a.classList.add("is-pressed"));
  ["pointerup", "pointercancel", "pointerleave"].forEach((t) => a.addEventListener(t, () => a.classList.remove("is-pressed")));
});

// ───────── Frame-rate probe (used by verification) ─────────
window.__isocast = {
  ctx, take, s,
  sample(ms = 2000) {
    return new Promise((resolve) => {
      const times = [];
      const t0 = performance.now();
      const tick = (t) => {
        times.push(t);
        if (t - t0 < ms) requestAnimationFrame(tick);
        else {
          const d = times.slice(1).map((t, i) => t - times[i]).sort((a, b) => a - b);
          const pct = (p) => d[Math.min(d.length - 1, Math.floor(p * d.length))];
          resolve({ frames: d.length, fps: +(1000 / (d.reduce((a, b) => a + b, 0) / d.length)).toFixed(1), p50: +pct(0.5).toFixed(2), p95: +pct(0.95).toFixed(2), max: +d[d.length - 1].toFixed(2), over20ms: d.filter((x) => x > 20).length });
        }
      };
      requestAnimationFrame(tick);
    });
  },
};
// Automation (which runs in an isolated world) asks through DOM events and
// reads the answer from data attributes.
document.addEventListener("isocast:sample", () => {
  const ms = +root.dataset.sampleMs || 2000;
  delete root.dataset.fps;
  window.__isocast.sample(ms).then((r) => { root.dataset.fps = JSON.stringify(r); });
});
document.addEventListener("isocast:seek", () => {
  hero3d?.seek(+root.dataset.seek || 0);
  kick();
});
document.addEventListener("isocast:state", () => {
  root.dataset.state = JSON.stringify({ phase: hero3d?.phase(), tc: ctx.tc, reduced: ctx.reduced, k: +ctx.k.toFixed(3), running: !!raf, enter: +s.enter.x.toFixed(3) });
});

mount();
