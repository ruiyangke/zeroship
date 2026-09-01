# The final shape of the data crates

Status: **planned, not started.** Every number here was measured on 2026-08-31 at `1bf9fdce4`.
Line counts are `find src -name '*.rs' -exec cat {} + | wc -l`.

## What was decided

Three choices, taken by the operator on 2026-08-31:

1. **Finish `data-plan`.** One query builder. Wire the typed IR into the data plane and delete the
   string builder it was written to replace. Not "leave both and revisit".
2. **CDC gets its own crate AND its own service** - a process that does not execute creator code.
3. **Keep `zeroship-migrate-mysql`.** The in-sourced engine stays dialect-complete so it does not
   diverge from the upstream `zero-migrate` project, accepting that zeroship targets only PostgreSQL
   (production) and SQLite (dev). This is a stated trade, not drift.

   **The trade is cheaper than it looks, because keeping the CODE and paying the BUILD are separable
   - measured 2026-08-31.** `zeroship-migrate/Cargo.toml` declares no `[features]` section and no
   optional dependencies, so all three dialects compile unconditionally and 20,018 lines of MySQL are
   built by every dependant. Nothing in zeroship selects that dialect: the addon's only two callers
   hardcode `dialect: "postgres"` (`sdks/vite-plugin/src/gen-types/index.ts:271`, `:326`).

   *This sentence used to continue "and every `mysql` occurrence in `crates/zeroship-migrate-node/src/`
   is a doc comment, never a dispatch." That is FALSE, and the correction below has said so for
   several revisions without anyone reconciling the two.* `bridge.rs` carries FIVE live
   `ApplyDialect::Mysql =>` dispatch arms (`:658`, `:921`, `:1134`, `:1255`, `:1332`), each
   constructing a `MysqlBackend`, plus a `use zeroship_migrate_mysql::DIALECT` at `lower.rs:35`. My
   original grep matched only the doc comments because I searched for the lowercase string and the
   dispatch spells it `Mysql`. **The claim the argument actually needs is the narrow one - nothing
   PASSES "mysql" - and that one is true.** The
   dialect string resolves through a *registry* - `preview_dialect` searches `shipping_backends()`
   and returns `Err("unknown dialect …")` at `crates/zeroship-migrate-node/src/verbs.rs:105` - so a
   default-off `mysql` feature degrades to a runtime rejection, not a compile error, and
   `--all-features` (what `clippy_gate.sh` lints under) still covers the code.

   **THE COST WAS COSTED WRONG - corrected 2026-08-31 by a build-lens review, verified.** This said
   "the cost is two hardcoded counts" and predicted a COMPILE failure. Both halves are false:

   - **A facade-only feature does not remove MySQL from the build.**
     `crates/zeroship-migrate-node/Cargo.toml:59` declares `zeroship-migrate-mysql` as a DIRECT
     dependency, bypassing the facade entirely, and the addon constructs `MysqlBackend` on five paths
     (apply, rollback, status, legacy status, baseline). So the addon still compiles MySQL - and then
     `ApplyDialect::parse` consults the now-two-vendor facade registry and REJECTS `"mysql"`
     (`verbs.rs:59-78`). **That is the worst available state: pay the build cost, lose the feature,
     and break the addon's published contract** (`zero-migrate-cli` documents MySQL 8 support and has
     a live N-API MySQL test).
   - **The predicted compile failure does not happen.** `crates/zeroship-migrate/Cargo.toml:83` keeps
     MySQL as an unconditional DEV-dependency, so the dialect-matrix tests still compile; their
     registry assertions fail at RUNTIME instead. And there are three such tests with vendor arrays,
     not one.

   So the real choice is: have `zeroship-migrate-node` explicitly enable `zeroship-migrate/mysql`
   (preserving its contract, accepting that N-API builds pay for MySQL), or add an addon-level
   feature gating the direct dependency, the enum variant, the lowerer, all five bridge arms,
   generated types, CLI branches, tests and docs. **Only the isolated `zeroship-migrate-server`
   benefits cheaply** - it depends on the facade and the PostgreSQL vendor and never on MySQL.

   This also means the target block's "`zeroship-migrate-*` untouched" is not true if the gate ships:
   the facade, the addon's feature propagation and the test matrix all change.

   *A near-miss worth recording:* `:161` of that test says "three is a promise to" drift, which reads
   as an argument against reducing three to two. It is not - it is about three SPELLINGS of the
   partition-capability fact, not three dialects. The sentence survives gating untouched.

A `zeroship-data-*` family converged in discussion, with `zeroship-schema` and `zeroship-data-plan`
merged and `zeroship-plugin-db` renamed. **A read-only review on 2026-08-31 overturned two thirds of
that shape, and the tree - not taste - is what overturned it.** The target below is the revised one;
what changed and why is recorded immediately after it, because the discarded version is the one a
reader is likely to arrive with.

## Target

```
libs/compio-postgres            driver, unchanged

zeroship-data-query-builder     the typed query grammar. ZERO dependencies, LEAF, and that stays
                                load-bearing. (today's zeroship-data-plan, renamed)
zeroship-data-core              the contract every backend implements, PLUS the backend-neutral
                                layer both already share: encryption, MaskKind, TypedCell/TypedRows,
                                and the four orphan helpers below. -> data-query-builder
zeroship-data-postgres          impl of the core contract.   -> data-core, compio-postgres
zeroship-data-sqlite            impl of the core contract.   -> data-core, rusqlite
zeroship-data-cdc-server        service tier: WAL stream, slot authority, reaper. Peer of
                                zeroship-migrate-server.
                                -> compio-postgres, zeroship-core. NOT data-core, NOT data-postgres.
zeroship-plugin-db              env.db surface, worker tier. impl NativePlugin. KEEPS ITS NAME.
                                V8, crud, broker, subscription lifecycle.
                                -> data-core, data-postgres, data-sqlite.
                                NOT -> data-cdc-server. The worker must not link the relay.

zeroship-core::change_event     the cross-process event type, beside usage_event and
                                replication_names, which are already there.

zeroship-migrate-*              the engine, dialect-complete, untouched
zeroship-migrate-server         the migration service host
```

**The five crates above are the DESTINATION. Only three of them should be built now, and the
justification this document originally gave for the family is WRONG - corrected 2026-08-31 by review,
verified twice.**

The original justification was: "`data-core` restores the predicate the prefix had lost - every
member depends on the contract crate, exactly what makes `migrate-*` a family." **Both halves fail.**

- **The edge `data-core -> data-query-builder` does not exist and is not a relocation away.** Nothing
  in `crates/zeroship-plugin-db/src/` references `zeroship_data_plan` at all - the only references in
  the workspace are two integration tests and `#[cfg(test)]` code in `zeroship-schema`. So NONE of
  `data-core`'s proposed contents names the query grammar. The edge appears only after Track A
  retypes the executor on `DbPlan` - which is ~3,420 lines across 205 call sites, with writes gated
  on #45. **A justification may not rest on a fact the plan's own sequencing places after the work
  being justified.** On the day `data-core` is created in this plan's order, the family still has no
  predicate.
- **"Every member declares `zeroship-migrate-ir`" is FALSE of the engine too.**
  `zeroship-migrate-policy` declares no migrate dependency at all, and `zeroship-migrate-node`
  declares the facade and three vendors but not `-ir`. The unqualified claim was mine and it is
  wrong. What IS true of `migrate-*` - and is the honest predicate to import - is: *a single spine
  with the contract at the waist; every member is above it, or is the one leaf below it.* Note the
  arrow: `migrate-ir/Cargo.toml:33` declares `zeroship-migrate-policy`, so the engine's CONTRACT
  depends on its LEAF, which is the same direction proposed here. The exemption is not the problem.
  **The missing spine is.**

So the honest statement is: **`data-*` becomes a family when Track A lands. Until then it is
unrelated crates and a prefix.** That is much weaker than "a core restores the predicate", and the
weaker sentence is the true one.

**And a core is not merely tidy here - it is REQUIRED, because four production call sites already run
the wrong way.** Backend-agnostic and PostgreSQL-path code calls *into* the SQLite module today:

| call site | reaches | on |
| --- | --- | --- |
| `crud/read_pipeline.rs:278` | `backend::sqlite::session_minter::parse_iso_to_millis` | the backend-agnostic read pipeline, so it runs for PG timestamps |
| `crud/mod.rs:409` | `backend::sqlite::vector::vec_to_le_bytes` | the write encoder |
| `crud/mod.rs:433` | `backend::sqlite::spatial::point_to_blob` | the write encoder |
| `v8_bridge.rs:34` | `backend::sqlite::session::{TypedCell, TypedRows}` | the V8 seam |

These are pure, side-effect-free helpers that merely LIVE under `backend/sqlite/`. Two consequences,
and the first kills a recommendation this document made two sections ago:

- **A whole-module `#[cfg]` on `backend/sqlite/` does not compile a PostgreSQL-only worker.** The
  "feature-gate the dev backend" step was costed here as "much smaller than extraction". It is not:
  it requires relocating these helpers first, which is the same first move the crate split needs.
- **`data-sqlite` cannot be extracted before they move either**, or `data-postgres` would have to
  depend on `data-sqlite`. `data-core` is where all four belong, alongside `encryption` (7 edges from
  each backend) and row-to-JSON. **One relocation unblocks the gate AND the split.**

