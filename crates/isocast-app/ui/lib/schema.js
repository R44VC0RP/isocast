// The setup's JSON Schema (the same one `isocast setup schema` prints),
// served by the shell at `/_schema.json`, read into simple field lists so
// source and filter settings get forms without hand-written panels.

let schema = null;
let loading = null;

export function loadSchema() {
  loading ??= fetch("/_schema.json")
    .then((r) => r.json())
    .then((s) => {
      schema = s;
      return s;
    })
    .catch(() => null);
  return loading;
}

export const schemaReady = () => schema != null;

function resolve(node) {
  let current = node;
  for (let i = 0; current?.$ref && i < 8; i++) {
    const name = current.$ref.replace("#/$defs/", "");
    const { $ref: _ref, ...rest } = current;
    const def = schema.$defs[name] ?? {};
    current = {
      ...def,
      ...rest,
      properties: def.properties || rest.properties ? { ...def.properties, ...rest.properties } : undefined,
      required: [...(def.required ?? []), ...(rest.required ?? [])],
      refName: name,
    };
  }
  return current;
}

/** The variant of a kind-tagged definition (`Source`, `Filter`,
 * `AudioFilter`, `Transition`) for `kind`, resolved. */
function variant(defName, kind) {
  const def = schema?.$defs?.[defName];
  const option = def?.oneOf?.find((o) => o.properties?.kind?.const === kind);
  return option ? resolve(option) : null;
}

/** The descriptions of an enum's values, by value. Of a choice between
 * forms (`anyOf`, like an easing: a name, or a cubic Bézier or steps), the
 * names it may take. */
function enumOptions(node) {
  if (node.enum) return node.enum.map((v) => ({ value: v, description: "" }));
  if (node.anyOf) {
    const forms = node.anyOf.map(resolve);
    const names = forms.map(enumOptions).filter(Boolean);
    const objects = forms.filter((form) => form.type === "object").length;
    return names.length === 1 && objects + 1 === forms.length ? names[0] : null;
  }
  // Variants as consts, or (for undocumented ones) grouped in an enum.
  if (node.oneOf?.every((o) => "const" in o || Array.isArray(o.enum)))
    return node.oneOf.flatMap((o) => ("const" in o ? [o.const] : o.enum).map((v) => ({ value: v, description: o.description ?? "" })));
  return null;
}

// Words in setup names that read better otherwise in the interface.
const WORDS = { fps: "frame rate", url: "address", css: "CSS", color: "colour", id: "ID", lut: "LUT", rgb: "RGB", srgb: "sRGB" };

/** "line_height" → "Line height", "fps" → "Frame rate". */
export function humanize(name) {
  const text = name
    .split("_")
    .map((word) => WORDS[word] ?? word)
    .join(" ");
  return text.charAt(0).toUpperCase() + text.slice(1);
}

// Units at the end of setup names ("attack_ms"): shown beside the value,
// not in the label.
const UNITS = [
  ["_ms", "ms"],
  ["_db", "dB"],
  ["_seconds", "s"],
  ["_kbps", "kb/s"],
  ["_gb", "GB"],
];

/** A field's name without its unit, and the unit. */
function splitUnit(name) {
  const found = UNITS.find(([suffix]) => name.endsWith(suffix) && name.length > suffix.length);
  return found ? [name.slice(0, -found[0].length), found[1]] : [name, undefined];
}

/** A range like "−1 to 1" or "1 to 1000" in a description. */
function rangeOf(description) {
  const match = /(^|[\s(:])([−-]?\d+(?:\.\d+)?)\s*(?:to|–)\s*([−-]?\d+(?:\.\d+)?)/.exec(description ?? "");
  if (!match) return null;
  const num = (s) => Number(s.replace("−", "-"));
  return { min: num(match[2]), max: num(match[3]) };
}

/** One field of a kind's settings. */
function field(name, raw, required) {
  let node = raw;
  let optional = !required;
  // `Option<T>`: anyOf [T, null], or type [T, "null"].
  if (node.anyOf?.length === 2 && node.anyOf.some((o) => o.type === "null")) {
    const inner = node.anyOf.find((o) => o.type !== "null");
    node = { ...inner, description: node.description ?? inner.description };
    optional = true;
  }
  if (Array.isArray(node.type) && node.type.includes("null")) {
    node = { ...node, type: node.type.find((t) => t !== "null") };
    optional = true;
  }
  const description = node.description ?? raw.description ?? "";
  const resolved = resolve(node);
  const [bare, unit] = splitUnit(name);
  const base = { name, label: humanize(bare), unit, description, optional, default: raw.default ?? node.default };
  const options = enumOptions(resolved);
  if (resolved.refName === "Color") return { ...base, type: "color" };
  if (options) return { ...base, type: "enum", options };
  if (resolved.type === "boolean") return { ...base, type: "boolean" };
  if (resolved.type === "integer" || resolved.type === "number") {
    const range = rangeOf(description);
    return {
      ...base,
      type: "number",
      integer: resolved.type === "integer",
      min: range?.min ?? resolved.minimum ?? -Infinity,
      max: range?.max ?? Infinity,
    };
  }
  if (resolved.type === "string") return { ...base, type: "string" };
  if (resolved.type === "array" && resolve(resolved.items ?? {}).type === "string") return { ...base, type: "strings" };
  if (resolved.type === "object" && resolved.properties) {
    return { ...base, type: "object", fields: fieldsOf(resolved) };
  }
  return { ...base, type: "json" };
}

// Fields people look for first come first; the schema lists them
// alphabetically.
const ORDER = [
  "display", "device", "app", "title", "url", "text", "file", "path", "files", "font", "size", "weight", "italic",
  "color", "key_color_type", "key_color", "background", "align", "width", "height", "fps", "cursor",
];

function fieldsOf(node) {
  const required = new Set(node.required ?? []);
  const rank = (name) => {
    const i = ORDER.indexOf(name);
    return i < 0 ? ORDER.length : i;
  };
  return Object.entries(node.properties ?? {})
    .filter(([name]) => name !== "kind")
    .sort(([a], [b]) => rank(a) - rank(b) || a.localeCompare(b))
    .map(([name, raw]) => field(name, raw, required.has(name)));
}

/** The settings fields of a source kind, filter kind ("Filter") or audio
 * filter kind ("AudioFilter"), or null if the schema isn't loaded. */
export function kindFields(defName, kind) {
  if (!schema) return null;
  const node = variant(defName, kind);
  return node ? fieldsOf(node) : [];
}

/** Every kind of a kind-tagged definition, with its description. */
export function kindsOf(defName) {
  const def = schema?.$defs?.[defName];
  return (def?.oneOf ?? [])
    .map((o) => ({ kind: o.properties?.kind?.const, description: resolve(o).description ?? "" }))
    .filter((k) => k.kind);
}

/** A starting value for a field that must be set. */
export function startValue(f) {
  if (f.default !== undefined && f.default !== null) return f.default;
  switch (f.type) {
    case "color":
      return "#000000";
    case "boolean":
      return false;
    case "number":
      return Number.isFinite(f.min) && f.min > 0 ? f.min : f.integer ? 0 : 1;
    case "enum":
      return f.options[0].value;
    case "strings":
      return [];
    case "object":
      return Object.fromEntries(f.fields.filter((x) => !x.optional).map((x) => [x.name, startValue(x)]));
    default:
      return "";
  }
}
