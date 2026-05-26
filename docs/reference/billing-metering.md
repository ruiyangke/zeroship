# Billing And Metering

This page documents the billing and usage plumbing that is present in the code today. The larger metering system described in older drafts has not shipped.

## Usage reporting

The worker-to-control usage payload is defined in [crates/core/src/types.rs](../../crates/core/src/types.rs) as `UsageReport` with per-app `AppUsage` counters:

- `requests`
- `cpu_us`
- `wall_us`
- `egress_bytes`
- `ingress_bytes`

The control-plane ingestion endpoint is implemented in [crates/control/src/internal.rs](../../crates/control/src/internal.rs). It records usage through the registry code in [crates/control/src/registry.rs](../../crates/control/src/registry.rs).

The current metering module itself is only a stub entry point: [crates/control/src/metering.rs](../../crates/control/src/metering.rs).

## Stripe integration that exists today

The current Stripe handlers live in [crates/control/src/stripe_handlers.rs](../../crates/control/src/stripe_handlers.rs), with persistence in [crates/control/src/stripe_store.rs](../../crates/control/src/stripe_store.rs).

Verified pieces in the tree:

- Creator onboarding endpoints exist in the control plane.
- Webhook signature verification is implemented in Rust and mirrored by [sdks/payments/src/webhook.ts](../../sdks/payments/src/webhook.ts).
- The SDK package is `@zeroship/payments`, exporting helpers from [sdks/payments/src/index.ts](../../sdks/payments/src/index.ts).

The current checkout helper is [sdks/payments/src/checkout.ts](../../sdks/payments/src/checkout.ts). It builds Stripe Checkout session parameters and expects creator attribution in metadata.

## Current limitation

The onboarding handler currently returns a placeholder Stripe Express URL rather than creating a live Account Link. See [crates/control/src/stripe_handlers.rs](../../crates/control/src/stripe_handlers.rs).
