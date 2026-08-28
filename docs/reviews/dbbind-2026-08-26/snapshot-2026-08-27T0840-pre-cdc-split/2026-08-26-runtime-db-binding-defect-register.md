# Defect register: live defects in the code this design touches

Extracted from `2026-08-26-runtime-db-binding-design.md` on 2026-08-27, and
extended the same day when six further live-defect sections were lifted out of
that document (L17 through L22).

## How to read this register

**These are defects in EXISTING code**, found while designing and implementing
the runtime DB binding. They are not proposals and not design decisions. They
are listed together because several of them constrain the design, and because a
register that lives inside a design document makes both harder to read.

**Every entry was verified against the tree by the pilot**, not accepted from a
review. That distinction earned its place: several reviewer claims were correct
about the defect and wrong about its mechanism or its magnitude, and where that
happened the correction is recorded in the entry.

**This is the most perishable document in the set.** Entries retire as fixes
land. **None has retired yet**, and the tally as of 2026-08-27 is: three carry
FIXED (L5, L8, L10), one carries DECIDED (L9), one carries PARTLY FIXED (L18),
and two v3 entries were reclassified rather than retired (bottom of this file).
It should never be read as design, and an entry's absence from a later revision
means it was closed, not that it was wrong.

**Each entry is a heading, not a table row.** The previous revision was one wide
three-column table of sixteen rows, and it had stopped rendering as one: a blank
line after the L9 row terminated the table, so the eight entries below it (L10
through L16, plus L12b) printed as literal pipes. Measured on that revision, the
longest single row was 2,319 characters (L9) and three more were over 1,400. The
status now lives in the entry heading, so the document outline is the index and
there is no second copy of the status to go stale.

Read `2026-08-26-runtime-db-binding-00-index.md` first for what is settled, what
is open, and what blocks what.

---

## Live defects, to fix independently

These exist in `main` and do not depend on this design. Each needs a regression
test that fails on the pre-fix code.

### L1 - mask policy forgeable from any bundled dependency

Mask policy forgeable from any bundled dependency via the global symbol
registry, persisted durably.

**Evidence:** `sdks/db/src/policy.ts:85,91-94`

### L2 - mask policy suppressible via `_flushPendingMaskPolicy`

Mask policy suppressible via `_flushPendingMaskPolicy`, dynamically importable
by any referrer.

**Evidence:** `bootstrap_modules.rs:73-80`

### L3 - `__zsSchemaReady` shadowable by an accessor before assignment

`__zsSchemaReady` shadowable by an accessor before assignment, so dispatch never
awaits it.

**Evidence:** `init.rs:515-518`

### L4 - CDC ships mask-only plaintext and companion columns

CDC ships mask-only plaintext and companion columns, with no policy check and no
audit row. The existing unit test asserts the **wrong property** and must be
replaced, not extended: it hand-builds an encrypted-and-masked fixture and
checks the frame round-trips it, so it rules on the fixture rather than the
pipeline and never constructs the mask-only case.

**Evidence:** load-bearing lines are `mask_pass.rs:150-156` (v3 cited `:29-31`,
which is only the module comment); `broker.rs:990`; the fixture-only test at
`broker.rs:1852`

### L5 (FIXED) - the V8 decoder elides a filter key whose getter throws

**FIXED - decode is now total; `DecodeError` at `v8_bridge.rs:165`, no silent
`continue` survives**

The V8 decoder elides a filter key whose getter throws, so `updateMany` can lose
its tenant predicate.

**Evidence:** `v8_bridge.rs:250`

### L6 - `__zeroship_admin` has no production provisioner

`__zeroship_admin` has no production provisioner, yet production code calls its
functions and propagates the error.

**Evidence:** `bootstrap.rs:95-96`; `mask_policy.rs:268,286`; `keys.rs:495`
names a migration that does not exist

### L8 (FIXED) - PostgreSQL answers `COMMIT` with a `ROLLBACK` tag

**FIXED - landed with its regression test; `exec_terminal_on_tx`,
`transaction/mod.rs:139`**

PostgreSQL answers `COMMIT` with a `ROLLBACK` tag for a failed transaction; the
driver detects it but plugin-db discards command tags, so a rolled-back commit
reports **success** and the settle path then drains pending emits for writes the
database discarded. **Scope the test to the explicit-transaction path** -
autocommit already goes through the driver wrapper that checks the tag, so a
test written there passes pre-fix and proves nothing.

**Evidence:** MEASURED: `BEGIN; SELECT 1/0; COMMIT;` -> server replies
`ROLLBACK`; control (clean tx) replies `COMMIT`. Driver detects at
`transaction.rs:186-188`; `backend/postgres.rs:201-212` returns `Ok(rows.len())`;
explicit `COMMIT` is routed there by `transaction/mod.rs:1001`

### L9 (DECIDED 2026-08-27) - the filter path is an unaudited plaintext oracle

**DECIDED 2026-08-27 - storage flip. The rationale and the four owed items are
in SC-6, named again at the end of this entry.**

Masking is projection-shaped, so the filter path is an unaudited plaintext
oracle. `build_where_with_dialect(filter, params, dialect)` takes **no schema
hint** (`query.rs:5274-5281`), so it cannot know a column is masked; a masked
column stores plaintext in the parent column, so `find({ssn:{$gt:"500-00-0000"}})`
renders `WHERE "ssn" > $1` against plaintext. The caller never sees an unmasked
value and does not need to - **the matching set is the answer**, and repeated
queries binary-search the exact value with no authorization check and no audit
row. Does NOT violate the letter of the audit guarantee (that covers `.unmask()`
calls, which this is not) - it defeats the protection goal through a channel the
guarantee never scoped. **DECIDED 2026-08-27: flip the storage.** `ssn` stores
the MASKED value, `ssn_raw` stores the real one, and `ssn_raw` is **reserved and
unqueryable** - not in a filter, not in a projection, not in a sort, not a field
of the generated type. Plaintext is reachable only through an explicit API,
where authorization and the audit row already live. This beats the three policy
options SC-6 had recorded, all of which kept plaintext in the natural-named
column and then policed the filter path on top of it - leaving the design
fail-open, which is precisely how the defect arose. After the flip the ignorant
path is the safe path, the `"ssn_masked" AS "ssn"` substitution is deleted
rather than extended, and the guard becomes a **reserved-suffix check needing no
schema** instead of a metadata lookup the filter builder does not have. It also
makes the audit guarantee simply true rather than narrow-but-true: plaintext
ends up with exactly one reader. What it owes: lookup BY plaintext must be
supported by the explicit API or the feature is closed rather than secured;
unique indexes and foreign keys must follow `ssn_raw`, since enforcing
uniqueness over masks is a silent integrity failure; and the AAD binds the
column name, so this is a migration-engine change too. Full rationale and the
four owed items in SC-6.

