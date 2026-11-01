// The three files of one take and its mic audio, and the small parts they're built from.

export const FILES = [
  { kind: "program", name: "program.mov" },
  { kind: "screen", name: "screen.mov" },
  { kind: "camera", name: "camera.mov" },
  { kind: "mic", name: "mic audio" },
];

export function h(tag, cls) {
  const el = document.createElement(tag);
  if (cls) el.className = cls;
  return el;
}

const SVGNS = "http://www.w3.org/2000/svg";
export function svg(tag, attrs) {
  const el = document.createElementNS(SVGNS, tag);
  if (attrs) for (const k in attrs) el.setAttribute(k, attrs[k]);
  return el;
}

// Fixed-width digits without the font's slashed tabular zero: one span per
// character, updated only when that character changes.
export class Ticker {
  constructor(el, text) {
    this.el = el;
    this.cells = [];
    this.text = "";
    this.set(text);
  }
  set(text) {
    if (text === this.text) return;
    while (this.cells.length < text.length) {
      const s = document.createElement("span");
      this.el.appendChild(s);
      this.cells.push(s);
    }
    for (let i = 0; i < text.length; i++) {
      const ch = text[i];
      if (this.text[i] !== ch) {
        this.cells[i].textContent = ch;
        this.cells[i].className = ch === ":" ? "tk-sep" : "tk-d";
      }
    }
    this.text = text;
  }
}

// The timecode badge that rides the playhead: the same mm:ss as every file.
export function tcBadge() {
  const el = h("div", "tc-badge");
  const t = new Ticker(el, "00:00");
  return { el, set(tc) { t.set(tc.mmss); } };
}

// The mic's waveform from real peaks (0-100): one bar per bucket, the
// loudest peak in it. Bars stay at least `bar` tall so silence still reads.
export function peakWave(peaks, w, hgt, { bar = 3.4, gap = 2.6, color = "#56ad9b" } = {}) {
  const n = Math.floor((w + gap) / (bar + gap));
  const el = svg("svg", { width: w, height: hgt, viewBox: `0 0 ${w} ${hgt}`, class: "wave", "aria-hidden": "true" });
  const g = svg("g", { fill: color });
  const mid = hgt / 2;
  for (let i = 0; i < n; i++) {
    const a = Math.floor((i * peaks.length) / n);
    const b = Math.max(a + 1, Math.floor(((i + 1) * peaks.length) / n));
    let m = 0;
    for (let j = a; j < b; j++) if (peaks[j] > m) m = peaks[j];
    const bh = Math.max(bar, (m / 100) * hgt * 0.94);
    g.appendChild(svg("rect", { x: (i * (bar + gap)).toFixed(1), y: (mid - bh / 2).toFixed(1), width: bar, height: bh.toFixed(1), rx: bar / 2 }));
  }
  el.appendChild(g);
  return el;
}

// Viewfinder corners over the camera file.
export function brackets() {
  const b = h("div", "cam-brackets");
  b.innerHTML = "<i></i><i></i><i></i><i></i>";
  return b;
}

// A thick macOS-style file window: header (mark, name, timecode) and body.
export function fileWindow(file, { w, bodyH, headH, colors, content }) {
  const el = h("div", "file");
  el.dataset.kind = file.kind;
  el.style.width = `${w}px`;
  el.style.setProperty("--top", colors[0]);
  el.style.setProperty("--bot", colors[1]);
  el.style.setProperty("--hl", colors[2]);

  const head = h("div", "file-head");
  head.style.height = `${headH}px`;
  const title = h("div", "file-title");
  const name = h("div", "file-name");
  name.textContent = file.name;
  title.append(h("div", "file-mark"), name);
  const tc = h("div", "file-tc");
  const ticker = new Ticker(tc, "00:00");
  head.append(title, tc);

  const bw = w - 16;
  const body = h("div", "file-body");
  body.style.height = `${bodyH}px`;
  body.appendChild(content(bw, bodyH));
  let dim = null, level = null;
  if (file.kind === "mic") {
    // Audio after the playhead reads as not yet played; the level at the
    // playhead rides along with it.
    dim = h("div", "wave-dim");
    level = h("div", "wave-level");
    body.append(dim, level);
  }
  const playhead = h("div", "playhead");
  const glare = h("div", "file-glare");
  body.append(playhead, glare);
  el.append(head, body);
  return { el, head, body, ticker, playhead, glare, dim, level, kind: file.kind };
}
