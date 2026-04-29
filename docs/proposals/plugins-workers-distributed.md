# Plugins + Workers in a distributed runtime

> Companion to `2026-04-23-distributed-architecture.md`. That doc covers
> the full stack; this one zooms in on the two layers that run user code:
> the worker fleet and the plugins that give user code its capabilities
> (db, kv, storage, auth, payments).
>
> **Critical finding up front**: the current plugin design works for a
> single node. For a distributed fleet, three of four plugins are
> subtly-to-severely broken, and the worker state model needs to flip
> from "stateful sharded" to "stateless with local cache."

---

## The principle that fixes everything

> **Workers are ephemeral compute. State lives in external services.
> Local storage is cache — correctness must not depend on it.**

This is Cloudflare Workers' core insight and why they scale to millions of
apps cheaply. It's what our current architecture violates in several
places.

The consequences of adopting it:

- Any worker can serve any app's request (no more "app X must hit worker Y")
- Worker failure is invisible to apps (new worker picks up, state is intact)
- Scaling workers is just spinning up processes (no rebalancing state)
- Cold start cost is bounded by bundle load, not state warm-up
- Plugins are backed by external services, never by process memory

**The CHWBL routing we have is still valuable** — it provides locality for
caching (hot apps stay on the same worker, bundle already loaded, isolate
already warm). But it's a perf optimization, not a correctness primitive.
If the routing breaks, apps still work — just slower.

---

## Per-plugin analysis

Each plugin has a different distributed story. Let me audit them.

### plugin-kv — **BROKEN for multi-worker**

Current state:
```rust
thread_local! {
    pub(crate) static STORE: RefCell<HashMap<String, Entry>> =
        RefCell::new(HashMap::new());
}
```

**In-memory, per-thread-local, per-worker-process.** Data written to
worker A is invisible on worker B. If a user's session cookie writes to
worker A on login and the next request hits worker B, the session is lost.

This is the single worst distributed-correctness bug in our plugin set.
It works in single-worker dev only.

**Fix**: introduce `trait KvBackend` just like we did for storage. Ship:

- `InMemory` — current behavior, dev only
- `Redis` / `Dragonfly` — network-backed, shared across all workers in a region
- (Future) `Upstash` — managed, global edge cache

Config via env: `ZEROSHIP_KV_BACKEND=memory|redis` + `ZEROSHIP_KV_URL=...`.

App scoping stays the same (`<app_id>:<user_key>`); the backend handles
the wire ops against Redis.

**Why Redis/Dragonfly specifically**: app needs atomic ops (incr, SETNX,
expire). HTTP key-value stores (DynamoDB, Firestore) don't give these
cheaply. Redis wire protocol is simple enough we can write a compio-native
client (~300 lines) to stay tokio-free.

Effort: ~1 day for the trait refactor + Redis impl.

### plugin-storage — **PARTIAL, fix in progress**

Current state:
- Just refactored to `trait Backend`
- Only impl is `LocalFs`
- Works for single-node dev, broken for multi-worker prod

`LocalFs` on worker A means files live on A's disk. Worker B can't see them.

**Fix**: already scoped as Phase 2 of the original roadmap — `S3` backend
behind the `s3` feature flag. Covers S3, R2, MinIO via endpoint config.

Once `S3` ships, all workers in all regions see the same objects.
Isolation is enforced via `<app_id>/` key prefix.

**R2 is our production target.** Zero egress fees matter at 10M+ end-users
hitting uploaded assets.

Effort: ~1 day (already scoped).

### plugin-db — **WORKS, but doesn't scale without sharding**

Current state:
- Connection pool per worker thread
- All apps share one Postgres URL via `DATABASE_URL`
- Schema-per-app isolation (`"app_id".table`)

**What works distributed**: every worker talks to the same authoritative
DB. Writes are consistent. No stale state.

