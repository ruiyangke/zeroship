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
time, for everyone, forever. No lint, no reviewer, no census run. This repository already uses the
technique: `serialize_derive_is_structurally_impossible` makes a capability unreachable by giving a
crate an empty manifest rather than by forbidding its use.

**But `E0433` fences the SPELLING, not the TYPE.** It stops a crate *naming* a path. It says nothing
about a crate *holding a value* of that type, and one is handed across today through a public field:

- `crates/zeroship-schema/src/error.rs:31` declares `pub source: compio_postgres::Error`.
- `crates/zeroship-plugin-db/src/error.rs:942` is `impl From<SchemaError> for DbError`, and its body
  passes that live value into a Postgres classifier.
- **The token `compio_postgres` does not appear on the line.** A `data-core` whose manifest omits the
  driver compiles it, because inference never needs the path.

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
                                load-bearing. (today's zeroship-data-plan, renamed)
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
| `diff.rs` introspection | **gone** - moved to `backend/pg_introspect.rs` |
| `diff.rs` TYPES - `MaskKind`, `Classification`, `MaskMeta`, `EncryptionMeta` | **live** - named by the masking policy AND both vendors |
| `mask_codec.rs`, `ident.rs`, `descriptors.rs`, `error.rs` | live |

`zeroship-migrate-core` does **not** depend on `zeroship-schema` - checked in its manifest, not
inferred. Its same-named `build_*` references are its own `schema/query.rs`.

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

**And the typed IR that would deliver portability is not wired at all:** `grep -c zeroship_data_plan`
across `crates/zeroship-plugin-db/src/` returns **0**. `data-core -> data-query-builder` is not an
edge that exists and is not one relocation away; it appears only after the executor is retyped.

---

## Where every module lands

| destination | modules |
| --- | --- |
| `plugin-db` (thin) | `v8_classes/`, `v8_bridge.rs`, `lib.rs` (the `DbPlugin` part), `tx_scope.rs` |
| `data-engine` | `crud/`, `transaction/`, `exec.rs`, `broker.rs`, `read_set.rs`, `tx_route.rs`, `drop_namespace.rs`, `cross_app_fk.rs`, `BackendHandle` |
| `data-core` | `error.rs` (less `to_op_error`), `descriptor.rs`, the six vendor-free capability traits |
| `data-encryption` (if split) | `encryption/`, plus `EncryptedColumn` |
| `data-postgres` | `backend/postgres.rs`, `pg_error.rs`, `pg_introspect.rs`, `PgSqlExecutor` |
| `data-sqlite` | `backend/sqlite/` |
| `data-cdc-server` | `wal_consumer.rs`, `replication.rs`, `slot_reaper.rs` |

**The `data-engine` row is the weakest line in this table.** It reads as whole modules moving intact.
Five do not: `crud/`, `transaction/`, `tx_route.rs`, `crud/unmask.rs` and `tx_scope.rs` hold
**production functions whose signatures carry `v8::`** - `run_op`, the `dispatch_*` family,
`transaction_dispatch` and the promise finalizer chain - inside a crate whose entire premise is that
it does not link V8. `tx_scope.rs` is not a split at all: all of its production functions are V8
context-map manipulation, so it moves to the adapter whole. The others are dispatch stacked on engine
and must be cut along that line first.

`tests/lib/tier_signature_census.sh` is the instrument that enumerates them; read the current count
from a run rather than from this document.

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
- **The dead-code decision.** Several modules are self-declared unreachable - `cross_app_fk.rs`,
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