**SETTLED WITHOUT A COMPILER, AND THE FULL COUNT IS 29 SITES ACROSS 13 FILES, NOT 4.** The review
that raised this flagged its own claim as "high confidence but not compiler-settled". It does not
need a compiler: the question reduces to whether the CALLERS are conditional, and they are not.
`crud/read_pipeline.rs` contains exactly one `#[cfg]` in the entire file - `#[cfg(test)]` at `:452`,
far below the call at `:278` - and `v8_bridge.rs:34` is a bare top-level `use`. An unconditional
caller plus a gated callee is a compile error by construction.

The same review noted it had not enumerated exhaustively. Enumerated (every `backend::sqlite`
reference from outside `backend/`, comment lines excluded):

| file | sites | what it reaches for |
| --- | --- | --- |
| `transaction/driver.rs` | 5 | `TerminalIntent`, `reservation::TerminalOutcome`, `SqliteSessionHandle` |
| `crud/mod.rs` | 4 | `vec_to_le_bytes`, `point_to_blob` (2 of the 4 are in tests) |
| `crud/mask_policy.rs` | 4 | `SqliteBackend` in type position |
| `context.rs` | 3 | `SqliteSessionHandle`, `SqliteBackend` |
| `transaction/cancel.rs` | 2 | `TerminalOutcome`, `SqliteCancelHandle` |
| `lib.rs` | 2 | construction at `:1109`, a test setter at `:728` |
| `exec.rs`, `v8_bridge.rs`, `crud/read_pipeline.rs`, `crud/unmask.rs`, `crud/mask_drift.rs`, `crud/write_pipeline.rs`, `transaction/mod.rs` | 1-2 each | `TypedCell`, `parse_iso_to_millis`, `SqliteBackend` |

