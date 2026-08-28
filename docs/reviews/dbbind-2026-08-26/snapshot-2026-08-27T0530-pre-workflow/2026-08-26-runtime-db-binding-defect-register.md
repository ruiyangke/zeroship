# Defect register: live defects in the code this design touches

Extracted from `2026-08-26-runtime-db-binding-design.md` on 2026-08-27.

**These are defects in EXISTING code**, found while designing and implementing
the runtime DB binding. They are not proposals and not design decisions. They
are listed together because several of them constrain the design, and because a
register that lives inside a design document makes both harder to read.

**Every entry was verified against the tree by the pilot**, not accepted from a
review. That distinction earned its place: several reviewer claims were correct
about the defect and wrong about its mechanism or its magnitude, and where that
happened the correction is recorded in the row.

**This is the most perishable document in the set.** Rows retire as fixes land -
five already have. It should never be read as design, and a row's absence from a
later revision means it was closed, not that it was wrong.

Read `2026-08-26-runtime-db-binding-00-index.md` first for what is settled, what
is open, and what blocks what.

---

## Live defects, to fix independently

These exist in `main` and do not depend on this design. Each needs a regression
test that fails on the pre-fix code.

| # | Defect | Evidence |
| --- | --- | --- |
| L1 | Mask policy forgeable from any bundled dependency via the global symbol registry, persisted durably | `sdks/db/src/policy.ts:85,91-94` |
| L2 | Mask policy suppressible via `_flushPendingMaskPolicy`, dynamically importable by any referrer | `bootstrap_modules.rs:73-80` |
| L3 | `__zsSchemaReady` shadowable by an accessor before assignment, so dispatch never awaits it | `init.rs:515-518` |
| L4 | CDC ships mask-only plaintext and companion columns, with no policy check and no audit row. The existing unit test asserts the **wrong property** and must be replaced, not extended: it hand-builds an encrypted-and-masked fixture and checks the frame round-trips it, so it rules on the fixture rather than the pipeline and never constructs the mask-only case | load-bearing lines are `mask_pass.rs:150-156` (v3 cited `:29-31`, which is only the module comment); `broker.rs:990`; the fixture-only test at `broker.rs:1852` |
| L5 **(FIXED - decode is now total; `DecodeError` at `v8_bridge.rs:165`, no silent `continue` survives)** | The V8 decoder elides a filter key whose getter throws, so `updateMany` can lose its tenant predicate | `v8_bridge.rs:250` |
| L6 | `__zeroship_admin` has no production provisioner, yet production code calls its functions and propagates the error | `bootstrap.rs:95-96`; `mask_policy.rs:268,286`; `keys.rs:495` names a migration that does not exist |
| L8 **(FIXED - landed with its regression test; `exec_terminal_on_tx`, `transaction/mod.rs:139`)** | PostgreSQL answers `COMMIT` with a `ROLLBACK` tag for a failed transaction; the driver detects it but plugin-db discards command tags, so a rolled-back commit reports **success** and the settle path then drains pending emits for writes the database discarded. **Scope the test to the explicit-transaction path** - autocommit already goes through the driver wrapper that checks the tag, so a test written there passes pre-fix and proves nothing | MEASURED: `BEGIN; SELECT 1/0; COMMIT;` -> server replies `ROLLBACK`; control (clean tx) replies `COMMIT`. Driver detects at `transaction.rs:186-188`; `backend/postgres.rs:201-212` returns `Ok(rows.len())`; explicit `COMMIT` is routed there by `transaction/mod.rs:1001` |
| L9 **(DECIDED 2026-08-27 - storage flip; see below)** | Masking is projection-shaped, so the filter path is an unaudited plaintext oracle. `build_where_with_dialect(filter, params, dialect)` takes **no schema hint** (`query.rs:5274-5281`), so it cannot know a column is masked; a masked column stores plaintext in the parent column, so `find({ssn:{$gt:"500-00-0000"}})` renders `WHERE "ssn" > $1` against plaintext. The caller never sees an unmasked value and does not need to - **the matching set is the answer**, and repeated queries binary-search the exact value with no authorization check and no audit row. Does NOT violate the letter of the audit guarantee (that covers `.unmask()` calls, which this is not) - it defeats the protection goal through a channel the guarantee never scoped. **DECIDED 2026-08-27: flip the storage.** `ssn` stores the MASKED value, `ssn_raw` stores the real one, and `ssn_raw` is **reserved and unqueryable** - not in a filter, not in a projection, not in a sort, not a field of the generated type. Plaintext is reachable only through an explicit API, where authorization and the audit row already live. This beats the three policy options SC-6 had recorded, all of which kept plaintext in the natural-named column and then policed the filter path on top of it - leaving the design fail-open, which is precisely how the defect arose. After the flip the ignorant path is the safe path, the `"ssn_masked" AS "ssn"` substitution is deleted rather than extended, and the guard becomes a **reserved-suffix check needing no schema** instead of a metadata lookup the filter builder does not have. It also makes the audit guarantee simply true rather than narrow-but-true: plaintext ends up with exactly one reader. What it owes: lookup BY plaintext must be supported by the explicit API or the feature is closed rather than secured; unique indexes and foreign keys must follow `ssn_raw`, since enforcing uniqueness over masks is a silent integrity failure; and the AAD binds the column name, so this is a migration-engine change too. Full rationale and the four owed items in SC-6 | `query.rs:5274-5281` (no hint); mask substitution is select-list-only at `query.rs:3351`, extended to aggregates by `aggregate_read_ident` at `:3412`; audit promise at `docs/reference/db.md` "Audit tables" |

