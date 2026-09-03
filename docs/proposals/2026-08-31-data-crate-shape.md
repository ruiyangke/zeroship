# The data-plane crate shape

**Status:** design settled, execution begun. Five preparatory moves have landed; no new crate exists
yet. Rewritten 2026-09-01 to carry the final shape only - six review rounds of superseded designs,
refuted proposals and correction archaeology were removed. What survives is what binds.

---

## The rule that decides everything

**The goal is a maintainable system under clean architecture. The crate split is the consequence, not
the objective.**

> **Source dependencies point INWARD.** Domain types know nothing of use cases; use cases know nothing
> of adapters; adapters know nothing of each other. Frameworks and drivers - V8, `compio-postgres`,
> `rusqlite` - are the outermost ring, and nothing inner may name them.

```
    frameworks & drivers   V8 . compio-postgres . rusqlite
    adapters               plugin-db (Rust<->V8)  .  data-postgres . data-sqlite
    use cases              crud pipeline . transaction reducer . masking policy
    domain                 DbError . TypedCell/TypedRows . ChangeEvent/ChangeOp . descriptor
```

Six review rounds found a long list of separate defects. Under this rule they are **one defect in
five places** - a dependency pointing outward:

| finding | the violation |
| --- | --- |
| `DbError` names `compio_postgres` in 8 signatures | domain -> driver |
| `to_op_error` returns `zeroship_runtime::OpError` from the core | domain -> delivery mechanism |
| `crud/`, `transaction/` return `OpResult`/`ResolveValue` | use case -> delivery mechanism |
| both backends call `crate::v8_bridge` for row decode | adapter -> a *different* adapter |
| `crud/mod.rs:554` stores a V8 fn pointer chosen on a data predicate | use case -> adapter internals |

This is why the answer is a **refactor, not a file move**, and why three of the six original open
decisions turned out to be redesigns. It is also the test for anything this document does not cover:
*does this make a dependency point inward?* If not, it is not the fix.

---

## The fence, and the hole in it

Vendor neutrality in the core is enforced by the **manifest**, not by discipline. `data-core`'s
`Cargo.toml` will list neither `compio-postgres` nor `rusqlite` nor `zeroship-runtime`. You cannot
name `compio_postgres::Error` in a crate that does not depend on it - that is `E0433`, at compile
time, for everyone, forever. No lint, no reviewer, no census run.

**This repository already uses the technique, and it is already enforced on the exact crate the split
renames.** `serialize_derive_is_structurally_impossible`
(`crates/zeroship-data-query-builder/tests/no_sql_text_escape_hatch.rs:210`) reads the crate's own manifest,
collects every name declared under `[dependencies]`, `[dev-dependencies]` and
`[build-dependencies]`, and asserts the set is **empty** - its own comment: "the manifest declares no
dependencies at all, so `serde` is not in scope and the derive would not compile even if someone
wrote it."

Two consequences worth naming. It enforces **zero dependencies**, not "no serde", so it is already
the fence this document wants rather than an analogy to it. And it guards `zeroship-data-query-builder`,
which becomes `data-query-builder` - so "ZERO dependencies, LEAF, and that stays load-bearing" in the
target block is not an aspiration a future reviewer must uphold. It is a test that fails the moment
anyone adds a line.

**But `E0433` fences the SPELLING, not the TYPE.** It stops a crate *naming* a path. It says nothing
about a crate *holding a value* of that type, and one is handed across today through a public field:

- `crates/zeroship-schema/src/error.rs:31` declares `pub source: compio_postgres::Error`.
- A `From<SchemaError> for DbError` impl passes that live value into a Postgres classifier.
- **The token `compio_postgres` does not appear on the line.** A crate whose manifest omits the
  driver compiles it, because inference never needs the path.

**THIS DOCUMENT CITED THAT IMPL AT `plugin-db/src/error.rs:942`, AND ITS OWN SEQUENCED WORK MOVED
IT.** Re-measured 2026-09-01: `error.rs` names `SchemaError` **zero** times. Move 1 (`6f3a482f6`)
relocated the impl into `backend/pg_error.rs` - the PostgreSQL vendor tier, where naming a vendor is
legitimate rather than a violation.

The mechanism above still stands: a public field of vendor type crosses a boundary without spelling
the path, and no manifest fence can see it. What no longer stands is the instance - the impl that
would have dragged the driver into the core has already moved out of the core's way. So the claim
narrows to the structural one: the dependency CLOSURE `data-core -> zeroship-schema ->
compio-postgres` is real and stays real until `zeroship-schema` drops the driver. No vendor VALUE
need cross for that to bind.

Recorded because it is this document's own warning coming true: a claim and the move that falsifies
it, edited independently. The claim was not re-measured after the move it sequenced.

`data-core` must declare `zeroship-schema`, because it owns `DbError` and two **unconditional `impl`
blocks** over `zeroship-schema` types. An `impl` with no `cfg` compiles in every build whether or not
anything calls it. So the floor is `data-core -> zeroship-schema -> compio-postgres`, and decision 5
is violated by the boundary this document proposes rather than by code that can be tidied under it.

That is what `tests/vendor_embedding_gate.sh` exists to catch, and why it must run **before** each
move rather than after - see the caveat in the execution order.

---

## The target

```
libs/compio-postgres            driver, unchanged

zeroship-data-query-builder     the typed query grammar. ZERO dependencies, LEAF, and that stays
                                load-bearing. (today's zeroship-data-query-builder, renamed)
zeroship-data-core              the contract every backend implements, PLUS the backend-neutral
                                layer both already share: encryption, MaskKind, TypedCell/TypedRows.
                                -> data-query-builder
zeroship-data-postgres          impl of the core contract.   -> data-core, compio-postgres
zeroship-data-sqlite            impl of the core contract.   -> data-core, rusqlite
zeroship-data-cdc-server        service tier: WAL stream, slot authority, reaper. Peer of
                                zeroship-migrate-server.
                                -> compio-postgres, zeroship-core. NOT data-core, NOT data-postgres.
zeroship-data-engine            the data plane's actual logic: crud pipeline, transactions, exec,
                                broker. -> data-core, AND -> data-postgres + data-sqlite for as
                                long as it owns BackendHandle. See the open question below.
zeroship-plugin-db              THIN. The worker/runtime plugin ADAPTER ONLY. -> data-engine.
                                NOT -> data-cdc-server; the worker must not link the relay.

zeroship-core::change_event     the cross-process event type, beside usage_event and
                                replication_names, which are already there.

zeroship-migrate-*              the engine, dialect-complete, untouched
zeroship-migrate-server         the migration service host
```

The count is **six** because the modules are six things, not because six is a nicer number. `crud/` +
`transaction/` + `exec.rs` is the single largest block in the crate, and it is pipeline and reducer -
not a contract, not a driver, not the grammar, not the relay. Putting it in `data-core` would make the
core the big crate again with drivers attached, which is the thing the split exists to undo.

---

## `plugin-db` is a very thin layer joining Rust to V8

Operator decision, 2026-08-31. It keeps its name and loses almost everything else: the V8 marshalling
boundary and the `env.db` op surface, and that is all. That is `v8_classes/`, `v8_bridge.rs`, the
`DbPlugin` part of `lib.rs`, and `tx_scope.rs` - **around a tenth of today's crate.**

**Three things this makes mandatory that were previously weighed as options:**

1. **The protocol inversion is required.** The 39 V8-signature functions belong in the thin layer -
   they *are* the boundary. Their `async move` bodies are not - those bodies are query pipeline. The
   engine must stop returning `OpResult`/`ResolveValue` and return data the adapter lowers. There is
   no version of "thin" that survives leaving the pipeline inside the dispatch functions.
