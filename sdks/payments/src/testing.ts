//! Test-only helpers. Importing this module gives you a webhook-signing
//! oracle — NEVER bundle it into production code. Kept out of the
//! default package entry (`./index.ts`) so a dependency audit can see
//! at a glance whether a caller is using the signing path.

/**
 * Generate a valid Stripe-Signature header for a given body + timestamp
 * + secret. The output matches the format Stripe sends on real webhook
 * deliveries — tests can feed it straight into `verifyWebhook`.
 */
export async function signWebhookForTest(
  rawBody: string | Uint8Array,
  secret: string,
  timestamp: number,
): Promise<string> {
  const bodyBytes = typeof rawBody === "string" ? new TextEncoder().encode(rawBody) : rawBody;
  const prefix = new TextEncoder().encode(`${timestamp}.`);
  const toSign = new Uint8Array(prefix.length + bodyBytes.length);
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
  const bytes = new Uint8Array(sig);
  let hex = "";
  for (let i = 0; i < bytes.length; i++) hex += bytes[i].toString(16).padStart(2, "0");
  return `t=${timestamp},v1=${hex}`;
}
