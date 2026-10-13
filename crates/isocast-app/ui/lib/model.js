// Reading the setup document: kinds, names, items and their geometry.

/** Every source kind the setup supports, in OBS's words where OBS has
 * the same thing (the names new sources get, and the Add menu's). */
export const KINDS = {
  display: { label: "Display Capture", icon: "display", video: true },
  window: { label: "Window Capture", icon: "window", video: true },
  application: { label: "Application Capture", icon: "application", video: true },
  camera: { label: "Video Capture Device", icon: "camera", video: true },
  microphone: { label: "Audio Input Capture", icon: "mic", audio: true },
  system_audio: { label: "Audio Output Capture", icon: "speaker", audio: true },
  application_audio: { label: "Application Audio Capture", icon: "appAudio", audio: true },
  image: { label: "Image", icon: "image", video: true },
  slideshow: { label: "Image Slide Show", icon: "slideshow", video: true },
  media: { label: "Media Source", icon: "media", video: true, audio: true },
  text: { label: "Text", icon: "text", video: true },
  browser: { label: "Browser", icon: "browser", video: true },
  color: { label: "Colour Source", icon: "color", video: true },
  test_pattern: { label: "Test Pattern", icon: "testPattern", video: true },
  tone: { label: "Tone", icon: "tone", audio: true },
};

export function kindOf(source) {
  return KINDS[source?.kind] ?? { label: source?.kind ?? "Source", icon: "box", video: true };
}

export function hasVideo(source) {
  return !!kindOf(source).video;
}

export function hasAudio(source) {
  if (!source) return false;
  if (source.kind === "browser") return !!source.control_audio;
  return !!kindOf(source).audio;
}

export const sourceName = (source) => source?.name || source?.id || "";
export const sceneName = (scene) => scene?.name || scene?.id || "";

export function findSource(setup, id) {
  return setup?.sources?.find((s) => s.id === id) ?? null;
}

export function findScene(setup, id) {
  return setup?.scenes?.find((s) => s.id === id) ?? null;
}

/** What patches call an item: its id, else its source or scene. */
export function key(item) {
  return item.id ?? item.source ?? item.scene;
}

/** The items from the top level down to the one at `path` (keys). */
export function itemChain(scene, path) {
  const chain = [];
  let items = scene?.items ?? [];
  for (const k of path ?? []) {
    const item = items.find((i) => key(i) === k);
    if (!item) return null;
    chain.push(item);
    items = item.children ?? [];
  }
  return chain.length ? chain : null;
}

/** How an item is labelled: its source's or scene's name, else a box or
 * group. */
export function itemLabel(setup, item) {
  if (item.source) return sourceName(findSource(setup, item.source)) || item.source;
  if (item.scene) return sceneName(findScene(setup, item.scene)) || item.scene;
  if (item.children?.length) return item.id ?? "Group";
  return item.id ?? "Box";
}

export function itemIcon(setup, item) {
  if (item.source) return kindOf(findSource(setup, item.source)).icon;
  if (item.scene) return "scene";
  return item.children?.length ? "group" : "box";
}

/** An id made from `name`: lowercase letters, digits and dashes, not one
 * of `taken` (or "program", which is reserved). */
export function newId(name, taken) {
  const base =
    String(name)
      .toLowerCase()
      .normalize("NFKD")
      .replace(/[^a-z0-9]+/g, "-")
      .replace(/^-+|-+$/g, "")
      .slice(0, 40) || "source";
  const used = new Set([...taken, "program"]);
  if (!used.has(base)) return base;
  for (let n = 2; ; n++) {
    if (!used.has(`${base}-${n}`)) return `${base}-${n}`;
  }
}

/** Every id a source or scene could clash with. */
export function takenIds(setup) {
  return [...(setup?.sources ?? []).map((s) => s.id), ...(setup?.scenes ?? []).map((s) => s.id)];
}

/** A name like "Scene 3" that no scene has yet. */
export function nextName(base, names) {
  const used = new Set(names);
  for (let n = 1; ; n++) {
    const name = `${base} ${n}`;
    if (!used.has(name)) return name;
  }
}

