// A monitor's picture: the daemon's H.264 stream for monitors (`frames`),
// relayed by the shell's scheme handler (`/_frames/<target>`) and decoded
// by WebCodecs on the media engines; or JPEG frames (`/_frame/<target>`),
// one request in flight at a time, from daemons or Macs that can't stream.
// Runs in a worker (see `monitor-worker.js`) and shows each frame through
// a sink: a video track the page's <video> plays (the media player puts
// frames on screen itself, so however busy the page's main thread is, as
// during a drag, the picture keeps up), or a canvas drawn with WebGL
// (`painter.js`), an OffscreenCanvas in the worker or, where a canvas
// can't be handed to a worker, the page's own. It tells its owner what
// happens with `emit`:
//
// - `{type: "open", id, fps}`: a stream opened, named `id` to the shell
//   (`{"native": "frames_rate", "id", "fps"}` changes its rate);
// - `{type: "frame", revision}`: a frame was drawn (and the setup revision
//   its layout came from, for the program and preview, or null);
// - `{type: "error", message}`: why there is no picture.

import { paint } from "./painter.js";

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** After a stream fails to start, JPEG frames for this long, then retry. */
const STREAM_RETRY_MS = 30000;

// The stream's binary records (see `frames` in the daemon's api.rs): a
// 24-byte big-endian header (length of the rest, kind, 3 zero bytes, seq,
// width, height), then the payload.
const HEADER = 24;
const CONFIG = 1;
const KEY_FRAME = 2;
const FRAME = 3;
const END = 4;
const REVISION = 5;

/** Whether this web view decodes H.264 itself; else JPEG frames. */
const canStream = typeof VideoDecoder === "function" && typeof EncodedVideoChunk === "function";

/** Show `image` (a VideoFrame or an ImageBitmap) through `sink`: a canvas
 * or OffscreenCanvas, or `{writer}` of a video track, which takes the
 * frame. Returns whether the sink took it (else the caller closes it). */
function present(sink, image) {
  if (!sink.writer) {
    paint(sink, image);
    return false;
  }
  const frame = image instanceof VideoFrame ? image : new VideoFrame(image, { timestamp: Math.round(performance.now() * 1000) });
  if (sink.writer.desiredSize !== null && sink.writer.desiredSize <= 0) {
    // The player is behind: drop rather than queue (and lag).
    frame.close();
  } else {
    sink.writer.write(frame).catch(() => {});
  }
  return image instanceof VideoFrame;
}

/** Show pictures through `canvas` (a sink, see `present`). Returns
 * `watch({target, width, fps})` (start, or change what is shown; a new
 * rate alone changes nothing but the next stream's and the JPEG pace) and
 * `pause()` (let go of the stream until the next `watch`). */
export function createMonitor(canvas, emit) {
  let job = null;
  let run = null;

  const start = () => {
    run?.stop.abort();
    const current = { stop: new AbortController() };
    run = current;
    loop(current);
  };

  const loop = async (current) => {
    const signal = current.stop.signal;
    let jpegUntil = canStream ? 0 : Infinity;
    let shownError = null;
    const show = (message) => {
      if (message !== shownError) {
        shownError = message;
        emit({ type: "error", message });
      }
    };
    const onFrame = (revision) => {
      if (shownError !== null) show(null);
      emit({ type: "frame", revision: revision ?? null });
    };
    const jpeg = jpegFrames(canvas, () => job, onFrame, show);
    while (!signal.aborted && job) {
      if (performance.now() < jpegUntil) {
        await jpeg.next(signal);
        continue;
      }
      try {
        const result = await stream(canvas, job, signal, onFrame, emit);
        if (signal.aborted) break;
        if (result.fallback) {
          // The daemon can't stream (an older build, or no hardware
          // encoder free): JPEG frames for a while.
          jpegUntil = performance.now() + (result.forever ? Infinity : STREAM_RETRY_MS);
        } else if (result.ended) {
          show(result.ended);
          await sleep(1000);
        } else if (result.closed) {
          // The daemon went away or hung up: don't ask again at once.
          await sleep(500);
        }
      } catch (err) {
        if (signal.aborted) break;
        show(err?.message ?? String(err));
        await sleep(1000);
      }
    }
  };

  return {
    watch(next) {
      const same = job && run && !run.stop.signal.aborted && job.target === next.target && job.width === next.width;
      job = { target: next.target, width: next.width, fps: next.fps };
      if (!same) start();
    },
    pause() {
      job = null;
      run?.stop.abort();
      run = null;
    },
  };
}

/** Show `job`'s H.264 stream in `canvas` until it ends or `signal` aborts,
 * at `job.fps` (the owner changes the rate through the shell). Resolves to
 * `{}` (start again at once), `{closed}` when the daemon hung up, `{ended:
 * reason}`, or `{fallback}` for JPEG frames. */
