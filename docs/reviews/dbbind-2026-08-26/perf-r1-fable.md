# Perf review r1 (fable) — feat/dbbind-impl scale/performance work

Reviewed at HEAD `fb383e98c` ("test(db): measure the in-memory multiplier"), which
landed MID-REVIEW (00:52) after the brief was written; the parent design doc was
revised in the same window (the 7x table at
`docs/proposals/2026-08-26-runtime-db-binding-design.md:1116-1135` did not exist
when I first read the file). All quotes below are against the current tree.
Note: `main` already contains the worker commit — `git merge-base main HEAD` =
`22c4d75f1`, so the branch proper is the three db/migrate commits plus fb383e98c.

All line cites verified by me against this worktree.

---

## 1. The cold-start fix

**The brief's claim is verified.** `read_live_schema` has no table predicate:
`WHERE n.nspname = $1 AND c.relkind = 'r'`
(`crates/zeroship-schema/src/diff.rs:620-644`), three sequential catalog queries
per call (columns :620, FKs :768, indexes :817). The pre-fix code cached only
the requested slice (`git show e8218c4c3~1:...introspect_schema.rs`, the
`cache_introspected_schema(app_id, collection, &token, schema.clone())` line),
so N collections cost N whole-schema reads per thread per deploy. The fix's
populate-all loop is real (`introspect_schema.rs:133-152`).

### F1 (top finding): the fix DELETED negative caching — a registered collection absent from the live catalog now costs 3 whole-schema catalog queries on EVERY op, forever