2. **Row decoding must leave `v8_bridge.rs`.** `:432`/`:471`/`:498` take `&compio_postgres::Row`, and
   `lib.rs:559`/`:588` are always-compiled `pub` bench exports naming the same. A thin layer joining
   Rust to V8 cannot link a database driver.
3. **`context.rs` does not stay.** It holds `Option<Rc<Pool>>` (`:178`) and
   `TxConnection::Postgres(OwnedPooledClient)` (`:80`), has **zero** `v8::` references, and its
   callers are overwhelmingly `exec.rs` and `transaction/driver.rs` rather than the adapter fringe. A
   per-thread cache of live vendor connections is not a V8 marshalling concern.

---

## `zeroship-schema` is DELETED, not moved

The large majority of the crate is deleted; a small vocabulary core is re-homed.

It is **not a vendor**: `query.rs` owns `pub enum SqlDialect { Postgres, Sqlite }` (`:107`) and
threads `dialect` through pervasively. It is dialect-PARAMETERISED - one builder serving N dialects -
so folding it into `data-postgres` would put SQLite DDL emission inside the Postgres crate. It sits
above the vendors by construction.

And it does **not** fold into the query-builder either: moving a string builder into the crate written
to obsolete it is the "two intermediate versions" the pre-launch stance forbids.

| piece | production callers |
| --- | --- |
| `query.rs` DDL builders | **ZERO.** Every hit in `plugin-db/src` is a doc comment; the two real uses in `crud/write_pipeline.rs` are past its `#[cfg(test)]` |
| `query.rs` DML builders | live, and `data-query-builder` is their replacement |
| `diff.rs` `compute_diff` | test-only; the LIVE twin is `zeroship-migrate-core/src/schema/diff.rs:418` |
| `diff.rs` introspection | the PG impl is **test-gated**, not gone - see the correction below |
| `diff.rs` TYPES | **7 live, 7 not** - see the correction below |
| `mask_codec.rs`, `ident.rs`, `descriptors.rs`, `error.rs` | live |

**The TYPES row said "`MaskKind`, `Classification`, `MaskMeta`, `EncryptionMeta` - live"
until 2026-09-02, and it was wrong in BOTH directions.** Measured at `d883c56a3` by
counting `diff::X` references in `zeroship-plugin-db/src` while excluding three
things, each of which changes the answer:

- **doc comments** - the differ's entire `src` presence is two `///` citations, at
  `backend/mod.rs:591` and `backend/sqlite/mod.rs:1085`. Counting prose as use
  makes `compute_diff` look live;
- **test-gated MODULES** - `backend/pg_introspect.rs` is declared
  `#[cfg(any(test, feature = "test-helpers"))]` at `backend/mod.rs:97`, and it is
  the only file that spells `use zeroship_schema::diff::{…}`. That single
  exclusion moves the whole introspection surface;
- **`#[cfg(test)]` REGIONS inside shipped files** - `WrappedType` reads as live off
  three `assert!`s at `backend/sqlite/mod.rs:2533/:2553/:2565`, and `LiveSchema`
  off three `assert_impl` helpers. The file is shipped; those lines are not.

| | items |
| --- | --- |
| **live** (7) | `MaskKind` 10, `LiveSchema` 4, `MaskMeta` 2, `ColumnInfo` 1, `EncryptionMeta` 1, `ForeignKeyInfo` 1, `IndexInfo` 1 |
| **test- or prose-only** (7) | `compute_diff`, `ChangeKind`, `ChangeClass`, `DiffOp`, `classify_add_column`, `Classification`, `WrappedType` |

So `Classification` is in the old "live" list and has **zero** shipped references, while
`LiveSchema`, `ColumnInfo`, `ForeignKeyInfo` and `IndexInfo` are live and were omitted.
Deleting on the old row strands `SqliteBackend`'s `SchemaIntrospect` impl, whose associated
type is `crate::diff::LiveSchema` (`backend/sqlite/mod.rs:827`) and which builds one at
`:863`. The introspection is not "gone": its PG half is test-gated, its SQLite half ships.

Also note `MaskKind`'s 10: nine are in `read_set.rs`, whose own header records that nothing
below it runs today. Its one live consumer is `crud/mask_pass.rs:81`.

A second spelling is why this needed re-measuring at all. `plugin-db/src/lib.rs:165`/`:167`
re-export the module (`pub(crate) use zeroship_schema::diff;`, cfg-forked), so almost every
consumer says `crate::diff::X` and NOT `zeroship_schema::diff::X`. A grep for the qualified
path alone finds one file and reports the differ as unreferenced.

`zeroship-migrate-core` does **not** depend on `zeroship-schema` - checked in its manifest, not
inferred. Its same-named `build_*` references are its own `schema/query.rs`.

### THE ENGINE HAS ALREADY DONE THIS DELETION, IN THE MIRROR DIRECTION

Measured 2026-09-02. `crates/zeroship-migrate-core/src/schema/mod.rs` is a rewrite of
`zeroship-schema/src/lib.rs` - same five modules, same headings - and its opening paragraph
records the cut:

> the data-plane query language that used to ride along here - the find and aggregate
> builders, the MongoDB-style filter->WHERE translator, the query limits - had **zero
> engine callers** and lived here only for a consumer (plugin-db) that is not in this
> repo, so it was deleted.

So the engine kept the write/diff/describe half and deleted the data-plane half.
`zeroship-schema` is the exact mirror: it kept BOTH, and the half the engine kept is the
half nothing here calls. Every one of its five modules has a live twin, and the twins are
split across two crates:

| `zeroship-schema` | live twin | code lines (schema / twin) | differing |
| --- | --- | --- | --- |
| `descriptors.rs` | `zeroship-migrate-backend/src/descriptors.rs` | 16 / 16 | **0** |
| `error.rs` | `zeroship-migrate-backend/src/schema_error.rs` | 22 / 22 | **0** |
| `mask_codec.rs` | `zeroship-migrate-backend/src/mask_codec.rs` | 296 / 365 | 117 |
| `diff.rs` | `zeroship-migrate-core/src/schema/diff.rs` | 1488 / 1666 | 652 |
| `query.rs` | `zeroship-migrate-core/src/schema/query.rs` | 10346 / 3900 | 9546 |

Comments and blank lines stripped, so the two crates' differing doc prose over the same
items does not count. `descriptors` and `error` are **line-for-line identical code** - 38
lines carrying `VectorMetric`, `EncryptionMode`, `GeoPoint` and `MaskSentinelError`.

The `query.rs` row is the whole story in one number: the schema copy is 2.6x the engine's
because it still carries the DML half the engine deleted - and that DML half is the only
part plugin-db calls. `migrate-core` reaches the three leaf modules by re-exporting them
from `migrate-backend` (`pub use zeroship_migrate_backend::{descriptors, mask_codec,
schema_error as error};`), which is the shape this split should copy rather than reinvent.

The sentinel WRITE half is dead here and live there: `build_mask_sentinel_comments` and
`build_encryption_sentinel_comments` are called from shipped code at
`zeroship-migrate-postgres/src/schema.rs:305-306`, while `zeroship-schema`'s own copies at
`query.rs:297-298` are reached only from the DDL emitter that has no production caller.

**The caveat that stops this being cheap.** "No production caller" is not "safe to delete". The DDL
builders are reached from `sqlite_integration.rs` and `integration.rs` by tests pinning real DDL
behaviour. The cost is deletion PLUS migrating those onto the migration engine's renderer.

---

## `backend/mod.rs` is DISSOLVED, not tiered

`BackendHandle` names both vendors, so the file cannot sit in any single tier - which is why it forms
the hidden leg `ENGINE -> SQLITE -> backend/mod.rs -> ENGINE`. Three reviewers converged on the same
resolution:

