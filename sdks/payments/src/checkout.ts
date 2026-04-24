//! Stripe Checkout Session builder.
//!
//! Returns a prepared HTTP request the caller can POST to Stripe with
//! `fetch`. The SDK is network-agnostic on purpose — the same bundle
//! runs in-worker, in-node, or in a test mock without conditional code.

export interface CreateCheckoutOpts {
  /** The `price_...` id for the subscription plan. */
  priceId: string;
  /** The creator's connected account — `acct_...`. */
  creatorAccountId: string;
  /** Platform fee in %. Default 15. Must be in [0, 100]. */
  applicationFeePercent?: number;
  /** Where Stripe sends the user after a successful checkout. */
  successUrl: string;
  /** Where Stripe sends the user after a cancelled checkout. */
  cancelUrl: string;
  /** Optional prefill. */
  customerEmail?: string;
  /**
   * Arbitrary key/value pairs. Written to BOTH the Checkout Session
   * metadata AND `subscription_data[metadata]` — Stripe doesn't
   * propagate session metadata to the subscription/invoice on its own,
   * so webhook handlers looking at `invoice.paid` events would
   * otherwise see no metadata at all.
   *
   * Must include `creator_id` so the platform's webhook ingest can
   * attribute revenue. The platform API will reject a session with no
   * `creator_id` key.
   */
  metadata?: Record<string, string>;
  /**
   * Pin a Stripe API version (optional). Defaults to whatever Stripe
   * promotes for the platform account, which may silently change.
   * Recommended to pin to a known-good version in production.
   */
  stripeVersion?: string;
}

export interface CheckoutSessionRequest {
  method: "POST";
  url: string;
  headers: Record<string, string>;
  body: string;
}

/** Metadata keys Stripe allows: /^[a-zA-Z0-9_]{1,40}$/ per the docs. */
const METADATA_KEY_RE = /^[a-zA-Z0-9_]{1,40}$/;

function isHttpUrl(s: string): boolean {
  try {
    const u = new URL(s);
    return u.protocol === "https:" || u.protocol === "http:";
  } catch {
    return false;
  }
}

/**
 * Build the HTTP request that creates a Stripe Checkout Session in the
 * creator's connected account, with zeroship taking the specified
 * `application_fee_percent`. Caller posts with `fetch(req.url, req)`.
 *
 * Errors are thrown for programmer mistakes (bad inputs); Stripe-side
 * errors come back as normal HTTP error responses from `fetch`.
 */
export function buildCheckoutSession(
  apiKey: string,
  opts: CreateCheckoutOpts,
): CheckoutSessionRequest {
  if (!apiKey) throw new Error("apiKey is required");
  if (!opts.priceId) throw new Error("priceId is required");
  if (!opts.creatorAccountId) throw new Error("creatorAccountId is required");
  if (!opts.successUrl) throw new Error("successUrl is required");
  if (!opts.cancelUrl) throw new Error("cancelUrl is required");

  if (!isHttpUrl(opts.successUrl)) throw new Error("successUrl must be a valid http(s) URL");
  if (!isHttpUrl(opts.cancelUrl)) throw new Error("cancelUrl must be a valid http(s) URL");

  const fee = opts.applicationFeePercent ?? 15;
  if (!Number.isFinite(fee) || fee < 0 || fee > 100) {
    throw new Error(`applicationFeePercent must be in [0, 100], got ${fee}`);
  }

  const metadata = opts.metadata ?? {};
  for (const k of Object.keys(metadata)) {
    if (!METADATA_KEY_RE.test(k)) {
      throw new Error(`metadata key '${k}' must match /^[a-zA-Z0-9_]{1,40}$/`);
    }
  }

  const params = new URLSearchParams();
  params.set("mode", "subscription");
  params.set("line_items[0][price]", opts.priceId);
  params.set("line_items[0][quantity]", "1");
  params.set("success_url", opts.successUrl);
  params.set("cancel_url", opts.cancelUrl);
  params.set("subscription_data[application_fee_percent]", String(fee));
  if (opts.customerEmail) params.set("customer_email", opts.customerEmail);

  // Write metadata on BOTH the session AND the subscription it will
  // create. Stripe stores these in separate fields; the session's
  // metadata never propagates to invoices on its own, so the webhook
  // ingest (which fires on invoice.paid) needs `subscription_data[metadata]`
  // set explicitly. Mirroring them on both surfaces means the same keys
  // are visible to payment-intent hooks AND invoice hooks.
  for (const [k, v] of Object.entries(metadata)) {
    params.set(`metadata[${k}]`, v);
    params.set(`subscription_data[metadata][${k}]`, v);
  }

  const headers: Record<string, string> = {
    authorization: `Bearer ${apiKey}`,
    "stripe-account": opts.creatorAccountId,
    "content-type": "application/x-www-form-urlencoded",
  };
  if (opts.stripeVersion) headers["stripe-version"] = opts.stripeVersion;

  return {
    method: "POST",
    url: "https://api.stripe.com/v1/checkout/sessions",
    headers,
    body: params.toString(),
  };
}
