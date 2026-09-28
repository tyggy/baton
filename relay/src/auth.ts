// Send credentials. A credential is HMAC-SHA256(REGISTRATION_SECRET,
// "baton-relay/v1|<env>|<device token>"), base64url. The relay stores
// nothing: it recomputes the MAC on every send. The credential only ever
// leaves the relay inside a push to the device token it names, so holding
// one proves the holder received pushes for that token (see docs/relay.md).
// Rotating REGISTRATION_SECRET revokes every credential at once.

import { b64Decode, b64urlEncode, type ApnsEnv } from "./apns";

const CONTEXT = "baton-relay/v1";

async function hmacKey(secret: string): Promise<CryptoKey> {
  return crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign", "verify"],
  );
}

function message(env: ApnsEnv, deviceToken: string): Uint8Array {
  return new TextEncoder().encode(`${CONTEXT}|${env}|${deviceToken.toLowerCase()}`);
}

export async function issueCredential(
  secret: string,
  env: ApnsEnv,
  deviceToken: string,
): Promise<string> {
  const mac = await crypto.subtle.sign("HMAC", await hmacKey(secret), message(env, deviceToken));
  return b64urlEncode(mac);
}

/** Constant-time check (WebCrypto `verify`). */
export async function verifyCredential(
  secret: string,
  env: ApnsEnv,
  deviceToken: string,
  credential: string,
): Promise<boolean> {
  if (!/^[A-Za-z0-9_-]{43}$/.test(credential)) return false;
  const std = credential.replace(/-/g, "+").replace(/_/g, "/") + "=";
  let mac: Uint8Array;
  try {
    mac = b64Decode(std);
  } catch {
    return false;
  }
  return crypto.subtle.verify("HMAC", await hmacKey(secret), mac, message(env, deviceToken));
}

/** Rate-limit keys are hashes, so the Durable Object never sees a token. */
export async function sha256Hex(s: string): Promise<string> {
  const d = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(s));
  return [...new Uint8Array(d)].map((b) => b.toString(16).padStart(2, "0")).join("");
}
