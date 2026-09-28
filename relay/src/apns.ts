// APNs provider-token auth (ES256 JWT, signed with WebCrypto) and delivery.
// Docs: "Establishing a token-based connection to APNs" and "Sending
// notification requests to APNs" (developer.apple.com).

export type ApnsEnv = "sandbox" | "production";

export const APNS_HOSTS: Record<ApnsEnv, string> = {
  production: "https://api.push.apple.com",
  sandbox: "https://api.sandbox.push.apple.com",
};

export interface ApnsConfig {
  key: string; // .p8 contents (PKCS#8 PEM)
  keyId: string;
  teamId: string;
  topic: string; // the iOS app's bundle id
}

export interface ApnsRequest {
  env: ApnsEnv;
  deviceToken: string;
  payload: unknown;
  pushType: "alert" | "background";
  priority: 10 | 5;
  collapseId?: string;
  expiration?: number; // unix seconds; 0 = deliver once or drop
}

export interface ApnsResult {
  status: number;
  apnsId?: string;
  reason?: string;
}

// ── base64 ────────────────────────────────────────────────────────────────

export function b64urlEncode(bytes: ArrayBuffer | Uint8Array): string {
  const u8 = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  let s = "";
  for (const b of u8) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

export function b64Decode(s: string): Uint8Array {
  const bin = atob(s);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

// ── JWT ───────────────────────────────────────────────────────────────────

function pemToPkcs8(pem: string): Uint8Array {
  const body = pem
    .replace(/-----BEGIN [^-]+-----/, "")
    .replace(/-----END [^-]+-----/, "")
    .replace(/\s+/g, "");
  return b64Decode(body);
}

export async function importApnsKey(pem: string): Promise<CryptoKey> {
  return crypto.subtle.importKey(
    "pkcs8",
    pemToPkcs8(pem),
    { name: "ECDSA", namedCurve: "P-256" },
    false,
    ["sign"],
  );
}

/** ES256 JWT for APNs. WebCrypto returns the raw r||s signature JWS wants. */
export async function signApnsJwt(
  key: CryptoKey,
  keyId: string,
  teamId: string,
  iatSeconds: number,
): Promise<string> {
  const enc = new TextEncoder();
  const header = b64urlEncode(enc.encode(JSON.stringify({ alg: "ES256", kid: keyId })));
  const claims = b64urlEncode(enc.encode(JSON.stringify({ iss: teamId, iat: iatSeconds })));
  const input = `${header}.${claims}`;
  const sig = await crypto.subtle.sign(
    { name: "ECDSA", hash: "SHA-256" },
    key,
    enc.encode(input),
  );
  return `${input}.${b64urlEncode(sig)}`;
}

// APNs rejects tokens older than an hour and throttles refreshes more often
// than every 20 minutes (TooManyProviderTokenUpdates). Refresh at 40.
const JWT_TTL_SECONDS = 40 * 60;

interface CachedJwt {
  fingerprint: string;
  key: CryptoKey;
  jwt: string;
  iat: number;
}
let cached: CachedJwt | undefined;

export function resetJwtCache(): void {
  cached = undefined;
}

async function providerToken(cfg: ApnsConfig, nowSeconds: number): Promise<string> {
  const fingerprint = `${cfg.keyId}|${cfg.teamId}|${cfg.key.length}|${cfg.key.slice(-24)}`;
  if (!cached || cached.fingerprint !== fingerprint) {
    cached = { fingerprint, key: await importApnsKey(cfg.key), jwt: "", iat: 0 };
  }
  if (!cached.jwt || nowSeconds - cached.iat >= JWT_TTL_SECONDS) {
    cached.jwt = await signApnsJwt(cached.key, cfg.keyId, cfg.teamId, nowSeconds);
    cached.iat = nowSeconds;
  }
  return cached.jwt;
}

// ── delivery ──────────────────────────────────────────────────────────────

async function post(cfg: ApnsConfig, req: ApnsRequest, jwt: string): Promise<ApnsResult> {
  const headers: Record<string, string> = {
    authorization: `bearer ${jwt}`,
    "apns-topic": cfg.topic,
    "apns-push-type": req.pushType,
    "apns-priority": String(req.priority),
    "content-type": "application/json",
  };
  if (req.collapseId) headers["apns-collapse-id"] = req.collapseId;
  if (req.expiration !== undefined) headers["apns-expiration"] = String(req.expiration);
  const url = `${APNS_HOSTS[req.env]}/3/device/${req.deviceToken}`;
  const resp = await fetch(url, {
    method: "POST",
    headers,
    body: JSON.stringify(req.payload),
  });
  const apnsId = resp.headers.get("apns-id") ?? undefined;
  if (resp.status === 200) return { status: 200, apnsId };
  let reason: string | undefined;
  try {
    reason = ((await resp.json()) as { reason?: string }).reason;
  } catch {
    reason = undefined;
  }
  return { status: resp.status, apnsId, reason };
}

/** Send one notification. Retries once with a fresh JWT if APNs says the
 *  provider token expired (e.g. the isolate's clock/JWT cache went stale). */
export async function sendApns(cfg: ApnsConfig, req: ApnsRequest): Promise<ApnsResult> {
  const now = Math.floor(Date.now() / 1000);
  const first = await post(cfg, req, await providerToken(cfg, now));
  if (first.status === 403 && first.reason === "ExpiredProviderToken") {
    if (cached) cached.jwt = "";
    return post(cfg, req, await providerToken(cfg, now));
  }
  return first;
}