1. **`ChangeStream` is the blocker, and it is not vendor-free.** Its two guard return types
   (`BrokerPauseGuard`, `SchemaPendingGuard`) have `Drop` bodies that ARE six `crate::broker::` calls.
   Move `ChangeStream` to core and the leg goes with it: `data-core -> data-engine` against
   `data-engine -> data-core` is a Cargo cycle, unbuildable. **Relabelling the cycle is not closing
   it.**
2. **`BackendHandle` goes to ENGINE.** Not core (it names both vendors), and not the adapter: its
   consumers are engine-tier, so placing it above them mints new upward edges.

   **Re-derived 2026-09-01; the earlier consumer list was wrong in both directions.** It named
   `crud/read_pipeline.rs`, `crud/unmask.rs` and `crud/mask_policy.rs`, none of which name the type
   in code, and omitted `context.rs` and `cdc_lifecycle.rs`, which do. The code-level consumers are
   `exec.rs`, `transaction/driver.rs`, `transaction/mod.rs`, `context.rs`, `drop_namespace.rs`,
   `cdc_lifecycle.rs` and `crud/mod.rs`.

   The correction **strengthens** the placement. No vendor file, no `error.rs` and no `lib.rs` names
   it in code - every hit in those is a doc comment - so neither backend consumes it and neither does
   the adapter. "Every below-ENGINE hit is a doc comment" survives; a filename-level grep is what
   made it look otherwise, and briefly suggested an unavoidable vendor cycle that does not exist.

   **It also couples the two open questions.** `context.rs` is among the heaviest consumers, and
   `context.rs` is the module this document has never placed. Wherever `BackendHandle` lands,
   `context.rs` either follows it or acquires an edge to it.

   What this does NOT change: the engine still depends on both vendor crates, because
   `BackendHandle`'s **definition** names `Rc<PostgresBackend>` and `Rc<SqliteBackend>`. That edge
   comes from owning the type, not from consuming it, so no consumer census can retire it.
3. **The cut is to DELETE `pause_broker` and `engage_schema_pending`**, not to port them. No
   production caller; both impls are byte-identical self-less expressions; and the one production
   pause consumer already routes around them - `cdc_lifecycle.rs` calls
   `crate::broker::SuppressGuard::activate(app_id)` directly in BOTH the Postgres and the SQLite
   arm. The surviving `ChangeStream` is then genuinely core-safe.

   **ALL THREE GROUNDS VERIFIED 2026-09-02, AND THE CUT IS STILL NOT A DELETION OF TWO METHODS.**
   The grounds hold exactly as written: `cdc_lifecycle.rs:273` and `:287` both call
   `broker::SuppressGuard::activate(app_id)` directly, and the two impls
   (`change_stream_pg.rs:269,276` and `backend/sqlite/cdc.rs:785,792`) are byte-identical and
   never touch `self` - free constructors wearing a trait method's clothes.

   What the decision does not account for is WHO ELSE REACHES THE GUARDS. Both
   `BrokerPauseGuard::new` and `SchemaPendingGuard::new` are `pub(crate)`
   (`backend/mod.rs:989`, `:1353`), and `tests/sqlite_integration.rs` is a separate crate. Three
   integration tests reach the guards ONLY through these methods or through
   `SqliteBackend::{pause_broker,engage_schema_pending}_for_tests`, which exist precisely BECAUSE
   the constructors are crate-private. One of the three
   (`sqlite_integration.rs:1679`) exists specifically to fence the trait method against being
   detached from the guard construction, so deleting the method deletes that test's subject.

   So the cut costs a decision the paragraph above does not make: either widen
   `Guard::new` to `pub` under `test-helpers` (the exact fence-widening Phase 0.5's first audit is
   trying to reduce), or delete all three tests, or keep a test-only accessor - which is what
   `*_for_tests` already is. Deleting the trait methods does NOT remove the need for the wrappers.
   Direction unchanged; the cost is larger than two methods and should be decided, not discovered
   mid-edit.

   **The SQLite arm says why, and the reason is a behaviour difference rather than a preference** -
   it takes "the startup-only suppression guard, whose Drop merely re-enables delivery; the general
   pause guard emits a Resync on Drop and would add a synthetic first message to every SQLite
   subscription." So the general guard is not merely unused here; using it would be a defect. That
   is the strongest argument for deleting rather than porting, and it is recorded in the code the
   deletion touches.

   **Where the deletion stops, because this is easy to over-cut.** What goes is the two *trait
   methods* on `ChangeStream` and their two impls (`change_stream_pg.rs`, `backend/sqlite/cdc.rs`).
   Every remaining reference is a test, a `*_for_tests` helper on `SqliteBackend`, or a doc comment.
   What STAYS is `BrokerPauseGuard` and `SchemaPendingGuard` as plain engine structs, and with them
   the `broker::engage_schema_pending` / `disengage_schema_pending` free functions their construction
   and `Drop` call - those are the guards' implementation, not the trait surface being removed. Delete
   the free functions too and the surviving guards stop working.

**Six vendor-free capability traits go to core**, not eight. `PgSqlExecutor` is ungated but not
vendor-free - its super-bound is `SqlExecutor<Client = compio_postgres::OwnedPooledClient>` and
`pool_handle` returns `&Rc<compio_postgres::Pool>` in a production signature. It goes to PG, or dies
with #114. `EncryptedColumn` belongs in ENCRYPT, not CORE: both impls bind
`type KeyHandle = crate::encryption::aead::AeadKey`.

**The enum stays; `dyn` is refused for measured reasons.** These are `async fn`-in-trait, so object
safety needs `Box<dyn Future>` per call - a per-CRUD-op allocation on the hottest path in the data
plane. The associated types cannot be erased without losing the concrete client that
`LockManager::acquire_advisory_lock` takes by `&Self::Client`. The backend set is closed, and an enum
is the canonical shape for a closed sum. Monomorphised dispatch is preserved, not replaced.

`Backend` is gated **on purpose** - it is a conformance marker so tests can assert the concrete
backends implement the sub-trait set. The production abstraction is `BackendHandle`.

---

## The decisions

**1. One query builder.** Wire the typed IR into the data plane and DELETE the string builder it was
written to replace. Not "leave both and revisit".

**2. CDC gets its own crate AND its own service** - a process that does not execute creator code.

**3. Keep `zeroship-migrate-mysql`.** The in-sourced engine stays dialect-complete so it does not
diverge from upstream, accepting that zeroship targets only PostgreSQL and SQLite. A stated trade.
Keeping the CODE and paying the BUILD are separable: `zeroship-migrate/Cargo.toml` declares no
`[features]`, so the whole MySQL dialect compiles for every dependant while nothing selects it - the
addon's only two callers hardcode `dialect: "postgres"`.

**4. NO RAW SQL IN THE ENGINE.** Adding a database must require ZERO changes to `data-engine`. This is
the acceptance test for decision 1 stated as an outcome: if supporting a new vendor means editing
`data-engine`, decision 1 has not landed.

The real surface is far smaller than the framing implies. A naive grep for statement keywords in
engine-tier files is mostly noise: most hits sit past a column-0 `#[cfg(test)]` or inside a
one-arm-gated module, and two are error-message strings beginning "UPDATE patch attempted to
overwrite..." that a keyword grep cannot distinguish from SQL. Opening the lines leaves this:

| site | disposition |
| --- | --- |
| `crud/unmask.rs` fetch encrypted cell, PG + SQLite | query builder |
| `crud/unmask.rs` fetch plaintext cell, PG + SQLite | query builder |
| `crud/unmask.rs` append audit row, PG + SQLite | query builder |
| `drop_namespace.rs:155` `DROP SCHEMA ... CASCADE` | **not the builder.** DDL; decision 10 removed DDL from the data plane and this one survived |
| `transaction/mod.rs:761` `BEGIN ISOLATION LEVEL` | **not the builder.** Transaction control - SQLite has no such syntax; belongs to the vendor's session capability |

