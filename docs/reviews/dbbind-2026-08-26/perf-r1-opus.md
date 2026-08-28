# Review: scale/performance work on `feat/dbbind-impl`

Reviewer lane: db path, scale. Read-only. All paths absolute-relative to
`/home/ruiyang/Projects/appbase/.worktrees/dbbind-impl`.

**The branch moved under me.** At start HEAD was `526b19e37`; a fifth commit
`fb383e98c test(db): measure the in-memory multiplier for a cached schema entry`
landed mid-review. Everything below is against `fb383e98c`. The brief lists four
commits; there are five.

## What I measured myself (not inherited)

| measurement | how |
| --- | --- |
| `measure_cached_entry_size` output | `cargo test -p zeroship-plugin-db --lib measure_cached_entry_size -- --nocapture` |
| `serde_json::Value` in-memory multiplier | standalone crate in `/tmp/jsonsize`, workspace's exact features (`raw_value`,`preserve_order`) |
| `pg_class` / `pg_attribute` index set | `docker exec zs-dbbind-pg psql ... \di pg_catalog.pg_class*` |
| `read_live_schema` plans at 2000 and 4000 tenants | throwaway `postgres:16.14` container `zs-catscale-probe`, 4000 schemas x 8 tables x 14 cols, `EXPLAIN (ANALYZE, BUFFERS)` on the three queries copied verbatim from `diff.rs` |
| `max_replication_slots` / `max_wal_senders` | `pg_settings` on this branch's own `zs-dbbind-pg` |
| bind-parameter wall | `postgres-protocol-0.6.12` `write_counted` source (version taken from `Cargo.lock`) |

Two subagents swept the write/read hot path and the CDC/encryption path. I
re-verified every finding of theirs that appears below by reading the cited
lines myself; findings I could not re-verify are not here.

---

## 1. The cold-start fix

### The claim is true, and understated

`crates/zeroship-schema/src/diff.rs:640-644` - the column query's only predicate
is `n.nspname = $1 AND c.relkind = 'r' AND a.attnum > 0 AND NOT a.attisdropped`.
No table predicate. Confirmed.

Understated in two ways:

- `read_live_schema` is **three** whole-schema queries, not one: columns
  (`diff.rs:620-645`), foreign keys (`diff.rs:768-782`), indexes
  (`diff.rs:814-831`). So the pre-fix cost was `3N` catalog round trips per
  cold start, not `N`.
- The commit calls it "quadratic work for linear information". It is not
  quadratic. It is `N` reads each costing `O(app schema size)` - linear in N,
  with a large constant. Calling it quadratic overstates a defect that did not
  need overstating.

### What the fix misses (headline): it DELETED the negative cache for an absent collection

This is the strongest finding in section 1 and it is a regression, not a gap.

Before (`introspect_schema.rs`, parent of `e8218c4c3`):

```rust
let schema = build_runtime_schema(&live, collection);
c.cache_introspected_schema(app_id, collection, &token, schema.clone());
```

`schema` is `Option<Value>`. When the collection is **absent from the live
catalog** `build_runtime_schema` returns `None` (`introspect_schema.rs:179`,
`live.tables.get(collection)?`) and the old line cached that `None`.

After (`introspect_schema.rs:104-122`):

```rust
let schema = build_runtime_schema(&live, collection);
crate::context::with_mut(|c| { cache_every_collection(c, app_id, &token, &live); });
Ok(schema)
```

`cache_every_collection` (`introspect_schema.rs:139`) iterates
`live.tables.keys()`. A collection that is **not** a key gets no entry. The
requested collection's `None` is returned to the caller and thrown away.

Consequence: a collection that is registered but has no live table now misses
the cache on **every single db op, forever**, and each miss runs the three
whole-schema catalog queries. The old code paid that once per (app, collection,
deploy, thread).

This breaks a contract that is documented in two places and tested in neither:

- `introspect_schema.rs:19-20`: "A goodie-free collection is cached as a
  NEGATIVE result (`None`) so it is not re-introspected each call."
- `context.rs:276-278`: the inner `Option` "distinguishes 'introspected,
  collection absent / has no goodies' (`None`) from 'not yet introspected' (no
  map entry)".

The "collection absent" half of that is now unreachable. `missing_collection_is_none`
(`introspect_schema.rs:499-503`) asserts `build_runtime_schema` returns `None`;
no test asserts the **cache** holds a negative, so nothing went red.

Reachability - all four are ordinary, not exotic:

1. app schema exists but migrations have not landed yet (`live.tables` empty,
   loop body never runs, zero entries cached, every op re-reads);
2. a collection in the descriptor whose table a later migration dropped;
3. a naming-strategy mismatch between descriptor collection name and physical
   table name;
4. the app's PG schema does not exist at all (first deploy pre-migrate).

Cost per miss, measured at 4000 tenants: ~27.5 ms of catalog execution (see
section 4.1). Per op.

