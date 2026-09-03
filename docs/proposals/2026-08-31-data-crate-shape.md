# The data-plane crate shape

**Status.** PARTIAL. Four of the six target crates exist and are load-bearing:
`crates/zeroship-data-query-builder`, `crates/zeroship-data-core`,
`crates/zeroship-data-postgres`, `crates/zeroship-data-sqlite`. Both vendor cuts are done -
`crates/zeroship-plugin-db/src/backend/mod.rs` is now a re-export ladder plus the `Backend`
conformance marker and `backend/cancel.rs`. `zeroship-data-engine` and `zeroship-data-cdc-server` do
not exist; their modules still sit in `crates/zeroship-plugin-db`.

---

## What it is

### The rule that decides everything

The goal is a maintainable system under clean architecture. The crate split is the consequence, not
the objective.

> **Source dependencies point INWARD.** Domain types know nothing of use cases; use cases know
> nothing of adapters; adapters know nothing of each other. Frameworks and drivers - V8,
> `compio-postgres`, `rusqlite` - are the outermost ring, and nothing inner may name them.

```
    frameworks & drivers   V8 . compio-postgres . rusqlite
    adapters               plugin-db (Rust<->V8)  .  data-postgres . data-sqlite
    use cases              crud pipeline . transaction reducer . masking policy
    domain                 DbError . TypedCell/TypedRows . ChangeEvent/ChangeOp . descriptor
```

This is the test for anything this document does not cover: *does this make a dependency point
inward?* If not, it is not the fix. It is also why the answer is a refactor rather than a file move.

### The target

```
libs/compio-postgres            driver, unchanged

zeroship-data-query-builder     the typed query grammar (DbPlan IR). ZERO dependencies, LEAF,
                                and that stays load-bearing.
zeroship-data-core              the contract every backend implements, PLUS the backend-neutral
                                layer both share: encryption, MaskKind, TypedCell/TypedRows.
zeroship-data-postgres          impl of the core contract.   -> data-core, compio-postgres
zeroship-data-sqlite            impl of the core contract.   -> data-core, rusqlite
zeroship-data-cdc-server        service tier: WAL stream, slot authority, reaper. Peer of
                                zeroship-migrate-server.
                                -> compio-postgres, zeroship-core. NOT data-core, NOT data-postgres.
zeroship-data-engine            the data plane's logic: crud pipeline, transactions, exec, broker.
                                -> data-core, AND -> data-postgres + data-sqlite, because it owns
                                BackendHandle.
zeroship-plugin-db              THIN. The worker/runtime plugin ADAPTER ONLY. -> data-engine.
                                NOT -> data-cdc-server; the worker must not link the relay.

zeroship-core::change_event     the cross-process event type, beside usage_event and
                                replication_names.

zeroship-migrate-*              the engine, dialect-complete, untouched
zeroship-migrate-server         the migration service host
```

Six, because the modules are six things. `crud/` + `transaction/` + `exec.rs` is the single largest
block in the crate, and it is pipeline and reducer - not a contract, not a driver, not the grammar,
not the relay. Putting it in `data-core` makes the core the big crate again with drivers attached,
which is what the split exists to undo.

### What exists today

| crate | holds |
| --- | --- |
| `zeroship-data-query-builder` | `plan.rs`, `predicate.rs`, `projection.rs`, `search.rs`, `write.rs`, `ident.rs`, `render/{mod,postgres}.rs`. Manifest declares no dependencies at all. |
| `zeroship-data-core` | `error.rs` (`DbError`), `binding.rs` (`DbBinding`), `budgets.rs`, `capability.rs`, `storage.rs` (the dispatch traits), `encryption/`, `lock_policy.rs`, `schema_cache.rs` |
| `zeroship-data-postgres` | `postgres.rs`, `pg_error.rs`, `pg_introspect.rs`, `pg_session_sql.rs`, `pg_row_json.rs`, `pg_autocommit.rs`, `lock_guard.rs` |
| `zeroship-data-sqlite` | the whole SQLite backend: `session.rs`, `cdc.rs`, `change_sink.rs`, `dialect.rs`, `lock.rs`, `reservation.rs`, `row_json.rs`, `spatial.rs`, `vector.rs`, `mask_policy_store.rs`, `error.rs` |

