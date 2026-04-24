# Stripe Connect — deferred integration work

The structural pieces landed in commits `862196c` (ledger store) and
`bd6456b` (HTTP handlers). The following items require live Stripe keys
+ creator dashboard UX and are intentionally left for a follow-up pass.

## Still to do

1. **Real onboarding URL**
   `stripe_handlers::onboard` currently returns a placeholder
   `https://connect.stripe.com/express_login?creator=<uuid>` URL. Swap in
   a real call to Stripe's `/v1/account_links` with the platform's
   `STRIPE_SECRET_KEY`. Pseudocode:

   ```rust
   let params = [
       ("account", "acct_xxx"),        // platform-owned connected account
       ("refresh_url", "https://dashboard.zeroship.ai/.../stripe/refresh"),
       ("return_url",  "https://dashboard.zeroship.ai/.../stripe/return"),
       ("type", "account_onboarding"),
   ];
   let resp = cyper::post("https://api.stripe.com/v1/account_links")
       .bearer_auth(platform_secret_key)
       .form(&params)
       .send().await?;
   ```

   Return the `url` field from the response to the dashboard caller.

2. **Dashboard wiring**
   - "Connect Stripe" button on creator settings → POST to
     `/api/creators/:id/stripe/onboard` → redirect user to `url`.
   - Return-URL page reads the query params Stripe appends, extracts
     the `acct_xxx`, and POSTs it to `/api/creators/:id/stripe/callback`.

3. **Webhook endpoint registration**
   Register `https://control.zeroship.ai/internal/webhooks/stripe` in
   the Stripe dashboard. Grab the webhook signing secret (`whsec_xxx`)
   and set it at control-plane startup via either:
   ```
   --stripe-webhook-secret=whsec_xxx
   STRIPE_WEBHOOK_SECRET=whsec_xxx
   ```

4. **CheckoutSession metadata convention**
   Creators MUST include `metadata.creator_id` (the platform-assigned
   UUID) when they build a Checkout Session via
   `@zeroship/payments`'s `buildCheckoutSession`. The ledger uses that
   metadata to attribute webhook events back to the creator. Consider
   auto-injecting it from `env.ZEROSHIP_CREATOR_ID` in a future SDK
   revision so creators can't forget.

5. **Additional event types**
   v1 only records `invoice.paid`. For full accounting, add:
   - `charge.refunded` — subtract from `net_amount`.
   - `charge.dispute.created` / `dispute.closed` — hold funds.
   - `payout.paid` — confirm money actually transferred.

   Each maps to a new `event_type` row in `payouts`; aggregate rules
   stay the same.

6. **Live end-to-end test**
   - Create test creator → onboard via dashboard → land back with `acct_`.
   - Create a price via Stripe dashboard.
   - End-user subscribes via Checkout Session (with `metadata.creator_id`).
   - Stripe fires `invoice.paid` to our webhook.
   - `GET /api/creators/:id/earnings` shows the expected values.
   - 15% platform fee lands in the platform's Stripe balance.

## Out of scope (permanent — intentional design choices)

- Disputes / refunds — Stripe handles the money movement; ledger
  corrections happen via `charge.refunded` events (see item 5).
- Multi-currency reconciliation — amounts are integer minor-units per
  Stripe's wire; downstream reporting is someone else's problem.
- Tax — Stripe Tax handles collection + remittance at checkout time.
- Payout scheduling — Stripe Express default is daily; creators can
  change it in their Express Dashboard.