**Evidence:** `query.rs:5274-5281` (no hint); mask substitution is
select-list-only at `query.rs:3351`, extended to aggregates by
`aggregate_read_ident` at `:3412`; audit promise at `docs/reference/db.md`
"Audit tables"

### L10 (FIXED 2026-08-27) - the deploy-invalidation token is keyed one level coarser

**FIXED 2026-08-27, `4c8e84134`; regression test proven red by mutation**

The deploy-invalidation token is keyed **one level coarser than the isolates it
protects**, and a misleading name hides it. `deploy_tokens: HashMap<String,
String>` is `app_id -> token` (`context.rs:296`), but the worker deliberately
keeps several isolates *of the same app at different deploys* alive on one
thread, keyed `PinnedWorkflowKey { app_id, deploy_hash }`
(`worker/src/cache.rs:28-32`) for deploy-pinned workflow replay - a documented
invariant, not an accident. Every runtime overwrites the single shared entry
when it mints its `Db` wrapper, so it is **last-writer-wins across deploys of
one app**: mint the pinned runtime last and the *current* runtime reads the old
token; mint the current one last and the pinned replay reads the new one.
`runtime_schema_for` then serves or repopulates introspected metadata under the
wrong deploy's key. The same coarseness lets a redeploy skip installing new
declared hints, because `registered_models` is keyed `(app, collection)` with no
deploy component and registration fast-returns on the stale mark. **What makes
this hard to see is a name:** the struct is `IsolateDbContext` and its comment
says "the per-isolate DB context", but it lives in a `thread_local!`
(`context.rs:952-957`) and the worker runs many isolates per thread. Keying by
`app_id` inside it would be correct if the name were true. The token must carry
the full binding identity `(app_id, deploy_hash)` - which `PinnedWorkflowKey`
already establishes one layer up - and the active binding must carry it, not
recover it from thread-global app state.

**Evidence:** `context.rs:296` (key shape); `context.rs:952-957`
(`thread_local!` vs "per-isolate"); `worker/src/cache.rs:28-32` (the finer key);
`v8_classes/db.rs:313-330` (the overwrite); `crud/introspect_schema.rs:80-90`
(the read)

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
design's cache-bound section rather than a per-map patch: one place to key
correctly instead of three places to keep in sync.

L10 is worth reading beside the "bound to the wrong thing" pattern this
document keeps hitting. Its history is a fix that moved in the right direction
and stopped one step short: the comment at `context.rs:281-296` explains, at
length and correctly, that the previous `std::env::var("ZEROSHIP_DEPLOY_ID")`
read was process-global and "would have been WRONG for a multi-app worker
thread". It then keys the replacement by `app_id` - which fixes multi-*app*
sharing and leaves multi-*deploy* sharing of one app, the case the worker
explicitly supports, still broken. The reasoning that identified the flaw was
sound; it was applied to one of the two dimensions the key needed.

### L11 (DECIDED 2026-08-27 - delete FTS; implemented, not yet merged) - PostgreSQL full-text search has no producer anywhere in the tree

> **DECIDED (operator, 2026-08-27): delete full-text search.** Not restore a
> PostgreSQL producer. The migration engine had already removed FTS
> deliberately; this finishes the removal in the three layers that still
> advertised it.
>
> **Implemented on `feat/db-delete-fts`**, three commits (`b3fa01659` the
> removal, `4ee6b70dd` a sync merge, `49e579293` a guard against the deleted
> migration mirrors reappearing). Two whole files deleted
> (`backend/sqlite/fts.rs` 547 lines, `zeroship-schema/src/fts_sqlite.rs` 398)
> plus references across `query.rs`, `sdks/db/src/types.ts`, the vite-plugin
> renderer, `docs/reference/db.md`, the divergence table, and four `.fts()`
> calls in `examples/db-e2e`.
>
> Verified on that branch: lib 630/0, integration 89/0/6, native_transaction
> 13/0, sqlite_integration 121/0, clippy exit 0, `pnpm build` exit 0. The lib
> count falls because FTS deletion removed 14 tests while a merged cache commit
> added five.
>
> **`db/` is untouched and `db/released_migrations.tsv` is untouched** - checked
> directly, because a migration a deployed database has applied is frozen and
> editing one makes the runner refuse every later run with `ChecksumMismatch`,
> permanently. The checksum goldens that did change are three engine *test*
> fixtures, which is the right place for an IR shape change.
>
> **NOT YET MERGED**, so 14 files in the working branch still carry the symbol.
> This entry retires when that merge lands and is verified, not before.
>
> Residual risk the implementer named and I am keeping: removing
> `engine_goodie_ddl` changes migration IR/checksum contracts, so an unknown
> out-of-tree consumer could still depend on it; no external deployed database
> was queried; and stale local SQLite FTS5 tables are deliberately left behind
> rather than migrated away, since this platform is pre-launch.


**NEW - found while unblocking `pnpm build`; NOT caused by this design**