`zeroship-plugin-db` is still one crate holding three future ones: the thin adapter, the engine and
the CDC service.

### Where every module lands

| destination | modules |
| --- | --- |
| `plugin-db` (thin) | `v8_classes/`, `v8_bridge.rs`, `lib.rs` (the `DbPlugin` part), `tx_scope.rs`, `op_error.rs`, `service.rs`, `context.rs` |
| `data-engine` | `crud/`, `transaction/`, `exec.rs`, `broker.rs`, `read_set.rs`, `tx_route.rs`, `tx_lanes.rs`, `backend_handle.rs`, `descriptor.rs`, `metrics.rs`, `system_shape_charter.rs`, `backend_selection.rs`, `cdc_lifecycle.rs` |
| `data-cdc-server` | `wal_consumer.rs`, `replication.rs`, `slot_reaper.rs`, `change_stream_pg.rs` |
| migrate-server (teardown coordinator) | `drop_namespace.rs` |
| DELETE | `auth/` |
| already moved | `encryption/`, the dispatch traits, `budgets`, `lock_policy` -> `data-core`; `backend/postgres.rs` + `pg_*` -> `data-postgres`; `backend/sqlite/` -> `data-sqlite` |

`test_support/` is test-only and follows whatever it supports.

`backend/mod.rs` splits into nothing new: what is left of it is the adapter's re-export ladder and
`Backend`, which is `plugin-db`'s own `pub(crate)` conformance marker and is pinned there by the
orphan rule. `backend/cancel.rs` holds `TxCanceller`, a two-arm enum over both vendors, so it travels
with the engine.

### `plugin-db` is a very thin layer joining Rust to V8

It keeps its name and loses almost everything else: the V8 marshalling boundary and the `env.db` op
surface. That is `v8_classes/`, `v8_bridge.rs`, the `DbPlugin` part of `lib.rs`, `tx_scope.rs`,
`op_error.rs` and `service.rs`.

Three consequences that are not optional:

1. **The protocol inversion.** The V8-signature dispatch functions belong in the thin layer - they
   *are* the boundary. Their `async move` bodies are not; those bodies are query pipeline. The engine
   returns data, and the adapter lowers it. This has shipped: no module assigned to `data-engine`
   names a `v8::` type in a signature or a struct field, and the only `v8::` occurrences left in that
   set are inside `tx_route.rs`'s `#[cfg(test)]` isolate harness.
2. **Row decoding does not live in `v8_bridge.rs`.** A thin layer joining Rust to V8 cannot link a
   database driver. Row-to-JSON lowering is `pg_row_json.rs` and `row_json.rs`, in their vendor
   crates.
3. **`context.rs` is a composition root, not an engine module.** After the ownership split it holds
   the adapter's half - `db_url`, `backend`, `backend_generation`, `backend_init_in_progress`,
   `resource_key`, `backend_selection` - and no longer names `compio_postgres::Pool`. The lanes are
   out in `tx_lanes.rs`; the schema cache is in `data-core`.

### `zeroship-schema` is DELETED, not moved

`crates/zeroship-schema` is a 17k-line leaf that every data crate still declares and no migrate crate
declares at all. Its five modules each have a live twin, and the twins are already split the way this
document wants:

| `zeroship-schema` module | live twin | status |
| --- | --- | --- |
| `descriptors.rs` | `crates/zeroship-migrate-backend/src/descriptors.rs` | line-for-line identical code |
| `error.rs` | `crates/zeroship-migrate-backend/src/schema_error.rs` | line-for-line identical code |
| `mask_codec.rs` | `crates/zeroship-migrate-backend/src/mask_codec.rs` | twin is the superset |
| `diff.rs` | `crates/zeroship-migrate-core/src/schema/diff.rs` | twin is the live differ |
| `query.rs` | `crates/zeroship-migrate-core/src/schema/query.rs` | twin kept the DDL half and deleted the DML half; the schema copy kept both |

