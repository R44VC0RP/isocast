// The layout checker's only way to answer: a `deliver_layout` tool that
// validates the answer and writes it as JSON to the path the coordinator
// chose (LAYOUT_AUDIT_OUT). It also narrows every model request to `read`
// plus this tool, and refuses to run anything else, on top of the agent's
// deny-all permissions (see ../agents/checker.md). The same isolation as
// .github/visual-judge: scripts/layout-audit.py runs OpenCode in a fresh
// copy of this folder with an empty global config.

import { appendFileSync, writeFileSync } from "node:fs"

const ALLOWED = new Set(["read", "deliver_layout"])
const VERDICTS = ["ok", "issues"]
const KINDS = [
  "clipped_text",
  "truncated_text",
  "overlap",
  "misaligned",
  "uneven_spacing",
  "overflow",
  "cut_off_control",
  "icon_problem",
  "contrast",
  "empty_or_broken",
  "other",
]
const SEVERITIES = ["low", "medium", "high"]
const MAX_ISSUES = 10

type Json = Record<string, unknown>

function text(value: unknown, field: string, max: number): string {
  if (typeof value !== "string" || value.trim() === "") throw new Error(`${field} must be a non-empty string`)
  if (value.length > max) throw new Error(`${field} is longer than ${max} characters`)
  return value.trim()
}

function oneOf(value: unknown, field: string, options: string[]): string {
  if (typeof value !== "string" || !options.includes(value)) {
    throw new Error(`${field} must be one of: ${options.join(", ")}`)
  }
  return value
}

function box(value: unknown, field: string, width: number, height: number): number[] {
  if (!Array.isArray(value) || value.length !== 4 || !value.every((n) => typeof n === "number" && Number.isFinite(n))) {
    throw new Error(`${field} must be [x, y, w, h]: four numbers in image pixels`)
  }
  const [x, y, w, h] = value.map((n) => Math.round(n as number))
  if (w < 1 || h < 1) throw new Error(`${field}: w and h must be at least 1`)
  if (x < 0 || y < 0) throw new Error(`${field}: x and y must be 0 or more`)
  if (width && x + w > width + 2) throw new Error(`${field}: x + w = ${x + w} is past the image's width ${width}`)
  if (height && y + h > height + 2) throw new Error(`${field}: y + h = ${y + h} is past the image's height ${height}`)
  return [x, y, Math.min(w, width ? width - x : w), Math.min(h, height ? height - y : h)]
}

function validate(input: unknown): Json {
  if (!input || typeof input !== "object" || Array.isArray(input)) throw new Error("the answer must be an object")
  const v = input as Json
  const extra = Object.keys(v).filter((k) => !["verdict", "issues"].includes(k))
  if (extra.length) throw new Error(`unknown fields: ${extra.join(", ")}`)
  const width = Number(process.env.LAYOUT_AUDIT_WIDTH ?? 0)
  const height = Number(process.env.LAYOUT_AUDIT_HEIGHT ?? 0)
  const verdict = oneOf(v.verdict, "verdict", VERDICTS)
  if (!Array.isArray(v.issues)) throw new Error("issues must be an array (empty when the verdict is ok)")
  if (v.issues.length > MAX_ISSUES) throw new Error(`at most ${MAX_ISSUES} issues: keep the most important`)
  const issues = v.issues.map((raw, i) => {
    if (!raw || typeof raw !== "object" || Array.isArray(raw)) throw new Error(`issues[${i}] must be an object`)
    const issue = raw as Json
    const extraIssue = Object.keys(issue).filter((k) => !["kind", "severity", "box", "element", "detail"].includes(k))
    if (extraIssue.length) throw new Error(`issues[${i}]: unknown fields: ${extraIssue.join(", ")}`)
    return {
      kind: oneOf(issue.kind, `issues[${i}].kind`, KINDS),
      severity: oneOf(issue.severity, `issues[${i}].severity`, SEVERITIES),
      box: box(issue.box, `issues[${i}].box`, width, height),
      element: text(issue.element, `issues[${i}].element`, 100),
      detail: text(issue.detail, `issues[${i}].detail`, 400),
    }
  })
  if (verdict === "ok" && issues.length) throw new Error('verdict "ok" must have an empty issues list')
  if (verdict === "issues" && !issues.length) throw new Error('verdict "issues" needs at least one issue')
  return { verdict, issues }
}

export default {
  id: "isocast.layout-audit",

  async setup(ctx: any) {
    const toolsLog = process.env.LAYOUT_AUDIT_TOOLS_LOG

    await ctx.tool.transform((editor: any) => {
      editor.add({
        name: "deliver_layout",
        description:
          "Deliver your layout verdict for this screen. Call it exactly once, as your last action; it is the only output that counts.",
        input: {
          type: "object",
          properties: {
            verdict: { type: "string", enum: VERDICTS, description: "ok: well laid out; issues: at least one problem" },
            issues: {
              type: "array",
              maxItems: MAX_ISSUES,
              items: {
                type: "object",
                properties: {
                  kind: { type: "string", enum: KINDS },
                  severity: { type: "string", enum: SEVERITIES },
                  box: {
                    type: "array",
                    items: { type: "number" },
                    minItems: 4,
                    maxItems: 4,
                    description: "[x, y, w, h] around the problem, in pixels of the attached image (origin top left)",
                  },
                  element: { type: "string", description: "The element, e.g. 'Record button' or 'Settings › Output, Folder field'" },
                  detail: { type: "string", description: "What is wrong, in one or two sentences" },
                },
                required: ["kind", "severity", "box", "element", "detail"],
                additionalProperties: false,
              },
            },
          },
          required: ["verdict", "issues"],
          additionalProperties: false,
        },
        options: { codemode: false },
        async execute(input: unknown) {
          const out = process.env.LAYOUT_AUDIT_OUT
          if (!out) throw new Error("LAYOUT_AUDIT_OUT is not set; the coordinator must set it")
          const answer = validate(input)
          writeFileSync(out, JSON.stringify(answer, null, 2))
          return { content: "Verdict recorded. You are done; stop now." }
        },
      })
    })

    // Only read and deliver_layout reach the model.
    await ctx.session.hook("context", (event: any) => {
      for (const name of Object.keys(event.tools ?? {})) {
        if (!ALLOWED.has(name)) delete event.tools[name]
      }
      event.options.maxTokens = 3000
      if (toolsLog) appendFileSync(toolsLog, JSON.stringify(Object.keys(event.tools ?? {}).sort()) + "\n")
    })

    // And nothing else runs, whatever the model asks for.
    await ctx.tool.hook("execute.before", (event: any) => {
      if (!ALLOWED.has(String(event.tool))) throw new Error(`tool ${event.tool} is not available to the layout checker`)
    })
  },
}
