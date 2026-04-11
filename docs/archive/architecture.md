# Appbase Platform Architecture

> **Last updated:** 2026-03-31 | Internal technical reference

---

## Crate Map

| Crate | Purpose | Key Dependencies | LOC |
|---|---|---|---|
| `appbase-plan` | Quota plan definitions, policies, period types | serde | ~200 |
| `appbase-core` | Interfaces: Plugin trait, config, event log, meter store traits | deno_core, serde, toml | ~550 |
| `appbase-compiler` | SWC-based JSX/TS compiler (lib + `appbase-compile` binary) | swc_core, serde | ~1250 |
| `appbase-isolate` | V8 isolate actor model, pool, CPU metering | appbase-core, deno_core, tokio | ~730 |
| `appbase-plugins` | Built-in plugins: db (SQLite), kv, env | appbase-core, deno_core, rusqlite | ~500 |
| `appbase-billing` | Pricing table, spending limits, SpendingReconciler | appbase-core, serde, tokio | ~560 |
| `appbase-metering` | Atomic counters, flush loop, period rollover, event log | appbase-core, appbase-billing, appbase-plan, rusqlite, tokio | ~1900 |
| `appbase-enforcement` | Rate limiter, quota enforcer, concurrency guard, error codes | appbase-metering, appbase-plan, serde | ~540 |
| `appbase-server` | Axum HTTP router, Tower middleware, multi-tenant routing | appbase-core, appbase-isolate, appbase-metering, appbase-enforcement, appbase-plan, appbase-billing, axum, tower-http | ~570 |
| `appbase` (cli) | CLI binary: `serve`, `dev`, `compile` commands | appbase-core, appbase-isolate, appbase-metering, appbase-enforcement, appbase-plan, appbase-plugins, appbase-server, appbase-billing, tokio | ~230 |

**Total:** ~7,030 LOC Rust

---

## Dependency Graph (Current)

```
                    appbase-plan (no deps)
                   /       |         \
                  /        |          \
  appbase-core  /   appbase-billing    \
   (deno_core) /     (core, plan?)      \
    |    |    /          |               |
    |    |   /           |               |
    |  appbase-metering -+               |
    |    (core, billing, plan)           |
    |        |                           |
    |   appbase-enforcement              |
    |     (metering, plan)               |
    |        |                           |
    |  appbase-isolate   appbase-plugins |
    |     (core)            (core)       |
    |        \               /           |
    |         \             /            |
    |     appbase-server ---------------+
    |       (core, isolate, metering, enforcement, plan, billing)
    |                |
    +--------  appbase (cli)
           (all crates)
```

Key edges:
- `metering -> billing`: metering reads SpendAction flags; billing sets them
- `metering -> core`: uses Plugin/PluginMeter trait, config types
- `enforcement -> metering`: reads counter values for quota decisions
- `enforcement -> plan`: reads plan definitions for limits

---

## Request Lifecycle

An HTTP request flows through these steps in `crates/server/src/router.rs`:

```
1. HTTP POST /rpc/:app_id/:function  (axum router)
2. Rate limit check            [enforcement::rate_limit::RateLimiter]
   - Token bucket CAS on packed AtomicU64 (~100ns)
   - Fail: 429 + RateLimit headers
3. Concurrency guard acquire   [enforcement::concurrency::ConcurrencyGuard]
   - AtomicU32 CAS loop (~50ns)
   - Fail: 429 + Retry-After
4. SpendAction flag check      [billing::spend_action::SpendAction]
   - Single AtomicU8 load (~1ns)
   - If blocked/degraded: 429 or degraded execution
5. Quota enforcement           [enforcement::quota::check()]
   - Reads metering counters (Acquire loads)
   - Compares against plan limits -> Allow/Warn/Deny
   - Fail: 429 + X-Quota-* headers
6. V8 dispatch                 [isolate::pool::IsolatePool]
   - Isolate actor receives request, runs JS
   - CPU/wall watchdog enforces time limits
7. Plugin ops record usage     [plugins -> core::plugin::PluginMeter]
   - e.g., db.read increments counter via PluginMeter::increment()
   - Name-based HashMap lookup (~30ns), acceptable for I/O-bound ops
8. Core usage recorded         [metering::meter::MeterRegistry]
   - request count, cpu_us via ResourceHandle (O(1), ~3ns)
9. Event enqueued              [metering::event_channel::EventSender]
   - UsageDelta sent to bounded mpsc channel (30K capacity)
   - Dropped on backpressure (counters are source of truth)
10. Response with headers
    - X-Quota-Remaining, X-Quota-Limit, RateLimit, X-CPU-Budget-Remaining
    - ConcurrencyGuard dropped (AtomicU32 decrement)
```

