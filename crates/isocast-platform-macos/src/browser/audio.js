// Isocast: send this page's sound to the recorder instead of the speakers.
// The primary AudioContext's render thread supplies the page's stereo mix.
(() => {
  const handler = window.webkit?.messageHandlers?.isocastAudio;
  const Context = window.AudioContext || window.webkitAudioContext;
  if (!handler || !Context || window.__isocastAudio) return;
  window.__isocastAudio = true;

  const connect = AudioNode.prototype.connect;
  const disconnect = AudioNode.prototype.disconnect;
  const createMediaElementSource = Context.prototype.createMediaElementSource;
  const play = HTMLMediaElement.prototype.play;

  // Shared by the worklet module and the CSP fallback. Even when a page
  // requests a different rate, the wire clock is 2048 frames / 48000 Hz.
  function chunker(rate, send) {
    let chunk = new Float32Array(4096), used = 0;
    let previousL, previousR, phase = 0;
    function append(l, r) {
      chunk[used++] = l;
      chunk[used++] = r;
      if (used === chunk.length) {
        send(chunk.buffer);
        chunk = new Float32Array(4096);
        used = 0;
      }
    }
    return (left, right, frames) => {
      for (let i = 0; i < frames; i++) {
        const l = left ? left[i] : 0, r = right ? right[i] : l;
        if (rate === 48000) {
          append(l, r);
        } else {
          // Linear interpolation, carrying both neighbours and the exact
          // fractional position across render quanta (no per-block rounding).
          if (previousL !== undefined) {
            while (phase < 48000) {
              const t = phase / 48000;
              append(previousL + (l - previousL) * t, previousR + (r - previousR) * t);
              phase += rate;
            }
            phase -= 48000;
          }
          previousL = l;
          previousR = r;
        }
      }
    };
  }

  function registerTap() {
    registerProcessor('isocast-audio', class extends AudioWorkletProcessor {
      constructor() {
        super();
        this.active = true;
        this.port.onmessage = () => { this.active = false; };
        this.write = chunker(sampleRate, buffer => this.port.postMessage(buffer, [buffer]));
      }
      process(inputs, outputs) {
        if (this.active) this.write(inputs[0][0], inputs[0][1], outputs[0][0].length);
        return this.active;
      }
    });
  }

  async function loadModule(context) {
    // A file page has an opaque origin in WebView2: it cannot load its
    // own blob:null worklet URL. A self-contained data module works there.
    const source = `${chunker}\n(${registerTap})()`;
    await context.audioWorklet.addModule('data:text/javascript,' + encodeURIComponent(source));
  }

  function post(buffer) {
    handler.postMessage('48000:' + btoa(String.fromCharCode.apply(null, new Uint8Array(buffer))));
  }

  const routes = new Map();
  let primary, output;
  async function listen(route, mix) {
    const context = route.context;
    let node;
    try {
      await (route.module ??= loadModule(context));
      if (mix !== output) return; // another context took over during load
      node = new AudioWorkletNode(context, 'isocast-audio', { channelCount: 2, channelCountMode: 'explicit' });
      node.port.onmessage = event => { if (mix === output) post(event.data); };
      console.info('Isocast page audio: AudioWorklet');
    } catch (error) {
      if (mix !== output) return;
      console.warn('Isocast page audio: ScriptProcessor fallback (AudioWorklet unavailable)', error);
      node = context.createScriptProcessor(2048, 2, 2);
      const write = chunker(context.sampleRate, post);
      node.onaudioprocess = ({ inputBuffer }) => {
        if (mix === output) write(inputBuffer.getChannelData(0), inputBuffer.getChannelData(1), inputBuffer.length);
      };
    }
    const mute = context.createGain();
    mute.gain.value = 0;
    connect.call(mix, node);
    connect.call(node, mute);
    connect.call(mute, context.destination);
    route.stop = () => {
      if (node.port) {
        node.port.postMessage('stop');
        node.port.close();
      }
      disconnect.call(mix);
      disconnect.call(node);
      disconnect.call(mute);
    };
  }

  // Hosts stamp one stream per page. Posting concurrent processors as
  // separate streams would concatenate, not mix, their samples. Only
  // secondary contexts cross a MediaStream; the common single one never does.
  function wire(route) {
    if (route === primary) {
      connect.call(route.bus, output);
    } else {
      const destination = route.context.createMediaStreamDestination();
      connect.call(route.bus, destination);
      route.bridge = primary.context.createMediaStreamSource(destination.stream);
      connect.call(route.bridge, output);
    }
  }

  function choosePrimary() {
    if (primary?.context.state === 'running') return;
    const live = [...routes.values()];
    const next = live.find(route => route.context.state === 'running') || live[0];
    if (primary === next) return;
    primary?.stop?.();
    for (const route of live) {
      disconnect.call(route.bus);
      if (route.bridge) disconnect.call(route.bridge);
      route.bridge = route.stop = null;
    }
    primary = next;
    output = next?.context.createGain();
    if (!next) return;
    for (const route of live) wire(route);
    listen(next, output);
  }

  function tap(context) {
    if (!(context instanceof Context)) return context.destination; // offline render
    let route = routes.get(context);
    if (!route) {
      route = { context, bus: context.createGain() };
      routes.set(context, route);
      if (primary) wire(route);
      choosePrimary();
      context.addEventListener('statechange', () => {
        if (context.state === 'closed') {
          if (route.bridge) disconnect.call(route.bridge);
          routes.delete(context);
        }
        choosePrimary();
      });
    }
    return route.bus;
  }
  AudioNode.prototype.connect = function (target, ...rest) {
    if (target instanceof AudioDestinationNode) target = tap(target.context);
    return connect.call(this, target, ...rest);
  };
  AudioNode.prototype.disconnect = function (...args) {
    if (args[0] instanceof AudioDestinationNode) args[0] = tap(args[0].context);
    return disconnect.apply(this, args);
  };

  // Media elements the page routes through Web Audio itself are left to
  // it; the others are captured when they start playing. Media from
  // another origin (without CORS) would be silenced by Web Audio, so it
  // plays aloud instead.
  const handled = new WeakSet();
  Context.prototype.createMediaElementSource = function (element) {
    handled.add(element);
    return createMediaElementSource.call(this, element);
  };
  function sameOrigin(element) {
    if (element.crossOrigin != null) return true;
    try {
      const url = new URL(element.currentSrc || element.src, location.href);
      return ['blob:', 'data:'].includes(url.protocol) || url.origin === location.origin;
    } catch {
      return false;
    }
  }
  let mediaContext;
  function capturePlayback(element) {
    if (handled.has(element) || !sameOrigin(element)) return;
    handled.add(element);
    try {
      // Plain media must keep playing even if a page-owned context closes.
      if (!mediaContext) {
        mediaContext = new Context({ sampleRate: 48000 });
        mediaContext.resume();
      }
      connect.call(createMediaElementSource.call(mediaContext, element), tap(mediaContext));
    } catch (error) {
      console.warn('Isocast could not capture a media element:', error);
    }
  }
  HTMLMediaElement.prototype.play = function () {
    capturePlayback(this);
    return play.call(this);
  };
  document.addEventListener('play', (event) => {
    if (event.target instanceof HTMLMediaElement) capturePlayback(event.target);
  }, true);
})();
