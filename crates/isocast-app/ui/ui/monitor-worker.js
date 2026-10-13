// A monitor's picture in a worker of its own (see `monitor-core.js`): the
// page hands over its canvas as an OffscreenCanvas, then says what to show
// (`watch`) or to let go (`pause`); the worker reads the stream, decodes
// and shows frames without waiting on the page's main thread. Where it
// can, it shows them as a video track (sent back as `{type: "track"}` for
// the page's <video>), which reaches the screen without the page's main
// thread too; else it draws in the canvas.

import { createMonitor } from "./monitor-core.js";

let monitor = null;

onmessage = ({ data }) => {
  if (data.type === "init") {
    let sink = data.canvas;
    if (typeof VideoTrackGenerator === "function") {
      const generator = new VideoTrackGenerator();
      sink = { writer: generator.writable.getWriter() };
      postMessage({ type: "track", track: generator.track }, [generator.track]);
    }
    monitor = createMonitor(sink, (message) => postMessage(message));
  } else if (data.type === "watch") {
    monitor?.watch(data);
  } else if (data.type === "pause") {
    monitor?.pause();
  }
};
