// The status bar along the bottom of the window, as in OBS: while live,
// dropped frames, the bitrate and the connection's health; then the
// LIVE and REC timers, the engine's CPU use and its frame rate. LIVE and
// REC open their details (REC also has the marker and split).

import { useEffect, useRef, useState } from "preact/hooks";
import { html } from "../lib/html.js";
import { setState, useStore } from "../lib/store.js";
import { findSource, sourceName } from "../lib/model.js";
import { Icon } from "./icons.js";
import { Popover } from "./popover.js";
import { addMarker, splitRecording } from "./controls-dock.js";

const anchorOf = (el) => {
  const r = el.getBoundingClientRect();
  return { left: r.left, top: r.top, right: r.right, bottom: r.bottom };
};

// When the stream and the recording started, by this computer's clock,
// from the daemon's elapsed seconds: kept between renders, so the timers
// tick on their own between status readings and don't jump.
const started = { stream: null, recording: null };

/** The start time (ms) for `which`, from `elapsed` seconds as of now. */
function since(which, elapsed) {
  if (elapsed == null) return started[which];
  const at = Date.now() - elapsed * 1000;
  if (started[which] == null || Math.abs(started[which] - at) > 1500) started[which] = at;
  return started[which];
}

/** When the stream went live (ms), from its elapsed time or `since`. */
function liveSince(stream) {
  if (!stream) return null;
  if (stream.elapsed_seconds != null) return since("stream", stream.elapsed_seconds);
  const at = stream.since ? Date.parse(stream.since) : NaN;
  return Number.isFinite(at) ? at : null;
}

export function formatKbps(kbps) {
  if (!kbps) return "0 kb/s";
  return kbps >= 1000 ? `${(kbps / 1000).toFixed(1)} Mb/s` : `${Math.round(kbps)} kb/s`;
}

/** A count with digit grouping (22,620). */
const count = (n) => Number(n ?? 0).toLocaleString();

const SERVICES = { twitch: "Twitch", youtube: "YouTube", kick: "Kick", facebook: "Facebook", custom: "Custom server" };

/** While streaming: where to, and how it is keeping up. */
export function StreamDetails() {
  const anchor = useStore((s) => s.streamDetails);
  const stream = useStore((s) => s.stream);
  if (!anchor || !stream || stream.state === "offline") return null;
  const close = () => setState({ streamDetails: null });
  const row = (label, value, cls = "") => html`<div class="kv"><span class="dim">${label}</span><span class=${cls}>${value}</span></div>`;
  const congestion = Math.round((stream.congestion ?? 0) * 100);
  return html`<${Popover} anchor=${anchor} align="right" onClose=${close} class="details">
    <div class="menu-label">Stream</div>
    <div class="kv-list tabular">
      ${row("To", `${SERVICES[stream.service] ?? stream.service} · ${stream.server}`, "ellipsis")}
      ${row("Picture", stream.width ? `${stream.width}×${stream.height} · ${stream.fps} fps` : "—")}
      ${row("Video", `${formatKbps(stream.video_kbps)} of ${formatKbps(stream.target_video_kbps)}`)}
      ${row("Audio", formatKbps(stream.audio_kbps))}
      ${row("Frames sent", count(stream.frames_sent))}
      ${row("Dropped", count(stream.dropped_frames), stream.dropped_frames ? "bad" : "")}
      ${row("Backed up", `${congestion}%`, congestion > 50 ? "warn" : "")}
      ${row("Reconnects", stream.reconnects ?? 0, stream.reconnects ? "warn" : "")}
      ${stream.last_error && row("Last error", stream.last_error, "bad")}
    </div>
  <//>`;
}

