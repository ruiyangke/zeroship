# Billing Provider Platform — stream-to-provider metering with two self-registering registries

- **Status:** Proposal (design only — not implemented; commit lands with the implementing PR)
- **Date:** 2026-07-08
- **Version:** v7 (see `## Changelog v6→v7` at the end — an ENFORCEMENT SIMPLIFICATION round driven by an owner decision: **enforcement freshness is relaxed from sub-minute to a TUNABLE CADENCE, default HOURLY.** This DELETES the sub-minute Redis-counter machinery — the shared Redis spend counter, the forwarder's best-effort `INCRBY`, the sub-minute evaluator, and the `MAX` re-base all GO; `libs/compio-redis` leaves the enforcement path. Enforcement becomes a periodic batch — the LOCAL per-app recompute (the SAME batch that is §6.3's billing `witness`, one recompute two consumers) run at the cadence, priced, written to `app_spend_state`, pulled by the gateway (~5s) and enforced instantly at the edge. Detection = cadence; action = instant. Overshoot (≤ `R × cadence`) is bounded by a coarse per-app throughput cap at the gateway. **The BILLING half is UNTOUCHED** (provider delegation, §6.2 forward+dedup, §6.3 correction spine, settle-window, registries, capability traits, Lite). v6's earlier "governing insight" — ENFORCEMENT CANNOT BE PROVIDER-SOURCED — is preserved: enforcement is still LOCAL-per-app-sourced, now via a once-per-cadence batch rather than a continuous Redis counter)
- **Branch:** `design/billing-provider-platform`
- **Owner-locked decisions:** stream-to-provider only (no in-house columnar meter), millions-of-users scale target, durable STREAM behind a pluggable `StreamTransport` seam (Redpanda default via `rust-rdkafka`/librdkafka), Postgres holds NO raw events, zero tokio *runtime* in-process, Lite = a real evaluation-grade self-hosted provider (not a dev-only test double), primary prod target = OpenMeter(meter)+Stripe(invoicer) or a single full-stack provider. **v5 owner decisions (governing): (1) the metering PROVIDER is the SINGLE canonical source of truth for usage + billing — zeroship keeps NO parallel local exactly-once ledger; (2) enforcement needs NOT per-event exactly-once accuracy, so it is a self-healing guardrail sourced from a **local per-app stream recompute** (the provider is off the enforcement path), not a fenced transactional fold. These two decisions DELETE machinery (the §6.1 atomic fold, `stream_offsets`, `fence_epoch`/CAS, the rebalance/zombie apparatus) rather than add it. v7 owner decision (governing): enforcement freshness is relaxed from sub-minute to a TUNABLE CADENCE (default HOURLY, tightenable e.g. to 15-min). This DELETES the sub-minute machinery too — the shared Redis counter, the best-effort `INCRBY`, the sub-minute evaluator, and the `MAX` re-base are all removed; enforcement becomes a periodic batch (the LOCAL per-app recompute at the cadence → `app_spend_state` → gateway pull → instant edge action) with overshoot bounded by a coarse per-app throughput cap. `libs/compio-redis` leaves the enforcement path entirely.**
- **Supersedes at land time:** `docs/reference/billing-metering.md` (the "Native reconciler is the default production billing pipeline" framing) and the closed-enum provider seam.

---

## 0. Governing principle (v5) — ONE exactly-once boundary (the provider), not two

