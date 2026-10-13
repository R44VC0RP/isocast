// Audio levels for the meters, kept outside the store so meters can draw
// them every animation frame without re-rendering anything. The daemon
// sends levels about 20 times a second (`levels` events), or once a second
// with recording status; meters ease towards them and hold peaks, and
// only animate while there is something to hear.

const latest = new Map();
let listener = null;

/** Call `listener` whenever new levels arrive. */
export function onLevels(fn) {
  listener = fn;
}

/** New levels from the daemon: [{source, peak_db, rms_db}, ...]. */
export function pushLevels(levels) {
  const now = performance.now();
  for (const level of levels ?? []) {
    latest.set(level.source, { peak: level.peak_db, rms: level.rms_db, at: now });
  }
  listener?.();
}

/** The newest level of `source` ("program" for the mix), if recent. */
export function levelOf(source) {
  const level = latest.get(source);
  if (!level || performance.now() - level.at > 1500) return null;
  return level;
}