| L10 **(FIXED 2026-08-27, `4c8e84134`; regression test proven red by mutation)** | The deploy-invalidation token is keyed **one level coarser than the isolates it protects**, and a misleading name hides it. `deploy_tokens: HashMap<String, String>` is `app_id -> token` (`context.rs:296`), but the worker deliberately keeps several isolates *of the same app at different deploys* alive on one thread, keyed `PinnedWorkflowKey { app_id, deploy_hash }` (`worker/src/cache.rs:28-32`) for deploy-pinned workflow replay - a documented invariant, not an accident. Every runtime overwrites the single shared entry when it mints its `Db` wrapper, so it is **last-writer-wins across deploys of one app**: mint the pinned runtime last and the *current* runtime reads the old token; mint the current one last and the pinned replay reads the new one. `runtime_schema_for` then serves or repopulates introspected metadata under the wrong deploy's key. The same coarseness lets a redeploy skip installing new declared hints, because `registered_models` is keyed `(app, collection)` with no deploy component and registration fast-returns on the stale mark. **What makes this hard to see is a name:** the struct is `IsolateDbContext` and its comment says "the per-isolate DB context", but it lives in a `thread_local!` (`context.rs:952-957`) and the worker runs many isolates per thread. Keying by `app_id` inside it would be correct if the name were true. The token must carry the full binding identity `(app_id, deploy_hash)` - which `PinnedWorkflowKey` already establishes one layer up - and the active binding must carry it, not recover it from thread-global app state | `context.rs:296` (key shape); `context.rs:952-957` (`thread_local!` vs "per-isolate"); `worker/src/cache.rs:28-32` (the finer key); `v8_classes/db.rs:313-330` (the overwrite); `crud/introspect_schema.rs:80-90` (the read) |

**All four per-app maps were re-read for keying, 2026-08-27.** None carries
deploy identity in its key: `deploy_tokens` is keyed by `app_id` alone;
`schemas` (declared) and `registered_models` by `format!("{app_id}:{collection}")`;
`introspected_schemas` by the same string, with the deploy token stored as part
of the *value* so staleness is detectable - but the token it compares against is
the app-keyed one, which is the bug. So `introspected_schemas` is the only map
that can even notice a deploy change, and it notices it against a value L10
shows is wrong.

**What the fix does, and what it deliberately leaves.** A new `DbBinding`
(`crates/zeroship-plugin-db/src/binding.rs`) captures `ZEROSHIP_DEPLOY_ID` from
the **active runtime's own environment** at `mint_db`, is stored immutably on the
`Db` and every `Collection` it mints, and is threaded through each asynchronous
CRUD continuation. The token is therefore never recovered from process-global
environment or app-keyed thread-local state. `IsolateDbContext` is renamed
`ThreadDbContext`, which removes the false name that hid the bug.

**The fix rests on one assumption the implementing agent flagged as unverified,
and it holds** - checked here rather than taken on trust: `build_runtime`
injects `ZEROSHIP_DEPLOY_ID` per runtime (`worker/src/cache.rs:433-435`), and
`load_pinned_workflow_app` passes `Some(deploy_hash)` - **its own** pinned hash,
the same one it keys `PinnedWorkflowKey` with - rather than the current deploy's
(`worker/src/cache.rs:524-540`). Had that been false the whole fix would be
inert, so it is the right thing to have named.