### The `is_model_registered` gate: the fix decouples it from cache population

`introspect_schema.rs:76` returns `Ok(None)` for an unregistered collection
before any cache work. Under the old code that gate also bounded *when an entry
could be created*: an entry for collection B could only exist after
`registerModel(B)` had run (`register_model/mod.rs:96`).

`cache_every_collection` now plants entries for every table in the app schema
regardless of registration, so B's entry exists before `registerModel(B)`.

**Not exploitable today**, and I want to be precise about why: on Postgres
`exec_register_model` is `Ok(())` - it applies no DDL (`register_model/mod.rs:183`,
and the module header at `:6-8` says so explicitly). No schema change happens
between plant and read. It is a latent invariant break: the moment any path
makes registration schema-affecting again (the SQLite ATTACH arm at `:203`, a
dev-tier apply), plant-before-register becomes a live staleness bug that reads a
pre-DDL snapshot under the current deploy token.

### SQLite arm: no regression, but the fast path there is dead code

`introspect_schema.rs:96-98` returns before `cache_every_collection`, so the
SQLite arm is untouched. Correct.

Worth stating anyway: on SQLite `introspected_schemas` is **never written**, so
the `if let Some(cached)` fast path at `:87-91` can never hit. Every SQLite db
op runs three `format!("{app}:{coll}")` allocations plus a full deep clone of
the declared schema (`context.rs:612-619`). Pre-existing; the commit's
"per-app cache budget" reasoning does not apply on that arm at all.

### Is populating every collection ever wrong? Yes - on memory

It converts the growth rate of an unbounded, never-evicted, thread-shared map
from *collections the app actually touches* to *every table in the app's
schema*. An app with 200 tables whose handlers use 2 now plants 200 entries on
every worker thread that ever serves it. Measured entry cost is 1.9 KB (8 plain
columns) to 47 KB (120 columns with encrypted+mask facets) - section 3.

That is the same map the design doc says is "a ceiling on apps-per-worker"
(`docs/proposals/2026-08-26-runtime-db-binding-design.md:1086-1091`). The
commit lands the growth-rate increase and the bound does not exist yet. The
ordering is backwards: bound first, then widen what fills it.

Not wrong on tenancy: `read_live_schema` is `nspname = $1`, so nothing
cross-tenant is learned.

### The internal-table filter: half of it cannot fire, and it is a second copy

`introspect_schema.rs:143`: `table.starts_with("__zeroship") || table.starts_with("__zs_")`.

- `__zeroship` fires: `__zeroship_migrations` (`audit.rs:234`) exists in every
  app schema; `__zeroship_audit_unmask` (`unmask.rs:835`),
  `__zeroship_audit_mask_drift` (`mask_drift.rs:785`) when used.
- `__zs_` **cannot fire**. Every `__zs_` name in the tree is a replication slot
  or publication (`replication.rs:107-115`), which are not tables. Grep of
  `crates/zeroship-plugin-db/src` and `crates/zeroship-schema/src` finds no
  `__zs_`-prefixed table.
- `__zeroship_mv_*` cannot appear either - `relkind = 'r'` excludes matviews.

So the filter saves 1-3 entries out of T. The "would spend the per-app cache
budget" justification in the commit message and in the test name is not
load-bearing at that ratio.

It is also a **divergent second copy** of a list that already exists:
`backend/sqlite/cdc.rs:460-463` enumerates the canonical set
(`__zeroship_mv_`, `__zeroship_audit_`, `__zeroship_migrations`, `__zs_`).
Two hand-maintained copies of one predicate in two crates.

---

## 2. The cache-bound design

Doc section: `docs/proposals/2026-08-26-runtime-db-binding-design.md:1064-1180`.

The problem statement is correct and I verified every cell of its eviction-site
table: `introspected_schemas` (`context.rs:279`), `deploy_tokens` (`:296`),
`schemas` (`:257`) have no production removal path; `mask_policies` (`:312`) has
one targeted per-app remove (`:736`). The only clears are
`#[cfg(any(test, feature="test-helpers"))]` (`:575`, `:602`).

I also verified the half the doc asserts without checking: **the worker never
signals plugin-db on eviction.** `evict_lru` (`cache.rs:735-789`) removes from
`cache.isolates` and `LOADED_META` and calls nothing in plugin-db. `evict_app`
(`cache.rs:648-656`) likewise. So the `mask_policies` comment at
`context.rs:310-311` - "isolate lifetime is bounded by the LRU worker cache, so
the policy lives as long as the app is hot" - is false: `ISOLATE_CTX` is a
`thread_local!` (`context.rs:952-957`) that outlives every isolate on the thread.
The doc is right to call this out.

### Entry-count is the wrong unit, and the doc's own fixture is why it looks right

The doc argues (`:1155-1159`): "entries are **small** individually and the count
is what runs away. That argues for bounding by **entry count** rather than
bytes."