`crates/zeroship-migrate-core/src/schema/mod.rs` re-exports the three leaf modules from
`zeroship-migrate-backend`. That is the shape to copy, not reinvent.

What is live in `zeroship-schema` from the data plane's side:

- **`query.rs` DML builders: live.** `build_write_target_probe` and
  `build_conflict_probe_with_dialect` in `crud/write_pipeline.rs`; `build_vector_search` and
  `build_spatial_near` in `zeroship-data-postgres`. `data-query-builder` is their replacement.
- **`query.rs` DDL builders: zero production callers.** The sentinel WRITE half is dead here and live
  there - `build_mask_sentinel_comments` / `build_encryption_sentinel_comments` ship from
  `crates/zeroship-migrate-postgres/src/schema.rs`, while `zeroship-schema`'s own copies are reached
  only from a DDL emitter nothing calls.
- **`diff.rs` `compute_diff`: test-only.** The live twin is `zeroship-migrate-core`'s.
- **`diff.rs` types: seven live, seven not.** Live: `MaskKind`, `LiveSchema`, `MaskMeta`,
  `ColumnInfo`, `EncryptionMeta`, `ForeignKeyInfo`, `IndexInfo`. Test- or prose-only: `compute_diff`,
  `ChangeKind`, `ChangeClass`, `DiffOp`, `classify_add_column`, `Classification`, `WrappedType`.
  `LiveSchema` in particular is `SqliteBackend`'s `SchemaIntrospect` associated type.
- **`mask_codec.rs`, `ident.rs`, `descriptors.rs`, `error.rs`: live.**

The crate is dialect-PARAMETERISED (`query.rs` owns `pub enum SqlDialect { Postgres, Sqlite }`), so it
is not a vendor and does not fold into `data-postgres`.

### The error hierarchy is neutral with per-vendor translators

```
  data-core        DbError                                     no vendor, no runtime
  data-postgres    translate(&compio_postgres::Error) -> DbError
  data-sqlite      translate(&rusqlite::Error)        -> DbError
  plugin-db        ToOpError: DbError -> OpError                the adapter's own translator
```

`DbError`'s variants name a vendor type zero times. The orphan rule constrains nothing here: it binds
`impl From<A> for B`, not functions, and every impl owns its destination.

### The decisions

**1. One query builder.** Wire the typed IR into the data plane and DELETE the string builder it was
written to replace. Not "leave both and revisit". The IR is built and tested; it is a
`[dev-dependencies]` entry of `zeroship-plugin-db` and `grep -rn data_query_builder
crates/zeroship-plugin-db/src/` returns zero. No shipped binary links it.

**2. CDC gets its own crate AND its own service** - a process that does not execute creator code.

**3. Keep `zeroship-migrate-mysql`.** The in-sourced engine stays dialect-complete so it does not
diverge from upstream, accepting that zeroship targets only PostgreSQL and SQLite. Keeping the CODE
and paying the BUILD are separable: `zeroship-migrate` declares no `[features]`, so the MySQL dialect
compiles for every dependant while nothing selects it.

**4. NO SQL AND NO DIALECT KNOWLEDGE IN THE ENGINE.** Adding a database must require a new crate, a
new variant in the three dispatch enums (`BackendHandle`, `TxConnection`, `TxCanceller`), and nothing
else: no statement text, no `match` on dialect to choose SQL, no vendor error handling. If supporting
a new vendor means writing a statement or reading a SQLSTATE inside `data-engine`, decision 1 has not
landed. (The wording is not yet ratified - see Open 4.)

The remaining surface is three logical operations, each written twice because each carries its own
dialect, all in `crates/zeroship-plugin-db/src/backend_handle.rs`: read an encrypted raw column, read
a plaintext raw column, append an unmask audit row. Everything else has gone: `crud/unmask.rs` holds
no SQL text, `transaction/mod.rs`'s `BEGIN ISOLATION LEVEL` is gone, and the `DROP SCHEMA` in
`drop_namespace.rs` and the whole of `crud/mask_drift.rs` are `#[cfg]`-gated out of every shipped
binary.