**What breaks at scale**:
- **Connection count**: Postgres caps around 1K concurrent connections.
  10K apps × 2 warm conns per worker × 16 workers = 320K connections to
  one DB. Breaks at ~10 apps if we're not careful.
- **DDL contention**: `registerModel` runs `CREATE TABLE IF NOT EXISTS`
  per app. Schema cache invalidation in Postgres is not free; at high
  churn it locks.
- **Single point of failure**: one DB down = everyone down.

**Fix in two stages**:

*Stage 1 (scales to 1K apps)*: Pooled proxy in front of Postgres
(PgBouncer/PgCat in transaction mode). One physical connection serves
many app requests. Our compio-postgres pool connects to the proxy, not
direct. App-level isolation still via schema.

*Stage 2 (scales to 10K+ apps)*: Shard Postgres by app_id.

```
app_id → hash(app_id) % N → shard_id → Postgres cluster URL
```

Plugin-db grows a shard routing layer:

```rust
// Pseudocode
impl DbPlugin {
    fn connection_for_app(&self, app_id: &str) -> &Pool {
        let shard = xxh3(app_id) as usize % self.shards.len();
        &self.shards[shard]
    }
}
```

Each shard is a physical Postgres cluster (~1K apps per shard). 1K shards
for 1M apps. Shards can be added with rebalancing (rare — schema-per-app
makes this non-trivial; RLS-per-row makes it easier).

Tiers (from the main arch doc):
- Free tier: shared cluster + RLS
- Paid tier: Neon branch per app
- Enterprise: dedicated cluster

**Effort**: Stage 1 is 2-3 days (just PgBouncer in front + test).
Stage 2 is ~1-2 weeks (routing layer, rebalancing, migration from
schema-per-app to tenant_id-per-row for free tier).

**Distributed correctness today**: safe. Postgres ACID + schema isolation
means we won't corrupt data even at scale. We'll just run out of
connections and tablespace.

### plugin-auth — **DISTRIBUTED-SAFE already**

Current state:
- Validates JWT cookie at gateway (stateless — just signature check + claim extraction)
- Session state is the JWT itself (no server-side storage needed for most flows)
- OAuth flows use short-lived state tokens (stored in KV for ~5 min)

**Why it works distributed**: JWT validation is pure computation. Any
worker can validate any request. The "state" is the claim inside the JWT,
signed by a shared secret all gateways know.

**The one catch**: OAuth flow state. The `state` param lives in KV for
the 30-60 seconds between OAuth redirect and callback. If KV is in-memory
per worker (current bug), the callback might hit a different worker and
fail.

**Fix**: this becomes correct automatically once plugin-kv gets a shared
Redis backend. No other changes needed.

### plugin-payments (future) — **designed for distributed from day one**

Not built yet. When we build it for Stripe Connect:

- Webhooks are HTTP POSTs from Stripe to our gateway — hit any worker
- Webhook signature is verified statelessly
- Idempotency is enforced by writing to Postgres with a unique event ID
  constraint (duplicate webhook = unique violation, ignored)
- Revenue metering emits events to the bus for aggregation

No worker-local state needed. Design from scratch for the stateless model.

---

## Worker state — the full audit

Let's enumerate every piece of per-worker state and classify each as
safe, cache, or correctness-breaking.

| State | Where | Classification | Distributed concern |
|---|---|---|---|
| Bundle bytes (loaded apps) | Worker memory | Cache | ✓ fine — reload from R2 on miss |
| V8 isolates per app | Worker memory | Cache | ✓ fine — rebuild from bundle |
| DB pool per thread | `plugin-db::DB_POOL` | Connection state | ✓ fine — connects to shared DB |
| Plugin config | Various thread-locals | Config state | ⚠ needs hot-reload (see below) |
| KV in-memory store | `plugin-kv::STORE` | **Correctness bug** | ❌ REPLACE with Redis backend |
| Storage LocalFs root | `plugin-storage::STORAGE_BACKEND` | **Correctness bug for multi-worker** | ❌ use S3/R2 in prod |
| Per-request state (logs, user, ctx) | `state.logs_by_id` etc. | Ephemeral | ✓ fine — request-scoped |
| `executing_request_id` | Runtime state | Ephemeral | ✓ fine |
| Registered models (`REGISTERED_MODELS`) | plugin-db thread-local | Cache (DDL idempotent) | ✓ fine — just avoids redundant DDL |
| Transaction conn (`TX_CONN`) | plugin-db thread-local | Session state | ✓ fine — tied to single JS context |
| Streams, WebSockets | `state.streams`, `state.websockets` | Session state | ⚠ see below |