Measured, workspace features, `/tmp/jsonsize`:

| shape | serialized | in-memory (floor) | per column |
| --- | ---: | ---: | ---: |
| 8 cols, plain | 273 B | 1,880 B | 235 B |
| 16 cols, plain | 551 B | 3,688 B | 230 B |
| 40 cols, plain | 1,391 B | 9,112 B | 227 B |
| 16 cols, +encrypted/mask | 995 B | 6,660 B | 416 B |
| 120 cols, +encrypted/mask | 7,259 B | 47,574 B | 396 B |

**A 25x spread in bytes per entry.** Entries are not uniformly small; the doc's
fixture makes them look so because every column is a plain `text` with a
13-character name and no facet. An entry-count bound therefore admits a 25x
range in the thing it is supposed to bound. The doc's own worked number -
"~10,000 entries caps the cache near 38 MB" (`:1152`) - is 10,000 x the *typical
plain* entry. At the goodie-bearing 16-column shape the same 10,000 entries is
~67 MB; at 120 columns with facets, ~476 MB. The bound does not bound.

The doc's counter-argument ("a byte budget on ~500-byte objects is a more
complicated way to express the same limit") only holds if entries really are
~500 bytes. They are 1.9-47 KB in memory. Bound bytes, or bound entries *and*
cap per-entry column count.

### The entry-count bound is also cross-tenant, and the fix makes that worse

`introspected_schemas` is one map per **OS thread**, keyed `"{app}:{coll}"`,
shared by every app on that thread (`context.rs:141-148` documents ~200 isolates
per thread). An LRU over it evicts *across tenants*.

Combined with `cache_every_collection`, one cold read by one app inserts T
entries atomically. A single 500-table tenant's first op can evict most of a
10,000-entry budget shared with 199 co-resident tenants. That is a
one-line-of-app-code metadata-cache DoS on neighbours, and it did not exist
before this commit inserted more than one entry per read.

### The isolate-cache tie: the anchor is itself unbounded

The doc proposes deriving the bound from the worker isolate LRU
(`:1163-1170`). The granularity objection in the brief does not hold - both
`AppCache` (`cache.rs:56-58`) and `ISOLATE_CTX` (`context.rs:952`) are
thread-locals on the same OS thread, reached from the same compio runtime. They
are the same granularity.

The real problem is that **half the isolate cache is not bounded**:

- `cache.isolates` is bounded: `load_app` checks
  `cache.isolates.len() >= cache.max_size` (`cache.rs:531`) and evicts.
- `cache.workflow_isolates` (`cache.rs:23`) is **not**.
  `load_pinned_workflow_app` (`cache.rs:524-570`) checks only
  `pinned_count_for_app(cache, &app_id) >= cache.max_pinned_isolates_per_app`
  (`:552`) - a **per-app** cap. There is no check against `max_size`, and
  `evict_lru` (`:735`) iterates `cache.isolates` only, never
  `workflow_isolates`. `evict_pinned_lru_for_app` (`:798`) is only ever called
  from the per-app loop above.

So `workflow_isolates` grows as `max_pinned_isolates_per_app x (distinct apps
with workflow replay on this thread)` with no global ceiling. Each entry is a
full V8 isolate - orders of magnitude more memory than a schema-cache entry.
Deriving the metadata bound from "the isolate bound" inherits an anchor that
does not bound.

Even for the bounded half, `max_size x typical_collection_count` is a bound on
the *average*, not a bound. One 500-table tenant defeats it.

### A concrete defect the isolate-cache tie surfaces: the deploy token is not per-isolate

`mint_db` (`v8_classes/db.rs:326-331`) writes
`set_deploy_token(app_id, token)` from `ZEROSHIP_DEPLOY_ID`. `build_runtime`
(`cache.rs:433-435`) injects `deploy_hash` - for a pinned workflow isolate,
"the run's pinned deploy hash", explicitly a different value
(`cache.rs:432-433` comment).

`deploy_tokens` is keyed by **app_id only**, in a map shared by the whole thread
(`context.rs:296`, `:670-673`). A live isolate for app X (deploy B) and a pinned
workflow isolate for app X (deploy A) co-reside on one thread by construction -
`cache.isolates` keyed by app, `cache.workflow_isolates` keyed by (app,
deploy_hash). Last mint wins the slot for both.

Effect: every pinned-isolate mint flips the app's token, so the *live* isolate's
next op sees a token mismatch on every entry and re-introspects the whole schema
- exactly the cost `e8218c4c3` set out to remove, reintroduced once per pinned
mint. Workflow replay churn thrashes it. Content is not corrupted (both isolates
introspect the same live catalog); the cost is.

Same shape one level down: `introspect_schema.rs:84` reads the token, `:100`
awaits `read_live_schema`, `:121` writes entries with the token variable
captured before the await. A mint on the same thread during that await stamps
every entry with a token nothing will ever read again - a wasted three-query
read plus T permanently dead entries.