**PostgreSQL full-text search has no producer anywhere in the tree, while three
layers still present it as a feature.** The migration engine removed FTS
outright - "Full-text support was removed from this engine, down to the
`IndexMethod` variant... There is no `.fts()` facet to fold: the authoring
surface has none, and no code path here produces either shape"
(`zeroship-migrate-core/src/render/declarative.rs:1823-1830`), and a second site
confirms "no `fts5` sentinel, no `.fts()` facet" (`:2971-2976`). It previously
folded `.fts()` into a `__fts` GENERATED `tsvector` column plus a GIN index on
PostgreSQL. Nothing replaced it: `tsvector` appears in the whole tree only in
that removal comment, and there is no producer in plugin-db's PostgreSQL
backend. Meanwhile `t.string().fts(language?)` is still callable and documented
in `@zeroship/db` (`sdks/db/src/types.ts:1172`),
`docs/reference/sqlite-divergences.md:14-15` documents PostgreSQL FTS as working
and merely *differing* from SQLite ("`language` selects the `tsvector`
configuration"), and SQLite full-text still works because plugin-db's runtime
creates the FTS5 table itself (`backend/sqlite/fts.rs`) on a path that never
touches the engine. So the divergence table describes a comparison between a
working backend and a non-existent one. The removal's stated rationale cites
`docs/proposals/fts-macro.md` - **which is not in the tree**.

**Evidence:** `zeroship-migrate-core/src/render/declarative.rs:1823-1830,2971-2976`;
`sdks/db/src/types.ts:1172`; `docs/reference/sqlite-divergences.md:14-15`;
`backend/sqlite/fts.rs`; absent: `docs/proposals/fts-macro.md`

L11 is listed here because it was found by implementation work on this design
and because it is a live user-facing gap, not because this design causes or
fixes it. It needs its own decision - restore an FTS producer, or remove
`.fts()` from the DSL and correct the divergence doc - and that decision is not
this document's to make. What is worth carrying across, though, is the shape:
the capability was deleted in one layer and left standing in three others, and
each of those three reads as evidence that it works. Nothing was lying; every
layer was locally consistent.

### L13 (NEW) - `MAX_SUBSCRIPTIONS_PER_APP` is enforced on zero production paths

**NEW - a cap that is green and dead at once, verified 2026-08-27**

**`MAX_SUBSCRIPTIONS_PER_APP = 256` is enforced on zero production paths.** It
is checked only inside `Broker::try_subscribe` (`broker.rs:490`), and the single
production mint site calls the **infallible** `broker::subscribe(app_id,
collection)` (`v8_classes/subscription.rs:315`). Verified by grep:
`try_subscribe` occurs only in `broker.rs` itself and in doc comments. So
`for(;;) db.users.openSubscription()` is unbounded, and each iteration allocates
a `DEFAULT_QUEUE_DEPTH = 1024`-slot event queue plus a CDC lease plus an entry
in the **process-global** routing table that every publish walks - so one tenant
degrades every co-resident tenant in the process. The code documents its own gap
at `broker.rs:452-454` ("New SDK call sites should prefer `try_subscribe`"), and
the one new SDK call site does not. **The test passes because it calls
`try_subscribe` directly**, which is the cannot-fail arm class exactly: the cap
is green and dead simultaneously.

**Evidence:** `broker.rs:144,151,452-454,490`; `v8_classes/subscription.rs:315`

**Still open, re-verified 2026-08-27 against the tree at `f44bcf6b6`.** The
production mint site is now `v8_classes/subscription.rs:314` and still calls
`broker::subscribe`; the cap is still `broker.rs:151` and still reached only
from `Broker::try_subscribe` at `broker.rs:490` and the free `try_subscribe` at
`broker.rs:856`. The index reported this closed; it is not.

### L14 (NEW) - `MAX_INSERT_MANY_BATCH` caps documents while the wall it cites counts binds

**NEW - a wide `insertMany` fails, and the comment says it cannot, verified
2026-08-27**

**`MAX_INSERT_MANY_BATCH` caps documents while the wall it cites counts binds.**
The constant is `1_000` documents and its comment justifies that number as
staying "well under Postgres' 65535-bind-param wall"
(`zeroship-schema/src/query.rs:597-600`). But `build_insert_many_with_dialect`
pushes **one bind per non-null cell** (`query.rs:4090-4114`; NULLs are inlined
as SQL literals and cost no bind). So a full batch of 1000 documents with 66
non-null columns each is 66,000 binds. The limit is exactly 65535 and is
enforced **client-side, as an error rather than a truncation**:
`postgres-protocol` (resolved to 0.6.12 in `Cargo.lock`, verified against that
exact version) does `let count = u16::from_usize(count)?` in `write_counted`
(`message/frontend.rs:108`). A legitimate `insertMany` of 1000 wide documents
therefore fails in the driver with a parameter-count error. **The threshold is
66 non-null columns per document at a full batch**, and nothing in the builder
counts binds. The cap and the wall are in different units.

**Evidence:** `zeroship-schema/src/query.rs:597-600`, `:4090-4114`;
`postgres-protocol-0.6.12/src/message/frontend.rs:108`; version taken from
`Cargo.lock`

**Still open, and the citation has drifted.** Re-verified 2026-08-27 against the
tree at `f44bcf6b6`: the constant is unchanged at `1_000` and now sits at
`zeroship-schema/src/query.rs:601`, with the justifying comment at `:598-600`.
The index reported this closed; it is not.

L14 belongs to the family this document keeps returning to: **a comment that
reads as protection**. The constant is not merely too large - it is measured in
the wrong unit for the guarantee its own comment claims, so no value of it makes
the claim true. A bind-aware cap is a different computation, not a smaller
number. It is also a good argument for the IR: a plan that knows its own bind
count can refuse or chunk before the driver does, and can say so in the
creator's vocabulary rather than as `parameter count out of range`.

### L15 (NEW) - `updateMany` can partially apply and then report failure

**NEW - verified 2026-08-27**

On a collection with a randomised-encrypted column, `updateMany` takes a per-row
path with **three** compounding defects. (a) **Uncapped SELECT**:
`resolve_target_row_ids(&route, &coll, &filter, None)` (`crud/mod.rs:1273`)
passes `None` as the limit, and the builder emits `LIMIT` only `if let Some(lim)
= limit` (`zeroship-schema/src/query.rs:3161-3163`), so the entire matching set
streams into the worker heap. (b) **A comment says this cannot happen**: DB-2 at
`crud/mod.rs:618-620` states an omitted limit "defaults to `MAX_QUERY_LIMIT` -
never 'no LIMIT' (which would stream the whole collection into the worker)".
That guard is real but lives in `dispatch_find`'s option parsing;
`resolve_target_row_ids` never traverses it. `dispatch_update_one` passes
`Some(1)` and is fine - `updateMany` is the only uncapped caller. (c) **Per-row
autocommit with no rollback**: the loop runs `exec_mutation_with_emit(...).await`
once per row (`crud/mod.rs:1350`) and on `Err` **returns immediately** (`:1354`).
Outside an explicit `db.transaction()` each row has already committed
independently, so the creator gets a rejection for an operation that **partially
applied**. There is no compensation and no indication of how far it got.

**Evidence:** `crud/mod.rs:1273`, `:618-620`, `:1307-1362`;
`zeroship-schema/src/query.rs:3161-3163`

**Still open, and the citation has drifted.** Re-verified 2026-08-27 against the
tree at `f44bcf6b6`: the uncapped call is now `crud/mod.rs:1279` and still
passes `None`; the DB-2 comment is now `crud/mod.rs:620-622`. The index reported
this closed; it is not.

L15's third part is the one that matters most and is easiest to miss behind the
first two. Unbounded memory is an availability problem; **a bulk write that
half-applies and reports failure is a correctness problem**, and it is
indistinguishable to the caller from one that applied nothing. It also
interacts with L8: a creator who retries a rejected `updateMany` re-applies the
prefix that already succeeded. Any per-row fan-out this design keeps must either
run inside one transaction or return how many rows it committed before failing -
silence is the one option that is not available.

### L16 (NEW) - every autocommit CRUD operation costs four round trips and a fresh parse

**NEW - the largest constant-factor tax on every op, verified 2026-08-27**

**Every autocommit CRUD operation costs four network round trips and a fresh
server-side parse+plan.** The single funnel at `exec.rs:310-341` does
`client.transaction()` (sends `BEGIN`), `tx.simple_query(&setup_sql)` (`SET
LOCAL` role + timeouts, rebuilt per op though it depends only on `app_id`),
`tx.query_text_params(...)`, then `tx.commit()` - four round trips for one
`find`. And the query itself uses the **unnamed** statement, so PostgreSQL
parses, rewrites and plans the SQL on every call. The driver HAS `prepare_cached`
(`libs/compio-postgres/src/prepare.rs`), but `statement_cache_capacity` defaults
to **0** (`libs/compio-postgres/src/config.rs:833`) and **plugin-db contains
zero references to either name** (verified by grep across
`crates/zeroship-plugin-db/src/`). SQLite mirrors it exactly:
`backend/sqlite/session.rs` calls `conn.prepare` twice and `prepare_cached` zero
times, recompiling every statement. All of this runs against `Pool::connect(&url,
8)` (`lib.rs:862`) - **8 connections per worker thread**, shared by the ~200
co-resident isolates that thread admits.

**Evidence:** `exec.rs:310-341`; `libs/compio-postgres/src/config.rs:833`;
`crates/zeroship-plugin-db/src/lib.rs:862`; `backend/sqlite/session.rs`

*(Citation corrected 2026-08-27: this entry and two other documents cited
`lib.rs:872` for the eight-connection pool. `Pool::connect(&url, 8)` is at
`lib.rs:862`; `:904` is the two-connection deprovision pool below and was always
right.)*

A smaller sibling, verified the same day and folded here rather than given its
own entry: **deprovision opens a fresh pool per deleted app.**
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

SC-3 records an overlapping measurement of the prepared-statement half, with
four citations this entry does not have (`Client::new_with_statement_cache`,
`statement_cache_execution_threshold`, `bind.rs:163`, `prepare.rs:312-356`) and
a self-correction of SC-3's own earlier draft. That duplication is accepted
rather than collapsed: the self-correction belongs with the draft it corrects.

### L17 - a partitioned creator table gets NO runtime metadata, so its protection passes are skipped

*Moved out of the design document on 2026-08-27. The governing RULE - that the
runtime needs its own enumeration of logical creator relations - stays in the
design's resolved-metadata section; what follows is the live defect.*

Security-relevant, pre-existing, and on a table shape the public authoring path
supports.

`read_live_schema` filters `AND c.relkind = 'r'`
(`crates/zeroship-schema/src/diff.rs:641`). That predicate:

- **includes physical partitions** - a partition is `relkind = 'r'` with
  `relispartition = true`;
- **excludes the partitioned parent**, which is `relkind = 'p'`.

So for a partitioned creator table the parent is invisible to introspection.
`build_runtime_schema` returns `None` for it, and `None` means *this collection
has no encrypted or masked columns - skip the passes*. **An encrypted or masked
partitioned table therefore reads with its encryption and mask passes turned
off**, because "no metadata" and "no protection needed" are the same value.

This is not an operator-only shape: `PARTITION BY` is emitted from the public
migration DSL (`sdks/migrate/src/ops.ts`, lowered in
`crates/zeroship-migrate-postgres/src/ddl.rs`).

**The repository already knows the right predicate and uses it elsewhere.**
`creator_table_query` in `crates/zeroship-migrated/src/publication.rs:15-23`
enumerates `relkind IN ('r','p') AND NOT c.relispartition` and excludes
`__zeroship_%` by name. Two paths in one codebase disagree about what a creator
table *is*, and the introspection path - the one that decides whether to decrypt
- has the wrong answer.

There is a second-order cost created by this branch: populating every collection
from one read caches **P entries for physical partitions** that no creator ever
requests, spending the per-app cache budget the design is separately trying to
bound. That half cannot be fixed in the populate loop, because `LiveSchema`
carries no relkind or partition flag to filter on
(`diff.rs:201-218`) - the information is discarded before the loop sees it.

The fix belongs at the source, and it is deliberately **not** made here:
`read_live_schema` is shared with the migration diff path, where enumerating
physical partitions may well be intended. Changing a shared catalog query to
serve the runtime without establishing what the migration side needs is how a
correct-looking fix breaks the other consumer. What the runtime needs is its own
enumeration of *logical creator relations*, plus a hard error when a registered
relation cannot be enumerated - rather than the silent `None` that currently
reads as "unprotected".

### L18 (PARTLY FIXED 2026-08-27) - the cold-start fix opened a cross-tenant eviction DoS

*Moved out of the design document on 2026-08-27, where it sat under the
cache-bound analysis. The text below is that analysis unchanged; the status
paragraph at the end is new and dated.*

This is a defect **introduced by the change the design recommended**, and it
is worth stating plainly rather than folding into the bound discussion.

`introspected_schemas` is one map per **OS thread**, keyed `"{app}:{coll}"` and
shared by every app resident on that thread - the context's own documentation
discusses exactly this co-residency, explaining that keying by `app_id` is what
keeps a parked transaction of app A "invisible and untouchable to B"
(`context.rs:141-150`). Any LRU over that map therefore **evicts across
tenants**.

Now combine that with `cache_every_collection`, which iterates
`live.tables.keys()` with **no cap** (`crud/introspect_schema.rs:166-179`) and
inserts one entry per table atomically. A single tenant with 500 tables performs
one cold read - one line of app code - and inserts 500 entries into a budget
shared with up to ~200 co-resident tenants. At a 10,000-entry bound that is 5%
of the shared cache consumed by one app's first operation, and a tenant with
enough tables can evict most of it.

**This did not exist before the fix**, because the previous behaviour inserted
exactly one entry per read. Trading N catalog reads for one read is right; doing
it by inserting N entries into an unbounded cross-tenant cache converted a
latency problem into a **neighbour-eviction vector**. Both halves of the fix
need to land together, and only the first half has:

- per-app sub-maps rather than one flat thread-global map, so eviction is
  scoped to the tenant that caused it (the hierarchical shape argued for in the
  design's cache-bound section and in L22, now load-bearing for isolation rather
  than just for lookup cost);
- a **per-app** cap on entries admitted by one populate, so a wide tenant
  cannot spend a shared budget in one operation.

Recording this because the sequencing matters: the populate-all change is
already committed and the bound is not, so the window in which this is
exploitable is open now.

**And the miss path has no singleflight, which multiplies the read this section
is about.** Verified 2026-08-27: `crud/introspect_schema.rs` contains **zero**
in-progress or singleflight markers, and the only such guard anywhere in the
context is `backend_init_in_progress` (`context.rs:325`), which covers backend
initialization rather than introspection. So two cold operations on the same
thread, interleaved at the `await`, both miss the cache and both run the whole
catalog read; a cold app's opening burst of K concurrent operations costs **K**
whole-schema reads per thread.

That compounds with the O(total tenants on the cluster) cost of each read
established in the design's cache-bound section - K copies of the expensive
query, not K copies of a cheap one - and it lands squarely on the case the
populate-all fix was written for, since a cold app is exactly the app whose
first request arrives as a burst.

The design's own end-state `#### Caches` section already mandates a per-thread
singleflight. The gap is that the interim shipped without it, so the design
states the requirement and the code does not meet it - which is a different
situation from an open design question and should not be filed as one.

**Status, re-derived against the tree 2026-08-27 at `f44bcf6b6`
("fix(db): bound and singleflight live schema cache fills").** Two of the three
things this entry asks for have landed and one has not, so the paragraphs above
are kept verbatim rather than rewritten:

- **Landed: the per-populate cap.** `MAX_COLLECTIONS_PER_POPULATE = 32`
  (`crud/introspect_schema.rs:59`) bounds one whole-catalog read to the
  requested collection plus at most 31 uncached siblings
  (`:216-234`), with the arm `one_populate_admits_at_most_the_per_populate_cap`.
  The 500-entry single-populate vector above is closed.
- **Landed: the singleflight.** Cold misses are singleflight per exact
  `(binding, collection)` key through `poll_schema_introspection`
  (`context.rs:666`) and a cancellation-safe `SchemaIntrospectionGuard`
  (`crud/introspect_schema.rs:61-90`). The K-whole-schema-reads burst is
  closed.
- **NOT landed: the flat cross-tenant map, and any total bound at all.**
  `introspected_schemas` is still one `HashMap<(DbBinding, String),
  Option<Value>>` per OS thread (`context.rs:293`) with insert, get and
  contains and **no eviction path whatsoever** - so the eviction DoS is
  currently unreachable only because nothing evicts, and unbounded growth
  replaces it. The first bullet above is still owed.

### L22 - the same map is a per-transaction CPU cost, not only a memory ceiling

*Moved out of the design document on 2026-08-27. The design keeps the commitment
this analysis produces - per-app metadata behind a cheap handle, resolved once
per operation - and points here for the measurement behind it.*

Memory is the obvious consequence of an unbounded map and it is not the
expensive one. **Every transaction start scans it.**

`mint_tx_view` needs the list of collection names to hang off `tx.<name>`, and
it gets them from `cached_schemas_for_app`
(`crates/zeroship-plugin-db/src/context.rs:697-707`), which does:

```rust
let prefix = format!("{app_id}:");
self.schemas.iter()
    .filter_map(|(k, v)| k.strip_prefix(&prefix).map(|coll| (coll.to_string(), v.clone())))
    .collect()
```

Two costs, and the second is pure waste:

1. **The scan is cross-tenant.** `self.schemas` is the thread-global map keyed
   `<app_id>:<collection>`, so the iteration is O(every collection of every app
   the thread has ever served) to find the handful belonging to this one. It is
   the unbounded map of the design's cache-bound section, walked linearly, on a
   hot path. At the stated target this is a million-entry scan per
   `db.transaction()`.

2. **Every matching schema is deep-cloned and immediately dropped.** `v.clone()`
   rebuilds the whole `serde_json::Value` - at the corrected ~18x multiplier and
   the measured 551 B typical serialized size, roughly **10 KB of fresh
   allocations per collection**, spread over one `IndexMap` and two `Value`
   nodes per column - and the caller is
   `.map(|(name, _schema)| name)` (`v8_classes/transaction.rs:75-81`). The
   underscore is the tell: the function's entire return payload beyond the key
   is constructed and discarded one line later. A transaction on a 20-collection
   app allocates and frees on the order of **200 KB** to produce 20 strings.

   (An earlier draft of this paragraph said ~76 KB, arrived at by multiplying
   the 7x table the design's cache-bound section had already retracted. Keeping
   the note because the failure is the interesting part: the retraction was
   *written* and the derived figures were *not re-derived*, so a stale constant
   walked straight into a new claim. Any figure in these documents traceable to
   that table is suspect unless it names the ~18x measurement.)

The accessor's own doc comment explains the shape - it was written for the
drift-check sweep, which genuinely wants `(collection, schema)` pairs - so this
is not a bug in `cached_schemas_for_app`. It is a hot path reusing an accessor
built for a cold one, and paying its full cost for a fraction of its result.
That is worth stating plainly because it changes the fix: the accessor is
correct and should stay, and what is owed is a **name-only enumeration** beside
it that borrows rather than clones.

**And it is the convention, not this one accessor.** The same shape recurs on
the warm CRUD path, which is the hottest path the plugin has:

- `deploy_token_for` returns `String` (`context.rs:680-684`) - a clone on hit,
  a fresh `"cold_start".to_string()` on miss, **every operation**, to produce a
  value that is only ever compared.
- the read pipeline resolves an **owned** schema per operation and hands it
  straight to `scope_schema` (`crud/read_pipeline.rs:62-70`).
- projection retention is `fields.iter().any(...)` inside a per-column loop -
  O(schema width x projection width) where a prebuilt set is O(width).
- write-stage construction scans the same schema **four separate times**
  (`crud/write_pipeline.rs:192-201`): `WriteStages::new` calls
  `schema_has_encrypted_columns`, `schema_has_masked_columns`,
  `schema_has_sqlite_binary_columns` and `schema_has_plain_bytes_columns` in
  turn, each walking every column, to produce four booleans that one pass could
  yield. (This one takes `Option<&Value>` - a borrow - so it costs scans, not
  allocations. Worth separating: the fix is precomputation, not ownership.)
- update and upsert resolve the schema **twice** on the route-decision path.
  `update_requires_per_row_encryption` and `upsert_requires_conflict_probe`
  each `await runtime_schema_for(...)` for an **owned** schema, use it for a
  single boolean, drop it - and the main write pipeline then resolves it again
  (`crud/write_pipeline.rs:359-383`). At the accessor's measured ~10 KB per
  entry that is two full rebuilds of the same object to answer one yes/no.

*(All four bullets re-read against the tree 2026-08-27 rather than taken from
the review. The projection line is
`obj.retain(|key, _| key.starts_with('_') || fields.iter().any(|f| f == key))`
at `crud/read_pipeline.rs:116` - a linear scan of the projection list per
column. It mutates the owned schema in place, so like the four-scan case it
costs time, not allocations.)*

None of these is expensive enough to notice in a profile of a single operation,
which is exactly why they are worth naming in a design document rather than
leaving to a later optimization pass: they are **decisions about what an
accessor returns**, and they are cheap to make correctly now and expensive to
unpick once every call site depends on owning its result.

### L23 (NEW) - dev-mode SSRF validation is bypassed from an ambient env read

**A pointer entry, not a moved one.** The full argument lives in SC-4's decision
that dev-ness is a typed input derived from the runtime's identity, never an
ambient environment read. Extracting it would gut that decision, which is stated
as a correction of an earlier draft's invariant and needs its own reasoning
intact. It is recorded here because a live bypass in shipped runtime code was,
until 2026-08-27, in **no** register at all. This follows the convention L9
already uses for SC-6.

`validate_url` returns `Ok(())` before any host or IP check whenever dev mode is
on:

> `// In dev mode, skip host/IP validation (allows localhost fetch to Vite).`
> `if dev_mode_enabled() { return Ok(()); }`

and `dev_mode_enabled()` resolves a process-wide cell from
`declared_env!(dev, "ZEROSHIP_DEV", ...)`. An environment variable is not a
construction boundary. The tree already states the opposite standard in the very
place a leak would land: "The authority is the worker's identity, not an env
flag: SQLite is refused even if `ZEROSHIP_DEV=1` leaked into a prod worker". The
SSRF gate does not meet it.

**Evidence:** verified 2026-08-27.
`crates/zeroship-runtime/src/transport/ssrf.rs:206-207` (the bypass), `:188`
(`validate_url`), `:66` (the `declared_env!` read);
`crates/zeroship-worker/src/main.rs:112-113` (the standard it fails to meet).
Argument and acceptance arm: SC-4.

---

## Cross-crate: per-app stores outside plugin-db

*Moved out of the design document on 2026-08-27, where these sat under the
heading "The pattern is not confined to plugin-db".*

Two more per-app stores outside this plugin have the same shape as the per-app
metadata caches above, and they matter to this design because they sit on the
paths it depends on. Neither is plugin-db's to fix, but a design that claims a
bounded per-app footprint while these are unbounded is claiming something the
process does not deliver.

### L19 - the worker's env cache retains decrypted secrets per app, indefinitely

**(Every citation below re-read against the tree 2026-08-27, not taken from the
review.** The code states the behaviour in its own comments, which is why this
is a design question rather than a bug report: both decisions are deliberate and
individually reasonable.)
`SharedEnvs` is a process-wide `HashMap<Uuid, Arc<CachedEnv>>`
(`worker/src/sync.rs:81-98`) holding the complete validated env JSON. Isolate LRU
eviction deliberately leaves the entry behind (`worker/src/cache.rs:780-785`),
and the only reclamation is
`e.retain(|app_id, _| versions.contains_key(app_id))` against the control
plane's known-app set (`worker/src/sync.rs:189-190`) - so retention tracks
"every app that still **exists** and was ever loaded by this process", not "apps
with a live isolate".

Both halves are deliberate and say so. Eviction declines to touch the env
because it is process-wide and another thread may still need it; the GC exists
because "per-thread reconcile only fires for locally-cached apps, so an app
that's been LRU-evicted from every thread gets no cleanup". Each decision is
locally right. Their composition is the leak: the only thing that can free an
entry is an app being **deleted**, and nothing frees one for an app that merely
stopped receiving traffic. The
control-plane endpoint that fills it decrypts every secret to build the response
(`control/src/env_store.rs:353-390`). The security consequence is the one worth
stating plainly: **a worker's plaintext-secret residency is a function of its
uptime**, and deletion GC is an eager cleanup rather than a capacity policy.

SC-5 carries the other half of this, and only the other half: **the GC key must
include the app incarnation**, not the app id alone. That is a contract
requirement on the service SC-5 defines, not a restatement of the leak above.

### L20 - the meter's drain is a stop-the-world proportional to app history

**(The unbounded half of this is FIXED as of 2026-08-27 - `drain` now evicts
apps that drained empty, and `Meter::tracked_app_count` exists so the bound can
be asserted rather than trusted. The stall itself remains; see the end of this
entry.)**
`Meter::drain` takes the **exclusive** lock on the process-wide app map
(`metering/src/meter.rs:216`) and holds it while iterating *every app ever
touched*, parsing a `Uuid` per app and allocating a `source.clone()` and a fresh
`Uuid::now_v7().to_string()` per emitted metric. It removes nothing, so the map
only grows. On the default ten-second cadence (`metering/src/outbox.rs:52`) that
is a periodic global pause whose length grows with the number of distinct apps
the process has ever served - and every `env.db` / `env.kv` / `env.storage`
usage increment blocks behind it.

The precise bug is visible in the code's own annotations. The function carries
`#[allow(clippy::readonly_write_lock)]` - clippy correctly observed the write
guard is never used to mutate, because the per-app drain works through interior
atomics. The lock is taken purely as **exclusion**, and the doc comment says
exactly why: "so no increment interleaves a partial drain (fixed-counter `swap`s
plus a `custom` take must be atomic **per app** relative to **that app's own**
increments)". The stated requirement is per-app atomicity; the implementation
buys it with a process-wide exclusive lock. That is the same
bound-to-the-wrong-thing shape as L10 above - a correct requirement enforced at
the wrong granularity - and the fix follows from the comment rather than
contradicting it: make the exclusion per-app (`Arc<AppCounters>` values, drained
one at a time under their own guard), and the global lock disappears along with
the pause.

