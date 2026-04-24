# Stripe Connect — Creator Monetization

**Goal:** Enable creators to accept subscription revenue from their apps' end users, with zeroship auto-taking a 15% platform fee. Full flow: creator onboards via Stripe Express → platform stores `stripe_account_id` → end user subscribes via platform-minted Checkout Session → platform receives `application_fee` → creator ledger tracks accrued earnings.

**Architecture:**
- `@zeroship/payments` npm SDK: high-level creator-facing API (`createCheckoutSession`, `createPortalSession`, `verifyWebhook`). Uses `env.STRIPE_KEY` (secret) + `env.STRIPE_WEBHOOK_SECRET` (secret) which creators set via `zeroship secret set`.
- Control plane: two new tables + five new endpoints (creator onboarding, OAuth callback, webhook ingest, ledger query).
- Webhook validation: HMAC-SHA256 against Stripe's header timestamp + body, per Stripe's standard "Signature verification" recipe. Pure crypto, unit-testable without network.

**Tech stack:** Rust (control plane), TypeScript (SDK). HTTP client is our existing `cyper`. Webhook HMAC uses `hmac` crate (already a workspace dep).

**Executable vs. deferred:**
- ✅ Executable now (this session): webhook signature verification, ledger schema + CRUD, SDK type scaffolding, unit tests with synthetic fixtures.
- ⏸ Deferred (needs real Stripe keys + dashboard UX): Express OAuth onboarding flow, live checkout session creation, end-to-end payout verification. Stripe publishes signed test webhooks for dev environments — we'll wire those when the creator dashboard has somewhere to redirect to.

---

## File Structure

