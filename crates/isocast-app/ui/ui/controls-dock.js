// The Controls dock: OBS's stacked buttons. Start Streaming and Start
// Recording (Stop … while running; beside Stop Recording, Isocast's marker
// and split), Studio Mode, Settings and Exit.

import { html } from "../lib/html.js";
import { call, native } from "../lib/bridge.js";
import { useStore } from "../lib/store.js";
import { failed, setStudio, toast, toggleRecording, toggleStream } from "../lib/session.js";
import { Dock } from "./docks.js";
import { Icon } from "./icons.js";
import { openSettings } from "./settings.js";

export async function addMarker(label = "Marker") {
  try {
    const marked = await call("marker", { label });
    toast("success", `Marked “${marked.label}”`, `Take ${marked.take} at ${marked.time_seconds.toFixed(1)} s`);
  } catch (error) {
    failed("Couldn’t add the marker", error);
  }
}

export async function splitRecording() {
  try {
    const started = await call("split");
    toast("success", "Split into a new take", `${started.files.length} new files`);
  } catch (error) {
    failed("Couldn’t split the recording", error);
  }
}

export function ControlsDock() {
  const { hasSetup, studio, record, streamState, busy } = useStore((s) => ({
    hasSetup: !!s.setup,
    studio: s.studio,
    record: s.status?.state ?? "idle",
    streamState: s.stream?.state ?? "offline",
    busy: s.busy,
  }));
  const recording = record === "recording";
  const streamOn = ["live", "connecting", "reconnecting"].includes(streamState);
  const streamLabel =
    streamState === "connecting" ? "Connecting…" : streamState === "reconnecting" ? "Reconnecting…" : streamOn ? "Stop Streaming" : "Start Streaming";
  const recordLabel = record === "starting" ? "Starting…" : busy.record && recording ? "Stopping…" : recording ? "Stop Recording" : "Start Recording";
  return html`<${Dock} title="Controls">
    <div class="controls">
      <button
        class=${"btn control" + (streamOn ? " active" : "") + (streamState === "reconnecting" ? " warn" : "")}
        disabled=${!hasSetup || busy.stream}
        onClick=${toggleStream}
      >
        ${streamLabel}
      </button>
      <div class="control-row">
        <button
          class=${"btn control" + (recording || record === "starting" ? " active" : "")}
          disabled=${!hasSetup || busy.record}
          onClick=${toggleRecording}
        >
          ${recordLabel}
        </button>
        ${recording &&
        html`<button class="btn control icon-only" title="Add a marker at this moment" onClick=${() => addMarker()}><${Icon} name="flag" /></button>
          <button class="btn control icon-only" title="Split: end these files at the next whole second and carry on in new ones" onClick=${splitRecording}>
            <${Icon} name="scissors" />
          </button>`}
      </div>
      <button class=${"btn control" + (studio ? " checked" : "")} disabled=${!hasSetup} onClick=${() => setStudio(!studio)}>Studio Mode</button>
      <button class="btn control" onClick=${() => openSettings()}>Settings</button>
      <button class="btn control" onClick=${() => native("quit")}>Exit</button>
    </div>
  <//>`;
}