Three flagged items:

**1. KV in-memory store** — must be replaced. Addressed above.

**2. Storage LocalFs** — must use S3/R2 in prod. Addressed above.

**3. Streams and WebSockets** — session state tied to a specific TCP
connection. Cannot survive worker migration. But they're OK because:

- WebSocket connection is pinned to its initial worker by TCP — client
  reconnects on worker death
- HTTP streaming is request-scoped — client retries if the worker dies mid-stream
- Long-running streams should use client-side resume tokens, not server-side
  resume state

If a creator builds a "chat room" where messages must be routed between
users on different workers, they need a pub/sub backend (Redis pub/sub
via plugin-kv). We should ship this pattern as an SDK primitive
(`@zeroship/realtime`) but it's not in the critical path.

**4. Config hot-reload** — worth its own section.

---

## Config propagation (the quiet requirement)

Today, plugin config is passed at worker startup via env vars:
- `DATABASE_URL`
- `ZEROSHIP_STORAGE_ROOT`
- `ZEROSHIP_KV_URL` (future)

**Problem at scale**: rotating a DB credential means restarting every
worker. Rolling a TLS cert means the same. Adding a new region means
updating env vars across the fleet.

**Fix**: control plane is the source of truth. Workers subscribe to
config changes via the event bus.

Shape:

```
control plane (writes) ─► event bus (NATS) ─► worker ─► plugin.reconfigure()
```

Each plugin gains a `reconfigure(new_config)` method:

```rust
pub trait ReconfigurablePlugin: NativePlugin {
    fn reconfigure(&self, config: serde_json::Value) -> Result<(), String>;
}
```

For plugin-db: close old pool, open new pool with new URL.
For plugin-storage: swap the `Arc<dyn Backend>` in the thread-local.
For plugin-kv: same pattern.

In-flight requests keep their current handle (they captured `Arc` at
call time). New requests see the new config. No downtime.

**Effort**: 2-3 days when we need it (probably Tier B).

---

## Worker model: target shape

### Today (stateful sharded)

```
┌────────────────────────────────┐
│ Worker process                 │
│  ┌───────────────────────────┐ │
│  │ V8 isolates per app       │ │
│  │ (LRU evicted after N idle)│ │
│  └───────────────────────────┘ │
│  ┌───────────────────────────┐ │
│  │ DB pool  (thread-local)   │ │
│  │ KV store (in-mem, BROKEN) │ │
│  │ Storage root (LocalFs)    │ │
│  └───────────────────────────┘ │
└────────────────────────────────┘
         ▲
         │ CHWBL routing pins app X to worker Y
         │
      Gateway
```

### Target (stateless compute + external state)

```
┌────────────────────────────────┐
│ Worker process                 │
│  ┌───────────────────────────┐ │
│  │ V8 isolate CACHE per app  │ │
│  │ (built from snapshot,     │ │
│  │  evictable, rebuildable)  │ │
│  └───────────────────────────┘ │
│  ┌───────────────────────────┐ │
│  │ Plugin handles:           │ │
│  │ - DB pool → sharded PG    │ │
│  │ - KV handle → Dragonfly   │ │
│  │ - Storage → R2 client     │ │
│  └───────────────────────────┘ │
└────────────────────────────────┘
         ▲
         │ CHWBL routing (locality hint, not correctness)
         │
      Gateway
         │
         ▼
   External state:
   - Sharded Postgres (per app_id → shard)
   - Dragonfly region cluster (KV)
   - R2 bucket (objects)
   - Control plane (bundles, routes, secrets)
```

