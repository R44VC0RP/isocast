// What each layer of the hero shows: the one place to swap content.
//
// Each layer is a short, silent, looping video (`video`: a basename; the
// files are `<base>-<size>.mp4`, `.webm` and a `.webp` poster at every size in
// `sizes`), or a still picture (`image`: `<base>-<size>.webp`). The mic layer
// draws a waveform from peaks (`peaks`: JSON with `peaks`, 4 per 60 fps frame
// over the loop, and `overview`, the whole take at 30 per second).
//
// All the videos are cut from the same stretch of one take, so they share one
// clock: the first video layer drives the others, and the hero's copper pin,
// playheads and timecodes read it. With no video layers, a plain timer runs.
//
// To swap in a new take: encode the loops and peaks
// (scratch/harness/encode.sh and peaks.py), update TAKE below, and point the
// layers at the new basenames.

export const TAKE = {
  start: 20.1667, // where the loop starts in the original take (s): timecodes show this
  length: 3.4, // loop length (s)
  total: 26.233, // the whole take (s): every file is this long
};

export const LAYERS = {
  program: { video: "media/program" },
  screen: { video: "media/screen" },
  camera: { video: "media/camera" },
  mic: { peaks: "media/mic-peaks.json" },
};
export const SIZES = [640, 960]; // encoded widths, about 2x the drawn size

const SOFT = 0.04; // drift (s) corrected by nudging the playback rate
const HARD = 0.25; // drift (s) corrected by seeking

function makeVideo(base, size) {
  const v = document.createElement("video");
  v.className = "take-video";
  v.muted = true;
  v.defaultMuted = true;
  v.loop = true;
  v.playsInline = true;
  v.preload = "none"; // nothing loads until the page has painted (attach())
  v.disablePictureInPicture = true;
  v.disableRemotePlayback = true;
  for (const a of ["muted", "playsinline", "loop"]) v.setAttribute(a, "");
  v.setAttribute("aria-hidden", "true");
  v.setAttribute("tabindex", "-1");
  v.poster = `${base}-${size}.webp`;
  v.width = size;
  v.height = (size * 9) / 16;
  return v;
}

function attachSources(v, base, size) {
  // H.264 first: hardware decoding everywhere. VP9 only where there's no H.264.
  for (const [type, ext] of [["video/mp4", "mp4"], ["video/webm", "webm"]]) {
    const s = document.createElement("source");
    s.type = type;
    s.src = `${base}-${size}.${ext}`;
    v.appendChild(s);
  }
  v.preload = "auto";
  v.load();
}

function makeStill(base, size) {
  const img = document.createElement("img");
  img.className = "take-video";
  img.src = `${base}-${size}.webp`;
  img.width = size;
  img.height = (size * 9) / 16;
  img.alt = "";
  img.decoding = "async";
  img.draggable = false;
  return img;
}

// The page counts as painted once `load` has fired; videos wait for it so the
// first paint never competes with them.
let pageLoaded = document.readyState === "complete";
const onLoad = [];
if (!pageLoaded) {
  addEventListener("load", () => {
    pageLoaded = true;
    for (const f of onLoad.splice(0)) f();
  }, { once: true });
}

// size: an entry of SIZES; still: posters only (reduced motion, data saver).
export function createTake({ size, still }) {
  const L = TAKE.length;
  const els = {};
  const videos = [];
  const bases = new Map();
  for (const [kind, layer] of Object.entries(LAYERS)) {
    if (layer.video) {
      const el = still ? makeStill(layer.video, size) : makeVideo(layer.video, size);
      els[kind] = el;
      if (!still) { videos.push(el); bases.set(el, layer.video); }
    } else if (layer.image) {
      els[kind] = makeStill(layer.image, size);
    }
  }
  const master = videos[0] || null;

  let attached = false;
  let started = false;
  let wanted = false; // visible and allowed to play
  let failed = false; // autoplay refused (Low Power Mode): stay on posters
  let scrubT = null;
  let lastT = 0;
  // Timer clock when there is no video to follow.
  let t0 = performance.now();
  const pending = new Set();

  const wrap = (d) => ((d % L) + L * 1.5) % L - L / 2;
  const playing = () => master && !still && !failed && started;

  function attach() {
    if (attached || !master || still || !wanted) return;
    if (!pageLoaded) { onLoad.push(attach); return; }
    attached = true;
    for (const v of videos) attachSources(v, bases.get(v), size);
    playAll();
  }

  async function playAll() {
    if (!attached || still || failed || !wanted || scrubT != null) return;
    // Start them together. (iOS only buffers once play() is called, so don't
    // wait for data first; sync() lines them up as they come in.)
    started = true;
    try {
      await Promise.all(videos.map((v) => v.play()));
    } catch (e) {
      if (e && e.name === "NotAllowedError") {
        failed = true;
        for (const v of videos) { v.pause(); v.currentTime = 0; }
      }
    }
  }
  function pauseAll() {
    for (const v of videos) if (!v.paused) v.pause();
  }
  for (const v of videos) v.addEventListener("seeked", () => pending.delete(v));

  function seekAll(t) {
    for (const v of videos) {
      if (pending.has(v)) continue; // one seek at a time per video: smooth scrubbing
      pending.add(v);
      v.currentTime = t;
    }
  }

  return {
    els,
    still,
    get failed() { return failed; },
    // The one clock, in loop seconds [0, L).
    time() {
      if (scrubT != null) return scrubT;
      if (!master) {
        if (still || !wanted) return lastT;
        lastT = (((performance.now() - t0) / 1000) % L + L) % L;
        return lastT;
      }
      if (!playing()) return lastT;
      lastT = master.currentTime % L;
      return lastT;
    },
    // Keep every video on the first one's clock.
    sync() {
      if (!playing() || scrubT != null || master.paused) return;
      const t = master.currentTime;
      for (const v of videos) {
        if (v === master || v.paused || v.seeking) continue;
        const d = wrap(v.currentTime - t);
        if (Math.abs(d) > HARD) { v.currentTime = t; v.playbackRate = 1; }
        else if (Math.abs(d) > SOFT) v.playbackRate = d > 0 ? 0.94 : 1.06;
        else if (v.playbackRate !== 1) v.playbackRate = 1;
      }
    },
    setVisible(on) {
      if (on && !wanted && !master) t0 = performance.now() - lastT * 1000;
      wanted = on;
      if (on) { attach(); playAll(); } else pauseAll();
    },
    scrubTo(t) {
      t = ((t % L) + L) % L;
      if (scrubT == null) pauseAll();
      scrubT = t;
      lastT = t;
      if (playing()) seekAll(t);
    },
    endScrub() {
      if (scrubT == null) return;
      const t = scrubT;
      scrubT = null;
      lastT = t;
      t0 = performance.now() - t * 1000;
      if (playing()) {
        for (const v of videos) v.currentTime = t;
        playAll();
      }
    },
    // Free the decoders when the hero is rebuilt.
    destroy() {
      wanted = false;
      for (const v of videos) {
        v.pause();
        v.replaceChildren();
        v.removeAttribute("src");
        v.load();
      }
    },
  };
}

// The mic layer's peaks.
export async function loadPeaks() {
  const url = LAYERS.mic && LAYERS.mic.peaks;
  if (!url) return null;
  try {
    const r = await fetch(url);
    if (!r.ok) return null;
    return await r.json();
  } catch {
    return null;
  }
}