**5. THE CORE AND EVERY OTHER NON-VENDOR CRATE NEVER EMBED A VENDOR DIRECTLY.** Not "should avoid" -
never. Enforced by `tests/vendor_embedding_gate.sh` (source) and `tests/data_crate_closure_gate.sh`
(dependency closure). The remaining decision-5 surface is six files, all inside `zeroship-plugin-db`:
`auth/bootstrap.rs`, `backend/cancel.rs`, `exec.rs`, `lib.rs`, `service.rs`, `tx_lanes.rs`. Run the
gate for the current list; the whole job is to shrink it.

### The dispatch surface

Eight capability traits live in `crates/zeroship-data-core/src/storage.rs`: `SqlExecutor`,
`LockManager`, `SchemaIntrospect`, `DialectBuilder`, `ChangeStream`, `VectorIndex`, `SpatialIndex`,
`Backup`. The shared value types are in `capability.rs`: `LockScope`, `ScalarRead`, `UnmaskAuditRow`,
`SnapshotHandle`, `SnapshotOpts`, `PitrTarget`, `BusyPolicy`, `SNAPSHOT_RESTORE_LOCK_TAG`.

**The enum stays; `dyn` is refused for measured reasons.** These are `async fn`-in-trait, so object
safety needs `Box<dyn Future>` per call - a per-CRUD-op allocation on the hottest path in the data
plane. The associated types cannot be erased without losing the concrete client that
`LockManager::acquire_advisory_lock` takes by `&Self::Client`. The backend set is closed, and an enum
is the canonical shape for a closed sum. Monomorphised dispatch is preserved, not replaced.

`BackendHandle` (`crates/zeroship-plugin-db/src/backend_handle.rs`) names both vendors in its
definition, so it belongs to the tier ABOVE both, which is `data-engine`. Neither vendor crate names
it - so the graph is a diamond, not a cycle.

### How a move is verified

Three build commands, not one. A `#[cfg]` attribute CHANGES MEANING when the item it guards crosses a
crate boundary, and `cargo check -p <crate> --features test-helpers --all-targets` has reported zero
errors while `cargo check -p zeroship-worker -p zeroship-cli` failed with four unresolved imports:

1. the crate alone, `--features test-helpers --all-targets`
2. the crate alone, WITHOUT them
3. at least one dependent

Plus the gates, before each move rather than after: `tests/vendor_embedding_gate.sh`,
`tests/data_crate_closure_gate.sh`, `tests/lib/tier_direction_census.sh`,
`tests/lib/tier_signature_census.sh`.

---

## Why it is this way

**Vendor neutrality is enforced by the MANIFEST, not by discipline.** You cannot name
`compio_postgres::Error` in a crate that does not depend on it - that is `E0433`, at compile time,
for everyone, forever. No lint, no reviewer, no census run. The technique is already load-bearing in
this tree: `serialize_derive_is_structurally_impossible`
(`crates/zeroship-data-query-builder/tests/no_sql_text_escape_hatch.rs`) reads the crate's own
manifest and asserts the declared dependency set is EMPTY, which is what makes "`DbPlan` must not
derive `Serialize`" a structural fact rather than a review item.

**But `E0433` fences the SPELLING, not the TYPE.** It stops a crate naming a path. It says nothing
about a crate HOLDING a value of that type through a public field, where inference never needs the
path and no manifest fence can see it. `tests/data_crate_closure_gate.sh` exists for the other half:
a crate can acquire a driver through a dependency without one line of its own source changing.
Neither gate substitutes for the other, and neither substitutes for the two censuses, which read
source direction rather than linkage.

**Privileged work does not live in a crate the worker links.** AGENTS.md's standing invariant:
privileged operations belong to a separate service that does not execute creator code. That is why
`drop_namespace.rs` is re-homed behind `zeroship-migrate-server` rather than assigned to
`data-engine` (its own entry point requires the caller "must not be the worker login"), and why
`auth/` is deleted rather than moved - role creation already ships from
`crates/zeroship-migrate-server/src/provisioning.rs` and `apply.rs`, and the live `SET LOCAL ROLE`
batch is `crates/zeroship-data-postgres/src/pg_session_sql.rs`, not `auth/bootstrap.rs`.