/** While recording: each file's counters, and marker and split. */
export function RecordingDetails() {
  const anchor = useStore((s) => s.details);
  const status = useStore((s) => s.status);
  const setup = useStore((s) => s.setup);
  const [label, setLabel] = useState("");
  if (!anchor || status?.state !== "recording") return null;
  const close = () => setState({ details: null });
  const files = status.live?.files ?? [];
  const mark = async () => {
    await addMarker(label.trim() || "Marker");
    setLabel("");
  };
  return html`<${Popover} anchor=${anchor} align="right" onClose=${close} class="details">
    <div class="menu-label">Files</div>
    <div class="file-table tabular">
      ${files.map(
        (f) => html`<div class="file-row" key=${f.source}>
          <span class="ellipsis">${f.source === "program" ? "Program" : sourceName(findSource(setup, f.source)) || f.source}</span>
          <span class="dim">${count(f.frames)} frames</span>
          <span class=${f.dropped_frames ? "bad" : "dim"}>${count(f.dropped_frames)} dropped</span>
          <span class=${f.audio_dropouts ? "warn" : "dim"} title="Times audio was missing and filled with silence">
            ${f.audio_dropouts} gap${f.audio_dropouts === 1 ? "" : "s"}
          </span>
        </div>`,
      )}
    </div>
    <div class="menu-sep" />
    <div class="details-actions">
      <div class="field grow"><input placeholder="Marker label" value=${label} onInput=${(e) => setLabel(e.currentTarget.value)}
        onKeyDown=${(e) => e.key === "Enter" && mark()} /></div>
      <button class="btn" onClick=${mark}><${Icon} name="flag" />Mark</button>
      <button class="btn" title="End these files at the next whole second and carry on in new ones" onClick=${splitRecording}><${Icon} name="scissors" />Split</button>
    </div>
  <//>`;
}

/** Seconds since `since` (ms), ticking once a second, just after each
 * whole second, while `active`. */
function useElapsed(active, since) {
  const [now, setNow] = useState(Date.now());
  // Read at each tick: a stream's start is worked out afresh each render.
  const sinceRef = useRef(since);
  sinceRef.current = since;
  useEffect(() => {
    if (!active) return undefined;
    let timer;
    const tick = () => {
      const at = Date.now();
      setNow(at);
      timer = setTimeout(tick, 1000 - ((at - (sinceRef.current ?? 0)) % 1000) + 5);
    };
    tick();
    return () => clearTimeout(timer);
  }, [active]);
  return active && since ? Math.max(0, (now - since) / 1000) : 0;
}

export function clock(seconds) {
  const s = Math.floor(seconds);
  const pad = (n) => String(n).padStart(2, "0");
  return `${pad(Math.floor(s / 3600))}:${pad(Math.floor(s / 60) % 60)}:${pad(s % 60)}`;
}

function RecTimer() {
  const { state, elapsed, files } = useStore((s) => ({
    state: s.status?.state ?? "idle",
    elapsed: s.status?.live?.elapsed_seconds,
    files: s.status?.live?.files,
  }));
  const recording = state === "recording";
  if (!recording) started.recording = null;
  const seconds = useElapsed(recording, recording ? since("recording", elapsed) : null);
  const dropped = (files ?? []).reduce((n, f) => n + (f.dropped_frames ?? 0), 0);
  const gaps = (files ?? []).reduce((n, f) => n + (f.audio_dropouts ?? 0), 0);
  const title = recording
    ? `Recording ${files?.length ?? 0} file${files?.length === 1 ? "" : "s"}: ${dropped} frames dropped, ${gaps} audio gaps. Click for details, markers and splits.`
    : "Not recording";
  return html`<button
    class=${"status-item rec" + (recording ? " on" : "") + (state === "starting" ? " starting" : "")}
    title=${title}
    disabled=${!recording}
    onClick=${(e) => setState({ details: anchorOf(e.currentTarget) })}
  >
    <span class="status-dot" />
    <span>REC</span>
    <span class="tabular">${clock(seconds)}</span>
    ${dropped > 0 && html`<span class="bad tabular">${count(dropped)} dropped</span>`}
  </button>`;
}

/** The stream's health, as OBS colours it: green, amber while it backs up
 * or reconnects, red when it fails or drops a lot. */
