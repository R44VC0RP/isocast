// Isocast: send this page's sound to the recorder instead of the speakers.
// The primary AudioContext's render thread supplies the page's stereo mix.
(() => {
  const handler = window.webkit?.messageHandlers?.isocastAudio;
  const Context = window.AudioContext || window.webkitAudioContext;
  if (!handler || !Context || window.__isocastAudio) return;
  window.__isocastAudio = true;

  const connect = AudioNode.prototype.connect;
  const disconnect = AudioNode.prototype.disconnect;
  const createGain = Context.prototype.createGain;
  const createDelay = Context.prototype.createDelay;
  const createOscillator = Context.prototype.createOscillator;
  const createMediaElementSource = Context.prototype.createMediaElementSource;
  const suspend = Context.prototype.suspend;
  const play = HTMLMediaElement.prototype.play;
  const appleWebKit = Boolean(window.__isocastAppleWebKit);
  const mediaDelaySeconds = appleWebKit ? 0.1 : 0;
  const CHUNK_FRAMES = 1024;
  const CHUNK_SAMPLES = CHUNK_FRAMES * 2;
  // See docs/browser-sources.md for measured A/V latencies per engine:
  // - Media elements (<video>/<audio>): Apple WebKit's AVPlayer delays video
  //   frames by CoreAudio outputLatency (up to 250 ms) while MTAudioProcessingTap
  //   delivers PCM ~100 ms early; Chromium and WebKitGTK switch media elements
  //   to WebAudio's pull clock, so only baseLatency applies there.
  // - Pure WebAudio: capped at 25 ms on Apple WebKit (whose getOutputTimestamp
  //   equals currentTime and offscreen snapshots have ~10 ms latency) and 55 ms
  //   elsewhere (matching Windows WASAPI + DWM composition latency).
  const MAX_APPLE_MEDIA_LATENCY_US = 250000;
  const MAX_APPLE_WEBAUDIO_LATENCY_US = 25000;
  const MAX_OTHER_WEBAUDIO_LATENCY_US = 55000;

  // Shared by the worklet module and the CSP fallback. Even when a page
  // requests a different rate, the wire clock is CHUNK_FRAMES / 48000 Hz;
  // endUs is Date.now() wall time in microseconds when the chunk flushed.
  function chunker(rate, send) {
    let chunk = new Float32Array(CHUNK_SAMPLES), used = 0, resumed = false;
    let previousL, previousR, phase = 0;
    function flush(endMs = Date.now()) {
      if (used === 0) return;
      const out = used === chunk.length ? chunk.buffer : chunk.buffer.slice(0, used * 4);
      const wasResumed = resumed;
      chunk = new Float32Array(CHUNK_SAMPLES);
      used = 0;
      resumed = false;
      send(out, Math.max(0, Math.round(endMs * 1000)), wasResumed);
    }
    function append(l, r, endMs) {
      chunk[used++] = l;
      chunk[used++] = r;
      if (used === chunk.length) flush(endMs);
    }
    const write = (left, right, frames, endMs = Date.now()) => {
      for (let i = 0; i < frames; i++) {
        const l = left ? left[i] : 0, r = right ? right[i] : l;
        if (rate === 48000) {
          append(l, r, endMs);
        } else {
          // Linear interpolation, carrying both neighbours and the exact
          // fractional position across render quanta (no per-block rounding).
          if (previousL !== undefined) {
            while (phase < 48000) {
              const t = phase / 48000;
              append(previousL + (l - previousL) * t, previousR + (r - previousR) * t, endMs);
              phase += rate;
            }
            phase -= 48000;
          }
          previousL = l;
          previousR = r;
        }
      }
    };
    write.flush = flush;
    write.resumeNext = () => { resumed = true; };
    return write;
  }

  function registerTap() {
    registerProcessor('isocast-audio', class extends AudioWorkletProcessor {
      constructor() {
        super();
        this.active = true;
        this.pauseArmed = false;
        this.gapEndMs = undefined;
        this.lastMs = undefined;
        this.write = chunker(sampleRate, (buffer, endUs, resumed) => {
          this.gapEndMs = undefined;
          this.port.postMessage({ buffer, endUs, resumed }, [buffer]);
        });
        this.port.onmessage = ({ data }) => {
          if (data?.type === 'pause') {
            if (this.gapEndMs !== undefined && this.gapEndMs > data.at) {
              this.write.resumeNext();
              this.gapEndMs = undefined;
            } else {
              this.write.flush(this.lastMs ?? Date.now());
              this.gapEndMs = undefined;
              this.pauseArmed = true;
            }
          } else if (data?.type === 'stop') {
            this.write.flush(this.lastMs ?? Date.now());
            this.active = false;
          }
        };
      }
      process(inputs) {
        if (!this.active) return false;
        const now = Date.now();
        if (this.lastMs !== undefined && now - this.lastMs >= 30) {
          this.write.flush(this.lastMs);
          if (this.pauseArmed) {
            this.write.resumeNext();
            this.pauseArmed = false;
            this.gapEndMs = undefined;
          } else {
            this.gapEndMs = now;
          }
        }
        this.write(inputs[0][0], inputs[0][1], inputs[0][0]?.length || 128, now);
        this.lastMs = now;
        return true;
      }
    });
  }

  // Serialize before the page can replace Function.prototype.toString.
  // A data module also works on WebView2 file pages (opaque origins).
  const moduleURL = 'data:text/javascript,' + encodeURIComponent(
    `const CHUNK_SAMPLES = ${CHUNK_SAMPLES};\n${chunker}\n(${registerTap})()`
  );

  let warnedPost = false;
  function warnPost(error) {
    if (!warnedPost) console.warn('Isocast could not send page audio:', error);
    warnedPost = true;
  }
  function send(message) {
    try {
      // WebKit may reject asynchronously when its native handler is gone.
      // The WebView2 shim returns nothing. Do not leak either failure into
      // the page, or flood its console while a view is being dismantled.
      handler.postMessage(message)?.catch(warnPost);
    } catch (error) {
      warnPost(error);
    }
  }
  function post(buffer, endUs, resumed) {
    const sep = resumed ? '+' : '@';
    send(`48000${sep}${endUs}:` + btoa(String.fromCharCode.apply(null, new Uint8Array(buffer))));
  }

  const routes = new Map();
  let primary, output, activeTap, hadAudio = false;
  function warn(error) {
    console.warn('Isocast could not route page audio:', error);
  }
  // Guard only our capture graph, not the page's native connect/disconnect
  // calls (invalid page connections should still raise their normal errors).
  function guarded(action) {
    try { return action(); } catch (error) { warn(error); }
  }
  function outputLatencyUs(route) {
    const context = route.context;
    const liveOut = Math.max(0, Number(context.outputLatency) || 0);
    if (liveOut > 0) route.lastOutputLatency = liveOut;
    const base = Math.max(0, Number(context.baseLatency) || 0);
    const out = liveOut > 0 ? liveOut : (route.lastOutputLatency || 0);
    let seconds, cap;
    if (route.hasMedia) {
      seconds = appleWebKit ? base + out : base;
      cap = appleWebKit ? MAX_APPLE_MEDIA_LATENCY_US : MAX_OTHER_WEBAUDIO_LATENCY_US;
    } else {
      seconds = base + out;
      cap = appleWebKit ? MAX_APPLE_WEBAUDIO_LATENCY_US : MAX_OTHER_WEBAUDIO_LATENCY_US;
    }
    return seconds > 0 && Number.isFinite(seconds) ? Math.min(cap, Math.round(seconds * 1e6)) : 0;
  }

  async function listen(route, mix) {
    const context = route.context;
    let node, keep, pauseNode, stopNode;
    const token = {};
    let tapResumed = false;
    // Fix output-latency compensation for each continuous run once the audio
    // backend reports a non-zero outputLatency, so device switches mid-run
    // cannot jump endUs past RESYNC without a Resumed marker.
    let lockedLatencyUs;
    function deliver(buffer, endUs, workletResumed) {
      if (token !== activeTap) return;
      const resumed = hadAudio && (tapResumed || workletResumed);
      if (resumed) lockedLatencyUs = undefined;
      const latencyUs = lockedLatencyUs ?? outputLatencyUs(route);
      if (lockedLatencyUs === undefined && (Number(context.outputLatency) || 0) > 0) {
        lockedLatencyUs = latencyUs;
      }
      hadAudio = true;
      tapResumed = false;
      post(buffer, endUs + latencyUs, resumed);
    }
    try {
      await (route.module ??= context.audioWorklet.addModule(moduleURL));
      if (mix !== output) return; // another context took over during load
      activeTap = token;
      tapResumed = hadAudio;
      route.pausing = false;
      // Feed an uncaptured keepalive oscillator into input 1 (process() reads
      // only inputs[0]) so WebKit never sees an all-silent graph in an
      // offscreen WKWebView and throttles AudioContext rendering to 2 Hz.
      node = new AudioWorkletNode(context, 'isocast-audio', {
        numberOfInputs: 2,
        channelCount: 2,
        channelCountMode: 'explicit',
      });
      if (typeof createOscillator === 'function') {
        keep = createOscillator.call(context);
        connect.call(keep, node, 0, 1);
        keep.start();
      }
      node.port.onmessage = ({ data }) => deliver(data.buffer, data.endUs, data.resumed);
      pauseNode = () => node.port.postMessage({ type: 'pause', at: Date.now() });
      stopNode = () => node.port.postMessage({ type: 'stop' });
      console.info('Isocast page audio: AudioWorklet');
    } catch (error) {
      if (mix !== output) return;
      console.warn('Isocast page audio: ScriptProcessor fallback (AudioWorklet unavailable)', error);
      activeTap = token;
      tapResumed = hadAudio;
      route.pausing = false;
      let pauseArmed = false, gapEndMs, lastMs;
      node = context.createScriptProcessor(CHUNK_FRAMES, 2, 2);
      const write = chunker(context.sampleRate, (buffer, endUs, resumed) => {
        gapEndMs = undefined;
        deliver(buffer, endUs, resumed);
      });
      node.onaudioprocess = ({ inputBuffer }) => {
        if (token !== activeTap) return;
        const now = Date.now();
        if (lastMs !== undefined && now - lastMs >= 30) {
          write.flush(lastMs);
          if (pauseArmed) {
            write.resumeNext();
            pauseArmed = false;
            gapEndMs = undefined;
          } else {
            gapEndMs = now;
          }
        }
        write(inputBuffer.getChannelData(0), inputBuffer.getChannelData(1), inputBuffer.length, now);
        lastMs = now;
      };
      pauseNode = () => {
        const at = Date.now();
        if (gapEndMs !== undefined && gapEndMs > at) {
          write.resumeNext();
          gapEndMs = undefined;
        } else {
          write.flush(lastMs ?? at);
          gapEndMs = undefined;
          pauseArmed = true;
        }
      };
      stopNode = () => write.flush(lastMs ?? Date.now());
    }
    const mute = context.createGain();
    mute.gain.value = 0;
    connect.call(mix, node, 0, 0);
    connect.call(node, mute);
    connect.call(mute, context.destination);
    route.pause = pauseNode;
    route.stop = () => {
      stopNode?.();
      if (keep) {
        try { keep.stop(); } catch {}
        disconnect.call(keep);
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
    guarded(() => {
      if (route.hasMedia && primary) primary.hasMedia = true;
      if (route === primary) {
        connect.call(route.bus, output);
      } else {
        const destination = route.context.createMediaStreamDestination();
        route.bridge = primary.context.createMediaStreamSource(destination.stream);
        connect.call(route.bridge, output);
        connect.call(route.bus, destination);
      }
    });
  }

  function choosePrimary() {
    guarded(() => {
      if (primary?.context.state === 'running') return;
      const live = [...routes.values()];
      const next = live.find(route => route.context.state === 'running');
      // Keep a suspended primary wired when nothing else is running, so
      // resume() captures from the first render quantum without rebuilding
      // its AudioWorkletNode on a contended main thread.
      if (!next && primary && primary.context.state !== 'closed') return;
      if (primary === next) return;
      const mix = next && createGain.call(next.context);
      primary?.stop?.();
      for (const route of live) {
        disconnect.call(route.bus);
        if (route.bridge) disconnect.call(route.bridge);
        route.bridge = route.stop = route.pause = null;
      }
      primary = next;
      output = mix;
      if (!next) return;
      for (const route of live) wire(route);
      listen(next, output).catch(warn);
    });
  }

  function pauseRoute(route) {
    if (route === primary && !route.pausing) {
      route.pausing = true;
      route.pause?.();
    }
  }

  function tap(context) {
    let route = routes.get(context);
    if (!route) guarded(() => {
      route = { context, bus: createGain.call(context), pausing: false };
      routes.set(context, route);
      if (primary) wire(route);
      choosePrimary();
      context.addEventListener('statechange', () => guarded(() => {
        if (context.state === 'running') {
          route.pausing = false;
        } else if (context.state === 'suspended') {
          pauseRoute(route);
        } else if (context.state === 'closed') {
          routes.delete(context);
          if (route.bridge) disconnect.call(route.bridge);
        }
        choosePrimary();
      }));
    });
    // A failed bridge leaves the page connected to its private (silent)
    // bus. If even the bus couldn't be allocated, never send it to speakers.
    return route?.bus;
  }
  Context.prototype.suspend = function (...args) {
    const route = routes.get(this);
    if (route) pauseRoute(route);
    return suspend.apply(this, args);
  };
  AudioNode.prototype.connect = function (target, ...rest) {
    if (target instanceof AudioDestinationNode && target.context instanceof Context) {
      target = tap(target.context);
      if (!target) return;
    }
    return connect.call(this, target, ...rest);
  };
  AudioNode.prototype.disconnect = function (...args) {
    if (args[0] instanceof AudioDestinationNode && args[0].context instanceof Context) {
      args[0] = tap(args[0].context);
      if (!args[0]) return;
    }
    return disconnect.apply(this, args);
  };

  // Media elements the page routes through Web Audio itself are left to
  // it; the others are captured when they start playing. Media from
  // another origin (without CORS) would be silenced by Web Audio, so it
  // plays aloud instead.
  const handled = new WeakSet();
  function markMediaRoute(context) {
    const bus = tap(context);
    const entry = routes.get(context);
    if (entry) entry.hasMedia = true;
    if (primary) primary.hasMedia = true;
    return bus;
  }
  function wrapMediaSource(context, source) {
    if (!(mediaDelaySeconds > 0) || typeof createDelay !== 'function') return source;
    const delay = createDelay.call(context, Math.max(0.25, mediaDelaySeconds));
    delay.delayTime.value = mediaDelaySeconds;
    connect.call(source, delay);
    source.connect = (target, ...rest) => AudioNode.prototype.connect.call(delay, target, ...rest);
    source.disconnect = (...args) => AudioNode.prototype.disconnect.apply(delay, args);
    return source;
  }
  Context.prototype.createMediaElementSource = function (element) {
    handled.add(element);
    markMediaRoute(this);
    return wrapMediaSource(this, createMediaElementSource.call(this, element));
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
      const bus = markMediaRoute(mediaContext);
      const source = wrapMediaSource(mediaContext, createMediaElementSource.call(mediaContext, element));
      if (bus) source.connect(bus);
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
