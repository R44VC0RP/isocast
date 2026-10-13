// A monitor: the live picture of the program, the preview or a source,
// letterboxed to the canvas. The picture comes from `monitor-core.js` in a
// worker of its own (`monitor-worker.js`), so it keeps up however busy
// this page is (a drag re-renders it every frame): the daemon's H.264
// stream for monitors at the monitor's own pixel width, decoded by
// WebCodecs, or JPEG frames from daemons or Macs that can't stream; shown
// as a video track in a <video> (the media player puts frames on screen
// without this page's main thread), else drawn with WebGL in the canvas.
// At the canvas's rate (at most 60), as in OBS; or, if the viewer chose to
// save power (Settings → General), 30 frames a second except while the
// user works on the canvas over it (the stream changes rate in place, no
// new keyframe). Nothing is fetched while the window is hidden, and a
// resize asks for the new width.

import { createContext } from "preact";
import { useEffect, useRef, useState } from "preact/hooks";
import { html } from "../lib/html.js";
import { getState, preference, savePreference, setState, useStore } from "../lib/store.js";
import { native } from "../lib/bridge.js";
import { createMonitor } from "./monitor-core.js";

/** The most frames a second a monitor shows (the canvas's rate, if lower). */
const MAX_FPS = 60;
/** Frames a second at rest when saving power; the canvas's rate while the
 * user drags, resizes, crops or nudges on the canvas. */
const POWER_SAVING_FPS = 30;
/** How long after letting go (or a nudge) monitors keep the faster rate. */
const GESTURE_TAIL_MS = 500;

/** The preview frame rate choices (Settings → General), per viewer. */
export const PREVIEW_RATES = [
  ["canvas", "Match canvas"],
  ["30", "30 fps (saves power)"],
];

/** This viewer's preview frame rate choice: "canvas" or "30". */
export const previewRate = () => getState().previewRate ?? preference("previewRate", "canvas");

/** Keep `rate` as this viewer's choice; monitors follow at once. */
export function setPreviewRate(rate) {
  savePreference("previewRate", rate);
  setState({ previewRate: rate });
}

/** The choice saved before this page loaded. */
const savedRate = preference("previewRate", "canvas");

/** How long after the last frame the picture counts as live. */
const SHOWN_FRESH_MS = 150;

/** The setup revision of the picture on screen under a monitor's children
 * (the canvas editor), while frames come; else null. */
export const ShownRevision = createContext(null);

/** The width to ask for: the canvas's pixels, in steps of 64. */
const pixelWidth = (canvas) => Math.min(1920, Math.max(256, Math.ceil((canvas.clientWidth * devicePixelRatio) / 64) * 64));

/** Whether a canvas can be handed to a worker to draw in. */
const offscreen = typeof HTMLCanvasElement.prototype.transferControlToOffscreen === "function";

/** The picture of `target` in the canvas while `enabled` and the window
 * shows, at most `fps` frames a second, telling `onShown` the setup
 * revision of each frame drawn (or null). Returns the last error (text)
 * and whether a frame has been drawn. */
function useFrames(target, canvasRef, enabled, fps, onShown) {
  const [error, setError] = useState(null);
  const [drawn, setDrawn] = useState(false);
  // A video track from the worker, shown by a <video> over the canvas.
  const [track, setTrack] = useState(null);
  const live = useRef({ fps, stream: null, monitor: null, watch: null });
  live.current.fps = fps;
  const shownTo = useRef(onShown);
  shownTo.current = onShown;

  // The drawing side, for as long as the canvas lives: a worker with the
  // canvas, or (where a canvas can't move) the same code on this thread.
  useEffect(() => {
    const canvas = canvasRef.current;
    const state = live.current;
    const receive = (message) => {
      if (message.type === "open") {
        state.stream = message.id;
        // The rate changed while the stream was starting.
        if (message.fps !== state.fps) native("frames_rate", { id: message.id, fps: state.fps }).catch(() => {});
      } else if (message.type === "frame") {
        setError(null);
        setDrawn(true);
        // A frame handed to the video track shows a display frame later
        // than one drawn in the canvas: the editor's boxes follow then.
        if (state.video) requestAnimationFrame(() => shownTo.current?.(message.revision));
        else shownTo.current?.(message.revision);
      } else if (message.type === "error") {
        setError(message.message);
      } else if (message.type === "track") {
        state.video = true;
        setTrack(message.track);
      }
    };
    if (offscreen) {
      const worker = new Worker(new URL("./monitor-worker.js", import.meta.url), { type: "module" });
      const surface = canvas.transferControlToOffscreen();
      worker.postMessage({ type: "init", canvas: surface }, [surface]);
      worker.onmessage = ({ data }) => receive(data);
      worker.onerror = (event) => setError(event.message || "The monitor stopped");
      state.monitor = {
        watch: (job) => worker.postMessage({ type: "watch", ...job }),
        pause: () => worker.postMessage({ type: "pause" }),
      };
      return () => worker.terminate();
    }
    const monitor = createMonitor(canvas, receive);
    state.monitor = monitor;
    return () => monitor.pause();
  }, []);

  // What to show: the target at the canvas's width, while enabled and the
  // window shows; again on resizes and new rates.
  useEffect(() => {
    const canvas = canvasRef.current;
    const state = live.current;
    if (!enabled) {
      state.monitor?.pause();
      state.stream = null;
      return undefined;
    }
    let width = 0;
    const update = () => {
      if (document.hidden || !canvas.clientWidth) {
        width = 0;
        state.stream = null;
        state.monitor?.pause();
        return;
      }
      const next = pixelWidth(canvas);
      if (next === width) return;
      width = next;
      state.stream = null;
      state.monitor?.watch({ target, width, fps: state.fps });
    };
    state.watch = () => width && state.monitor?.watch({ target, width, fps: state.fps });
    const observer = new ResizeObserver(update);
    observer.observe(canvas);
    document.addEventListener("visibilitychange", update);
    update();
    return () => {
      observer.disconnect();
      document.removeEventListener("visibilitychange", update);
      state.watch = null;
      state.stream = null;
      state.monitor?.pause();
    };
  }, [target, enabled]);

  // A new rate: the open stream changes in place; later streams and JPEG
  // frames start at it.
  useEffect(() => {
    const state = live.current;
    if (state.stream != null) native("frames_rate", { id: state.stream, fps }).catch(() => {});
    state.watch?.();
  }, [fps]);

  return { error, drawn, track };
}