One correction to how this was reported to us, because it affects the fix: the
review called the increment path contended "contrary to its lock-free comment".
The increment fast path takes a **read** guard (`meter.rs:177-181`), and
concurrent readers do not exclude one another, so increment-versus-increment is
genuinely uncontended and the comment's intent is defensible. The contention is
entirely increment-versus-drain. Optimizing the fast path would buy nothing;
only removing the global write lock does.

**What landed, and what deliberately did not.** `drain` now evicts any app whose
counters drained empty, so the map is bounded by apps with traffic since the
last drain rather than by every app the process has ever touched - and because
the scan is over that same map, its exclusive-lock hold time is bounded by the
same number. Regression test `drain_evicts_apps_that_went_idle`, red before the
change on the retention assertion.

The trade is worth stating because it is a real one rather than a free win: the
type promised a write lock "only on first-touch per app", and eviction means an
app idle across a drain pays first-touch again on its next increment. That is
the right side of the trade - an app busy enough for the write lock to matter
never idles through a whole ten-second window, and one that does idle is by
definition not hot - but it IS a behaviour change, not a pure deletion.

**The stall itself was left alone on purpose.** Making the exclusion per-app is
a different change with a different risk profile, and bundling it would have
meant shipping an atomicity refactor under cover of a leak fix. The unbounded
*growth* was the part that made the stall unsurvivable at the platform's target
scale; the fixed-size stall is a normal optimization that can be argued on its
own merits.

