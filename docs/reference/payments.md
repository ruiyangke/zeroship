# `@zeroship/payments`

`@zeroship/payments` is the client for taking payments from **your own end users**
through Stripe **Connect**. Your customers pay you; the platform takes an
application fee on each charge. That is the opposite money direction from the
infrastructure bill the platform sends you, which
[`billing-metering.md`](./billing-metering.md) documents in full — the fee
policy, the plans, the invoice reads and the account states. This page covers the
payment surface and does not repeat that model.

The application fee is a platform-held policy, **15% of each charge by default**,
and an operator may set a different per-organization policy — a different
percentage, a fixed fee, or a percentage with a cap or a floor. Your code cannot
name, set, or override it: no operation here takes a fee parameter, and the
request body carries business fields only. The platform resolves the policy
server-side and stamps the fee on the charge. There is no endpoint that reads the
policy back, so the per-charge `application_fee_cents` and the earnings totals
below are the only fee figures you can observe. See
[`billing-metering.md`](./billing-metering.md) for how the fee is computed and
who may change it.

The package exports `createPaymentsClient`, the `PaymentsClient` class,
`PaymentsError` and `verifyWebhook`, plus the types `PaymentsClientOptions`,
`CheckoutInput`, `CheckoutResult`, `OnboardingResult`, `VerifyOpts` and
`VerifyResult` so you can name them in your own signatures. The
`@zeroship/payments/testing` subpath exports the signing oracle described below.

> **Read "Package status" before wiring a payment path.** The package's public API
> (client options, methods, error re-export, webhook verification, testing
> oracle) is current, but the HTTP paths and the scoping id it sends are not.
> The platform serves organization-scoped routes; the package still requests
> creator-scoped ones. The runnable path is the organization-scoped contract in
> this page; the section at the end states the gap exactly.

## Money authority is the organization's

A Connect account, its onboarding state and its charges belong to an
**organization** (`org_…`). Every call is authorized against the caller's money
authority at that organization — the `billing` seat, a membership role whose
money authority (`billing_rank`) ranks separately from app authority and
outranks `admin` for money. In the built-in ladder the money seats are `billing`
and `owner`: they may onboard, charge and unlink. `admin` administers apps and
may read earnings, but cannot perform a money action. The `org_…` id is the `id`
from `GET /api/organizations`; a slug is not an id, and a slug sent where an id
belongs resolves to no membership at all. A seat is granted through the
organization membership surface (`addMember` / `changeMemberRole` in
[`control.md`](./control.md)), and only a seat that outranks `billing` and
carries at least its money authority — in the built-in ladder, the owner — can
grant one. [`control.md`](./control.md) is the full ladder and the grant rule.

Every request carries a bearer credential. For a creator that is a **personal
access token (PAT)** produced by `zeroship login`; the CLI stores it and also
accepts `--token=<PAT>` or the `ZEROSHIP_TOKEN` environment variable. Send it as
`Authorization: Bearer <value>`. The credential identifies you; the billing seat
is what authorizes the call at a given organization.

## The simplest flow

The runnable path today is the platform's organization-scoped HTTP routes called
with a personal access token. The package's `startOnboarding()` and `checkout()`
are equivalent calls, but as shipped they target paths the platform does not
serve — see Package status. Everything below is the contract you can rely on.

```ts
const controlUrl = "https://console.zeroship.ai"; // control-plane origin
const token = process.env.ZEROSHIP_TOKEN!;        // from `zeroship login`

// The organization id is the `id` from GET /api/organizations (org_…).
const orgId = "org_0000000002e4nenowz3qmamtd";
const auth = { authorization: `Bearer ${token}` };

// 1. Once per organization: get the Stripe-hosted onboarding link.
const onboard = await fetch(
  `${controlUrl}/api/organizations/${orgId}/stripe/onboard`,
  { method: "POST", headers: auth },
);
const { url } = await onboard.json();
location.href = url; // the organization completes Stripe's own flow

// 2. After Stripe sends the organization back, refresh onboarding status.
const status = await fetch(
  `${controlUrl}/api/organizations/${orgId}/stripe/callback`,
  {
    method: "POST",
    headers: { ...auth, "content-type": "application/json" },
    body: "{}",
  },
);
const { charges_enabled } = await status.json();

// 3. Charge an end user. Business params only; the fee is server-stamped.
const res = await fetch(
  `${controlUrl}/api/organizations/${orgId}/connect/checkout`,
  {
    method: "POST",
    headers: { ...auth, "content-type": "application/json" },
    body: JSON.stringify({
      amount_cents: 1000,
      currency: "usd",
      cart_id: "cart_42", // stable per-cart idempotency key
    }),
  },
);
const { client_secret, application_fee_cents } = await res.json();
// Hand client_secret to Stripe.js in the browser to confirm the payment.
// application_fee_cents is the SERVER-stamped fee, surfaced read-only.
```

