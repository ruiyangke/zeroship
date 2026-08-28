# Review: scale/performance work on `feat/dbbind-impl`

The review target is frozen at the four commits requested: `22c4d75f1` through
`526b19e37`, on base `516b1416d`. While this review was in progress, another
writer advanced the shared worktree to `fb383e98c` and then `ee1255697`; those
follow-ups are outside this report. Every committed-code citation is therefore
qualified as `526b19e37:path:line`, rather than referring to the moving worktree.

The cited untracked proposal bytes are identified by sha256:

- `runtime-db-binding-design.md`: `da432db595588c4499126adb4fcf0e4f671c7902b3e32d495dbe631045f90535`
- `sc3-dbplan-ir-and-ledger.md`: `9b982dec78d3562103787437f7886f893de01903e272939e7de33fe3a1615968`
- `sc5-service-ownership.md`: `39f2d98903102d140f768a8feba6c62e99da59bde5b057256172334c2951686c`

Findings are ordered by production consequence, not commit order.

## 1. P0: the tenant-scoped catalog query is O(all relations in the database)

The `nspname = $1` predicate prevents cross-tenant rows from being returned, but
it does not give PostgreSQL 16 an index path to seek directly to one tenant's
relations. The column query joins through `pg_class` and constrains namespace but
not relation name (`526b19e37:crates/zeroship-schema/src/diff.rs:620-644`). In
PostgreSQL 16, the relevant catalog index is ordered
`(relname, relnamespace)`, with `relnamespace` second
([PostgreSQL `REL_16_STABLE` `pg_class.h`, index declaration](https://github.com/postgres/postgres/blob/REL_16_STABLE/src/include/catalog/pg_class.h#L158-L160)).
PostgreSQL 16's own multicolumn-index rule says that a predicate only on a
non-leading column requires the entire index to be scanned, and the planner will
usually choose a sequential table scan instead
([PostgreSQL 16, Multicolumn Indexes](https://www.postgresql.org/docs/16/indexes-multicolumn.html)).

The namespace lookup itself can be cheap, but finding the matching `pg_class`
rows is therefore O(all relations in this database), not O(relations in this app).
The FK and index arms repeat the same namespace-through-`pg_class` enumeration
(`526b19e37:crates/zeroship-schema/src/diff.rs:763-788`,
`526b19e37:crates/zeroship-schema/src/diff.rs:816-837`). With the platform's
schema-per-app layout, every cold metadata resolution becomes more expensive as
unrelated apps add tables, indexes, sequences, partitions, and TOAST relations to
the same database. Populate-all changes how many times one app repeats the work;
it does not change this database-cardinality term.

Do not make a runtime cold path enumerate `pg_catalog` by namespace at this
scale. The deploy/migration authority should publish typed, epoch-keyed runtime
facts into an operator-owned table or artifact indexed by full app identity. If
live verification remains mandatory, use the descriptor's expected relation
names so lookups constrain both `(relname, relnamespace)` and refuse unexpected
or missing relations; querying `information_schema` merely wraps the same
catalogs.

### Counterargument against this finding

Catalog pages will usually be memory-resident; populate-all amortizes a completed
scan over an app's collections; CHWBL tends to keep a hot app on one worker; and
database sharding could cap relation count. Those reduce constants or frequency.
They do not change the PostgreSQL 16 access path, and no shard-size invariant is
enforced in this code. The target workload is dominated by long-tail cold apps,
the cache is evicted, deploy epochs invalidate entries, and thread/worker spill
duplicates resolution. Warm catalog buffers turn the failure into database CPU
and memory-bandwidth saturation rather than removing it.

## 2. P0: the nominal per-app cold-load endpoint rebuilds the entire platform map

An isolate miss is already O(total platform apps) before plugin-db can perform
schema introspection:

- Every dispatch miss calls `load_on_demand` (`526b19e37:crates/zeroship-worker/src/handler.rs:277-286`),
  which calls `fetch_app_version` (`526b19e37:crates/zeroship-worker/src/handler.rs:1398-1404`).
- That helper calls the ostensibly per-app `/internal/apps/{id}` endpoint
  (`526b19e37:crates/zeroship-worker/src/sync.rs:466-471`).
- The endpoint does not query one app. It calls `Registry::get_versions()` and
  only then looks up the requested UUID in the resulting map
  (`526b19e37:crates/zeroship-control/src/internal.rs:143-168`).
- `get_versions` selects every row from `zeroship.apps` with no predicate
  (`526b19e37:crates/zeroship-control/src/registry.rs:516-531`), separately selects every
  egress rule (`526b19e37:crates/zeroship-control/src/registry.rs:532-542`), builds the
  all-app rule map (`526b19e37:crates/zeroship-control/src/registry.rs:543-572`), then
  parses every manifest and constructs every `AppVersionInfo`
  (`526b19e37:crates/zeroship-control/src/registry.rs:573-622`).

At one million apps, one long-tail request performs two platform-wide result-set
materializations, parses all manifests, allocates an all-app `HashMap`, serializes
one entry, and discards the rest. The worker's default current-isolate limit is
only 200 per thread (`526b19e37:crates/zeroship-worker/src/config.rs:128-134`), so a
million-app long tail guarantees recurring misses; CHWBL affinity cannot turn a
200-entry LRU into a million-entry cache. Concurrent cold requests independently
invoke the same control handler.

The periodic version path is a second scale cliff. Each worker process downloads
and deserializes the full `VersionMap` every default five seconds
(`526b19e37:crates/zeroship-worker/src/config.rs:120-122`,
`526b19e37:crates/zeroship-worker/src/sync.rs:128-210`), and every ntex thread then deep-clones
that whole map every reconcile interval (`526b19e37:crates/zeroship-worker/src/sync.rs:213-235`).
The work performed after the clone needs only env-cache keys and locally resident
app ids (`526b19e37:crates/zeroship-worker/src/sync.rs:299-335`). Thus the present shape is
O(apps x worker processes) wire/deserialization work and O(apps x threads) clone
work every five seconds.

Required change: implement an indexed `get_app_version(id)` query, including an
`app_id` predicate for egress rules. On-demand loading should first consult the
process snapshot and use the indexed endpoint only as a freshness fallback. The
periodic path needs a shared `Arc` snapshot consumed by reference immediately,
then a sharded/delta protocol before the platform approaches this cardinality;
deletion tombstones can preserve the reason the full snapshot exists today.

### Counterargument against this finding

The full snapshot gives simple deletion detection and avoids one poll per hot app,
and the code has already made its HTTP poller process-wide
(`526b19e37:crates/zeroship-worker/src/sync.rs:74-113`). CHWBL normally keeps repeat traffic
warm (`526b19e37:docs/architecture/distributed.md:42-45`). Those facts justify one shared
snapshot or delta feed. They do not justify rebuilding that snapshot inside the
per-id endpoint, cloning it once per thread, or assuming affinity eliminates
misses once distinct apps exceed the configured LRU by four orders of magnitude.

## 3. P0/security: registration absence means “serve without protection”

The `is_model_registered` gate is not merely a cold-start optimization. It is a
protection switch whose false arm fails open:

- `runtime_schema_for` returns `Ok(None)` before consulting either cache or the
  live catalog when the app/collection mark is absent
  (`526b19e37:crates/zeroship-plugin-db/src/crud/introspect_schema.rs:64-78`).
- Creator code can mint a native collection for any non-empty name; there is no
  registration or descriptor check (`526b19e37:crates/zeroship-plugin-db/src/v8_classes/db.rs:110-139`).
- The find path gets its SQL projection from the separate declared-schema cache
  (`526b19e37:crates/zeroship-plugin-db/src/crud/mod.rs:676-697`). With `schema=None`, the
  query contract emits `SELECT *`; only a present mask schema substitutes
  `"<col>_masked" AS "<col>"` (`526b19e37:crates/zeroship-schema/src/query.rs:2911-2928`,
  `526b19e37:crates/zeroship-schema/src/query.rs:3000-3012`). For a mask-only column, that
  exposes the plaintext parent instead of the masked sibling.
- The post-read path performs decryption/mask wrapping only inside the
  `Some(schema)` arms (`526b19e37:crates/zeroship-plugin-db/src/crud/read_pipeline.rs:62-100`).
  The write pipeline likewise turns absent metadata into a no-op
  (`526b19e37:crates/zeroship-plugin-db/src/crud/write_pipeline.rs:111-115`,
  `526b19e37:crates/zeroship-plugin-db/src/crud/write_pipeline.rs:204-221`).

Consequently a raw/schema-less app that knows the name of an already migrated
masked table can bypass the protected projection, and an unregistered write can
bypass encryption/mask transforms. Table-name secrecy is not a security boundary.
Populate-all does not itself reveal cache contents to an unregistered caller—the
gate precedes the lookup—but that ordering is exactly why populating all tables
does not repair this path.

Pre-launch permits the direct fix: descriptor/authority membership should decide
whether a collection is addressable, and an addressable collection must resolve
live protection metadata or fail closed. “Not registered” cannot mean
“unprotected.”

## 4. P0: populate-all enumerates physical partitions as collections and omits the logical table

The brief's basic catalog claim is verified, but it hides a relation-kind defect.
The column query is namespace-wide—its only tenant predicate is
`n.nspname = $1`, with no table-name predicate
(`526b19e37:crates/zeroship-schema/src/diff.rs:620-644`). However it also requires
`c.relkind = 'r'` and does not exclude `c.relispartition` at those same lines.
It therefore includes physical child partitions and excludes a partitioned
parent (`relkind = 'p'`).

The repository's correct creator-table enumeration is
`relkind IN ('r', 'p') AND NOT c.relispartition`
(`526b19e37:crates/zeroship-migrate-server/src/publication.rs:15-23`). Partitioned creator tables
are supported by the public migration authoring path
(`526b19e37:sdks/migrate/src/ops.ts:3054-3063`) and emitted as PostgreSQL `PARTITION BY`
(`526b19e37:crates/zeroship-migrate-postgres/src/ddl.rs:504-520`); this is not an operator-only
relation shape.

The new helper blindly caches every key in `LiveSchema.tables`
(`526b19e37:crates/zeroship-plugin-db/src/crud/introspect_schema.rs:133-151`), while
`LiveSchema` carries no relkind/partition-child field with which to correct the
enumeration (`526b19e37:crates/zeroship-schema/src/diff.rs:201-218`). A logical table with P
partitions therefore consumes P cache entries for physical tables no creator
registers, omits the creator-visible parent, and returns no runtime metadata for
the parent (`526b19e37:crates/zeroship-plugin-db/src/crud/introspect_schema.rs:178-195`).
For an encrypted partitioned parent this returns no decryption/type metadata; for
an unregistered parent the fail-open path in finding 3 still applies. It also
causes the repeated catalog path in finding 5.

A runtime-specific introspector should enumerate logical creator relations, not
reuse the migration diff snapshot. At minimum it needs `('r','p')` plus
`NOT relispartition`, an explicit internal-table predicate, and a hard error when
an expected registered/declared relation cannot be enumerated.

## 5. P1: absent and concurrent misses still repeat the whole schema walk

The patch removes the requested-key negative cache. It computes the requested
`schema` (`526b19e37:crates/zeroship-plugin-db/src/crud/introspect_schema.rs:100-104`) but
only inserts names that exist in `live.tables`
(`526b19e37:crates/zeroship-plugin-db/src/crud/introspect_schema.rs:133-151`). The cache API
explicitly represents `Some(None)` as a current negative result
(`526b19e37:crates/zeroship-plugin-db/src/context.rs:621-659`). Before `e8218c4c3`, the miss
path inserted `schema.clone()` for the requested key even when it was `None`
(`e8218c4c3^:crates/zeroship-plugin-db/src/crud/introspect_schema.rs:104-110`).
A registered but absent table, partial restore, drift, or partitioned parent now
pays the full walk on every operation. An unregistered name exits at the earlier
gate instead.

There is also no singleflight. Cache lookup precedes the await and publication
follows it (`526b19e37:crates/zeroship-plugin-db/src/crud/introspect_schema.rs:86-122`), so
all simultaneous cold operations can observe the same miss. Native operations
are concurrently polled in `FuturesUnordered`
(`526b19e37:crates/zeroship-runtime/src/core/runtime.rs:174-191`,
`526b19e37:crates/zeroship-runtime/src/core/runtime.rs:2639-2658`). Populate-all removes
sequential N-collection reads after the first completes; it does not remove the
initial concurrent fan-out.

Finally, one `read_live_schema` is three namespace-wide database calls, not one
read: columns (`526b19e37:crates/zeroship-schema/src/diff.rs:620-649`), foreign keys
(`526b19e37:crates/zeroship-schema/src/diff.rs:763-788`), and indexes
(`526b19e37:crates/zeroship-schema/src/diff.rs:816-837`). Runtime conversion consults only
`live.tables` (`526b19e37:crates/zeroship-plugin-db/src/crud/introspect_schema.rs:178-195`),
so both FK/index result sets and most column-diff metadata are discarded. The
column query still pays the per-column correlated volatility subquery
(`526b19e37:crates/zeroship-schema/src/diff.rs:620-631`). A runtime-specific typed facts
query plus success-only singleflight is materially cheaper than caching more of
the migration diff result.

## 6. P0/correctness: deploy and registration identity are thread-global, so current and pinned runtimes contaminate each other

Names in this code call the state “per-isolate,” but its sole owner is one
`thread_local!` `IsolateDbContext` (`526b19e37:crates/zeroship-plugin-db/src/context.rs:952-957`).
Current and deploy-pinned runtimes intentionally coexist on one thread under
different hashes (`526b19e37:crates/zeroship-worker/src/cache.rs:21-39`,
`526b19e37:crates/zeroship-worker/src/cache.rs:524-573`). Yet:

- `deploy_tokens` is only `app_id -> token`
  (`526b19e37:crates/zeroship-plugin-db/src/context.rs:281-296`). Every runtime mints its DB
  wrapper by overwriting that one entry (`526b19e37:crates/zeroship-plugin-db/src/v8_classes/db.rs:313-330`,
  `526b19e37:crates/zeroship-plugin-db/src/context.rs:664-684`).
- `runtime_schema_for` reads that shared last-writer-wins token, not the active
  runtime's identity (`526b19e37:crates/zeroship-plugin-db/src/crud/introspect_schema.rs:80-90`).
- `registered_models` is keyed only by app and collection
  (`526b19e37:crates/zeroship-plugin-db/src/context.rs:134-136`,
  `526b19e37:crates/zeroship-plugin-db/src/context.rs:556-568`); its only remover is
  test/helper-gated (`526b19e37:crates/zeroship-plugin-db/src/context.rs:570-578`).
- Registration fast-returns on that stale mark before refreshing the declared
  schema (`526b19e37:crates/zeroship-plugin-db/src/register_model/mod.rs:71-104`), even
  though the declared cache holds information live introspection cannot recover,
  including typed-id prefixes (`526b19e37:crates/zeroship-plugin-db/src/register_model/mod.rs:152-166`).

If an old pinned runtime is minted last, current-runtime operations use the old
token and can consume or repopulate old-token metadata; if the current runtime is
minted last, the pinned runtime does the inverse. A redeploy of the same
app/collection can also skip installing its new declared hints because an older
isolate left the thread-global registration mark behind. The cache key must be
the full binding identity and the active binding must carry its token; it cannot
be recovered from thread-global app state.

The SQLite branch does not execute the new populate-all helper: absence of a PG
pool returns the declared `schema_for` directly
(`526b19e37:crates/zeroship-plugin-db/src/crud/introspect_schema.rs:93-98`,
`526b19e37:crates/zeroship-plugin-db/src/crud/introspect_schema.rs:154-161`). Thus
`e8218c4c3` does not directly regress its catalog behavior. It leaves SQLite on
the same thread-global stale registration/schema maps; the early registration
return also skips the SQLite attach path that otherwise runs later in that
dispatch (`526b19e37:crates/zeroship-plugin-db/src/register_model/mod.rs:79-104`,
`526b19e37:crates/zeroship-plugin-db/src/register_model/mod.rs:173-185`). A changed
SQLite/HMR schema can therefore keep the prior deploy's metadata.

The plugin-sharing commit does not supply the missing ownership boundary. Its
prototype cache is itself per-thread (`526b19e37:crates/zeroship-worker/src/cache.rs:170-202`),
whereas SC-5 specifies one process-wide `Arc<DbService>`
(`docs/proposals/2026-08-26-sc5-service-ownership.md:31-47`).
The object it now reuses owns only URL, worker-id, and meter configuration
(`526b19e37:crates/zeroship-plugin-db/src/lib.rs:310-321`); registration merely stamps the
already shared TLS context (`526b19e37:crates/zeroship-plugin-db/src/lib.rs:360-395`), and
that context still opens its one lazy backend/pool
(`526b19e37:crates/zeroship-plugin-db/src/lib.rs:845-897`). Thus this commit reduces
per-runtime plugin/vector/config allocation, but it does not change backend or
metadata-cache cardinality and cannot be counted as the SC-5 ownership cutover.

## 7. P0: isolate eviction is not a sound hard bound for DB metadata

The parent first argues for entry count instead of bytes
(`docs/proposals/2026-08-26-runtime-db-binding-design.md:1220-1224`), then mandates
both entries and bytes for the eventual process-wide cache
(`docs/proposals/2026-08-26-runtime-db-binding-design.md:1258-1266`). These are
different acceptance contracts. The latter is the defensible one: the future
entry is an entire `LiveAppSchemaFacts`, with per-collection slices
(`docs/proposals/2026-08-26-runtime-db-binding-design.md:383-388`), so one entry
can vary by orders of magnitude. “Isolate count x typical collections” is not a
hard upper bound; one pathological app can consume it.

The proposed isolate-eviction signal is useful reclamation information, but it
cannot be the memory bound:

- `max_size` bounds only the current-isolate map on one OS thread
  (`526b19e37:crates/zeroship-worker/src/cache.rs:21-26`,
  `526b19e37:crates/zeroship-worker/src/cache.rs:111-118`). The worker creates one such
  cache per ntex thread (`526b19e37:crates/zeroship-worker/src/main.rs:607-648`), defaulting
  to one thread per core (`526b19e37:crates/zeroship-worker/src/config.rs:22-30`).
- Pinned isolates live in a separate map and have only a per-app limit; the
  process-wide total across apps is unbounded
  (`526b19e37:crates/zeroship-worker/src/cache.rs:524-577`). Current and pinned eviction are
  independent (`526b19e37:crates/zeroship-worker/src/cache.rs:735-841`).
- CHWBL deliberately spills an app when its preferred worker is loaded
  (`526b19e37:crates/zeroship-gateway/src/proxy.rs:79-89`,
  `526b19e37:docs/architecture/distributed.md:42-45`), so fleet sizing cannot assume one
  metadata copy per app.
- The design explicitly permits old full-identity/epoch keys to coexist until
  cache eviction (`docs/proposals/2026-08-26-runtime-db-binding-design.md:1017-1022`).
  A hot isolate can therefore accumulate generations without generating an
  isolate-eviction event.

Use an independent hard LRU with both entry and retained-byte ceilings, plus an
explicit policy for a single entry larger than the byte budget. Treat
current/pinned lifecycle events as full-identity refcount decrements or eager
eviction hints, not as the only bound. Add a process-wide pinned-isolate ceiling.

The current “four maps” census also omits `registered_models`: the proposal lists
only schemas, introspected schemas, deploy tokens, and mask policies
(`docs/proposals/2026-08-26-runtime-db-binding-design.md:1109-1124`), while the
fifth app/collection history set is declared at
`526b19e37:crates/zeroship-plugin-db/src/context.rs:134-136` and has no production removal
(`526b19e37:crates/zeroship-plugin-db/src/context.rs:564-578`).

## 8. P1: the 34-byte serialization figure is not a memory measurement; a local allocator probe was 17–19x

`measure_cached_entry_size` serializes an `Option<Value>` and prints its length;
it asserts nothing and measures no live allocation
(`526b19e37:crates/zeroship-plugin-db/src/crud/introspect_schema.rs:417-444`). The fixture is
one collection of text-only fields with no encryption or mask facets
(`526b19e37:crates/zeroship-plugin-db/src/crud/introspect_schema.rs:424-438`). The workspace
enables serde_json `preserve_order` (`526b19e37:Cargo.toml:53`), so object maps carry
`IndexMap` storage, not a compact serialized-object representation.

At the requested tip there is no live-memory measurement at all; only the
serialization harness above. `sizeof(serde_json::Value)` on this build is 72 B,
and every object also carries map storage, keys, values, and allocator capacity,
so the serialized length cannot supply a defensible multiplier by itself.

I used a local, uncommitted x86_64-gnu scratch harness to reproduce the exact
text-only `Value` shapes. It constructed them single-threaded, subtracted the
pre-construction live-allocation baseline, and summed net-live glibc
`malloc_usable_size` after construction. This is diagnostic evidence, not a
repo-reproducible gate. The retained payload results were:

| fixture | serialized | allocator-usable heap | heap/column | multiplier |
| --- | ---: | ---: | ---: | ---: |
| 8 text columns | 273 B | 5,152 B | 644 B | 18.9x |
| 16 text columns | 551 B | 10,288 B | 643 B | 18.7x |
| 40 text columns | 1,391 B | 24,048 B | 601 B | 17.3x |
| 16 columns, each with encryption + mask facets | 2,583 B | 26,656 B | 1,666 B | 10.3x |

The faceted ratio is numerically lower only because serialization contains much
more payload; its absolute heap cost is 2.6x the plain 16-column case. These are
still floors. They exclude the outer `introspected_schemas` bucket, owned
app/collection key, deploy-token `String`, `HashMap` spare capacity, `Arc`/binding
overhead in the proposed shape, allocator-arena fragmentation, the transient
`LiveSchema`, and peak memory while values are cloned. Those omitted fields are
visible in the actual cache layout (`526b19e37:crates/zeroship-plugin-db/src/context.rs:257-312`)
and the hit path deep-clones the payload (`526b19e37:crates/zeroship-plugin-db/src/context.rs:632-643`).

I would not set any bound from 34 B/column or from the isolated numbers above.
Measure the final typed `LiveAppSchemaFacts` in the
real cache with a counting allocator plus DHAT/heaptrack (or equivalent): hold a
production-shaped distribution of apps, collections, widths, indexes, FKs,
encryption/mask facets, and long identifiers; include map capacity, identity
keys, `Arc`s, and allocator usable bytes; record steady retained bytes and peak
during population/cache-hit operations; report p50/p95/p99 plus an adversarial
maximum. Then enforce both an entry ceiling and a byte ceiling.

## 9. P0: transaction creation is O(all historical collections on the thread)

`cached_schemas_for_app` formats an app prefix, scans every entry of the global
per-thread `schemas` map, and deep-clones every matching `serde_json::Value`
(`526b19e37:crates/zeroship-plugin-db/src/context.rs:687-707`). Every transaction calls it,
then immediately discards the cloned schemas and retains only collection names
(`526b19e37:crates/zeroship-plugin-db/src/v8_classes/transaction.rs:66-88`). Because that
map has no eviction and spans all isolates on the thread, transaction-start time
is O(total collections ever seen on the thread), not O(collections in the active
app), with avoidable deep clones. At million-app history it adds a million-entry
cross-tenant scan to every transaction start before cloning the matching values.

Use a hierarchical app-identity map or the isolate binding's descriptor to
enumerate names. Do not scan across tenants or clone schema values to mint a list
of names.

## 10. P0/security: the process env cache retains secrets for every live app ever loaded

`SharedEnvs` is a process-wide `HashMap<Uuid, Arc<CachedEnv>>`
(`526b19e37:crates/zeroship-worker/src/sync.rs:81-98`), and each entry owns the complete
validated env JSON string (`526b19e37:crates/zeroship-worker/src/sync.rs:488-512`). Isolate
loading fetches and inserts that env before committing the runtime
(`526b19e37:crates/zeroship-worker/src/handler.rs:1422-1434`). Isolate
LRU eviction deliberately leaves it behind
(`526b19e37:crates/zeroship-worker/src/cache.rs:780-785`). The only central reclamation
retains every app that still exists in the control-plane version map and removes
only deleted apps (`526b19e37:crates/zeroship-worker/src/sync.rs:181-190`).

The internal endpoint returns the merged worker env
(`526b19e37:crates/zeroship-control/src/internal.rs:91-115`), whose store path
explicitly decrypts every secret before constructing that response
(`526b19e37:crates/zeroship-control/src/env_store.rs:353-390`). Memory and
plaintext-secret retention therefore track distinct currently existing apps ever
loaded by this worker process, not active isolates or the 200-entry/thread
LRU. A long-lived worker serving the million-app long tail converges toward one
full env/secret payload per app. Add a hard byte-and-entry bound and active
current/pinned-runtime reference tracking; retain the `Arc` for in-flight users,
but remove the map entry once no local runtime needs it. Deletion GC remains an
eager security cleanup, not the capacity policy.

## 11. P0: metering permanently retains every touched app and globally blocks DB metrics during full drains

The process-wide meter stores `RwLock<HashMap<String, AppCounters>>`
(`526b19e37:crates/zeroship-metering/src/meter.rs:133-140`). Contrary to its “lock-free”
comment, every increment acquires the global read lock and executes the callback
while holding it (`526b19e37:crates/zeroship-metering/src/meter.rs:170-189`). DB metrics
then acquire a per-app `Mutex<HashMap<...>>` and allocate the metric-name String
on every increment (`526b19e37:crates/zeroship-metering/src/meter.rs:69-92`). Plugin-db also
constructs an owned-app-id `MeterHandle` for each emission
(`526b19e37:crates/zeroship-plugin-db/src/context.rs:433-440`,
`526b19e37:crates/zeroship-metering/src/lib.rs:55-81`,
`526b19e37:crates/zeroship-plugin-db/src/exec.rs:75-85`).

Every drain takes the global write lock, walks every ever-touched app, and removes
none (`526b19e37:crates/zeroship-metering/src/meter.rs:205-254`). It runs on the default
ten-second cadence (`526b19e37:crates/zeroship-metering/src/outbox.rs:52`,
`526b19e37:crates/zeroship-metering/src/outbox.rs:741-752`). At million-app churn this is
permanent O(ever-seen apps) memory and an O(ever-seen apps) stop-the-world period
for all request and DB metric increments every ten seconds.

Store stable `Arc<AppCounters>` values behind a sharded/admission map so an
existing-app increment holds no global lock, give known DB metrics dedicated
atomic slots rather than a string map, bind the handle once per isolate/binding,
and reclaim inactive zero counter sets after a race-safe drain.

## 12. P0/security: the encryption key cache is another unbounded app cache and resolves per cell

`KeyStore` states that each `(app_id, key_id)` remains for its entire lifetime
(`526b19e37:crates/zeroship-plugin-db/src/encryption/keys.rs:48-55`) and implements that as
an unbounded owned-string `HashMap`
(`526b19e37:crates/zeroship-plugin-db/src/encryption/keys.rs:253-272`). Even a hit allocates
two temporary Strings and clones the key (`526b19e37:crates/zeroship-plugin-db/src/encryption/keys.rs:316-329`);
misses insert permanently (`526b19e37:crates/zeroship-plugin-db/src/encryption/keys.rs:350-355`).
Each `AeadKey` retains 64 bytes of key material before strings/map overhead and is
zeroized only on drop (`526b19e37:crates/zeroship-plugin-db/src/encryption/aead.rs:39-53`).
The store belongs to the backend (`526b19e37:crates/zeroship-plugin-db/src/backend/postgres.rs:35-49`,
`526b19e37:crates/zeroship-plugin-db/src/backend/postgres.rs:110-121`), which lives in the
thread-local DB context until backend reset/thread exit, not isolate eviction
(`526b19e37:crates/zeroship-plugin-db/src/context.rs:445-480`,
`526b19e37:crates/zeroship-plugin-db/src/context.rs:952-957`).

Reads invoke key resolution per encrypted column per returned row
(`526b19e37:crates/zeroship-plugin-db/src/crud/read_pipeline.rs:352-387`,
`526b19e37:crates/zeroship-plugin-db/src/crud/encryption_pass.rs:275-346`); writes resolve
per encrypted field (`526b19e37:crates/zeroship-plugin-db/src/crud/encryption_pass.rs:174-214`).
That is at least two key-lookup String allocations and one 64-byte key clone per
encrypted cell, plus permanent tenant-key retention for every encrypted app ever
seen on the thread.

Resolve the unique `(app,key)` set once per operation/batch, use borrowed lookup,
and use a bounded, zeroizing cache keyed by full app incarnation/authority
identity. Isolate/deprovision lifecycle should eagerly remove its entries, while
the independent byte/entry bound remains the backstop.

## 13. P1: warm CRUD replaces catalog reads with repeated deep clones and schema scans

On every registered DB operation, the warm path formats an app/collection key,
clones the deploy-token `String`, and deep-clones the cached `serde_json::Value`
(`526b19e37:crates/zeroship-plugin-db/src/context.rs:558-561`,
`526b19e37:crates/zeroship-plugin-db/src/context.rs:632-643`,
`526b19e37:crates/zeroship-plugin-db/src/context.rs:675-684`). Reads and writes call this
resolver per operation (`526b19e37:crates/zeroship-plugin-db/src/crud/read_pipeline.rs:62-70`,
`526b19e37:crates/zeroship-plugin-db/src/crud/write_pipeline.rs:111-115`). A projected read
then retains schema fields with `fields.iter().any(...)`, which is O(schema width
x projection width) (`526b19e37:crates/zeroship-plugin-db/src/crud/read_pipeline.rs:110-118`).
Write-stage construction independently scans the same schema four times
(`526b19e37:crates/zeroship-plugin-db/src/crud/write_pipeline.rs:184-201`). Update/upsert
route decisions can resolve it once before the main write pipeline resolves it
again (`526b19e37:crates/zeroship-plugin-db/src/crud/write_pipeline.rs:359-383`).

Store immutable typed metadata behind `Rc`/`Arc`, use structured/nested keys that
support borrowed lookup, precompute facet flags and field indexes, and pass one
resolved handle through the complete operation.

## 14. P1: isolate admission performs the expensive build before it can know the cache will reject it

`load_app` builds and initializes V8 first
(`526b19e37:crates/zeroship-worker/src/cache.rs:405-457`,
`526b19e37:crates/zeroship-worker/src/cache.rs:471-488`), then discovers that a full cache
whose entries are all leased cannot evict and rejects the runtime
(`526b19e37:crates/zeroship-worker/src/cache.rs:490-505`). Pinned loading has the same order
(`526b19e37:crates/zeroship-worker/src/cache.rs:524-560`). Under saturation, requests for
distinct cold apps can repeatedly compile/evaluate runtimes that can never enter
the cache. Preflight the “new app and no evictable slot” case before build, while
retaining the existing build-before-replace ordering for a last-good runtime of
the same app.

## Acceptance and test arms that cannot pass or cannot fail

### Cannot pass/compile as specified

1. **SC-5 cross-thread singleflight is jointly impossible with the parent.** SC-5
   requires two OS threads racing the same full identity to produce exactly one
   process-wide catalog resolution
   (`docs/proposals/2026-08-26-sc5-service-ownership.md:165-168`). The parent
   explicitly prescribes per-thread singleflight and accepts up to `n_threads`
   catalog walks (`docs/proposals/2026-08-26-runtime-db-binding-design.md:1258-1266`).
   A forced simultaneous miss cannot satisfy both arms. Choose process-wide
   reservation/wakeup or change SC-5's expected count.

2. **SC-5's pointer-identity arm cannot compile as written.** It asks for
   `Arc::ptr_eq` on current/pinned `DbThreadResources`
   (`docs/proposals/2026-08-26-sc5-service-ownership.md:178-182`), while the parent
   type is `Rc<DbThreadResources>`
   (`docs/proposals/2026-08-26-runtime-db-binding-design.md:420-430`) and SC-5
   explicitly keeps driver resources as non-Send `Rc`
   (`docs/proposals/2026-08-26-sc5-service-ownership.md:44-47`). This must be
   `Rc::ptr_eq`, or the arm must state that it compares the `Arc<DbService>`.

### Cannot fail on the stated production regression

1. **`one_read_populates_every_collection` bypasses the production path.** It
   constructs a `LiveSchema` and directly calls `cache_every_collection`
   (`526b19e37:crates/zeroship-plugin-db/src/crud/introspect_schema.rs:339-363`). Replacing
   the production call at `526b19e37:crates/zeroship-plugin-db/src/crud/introspect_schema.rs:120-122`
   with requested-only caching—or deleting the call while leaving the helper
   dead—keeps the test green. Its outer `.is_some()` also passes if every table is
   incorrectly cached as `None`. It needs a counting introspection seam that
   invokes `runtime_schema_for` for two sibling collections and asserts one
   catalog resolution plus correct inner values.

2. **`one_read_stamps_one_token` cannot exercise the race it describes.** It
   passes a constant token into a fresh local context
   (`526b19e37:crates/zeroship-plugin-db/src/crud/introspect_schema.rs:385-414`); there is no
   `mint_db`, shared TLS context, await, concurrent current/pinned runtime, or
   deploy transition. It stays green when the last-writer-wins token bug in
   finding 6 exists.

3. **The internal-table arm covers only one of the two production prefixes.**
   Production skips `__zeroship` and `__zs_`
   (`526b19e37:crates/zeroship-plugin-db/src/crud/introspect_schema.rs:139-145`), but
   `internal_tables_are_not_cached_as_collections` supplies only a
   `__zeroship_...` fixture
   (`526b19e37:crates/zeroship-plugin-db/src/crud/introspect_schema.rs:365-382`). Deleting
   the `__zs_` exclusion leaves it green.

4. **`missing_collection_is_none` cannot detect the lost negative cache.** It
   only calls `build_runtime_schema`
   (`526b19e37:crates/zeroship-plugin-db/src/crud/introspect_schema.rs:446-450`); it never
   looks up the cache twice or counts catalog reads.

5. **`measure_cached_entry_size` cannot fail on growth.** It is explicitly a
   print-only test with no assertion
   (`526b19e37:crates/zeroship-plugin-db/src/crud/introspect_schema.rs:417-444`). A
   print-only harness is not an acceptance gate for a bound or a footprint
   regression.

6. **`plugin_set_is_shared_per_thread_and_invalidated_by_init_cache` does not
   guard `build_runtime`.** It calls `plugin_set()` directly
   (`526b19e37:crates/zeroship-worker/src/cache.rs:953-985`) and never calls the production
   runtime builder. Reverting `build_runtime` to `create_plugins()` at
   `526b19e37:crates/zeroship-worker/src/cache.rs:405-445` leaves this test green. The arm
   must build current and pinned runtimes through their real paths and count
   service/plugin identity, factory opens, and backend opens separately.

7. **SC-3's deterministic-render arm cannot prove the performance claim.** SC-3
   says byte-stable SQL enables prepared-statement-cache hits
   (`docs/proposals/2026-08-26-sc3-dbplan-ir-and-ledger.md:452-467`) but accepts
   only byte-identical rendering
   (`docs/proposals/2026-08-26-sc3-dbplan-ir-and-ledger.md:640-642`). Current CRUD
   executes through `query_text_params`
   (`526b19e37:crates/zeroship-plugin-db/src/exec.rs:174-207`,
   `526b19e37:crates/zeroship-plugin-db/src/exec.rs:293-334`), which unconditionally sends
   unnamed Parse/Bind/Describe/Execute/Sync
   (`526b19e37:libs/compio-postgres/src/query.rs:146-232`). The driver's statement cache is
   entered only through the `ToStatement::Query` path
   (`526b19e37:libs/compio-postgres/src/to_statement.rs:25-45`,
   `526b19e37:libs/compio-postgres/src/prepare.rs:305-366`). Render equality can therefore
   pass while executed cache hits remain permanently zero. Add an execute-twice
   assertion over prepare count/cache-hit count, not only a renderer assertion.