Key differences:
1. Worker holds only **cache** (bundles, isolates). Evict anything, nothing breaks.
2. All **state** lives in shared services. Any worker can serve any request.
3. CHWBL becomes a **locality hint** — keeps apps warm on the same worker — not a correctness requirement.
4. Plugin handles are **network handles**, not in-process state.

---

## Cold start in the distributed world

With stateless workers, every cache miss is a cold path. Three layers of caching:

### 1. Bundle cache (per-worker)

Worker needs app bundle. Cache miss → fetch from control plane's object storage.
- Hot: in-memory (~10 MB per app, ~1K apps / GB RAM = 100K apps on a 100 GB worker)
- Warm: local disk (evicted from memory, faster than remote fetch)
- Cold: remote fetch from R2 (10-50ms)

### 2. V8 isolate cache (per-worker)

Bundle is loaded, but V8 needs to compile + evaluate top-level. First time: expensive (100-500ms for a typical bundle).

**V8 snapshots**: after first evaluation, serialize the heap. Next cold start = deserialize snapshot (<10ms). Snapshot is per-bundle, cacheable per worker.

Snapshots can also be pre-computed at deploy time and shipped alongside the bundle, so the *first* cold start on any worker is fast.

### 3. Request-level isolate

For strongest isolation, CF creates a fresh isolate per request from a warm V8 context template. We could eventually do this; today, we reuse isolates across requests.

**Order of impact**:
1. V8 snapshots (Tier B, 2-3 weeks) — the big unlock, drops cold start from 500ms to <10ms
2. Bundle cache warming (Tier A, 1 day) — pre-pull bundles when app is deployed
3. Request-level isolates (Tier C, months) — strongest isolation, only needed for hostile-code threats

---

## Routing in a distributed fleet

### Today

Single region. Gateway → CHWBL → worker pool → worker. ~1ms routing overhead.

### Target

```
User → DNS (anycast) → nearest edge POP
                            │
                            ▼
                      Edge POP: cert, auth, cache, route lookup
                            │
                            ▼
              (pick nearest region with app's primary state)
                            │
                            ▼
                     Origin region gateway
                            │
                            ▼
                 CHWBL → worker (pool local)
                            │
                            ▼
                      Plugin calls → external services
                        ├── Sharded Postgres
                        ├── Dragonfly KV
                        └── R2 storage
```

**Routing table**: `app_id → {home_region, bundle_version, db_shard, flags}`

Lives in control plane. Replicated to every edge POP via event bus. Updates propagate in <1s.

**App migration between regions**: creator requests, control plane orchestrates:
1. Set `home_region` to target
2. Await event bus propagation
3. Trigger data replication (DB snapshot → new region)
4. Flip routing
5. Drain old region
6. Decommission

Complex. Year-3 feature.

---

## Failure modes + recovery

### Worker dies

- CHWBL reassigns app → next request lands on a different worker
- New worker: bundle cache miss → fetch bundle, init isolate, execute
- Cold-start cost: 100-500ms without snapshot, <10ms with
- Requests in-flight on dead worker: client gets 5xx, retries (HTTP semantics)

### Region degraded (slow but not dead)

- Gateway detects via health checks, routes to secondary region
- App cold-starts in new region (bundle pulled from global R2)
- DB access: slow if primary is still in degraded region (read replica in secondary helps)
- Creator notices latency; premium tier has active-active DB for zero-latency failover

### Region down

- DNS steers traffic away from dead region
- Apps with data only in dead region are DOWN until region recovers
- Premium apps have replicated data → auto-failover works

### Control plane unavailable

- Data plane keeps serving from last cached routing table
- Deploys block
- Metering events buffer locally, emit when control plane returns