**Still app-keyed after this change**, and these are exactly the maps the
"cannot be re-keyed alone" note below predicted: `registered_models`, the
declared `schemas` cache, the SQLite declared-schema fallback, and mask-policy
caching. Only introspected runtime metadata is deploy-bound today. The class is
not closed; one member of it is.

**The identity is under-specified in a SECOND dimension: the database itself.**
`clear_pool` drops the pool and backend and nothing else (`context.rs:478-482`);
all four metadata maps survive it. So a `set_db_url` swap leaves
`introspected_schemas`, `schemas`, `registered_models` and `deploy_tokens`
holding metadata introspected from the **previous database**, served under the
same `(app, token)` key. Column types, encryption `keyId`/`wraps` and mask
classifications from database A would then be applied to rows in database B.

Scope it honestly, because the reviewer who found it did: this is **unreachable
on the production worker**, where the URL is fixed for the process lifetime. It
is latent for the CLI and dev vectors, and it becomes reachable the moment any
multi-URL work lands. Recorded not as a live exploit but because it shows the
shape of the error: the cache key answers "which app, which collection" and
gestures at "which deploy", while the thing that actually determines what the
metadata *describes* - which database it was read from - appears in the key not
at all. A fix that adds only the deploy component leaves this one standing.

**The misleading name is systematic, not one comment.** Besides
`IsolateDbContext` itself, `is_model_registered`'s doc says the model "has been
registered on **this isolate**" (`context.rs:557`) - same false claim, same
thread-local. Anyone auditing this file for the L10 bug reads three independent
assurances that the state is per-isolate, and all three are wrong in the same
direction. Renaming the type is therefore not cosmetic cleanup bundled into a
fix; it removes the thing that made the bug survive review.

**`registered_models` cannot be re-keyed on its own.** A review pass looking at
the obvious follow-up found that the declared `schemas` cache is keyed
`(app, collection)` with no deploy component either, so fixing only the
registration mark would still let one deploy's declared hints overwrite
another's - the same defect one cache to the left. This matters for scoping the
fix: L10 is not "add a deploy component to `deploy_tokens`", it is **carry the
active binding identity through every per-app metadata cache**, and a change
that touches one of them has not fixed the class. That is also the argument for
the hierarchical `app -> deploy -> {collection -> ...}` shape proposed in the
cache-bound section rather than a per-map patch: one place to key correctly
instead of three places to keep in sync.

L10 is worth reading beside the "bound to the wrong thing" pattern this
document keeps hitting. Its history is a fix that moved in the right direction
and stopped one step short: the comment at `context.rs:281-296` explains, at
length and correctly, that the previous `std::env::var("ZEROSHIP_DEPLOY_ID")`
read was process-global and "would have been WRONG for a multi-app worker
thread". It then keys the replacement by `app_id` - which fixes multi-*app*
sharing and leaves multi-*deploy* sharing of one app, the case the worker
explicitly supports, still broken. The reasoning that identified the flaw was
sound; it was applied to one of the two dimensions the key needed.

| L11 **(NEW - found while unblocking `pnpm build`; NOT caused by this design)** | **PostgreSQL full-text search has no producer anywhere in the tree, while three layers still present it as a feature.** The migration engine removed FTS outright - "Full-text support was removed from this engine, down to the `IndexMethod` variant... There is no `.fts()` facet to fold: the authoring surface has none, and no code path here produces either shape" (`zeroship-migrate-core/src/render/declarative.rs:1823-1830`), and a second site confirms "no `fts5` sentinel, no `.fts()` facet" (`:2971-2976`). It previously folded `.fts()` into a `__fts` GENERATED `tsvector` column plus a GIN index on PostgreSQL. Nothing replaced it: `tsvector` appears in the whole tree only in that removal comment, and there is no producer in plugin-db's PostgreSQL backend. Meanwhile `t.string().fts(language?)` is still callable and documented in `@zeroship/db` (`sdks/db/src/types.ts:1172`), `docs/reference/sqlite-divergences.md:14-15` documents PostgreSQL FTS as working and merely *differing* from SQLite ("`language` selects the `tsvector` configuration"), and SQLite full-text still works because plugin-db's runtime creates the FTS5 table itself (`backend/sqlite/fts.rs`) on a path that never touches the engine. So the divergence table describes a comparison between a working backend and a non-existent one. The removal's stated rationale cites `docs/proposals/fts-macro.md` - **which is not in the tree** | `zeroship-migrate-core/src/render/declarative.rs:1823-1830,2971-2976`; `sdks/db/src/types.ts:1172`; `docs/reference/sqlite-divergences.md:14-15`; `backend/sqlite/fts.rs`; absent: `docs/proposals/fts-macro.md` |