So decision 4 is **three logical operations in `crud/unmask.rs`**, each written twice because each
carries its own dialect. Note WHY it is small: the search family was ported to the IR under #12 and
unmask was deferred then to avoid a collision. This is that deferral coming due.

**5. THE CORE AND EVERY OTHER NON-VENDOR CRATE NEVER EMBED A VENDOR DIRECTLY.** Not "should avoid" -
never. Already violated in the tree (#97), and decision 5 makes that a blocker rather than a finding.

---

## Is `data-engine` portable? No, and the measurement says why

**What is vendor-agnostic:** backend dispatch is sparse across the engine, and a large block of it has
ZERO dispatch sites - `crud/mask_pass.rs`, `system_fields_pass.rs`, `write_pipeline.rs`,
`encryption_pass.rs`, `bytes_pass.rs`, the production `transaction/reducer/`, and `broker.rs`. Those
survive a new vendor untouched. That is the argument against folding the engine into the backends, and
it holds.

**What is not portable, and it is three separate things:**

1. The engine hand-writes dialect SQL (decision 4 closes this).
2. `BackendHandle` is a closed two-arm enum, not an open trait. A third vendor edits the core type and
   every match arm.
3. The engine downcasts to CONCRETE vendor types, not capability traits: `as_postgres(&self) ->
   Option<&PostgresBackend>`, then `pg.vector_search(...)` under `use crate::backend::VectorIndex as _`
   - a TRAIT method on a CONCRETE receiver obtained by downcast. Tracked as #119.

**And the typed IR that would deliver portability is not wired at all:** `grep -c zeroship_data_query_builder`
across `crates/zeroship-plugin-db/src/` returns **0**. `data-core -> data-query-builder` is not an
edge that exists and is not one relocation away; it appears only after the executor is retyped.

---

## Where every module lands

| destination | modules |
| --- | --- |
| `plugin-db` (thin) | `v8_classes/`, `v8_bridge.rs`, `lib.rs` (the `DbPlugin` part), `tx_scope.rs` |
| `data-engine` | `crud/`, `transaction/`, `exec.rs`, `broker.rs`, `read_set.rs`, `tx_route.rs`, `BackendHandle` |
| migrate-server (teardown coordinator) | `drop_namespace.rs` - **reassigned 2026-09-02, see below** |
| `data-core` | `error.rs` (less `to_op_error`), `descriptor.rs`, the six vendor-free capability traits |
| `data-encryption` (if split) | `encryption/` |

**Two entries left this table on 2026-09-02, under Phase 0.5's own audits.**

`cross_app_fk.rs` is **deleted**, which is the dead-code decision reaching its
first verdict rather than a change of assignment. 235 lines and eleven tests,
exported `pub` and ungated so it shipped in every binary, with zero production
callers - its own rustdoc said "in a default build, nobody". It could not be
wired: decision 10 removed all DDL from the crate, so nothing there sees a
`refTarget` any more. The live refusal is the engine's `validate_ident`, and its
two surviving tests moved beside it (`bare_identifier_tests` in
`zeroship-migrate-core/src/render/declarative.rs`) because nothing had ever
asserted that refusal.

`EncryptedColumn` is **deleted** too, so `data-encryption` would carry only
`encryption/`. Its two impls were identical line for line, every `KeyHandle` in
the tree bound to `AeadKey`, and both backends held the same `KeyStore` from the
same source - a vendor-shaped trait with no vendor content, whose only effect
was making the engine ask which backend it was on to reach code that does not
depend on the answer.

**`drop_namespace.rs` is REASSIGNED off `data-engine`, and it is not a
dead-code deletion.** The dead-code audit reached it and returned a third
answer: neither delete nor wire, but MOVE. It is the only record of the
privileged PostgreSQL teardown ORDER (subscription gate -> broker drain -> slot
teardown -> `DROP SCHEMA CASCADE` -> `DROP ROLE`), and unlike the two modules
deleted above it has real evidence - six integration tests in
`tests/integration.rs` driving it against live PostgreSQL.

The assignment was wrong because those steps are privileged and the entry
point's own doc requires the caller "must not be the worker login".
`data-engine` is linked by `plugin-db`, which is linked by the worker, and
AGENTS.md's standing invariant puts privileged work in "a separate service that
does not execute creator code". Nothing is exposed today - the `#[cfg]` gate at
`lib.rs:266` keeps it out of every shipped binary - so this is a destination
correction, not a live defect. The module's own header already named the
destination: a teardown coordinator behind `zeroship-migrate-server`.
| `data-postgres` | `backend/postgres.rs`, `pg_error.rs`, `pg_introspect.rs`, `PgSqlExecutor` |
| `data-sqlite` | `backend/sqlite/` |
| `data-cdc-server` | `wal_consumer.rs`, `replication.rs`, `slot_reaper.rs` |

### The eight modules this table did not assign

Measured 2026-09-02 by diffing plugin-db's `mod` declarations against the rows
above. An unassigned module is an extraction blocker in the most literal way:
when a tier becomes a crate, every `crate::X` it names must resolve to a crate
at or below it, and a module with no destination has no answer.

| module | lines | consumed by | destination |
| --- | --- | --- | --- |
| `budgets` | 35 | engine 1, vendor 1 | **`data-core`** - three `const u32` timeouts named by BOTH `backend/pg_session_sql.rs` and `transaction/driver.rs`, so it has to sit below both |
| `metrics` | 77 | engine 8 | **`data-engine`** |
| `system_shape_charter` | 234 | engine 4 | **`data-engine`** |
| `op_error` | 722 | adapter 7 | **`plugin-db` (thin)** - the table already calls `to_op_error` "the adapter's own translator" |
| `backend_selection` | 53 | engine 9, vendor 2 (both `cfg(test)`) | **`data-engine`** - it is the factory that picks a backend, not a backend |
| `auth` | 1407 | vendor 5, all test-gated | **DELETE - see below** |
| `service` | 611 | none of the three tiers | plugin registration; **`plugin-db` (thin)** unless a consumer census says otherwise |
| `test_support` | 336 | none of the three tiers | test-only; follows whatever it supports |

### `backend/mod.rs` is TWO destinations, and the trait count is nine

The `data-engine` row lists `BackendHandle`, and the `data-core` row lists "the
six vendor-free capability traits". Both are in `backend/mod.rs`, and that file
cannot go to one crate: the vendors have to see the traits, and the handle has
to see the vendors.

Measured 2026-09-02 - what `backend/postgres.rs`, `pg_*.rs` and `backend/sqlite/`
name under `crate::backend::`, with sibling references (`pg_error`,
`pg_session_sql`, `pg_row_json`, `sqlite`) excluded because those move with
their own tier:

| what the vendors need from the shared module | items |
| --- | --- |
| dispatch traits - **nine, not six** | `SqlExecutor`, `LockManager`, `SchemaIntrospect`, `SessionMinter`, `DialectBuilder`, `ChangeStream`, `VectorIndex`, `SpatialIndex`, `Backup` |
| shared value types they take or return | `SnapshotHandle`, `SnapshotOpts`, `PitrTarget`, `MintedToken`, `SchemaPendingGuard`, `BrokerPauseGuard`, `SessionInit`, `LockScope`, `BusyPolicy`, `ScalarRead`, `UnmaskAuditRow`, `SNAPSHOT_RESTORE_LOCK_TAG` |

So `backend/mod.rs` splits: **the nine traits and those value types go to
`data-core`**, below both vendors; **`BackendHandle` and its dispatch methods go
to `data-engine`**, above them. (`EncryptionMode`, `VectorMetric` and `GeoPoint`
also appear in the vendors' `crate::backend::` paths, but they are re-exports of
`zeroship_schema::descriptors` and are already below. `Backend` is the test-only
conformance marker of decision #112.)

**The graph is acyclic, and this is the measurement that settles it: NEITHER
vendor tier names `BackendHandle` anywhere, comments included.** A vendor naming
the handle would be `vendor -> engine` while `engine -> vendor` already exists
(the enum wraps an `Rc` of each), and that cycle would have to be broken before
either crate could be created. It does not exist.

#### One of the nine does not fit at rank 0 as written: `LockManager`

Measured 2026-09-02. Of the twenty items above, nineteen reference nothing but
`DbError`, `DbBinding`, `serde_json`, `std`, their own siblings, and one
`crate::query` path that is already `zeroship-schema`. The twentieth does not.

`LockManager::try_acquire_with_backoff` (`backend/mod.rs:407-...`) is a DEFAULT
METHOD BODY, not a signature: a five-attempt retry schedule
(`0+50+200+500+1000 = 1750ms`) that calls `compio::time::sleep` between
attempts and `tracing::warn!` on each contended try. Moving the trait verbatim
would put the async executor into `data-core`, whose manifest description is
"names no database driver, no V8, and **no runtime**".

Note that `tests/data_crate_closure_gate.sh` would NOT catch this: it pins the
closure against `compio-postgres`, `rusqlite`, `zeroship-runtime` and `v8`, and
`compio` itself is none of those. The fence is on drivers, and this is the
executor - so the gate would stay green while the crate quietly stopped being
what its own description says it is.

**`LockManager` is two things.** The CONTRACT - `type Client`,
`try_acquire_advisory_lock`, `LockScope` and its key derivation - is rank-0
vocabulary and moves. The POLICY - the schedule, the sleeps, the retry logging -
is engine behaviour and belongs with `data-engine`, reachable as a free function
or an extension trait over the contract.

That is the same cut this document already makes twice: `budgets` keeps the
NUMBERS at rank 0 and leaves the `SET LOCAL` strings that spend them with the
vendor; the error hierarchy keeps `DbError` neutral and leaves the per-vendor
translators with the vendors. A default body that sleeps is the third instance
of it, and the only one where the trait has to be cut rather than merely placed.

**DONE 2026-09-02.** The cut shipped as an extension trait, the same shape the
tree already used for `ToOpError`:

| where | what | names a runtime? |
| --- | --- | --- |
| `backend::LockManager` (rank 0) | 3 primitives + `try_acquire` / `release`, which only call `LockScope::to_keys` | no - measured over the trait's own 101-line span, code with comments stripped |
| `lock_policy::BoundedLockAcquire` (engine) | `acquire`, `try_acquire_with_backoff`, the `SCHEDULE` const | yes: `compio::time::sleep`, `tracing::warn!` |

Blanket-implemented (`impl<T: LockManager + ?Sized> BoundedLockAcquire for T`),
so a call site needs only the trait in scope. Three call sites took the import:
`backend/lock_guard.rs`, one shape-check in `backend/mod.rs`, and
`tests/sqlite_integration.rs`.

Two notes worth carrying into the remaining moves:

**The pinning test already existed** - `try_acquire_with_backoff_exhaustion_
yields_lock_contention` asserts `attempts == 5`, not merely that contention
surfaces. It moved with the policy it pins. A second test now pins the
schedule's arithmetic (5 entries, 1750ms, first wait 0, 1-based indices)
independently of the loop. Mutation: dropping the 1000ms attempt reddens
**exactly those two** of 682 lib tests.

**`cargo check -p zeroship-plugin-db` did not see the break.** The only
production caller, `backend/lock_guard.rs`, is `#[cfg(any(test,
feature = "test-helpers"))]`, so the default check compiled a green tree with
`acquire` deleted and its caller unbuilt. `--features test-helpers --all-targets`
found it immediately.

**And that command is not sufficient either - proved on the very next move.**
A cfg attribute CHANGES MEANING when the item it guards crosses a crate
boundary. `SnapshotOpts`, `BusyPolicy`, `SnapshotHandle` and `PitrTarget` carry
`#[cfg(feature = "test-helpers")]`; in `zeroship-plugin-db` that named
plugin-db's feature, and in `zeroship-data-core` it names data-core's. The two
are wired together (`test-helpers = ["zeroship-data-core/test-helpers"]`), so
they turn on together - and a check that turns them on sees four items that a
dependent, building plugin-db with default features, does not.

Measured 2026-09-02: `cargo check -p zeroship-plugin-db --features test-helpers
--all-targets` reported ZERO errors while `cargo check -p zeroship-worker
-p zeroship-cli` failed with four unresolved imports. The `pub use` had to be
split into an ungated arm and a `#[cfg(feature = "test-helpers")]` arm.

`zeroship-data-core`'s own manifest already warned about the neighbouring case -
"across a crate boundary `cfg(test)` is THIS crate's test build and never fires
for a consumer" - and the feature arm fails the same way for the opposite
reason. **The verification standard for every remaining move is therefore three
commands, not one**: the crate alone with helpers and all targets, the crate
alone WITHOUT them, and at least one dependent.

### `encryption/` moves BEFORE the vendors, and it can

Measured 2026-09-02. The vendor tiers name `crate::encryption::{KeyStore,
LocalKeySource}` eleven times, so `encryption/` has to be out of
`zeroship-plugin-db` before either backend can be its own crate - otherwise
`data-postgres` would declare a dependency on the adapter that depends on it.
The target block above already assigns it: `data-core` carries "the
backend-neutral layer both already share: encryption, MaskKind,
TypedCell/TypedRows".

**The one thing that could have blocked it does not.** A first scan showed
`encryption/` naming `compio` five times, which would put a runtime in the crate
whose manifest declares none. All five are `compio::runtime::Runtime::new()` at
`keys.rs:602-776`, and `keys.rs`'s `#[cfg(test)] mod tests` opens at `:432` - so
every one is inside the test module. Production code names no runtime.

Its production dependency set, measured over the four non-test files plus
`keys.rs:1-431`:

| needs | why it is fine for `data-core` |
| --- | --- |
| `aes_gcm`, `hmac`, `hkdf`, `sha2`, `zeroize`, `rand` | pure compute; no driver, no runtime, no V8 |
| `zeroship-core`, `zeroship-schema` | already `data-core` dependencies |
| `compio` | **dev-dependency only**, for the five test runtimes |

1,670 lines across five files. Its only upward reference is
`crate::backend::EncryptionMode`, which is `pub use
zeroship_schema::descriptors::EncryptionMode` - a re-export, so it repoints
rather than moves.

### The vendor cut's public surface is 21 items, 12 of which need promoting

Measured 2026-09-02, after the three blockers below were cleared. A crate
boundary caps visibility: a `pub(crate)` item becomes invisible to the engine
the moment the vendor is its own crate, and **the compiler cannot warn about it
in the current tree**, because the boundary does not exist yet. So the promotion
list has to be derived before the move, exactly as Phase 0.5 audit 1 was.

| tier | items the engine reaches for | already `pub` | need promoting |
| --- | --- | --- | --- |
| Postgres | 8 | 2 | 6 |
| SQLite | 13 | 7 | 6 |

**Postgres** - `pg_error::classify` and `postgres::PostgresBackend` are already
`pub`. Promote `postgres::{terminal_from_tag, terminal_from_status, cleanup}`,
`pg_session_sql::tx_session_setup_sql`, `pg_row_json::rows_to_json_value`,
`pg_autocommit::roled_rows`. Both `terminal_*` are called from `context.rs:203`
and `:209`.

**SQLite** - the seven types are already `pub` (`SqliteBackend`,
`SqliteChangeStream`, `TypedCell`, `TerminalIntent`, `SqliteSessionHandle`,
`SqliteCancelHandle`, `TerminalOutcome`). Promote
`vector::vec_to_le_bytes`, `spatial::point_to_blob`,
`reservation::{terminal_result, cleanup}`,
`row_json::typed_rows_to_json_value`, and the `change_sink` module itself.

**Two measurement notes, because both instruments were wrong first.**

A bare `\bname\b` sweep for these identifiers is worthless: `new`, `id`, `kind`,
`open`, `lock`, `error`, `session`, `vector` and `spatial` are all declared
`pub(crate)` somewhere in a vendor tier AND appear in most files in the crate.
The first run "found" cross-boundary uses in `broker.rs`, `op_error.rs` and
`lock_policy.rs` for nine such names, every one of them spelling rather than
reachability. Match QUALIFIED paths (`crate::backend::sqlite::…::name`) and
`use` lines instead - a cross-crate use has to name the path at least once.

The visibility check then under-reported by one, because its regex expected
`pub(crate) fn` and `terminal_from_status` is `pub(crate) const fn`. Read the
list as a floor and re-derive it against the compiler once the crates exist:
the promotion set is what `cargo check` will name, one error at a time.

### The vendor cut is blocked by three names, not by twelve thousand lines

Measured 2026-09-02, after the traits landed in `data-core`. The two vendor
tiers are 12,048 production lines - `backend/postgres.rs` and the five `pg_*`
files at 3,123, `backend/sqlite/` at 8,925 - and that size is what made this cut
look like the hard one. It is not. Resolving every `crate::` reference in those
files to a specific ITEM rather than a module gives a blocker set of three.

**Free, despite appearing in the first-segment counts:**

| edge | refs | why it is free |
| --- | --- | --- |
| `crate::diff::*` | 15 | `lib.rs:173` is `pub use zeroship_schema::diff;` - `data-core` already depends on that crate |
| `crate::query::*` | 25 | same shape, `pub use zeroship_schema::query` |
| `crate::backend::{the 8 traits, the 8 value items, VectorMetric, GeoPoint, EncryptionMode}` | ~30 | all now `data-core` re-exports; they repoint, they do not move |
| `crate::backend::{pg_error, pg_session_sql, pg_row_json, sqlite}` | 30 | intra-vendor |
| `crate::backend_selection::{open,new}_sqlite_backend` | 2 | both inside `#[cfg(test)] mod tests` (`sqlite/mod.rs:2064`, `mask_policy_store.rs:179`), verified by finding the nearest enclosing gate rather than by eye |

**The actual blockers:**

| item | refs | sites |
| --- | --- | --- |
| `crate::descriptor::collection_schema` | 4 | `postgres.rs:558,648`, `sqlite/mod.rs:1117,1235` - the `schema_hint` in vector-search and spatial-near, production on both arms |
| `crate::context::isolate_key_source` | 2 | `postgres.rs:103`, `sqlite/mod.rs:374` - backend construction, production on both arms |
| `crate::encryption::{KeyStore, LocalKeySource}` | 7 | rank question rather than a defect: ENCRYPT is its own tier, and if it sits below the vendors this is not an up-edge at all |

Thirteen references. `descriptor` and `context` are both ENGINE - `data-core`'s
own `lib.rs` already explains why the descriptor travels with the engine, since
both its production functions call `context::with`. So the vendor cut needs the
same contract-vs-behaviour question asked of exactly two functions, and an
answer to where ENCRYPT ranks.

**Read the size and the difficulty as unrelated.** Every cut this session came
in smaller than its line count implied, and the three that dissolved entirely
did so because a name was in the wrong place rather than because a design was
wrong.

#### And two of the value types close a cycle: the broker guards

Measured 2026-09-02, and found only by scanning `impl` blocks rather than
declarations. The first scan asked what each type's DEFINITION references and
all twelve came back clean; that is not the whole item, because behaviour lives
in `impl` blocks elsewhere in the file.

| type | `impl` blocks | what those bodies reach |
| --- | --- | --- |
| `SchemaPendingGuard` | 2 | `crate::broker`, `tracing` |
| `BrokerPauseGuard` | 2 | `crate::broker`, `tracing` |
| the other nine | 0 or 1 | nothing below rank 0 |

`broker.rs` is assigned to `data-engine`. So these two want to be in `data-core`
- `ChangeStream::pause_broker` and `::engage_schema_pending` RETURN them, and
`backend/sqlite/cdc.rs:785` and `:792` implement those methods, so a vendor
constructs them - while their `new` and `Drop` drive engine state. That is
`data-core -> data-engine` against the `data-engine -> data-core` that already
exists: a Cargo cycle, unbuildable, exactly the one `data-core`'s own `lib.rs`
already refuses for `descriptor.rs`.

**RESOLVED 2026-09-02, and the resolution refutes the framing above.** The
paragraph assumed a genuine contract-vs-behaviour tension, the kind
`LockManager` had. There was none. All FOUR impls of the two methods - PG at
`change_stream_pg.rs:269,276`, SQLite at `backend/sqlite/cdc.rs:785,792` - were
the same two lines:

```rust
fn pause_broker(&self, app_id: &str) -> BrokerPauseGuard {
    BrokerPauseGuard::new(app_id.to_string())
}
```

They ignore `self` and read no backend state. Pausing the broker does not depend
on which database you are talking to, so these were never vendor behaviour -
they were free functions hung on a trait, and the "cycle" was an artifact of
where someone hung them.

So: no cut. The two methods are DELETED from `ChangeStream`, all four impls with
them, plus the two `*_for_tests` forwarders on `SqliteBackend` that existed only
because the guards looked like a backend concern. Both guards moved to
`broker.rs`, the module owning the registries their `Drop` mutates; their impls
now reach only `self::`, measured with comments stripped. `new` became `pub` -
not a widening, since the deleted `pub trait ChangeStream` method reached the
same capability through `BackendHandle::as_change_stream_pg`.

**Read this as the pattern, not the exception.** Three of the five items that
looked like blockers this session dissolved on measurement rather than needing a
design: `SessionMinter` was dead, these two were misfiled, and only `LockManager`
had a real tension. Before designing a cut, check whether every impl of the
method is the same body - a trait method no implementor specialises is not a
contract.

**This is the pattern to expect for the rest of the extraction, and the reason
a declaration-level census is not enough.** Three items so far - `budgets`,
`LockManager`, these two guards - are each a rank-0 name with rank-1 behaviour
welded on. Nineteen of twenty passed a signature scan; two of eleven failed an
`impl` scan. Run both before moving anything, and expect the compiler to find a
third class neither scan sees.

**`auth/` is a leftover copy of work that already moved to the migration
service.** Its own header says the data plane's `SET LOCAL ROLE` batch "comes
from here on every transaction". It does not:

- **Two implementations exist.** The live one is `backend/pg_session_sql.rs:39`
  and `:66` - the string `pg_error.rs:263` asserts against. `auth/bootstrap.rs:140`
  is the other, and every caller of its `set_local_role_sql`,
  `ensure_per_app_role` and `drop_per_app_role` is a plugin-db TEST target
  (`mask_flip.rs`, `native_transaction.rs`, `integration.rs`, `parity/mod.rs`).
  Zero production callers anywhere under `crates/`.
- **Role creation belongs to migrate-server and is already there**:
  `zeroship-migrate-server/src/provisioning.rs:176` and `apply.rs:1389-1400`
  issue the `CREATE ROLE` for both the template and the per-app role, through
  `zeroship_core::database_role::per_app_role_name`.

That is AGENTS.md's standing invariant already satisfied - privileged work in a
service that does not execute creator code - with plugin-db keeping a copy
nothing calls. `auth/util.rs` is separately `#[cfg]`-gated and only its
test-gated SQLite consumers name it.

Deleting it is not free: the test targets above use `ensure_per_app_role` as
FIXTURE SETUP, so they need repointing at the migrate-server path or their own
helper first. Sized, not done.

**The `data-engine` row WAS the weakest line in this table. It is not any more, and the
paragraph that said so is kept below because the reason it stopped being true is the
work itself.**

It used to read: *"Five do not: `crud/`, `transaction/`, `tx_route.rs`, `crud/unmask.rs`
and `tx_scope.rs` hold production functions whose signatures carry `v8::` - `run_op`, the
`dispatch_*` family, `transaction_dispatch` and the promise finalizer chain - inside a
crate whose entire premise is that it does not link V8."*

**Re-measured 2026-09-02 at `b5d37e1dc`, across every module this row assigns to
`data-engine`** (`crud/`, `transaction/`, `exec.rs`, `broker.rs`, `read_set.rs`,
`tx_route.rs`, and `backend/mod.rs` for `BackendHandle`):

| | count |
| --- | --- |
| `v8::` in a signature or a struct field | **0** |
| `v8::` anywhere outside a comment | **4** |

All four are `tx_route.rs:219-222`, inside a `macro_rules! in_scope` that lives under the
file's `#[cfg(test)]` at `:187` - a test harness that spins up an isolate, not a shipped
path. So the row now reads as it always claimed to: these modules move without a V8 cut.

Two further corrections to the old text. `tx_scope.rs` was never in this row - the table
assigns it to `plugin-db (thin)`, so its V8 content was never an obstacle to extracting
`data-engine`. And the instrument it named, `tests/lib/tier_signature_census.sh`, does not
exist in the tree; the measurement above is a direct grep over the assigned modules,
separating signature/field hits from body hits because a `v8::` in a body is equally fatal
to extraction (the crate would still link V8) but is a different amount of work to remove.

Re-derive this before relying on it. It has been wrong once in each direction.

**The error hierarchy is neutral with per-vendor translators** (Spring Data's shape). `DbError`'s 14
variants name a vendor type **zero** times - the type was never the problem, only the translation is
vendor-bound and it sits in the wrong crate.

```
  data-core        DbError                                     no vendor, no runtime
  data-postgres    translate(&compio_postgres::Error) -> DbError
  data-sqlite      translate(&rusqlite::Error)        -> DbError
  plugin-db        to_op_error(DbError) -> OpError              the adapter's own translator
```

The orphan rule constrains nothing here: it binds `impl From<A> for B`, not functions, and every impl
in the crate owns its destination. The real constraint is the dependency floor above.

---

## Execution order

### What has shipped, verified 2026-09-01

| # | move | commit |
| --- | --- | --- |
| - | close the `ENGINE <-> PG` cycle: roled-autocommit funnel into the vendor tier | `f77ead1d8` |
| - | gate decision 5 BEFORE moving anything | `b2263ebc9` |
| 1 | PG error classification out of `error.rs` into `backend/pg_error.rs` | `6f3a482f6` |
| - | extend the gate to `zeroship-schema` - found a second bearer nobody had recorded | `e96849f0a` |
| 2 | PG introspection out of the floor crate into `backend/pg_introspect.rs` | `fffa857b0` |

The gate passes, and its baseline arm confirms every recorded entry still describes a live violation -
so the baseline is not rotting into a rubber stamp.

**The remaining decision-5 surface is entirely inside `zeroship-plugin-db`.** The floor crate is
clean, which was the point of doing it first: a vendor in the floor is inherited by every crate above
it. `./tests/vendor_embedding_gate.sh` prints the current file list; do not copy it here, because the
whole job is to shrink it.

### THE CAVEAT THAT REORDERED EVERYTHING

`git mv`-ing a file that NAMES a vendor into a new crate **does not fail**. Cargo simply wants the
dependency declared; you declare it, and the build goes GREEN having made the violation permanent and
official.

**Structural edges fail loudly as `E0433`. Vendor embedding fails silently.**

So the operator's "move first, let the compiler produce the task list" works for structural edges and
NOT for decision 5. There, the compiler is not the task list - `tests/vendor_embedding_gate.sh` is,
and it runs before each move. That gate immediately showed decision 5's scope was thirteen files, not
the one #97 named, and that two of them were in `zeroship-schema`, the crate the split places
`data-core` ON. Which reordered the queue: **the floor crate came before anything inside plugin-db**,
because a vendor in the floor is inherited by every crate above it.

### Phase 0 - refactors inside today's crate, no new crates

Five refactors, and **every one improves the tree on its own terms**: a query engine that does not
link V8, contract traits that name no vendor, crypto both backends share from below rather than
beside. If the crate split were cancelled tomorrow, Phase 0 would still be worth having. That is the
test a prerequisite should pass, and it is why this ordering is safe to start before the count is
decided.

**0.1 is the headline, not the tail.** Separating 39 dispatch functions from the query engine across
five modules is a visible, reviewable change to how `env.db` is structured, and should be planned as
one. Phase 0 is still worth doing first and is still individually shippable; **it is not small.**

### Phase 0.5 - three audits that must precede ANY crate boundary

- **The `pub(crate)` audit.** Every symbol going `pub(crate) -> pub`, and whether the fence was
  load-bearing. Four are named security controls (`sanitize_app_actor`, `TxRoute::capture`,
  `DbBinding::cold_start`, `context::with_mut`). This repository has already shipped this mistake once
  and written a comment claiming it had not.
- **The dead-code decision. ALL FOUR ARE MEASURED (2026-09-02), and they took THREE different
  answers, which is the finding.** `cross_app_fk.rs` and `crud/mask_backfill.rs` are DELETED.
  `drop_namespace.rs` is REASSIGNED (see the target table). `read_set.rs` is the one genuine
  delete-or-wire call and is stated below. Both
  deletions were confirmed the only way that is not a grep - remove the file and compile - and both
  turned up a stale claim on the way out: `cross_app_fk`'s declaration named an enforcer
  (`zeroship_schema::query`) with zero cross-app references, and three crates described
  `mask_backfill` as holding a `run_mask_backfill` / `run_mask_rewrite` runner that its own header
  said in its first four lines had never existed. Original text follows.

  **`read_set.rs` (709 lines) - "inert on both ends" CONFIRMED, and it is two claims, so each was
  checked separately.** PRODUCER: `record_if_active` has one real production call site
  (`crud/mod.rs:242`), which is why a caller grep reads as live - but it opens `if !is_active()`,
  and `Active::begin` has no caller outside the module, so nothing is ever recorded. CONSUMER:
  `broker::Subscription::set_read_set` has ten call sites, all inside `broker.rs`'s own test module
  (boundary line 1106), so `read_set` is `None` on every real subscription and `accepts()` returns
  `true` unconditionally.

  **The consequence is a delivery-semantics fact, not just dead weight**: every subscriber on
  `(app_id, collection)` receives every change to that collection, including rows its filter
  excludes - the exact coarse-grained behaviour the module says it removes. Wiring is two
  connections (open a capture around the `query()` dispatch; hand `Active::take`'s entries to the
  subscription), not a rewrite. **That changes what subscribers receive, so it is an operator
  decision, and it is the one item in this audit still open.**

  Several modules are self-declared unreachable - `cross_app_fk.rs`,
  `drop_namespace.rs`, `crud/mask_backfill.rs` - plus `read_set.rs`, inert on both ends. **Giving
  dead code a crate is how the existing clusters got there.** Decide delete-or-wire BEFORE assigning.
- **The tier-signature audit.** For each module, does any signature name a crate its assigned tier may
  not depend on? This catches what a module walk and a type walk both miss.

### Phase 1 - needs no new prerequisites

- **Step 0's deletion**, once its prerequisite lands: migrate the live security tests off the dead DDL
  builders onto the engine's renderer.
- **`data-cdc-server`.** ~~Needs neither `data-core` nor `data-postgres`.~~ **THAT PRECONDITION NO
  LONGER HOLDS, and a move listed in "What has shipped" is what broke it.** Re-measured 2026-09-01:
  `replication.rs:36` and `slot_reaper.rs:25` both carry `use crate::backend::pg_error;` - real
  imports, not doc comments. `pg_error` is the module move 1 created, and `git show --stat 6f3a482f6`
  lists both CDC files among its edits, so that move rewired them off `crate::error` and onto the
  vendor tier. The original "zero `crate::backend`" reading was correct when taken and was
  invalidated by work this same document sequences.

  Its cost is still four edits, of which the suppression handshake is the hard one - three brackets
  with an overlap invariant. But it is no longer dependency-free, and the fix is a design question
  rather than a cleanup:

  1. **CDC depends on `data-postgres`.** Honest, since CDC is Postgres-only by nature - but it drags
     `data-core` and `data-query-builder` behind it, and the point of a separate relay service is
     that it does not link the worker's data plane.
  2. **Extract the classifier lower**, somewhere both `data-postgres` and `data-cdc-server` reach.
     This fights the settled error design, which places the vendor translator in `data-postgres`
     precisely because it is vendor-bound.
  3. **CDC carries its own error handling** and shares no classifier.

  **The general lesson outlasts the instance.** A precondition stated in one section was falsified by
  a move executed under another, in the same document, and nothing re-measured it. Treat every "needs
  no prerequisites" claim here as current only up to the last landed move.

### Phase 2 - the crates

`data-query-builder` (rename only), then `data-core`, `data-postgres`, `data-sqlite`, `data-engine`,
with `plugin-db` reduced to the adapter.

---

## Constraints that will be got wrong if not stated

**Do not embed counts that the work itself changes.** Line totals, occurrence tallies, gate arm
numbers, commit gaps and per-module sizes all rot within days, and this document has already been
wrong about every one of them - its figures for the branch gap and for two crates' sizes had all
drifted by the time anyone re-derived them. A stale number is worse than no number, because it reads
as measured. **State the shape and name the command that produces the figure.** The counts that
matter here are outputs of `./tests/vendor_embedding_gate.sh`,
`tests/lib/tier_signature_census.sh` and `git rev-list --count origin/main..HEAD`.

**And a moving count is not automatically progress.** The census says so in its own footer, about a
figure this document used to carry: the 2026-08-31 headline "49 violations" was *right by
cancellation* - an under-reading instrument and a stale `tier()` map erred in opposite directions by
the same amount. Two defects producing one plausible number is the worst case for a written-down
figure, because nothing about it looks wrong. Before reading any movement in that census as a fix,
check that `tier()` still matches the placement table. It currently does: the PG arm names
`pg_error.rs` and `pg_introspect.rs`, so the map was updated with the moves that created them.

**No CI has ever run any of this.** This branch has never been pushed - the standing rule is
commit-only - so it sits hundreds of commits ahead of `origin/main` and `.github/workflows/ci.yml`
describes an intent, not an executed check. **The local commands are the only oracles, and a gate
nobody runs is a census with a stricter name.** Demonstrated twice in one day: the `-p zeroship`
breakage (#93) sat inside `ci.yml` itself, and `sqlite_integration.rs` was RED for days (#124) while
`ci.yml` invoked it correctly - because the two commands a person reaches for locally both exclude
that target. Re-derive the gap with `git rev-list --count origin/main..HEAD` if it matters.

*Do not cite a `gh` 404 as evidence the repository is absent: the `gh` account differs from the
remote's owner, and GitHub returns 404 for private repositories the caller cannot see. The commit gap
is the evidence; the 404 is not.*

**The census's `tier()` map is a copy of the assignment table**, so re-drawing any boundary invalidates
every verdict it prints. Change both in the same commit or it reports on a shape nobody proposed. It
is a census, not a gate - deliberately not named `*_gate.sh` so `tests/gate_arm_census.sh` does not
adopt it - and it becomes a gate the moment the first crate boundary exists.

**The census is currently blind to every `backend/*` internal edge**, by two independent mechanisms
that **mask each other**: fixing either alone changes nothing, measured by a one-variable control
whose diff was empty (#128). No post-split green from that census can be believed until both land.

**Cite headings, never line numbers, for anything inside this file.** Internal `:NNN` self-citations
rot in a document being edited - a correction written to stop a contradiction going unnoticed once
rotted into pointing at a blank line. `tests/doc_citation_gate.sh` checks citations into the tree and
cannot check a document's references to itself.

**Four instances of built-tested-unreferenced code sit in one dependency closure:** the DDL builders,
the index builders, the differ plus live introspection, and `data-plan` itself. Every one has passing
tests, which is exactly why none looked dead. When this shape appears again, the question is not "do
the tests pass" but **"who calls this in production"**. Grep answers spelling, the compiler answers
"is this named", and neither answers "does production reach this" - only walking outward from a real
entry point does.

**Keep one inventory.** Two of this document's defects were not reasoning errors at all: it held one
module inventory in two places and edited them independently, so a correction applied to one copy left
the other stale - twice, for two modules, in consecutive rounds. The placement table is the inventory.

---

## Open

- **Putting `BackendHandle` in ENGINE makes a use case depend on both adapters, and the governing
  rule forbids exactly that.** Verified 2026-09-01: `backend/mod.rs:1494-1502` is
  `Postgres(Rc<PostgresBackend>)` plus its Sqlite sibling, so the enum names both concrete vendor
  types. Whichever crate owns it must declare `data-postgres` and `data-sqlite`. There is no cycle -
  both vendors sit on `data-core`, so the graph is a diamond - but the ring diagram at the top of this
  document puts adapters OUTSIDE use cases, and this points a use case outward at two of them.

  This document asserted `data-engine -> data-core` in the target block while separately deciding
  `BackendHandle` goes to ENGINE. Both statements were edited independently and cannot both be true;
  the target block has been corrected rather than quietly reconciled.

  Three resolutions, none free:

  1. **Own it above ENGINE** - the adapter or a small selection crate. Rejected once already, on
     measurement: eight ENGINE files consume `BackendHandle`, so this mints many new upward edges.
  2. **Accept that `data-engine` is a composition layer** that may name vendors, and redraw the ring
     so it sits above them. Coherent - what it names is a closed sum of backends, its own dispatch
     mechanism - but it weakens the governing rule to a guideline, which deserves to be a decision
     rather than a side effect of a placement.
  3. **Make the engine generic over the backend.** This dodges every measured objection to `dyn`:
     generics monomorphise, keep associated types, and add no per-call allocation. The cost is
     different and real - `<B: Backend>` is viral across `crud/`, `transaction/` and `exec.rs`, and a
     closed two-member set monomorphises the whole engine twice.

  Option 2 is the working assumption because the enum is a defended choice, but the ring diagram is
  this document's foundation and should not be amended as a consequence of a file placement.

- **`context.rs`.** It carries Postgres in its **fields** - `TxConnection::Postgres(OwnedPooledClient)`
  and `pool: Option<Rc<Pool>>` - and field position is invisible to the module walk, the type walk AND
  the signature census. The thin-adapter decision sends it down; earlier placement tables sent it to
  `data-engine`. It is the per-isolate connection cache, and where it lands is a real decision this
  document has twice recorded as already made (#100).
- **The crate count.** Six is the working answer; the `data-engine` boundary drew disagreement across
  rounds.
- **`zeroship-migrate-server`** is a service HOST, not engine. Left in the `migrate-*` family; a
  `-service` suffix is arguable.
- **Feature-gating the SQLite backend** - a decision, not a discovery. It removes the whole SQLite
  backend from the production worker and hardens a guard the worker already implements at runtime.
- **`AGENTS.md`'s `zeroship-schema` entry** needs correcting with this work, saying "present but
  uncalled" rather than deleting the clauses, so the next reader does not re-add them. Its contents
  clauses are true as descriptions of what the file HOLDS; the "reused by the migration engine" clause
  is false.

**Settled, previously open:** the worker stops decoding WAL and its role becomes `NOREPLICATION`
(FULL, 2026-08-31) - every "moves to the relay" row is unconditional. The `plugin-*` family does not
break, because `plugin-db` keeps its name.
