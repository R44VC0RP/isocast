// isocast.com — one loop, one clock, springs for everything that moves.
import { Spring, clamp, easeOutCubic, timecode } from "./spring.js";
import { mountHero } from "./hero.js";

const root = document.documentElement;
root.classList.add("js");

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
// A 04:12 take, played back in real time. Dragging scrubs it.
const take = {
  dur: 252,
  pos: 97.4,
  pos0: 97.4,
  t0: performance.now(),
  frac: 0,
  introUntil: 0,
};

const ctx = {
  stage, inner, s, take,
  reduced: reducedMQ.matches,
  compact: compactMQ.matches,
  k: 1,
  w: 700, h: 560,
  hover: false,
  scrubbing: false,
  tc: { mmss: "00:00" },
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
function mount({ intro = true } = {}) {
  measure();
  if (hero3d) hero3d.destroy();
  inner.replaceChildren();
  hero3d = mountHero(ctx);
  if (intro && !ctx.reduced) {
    s.enter.snap(0);
    s.enter.target = 1;
    take.introUntil = performance.now() + 1300;
  } else {
    s.enter.snap(1);
  }
  kick();
}

function updateClock(now) {
  if (ctx.reduced) {
    take.pos = take.pos0; // calm version: nothing travels
  } else if (ctx.scrubbing) {
    take.t0 = now; // the finger holds the clock
  } else if (now < take.introUntil) {
    const t = 1 - (take.introUntil - now) / 1300;
    take.pos = take.pos0 * easeOutCubic(clamp(t, 0, 1));
    take.t0 = take.introUntil;
  } else {
    take.pos = (take.pos0 + Math.max(0, now - take.t0) / 1000) % take.dur;
  }
  take.frac = clamp(take.pos / take.dur, 0, 1);
  ctx.tc = timecode(take.pos);
}

// Scrub: move the one clock by a fraction of the take.
ctx.scrubBy = (df) => {
  const now = performance.now();
  const p = ((take.pos + df * take.dur) % take.dur + take.dur) % take.dur;
  take.pos = take.pos0 = p;
  take.t0 = now;
  take.introUntil = 0;
};

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
  }

  updateClock(now);
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

// Keyboard: with the stage focused, arrows scrub the clock (Shift for bigger steps).
stage.tabIndex = 0;
stage.addEventListener("keydown", (e) => {
  if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
  e.preventDefault();
  const dir = e.key === "ArrowLeft" ? -1 : 1;
  ctx.scrubBy((dir * (e.shiftKey ? 10 : 1)) / take.dur);
  hero3d.onRelease(ctx);
  kick();
});

// Visibility: run the loop only while the hero can be seen.
document.addEventListener("visibilitychange", () => {
  visible = document.visibilityState === "visible";
  if (visible) kick();
});
new IntersectionObserver((entries) => {
  inView = entries[entries.length - 1].isIntersecting;
  if (inView) kick();
}, { rootMargin: "80px" }).observe(hero);

reducedMQ.addEventListener("change", () => {
  ctx.reduced = reducedMQ.matches;
  if (ctx.reduced) { s.tiltX.target = s.tiltY.target = 0; }
  mount({ intro: false });
});

// ───────── Below the hero ─────────
const io = new IntersectionObserver((entries) => {
  for (const en of entries) if (en.isIntersecting) { en.target.classList.add("is-in"); io.unobserve(en.target); }
}, { rootMargin: "0px 0px -12% 0px" });
document.querySelectorAll(".reveal").forEach((el) => io.observe(el));

const flow = document.getElementById("flow");
flow.querySelectorAll(".chip[data-src]").forEach((chip) => {
  const on = () => (flow.dataset.hot = chip.dataset.src);
  const off = () => delete flow.dataset.hot;
  chip.addEventListener("pointerenter", on);
  chip.addEventListener("pointerleave", off);
});

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