<!-- Added in round 5: the whole reframe. v1–v4 conflated two DIFFERENT accuracy requirements into one correctness-critical local exactly-once fold and then spent three rounds (R2 must-fix #1, R3/R4 CRITICAL fence_epoch) making that fold safe under multi-forwarder rebalance. v5 splits the two requirements and keeps exactly-once in ONE place — the provider — which deletes the fold and everything built to fence it. -->

**v5 splits the two accuracy requirements v1–v4 conflated. There is exactly ONE exactly-once boundary, and it is the provider.**

- **BILLING = EXACTLY-ONCE = the PROVIDER.** The provider (OpenMeter / Stripe / Metronome / Orb / Lago, or the self-hosted `lite` provider) is the **single canonical source of truth** for usage AND the biller. The forwarder ships events **at-least-once**; the provider **dedups on `event_id` within its `DedupContract.ttl`** (§6.2, unchanged); the billing **reconciliation/correction spine** (§6.3 — `CorrectionCapability`, `Backfiller`, signed `adjustment_note`, `(subject, period, correction_seq)` idempotency keys) is **UNCHANGED**. Money stays exact because exactly-once lives at the provider, where the events actually land and where the invoice is rated.

- **ENFORCEMENT = A PERIODIC LOCAL BATCH AT A TUNABLE CADENCE (NOT exactly-once, NOT sub-minute) + LOCAL-SOURCED + PER-APP, with INSTANT edge action.** Spend limiting is a **guardrail with hysteresis** (Warn ~80% → Degrade ~95% → Block ~100%, `spend.rs::derive_state`, `SpendThresholds`). A guardrail tolerates bounded lateness, so it needs neither exactly-once nor sub-minute freshness. **v7 relaxes detection to a periodic batch at a tunable cadence (default 1 hour, tightenable e.g. to 15-min).** At each cadence tick the **LOCAL per-app recompute** — `Σ` per `(subject=app, metric, period)` from the retained stream, the SAME batch that is §6.3's billing `witness` (one recompute, two consumers) — is priced through the CU model (`pricing::charge_cents`) and written to `app_spend_state` (Postgres). The **gateway pulls `app_spend_state` (~5s, cached per-node, the existing route-registry pull) and enforces INSTANTLY at the edge** (`enforce::check_spend` reading `RouteEntry.spend_state`): Warn notify · Degrade throttle · Block 402-before-dispatch. **Detection = the cadence; the enforcement ACTION stays instant.** There is no Redis counter, no per-event increment, and no re-base — the recompute IS the count, computed at the cadence. It **never produces a wrong bill**, because it is not the bill.

<!-- v7: the governing insight is unchanged (enforcement is LOCAL-per-app-sourced, provider off the enforcement path); what changes is that the local source runs at a tunable cadence instead of continuously into a Redis counter. -->

**The governing insight (unchanged from v6): ENFORCEMENT CANNOT BE PROVIDER-SOURCED.** Spend limits are **per-app** (`app_spend_limit` / `app_spend_state`; AGENTS.md — "a per-app configurable spend limit"). The provider meters **per-creator/subject** (some providers can attribute ONLY at creator grain — OQ-2) **AND it lags**. So the provider **cannot source enforcement**: it (a) cannot reconstruct N per-app numbers from one creator aggregate and (b) lags behind true consumed usage → **under-enforce**. Enforcement therefore uses a **local per-app usage source** — the periodic batch recompute of per-app period totals from the retained stream (`witness`, §6.3), which is per-app by construction (the stream is partition-keyed by `subject`, which carries `app_id`). This does NOT contradict "the provider is the single canonical source for BILLING": **enforcement and billing have different grains** (per-app vs per-creator) and **different freshness needs** (cadence guardrail vs settled-at-close invoice), so they legitimately draw from different sources. The provider is **entirely off the enforcement path** (billing only).

<!-- v7: the new bound. Detection is up to one cadence-period behind, so overshoot is bounded by a coarse per-app throughput cap. -->

**Bounding overshoot — the throughput backstop (v7).** Because detection now runs up to one cadence-period behind, a runaway app can overshoot its cap by at most `R × cadence`, where `R` = its maximum spend rate. This is bounded by a **coarse per-app hard rate/concurrency cap at the gateway** — which largely already exists (`enforce::RateLimitRegistry` / `ConcurrencyRegistry`, `crates/zeroship-gateway/src/enforce.rs`). Capping `R` makes **worst-case overshoot ≤ R × cadence bounded regardless of the cadence.** The **free tier** (uncapped-by-card, so it must not overshoot catastrophically) gets a specifically tighter cap. Both the **cadence** and the **per-app throughput cap** are tunable operator knobs.

**Why this is a simplification, not a new layer.** v5 already deleted the LOCAL exactly-once fold — the §6.1 atomic `usage_aggregates += / spend_dirty / offset` PG transaction, the `stream_offsets` table, the `fence_epoch` monotonic-CAS, and the whole rebalance/zombie-double-fold defence (the R3/R4 CRITICAL). **v7 deletes the machinery v5/v6 ADDED to keep the guardrail sub-minute** — the shared Redis counter, the forwarder's best-effort `INCRBY`, the sub-minute evaluator, and the conservative `MAX` re-base. What remains is close to what the platform already ships: `spend.rs::evaluate_all` already prices per-app usage and writes `app_spend_state`; the gateway already pulls it (~5s) and enforces at the edge (`enforce::check_spend`). Enforcement is now that batch, sourced from the stream recompute, run once per cadence. `libs/compio-redis` leaves the enforcement path; a once-per-cadence O(apps) job is trivially tractable (no Redis-cluster counter sharding, no CHWBL-sharded sub-minute evaluator).

```
                          ┌──────────────── EXACTLY-ONCE (money, per-creator) ────────────────┐
 worker → Redpanda stream → forwarder ──at-least-once ingest──► PROVIDER (canonical meter + biller)
              │               │                                   ▲ dedup(event_id, ttl) · §6.2
              │               │                                   │ §6.3 correction spine (UNCHANGED)
              │               └─ forwarder ONLY: ship to provider + commit Kafka offset
              │                  (no Redis, no INCRBY)             │  billing basis / health x-check
              │        local per-app recompute (§6.3 witness)      ▼  ← close AFTER settle window
              └───────► batch SUM per (subject=app, metric, period)
                        │  ONE recompute serves BOTH:
                        │  (a) ENFORCEMENT: price → app_spend_state (Postgres)  ← at the CADENCE (default 1h)
                        │  (b) BILLING provider-loss witness (§6.3)
                          ┌── PERIODIC BATCH (guardrail, per-app, cadence detection) ──┐
                          └► app_spend_state ──5s pull──► gateway: INSTANT edge action
                                                          (Warn/Degrade/Block)
        overshoot ≤ R × cadence, bounded by a coarse per-app throughput cap at the gateway
```

The rest of this document is unchanged in its BILLING half (the two registries, capability sub-traits, dedup-TTL forwarding, the correction spine, Lite) and simplified in its ENFORCEMENT half to a periodic-cadence recompute + instant gateway edge + a throughput backstop.

---

## 1. Context

The infrastructure usage billing path shipped as an in-house pipeline: workers pre-aggregate per-app atomic counters (`crates/zeroship-metering/src/meter.rs`), flush a `UsageReport` over HTTP to control (`crates/zeroship-metering/src/flush.rs`), control dedups + aggregates into `usage_aggregates` (`crates/zeroship-control/src/metering/mod.rs`), a spend engine enforces caps (`crates/zeroship-control/src/spend.rs`), and a Stripe reconciler invoices (`crates/zeroship-control/src/cron/billing_reconcile.rs`). A `MeteringProvider` trait (`crates/zeroship-control/src/metering/provider/mod.rs`) was retrofitted on top so usage can *also* be exported to OpenMeter / Stripe Billing Meters, but production billing is still the in-house "Native" rail.

The platform owner has decided to **invert this at scale**. The design target is now explicitly **millions of end-users** of creator apps. The pipeline must be a purpose-built high-throughput streaming design, not "Postgres-as-event-store." Two owner decisions define the new shape:

1. **Stream-to-provider only.** zeroship does NOT run its own columnar meter (no ClickHouse, no in-house rating warehouse). The chosen third-party provider **IS** the meter + invoicer at scale. zeroship runs exactly three things on the money path: **producers** (worker + data-primitive counters) → a **durable stream** → an **event-forwarder** that ships events to the provider. Enforcement (v7) is a **periodic local batch** — the per-app stream recompute run at a tunable cadence (default 1h), priced into `app_spend_state`, which the gateway pulls and enforces at the edge — so spend caps never call a provider on the request path and need no Redis counter. That is the whole pipeline.
2. **The provider is the canonical source of truth for usage AND billing; the stream is the buffer; Postgres holds no raw events AND no per-event ledger.** Control Postgres keeps only config, spend limits/state, invoice bookkeeping, reconciliation findings, and the periodic per-app spend state the enforcement batch writes. The v1 idea of an immutable `usage_events` table in Postgres as the SoT is **removed** — it does not scale to millions of subjects × per-request events, and it duplicates what the provider already stores authoritatively. **v5 goes further:** there is no local *exactly-once* aggregate either. The provider is the single canonical number for **billing** (per-creator, settled); enforcement (per-app) is derived by a **periodic local per-app recompute of the retained stream** (§Pillar 4/§6.3 `witness`) — NOT from the provider (the provider lags and meters at the wrong grain for a per-app cap; see §0 governing insight). **v7:** that recompute runs at a tunable cadence into `app_spend_state`; there is no Redis enforcement counter.

This document is the design for that streaming inversion plus the structural fixes the current seam needs.

zeroship is **pre-launch with an explicit no-back-compat mandate** (`AGENTS.md` → "Development status — pre-launch, no back-compat"). No `@deprecated` aliases, no migration shims, no legacy-mode fallbacks: rename / break / delete freely and update every caller in the same change. This design takes full advantage of that — it removes the closed enum, the `&AppState`-coupled verbs, the `(worker_id, sequence)` dedup model, and the Postgres event-store idea outright rather than layering over them.

---

## 2. Goals & non-goals

### Goals

1. **Two extensibility seams, each "one file + a fixed 2-line delta."** Adding a **billing provider** OR a **stream transport** is a self-contained adapter: one new file + a `mod` line + a `register` line in that seam's tiny index, with ZERO edits to the core pipeline, either trait, any enum, or a central config struct. This is the headline success criterion (see §5.1 / §5.2 for the honest edit count).
2. **Stream-to-provider at millions-of-users scale.** Producers → durable stream → event-forwarder → provider. No in-house columnar store; no Postgres raw-event table.
3. **Durable STREAM behind a pluggable `StreamTransport` seam** (Redpanda default, `rust-rdkafka`/librdkafka producer — C threads, no async runtime). The seam is honestly **Kafka-family-scoped** (log-structured: partition offsets + consumer groups + partition-key ordering); Kafka / Kinesis-Kafka-API / EventHubs-Kafka-API drop in as one adapter, and a non-Kafka bus must MEET that documented contract or is out of scope (§Pillar 2).
4. **Keep enforcement (spend caps) simple, per-app, and provider-independent** — a **periodic local per-app recompute at a tunable cadence** (default 1h) priced into `app_spend_state`, pulled by the gateway (~5s) and enforced INSTANTLY at the edge; never a provider call and no Redis counter anywhere on the enforcement path (v7, §0/§Pillar 4/5). Enforcement is a guardrail whose detection lags by at most one cadence and whose overshoot is bounded by a coarse per-app throughput cap — not an exactly-once ledger, not provider-sourced, and no longer a continuously-incremented shared counter.
5. **Exactly-once billing at the provider** (the single canonical usage/billing SoT): forward at-least-once + provider `event_id` dedup, and a period reconciliation/correction spine that cannot double-bill even when a replay outruns a provider's dedup-identifier TTL (§6). Resolve bill-01/03/06 and bound/address bill-02/04/05.
6. **Delegate metering/invoicing to a configurable provider in production**; keep the in-house pipeline alive as a real, self-hosted **evaluation-grade `LiteProvider`** (§Pillar 6) — production-readiness-gated, not "dev-only."
7. **Zero tokio *runtime* in-process.** Link-level tokio (via cyper→hyper-util, being separately removed) is tolerated; no new component instantiates a tokio reactor. librdkafka = C background threads; provider/stream forwarding over cyper (compio). See §13.

### Non-goals

- Stripe Connect payment processing. `creator_fee_policy` / `invoice_payments` / the Connect path are orthogonal (but see section 12.4 for the customer-model collision note).
- Building a new metering third party or a hosted rating DSL. Rating lives inside the invoicer/provider close path (the existing `charge_cents` model for Lite/`stripe_invoice`).
- Changing the CU pricing model (`crates/zeroship-control/src/pricing.rs`), the plan catalog, or the FeePolicy shapes.
- An object-storage/Parquet cold archive of raw events for provider-independence. It is an **Open Question** (OQ-6), NOT built by default under "stream-to-provider only."

---

## 3. The locked decisions (design to these — do not relitigate)

| # | Decision | Consequence in this design |
| --- | --- | --- |
| A1 | **Scale target: millions of users.** Purpose-built streaming pipeline; new infra is OK. | §Pillar 4. Per-request events collapse to windowed deltas; the stream absorbs volume, not Postgres. |
| A2 | **Stream-to-provider only.** Provider IS meter+invoicer AND the single canonical usage/billing SoT **for BILLING** (per-creator, settled); zeroship runs producers → durable stream → forwarder. Enforcement (v7) is a **periodic per-app batch recompute** from the retained stream at a tunable cadence → `app_spend_state` → gateway edge (not the provider, no Redis counter — §0 governing insight). | §Pillar 4 + §Event-forwarder. No ClickHouse, no in-house warehouse, no local exactly-once ledger, no Redis enforcement counter. Provider off the enforcement path. |
| A3 | **Durable STREAM behind a `StreamTransport` seam** (Redpanda default, librdkafka producer). | §Pillar 2 + §7. Second registry, symmetric to the provider registry — scoped to the Kafka log family with a documented offset/consumer-group/partition-key contract. **v5:** Kafka's own offset store holds the consumer position (committed after provider-ship + counter-incr) — there is no PG `stream_offsets` table. |
| A4 | **Postgres holds NO raw events AND no per-event exactly-once ledger (v5).** Only config, spend limits/state, invoice bookkeeping, reconciliation findings, and the periodic per-app spend state the cadence batch writes. | §9. `usage_events`-as-SoT is DELETED; the exactly-once `usage_aggregates` fold + `stream_offsets` are DELETED; enforcement is a periodic batch writing `app_spend_state` (v7 — no Redis counter). |
| A5 | **Zero tokio *runtime* in-process.** Link-level tokio tolerated; no new reactor. | §13. librdkafka C threads; cyper HTTP on compio. |
| A6 | **Lite = real evaluation-grade self-hosted provider**, production-readiness-gated (not dev-only). Primary prod = OpenMeter(meter)+Stripe(invoicer) OR one full-stack provider. Metronome/Orb/Lago registry-addable later. | §Pillar 6. Local-only invoice sink for zero-external-account trials. |
| L1 | **One file + a fixed 2-line delta** to add a provider OR a stream transport. | §Pillar 1/2 (registries + capability sub-traits). Primary success criterion. |
| L2 | Provider-neutral, capability-typed adapters; construction context is narrow (`ProviderCtx`), never `&AppState`. | §Pillar 3. |

---

## 4. Current-state analysis (with file:line)

### 4.1 The provider seam is a closed enum with per-kind config and a match

- **Closed enum:** `MeteringProviderKind { Native, Stripe, OpenMeter }` — `provider/types.rs:127-135`, `parse()` hardcodes the three names (`types.rs:143-150`).
- **Per-kind Option config struct:** `MeteringProviderConfig { kind, stripe_meter: Option<…>, openmeter: Option<…> }` — `provider/mod.rs:219-259`. Every new provider adds an `Option<…Config>` field.
- **Central build match:** `build_provider()` — `provider/mod.rs:273-325`.
- **Hardcoded CLI surface + duplicated guards:** `--metering-provider`, `--stripe-meter-*`, `--openmeter-*` — `main.rs:109-214` — plus boot guards at `main.rs:965-1010` **and** inside `build_provider`.

**To add a provider today you edit ≥ six sites.** The exact opposite of L1.

### 4.2 The trait is export/invoice-only, per-creator-CU, and `&AppState`-coupled

`MeteringProvider` (`provider/mod.rs:52-122`) has six verbs — `kind`, `ensure_customer`, `report_usage`, `reported_total`, `invoice`, `handle_webhook` — and **every verb takes `&AppState`** (`provider/mod.rs:59,77,95,105,116`). An adapter cannot be constructed or unit-tested without a whole `AppState`. `report_usage` carries a pre-summed per-creator `compute_units: u64` (`provider/mod.rs:77-85`): the seam only ever moves one scalar CU per `(creator, period)`, computed by the export cron (`cron/metering_export.rs`), not events.

**Crucially — the read-back-delta model is deliberate.** `reported_total` (`provider/mod.rs:88-94`) exists so the export cron pushes `current_local − reported_total`, precisely so "a crash-then-re-drive past the external dedup window NEVER double-counts … does not depend on any time-bounded idempotency window." `StripeMeterConfig.meter_id` (`provider/mod.rs:139-143`) is documented as REQUIRED for exactly this reconcile, because Stripe's meter-event `identifier` dedup is only **~24h**. Any v2 that leans on naive per-event replay must preserve an equivalent safety net (§6).

### 4.3 The worker→control path is pre-aggregated and sequence-deduped (bill-01, bill-03, bill-06)

- **Pre-aggregation (bill-06):** the counter core is welded to `UsageReport` + the HTTP POST — `build_report` stamps a `sequence` (`meter.rs:273-287`), `flush.rs:134-178` POSTs it. No event granularity, no seam between "count" and "ship."
- **Sequence dedup (bill-01):** control dedups on `(worker_id, sequence)` via `usage_reports_seen`. On a lost ack the flush task **re-sends the drained snapshot under a FRESH sequence** (`flush.rs:97-129`). The comment at `flush.rs:116-120` claims this "never double-counts" — **wrong**: if the POST landed but the ack was lost, the same deltas re-apply under a never-seen `(worker_id, sequence)` → double-count.
- **Unbounded dedup ledger (bill-03):** `usage_reports_seen` grows one row per `(worker_id, sequence)` forever; nothing GCs it.

### 4.4 Enforcement is local but the sweep is O(apps) every tick (bill-02, bill-05)

`SpendEngine::evaluate_all` (`spend.rs:237-425`) reads **every** app (`SELECT ... FROM zeroship.apps`, `spend.rs:213-217`), prices each, and UPSERTs `app_spend_state` **every tick even when nothing changed** (`spend.rs:408-422`, for dashboard freshness). Already partly batched, but O(apps) per tick (bill-02). Enforcement rides the 5s registry pull of `RouteEntry.spend_state`, so cap→enforce lag is `sweep_interval + pull_interval` (bill-05), undocumented. `limit_changed` (`spend.rs:387`) is detected only when the app is actually evaluated — a naive dirty-set that skips idle apps would skip the app whose limit just changed (see §Pillar 5).

### 4.5 Internal auth is a flat shared secret (bill-04)

`/internal/usage` authenticates with a single process-wide `control_key` bearer (`internal.rs:27-40,253-257`). Every worker, and anything holding the key, can post usage for any app. No per-worker identity, no scoping.

---

## 5. Target architecture — the two seams and six pillars

The design has **two symmetric registries** — one for **billing providers** (who meters/rates/invoices) and one for **stream transports** (how events travel durably from producer to forwarder). Both satisfy L1 identically. The six pillars build on them.

### Pillar 1 — Extensible provider REGISTRY (kills the closed enum)

Replace the enum + per-kind `Option` config + `build_provider` match with a **string-keyed factory registry**.

```rust
// crates/zeroship-control/src/metering/provider/registry.rs  (NEW)

/// A provider factory: given the narrow construction context, build the provider
/// or fail closed. The registry wraps the result to enforce capability↔downcast
/// consistency at build time (see Pillar 2).
pub type ProviderFactory =
    fn(&ProviderCtx) -> Result<std::sync::Arc<dyn MeteringProvider>, ProviderError>;

#[derive(Default)]
pub struct ProviderRegistry {
    factories: std::collections::HashMap<&'static str, ProviderFactory>,
}

impl ProviderRegistry {
    pub fn register(&mut self, id: &'static str, factory: ProviderFactory) {
        assert!(self.factories.insert(id, factory).is_none(), "duplicate provider id {id}");
    }

    /// Build the selected provider, then assert capability↔downcast consistency
    /// (Pillar 2) so a mis-declared adapter fails at boot, not at first dispatch.
    pub fn build(&self, id: &str, ctx: &ProviderCtx)
        -> Result<std::sync::Arc<dyn MeteringProvider>, ProviderError>
    {
        let f = self.factories.get(id).ok_or_else(|| ProviderError::Config(
            format!("unknown metering provider '{id}' — known: {}", self.known().join(", "))))?;
        let p = f(ctx)?;                    // each factory validates ITS OWN config
        assert_capability_consistency(&*p)?; // METER bit ⇔ as_meter().is_some(), etc.
        Ok(p)
    }

    pub fn known(&self) -> Vec<&'static str> {
        let mut v: Vec<_> = self.factories.keys().copied().collect();
        v.sort_unstable(); v
    }
}
```

Config becomes **provider-agnostic**: `--metering-provider <id>` selects the factory; a single generic `--provider-config <json>` (or a `provider_config` DB row) carries an opaque JSON blob that **each adapter parses and validates itself**. Secrets are NOT plaintext in that blob — they are secret *handles* resolved through `ctx.secrets` (§Pillar 3, resolves critique #13). The hardcoded per-provider CLI flags and duplicated boot guards are DELETED.

```rust
// crates/zeroship-control/src/metering/provider/adapters/openmeter.rs  (NEW, self-contained)
#[derive(serde::Deserialize)]
struct OpenMeterCfg { base_url: String, token: SecretHandle, event_type: String, meter_slug: String }

pub fn factory(ctx: &ProviderCtx) -> Result<Arc<dyn MeteringProvider>, ProviderError> {
    let cfg: OpenMeterCfg = ctx.parse_config()?;
    let token = ctx.secrets.resolve(&cfg.token)?;     // secret backend, not plaintext
    if cfg.base_url.trim().is_empty() || token.is_empty() {
        return Err(ProviderError::Config(
            "openmeter: base_url + token required - refusing to boot (silent billing gap)".into()));
    }
    if cfg.meter_slug.trim().is_empty() {
        return Err(ProviderError::Config("openmeter: meter_slug required for aggregate read-back".into()));
    }
    // `ctx.http` is a per-thread client FACTORY, never a shared client (Pillar 3).
    Ok(Arc::new(OpenMeterProvider::new(cfg, token, ctx.http.clone(), ctx.clock.clone())))
}
```

#### 5.1 The L1 proof (honest edit count)

| To add a provider | **Today** (closed enum) | **After** (registry) |
| --- | --- | --- |
| New adapter file | ✅ `provider/<name>.rs` | ✅ `provider/adapters/<name>.rs` |
| Enum variant / `parse()` arm | ✏️ `types.rs:127-150` | — |
| Config struct field + constructor | ✏️ `mod.rs:220-259` | — |
| `build_provider` match arm + guards | ✏️ `mod.rs:273-325` | — |
| CLI flag struct + `main.rs` boot guards | ✏️ `main.rs:109-214,965-1010` | — |
| Registration index | ✏️ `mod.rs:34-37` | ✏️ `adapters/mod.rs`: a `mod` line + a `register` line |
| **Total core edits** | **≥ 6 files** | **1 new file + exactly 2 lines in the tiny index** |

The honest claim is **"one file + a fixed 2-line delta"** (a `mod` line and a `register` line) — not "one line." The two lines live in the seam's own index (`adapters/mod.rs`), not in the pipeline, the trait, any enum, or a central config struct, so L1 ("zero edits to the core") holds. (Resolves critique #12, which flagged the v1 "one file + one line" vs "two lines" inconsistency.)

```rust
// crates/zeroship-control/src/metering/provider/adapters/mod.rs  (NEW)
pub mod lite; pub mod openmeter; pub mod stripe_meters; pub mod stripe_invoice;
pub mod metronome; pub mod orb; pub mod lago;

pub fn register_builtin(r: &mut ProviderRegistry) {
    r.register("lite",           lite::factory);
    r.register("openmeter",      openmeter::factory);
    r.register("stripe_meters",  stripe_meters::factory);
    r.register("stripe_invoice", stripe_invoice::factory);  // §Pillar 3, resolves critique #3
    r.register("metronome",      metronome::factory);
    r.register("orb",            orb::factory);
    r.register("lago",           lago::factory);
}
```

**Registration mechanism.** An explicit `register_builtin()` list (RECOMMENDED) over `inventory`/`linkme` auto-registration: the list is greppable, deterministic, adds no dependency, and cannot be dead-code-eliminated by `--release` LTO + `gc-sections` (a link-section `submit!` referenced by nothing can be silently dropped - a *silent billing gap*). The 2-line delta is small enough that robustness wins.

### Pillar 2 — Extensible STREAM-TRANSPORT REGISTRY (the second seam) — scoped to the Kafka-family

The durable buffer is a **pluggable stream**, introduced with a registry EXACTLY like the provider one — same L1 property. **But the trait is honestly Kafka-shaped, and v3 scopes it that way (resolves R2 must-fix #6 / critique Part 6).** The abstraction is a **log-structured transport with partition offsets, consumer groups, and partition-key ordering** — it does NOT transparently cover at-most-once or offset-less buses.

**In scope (drop-in, one file + 2 lines):** the **Kafka wire family** — Redpanda (default), Apache Kafka, Kinesis via its Kafka API, Azure Event Hubs via its Kafka API. Same adapter, different broker URLs.

**Out of scope unless an adapter MEETS the documented contract below:** NATS-core (at-most-once, no offsets — cannot back a durable buffer), SQS-standard / PubSub (no ordering, no committable position — the "process batch then commit one offset" model is unimplementable; per-message visibility-timeout ack redelivers already-processed messages → duplicate ship/incr with no committable resume point). NATS **JetStream** can be adapted (durable consumers + per-stream sequence as the offset, subjects as partition keys) but its redelivery/ordering semantics differ and it must be validated against the contract, not assumed. The design does NOT pretend one trait covers these transparently.

**The documented `StreamTransport` contract (what an adapter MUST provide):**

1. **Partition offsets** — a monotonic, committable position per partition, stored in the transport's OWN offset store (v5: this IS the authoritative consumer position; there is no PG `stream_offsets`). Committed after provider-ship (v7: the forwarder does nothing else — no Redis INCRBY).
2. **Consumer groups** — partitions assigned to exactly ONE consumer in a group, so >1 forwarder ships to the provider and increments the counter without cross-consumer duplication in steady state (§Pillar 7).
3. **Partition-key ordering** — events with the same key land in the same partition in publish order. **The partition key is the `subject`** (`{app_id, creator_id}`), so one subject's events are ordered → the §6.3 period reconcile and the on-demand stream `witness` see a consistent order for that subject.

<!-- Round 5: contract item #4 (the monotonic assignment `fence_epoch`) is REMOVED. It existed solely to fence the deleted §6.1 exactly-once fold against a zombie old owner. With no local exactly-once fold, a rebalance's only effect is an idempotent provider re-ingest (deduped) — nothing to fence (v7: and no Redis INCRBY either). Ordinary at-least-once redelivery is sufficient. -->
4. **(REMOVED in v5)** ~~A monotonic assignment `fence_epoch`~~ — deleted along with the exactly-once fold it fenced. A contract-meeting transport no longer needs to surface an assignment epoch; ordinary Kafka-family consumer-group semantics (offsets + one-consumer-per-partition + partition-key ordering) suffice.

```rust
// crates/zeroship-core/src/stream/mod.rs  (NEW — shared: worker produces, control consumes)

#[async_trait::async_trait(?Send)]
pub trait StreamTransport: Send + Sync {
    fn id(&self) -> &str;
    /// PRODUCER (worker): durably append a batch to `topic`, partition-keyed by
    /// `key` = the SUBJECT, so one subject's events stay ordered (contract #3).
    /// Returns once the transport ACCEPTED the batch for durable delivery
    /// (Redpanda: acks=all delivery report). librdkafka owns its own C threads.
    async fn publish(&self, topic: &str, key: &[u8], batch: &[UsageEvent]) -> Result<(), StreamError>;
    /// CONSUMER (forwarder): pull the next batch for a consumer group with its
    /// committable per-partition offset (contract #1/#2).
    async fn poll(&self, group: &str, max: usize) -> Result<StreamPoll, StreamError>;
    /// AUTHORITATIVE offset commit (v5) — the transport's own offset store IS the
    /// consumer position (there is no PG `stream_offsets`). Committed after
    /// provider-ship (v7: the forwarder does nothing else); normal at-least-once
    /// redelivery on the tail.
    async fn commit(&self, group: &str, offset: StreamOffset) -> Result<(), StreamError>;
}

// v5: no `fence_epoch` field — there is no exactly-once local fold to fence, so the
// monotonic assignment epoch (v4 contract #4) is dropped from the poll result.
pub struct StreamPoll { pub events: Vec<UsageEvent>, pub partition: i32, pub offset: StreamOffset }

#[derive(Default)]
pub struct StreamRegistry { factories: HashMap<&'static str, StreamFactory> }
// register/build/known: byte-for-byte the ProviderRegistry shape.
```

```rust
// crates/zeroship-core/src/stream/adapters/mod.rs  (NEW)
pub mod redpanda; pub mod jetstream; // a transport that MEETS the contract = one mod + one register line
pub fn register_builtin(r: &mut StreamRegistry) {
    r.register("redpanda",  redpanda::factory);   // default; rust-rdkafka / librdkafka (Kafka-family)
    r.register("jetstream", jetstream::factory);  // NATS JetStream — validated against the contract, not assumed
}
```

**Redpanda default via `rust-rdkafka`.** The producer uses `rdkafka` (bindings over **librdkafka**, a C library with its **own background threads**), so it adds **no async runtime** — the same category as `libpg_query` in `zeroship-migrate` (native C, no tokio). Kafka-wire compatible, so Redpanda ↔ Apache Kafka ↔ managed Kafka-API endpoints are all the same adapter with different broker URLs. In v5 the transport's own consumer-group offset store IS the authoritative consumer position (committed after provider-ship; v7: nothing else runs in the forwarder loop); there is no PG offset table and no exactly-once fold to fence. See §7 for the local-durability-floor evaluation.

### Pillar 3 — Capability sub-traits + portable, per-thread-safe `ProviderCtx`

Providers differ in what they do. OpenMeter is meter-only; Metronome/Orb/Lago/Stripe-Meters do meter+invoice; a "Stripe-invoice-from-aggregate" role does invoice-only. Model this as a thin **identity** trait composed of **optional capability sub-traits**, replacing the one-size six-verb `&AppState` trait.

```rust
// provider/mod.rs (REWRITTEN)

pub trait MeteringProvider: Send + Sync {
    fn id(&self) -> &str;
    fn capabilities(&self) -> Capabilities;
    fn as_meter(&self)    -> Option<&dyn Meter>       { None }
    fn as_invoicer(&self) -> Option<&dyn Invoicer>    { None }
    fn as_backfiller(&self) -> Option<&dyn Backfiller> { None } // Some ⇔ CorrectionCapability::Backfill
    /// Per-provider dedup contract (resolves R2 critique #2). Declares the identifier
    /// the provider dedups on AND how long its dedup window lasts. The forwarder
    /// uses this ONLY to decide the within-window fast path (§6.2); it does NOT
    /// drive correction (that is `correction()` below).
    fn dedup(&self) -> DedupContract;
    /// Per-provider REAL correction capability (resolves R2 must-fix #2 / critique
    /// #3,#6). There is NO universal `adjust` verb — meter events are immutable on
    /// every primary target. This declares what THIS adapter can actually do when the
    /// §6.3 safety-net pass finds a correction is owed. The comparison basis differs
    /// by ownership (§6.3): for an OWNED invoicer the correction fires on
    /// `witness ≠ invoiced` (a straggler/loss vs what we billed) via `InvoiceCredit`;
    /// for a SELF-invoicing provider the pass compares `witness − provider_meter` and
    /// dispatches `Backfill` (`witness` = the on-demand stream recompute, §6.3). Where
    /// it is `None` the drift is a surfaced operator finding, never an assumed API call.
    fn correction(&self) -> CorrectionCapability;
}

/// The REAL corrective surface each target exposes, researched per-provider
/// (§8 matrix `Correction` column). Replaces the mythical uniform `Meter::adjust`.
pub enum CorrectionCapability {
    /// The provider accepts a backfill/amendment of usage events and RE-RATES the
    /// affected period itself (Orb backfill+archive, Lago new-`transaction_id`,
    /// Metronome .jsonl amend/void+regenerate). `window` = how far back the provider
    /// accepts historical events (Metronome ~34d, Stripe-meter events ~35d); `closed`
    /// = whether it can re-open/regenerate an ALREADY-finalized invoice (Metronome
    /// yes; Orb/Lago open-period only, closed period needs a credit note). Dispatched
    /// via `Backfiller::backfill`.
    Backfill { window: Duration, closed: ClosedPeriodPolicy },
    /// Correction lives at the INVOICER WE OWN, not the meter: a credit-or-debit
    /// adjustment line on the NEXT invoice (`invoices`/`invoice_lines`). This is the
    /// PRIMARY stack's real path — `openmeter`(append-only meter)+`stripe_invoice`:
    /// we rate the provider's settled aggregate and own the invoice, so a late event
    /// after finalize is an `Invoicer::adjustment_note` on the following period. Also
    /// `stripe_meters` (meter events immutable — a `MeterEventAdjustment` can only
    /// CANCEL an event within Stripe's ~24h cancel window, distinct from the ~35d
    /// event-backdate ingest window; a closed-period under-bill needs a Stripe
    /// credit/debit note) and `lite` (local sink).
    InvoiceCredit,
    /// The provider's meter is append-only with NO correction API and WE do not own
    /// its invoicing. The reconcile pass emits a `provider_meter_drift` finding +
    /// hard alert — fail-toward-underbill, documented honestly, NOT a silent loss
    /// and NOT an assumed call. (No primary stack lands here; it is the honest floor
    /// for a hypothetical meter-only-self-invoicing provider with no amend API.)
    None,
}

pub enum ClosedPeriodPolicy { RegeneratesInvoice, OpenPeriodOnly }

bitflags::bitflags! {
    pub struct Capabilities: u8 { const METER=1; const INVOICE=2; }
}

/// Enforced at REGISTRY BUILD time (resolves critique #9): the bitflags and the
/// as_*() downcasts must agree, else boot fails — dispatch never meets a None.
pub fn assert_capability_consistency(p: &dyn MeteringProvider) -> Result<(), ProviderError> {
    let c = p.capabilities();
    let ok = c.contains(Capabilities::METER)   == p.as_meter().is_some()
          && c.contains(Capabilities::INVOICE) == p.as_invoicer().is_some()
          // correction() and the as_backfiller() downcast must agree: a Backfill
          // provider MUST expose Backfiller; a None/InvoiceCredit one MUST NOT.
          && matches!(p.correction(), CorrectionCapability::Backfill { .. }) == p.as_backfiller().is_some()
          // InvoiceCredit is an invoicer-owned correction and must have a backing Invoicer.
          && (!matches!(p.correction(), CorrectionCapability::InvoiceCredit) || p.as_invoicer().is_some());
    if ok { Ok(()) } else {
        Err(ProviderError::Config(format!("{}: capabilities() disagree with as_*() downcasts", p.id())))
    }
}
```

```rust
#[async_trait::async_trait(?Send)]
pub trait Meter {
    /// Push a batch of immutable, idempotency-keyed usage events. The provider
    /// dedups per its DedupContract; the forwarder only pushes events that are
    /// still WITHIN that window (§6.2). Meter events are IMMUTABLE on every target
    /// — there is deliberately no un-send / decrement here.
    async fn ingest(&self, batch: &[UsageEvent]) -> Result<IngestAck, ProviderError>;
    /// The provider's externally-aggregated total for a (subject, meter, period)
    /// window. This is the CANONICAL BILLING usage number (per-creator/subject): it
    /// is read ONLY after the provider's SETTLE WINDOW (§5.3) so it is complete at
    /// rating time. It is used for (a) an owned invoicer's close-period rating basis
    /// (cross-checked against the local per-app recompute — §5.3) and (b) the §6.3
    /// HEALTH cross-check. It is DELIBERATELY NOT the enforcement source (v6/v7):
    /// enforcement is per-app and provider-independent, so it is derived from the
    /// LOCAL stream recompute, never this call (§0 governing insight, §Pillar 4).
    async fn read_aggregate(&self, q: &AggregateQuery) -> Result<u64, ProviderError>;
    /// Ensure the provider knows this subject (Stripe cus_…; OpenMeter no-op).
    async fn ensure_subject(&self, subject: &Subject) -> Result<SubjectRef, ProviderError>;
}

/// ONLY implemented by adapters whose `correction()` is `Backfill` (Orb, Lago,
/// Metronome). Dispatched by the reconcile pass (§6.3) when `witness − provider ≠ 0`.
/// This is the provider's REAL amend API — not an assumed universal verb.
#[async_trait::async_trait(?Send)]
pub trait Backfiller {
    /// Replace/append events for `(subject, period)` so the provider re-rates the
    /// period from the true total (the §6.3 stream `witness`). Idempotent on
    /// `(subject, period, reconcile_watermark)`. Errors `ClosedPeriod` if `period`
    /// is finalized and the
    /// provider's `ClosedPeriodPolicy == OpenPeriodOnly` — the pass then falls back
    /// to a `provider_meter_drift` finding for operator credit-note handling.
    async fn backfill(&self, subject: &SubjectRef, period: BillingPeriod, correct_total: u64)
        -> Result<(), ProviderError>;
}

#[async_trait::async_trait(?Send)]
pub trait Invoicer {
    /// Close + bill a period. Lite / stripe_invoice: create+finalize invoice items.
    /// The close is triggered only AFTER the provider's SETTLE WINDOW (§5.3) has
    /// elapsed for the period, so the rating basis is complete. The basis is the
    /// LOCAL per-app recompute (§6.3 `witness`, complete after settle) CROSS-CHECKED
    /// against the provider's settled `read_aggregate` (a `provider_meter_drift`
    /// health finding on mismatch — reconciling the v4 MF#3 "rate from local" intent
    /// with the v5 provider-canonical decision; for `lite`, its own meter store).
    /// A straggler that arrives AFTER settle → the §6.3 correction path (a signed
    /// credit/debit `adjustment_note` on the next invoice, or a terminal true-up at
    /// account close — §5.3), NEVER a silent underbill. stripe_meters / metronome /
    /// orb / lago: no-op — the provider self-invoices from its own meter.
    async fn close_period(&self, subject: &SubjectRef, period: BillingPeriod)
        -> Result<InvoiceRef, ProviderError>;
    /// SIGNED post-finalize adjustment on the NEXT invoice — the `InvoiceCredit`
    /// correction path (§6.3). `AdjustmentNote.amount_cents` is signed: NEGATIVE for
    /// a provider-issued credit/refund (§12.2), POSITIVE (debit) for a late
    /// UNDER-bill that must add charge (resolves R2 critique #6 — a credit is the
    /// wrong direction for missed usage). Written as an `invoice_lines` entry
    /// (`credit_note` / `debit_note` kind) on a linked `invoices` row.
    async fn adjustment_note(&self, subject: &SubjectRef, note: &AdjustmentNote)
        -> Result<InvoiceRef, ProviderError>;
}

```

#### 5.3 The role stack — 3 roles, N ids, explicit fan-out (resolves critique #3)

The v1 "two ids for a three-role stack" was incoherent: it could not express meter=A / invoice=B where B needs its OWN usage feed, and its flagship `openmeter+stripe_meters` example billed $0 (OpenMeter got the events, Stripe's meter got nothing). v2 makes the stack explicit and distinguishes the **two different Stripe roles the v1 doc conflated**:

- **`stripe_meters`** — Stripe IS the self-metering meter **and** invoicer. Events are pushed into *Stripe's* meter; Stripe self-invoices from its metered Price+Subscription. You would **NOT** pair it with OpenMeter (that would double-meter). Single-id full-stack selection.
- **`stripe_invoice`** — invoice-only. It has NO meter of its own; its `Invoicer::close_period` fires **only after the provider's SETTLE WINDOW** (below) and **rates the LOCAL per-app recompute** (§6.3 `witness`, complete after settle), **cross-checked against** the provider's settled `read_aggregate` (a `provider_meter_drift` health finding on mismatch), then creates Stripe invoice items (same mechanism as Lite, pointed at a real Stripe account). v4 rated this from a LOCAL exactly-once *fold*; v5 swung to the provider's settled aggregate; **v6/v7 rates from the local per-app recompute (the same batch that drives enforcement) and keeps the provider aggregate as the canonical cross-check** — reconciling the v4 MF#3 "rate from local, it's complete" intent with the v5 "provider is canonical" decision. Any straggler after settle is corrected by §6.3 (`witness` vs `invoiced`), and the terminal period at account close is handled below. THIS is the correct partner for OpenMeter.

<!-- Added in round 6, CRITICAL #2: size the settle window + specify the "provider-settled" signal + terminal-period handling (the round-5 MF#3 spec gap). -->

**The settle window — sized, signalled, terminal-period-handled (v6, CRITICAL #2).** The entire "close after settle" non-regression argument rests on a settle window that v5 left as an unsized TODO. v6 pins it:

- **Sizing rule.** `settle_window ≥ max(forward_lag + provider_processing_lag)` observed over a trailing window, with a concrete conservative **default of a few minutes** (operator-tunable per provider; a self-invoicing provider that publishes a settled signal can use it directly instead of the fixed delay). `forward_lag` = stream→forwarder→provider ship latency (a first-class metric, §Pillar 7); `provider_processing_lag` = the provider's own ingest→queryable delay (measured against the conformance sandbox in S7, per-provider).
- **The "provider-settled" signal (not just a fixed delay).** A period `(subject, P)` is treated as SETTLED for close/reconcile when the **forwarder's committed-through position** (the Kafka consumer-group committed offset — the authoritative watermark, §6.2) for every partition carrying `P`'s subjects has advanced past the last event of `P` **AND** wall-clock ≥ `period_end(P) + settle_window`. The committed-offset condition proves zeroship has *shipped* everything for `P`; the wall-clock condition covers the provider's own processing lag. Where a provider exposes a real period-state / settled-total API, the platform uses it as a tighter signal; where it does not (the OQ-7 no-period-state case), the fixed `settle_window` delay is the fallback.
- **A straggler AFTER settle** (an event that lands past the settled point — rare, e.g. a forwarder recovering from a long backlog) → the §6.3 correction path: a signed **debit** `adjustment_note` on the NEXT invoice, keyed `(subject, period, correction_seq)`. Never a silent underbill.
- **Terminal period (account close / creator churn).** There is no "next invoice" to carry a straggler debit, so the terminal period is closed **specially**: the final invoice is withheld until `period_end + settle_window` (so it is complete by construction), then a **final §6.3 reconcile** runs the local recompute vs `invoiced`; any residual is a **terminal true-up settlement** (a final credit/debit line on the closing invoice, or a `terminal_period_trureup` finding + operator settlement if the account is already zeroed). This closes the round-5 gap where the terminal straggler was an uncorrectable underbill.

```rust
pub struct BillingStack {
    meter:    Arc<dyn MeteringProvider>,        // exposes Meter — the event sink
    invoicer: Arc<dyn MeteringProvider>,        // exposes Invoice
}
```

Selection is **role-addressed**, so any composition is expressible:

```
# canonical delegated stack — OpenMeter meters, Stripe invoices FROM the aggregate:
--meter-provider    openmeter
--invoicer-provider stripe_invoice

# full-stack single provider — one id fills both roles:
--provider metronome        # sugar: sets meter=invoicer=metronome

# evaluation, no external usage-billing account:
--provider lite
```

**Who feeds the invoicer's meter?** In the OpenMeter+`stripe_invoice` stack, `stripe_invoice` has NO meter — it does not need a usage feed. At period close (after the settle window) its `close_period` rates the **local per-app recompute** for `(subject, period)` (complete after settle), **cross-checks** it against OpenMeter's settled `read_aggregate` (health finding on mismatch), applies the CU model, and posts Stripe invoice items. Stripe is billed the real amount. The §6.3 pass catches any post-settle straggler (`witness` vs `invoiced`). The event-forwarder fans usage to **exactly the providers that expose `Meter`** — here, only OpenMeter. If a stack ever needs usage in *two* meters (e.g. a provider that both aggregates and self-invoices, paired with a separate analytics meter), the forwarder fans to every `Meter`-capable provider in the stack; the stack lists them, so the fan-out is explicit, not implicit.

**Boot-time composition validation (fail closed):**

- The `meter` role MUST expose `Meter`, else "no meter configured."
- The `invoicer` role MUST expose `Invoice`. An **owned** invoicer (`lite`/`stripe_invoice`) rates the local per-app recompute at close (cross-checked against the provider's settled aggregate; for `lite`, its own meter store), so it always has usage to rate. A **self-invoicing** invoicer (`stripe_meters`/`metronome`/`orb`/`lago`) rates its OWN meter, so it MUST also be the `meter` role (or share a stack whose meter IS it) — else "self-invoicing provider has no meter feed" (the v1 $0 bug, now a boot error).
- A meter-only provider (OpenMeter) with no separate invoicer → reject ("usage metered but never billed").
- `lite` (or any provider whose `production_ready()==false`) selected without `--allow-unsupported-billing` → reject (§Pillar 6).

#### 5.4 `ProviderCtx` — narrow, per-thread-safe, secret-aware

```rust
// provider/ctx.rs (NEW)

pub struct ProviderCtx {
    /// Per-thread cyper client FACTORY, not a shared client. `client()` returns the
    /// CALLING thread's client from a thread_local, so the Arc<dyn MeteringProvider>
    /// can be shared across cron threads while never carrying a SendWrapper across
    /// threads (resolves critique #11 — flush.rs:150-152/180-186 shows the client
    /// MUST be thread-local, not a struct field).
    pub http: HttpClientFactory,
    pub raw_config: serde_json::Value,          // adapter parses+validates (NON-secret fields)
    pub secrets: std::sync::Arc<dyn SecretResolver>, // secret handles → values (resolves #13)
    pub clock: Clock,
    pub store: Option<std::sync::Arc<dyn LiteStore>>, // ONLY lite / stripe_invoice use it
}

impl ProviderCtx {
    pub fn parse_config<T: serde::de::DeserializeOwned>(&self) -> Result<T, ProviderError> { … }
}

/// A cheaply-cloneable handle that hands out the calling thread's cyper client.
#[derive(Clone)]
pub struct HttpClientFactory(/* Arc<dyn Fn() -> cyper::Client> over a thread_local cache */);
impl HttpClientFactory { pub fn client(&self) -> cyper::Client { /* thread_local clone */ } }
```

Adapters call `self.http.client()` **inside each async method** (on whatever thread the cron runs), never storing a client on the `Arc`'d struct. The `SecretResolver` is the config-hardening secret-backend seam (env / Vault / AWS-SM); provider API keys flow through it, so a new secret-needing provider is still one adapter file + 2 index lines — no core edit to plumb secrets (resolves critique #13, which noted v1's plaintext `raw_config` forced a core edit for the exact L4 providers).

Only `lite` and `stripe_invoice` need DB access, through a narrow `LiteStore`, not `&AppState` (see §5.5).

#### 5.5 The Lite invoicer refactor — honest about what it is (resolves critique #1)

v1 claimed the Lite `Invoicer::close_period` reuses `bill_creator` **"verbatim"** *and* reaches deps through a 5-method `LiteStore`. That is a direct contradiction: `bill_creator` is `pub(crate) async fn bill_creator<S: StripeApi>(state: &AppState, …)` (`cron/billing_reconcile.rs:330`) and its body directly touches `state.registry.conn()` (`:347`), `state.stripe_store.get_customer()` (`:379`), `state.stripe_secret_key`/`state.stripe_base_url`, and `owned_app_ids(state, …)`. You cannot keep it byte-for-byte AND route its deps through a trait.

**v2 resolution — drop the "verbatim" claim.** This is a **refactor with preserved behaviour**, not a byte-for-byte reuse:

1. `bill_creator`'s signature changes from `&AppState` to `&dyn LiteStore` + `&dyn Invoicer`-neutral inputs. Its body's four `state.*` touchpoints are replaced by the corresponding `LiteStore` methods. The **pricing/segment-proration logic** (the C1/C2 snapshot, the plan-change segmentation) is moved **unchanged** — it operates on values (`weights`, `catalog`, totals), not on `AppState`. So the *money math* is identical; only the *dependency access* is rerouted.
2. `LiteStore` is implemented once, in control, over the real `Registry`/`StripeStore`/`PricingStore`/`PlanCatalog`. The relocation is a mechanical signature change with the same test vectors; the billing-ops regression suite (void/reissue, proration segments) is the fidelity gate.

```rust
#[async_trait::async_trait(?Send)]
pub trait LiteStore {
    async fn owned_app_ids(&self, creator: &Uuid) -> Result<Vec<Uuid>, ProviderError>;
    async fn period_totals(&self, app: &Uuid, period_start: i64) -> Result<HashMap<String,i64>, ProviderError>;
    async fn plan_changes(&self, app: &Uuid, period_start: i64) -> Result<Vec<PlanChange>, ProviderError>;
    async fn ensure_customer(&self, creator: &Uuid, email: &str) -> Result<Option<CustomerRef>, ProviderError>;
    async fn plan_and_weights(&self) -> Result<RatingInputs, ProviderError>;
    async fn record_invoice(&self, inv: &FinalizedInvoice) -> Result<InvoiceRef, ProviderError>;
    /// Local-only invoice sink for zero-external-account evaluation (§Pillar 6):
    /// write invoices/invoice_lines with NO Stripe call.
    async fn record_local_invoice(&self, inv: &FinalizedInvoice) -> Result<InvoiceRef, ProviderError>;
}
```

Commercial adapters never see `LiteStore` (`ctx.store == None`). The honest property is: **the reconciler's behaviour is preserved; its code is relocated behind `LiteStore`, not reused verbatim.**

### Pillar 4 — Stream-to-provider pipeline + periodic-cadence enforcement (resolves bill-01/03/06; A1/A2/A4)

<!-- Rewritten in round 7: enforcement is a periodic batch at a TUNABLE CADENCE (default hourly). The forwarder now ONLY ships to the provider + commits the Kafka offset — no Redis counter, no INCRBY. The sub-minute evaluator and the MAX re-base are DELETED. Detection = cadence; the gateway edge action stays instant. Overshoot is bounded by a coarse per-app throughput cap. -->

```
 PRODUCERS (worker)                DURABLE STREAM              CONTROL (forwarder — billing only)         PROVIDER
 ─────────────────                 ─────────────               ──────────────────────────────            ────────
 Meter.drain() → Vec<UsageEvent>   Redpanda topic              forwarder (one consumer group):            Meter.ingest(batch)
   each event_id = uuidv7          partitioned by subject   ┌─ 1. forward batch to every Meter-  ───────►  (CANONICAL usage
   (per drain WINDOW, not          (acks=all, replicated)   │     capable provider (§6.2, dedup)            + biller; dedup
    per request — OQ-1)                  │                   └─ 2. commit KAFKA offset after 1            per DedupContract)
   ↓ StreamTransport.publish  ─────────►│                     (transport's own offset store)             §6.3 correction spine
 (optional tiny redb floor for                               NO Redis, NO INCRBY                          (UNCHANGED)
  the pre-ack window — §7)

 ENFORCEMENT (periodic batch, per-app, TUNABLE CADENCE — default 1h):
   stream → recompute Σ per (subject=app, metric, period) → price (charge_cents) → app_spend_state (Postgres)
                        │  ONE recompute serves BOTH: (a) enforcement price→spend_state  (b) §6.3 billing witness
   app_spend_state ──5s registry pull──► gateway: INSTANT edge action (Warn/Degrade/Block, enforce::check_spend)
   overshoot ≤ R × cadence, bounded by a coarse per-app throughput cap at the gateway (RateLimit/Concurrency registry)
```

**Split the counter core from transport (bill-06).** `crates/metering` keeps the atomic-counter fast path (`meter.rs` `AppCounters`) but `drain()` now yields `Vec<UsageEvent>` (each with a freshly-minted stable `event_id`) instead of a `UsageReport`. The counter core has **no** knowledge of the stream or the wire type. `build_report` / `SequenceSource` / the `(worker_id, sequence)` machinery are **deleted**. A new `crates/zeroship-metering/src/outbox.rs` owns the `StreamTransport.publish` call (+ the optional local floor).

**No Postgres on the per-event path (A4, v5).** The stream IS the buffer; the provider IS the canonical event store AND aggregate. The v1 `usage_events` table and `usage_reports_seen` are deleted; **v5 also deletes the exactly-once `usage_aggregates` fold and the `stream_offsets` table.** The only per-event side-effect downstream of the stream is an HTTP ingest to the provider — it does not touch Postgres. PG is written only *periodically* (the cadence spend recompute → `app_spend_state`, invoices, findings). **v7 removes the Redis `INCRBY` that was the second per-event side-effect** — enforcement no longer runs per event. See §6, §9.

**Two DIFFERENT accuracy requirements — the load-bearing split (v5 reframe of R2 must-fix #1).** The pipeline gives two different guarantees to two different consumers of an event, and — crucially — **exactly-once lives in only ONE of them:**

- **Provider forward = AT-LEAST-ONCE + provider dedup = the EXACTLY-ONCE BILLING boundary.** Shipping events to the provider (§6.2) is at-least-once; the provider's `event_id` dedup (within its `DedupContract` window) makes the *provider's* stored total effectively-once. The provider is the canonical usage number and the biller. Meter events are immutable, so there is no compensating decrement to get wrong. **This is where money exactly-once lives — and the only place it needs to.**
- **Enforcement = a PERIODIC RECOMPUTE + a THROUGHPUT-CAPPED overshoot = a GUARDRAIL, deliberately NOT exactly-once and NOT sub-minute.** Enforcement reads the retained stream at a tunable cadence, not per event. It tolerates lateness up to one cadence period; a coarse per-app throughput cap bounds how far a runaway app can overshoot in that window. **That is acceptable** — enforcement never needs to be exact or continuous; it needs to be *bounded*.

v1–v4 tried to make a LOCAL fold exactly-once so it could serve BOTH billing (owned-invoicer rating input) and enforcement. That forced the §6.1 atomic fold and, at multi-forwarder scale, the `fence_epoch`/CAS zombie defence (the R3/R4 CRITICAL). **v5 removes the shared burden: billing reads the provider (canonical), enforcement reads a local per-app recompute.** Nothing needs a fenced transactional fold, so §6.1, `stream_offsets`, and `fence_epoch` are DELETED. **v7 removes the last continuous mechanism: enforcement no longer maintains a running Redis counter incremented per event — it recomputes at a cadence.**

**The periodic per-app recompute (the enforcement source, run at the cadence).** At each cadence tick (default 1 hour, operator-tunable, tightenable e.g. to 15-min) a batch computes `recompute(subject, period, metric)` = the SUM of the retained stream's events for each `(subject=app, period, metric)`. This is per-**app** by construction (the stream is partition-keyed by `subject`, which carries `app_id`), so it lands at exactly the per-app grain the spend limit is set at, with **no dependence on the provider's attribution grain** and **no Redis, no per-event increment, no re-base** — the recompute IS the count. It is the **SAME batch that §6.3 uses as its billing `witness`** (§6.3): ONE recompute, two consumers — (a) enforcement prices it into `app_spend_state`, (b) §6.3 uses it as the billing provider-loss witness. It is an **idempotent batch SUM, NOT a per-event exactly-once fold** — no PG transaction, no offset fence — so the v5 concurrency win (PG off the per-event path) fully survives.

**Pricing + writing `app_spend_state`.** The cadence batch rates each app's recomputed per-metric totals through the CU pricing model (`pricing::charge_cents`), resolves the effective limit, runs the existing hysteresis state machine (`spend.rs::derive_state`), and UPSERTs `app_spend_state`. This is essentially what `spend.rs::evaluate_all` already does today (it prices per-app usage and writes `app_spend_state`) — v7 just sources its per-app totals from the stream recompute and runs it at the cadence rather than reading `usage_aggregates` every tick. The gateway pulls `app_spend_state` via the existing ~5s registry pull of `RouteEntry.spend_state` (Decision D1, `spend.rs` header) and enforces instantly at the edge (`enforce::check_spend`, `crates/zeroship-gateway/src/enforce.rs`). **Detection latency = the cadence; the enforcement ACTION is instant.** The batch touches **NO provider and NO Redis**.

<!-- Added in round 7: the overshoot bound + the throughput backstop that makes it tunable-cadence-safe. -->

**Bounding overshoot — the throughput backstop (v7).** Because detection runs up to one cadence-period behind, a runaway app can overshoot its cap by at most `R × cadence`, where `R` = its maximum spend rate. This is bounded by a **coarse per-app hard rate/concurrency cap at the gateway** — which largely already exists (`enforce::RateLimitRegistry` / `ConcurrencyRegistry`, `crates/zeroship-gateway/src/enforce.rs`), the same edge that already throttles a Degraded app. Capping `R` makes **worst-case overshoot ≤ R × cadence bounded regardless of the cadence.** The **free tier** (uncapped-by-card, so it must not overshoot catastrophically) gets a specifically tighter cap. Both the **cadence** and the **per-app throughput cap** are tunable operator knobs; tightening the cadence and/or the cap shrinks the overshoot linearly.

**INVARIANT (v7): a runaway app is stopped within one cadence period, having over-spent at most its capped rate × cadence.** This is stated honestly: enforcement is not instant-detection; it is bounded-detection. The bound is a product of two knobs the operator controls.

**On a control/batch outage** the last-written `app_spend_state` is retained and the gateway keeps enforcing it; when the batch resumes it recomputes from the retained stream (bounded by stream retention, OQ-7) and catches up. There is no ephemeral counter to lose and no re-base to run — the recompute IS the count, rebuilt from the stream each cadence. Billing is unaffected (it never reads `app_spend_state`).

**Cross-worker forgery is closed at the source.** `event_id` is namespaced by an **authenticated `source`** (per-worker auth, bill-04, pulled early — §10 S2), and the provider's dedup key is `(source, event_id)`, so one worker cannot mint or suppress another's ids. There is no Postgres `PRIMARY KEY (event_id)` and no local ledger, so there is no global-PK cross-worker suppression vector (resolves critique #4).

**Event-time bucketing — scoped honestly (resolves critique #6).** The **enforcement recompute** buckets by `event_time` period (`period_start_unix(event.event_time)`), which is correct for enforcement; its exactness does not matter (it is re-derived from the stream each cadence). BILLING period semantics are owned entirely by the provider: the forwarder ships promptly so events land in the provider's OPEN period; a late event that misses a CLOSED period is corrected via the provider's REAL `CorrectionCapability` (§6.3 — Backfill re-rate for Orb/Lago/Metronome, or an owned-invoicer adjustment note), NOT a mythical universal `adjust`. The doc does not claim event-time bucketing fixes the provider-billed path.

### Pillar 5 — Enforcement is a periodic per-app batch at a tunable cadence, with instant edge action (bill-02, bill-05; resolves critique #10)

<!-- Rewritten in round 7: enforcement is a periodic batch at a TUNABLE CADENCE (default hourly). The Redis counter, the sub-minute evaluator, and the MAX re-base are DELETED. Detection = cadence; action = instant at the gateway. Overshoot is bounded by a coarse per-app throughput cap. -->

Spend Warn/Degrade/Block is derived by the **periodic per-app recompute** (rated via `spend.rs` + `pricing::charge_cents`), **never** a provider anywhere on the enforcement path and **no Redis counter**. **Invariant, v7:**

- **Detection = the cadence; the ACTION = instant.** The cadence batch (default 1h) recomputes each app's per-metric totals from the retained stream, prices them, runs `derive_state`, and writes `app_spend_state`. The gateway pulls `app_spend_state` (~5s registry pull) and enforces at the edge INSTANTLY (`enforce::check_spend`). An app trips its cap within one cadence period; the 402 itself is instant.
- **The Warn / Degrade / Block thresholds are cleanly separated** (`SpendThresholds`: `warn_pct=80`, `degrade_pct=95`, `block_pct=100`, `deadband_pct=5` - `spend.rs`). **Warn** (~80%, a dashboard/email signal) and **Degrade** (~95%, gateway throttle - tighter concurrency + rate limit, **the app stays up**) are the "app keeps running" states of the documented spend-control model. **Block** (~100%, a 402 before dispatch) is the terminal hard cap the creator opted into.
- **Overshoot is bounded by the throughput backstop, not by counter accuracy.** Because detection lags by up to one cadence, a runaway app can overshoot by at most `R × cadence`. The coarse per-app rate/concurrency cap at the gateway (`enforce::RateLimitRegistry`/`ConcurrencyRegistry`) caps `R`, so the overshoot is bounded regardless of cadence (§Pillar 4). The free tier gets a tighter cap. This is per-app and isolated: one app's overshoot cannot move any other app.
- **BILLING never reads `app_spend_state`.** The provider is the canonical billing number: **self-invoicing** providers rate their own meter; **owned invoicers** (`stripe_invoice`/`lite`) rate the **local per-app recompute at close** (after the settle window, §5.3), **cross-checked against** the provider's settled `read_aggregate` (health finding on mismatch). This reconciles v4 MF#3's "rate from local, it's complete" intent with the v5 "provider is canonical" decision: the local recompute is the *basis* (complete after settle), the provider aggregate is the *cross-check*. Any post-settle straggler is corrected by §6.3; the terminal period is handled by the §5.3 true-up. See §5.3.
- The §6.3 correction spine is **UNCHANGED** (`CorrectionCapability`, `Backfiller`, signed `adjustment_note`, `(subject, period, correction_seq)` idempotency keys). Its independent witness is the **local per-app recompute** — the SAME batch that drives enforcement (one recompute, two consumers).

- **bill-02 (O(apps) sweep → once per cadence).** The recompute is an O(apps) batch, but it runs **once per cadence (default hourly), not 60×/min** — a trivially tractable periodic job, shardable by app-id range across control instances. Only apps with stream activity this period have a non-zero recompute; apps flagged by a non-usage enforcement mutation (`spend_dirty`) are always re-derived. There is no per-tick full PG sweep and no continuous evaluator.
- **bill-05 → the enforcement SLO + overshoot bound (v7).** Two knobs — the **cadence** and the **per-app throughput cap** — with no cross-coupling:
  - **Detection latency** = the cadence (default 1h; the recompute + price + `app_spend_state` write + the ~5s route pull are small relative to the cadence). Tighten the cadence (e.g. to 15-min) to reduce it. Measured in S6, not estimated.
  - **Worst-case overshoot** = `R × cadence`, where `R` is bounded by the per-app throughput cap. Tighten either knob to shrink it. This is the honest bound — not "sub-minute," but "one cadence period × capped rate."
  - There is **no over-count / under-enforce drift term** any more: the recompute IS the count each cadence, so there is no ephemeral counter to drift, no re-processed-tail over-count, and no re-base window. The only latency is the cadence itself.

**Every enforcement-changing NON-USAGE mutation still flags the app (resolves critique #10).** A usage-driven change is picked up by the next cadence recompute (which reads that app's stream). But an *idle-but-limit-lowered* app has no stream activity, so a mutation that changes the enforcement outcome without usage must flag the app for an immediate re-derive. `spend_dirty` (a tiny `(app_id, period)` set) is written at every such point, and a lightweight flag-driven re-derive runs off-cadence for flagged apps so a lowered limit does not wait a full cadence:

| Mutation | Write point (file:line) | Flags |
| --- | --- | --- |
| Usage | picked up by the next cadence recompute (its stream is read) | `(app_id, event_time_period)` |
| Spend-limit change | `SpendEngine::set_limit` (`spend.rs:528…`), reached from `api.rs` | `(app_id, current_period)` |
| Plan change | `Registry::set_plan` (`registry.rs`), reached from `api.rs` | all `app_id` on the creator, current period |
| FX-rate change | operator FX mutation path | all apps priced in that currency |
| Suspend / unsuspend | creator-state mutation (`registry.rs`) | all apps on the creator |

`set_limit`/`set_plan`/suspend all already run through control handlers, so each gets a one-line `spend_dirty.mark(...)`. A flagged app re-derives its `app_spend_state` promptly (a targeted recompute + price for just that app, not the full-fleet cadence batch), so a lowered limit enforces without waiting a full cadence. The cadence batch remains the periodic backstop.

### Pillar 6 — Lite = evaluation-grade provider, production-readiness-gated (resolves critique #7/#8; A6)

v1 mis-framed Lite as "a dev-only test double." It is not. **Lite is a real, self-contained, evaluation-grade billing provider** — the relocated in-house engine — that lets someone stand up the whole platform in a local/self-hosted cluster and **trial zeroship end-to-end with no external billing account**. It is simply **not production-hardened**. The framing is **production-READINESS, not dev-vs-prod**.

- **`lite` adapter** implements `Meter` + `Invoicer` against local Postgres via `LiteStore`. As a self-contained provider, `lite` keeps its OWN meter store (a `lite_usage` aggregate — this is a PROVIDER's event store, dedup'd on `event_id`, NOT the deleted platform enforcement fold); `Meter::ingest` writes it, `Meter::read_aggregate` reads it (the canonical number in a `lite` deployment), and `Invoicer::close_period` rates that store via the relocated `bill_creator` body (§5.5). Enforcement in a `lite` deployment is the same periodic-cadence per-app recompute → `app_spend_state` → gateway edge — not from `lite`'s meter (enforcement is provider-independent by construction, v7).
- **Zero-external-account evaluation (resolves critique #7/#8a).** Lite's default invoicer uses the **local-only invoice sink** (`LiteStore::record_local_invoice`): it writes `invoices`/`invoice_lines` with **no Stripe call**, so an out-of-the-box trial needs zero external credentials. A Lite operator who *does* want to see real Stripe invoices can opt into the Stripe path by supplying a key — but the default trial is fully local.
- **Production-readiness guard.** `MeteringProvider::production_ready()` returns `false` for `lite`. Selecting a not-production-ready provider requires an explicit `--allow-unsupported-billing` boot flag; without it `build_stack()` refuses ("the Lite provider is evaluation-grade and not production-hardened; pass --allow-unsupported-billing to run it knowingly"). The rationale is **"not production-hardened,"** not "it's a fake." Because Lite *can* reach real money (if the Stripe path is enabled), this flag is load-bearing and gets a dedicated failure-mode row (§11).
- **Recording fakes are SEPARATE from Lite (resolves critique #8b).** The conformance suite (§Pillar 7) uses per-adapter **recording fakes** (localhost mock servers), a distinct concept from Lite. Lite is not "the test double"; Lite simply **passes conformance like any real provider**.

### Pillar 7 — The event-forwarder as a first-class component + conformance + DX

**The event-forwarder is a designed component, not a clause (resolves Missing-Concept #4).** It is a control-side consumer of the stream, spawned in `cron/mod.rs` as `cron/event_forwarder.rs`:

- **Consumer loop (v7 — billing only; no PG transaction, no fenced offset, no Redis):** `StreamTransport.poll(group, max)` → for each batch:
  1. **At-least-once:** forward the batch to every `Meter`-capable provider (§6.2). Provider `event_id` dedup absorbs a re-ship after a crash. This is the exactly-once BILLING boundary.
  2. **Commit the KAFKA consumer offset** (the transport's own offset store) after 1. On restart the forwarder resumes from the committed Kafka offset; the un-committed tail is re-forwarded (provider dedups → billing exact). **There is no `stream_offsets` PG table, no `fence_epoch`, no monotonic-CAS, and no Redis `INCRBY`** — the forwarder is now a pure ship-and-commit loop.

  Enforcement is entirely decoupled from this loop: it is a separate periodic-cadence batch that reads the retained stream directly (§Pillar 4). A provider outage cannot stall enforcement (enforcement never touches the provider) and never touched a shared counter that could blip a bill.
- **>1 forwarder via consumer-group partition assignment (v5 — rebalance is harmless).** At millions scale multiple forwarders run in ONE consumer group; Kafka/Redpanda assigns each partition to exactly ONE consumer. A **rebalance with a stalled old owner** — the R3/R4 CRITICAL that forced the `fence_epoch` monotonic-CAS in v4 — is now a **non-issue**: the only effect a zombie old owner can have is re-forwarding a batch to the provider (absorbed by `event_id` dedup). There is no local exactly-once fold to double-apply and (v7) no Redis counter to stray-increment, so there is nothing to fence. The whole epoch/CAS apparatus is DELETED. Kafka's own consumer-group offset commit (with normal at-least-once redelivery) is all that is needed.
- **Backpressure (provider slow-but-up):** forwarding retries with bounded in-flight; if the provider can't keep up, the consumer lags the stream (Redpanda retains it) — it does NOT drop and does NOT block enforcement (enforcement reads the stream independently). Lag is a first-class metric.
- **Dead-letter for permanent rejects (resolves critique #14):** a provider `4xx` (unmapped customer, unknown subject, malformed dims) is NOT retried forever. The batch's rejected events go to a `provider_dead_letter` finding (kind `provider_reject`) with the provider's error, an operator surface, and a metric; the offset still commits so the poison event never wedges the pipeline. Reconciliation flags the resulting drift.
- **Observability:** forwarder lag, ingest ack/dedup counts, dead-letter count, per-provider forward latency, drift findings.

**Provider conformance suite (`crates/zeroship-control/tests/provider_conformance.rs`).** Every adapter runs it against its per-adapter recording fake:

1. **Ingest idempotency under retry** — same `event_id`s twice → aggregate read-back unchanged.
2. **Watermark re-forward safety + dedup-window fidelity (resolves critique #2 / Missing-Concept #7)** — the mock advances its clock past the adapter's **declared `DedupContract.ttl`**, kills the forwarder mid-batch, and asserts (a) the un-acked tail (above the Kafka committed offset) is re-ingested and deduped, and (b) NO event at-or-below the committed offset is re-shipped. A `dedup_contract_matches_docs` assertion pins each adapter's `DedupTtl` variant + value to the provider's documented value (Stripe `Bounded(~24h)`, Orb/Lago `Unbounded`, etc.), so the one quirk that causes real double-bills is a checked contract, not an assumption.
3. **Aggregate read-back correctness** (the owned-invoicer rating input at close + the §6.3 witness cross-check).
4. **Invoice close** (if `INVOICE`) — stable `InvoiceRef`, idempotent re-close; owned invoicers rate the provider's SETTLED `read_aggregate` at close (assert the close happens after the settle window, and re-close is idempotent).
5. **Correction fidelity** (per `correction()`): `Backfill` adapters — `Backfiller::backfill` re-rates a drifted period idempotently; `InvoiceCredit` adapters — a signed `adjustment_note` (debit for under-bill, credit for over-bill) lands on the next invoice; `None` adapters — drift produces a `provider_meter_drift` finding, no phantom API call.
6. **Fail-closed config** — factory rejects empty/partial config and unresolved secret handles.
7. **Event-time bucketing** — a past-`event_time` event lands in its own period bucket (the enforcement recompute's period grouping + the provider's period).

**`docs/reference/adding-a-metering-provider.md`** and **`docs/reference/adding-a-stream-transport.md`** (NEW): copy an adapter, implement the sub-traits, declare + validate config, add 2 index lines, run conformance.

---

## 6. Idempotency, forwarding, and reconciliation (the load-bearing money-correctness section)

<!-- Rewritten in round 5: §6.1 (the exactly-once local fold + fenced offset CAS) is DELETED — with the provider as the single canonical billing SoT and enforcement demoted to an approximate guardrail, nothing needs a fenced transactional local fold. §6.2 (provider forward) and §6.3 (billing reconciliation/correction) are BILLING and stay, with their orphaned references to the deleted local fold re-pointed to the provider aggregate / an on-demand stream recompute. -->

Money-correctness in v5 lives in exactly TWO places, both about the PROVIDER (never a local exactly-once ledger): §6.2 forwards events to the provider at-least-once and the provider dedups them (the exactly-once billing boundary); §6.3 is the periodic billing reconciliation/correction spine that catches the residual (stragglers, provider-side drift). The v1–v4 §6.1 — a fenced, transactional, exactly-once LOCAL fold — is **DELETED** (see the note below); enforcement no longer needs it because it is now a periodic-cadence per-app recompute (§Pillar 4/5, v7 — no Redis counter).

### 6.1 (DELETED in v5) — there is no exactly-once local fold

<!-- Added in round 5: this subsection's entire mechanism (the fold+spend_dirty+stream_offsets PG transaction, the fence_epoch monotonic-CAS, the rebalance/zombie-double-fold defence — the R2 must-fix #1 and R3/R4 CRITICAL) is removed. -->
v1–v4 maintained a LOCAL exactly-once aggregate so a single fold could serve BOTH enforcement AND owned-invoicer billing. Making that fold exactly-once under >1 forwarder required a Postgres transaction binding `usage_aggregates += / spend_dirty / stream_offsets`, plus a `fence_epoch` monotonic-CAS to defeat a zombie old partition owner after a rebalance (the R3/R4 CRITICAL). **v5 deletes all of it.** Billing reads the provider (canonical); enforcement is a periodic local per-app recompute of the retained stream (v7 — no Redis counter). With no correctness-critical local fold, there is nothing to fence: consumer offsets live in **Kafka's own offset store** (committed after provider-ship), the `stream_offsets` table is dropped (§9), and a rebalance/zombie is harmless (the only effect is an idempotent provider re-ingest, deduped — §11). This is the single largest simplification in v5 — a whole subsection and its two rounds of hardening removed, not carried; v7 additionally removes the Redis counter and re-base that v5/v6 had added for sub-minute enforcement.

### 6.2 Per-batch forwarding = within-window `event_id` ingest ONLY (no per-batch delta)

Forwarding to a `Meter`-capable provider is JUST idempotent event ingest. It does **not** compute deltas, does **not** call `read_aggregate`, and does **not** do any correction — those move to the periodic pass (§6.3). This removes the R2 Part 2.1 per-batch over-push entirely: there is no `local − provider` arithmetic in the hot forward loop, so there is nothing to double-apply.

Each adapter declares its dedup contract with an explicit tri-state TTL (resolves R2 critique #2 point 5 — `None` was overloaded as both "unbounded" and "unknown"):

```rust
pub struct DedupContract {
    pub key: DedupKey,   // (source,id) | idempotency_key | transaction_id | …
    pub ttl: DedupTtl,
}
pub enum DedupTtl {
    Bounded(Duration), // Stripe meter events ~24h; the risky case
    Unbounded,         // provider dedups forever on the key (Orb/Lago transaction_id) → always safe to re-ingest
    Unknown,           // undocumented → treat conservatively as Bounded(0): never re-ingest a re-shipped event; rely on §6.3
}
```

The forward rule uses a **forward-watermark** — the highest stream offset successfully `ingest`-acked to the provider(s). In v5 this is simply the **Kafka consumer-group committed offset** (the transport's own offset store), committed after the batch is shipped to every `Meter`-capable provider (v7: that is the forwarder's only side-effect — no Redis `INCRBY`). There is **no PG `provider_forward_watermark` table** (deleted, v5 — keeping PG off the per-event path). This avoids per-event first-ingest bookkeeping at millions scale AND fixes the clock-skew reference (resolves R2 Part 2.2): "age" is never computed from the worker `event_time` at all. (With a single forwarder consumer group shipping to all `Meter` providers before committing, the committed offset covers every provider; a stack that needs independent per-provider progress runs a consumer group per provider — still Kafka offsets, still no PG table.)

- **A first forward always ingests** (offset > watermark): the event has never been sent, so the provider's dedup window is irrelevant. Ship it, advance the watermark.
- **A re-forward only happens for the un-acked tail** (offsets between the watermark and a crash point) — by construction that tail is recent (the seconds since the last watermark advance), so it is always inside any provider dedup window. `event_id` dedup absorbs it. There is NO path that re-ships an event whose provider-window may have already expired, because anything at-or-below the watermark is never re-sent.
- For `DedupTtl::Bounded`, if the provider is DOWN long enough that the un-acked tail ages past the TTL before it can be forwarded, the forwarder does NOT blind-ingest it (that would risk a stale double-send after the provider recovers and its old dedup entries expired); instead it hands `(subject, period)` to the periodic pass (§6.3), which reconciles at period grain. `DedupTtl::Unbounded` (Orb/Lago `transaction_id`) is always safe to (re-)ingest regardless of age. `DedupTtl::Unknown` is treated as `Bounded(0)` — never blind re-ingest, always let §6.3 own drift.
- **Batch straddle (resolves R2 Part 2.3)** is a non-issue under the watermark model: the split is by offset-vs-watermark, which is monotonic within a partition, so a polled batch is cleanly `[≤watermark: skip][>watermark: ingest]` with no per-event age decision that could mis-classify a straddling batch.

**Closed-period-within-TTL (resolves R2 Part 2.4).** The fresh/ingest decision is gated not only on TTL but on the provider's period state. The forwarder uses the **provider-acknowledged open period** (from the provider's period metadata / the last `read_aggregate` period marker), not local wall-clock, to decide whether an event's target period is still open. An event younger than the TTL whose provider period has already CLOSED is NOT ingested (the provider would drop/reject it) — it is routed to §6.3 for correction. This closes the crack where a "fresh" event fell into a closed period and was silently lost. (The per-subject period-state lookup this implies is a scaling cost — coarse-grained cached period markers, and an explicit "unknown ⇒ route to §6.3" rule for providers with no period-state API — tracked in OQ-7 alongside the §6.3 `read_aggregate` fan-out.)

### 6.3 Periodic reconciliation pass = a bounded SAFETY NET, once per `(subject, period)` (resolves must-fix #4 / R2 Part 2.1 + the R4 owned-invoicer comparison-basis over-bill)

<!-- Round 5 note: the spine (owned/self split, correction verbs, idempotency key, once-per-period cadence) is UNCHANGED. The only substitution: the independent number formerly called `local`/`local_final` was the deleted §6.1 exactly-once PG fold; in v5 it is an ON-DEMAND STREAM RECOMPUTE — `witness(subject, period)` = a batch re-aggregation of the retained stream for that period, computed ONLY by this periodic pass (not a persisted fold, not exactly-once, not on the hot path). This keeps the independent-witness / provider-loss-detection property (critique #5) without reintroducing a local ledger. Owned invoicers now rate the provider's SETTLED aggregate at close (§5.3/Pillar 5), so `invoiced ≈ provider_settled`. -->
**Framing, honestly.** §6.3 is a **bounded safety net, NOT the primary money path.** The invoice is correct *by construction* on every primary stack independent of whether this pass keeps up: self-invoicers bill from their own meter fed by the §6.2 at-least-once ship; owned invoicers rate the provider's SETTLED aggregate at close (after the provider's settle window). §6.3 exists to catch the *residual* — a provider-side loss, or a post-finalize straggler — and its worst-case miss is fail-toward-underbill (an accepted class). It is a once-per-`(subject, period)` sweep (self-correcting and idempotent BECAUSE it runs once per period, not per batch), a separate cron pass, NOT the forward loop. This bounded framing is what justifies deferring OQ-7's scaling problem.

**The independent witness is a LOCAL per-app recompute of the retained STREAM — the SAME batch that drives enforcement (v6/v7).** Where the pass needs a number to compare against the provider (or to rate an owned invoice), it re-aggregates the retained stream on demand: `witness(subject, period, metric) = Σ value over the stream's events for (subject=app, period, metric)`. This is **exactly the `recompute` that §Pillar 4 runs at the enforcement cadence** — ONE mechanism, two consumers: (a) enforcement prices it into `app_spend_state` (v7 — no Redis counter, no re-base), and (b) this §6.3 pass uses it as the billing provider-loss witness and (for owned invoicers) the close-period rating basis. It is a periodic **BATCH** (not per-event), NOT persisted, and NOT exactly-once, but it is a sufficient audit witness to detect a provider-side loss and complete-after-settle enough to rate an owned invoice. Keeping it a single shared batch is what keeps PG off the per-event path (the v5 win) while giving enforcement a local source (the v6/v7 fix). **Cost + retention are a real OQ (round-5 MAJOR):** the recompute reads the stream, so it requires **Kafka retention ≥ settle_window + reconcile_cadence + reconcile_duration**, and at millions of subjects its fan-out is a genuine cost — mitigations (rolling per-partition slices, incremental recompute from a checkpoint, cadence tuning) are folded into **OQ-7**. The pass is split by `correction()`/ownership:

**(A) `InvoiceCredit` stacks — WE own/can issue the invoice** (`openmeter`+`stripe_invoice`, `lite`, and `stripe_meters` where we hold the Stripe account). The correctness basis is **what we ACTUALLY invoiced vs the true count** — and because owned invoicers rate the provider's settled aggregate, the invoice already tracks the provider; the stream `witness` catches what the provider (hence the invoice) may have missed. Per `(subject, period)`:
1. `witness = stream_recompute(subject, period)` — the on-demand stream re-aggregation (the true count; independent of the provider).
2. `invoiced = Σ quantity of the finalized `invoice_lines` for `(subject, period)`` (what we ACTUALLY billed — rated from the provider's settled aggregate).
3. `billing_drift = witness − invoiced`. A correction is issued **only when THESE differ** — a straggler that reached the provider after `close_period` finalized, or a provider-meter loss that made the settled aggregate (hence the invoice) short. Emit a signed `Invoicer::adjustment_note` — DEBIT for an under-bill, CREDIT for an over-bill — on the NEXT invoice.
4. `provider_meter = meter.read_aggregate(subject, meter, period)` is a **HEALTH cross-check**: if `|witness − provider_meter| > tolerance` it writes a `provider_meter_drift` FINDING (a data-integrity signal about the provider meter's lag/loss). The bill is corrected off `witness − invoiced` (step 3); the finding surfaces provider health.

**(B) Self-invoicing stacks** (`metronome` / `orb` / `lago`) — the provider's own meter IS the billing SoT, fed at-least-once by §6.2. Per `(subject, period)` the provider acknowledges (its period clock, not local wall-clock — R2 Part 2.2/2.4):
1. `witness = stream_recompute(subject, period)` (the true count, from the retained stream).
2. `provider = meter.read_aggregate(subject, meter, period)` (here the billing basis).
3. `drift = witness − provider`. If `|drift| ≤ tolerance`, done.
4. Otherwise dispatch the provider's REAL `CorrectionCapability` (must-fix #2):
   - **`Backfill`** (Orb / Lago / Metronome): `Backfiller::backfill(subject, period, correct_total = witness)` — the provider re-rates from the true count. If the period is finalized and `ClosedPeriodPolicy == OpenPeriodOnly`, fall through to the `None` handling.
   - **`None`**: no provider correction API and we do not own the invoice → `provider_meter_drift` finding + hard alert. Fail-toward-underbill, surfaced, never silent.

**Every correction carries a stable idempotency key so it is applied at most once and never re-fires.** The key is `(subject, period, correction_seq)`, where `correction_seq` increments only when the *corrected quantity actually changes* (a fresh straggler moves `witness`), so a re-run of the pass with the same numbers is a no-op. It is persisted the same way the existing reconciler already dedups (`cron/stripe_reconcile.rs:617-664`): a UNIQUE `dedup_key` on `billing_reconciliation_findings` / the adjustment record with `ON CONFLICT (dedup_key) DO NOTHING`. So:
- an owned-invoicer `adjustment_note` is keyed `(subject, period, correction_seq)` and written once even if the pass runs every cycle;
- a `Backfiller::backfill` is idempotent on the same key (re-running with the same `local` is a no-op re-rate);
- a `provider_meter_drift` finding dedups on the drift identity (unchanged existing behaviour) so a persistent provider-lag drift is recorded once, not re-alarmed each cycle.

**Period straddling the TTL / closing within the TTL** is handled here, not in the forward loop: the pass reconciles the whole period once the provider marks it settled, so an event that missed the within-window ingest path (§6.2) is caught. This is why §6.2 can be a pure ingest with no correction logic.

The conformance suite's clock-advancing test (Pillar 7 #2) verifies the §6.2 within-window boundary AND the §6.3 fallthrough per adapter: (a) an owned-invoicer stack where the provider merely LAGS but settles before close produces `witness ≈ invoiced` → **no** `adjustment_note` (waiting for the settle window absorbs lag); a provider that LOSES an event produces `witness > invoiced` → exactly one keyed DEBIT `adjustment_note` plus a `provider_meter_drift` finding; (b) a straggler after finalize produces exactly one keyed `adjustment_note` and re-running the pass does not duplicate it. The scaling cost of `read_aggregate` (and the on-demand stream `witness` recompute) at millions of subjects is a real open item — see OQ-7 (scoped as safety-net coverage, not a correctness dependency).

---

## 7. Durable-buffer choice — the stream, and the local-durability-floor evaluation

**Requirement (A1/A3):** millions of subjects × frequent events, a worker crash must not silently drop the unflushed window, and it must be a pluggable seam.

**Chosen: a durable STREAM (Redpanda default) behind the `StreamTransport` seam (§Pillar 2).** Redpanda is Kafka-wire-compatible, replicates with `acks=all`, retains events across a control-side outage (bounded by retention), and scales horizontally by partition — the right tool for millions of events, which a partitioned Postgres table is not. The `rust-rdkafka`/librdkafka producer uses **C background threads**, adding **no async runtime** (A5, §13).

**The local-durability-floor question (owner asked to evaluate).** librdkafka's producer queue is **in-memory** — it is not crash-durable for events queued but not yet `acks=all`-confirmed by the broker. Two options for that pre-ack window:

- **(a) Thin redb WAL floor (RECOMMENDED, small).** The worker appends each drained event to a bounded local **redb** segment before `publish`, and trims it on the librdkafka **delivery-report** callback (broker-confirmed). Crash → restart → re-publish only the un-confirmed tail (same `event_id`s → idempotent). This preserves the "worker crash replays" property with a tiny, bounded footprint (seconds of events, trimmed continuously). **Honest dependency note (resolves critique #7):** `redb` is currently ONLY in `crates/zeroship-kv/Cargo.toml` behind a feature — it is **not** a `[workspace.dependencies]` entry — so `crates/metering` would gain a **new** dependency and per-drain fsync cost. That cost is bounded (batched per drain window, not per request) and must be measured in S5, not estimated.
- **(b) No floor — accept the pre-ack window as fail-toward-underbill.** Rely solely on librdkafka's in-memory queue + `acks=all`. Simpler (no redb in `metering`), but a worker crash loses events queued-but-not-confirmed at crash time. This is the same fail-toward-underbill class the platform already tolerates, and the window is only the un-confirmed tail (typically sub-second under a healthy broker).

**Default: TBD pending the S5 measurement (resolves R2 Part 7.7 — do not recommend the costly option before measuring).** Option (a) upgrades "worker crash = lose the tail" to "worker crash = replay the tail," but its per-drain fsync cost on the 200K-req/s producer is UNMEASURED, and the project rule is measure-don't-estimate. So S5 measures (a)'s fsync cost first; (a) ships only if the cost is immaterial, else (b) is the default and (a) is an opt-in for operators who value the tail over throughput. Either way the choice is **local to `outbox.rs`** behind the `StreamTransport.publish` seam — it does not touch the provider or forwarder contracts.

**Why not "Postgres as the event store" (v1 Option A landing table).** It does not meet A1 (millions of events), it duplicates the provider's authoritative store (A2/A4), and it re-introduced the global-PK suppression vector (critique #4). Removed.

---

## 8. Capability × provider matrix

| Provider (`id`) | Meter | Invoicer | `production_ready` | DedupContract | **Correction** (real API) | Config (self-validated; secrets via `SecretResolver`) | Notes |
| --- | :---: | :---: | :---: | --- | --- | --- | --- |
| `openmeter` | ✅ | — | ✅ | `(source,id)`, **Unbounded** | **None** (append-only meter; NO adjust verb) — but the STACK corrects at the owned invoicer | `base_url, token*, event_type, meter_slug` | CloudEvents-native. Meter-only; MUST pair with an invoicer. Correction owned by `stripe_invoice` (InvoiceCredit). |
| `stripe_meters` | ✅ | ✅ | ✅ | `identifier`, **Bounded ~24h cancel** | **InvoiceCredit** (meter events immutable; `MeterEventAdjustment` cancels an event only within the ~24h cancel window — distinct from the ~35d event-backdate ingest window; a closed-period under-bill → platform debit/credit note) | `secret_key*, event_name, meter_id` (+ `LiteStore`) | Stripe self-invoices from its metered Price+Subscription. `close_period` no-op; correction notes are platform invoice adjustments. Do NOT pair with OpenMeter. |
| `stripe_invoice` | — | ✅ | ✅ | n/a (no meter) | **InvoiceCredit** (we own the invoice → the §6.3 owned-invoicer correction compares the stream `witness` vs the finalized `invoice_lines`; a straggler/loss → signed `adjustment_note` on next invoice, keyed `(subject,period,correction_seq)`) | `secret_key*` (+ `LiteStore`) | **Invoice-only.** Rates the meter provider's **SETTLED `read_aggregate`** at close (the single canonical usage number, v5) into Stripe invoice items. THE correct partner for `openmeter`. Bills Stripe the real amount (resolves critique #3). |
| `metronome` | ✅ | ✅ | ✅ | `transaction_id`, **Unbounded** (dedup ≤34d ingest) | **Backfill{window ~34d, closed: RegeneratesInvoice}** (.jsonl amend/void + regenerate finalized invoice) | `api_token*, contract_id` | Full-stack single-id. |
| `orb` | ✅ | ✅ | ✅ | `idempotency_key` = our `event_id`, **Unbounded** | **Backfill{window: current period, closed: OpenPeriodOnly}** (backfill archives+replaces events, re-rates; issued invoices → credit note) | `api_key*, external_customer_id_map?` | Full-stack; real Orb backfill API. |
| `lago` | ✅ | ✅ | ✅ | `transaction_id` = our `event_id`, **Unbounded** | **Backfill{window: open period, closed: OpenPeriodOnly}** (new `transaction_id` on open period; closed period immutable → Lago credit note) | `api_url, api_key*, billable_metric_code` | OSS, self-hostable; full-stack. |
| `lite` | ✅ | ✅ | **❌** | `event_id`, **Unbounded** (local) | **InvoiceCredit** (local sink → negative/positive `invoice_lines`) | *(none required; local invoice sink)* (+ `LiteStore`) | Evaluation-grade relocated in-house engine. Default invoicer = **local-only** (no Stripe). Refused without `--allow-unsupported-billing`. |

`*` = secret handle resolved via the secret backend, never plaintext in `provider_config`.

**Correction column source of truth (researched, not assumed):** Stripe meter events are immutable — the only meter-level correction is a `MeterEventAdjustment` that *cancels* an event, and that cancel is allowed only within the **~24h cancel window** (after 24h you instead send a negative-value event); this is a DIFFERENT window from the **~35d event-backdate ingest window** (how far back a new meter event may be timestamped). A closed-period correction is a Stripe credit/debit note. OpenMeter is append-only with no adjust verb. Orb's backfill API archives+replaces events and re-rates, but does not reflect into already-issued invoices. Lago corrects an open period via a new/replacing `transaction_id` (Postgres backend rejects same-id duplicates; ClickHouse replaces on same id+timestamp); a closed period is immutable. Metronome accepts .jsonl backfill/amend/void and can regenerate even a finalized invoice (≤34d). These map to `CorrectionCapability::{None | InvoiceCredit | Backfill{…}}` above; where it is `None`, the corrective path is a surfaced operator finding, documented honestly.

**Canonical prod stack:** `--meter-provider openmeter --invoicer-provider stripe_invoice` (OpenMeter meters; Stripe is billed from the aggregate). **Full-stack:** `--provider metronome|orb|lago`. **Evaluation:** `--provider lite` (fully local, no external account).

---

## 9. Data-model delta (Postgres holds NO raw events AND no per-event ledger — A4, v5)

<!-- Rewritten in round 5: usage_aggregates is no longer a per-event exactly-once fold; stream_offsets and provider_forward_watermark are DELETED (Kafka holds offsets). Round 7: the Redis enforcement counter is DELETED — enforcement is the periodic per-app recompute batch writing app_spend_state; there is no new per-event table. -->

All in the `zeroship` schema; existing billing tables in `db/migrations-ts/20260702000400_billing_metering_invoice_tables.ts`.

| Table | Change | Why |
| --- | --- | --- |
| **`usage_events`** | **NOT CREATED (removed from v1).** | A4: the stream is the buffer, the provider is the event SoT. No Postgres raw-event store at millions scale. |
| **`usage_reports_seen`** | **DELETED.** | The `(worker_id, sequence)` model is gone; dedup is per-event at the provider; the consumer offset lives in Kafka. |
| `usage_aggregates` | **NO LONGER a per-event exactly-once counter (v5).** Not on the per-event path. May serve as `lite`'s own provider-side meter store (`lite_usage`). Enforcement no longer reads it per tick — the cadence batch sources its per-app totals from the stream recompute and writes `app_spend_state`. | Enforcement is a periodic recompute (A2, Pillar 4/5); PG stays off the per-event path. |
| ~~**Redis enforcement counter**~~ | **DELETED (v7).** There is NO Redis counter. Enforcement freshness relaxed from sub-minute to a tunable cadence, so the best-effort `INCRBY`, the sub-minute evaluator, and the `MAX` re-base are all removed; `libs/compio-redis` leaves the enforcement path. | Governing v7 decision: tunable-cadence detection makes the continuous counter unnecessary. |
| **Local per-app recompute** | **NEW — a periodic BATCH JOB, not a table.** Reads the retained stream at a **tunable cadence (default 1h)**, computes `Σ value` per `(subject=app, period, metric)`. ONE job serves BOTH enforcement (price → `app_spend_state`, §Pillar 4/5) and the §6.3 billing `witness`. Requires **retention ≥ settle_window + reconcile_cadence + reconcile_duration** (OQ-7). No per-event fold, no transaction, no Redis. | Gives enforcement a LOCAL per-app source (v6/v7) without a per-event ledger OR a continuous counter; shares cost with §6.3. |
| ~~**`stream_offsets`**~~ | **DELETED (v5).** The consumer position lives in the transport's own offset store (Kafka consumer-group offset), committed after provider-ship (v7: the forwarder's only side-effect). There is no exactly-once local fold to gate, so no PG offset row and no `fence_epoch`. | The whole §6.1 fold+fence machinery is removed (governing decision #2). |
| ~~**`provider_forward_watermark`**~~ | **DELETED (v5).** The forward-watermark is the Kafka consumer-group committed offset (committed after provider-ship). | Keeps PG off the per-event path; Kafka already tracks it. |
| **`spend_dirty`** | **KEPT (v5), tiny.** `(app_id, period)` PK. Written on every enforcement-changing NON-USAGE mutation (limit/plan/FX/suspend — Pillar 5 table); usage-driven dirtiness is picked up by the next cadence recompute. A flagged app gets a prompt off-cadence re-derive. | bill-02 without the critique #10 regression; no longer coupled to any fold or counter. |
| **`provider_dead_letter`** | **NEW.** `(id, provider_id, subject, meter, event_time, value, reason, created_at)`. | Permanent-reject surface (resolves critique #14). |
| `metering_exports` | **DELETED.** | The per-`(creator,period)` high-water/delta is subsumed by the §6.3 periodic per-provider reconciliation pass. |
| **`provider_config`** | **NEW (optional).** `(role, provider_id, config jsonb, updated_at)` — NON-secret blob only; secrets are handles resolved via the secret backend. | Pillar 1 provider-agnostic config. |
| `app_spend_state` | **KEPT.** UPSERTed by the periodic-cadence recompute batch (priced via `charge_cents`/`derive_state`); pulled by the gateway via `RouteEntry.spend_state` (Decision D1) and enforced instantly at the edge. | The enforcement decision surface (unchanged shape); v7 sources it from the cadence recompute, not a Redis counter. |
| `billing_reconciliation_findings` | **KEPT + extended.** New kinds: `provider_meter_drift`, `provider_reject`, `late_period_adjustment`, `forwarder_down_exceeds_retention`, `subject_attribution_mismatch`, `terminal_period_trureup` (v6, terminal-period settlement — §5.3/§11). | Pillar 5/7 reconciliation + dead-letter + §6.3 corrective path + honest local-gap alert (§11) + attribution cross-check (OQ-10). |
| `billing_metrics`, `metric_weights`, `plans`, `pricing_config` | **KEPT unchanged.** | CU pricing + plan catalog out of scope; the Lite/`stripe_invoice` invoicer path + enforcement read them. |
| `invoices`, `invoice_lines`, `billing_provider_refs`, `billing_customer_refs` | **KEPT.** Written by the Lite/`stripe_invoice` invoicer (incl. the local-only sink) and `SubjectRef` mapping. | Invoice bookkeeping provider-neutral. |
| `creator_fee_policy`, `invoice_payments`, disputes/refunds/payout tables | **UNTOUCHED** by the pipeline, but see section 12.2 for the credit-note flow into `invoices`/`invoice_lines`. | Connect payment lifecycle. |

Worker-side (not a DB table): the optional bounded **redb WAL floor** (`$DATA/usage-outbox.redb`) holding the pre-ack window for the SHIP path (§7, option a).

---

## 10. Migration / rollout plan (pre-launch, no back-compat — break freely, in landable slices)

No shims, no dual-write, no `@deprecated`; each slice deletes what it replaces and updates all callers in the same change. Ordered so the tree stays green.

1. **S1 — Provider registry + capability traits + `ProviderCtx` + `SecretResolver`.** Add `ProviderRegistry`, identity + sub-traits, `assert_capability_consistency`, `ProviderCtx` (per-thread `HttpClientFactory`, secret backend), `adapters/mod.rs`. Reshape the three existing adapters (native→`lite`, `openmeter`, `stripe_meters`) + add `stripe_invoice`. Delete the enum, `MeteringProviderConfig`, `build_provider`, per-provider CLI flags, duplicated guards. Wire role-addressed selection (§5.3). Relocate `bill_creator` behind `LiteStore` (§5.5). *Control-side; worker path unchanged.*
2. **S2 — Per-worker internal auth (bill-04), pulled EARLY (resolves critique #4).** Replace the flat `control_key` on `/internal/*` (`internal.rs:27-40`) with per-worker credentials so `source` on an event is authenticated BEFORE the event-first pipeline lands. Landing this before S3/S4 means the streaming path never ships with forgeable `source`.
3. **S3 — `StreamTransport` registry + Redpanda adapter.** Add `StreamRegistry`, the trait (v5 `StreamPoll` has NO `fence_epoch`), `stream/adapters/mod.rs`, the `redpanda` (rdkafka) adapter using the transport's own consumer-group offset store. Stand up Redpanda in compose. **No `stream_offsets` PG table.**
4. **S4 — Event-forwarder + reconcile pass (control consumer).** `cron/event_forwarder.rs`: poll → **forward to `Meter`-capable providers (§6.2) → commit the Kafka consumer offset.** NO PG transaction, no fold, no fence, no Redis. Add `cron/billing_reconcile_pass.rs` (or extend the existing reconciler) for the period-level §6.3 pass (owned/self split; on-demand stream `witness`) that dispatches `CorrectionCapability`. Dead-letter path + observability. Consumer-group config for >1 forwarder (rebalance is harmless — no fencing needed).
5. **S5 — Event-first producer + durable buffer (worker).** `crates/metering`: `drain()`→`Vec<UsageEvent>`; `outbox.rs` with `StreamTransport.publish` + the optional redb floor for the SHIP path (measure the fsync cost — §7). Delete `build_report`/`SequenceSource`/`flush.rs`'s report path (bill-01/06 structural fix).
6. **S6 — Periodic-cadence enforcement recompute + throughput backstop + SLO (a SMALL slice — no Redis-counter/re-base/sharded-evaluator work).** Wire the **LOCAL per-app recompute** batch (`Σ` over the retained stream per `(subject, metric, period)`) at a **tunable cadence** (default 1h) → price (`charge_cents`) → `derive_state` → `app_spend_state`; the gateway pull + `enforce::check_spend` already exist. Reuse the SAME recompute as the §6.3 witness (one job). Add the **coarse per-app throughput cap** (reuse `enforce::RateLimitRegistry`/`ConcurrencyRegistry`) with a tighter free-tier default, so overshoot ≤ `R × cadence` is bounded. Wire the `spend_dirty` write points + the flag-driven off-cadence re-derive for non-usage mutations (Pillar 5 table). **Measure** the detection latency (= cadence) and the worst-case overshoot (`R × cadence`) under load; document both + the two knobs as the enforcement SLO (bill-02/05). **No `compio-redis` on the enforcement path; no `INCRBY`, no evaluator, no re-base.**
7. **S7 — Conformance suite** (incl. the clock-advancing dedup-TTL test) + make `lite`/`openmeter`/`stripe_meters`/`stripe_invoice` pass. Land the `--allow-unsupported-billing` prod guard.
8. **S8 — First commercial adapter (`lago` or `metronome`) + a second `StreamTransport` (`nats`).** Prove BOTH L1 claims end-to-end: a new provider AND a new transport, each one file + 2 lines, green on conformance.
9. **S9 — Docs.** `adding-a-metering-provider.md`, `adding-a-stream-transport.md`; rewrite `billing-metering.md` to the stream-to-provider model.

Hard orderings: S1→S5 (traits before producer), S3→S4→S5 (stream + consumer before producer emits), S2 before S4 (authenticated `source` before forwarding).

---

## 11. Failure modes & fail-closed guarantees

| Failure | Behaviour | Guarantee |
| --- | --- | --- |
| **Provider unreachable (transient)** | Forwarder retries with backoff; the stream retains events; enforcement is unaffected (it reads the stream directly, never the provider) and its next cadence recompute sees all retained events. Billing is buffered in the stream and ships when the provider is back. | Provider outage never takes down enforcement or app traffic; no event lost within stream retention; enforcement never depended on the provider. |
| **PROVIDER-side gap — provider missed/rejected an event (resolves critique #5)** | The event is retained in the STREAM but the provider dropped/rejected/lost it. §6.3's on-demand `witness` (stream recompute) is the independent audit witness: for a **self-invoicing** stack (metronome/orb/lago) the provider meter IS the bill, so `witness > provider` → re-rate via `Backfiller::backfill`; for an **owned-invoicer** stack (openmeter+stripe_invoice, lite, or `stripe_meters` where we hold the account) `witness > invoiced` → a keyed DEBIT `adjustment_note` on the next invoice, plus a `provider_meter_drift` finding. | Provider-side loss is recovered from the stream witness; because the correction is toward `witness` (the true count) and keyed `(subject,period,correction_seq)`, it applies once and never over-bills. |
| **LOCAL/stream gap — events age out of the stream before reaching the provider (honest downgrade, resolves must-fix #5)** | If the forwarder is down longer than stream retention, events age out and NEVER reach the provider — and the §6.3 `witness` recompute reads the same (already-truncated) stream, so it recovers NOTHING here. This is explicitly a **fail-toward-underbill + HARD ALERT**, not a silent loss and NOT falsely claimed as recovered. The design guarantee is: **stream retention ≥ max forwarder-downtime SLO**; a `forwarder_down_exceeds_retention` finding + page fires the moment consumer lag approaches retention. Beyond that SLO, usage is lost and billed short — documented, alarmed, bounded by retention sizing. | The only honest guarantee. Reconciliation recovers PROVIDER-side gaps (row above), NOT truncated-stream gaps — the two are different failures and are not conflated. |
| **Self-invoicing provider period closes with late events (resolves critique #6)** | Forwarder ships promptly to hit the OPEN period; a straggler that misses close is corrected by the §6.3 pass via the provider's REAL `CorrectionCapability`: `Backfill` re-rates (incl. Metronome regenerating a finalized invoice), else a signed `adjustment_note` DEBIT on the next invoice (missed usage adds charge — a credit would be the wrong direction), plus a `late_period_adjustment` finding. NEVER a mythical `Meter::adjust`. | Late usage is corrected via the provider's actual mechanism, never assumed to bill by event-time alone, never assumed on an API that doesn't exist. |
| **Provider PERMANENT reject 4xx (resolves critique #14)** | Rejected events → `provider_dead_letter` + `provider_reject` finding + metric; offset commits so the pipeline never wedges. | Poison events are quarantined + surfaced, not retried forever. |
| **Worker crash** | Restart re-publishes the un-confirmed redb-floor tail (option a) with the same `event_id`s → provider dedup / broker idempotent-producer → no double, no drop. | At-least-once publish + idempotent landing = effectively-once (within §6 window). |
| **Replay past provider dedup TTL (resolves critique #2)** | A re-forward only touches the un-committed tail above the Kafka committed offset (§6.2), which is recent by construction; a stale event is NEVER blind re-ingested — it is reconciled at period grain by the §6.3 pass. | No double-bill even on >TTL outage / month-spanning backlog. |
| **Forwarder crash mid-batch (v5/v7 — no fold, no counter to corrupt)** | On restart the forwarder re-processes the tail above the last committed Kafka offset: the provider `event_id`-dedups the re-shipped events (billing exact). There is no PG fold, `stream_offsets`, fence, OR Redis counter to get wrong. Enforcement is a separate cadence batch reading the same retained stream — unaffected. | Billing stays exact (provider dedup); enforcement is stateless (recomputed from the stream). The R2 "new bill-01" and the whole §6.1 machinery are gone. |
| **Consumer rebalance / zombie old owner (v5/v7 — harmless, the R4 CRITICAL is dissolved)** | A stalled OLD owner committing late after reassignment can only re-ship a batch to the provider (deduped). There is no exactly-once local fold to double-apply and (v7) no Redis counter to stray-increment, so **there is nothing to fence** — the `fence_epoch`/monotonic-CAS apparatus is DELETED. | No billing error (provider dedup) and no enforcement error (enforcement never touches the forwarder loop). The multi-forwarder claim needs no fencing. |
| **Cadence gap — a runaway app between recomputes (v7 enforcement failure mode)** | Detection lags by up to one cadence (default 1h), so a runaway app can overshoot its cap by at most `R × cadence`. This is bounded by the **coarse per-app throughput cap** at the gateway (`enforce::RateLimitRegistry`/`ConcurrencyRegistry`), which caps `R` — the free tier gets a tighter cap. The next cadence recompute writes `app_spend_state` and the gateway Blocks instantly. Enforcement is a **stateless cron** — no counter to lose, nothing to rebuild. | Bounded overshoot ≤ `R × cadence`, shrinkable by tightening EITHER knob (cadence, cap). Never a wrong bill (billing is the provider). Per-app and isolated — no platform-wide term. |
| **Control/enforcement-batch outage** | The last-written `app_spend_state` is retained and the gateway keeps enforcing it; when the batch resumes it recomputes from the retained stream (bounded by stream retention, OQ-7) and catches up. No ephemeral state is lost. | Enforcement degrades to "stale but enforcing"; recovers fully from the stream. Billing unaffected (it never reads `app_spend_state`). |
| **Terminal period at account close / creator churn (v6, CRITICAL #2)** | No "next invoice" to carry a straggler debit. The terminal invoice is withheld until `period_end + settle_window`, then a final §6.3 reconcile runs the local recompute vs `invoiced`; residual → a terminal true-up line on the closing invoice, or a `terminal_period_trureup` finding + operator settlement if the account is already zeroed. | The last period is complete-by-construction (closed after settle); the terminal straggler is a surfaced true-up, never a silent uncorrectable underbill. |
| **Billing gap (misconfiguration)** | `build_stack()` refuses: no meter, no invoicer, meter-only without an invoicer, a self-invoicing provider that is not also the meter feed, or a not-`production_ready` provider without `--allow-unsupported-billing`. | Generalizes today's "refuse to boot with no creds." |
| **`lite` reaches real Stripe (A6 guard)** | Default Lite invoicer is the LOCAL-only sink (no Stripe). The Stripe path is opt-in AND `production_ready()==false` gates the whole provider behind `--allow-unsupported-billing`. | Evaluation cannot accidentally bill real creators; the money path is doubly gated. |
| **Compromised/buggy worker** | Per-worker auth (bill-04, S2) scopes `source`; provider dedup key `(source, event_id)` isolates namespaces; `i64::try_from` skip-not-wrap retained; custom-metric cardinality cap (100/app) retained. **Residual (R2 Part 7.8):** the dedup key isolates namespaces but not ATTRIBUTION — `subject.app_id` is stamped by the trusted runtime injection (server-injected `app_id`, per AGENTS.md), not app code, but a fully-compromised worker could still assert another app's subject. Mitigation (OQ-10): the forwarder cross-checks `subject.app_id` against the route registry's worker→app assignment before forwarding/incrementing; a mismatch → `subject_attribution_mismatch` finding + drop. | Cross-worker suppression + overflow + cardinality closed; attribution forgery bounded by the registry cross-check (OQ-10). |
| **Enforcement-changing mutation on an idle app** | `set_limit`/`set_plan`/FX/suspend all write `spend_dirty` (Pillar 5 table). | A lowered limit enforces on the next fast tick, not the hourly sweep (resolves critique #10). |

---

## 12. Cross-cutting concerns the v1 doc omitted

### 12.1 Multi-currency / FX under delegated invoicing (Missing-Concept #1)

Local enforcement + the CU pricing model are **USD-only today**. Under delegation, a provider (Metronome/Orb/Lago) may invoice a creator in the creator's own currency. Rule: **enforcement stays USD** (the spend limit is a USD number against USD-priced CU); the **invoice currency is the provider's/creator's**, and the `stripe_invoice`/Lite invoicer path converts CU→amount at the operator FX rate (the existing `default_fx` in `bill_creator`). Reconciliation compares **CU quantities** (currency-neutral) between the stream `witness` and the provider aggregate, not amounts, so FX never confuses drift detection. Full multi-currency enforcement (non-USD spend limits) is deferred (OQ-5).

### 12.2 Refunds / credits / disputes under delegated invoicing (Missing-Concept #2)

Delegating invoicing means credit notes, proration reversals, and dispute webhooks originate at the provider. The correction surface is:
- `Invoicer::adjustment_note(subject, AdjustmentNote)` — a SIGNED note flows back into local bookkeeping as an `invoice_lines` entry (negative = `credit_note` for a provider-issued credit/refund; positive = `debit_note` for a late under-bill that adds charge — resolves critique #6's wrong-direction bug) on a linked `invoices` row, so the creator dashboard and local totals reflect it. This is the same verb the §6.3 `InvoiceCredit` correction path uses.
- Stripe dispute/refund/invoice webhooks are handled by the existing production route (`stripe_handlers::webhook`, wired at `/internal/webhooks/stripe`) with its own signature verification and redelivery idempotency. Provider adapters do not expose a separate webhook capability.

### 12.3 Provider→provider migration (Missing-Concept #3)

Switching providers (e.g. `lite`→`metronome` at launch, or Metronome→Orb) mid-period:
- **Cutover at a period boundary** is the supported path: close the old provider's current period (its `close_period`/self-invoice runs on its own cycle), then repoint the `--meter/--invoicer-provider` roles. The forwarder's Kafka consumer-group offsets are per-group, so a new provider starts forwarding from the cutover offset — no replay of already-billed events.
- **Mid-period cutover** requires a backfill: seed the new provider with the current period's usage via the on-demand stream `witness` recompute. If the new provider's `correction()` is `Backfill` (Orb/Lago/Metronome), `Backfiller::backfill(subject, period, correct_total = witness)`; if it is an owned-invoicer stack (`stripe_invoice`/`lite`), it rates the provider's aggregate at close so the new meter feed suffices (just forward from the cutover offset); freeze the old provider (stop forwarding). A `provider_migration` runbook, not automated tooling (there are no production tenants — the pre-launch stance means we design the procedure, not build migration tooling). Double-billing across the switch is prevented by the per-provider dedup key + the §6.3 period reconcile.

### 12.4 Creator-facing usage/billing UX under a split SoT (Missing-Concept #5)

The split SoT means the dashboard could show different numbers than the invoice. Rule (the invoice number is the provider's; the live widget is the fast approximate one):
- The **live usage widget** (current-period) reads the **per-app `app_spend_state`** (the same per-app number enforcement uses) — the value the cadence recompute last wrote. It is explicitly a "so far this period (as of the last refresh)" figure, refreshed each cadence (default 1h). It may lag current usage by up to one cadence; the widget is labelled accordingly. The canonical *billing* number remains the invoice (below).
- The **invoice/billing history** reads whatever produced the invoice: for an **owned-invoicer** stack (openmeter+`stripe_invoice`, `lite`) that is the finalized `invoices`/`invoice_lines` rows WE wrote by rating the provider's settled aggregate; for a **self-invoicing** provider it reads the provider's invoice API — the number the provider rated from its own meter.
- Because the live widget is approximate and the invoice is the provider's canonical number, a small live-vs-final difference is expected and labelled; a **material** drift surfaces a reconciliation banner sourced from `billing_reconciliation_findings`, and the creator sees the invoicing party's figure on anything labelled "invoice." This is documented so the split is intentional and legible, not a silent inconsistency.

### 12.5 Stripe Connect customer-model note (Missing-Concept #8)

Connect payments are a non-goal here, but the platform-side billing Customer and each creator-linked Connect account are **different Stripe objects on different accounts** and do not collide. The `SubjectRef` customer mapping here is platform-side only.

---

## 13. Zero-tokio compliance (A5 — "no tokio RUNTIME in-process")

The invariant is precisely: **no component instantiates a tokio reactor.** Link-level tokio is a *compile-time* transitive dependency of `cyper→hyper-util` and is tolerated (and being separately removed) — `crates/zeroship-metering/Cargo.toml:10-13` already documents "cyper → hyper pulls tokio into the lock file … but NO tokio runtime is ever instantiated here — the flush task runs on compio."

Every new component upholds it:
- **Producer / worker outbox:** the `rust-rdkafka`/librdkafka producer runs on **librdkafka's own C background threads** — a native C library, the same category as `libpg_query` in `zeroship-migrate`. No async runtime. The optional redb floor is compio file IO.
- **Control forwarder + provider HTTP:** stream consume via rdkafka C threads (or a compio poll loop for a non-rdkafka transport); provider/aggregate calls over **cyper (compio)**. The `ProviderCtx.http` factory hands out per-thread cyper clients (§5.4) — no cross-thread `SendWrapper`, no reactor.
- **Enforcement (v7):** `libs/compio-redis` is **no longer on the enforcement path** — the Redis counter and its `INCRBY`/`MGET`/`pexpire`/`SCAN`/`EVAL` are deleted. Enforcement is the periodic per-app recompute: a stream read (rdkafka C threads) + in-process SUM + `charge_cents`/`derive_state` + a Postgres UPSERT to `app_spend_state` over `compio-postgres` — all compio, no new reactor. (`compio-redis` remains available to the platform for other uses; it simply isn't needed here.)
- **No new tokio reactor anywhere** on the money path. rdkafka uses C threads; `compio-postgres`/`cyper` are compio; nothing instantiates a tokio runtime.

---

## 14. Best-practice scorecard (14 principles)

| # | Principle | How this design hits it |
| --- | --- | --- |
| 1 | **Event-first** | `UsageEvent` is the produced unit (windowed delta, OQ-1); aggregates derived. |
| 2 | **Exactly-once where it matters (billing), bounded-cadence where it doesn't (enforcement)** | v7: exactly-once lives ONLY at the provider — provider forward = at-least-once + `event_id` dedup within the Kafka committed offset (§6.2). Enforcement = a **periodic per-app recompute at a tunable cadence** (default 1h) → `app_spend_state` → instant gateway edge (§Pillar 4/5), **provider OFF the enforcement path** (different grains/freshness — §0). No local exactly-once fold, no `stream_offsets`, no `fence_epoch`, and (v7) **no Redis counter, no `INCRBY`, no re-base** — the R2 "new bill-01" and R3/R4 zombie-double-fold stay dissolved. Overshoot ≤ `R × cadence`, bounded by a coarse per-app throughput cap. |
| 3 | **Durable buffer** | Replicated Redpanda stream (`acks=all`) + optional worker redb floor for the ship path (§7). |
| 4 | **Provider is the source of truth (for BILLING)** | No Postgres raw-event table AND no per-event local ledger (A4, v5); the provider stores + aggregates authoritatively for billing. Enforcement is a different grain (per-app) drawn from a LOCAL per-app stream recompute — the provider is NOT its source (§0). Local keeps only a periodic-cadence recompute → `app_spend_state` (no Redis counter, v7). |
| 5 | **Pluggable transport** | `StreamTransport` registry — Kafka/NATS/managed as one file + 2 lines (Pillar 2). |
| 6 | **Decoupled rating** | Rating is internal to the provider or owned-invoicer close path — never on the hot path. |
| 7 | **Fail-toward-underbill** | Outage never blocks traffic; overflow/dead-letter shed with metrics + findings + corrective path (§11); an enforcement-batch outage → the gateway keeps enforcing the last `app_spend_state`, recomputed from the retained stream on resume (no ephemeral state to lose); a runaway app's overshoot is bounded ≤ `R × cadence` by the throughput cap (§Pillar 4/5). |
| 8 | **Reconciliation + REAL correction (bounded safety net)** | Period-level (§6.3), independent witness = the LOCAL per-app stream recompute — the SAME batch that drives enforcement (one job, two consumers). Owned-invoicer rates the recompute at close (after settle) + corrects `witness` vs finalized `invoice_lines` (a keyed `adjustment_note` on a real straggler/loss; terminal period via a true-up); self-invoicer reconciles `witness` vs the provider meter and dispatches the provider's actual `CorrectionCapability` (`Backfill`/`None`). Every correction keyed `(subject, period, correction_seq)`. Spine UNCHANGED from v4. |
| 9 | **Event-time bucketing (scoped)** | The recompute buckets by event-time (re-derived each cadence); self-invoicing provider late events corrected via §6.3 per real capability (honest, §Pillar 4/§6). |
| 10 | **Commutative aggregation** | The recompute is an additive `Σ` per-`(app,meter,period)` over the retained stream; out-of-order + late events sum correctly each cadence. |
| 11 | **Immutable invoices** | `invoices`/`invoice_lines`/`billing_provider_refs` unchanged; credits are new negative lines (§12.2); providers self-invoice. |
| 12 | **Tamper-resistant signal** | No `env.meter`; platform-measured; per-worker auth scopes `source` (bill-04, S2); dedup key `(source,event_id)`; overflow skip-not-wrap. |
| 13 | **Cardinality control** | `dims` low-cardinality by contract; per-app custom-metric cap (100); conformance can assert dim bounds. |
| 14 | **Money-path observability** | Forwarder lag, ingest ack/dedup counts, dead-letter count, drift/adjustment findings, floor-shed metric; the reconciliation job surfaces divergence. |

---

## 15. Open questions / risks

- **OQ-1 — Event granularity vs volume (load-bearing).** At millions of users, per-request events are too many. Recommendation: ship **windowed deltas** — the "event" is a per-`(subject, meter, drain-window)` delta with a stable `event_id`, not a single request. This keeps immutable/idempotent properties while collapsing volume ~1000×. Needs sign-off that windowed events satisfy each provider's rating granularity (most usage billers accept periodic aggregated ingest). This is now the primary scaling lever, not the removed Postgres table.
- **OQ-2 — Subject grain (not load-bearing for ENFORCEMENT).** Providers meter per customer/subject; enforcement is per app. `Subject { app_id, creator_id }` carries both. The enforcement recompute reads the LOCAL per-app stream (partition-keyed by `subject`, so per-app by construction), NOT the provider — so a provider that can attribute ONLY at creator grain does not affect enforcement. This OQ is now purely a **billing/dashboard** concern: confirm each target provider can attribute per-app dims under one customer for the creator dashboard's per-app read-back, or accept creator-grain provider-side + per-app locally (the local recompute already provides per-app for the dashboard's live widget, §12.4).
- **OQ-3 — Local durability floor cost (§7).** Measure the redb-floor fsync cost on the 200K-req/s producer in S5; the DEFAULT (a vs b) is measurement-gated, not pre-recommended (R2 Part 7.7).
- **OQ-7 — the LOCAL per-app recompute fan-out + retention constraint at scale (v7: now a ONCE-PER-CADENCE batch, not continuous).** The enforcement recompute reads the LOCAL stream (not the provider) and is the SAME batch that produces the §6.3 billing `witness` (one job, two consumers). **v7 relaxes this OQ substantially:** the recompute runs **once per cadence (default hourly), not 60×/min**, so its fan-out is a periodic O(apps) batch — trivially shardable by app-id range across control instances, and lengthening the cadence directly cuts its cost. The sub-minute-era scaling concerns are **removed**: there is no Redis-cluster counter to shard, no CHWBL-sharded sub-minute evaluator, and no whale-partition-for-enforcement worry (whale partitioning remains a billing-throughput OQ-8 concern only). Two residual costs to size in S6/S7:
  - **Retention constraint (explicit):** the recompute reads the stream, so **Kafka retention ≥ settle_window + reconcile_cadence + reconcile_duration** for every period — otherwise the recompute reads a truncated stream and both enforcement AND the §6.3 witness go short. (§11 also carries `retention ≥ max forwarder-downtime SLO`; this is the tighter of the two.)
  - **Recompute fan-out (once per cadence):** at millions of subjects a full-stream re-aggregation per cadence is a batch job. Mitigations: (i) **incremental recompute from a per-partition checkpoint** (re-aggregate only offsets since the last recompute + carry a running per-`(subject,period)` sum), (ii) **shard by app-id range** across control instances, (iii) only recompute subjects with stream activity since the last pass, (iv) cadence tuning (a longer cadence linearly cuts cost and linearly grows the detection-latency bound — §Pillar 5 SLO). The residual PROVIDER reads — §6.3's `read_aggregate` health cross-check and §6.2's closed-period gate — remain per-subject provider calls but are billing-side, slow-cadence, and off the enforcement path. Because the recompute is once-per-cadence and §6.3 is a bounded safety net, this fan-out degrades *timeliness/coverage*, NOT invoice correctness. Much reduced from v6; still the top scaling item on the recompute/reconcile path.
- **OQ-8 — Hot partition / whale subject (R2 Part 7.4).** Partition-key = subject pins one high-volume creator/app to a single partition → a per-subject throughput ceiling. Options: a composite key `(subject, shard)` with N shards for whale subjects (aggregation still sums across shards per `(app, period)`), or an operator-set whale list. Deferred; documented so the ordering guarantee (§Pillar 2 contract #3) is understood to be per-subject, and sharding a whale trades strict per-subject order for throughput.
- **OQ-9 — Multi-region (R2 Part 7.5).** v3 assumes one Redpanda cluster + a forwarder consumer group co-located with control. Cross-region stream placement, forwarder region affinity, and provider-region routing are unaddressed and deferred to a scale-out epic.
- **OQ-4 — Provider webhook expansion.** Stripe platform webhooks use the existing `/internal/webhooks/stripe` route. A future non-Stripe provider webhook path should be designed as a production HTTP route, not as an unused provider capability.
- **OQ-5 — Non-USD spend limits.** Full multi-currency enforcement (a creator whose spend limit is in EUR) is deferred; v2 keeps enforcement USD, invoice currency provider-side (§12.1).
- **OQ-6 — Provider-independent cold archive.** An object-storage/Parquet archive of the stream (via the existing compio-s3) would give provider-independence + replayable history for migration. NOT built by default under "stream-to-provider only" (A2); documented as the escape hatch if provider lock-in becomes a concern.
- **OQ-10 — Subject attribution binding (R2 Part 7.8).** `subject.app_id` is runtime-stamped, not app-asserted, but a compromised worker could forge another app's subject. Evaluate the forwarder-side registry cross-check (worker→app assignment) vs signing the subject into the per-worker credential. Deferred to S2/S4 hardening.
- **Risk — L1 "fits-the-shape" preconditions (R2 Part 5).** The one-file+2-line claim holds for an adapter that fits the existing shape. A genuinely novel dedup identity (a variant on `DedupKey`), a brand-new capability (a `Capabilities` flag / `as_*()` method + consistency arm), a `CorrectionCapability` beyond the three variants, or an adapter needing DB access beyond `LiteStore`'s methods still forces a core edit. v3 enumerates these as the known preconditions rather than claiming universality; the common commercial-provider shapes (meter/rate/invoice + one of the three correction modes) are all in-shape.
- **Risk — `LiteStore` scope creep.** Keep it to the ~7 methods the reconciler + local sink need; review at S1.
- **Risk — enforcement detection-latency + overshoot SLO.** Detection latency = the cadence (default 1h, tunable to e.g. 15-min); worst-case overshoot = `R × cadence` bounded by the per-app throughput cap. Both are SLOs to be MEASURED in S6 (measure-don't-estimate), not assumed bounds. The two knobs (cadence, cap) are the operator's levers to trade cost for tightness.
- **Risk — conformance fidelity.** A localhost mock cannot fully reproduce a provider's real dedup TTL / period-close; the clock-advancing test + the `dedup_contract_matches_docs` pin (Pillar 7 #2) mitigate the highest-risk quirk, but production dedup behaviour is verified against a real provider sandbox in S7, not only the mock.

---

## Changelog v1→v2

**Re-scope (owner-locked A1–A6):**
- **Stream-to-provider only.** Removed the in-house "immutable `usage_events` in Postgres as SoT" model entirely (A2/A4). The provider is the event store of truth; Postgres holds only config, spend, invoice bookkeeping, findings, and the derived enforcement aggregate.
- **Durable STREAM behind a second registry.** Added `StreamTransport` (Pillar 2) — symmetric to the provider registry, Redpanda default via `rust-rdkafka`/librdkafka (C threads, no runtime). Replaced the v1 "redb WAL + Postgres landing table" with the stream + an optional small redb *floor* (§7), evaluated both ways.
- **Millions-scale (A1).** Windowed-delta events (OQ-1) as the primary volume lever instead of a Postgres table.
- **Lite reframed (A6).** From "dev-only test double" to a real evaluation-grade, production-readiness-gated provider with a **local-only invoice sink** (zero external account) and `--allow-unsupported-billing`. Recording fakes separated from Lite.

**Critique CRITICALs resolved:**
- **#1** — Dropped the false "reuse `bill_creator` verbatim" claim; it is an honest refactor (signature `&AppState`→`&dyn LiteStore`) that preserves the money math, gated by the billing-ops regression suite (§5.5).
- **#2** — Preserved the deliberate read-back/delta safety net via a per-adapter `DedupContract{key, ttl}`: naive `event_id` replay only within the provider's window, aggregate-delta past it (§6). Never double-bills past Stripe's ~24h TTL.
- **#3** — Split the two Stripe roles: `stripe_meters` (self-metering meter+invoicer) vs `stripe_invoice` (invoice-only, rates from the meter's `read_aggregate`). Fixed the 3-role/2-id mismatch with role-addressed selection; the OpenMeter+Stripe stack now actually bills (§5.3).
- **#4** — Largely moot (no Postgres `event_id` PK). Closed the stream-world equivalent: broker-monotonic offset for the local fold, per-worker auth (bill-04) pulled to S2, dedup key `(source, event_id)`.
- **#5** — Designed the corrective path for outages exceeding stream retention: dead-letter + alert + read-back/`adjust`/credit recovery (§11), not a silent loss.
- **#6** — Event-time bucketing scoped to the local/enforcement + Lite path only; provider period-close handled via `adjust`/credit (§6, §Pillar 4).
- **#7** — Fully-local invoice sink for zero-external-account evaluation; recording fakes separated from Lite (§Pillar 6).
- **#8** — Every enforcement-changing mutation (limit/plan/FX/suspend) writes `spend_dirty` (Pillar 5 table), not just usage folds.

**Smaller flags + missing concepts resolved:** per-thread `HttpClientFactory` (no shared `SendWrapper`), `capabilities()` and `as_*()` consistency enforced at registry build, honest redb dependency accounting (section 7), honest "one file + 2 lines" count, `SecretResolver` into factories, `provider_dead_letter`, and new sections for multi-currency (section 12.1), refunds/credits/disputes (section 12.2), provider-to-provider migration (section 12.3), the event-forwarder as a first-class component (Pillar 7), creator-facing UX under a split SoT (section 12.4), and the Connect customer-model note (section 12.5).

---

## Changelog v2→v3

R2 scored v2 **57/100 — NOT converged**: the money path's exactly-once story was broken in three independent places and the corrective spine assumed an API the primary providers do not have. v3 addresses all six R2 must-fixes. (The four R1 CRITICALs the R2 critic marked resolved — #1 LiteStore refactor, #3 split Stripe roles, #7 Lite framing, #8/#10 spend_dirty — are re-verified below and remain resolved.)

**Must-fix #1 — Forwarder fold↔offset atomicity (R2 Part 4, the new bill-01).** New §6.1: the local fold (`usage_aggregates +=`), `spend_dirty` mark, and `stream_offsets` UPSERT commit in ONE Postgres transaction. The committed offset gates re-consumption, so the enforcement aggregate is EXACTLY-ONCE by construction; the broker `commit` becomes a lossy optimization. Stated explicitly as a load-bearing split: **local fold = exactly-once (txn+offset); provider forward = at-least-once + provider dedup.** Pillar 4, Pillar 7 loop, §9 data-model, and a §11 row updated.

**Must-fix #2 — Killed the mythical universal `Meter::adjust` (R2 Part 7.1 / critique #3,#6).** Removed `adjust` from the `Meter` trait. Added a per-adapter `CorrectionCapability::{Backfill{window,closed} | InvoiceCredit | None}` (`correction()` on the identity trait) plus a `Backfiller` capability sub-trait and a signed `Invoicer::adjustment_note`. Researched each target's REAL API (§8 `Correction` column, sources in-text): Stripe meter events immutable → `InvoiceCredit`; OpenMeter append-only → `None` at the meter but the owned invoicer corrects; Orb backfill/Lago new-`transaction_id`/Metronome amend+regenerate → `Backfill`; `stripe_invoice`/`lite` → `InvoiceCredit` (we own the invoice). Where `None`, the corrective path is a surfaced operator finding, documented honestly.

**Must-fix #3 — `stripe_invoice` rate-at-close race (R2 Part 3).** Owned invoicers (`stripe_invoice`/`lite`) now rate the **LOCAL** aggregate (complete, folded exactly-once) at close, NOT the lagging provider `read_aggregate`. `read_aggregate` is demoted to the §6.3 reconciliation cross-check only. §5.3, Pillar 5, the `stripe_invoice` row, and the `Invoicer` trait doc updated.

**Must-fix #4 — §6 reconciliation grain (R2 Part 2).** §6 restructured into three subsections: §6.2 per-batch forwarding = within-watermark `event_id` ingest ONLY (no per-batch delta → the Part 2.1 over-push is gone); §6.3 a SEPARATE periodic pass computes `local_period_total − provider_read_aggregate` ONCE per `(subject, period)` and corrects via must-fix #2's real mechanism. The three interleavings fixed: batch-straddle → offset-vs-watermark split (monotonic, no per-event age); clock skew → per-provider forward-watermark (never re-ships below it; no `event_time` age arithmetic); period-closes/closed-within-TTL → the period-state gate uses the provider-acknowledged period, and closed-period events route to §6.3. `DedupTtl` split into `Bounded/Unbounded/Unknown` (critique #2 point 5). New `provider_forward_watermark` table.

**Must-fix #5 — Outage-exceeds-retention honesty (R2 Part 1 #5 / Part 7.3).** §11 now separates two DIFFERENT failures: a PROVIDER-side gap (local folded, provider missed) IS recovered by §6.3; a LOCAL gap (forwarder down > retention) recovers NOTHING (never folded locally) and is an honest **fail-toward-underbill + hard alert** with the guarantee **retention ≥ max forwarder-downtime SLO** (a `forwarder_down_exceeds_retention` page fires as lag approaches retention). No more false "reconciliation recovers it" claim.

**Must-fix #6 — StreamTransport abstraction honesty (R2 Part 6).** Pillar 2 scoped to the **Kafka log family** (Redpanda default + Kafka / Kinesis-Kafka-API / EventHubs-Kafka-API) with an explicit documented contract (partition offsets, consumer groups, partition-key ordering). Non-Kafka buses must MEET the contract (JetStream, validated) or are out of scope (NATS-core, SQS, PubSub named as unfit). Partition key = `subject` (per-subject ordering). >1 forwarder via consumer-group partition assignment (each partition folded by exactly one consumer; rebalance safe via §6.1's transactional offset).

**Also (R2 residuals):** webhook redelivery idempotency (§12.2); subject-attribution forgery residual + registry cross-check (§11, OQ-10); redb floor default de-recommended pending S5 (§7, OQ-3); reconcile-at-scale (OQ-7), hot-partition/whale (OQ-8), multi-region (OQ-9), and the L1 "fits-the-shape" preconditions (Risk) all called out. Scorecard rows 2/8/9 and the §14/§13 wording updated to the new model.

---

## Changelog v3→v4

R3 scored v3 **64/100 — NOT converged**, with exactly TWO substantive holes (both small, local fixes) plus doc-accuracy nits. All four R1 CRITICALs and all six R2/R3 must-fixes remain resolved (re-verified — no regressions). v4 closes the two holes and the nits; it does NOT redesign anything else.

**MUST-FIX 1 (CRITICAL) — Rebalance/zombie double-fold: fence the fold↔offset write (§6.1).** The R3 `stream_offsets` UPSERT was a blind `SET offset=EXCLUDED.offset` — a stalled OLD partition owner committing its fold AFTER a rebalance (new owner already folded) double-counted `usage_aggregates`, so the "rebalance safe" / ">1 forwarder" claim was not actually delivered. v4 makes the offset write **fenced + monotonic-CAS inside the same fold txn**:
- `stream_offsets` PK `(consumer_group, topic, partition)` gains a `fence_epoch BIGINT` column (§9).
- On partition ASSIGNMENT the forwarder obtains a monotonic `fence_epoch` — the **Kafka consumer-group generation id** (the group coordinator bumps it every rebalance and never lowers/reissues it, so a new owner always has a strictly-higher epoch than a zombie; `rdkafka` surfaces it); a contract-meeting non-Kafka transport supplies an equivalent monotonic assignment counter (StreamTransport contract #4, §Pillar 2; `StreamPoll.fence_epoch`).
- The fold txn's offset write is `UPDATE zeroship.stream_offsets SET offset=$new_off, fence_epoch=$my_epoch WHERE consumer_group=$g AND topic=$t AND partition=$p AND fence_epoch <= $my_epoch AND offset < $new_off`, and **on 0 rows updated the WHOLE fold txn ABORTS** (rolls back `usage_aggregates +=` and `spend_dirty`). A zombie's stale epoch (or an already-advanced offset) matches 0 rows → its late commit is discarded, never double-folded. BOTH predicates are load-bearing: `offset < $new_off` catches the same-batch zombie; `fence_epoch <= $my_epoch` additionally catches the partial-overlap case (new owner folded only a prefix). Updated §6.1, §Pillar 2 (contract #4 + `StreamPoll`), §Pillar 7 (>1-forwarder claim now delivered by fencing, not asserted), §9 (`stream_offsets` gains `fence_epoch`), §11 (new "consumer rebalance / zombie old owner" row), scorecard row 2.

**MUST-FIX 2 (MAJOR) — §6.3 reconcile: correct comparison basis + idempotency + honest framing.** The R3 pass computed `drift = local − provider_meter` and issued a debit for EVERY stack — but for OWNED-invoicer stacks (openmeter+`stripe_invoice`, `lite`, owned `stripe_meters`) the invoice IS the local aggregate, so that measured provider-meter LAG, not billing error → spurious debit → OVER-bill, re-firing every cycle (no idempotency key). v4 splits §6.3 by capability:
- **Owned-invoicer:** the correctness reconcile compares `local_final` vs the finalized `invoice_lines` quantity; a correction (a signed `adjustment_note`) is issued ONLY when THOSE differ (a straggler after finalize). `read_aggregate` is demoted to a **health cross-check** that writes a `provider_meter_drift` FINDING, never a bill.
- **Self-invoicer:** reconcile `local` vs the provider meter and correct via `CorrectionCapability::Backfill`/`None` (unchanged).
- **Idempotency:** every correction carries a stable `(subject, period, correction_seq)` key (`correction_seq` bumps only when the corrected quantity actually changes), persisted via the existing UNIQUE-`dedup_key` + `ON CONFLICT DO NOTHING` mechanism the reconciler already uses (`cron/stripe_reconcile.rs:617-664`), so a correction is applied at most once and never re-fires.
- **Framing:** §6.3 is reframed from "the spine everything depends on" to a **bounded SAFETY NET** — after MF#3 the primary paths bill correctly without it, and its worst-case miss is fail-toward-underbill. Updated §6.3, Pillar 5, §8 (`stripe_invoice` Correction note + `correction()` trait doc), §11 (provider-side gap row), scorecard row 8.

**MINOR nits.** (1) Stripe windows disambiguated everywhere: the **~24h meter-event cancel window** (MeterEventAdjustment cancel; after 24h send a negative event) vs the **~35d event-backdate ingest window** — the `InvoiceCredit` mapping is unchanged (§8 matrix + source-of-truth note + `CorrectionCapability` doc). (2) "complete/authoritative at close" softened to "the local aggregate at close (complete modulo forwarder lag; stragglers caught by §6.3)" (§5.3, Pillar 5, `Invoicer::close_period` doc). (3) The closed-period per-subject provider-query cost of §6.2/§6.3 folded into OQ-7 as the same fan-out family (coarse cached period markers + "unknown ⇒ route to §6.3").

**Convergence:** the two R3 holes are closed with local, verifiable mechanisms (a fenced CAS on `stream_offsets`; a corrected comparison basis + `(subject, period, correction_seq)` idempotency key). Residual open items are all deferrable scaling/hardening questions (OQ-7 reconcile/gate fan-out, OQ-8 whale partition, OQ-9 multi-region, OQ-10 attribution binding, the S5 redb-floor measurement, the S6 enforcement-lag SLO) — none is a correctness blocker on the primary money paths. The design is production-plannable.

---

## Changelog v4→v5

**A SIMPLIFICATION round, not a hardening round.** v5 is driven by two owner decisions that let the design DELETE a large amount of machinery: **(1)** the metering PROVIDER is the SINGLE canonical source of truth for usage + billing (no parallel local exactly-once ledger); **(2)** enforcement needs minute/sub-minute latency but NOT per-event exactly-once accuracy. Splitting these two accuracy requirements — which v1–v4 conflated into one correctness-critical local exactly-once fold — keeps exactly-once in ONE place (the provider) and demotes enforcement to a fast approximate self-healing guardrail. The billing half of the design (both registries, capability sub-traits, the §6.2 dedup-TTL forwarding, the §6.3 correction spine, Lite, per-worker auth, the Kafka-family transport contract) is UNCHANGED. See the new **§0. Governing principle**.

**DELETED (the simplification — machinery with nothing left to protect):**
- **The §6.1 exactly-once local fold** — the `usage_aggregates += / spend_dirty / stream_offsets` single-PG-transaction. Gone. §6.1 is now a one-paragraph tombstone explaining the removal.
- **The `fence_epoch` monotonic-CAS + the whole rebalance/zombie-double-fold apparatus** (the R3/R4 CRITICAL, StreamTransport contract item #4, `StreamPoll.fence_epoch`). With no correctness-critical local fold, there is nothing to fence — a rebalance's only effects are an idempotent provider re-ingest (deduped) and a bounded stray Redis `INCRBY` (re-based away). Removed from §6.1, §Pillar 2 (contract #4 struck), §Pillar 7 (>1-forwarder claim now needs no fencing), §11 (the "zombie double-fold" row reframed to "harmless").
- **The `stream_offsets` PG table** — the consumer position lives in Kafka's own offset store (committed after provider-ship + counter-`INCRBY`). §9.
- **The `provider_forward_watermark` PG table** — the forward-watermark is the Kafka committed offset. §6.2, §9.
- **`usage_aggregates` as a real-time per-event exactly-once counter** — demoted to an OPTIONAL periodic spend snapshot (dashboards/history) and/or `lite`'s own provider-side meter store. §9.
- **Postgres is now fully OFF the per-event stream path.** The only per-event side-effects downstream of the stream are an HTTP provider ingest and a Redis `INCRBY`; PG is written only periodically (spend snapshot, invoices, findings).

**ADDED (the one new mechanism — a fast approximate enforcement counter):**
- **A shared fast spend counter in Redis** (`libs/compio-redis`, already exposing `incr_by`/`mget`/`set`/`pexpire`): the forwarder/enforcement consumer best-effort `INCRBY`s a per-`(subject_or_app, period, metric)` key as events flow — no transaction, no offset fence. Shared (not per-gateway-node memory) because CHWBL spreads an app across nodes. §Pillar 4.
- **Sub-minute enforcement read:** a periodic evaluator `MGET`s the counter, rates it (`pricing::charge_cents`), runs `spend.rs::derive_state`, and UPSERTs `app_spend_state`, which the gateway pulls via the existing 5s `RouteEntry.spend_state` (Decision D1). No provider, no PG-per-event on the fast path. §Pillar 5.
- **Periodic RE-BASE to canonical truth:** every few minutes a task `SET`s each active counter from provider aggregate rating (or a stream batch recompute), healing best-effort-`INCRBY` drift and rebuilding after a full Redis loss. The provider read (OQ-7 fan-out) happens only at this SLOW cadence, off the request path. §Pillar 4.

**REFRAMED (billing basis follows from decision #1):**
- **Owned invoicers now rate the provider's SETTLED aggregate at close** (`read_aggregate` after the provider's settle window), NOT a local exactly-once fold (v4 MF#3). The lag MF#3 dodged is handled by closing after settle; post-finalize stragglers are corrected by §6.3. §5.3, Pillar 5, §8 `stripe_invoice` row, `Invoicer`/`Meter` trait docs.
- **§6.3's independent witness is now an on-demand STREAM RECOMPUTE** (`witness(subject, period)`), not the deleted PG fold — preserving provider-loss detection (critique #5) without a local ledger. The correction verbs, keys, and once-per-period cadence are UNCHANGED. §6.3.
- **bill-05 reframed to an enforcement SLO + drift bound:** freshness = counter-incr lag + evaluator tick (target sub-minute); worst-case overshoot = drift within one re-base window + measurement lag — bounded, self-correcting, errs slightly-early/late on Block, never a wrong bill. Two knobs: re-base cadence + evaluator tick. §Pillar 5.

**Updated for coherence:** §0 (new), §1–§3 (context/goals/locked decisions), §Pillar 2 (offsets in Kafka, contract #4 removed), §Pillar 4/5 (rewritten), §6 (§6.1 deleted; §6.2 watermark→Kafka offset; §6.3 witness), §7 (redb floor = SHIP path only), §8 (`stripe_invoice`), §9 (data-model delta), §10 (S3/S4/S6 slices), §11 (failure modes — deleted the fold/zombie rows, added Redis-loss + drift rows), §12.1/12.3/12.4, §13 (compio-redis zero-tokio), §14 (scorecard rows 2/3/4/7/8/9/10), §15 (OQ-7 re-base fan-out).

**No billing-correctness regression:** the provider-side exactly-once boundary (§6.2 forward + dedup) and the correction spine (§6.3 `CorrectionCapability` / `Backfiller` / signed `adjustment_note` / `(subject,period,correction_seq)` keys) are untouched. Money stays exact at the provider; only the redundant local exactly-once ledger — and everything built to make it safe under rebalance — is removed.

---

## Changelog v5→v6

**A CONVERGENCE round for the v5 simplification.** R5 scored v5 **66/100 — NOT converged**: the v5 deletion (fold + `fence_epoch`/CAS + two PG tables) was correct and the billing half intact, but the ONE mechanism v5 *added* — the Redis enforcement counter + its re-base self-heal — was under-specified in three correctness-relevant places, and its stated safety direction was **backwards**. v6 fixes all three CRITICALs + both MAJORs. **No v5 win is walked back:** the §6.1 exactly-once fold, `fence_epoch`/CAS, `stream_offsets`, and `provider_forward_watermark` stay DELETED; PG stays off the per-event path; the §6.2 forward+dedup and §6.3 correction spine stay intact.

**The governing insight added up front (§0):** *ENFORCEMENT CANNOT BE PROVIDER-SOURCED.* Spend limits are per-app; the provider meters per-creator/subject AND lags — so it cannot source the per-app enforcement re-base. Enforcement gets a LOCAL per-app usage source; billing keeps the provider. Different grains, different freshness, legitimately different sources — this does NOT contradict "provider = single canonical BILLING SoT."

**CRITICAL #1 (re-base DIRECTION was inverted → under-enforce) — FIXED.** v5's provider-aggregate rebase used a LAGGING number and discarded in-flight `INCRBY`s → SET the counter BELOW true usage → Block fired LATE (under-enforce), the OPPOSITE of the claimed "slightly-early Block is safe." v6 makes the re-base **`counter = MAX(counter, recompute)`** (set-only-if-higher, atomic Lua `eval`) — **monotonic within a period, never lowered below true consumed usage**. Stated as an explicit invariant: **enforcement may Block slightly EARLY, never LATE.** §0, §Pillar 4 (rewritten re-base para + invariant), §Pillar 5, §11 (drift + Redis-loss rows), scorecard 2/7.

**CRITICAL #3 (re-base SOURCE must be LOCAL per-app, not the provider) — FIXED.** The re-base source is now a **periodic LOCAL per-app recompute of the retained stream** (`Σ` per `(subject=app, metric, period)` — partition-keyed by subject, so per-app by construction), NOT `provider.read_aggregate`. This dissolves the round-5 grain mismatch (a per-creator provider aggregate can't reconstruct N per-app counters) AND takes the provider **entirely off the enforcement path**. It is the SAME recompute §6.3 uses as its billing `witness` — ONE batch, two consumers (enforcement re-base + provider-loss detection). It stays a periodic BATCH (no per-event exactly-once fold), so PG stays off the per-event path and the v5 concurrency win survives. §0, §Pillar 4, §6.3, §9 (new "Local per-app recompute" batch-job row), OQ-2 (no longer enforcement-load-bearing).

**CRITICAL #2 (settle window unsized + terminal period unhandled) — FIXED.** The owned-invoicer close settle window is now sized (`settle_window ≥ max(forward_lag + provider_processing_lag)`, concrete few-minutes default), with a concrete **"provider-settled" signal** (the forwarder's Kafka committed-through position for the period past `period_end + settle_window`). A straggler after settle → the §6.3 credit/debit note. The **terminal period** at account close is closed specially (withhold the final invoice until `period_end + settle_window`, final reconcile, terminal true-up settlement — new `terminal_period_trureup` finding) so it is never a silent uncorrectable underbill. Owned invoicers rate the LOCAL recompute at close (complete after settle) cross-checked against the provider aggregate (health finding) — reconciling the v4 MF#3 "rate from local" intent with the v5 provider-canonical decision. §5.3 (new settle-window block), §5.3 role text, `Meter::read_aggregate`/`Invoicer::close_period` docs, §11 (new terminal-period row).

**MAJOR (Redis early-402 blast radius + Block/Warn conflation) — RESOLVED.** Warn(~80%)/Degrade(~95%, throttle, app stays up)/Block(~100%, 402) are cleanly separated (actual `SpendThresholds`); the fast approximate path drives **Block only past the hard cap**. Because the counter errs conservatively (over-count) and each app's counter is **independent + per-app**, the blast radius of counter error is **bounded to a few apps near their OWN limit — not a platform-wide mass-402**. Redis loss → each app rebuilt **independently per-app** from the local recompute (bounded by the recompute cadence), never a synchronized platform-wide hole. §Pillar 5 (threshold + blast-radius bullets), §11 (Redis-loss row rewritten per-app).

**MAJOR (§6.3 witness/recompute cost + retention) — RESOLVED as an explicit OQ.** The shared local recompute reads the stream, so it needs **Kafka retention ≥ settle_window + reconcile_cadence + reconcile_duration**, and at millions of subjects its fan-out is a real cost (mitigate: rolling per-partition slices, incremental recompute from a checkpoint, cadence tuning). Folded into **OQ-7** (retitled to the local-recompute fan-out + retention constraint). The honest under-enforce bound = full-sweep time (rolling slice), NOT "one re-base window" — the round-5 contradiction is corrected in the §Pillar 5 SLO.

**Also (round-5 MINORs):** the `(subject_or_app, …)` key hedge is resolved to **per-app grain** (`subject` carries `app_id`) everywhere (§Pillar 4, §9); the overloaded re-base cadence knob is **deconflicted by direction** (over-count is crash-tail-bounded, not cadence-bound; the cadence bounds only the under-enforce window; the provider is no longer read at all on re-base — §Pillar 5 SLO); the active-set uses `SCAN` (backed by `compio-redis`, no `sadd` in the driver); the changelog no longer under-counts the added mechanisms.

**No v5 win walked back — re-verified against code:** `spend.rs` `SpendThresholds{80/95/100/5}` + `derive_state` drive the separated states; the fold/fence/`stream_offsets`/`provider_forward_watermark` deletions and PG-off-the-per-event-path stance are unchanged. §6.2 forward+dedup and §6.3 CorrectionCapability + idempotency keys are intact.

---

## Changelog v6→v7

**An ENFORCEMENT SIMPLIFICATION round, driven by one owner decision: enforcement freshness is relaxed from sub-minute to a TUNABLE CADENCE (default HOURLY, tightenable e.g. to 15-min).** This is a *simplification*, not a hardening — v7 removes the sub-minute machinery v5/v6 had added rather than layering over it. The **BILLING half is UNTOUCHED**: provider delegation, §6.2 forward+dedup, §6.3 `CorrectionCapability`/`Backfiller`/signed `adjustment_note`/`(subject,period,correction_seq)` idempotency, the settle-window + terminal-period spec, the two registries, capability sub-traits, Lite, and PG-off-the-per-event-path all stay exactly as in v6. The v6 governing insight — **ENFORCEMENT CANNOT BE PROVIDER-SOURCED** — is preserved; enforcement is still LOCAL-per-app-sourced, now via a once-per-cadence batch instead of a continuous Redis counter.

**DELETED (the simplification — machinery that existed only to keep enforcement sub-minute):**
- **The shared Redis spend counter** and the forwarder's best-effort `INCRBY` — GONE. The forwarder now ONLY ships events to the provider + commits the Kafka offset (a pure ship-and-commit loop). §Pillar 4/7, §6.2.
- **The sub-minute evaluator** and the **conservative `MAX` re-base** (`counter = MAX(counter, recompute)` via Lua `eval`) — GONE. There is no ephemeral counter to evaluate or re-base; the periodic recompute IS the count. §Pillar 4/5.
- **`libs/compio-redis` leaves the enforcement path entirely** — one fewer moving part on the money path. §13, §9.
- **The sub-minute-only enforcement scaling OQs** — Redis-cluster sharding of the counter, the CHWBL-sharded sub-minute evaluator, the whale-partition-for-enforcement concern — REMOVED (whale partitioning remains a billing-throughput OQ-8 concern only). Residual folded into the relaxed OQ-7. §15.

**CHANGED (the new enforcement mechanism):**
- **Enforcement = a periodic batch at a tunable cadence (default 1h).** The **LOCAL per-app recompute** (`Σ` per `(subject=app, metric, period)` from the retained stream — the SAME batch that is §6.3's billing `witness`, one recompute two consumers) runs at the cadence, is priced (`charge_cents`/`derive_state`), and is written to `app_spend_state` (Postgres). The **gateway pulls `app_spend_state` (~5s) and enforces INSTANTLY at the edge** (`enforce::check_spend`): Warn notify · Degrade throttle · Block 402-before-dispatch. **Detection = the cadence; the enforcement ACTION stays instant.** §0, §Pillar 4/5.
- **ADDED — the throughput backstop.** Because detection lags by up to one cadence, a runaway app can overshoot by `R × cadence` (`R` = max spend rate). A **coarse per-app hard rate/concurrency cap at the gateway** (largely already `enforce::RateLimitRegistry`/`ConcurrencyRegistry`) caps `R`, so **worst-case overshoot ≤ R × cadence is bounded regardless of cadence**. The **free tier** (uncapped-by-card) gets a tighter cap. Both the **cadence** and the **per-app throughput cap** are tunable knobs. §0, §Pillar 4/5, §11.

**REFRAMED:**
- **The enforcement SLO** is now stated honestly as: detection latency = the cadence (default 1h); enforcement action instant at the gateway; worst-case overshoot = `R × cadence`, bounded by the throughput backstop. A runaway app is stopped within one cadence period, over-spending at most its capped rate × cadence. There is no over-count / under-enforce drift term any more (no counter to drift, no re-base). §Pillar 5, §15.
- **Scalability is now trivial.** A once-per-cadence O(apps) batch (default hourly, not 60×/min) is trivially tractable — shardable by app-id range, run once per cadence. The recompute fan-out (OQ-7) is now a once-per-cadence job, a major scalability simplification. §Pillar 5, OQ-7.

**Updated for coherence:** §0 (rewritten enforcement half + ASCII diagram — dropped the Redis counter, INCRBY, sub-minute evaluator, MAX-re-base; enforcement = periodic recompute → `app_spend_state` → gateway; added the throughput backstop), owner-locked decisions + §1–§3 (goals/locked decisions A2/A4), §Pillar 4/5 (rewritten around periodic-cadence recompute + gateway edge + throughput backstop), §Pillar 6 (`lite` enforcement note), §Pillar 7 (forwarder = ship + commit only), §6.1/§6.2/§6.3 (cross-references to the deleted counter/re-base re-pointed to the periodic recompute), §9 (removed the Redis-counter row; enforcement = periodic recompute + `app_spend_state`; no new per-event table), §11 (deleted the Redis-loss + drift rows; new "cadence gap = up-to `R×cadence` overshoot, bounded by the throughput backstop" + "control/batch outage" rows; enforcement is a stateless cron), §12.4 (live widget reads `app_spend_state`), §13 (compio-redis no longer needed for enforcement), §14 (scorecard rows 2/4/7/8/9/10), the rollout S6 slice (shrunk dramatically — a cron + the existing gateway pull + the throughput cap; no Redis-counter/re-base/sharded-evaluator work) + S4 (forwarder no INCRBY), §15 (OQ-2/OQ-7 relaxed, enforcement SLO risk reframed).

**No billing-correctness regression — re-verified against code:** the gateway already pulls `RouteEntry.spend_state` at `poll_interval_secs` and enforces at dispatch via `enforce::check_spend` (`crates/zeroship-gateway/src/enforce.rs`, `router/dispatch.rs`); `spend.rs::evaluate_all` already prices per-app usage and writes `app_spend_state` (the v7 cadence batch is that, sourced from the stream recompute); the gateway already has `RateLimitRegistry`/`ConcurrencyRegistry` for the throughput backstop. The provider-side exactly-once boundary (§6.2 forward + dedup) and the correction spine (§6.3) are untouched. The design is simpler and CONVERGED.