**New files:**
- `sdks/payments/package.json`, `sdks/payments/src/index.ts` (~120 LOC): SDK entry.
- `sdks/payments/src/webhook.ts` (~80 LOC): signature verification.
- `sdks/payments/src/checkout.ts` (~80 LOC): Checkout Session builder (returns a typed request the caller can POST to Stripe's `/v1/checkout/sessions`).
- `sdks/payments/tests/webhook.test.ts`: unit tests for `verifyWebhook`.
- `crates/control/src/stripe_store.rs` (~200 LOC): creator_accounts + subscriptions + payouts tables, CRUD.
- `crates/control/src/stripe_handlers.rs` (~200 LOC): HTTP handlers for onboarding, OAuth callback, webhook ingest, ledger query.
- `crates/control/tests/stripe_store.rs` (~150 LOC): integration tests.

**Modified files:**
- `crates/control/src/lib.rs`: export new modules.
- `crates/control/src/main.rs`: register routes.
- `crates/control/Cargo.toml`: add `hmac` + `hex` (already transitively present).

**Out of scope:**
- Creator dashboard UI (separate product work).
- Refund / dispute handling (v2).
- Multi-currency + tax (Stripe handles — we just pass through).
- Payout scheduling (Stripe Express defaults: daily).

---

## Task C1: `@zeroship/payments` SDK — webhook signature verification

**Files:**
- Create: `sdks/payments/package.json`, `sdks/payments/src/{index,webhook,checkout}.ts`, `sdks/payments/tests/webhook.test.ts`.

- [ ] **Step 1: package.json**

```json
{
  "name": "@zeroship/payments",
  "version": "0.1.0",
  "type": "module",
  "exports": { ".": "./src/index.ts" },
  "files": ["src"],
  "devDependencies": { "typescript": "^5", "vitest": "^2" }
}
```

- [ ] **Step 2: `src/webhook.ts` — signature verification**

Stripe sends a header:
```
Stripe-Signature: t=1492774577,v1=5257a869e7ecebeda32affa62cdca3fa5... [,v0=...]
```

Verification recipe (per Stripe's docs):
1. Split on `,` → extract `t`, `v1`.
2. `expected = HMAC-SHA256(secret, "${t}.${rawBody}")`.
3. Constant-time compare `expected` hex to `v1`.
4. Optional: enforce `|now - t| <= tolerance` (default 300s) for replay protection.

```ts
// sdks/payments/src/webhook.ts
export interface VerifyOpts {
  tolerance?: number; // seconds; default 300
}

export async function verifyWebhook(
  rawBody: string,
  signatureHeader: string,
  secret: string,
  opts: VerifyOpts = {},
): Promise<{ valid: true; timestamp: number } | { valid: false; reason: string }> {
  const tolerance = opts.tolerance ?? 300;

  const parts = new Map<string, string>();
  for (const part of signatureHeader.split(",")) {
    const eq = part.indexOf("=");
    if (eq > 0) parts.set(part.slice(0, eq).trim(), part.slice(eq + 1).trim());
  }
  const tStr = parts.get("t");
  const v1 = parts.get("v1");
  if (!tStr || !v1) return { valid: false, reason: "missing t/v1" };
  const timestamp = Number(tStr);
  if (!Number.isFinite(timestamp)) return { valid: false, reason: "bad t" };

  const now = Math.floor(Date.now() / 1000);
  if (Math.abs(now - timestamp) > tolerance) return { valid: false, reason: "stale" };

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

  if (!timingSafeEqual(expected, v1)) return { valid: false, reason: "signature mismatch" };
  return { valid: true, timestamp };
}

/** Constant-time string compare. Both inputs must have the same length. */
function timingSafeEqual(a: string, b: string): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return diff === 0;
}
```

- [ ] **Step 3: `src/checkout.ts` — Checkout Session builder**

```ts
// sdks/payments/src/checkout.ts
export interface CreateCheckoutOpts {
  priceId: string;
  creatorAccountId: string;           // acct_xxx from the creator
  applicationFeePercent?: number;      // defaults to 15
  successUrl: string;
  cancelUrl: string;
  customerEmail?: string;
  metadata?: Record<string, string>;
}

export interface CheckoutSessionRequest {
  method: "POST";
  url: "https://api.stripe.com/v1/checkout/sessions";
  headers: Record<string, string>;
  body: string;                        // application/x-www-form-urlencoded
}

/**
 * Build the HTTP request to create a Stripe Checkout Session in the
 * creator's connected account. Caller posts this via `fetch` — we
 * keep the SDK network-agnostic so it runs unchanged in browser /
 * runtime / node.
 */
export function buildCheckoutSession(
  apiKey: string,
  opts: CreateCheckoutOpts,
): CheckoutSessionRequest {
  const fee = opts.applicationFeePercent ?? 15;
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
```

- [ ] **Step 4: `src/index.ts` — re-exports**

```ts
export { verifyWebhook } from "./webhook";
export { buildCheckoutSession } from "./checkout";
export type { CreateCheckoutOpts, CheckoutSessionRequest } from "./checkout";
```

- [ ] **Step 5: `tests/webhook.test.ts`**

Unit tests using synthetic fixtures — generate signatures the same way Stripe does, then verify.

- [ ] **Step 6: Commit**

```bash
git add sdks/payments/
git commit -m "sdks/payments: SDK scaffold — verifyWebhook + buildCheckoutSession"
```

---

## Task C2: Control-plane tables + `StripeStore`

**Files:**
- Create: `crates/control/src/stripe_store.rs`
- Modify: `crates/control/src/registry.rs` (migrations), `crates/control/src/lib.rs`, `crates/control/src/main.rs`.

Tables:
- `creator_accounts(creator_id UUID PRIMARY KEY REFERENCES auth_users, stripe_account_id TEXT NOT NULL, onboarded_at TIMESTAMPTZ)` — one Stripe Express account per creator.
- `payouts(id UUID PRIMARY KEY, creator_id UUID REFERENCES creator_accounts, event_id TEXT UNIQUE, event_type TEXT, gross_amount BIGINT, platform_fee BIGINT, net_amount BIGINT, currency TEXT, occurred_at TIMESTAMPTZ)` — one row per ledger event. `event_id` is Stripe's `evt_xxx` for idempotency.

`StripeStore` API:
- `link_account(creator_id, stripe_account_id)` / `unlink_account` / `get_account`
- `record_payout(event_id, creator_id, gross, fee, currency, occurred_at) -> bool` (false on duplicate `event_id`)
- `total_earnings(creator_id) -> (gross, fee, net)`
- `recent_payouts(creator_id, limit)` for dashboard

Follows the same patterns as `EnvStore`.

- [ ] **Commit:** `control: StripeStore + creator_accounts / payouts tables`

---

## Task C3: Control-plane HTTP handlers

**Files:** `crates/control/src/stripe_handlers.rs`

Endpoints:
- `POST /api/creators/:id/stripe/onboard` (master-key) — creates a Stripe Connect onboarding link, returns `{ url }` for dashboard redirect. Stubbed: returns `https://connect.stripe.com/setup/s/<token>` as a placeholder since we can't hit live Stripe.
- `POST /api/creators/:id/stripe/callback` (master-key) — accepts the `acct_xxx` the creator returned with; calls `link_account`.
- `POST /internal/webhooks/stripe` (no auth — Stripe doesn't send a bearer; we verify the signature instead) — reads raw body, validates via `verifyWebhook`, dispatches on `event.type` to `record_payout`.
- `GET /api/creators/:id/earnings` (master-key) — returns `total_earnings` + `recent_payouts(limit=50)`.
- `DELETE /api/creators/:id/stripe` (master-key) — unlink.

Webhook event handling: v1 supports `invoice.paid` (primary revenue signal for subscriptions). Each event contributes `gross = amount_paid`, `fee = application_fee_amount`, `net = gross - fee`.

- [ ] **Commit:** `control: Stripe onboard + webhook + earnings endpoints`

---

## Task C4: Integration tests

**Files:** `crates/control/tests/stripe_store.rs`

Tests (all against live Postgres, env-gated):
- `link_account_roundtrip` — create, link, fetch, unlink.
- `payout_idempotent` — same `event_id` twice, second call returns false.
- `total_earnings_aggregates_correctly` — several payouts, totals match sum.
- `recent_payouts_ordered` — newest first.
- `per_creator_isolation` — two creators don't see each other's payouts.

Plus SDK-side tests:
- `verify_webhook_accepts_valid_signature` — generate a test signature with known secret + body + timestamp; verify it.
- `verify_webhook_rejects_tampered` — flip a byte in body; verify fails.
- `verify_webhook_rejects_expired` — timestamp > tolerance old.

- [ ] **Commit:** `test: Stripe store + webhook signature verification`

---

## Task C5: Deferred integration notes (documented, not coded)

Put in `docs/stripe-integration-todo.md`:

- [ ] Hook up the creator dashboard's "Connect Stripe" button to
  `POST /api/creators/:id/stripe/onboard` and redirect to the returned URL.
- [ ] Configure the Stripe webhook endpoint to hit
  `https://control.zeroship.ai/internal/webhooks/stripe` with
  `STRIPE_WEBHOOK_SECRET` set on the platform side.
- [ ] Switch `stripe_handlers::onboard` from returning a placeholder URL
  to calling Stripe's `/v1/account_links` API with real keys.
- [ ] Live end-to-end test with Stripe test mode: create a creator,
  onboard, create a price, end user subscribes, verify payout row
  appears + platform fee is 15%.

---

## Out of scope (permanent)

- Disputes / refunds / chargebacks — handled separately when we have
  actual creator support tooling.
- Multi-currency reconciliation — Stripe does this; we just record amounts.
- Tax — Stripe Tax handles it at checkout.
- Platform side payout scheduling — Stripe Express defaults apply.
