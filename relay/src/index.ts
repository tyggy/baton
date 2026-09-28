// Baton push relay (Cloudflare Worker). Protocol: docs/relay.md.
//
//   POST /v1/register  {device_token, env}
//        → pushes a send credential to that device (never in the response)
//   POST /v1/send      {device_token, env, credential, ciphertext,
//                       collapse_id?, priority?, ttl?}
//        → one APNs alert with a generic fallback and the ciphertext
//   GET  /v1/health
//
// The relay never sees plaintext and stores nothing but short-lived
// rate-limit counters keyed by hashes.

import { sendApns, b64Decode, type ApnsConfig, type ApnsEnv, type ApnsResult } from "./apns";
import { issueCredential, sha256Hex, verifyCredential } from "./auth";
import {
  RateLimiter,
  REGISTER_IP_RULES,
  REGISTER_TOKEN_RULES,
  SEND_RULES,
  type Rule,
} from "./ratelimit";

export { RateLimiter };

export interface Env {
  RATE_LIMITER: DurableObjectNamespace<RateLimiter>;
  APNS_KEY: string;
  APNS_KEY_ID: string;
  APNS_TEAM_ID: string;
  APNS_TOPIC: string;
  REGISTRATION_SECRET: string;
}

export const PROTOCOL_VERSION = 1;
/** Custom payload key holding the ciphertext. */
export const PAYLOAD_KEY = "baton";
/** Shown if the Notification Service Extension can't decrypt in time. */
export const FALLBACK_ALERT = { title: "Baton", body: "New activity" };

const MAX_BODY_BYTES = 8 * 1024;
/** enc (32) + AEAD tag (16) is the smallest valid HPKE message. */
const MIN_CIPHERTEXT_BYTES = 48;
/** Keeps the APNs payload well under its 4096-byte limit. */
const MAX_CIPHERTEXT_BYTES = 2048;
const DEFAULT_TTL_SECONDS = 3600;
const MAX_TTL_SECONDS = 86400;

const TOKEN_RE = /^[0-9a-fA-F]{64,200}$/;
const B64_RE = /^[A-Za-z0-9+/]+={0,2}$/;

function json(status: number, body: unknown, extra: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", ...extra },
  });
}

const fail = (status: number, error: string, extra: Record<string, string> = {}) =>
  json(status, { ok: false, error }, extra);

function apnsConfig(env: Env): ApnsConfig | null {
  if (!env.APNS_KEY || !env.APNS_KEY_ID || !env.APNS_TEAM_ID || !env.APNS_TOPIC) return null;
  return { key: env.APNS_KEY, keyId: env.APNS_KEY_ID, teamId: env.APNS_TEAM_ID, topic: env.APNS_TOPIC };
}

async function readJson(req: Request): Promise<Record<string, unknown> | null> {
  const len = Number(req.headers.get("content-length") ?? "0");
  if (len > MAX_BODY_BYTES) return null;
  const text = await req.text();
  if (text.length > MAX_BODY_BYTES) return null;
  try {
    const v = JSON.parse(text);
    return v && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : null;
  } catch {
    return null;
  }
}

function parseEnv(v: unknown): ApnsEnv | null {
  return v === "sandbox" || v === "production" ? v : null;
}

async function limit(env: Env, key: string, rules: Rule[]): Promise<Response | null> {
  const stub = env.RATE_LIMITER.get(env.RATE_LIMITER.idFromName(await sha256Hex(key)));
  const r = await stub.hit(rules);
  return r.ok ? null : fail(429, "rate_limited", { "retry-after": String(r.retryAfter) });
}

/** Map an APNs outcome onto the relay's response. 410 tells the daemon to
 *  drop the device; the reason string is Apple's, never payload data. */
function apnsOutcome(r: ApnsResult): Response {
  if (r.status === 200) return json(200, { ok: true, apns_id: r.apnsId });
  const gone =
    r.status === 410 ||
    r.reason === "BadDeviceToken" ||
    r.reason === "Unregistered" ||
    r.reason === "DeviceTokenNotForTopic";
  if (gone) return json(410, { ok: false, error: "unregistered", reason: r.reason });
  if (r.status === 429) return json(429, { ok: false, error: "apns_throttled", reason: r.reason });
  return json(502, { ok: false, error: "apns_error", status: r.status, reason: r.reason });
}

