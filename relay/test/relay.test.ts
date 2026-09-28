import { env as baseEnv, runDurableObjectAlarm, runInDurableObject } from "cloudflare:test";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import worker, { FALLBACK_ALERT, PAYLOAD_KEY, type Env } from "../src/index";
import { b64Decode, resetJwtCache, signApnsJwt, importApnsKey } from "../src/apns";
import { issueCredential, sha256Hex, verifyCredential } from "../src/auth";
import { RateLimiter } from "../src/ratelimit";

// ── fixtures ────────────────────────────────────────────────────────────────

let pem = "";
let publicKey: CryptoKey;

function toPem(der: ArrayBuffer): string {
  const b64 = btoa(String.fromCharCode(...new Uint8Array(der)));
  return `-----BEGIN PRIVATE KEY-----\n${b64.match(/.{1,64}/g)!.join("\n")}\n-----END PRIVATE KEY-----\n`;
}

beforeAll(async () => {
  const kp = (await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, true, [
    "sign",
    "verify",
  ])) as CryptoKeyPair;
  pem = toPem((await crypto.subtle.exportKey("pkcs8", kp.privateKey)) as ArrayBuffer);
  publicKey = kp.publicKey;
});

function testEnv(overrides: Partial<Env> = {}): Env {
  return { ...(baseEnv as unknown as Env), APNS_KEY: pem, ...overrides };
}

let tokenSeq = 0;
function newToken(): string {
  tokenSeq += 1;
  return (tokenSeq.toString(16).padStart(8, "0") + "ab".repeat(28)).slice(0, 64);
}

function ciphertext(bytes = 120): string {
  const u8 = crypto.getRandomValues(new Uint8Array(bytes));
  return btoa(String.fromCharCode(...u8));
}

interface Captured {
  url: string;
  headers: Record<string, string>;
  body: any;
}

let apnsCalls: Captured[] = [];
let apnsReplies: Array<() => Response> = [];

beforeEach(() => {
  apnsCalls = [];
  apnsReplies = [];
  resetJwtCache();
  vi.spyOn(globalThis, "fetch").mockImplementation(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
    if (!url.startsWith("https://api.push.apple.com/") && !url.startsWith("https://api.sandbox.push.apple.com/")) {
      throw new Error(`unexpected outbound fetch: ${url}`);
    }
    const headers: Record<string, string> = {};
    new Headers(init?.headers).forEach((v, k) => (headers[k] = v));
    apnsCalls.push({ url, headers, body: JSON.parse(String(init?.body)) });
    const next = apnsReplies.shift();
    return next ? next() : new Response(null, { status: 200, headers: { "apns-id": "11111111-2222-3333-4444-555555555555" } });
  });
});

afterEach(() => {
  vi.restoreAllMocks();
});

async function call(path: string, body?: unknown, env: Env = testEnv(), method = "POST", ip = "203.0.113.7") {
  const req = new Request(`https://relay.test${path}`, {
    method,
    headers: { "content-type": "application/json", "cf-connecting-ip": ip },
    body: body === undefined ? undefined : typeof body === "string" ? body : JSON.stringify(body),
  });
  const resp = await worker.fetch(req, env);
  return { status: resp.status, headers: resp.headers, json: (await resp.json()) as any };
}

async function sendBody(token: string, apnsEnv: "sandbox" | "production" = "sandbox", extra: object = {}) {
  const env = testEnv();
  return {
    device_token: token,
    env: apnsEnv,
    credential: await issueCredential(env.REGISTRATION_SECRET, apnsEnv, token),
    ciphertext: ciphertext(),
    ...extra,
  };
}

function decodeJwt(jwt: string) {
  const [h, c, s] = jwt.split(".");
  const dec = (x: string) => JSON.parse(atob(x.replace(/-/g, "+").replace(/_/g, "/")));
  return { header: dec(h), claims: dec(c), signingInput: `${h}.${c}`, sig: s };
}

// ── tests ───────────────────────────────────────────────────────────────────