### CHWBL

CHWBL (`gateway/src/proxy.rs:50`) picks a worker **node**. Inside a node,
`main.rs:648` is `.workers(workers_count)` - ntex distributes connections across
worker threads with no app affinity. So one app hot on a 16-thread worker has 16
isolates, 16 `IsolateDbContext`s, 16 copies of its metadata, and 16 independent
whole-schema catalog reads per deploy. See 4.1.

---

## 3. The measurement

Reproduced: `MEASURED narrow: 8 cols -> 273 bytes (34 b/col)`, `typical: 16 ->
551 (34)`, `wide: 40 -> 1391 (34)`.

**The three shapes produce an identical 34 b/col.** The loop's varied dimension
cannot distinguish anything - column count scales the numerator and denominator
together by construction. Three labels, one data point. Varying width was the
wrong axis; the axis that matters is *what a column carries*.

**The real in-memory multiplier**, measured independently at
`/tmp/jsonsize` under the workspace's exact serde_json features: **6.5-6.9x**,
`size_of::<serde_json::Value>() == 72`, `size_of::<String>() == 24`. The new
`measure_value_memory_overhead` (`introspect_schema.rs:458-497`) reports 7.0x
and 72 B - independently corroborated, and its slightly higher figure is the
safe direction. Two accounting notes for whoever sets the bound from it: it
adds `string_sz` for a `Value::String`'s payload on top of the 72-byte `Value`
that already contains the `String` header (over-counts 24 B per string value),
and it omits `IndexMap`'s raw hash-index table (under-counts). The two roughly
cancel; my independent walk lands 2-5% below theirs.

**The fixture is not representative in the one dimension that matters.** Every
column is plain `text`. The cache exists to hold `encrypted` (mode/keyId/wraps)
and `mask` (kind/classification) blocks. Measured, those columns cost **396-420
B/col in memory vs 226-235 B/col plain - 1.75x**. The doc's caveat 2 (`:1136`)
says "a real schema with those stores more per column" without a number; the
number is 1.75x and it is one line of fixture away.

**Would I set a bound from this? No.** What I would measure instead:

1. the same probe over a real `RuntimeSchemaDescriptor` from `examples/starter`
   and `examples/db-todos`, i.e. schemas someone actually wrote;
2. per-*app* totals, not per-collection - after `cache_every_collection` the
   unit of insertion is the app's whole table set, so the entry is the wrong
   grain to bound;
3. RSS delta around N inserts, not a structural walk - the structural figure is
   a floor that excludes allocator rounding and IndexMap slack, and both
   understate, which is the dangerous direction for a ceiling.

---

## 4. What nobody has looked at

### 4.1 `read_live_schema` is O(total tenants on the cluster), measured

Every cold-start schema read scans a **global** catalog, not this tenant's slice.

`pg_class` has exactly three indexes (measured, PG 16.14):
`pg_class_oid_index`, `pg_class_relname_nsp_index` (`relname` leading), and
`pg_class_tblspc_relfilenode_index`. **Nothing is indexed on `relnamespace`
alone**, so a query filtered only by namespace cannot seek.

Measured on `zs-catscale-probe` (`postgres:16.14`), the three queries copied
verbatim from `diff.rs`, `EXPLAIN (ANALYZE, BUFFERS)`, `ANALYZE` run first:

| | 2000 tenants | 4000 tenants | ratio |
| --- | ---: | ---: | ---: |
| `pg_class` rows | 64,413 | 128,413 | 2.0x |
| **columns query** | Seq Scan `pg_class`, 64,413 rows, 1,606 buf, **17.4 ms** | full Index Scan of `pg_class_relname_nsp_index`, 919 buf + 934 disk reads, **10.1 ms** | plan flips; neither seeks |
| **indexes query** | Seq Scan `pg_index`, 32,102 rows filtered, 700 buf, **4.6 ms** | Seq Scan `pg_index`, **64,102** rows filtered, **1,395** buf, **10.6 ms** | **2.0x rows, 2.0x buffers** |
| **FK query** | Seq Scan `pg_constraint`, 16,112 filtered, 329 buf, **1.7 ms** | Seq Scan `pg_constraint`, **32,112** filtered, **656** buf, **6.8 ms** | **2.0x rows, 2.0x buffers** |

Rows scanned and buffers touched double exactly with tenant count. This returns
**8 tables / 112 columns** either way. Aggregate at 4000 tenants: **~27.5 ms
execution + ~4 ms planning per cold-start schema read**, and the tenant returns
the same 112 columns at any scale.

Note the indexes query's shape: the `pg_index` seq scan runs *before* the
namespace filter (`Join Filter: (c.relnamespace = n.oid)`), so every tenant's
cold start scans every other tenant's indexes.