function health(stream) {
  if (!stream) return "off";
  if (stream.state === "failed") return "bad";
  if (stream.state === "reconnecting" || stream.state === "connecting") return "warn";
  const sent = (stream.frames_sent ?? 0) + (stream.dropped_frames ?? 0);
  const dropRate = sent ? (stream.dropped_frames ?? 0) / sent : 0;
  if (dropRate > 0.05 || (stream.congestion ?? 0) > 0.8) return "bad";
  if (dropRate > 0.01 || (stream.congestion ?? 0) > 0.5) return "warn";
  return "ok";
}

/** A square light (not a dot, so it never reads as REC), and a word
 * when the connection isn't fine. */
function Health({ stream }) {
  const level = health(stream);
  const state = stream?.state ?? "offline";
  const word =
    level === "ok" || level === "off"
      ? null
      : { reconnecting: "Reconnecting", connecting: "Connecting", failed: "Failed" }[state] ?? (level === "bad" ? "Poor connection" : "Unstable");
  return html`<span class=${"status-item" + (level === "warn" ? " warn" : "")} title=${`Connection: ${word ?? state}`}>
    <span class=${"health " + level} />
    ${word && html`<span class=${level === "bad" ? "bad" : ""}>${word}</span>`}
  </span>`;
}

function StreamItems() {
  const stream = useStore((s) => s.stream);
  const state = stream?.state ?? "offline";
  const streaming = state === "live" || state === "reconnecting";
  if (!streaming) started.stream = null;
  const seconds = useElapsed(streaming, streaming ? liveSince(stream) : null);
  const on = streaming || state === "connecting";
  const dropped = stream?.dropped_frames ?? 0;
  const total = dropped + (stream?.frames_sent ?? 0);
  const open = (e) => setState({ streamDetails: anchorOf(e.currentTarget) });
  return html`
    ${on &&
    html`<button class="status-item" title="Stream frames that couldn’t be sent (the program and recordings keep them)" onClick=${open}>
        <span>Dropped Frames</span>
        <span class=${"tabular" + (dropped ? " bad" : "")}>${count(dropped)} (${total ? ((dropped / total) * 100).toFixed(1) : "0.0"}%)</span>
      </button>
      <button class="status-item" title="Sent over the last second" onClick=${open}>
        <span class="tabular">${formatKbps(stream.total_kbps)}</span>
      </button>
      <${Health} stream=${stream} />`}
    <button class=${"status-item live" + (on ? " on" : "")} title=${on ? "Streaming: click for details" : "Not streaming"} disabled=${!on} onClick=${open}>
      <${Icon} name="live" size=${14} />
      <span>LIVE</span>
      <span class="tabular">${clock(seconds)}</span>
    </button>
  `;
}

function Performance() {
  const { cpu, fps, target, missed, running } = useStore((s) => ({
    cpu: s.status?.cpu_percent,
    fps: s.status?.engine?.fps,
    target: s.status?.engine?.canvas?.fps ?? s.setup?.canvas?.fps,
    missed: s.status?.engine?.missed_frames ?? 0,
    running: !!s.status?.engine,
  }));
  // Amber when the program falls clearly behind the canvas's rate.
  const lagging = running && fps > 0 && target && fps < target * 0.95;
  return html`
    <span class="status-item" title="The engine’s share of this Mac’s processor (all cores)">
      <span>CPU:</span><span class="tabular">${cpu == null ? "—" : `${cpu.toFixed(1)}%`}</span>
    </span>
    <span
      class=${"status-item" + (lagging ? " warn" : "")}
      title=${running
        ? `Program frames drawn per second, of the canvas’s ${target}. ${missed} missed since the engine started.`
        : "The engine isn’t running"}
    >
      <span class="tabular">${running && fps != null ? fps.toFixed(2) : "—"} / ${target != null ? Number(target).toFixed(2) : "—"} FPS</span>
    </span>
  `;
}

export function StatusBar() {
  return html`<footer class="statusbar">
    <div class="status-left" />
    <div class="status-right">
      <${StreamItems} />
      <${RecTimer} />
      <${Performance} />
    </div>
  </footer>`;
}