Onboarding is a prerequisite, not an optional step: until Stripe enables charges,
`checkout` is refused with a `400` (see Errors). The fee is stamped for you and
returned read-only.

**Stripe terms used here.** Stripe **Connect** is Stripe's product for paying
money to accounts other than your own. The **connected account** (`acct_…`) is
the organization's own Stripe account and receives each charge. A
**PaymentIntent** (`pi_…`) is Stripe's object for one charge; its
`client_secret` is what **Stripe.js** (Stripe's browser library) uses to confirm
the payment in the end user's browser.

## Client options

`createPaymentsClient(options)` returns a client. The options are the control
client's options plus the required scoping id. Missing `baseUrl` or a fetch
implementation, or a missing scoping id, throws immediately rather than on the
first call.

| Option | Required | Meaning |
| --- | --- | --- |
| `baseUrl` | yes | Control-plane origin (`string` or `URL`). |
| `creatorId` | yes | The scoping id. The package types and sends it as a platform user id (`usr_…`); the platform's Connect routes are keyed by organization id (`org_…`); see Package status. |
| `auth` | no | Bearer credential provider: a personal access token (from `zeroship login`) or an operator master key. A value that already carries a scheme is sent unchanged; a bare value becomes `Authorization: Bearer <value>`. |
| `cookie` | no | Cookie-header provider for server-side session forwarding. |
| `headers` | no | Headers applied to every request. |
| `fetch` | no | Overrides `globalThis.fetch`. |
| `onSetCookie` | no | Called for every upstream `Set-Cookie` header. |

`auth`, `cookie`, `headers` and `onSetCookie` may each be a value or a function,
and a function may be async. [`control.md`](./control.md) documents the same
options in more detail. This client is a thin façade over the control client, so
it sends the same `Authorization` and forwarded `cookie` headers, JSON-encodes
the body the same way, and maps a non-2xx response to the same error class.

## Operations

These are the platform's Connect routes. They are the authoritative contract; the
package's two methods build different, creator-scoped paths (see Package status).

| Operation | Platform route | Result |
| --- | --- | --- |
| Start onboarding | `POST /api/organizations/{id}/stripe/onboard` | `{ url, account_id }` |
| Refresh onboarding status | `POST /api/organizations/{id}/stripe/callback` | `{ account_id, charges_enabled, payouts_enabled, details_submitted }` |
| Charge an end user | `POST /api/organizations/{id}/connect/checkout` | `{ payment_intent_id, client_secret, application_fee_cents }` |
| Read earnings | `GET /api/organizations/{id}/earnings` | `{ totals: { gross, fee, net }, recent: [...] }` |
| Unlink the account | `DELETE /api/organizations/{id}/stripe` | `204`, or `404` when none is linked |

The wire bodies use snake_case. `checkout` sends `amount_cents`, `currency`,
`cart_id` and an optional `description`, and reads back `payment_intent_id`,
`client_secret` and `application_fee_cents`. Onboarding reads back `url` and
`account_id`. The package's methods map these to the camelCase fields above.

`GET /api/organizations/{id}/earnings` returns `totals` (`gross`, `fee`, `net`)
and up to 50 recent payouts, each carrying `event_type`, `gross_amount`,
`platform_fee`, `net_amount`, `currency` and `occurred_at`. It is the
after-the-fact fee record. It needs only read authority (`billing`, `owner`, or
`admin`), while the other routes in the table need write authority (`billing` or
`owner`).

## Checkout

`checkout(input)` takes:

| Field | Required | Meaning |
| --- | --- | --- |
| `amountCents` | yes | What to charge the end user, in the currency's minor unit (cents). A positive integer. |
| `currency` | yes | ISO 4217 currency, lowercase 3-letter (for example `"usd"`). |
| `cartId` | yes | Stable per-cart idempotency discriminator. Non-empty. |
| `description` | no | Human-readable description stamped on the charge. |

The client checks locally, before any network call, that `amountCents` is a
positive integer, `currency` is present, and `cartId` is non-empty; it rejects
otherwise with a plain `Error` (not a `PaymentsError`) whose message is one of
`amountCents must be a positive integer`, `currency is required`, or
`cartId is required`. There is no code or field to branch on, so do not branch
on those strings — they are not a contract. The server independently rejects a
zero amount, a currency that is not a lowercase 3-letter ISO code, and a missing
or blank cart id.

**The cart id is the idempotency key.** Idempotency here means a repeated call
with the same discriminator returns the same result instead of creating a second
charge. A retry of the same cart with the same amount and currency replays the
same PaymentIntent; a different cart, or a changed amount or currency, produces a
distinct charge. Reusing one cart id across different amounts would otherwise
collapse them onto one intent, which is why a blank cart id is refused rather
than defaulted.

**The fee is server-stamped.** The request carries no fee field, and a stray
`application_fee*` property forced past the types is dropped before the wire. The
returned `applicationFeeCents` is the platform's resolved fee for this charge,
read-only. It can never exceed the amount charged. The policy itself is not
readable before the charge: the platform exposes no endpoint that returns your
organization's fee policy, so `applicationFeeCents` and the earnings `fee` total
are the only fee figures you can see. The 15% default applies only when an
operator has set no per-organization policy; do not hard-code 15% as your fee.

`description` is capped by the platform at 1000 characters on a character
boundary; a longer value is truncated rather than refused. When omitted, the
charge carries the platform default description, `"zeroship connect charge"`.

## Onboarding

`startOnboarding()` returns `{ url, accountId }`. `url` is the Stripe-hosted
onboarding link to redirect the organization to; `accountId` is the Connect
account id (`acct_…`).

The call is idempotent: a second call resumes the same Connect account rather
than minting another. The account is created server-side and the platform owns
the account id; the client never supplies one.

Onboarding completion is verified server-side against Stripe, not against the
organization's return URL. After Stripe sends the organization back, call
`POST /api/organizations/{id}/stripe/callback` — the dashboard does this on
return — to refresh and read the flags:

```json
{
  "account_id": "acct_…",
  "charges_enabled": true,
  "payouts_enabled": true,
  "details_submitted": true
}
```

`charges_enabled` is what gates `checkout`: until it is `true`, `checkout` is
refused with a `400`. The callback body may carry an optional
`{"stripe_account_id": "acct_…"}` hint, which must equal the account the
platform minted for the organization; a foreign or forged one is refused with
`403`. There is no method on this package for this route, and no method on this
package to read or refresh onboarding state at all; it is reachable only over
HTTP (see Package status).

## Errors

A non-2xx control response throws `PaymentsError`, the control client's error
re-exported so payments callers need a single import. It carries:

| Field | Meaning |
| --- | --- |
| `status` | HTTP status code. |
| `statusText` | HTTP status text. |
| `body` | The parsed response body — the refusal itself. |
| `message` | The server's error text, or `HTTP <status>` when the body carries none. |
| `code` | The body's `code` when the server sends one; normally `undefined` for these endpoints. |
| `trace_id` | The server's correlation id, present only when it answered `{"error": "internal error"}` — an infrastructure failure, not an ordinary refusal. Quote it to an operator. |
| `response` | The original `Response`. |
| `name` | `"ControlError"`. |

The Connect endpoints return their refusals as an `error` string, not a `code`,
so branch on `status` together with `body.error`, never on message text. The
local validation described under Checkout throws a plain `Error` and is not a
`PaymentsError`. [`control.md`](./control.md) documents the error contract shared
with the control client.

| Situation | Result |
| --- | --- |
| Caller lacks billing authority at the organization | `403` `{"error":"forbidden"}` |
| Organization id is not a well-formed typed id | `400` `{"error":"bad organization_id"}` |
| Callback before onboarding | `400` `{"error":"no connect account; call onboard first"}` |
| Callback account hint or account ownership does not match | `403` `{"error":"stripe account not owned by this organization"}` |
| Amount is zero | `400` `{"error":"amount_cents must be positive"}` |
| Currency is not a 3-letter lowercase ISO code | `400` `{"error":"currency must be a 3-letter ISO code (lowercase)"}` |
| Cart id is absent or blank | `400` `{"error":"cart_id is required"}` |
| Organization has no connected account | `400` `{"error":"organization has no connected stripe account"}` |
| Connected account has not finished onboarding | `400` `{"error":"organization stripe account not ready (complete onboarding)"}` |
| Onboarding for an unknown organization | `404` `{"error":"organization not found"}` |
| Platform Stripe is not configured | `500` `{"error":"stripe not configured"}` |
| Stripe rejected the upstream call | `502` `{"error":"stripe upstream error"}` |
| Internal store failure | `500` `{"error":"internal error"}` |

## Webhook verification

`verifyWebhook(rawBody, signatureHeader, secret, opts?)` validates a Stripe
webhook signature and resolves to `{ valid: true, timestamp }` or
`{ valid: false, reason }`. It verifies with HMAC-SHA256 and needs no
library-specific HMAC dependency, so it runs in the app runtime and in browsers,
Node and Bun.

- `rawBody` is the exact bytes Stripe sent — a `string` or, preferably, a
  `Uint8Array`. Never pass re-stringified JSON: whitespace or key-order changes
  fail verification.
- `signatureHeader` is the `Stripe-Signature` header.
- `secret` is the signing secret. An empty secret is invalid, never a wildcard.
- `opts.tolerance` is the allowed clock skew or delivery lag in seconds;
  default `300`. A delivery older than the tolerance is refused as `stale`.
- `opts.now` overrides the clock, returning seconds since the epoch. It is for
  tests.

The header is parsed into a timestamp `t` and a list of `v1` signatures. The
signature is HMAC-SHA256 over `"{t}.{rawBody}"`, and the delivery is accepted if
any `v1` matches in constant time — Stripe sends one `v1` per active secret
during a rotation window. Unknown scheme tags (such as `v0`) are ignored, and
whitespace around the parts is tolerated.

A failure resolves `{ valid: false, reason }` with one of these reasons:

| Reason | Meaning |
| --- | --- |
| `empty signing secret` | `secret` was empty. |
| `missing t` | The header had no `t` element. |
| `bad t` | `t` was absent, empty, non-numeric, or not an integer. |
| `missing v1` | The header had no `v1` element. |
| `stale` | The timestamp is outside the tolerance window. |
| `signature mismatch` | No `v1` matched the computed signature. |

## Local webhook testing

The `@zeroship/payments/testing` subpath exports `signWebhookForTest(rawBody,
secret, timestamp)`, which produces a valid `Stripe-Signature` header in the
format Stripe sends, so a test can feed a body straight into `verifyWebhook`.
`rawBody` is a `string` or `Uint8Array`; `timestamp` is in seconds.

```ts
import { verifyWebhook } from "@zeroship/payments";
import { signWebhookForTest } from "@zeroship/payments/testing";

const body = '{"type":"invoice.paid"}';
const header = await signWebhookForTest(body, "whsec_test_EXAMPLE", 1_700_000_000);
await verifyWebhook(body, header, "whsec_test_EXAMPLE", { now: () => 1_700_000_000 });
// → { valid: true, timestamp: 1700000000 }
```

The signing oracle is **test-only**. It is deliberately kept off the default
package entry so a dependency audit can see at a glance whether production code
imports the signing path. Do not bundle it into production code.

## Package status

The package's public API — client options, the two operations, the
`PaymentsError` re-export, `verifyWebhook` and the testing oracle — is as this
page describes. The HTTP contract it targets is not.

The package builds its requests under `/api/creators/{id}/stripe/onboard` and
`/api/creators/{id}/connect/checkout`, and it types its scoping id as a platform
user id (`usr_…`). The control plane registers no `/api/creators/…` route: the
Connect routes are organization-scoped (`/api/organizations/{id}/…`), and the
`{id}` is an organization id (`org_…`), authorized against a billing seat at
that organization. The platform's truth is the organization-scoped contract in
this page and in [`billing-metering.md`](./billing-metering.md).

Consequently, as shipped, `startOnboarding()` and `checkout()` cannot complete
against the current platform: the paths do not resolve, and the id the package
sends is the wrong kind. Until the package is updated to match, call the
organization-scoped endpoints in the Operations table directly, with an
organization id and a bearer token that carries billing authority at it — a
personal access token from `zeroship login` whose principal holds a `billing` or
`owner` seat at the organization. The non-transport surface — the error shape,
`verifyWebhook`, and the testing oracle — remains usable as documented.