Extrapolating (an extrapolation, not a measurement) 250x to 1e6 tenants puts one
cold-start schema read in the multi-second range, and it is paid per (app,
deploy, **worker thread**) - 16 threads x however many worker nodes CHWBL spills
to. `e8218c4c3` removed the `N_collections` factor and left the `N_threads`
factor and this one entirely.

What actually fixes it: stop reading `pg_catalog` on the data plane. The
migration service already knows the applied schema; publish it (the design's
epoch-keyed `LiveAppSchemaFacts`, `:1195-1206`) and have the runtime read one
row keyed by app, not the cluster catalog. Short of that, `estimate_row_count`
(`diff.rs:878-884`) shows the seekable shape: `nspname = $1 AND relname = $2`
can use `pg_class_relname_nsp_index` properly.

Side observation from the same probe: creating 2000 app schemas in one
transaction dies with `out of shared memory / max_locks_per_transaction`. Not my
lane, but per-app schema provisioning has a transaction-scoping constraint.

### 4.2 Every db op costs 4 round trips and a server-side parse+plan; the driver's statement cache is off

`exec.rs:293-350`, the single funnel for every autocommit CRUD op:

1. `pool.get()` (`:299`)
2. `client.transaction()` (`:312`) - sends and awaits `START TRANSACTION`
3. `tx.simple_query(&setup_sql)` (`:322`) - `SET LOCAL` role + timeouts,
   rebuilt per op by `auth/bootstrap.rs:1502-1509` though it depends only on
   `app_id`
4. `tx.query_text_params(...)` (`:331`)
5. `tx.commit()` (`:340`)

Four network round trips for one `find`.

`query_text_params` (`libs/compio-postgres/src/query.rs:156-193`) uses the
**unnamed** statement - `frontend::parse("", query, ...)` on every call - so the
server parses, rewrites and plans the SQL every time. The driver has
`prepare_cached` (`libs/compio-postgres/src/prepare.rs:308`); plugin-db never
calls it, and `statement_cache_capacity` defaults to **0**
(`libs/compio-postgres/src/config.rs:833`) with nothing in plugin-db setting it.

The pool is `Pool::connect(&url, 8)` (`plugin-db/src/lib.rs:872`) - **8
connections per worker thread**, shared by ~200 co-resident isolates. Holding a
connection 4x longer than necessary divides achievable ops/sec per thread by ~4
against that fixed 8.

SQLite mirrors it: `backend/sqlite/session.rs:901` and `:965` use
`conn.prepare`, not `prepare_cached`, recompiling every statement.

This is the single largest constant-factor tax in the db path and no commit on
this branch touches it.

### 4.3 `updateMany` on a randomised-encrypted collection: uncapped SELECT, then one transaction per row

`crud/mod.rs:1273`:

```rust
write_pipeline::resolve_target_row_ids(&route, &coll, &filter, None).await
```

The `None` is the limit. It reaches `build_find` (`write_pipeline.rs:336-345`),
and `query.rs:3161-3163` emits `LIMIT` only when `limit.is_some()`. So this is
`SELECT id FROM app.coll WHERE <filter>` with **no LIMIT** - the entire matching
set streams into the worker heap.

The DB-2 comment at `crud/mod.rs:618-623` claims exactly this cannot happen:
"an omitted `limit` defaults to `MAX_QUERY_LIMIT` - never 'no LIMIT' (which
would stream the whole collection into the worker)". That guard lives in
`dispatch_find`'s option parsing; `resolve_target_row_ids` bypasses it.
`dispatch_update_one` passes `Some(1)` (`crud/mod.rs:1017-1022`) and is fine.
`updateMany` is the only uncapped caller.

Then `crud/mod.rs:1307-1362` loops per row: clone the patch (`:1310`),
`write_pipeline::apply` (`:1311`, a full schema deep clone each), rebuild SQL
(`:1332`), and `await exec_mutation_with_emit` (`:1350`) - a separate 4-round-trip
autocommit transaction per row. Outside an explicit `db.transaction()` each row
commits independently, so a mid-loop failure leaves a partially applied
`updateMany` and returns a rejection.

### 4.4 One replication slot + one walsender + one dedicated connection per (app, worker); the wall is 10, not 1e6

`replication.rs:107-115` - `worker_slot_name` composes
`__zs_slot_<sha(app)>__<sha(worker)>`, per **(app x worker process)**, because
"a logical slot can have only one active consumer" (`:100-106`).
`replication.rs:208-212` creates it with `temporary=false`.
`wal_consumer.rs:368` opens a **dedicated non-pooled** replication connection.
`cdc_lifecycle.rs:87-111` refcounts leases per app with **no cap on apps**.

Measured on this branch's own dev database (`zs-dbbind-pg`):
`max_replication_slots=10`, `max_wal_senders=10`, `max_connections=100`.
`deploy/compose/docker-compose.yml:117-121` passes only `wal_level=logical` and
`max_prepared_transactions=10` - slots and senders left at the default.

