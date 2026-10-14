// Maintainer authentication for GET /reports/:id/zip: the MAINTAINER_TOKEN
// bearer token, or (when ACCESS_TEAM_DOMAIN and ACCESS_AUD are set) a
// Cloudflare Access JWT checked against the team's signing keys.

export interface AccessEnv {
  MAINTAINER_TOKEN?: string;
  ACCESS_TEAM_DOMAIN: string;
  ACCESS_AUD: string;
}

async function sameSecret(a: string, b: string): Promise<boolean> {
  const digest = async (s: string) => new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(s)));
  const [x, y] = await Promise.all([digest(a), digest(b)]);
  return crypto.subtle.timingSafeEqual(x, y);
}

function decode(part: string): Uint8Array {
  const b64 = part.replace(/-/g, "+").replace(/_/g, "/");
  return Uint8Array.from(atob(b64 + "=".repeat((4 - (b64.length % 4)) % 4)), (c) => c.charCodeAt(0));
}

async function accessJwt(env: AccessEnv, jwt: string): Promise<boolean> {
  const [head, claims, signature] = jwt.split(".");
  if (!head || !claims || !signature) return false;
  const header = JSON.parse(new TextDecoder().decode(decode(head))) as { kid?: string; alg?: string };
  if (header.alg !== "RS256") return false;
  const team = env.ACCESS_TEAM_DOMAIN.replace(/\/$/, "");
  const certs = (await (await fetch(`${team}/cdn-cgi/access/certs`)).json()) as { keys: (JsonWebKey & { kid: string })[] };
  const jwk = certs.keys.find((k) => k.kid === header.kid);
  if (!jwk) return false;
  const key = await crypto.subtle.importKey("jwk", jwk, { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" }, false, ["verify"]);
  const valid = await crypto.subtle.verify("RSASSA-PKCS1-v1_5", key, decode(signature), new TextEncoder().encode(`${head}.${claims}`));
  if (!valid) return false;
  const body = JSON.parse(new TextDecoder().decode(decode(claims))) as { aud?: string | string[]; iss?: string; exp?: number };
  const audiences = Array.isArray(body.aud) ? body.aud : [body.aud];
  return body.iss === team && audiences.includes(env.ACCESS_AUD) && typeof body.exp === "number" && body.exp > Date.now() / 1000;
}

export async function maintainer(env: AccessEnv, request: Request): Promise<boolean> {
  const auth = request.headers.get("Authorization") ?? "";
  if (env.MAINTAINER_TOKEN && auth.startsWith("Bearer ") && (await sameSecret(auth.slice(7), env.MAINTAINER_TOKEN))) {
    return true;
  }
  const jwt = request.headers.get("Cf-Access-Jwt-Assertion");
  if (env.ACCESS_TEAM_DOMAIN && env.ACCESS_AUD && jwt) {
    try {
      return await accessJwt(env, jwt);
    } catch {
      return false;
    }
  }
  return false;
}
