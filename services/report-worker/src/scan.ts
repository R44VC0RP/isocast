// The server-side secret scan. The same rules as the engine's pre-upload
// check (crates/isocast-diagnostics/src/scan.rs): scripts/publish-snapshot.sh's
// secret patterns, plus anything the engine's redactor would have replaced
// but did not (a credential field, a credential in text, a URL with a path,
// query or user name). Findings name the file and rule, never the text.

const FIXED: [string, RegExp][] = [
  ["an Apple app-specific password", /(?:^|[^a-z-])[a-z]{4}-[a-z]{4}-[a-z]{4}-[a-z]{4}(?:[^a-z-]|$)/m],
  ["an AWS access key", /AKIA[0-9A-Z]{16}/],
  ["a GitHub token", /gh[pousr]_[A-Za-z0-9]{30,}/],
  ["a GitHub token", /github_pat_[A-Za-z0-9_]{30,}/],
  ["an API key", /sk-[A-Za-z0-9_-]{30,}/],
  ["a private key", /BEGIN [A-Z ]*PRIVATE KEY/],
  ["a stream URL with a key", /rtmps?:\/\/[^ "]*\/(?:live|x|app)\/[A-Za-z0-9_-]{16,}/],
];

const WORD = "(?:key|token|password|passwd|secret|authorization|cookie|credential)";
// A JSON field the redactor treats as a credential (redact.rs `sensitive`).
const FIELD = new RegExp(
  `"((?:[A-Za-z0-9]+[_.-])*${WORD}|apikey|accesstoken|refreshtoken|clientsecret|streamkey)"\\s*:\\s*`,
  "gi",
);
// A credential in text (redact.rs ASSIGN).
const ASSIGN = new RegExp(
  `(?:\\b(?:[\\w.-]+[_-])?${WORD}["']?\\s*(?::|=|\\bis\\b)\\s*|\\bBearer\\s+)(?:(?:Bearer|Basic)\\s+)?`,
  "gi",
);
const URL_RE = /(?<![A-Za-z0-9+.-])(?:https?|rtmps?):\/\/[^\s"'<>\\]+/gi;
// The value the redactor would have replaced (redact.rs ASSIGN's tail).
const VALUE = /^(?:"[^"]*"|'[^']*'|[^\s,;}]+)/;

function leaks(rest: string, field: boolean): boolean {
  if (/^["']?\[redacted\]/.test(rest)) return false;
  // A JSON field's whole value is replaced; text needs a value to replace.
  return field || VALUE.test(rest);
}

/** Why `text` can't be stored, or null. */
export function findSecret(text: string): string | null {
  for (const [rule, re] of FIXED) {
    if (re.test(text)) return rule;
  }
  for (const re of [FIELD, ASSIGN]) {
    re.lastIndex = 0;
    let m: RegExpExecArray | null;
    while ((m = re.exec(text))) {
      if (m[0].length === 0) {
        re.lastIndex++;
        continue;
      }
      const rest = text.slice(m.index + m[0].length, m.index + m[0].length + 256);
      if (leaks(rest, re === FIELD)) {
        return re === FIELD ? "a credential field that is not redacted" : "a credential in text";
      }
    }
  }
  URL_RE.lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = URL_RE.exec(text))) {
    const rest = m[0].slice(m[0].indexOf("://") + 3);
    const slash = rest.indexOf("/");
    const authority = slash < 0 ? rest : rest.slice(0, slash);
    const path = slash < 0 ? "" : rest.slice(slash + 1);
    if (authority.includes("@") || authority.includes("?") || authority.includes("#")) {
      return "a URL with a user name, query or fragment";
    }
    if (path !== "" && !path.startsWith("[redacted]")) {
      return "a URL with a path (capability URLs must be redacted)";
    }
  }
  return null;
}

/**
 * Scans a stream of text whole lines at a time (no rule spans a line), so
 * a secret split between two chunks is still seen whole.
 */
export class StreamScan {
  private carry = "";
  finding: string | null = null;
  push(chunk: string): void {
    if (this.finding) return;
    const text = this.carry + chunk;
    const end = text.lastIndexOf("\n") + 1;
    // A line longer than the engine's 4 MiB file cap is scanned as it is.
    const cut = end === 0 && text.length > 8 << 20 ? text.length : end;
    this.carry = text.slice(cut);
    if (cut > 0) this.finding = findSecret(text.slice(0, cut));
  }
  end(): string | null {
    if (!this.finding && this.carry) this.finding = findSecret(this.carry);
    this.carry = "";
    return this.finding;
  }
}