**Total overhead: < 3 us** (V8 dispatch: 50-500 us; JS execution: 1-100 ms)

---

## Background Services

| Service | Interval | What It Does | Crate |
|---|---|---|---|
| UsageFlusher | 5s | Snapshots atomic counters, batch-upserts to MeterStore (warm tier) | metering |
| EventLogger | continuous | Batch-writes event log entries from mpsc channel to append-only file | metering |
| PeriodRoller | on schedule | Double-buffered period rollover: swap counters, drain, snapshot, archive | metering |
| SpendingReconciler | 10s | Reads metering snapshots, applies PricingTable, sets SpendAction flags | billing |
| TrialManager | 60s | Manages trial period expirations and plan transitions | metering |
| MemoryWatchdog | 5s | Monitors RSS, triggers graceful degradation (dedup shrink, event log disable) | server |
| StorageSampler | 60s | Samples disk usage for storage metering | metering |

**Ordering constraint:** PeriodRoller and TrialManager share a Mutex; PeriodRoller has priority.

---

## Three-Tier Metering Pipeline

```
HOT (in-memory, nanoseconds)
  AtomicU64 counters per app per resource
  CounterRegistry: O(1) index via ResourceHandle (core resources)
                   HashMap lookup via name (plugin resources)
  Token buckets: packed AtomicU64 (tokens + timestamp)
  Concurrency gauges: AtomicU32
  SpendAction flags: AtomicU8 per app
       |
       | UsageFlusher (every 5s, batch upsert)
       v
WARM (durable store, milliseconds)
  MeterStore trait with adapters:
    SqliteMeterStore (default) - WAL mode, INSERT ON CONFLICT
    RedisMeterStore - HINCRBY pipeline
    PostgresMeterStore - batched upserts
    MmapMeterStore - direct memory-mapped writes
    MemoryMeterStore - testing only
       |
       | PeriodRoller (at period end, snapshot + archive)
       v
COLD (append-only, archival)
  Event log: append-only file, externally anchored
  Usage history: per-period snapshots in MeterStore
  Billing ledger: spend per app per period (millicents)
```

**Data loss window:** max 5 seconds (last flush to crash). Recoverable via event log replay.

---

## Billing / Quota Separation

| Concern | Owner | Hot Path? | Data |
|---|---|---|---|
| Plan definitions (limits, periods, tiers) | `appbase-plan` | read-only | QuotaPlan, PolicyAction |
| Counter increment, flush, rollover | `appbase-metering` | yes (atomic adds) | CounterRegistry, MeterStore |
| Allow/warn/deny decisions | `appbase-enforcement` | yes (atomic reads) | QuotaDecision |
| Rate limiting, concurrency control | `appbase-enforcement` | yes (CAS) | TokenBucket, ConcurrencyGuard |
| Pricing, cost computation | `appbase-billing` | **no** (background) | PricingTable |
| Spending limits, budget alerts | `appbase-billing` | **no** (background) | SpendingReconciler |
| spend_action flag (bridge) | `appbase-billing` (write) / `appbase-enforcement` (read) | 1 atomic load | AtomicU8 |

**Principle:** No pricing math on the request hot path. Billing reads metering snapshots asynchronously.

---

## Plugin Metering Protocol

1. **Declaration:** Plugin implements `meter_resources() -> Vec<MeterResource>`:
   ```rust
   MeterResource { name: "db.reads", unit: "ops", aggregation: Sum, category: "db" }
   ```

2. **Registration:** At startup, CounterRegistry collects all MeterResource descriptors
   from all plugins. Each gets an atomic counter slot.