async function register(req: Request, env: Env): Promise<Response> {
  const cfg = apnsConfig(env);
  if (!cfg || !env.REGISTRATION_SECRET) return fail(503, "relay_not_configured");
  const body = await readJson(req);
  if (!body) return fail(400, "bad_json");
  const token = body.device_token;
  const apnsEnv = parseEnv(body.env);
  if (typeof token !== "string" || !TOKEN_RE.test(token)) return fail(400, "bad_device_token");
  if (!apnsEnv) return fail(400, "bad_env");

  const ip = req.headers.get("cf-connecting-ip") ?? "unknown";
  const limited =
    (await limit(env, `ip|${ip}`, REGISTER_IP_RULES)) ??
    (await limit(env, `reg|${apnsEnv}|${token.toLowerCase()}`, REGISTER_TOKEN_RULES));
  if (limited) return limited;

  const credential = await issueCredential(env.REGISTRATION_SECRET, apnsEnv, token);
  // Background push: the app is in the foreground during pairing and hands
  // the credential to the daemon. Nothing user-visible is shown.
  const result = await sendApns(cfg, {
    env: apnsEnv,
    deviceToken: token.toLowerCase(),
    payload: {
      aps: { "content-available": 1 },
      [PAYLOAD_KEY]: { v: PROTOCOL_VERSION, type: "register", env: apnsEnv, credential },
    },
    pushType: "background",
    priority: 5,
    expiration: Math.floor(Date.now() / 1000) + 600,
  });
  if (result.status === 200) return json(202, { ok: true, delivery: "push" });
  return apnsOutcome(result);
}

async function send(req: Request, env: Env): Promise<Response> {
  const cfg = apnsConfig(env);
  if (!cfg || !env.REGISTRATION_SECRET) return fail(503, "relay_not_configured");
  const body = await readJson(req);
  if (!body) return fail(400, "bad_json");
  const { device_token: token, credential, ciphertext, collapse_id: collapseId } = body;
  const apnsEnv = parseEnv(body.env);
  if (typeof token !== "string" || !TOKEN_RE.test(token)) return fail(400, "bad_device_token");
  if (!apnsEnv) return fail(400, "bad_env");
  if (typeof credential !== "string") return fail(401, "missing_credential");
  if (!(await verifyCredential(env.REGISTRATION_SECRET, apnsEnv, token, credential))) {
    return fail(403, "bad_credential");
  }
  if (typeof ciphertext !== "string" || !B64_RE.test(ciphertext)) return fail(400, "bad_ciphertext");
  let ctLen: number;
  try {
    ctLen = b64Decode(ciphertext).length;
  } catch {
    return fail(400, "bad_ciphertext");
  }
  if (ctLen < MIN_CIPHERTEXT_BYTES || ctLen > MAX_CIPHERTEXT_BYTES) return fail(400, "bad_ciphertext");
  if (
    collapseId !== undefined &&
    (typeof collapseId !== "string" || collapseId.length === 0 || new TextEncoder().encode(collapseId).length > 64)
  ) {
    return fail(400, "bad_collapse_id");
  }
  const priority = body.priority ?? "high";
  if (priority !== "high" && priority !== "normal") return fail(400, "bad_priority");
  const ttl = body.ttl ?? DEFAULT_TTL_SECONDS;
  if (typeof ttl !== "number" || !Number.isInteger(ttl) || ttl < 0 || ttl > MAX_TTL_SECONDS) {
    return fail(400, "bad_ttl");
  }

  const limited = await limit(env, `send|${apnsEnv}|${token.toLowerCase()}`, SEND_RULES);
  if (limited) return limited;

  const result = await sendApns(cfg, {
    env: apnsEnv,
    deviceToken: token.toLowerCase(),
    payload: {
      aps: { alert: FALLBACK_ALERT, "mutable-content": 1, sound: "default" },
      [PAYLOAD_KEY]: { v: PROTOCOL_VERSION, ct: ciphertext },
    },
    pushType: "alert",
    priority: priority === "high" ? 10 : 5,
    collapseId: collapseId as string | undefined,
    expiration: ttl === 0 ? 0 : Math.floor(Date.now() / 1000) + ttl,
  });
  return apnsOutcome(result);
}

export default {
  async fetch(req: Request, env: Env): Promise<Response> {
    const { pathname } = new URL(req.url);
    if (pathname === "/v1/health" && req.method === "GET") {
      return json(200, { ok: true, protocol: PROTOCOL_VERSION, configured: apnsConfig(env) !== null });
    }
    if (pathname === "/v1/register" || pathname === "/v1/send") {
      if (req.method !== "POST") return fail(405, "method_not_allowed", { allow: "POST" });
      try {
        return pathname === "/v1/register" ? await register(req, env) : await send(req, env);
      } catch (e) {
        console.error("relay error", e instanceof Error ? e.message : String(e));
        return fail(500, "internal");
      }
    }
    return fail(404, "not_found");
  },
} satisfies ExportedHandler<Env>;