/** The transform part of an item's style, with defaults. */
export function transformOf(item) {
  const t = item.style?.transform ?? {};
  return { translate: t.translate ?? [0, 0], scale: t.scale ?? 1, rotate: t.rotate ?? 0 };
}

/** An item's rect; none (an item placed by its `layout`, which the
 * editor doesn't move yet) is an empty one at the top left. */
export const rectOf = (item) => item.rect ?? { x: 0, y: 0, width: 0, height: 0 };

/** Where an item is drawn on the canvas: its centre, its drawn size and
 * its angle, given its parent's top left corner. (A parent's own scale
 * and rotation aren't applied to children here.) */
export function placement(item, origin = [0, 0]) {
  const { translate, scale, rotate } = transformOf(item);
  const { x, y, width, height } = rectOf(item);
  return {
    cx: origin[0] + x + width / 2 + translate[0],
    cy: origin[1] + y + height / 2 + translate[1],
    w: width * scale,
    h: height * scale,
    rotate,
    scale,
  };
}

/** The axis-aligned box around a placement. */
export function bounds(p) {
  const r = (p.rotate * Math.PI) / 180;
  const c = Math.abs(Math.cos(r));
  const s = Math.abs(Math.sin(r));
  const w = p.w * c + p.h * s;
  const h = p.w * s + p.h * c;
  return { left: p.cx - w / 2, top: p.cy - h / 2, right: p.cx + w / 2, bottom: p.cy + h / 2 };
}

/** The top-left corner children of the item at the end of `chain` are
 * placed from (the sum of their parents' rects). */
export function originOf(chain) {
  let x = 0;
  let y = 0;
  for (const item of chain.slice(0, -1)) {
    const { translate } = transformOf(item);
    x += rectOf(item).x + translate[0];
    y += rectOf(item).y + translate[1];
  }
  return [x, y];
}

/** Whether `scene` shows `target` through nested scenes (to refuse loops). */
export function showsScene(setup, sceneId, target, depth = 0) {
  if (sceneId === target) return true;
  if (depth > 8) return false;
  const scene = findScene(setup, sceneId);
  const walk = (items) =>
    items.some((item) => (item.scene && showsScene(setup, item.scene, target, depth + 1)) || walk(item.children ?? []));
  return scene ? walk(scene.items ?? []) : false;
}

/** Remove every reference to source `id` from a setup (a new setup). */
export function withoutSource(setup, id) {
  const prune = (items) =>
    items.filter((item) => item.source !== id).map((item) => (item.children ? { ...item, children: prune(item.children) } : item));
  const drop = (list) => (list ?? []).filter((s) => s !== id);
  const recording = setup.recording;
  return {
    ...setup,
    sources: setup.sources.filter((s) => s.id !== id),
    scenes: setup.scenes.map((scene) => ({ ...scene, items: prune(scene.items ?? []) })),
    recording: {
      ...recording,
      program: { ...recording.program, audio: drop(recording.program?.audio) },
      iso: recording.iso
        ? { ...recording.iso, sources: drop(recording.iso.sources), audio: drop(recording.iso.audio) }
        : recording.iso,
    },
  };
}

/** Remove scene `id` and every item showing it (a new setup). */
export function withoutScene(setup, id) {
  const prune = (items) =>
    items.filter((item) => item.scene !== id).map((item) => (item.children ? { ...item, children: prune(item.children) } : item));
  return {
    ...setup,
    scenes: setup.scenes.filter((s) => s.id !== id).map((scene) => ({ ...scene, items: prune(scene.items ?? []) })),
  };
}

/** The native size of video sources whose settings say it. */
export function declaredSize(setup, source) {
  const canvas = setup.canvas;
  switch (source?.kind) {
    case "test_pattern":
      return { width: source.width ?? 1920, height: source.height ?? 1080 };
    case "color":
      return source.width && source.height ? { width: source.width, height: source.height } : { width: canvas.width, height: canvas.height };
    case "browser":
      return { width: source.width ?? 800, height: source.height ?? 600 };
    case "camera":
    case "image":
    case "slideshow":
    case "text":
      return source.width && source.height ? { width: source.width, height: source.height } : null;
    default:
      return null;
  }
}