Old code cached `schema.clone()` where `schema` could be `None`
(`build_runtime_schema` returns `None` for an absent table,
`introspect_schema.rs:178-179`), and `introspected_schemas` explicitly
distinguishes the negative entry from a missing one — the field doc still says
so (`context.rs:275-278`: "the inner Option distinguishes ... so a goodie-free
collection is cached as a negative result rather than re-introspected every
call"). New code caches ONLY tables present in `live.tables.keys()`
(`introspect_schema.rs:139-151`); `runtime_schema_for` never writes an entry
for a requested collection the read did not cover (`:104-123`). So for a
collection that passes the `is_model_registered` gate (`:76`) but is absent
from the catalog, every single op is: miss (`:87-91`) -> full 3-query
whole-schema read (`:100`) -> populate everything except the requested key ->
return None. Next op repeats.

When does "registered but absent" happen: (a) the first-deploy window —
`read_live_schema` "returns an empty LiveSchema if the schema itself doesn't
exist yet (first deploy)" (`diff.rs:597-599`), which caches NOTHING; (b) a
rolling deploy where a migration dropped a table while old-code isolates still
register it; (c) any descriptor/DDL drift. The cost lands on the shared
`pg_catalog` of the one platform cluster, so a handful of hot apps in this
state hammer every tenant's catalog. The module doc at :17-20 is now stale — it
still promises the negative-result caching the commit removed.

Fix is one line: after the populate loop, if the requested collection got no
entry, cache `(token, None)` for it.

**Against my own finding (required):** the trigger states are drift states the
platform elsewhere makes hard: registerModel is not creator-reachable
(`register_model/mod.rs:55-58`), it comes from the generated descriptor folded
from the same migrations that create the tables, and migrations apply at deploy
before serving. In the common case the fix is strictly better. I keep it as the
top finding anyway because (i) the first-deploy empty-schema window is routine,
not drift; (ii) the failure mode is per-op catalog scans with cluster-wide blast
radius, i.e. it converts one app's stale state into everyone's problem; and
(iii) the pre-fix behaviour bounded the same states at one read per thread per
deploy, so this is a regression introduced by the perf commit itself, invisible
to every test on the branch (see §5).

### F2: no singleflight on the miss path

Two concurrent cold ops (same thread, interleaved at the await) both miss and
both run the 3-query read — nothing marks introspection in progress
(`introspect_schema.rs:87-123`; the only singleflight in the context is
`backend_init_in_progress`, `context.rs:338`). A cold app's first request burst
of K concurrent ops costs K whole-schema reads per thread. The parent doc's own
end-state Caches section mandates a per-thread singleflight
(`runtime-db-binding-design.md:1170-1172`); the landed interim has none.

### F3: `deploy_tokens` is keyed by app_id only, and pinned workflow isolates share the slot

`mint_db` stamps `set_deploy_token(app_id, token)` from the runtime's
`ZEROSHIP_DEPLOY_ID` (`v8_classes/db.rs:326-331`); the worker sets that env var
to the PINNED deploy hash for workflow-replay isolates
(`worker/cache.rs:433-435`), which are loaded into the same thread-local cache
as the current isolate (`cache.rs:56`, `PinnedWorkflowKey` :29-32) and share the
same per-THREAD `ISOLATE_CTX` (`context.rs:952-956`). So building a pinned
isolate overwrites the app's one token slot with the old hash, invalidating the
app's entire introspection cache (token mismatch, `context.rs:639-641`), and the
next op re-reads the whole schema and re-stamps everything under the old hash.
Each mint of a different-hash isolate for the same app = one spurious full
catalog read + full re-populate. Not a data-staleness hole (introspection always
reads the single live catalog), but the "deploy-keyed invalidation" identity is
wrong for multi-deploy threads — the SC-5 identity work
(`(authority_domain, app_id, incarnation, epoch)`) needs to reach this map too.

### F4: populate-all + any future LRU bound = self-inflicted eviction pressure

`cache_every_collection` inserts the app's ENTIRE collection set on any single
miss, including collections never requested and not yet registered. One cold
tail app with many collections inserts its whole footprint at once (at the real
~10 KB/entry, §3, an app with 100 collections is ~1 MB in one op). Under the
doc's proposed entry-count LRU, CHWBL spill traffic (one-hit apps) is classic
scan traffic — LRU's worst case — evicting hot apps' metadata wholesale. If a
bound lands, eviction should be grouped per app (outer map by app, LRU over
apps), which also matches the doc's own end-state signal.

Also minor: the requested collection's schema is built twice per miss
(`:104` and again inside the loop at `:146`).

### SQLite arm: no regression

The SQLite path returns before the populate loop (`:96-98`) and gets no benefit;
`sqlite_fallback_schema` unchanged.

---

## 2. The cache-bound design (doc §"Every per-app cache MUST be bounded")

### F5: the census of unbounded maps is incomplete — it lists 4, I count at least 5, and the deprovision path can clear none of them

The doc's table (`runtime-db-binding-design.md:1070-1075`) lists
`introspected_schemas`/`deploy_tokens`/`schemas`/`mask_policies`. Missing:
**`registered_models: HashSet<String>`** keyed `"{app_id}:{collection}"`
(`context.rs:134-136`), whose only removal is `cfg(test/test-helpers)`-gated
(`context.rs:574-577`; production callers of `clear_model_registered` and
`clear_schemas_for_app` are test helpers only, `lib.rs:286-287,589-596`). Same
growth law: one entry per (app, collection) a thread has ever served.

Also unpriced: on the PG path every registerModel stores the full DECLARED
schema JSON into `schemas` (`register_model/mod.rs:100-104`) while the read/
write pipelines use the INTROSPECTED copy — the thread holds ~2x per collection
(the declared copy IS still read: unmask lookups `crud/mod.rs:187-234`,
system-fields pass `:126`, tx-view `v8_classes/transaction.rs:77`). The doc's
byte arithmetic sizes only `introspected_schemas`.

And the doc's owed "eviction on deprovision, which the tombstone already
provides the signal for" (`:1099-1100`) has no delivery mechanism: the maps are
`thread_local!` on N worker threads; the deprovision path runs on the
process-wide version poller and clears nothing in any context
(`deprovision_app_cdc`, `lib.rs:907-921`). A tombstone signal must fan out to
every thread's `ISOLATE_CTX`, a channel that does not exist. The honest
resolution is the doc's own SC-5 note (`:1147-1150`): make the cache
process-wide first, then bound it — bounding N thread-local copies is interim
work on a structure the design intends to delete.

### F6: entry-count-only is the wrong unit because per-entry size is unbounded and 2.5x larger than the doc's constant

The doc argues entries are "small individually" and a byte budget "on ~500-byte
objects is a more complicated way to express the same limit" (`:1156-1160`).
Two problems. First, that ~500 B sentence is a leftover from the pre-revision
text and contradicts the SAME section's new 3.8 KB figure (`:1143`) — a stale
number carried forward within one revision. Second, the real live cost is
~10 KB/typical entry (measured, §3), and per-entry size is unbounded: PG allows
1600 columns/table; at the measured ~600 B/col live, one entry can be ~1 MB. An
entry bound alone admits bound x 1 MB per thread. The end-state SC-5 cache
states its bound "in both entries and bytes" (`:1166-1168`) — the interim bound
should too, or at least cap columns-per-entry.

### F7: deriving the bound from the isolate LRU is the wrong coupling, and the "eviction signal" end state defeats the branch's own cold-start fix

`max_isolates` defaults to 200 PER THREAD (`worker/config.rs:129`; `AppCache`
is `thread_local!`, `cache.rs:56`). The doc's "isolate bound times a typical
collection count" (`:1135-1137`) imports an unmeasured "typical" multiplier in
the same section that demands "nothing here should carry a byte number that was
not measured" (`:1101-1103`). Worse, the "cleanest version ... a signal: evict
an app's metadata when its isolate is evicted" (`:1140-1142`) throws away
exactly what makes the cold-start fix matter: under LRU churn and CHWBL spill
oscillation (`gateway/proxy.rs:97-113` walks the ring on saturation), isolate
evict-then-reload is the COMMON case at millions of apps, and 1:1 coupling
turns every reload into a fresh 3-query catalog read. Metadata (~10 KB/entry)
is orders of magnitude cheaper than an isolate; the correct relation is
metadata entries >> isolate entries, i.e. an independent, larger, byte-capped
bound. Isolate eviction is a fine PRUNING hint; it is not the right bound.

Mechanically the signal is deliverable for LRU eviction (evict_lru runs on the
owning thread, `cache.rs:735-781`, same thread as `ISOLATE_CTX`) — but nothing
today calls from eviction into plugin-db (verified: no such call in
`cache.rs`), and the deprovision arm cannot be (F5).

### CHWBL specifically

Spill seeds an app's metadata on arbitrary extra workers, and worker-side on
whichever of the threads serves the request — up to threads x copies per app
per worker. The doc correctly notes SC-5's process-wide cache fixes the
constant factor (`:1147-1150`). What it does not note: spill is also the
access pattern that makes a per-thread interim LRU thrash (F4/F7).

---

## 3. The measurement — the branch's own number moved mid-review, and it is still wrong as a memory figure

`measure_cached_entry_size` reports serialized bytes (34 B/col, verified:
273/551/1391 B at 8/16/40 cols). The mid-review commit `fb383e98c` added
`measure_value_memory_overhead` and the doc now states "**7x, flat across
shapes**" (`runtime-db-binding-design.md:1126`), sizing the interim bound from
it: "~10,000 entries per thread caps the cache near 38 MB" (`:1151-1152`).

**I measured the real thing.** Counting global allocator (tracks
`Layout::size` on alloc/dealloc/realloc), serde_json pinned to the workspace's
resolved `=1.0.149` with `preserve_order` (workspace `Cargo.toml:53`,
`Cargo.lock`), same shape `build_runtime_schema` emits, harness at
`scratchpad/memmeasure/`:

| shape | serialized | their "structural" | live heap (measured) | real ratio |
| --- | ---: | ---: | ---: | ---: |
| 8 cols | 273 B | 1,912 B (7.0x) | 4,800 B | 17.6x |
| 16 cols | 551 B | 3,830 B (7.0x) | 9,584 B | 17.4x |
| 40 cols | 1,391 B | 9,590 B (6.9x) | 22,336 B | 16.1x |
| full entry, 16 cols (map key + 64-char token + table slot) | — | — | ~9,959 B/entry over 1000 entries | ~18x |

The 7x "structural" count (`introspect_schema.rs:458-497`) counts only
`sizeof(String)` + key len + `sizeof(Value)` per node. It omits what the
allocator actually pays: with `preserve_order`, EVERY per-column `{type: ...}`
object is its own IndexMap — a hashbrown RawTable allocation plus a bucket Vec
(104 B/slot: hash+String+Value) at minimum capacity for one entry, plus the
outer map's table and capacity slack. That is ~360 B/col of real allocations
against the ~100 B they count. The commit message states "The multiplier is 7x"
unqualified; the doc calls 7x "a floor ... but a much tighter one" and then
uses it as the sizing constant anyway — sizing a bound from a self-declared
floor, which the doc itself names as "the dangerous direction" (`:1134-1136`).

Corrected arithmetic: the proposed 10,000-entry interim bound is ~96-100 MB per
thread, ~1.6 GB per 16-thread worker — not 38 MB. The "100,000 distinct apps
=> ~1.9 GB per thread" projection (`:1146-1148`) is ~4.8-5 GB. My figure is
itself a floor (requested bytes, not malloc bins/RSS).

Would I set a bound from the branch's number: no. The measurement to take is
the one I took — a counting allocator (or RSS delta) across N entries inserted
through `cache_introspected_schema`, including key + token + table overhead —
and it should live in the tree the same way the serialized test does, with a
band assertion so it can fail (see §5). The fixture is otherwise reasonable but
omits encrypted/mask facets (doc concedes) and the fact that PG threads hold a
second declared-schema copy per collection (F5).

---

## 4. What nobody looked at (highest value)

### F8: every cache HIT deep-clones the whole schema Value — the steady-state hot path pays ~10 KB and ~50 allocations per op

`introspected_schema_for` returns `Some(schema.clone())` (`context.rs:640`) —
a full deep clone of the Value (measured: 9,584 B live for 16 cols) on EVERY
read op (`crud/read_pipeline.rs:65-68`) and every write op
(`crud/write_pipeline.rs:114`); updates and upserts resolve it TWICE per op
(`write_pipeline.rs:367,379` then `:114`). `schema_for` likewise `.cloned()`
per unmask lookup (`context.rs:618`). Plus 3-4 `format!` key allocations per
op (`context.rs:560,566,638,657`) and a token String clone (`:680-686`). The
cold-start fix optimizes the miss path; the hit path — the one every request
takes — allocates a fresh copy of the metadata it exists to amortize. Fix:
store `Rc<Value>` in the per-thread map (no Send needed; `ISOLATE_CTX` is
thread-local) and hand out refcount bumps. The SC-5 end state already says
"immutable plain data behind Arc" (`design.md:1164-1166`); the interim can have
it today in one line-of-shape change.

### F9: `db.transaction()` scans EVERY (app, collection) the thread has ever served, and deep-clones every schema of the calling app just to throw the clones away

`mint_tx_view` (called per `env.db.transaction(fn)`,
`transaction/mod.rs:711`) calls `cached_schemas_for_app`
(`v8_classes/transaction.rs:77`), which iterates the ENTIRE per-thread
`schemas` map filtering on a string prefix and `.clone()`s every matching
Value (`context.rs:696-708`) — and the caller keeps only the NAMES
(`transaction.rs:79-82`, `map(|(name, _schema)| name)`). Cost per transaction:
O(total entries on the thread — unbounded, grows for the thread's lifetime)
string compares, plus (app's collections x ~10 KB) of allocations discarded
immediately. This is quadratic-with-uptime work on the transaction-open path;
it is invisible at 100 apps and dominates at 10^5+ distinct apps per thread.
Fix: keep a per-app collection-name index (or key the map (app -> coll ->
schema)), and return names without cloning values.

### F10: replication slot cardinality is the un-named wall in the db path

Change streams create a logical replication slot per (app, worker process)
(`replication.rs:108,144-160`; `cdc_worker_id` doc, `context.rs:128-132`). PG
slots are a hard, cluster-global resource: each holds a walsender + decoder
and pins WAL at its `restart_lsn`; the slowest slot pins WAL for the cluster.
At 1M apps, even 0.1% adoption x W worker processes is thousands of slots —
beyond any tuned `max_replication_slots`, and the WAL-pinning failure mode is
cluster-wide disk exhaustion. Neither the parent doc nor any of SC-1..SC-6
addresses slot cardinality (checked: every "slot" hit in the doc set is a
transaction or V8 slot; the CDC sections cover epochs and projections, not
fan-out). This needs a shared-decoder/fan-out design (one slot per cluster or
per worker, demuxed per app) before change streams can be offered at scale.

### F11: the whole-schema read's own cost grows with TOTAL platform size

Schema-per-app on one cluster (AGENTS.md "One database, separate schemas")
means 1M apps -> pg_class ~10^7 rows, pg_attribute ~10^8-10^9. `read_live_schema`
is 3 catalog joins with an `ORDER BY c.relname, a.attnum` sort and a correlated
pg_depend/pg_proc subquery per column (`diff.rs:620-644`). Index lookups keep
the per-app row set small, but per-backend catcache/relcache growth, catalog
bloat/autovacuum, and the pinned pool connections touching millions of
relations are cluster-level costs the doc never measures. The chosen direction
(whole-schema read + cache all) beats N reads only while the whole-schema read
stays cheap; a `WHERE c.relname = ANY($2)` predicate variant is the natural
fallback and nothing on the branch measured either at representative catalog
size. The doc's "MEASURED against PostgreSQL 16.14 in a dedicated container"
runs were on a near-empty catalog.

### F12: deprovision opens a fresh 2-connection pool per deleted app

`deprovision_app_cdc` runs `Pool::connect(db_url, 2)` per deletion from the
version poller (`lib.rs:907-921`). At platform-scale churn that is a constant
connect/auth/TLS load against PG. SC-5 names it (`sc5:23-26`); nothing on the
branch fixes it.

### F13 (hygiene): the napi fix left the tracked generated file stale and has no guard

`262519f23` fixed `wire.rs` but did not commit the regenerated `index.d.ts`;
the worktree shows it modified (uncommitted diff includes the `*\/` removal and
unrelated `zero_migrate::` -> `zeroship_migrate::` link renames). A clean
checkout that builds dirties the tree, and nothing asserts the generated d.ts
parses — the next `*/` in any napi doc comment reproduces the same hidden
failure (no regression test for the fix).

### Adjacent identity bug (same family as the worker env-cache find)

`clear_pool` (`context.rs:478-482`) drops pool+backend but leaves
`introspected_schemas`/`schemas`/`registered_models`/`deploy_tokens` intact, so
a `set_db_url` swap (`:538-544`) serves metadata introspected from the PREVIOUS
database under the same (app, token) identity. Unreachable on the prod worker
(URL fixed per process); latent for CLI/dev vectors and any future multi-URL
work.

---

## 5. Arms/tests that CANNOT PASS or CANNOT FAIL

1. **The commit's headline property has no arm that can fail.**
   "an app with N collections pays one read on cold start" is tested nowhere:
   `one_read_populates_every_collection`, `one_read_stamps_one_token`, and
   `internal_tables_are_not_cached_as_collections`
   (`introspect_schema.rs:346-415`) all call `cache_every_collection` directly.
   If `runtime_schema_for` stopped calling it — or re-grew a per-collection
   read some other way — all three stay green; no test counts reads. And the
   property is currently FALSE on the registered-but-absent arm (F1), which no
   test can see either. The in-code comment concedes counting queries "would
   need a pool" (`:129-132`); the pool-backed suites exist elsewhere in this
   crate and one arm there should count `read_live_schema` executions.
2. **Both measurement tests cannot fail** (`measure_cached_entry_size`
   `:429-444`, `measure_value_memory_overhead` `:458-497`): println-only, no
   assertion. Self-declared, but the doc's tables are hand-copied outputs of
   one run; when the entry shape grows (encrypted/mask facets, extra fields)
   nothing goes red and the doc's bound arithmetic silently detaches from the
   code. A band assertion (serialized b/col and live-multiplier within stated
   bounds) makes drift visible; as shipped these tests also let the WRONG
   multiplier (7x, §3) into the design doc without any instrument able to
   object.
3. **Doc-level:** the parent doc's requirement "a bound ... stated as a number
   the way the lease retry policy is" (`:1095-1096`) is satisfied by the
   revision with "~10,000 entries ... near 38 MB" — a number derived from a
   floor measurement that is 2.5x low (§3). The arm as stated ("a number")
   passes while the property it wants (a bound an operator can size memory by)
   fails. SC-5's acceptance arms are notably well-armed against their own
   cannot-fail modes (`sc5:135-161` names the thread-local trap explicitly);
   no complaint there from this lane.

---

## Disposition of the four commits

- `e8218c4c3` (populate-all): the asymptotic claim is real, but it ships F1
  (negative-cache regression, one-line fix required), F2 (no singleflight), and
  interacts badly with any future entry-LRU (F4).
- `22c4d75f1` (plugin set per thread, already on main): no perf concern found;
  the test can fail and does test the invalidation half.
- `526b19e37` + `fb383e98c` (measurements): the serialized figure is honest;
  the 7x memory multiplier is wrong as a memory figure (real ~17-18x measured
  with a counting allocator) and the doc's bound arithmetic built on it
  understates the interim cache by ~2.5x.
- `262519f23` (napi comment): F13, stale tracked artifact + no guard.

Harness for the memory measurement: `scratchpad/memmeasure/` (this scratchpad),
serde_json `=1.0.149` + `preserve_order` matching the workspace resolution.