3. **Recording:** During request execution, plugin ops call:
   ```rust
   ctx.meter.increment("db.reads", 1);  // PluginMeter trait, ~30ns HashMap lookup
   ```

4. **Flow:** `Plugin::meter_resources()` -> `CounterRegistry` -> `PluginMeter::increment()` -> atomic counter -> UsageFlusher -> MeterStore

Core resources (requests, cpu_us) use `ResourceHandle` for O(1) direct index access (~3ns).

---

## Key Design Decisions

| Decision | Rationale | Alternative Considered |
|---|---|---|
| deno_core for V8 bindings | Battle-tested FFI, op dispatch, snapshot support | Raw rusty_v8 (too low-level) |
| SWC for compilation | Fast Rust-native JSX/TS transform | Babel (Node.js, slower) |
| Atomic counters (not DB writes) on hot path | Sub-microsecond enforcement | Direct SQLite writes (~1ms) |
| Separate billing from metering | No pricing math on hot path; clean crate boundary | Inline spend tracking (v2.1, removed) |
| Double-buffered period rollover | Zero-downtime counter swap | Lock-based rollover (causes 503s) |
| Plugin-declared dynamic meters | Extensible without core changes | Hardcoded db/kv counters |
| SpendAction as AtomicU8 bridge | 1ns hot-path cost for spending limit enforcement | Query billing on every request |
| MeterStore trait | Pluggable warm-tier storage | Hardcoded SQLite |
| drain-and-wait for rollover | Simpler + more correct than epoch reclamation | crossbeam-epoch (doesn't block readers) |

---

## Known Issues / Tech Debt

1. **[HIGH] `appbase-core` depends on `deno_core`** -- Core should be interface-only, but
   `Plugin::ops()` returns `deno_core::OpDecl`. Need a `core-types` crate without V8 deps
   or a type-erased op registration.

2. **[HIGH] `enforcement -> metering` creates tight coupling** -- Enforcement reads counters
   directly from metering. Target: enforcement depends only on plan + a trait from core.

3. **[HIGH] `metering -> billing` cycle risk** -- Metering depends on billing for SpendAction
   types. Should be broken by moving SpendAction into core or plan.

4. **[MED] No `core-types` crate** -- Plan, config, and shared types are split across core
   and plan. A dedicated types crate would clean up the dependency graph.

5. **[MED] Metering crate is too large (~1900 LOC)** -- Contains counters, flusher, rollover,
   event log, store, config. Could be split into metering-core + metering-store.

6. **[MED] Plugin features not fully gated** -- `appbase-plugins` has feature flags for
   db/kv/env but rusqlite is only optional for db. KV and env have no optional deps to gate.

7. **[LOW] Compiler crate is standalone** -- No integration with the metering/quota system.
   Fine for now but may need to participate in build-time metering later.

8. **[LOW] CLI wires all crates directly** -- Could use a facade crate to reduce import surface.

---

## Target Architecture

```
appbase-core-types          (serde only, no deno_core)
  - QuotaPlan, PolicyAction, SpendAction, MeterResource
  - PluginMeter trait, MeterStore trait
  - Config types, event log types

appbase-plan                (core-types only)
  - Plan parsing, validation

appbase-core                (core-types + deno_core)
  - Plugin trait (ops, js_bridge, init)
  - V8-specific types

appbase-billing             (core-types only)
  - PricingTable, SpendingReconciler
  - Reads snapshots, writes SpendAction via trait

appbase-metering            (core-types only, no billing dep)
  - CounterRegistry, MeterStore adapters
  - UsageFlusher, PeriodRoller, EventLogger

appbase-enforcement         (core-types + plan only, no metering dep)
  - Reads counters via trait (not direct metering import)
  - Rate limiter, concurrency guard, quota enforcer

appbase-isolate             (core + core-types)
appbase-plugins             (core + core-types)
appbase-server              (all, wiring layer)
appbase (cli)               (all, entry point)
```

Key improvements:
- `core-types` breaks the deno_core dependency for non-V8 crates
- `enforcement` depends on traits, not concrete metering implementation
- `metering` no longer imports billing; SpendAction lives in core-types
- Clean DAG with no cycles or unnecessary coupling