**The relay must not be linked by the worker.** `zeroship-plugin-db` may not depend on
`zeroship-data-cdc-server`. Extracting the CDC modules is necessary but not sufficient for that:
`crates/zeroship-worker/src/slot_reaper.rs` imports `zeroship_plugin_db::slot_reaper` directly, so the
worker keeps its REPLICATION privilege until that call site is re-pointed too.

**Dead code does not get a crate.** Giving unreferenced modules a home is how the current clusters
formed. Decide delete-or-wire BEFORE assigning. Applied so far: `cross_app_fk.rs` and
`crud/mask_backfill.rs` deleted, `drop_namespace.rs` reassigned, `read_set.rs` wired.

**A contract's SHAPE must not vary by feature.** `zeroship-data-core` gates trait members
(`SqlExecutor::pool_exec_ddl`, `DialectBuilder::sql_dialect`) and whole traits (`Backup`,
`SchemaIntrospect`) on its own `test-helpers`. Feature forwarding is one-directional, so any crate
that enables core's feature while depending on exactly one vendor gets a trait requiring a method and
an impl compiled out. That is an observed `E0046`, not a hypothesis. The current graph avoids it by
coincidence.

**Do not embed counts that the work itself changes.** Line totals, occurrence tallies, gate arm
numbers and per-module sizes rot within days, and this document has been wrong about every one of
them. State the shape and name the command that produces the figure.

**No CI has ever run any of this.** This branch has never been pushed, so `.github/workflows/ci.yml`
describes an intent, not an executed check. The local commands are the only oracles, and a gate
nobody runs is a census with a stricter name.

---

## Open

1. **The last two ENGINE -> ADAPTER edges.** `crud/mod.rs` and `exec.rs` both reach
   `crate::context`, which is the cycle blocking `data-engine`'s extraction
   (`tests/lib/tier_direction_census.sh` currently reports ADAPTER <-> ENGINE at 15 edges one way, 2
   the other). Both fixes are known: carry the backend as a value (`route.backend()` where the call
   routes SQL, `&BackendHandle` where it does not, `&KeyStore` where it needs a key), and have the
   adapter stamp `SqlDialect` into `plan_*` synchronously because `plan_delete_one` is a sync
   `pub(crate) fn` needing the dialect in the V8 prelude before any bind exists. BUILDABLE, 6-10
   hours for the two edges plus the extraction they unblock.

2. **The CDC crate's dependency on `pg_error`.** `replication.rs` and `slot_reaper.rs` both
   `use crate::backend::pg_error`, and `change_stream_pg.rs` names `backend::postgres`, so
   `data-cdc-server` is not dependency-free. Three answers, none free: (a) CDC depends on
   `data-postgres`, honest since CDC is Postgres-only, but it drags `data-core` behind it and the
   point of a separate relay is that it does not link the worker's data plane; (b) extract the
   classifier lower, which fights the settled error design that places the vendor translator in
   `data-postgres` precisely because it is vendor-bound; (c) CDC carries its own error handling and
   shares no classifier. NEEDS-DECISION.

3. **The CDC modules' upward edges to `broker`.** `cdc_lifecycle.rs` and `wal_consumer.rs` reach
   `crate::broker`, and `cdc_lifecycle.rs` also reaches `crate::context::with`. The
   suppression handshake is the hard one - three brackets with an overlap invariant. BUILDABLE once
   Open 2 is answered, 4-8 hours.

4. **Decision 4's wording.** As written ("adding a database must require ZERO changes to
   `data-engine`") it is unsatisfiable under the enum dispatch the same document mandates: a closed
   sum necessarily gains an arm per backend. The body above states the intended property; ratify it
   or state a different one. NEEDS-DECISION.

