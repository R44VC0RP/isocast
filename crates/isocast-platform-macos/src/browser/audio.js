// Isocast: send this page's sound to the recorder instead of the speakers.
// Everything the page connects to an AudioContext's speakers, and every
// <audio> or <video> it plays, is routed into one capture context at
// 48 kHz, whose stereo mix goes to `webkit.messageHandlers.isocastAudio`.
(() => {
  const handler = window.webkit?.messageHandlers?.isocastAudio;
  const Context = window.AudioContext || window.webkitAudioContext;
  if (!handler || !Context || window.__isocastAudio) return;
  window.__isocastAudio = true;

  const connect = AudioNode.prototype.connect;
  const disconnect = AudioNode.prototype.disconnect;
  const createMediaElementSource = Context.prototype.createMediaElementSource;
  const play = HTMLMediaElement.prototype.play;

  let capture = null;
  let mix = null;
  // Opened on first use, so silent pages cost nothing.
  function open() {
    if (capture) return;
    capture = new Context({ sampleRate: 48000 });
    mix = capture.createGain();
    const processor = capture.createScriptProcessor(2048, 2, 2);
    connect.call(mix, processor);
    // The processor only runs when connected; its output stays silent.
    connect.call(processor, capture.destination);
    processor.onaudioprocess = (event) => {
      const input = event.inputBuffer;
      const left = input.getChannelData(0);
      const right = input.numberOfChannels > 1 ? input.getChannelData(1) : left;
      const samples = new Float32Array(left.length * 2);
      for (let i = 0; i < left.length; i++) {
        samples[2 * i] = left[i];
        samples[2 * i + 1] = right[i];
      }
      const bytes = new Uint8Array(samples.buffer);
      let binary = '';
      for (let i = 0; i < bytes.length; i += 0x8000) {
        binary += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
      }
      handler.postMessage(capture.sampleRate + ':' + btoa(binary));
    };
    capture.resume();
  }

  // The page's own contexts reach the capture context through a stream.
  const taps = new WeakMap();
  function tap(context) {
    if (context === capture) return context.destination;
    let destination = taps.get(context);
    if (!destination) {
      open();
      destination = context.createMediaStreamDestination();
      connect.call(capture.createMediaStreamSource(destination.stream), mix);
      taps.set(context, destination);
    }
    return destination;
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
  function capturePlayback(element) {
    if (handled.has(element) || !sameOrigin(element)) return;
    handled.add(element);
    open();
    try {
      connect.call(createMediaElementSource.call(capture, element), mix);
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
