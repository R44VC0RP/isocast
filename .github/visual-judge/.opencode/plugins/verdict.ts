// The visual judge's only way to answer: a `deliver_verdict` tool that
// validates the verdict and writes it as JSON to the path the workflow
// chose (VISUAL_JUDGE_OUT). It also narrows every model request to the
// read-only tools plus this one, and refuses to run anything else, on top
// of the agent's deny-all permissions (see ../agents/judge.md).
//
// Loaded only from the trusted copy of .github/visual-judge that
// scripts/visual_judge.py runs OpenCode in; nothing from the pull request
// is ever on OpenCode's config path.

import { appendFileSync, writeFileSync } from "node:fs"

const ALLOWED = new Set(["read", "glob", "grep", "deliver_verdict"])
const VERDICTS = ["no_change", "intended_change", "possible_regression", "broken"]
const KINDS = [
  "clipped_text",
  "overlap",
  "missing_control",
  "misplaced_control",
  "misalignment",
  "colour",
  "layout",
  "other",
]
const SEVERITIES = ["low", "medium", "high"]

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

function validate(input: unknown): Json {
  if (!input || typeof input !== "object" || Array.isArray(input)) throw new Error("the verdict must be an object")
  const v = input as Json
  const extra = Object.keys(v).filter(
    (k) => !["view", "verdict", "issues", "summary", "ready_to_merge", "ready_reason"].includes(k),
  )
  if (extra.length) throw new Error(`unknown fields: ${extra.join(", ")}`)
  const expected = process.env.VISUAL_JUDGE_VIEW ?? ""
  const view = text(v.view, "view", 80)
  if (expected && view !== expected) throw new Error(`view must be "${expected}"`)
  const verdict = oneOf(v.verdict, "verdict", VERDICTS)
  if (!Array.isArray(v.issues)) throw new Error("issues must be an array (empty when there are none)")
  if (v.issues.length > 8) throw new Error("at most 8 issues")
  const issues = v.issues.map((raw, i) => {
    if (!raw || typeof raw !== "object" || Array.isArray(raw)) throw new Error(`issues[${i}] must be an object`)
    const issue = raw as Json
    const extraIssue = Object.keys(issue).filter((k) => !["kind", "where", "description", "severity"].includes(k))
    if (extraIssue.length) throw new Error(`issues[${i}]: unknown fields: ${extraIssue.join(", ")}`)
    return {
      kind: oneOf(issue.kind, `issues[${i}].kind`, KINDS),
      where: text(issue.where, `issues[${i}].where`, 120),
      description: text(issue.description, `issues[${i}].description`, 400),
      severity: oneOf(issue.severity, `issues[${i}].severity`, SEVERITIES),
    }
  })
  if ((verdict === "possible_regression" || verdict === "broken") && issues.length === 0) {
    throw new Error(`a ${verdict} verdict needs at least one issue`)
  }
  if (typeof v.ready_to_merge !== "boolean") throw new Error("ready_to_merge must be true or false")
  if (v.ready_to_merge && (verdict === "possible_regression" || verdict === "broken")) {
    throw new Error(`ready_to_merge can't be true with a ${verdict} verdict`)
  }
  return {
    view,
    verdict,
    issues,
    summary: text(v.summary, "summary", 600),
    ready_to_merge: v.ready_to_merge,
    ready_reason: text(v.ready_reason, "ready_reason", 200),
  }
}

export default {
  id: "isocast.visual-judge",

  async setup(ctx: any) {
    const toolsLog = process.env.VISUAL_JUDGE_TOOLS_LOG

    await ctx.tool.transform((editor: any) => {
      editor.add({
        name: "deliver_verdict",
        description:
          "Deliver your verdict for this view. Call it exactly once, as your last action; it is the only output that counts.",
        input: {
          type: "object",
          properties: {
            view: { type: "string", description: "The view id you were given, e.g. main-1440x900" },
            verdict: { type: "string", enum: VERDICTS },
            issues: {
              type: "array",
              maxItems: 8,
              items: {
                type: "object",
                properties: {
                  kind: { type: "string", enum: KINDS },
                  where: { type: "string", description: "Where on screen, e.g. 'status bar, right end'" },
                  description: { type: "string" },
                  severity: { type: "string", enum: SEVERITIES },
                },
                required: ["kind", "where", "description", "severity"],
                additionalProperties: false,
              },
            },
            summary: { type: "string", description: "One or two sentences" },
            ready_to_merge: {
              type: "boolean",
              description: "Whether this view, as drawn, is fine to merge",
            },
            ready_reason: { type: "string", description: "One line: why it is or isn't ready" },
          },
          required: ["view", "verdict", "issues", "summary", "ready_to_merge", "ready_reason"],
          additionalProperties: false,
        },
        options: { codemode: false },
        async execute(input: unknown) {
          const out = process.env.VISUAL_JUDGE_OUT
          if (!out) throw new Error("VISUAL_JUDGE_OUT is not set; the workflow must set it")
          const verdict = validate(input)
          writeFileSync(out, JSON.stringify(verdict, null, 2))
          return { content: "Verdict recorded. You are done; stop now." }
        },
      })
    })

    // Only the read-only tools and deliver_verdict reach the model.
    await ctx.session.hook("context", (event: any) => {
      for (const name of Object.keys(event.tools ?? {})) {
        if (!ALLOWED.has(name)) delete event.tools[name]
      }
      event.options.maxTokens = 4000
      if (toolsLog) appendFileSync(toolsLog, JSON.stringify(Object.keys(event.tools ?? {}).sort()) + "\n")
    })

    // And nothing else runs, whatever the model asks for.
    await ctx.tool.hook("execute.before", (event: any) => {
      if (!ALLOWED.has(String(event.tool))) throw new Error(`tool ${event.tool} is not available to the visual judge`)
    })
  },
}
