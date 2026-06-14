# Metering-coverage gap (#27 / G4) — design

**Status:** DESIGN ONLY — no implementation, no code, no DB, no git. ·
**Date:** 2026-06-14 · **Worktree:** `appbase-billing` @ `feat/billing-metering` ·
**Latest changeset:** `0046` (new weights land in `0047`).

This is the deep, decision-resolving design for the **metering-coverage gap**
(G4 in `docs/proposals/2026-06-14-billing-gap-closure-roadmap.md`). The roadmap
sketches G4a/G4b/G4c at a paragraph each; this doc resolves the *open* design
questions the roadmap deferred: the gateway-egress producer model (the #1
unknown), the long-lived-connection metric model and recording point, the
cpu/db-kv-bytes scope decision, and the exact new metrics → weights → pricing
flow. It composes with the **schema redesign**
(`2026-06-14-billing-schema-redesign.md`) but, per the dependency matrix,
**every part of G4 is schema-independent**: new metric names ride
`AppUsage.custom` and `usage_aggregates` raw rows — no per-metric DDL. The
*only* DB change is new `metric_weights` seed rows (one changeset).

It applies the same rigor as the built metering: restart-safe producer ids
(the boot-nonce dedup lesson), idempotent ingest, server-attributed `app_id`,
and faithful TDD (real path, no shims).

---

## 0. Grounding — what is built today (read from this worktree)

The current meter is **per-worker, per-request**:

- **Worker producer** (`crates/worker/src/handler.rs::dispatch`): starts a wall
  clock + samples `CLOCK_THREAD_CPUTIME_ID` around the synchronous V8 entry,
  then calls `cache::record_request(app_id, cpu_us, wall_us, egress, ingress)`
  once per dispatch — five fixed counters: `requests`, `cpu_us`, `wall_us`,
  `egress_bytes`, `ingress_bytes`. For a **buffered** response egress is the
  body length recorded inline; for a **streaming** response (SSE) egress and
  the final `wall_us` are deferred into the drain task and landed once at
  stream finalize via `metering_on_complete` (`handler.rs:355-365`). A
  **WebSocket upgrade over HTTP dispatch is rejected 500** (`handler.rs:296`).
- **Data-primitive producers** (`plugin-db`/`plugin-kv`/`plugin-storage`):
  emit raw metrics (`db_reads`, `db_writes`, `kv_reads`, …, `storage_ops`,
  `storage_bytes`, `storage_egress_bytes`) at the op boundary, success-arm
  only, via a per-app `MeterHandle` (`crates/metering/src/lib.rs`).
- **Meter core** (`crates/metering/src/meter.rs`): a process-wide `Arc<Meter>`
  with atomic per-`(app_id,metric)` counters + a locked `custom` map for
  open-set metric names; `drain()` snapshots-and-zeroes.
- **Flush** (`crates/metering/src/flush.rs`): one compio task per process
  drains every ~10s, builds a `UsageReport { worker_id, report_id, sequence,
  counters }`, POSTs to control `/internal/usage`; on failure `merge`s the
  snapshot back (at-least-once). `worker_id = boot_worker_id($HOSTNAME)` folds
  a per-boot nonce so the per-process `SequenceSource` (resets to 1 each boot)
  cannot collide with pre-restart `(worker_id, sequence)` rows.
- **Ingest** (`crates/control/src/metering/mod.rs`): idempotent dedup on
  `(worker_id, sequence)` in `zeroship.usage_reports_seen` (`0037`), then UPSERT
  `total += delta` per `(app_id, period_start, metric)` into
  `zeroship.usage_aggregates`. The five fixed counters **and** every `custom`
  metric aggregate identically (`usage_deltas`). `u64→i64` overflow is
  skip-with-warn, never wrap-negative.
- **Pricing** (`metric_weights` / `pricing_config`, `0041`): `usage_aggregates`
  is raw-metric-keyed; CU are derived at pricing time as
  `total × units_per_op ÷ per_units`, then `× fx ÷ 10^12` to cents. **An
  unweighted metric bills $0** (no `metric_weights` row ⇒ 0 CU). A re-weight
  reprices history without touching stored usage.
- **Gateway** (`crates/gateway/`): **owns no meter** and **does not depend on
  the metering crate** (confirmed: no `metering` in `gateway/Cargo.toml`, no
  `Meter` in `gateway/src/main.rs`). It serves static assets
  (`router/static_serve.rs`), redirects, and proxies worker dispatch
  (`router/proxy.rs`) — all egress it emits directly is **wholly unmetered**.

### The three holes