5. **A SQLite lowering for the query builder.** `render/postgres.rs` is the only lowering, and its
   own header refuses to add a second in passing because that means deciding whether `ILIKE` is
   emulated by `LIKE ... COLLATE NOCASE` - a divergence for
   `docs/reference/sqlite-divergences.md`. The three unmask operations need no such node: they are
   single-column `SELECT ... WHERE id = ?` plus a fixed nine-column INSERT, and the only real dialect
   differences are the placeholder and identifier quoting. Preferred option is a minimal
   `render/sqlite.rs` serving only that node set and REFUSING everything else via `RenderError`,
   which the builder's own decision 2 sanctions. BUILDABLE, 4-8 hours.

6. **`zeroship-schema`'s deletion cost.** "No production caller" is not "safe to delete": the DDL
   builders are pinned by live security tests in `crates/zeroship-plugin-db/tests/integration.rs` and
   `sqlite_integration.rs`. The cost is deletion PLUS migrating those onto the migration engine's
   renderer. There is no ledger counting what is unported, so the condition cannot be measured
   today - build one, or state the exit differently. BUILDABLE, 8-16 hours.

7. **`auth/`'s deletion cost.** The plugin-db test targets (`mask_flip.rs`, `native_transaction.rs`,
   `integration.rs`, `parity/`) use `ensure_per_app_role` as FIXTURE SETUP, so they need repointing at
   `zeroship-migrate-server`'s path first. BUILDABLE, 3-5 hours.

8. **`service.rs`'s operator pool.** `operator_pool() -> Result<Rc<Pool>, DbError>` is the one
   remaining signature-position vendor violation. It is the OPERATOR pool - provisioning and
   lifecycle connections - with its own lifetime, sizing and callers, so whether it follows the app
   pool down into `data-postgres` needs its own decision rather than a copy of that one. It does not
   appear in any tier cycle. NEEDS-DECISION.

9. **Feature-gated trait members in `data-core`.** Remove the gates from the traits and trait members,
   leaving the feature to gate helpers and fixtures only. The cost is `Backup` and `SchemaIntrospect`
   present unconditionally in a release build - code size, no behaviour. NEEDS-DECISION on paying it.

10. **`crud/mask_drift.rs`.** Fully built, test-gated, zero production callers, and the largest block
    of hand-written SQL left in the engine tier. Delete or wire. NEEDS-DECISION.

11. **`zeroship-migrate-server`'s name.** It is a service HOST, not engine. Left in the `migrate-*`
    family; a `-service` suffix is arguable. NEEDS-DECISION.

12. **Feature-gating the SQLite backend** out of the production worker. A decision, not a discovery:
    it hardens a guard the worker already implements at runtime. NEEDS-DECISION.

13. **`AGENTS.md`'s `zeroship-schema` entry.** Its "reused by the migration engine (write/diff)"
    clause is false - no `zeroship-migrate-*` manifest declares the crate. Correct it to "present but
    uncalled by the engine" rather than deleting the clause, so the next reader does not re-add it.
    BUILDABLE, under an hour.

---

## History

Deliberation lives in this file's git history and in the task ledger (#91 through #166), which
records each finding, its measurement and its verdict. Six review rounds of superseded designs were
removed on 2026-09-01; the correction archaeology that followed was removed on 2026-09-03. What
follows are the mistakes that would otherwise be remade.

- **Do not `git mv` a vendor-naming file into a new crate and declare the dependency.** It does not
  fail - Cargo simply wants the dependency, you declare it, and the build goes GREEN having made the
  violation permanent and official. Structural edges fail loudly as `E0433`; vendor embedding fails
  silently. "Move first, let the compiler produce the task list" works for the former and not the
  latter. Run `tests/vendor_embedding_gate.sh` BEFORE each move.

- **Do not verify a move with `cargo check -p <crate>` alone.** It is blind to test cfg and to
  dependents. A `pub use` carrying `#[cfg(feature = "test-helpers")]` had to be split into a gated and
  an ungated arm after the adapter's own check reported zero errors and two dependents failed to
  build. Use the three commands under "How a move is verified".

- **Do not fold `zeroship-schema` into `zeroship-data-query-builder`.** Moving a string builder into
  the crate written to obsolete it is the "two intermediate versions" the pre-launch stance forbids.

