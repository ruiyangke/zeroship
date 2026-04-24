//! Webhook signature verification.
//!
//! Stripe signs every webhook body with a timestamp-prefixed HMAC-SHA256.
//! Header shape:
//!   Stripe-Signature: t=1492774577,v1=<hex>[,v0=<hex>]
//!
//! Verification steps (per stripe.com/docs/webhooks/signatures):
//!   1. Parse header into a map.
//!   2. Compute HMAC-SHA256(secret, "{t}.{rawBody}").
//!   3. Compare to v1 in constant time.
//!   4. Reject if |now - t| > tolerance (default 300s) — replay guard.
//!
//! Uses WebCrypto, so the same code runs in browser, runtime V8, node,
//! and bun without a library-specific HMAC dep.

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
  rawBody: string,
  signatureHeader: string,
  secret: string,
  opts: VerifyOpts = {},
): Promise<VerifyResult> {
  const tolerance = opts.tolerance ?? 300;
  const now = (opts.now ?? (() => Math.floor(Date.now() / 1000)))();

  // --- Parse header ---
  const parts = new Map<string, string>();
  for (const part of signatureHeader.split(",")) {
    const eq = part.indexOf("=");
    if (eq <= 0) continue;
    parts.set(part.slice(0, eq).trim(), part.slice(eq + 1).trim());
  }
  const tStr = parts.get("t");
  const v1 = parts.get("v1");
  if (!tStr || !v1) return { valid: false, reason: "missing t/v1" };
  const timestamp = Number(tStr);
  if (!Number.isFinite(timestamp)) return { valid: false, reason: "bad t" };

  // --- Replay guard ---
  if (Math.abs(now - timestamp) > tolerance) {
    return { valid: false, reason: "stale" };
  }

  // --- Signature check ---
  const key = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const sig = await crypto.subtle.sign(
    "HMAC",
    key,
    new TextEncoder().encode(`${timestamp}.${rawBody}`),
  );
  const expected = [...new Uint8Array(sig)]
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");

  if (!timingSafeEqual(expected, v1)) {
    return { valid: false, reason: "signature mismatch" };
  }
  return { valid: true, timestamp };
}

/** Constant-time string compare. Returns false for mismatched lengths. */
function timingSafeEqual(a: string, b: string): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) {
    diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  }
  return diff === 0;
}

/**
 * Helper (test-only): generate a valid signature header for a given
 * body + timestamp + secret. Useful for tests and for simulating
 * webhook deliveries in dev.
 */
export async function signWebhookForTest(
  rawBody: string,
  secret: string,
  timestamp: number,
): Promise<string> {
  const key = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const sig = await crypto.subtle.sign(
    "HMAC",
    key,
    new TextEncoder().encode(`${timestamp}.${rawBody}`),
  );
  const hex = [...new Uint8Array(sig)]
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
  return `t=${timestamp},v1=${hex}`;
}