| # | Hole | Today | Consequence |
| --- | --- | --- | --- |
| **H1** | Long-lived conns (WS / SSE / streaming) | SSE bills once at finalize; WS over dispatch errors 500; a multi-hour stream accrues nothing until close (and is lost on crash) | wall/duration + sustained bytes + message counts under-counted |
| **H2** | Gateway / static-asset egress | Gateway emits asset/redirect/proxy bytes with **no meter at all** | asset-heavy apps (SPAs, media) serve free egress the platform eats |
| **H3** | `cpu_us` sync-only + db/kv byte metrics | `thread_cpu_time` captured once around the sync V8 entry; async-continuation CPU unattributed; `db_*`/`kv_*` byte volume not emitted (no weight) | `cpu_us` is a documented lower bound; data-byte volume invisible |

All three under-bill, which is **customer-favourable** — none is a financial-loss
risk to the platform's *spend caps* (those read the same `usage_aggregates`, so
they simply cap on less). That is why the roadmap sequences G4 after the
revenue-*protecting* gaps. This design closes H1 (SSE/streaming now; WS deferred
to the WS-transport epic) and H2 (the real unknown), and makes a reasoned
**defer** call on H3.

---

## 1. The composition contract (what must NOT change)

Every option below preserves:

1. **One ingest sink.** All producers POST `UsageReport` to control
   `/internal/usage`; dedup is `(worker_id, sequence)`; aggregation is
   `(app_id, period_start, metric)`. **No new endpoint, no new table, no new
   sink** — the gateway and the streaming path are just *more producers* of the
   same wire type.
2. **Raw-metric-keyed aggregation.** New metrics are new *names* in
   `AppUsage.custom`; `usage_deltas` already aggregates the custom map. No DDL
   per metric.
3. **Pricing unchanged.** A new metric bills $0 until a `metric_weights` row
   exists; once seeded it flows through `total_units`/`charge_cents` unchanged.
4. **Server-attributed `app_id`.** The producer (worker via APP_ID; gateway via
   the resolved route) derives `app_id` server-side — never a client value.
5. **Spend enforcement untouched.** `spend.rs`/`enforce.rs` read
   `usage_aggregates`; more metrics there means the spend total simply
   reflects more usage. The providers (`metering/provider/*`) export the same
   raw rows. **No code in the spend/provider/enforce path changes.**
6. **Restart-safe producer ids + idempotency** (the boot-nonce lesson) apply to
   the new gateway producer exactly as to the worker.
7. **Zero tokio** — compio intervals, `cyper` HTTP, `compio-postgres`.

---

## 2. H2 — Gateway egress metering (the #1 unknown) — RESOLVED

### 2.1 The three options

**(a) Gateway gets its own `Meter` + flush — a second metering producer.**
The gateway depends on the metering crate, builds one process-wide
`Arc<Meter>`, accumulates per-app egress at each response it emits directly
(static, redirect, and — see §2.4 — the worker-proxy body it forwards), and
spawns the **same** `spawn_flush_task` with a gateway-flavoured producer id.
Control's `/internal/usage` ingest is reused verbatim.

**(b) Attribute gateway egress back through the worker.** The gateway would
have to round-trip an out-of-band "charge app X for N egress bytes" message to
*a* worker (which one? the gateway picks a worker per request, but static
assets never touch a worker), and the worker would fold it into its meter. This
invents a gateway→worker control channel that does not exist, picks an
arbitrary worker to attribute to, and couples two services that are
deliberately decoupled.

**(c) Gateway reports egress via a NEW internal endpoint.** A bespoke
`/internal/gateway-egress` on control with its own dedup/aggregate logic —
duplicating the entire idempotent ingest path for one metric.

### 2.2 Recommendation: **(a)** — the gateway is a first-class metering producer.

