// Advanced Audio Properties, as in OBS: one row per audio source with its
// volume, mute, balance, sync offset and monitoring, and where it is
// recorded (Isocast's tracks: the program mix, and the ISO files).

import { html } from "../lib/html.js";
import { useStore } from "../lib/store.js";
import { hasAudio, sourceName } from "../lib/model.js";
import { edit } from "../lib/session.js";
import { NumberField, Select, Slider } from "./controls.js";
import { Dialog, closeDialog, registerDialog } from "./dialog.js";
import { MONITORING, inProgramMix, setAudio, setProgramMix } from "./mixer.js";

function balanceLabel(b) {
  if (Math.abs(b) < 0.005) return "C";
  return `${b < 0 ? "L" : "R"}${Math.round(Math.abs(b) * 100)}`;
}

function setIsoAudio(setup, id, on) {
  const list = setup.recording.iso?.audio ?? [];
  return edit({ recording: { iso: { audio: on ? [...list.filter((s) => s !== id), id] : list.filter((s) => s !== id) } } });
}

function AdvancedAudioDialog() {
  const setup = useStore((s) => s.setup);
  const sources = (setup?.sources ?? []).filter(hasAudio);
  const isoOn = !!setup?.recording?.iso?.enabled;
  return html`<${Dialog}
    title="Advanced Audio Properties"
    width=${Math.min(1060, innerWidth - 40)}
    class="audio-dialog"
    onCancel=${closeDialog}
    onAccept=${closeDialog}
    buttons=${html`<button class="btn primary" onClick=${closeDialog}>Close</button>`}
  >
    <div class="audio-table">
      <div class="audio-row head">
        <span>Name</span>
        <span>Volume</span>
        <span class="center">Mute</span>
        <span>Balance</span>
        <span>Sync Offset</span>
        <span>Audio Monitoring</span>
        <span title="Where it is recorded: the program mix (Program), and attached to every ISO file (ISO)">Tracks</span>
      </div>
      ${sources.length === 0 && html`<div class="list-empty">No audio sources.</div>`}
      ${sources.map((source) => {
        const audio = { gain_db: 0, muted: false, offset_ms: 0, balance: 0, monitor: "off", ...(source.audio ?? {}) };
        const live = (name) => ({ key: `audio:${source.id}:${name}`, interval: 50 });
        const iso = (setup.recording.iso?.audio ?? []).includes(source.id);
        return html`<div class="audio-row" key=${source.id}>
          <span class="ellipsis" title=${sourceName(source)}>${sourceName(source)}</span>
          <span><${NumberField} value=${audio.gain_db} min=${-100} max=${30} step=${0.1} unit="dB" onChange=${(gain_db) => setAudio(source, { gain_db })} /></span>
          <span class="center"><input type="checkbox" aria-label=${`Mute ${sourceName(source)}`} checked=${audio.muted} onChange=${(e) => setAudio(source, { muted: e.currentTarget.checked })} /></span>
          <span class="balance">
            <${Slider} value=${audio.balance} min=${-1} max=${1} step=${0.01} origin=${0} label=${`Balance of ${sourceName(source)}`}
              onInput=${(balance) => setAudio(source, { balance }, live("balance"))} onChange=${(balance) => setAudio(source, { balance })} />
            <span class="tabular dim">${balanceLabel(audio.balance)}</span>
          </span>
          <span><${NumberField} value=${audio.offset_ms} min=${-5000} max=${5000} unit="ms" onChange=${(offset_ms) => setAudio(source, { offset_ms })} /></span>
          <span><${Select} value=${audio.monitor} options=${MONITORING} onChange=${(monitor) => setAudio(source, { monitor })} /></span>
          <span class="tracks">
            <label class="check"><input type="checkbox" checked=${inProgramMix(setup, source.id)} onChange=${(e) => setProgramMix(source.id, e.currentTarget.checked)} />Program</label>
            <label class=${"check" + (isoOn ? "" : " faint")} title=${isoOn ? "Recorded with every ISO file" : "ISO recording is off (Settings → Output)"}>
              <input type="checkbox" checked=${iso} onChange=${(e) => setIsoAudio(setup, source.id, e.currentTarget.checked)} />ISO
            </label>
          </span>
        </div>`;
      })}
    </div>
  <//>`;
}

registerDialog("advanced-audio", AdvancedAudioDialog);