The 11th (app, worker) pair to open a subscription fails
`pg_create_logical_replication_slot`. `max_replication_slots` is a restart-only
shared-memory GUC and each active slot is a walsender **backend** competing for
`max_connections`. There is no tuning that makes one slot per tenant work at
1e6.

Reclamation: the clean path drops the slot when the last lease goes
(`cdc_lifecycle.rs:130-136` -> `change_stream_pg.rs:217-222`). A crashed worker
leaves `active=false` with `restart_lsn` pinned, so Postgres cannot recycle WAL
past it and `pg_wal` grows until the **whole cluster** dies - every tenant. The
reaper `drop_abandoned_slots` (`replication.rs:466-499`) is reachable only from
tenant JS via `db.replication.dropAbandoned()` (`replication_ops.rs:49-68`);
grep across `crates/zeroship-control/src` and `crates/zeroship-worker/src` finds
no caller. There is no operator-side watchdog.

Mitigating: slots are demand-driven - only `collection.openSubscription()`
reaches `acquire` (`v8_classes/subscription.rs:315`), so apps that never use
live queries cost nothing. The bound is *concurrently subscribed* apps.

### 4.5 The per-app subscription cap has zero production callers

`broker.rs:151` - `MAX_SUBSCRIPTIONS_PER_APP: usize = 256`, enforced only in
`Broker::try_subscribe` (`broker.rs:490-528`).

The only production mint site calls the **infallible** variant:
`v8_classes/subscription.rs:315`, `let broker_sub = broker::subscribe(app_id, collection);`.
Grep for `try_subscribe` across `crates/` returns `broker.rs` itself and its own
unit test (`broker.rs:1068-1073`). `broker.rs:452-454` even documents the gap.

So `for(;;) db.users.openSubscription()` is unbounded. Each iteration allocates
a `VecDeque` that grows to `DEFAULT_QUEUE_DEPTH = 1024` events
(`broker.rs:144`), a `CdcLease`, and a slot in the **process-global** routing
table (`static BROKER`, `broker.rs:788`) that every publish walks. One tenant
degrades every co-resident tenant in the process.

The DB-12 test passes against `Broker::try_subscribe` directly. The cap is green
and dead simultaneously.

### 4.6 Read-set narrowing is `#[cfg(test)]`; every subscriber gets every row change

`read_set.rs:317` - `#[cfg(test)] pub struct Active`, the only thing that sets
`CURRENT_BUFFER` (`:299-303`, `Active::begin` at `:326`, also `#[cfg(test)]`).
So `record_if_active`, called from three production sites
(`crud/mod.rs:616`, `:1764`, `:1913`), always returns at its `!is_active()`
guard. `Subscription::set_read_set` (`broker.rs:267`) has no non-test caller, so
`accepts` always takes `broker.rs:283-285`'s `else { return true }`.

The routing table itself is correctly indexed by `(app, collection)`
(`broker.rs:578-583`) - not a linear scan over all apps. But because narrowing
never engages, a 10k-row UPDATE fans 10k full events to every subscriber on the
collection, each carrying two `HashMap<String,String>` row images
(`exec.rs:531-541`, `wal_consumer.rs:635-636`).

### 4.7 `MAX_INSERT_MANY_BATCH` caps documents, not binds - and the comment's claim is false

`query.rs:598-601`: the cap is `1_000` documents, with the comment "stays well
under Postgres' 65535-bind-param wall".

`build_insert_many_with_dialect` pushes one bind per **non-null cell**
(`query.rs:4090-4114`). 1000 docs x 66 non-null columns = 66,000 binds.

The wall is exactly 65535 and it is enforced client-side:
`postgres-protocol-0.6.12` `write_counted` does `u16::from_usize(count)?`
(`message/frontend.rs:108`) - it errors rather than truncating. So a legitimate
`insertMany` of 1000 wide documents fails in the driver with a parameter-count
error. The threshold is **66 non-null columns per document** at the full batch;
nothing in the builder counts binds.

### 4.8 Per-op and per-row allocation on the hot path

- Every `runtime_schema_for` call costs, before any SQL: `format!` in
  `is_model_registered` (`context.rs:560`), a cloned `String` in
  `deploy_token_for` (`context.rs:681`), a `format!` in
  `introspected_schema_for` (`context.rs:638`), and a **full deep clone** of the
  schema `Value` (`context.rs:640`). Measured clone cost: **4 heap allocations
  per column** (161 for a 40-column table, 865 for a 120-column table with
  facets). The write path takes it by reference (`WriteStages::new(schema.as_ref())`,
  `write_pipeline.rs:114-115`) - the clone there is pure waste. The fix should
  be `Rc<Value>`.