## Cross-crate: the worker

### L21 - isolate admission builds the runtime before it can know it will be rejected

*Moved out of the design document on 2026-08-27, where it had been spliced into
the middle of L20's meter analysis. It is not a per-app store, which is why it
read as inserted there.*

`load_app` compiles and initializes the V8 runtime first, and only then takes
the cache lock to discover that a full cache whose entries are all leased cannot
evict, at which point it drops the runtime it just built
(`worker/src/cache.rs:490-505`). Under saturation, requests for distinct cold
apps repeatedly pay a full compile-and-evaluate for a runtime that can never be
admitted - which is exactly the long-tail regime the platform's stated scale
implies.

This one deserves care rather than a straight inversion, because the ordering is
**deliberate** and the code says why: "Only mutate the cache after the new
runtime has initialized. A corrupt descriptor during reload must not evict the
last-good isolate." That guarantee is real and must survive. But it only governs
the *reload* case, where the app is already in the cache. The wasted work is in
the disjoint case - a **new** app arriving at a full, fully-leased cache - and
the two conditions that identify it (`isolates.len() >= max_size` and
`!isolates.contains_key(&app_id)`) are both cheap and read-only. Only
`evict_lru` mutates. So a preflight can reject the hopeless case before the
build while leaving build-before-replace untouched for same-app reloads. Stated
that way it is not a trade-off at all, which is the useful thing to notice: what
looked like "safety ordering versus wasted work" is two different cases sharing
one code path.