async function stream(canvas, job, signal, onFrame, emit) {
  const { target, width, fps } = job;
  // Names this stream to the shell; unique enough within a page.
  const id = Math.floor(Math.random() * 2 ** 52);
  let decoder = null;
  let frames = 0;
  try {
    const url = new URL(`/_frames/${encodeURIComponent(target)}?w=${width}&fps=${fps}&id=${id}`, location.href);
    const response = await fetch(url, { signal });
    if (response.status === 501) return { fallback: true, forever: true };
    if (!response.ok) throw new Error((await response.text()) || `No picture (${response.status})`);
    emit({ type: "open", id, fps });
    let failed = null;
    const aborted = new AbortController();
    // The setup revision of frames sent to the decoder, by timestamp.
    const revisions = new Map();
    let revision = null;
    decoder = new VideoDecoder({
      output: (frame) => {
        const shown = revisions.get(frame.timestamp);
        for (const [time] of revisions) {
          if (time > frame.timestamp) break;
          revisions.delete(time);
        }
        if (!present(canvas, frame)) frame.close();
        frames++;
        onFrame(shown);
      },
      error: (err) => {
        failed = err;
        aborted.abort();
      },
    });
    const reader = response.body.getReader();
    const stopReading = () => reader.cancel().catch(() => {});
    aborted.signal.addEventListener("abort", stopReading);
    signal.addEventListener("abort", stopReading);
    const records = new Records();
    for (;;) {
      let chunk;
      try {
        chunk = await reader.read();
      } catch (err) {
        if (failed) return { fallback: true };
        if (signal.aborted) return {};
        throw err;
      }
      if (failed) return { fallback: true };
      if (signal.aborted) return {};
      if (chunk.done) return { closed: true };
      for (const record of records.push(chunk.value)) {
        if (record.kind === CONFIG) {
          decoder.configure({
            codec: codecName(record.payload),
            description: record.payload,
            codedWidth: record.width,
            codedHeight: record.height,
            hardwareAcceleration: "prefer-hardware",
            optimizeForLatency: true,
          });
        } else if (record.kind === REVISION) {
          revision = Number(new DataView(record.payload.buffer, record.payload.byteOffset, 8).getBigUint64(0));
        } else if (record.kind === KEY_FRAME || record.kind === FRAME) {
          if (decoder.state !== "configured") continue;
          const timestamp = Number(record.seq / 1000n);
          if (revision != null) revisions.set(timestamp, revision);
          decoder.decode(
            new EncodedVideoChunk({
              type: record.kind === KEY_FRAME ? "key" : "delta",
              timestamp,
              data: record.payload,
            }),
          );
        } else if (record.kind === END) {
          const reason = new TextDecoder().decode(record.payload);
          // Nothing shown yet: the encoder couldn't start, so use JPEG.
          return frames ? { ended: reason } : { fallback: true };
        }
      }
    }
  } catch (err) {
    if (signal.aborted) return {};
    throw err;
  } finally {
    if (decoder && decoder.state !== "closed") decoder.close();
  }
}

/** Splits the stream's bytes, however they arrive, into records. */
class Records {
  constructor() {
    this.pending = null;
  }

  *push(chunk) {
    let bytes = chunk;
    if (this.pending) {
      bytes = new Uint8Array(this.pending.length + chunk.length);
      bytes.set(this.pending);
      bytes.set(chunk, this.pending.length);
      this.pending = null;
    }
    let at = 0;
    while (bytes.length - at >= HEADER) {
      const view = new DataView(bytes.buffer, bytes.byteOffset + at, HEADER);
      const length = view.getUint32(0);
      if (bytes.length - at < 4 + length) break;
      yield {
        kind: view.getUint8(4),
        seq: view.getBigUint64(8),
        width: view.getUint32(16),
        height: view.getUint32(20),
        payload: bytes.subarray(at + HEADER, at + 4 + length),
      };
      at += 4 + length;
    }
    if (at < bytes.length) this.pending = bytes.slice(at);
  }
}

/** The WebCodecs codec string (`avc1.PPCCLL`) of an avcC record. */
function codecName(avcc) {
  const hex = (n) => n.toString(16).padStart(2, "0");
  return `avc1.${hex(avcc[1])}${hex(avcc[2])}${hex(avcc[3])}`;
}

/** JPEG frames of the current job, for daemons that can't stream: each
 * `next()` shows one (waiting for it), at most `job.fps` a second. */
function jpegFrames(canvas, currentJob, onFrame, show) {
  let after = null;
  let lastAsked = 0;
  return {
    async next(signal) {
      const job = currentJob();
      if (!job) return;
      const wait = 1000 / job.fps - (performance.now() - lastAsked);
      if (wait > 0) await sleep(wait);
      lastAsked = performance.now();
      try {
        const query = `w=${job.width}${after != null ? `&after=${after}` : ""}`;
        const url = new URL(`/_frame/${encodeURIComponent(job.target)}?${query}`, location.href);
        const response = await fetch(url, { signal });
        // Nothing new within the long poll: ask again.
        if (response.status === 204) return;
        if (!response.ok) {
          show((await response.text()) || `No picture (${response.status})`);
          await sleep(1000);
          return;
        }
        after = Number(response.headers.get("X-Isocast-Seq")) || after;
        const revision = response.headers.get("X-Isocast-Revision");
        const bitmap = await createImageBitmap(await response.blob());
        if (signal.aborted) {
          bitmap.close();
          return;
        }
        present(canvas, bitmap);
        bitmap.close();
        onFrame(revision == null ? null : Number(revision));
      } catch (err) {
        if (signal.aborted) return;
        show(err?.message ?? String(err));
        await sleep(1000);
      }
    },
  };
}
