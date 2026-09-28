// Fixed-window rate limiter, one Durable Object per key (a SHA-256 of the
// device token or client IP). Storage holds only {windowStart, count} per
// rule and is wiped by an alarm once every window has expired, so an idle
// key leaves nothing behind.

import { DurableObject } from "cloudflare:workers";

export interface Rule {
  name: string;
  windowSeconds: number;
  max: number;
}

export interface HitResult {
  ok: boolean;
  retryAfter: number; // seconds; 0 when ok
}

interface Window {
  start: number; // ms
  count: number;
}

export class RateLimiter extends DurableObject {
  /** Count one event against every rule; all-or-nothing (a rejected hit
   *  doesn't consume budget from the rules that still had room). */
  async hit(rules: Rule[], now: number = Date.now()): Promise<HitResult> {
    const storage = this.ctx.storage;
    const windows = new Map<string, Window>();
    let retryAfter = 0;
    for (const r of rules) {
      const span = r.windowSeconds * 1000;
      let w = (await storage.get<Window>(r.name)) ?? { start: now, count: 0 };
      if (now - w.start >= span) w = { start: now, count: 0 };
      windows.set(r.name, w);
      if (w.count >= r.max) {
        retryAfter = Math.max(retryAfter, Math.ceil((w.start + span - now) / 1000));
      }
    }
    if (retryAfter > 0) return { ok: false, retryAfter };
    let expires = 0;
    for (const r of rules) {
      const w = windows.get(r.name)!;
      w.count += 1;
      await storage.put(r.name, w);
      expires = Math.max(expires, w.start + r.windowSeconds * 1000);
    }
    // Relative to the wall clock, so a caller-supplied `now` (tests) can't
    // schedule a cleanup in the past.
    const cleanupAt = Date.now() + (expires - now);
    const alarm = await storage.getAlarm();
    if (alarm === null || alarm < cleanupAt) await storage.setAlarm(cleanupAt);
    return { ok: true, retryAfter: 0 };
  }

  async alarm(): Promise<void> {
    await this.ctx.storage.deleteAll();
  }
}

export const SEND_RULES: Rule[] = [
  { name: "send-min", windowSeconds: 60, max: 20 },
  { name: "send-hour", windowSeconds: 3600, max: 300 },
];

export const REGISTER_TOKEN_RULES: Rule[] = [{ name: "reg-hour", windowSeconds: 3600, max: 5 }];

export const REGISTER_IP_RULES: Rule[] = [{ name: "reg-ip-hour", windowSeconds: 3600, max: 30 }];