---

## The open decision: L12

L12 is not a defect entry like the others and this section says so in its own
heading. It is **the open decision of this document set**: a hard ceiling on the
feature the design exists to serve, with four options weighed below and a
recommendation that **remains a recommendation**. Nothing here decides it.

### L12 (NEW) - live subscriptions cost one replication slot per (app x worker), ceiling 10

**NEW - the hard scaling wall, verified 2026-08-27**

**Live subscriptions cost one PostgreSQL logical replication slot per (app x
worker process), and the default ceiling is 10.** `worker_slot_name` composes
`__zs_slot_<sha(app)>__<sha(worker)>` (`replication.rs:108-115`) - per pair,
because a logical slot admits only one active consumer - and
`wal_consumer.rs:368` opens a **dedicated, non-pooled** replication connection
for each. `cdc_lifecycle.rs` refcounts leases per app with **no cap on apps**.
Measured on this branch's own dev database: `max_replication_slots=10`,
`max_wal_senders=10` (the PostgreSQL defaults;
`deploy/compose/docker-compose.yml` sets only `wal_level` and
`max_prepared_transactions`). The 11th concurrently-subscribed pair fails
`pg_create_logical_replication_slot`. `max_replication_slots` is a
**restart-only** shared-memory GUC and every active slot is a walsender backend
competing for `max_connections`, so **no tuning makes one-slot-per-tenant reach
the platform's stated scale**. Mitigating and worth stating: slots are
demand-driven - only `collection.openSubscription()` reaches `acquire`
(`v8_classes/subscription.rs:315`) - so the bound is *concurrently subscribed*
apps, not all apps.

