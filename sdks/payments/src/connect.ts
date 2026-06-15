//! Thin client for the platform's Stripe **Connect** money-movement endpoints.
//!
//! The platform owns the money path. This SDK is a typed wrapper over the
//! control-plane HTTP API — it carries BUSINESS intent (what to charge, which
//! cart) and nothing else. Crucially, **the SDK cannot name, set, or override
//! the platform application fee**: the fee is server-authoritative (ISS-29).
//! The server resolves each creator's fee policy and stamps
//! `application_fee_amount` on the PaymentIntent itself. There is no fee
//! parameter on any method here, by construction.
//!
//! Conventions (baseUrl, auth, fetch, cookie forwarding, error mapping) are
//! inherited from `@zeroship/control`'s `ControlClient` — this is a thin
//! façade over it, not a new client style. Server errors surface as
//! `ControlError` (re-exported here as `PaymentsError`).

import {
  ControlClient,
  ControlError,
  createControlClient,
  type ControlClientOptions,
} from "@zeroship/control";

/**
 * Options for {@link createPaymentsClient}. A superset of
 * `@zeroship/control`'s client options (baseUrl, auth, fetch, cookie, …) plus
 * the creator the money path is scoped to.
 */
export interface PaymentsClientOptions extends ControlClientOptions {
  /**
   * The creator (`usr_…` / uuid) whose Connect account the calls act on. The
   * server binds this to the authenticated principal (self-service) or an
   * operator with `BillingWrite`.
   */
  creatorId: string;
}

/**
 * Business parameters for a Connect charge. NOTE the absence of any
 * `applicationFeePercent` / `fee` / `applicationFeeAmount` field — the fee is
 * server-authoritative and cannot be expressed here. The platform stamps it.
 */
export interface CheckoutInput {
  /**
   * Amount to charge the end-user, in the currency's minor unit (cents). This
   * is BUSINESS input: the creator names what to charge THEIR customer. It is
   * NOT the fee — the platform computes and stamps the fee server-side.
   */
  amountCents: number;
  /** ISO 4217 currency, lowercase 3-letter (e.g. `"usd"`). */
  currency: string;
  /**
   * Stable per-cart idempotency discriminator. A retry of the SAME cart
   * replays the same PaymentIntent; a different cart (or a changed
   * amount/currency) gets a distinct one. Required.
   */
  cartId: string;
  /** Optional human-readable description stamped on the charge. */
  description?: string;
}

/**
 * Result of {@link PaymentsClient.checkout}. The `clientSecret` is handed to
 * Stripe.js on the end-user's browser to confirm the PaymentIntent. The fee
 * the server stamped is echoed read-only as `applicationFeeCents` for
 * transparency — it is NOT something the caller chose or can change.
 */
export interface CheckoutResult {
  /** The Stripe PaymentIntent id (`pi_…`). */
  paymentIntentId: string;
  /** The PaymentIntent client secret for browser-side confirmation. */
  clientSecret: string;
  /**
   * The platform fee the SERVER stamped on this charge, in cents. Read-only —
   * surfaced for display/reconciliation only; the caller did not and cannot
   * set it.
   */
  applicationFeeCents: number;
}

/**
 * Result of {@link PaymentsClient.startOnboarding}. `url` is the Stripe-hosted
 * onboarding link the creator is redirected to.
 */
export interface OnboardingResult {
  /** The Stripe account-links hosted-onboarding URL to redirect the creator to. */
  url: string;
  /** The creator's Connect account id (`acct_…`). */
  accountId: string;
}

/** Wire shape returned by `POST /api/creators/:id/connect/checkout`. */
interface CheckoutResponseWire {
  payment_intent_id: string;
  client_secret: string;
  application_fee_cents: number;
}

/** Wire shape returned by `POST /api/creators/:id/stripe/onboard`. */
interface OnboardResponseWire {
  url: string;
  account_id: string;
}

/**
 * Thin client over the platform's Connect money-movement endpoints.
 *
 * The whole point (ISS-29): a creator CANNOT name the platform fee. There is
 * no fee parameter on any method — `checkout` posts only business params, and
 * the server stamps `application_fee_amount` from the creator's server-held
 * fee policy.
 */
export class PaymentsClient {
  readonly #control: ControlClient;
  readonly #creatorId: string;

  constructor(options: PaymentsClientOptions) {
    if (!options.creatorId) {
      throw new Error("createPaymentsClient requires a creatorId");
    }
    this.#creatorId = options.creatorId;
    this.#control = createControlClient(options);
  }

  /**
   * Create a Connect charge for the end-user. Posts ONLY business params
   * (`amount_cents`, `currency`, `cart_id`, optional `description`) to
   * `POST /api/creators/:id/connect/checkout`. The server resolves the
   * creator's fee policy and stamps `application_fee_amount` — the body
   * carries NO fee field, and there is no SDK surface to express one.
   *
   * Returns `{ paymentIntentId, clientSecret, applicationFeeCents }` where the
   * fee is the SERVER-stamped value (read-only).
   */
  async checkout(input: CheckoutInput): Promise<CheckoutResult> {
    if (!Number.isInteger(input.amountCents) || input.amountCents <= 0) {
      throw new Error("amountCents must be a positive integer");
    }
    if (!input.currency) throw new Error("currency is required");
    if (!input.cartId || input.cartId.trim() === "") {
      throw new Error("cartId is required");
    }

    // BUSINESS PARAMS ONLY. No fee field exists on this body, by design — the
    // platform owns the fee (ISS-29). Build the body explicitly (rather than
    // spreading `input`) so the wire shape can never carry a stray property.
    const body: {
      amount_cents: number;
      currency: string;
      cart_id: string;
      description?: string;
    } = {
      amount_cents: input.amountCents,
      currency: input.currency,
      cart_id: input.cartId,
    };
    if (input.description !== undefined) {
      body.description = input.description;
    }

    const res = await this.#control.request<CheckoutResponseWire>(
      `/api/creators/${pathPart(this.#creatorId)}/connect/checkout`,
      { method: "POST", body },
    );
    return {
      paymentIntentId: res.payment_intent_id,
      clientSecret: res.client_secret,
      applicationFeeCents: res.application_fee_cents,
    };
  }

  /**
   * Start (or resume) Stripe Connect onboarding via
   * `POST /api/creators/:id/stripe/onboard`. Returns the hosted onboarding URL
   * the creator should be redirected to (and their `acct_…`). Idempotent: a
   * second call resumes the SAME account.
   */
  async startOnboarding(): Promise<OnboardingResult> {
    const res = await this.#control.request<OnboardResponseWire>(
      `/api/creators/${pathPart(this.#creatorId)}/stripe/onboard`,
      { method: "POST" },
    );
    return { url: res.url, accountId: res.account_id };
  }
}

/** Construct a {@link PaymentsClient}. Mirrors `createControlClient`. */
export function createPaymentsClient(
  options: PaymentsClientOptions,
): PaymentsClient {
  return new PaymentsClient(options);
}

/**
 * The error thrown on a non-2xx control-plane response. Re-export of
 * `@zeroship/control`'s `ControlError` so payments callers have a single
 * import; `.status`, `.code`, and `.body` carry the server's error detail
 * (e.g. 400 `cart_id is required`, `creator stripe account not ready`).
 */
export { ControlError as PaymentsError };

function pathPart(value: string): string {
  return encodeURIComponent(value);
}