| L12 **(NEW - the hard scaling wall, verified 2026-08-27)** | **Live subscriptions cost one PostgreSQL logical replication slot per (app x worker process), and the default ceiling is 10.** `worker_slot_name` composes `__zs_slot_<sha(app)>__<sha(worker)>` (`replication.rs:108-115`) - per pair, because a logical slot admits only one active consumer - and `wal_consumer.rs:368` opens a **dedicated, non-pooled** replication connection for each. `cdc_lifecycle.rs` refcounts leases per app with **no cap on apps**. Measured on this branch's own dev database: `max_replication_slots=10`, `max_wal_senders=10` (the PostgreSQL defaults; `deploy/compose/docker-compose.yml` sets only `wal_level` and `max_prepared_transactions`). The 11th concurrently-subscribed pair fails `pg_create_logical_replication_slot`. `max_replication_slots` is a **restart-only** shared-memory GUC and every active slot is a walsender backend competing for `max_connections`, so **no tuning makes one-slot-per-tenant reach the platform's stated scale**. Mitigating and worth stating: slots are demand-driven - only `collection.openSubscription()` reaches `acquire` (`v8_classes/subscription.rs:315`) - so the bound is *concurrently subscribed* apps, not all apps | `replication.rs:108-115`, `:208-212`; `wal_consumer.rs:368`; `cdc_lifecycle.rs:87-111`; server GUCs measured directly |
| L12b **(availability, same finding)** | **A crashed worker's abandoned slot can take down the whole cluster, and only the tenant can clean it up.** The clean path drops the slot when the last lease goes, but a crash leaves `active=false` with `restart_lsn` pinned - PostgreSQL then cannot recycle WAL past it and `pg_wal` grows without bound, which is a **cluster-wide** failure affecting every tenant on it, caused by one tenant's worker dying. The reaper `drop_abandoned_slots` exists (`replication.rs:466-499`) but is reachable **only from tenant JS** via `db.replication.dropAbandoned()` (`replication_ops.rs:48-68`, `v8_classes/replication.rs:78`); verified 2026-08-27 that no control-plane or worker code calls it. Operator-side cleanup of an operator-side failure is delegated to the tenant, who has no reason to run it | `replication.rs:466-499`; `replication_ops.rs:48-68`; grep across control/worker finds no caller |
| L13 **(NEW - a cap that is green and dead at once, verified 2026-08-27)** | **`MAX_SUBSCRIPTIONS_PER_APP = 256` is enforced on zero production paths.** It is checked only inside `Broker::try_subscribe` (`broker.rs:490`), and the single production mint site calls the **infallible** `broker::subscribe(app_id, collection)` (`v8_classes/subscription.rs:315`). Verified by grep: `try_subscribe` occurs only in `broker.rs` itself and in doc comments. So `for(;;) db.users.openSubscription()` is unbounded, and each iteration allocates a `DEFAULT_QUEUE_DEPTH = 1024`-slot event queue plus a CDC lease plus an entry in the **process-global** routing table that every publish walks - so one tenant degrades every co-resident tenant in the process. The code documents its own gap at `broker.rs:452-454` ("New SDK call sites should prefer `try_subscribe`"), and the one new SDK call site does not. **The test passes because it calls `try_subscribe` directly**, which is the cannot-fail arm class exactly: the cap is green and dead simultaneously | `broker.rs:144,151,452-454,490`; `v8_classes/subscription.rs:315` |