/** Where a <video> playing the worker's track sits: over the canvas. */
const VIDEO_STYLE = {
  position: "absolute",
  inset: "0",
  width: "100%",
  height: "100%",
  display: "block",
  objectFit: "fill",
  pointerEvents: "none",
};

/** A <video> playing `track`, which the monitor's worker fills. */
function TrackVideo({ track }) {
  const ref = useRef();
  useEffect(() => {
    const video = ref.current;
    video.srcObject = new MediaStream([track]);
    video.play().catch(() => {});
    // WebKit pauses it while the window is hidden (covered, minimized);
    // the stream comes back when it shows, and so must playback.
    const resume = () => {
      if (!document.hidden && video.paused) video.play().catch(() => {});
    };
    document.addEventListener("visibilitychange", resume);
    return () => {
      document.removeEventListener("visibilitychange", resume);
      video.srcObject = null;
    };
  }, [track]);
  return html`<video ref=${ref} muted autoplay playsinline disablepictureinpicture style=${VIDEO_STYLE} />`;
}

/** Whether the user is working on the canvas over `ref`'s element: from a
 * press there until a moment after letting go, and for a moment after
 * each arrow-key nudge (outside text fields). */
function useGesture(ref, enabled) {
  const [active, setActive] = useState(false);
  useEffect(() => {
    const element = ref.current;
    if (!enabled || !element) return undefined;
    let timer = null;
    let pressed = false;
    const start = () => {
      clearTimeout(timer);
      setActive(true);
    };
    const settle = () => {
      clearTimeout(timer);
      timer = setTimeout(() => setActive(false), GESTURE_TAIL_MS);
    };
    const down = (event) => {
      if (event.button !== 0) return;
      pressed = true;
      start();
    };
    const up = () => {
      if (!pressed) return;
      pressed = false;
      settle();
    };
    const key = (event) => {
      if (!event.key.startsWith("Arrow") || event.target?.closest?.("input, textarea, select, [contenteditable]")) return;
      start();
      settle();
    };
    element.addEventListener("pointerdown", down, true);
    window.addEventListener("pointerup", up, true);
    window.addEventListener("pointercancel", up, true);
    window.addEventListener("keydown", key, true);
    return () => {
      clearTimeout(timer);
      element.removeEventListener("pointerdown", down, true);
      window.removeEventListener("pointerup", up, true);
      window.removeEventListener("pointercancel", up, true);
      window.removeEventListener("keydown", key, true);
      setActive(false);
    };
  }, [enabled]);
  return active;
}

/** One monitor with its heading. `children` (the canvas editor) sit over
 * the picture; when saving power, working on them raises the monitor to
 * the canvas's rate. */
export function Monitor({ target, tag, tagClass, name, extra, live, enabled = true, empty, children }) {
  const canvasRef = useRef();
  const monitorRef = useRef();
  const connected = useStore((s) => s.connection.state === "connected");
  const canvasFps = useStore((s) => s.setup?.canvas?.fps) ?? MAX_FPS;
  const savingPower = (useStore((s) => s.previewRate) ?? savedRate) === "30";
  const on = enabled && connected && !empty;
  const gesturing = useGesture(monitorRef, on && !!children && savingPower);
  const fps = Math.max(1, Math.min(canvasFps, savingPower && !gesturing ? POWER_SAVING_FPS : MAX_FPS));
  const shown = useShown(!!children);
  const { error, drawn, track } = useFrames(target, canvasRef, on, fps, shown.onFrame);
  return html`<div class="monitor-slot">
    <div class="monitor-head">
      ${tag && html`<span class=${"tag " + (tagClass ?? "")}>${tag}</span>`}
      <span class="name ellipsis">${name}</span>
      ${extra && html`<span class="extra">${extra}</span>`}
    </div>
    <div class=${"monitor" + (live ? " program-live" : "")} ref=${monitorRef}>
      <canvas ref=${canvasRef} />
      ${track && html`<${TrackVideo} track=${track} />`}
      ${(empty || !drawn || (error && !drawn)) &&
      html`<div class="placeholder">
        ${empty ? empty : error ? error : html`<div class="spinner" />`}
      </div>`}
      <${ShownRevision.Provider} value=${shown.revision}>${children}<//>
    </div>
  </div>`;
}

/** The setup revision of the frame on screen, for children that draw over
 * it (when `wanted`); null when unknown or when frames stop coming. */
function useShown(wanted) {
  const [revision, setRevision] = useState(null);
  const state = useRef({ revision: null, timer: null });
  const onFrame = useRef((next) => {
    const current = state.current;
    clearTimeout(current.timer);
    if (next !== current.revision) {
      current.revision = next;
      setRevision(next);
    }
    current.timer = setTimeout(() => {
      current.revision = null;
      setRevision(null);
    }, SHOWN_FRESH_MS);
  }).current;
  useEffect(() => () => clearTimeout(state.current.timer), []);
  return { revision: wanted ? revision : null, onFrame: wanted ? onFrame : null };
}