**The transaction layer is the surprise, and it is the expensive one.** `transaction/driver.rs` and
`transaction/cancel.rs` carry seven references to SQLite terminal-outcome and cancel-handle types -
this is the SC-1 reducer (#8, #21), which was built to be backend-neutral and is not. Splitting or
gating the dev backend therefore reaches into the transaction reducer, not just the read pipeline.

Sites in type position naming `SqliteBackend` itself are expected to gate WITH the backend; the ones
that block are the type and helper imports in neutral code.

**THAT CENSUS IS A SPELLING COUNT, NOT THE FOOTPRINT, AND ITS EXCLUSION HIDES A CYCLE - corrected
2026-08-31 by review, verified.** Two defects in the method, both mine:

- **It greps the literal string `backend::sqlite`, so it misses every SEMANTIC reference.** Counted:
  **34 more** sites outside `backend/` spell the dependency `BackendHandle::Sqlite` or
  `TxConnection::Sqlite` instead. The real footprint is 63+, not 29.
- **It excludes all of `backend/`, but the cut line is `backend/sqlite/`.** That exclusion silently
  removed `backend/mod.rs` - the shared file that sits ON the cut and contains
  `pub enum BackendHandle { Sqlite(Rc<SqliteBackend>), ... }` (`:1475`), the SQLite re-export
  (`:91`), and a concrete SQLite change-stream return type (`:1622`).

**The second is structural, not another missed spelling.** `BackendHandle` names the SQLite backend
by value, so moving it into `data-core` produces `data-core -> data-sqlite -> data-core`. Leaving it
above the vendors means the production composition seam has no home in the proposed graph and must be
designed. Either way, a census that excluded the file holding the vendor-bearing enum could not have
found this.

**This is the third time in this document's history that a grep has been the defect** - a Cargo.toml
comment read as a dependency edge, source comments counted as code, and now a literal spelling
standing in for a semantic dependency. The pattern is not carelessness about greps; it is that each
census answered the question it could ask rather than the question that was being decided.

So the honest cost of "gate the dev backend" is unknown but larger than a 13-file relocation, and the
first thing any implementer must do is settle where `BackendHandle` lives.

### What to build NOW: three moves, not five crates

**`data-cdc-server` needs neither `data-core` nor `data-postgres`, and that is measured, not
argued.** Counting `crate::<module>` reach out of the three modules that move (`wal_consumer.rs`,
`replication.rs`, `slot_reaper.rs`), comment lines stripped:

| reaches | count | note |
| --- | --- | --- |
| `crate::broker` | 30 | becomes the wire this plan already prices |
| `crate::replication` | 5 | intra-group; moves with them |
| `crate::error` | 3 | a service that never crosses V8 should define its own |
| `crate::query` | 1 | `replication.rs:674`, inside `#[cfg(test)]` |

**Zero `crate::backend`. Zero `crate::encryption`. Zero `crate::descriptor`. Zero `crate::crud`.**
Their top-level imports are `compio_postgres`, `sha2`, `zeroship_core::replication_names` and the
error type - nothing else. So the CDC tier can be extracted TODAY, independently of Step 0, Track A,
the encryption relocation, the driver-neutral sub-traits and the `v8_bridge` cycle.

And it buys the whole measured security payoff: `zeroship-worker/src/db_posture.rs:123-126`
currently REQUIRES the worker role to hold `REPLICATION`, and full extraction is what lets that
requirement be deleted.

So the executable plan is:

1. **`zeroship-data-query-builder`** - rename `data-plan`, keep the empty manifest. Rename only.
2. **`zeroship-data-cdc-server`** - one new crate, no new prerequisites.
3. **SQLite feature-gated inside today's `plugin-db`** - after the 29-site relocation.

`data-core`, `data-postgres` and `data-sqlite` are the DESTINATION and should not be minted until the
two things that would make `data-core` a contract crate exist: driver-neutral sub-traits
(`backend/mod.rs:755`, `:786`) and a `DbPlan`-typed executor (Track A). **Created earlier,
`data-core` would name two vendors and V8 and depend on nothing below it - which is `plugin-db` with
a smaller line count.**

### Three independent answers on the count, and what they agree on

Reviewed 2026-08-31 by three reviewers with different lenses. They landed on three different numbers
- and the disagreement is entirely about the DESTINATION, not about what to build first.

| lens | lands on | the difference |
| --- | --- | --- |
| operator | five | as proposed |
| architecture | **three** | `data-core`/`-postgres`/`-sqlite` are a destination, not a plan; build the rename, the CDC tier, and the in-place feature gate |
| dependency graph | **six** | splits `data-encryption` out of `data-core`, so the core stays comparable to `migrate-backend` and the CDC tier never inherits crypto |

**All three agree on the two things that decide the first move:**

1. **`data-cdc-server` needs neither `data-core` nor `data-postgres`** - independently measured by
   two of them and re-derived here. The proposed `cdc-server -> data-postgres` edge is unsupported by
   any code in the tree.
2. **The CDC tier is extractable first and alone**, and it carries the whole measured security
   payoff (the worker's `REPLICATION` requirement at `db_posture.rs:123-126` becomes deletable).

The six-crate variant's argument is worth keeping even if the count is not settled: `encryption/` is
already a cohesive security subsystem - AEAD, key derivation and cache, AAD, versioned wire framing -
and folding it into a contract crate is what would force `data-cdc-server` to link crypto it never
uses. If `data-core` is ever built, build `data-encryption` beside it rather than inside it.

### Why `data-core` cannot hold three of its four proposed contents yet

The repo's actual standard for a contract crate is not "only traits" - `zeroship-migrate-backend` is
21,968 lines and holds shared implementation, a codec and value formatting. Its standard is stated on
its own manifest line: *"Sits between zeroship-migrate-ir and the per-vendor crates; **names no
dialect and ships no vendor**."* Mechanically checkable, and it holds - no driver in its
dependencies. Judged against THAT, `data-core` as specified fails on three contents:

- **Row-to-JSON is not a shared layer.** It is two vendor converters in one V8 file:
  `rows_to_json_value`/`row_to_json`/`column_to_json` take `compio_postgres::Row`, while
  `typed_rows_to_json_value`/`typed_cell_to_json` take the SQLite `TypedRows`/`TypedCell`. Moving it
  makes `data-core` declare `compio-postgres` AND own the SQLite cell enum. This document treated
  `PgSqlExecutor`'s driver-typed bound as a blocker while waving this one through; they are two
  instances of one defect.
- **Everything proposed transitively links V8.** `error.rs:67` imports
  `zeroship_runtime::state::OpError`, `zeroship-runtime` declares `v8`, the encryption modules all
  use `crate::error::DbError`, and `backend/mod.rs` names `DbError` 48 times. So `data-core` would
  link V8, and `data-cdc-server` would inherit it - **the relay whose entire justification is that it
  does not execute creator code would link the V8 runtime.** Solvable, but unnamed work.
- **Even `encryption`, the one genuinely neutral content, carries a plugin identity.**
  `encryption/keys.rs:318` declares its column-key env family against `crate::PluginDbConsumer`,
  whose `target` is bound to the cargo package name at `lib.rs:74`. Moving it either drags a
  plugin-identity type into the contract crate or changes a declared-env identity that
  `docs/reference/env-vars.md` documents by service.

**Two corrections to the proposed shape:**

1. **`data-postgres`, not `data-postgresql`.** The tree spells it `zeroship-migrate-postgres` and
   `libs/compio-postgres` without exception.
2. **Do not let one crate implement both the query contract and the CDC contract.** The worker links
   the PostgreSQL backend for ordinary queries; if that crate also carries the CDC implementation, the
   worker links the CDC code too and the trust-tier separation is cosmetic at the crate level. Either
   gate it (`data-postgres/cdc`, default-off) or split `data-postgres-cdc` out. **But the crate
   boundary is not the fence:
   the crate boundary is defence in depth, not the fence.** The fence is the role attribute - a
   worker holding `REPLICATION` can drop ANY slot regardless of which crate the code sits in.

**THIS BLOCK DOES ENUMERATE MODULES, AND THE CLAIM THAT IT DOES NOT WAS FALSE FOR SEVERAL
REVISIONS.** It said "This block names crates. It does NOT enumerate modules - that is Track B's
table." Then it listed "V8, crud, broker, subscription lifecycle" for `plugin-db` and "WAL stream,
slot authority, reaper" for the relay. `broker` is *precisely* the module whose double-listing was
correction instances five and six.

So the structural fix this document congratulated itself on was never in force. Two rules now, and
they are rules rather than descriptions:

1. **Track B's table is the module inventory.** The lines above are a reading aid; where they and the
   table differ, the table wins.
2. **The CDC rows in both places are conditional on the Full/Partial decision, which is STILL OPEN**
   (see "Not decided"). Under Partial only `slot_reaper` moves and the worker keeps decoding, which
   makes "WAL stream, slot authority" above wrong. An implementer must settle Full-vs-Partial before
   reading either list as an instruction.

A self-congratulating claim is worse than the defect it claims to have fixed, because it tells the
next reader not to look.

**Why the prefix survives, having briefly been dropped.** An intermediate revision of this document
DELETED the `data-*` prefix, on the argument that a prefix must denote something you can CHECK.
That argument is right and still stands: `plugin-*` means "implements `NativePlugin`
(`zeroship-runtime/src/core/plugin.rs:36`) and is composed at `zeroship-worker/src/cache.rs:246`";
`migrate-*` names a dependency-closed stack in which every member declares `zeroship-migrate-ir`.
At that moment `data-*` had no such predicate - its two members shared no production edge at all
(measured: across `wal_consumer.rs`, `replication.rs` and `slot_reaper.rs` there is exactly ONE
reference to any query-building symbol, `replication.rs:674`, inside the `#[cfg(test)]` module opened
at `:598`).

**`data-core` was proposed as the missing predicate. It does not supply one yet, and this paragraph
asserted that it did for several revisions after the target block had already withdrawn the claim.**
See the target block: the edge `data-core -> data-query-builder` does not exist in the tree, the
grammar is a zero-dependency leaf that by construction depends on NOTHING including the core, and
"every member depends on the contract crate" is false of `migrate-*` as well. The predicate arrives
with Track A or not at all.

**One naming argument from that revision survives and is why `data-query-builder` beats
`data-query-ir`.** `-ir` is already taken in this workspace and means the OPPOSITE:
`zeroship-migrate-ir` is "the zeroship-migrate **wire contract**" (`Cargo.toml:6`), whose
`MigrationIr` derives `Serialize, Deserialize, JsonSchema` and whose first dependency is serde. The
data-plane leaf is defined by forbidding exactly that. Naming it `-ir` would hand a reader that
expectation and then ban it.

`zeroship-schema` DISSOLVES: its live query building is replaced by the typed grammar rather than
moved, **`MaskKind` alone** is rehomed, and its dead regions are deleted (below).

*This sentence used to send "`MaskKind` and the sentinel codec" to `data-core`.* Both halves were
wrong by the time it was written: only `MaskKind` of the five mask types is live, and `mask_codec.rs`
is a dead fork of `zeroship-migrate-backend`'s copy with zero callers - it is DELETED, not moved. A
destination for a file the same document proves should not exist is the most confusing kind of stale
instruction, because it reads as a decision rather than an oversight.

### Three changes the review forced, each with the evidence that forced it

**1. The schema + data-plan merge is refused BY A TEST, not by preference.**
`crates/zeroship-data-plan/Cargo.toml` has an EMPTY `[dependencies]` table, and its own manifest
comment says why: `zeroship-schema` "was the obvious candidate … and is REFUSED, because it is not a
leaf: its manifest declares `compio-postgres`, `zeroship-core` and `tracing`. Depending on it would
drag a live PostgreSQL driver into a crate whose whole claim is that it can be built and tested
without a database, a runtime or an isolate."

The emptiness is a security mechanism, not tidiness: "`serde` is not in scope in this crate, so the
derive does not compile", which is what makes SC-3 decision 3 - `DbPlan` MUST NOT derive `Serialize`
- structural rather than reviewable. `serialize_derive_is_structurally_impossible`
(`tests/no_sql_text_escape_hatch.rs:210`) parses the manifest and fails if any dependency is
declared. **Merging `zeroship-schema` into it deletes that guarantee and turns that test red.**

So the IR core keeps its empty manifest and gets renamed, not merged. JSON decoding and JSON->IR
lowering live OUTSIDE it, in `plugin-db`, which may depend on whatever it needs.

*How I got this wrong:* I grepped `Cargo.toml` files for `zeroship-schema` and found data-plan among
them, concluding data-plan depends on schema. Both hits are in the comment quoted above - the comment
explaining the refusal. The enforcement test skips comment lines
(`trimmed.starts_with('#')`, `:224`); my grep did not. **That is the third time this session a
Cargo.toml comment has been read as a dependency edge.**

**2. `zeroship-data-binding` is the wrong name, and the argument I gave for it was factually false.**
I wrote that the reading "holds only because CDC leaves - change streams are not something an app
gets through its binding". `Collection` holds `pub(crate) binding: DbBinding`
(`v8_classes/collection.rs:36`) and mints its change stream through it (`:565`). A change stream is
*exactly* something an app gets through its binding.

And `DbBinding` is not the crate's public concept: it is private in release and carries no behaviour
beyond two strings (`binding.rs:13`). The crate's architectural role is `DbPlugin`, which implements
`NativePlugin` and registers `env.db` (`lib.rs:285`, `:340`), composed by the worker alongside the
KV, storage and workflow plugins (`zeroship-worker/src/cache.rs:246`). **`plugin-db` is already the
accurate name.** Keeping it also dissolves the "plugin-* family breaks" open question below at zero
cost - there is no asymmetry to accept or to fix by renaming four crates.

**3. The CDC boundary in the previous target contradicted this document's own Track B.** The old
target listed `broker` inside `zeroship-data-cdc` while Track B argued the broker stays in the
worker. Both sentences were mine, in one document. Beyond that contradiction, `cdc_lifecycle.rs` is
not relay-side lifecycle at all: it bridges V8 subscription leases to one consumer per worker process
(`cdc_lifecycle.rs:1`, `:69`), and the V8 wrapper owns and drops that lease (`subscription.rs:42`).
It stays. See Track B for what actually moves.

## The backends: PostgreSQL and SQLite

Measured 2026-08-31. The data plane's backend layer is 13,560 lines:

| | lines | tier |
| --- | --- | --- |
| `backend/sqlite/` | 9,485 | **dev only** |
| `backend/mod.rs` (trait + shared) | 2,201 | both |
| `backend/postgres.rs` | 1,477 | production |

Those three rows total **13,163**. A `find`-over-`backend/` gives 13,560; the 397-line gap is
`backend/lock_guard.rs`, which is `#[cfg(any(test, feature = "test-helpers"))]` (`backend/mod.rs:72`)
and so is not in a production build at all. **An earlier version of this section printed the 13,560
total above a table that adds to 13,163** - a table and its own total disagreeing, which is the
cheapest kind of error to catch and the easiest to skim past.

**The dev-only backend is 6.4x the production one, and the production worker compiles all of it -
but state that claim carefully.** 9,485 is TOTAL SQLite SOURCE lines, and it includes test regions of
its own (e.g. `backend/sqlite/mod.rs:2355-2369`, `session.rs:2692-2707`). The defensible claim is
that a feature-off build stops compiling the production-reachable SQLite module; **how many linked
bytes or symbols that removes is unmeasured, and this document should not imply it has been
measured.**
`zeroship-plugin-db` declares exactly two features, `test-helpers` and `live-db-tests`
(`Cargo.toml:180-188`); neither gates a backend. The `cfg(feature = "test-helpers")` at
`backend/mod.rs:85` only widens `pub(crate) mod sqlite` to `pub mod sqlite` - it changes visibility,
never whether the module is built.

**And the worker already refuses SQLite at RUNTIME, which is the argument for gating it at compile
time.** `zeroship-worker/src/main.rs:314-325`: "SQLite is the DEV TIER ONLY - refuse it on the
worker … N replicas fed a `sqlite:`/`file:` DSN would each open their own SqliteBackend on a
(possibly shared-volume) file with the engine's project-lock a no-op - concurrent cross-process apply
with zero serialization (a data-corruption class)." The guard is deliberate and its own comment
states the principle: "The authority is the worker's IDENTITY, not an env flag - this refuses SQLite
even if someone exported `ZEROSHIP_DEV=1` into a prod worker."

**A cargo feature keyed to the binary IS identity.** The worker went to real trouble to build a
runtime refusal for a backend it has no reason to contain. Gating it stops the production worker
COMPILING the dev backend, and it strengthens exactly the guard that already exists rather than
duplicating it.

*Say it that way and no other.* An earlier version of this sentence said gating "removes 9,485 lines
from the production binary and from the attack surface" - two overclaims in one clause. 9,485 is
total source including test regions, and no linked-byte measurement has been taken; and the review
that examined this found the runtime refusal is already TOTAL for the reachable surface, since a
`SqliteBackend` is constructed on exactly one production path (`lib.rs:1108-1113`) which the worker
refuses by DSN. So the gate removes DORMANT code. That is worth doing - defence in depth, and the
compile-time fence cannot be misconfigured - but it is not a reduction in what an attacker can reach
today.

### The split is right, and it is blocked by a measurable cycle

The target is the engine's own shape - `zeroship-migrate-backend` is a contract that
`-postgres`/`-sqlite`/`-mysql` implement without depending on the engine or each other. The data
plane should match it: `zeroship-data-core` + `zeroship-data-postgres` + `zeroship-data-sqlite`.

**It cannot be done by moving files today, because the backends reach back up into the plugin.**
Counting `crate::<module>` references out of each backend, **with comment lines stripped**:

| module reached | from `sqlite/` | from `postgres.rs` |
| --- | --- | --- |
| `encryption` | **7** | **7** |
| `auth` | 5 | 0 |
| `broker` | 3 | 0 |
| `v8_bridge` | 3 | 2 |
| `descriptor` | 2 | 2 |
| `binding` | 2 | 2 |
| `context` | 2 | 1 |
| `exec` | 0 | 2 |
| `wal_consumer` | 1 | 0 |
| `crud` | 0 | 0 |

**THE PREVIOUS VERSION OF THIS TABLE WAS WRONG IN BOTH DIRECTIONS, AND SO WAS THE CONCLUSION DRAWN
FROM IT - corrected 2026-08-31 by review, then re-derived independently to the same numbers.** It
read `broker` 6, `crud` 3, and omitted `encryption`, `auth`, `v8_bridge`, `binding` and
`wal_consumer` entirely. Half the broker hits were prose (`cdc.rs:7`, `:151`, `mod.rs:278`); ALL
THREE `crud` hits were prose. The count used `grep -o` over raw source, which cannot tell code from a
doc comment - the same instrument error that has now misread a Cargo.toml comment as a dependency
edge three separate times in this document's history.

**So "invert the broker edge, then split" was sequencing against the wrong edge.** The largest shared
out-edge is `crate::encryption`, 7 from each backend, and it has nothing to do with CDC:
`encryption/mod.rs:1-3` - "Cross-backend column encryption ... used by BOTH the Postgres and SQLite
`crate::backend` impls." In the engine, the equivalent layer sits BELOW the vendors, in the contract
crate. **`encryption` (1,591 lines) is what must move down first.**

**And `v8_bridge` is a two-way edge that no injection fixes.** `v8_bridge.rs:34` imports
`backend::sqlite::session::{TypedCell, TypedRows}` while both backends call back into it
(`postgres.rs:476`, `:572`; `sqlite/mod.rs:232`, `:1440`, `:1571`). Its own header claims "nothing
here knows about SQL or schema", contradicted at `:34` of the same file. Row-to-JSON conversion is
the real backend/plugin seam, and it is legal only because it is all one crate today.

**A shared contract crate is further away than "mirror the engine" implies.** Two of the would-be
contract traits name a vendor driver in their bounds: `PgSqlExecutor: SqlExecutor<Client =
compio_postgres::OwnedPooledClient>` (`backend/mod.rs:755`) and `PgLockManager` (`:786`). The
engine's contract crate names NO driver - `zeroship-migrate-backend`'s entire dependency list is
`zeroship-migrate-ir`, `zeroship-migrate-policy`, serde, serde_json, sha2, hex, uuid, base64,
thiserror. Making the sub-traits driver-neutral is a bigger job than any edge inversion and is not
otherwise in this plan.

**Nor is there a production trait to split on.** `backend/mod.rs:1443`'s `Backend` trait is
`#[cfg(any(test, feature = "test-helpers"))]` and says so itself at `:1438-1441`: "a **conformance
marker, not the production abstraction** ... nothing takes `dyn Backend`." Production dispatch is
`BackendHandle`, a closed enum. Of the 13 `pub trait` declarations in that file, exactly one -
`EncryptedColumn` - is used as a generic bound outside `backend/`.

**So the ORDER is longer than this plan claimed:** move `encryption` and row-to-JSON below the
vendors, make the sub-traits driver-neutral, resolve the `v8_bridge` cycle, THEN invert the broker
edge, THEN split. "Extract the backends" is not one move with one prerequisite.

**There is no cheap win here, and this paragraph used to claim one.** It said feature-gating the
SQLite backend inside today's `plugin-db` "is a much smaller change than extraction and delivers the
whole production-binary benefit. Do that first." Both halves are contradicted elsewhere in this same
document and the contradiction stood for several revisions:

- **Not much smaller.** The gate needs the same relocation the split needs - 63+ inbound sites, and
  `BackendHandle` rehomed first. A module `#[cfg]` does not compile a PostgreSQL-only worker.
- **The "production-binary benefit" is unmeasured.** 9,485 is total SQLite SOURCE including its own
  test regions; how many linked bytes a feature-off build removes has never been measured.

What survives is the security argument, which does not depend on either claim: the worker holds a
runtime refusal for a backend it has no reason to contain, and a cargo feature keyed to the binary is
the same authority applied earlier. Do it because the fence belongs at compile time, not because it
is cheap.

**AND THERE IS A THIRD OBSTACLE, STRUCTURAL, THAT NEITHER THE GATE'S COST NOR ITS BENEFIT SURVIVES
UNCHANGED.** CI builds the worker and the CLI in ONE cargo invocation:

```
cargo build --release -p zeroship-control -p zeroship-worker \
    -p zeroship-gateway -p zeroship-cli -p zeroship-migrate-server --bins
```
(`.github/workflows/ci.yml:1829-1830`)

The CLI is the dev tier and NEEDS SQLite; the worker must not have it. Cargo computes one feature set
per package per invocation, so a single `plugin-db` would be built with the union and BOTH binaries
would link it - the worker would contain the SQLite backend despite being "built without" the
feature. Getting the benefit means splitting that into separate invocations, which compiles
`plugin-db` twice and changes the CI job.

**MEASURED 2026-08-31 ON A SCRATCH WORKSPACE. IT UNIFIES.** This paragraph previously flagged the
unification as documented cargo behaviour that could not be observed here, because the feature does
not exist yet, and said "before anyone builds the gate, prove it on a scratch workspace". Done.

Three crates - `featlib` with a default-off `sqlite` feature exposing
`pub fn has_sqlite() -> bool { cfg!(feature = "sqlite") }`, `featworker` depending on it WITHOUT the
feature, `featcli` depending on it WITH it - in a `resolver = "3"` workspace, mirroring
`zeroship-worker` and `zeroship-cli`:

```
CONTROL  cargo run -p featworker              -> worker sees sqlite = false
TEST     cargo build -p featworker -p featcli -> worker sees sqlite = TRUE
                                                 cli    sees sqlite = true
```

The control proves the feature is genuinely off by default and the probe can tell the difference. The
test is the CI vector, and the worker binary reports the feature ON because the CLI asked for it in
the same invocation.

**So a default-off `sqlite` feature would NOT keep the backend out of the worker binary that CI
builds** - one `plugin-db` is compiled with the union and both binaries link it. Worse, it would look
correct: the worker's own manifest would not name the feature, and nothing today would report the
discrepancy.

**Two consequences for anyone who builds the gate anyway.** It needs separate cargo invocations for
the worker and the CLI, which compiles `plugin-db` twice and changes `ci.yml`. And it needs a GUARD
of its own - an assertion that the shipped worker binary really lacks the SQLite symbols - because a
gate whose bypass is invisible is the exact shape of fence this document keeps finding elsewhere.

## Measured starting point

| crate | src lines | |
| --- | --- | --- |
| `zeroship-migrate-core` | 95,528 | render 2.2 MB, model, schema, apply, engine |
| `zeroship-plugin-db` | 57,427 | includes ~200 KB of CDC |
| `zeroship-migrate-postgres` | 25,239 | |
| `zeroship-migrate-backend` | 21,968 | |
| `zeroship-migrate-mysql` | 20,018 | kept, by decision |
| `zeroship-migrate-sqlite` | 17,656 | |
| `zeroship-schema` | 17,169 | `query.rs` alone is 13,814 |
| `zeroship-migrate-ir` | 14,863 | |
| `zeroship-migrate-policy` | 7,012 | |
| `zeroship-migrate-server` | 6,994 | |
| `zeroship-data-plan` | 6,282 | **unwired** |
| `zeroship-migrate` | 162 | facade |

Two facts that shape everything below, both contradicting `AGENTS.md:143`:

- **No `zeroship-migrate*` crate depends on `zeroship-schema`.** Its only dependants are
  `zeroship-plugin-db` (a normal dependency) and `zeroship-schema`'s own dev-dependency on
  `zeroship-data-plan` - which runs the OTHER WAY: `schema` dev-depends on `data-plan`, and
  `data-plan` depends on nothing at all. *This bullet named `zeroship-data-plan` as a dependant of
  `zeroship-schema` until 2026-08-31; that is the reversed-edge error this document corrects at
  length two sections above, surviving in a stale bullet the correction did not sweep.* The engine
  carries parallel copies of the same code, so the `data-*` family has no cross-family edge.
- **`zeroship-data-plan` has zero production consumers.** It is a `[dev-dependencies]` entry in both
  dependants, and the `zeroship-schema` uses of it sit after `#[cfg(test)]` at `query.rs:6446`.

  **AND THE STALL HAS A COST THAT #12 PREDICTED IN ADVANCE - measured 2026-08-31.** #12, the task
  that built the IR families, closed with a conditional warning: *"the crate currently DUPLICATES
  four constants and two fence tables that zeroship-schema owns rather than replacing them. If the
  port stalls, that duplication is a liability - two copies of the reserved-name rules that can
  drift."* The port stalled, so the condition is met and the liability is live.

  `zeroship-data-plan/src/ident.rs` holds seven reservation surfaces:
  `PLATFORM_RESERVED_COLLECTION_PREFIXES` (`:97`), `NAMESPACE_RESERVATIONS` (`:194`),
  `BACKEND_CATALOG_RESERVATIONS` (`:210`), `COLUMN_RESERVATIONS` (`:222`), `ALIAS_RESERVATIONS`
  (`:251`), `DERIVED_NAME_RESERVATIONS` (`:260`, empty) and `MASKED_SUFFIX` (`:457`).

  **Exactly ONE of the seven is pinned against the other crates.**
  `reserved_collection_prefixes_match_migration_engine` (`zeroship-schema/src/query.rs:10504-10515`)
  asserts `PLATFORM_RESERVED_COLLECTION_PREFIXES` matches BOTH
  `zeroship_migrate_core::schema::query::` and `zeroship_data_plan::ident::` - a genuinely good
  three-way guard. Nothing guards the other six.

  `MASKED_SUFFIX` shows what that costs: data-plan names it as a constant at `ident.rs:457`, while
  `zeroship-schema` spells the same fact as a bare literal, `ReservedName::Suffix("_masked")`
  (`query.rs:786`). One fact, two spellings, no test relating them.

  **This is the third fork found in this dependency closure today**, after the two `mask_codec.rs`
  copies (whose prefixes HAVE already diverged) and the DDL/index builders duplicated inside the
  engine. The pattern is not carelessness: each fork was created deliberately, to keep a leaf crate
  dependency-free, and each was expected to be temporary. **What makes them liabilities is stalling,
  not forking** - which is an argument for finishing Track A rather than for never duplicating.
  #12 is marked complete; the consumer never landed.

## Step 0 - delete the dead. Independent, and first.

Roughly **2,044 lines** in `query.rs`, plus the dead half of `diff.rs`, delete before any porting
starts. Detail and per-symbol evidence in **#91** and **#92**. Summary:

| region | lines | why dead |
| --- | --- | --- |
| `query.rs:1057-1755` DDL builders | 699 | no PRODUCTION ROOT; engine uses its own `crate::schema::query`. It does have intra-crate callers - see the experiment below |
| `query.rs:1756-3022` index builders | 1,267 | engine calls its own `index_name`; `migrate-backend/src/ddl.rs:123` says "i.e. the ENGINE's" |
| `query.rs:135-140`, `:221`, `:335`, `:465` `system_field_indexes` | 78 | the trait declaration and its three impls, ABOVE the region table's "live" boundary. Correction 2 below |
| `diff.rs` differ + introspection | ~2,000 | `compute_diff` has no production caller; `read_live_schema` / `estimate_row_count` reachable only from `tests/sqlite_integration.rs` |

**"No callers" and "no production root" are different claims, and this table used to conflate them.**
The DDL builders row said "no callers" while the experiment below concedes seven intra-crate callers
inside `compute_diff`. Both facts are true and neither is the other: the builders are unreachable
from anything production runs, AND they are called from code that is itself unreachable. The
distinction is what makes the deletion ORDER necessary rather than arbitrary.

**Cut by SYMBOL, never by region marker.** The index region also holds LIVE code -
`raw_column_name` (`:2218`, 21 call sites), the mask and encryption sentinel builders,
`def_to_column_type_for_dialect`.

**`diff.rs` holds ONE live mask type, not five - corrected 2026-08-31 by review, verified.** This
document and #91 both said `MaskKind`, `MaskMeta`, `EncryptionMeta`, `WrappedType` and
`Classification` were all live. Only `MaskKind` is: the write mask pass uses it
(`crud/mask_pass.rs:81`, `crud/write_pipeline.rs:237`) and subscription predicate lowering uses it
(`read_set.rs:320`). The other four appear only behind `cfg(test)` / `test-helpers` in the SQLite
introspection path, and their one other consumer, `crud::mask_backfill`, is gated AND states in its
own header that it is unreachable: **"`crud::mask_backfill` has zero consumers"**, grepped 2026-08-28
(`crud/mask_backfill.rs:3-8`). Production mask policy uses string constants, not `Classification`
(`crud/mask_policy.rs:68`).

So the survivor set is `MaskKind` alone.

**And `mask_codec.rs` has now had that production-root test. IT FAILS, AND IT IS ALSO A FORK -
resolved 2026-08-31.** This document said the 407-line `zeroship-schema/src/mask_codec.rs` "needs the
same production-root test before it is carried across rather than deleted." Run:

- **Its parser has ZERO callers.** Every `parse_mask_sentinel` hit in `crates/` is inside
  `zeroship-migrate-backend/src/mask_codec.rs` - the ENGINE's own copy. Nothing in `plugin-db`, or
  anywhere else, calls the schema-side one.
- **It is a fork of a live engine module.** The engine ships its own `mask_codec.rs` (512 lines) whose
  header is near word-for-word identical and which round-trips the same four types. The engine's is
  the generalisation: prefixes are a `SentinelPrefix` knob defaulting to `zero-migrate:enc:` /
  `zero-migrate:mask:`, and it labels zeroship's `__zsmask:` a "legacy interop prefix - compat-only".
  The schema-side fork hardcodes `__zsmask:` and REFUSES anything else.
- **The engine's copy is the live PRODUCER.** `zeroship-migrate-postgres/src/schema.rs:305` calls
  `build_mask_sentinel_comments` from its `SchemaRenderer`, and `migrate-server` - the service that
  applies creator migrations - depends on `migrate-postgres`.

**This is NOT a live masking bug, and the reason matters.** Neither host configures
`SentinelPrefix`, so the engine writes `zero-migrate:mask:` while the schema-side reader accepts only
`__zsmask:` - which would be a silent unmasking defect IF the data plane read sentinels at runtime.
It does not: `descriptor.rs:1-30` records that the runtime descriptor is "the data plane's SOLE
schema authority" and names sentinel-reading as what it REPLACED. So the two prefixes never meet.

**Conclusion: delete it.** `mask_codec.rs` is a FIFTH dead region in `zeroship-schema`, and unlike the
others it has a live twin that is strictly more capable. Do not carry it into any new crate. Cutting on the region boundary
still breaks the read path - but at one point, not two, and the rest of that region is larger dead
weight than this plan credited.

**PROVED BY EXPERIMENT, 2026-08-31, and it corrected this plan.** I deleted `query.rs:1056-1754`
(the DDL region) and ran `cargo check --workspace --all-targets`. It FAILED with 16 errors, and the
two causes both change the sequencing above:

- **The DDL builders and the differ are ONE dead cluster, not two regions.** `diff.rs:998-1184` -
  inside `compute_diff` - calls `build_add_column`, `build_add_foreign_key`, `normalize_fk_action`
  and `build_drop_foreign_key`. They are transitively dead TOGETHER; neither deletes alone.
- **`SqliteEmitScope` is named from the renderer trait** at `query.rs:135`, `:221`, `:335`, `:465`.

**AND THAT SECOND BULLET WAS WRONG - CORRECTED 2026-08-31 BY REVIEW, AFTER RE-DERIVING IT.** It
previously read "used by the LIVE renderer … must be kept and rehomed, not deleted with its
neighbours." The four sites are not four users: they are ONE method - `system_field_indexes`, its
trait declaration plus its three dialect impls - and that method emits `CREATE INDEX IF NOT EXISTS`,
which is DDL, not query rendering. Its only caller is `build_system_field_indexes` (`:1556`, calling
at `:1562`), whose only caller is `:1469`, which sits inside
`build_create_table_with_fks_for_dialect_scoped_statements` (`:1241`) - inside the dead region.
Nothing outside `query.rs` names `system_field_indexes` at all.

So the cut is **larger** than this plan said, not smaller, and it reaches UP out of the region table:
delete `SqliteEmitScope`, the trait method and its three impls, `build_system_field_indexes`, and the
`:1469` call. "Keep and rehome" was exactly backwards.

**The lesson is about my instrument, and it revises the claim this section ends on.** The experiment
deleted a region and read 16 compile errors as "these symbols are live". A compile error proves only
that *something names the symbol* - it cannot tell you whether the NAMER is itself dead. Four of
those errors came from a dead method sitting in a region I had labelled live, so the boundary at
`:1056` is a region marker, not a liveness boundary. The compiler settles **reachability from a
root**; it does not settle **which roots are real**. That still has to be answered by walking the
call chain to something production calls - which is what finally settled this one.

So the grep-derived plan would have broken the build. Anyone executing step 0 should repeat this
experiment per cut rather than trusting the region table: **"no callers found" and "nothing can call
it" are different claims, and only the compiler settles the second.** The restore is clean -
`cargo check -p zeroship-schema --all-targets` returns 0 errors at `13,814` lines.

Delete the tests whose SUBJECT is the dead code, in the same change. They are the only thing making
it look alive, and keeping them is how the next reader concludes it still runs.

**BUT NOT BLANKET-DELETE, AND THIS IS A PREREQUISITE STEP 0 DOES NOT OTHERWISE HAVE - measured
2026-08-31.** A round-two review flagged that some tests use a dead DDL builder only as FIXTURE
SETUP; the correction went into #91's conditions and never into this section, which kept saying
"delete the tests that reach the dead code" without qualification. Measured scope:

- `crates/zeroship-plugin-db/tests/mask_flip.rs:104` defines `async fn fixture(...)`, the shared
  setup helper, which calls `build_create_table_with_fks` at `:111`. **All 7 tests in that file route
  through it.**
- `:829` additionally calls `build_create_indexes` - a builder from the INDEX region, which the
  review did not mention, so the fixture dependency spans BOTH dead regions rather than one.
- `crates/zeroship-plugin-db/tests/column_grants.rs:63` imports the same builder and uses it at
  `:155`; that file holds 5 tests.

**Twelve live tests, and their subjects are exactly what must not regress:**
`a_range_filter_on_a_masked_column_cannot_narrow_the_plaintext` (an information-leak oracle),
`the_raw_column_is_refused_on_every_inbound_surface`,
`no_write_verb_hands_back_a_column_the_descriptor_does_not_declare` (L24's regression guard),
`the_real_value_is_still_stored_and_still_reachable_by_the_audited_path`, and
`a_masked_predicate_is_lowered_for_the_change_stream`.

The first of those matters MORE after #45 settled, not less: masking is now explicitly a hygiene
feature the creator relies on, so the test proving a range filter cannot narrow the plaintext is the
test proving the feature works at all.

**So Step 0 is not pure subtraction.** Its real first move is migrating those twelve tests' fixture
off the dead builders, and only then deleting them. Any costing of Step 0 as "delete N lines" is
missing that step.

**The replacement path exists and is cheap - checked 2026-08-31, because the sentence above used to
say "a sanctioned provisioning path" without establishing that one was available.**

`zeroship-plugin-db` already declares `zeroship-migrate-server` as a DEV-dependency (`Cargo.toml:141`,
under a comment noting "`zeroship-migrate-server` does not depend on plugin-db, so no cycle"), and
`zeroship-migrate-server` in turn depends on `zeroship-migrate` (the facade) and
`zeroship-migrate-postgres`. **So the engine and the PostgreSQL vendor are ALREADY in plugin-db's dev
dependency graph, transitively.** Adding the facade as a direct dev-dependency adds no compilation
weight and introduces no cycle: nothing in the migrate family depends on plugin-db.

That makes the fixtures rewritable against the engine's own renderer -
`zeroship_migrate::render_artifacts_from_descriptors` on `zeroship_migrate_postgres::DIALECT` - which
is the same producer that writes creator DDL in production, so the fixture would exercise the
shipped shape rather than a parallel one.

**Do NOT copy `distributed_live.rs`'s approach, which solves the same problem badly.** That file
avoids the dead builders by pasting in descriptor bytes generated OFFLINE by
`render_artifacts_from_descriptors` (`:62-66`) beside a hand-written `EVENTS_DDL` constant (`:193`),
and its own comment states the hazard: the two "must agree, column for column" and **"Nothing checks
the descriptor against the catalog any more, so a field here that the table does not have surfaces as
a Postgres `42703 column does not exist` at read time."** That trades a dead-builder dependency for
an unchecked hand-maintenance burden. It is a workaround for the missing live call, not a model.

**This qualifies #1 (decision 10, "remove all DDL from plugin-db").** The DDL left plugin-db's own
source but stayed reachable through `zeroship-schema`, which plugin-db depends on and re-exports as
`crate::query` (`lib.rs:94`). Nothing called it, so nothing failed.

## Track A - one query builder

Rename `zeroship-data-plan` to `zeroship-data-query-builder`, keeping its empty manifest, then
replace the string builder from OUTSIDE it - the callers move to the typed grammar, and
`zeroship-schema`'s builder is deleted rather than merged in.

**This instruction used to read "merge `zeroship-schema` + `zeroship-data-plan` into
`zeroship-data-query`", which the same document then refuted two sections earlier and left standing
here for three revisions.** The merge is refused by
`serialize_derive_is_structurally_impossible`: `zeroship-schema` declares `compio-postgres`,
`zeroship-core` and `tracing`, and the test fails on ANY declared dependency. There is no version of
that merge that keeps the fence.

The real port is **~3,420 lines** of string building (`query.rs` regions 3023-6442: query builders,
soft-delete, HAVING, WHERE) - not 13,814.

**The call-site count was re-measured on 2026-08-31 and the old figure did not survive.** This
document said "35 `build_*` call sites in the data plane". That number is not reproducible under any
definition, and "the data plane" was doing ambiguous work. The method that IS reproducible: take the
58 `pub fn build_*` names `zeroship-schema/src/query.rs` exports, then count call sites of exactly
those names per compiled root (a bare `build_*(` grep overcounts - plugin-db has builders of its own;
a `crate::query::build_*` grep undercounts to 9, because most are imported unqualified via
`use crate::query::{build_insert, …}`):

| root | call sites |
| --- | --- |
| `crates/zeroship-plugin-db/src` | 26 |
| `crates/zeroship-plugin-db/tests` | 177 |
| `crates/zeroship-plugin-db/benches` | 2 |

**205 total, and the tests outnumber the production sites 7:1.** That ratio, not the 26, is the cost
of Track A: porting the data plane is a day of work whose bill is paid in the test suite. Any plan
that costs this as "26 call sites" is off by an order of magnitude.

The independent review reached **23** production call expressions against my 26, measuring
`(?:crate::)?query::build_<name>(` and checking imports by hand. The gap is that my count is of
non-comment LINES matching a builder name, which also catches type positions and re-exports; 23
counts call EXPRESSIONS. Take 23 as the production figure and 26 as its upper bound. Both refute the
35 this document used to assert, which is the point.

**And `6,282` is a FLOOR for the IR, not the replacement cost.** `data-plan` implements three of six
planned families (`lib.rs:49`), ships only a PostgreSQL renderer (`render/mod.rs:74`) while live CRUD
also selects SQLite (`crud/mod.rs:182`), and has no JSON->IR lowering at all. The completed IR is
materially larger than the string builder it replaces - so the "correctness trade, not a size
reduction" framing below is right, and understated.

**The security property that pays for it is concrete, not abstract.** Today's update and delete
builders omit `WHERE` entirely for an empty filter (`query.rs:4528`, `:4562`), and ordinary update and
purge paths call them with no mandatory bound (`crud/mod.rs:1386`, `:1631`). The typed `Update` and
`Delete` require a `RowLimit` to construct (`data-plan/src/write.rs:602`, `:754`). That is the whole
argument for the port in one line: an unbounded write is currently expressible and would become
unrepresentable.

Of the 100 `crate::query::` references (this document said 127), most are naming helpers rather than
builders: `quote_ident`, `raw_column_name`, `field_to_column`.

**One of those 26 refutes a Step 0 claim.** `crud/write_pipeline.rs:812` calls
`build_create_table_with_fks_for_dialect` - a DDL builder the Step 0 table lists as having no callers
outside `zeroship-schema`. It is inside `#[cfg(test)] mod tests` (opened at `:636`), so "no
PRODUCTION caller" still holds, but "no caller outside the crate" is false. Deleting the DDL region
breaks that test, in a different crate from the one being cut - so the "delete the tests that reach
the dead code in the same change" instruction above is not confined to `zeroship-schema`.

**Argue this as a correctness trade, not a simplification.** `data-plan` is 6,282 lines against
~3,420 replaced: the typed IR is LARGER, because `Ident`, `Literal`, `Predicate` and bounded depth
cost lines that `format!` does not. What is bought is that "an identifier is a value whose only
constructor validates" rather than "a `&str` that a validator was called on somewhere upstream".

Sequence: reads first (the search family, whose shape #12 already established), then writes.

**A3 (writes) WAS gated on #45. IT IS NOT ANY MORE - #45 was settled by the operator on 2026-08-31.**
The gate said: the `RETURNING` projection is exactly where column grants and the unmask primitive
meet, so porting writes first meant building against a contract that might be deleted.

The contract is now fixed, and in the direction that removes the entanglement entirely: **`unmask()`
stays worker-side and creator-controlled, and database-enforced column grants are NOT built.** The
reasoning is a reframing rather than a concession - masking is a HYGIENE feature, not a containment
boundary. The rows are the creator's own data in the creator's own schema, `defineMaskPolicy()`
exists so the creator sets the rule, and per-column withholding would have been the platform
overriding the creator's stated policy about the creator's own data.

So Track A can be ported reads-then-writes with no external dependency. Two things carry forward into
the write port:

- **`sanitize_app_actor` remains load-bearing**, and its justification is now sharper: a forged
  `actor: {kind:"auto"}` (DB-3) defeats THE CREATOR'S OWN policy, and creator control is the whole
  basis of the decision. The typed IR must not offer a path around it.
- **The unbounded-write bound is still worth having**, but for the reason the security review
  established rather than the one this document first gave: the residual is that every affected row
  materialises into a worker isolate SHARED ACROSS TENANTS, so it is a bounded availability concern,
  not tenant isolation. And `{}`-means-all-rows is a DOCUMENTED capability
  (`sdks/db/src/utils.ts:82`), so a mandatory `RowLimit` is a contract break to `@zeroship/db` that
  needs a replacement idiom, not free hardening.

## Track B - CDC out of the worker

**THE BROKER STAYS. This corrects an earlier draft of this document**, which listed it among the
modules to extract. `broker.rs` is a process-wide in-memory router that merges TWO sources - its own
header: "local mutations within any isolate in this process, and pgoutput WAL frames decoded by the
streaming consumer". `crates/zeroship-plugin-db/src/backend/sqlite/cdc.rs:646` calls
`crate::broker::publish(&event)` directly.

**The broker stays, but the reason given here was WRONG - corrected 2026-08-31 by review, verified
against `exec.rs`.** This paragraph used to end "Moving it would turn every local `db.insert(...)`
into a network round trip to observe your own write." That is not what the code does. `emit_for_rows`
(`crates/zeroship-plugin-db/src/exec.rs:470`) returns BEFORE emitting locally in both production
configurations:

- `:478` - on SQLite, because the writer actor's commit hook already publishes; the SDK-local emit
  "races the CDC publisher and produces duplicate identical live snapshots" (`:481-482`).
- `:485` - on PostgreSQL whenever the WAL consumer is running for that app, because, in its own
  words, "the WAL consumer is running for this app, [so] it owns the publish path for events this
  isolate writes. The corresponding `emit_local` call would be a no-op" (`:448-451`).

So the SDK-local emit at `:490` onwards is reached only on what `:480` names "the Postgres/no-WAL-
consumer path". **A PostgreSQL subscriber already observes its own write via WAL today.** There is no
in-process short circuit to protect.

**The correct reason the broker stays is registry locality, not write latency.** It is the
subscriber registry that V8 objects hold handles into: `v8_classes/subscription.rs:3` - "The wrapper
owns the `broker::Subscription` directly in its V8 [slot]" - with `:316` minting it through
`broker::try_subscribe`. A registry whose handles live in V8 slots cannot leave the process that
runs V8. That argument holds regardless of which side publishes.

So the seam is one module lower. **THIS TABLE IS THE MODULE INVENTORY, AND IT WINS WHERE THE TARGET
BLOCK DIFFERS.** The target block does enumerate some modules as a reading aid, contrary to what this
document claimed for several revisions; see the correction there.

**Every "moves to the relay" row below is conditional on Full**, which is still an open decision.
Under Partial, only `slot_reaper.rs` moves and `wal_consumer.rs`/`replication.rs` stay.

| stays in the worker | moves to the relay |
| --- | --- |
| `broker.rs` - the subscriber registry V8 holds handles into | `wal_consumer.rs` - decodes pgoutput |
| `cdc_lifecycle.rs` - bridges V8 subscription leases to one consumer per worker process (`:1`, `:69`) | `replication.rs` - slot and publication lifecycle, **rewritten not moved** (below) |
| `replication_ops.rs` (47 lines) - V8 bridge for replication diagnostics; calls `replication::watchdog_query` (`:33`), which moves | `slot_reaper.rs` - the privileged, destructive part |
| `change_stream_pg.rs` (315 lines) - "the single ownership path for provisioning, starting, stopping and cleaning up a worker's logical-decoding consumer" (`:1-4`); `backend/mod.rs` names it 4 times in code | |

**Corrected 2026-08-31 by review. Three defects in the previous version of this table:**

- **`cdc_lifecycle.rs` was listed as moving while the target block said it stays.** Both sentences
  were mine. The tree settles it against the table: it is worker-side, and the V8 wrapper owns the
  lease it hands out (`subscription.rs:42`).
- **`change_stream_pg.rs` (315 lines) and `replication_ops.rs` (47 lines) were unassigned entirely**,
  and the boundary forces both. Moving `wal_consumer.rs` orphans `cdc_lifecycle`'s handle type
  (`cdc_lifecycle.rs:21` imports `WalConsumerHandle` from `change_stream_pg`); moving `replication.rs`
  orphans a V8 dispatch. Both become new cross-process calls this plan had not priced.
- **`replication.rs` cannot be extracted, only rewritten.** It is keyed per app per worker (`:1`,
  `:96`), while the settled target is one slot and publication per Datastore with relay fan-out
  (`2026-08-28-app-database-decoupling.md:955`). Moving the file would move the wrong data model.

Extract the right-hand column into the relay crate, hosted by a process that never executes creator
code - plausibly the relay that #5 builds, which would make the extraction its prerequisite rather
than a separate job.

### The blocker no round found until now: moving `wal_consumer` severs the dedup fence

**Full extraction breaks local-emit suppression, and would double-deliver every change to every
subscriber.** Traced 2026-08-31 from a call site a reviewer noticed but did not follow.

`change_stream_pg.rs:202` calls `wal_consumer::run_supervised_controlled`, whose signature
(`wal_consumer.rs:771-775`) is already not RPC-shaped - it takes a live `WalConsumer` plus a
`flume::Sender` and `flume::Receiver`, in-process channels. But the fatal line is the next one:

```rust
let _suppression = SuppressGuard::activate(&app_id);   // wal_consumer.rs:781
```

held, per its own comment, "for the supervisor's full lifetime, including reconnect backoff", because
"allowing local emit during the gap would deliver once locally and again when WAL replay catches up"
(`:777-781`).

That guard writes **`static SUPPRESSED_APPS: LazyLock<Mutex<HashMap<String, usize>>>`**
(`wal_consumer.rs:72`) - a PROCESS-GLOBAL refcount. And its reader is `exec.rs:485`, on the mutation
path, which stays in the worker:

```
relay process                          worker process
  run_supervised_controlled              exec.rs:485  is_app_suppressed(app_id) -> FALSE
    SuppressGuard -> SUPPRESSED_APPS       (nothing populates the worker's map)
                                         -> local emit RESUMES
                                         + relay also delivers the same change over WAL
                                         = every change delivered TWICE
```

**This is not a cross-process call to price. It is a process-global invariant that a process split
severs**, and it fails OPEN - no error, no refusal, just duplicate events reaching app JS. It is
exactly the hazard the guard was written to prevent, reintroduced by moving the guard's owner into a
different process from the code it guards.

So Track B's "Full" option acquires a third requirement, alongside the wire protocol and the
`db_posture` inversion: **the relay must drive the worker's suppression state remotely** - the worker
has to learn "app X is now WAL-fed, stop emitting locally" and, critically, "app X's stream is
disconnected but its slot is retaining, KEEP suppressing" - which is the reconnect-backoff case the
comment singles out. A naive "suppress while connected" signal gets the backoff window wrong and
double-delivers exactly there.

**This strengthens the case for extracting the CDC tier FIRST and alone**, because the suppression
fence is the one piece of the WAL side that is genuinely entangled with the worker, and it is far
easier to see and design when nothing else is moving at the same time.

Hazards this track inherits, all measured:

- **#25** - the CDC path has no executor-side fence and cannot have one. `SET LOCAL ROLE` is
  executor-side; decoding does not go through the executor. Publication membership and column lists
  are the only fence, and the design measured a plaintext value arriving in a decoded stream when a
  publication has no column list. The new crate owns publication correctness.
- **#60** - the leader election uses `pg_try_advisory_lock`, and advisory locks are DATABASE-scoped
  (measured: the same key held simultaneously in two databases of one cluster). A cluster-wide
  service needs a different coordinator, and widening the reaper's queries before that exists gives
  N databases N unsynchronised reapers.
- **#54** - the SQLite ATTACH alias is the app id, and CDC routing keys on it.
- **#89** - archived apps retain their CDC.

**Open, and not settled by the "own service" decision:** does the worker STOP DECODING WAL, or does
it keep decoding its own stream while only the privileged and destructive parts move? Measured:
`wal_consumer.rs:21-23` opens a `replication=database` connection and issues `START_REPLICATION SLOT
... LOGICAL`, so REPLICATION is legitimately required today and moving the reaper ALONE does NOT let
`db_posture.rs:123-126` narrow.

- **Partial** - only `slot_reaper` moves. The worker keeps REPLICATION and keeps decoding. It then
  still holds a privilege PostgreSQL cannot distinguish from "drop anyone's slot", so the only fence
  is WHICH PROCESS ISSUES THE DROP - defence in depth, not capability removal.

  **MEASURED 2026-08-31, on live PostgreSQL 18.6, rather than inferred from the documentation.** Two
  non-superuser roles, both carrying only the `REPLICATION` attribute. `probe_owner` created a
  logical slot; `probe_intruder` - a DIFFERENT role - then ran
  `pg_drop_replication_slot('probe_slot')`. It succeeded silently, and the slot count went 1 to 0.
  **PostgreSQL enforces NO per-slot ownership check.**

  So this is now a fact, not a reading of the role-attribute model: a worker holding `REPLICATION`
  can drop ANY slot on the cluster, including slots belonging to other tenants' workers and to the
  relay itself. Relocating the reaper's CODE changes nothing about that. The capability is the role
  attribute, and the only way to remove it is `NOREPLICATION` - which is what "Full" means and
  "Partial" does not.

  *This was the security review's S3 claim, which that review explicitly flagged as resting on
  "PostgreSQL's documented role-attribute model, not a live probe". It holds.*

  **A second probe closes #60's open question, in the dangerous direction.** #60 asked whether a
  widened reaper query would fail SILENT - whether other databases' slot rows are even visible to a
  non-superuser - because that would be a gentler failure than over-reaping. They are visible: a slot
  created in database `slotvis_a` is returned to a `REPLICATION`-only role connected to database
  `postgres`, with its `database` column reading `slotvis_a`, while the `current_database()` filter
  returns 0 for it.

  So the two capabilities compose: **any `REPLICATION` role can SEE every slot on the cluster and
  DROP any of them.** The reaper's `database = current_database()` predicate
  (`slot_reaper.rs:213`, `:251`) is the entire blast-radius fence, and it is application SQL, not a
  boundary PostgreSQL enforces. That is the sharpest available argument for Full: the database
  declines to draw the line, so the only place it can be drawn is which process holds the attribute.
- **Full** - the whole WAL side moves and the worker loses REPLICATION outright, satisfying the
  invariant rather than approximating it.

### What Full costs, every item verified against the tree

Four edits. The first two are design work; the last two are small and mechanical, and they are the
ones a plan would forget because nothing in the crate graph points at them.

**1. A wire protocol, not an adapter.** `ChangeEvent` derives `Debug, Clone` and nothing else
(`broker.rs:80`), and its own doc records that `schema` is conflated with `app_id` (`:78`) - a
conflation the decoupling work exists to undo. Track B owns authenticated framing, versioning, event
identity and deduplication, reconnect and replay, backpressure, subscriber registration and
grant-revision purging. Feeding `broker::publish` is the last adapter in that chain, not the design.

**2. The suppression handoff, which is the blocker no review round found.** `SuppressGuard::activate`
(`wal_consumer.rs:781`) writes `static SUPPRESSED_APPS` (`:72`), a process-global refcount that
`exec.rs:485` reads on the mutation path. Move the writer to the relay and leave the reader in the
worker and every change is delivered TWICE, silently. The relay must drive the worker's suppression
state remotely, with TWO signals: "app X is WAL-fed now", and "app X is disconnected but its slot is
retaining, KEEP suppressing" - the reconnect-backoff case `:777-781` singles out.

**3. Invert the boot check.** `crates/zeroship-worker/src/db_posture.rs:123` currently reads:

```rust
if !posture.replication || !posture.bypass_rls {
    return Err("worker database role requires only REPLICATION and BYPASSRLS for logical decoding")
}
```

A `NOREPLICATION` worker FAILS THIS CHECK AT BOOT. The requirement must be inverted, not deleted -
and note the two attributes share ONE condition, so a half-edit that drops the replication clause
leaves `BYPASSRLS` required by accident rather than by decision. Decide `BYPASSRLS` deliberately at
the same time.

**4. A new migration revoking the attribute.**
`db/migrations-ts/20260818000200_worker_database_authority.ts:35` grants it:
`ALTER ROLE zeroship_worker WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE INHERIT REPLICATION
BYPASSRLS`, via raw SQL because "the role DSL does not expose PostgreSQL's REPLICATION attribute"
(`:36`). The template for the reverse is two lines below at `:39`, where `zeroship_workflow_owner`
gets `NOREPLICATION NOBYPASSRLS`.

**And the payoff is measured, not argued** (see the probes above): `REPLICATION` confers
see-every-slot AND drop-any-slot, cluster-wide, with no ownership check. Partial removes none of it,
because the capability is the attribute and not the code's location.

**Full is cheaper than an earlier draft of this document claimed - and the reason is now the
measured one.** That draft said full extraction "adds a hop to every subscription". A second draft
answered that "the broker stays, so LOCAL writes still short-circuit in-process with no network at
all", which is false: `exec.rs:485` suppresses the local emit for exactly the apps whose WAL consumer
is running. The correct answer is stronger than either. **On PostgreSQL a subscriber's own write is
ALREADY WAL-bound today**, so extraction cannot add a round trip to a path that never had one - it
adds one process hop to a path already going through WAL.

**But "the worker needs no new client machinery" was ALSO wrong, and this is the hidden cost of the
whole track.** That claim rested on the relay feeding `broker::publish` over a transport exactly
where `wal_consumer` feeds it in-process. The event cannot cross a process boundary as it stands:
`ChangeEvent` derives `Debug, Clone` and nothing else (`broker.rs:80`), and its own doc records that
"`schema` is conflated with `app_id`" (`:78`) - a conflation the decoupling work exists to undo.

So Track B owns a **new security-sensitive wire protocol**, not an adapter: authenticated framing,
versioning, event identity and deduplication, reconnect and replay, backpressure, subscriber
registration, and grant-revision queue purging - plus resolving datastore/schema/database/grant/app
at the relay and enforcing a revision barrier on both sides
(`2026-08-28-app-database-decoupling.md:980`, `:992`). Feeding `broker::publish` is the LAST adapter
in that chain, not the design. Any estimate of Track B that prices it as "move three modules" is
pricing the wrong work.

## The rename that is NOT happening, and what its cost bought

**`zeroship-plugin-db` KEEPS ITS NAME.** This section proposed renaming it to
`zeroship-data-binding`; that was withdrawn (the target block and its correction 2 carry the
reasons). **The section is kept for one reason only: the measurement below is the cost of ANY
whole-crate rename in this repository, and it is the argument for not doing one casually.** Read
every number here as "what a rename costs", not as "what we are about to do."

Re-measured 2026-08-31 with the pattern stated, so the next reader can reproduce it rather than
inherit it:

```
grep -rIoP 'zeroship[-_]plugin[-_]db' crates libs sdks tests docs deploy db schema | wc -l   # 1520
grep -rIlP 'zeroship[-_]plugin[-_]db' crates libs sdks tests docs deploy db schema | wc -l   # 159
grep -rIoP '(?<!zeroship[-_])plugin[-_]db' ...                                               # 675 more, bare
```

**1,520 occurrences of the full crate name across 159 files**, plus 675 bare `plugin-db` mentions in
prose and comments. And the breakdown is the part that matters, because it is not what "1,520
references" suggests:

| root | files carrying the crate name |
| --- | --- |
| `docs` | 97 |
| `crates` | 49 |
| `tests` | 9 |
| `libs` | 3 |
| `sdks` | 1 |

**61% of the affected files are documentation.** This is a prose edit with a code edit inside it, not
a refactor with some docs to fix - which changes who should do it and what "mostly mechanical" is
claiming. The compiler covers the 49; nothing covers the 97 except `doc_citation_gate.sh`, and that
checks path citations, not the 675 bare prose mentions.

Two things still make it safer than it sounds: `deploy/Dockerfile` uses `COPY crates/ crates/`
wholesale rather than a curated list, so #64's failure mode is already gone; and
`doc_citation_gate.sh` checks 139 `AGENTS.md` paths, so broken doc references fail loudly.

*This document said "1,467 references" until it was re-measured.* The file count (159) was and is
exact; the occurrence count drifted by 53 - and the drift is self-inflicted, because writing THIS
DOCUMENT added `crates/zeroship-plugin-db/...` citations to the corpus being counted. A measurement
of a corpus you are actively writing into goes stale as you write.

**The rule this leaves behind, now that the rename itself is withdrawn:** a whole-crate rename in
this repository costs ~1,520 edits across 159 files, 97 of them prose that no compiler checks. Never
do one on its own; ride it with a change that was already reshaping the crate. That is why
`plugin-db` keeping its name is worth more than the tidiness the rename would have bought, and why
the four NEW crates are the place to spend naming effort - they cost nothing, because nothing cites
them yet.

## Not decided

- **`zeroship-migrate-server`** is a service HOST, not engine. Left in the `migrate-*` family for
  now; a `-service` suffix is arguable.
- ~~**The `plugin-*` family breaks.**~~ **CLOSED - it does not.** This entry asked whether to accept
  an asymmetry or rename four crates, because db (57,427) was leaving while kv (3,019), storage
  (3,808) and workflow (10,847) stayed. Since `plugin-db` keeps its name, `AGENTS.md`'s "Adding a
  native primitive" table stays true and there is no asymmetry to declare. *The entry also said db is
  "15x kv"; 57,427/3,019 is 19.0. Fifteen is the db/storage ratio - two ratios, one sentence.*
- **Whether the worker keeps decoding WAL** (above).
- **Feature-gating the SQLite backend** (above) - a decision, not a discovery: it removes 9,485 lines
  from the production worker and hardens a guard the worker already implements at runtime.
- **`AGENTS.md:143`** needs correcting with this work, saying "present but uncalled" rather than
  deleting the clauses, so the next reader does not re-add them. Its four contents clauses are true
  as descriptions of what the file HOLDS; the "reused by the migration engine" clause is false.

## The pattern worth naming

Four instances of built-tested-unreferenced code sit in one dependency closure: the DDL builders,
the index builders, the differ plus live introspection, and `data-plan` itself. Every one has
passing tests, which is exactly why none of them looked dead. When this shape is found again, the
question to ask is not "do the tests pass" but "who calls this in production".

## The pattern in how this document got things WRONG

Five claims in this document have been corrected since it was written, and they share one shape:
**each substituted a proxy for the execution path, and each proxy was locally accurate.**

| the claim | the proxy trusted | why the proxy was not wrong, just not the answer |
| --- | --- | --- |
| `env.db` must grow a name for a second database | a general principle about naming | true in general; the design had already scoped one-app-N-databases out |
| the DDL region is dead and deletes cleanly | grep, searched OUTSIDE the crate | correct about the outside; seven callers were inside |
| `SqliteEmitScope` is live and must be rehomed | a compile error naming it | the symbol WAS named - by a method that is itself dead |
| moving the broker adds a round trip to local writes | `broker.rs`'s own header, "merges local mutations … and WAL frames" | accurate about the BROKER; its caller suppresses the local side (`exec.rs:485`) |
| the DDL builders have "no callers" | the same grep, restated in a table | conflated "no production root" with "no callers"; both true, neither the other |

The fourth is the sharpest, because the misleading source was a correct comment. `broker.rs` really
does merge two inputs. What it cannot tell you - what no module header can - is whether anything
still feeds one of them. **A module's documentation describes the module; questions about the SYSTEM
are answered only at the call sites.**

**THAT GENERALISATION IS INCOMPLETE, AND THE OMISSION IS FLATTERING - added 2026-08-31 after a
second review.** Every row above is an EPISTEMIC failure: I believed a wrong thing because I asked a
proxy. But two of this document's defects were not that at all. `broker.rs` was listed in the target
block and contradicted in Track B; the round-one fix corrected Track B and left `cdc_lifecycle.rs`
contradicting the target block the same way. Nothing was mis-believed there. **The document held one
module inventory in two places and edited them independently**, so a correction applied to one copy
left the other stale - and did so twice, for two different modules, in two consecutive rounds.

A table of "how I reasoned badly" cannot catch that, because the cause is not reasoning. The fix is
structural and now stated where it binds: **Track B's table is the only module inventory, and the
target block names crates only.** Diagnosing every defect as a thinking error is its own bias - it
implies the remedy is to think harder, when the remedy here was to keep one list.

The practical rule: grep answers spelling, the compiler answers "is this named", and neither answers
"does production reach this". Only walking outward from a real entry point does. Every correction
above arrived when someone walked that path - which is the argument for doing it BEFORE writing the
claim, not after review returns.
