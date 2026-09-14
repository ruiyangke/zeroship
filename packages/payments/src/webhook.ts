//! Webhook signature verification.
//!
//! Stripe signs every webhook body with a timestamp-prefixed HMAC-SHA256.
//! Header shape:
//!   Stripe-Signature: t=1492774577,v1=<hex>[,v1=<hex>...][,v0=<hex>]
//!
//! Verification steps (per stripe.com/docs/webhooks/signatures):
//!   1. Parse header into timestamp `t` and a LIST of `v1` signatures
//!      (Stripe can deliver multiple during secret rotation).
//!   2. Compute HMAC-SHA256(secret, "{t}.{rawBody}").
//!   3. Compare to EACH v1 in constant time — accept if any match.
//!   4. Reject if |now - t| > tolerance (default 300s) — replay guard.
//!
//! Uses WebCrypto, so the same code runs in browser, runtime V8, node,
//! and bun without a library-specific HMAC dep.
//!
//! `rawBody` MUST be the exact bytes Stripe sent — NEVER re-stringified
//! JSON (whitespace/key-order differences will fail verification). If
//! you can, prefer `Uint8Array` so there's no encoding ambiguity.

export interface VerifyOpts {
  /** Seconds of allowed clock skew / delivery lag. Default 300. */
  tolerance?: number;
  /**
   * Override the clock — primarily for tests. Returns seconds since
   * epoch. Defaults to `Math.floor(Date.now() / 1000)`.
   */
  now?: () => number;
}

export type VerifyResult =
  | { valid: true; timestamp: number }
  | { valid: false; reason: string };

export async function verifyWebhook(
  rawBody: string | Uint8Array,
  signatureHeader: string,
  secret: string,
  opts: VerifyOpts = {},
): Promise<VerifyResult> {
  if (!secret) return { valid: false, reason: "empty signing secret" };
  const tolerance = opts.tolerance ?? 300;
  const now = (opts.now ?? (() => Math.floor(Date.now() / 1000)))();

  // --- Parse header ---
  let tStr: string | undefined;
  const v1s: string[] = [];
  let hadT = false;
  for (const part of signatureHeader.split(",")) {
    const eq = part.indexOf("=");
    if (eq <= 0) continue;
    const k = part.slice(0, eq).trim();
    const v = part.slice(eq + 1).trim();
    if (k === "t") { hadT = true; tStr = v; }
    else if (k === "v1") v1s.push(v);
  }
  if (!hadT) return { valid: false, reason: "missing t" };
  if (tStr === undefined || tStr === "") return { valid: false, reason: "bad t" };
  const timestamp = Number(tStr);
  if (!Number.isFinite(timestamp) || !Number.isInteger(timestamp)) {
    return { valid: false, reason: "bad t" };
  }
  if (v1s.length === 0) return { valid: false, reason: "missing v1" };

  // --- Replay guard ---
  if (Math.abs(now - timestamp) > tolerance) {
    return { valid: false, reason: "stale" };
  }

  // --- Signature check ---
  const bodyBytes = typeof rawBody === "string" ? new TextEncoder().encode(rawBody) : rawBody;
  const toSign = new Uint8Array(
    new TextEncoder().encode(`${timestamp}.`).length + bodyBytes.length,
  );
  const prefix = new TextEncoder().encode(`${timestamp}.`);
  toSign.set(prefix, 0);
  toSign.set(bodyBytes, prefix.length);

  const key = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const sig = await crypto.subtle.sign("HMAC", key, toSign);
  const expected = bytesToHex(new Uint8Array(sig));

  // Try every v1 in constant time. During secret rotation Stripe
  // sends one v1 per active secret — we accept the event if any
  // matches ours.
  let matched = 0;
  for (const v1 of v1s) {
    matched |= timingSafeEqual(expected, v1);
  }
  if (matched === 0) return { valid: false, reason: "signature mismatch" };
  return { valid: true, timestamp };
}

function bytesToHex(bytes: Uint8Array): string {
  let out = "";
  for (let i = 0; i < bytes.length; i++) {
    out += bytes[i].toString(16).padStart(2, "0");
  }
  return out;
}

/**
 * Constant-time string compare. Returns 1 on match, 0 otherwise.
 * MUST NOT short-circuit on length mismatch or first-differing byte.
 */
function timingSafeEqual(a: string, b: string): number {
  // If lengths differ we still accumulate; force zero at the end.
  const len = Math.min(a.length, b.length);
  let diff = 0;
  for (let i = 0; i < len; i++) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return a.length === b.length && diff === 0 ? 1 : 0;
}