describe("health and routing", () => {
  it("reports health and configuration", async () => {
    const r = await call("/v1/health", undefined, testEnv(), "GET");
    expect(r.status).toBe(200);
    expect(r.json).toEqual({ ok: true, protocol: 1, configured: true });
    const unconf = await call("/v1/health", undefined, testEnv({ APNS_KEY: "" }), "GET");
    expect(unconf.json.configured).toBe(false);
  });

  it("rejects unknown paths and methods", async () => {
    expect((await call("/nope", {})).status).toBe(404);
    expect((await call("/v1/send", undefined, testEnv(), "GET")).status).toBe(405);
  });

  it("refuses to work unconfigured", async () => {
    const r = await call("/v1/send", await sendBody(newToken()), testEnv({ APNS_TEAM_ID: "" }));
    expect(r.status).toBe(503);
    expect(apnsCalls).toHaveLength(0);
  });
});

describe("APNs JWT", () => {
  it("signs a verifiable ES256 token with kid and iss", async () => {
    const jwt = await signApnsJwt(await importApnsKey(pem), "KEYID12345", "TEAMID1234", 1_700_000_000);
    const { header, claims, signingInput, sig } = decodeJwt(jwt);
    expect(header).toEqual({ alg: "ES256", kid: "KEYID12345" });
    expect(claims).toEqual({ iss: "TEAMID1234", iat: 1_700_000_000 });
    const raw = b64Decode(sig.replace(/-/g, "+").replace(/_/g, "/") + "==".slice(0, (4 - (sig.length % 4)) % 4));
    expect(raw.length).toBe(64); // r||s, not DER
    const ok = await crypto.subtle.verify(
      { name: "ECDSA", hash: "SHA-256" },
      publicKey,
      raw,
      new TextEncoder().encode(signingInput),
    );
    expect(ok).toBe(true);
  });

  it("reuses the provider token between sends", async () => {
    const t = newToken();
    await call("/v1/send", await sendBody(t));
    await call("/v1/send", await sendBody(t));
    expect(apnsCalls).toHaveLength(2);
    expect(apnsCalls[0].headers.authorization).toBe(apnsCalls[1].headers.authorization);
  });

  it("retries once with a fresh token on ExpiredProviderToken", async () => {
    apnsReplies.push(() => Response.json({ reason: "ExpiredProviderToken" }, { status: 403 }));
    const r = await call("/v1/send", await sendBody(newToken()));
    expect(r.status).toBe(200);
    expect(apnsCalls).toHaveLength(2);
  });
});

describe("registration", () => {
  it("delivers the credential by push, never in the response", async () => {
    const t = newToken();
    const r = await call("/v1/register", { device_token: t, env: "sandbox" });
    expect(r.status).toBe(202);
    expect(r.json).toEqual({ ok: true, delivery: "push" });
    expect(JSON.stringify(r.json)).not.toContain("credential");

    expect(apnsCalls).toHaveLength(1);
    const c = apnsCalls[0];
    expect(c.url).toBe(`https://api.sandbox.push.apple.com/3/device/${t}`);
    expect(c.headers["apns-push-type"]).toBe("background");
    expect(c.headers["apns-priority"]).toBe("5");
    expect(c.headers["apns-topic"]).toBe("dev.baton.test");
    expect(c.body.aps).toEqual({ "content-available": 1 });
    const cred = c.body[PAYLOAD_KEY].credential;
    expect(c.body[PAYLOAD_KEY]).toMatchObject({ v: 1, type: "register", env: "sandbox" });
    expect(await verifyCredential(testEnv().REGISTRATION_SECRET, "sandbox", t, cred)).toBe(true);
  });

  it("validates input", async () => {
    expect((await call("/v1/register", { device_token: "xyz", env: "sandbox" })).status).toBe(400);
    expect((await call("/v1/register", { device_token: newToken(), env: "staging" })).status).toBe(400);
    expect((await call("/v1/register", "not json")).status).toBe(400);
    expect((await call("/v1/register", [1, 2])).status).toBe(400);
    expect(apnsCalls).toHaveLength(0);
  });

  it("rate-limits registrations per token", async () => {
    const t = newToken();
    for (let i = 0; i < 5; i++) {
      expect((await call("/v1/register", { device_token: t, env: "sandbox" }, testEnv(), "POST", `198.51.100.${i}`)).status).toBe(202);
    }
    const r = await call("/v1/register", { device_token: t, env: "sandbox" }, testEnv(), "POST", "198.51.100.99");
    expect(r.status).toBe(429);
    expect(Number(r.headers.get("retry-after"))).toBeGreaterThan(0);
    expect(apnsCalls).toHaveLength(5);
  });

  it("rate-limits registrations per client IP", async () => {
    for (let i = 0; i < 30; i++) {
      expect((await call("/v1/register", { device_token: newToken(), env: "sandbox" }, testEnv(), "POST", "192.0.2.50")).status).toBe(202);
    }
    const r = await call("/v1/register", { device_token: newToken(), env: "sandbox" }, testEnv(), "POST", "192.0.2.50");
    expect(r.status).toBe(429);
  });
});

