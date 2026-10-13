// The choices a setting naming a device, a display, an app or a window
// has, from the engine's `devices` list, as [value, label, hint] options
// for a Select. They match setup values the way the engine does (an id,
// a name, or part of a name), so the option for what a source already
// names is the one shown; a value nothing matches stays as an option,
// "(not connected)", and is never replaced on its own.

const CAMERA_HINTS = {
  built_in: "built-in",
  external: "USB",
  continuity: "iPhone",
  device_screen: "iPhone or iPad screen",
  desk_view: "Desk View",
  virtual: "virtual",
};

/** The device in `list` a setup value names: its id or name, the name
 * ignoring case, then the shortest name containing it (as the engine
 * matches cameras and microphones). */
export function matchDevice(list, query) {
  if (!query || !list?.length) return null;
  const lower = String(query).toLowerCase();
  return (
    list.find((d) => d.id === query || d.name === query) ??
    list.find((d) => d.name.toLowerCase() === lower) ??
    list
      .filter((d) => d.name.toLowerCase().includes(lower))
      .sort((a, b) => a.name.length - b.name.length)[0] ??
    null
  );
}

/** Options for devices: each by name (by id if two share the name, then
 * told apart as "1 of 2"), the one `value` names under `value` itself so
 * it shows as chosen. */
function deviceOptions(list, value, label, hint = () => "") {
  const matched = matchDevice(list, value);
  return list.map((d) => {
    const twins = list.filter((o) => o.name === d.name);
    const twin = twins.length > 1;
    const v = d === matched ? value : twin ? d.id : d.name;
    const which = twin ? `${twins.indexOf(d) + 1} of ${twins.length}` : "";
    return [v, label(d), [hint(d), which].filter(Boolean).join(", ")];
  });
}

/** With `value` kept, as "(not connected)", if no option is it. */
function keep(options, value, missing = "not connected") {
  if (value == null || value === "" || options.some(([v]) => v === value)) return options;
  return [[value, `${value} (${missing})`, ""], ...options];
}

/** With a "Choose…" first while nothing is chosen, so choosing the first
 * real option is a change. */
function prompt(options, value, text) {
  return value == null || value === "" ? [["", text, ""], ...options] : options;
}

export function cameraOptions(devices, value) {
  const cameras = devices?.cameras ?? [];
  const options = deviceOptions(cameras, value, (c) => c.name, (c) =>
    [CAMERA_HINTS[c.kind], c.suspended ? "unavailable" : ""].filter(Boolean).join(", "),
  );
  return prompt(keep(options, value), value, "Choose a camera…");
}

export function microphoneOptions(devices, value) {
  const mics = devices?.microphones ?? [];
  const options = [
    ["default", "System default", mics.find((m) => m.default)?.name ?? ""],
    ...deviceOptions(mics, value === "default" ? null : value, (m) => m.name, (m) => (m.default ? "default" : "")),
  ];
  return keep(options, value);
}

export function outputOptions(devices, value) {
  const outputs = devices?.audio_outputs ?? [];
  const options = [
    ["default", "Default", outputs.find((o) => o.default)?.name ?? ""],
    ...deviceOptions(outputs, value === "default" ? null : value, (o) => o.name, (o) => (o.default ? "current" : "")),
  ];
  return keep(options, value);
}

/** Displays: "main", then each by its id (stable, where names like
 * "LG HDR 4K (1)" may swap), the one `value` names under `value` itself. */
export function displayOptions(devices, value) {
  const displays = devices?.displays ?? [];
  const main = displays.find((d) => d.main);
  const lower = String(value ?? "").toLowerCase();
  const matched =
    value === "main" ? null : displays.find((d) => d.id === value || d.name === value) ?? displays.find((d) => d.name.toLowerCase() === lower);
  const options = [
    ["main", "Main display", main ? `${main.name}, ${main.width}×${main.height}` : ""],
    ...displays.map((d) => [d === matched ? value : d.id, d.name, `${d.width}×${d.height}`]),
  ];
  return keep(options, value);
}

/** The apps with windows that can be captured, sorted by name, each as
 * {key: its bundle id (or name without one), name, windows}. */
export function apps(devices) {
  const found = new Map();
  for (const w of devices?.windows ?? []) {
    if (!w.app) continue;
    const key = w.bundle_id || w.app;
    if (!found.has(key)) found.set(key, { key, name: w.app, windows: [] });
    found.get(key).windows.push(w);
  }
  return [...found.values()].sort((a, b) => a.name.localeCompare(b.name));
}

/** The app a setup's `app` names (a bundle id, a name, or part of a name,
 * ignoring case), as the engine matches it. */
export function matchApp(devices, value) {
  if (!value) return null;
  const lower = value.toLowerCase();
  const all = apps(devices);
  return (
    all.find((a) => a.key.toLowerCase() === lower || a.name.toLowerCase() === lower) ??
    all.find((a) => a.name.toLowerCase().includes(lower)) ??
    null
  );
}

/** Apps, each by bundle id: the engine takes a name as part of a name, so
 * "Code" would also match Xcode. */
