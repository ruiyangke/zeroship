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
  /** Platform fee in %. Default 15. Must be 0–100. */
  applicationFeePercent?: number;
  /** Where Stripe sends the user after a successful checkout. */
  successUrl: string;
  /** Where Stripe sends the user after a cancelled checkout. */
  cancelUrl: string;
  /** Optional prefill. */
  customerEmail?: string;
  /** Arbitrary key/value pairs. Persisted on the Subscription. */
  metadata?: Record<string, string>;
}

export interface CheckoutSessionRequest {
  method: "POST";
  url: string;
  headers: Record<string, string>;
  body: string;
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

  const fee = opts.applicationFeePercent ?? 15;
  if (!Number.isFinite(fee) || fee < 0 || fee > 100) {
    throw new Error(`applicationFeePercent must be 0..=100, got ${fee}`);
  }

  const params = new URLSearchParams();
  params.set("mode", "subscription");
  params.set("line_items[0][price]", opts.priceId);
  params.set("line_items[0][quantity]", "1");
  params.set("success_url", opts.successUrl);
  params.set("cancel_url", opts.cancelUrl);
  params.set("subscription_data[application_fee_percent]", String(fee));
  if (opts.customerEmail) params.set("customer_email", opts.customerEmail);
  for (const [k, v] of Object.entries(opts.metadata ?? {})) {
    params.set(`metadata[${k}]`, v);
  }

  return {
    method: "POST",
    url: "https://api.stripe.com/v1/checkout/sessions",
    headers: {
      authorization: `Bearer ${apiKey}`,
      "stripe-account": opts.creatorAccountId,
      "content-type": "application/x-www-form-urlencoded",
    },
    body: params.toString(),
  };
}