describe("send", () => {
  it("forwards an opaque alert with mutable-content and the ciphertext", async () => {
    const t = newToken();
    const body = await sendBody(t, "production", { collapse_id: "s-3f9a", priority: "high", ttl: 600 });
    const before = Math.floor(Date.now() / 1000);
    const r = await call("/v1/send", body);
    expect(r.status).toBe(200);
    expect(r.json).toEqual({ ok: true, apns_id: "11111111-2222-3333-4444-555555555555" });

    expect(apnsCalls).toHaveLength(1);
    const c = apnsCalls[0];
    expect(c.url).toBe(`https://api.push.apple.com/3/device/${t}`);
    expect(c.headers["apns-push-type"]).toBe("alert");
    expect(c.headers["apns-priority"]).toBe("10");
    expect(c.headers["apns-collapse-id"]).toBe("s-3f9a");
    expect(c.headers["apns-topic"]).toBe("dev.baton.test");
    const exp = Number(c.headers["apns-expiration"]);
    expect(exp).toBeGreaterThanOrEqual(before + 600);
    expect(exp).toBeLessThanOrEqual(before + 602);
    expect(c.headers.authorization).toMatch(/^bearer [\w-]+\.[\w-]+\.[\w-]+$/);
    expect(c.body).toEqual({
      aps: { alert: FALLBACK_ALERT, "mutable-content": 1, sound: "default" },
      [PAYLOAD_KEY]: { v: 1, ct: body.ciphertext },
    });
  });

  it("maps normal priority to 5 and defaults the ttl", async () => {
    await call("/v1/send", await sendBody(newToken(), "sandbox", { priority: "normal" }));
    expect(apnsCalls[0].headers["apns-priority"]).toBe("5");
    expect(apnsCalls[0].headers["apns-collapse-id"]).toBeUndefined();
    expect(Number(apnsCalls[0].headers["apns-expiration"])).toBeGreaterThan(Date.now() / 1000 + 3500);
  });

  it("rejects a missing, forged or cross-token credential", async () => {
    const t = newToken();
    const good = await sendBody(t);
    expect((await call("/v1/send", { ...good, credential: undefined })).status).toBe(401);
    expect((await call("/v1/send", { ...good, credential: "A".repeat(43) })).status).toBe(403);
    const other = await sendBody(newToken());
    expect((await call("/v1/send", { ...good, credential: other.credential })).status).toBe(403);
    const wrongSecret = await issueCredential("another-relay-secret", "sandbox", t);
    expect((await call("/v1/send", { ...good, credential: wrongSecret })).status).toBe(403);
    expect(apnsCalls).toHaveLength(0);
  });

  it("binds the credential to the APNs environment", async () => {
    const t = newToken();
    const sandbox = await sendBody(t, "sandbox");
    const r = await call("/v1/send", { ...sandbox, env: "production" });
    expect(r.status).toBe(403);
    expect(apnsCalls).toHaveLength(0);
  });

  it("accepts the token in either case", async () => {
    const t = newToken();
    const body = await sendBody(t);
    const r = await call("/v1/send", { ...body, device_token: t.toUpperCase() });
    expect(r.status).toBe(200);
    expect(apnsCalls[0].url.endsWith(t)).toBe(true);
  });

  it("validates ciphertext, collapse id, priority and ttl", async () => {
    const t = newToken();
    const b = await sendBody(t);
    const bad = [
      { ciphertext: "not base64!" },
      { ciphertext: ciphertext(47) },
      { ciphertext: ciphertext(2049) },
      { ciphertext: 7 },
      { collapse_id: "x".repeat(65) },
      { collapse_id: "" },
      { priority: "urgent" },
      { ttl: -1 },
      { ttl: 86401 },
      { ttl: 1.5 },
      { env: "dev" },
      { device_token: "zz" },
    ];
    for (const patch of bad) {
      const r = await call("/v1/send", { ...b, ...patch });
      expect(r.status, JSON.stringify(patch).slice(0, 60)).toBe(400);
    }
    const huge = await call("/v1/send", JSON.stringify({ ...b, pad: "x".repeat(9000) }));
    expect(huge.status).toBe(400);
    expect(apnsCalls).toHaveLength(0);
  });

  it("rate-limits sends per device token", async () => {
    const t = newToken();
    for (let i = 0; i < 20; i++) expect((await call("/v1/send", await sendBody(t))).status).toBe(200);
    const r = await call("/v1/send", await sendBody(t));
    expect(r.status).toBe(429);
    const ra = Number(r.headers.get("retry-after"));
    expect(ra).toBeGreaterThan(0);
    expect(ra).toBeLessThanOrEqual(60);
    expect(apnsCalls).toHaveLength(20);
    // Another device is unaffected.
    expect((await call("/v1/send", await sendBody(newToken()))).status).toBe(200);
  });

  it("tells the daemon to forget unregistered devices", async () => {
    apnsReplies.push(() => Response.json({ reason: "Unregistered", timestamp: 1 }, { status: 410 }));
    const r = await call("/v1/send", await sendBody(newToken()));
    expect(r.status).toBe(410);
    expect(r.json).toEqual({ ok: false, error: "unregistered", reason: "Unregistered" });

    apnsReplies.push(() => Response.json({ reason: "BadDeviceToken" }, { status: 400 }));
    expect((await call("/v1/send", await sendBody(newToken()))).status).toBe(410);
  });

  it("surfaces other APNs failures as 502 / 429", async () => {
    apnsReplies.push(() => Response.json({ reason: "InternalServerError" }, { status: 500 }));
    expect((await call("/v1/send", await sendBody(newToken()))).json).toEqual({
      ok: false,
      error: "apns_error",
      status: 500,
      reason: "InternalServerError",
    });
    apnsReplies.push(() => Response.json({ reason: "TooManyRequests" }, { status: 429 }));
    expect((await call("/v1/send", await sendBody(newToken()))).status).toBe(429);
  });
});

