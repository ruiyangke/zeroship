# @zeroship/payments

The creator-facing payments SDK for zeroship. A **thin client** over the
platform control plane for Stripe **Connect** money movement, plus a
runtime-agnostic webhook signature verifier.

## The platform owns the fee

This SDK **cannot name, set, or override the platform application fee**. That is
the whole point (ISS-29). There is no `applicationFeePercent`, no `fee`, no
`applicationFeeAmount` parameter anywhere in the surface — the creator literally
cannot express a fee.

When you call `checkout`, the SDK posts **business params only** (`amountCents`,
`currency`, `cartId`). The control plane resolves the creator's server-held fee
policy and stamps `application_fee_amount` on the Stripe PaymentIntent itself.
The fee is server-authoritative; the client never touches it. (Fee policy is
operator-only and lives in the admin surface, not here.)

The earlier version of this SDK built the Stripe Checkout Session directly in
creator-controlled code with `application_fee_percent` defaulting to 15% —
trivially settable to 0. That path is **deleted**. The money path now runs
server-side.

## Usage

```ts
import { createPaymentsClient } from "@zeroship/payments";

const payments = createPaymentsClient({
  baseUrl: "https://console.zeroship.ai", // control-plane origin
  creatorId: "usr_…",                      // the creator this acts on
  auth: () => myBearerToken,               // PAT / session — same shape as @zeroship/control
});

// 1. Onboard the creator to Stripe Connect (redirect them to `url`).
const { url } = await payments.startOnboarding();
location.href = url;

// 2. Charge an end-user. Business params ONLY — the platform stamps the fee.
const { clientSecret, paymentIntentId, applicationFeeCents } =
  await payments.checkout({
    amountCents: 1000,
    currency: "usd",
    cartId: "cart_42", // stable per-cart idempotency key
  });
// → hand `clientSecret` to Stripe.js on the browser to confirm the PaymentIntent.
// `applicationFeeCents` is the SERVER-stamped fee, surfaced read-only.
```

### Configuration

`createPaymentsClient` accepts every `@zeroship/control` `ControlClientOptions`
field — `baseUrl`, `fetch`, `auth`, `cookie`, `headers`, `onSetCookie` — plus
the required `creatorId`. It is a thin façade over `ControlClient`, so auth,
cookie forwarding, JSON encoding, and error mapping are identical.

### Errors

A non-2xx control response throws `PaymentsError` (re-export of
`@zeroship/control`'s `ControlError`). It carries `.status`, `.code`, and
`.body`:

```ts
import { PaymentsError } from "@zeroship/payments";

try {
  await payments.checkout({ amountCents: 100, currency: "usd", cartId: "c" });
} catch (e) {
  if (e instanceof PaymentsError && e.status === 400) {
    // e.g. "creator stripe account not ready (complete onboarding)"
  }
}
```

## Endpoints

| SDK method          | Control-plane endpoint                         | Returns                                          |
| ------------------- | ---------------------------------------------- | ------------------------------------------------ |
| `startOnboarding()` | `POST /api/creators/:id/stripe/onboard`        | `{ url, accountId }`                              |
| `checkout(input)`   | `POST /api/creators/:id/connect/checkout`      | `{ paymentIntentId, clientSecret, applicationFeeCents }` |

## Webhook verification

`verifyWebhook(rawBody, signatureHeader, secret, opts?)` validates a Stripe
webhook signature with WebCrypto HMAC-SHA256 (runs in V8/browser/Node/bun). Pass
the **exact** bytes Stripe sent — never re-stringified JSON. See `src/webhook.ts`.
A test-only signing oracle lives at `@zeroship/payments/testing`.