export function appOptions(devices, value) {
  const matched = matchApp(devices, value);
  const all = apps(devices);
  const options = all.map((a) => {
    const twin = all.filter((o) => o.name === a.name).length > 1;
    const count = `${a.windows.length} window${a.windows.length === 1 ? "" : "s"}`;
    return [a === matched ? value : a.key, a.name, twin ? `${a.key}, ${count}` : count];
  });
  return prompt(keep(options, value, "not open"), value, "Choose an app…");
}

/** The window of `app` a window source with `title` captures: the
 * largest whose title contains it (ignoring case), as the engine picks;
 * the largest of all without one. */
export function matchWindow(devices, app, title) {
  const found = matchApp(devices, app);
  const lower = title?.toLowerCase();
  const windows = (found?.windows ?? []).filter((w) => !lower || w.title?.toLowerCase().includes(lower));
  return windows.reduce((best, w) => (!best || w.width * w.height > best.width * best.height ? w : best), null);
}

/** The option standing for part of a title, typed. */
export const TITLE_PART = "\u200bpart of a title";

/** A window source's title choices: any window of the app, one of its
 * windows' whole titles, or part of a title (`TITLE_PART`, chosen when
 * `value` isn't a whole title, naming the window it matches). `""`
 * stands for none (any window). */
export function titleOptions(devices, app, value) {
  const found = matchApp(devices, app);
  const windows = (found?.windows ?? []).filter((w) => w.title);
  const titles = [...new Set(windows.map((w) => w.title))];
  const size = (w) => `${w.width}×${w.height}`;
  const options = [
    ["", "Any window", "the largest"],
    ...titles.map((t) => [t, t, size(windows.filter((w) => w.title === t).reduce((a, b) => (b.width * b.height > a.width * a.height ? b : a)))]),
  ];
  if (value && !titles.includes(value)) {
    const match = matchWindow(devices, app, value);
    options.push([TITLE_PART, "Part of a title", match ? match.title : "no window has it now"]);
  } else {
    options.push([TITLE_PART, "Part of a title…", ""]);
  }
  return options;
}

// ---------- Camera formats ----------

/** The sizes a camera offers, largest first, without repeats. */
export function cameraSizes(camera) {
  const seen = new Set();
  return (camera?.formats ?? [])
    .filter((f) => {
      const key = `${f.width}x${f.height}`;
      if (seen.has(key)) return false;
      seen.add(key);
      return true;
    })
    .map((f) => ({ width: f.width, height: f.height }))
    .sort((a, b) => b.width * b.height - a.width * a.height || b.width - a.width);
}

/** A format's frame-rate ranges (just its top rates from an engine that
 * doesn't list ranges). */
export const rateRanges = (f) => (f.frame_rate_ranges?.length ? f.frame_rate_ranges : f.frame_rates.map((r) => ({ min: r, max: r })));

/** Rates people pick, listed when a range holds them. */
const COMMON_RATES = [24, 25, 30, 50, 60];

/** The frame rates a camera offers at `size` (every size's, without one),
 * highest first, as {value: the whole number a setup holds, label}: each
 * range's top, and the common rates inside a range (the engine runs at
 * any rate in one, within half a frame). */
export function cameraRates(camera, size) {
  const rates = new Map();
  const add = (r) => {
    const whole = Math.round(r);
    // 30 and 29.97 are both 30 to a setup; show the exact one if offered.
    const exact = Math.abs(r - whole) < 0.005;
    if (!rates.has(whole) || exact) rates.set(whole, exact ? whole : Math.round(r * 100) / 100);
  };
  for (const f of camera?.formats ?? []) {
    if (size && (f.width !== size.width || f.height !== size.height)) continue;
    for (const range of rateRanges(f)) {
      add(range.max);
      for (const r of COMMON_RATES) if (r >= range.min - 0.5 && r <= range.max + 0.5) add(r);
    }
  }
  return [...rates.entries()].sort((a, b) => b[0] - a[0]).map(([value, shown]) => ({ value, label: `${shown} fps` }));
}

/** Whether `camera` offers `size` (any size if null) at `fps` (any if null). */
export function cameraSupports(camera, size, fps) {
  if (!camera) return false;
  if (size && !cameraSizes(camera).some((s) => s.width === size.width && s.height === size.height)) return false;
  if (fps != null && !cameraRates(camera, size).some((r) => r.value === fps)) return false;
  return true;
}

/** The change choosing camera `device` makes to a camera source's
 * `settings`: its resolution and frame rate stay only if the new camera
 * offers them, else they go back to Auto. */
export function cameraChange(devices, settings, device) {
  const camera = matchDevice(devices?.cameras, device);
  const size = settings.width && settings.height ? { width: settings.width, height: settings.height } : null;
  const keepSize = !size || cameraSupports(camera, size, null);
  const keepFps = settings.fps == null || cameraSupports(camera, keepSize ? size : null, settings.fps);
  return { device, ...(keepSize ? {} : { width: null, height: null }), ...(keepFps ? {} : { fps: null }) };
}
