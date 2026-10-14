// GitHub as the Isocast GitHub App: a short-lived JWT signed with the App's
// private key buys an installation token (Issues: write, this repository
// only) for each request. Nothing is cached between requests.

export interface GitHubEnv {
  GITHUB_API: string;
  GITHUB_REPO: string;
  GITHUB_APP_ID: string;
  GITHUB_APP_PRIVATE_KEY?: string;
}

export class GitHubError extends Error {}

function base64url(data: ArrayBuffer | Uint8Array | string): string {
  const bytes = typeof data === "string" ? new TextEncoder().encode(data) : new Uint8Array(data);
  let binary = "";
  for (const b of bytes) binary += String.fromCharCode(b);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function der(tag: number, content: Uint8Array): Uint8Array {
  const n = content.length;
  const length =
    n < 0x80 ? [n] : n < 0x100 ? [0x81, n] : n < 0x10000 ? [0x82, n >> 8, n & 0xff] : [0x83, n >> 16, (n >> 8) & 0xff, n & 0xff];
  const out = new Uint8Array(1 + length.length + n);
  out[0] = tag;
  out.set(length, 1);
  out.set(content, 1 + length.length);
  return out;
}

function concat(...parts: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}

/**
 * GitHub hands out PKCS#1 ("RSA PRIVATE KEY") PEM files; WebCrypto
 * imports PKCS#8. Both are accepted, so the downloaded file can be stored
 * as the secret unchanged.
 */
export function pkcs8(pem: string): Uint8Array {
  const pkcs1 = pem.includes("RSA PRIVATE KEY-----");
  const body = pem.replace(/-----[^-]+-----/g, "").replace(/\s+/g, "");
  const key = Uint8Array.from(atob(body), (c) => c.charCodeAt(0));
  if (!pkcs1) return key;
  const algorithm = Uint8Array.from([
    0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00,
  ]);
  return der(0x30, concat(Uint8Array.from([0x02, 0x01, 0x00]), algorithm, der(0x04, key)));
}

export async function appJwt(env: GitHubEnv, now = Math.floor(Date.now() / 1000)): Promise<string> {
  if (!env.GITHUB_APP_ID || !env.GITHUB_APP_PRIVATE_KEY) {
    throw new GitHubError("the GitHub App is not configured (GITHUB_APP_ID, GITHUB_APP_PRIVATE_KEY)");
  }
  const key = await crypto.subtle.importKey(
    "pkcs8",
    pkcs8(env.GITHUB_APP_PRIVATE_KEY),
    { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const head = base64url(JSON.stringify({ alg: "RS256", typ: "JWT" }));
  const claims = base64url(JSON.stringify({ iat: now - 60, exp: now + 540, iss: env.GITHUB_APP_ID }));
  const signature = await crypto.subtle.sign("RSASSA-PKCS1-v1_5", key, new TextEncoder().encode(`${head}.${claims}`));
  return `${head}.${claims}.${base64url(signature)}`;
}

export class GitHub {
  private constructor(
    private env: GitHubEnv,
    private token: string,
  ) {}

  static async connect(env: GitHubEnv): Promise<GitHub> {
    const jwt = await appJwt(env);
    const installation = await request<{ id: number }>(env, jwt, "GET", `/repos/${env.GITHUB_REPO}/installation`);
    const repo = env.GITHUB_REPO.split("/")[1];
    const token = await request<{ token: string }>(
      env,
      jwt,
      "POST",
      `/app/installations/${installation.id}/access_tokens`,
      { repositories: [repo], permissions: { issues: "write" } },
    );
    return new GitHub(env, token.token);
  }

  createIssue(title: string, body: string, labels: string[]): Promise<{ number: number; html_url: string }> {
    return request(this.env, this.token, "POST", `/repos/${this.env.GITHUB_REPO}/issues`, { title, body, labels });
  }

  comment(issue: number, body: string): Promise<{ html_url: string }> {
    return request(this.env, this.token, "POST", `/repos/${this.env.GITHUB_REPO}/issues/${issue}/comments`, { body });
  }

  /** Open issues filed by the app (newest first, at most 300). */
  async openReports(): Promise<{ number: number; html_url: string; body: string | null }[]> {
    const out = [];
    for (let page = 1; page <= 3; page++) {
      const issues = await request<{ number: number; html_url: string; body: string | null; pull_request?: unknown }[]>(
        this.env,
        this.token,
        "GET",
        `/repos/${this.env.GITHUB_REPO}/issues?state=open&labels=from-app&per_page=100&page=${page}`,
      );
      out.push(...issues.filter((i) => !i.pull_request));
      if (issues.length < 100) break;
    }
    return out;
  }
}

async function request<T>(env: GitHubEnv, token: string, method: string, path: string, body?: unknown): Promise<T> {
  const response = await fetch(`${env.GITHUB_API.replace(/\/$/, "")}${path}`, {
    method,
    headers: {
      Accept: "application/vnd.github+json",
      Authorization: `Bearer ${token}`,
      "User-Agent": "isocast-report-worker",
      "X-GitHub-Api-Version": "2022-11-28",
      ...(body ? { "Content-Type": "application/json" } : {}),
    },
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!response.ok) {
    // GitHub's message only: never the request (it holds a token).
    const detail = await response.text().catch(() => "");
    let message = detail.slice(0, 300);
    try {
      message = (JSON.parse(detail) as { message?: string }).message ?? message;
    } catch {
      /* not JSON */
    }
    throw new GitHubError(`GitHub ${method} ${path.split("?")[0]}: ${response.status} ${message}`);
  }
  return (await response.json()) as T;
}