| L14 **(NEW - a wide `insertMany` fails, and the comment says it cannot, verified 2026-08-27)** | **`MAX_INSERT_MANY_BATCH` caps documents while the wall it cites counts binds.** The constant is `1_000` documents and its comment justifies that number as staying "well under Postgres' 65535-bind-param wall" (`zeroship-schema/src/query.rs:597-600`). But `build_insert_many_with_dialect` pushes **one bind per non-null cell** (`query.rs:4090-4114`; NULLs are inlined as SQL literals and cost no bind). So a full batch of 1000 documents with 66 non-null columns each is 66,000 binds. The limit is exactly 65535 and is enforced **client-side, as an error rather than a truncation**: `postgres-protocol` (resolved to 0.6.12 in `Cargo.lock`, verified against that exact version) does `let count = u16::from_usize(count)?` in `write_counted` (`message/frontend.rs:108`). A legitimate `insertMany` of 1000 wide documents therefore fails in the driver with a parameter-count error. **The threshold is 66 non-null columns per document at a full batch**, and nothing in the builder counts binds. The cap and the wall are in different units | `zeroship-schema/src/query.rs:597-600`, `:4090-4114`; `postgres-protocol-0.6.12/src/message/frontend.rs:108`; version taken from `Cargo.lock` |

| L15 **(NEW - `updateMany` can partially apply and then report failure, verified 2026-08-27)** | On a collection with a randomised-encrypted column, `updateMany` takes a per-row path with **three** compounding defects. (a) **Uncapped SELECT**: `resolve_target_row_ids(&route, &coll, &filter, None)` (`crud/mod.rs:1273`) passes `None` as the limit, and the builder emits `LIMIT` only `if let Some(lim) = limit` (`zeroship-schema/src/query.rs:3161-3163`), so the entire matching set streams into the worker heap. (b) **A comment says this cannot happen**: DB-2 at `crud/mod.rs:618-620` states an omitted limit "defaults to `MAX_QUERY_LIMIT` - never 'no LIMIT' (which would stream the whole collection into the worker)". That guard is real but lives in `dispatch_find`'s option parsing; `resolve_target_row_ids` never traverses it. `dispatch_update_one` passes `Some(1)` and is fine - `updateMany` is the only uncapped caller. (c) **Per-row autocommit with no rollback**: the loop runs `exec_mutation_with_emit(...).await` once per row (`crud/mod.rs:1350`) and on `Err` **returns immediately** (`:1354`). Outside an explicit `db.transaction()` each row has already committed independently, so the creator gets a rejection for an operation that **partially applied**. There is no compensation and no indication of how far it got | `crud/mod.rs:1273`, `:618-620`, `:1307-1362`; `zeroship-schema/src/query.rs:3161-3163` |

| L16 **(NEW - the largest constant-factor tax on every op, verified 2026-08-27)** | **Every autocommit CRUD operation costs four network round trips and a fresh server-side parse+plan.** The single funnel at `exec.rs:310-341` does `client.transaction()` (sends `BEGIN`), `tx.simple_query(&setup_sql)` (`SET LOCAL` role + timeouts, rebuilt per op though it depends only on `app_id`), `tx.query_text_params(...)`, then `tx.commit()` - four round trips for one `find`. And the query itself uses the **unnamed** statement, so PostgreSQL parses, rewrites and plans the SQL on every call. The driver HAS `prepare_cached` (`libs/compio-postgres/src/prepare.rs`), but `statement_cache_capacity` defaults to **0** (`libs/compio-postgres/src/config.rs:833`) and **plugin-db contains zero references to either name** (verified by grep across `crates/zeroship-plugin-db/src/`). SQLite mirrors it exactly: `backend/sqlite/session.rs` calls `conn.prepare` twice and `prepare_cached` zero times, recompiling every statement. All of this runs against `Pool::connect(&url, 8)` (`lib.rs:872`) - **8 connections per worker thread**, shared by the ~200 co-resident isolates that thread admits | `exec.rs:310-341`; `libs/compio-postgres/src/config.rs:833`; `crates/zeroship-plugin-db/src/lib.rs:872`; `backend/sqlite/session.rs` |

A smaller sibling, verified the same day and folded here rather than given its
own row: **deprovision opens a fresh pool per deleted app.**
`deprovision_app_cdc` calls `Pool::connect(db_url, 2)` (`lib.rs:904`) on every
deletion driven by the version poller - two connects, two authentications and two
TLS handshakes per app, discarded immediately. At the churn rate the platform's
target implies that is a constant connect load against PostgreSQL for work that
could share one long-lived platform-role pool. SC-5 already names the ownership
that would fix it; nothing on this branch does.