**Evidence:** `replication.rs:108-115`, `:208-212`; `wal_consumer.rs:368`;
`cdc_lifecycle.rs:87-111`; server GUCs measured directly

### L12b - a crashed worker's abandoned slot can take down the whole cluster

**Availability, same finding**

**A crashed worker's abandoned slot can take down the whole cluster, and only
the tenant can clean it up.** The clean path drops the slot when the last lease
goes, but a crash leaves `active=false` with `restart_lsn` pinned - PostgreSQL
then cannot recycle WAL past it and `pg_wal` grows without bound, which is a
**cluster-wide** failure affecting every tenant on it, caused by one tenant's
worker dying. The reaper `drop_abandoned_slots` exists
(`replication.rs:466-499`) but is reachable **only from tenant JS** via
`db.replication.dropAbandoned()` (`replication_ops.rs:48-68`,
`v8_classes/replication.rs:78`); verified 2026-08-27 that no control-plane or
worker code calls it. Operator-side cleanup of an operator-side failure is
delegated to the tenant, who has no reason to run it.

**Evidence:** `replication.rs:466-499`; `replication_ops.rs:48-68`; grep across
control/worker finds no caller

### RETRACTED: "the ceiling of ten already binds on one dev box"

**This section claimed a measurement it did not have, and the claim is
withdrawn (2026-08-27).** The narrative below is left in place because the
retraction is more instructive than a silent deletion, but do not cite it.