- `insertMany` deep-clones the declared schema **per document** -
  `system_fields_pass.rs:243-258` calls `prefix_for_collection` inside the
  per-doc loop. 1000 docs = 1000 whole-schema deep clones to read one
  per-collection id prefix.
- `encryption/keys.rs:327`: the cache **hit** path is
  `cache.get(&(app_id.to_string(), key_id.to_string()))` - two heap Strings
  allocated per lookup, and the lookup runs per (row x encrypted column)
  (`encryption_pass.rs:336` inside `read_pipeline.rs:362-372`). 500 rows x 3
  columns = 3000 throwaway Strings per find.
- `aead.rs:120` and `:146` call `Aes256Gcm::new_from_slice(&key.k_enc)` **per
  value** - a full AES-256 key schedule plus GHASH table init for every column
  value. For short plaintexts this plausibly dominates the encryption; I did not
  benchmark it, so treat the ranking as unmeasured.
- `query.rs:3363-3380` rebuilds the read projection per op: ~3 `String`
  allocations per column plus a `join`, producing a byte-identical string for
  every call with the same `(schema, unmask_set)`.
- Result sets cross into V8 as four full materializations - PG buffer ->
  `serde_json::Value` (`v8_bridge.rs:455-468`, one key `String` per cell) ->
  one JSON `String` (`crud/mod.rs:531`) -> V8 string ->
  `v8::json::parse` (`runtime/src/core/runtime.rs:2936-2940`). Masked reads add
  ~9,000 V8 string allocations for four literal key names per 500x3 find
  (`masked_value.rs:753-758`, a bare `v8::String::new`, never interned).

### 4.9 Process-global mutexes on every mutation

`static BROKER` (`broker.rs:788`), `static SUPPRESSED_APPS` (`wal_consumer.rs:72`),
`static MANAGER` (`cdc_lifecycle.rs:60`). Every successful CRUD mutation, every
app, every isolate, every thread takes two of them before doing anything
(`exec.rs:501-502`), and `emit_local` takes both again (`wal_consumer.rs:152-163`).
`matching_subscriptions` holds `BROKER` across `retain` **and** every
`Subscription::accepts`, each of which takes a second per-subscription mutex
(`broker.rs:282`).

The comment at `exec.rs:472-474` still describes the broker as "thread-local"; it
has been process-wide since `broker.rs:780-788`.

### 4.10 Smaller, verified

- `find(..., {unmask:[...]})` is O(rows x columns) sequential single-row
  transactions: `unmask.rs:1366-1401` loops rows x columns, each calling
  `fetch_and_decrypt`/`fetch_plaintext_parent` (`unmask.rs:470-476`) which runs
  a fresh 4-round-trip autocommit transaction. At `MAX_QUERY_LIMIT = 500` x 2
  columns that is 1000 sequential queries / 4000 round trips for one `await` -
  and the ciphertext was already decrypted in-process by
  `encryption_pass.rs:335-349` before being re-fetched.
  `lookup_encryption_meta` (`unmask.rs:216`) deep-clones the whole collection
  schema per cell.
- No negative cache for mask policy: `unmask.rs:335-338` gates on
  `has_mask_policy`, and a storage miss installs nothing
  (`unmask.rs:346-349`), so every unmask on an app **without** a policy - the
  default case - re-issues `SELECT __zeroship_admin.get_mask_policy($1)` and
  then default-denies anyway.
- Advisory-lock keys are 32-bit: `backend/lock_guard.rs:186-187` uses
  `hashtext($1)::int4`. At 1e6 apps roughly `n^2/2^33` ~ 100 app pairs collide
  and serialise each other's cold starts across tenants.
- Vector search dimension is uncapped: `crud/mod.rs:2202-2227` never consults
  the column's `vectorDims`; the only bound is `MAX_DECODE_NODES = 200_000`
  (`v8_bridge.rs:155`), and `query.rs:4941-4952` renders the literal with one
  `to_string()` allocation per element.

### 4.11 The worker commit does not do what its message says

`22c4d75f1` says: "so two isolates on one thread share a backend instead of each
resolving their own."

`DbPlugin` holds `url`, `worker_id`, `meter` and nothing else
(`plugin-db/src/lib.rs:311-321`). The pool and `BackendHandle` live in the
thread-local `IsolateDbContext` (`context.rs:120-121`, `:329`), installed by
`init_pool_async`. Two `DbPlugin` instances on one thread **already** shared one
backend and one cache - `register()` writes the same thread-local either way.

What the commit actually saves, per runtime build (not per request): ~5 `Arc`
allocations, ~6 `String` clones, one `Redis::new`, and one
`zeroship_plugin_storage::build_backend` (`cache.rs:230-239`) - the last of
which constructs an S3 client and is the only non-trivial item. That is a real
if modest saving. The test's own doc comment concedes the point ("this is the
first, behaviour-neutral half of that"); the commit message does not.

---

## 5. Arms that cannot pass or cannot fail