describe("RateLimiter", () => {
  it("resets after the window and cleans up on alarm", async () => {
    const env = testEnv();
    const stub = env.RATE_LIMITER.get(env.RATE_LIMITER.idFromName(await sha256Hex("unit")));
    const rules = [{ name: "w", windowSeconds: 10, max: 2 }];
    expect(await stub.hit(rules, 1_000)).toEqual({ ok: true, retryAfter: 0 });
    expect(await stub.hit(rules, 2_000)).toEqual({ ok: true, retryAfter: 0 });
    expect(await stub.hit(rules, 3_000)).toEqual({ ok: false, retryAfter: 8 });
    expect(await stub.hit(rules, 11_000)).toEqual({ ok: true, retryAfter: 0 });
    await runInDurableObject(stub, async (_obj: RateLimiter, state) => {
      expect(await state.storage.get("w")).toEqual({ start: 11_000, count: 1 });
      expect(await state.storage.getAlarm()).not.toBeNull();
    });
    expect(await runDurableObjectAlarm(stub)).toBe(true);
    await runInDurableObject(stub, async (_obj: RateLimiter, state) => {
      expect((await state.storage.list()).size).toBe(0);
    });
  });

  it("does not consume budget on a rejected hit", async () => {
    const env = testEnv();
    const stub = env.RATE_LIMITER.get(env.RATE_LIMITER.idFromName(await sha256Hex("unit2")));
    const rules = [
      { name: "a", windowSeconds: 60, max: 1 },
      { name: "b", windowSeconds: 60, max: 5 },
    ];
    await stub.hit(rules, 0);
    await stub.hit(rules, 1);
    await stub.hit(rules, 2);
    await runInDurableObject(stub, async (_obj: RateLimiter, state) => {
      expect(await state.storage.get("b")).toEqual({ start: 0, count: 1 });
    });
  });
});
