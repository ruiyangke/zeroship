# Adding A Metering Provider

Billing providers plug into the control plane through the registry in
`crates/zeroship-control/src/metering/provider/`. A provider is a self-contained adapter:
it implements the capability traits it supports, validates its own config in a
factory, registers one id in `provider/adapters/mod.rs`, and passes the shared
provider conformance suite.

The provider is the canonical billing boundary. The event forwarder ships usage
events to the selected meter provider, provider-side deduplication handles
at-least-once delivery, and reconciliation uses the provider's declared
correction capability to repair drift.

## Provider Surface

The base trait is `MeteringProvider` in
`crates/zeroship-control/src/metering/provider/mod.rs`.

Every adapter implements:

- `id() -> &str`: the stable registry id, such as `openmeter` or `lago`.
- `capabilities() -> Capabilities`: the roles this adapter serves.
- `as_meter()`, `as_invoicer()`, and `as_backfiller()`: downcasts for the
  capability traits.
- `dedup() -> DedupContract`: the provider's idempotency key and TTL semantics.
- `correction() -> CorrectionCapability`: how reconciliation can correct drift.
- `production_ready() -> bool`: return `false` for evaluation-only providers.

The capability flags must match the `as_*` methods. Boot calls
`assert_capability_consistency`, so a provider that declares `Capabilities::METER`
but returns `None` from `as_meter()` fails closed before the server starts.

Implement only the sub-traits your provider actually supports:

- `Meter`: `ingest`, `read_aggregate`, and `ensure_subject`.
- `Invoicer`: closes a billing period and records adjustment notes.
- `Backfiller`: sends a corrected total when `CorrectionCapability::Backfill`
  is declared.

## Config And Secrets

Each adapter owns its config shape. Define a private `serde::Deserialize` struct
in the adapter file and parse it in the factory:

```rust
#[derive(Debug, serde::Deserialize)]
struct AcmeCfg {
    api_url: String,
    api_key: SecretHandle,
    meter_code: String,
}

pub fn factory(ctx: &ProviderCtx) -> Result<Arc<dyn MeteringProvider>, ProviderError> {
    let cfg: AcmeCfg = ctx.parse_adapter_config("acme")?;
    let api_key = ctx.secrets.resolve(&cfg.api_key)?;

    if cfg.api_url.trim().is_empty() {
        return Err(ProviderError::Config(
            "acme: api_url required - refusing to boot".to_string(),
        ));
    }
    if api_key.expose_secret().trim().is_empty() {
        return Err(ProviderError::Config(
            "acme: api_key resolved empty".to_string(),
        ));
    }

    Ok(Arc::new(AcmeProvider::new(cfg, api_key, ctx.http)))
}
```

Config parsing must be fail-closed. Missing endpoint URLs, meter identifiers,
webhook secrets, or read-back fields are boot errors, not warnings.

Secrets are handles. Use `SecretHandle` fields and resolve them through
`ctx.secrets.resolve(...)`; do not put plaintext secrets in provider config or
log output. `ProviderCtx` also exposes the per-thread HTTP client factory, clock,
raw JSON config, and optional `LiteStore` for providers that reuse the platform's
local invoice store.

## Registration

Add the adapter file under `crates/zeroship-control/src/metering/provider/adapters/`, then
register it in `adapters/mod.rs`:

```rust
pub mod acme;

pub fn register_builtin(registry: &mut ProviderRegistry) {
    registry.register("acme", acme::factory);
}
```

No core trait, enum, stack-builder, forwarder, or cron code should change for a
new provider.

## Correction Capability

Declare the correction path the reconciler can use:

- `CorrectionCapability::Backfill { window, closed }`: the provider accepts a
  corrected total or compensating event. Implement `Backfiller`.
- `CorrectionCapability::InvoiceCredit`: the provider can record a signed
  adjustment note or credit on the invoice path.
- `CorrectionCapability::None`: drift is recorded as a reconciliation finding
  and requires operator action.

The `dedup()` contract and `correction()` contract must describe the real
provider behavior. Do not advertise unbounded deduplication or closed-period
backfill unless the provider actually guarantees it.

## Worked Examples

`openmeter` is a meter-only adapter in
`crates/zeroship-control/src/metering/provider/adapters/openmeter.rs`.

- Config: `base_url`, `token`, `event_type`, and `meter_slug`.
- Capabilities: `METER`.
- `ingest` sends CloudEvents.
- `read_aggregate` queries the meter slug for a subject and period.
- Dedup contract: source plus event id with unbounded provider-side storage.
- Correction capability: none; pair it with an invoicer that can correct bills.

`lago` is a full-stack adapter in
`crates/zeroship-control/src/metering/provider/adapters/lago.rs`.

- Config: `api_url`, `api_key`, and `billable_metric_code`.
- Capabilities: `METER | INVOICE`.
- `ingest` posts usage events with transaction ids.
- `read_aggregate` reads current usage for the configured billable metric.
- `Backfiller` posts a corrected total within its declared backfill window.
- Dedup contract: transaction id with unbounded provider-side storage.

## Conformance

Run the shared suite after adding or changing an adapter:

```bash
nix develop --command cargo test -p zeroship-control --test provider_conformance
```

The suite checks capability/downcast consistency, fail-closed config, retry
idempotency, aggregate read-back, dedup TTL behavior, invoice close idempotency,
webhook verification, and correction capability declarations. Add a fixture for
the new provider in `crates/zeroship-control/tests/provider_conformance.rs` with a DB-free
recording backend or mock HTTP server.