1. **`measure_cached_entry_size` (`introspect_schema.rs:428-444`) and
   `measure_value_memory_overhead` (`:457-497`) contain no assertion and
   cannot fail.** Both are labelled MEASUREMENT, which is honest. The problem is
   downstream: the design doc's two evidence tables (`:1108-1112`, `:1120-1124`)
   are transcribed from their stdout, and nothing checks the tables against the
   code. `measure_cached_entry_size` would still print and pass if
   `build_runtime_schema` returned `None` (`serde_json::to_string(&None)` is
   `"null"`, 4 bytes) or an empty object. If the schema shape changes, both
   tests stay green and both doc tables silently go stale. Assert a range, or
   emit the numbers from a golden file the doc includes.

2. **`measure_cached_entry_size`'s three shapes cannot distinguish anything.**
   Verified by running: `34 b/col` for all of narrow/typical/wide. Column count
   scales numerator and denominator together. Three labels, one data point; the
   axis that would vary the answer (encrypted/mask facets) is held constant.

3. **`internal_tables_are_not_cached_as_collections` (`:367-383`) tests half a
   predicate, and the other half cannot fire.** The `__zs_` arm of
   `introspect_schema.rs:143` can never match: every `__zs_` name in the tree is
   a replication slot or publication (`replication.rs:107-115`), never a table,
   and `relkind = 'r'` excludes matviews. Dead branch, untested.

4. **`one_read_stamps_one_token` (`:389-415`) asserts a property of the function
   signature, not of the system.** `cache_every_collection` takes `token: &str`
   by value, so "every entry from one read carries the same token" is
   structurally impossible to violate; no mutation of the body could make the
   miss-under-`deploy_b` assertion fail. The stated hazard - "a redeploy landing
   mid-populate" - is a *cross-call* race between `introspect_schema.rs:84`
   (read token), `:100` (await), `:121` (write), and a co-resident isolate's
   `mint_db`. That race is real (section 2) and this test cannot see it. The
   added HIT half is a genuine improvement to the *other* half of the test and
   the comment explaining why is correct.

5. **`plugin_set_is_shared_per_thread_and_invalidated_by_init_cache`
   (`cache.rs:954-...`) is a sound test of a claim the commit message
   overstates.** Both halves can fail (`a` is still live when `c` is compared,
   so addresses cannot be recycled). But pointer identity of plugin prototypes
   does not establish "two isolates share a backend" - see 4.11.

6. **The negative-cache regression (section 1) has no arm at all.**
   `missing_collection_is_none` tests `build_runtime_schema`; nothing tests that
   `runtime_schema_for` caches a negative for an absent collection, which is why
   deleting that behaviour was green.

---

## 6. Argument against my own most consequential finding

My top finding is 4.1: `read_live_schema` is O(total tenants) and the cold-start
fix leaves that factor plus the per-thread factor untouched.

The strongest case against it:

- **The catalog is heavily cached.** At 2000 tenants the columns query was
  `shared hit=2364, read=7` - essentially all buffer-cache hits, 17 ms of pure
  CPU walking cached pages, not I/O. A 1e6-tenant `pg_class` would be ~3 GB;
  with enough `shared_buffers` it stays resident and the cost stays CPU-bound
  and predictable rather than falling off a disk cliff.
- **My extrapolation is an extrapolation.** I measured two points, 2000 and
  4000. Between them the columns query *changed plan* (seq scan -> full index
  scan) and got **faster** in wall time (17.4 -> 10.1 ms) even as it doubled in
  rows. Wall time is not linear in tenant count over the range I measured; only
  rows-scanned and buffers-touched are. Anyone quoting "multi-second at 1e6"
  from my numbers is quoting me beyond my data.
- **The fix is still correct and still a large win.** It removes a factor of
  `N_collections` from a cost I am arguing is large. Multiplying a large cost by
  1 instead of 20 is worth doing whether or not the base cost also needs fixing.
- **Millions of apps on one Postgres cluster is not the only deployment.**
  If tenants shard across clusters at, say, 10k per cluster, the measured
  4000-tenant figure (~27.5 ms) is close to the real steady state, and 27.5 ms
  once per (app, deploy, thread) is defensible.

What survives the argument: the *shape* is O(total tenants on the cluster), the
2x scaling of rows and buffers is measured and unambiguous, and nothing in the
db path bounds tenants per cluster. That makes it a sharding constraint that is
currently undocumented, which is worth knowing even if the millisecond figures
move.

I would rank 4.2 (four round trips + no statement cache + 8 connections per
thread) as the more certain win: it is a pure constant factor on **every** op,
it needs no scale assumption to matter, and it is measurable today.

---

## Cleanup

The throwaway probe container `zs-catscale-probe` (port 5471) is still running
if anyone wants to re-check the plans; `docker rm -f zs-catscale-probe` when
done. It touched no existing database. `/tmp/jsonsize` holds the memory probe.