- **Do not restore `ChangeStream::pause_broker` / `engage_schema_pending`.** All four impls were the
  same two self-less lines - free functions wearing a trait method's clothes - and the SQLite arm
  records a behaviour reason, not a preference: it needs the startup-only suppression guard, whose
  `Drop` merely re-enables delivery, while the general pause guard emits a Resync on `Drop` and would
  add a synthetic first message to every SQLite subscription. The guards themselves live in
  `crates/zeroship-plugin-db/src/broker.rs`; deleting `broker::engage_schema_pending` /
  `disengage_schema_pending` breaks them, because those are their implementation.

- **Do not put an executor call in a `data-core` trait's default body.** `LockManager` carried a
  five-attempt retry schedule calling `compio::time::sleep`, which would have put the async executor
  into a crate whose description says it names no runtime.
  `tests/data_crate_closure_gate.sh` would NOT have caught it: it pins `compio-postgres`, `rusqlite`,
  `zeroship-runtime` and `v8`, and `compio` is none of those. The cut shipped as
  `lock_policy::BoundedLockAcquire`, blanket-implemented over the contract.

- **Do not census by declaration alone.** Three items so far - `budgets`, `LockManager`, the two
  broker guards - were rank-0 names with rank-1 behaviour welded on, and behaviour lives in `impl`
  blocks a signature scan never opens. Nineteen of twenty items passed a signature scan; two of
  eleven failed an `impl` scan. Run both, and expect the compiler to find a third class neither sees.

- **Do not sweep for bare identifiers.** `new`, `id`, `kind`, `open`, `lock`, `error`, `session`,
  `vector` and `spatial` are all declared `pub(crate)` somewhere in a vendor tier and appear in most
  files in the crate; the first run "found" cross-boundary uses for nine of them, every one spelling
  rather than reachability. Match qualified paths and `use` lines. A qualified-path sweep is in turn
  blind to `use super::`, which is how `PgSqlExecutor` and `PgLockManager` were missed. Three passes
  found three different gaps; prefer `cargo check` against a real crate boundary.

- **Do not read a falling census number as progress.** The 2026-08-31 headline of "49 violations" was
  right by CANCELLATION - an under-reading instrument and a stale `tier()` map erring in opposite
  directions by the same amount. Two defects producing one plausible number is the worst case for a
  written-down figure. Check that `tier()` still matches the placement table before reading movement
  as a fix; the map is a copy of that table, so re-drawing a boundary invalidates every verdict it
  prints. Change both in the same commit.

- **When extracting `data-engine`, do not force a backend onto every call site.**
  `crud/mask_policy.rs`'s `ensure_backend` call is an INITIALISER that warms a cold isolate at boot,
  and its dispatcher captures no route: drop its `init_pool_async` arm and `installSchema`'s
  `setMaskPolicy` returns `not_configured` on every cold isolate, which is every boot on the SQLite
  dev tier. And five lib tests drive `emit_for_rows` with NO backend deliberately, exercising the
  Postgres/WAL branch; supplying one would put them on the SQLite early-return path and void every
  assertion silently.

- **Do not cite a `gh` 404 as evidence the repository is absent.** The `gh` account differs from the
  remote's owner and GitHub returns 404 for private repositories the caller cannot see. The commit
  gap is the evidence.

- **Cite headings, never line numbers, for anything inside this file.** Internal `:NNN`
  self-citations rot in a document being edited. `tests/doc_citation_gate.sh` checks citations into
  the tree and cannot check a document's references to itself.

- **Keep one inventory.** Two of this document's defects were not reasoning errors: it held one module
  inventory in two places and edited them independently, so a correction applied to one copy left the
  other stale. The placement table is the inventory.

- **Built-tested-unreferenced is the recurring shape here**, and it has appeared four times in one
  dependency closure: the DDL builders, the index builders, the differ plus live introspection, and
  the DbPlan IR. Every one has passing tests, which is exactly why none looked dead. The question is
  never "do the tests pass" but "who calls this in production" - and only walking outward from a real
  entry point answers it.