**What is measured and what is not.** The four round trips, the unnamed
statement, the zero-capacity default, the absent `prepare_cached` calls and the
pool size are all read directly from the tree. The *throughput* consequence is
**not measured** - holding a connection longer plainly reduces ops/sec against a
fixed pool of 8, but this document does not have a benchmark and should not
carry a multiplier it did not run. That measurement is owed before any figure is
quoted.

L16 is the finding most directly served by the IR this design proposes, which is
worth saying because it turns a cleanup into a design argument. A `DbPlan` has a
**stable shape**: the same operation against the same collection produces the
same SQL modulo parameters. That is exactly the precondition for a named
prepared statement, and it is the property raw per-call SQL construction throws
away. An IR that renders to a cacheable statement key gets prepare-once for free,
where the current path cannot have it at any capacity setting because nothing
ever asks for a named statement. The `SET LOCAL` rebuild has the same character:
it depends only on `app_id`, so it is a per-binding constant being recomputed
per operation.

L15's third part is the one that matters most and is easiest to miss behind the
first two. Unbounded memory is an availability problem; **a bulk write that
half-applies and reports failure is a correctness problem**, and it is
indistinguishable to the caller from one that applied nothing. It also
interacts with L8: a creator who retries a rejected `updateMany` re-applies the
prefix that already succeeded. Any per-row fan-out this design keeps must either
run inside one transaction or return how many rows it committed before failing -
silence is the one option that is not available.

L14 belongs to the family this document keeps returning to: **a comment that
reads as protection**. The constant is not merely too large - it is measured in
the wrong unit for the guarantee its own comment claims, so no value of it makes
the claim true. A bind-aware cap is a different computation, not a smaller
number. It is also a good argument for the IR: a plan that knows its own bind
count can refuse or chunk before the driver does, and can say so in the
creator's vocabulary rather than as `parameter count out of range`.

### L12 is not theoretical: the ceiling of ten already binds on one dev box

Before the options, a measurement that arrived by accident and settles how
seriously to take this.

On 2026-08-27, verifying a merge, two replication tests failed:
`p8a2_consumer_publishes_wal_event_to_broker` and
`p8a2_supervised_consumer_reconnects_after_kill`, both with
`CDC must reach START_REPLICATION: "wal consumer: db error"`. Re-run **in
isolation, unchanged, they both pass.** The failure was contention.

What they were contending for is the finding. Four implementation agents plus
one verification run were doing replication work against one server whose
`max_replication_slots` and `max_wal_senders` are both **10**. Those are
**server-wide**, so the obvious isolation - giving the verification run its own
database - does **not** separate them. Neither does giving each agent its own
schema, or its own app id.

**So the ceiling of ten is already the binding constraint for FIVE concurrent
consumers on a single development machine.** Not five hundred tenants, not five
thousand - five. The production claim this design is written against is millions
of apps, and the same resource is consumed one slot per (app x worker).

Two things follow. First, any argument that the ceiling is a distant scaling
concern is answered: it is a present-tense operational one, and it has already
cost a false failure in this session's own verification. Second, it is worth
noting how nearly this was misread - the first hypotheses were "the fresh
database is missing setup" and "db-cache's change broke replication", and the
server log offered a plausible-looking decoy (`permission denied to use
replication slots`) that turned out to be a DIFFERENT test deliberately proving
a per-app role cannot touch slots. The isolation re-run is what separated
contention from defect, and it is the check to reach for first when a
replication test fails on a shared server.

### L12: what the ceiling of ten actually rests on, and the four ways out

The slot name carries two dimensions and they have very different standing.

**The worker dimension is a PostgreSQL constraint.** `worker_slot_name`'s own
doc says why: "a logical slot can have only one active consumer", so two worker
containers cannot share one. That is not negotiable.

**The app dimension is OUR choice, not PostgreSQL's.** It exists because
publications are per-app: `publication_name(app_id)` mints one per app, and
`zeroship-migrated/src/publication.rs:45` creates it `FOR TABLE <schema>.<table>,
...` with an explicit table list - deliberately, since a test at `:138` asserts
it does **not** use `FOR TABLES IN SCHEMA`. A publication can perfectly well span
schemas; ours does not, for tenant isolation. **That is the decision to
re-examine, because it is what multiplies the slot count by the app count.**

Four options, and they are not close in merit:

**A. Keep per-app publications.** Slots scale as apps x workers; the ceiling is
10 on a default server. This is today, and it does not reach the stated scale by
any amount of tuning.

**B. One publication and one slot per worker; demultiplex by schema in the
worker.** Slot count drops to the number of workers - 10 fits comfortably. The
cost is a **tenant-isolation regression**: every worker's CDC stream then carries
every app's row changes, so a demux bug leaks one tenant's rows into another
tenant's subscribers, and every worker pays WAL decode cost for apps it does not
serve. Trading a scaling wall for a cross-tenant data-leak surface is the wrong
direction for this platform.

**C. A dedicated CDC service that owns the slots and fans out.** One component
holds O(1) slots, decodes once, and distributes to workers over the broker and
stream machinery that already exists. Slot count stops scaling with apps AND
with workers. Tenant filtering happens exactly once, in a process whose only job
is that - which is a far better place for it than in every worker. It also
**closes L12b for free**: a worker that crashes cannot abandon a slot it never
held, so the `pg_wal`-growth cluster outage stops being reachable from worker
death. This is the largest change and the only one that answers the question
that was asked.

**D. Raise `max_replication_slots`.** A stopgap, and a poor one: it is a
restart-only shared-memory GUC, and every active slot is a walsender **backend**
competing for `max_connections` (default 100). It buys a small constant and
cannot approach millions of apps.

**Recommendation: C**, with **D** as an explicit interim if something must ship
before C lands. B should be rejected rather than deferred - it is the option
that looks cheapest and creates a cross-tenant leak path, and this document has
already catalogued what happens when isolation is left to a filter that
something forgets to apply.

The consequence for sequencing is the thing to take away: **SC-3's subscription
surface is provisional until this is answered**, because C moves subscription
transport out of the worker entirely. Building the IR against A means building
it twice.

L12 and L13 were in the performance round's opus report and this document had
folded **none** of them - the revision pass concentrated on one reviewer's
findings and treated the other two reports as already-absorbed. That is a
process failure worth naming, because it is the same shape as the enumeration
bugs catalogued above: three sources were consulted, one was read closely, and
the coverage claim was made over the set rather than over what was actually
read. L12 in particular is the single most consequential finding of the round -
a hard ceiling of ten on the feature this design exists to serve - and it sat
unread in a file on disk.

L11 is listed here because it was found by implementation work on this design
and because it is a live user-facing gap, not because this design causes or
fixes it. It needs its own decision - restore an FTS producer, or remove
`.fts()` from the DSL and correct the divergence doc - and that decision is not
this document's to make. What is worth carrying across, though, is the shape:
the capability was deleted in one layer and left standing in three others, and
each of those three reads as evidence that it works. Nothing was lying; every
layer was locally consistent.

Two entries from v3 are reclassified rather than kept, because neither is a
defect with a failing pre-fix test:

- **L7 is not a defect, it is a missing gate arm - and it is now partly fixed.**
  The diagnostic the sanitization rail relies on ("diagnosable only from a
  worker log", `dispatch.rs:245-246`) went to a discarded stream because no
  integration binary installed a subscriber, so `RUST_LOG` had nothing to
  configure. `native_transaction.rs:107` now installs one (commit `018291e36`),
  which immediately surfaced the cause of four failures that had been opaque:
  `permission denied for schema default`, from a `DROP SCHEMA ... CASCADE` in
  the harness that destroyed the per-app grants without restoring them
  (fixed in `8cbeda1c0`). Coverage is **1 of 9 binaries**, not 0. The remaining
  eight are the gate arm.

  Worth recording because it is corroboration rather than coincidence: that
  harness bug is the **same failure mode** this proposal documents for restore -
  `DROP SCHEMA CASCADE` destroys grants and `ALTER DEFAULT PRIVILEGES` entries,
  and whatever recreates the schema must restore them. It was found from a
  completely different direction.
- **L9 was misattributed.** `install_get_column_key_function` is
  `#[cfg(any(test, feature = "test-helpers"))]` (`bootstrap.rs:612`) - the same
  gate L6 is about - so `get_column_key`'s `EXECUTE TO PUBLIC` over a
  `key_id`-keyed table cannot be live in `main`. It is restated as a
  **constraint on the step-7 provisioner**: when `__zeroship_admin` is stood up
  for real, that function must not be recreated with its current grant or its
  current keying.