---

## The migration path from today

### Week 1 (part of Tier A)

Ship S3/R2 storage backend. Plugin-storage already has the trait — just
add `S3` impl and feature-gate. Unblocks creators deploying real apps
to the distributed fleet; no more LocalFs-on-one-worker landmine.

### Week 2 (part of Tier A)

Replace plugin-kv in-memory with Redis/Dragonfly backend. Same trait
pattern as storage. Removes the single worst distributed-correctness
bug in our plugin set.

### Week 3 (part of Tier B)

PgBouncer (or PgCat) in front of Postgres. Plugin-db talks to the proxy.
Connection scaling to 1K+ apps per cluster without changing plugin code.

### Week 4-6 (part of Tier B)

V8 snapshots in the runtime. Cold-start drops to <10ms. This is where the
stateless-worker model becomes economically viable (before this, cold
start penalty made CHWBL pinning necessary).

### Week 7-8 (part of Tier B)

Shard routing in plugin-db. Move free tier to RLS-per-row. Migration
tooling from schema-per-app.

### Week 9-10 (part of Tier B)

Event bus (NATS) for config propagation. Plugins gain `reconfigure()`.
Removes worker-restart requirement for secret/config rotation.

### Week 11+ (Tier C)

Multi-region primary, app migration, predictive pre-warming. Only if
traffic signals demand.

---

## What we ship THIS WEEK to make distributed viable

Tactical list. Each is a separate commit.

1. **plugin-kv Redis backend** (~1 day) — fixes the worst correctness bug.
   New trait `KvBackend`, impls: `InMemory` (current), `Redis` (new).
   Uses a minimal compio-native Redis client (~200 lines).

2. **plugin-storage S3 backend** (~1 day) — covers R2, MinIO, Spaces.
   Implements the existing `Backend` trait. Hand-rolled on cyper + aws-sigv4.

3. **Smoke test for multi-worker correctness** — spin up 2 workers,
   route requests, verify app state is consistent. Catches any regression
   where state sneaks into worker-local storage.

4. **Worker state audit doc** — the table above, kept current. Any new
   plugin or state must be categorized before landing.

---

## Open questions for the next spike

1. **Can we run compio-redis on the same runtime as compio-postgres without
   deadlocks?** Both have connection pools; ideally they share the executor.
2. **Does V8's snapshot API work for modules with top-level await?** Our
   runtime uses TLA extensively (user bundles often await DB setup at init).
3. **How hot is Dragonfly's Rust wire?** The [dragonfly-rs](https://github.com/dragonfly-io/dragonfly-rs)
   client exists but may pull tokio.
4. **What's the latency cost of NATS JetStream for per-request metering
   emit?** If it's >1ms, we need to batch locally first.
5. **How do we handle bundle integrity across the fleet?** SHA-256 per
   bundle (already in `.appbundle`), verified on worker fetch. Needs integration with the bundle cache.

---

## TL;DR

| Concern | Status | Fix |
|---|---|---|
| plugin-kv in-memory state | **Broken for multi-worker** | Redis/Dragonfly backend (~1 day) |
| plugin-storage LocalFs | Dev only | S3/R2 backend (~1 day) |
| plugin-db connection scaling | Works to ~1K apps | PgBouncer proxy + later sharding |
| plugin-auth | Already distributed-safe | — |
| Worker state | Stateful sharded | Migrate to stateless + cache |
| Cold start | 100-500ms | V8 snapshots → <10ms (Tier B) |
| Config hot-reload | Worker restart needed | Event bus + `reconfigure()` method |

Concretely: **next session should fix the two plugin correctness bugs
(kv Redis backend, storage S3 backend) before anything else.** Until
those land, "distributed" means "probably corrupted." Everything else
(V8 snapshots, sharding, event bus) is optimization.

The architecture in the main doc is the destination. The two plugin
fixes are what we ship THIS WEEK to make the current worker fleet not-broken.