(b) violates "the gateway is dumb" the *wrong* way (it adds gateway→worker
coupling) and has no sound attribution for asset egress (no worker is
involved). (c) duplicates a hardened, idempotent ingest path. (a) reuses the
*entire* existing pipeline — `Meter`, `drain`, `build_report`,
`spawn_flush_task`, `post_report`, and control's idempotent `ingest` — by
adding the gateway as a second producer. It is the least new surface and the
most faithful to the built design. **The metering crate is already
service-agnostic** (`lib.rs`: "no V8 … depends only on `zeroship-core` +
`compio` + `cyper` + `uuid`"), so the gateway can take it as a dependency with
zero new abstraction.

### 2.3 Producer-id / dedup / restart-safety (the boot-nonce lesson)

The gateway flush MUST be restart-safe under the **same** invariant the worker
proved: the per-process `SequenceSource` resets to 1 every boot, so the
`worker_id` (the dedup key's free-text component, `TEXT` in
`usage_reports_seen`) MUST change on every restart or post-restart sequences
collide with pre-restart rows and get dropped as phantom duplicates (silent
under-bill).

- **Producer id:** `boot_worker_id("gate-<base>")` where `<base>` is
  `$HOSTNAME` (k8s/compose) else the gateway bind addr — **reusing
  `crate::flush::boot_worker_id` unchanged** (it already folds a per-boot
  UUID nonce). The `gate-` prefix is purely cosmetic/observability; the nonce
  is what makes it restart-unique. The `worker_id` column is free-text, so no
  schema change — exactly as the worker's identity needed none.
- **Sequence:** a gateway-local `SequenceSource` (the existing type), per
  process, monotonic from 1.
- **Dedup:** `(producer_id, sequence)` is unique per gateway boot ⇒ a
  re-transmitted in-flight report (same process, same nonce) dedups
  exactly-once; a fresh boot's sequence-1 lands under a fresh key. **Identical
  to the worker.** Distinct gateway and worker producer ids never collide
  (gateway ids carry the `gate-` prefix and a different nonce), so a gateway
  report and a worker report for the same app simply *sum* in
  `usage_aggregates` — which is the intent.

### 2.4 Which metric(s) — and the no-double-counting rule (load-bearing)

**Decision: a NEW metric `egress_bytes` is NOT reused at the gateway. The
gateway emits a distinct metric so ownership is unambiguous, and the
double-count question is answered by metric-name partition, not by
coordination.**

The gateway emits egress in two situations:

1. **Gateway-originated egress** (static assets, redirects) — bytes the
   *worker never sees*. There is no worker `egress_bytes` for these. → metric
   **`gateway_egress_bytes`**.
2. **Worker-proxied egress** (the worker's response body the gateway forwards
   to the client) — the worker **already** counts these as `egress_bytes` in
   `record_request`/`metering_on_complete`. If the gateway *also* counted the
   proxied body, the same bytes bill twice.

> **Scope (deliberate): `gateway_egress_bytes` covers static + redirect
> response bodies only.** Gateway-emitted error / 4xx / 5xx / 204 envelopes
> (404 no-route / in-tree, 400 non-canonical path, 402 ACCOUNT_SUSPENDED /
> SPEND_LIMIT, 403 CSRF, 405, 413, 426, insufficient-scope, CORS-preflight 204,
> etc.) are **platform overhead and are deliberately NOT billed.** They are
> tiny and frequently attacker-driven (probes, rate-limit hits, spoof attempts)
> rather than legitimate end-user delivery, so metering them would charge a
> creator for traffic they neither served nor wanted. The static arm's OWN
> 404/503 bodies (a matched static resource whose blob is missing/unavailable)
> *are* billed — they are a consequence of serving that app's asset map, not a
> gateway-edge reject — and are counted by their known size at step 8b.

**The ownership rule (no double-count):**

> **`egress_bytes` is owned by the WORKER.** The gateway counts ONLY
> egress the worker did not — and emits it under the distinct name
> **`gateway_egress_bytes`**. The gateway NEVER increments `egress_bytes`,
> and NEVER counts the worker-proxy response body.

Concretely, in `execute_resource_tree`'s action match
(`router/dispatch.rs:806`):

- `ResolvedAction::Static { .. }` (→ `serve_resource_tree_static`) → meter the
  served body length as `gateway_egress_bytes`. Buffered bodies (and the static
  arm's own 404/503 error bodies) are recorded by their fully-delivered
  `BodySize::Sized` at step 8b; the streamed-static (`SizedStream`) path meters
  DELIVERED bytes inside its drain so a client disconnect bills only what was
  written, not the intended asset length (see §2.5).
- `ResolvedAction::Redirect { .. }` → meter the (small) redirect response body
  as `gateway_egress_bytes` (a redirect is cheap but non-zero; including it
  keeps the rule simple — "gateway-emitted *successful* bytes are
  gateway-owned").
- `ResolvedAction::WorkerRpc | WorkerSsr | Rewrite` (→ `handle_dispatch`) → the
  gateway does **NOT** meter the body. The worker already counted it as
  `egress_bytes`. (The gateway adds only a few framing/header bytes, which we
  deliberately do not bill — they are platform overhead, not app egress.)
- **Gateway-edge error/4xx/5xx/204 envelopes** (the early-return arms above the
  action match — auth, CSRF, max-input, method/upgrade gates, account/spend
  gates, non-canonical path, no-route 404, CORS preflight) → **NOT metered.**
  Platform overhead, per the scope note above.

This makes the partition **structural**: `egress_bytes` and
`gateway_egress_bytes` are disjoint by construction (worker-body vs
gateway-body), so the two producers can never count the same byte. Both flow
through the same `usage_aggregates`; the creator's "total egress" is the sum of
the two metrics at read/pricing time, and spend enforcement caps on the sum
automatically (it reads all rows for the app/period).

> **Rejected alternative — merge into one `egress_bytes`.** Tempting (one
> "egress" line on the bill), but it forces the gateway and worker to *not*
> overlap on the *same metric name* across a service boundary, which is exactly
> the coordination this design avoids. Keeping a distinct name makes the
> non-overlap a property of the *names*, observable in `usage_aggregates`, and
> lets the operator weight gateway egress differently from worker egress if CDN
> economics ever differ. The pricing layer can still present them as one line.

### 2.5 Where the bytes are counted (the recording point)

Static/streaming static responses can themselves be large and streamed
(`serve_static_streaming` in `static_serve.rs`). So the gateway recording point
must mirror the worker's buffered-vs-streamed split:

- **Buffered static / redirect** → the body length is known when the
  `HttpResponse` is built; record `gateway_egress_bytes` inline at that point
  (a synchronous `meter.increment(app_id, "gateway_egress_bytes", n)`). The
  static arm's own 404/503 bodies (matched-resource-but-missing-blob) ride this
  same buffered path — they are app-asset-serving overhead, not a gateway-edge
  reject, so they are billed by their known size. (Gateway-edge error/4xx/5xx/
  204 envelopes are NOT billed — §2.4 scope note.)
- **Streamed static** (`serve_static_streaming`, range/large assets) → the
  gateway already spawns a drain; the drain meters the bytes ACTUALLY DELIVERED
  to the client (incremental accrual flushed every ~1 MiB, plus a final delta
  on completion / client disconnect), NOT the intended asset size recorded up
  front. A client aborting a large-asset download is therefore billed ~what was
  delivered, not the whole file — the over-bill-on-disconnect fix. This mirrors
  the worker's `stream_response` drain (`handler.rs:410`). The streamed response
  carries an internal marker so the step-8b size record skips it (no
  double-count with the drain's delivered-bytes accounting).

The `app_id` is the **route's** app_id — `execute_resource_tree` already holds
`app_id: &Uuid` resolved server-side from `lookup_by_name` (§route resolution),
never a client value. This satisfies the server-attributed-app_id invariant
trivially.

### 2.6 New code (H2)

- **`crates/gateway/Cargo.toml`** — add `zeroship-metering` dependency.
- **`crates/gateway/src/main.rs`** — build `Arc<Meter>`; compute
  `producer_id = boot_worker_id("gate-<HOSTNAME|bind>")`; `spawn_flush_task`
  with a `FlushConfig` pointing at the same `control_url` / `control_key`.
  Store the `Arc<Meter>` in `GateState` (new field).
- **`crates/gateway/src/router/dispatch.rs`** — in `execute_resource_tree`,
  after the Static/Redirect actions produce a response, record
  `gateway_egress_bytes` against `state.meter` for the route's `app_id`. For
  the streamed-static path, thread the meter into the drain task (mirroring
  `handler.rs::stream_response`'s `on_complete`).
- **`crates/gateway/src/router/static_serve.rs`** — the streamed-static drain
  accumulates and reports bytes on finalize (a small `on_complete`-style hook,
  same shape as the worker).
- **No control-side change** — `/internal/usage` ingest already handles any
  `custom` metric and any `worker_id` string.

### 2.7 Why spend/providers stay untouched (H2)

`gateway_egress_bytes` is just another `custom` metric row in
`usage_aggregates`. `spend.rs::derive_state` sums an app's CU across all its
metric rows (via the weights) — adding a weighted `gateway_egress_bytes` raises
the same total it already computes; no spend code changes. The providers
(`native`/`stripe_meters`/`openmeter`) export `usage_aggregates` rows verbatim;
a new metric name flows through with no provider change.

---

## 3. H1 — Long-lived connections (SSE / streaming; WS deferred) — RESOLVED

### 3.1 The metric model

A long-lived connection has three billable dimensions. We model each as a
distinct metric so weights can price them independently, and so a connection
that is *idle-but-open* (holding a worker slot) still accrues *duration*:

| Dimension | Metric | Applies to |
| --- | --- | --- |
| Connection duration (wall) | **`stream_wall_us`** (SSE/streaming); **`ws_conn_us`** (WS, deferred) | the open lifetime of the connection |
| Bytes streamed to client | `egress_bytes` (existing — worker-owned) | every byte the worker streams out |
| Bytes received from client | `ingress_bytes` (existing) | WS inbound frames (deferred); SSE has none after open |
| Message/frame count | **`ws_messages`** (WS, deferred) | per-frame counting (WS only) |

**Decision: SSE/streaming reuses `egress_bytes` (it IS worker egress) but adds a
new `stream_wall_us` for the *sustained duration* the current single `wall_us`
under-counts.** Rationale: today `wall_us` is recorded once and *does* span the
stream lifetime via `metering_on_complete` (the wall clock starts at dispatch
and is read at finalize) — but only on a clean finalize, and only once at the
end, so a multi-hour stream contributes nothing until close and **loses
everything on crash**. `stream_wall_us` is the *incrementally-flushed* duration
(§3.3) so duration accrues continuously and is crash-bounded. We keep
`egress_bytes` for the streamed bytes (no new name — they are ordinary worker
egress; the existing weight already prices them).

> **Why a separate `stream_wall_us` instead of just fixing `wall_us`?** Two
> reasons. (1) Pricing flexibility: a held-open idle stream (a worker slot
> occupied with little CPU/egress) is a real cost the operator may want to price
> differently from a fast unary request's wall time. (2) Observability: keeping
> the long-lived duration in its own metric makes "how much wall time is in
> long-lived connections" directly queryable in `usage_aggregates`. The unary
> `wall_us` semantics stay exactly as built.

### 3.2 WebSocket — DEFER (documented bound), not silent under-bill

WS is **not a first-class worker transport** today: an upgrade over HTTP
dispatch is rejected with 500 (`handler.rs:296`), and the gateway's
subscription proxy is a 501 stub (`handle_subscription_dispatch`). So WS
**cannot silently under-bill — it does not run.** Metering WS is gated on the
WS-transport epic (it needs a per-connection accumulator on the worker's WS
state — `WebSocketState` in `runtime/src/core/state.rs` — to count frames +
bytes + duration). This design **reserves the metric names** (`ws_conn_us`,
`ws_messages`, `ws_ingress_bytes`/`ws_egress_bytes`) and the weights (§5) so the
WS epic only wires the emit points, but does **not** implement WS metering here.
The bound is explicit: *until WS lands, WS traffic is zero (it errors), not
under-counted.*

### 3.3 The recording point — incremental flush (the load-bearing decision)

`record_request` fires once at dispatch end. A long-lived connection needs
*periodic* recording so (a) usage accrues continuously and (b) a crash loses
only the last interval, not the whole connection. The drain task in
`stream_response` (`handler.rs:410`) is the natural home: it already runs for
the stream's lifetime and is woken by the stream writer.

**Design:** the streaming drain accumulates `egress` (already does) plus tracks
`last_flush = Instant`. On each wakeup, if `bytes_since_flush ≥ FLUSH_BYTES`
**or** `now - last_flush ≥ FLUSH_INTERVAL`, it records a *delta*:

- `meter.increment(app_id, "egress_bytes", bytes_since_flush)`
- `meter.increment(app_id, "stream_wall_us", micros_since_flush)`

…and resets the running deltas. On finalize/disconnect it flushes the final
delta. **`requests` is still counted exactly once** at dispatch (a long stream
is one request), so the incremental path increments only the byte/duration
metrics — never `requests`.

Thresholds (config, not estimates — defaulted, tunable): `FLUSH_INTERVAL ≈ 10s`
(aligns with the meter flush cadence so a delta is rarely stranded in-memory
more than one flush) and `FLUSH_BYTES ≈ 1 MiB` (bounds in-memory un-recorded
egress). These are *recording-cadence* knobs, distinct from the *flush-to-
control* cadence; they only affect crash-loss granularity.

**Crash-loss bound (honest, not a durability guarantee):** a worker crash loses
at most one recording-interval's delta per in-flight stream (≤ `FLUSH_INTERVAL`
of duration and ≤ `FLUSH_BYTES` of egress). This is strictly better than today
(lose the *whole* stream on crash) and matches the roadmap's hardening-backlog
(a) note that full crash-durability is deferred until a durability SLA exists.

### 3.4 New code (H1)

- **`crates/worker/src/handler.rs`** — `stream_response`'s drain loop gains the
  incremental-flush logic (delta `egress_bytes` + `stream_wall_us` on
  interval/byte threshold; final delta on finalize). `metering_on_complete`
  becomes the *final-delta* recorder rather than the *only* recorder.
  `record_request` for the non-stream path is unchanged.
- **`crates/metering/src/meter.rs`** — no change required: `stream_wall_us` is a
  `custom` metric, handled by the open-set path. (If desired it can be promoted
  to a fixed atomic later; not necessary for correctness.)
- **No gateway change for SSE** — SSE flows through the worker; the gateway just
  proxies the stream (and, per §2.4, does NOT meter the proxied body).
- **WS:** no code now (deferred); metric names + weights reserved.

---

## 4. H3 — `cpu_us` full attribution + db/kv byte metrics — DEFER (with bounds)

### 4.1 `cpu_us` async-continuation attribution — DEFER

Today `cpu_us` samples `CLOCK_THREAD_CPUTIME_ID` around the **synchronous** V8
entry only (`handler.rs:253-270`). A `Pending` handler's async continuation runs
on the shared V8 actor thread via the pump and is **not attributable to this
request without a per-request CPU accumulator the kernel does not expose**
(the handler's own comment says exactly this). Closing it faithfully requires a
**runtime kernel change**: a per-request CPU accumulator that the pump
charges as it runs each request's continuations — a non-trivial change to the
V8 actor/pump (`crates/runtime/`), touching the hot path.

**Recommendation: DEFER, with the documented bound already in place.** The
current `cpu_us` is a *faithful lower bound* (the comment is explicit and
non-fabricated — it does not pretend to be total CPU). Per
`feedback_never_estimate`, we do not guess what fraction is missing; we state
the bound: **`cpu_us` undercounts by the async-continuation CPU, which is
unattributed.** Since `cpu_us` is weighted modestly (1 CU/ms, dominated by
`requests` at 1 CU each — see `0041` seed) and under-billing is
customer-favourable, the kernel-accumulator work is not justified ahead of the
revenue-protecting gaps. It becomes worthwhile only if/when CPU-heavy async
workloads dominate a plan's cost — a *measured* trigger, not a speculative one.

> **If undertaken later:** the accumulator lives on the runtime's per-request
> `RequestCtx`; the pump samples thread CPU before/after each continuation slice
> and adds the delta to the ctx; the worker reads the ctx total at finalize
> instead of the single sync delta. This is the same delta-capture-at-each-flush
> idea §3.3 uses for streaming, applied at continuation granularity.

### 4.2 db/kv byte metrics — DEFER (weight-ready)

`plugin-db`/`plugin-kv` emit op-count metrics (`db_reads`/`db_writes`/
`kv_reads`/`kv_writes`) and row counts (`db_rows_written`), but **not byte
volume**. `storage` already emits `storage_bytes`/`storage_egress_bytes`, so the
*pattern* exists. Adding `db_bytes`/`kv_bytes` is a small per-primitive change
(measure the serialized payload at the op boundary, emit on the success arm via
the existing `MeterHandle`).

**Recommendation: DEFER, but it is the cheapest of the three to close** (no
kernel work, no new service, mirrors `storage_bytes`). Defer because: (a) op
counts already capture the dominant db/kv cost signal (the `0041` seed weights
`db_writes` 2× `db_reads`, etc.); (b) byte volume is a *secondary* signal whose
weight would be sub-unit anyway; (c) it adds emit points to two hot
primitives for marginal billing fidelity. **Reserve the names** (`db_bytes`,
`kv_bytes`) and seed **zero-weight-by-omission** (no `metric_weights` row ⇒ $0)
so they can be turned on later by adding a weight row + the emit point in one
slice. The roadmap already lists this under "db/kv byte metrics deferred
(0-weight)" — this design confirms the defer and the turn-on path.

---

## 5. New metrics → weights → pricing (changeset `0047`)

Every new metric needs a `metric_weights` row or it bills $0 (the catalog FK in
the redesign; the `0041` table today). Below: the metrics introduced/reserved by
this design, whether they emit in v1, and the proposed weight. Weights are
expressed as `units_per_op CU per per_units ops`, consistent with the `0041`
seed conventions (bytes priced sub-unit; egress > ingress; CPU 1 CU/ms).

| Metric | Emits in v1? | `units_per_op` | `per_units` | Rationale |
| --- | --- | --- | --- | --- |
| `gateway_egress_bytes` | **Yes** (H2) | 1 | 1000 | Same as `egress_bytes` (1 CU / 1000 B) — gateway egress is egress; equal weight keeps "total egress" coherent across the two metrics. Operator may diverge later if CDN economics differ. |
| `stream_wall_us` | **Yes** (H1 SSE) | 1 | 10000 | Mirror `wall_us` (1 CU / 10 ms) — a held-open stream's wall is priced like any wall time. |
| `ws_conn_us` | No (WS deferred) | 1 | 10000 | Reserved; mirror `wall_us`. Add the row when WS metering lands. |
| `ws_messages` | No (WS deferred) | 1 | 100 | Reserved; per-frame, cheap. Add with the WS epic. |
| `ws_egress_bytes` | No (WS deferred) | 1 | 1000 | Reserved; mirror egress. |
| `ws_ingress_bytes` | No (WS deferred) | 1 | 10000 | Reserved; mirror ingress. |
| `db_bytes` | No (H3 deferred) | — (omit) | — | Omit the row ⇒ $0 until the emit point + weight land together. |
| `kv_bytes` | No (H3 deferred) | — (omit) | — | Same — turn on later in one slice. |

**Changeset `0047_metering_coverage_weights.sql`** (the ONLY DB change in G4):
an `INSERT … ON CONFLICT (metric) DO NOTHING` adding the rows for the metrics
that **emit in v1** — `gateway_egress_bytes` and `stream_wall_us`. The deferred
WS/db/kv names are **intentionally NOT seeded** (an unemitted-but-weighted
metric is dead config, and an emitted-but-unweighted metric is free — both are
acceptable, but seeding only what emits keeps the table honest, matching the
`0041` "no `db_rows_read` weight — no primitive emits it" precedent). When WS or
db/kv-bytes land, *that* slice adds *its* weight row in *its* changeset
alongside the emit point.

**Pricing flow is unchanged.** These rows are read by `pricing_store.rs`/
`pricing.rs::total_units` exactly like the `0041` seeds: `total_units +=
aggregate_total × units_per_op ÷ per_units`, then `charge_cents` applies the FX.
No pricing-code change. A re-weight (via the operator endpoint G5a, once built)
reprices history without touching stored usage — the same property the existing
weights have.

---

## 6. Per-gap summary table

| Sub-gap | New code (files) | New metric(s) | Changeset | Spend/provider touched? | Schema-redesign dep |
| --- | --- | --- | --- | --- | --- |
| **H2** gateway egress | `gateway/Cargo.toml` (+dep), `gateway/src/main.rs` (Meter+flush), `gateway/src/router/dispatch.rs` (record on Static/Redirect), `gateway/src/router/static_serve.rs` (streamed drain hook) | `gateway_egress_bytes` | `0047` | **No** | **No** |
| **H1** SSE/streaming | `worker/src/handler.rs` (incremental drain flush) | `stream_wall_us` (+ existing `egress_bytes`) | `0047` | **No** | **No** |
| **H1** WS | — (deferred to WS epic) | reserved: `ws_conn_us`, `ws_messages`, `ws_*_bytes` | — | No | No |
| **H3** cpu async | — (deferred; kernel accumulator) | — (existing `cpu_us`, bound documented) | — | No | No |
| **H3** db/kv bytes | — (deferred; weight-ready) | reserved: `db_bytes`, `kv_bytes` | — | No | No |

**No part of G4 depends on the schema redesign** (matrix row G4a/b: "none —
reuses `usage_aggregates` raw rows"). The only DB change is `0047`'s two weight
rows, which serialize behind the latest changeset (`0046`) per the linear-
numbering rule.

---

## 7. Enforcement-independence (explicit)

The spend engine (`crates/control/src/spend.rs::derive_state`,
`cron/spend_reconcile.rs`) and the gateway enforcement (`enforce.rs::check_spend`
/ `check_account`) read **only** `usage_aggregates` totals priced through the
weights. Adding `gateway_egress_bytes` and `stream_wall_us` rows raises the same
CU total the spend engine already computes — so a spend cap automatically
includes the newly-metered usage **with zero spend-code change**. The providers
(`metering/provider/{native,stripe_meters,openmeter}`) export `usage_aggregates`
rows verbatim; new metric names flow through unchanged. **No code in the
spend/enforce/provider path is modified by this design.** This is the payoff of
the raw-metric-keyed aggregate: coverage is added at the *producer* edge and
everything downstream inherits it.

---

## 8. Faithful TDD plan (real path, no shims — per `feedback_faithful_e2e_tests`)

Every fix ships a regression test that fails pre-fix (`feedback_regression_test
_per_fix`).

**H2 — gateway egress:**
- *Unit* (`gateway` crate): drive `execute_resource_tree` with a
  `ResolvedAction::Static` against the real `RouteCache`/`CompiledRoute`, a real
  `Arc<Meter>` in `GateState`, and a stub blob store returning N bytes; assert
  `meter.drain()` shows `gateway_egress_bytes == N` for the route's app_id, and
  **`egress_bytes == 0`** (gateway never touches the worker-owned metric).
  RED pre-fix: gateway has no meter ⇒ nothing recorded.
- *Regression* `gateway_does_not_meter_worker_proxy_body`: drive a `WorkerRpc`
  action (worker proxy) and assert the gateway records **no**
  `gateway_egress_bytes` for the proxied body — the no-double-count rule. RED if
  someone later meters the proxy path.
- *Restart-safety* `gateway_producer_id_is_restart_unique`: two
  `boot_worker_id("gate-...")` calls differ (reuses the existing
  `boot_worker_id` test pattern in `flush.rs`).
- *Faithful e2e:* extend the metering full-stack script — serve a static asset
  through the real gateway, let the gateway flush to the real control
  `/internal/usage`, and assert `usage_aggregates` has a `gateway_egress_bytes`
  row for the app, and that it **prices** (CU > 0) once `0047` is applied.

**H1 — SSE/streaming incremental flush:**
- *Unit* (`worker` crate, real dispatch): the existing
  `dispatch_feeds_all_five_platform_counters` is the template. Add a streaming
  handler that emits chunks across two recording intervals (drive the drain with
  a controllable interval/byte threshold injected for the test) and assert the
  meter shows **multiple** `stream_wall_us`/`egress_bytes` deltas summing to the
  total — not a single end-of-stream record. RED pre-fix: today only one
  finalize record exists.
- *Regression* `long_stream_accrues_before_close`: assert a delta is recorded
  *before* the stream finalizes (the crash-loss-bound property). RED pre-fix.
- *Regression* `streaming_counts_request_exactly_once`: a long stream increments
  `requests` exactly 1, not per-delta.

**H3 (deferred):** no new tests now; the existing
`dispatch_feeds_all_five_platform_counters` already pins `cpu_us > 0` as a lower
bound. When H3 is undertaken, its slice adds the faithful test.

**Pricing wiring:** a unit test that `total_units` over a synthetic
`usage_aggregates` containing `gateway_egress_bytes` + `stream_wall_us` (with
the `0047` weights) yields the expected CU — proving the new metrics price
through the unchanged pricing path.

---

## 9. Dependencies, sequencing, risk

- **Schema dep:** none beyond `0047` (two weight rows), which serializes behind
  `0046`. No dependency on the schema redesign (confirmed against the dependency
  matrix).
- **Sequencing:** H2 and H1-SSE are **independent of each other** (different
  crates: gateway vs worker) and of the revenue-protecting gaps (G1/G2). Per the
  roadmap they sit in Wave 3 — *after* the revenue-protecting gaps, because
  under-billing is customer-favourable, not a loss.
- **Risk:** **Low.** Both add usage that today is missed; neither can
  *over*-bill (H2's metric is disjoint-by-construction from the worker's; H1
  only adds deltas of bytes/duration that genuinely occurred). The one
  load-bearing correctness point is the **no-double-count partition** (§2.4) —
  pinned by the `gateway_does_not_meter_worker_proxy_body` regression.

---

## 10. The riskiest / most load-bearing design points

1. **The egress ownership partition (§2.4).** `egress_bytes` worker-owned vs
   `gateway_egress_bytes` gateway-owned, disjoint by construction (worker-body
   vs gateway-body). Get this wrong and the same byte bills twice (over-bill,
   the one non-customer-favourable failure) or zero times. Mitigated by the
   structural partition + a dedicated regression test.
2. **Gateway producer-id restart-safety (§2.3).** If the gateway's producer id
   were stable across restarts, post-restart sequence-1 reports would be dropped
   as phantom duplicates against pre-restart rows — silent under-bill, exactly
   the bug the worker's boot-nonce fixed. Mitigated by reusing `boot_worker_id`
   verbatim.
3. **Incremental-flush crash-loss bound (§3.3).** The recording cadence is the
   crash-loss granularity. Too-coarse loses more on crash; too-fine adds meter
   churn on long streams. The chosen `~10s / ~1 MiB` defaults bound in-memory
   un-recorded usage without a durability guarantee (which stays deferred until a
   durability SLA exists — roadmap hardening-backlog (a)).

---

## 11. Open questions for the operator

1. **Gateway-egress weight parity.** Price `gateway_egress_bytes` equal to
   `egress_bytes` (recommended — egress is egress) or cheaper (asset egress is
   often CDN-fronted and cheap)? Affects only the `0047` weight, re-tunable
   anytime via G5a.
2. **Redirect egress.** Meter the (tiny) redirect body as
   `gateway_egress_bytes` (recommended — keeps "gateway-emitted *successful*
   bytes ⇒ gateway-owned" simple) or exclude redirects as de-minimis?
   (RESOLVED for error envelopes: gateway-edge error/4xx/5xx/204 bodies are
   platform overhead and NOT billed — §2.4 scope note.)
3. **`stream_wall_us` vs folding into `wall_us`.** Keep long-lived duration in
   its own metric (recommended — pricing + observability flexibility) or unify?
4. **Recording cadence defaults** (`FLUSH_INTERVAL` / `FLUSH_BYTES`) — confirm
   `~10s` / `~1 MiB`, or tighten for a stricter crash-loss bound.
5. **H3 trigger.** Confirm deferring `cpu_us` async attribution + db/kv bytes
   until a *measured* CPU/byte-dominated cost case justifies the kernel/primitive
   work.