**What was actually measured, afterwards:** a consumer holds a **peak of one**
replication slot (sampled `pg_replication_slots` every 300ms across the CDC test
run: peak 1, ceiling 10, zero before and after). Five concurrent consumers
therefore drew five of ten. **Nowhere near saturation.**

**The real cause of the failure** is a test-cleanup blast radius, now filed as
its own defect: `integration.rs:2018-2024` runs
`pg_drop_replication_slot(slot_name) ... WHERE slot_name LIKE '__zs_%' AND
active = false` - dropping every zeroship slot on the **server** that is
momentarily inactive, including other runs' - and `CDC_TEST_WORKER_ID` is a
fixed constant, so concurrent runs mint identical slot names. Same shape as the
`reset_world()` defect fixed the same day: a per-test cleanup acting on global
state it does not own.

**The tell was in the error the whole time.** It read
`CDC must reach START_REPLICATION`. Slot exhaustion fails at slot *creation*,
not at start. The message never matched the hypothesis, and it took a
measurement rather than a re-reading to notice - because the hypothesis
flattered a conclusion this document already held.

**L12 does not need this incident.** One slot per (app x worker), a server-wide
ceiling of 10, a restart-only GUC, and each slot also a walsender competing for
`max_connections` - that does not reach millions of apps by any arithmetic. The
options below stand unchanged. What changes is that they rest on the mechanism,
not on an anecdote.

---

Before the options, the narrative as it was written (retained for the lesson,
not as evidence):

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

### How L12 was nearly missed

L12 and L13 were in the performance round's opus report and this document had
folded **none** of them - the revision pass concentrated on one reviewer's
findings and treated the other two reports as already-absorbed. That is a
process failure worth naming, because it is the same shape as the enumeration
bugs catalogued above: three sources were consulted, one was read closely, and
the coverage claim was made over the set rather than over what was actually
read. L12 in particular is the single most consequential finding of the round -
a hard ceiling of ten on the feature this design exists to serve - and it sat
unread in a file on disk.

---

## Reclassified from v3: not live defects

Two entries from v3 are reclassified rather than kept, because neither is a
defect with a failing pre-fix test.

### L7 is not a defect, it is a missing gate arm - and it is now partly fixed

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

### The v3 `get_column_key` finding was misattributed

*This entry was numbered L9 in v3. It keeps its text and loses the number: the
live L9 above (the masked-filter oracle, DECIDED) is a different defect, and
SC-6, the index and the design all cite that one. Two unrelated findings shared
one label until 2026-08-27.*

`install_get_column_key_function` is
`#[cfg(any(test, feature = "test-helpers"))]` (`bootstrap.rs:612`) - the same
gate L6 is about - so `get_column_key`'s `EXECUTE TO PUBLIC` over a
`key_id`-keyed table cannot be live in `main`. It is restated as a
**constraint on the step-7 provisioner**: when `__zeroship_admin` is stood up
for real, that function must not be recreated with its current grant or its
current keying.
