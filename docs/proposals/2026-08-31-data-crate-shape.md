# The final shape of the data crates

Status: **planned, not started.** Every number here was measured on 2026-08-31 at `1bf9fdce4`.
Line counts are `find src -name '*.rs' -exec cat {} + | wc -l`.

## How to read this document

**It is written correction-in-place: where a claim was wrong, the wrong claim is QUOTED and then
overturned, in the same paragraph.** That preserves why the mistake was reachable, which has been
worth more than one re-derivation. It also means a sentence in quotation marks may be the OPPOSITE of
what is currently believed, and three reviewers independently mistook history for instruction.

So: **a paragraph that quotes a claim and corrects it is history. The correction is the live text.**

This section is deliberately NAVIGATIONAL, not a summary. It names where each thing is decided and
does not restate any of it - a third copy of a fact is a third thing that can go stale, which is
precisely the defect that produced six of this document's contradictions.

| what | where | status |
| --- | --- | --- |
| the crate graph | "Target" | operator-proposed, five crates; only three to be built now |
| which modules move | the placement table; Track B's table for the CDC stays/moves detail | SETTLED: Full |
| what to delete | "Step 0" | blocked on a fixture migration, see that section |
| the query-builder port | "Track A" | ungated since #45 settled |
| the CDC extraction | "Track B" | **SETTLED: FULL.** The worker loses `REPLICATION`. |
| the SQLite feature gate | "The backends" | three measured obstacles; recommendation is against |
| the plugin-db rename | "The rename that is NOT happening" | WITHDRAWN; kept only for its cost measurement |
| everything still undecided | "Not decided" | - |

**Settled by the operator, and binding on the rest:** `#45` - `unmask()` stays worker-side and
creator-controlled, masking is a hygiene feature rather than a containment boundary, and
database-enforced column grants are not built. That is what ungates Track A's writes.

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
zeroship-plugin-db              THIN. The worker/runtime plugin ADAPTER ONLY: impl NativePlugin,
                                the V8 objects, the V8 seam, per-isolate context. ~7,400 lines.
                                -> data-engine. NOT -> data-cdc-server; the worker must not
                                link the relay.
zeroship-data-engine            the data plane's actual logic: crud pipeline, transactions,
                                exec, broker. ~25,000 lines. -> data-core

zeroship-core::change_event     the cross-process event type, beside usage_event and
                                replication_names, which are already there.

zeroship-migrate-*              the engine, dialect-complete, untouched
zeroship-migrate-server         the migration service host
```

### `plugin-db` becomes a THIN ADAPTER - operator decision, 2026-08-31

**It keeps its name and loses almost everything else.** It becomes the worker/runtime plugin adapter
only: the `NativePlugin` impl, the V8 objects, the V8 seam and per-isolate context. Everything else
moves down.

**That decision exposed a hole in the five-crate target, and the measurement is why the count grew.**
`zeroship-plugin-db` is 57,427 lines:

| module | lines | |
| --- | --- | --- |
| `backend/` | 13,560 | both drivers plus the shared trait |
| `crud/` | 12,972 | the read/write pipeline |
| `transaction/` | 8,592 | the SC-1 reducer and driver |
| `v8_classes/` | 3,455 | Db, Collection, DbPlatform, MaskedValue, Replication, Subscription |
| `encryption/` | 1,591 | |
| `auth/` | 1,459 | session setup, `SET LOCAL ROLE` |
| top-level | 15,462 | broker 1,937, exec 1,769, error 1,703, context 1,688, wal_consumer 1,440, lib 1,357, replication 908, v8_bridge 867 |

A genuine adapter is `lib.rs` + `v8_classes/` + `v8_bridge.rs` + `context.rs`, about **7,400 lines**.

> **This line contradicts `:191` and `:1733`, which send `context.rs` (1,688 lines) to `data-engine`.
> Flagged 2026-08-31; not silently reconciled, because the right answer is now in doubt.** `:191`
> declares the question "RESOLVED by the `BackendHandle` finding", and a round-5 reviewer refuted that:
> `context.rs` carries Postgres in its **fields**, not just in `BackendHandle` - `:80`
> `TxConnection::Postgres(OwnedPooledClient)` and `:178` `pool: Option<Rc<Pool>>`. Field position is
> invisible to the module walk, the type walk AND the signature census. A module holding a live PG
> pool is not obviously engine and not obviously adapter; it is the per-isolate connection cache, and
> where it lands is a real decision this document has twice recorded as already made. **Both claims
> stay visible until it is actually decided.**
So roughly **50,000 lines need a destination**, and the five-crate target had one for most of them and
**none for the largest single block**: `crud/` + `transaction/` + `exec.rs` is **~23,000 lines** of
pipeline and reducer that is not a contract, not a driver, not the grammar and not the relay. Putting
it in `data-core` would make the core the big crate again with drivers attached, which is the thing
the split exists to undo.

Hence `zeroship-data-engine`. The count is **six**, and it is six because the modules are six things,
not because six is a nicer number.

### Where every module lands, and the eight that need a decision rather than a placement

All 57,427 lines assigned. **The point of the exercise is the second table, not the first** - `crud/`
had no home until someone enumerated, and enumerating found seven more like it.

| destination | modules | lines |
| --- | --- | --- |
| `plugin-db` (thin) | `v8_classes/` 3,455, `v8_bridge.rs` 867, `lib.rs` (the `DbPlugin` part) | ~5,700 |
| `data-engine` | `crud/` 12,972, `transaction/` 8,592, `exec.rs` 1,769, `broker.rs` 1,937, `read_set.rs` 659, `tx_route.rs` 267, ~~`tx_scope.rs` 142~~, `drop_namespace.rs` 218, `cross_app_fk.rs` 235 | ~26,700 |
| `data-core` | `error.rs` 1,703 (less one method), `descriptor.rs` 139, the driver-neutral half of `backend/mod.rs` | ~3,000 |
| `data-encryption` (if split) | `encryption/` | 1,591 |
| `data-postgres` | `backend/postgres.rs` | 1,477 |
| `data-sqlite` | `backend/sqlite/` | 9,485 |
| `data-cdc-server` | `wal_consumer.rs` 1,440, `replication.rs` 908, `slot_reaper.rs` 593 | 2,941 |

**The `data-engine` row is the weakest line in this table, and four review rounds did not catch why.**
It reads as nine whole modules moving intact. Five of them do not: `crud/`, `transaction/`,
`tx_route.rs`, `crud/unmask.rs` and `tx_scope.rs` contain **39 production functions whose signatures
carry `v8::`** - `run_op`, all 17 `dispatch_*`, `transaction_dispatch` and the promise finalizer chain -
totalling 117 production `v8::` references inside a crate whose entire premise is that it does not link
V8. `tx_scope.rs` is struck out above because it is not a split at all: all six of its production
functions are V8 context-map manipulation, so the file moves to the adapter whole. The others are
dispatch stacked on engine and must be cut along that line first. `exec.rs` is the control *for V8* -
one `v8::` reference, none in a signature - which is what makes this a real seam rather than a grep
artefact. It is **not** a clean module: it names `compio_postgres` in four signatures and its own
header calls it "the only consumer of `compio_postgres::Pool`". Full measurement and the method that
found it: Phase 0.1 in the execution order.

**And the eight that do not place cleanly.** Each needs an answer before the split, not during it:

| module | lines | why it does not place |
| --- | --- | --- |
| `backend/mod.rs` | 2,201 | **must be split, and the split is now settled - see below.** |
| `error.rs` | 1,703 | **cannot be split cleanly at all, and one of the three reasons is a language rule rather than a placement choice.** (1) `to_op_error` belongs in the adapter but cannot go until the dispatch surface does - 44 of its 51 production callers are engine-tier. (2) It names `compio_postgres` in **eight** signatures (`from_pg` `:355`, `coded_sql` `:731`, `walk_pg_chain` `:907`, five more). (3) **The orphan rule pins five `From` impls here permanently** - see below. |

**`DbError`'s five `From` impls fix `data-core`'s dependency floor by coherence, and the document has
been costing this as if relocation were an option.** Found in round 5 by the reviewer assigned impl
position - the one place no other instrument looked.

```
error.rs:797  impl From<compio_postgres::Error>                    for DbError
error.rs:803  impl From<zeroship_core::database_role::PerAppRoleNameError> for DbError
error.rs:814  impl From<crate::query::QueryError>                   for DbError   <- Track A DELETES this module
error.rs:870  impl From<zeroship_schema::error::SchemaError>        for DbError
error.rs:884  impl From<zeroship_schema::error::MaskSentinelError>  for DbError
```

`impl From<A> for B` is legal only in the crate that owns `A` or `B`. For `:797`, **both sides are
foreign to `data-postgres`** - so unlike `from_pg` and `coded_sql`, that impl **cannot follow the
vendor code down**. It stays wherever `DbError` lives. There are exactly three exits, and all are
design decisions rather than cleanup:

1. **`data-core` keeps the `compio-postgres` dependency.** Then the vendor-neutral contract crate is
   not vendor-neutral, and `data-sqlite` links the Postgres driver - the finding above, made permanent.
2. **Delete the `From`.** `error.rs:794-796` says it exists so callers "gain the ergonomic `?`
   operator". The codebase already overwhelmingly does not use it: **45 production `from_pg` /
   `coded_sql` call sites** (definitions excluded), 35 of them `DbError::from_pg` specifically,
   against a single implicit `e.into()` at `:732`.

   > *The figure above was published as 17 and is corrected here to 45. The review that raised it
   > undercounted; my first re-count said 48 by matching `fn coded_sql(` definitions as if they were
   > calls - **occurrences reported as call sites, in the check written to verify someone else's
   > count.** The correction runs in the argument's favour: 45 explicit sites is stronger evidence
   > that this codebase already prefers the explicit form than 17 was.*
   >
   > **The number that decides this is still unmeasured.** Explicit sites are unaffected by deleting
   > the impl - they are already explicit. The cost is the IMPLICIT users: every `?` propagating a
   > `compio_postgres::Error` into a `DbError`-returning function. That cannot be counted by grep,
   > because `?` names neither type. **Delete the impl and let `rustc` enumerate the breaks** - one
   > `cargo check`, and the list is exhaustive by construction. Deferred only because three read-only
   > reviewers were reading this worktree when it was measured; mutating `error.rs` under them would
   > have voided their verdicts.

   Incidental finding from the recount: **`auth/bootstrap.rs:67` defines its own `coded_sql`**,
   duplicating `error.rs:731` with the same signature. Ten of the 45 calls go to the local copy. Two
   functions, one name, one crate - worth collapsing whether or not the split happens.
3. **Newtype the pg error inside `data-postgres`** and impl `From<Newtype> for DbError` there. Legal,
   and it rewrites every conversion chain.

**The same rule pins the other four**, so `data-core`'s true dependency floor is
`{zeroship-schema, zeroship-core, compio-postgres-or-a-redesign}` - while the target block declares
`data-core -> data-query-builder` **and nothing else**. The declared crate graph is wrong about the
contract crate's dependencies, which is the one row four review rounds were most confident about.

And `:814` is a scheduling trap: `From<crate::query::QueryError>` welds `DbError` to the string
builder **Track A deletes**. That `From` must die *with* Track A, and no Track A step lists it.
| ~~`context.rs`~~ | 1,688 | **RESOLVED by the `BackendHandle` finding.** It holds `Option<BackendHandle>` (`:422`) and constructs both variants (`:561`, `:587`), so it follows the enum UP into `data-engine`. |
| `auth/` | 1,459 | **it is THREE things wearing one name - see below.** |
| `service.rs` | 606 | **it straddles, and its own header proves it.** |
| `cdc_lifecycle.rs` | 523 | bridges V8 subscription leases to one consumer per process. Stays worker-side (established), but adapter or engine is open. |
| `change_stream_pg.rs` | 315 | **orphaned by Full.** It is "the single ownership path for provisioning, starting, stopping and cleaning up a worker's logical-decoding consumer" - and the consumer it owns moves to the relay. |
| `replication_ops.rs` | 47 | a V8 bridge that calls `replication::watchdog_query` (`:33`), which moves. Becomes a cross-process call, or the diagnostic goes away. |

The last two are consequences of the Full decision and did not exist as problems before it. Neither
is large; both are load-bearing, because they are the seam where the worker used to own its own
stream and now must ask another process about it.

#### The module walk is blind to TYPES, and two of them have no destination

The assignment was produced by enumerating modules. That method finds modules; it cannot find a type
DEFINED in one module and NAMED by several, which is a different question and the one that decides
whether a graph compiles. `BackendHandle` was exactly this shape and was caught only because it
happened to share a file with the contract. Enumerated properly, two more:

- **`ChangeOp` must travel with `ChangeEvent`.** The target block sends the event to
  `zeroship-core::change_event` and stops there. But `ChangeEvent` (`broker.rs:81`) has a field
  `pub op: ChangeOp` (`:88`), and `ChangeOp` is a separate enum at `:121`. Naming only the event in
  the target is the kind of omission that compiles nowhere.
- **`BuiltQuery` has no destination, and correctly so.** It is the CURRENT builder's output type
  (`zeroship-schema/src/query.rs:72`, "A built SQL query with text parameters"), so it belongs to the
  thing Track A deletes. It should not be placed; it should be **replaced** by the typed plan plus a
  rendered-SQL type. Any assignment that finds it a crate is preserving the string builder by
  accident.

Other types the enumeration confirms are placed correctly: `broker::Subscription` (the adapter stores
an opaque handle, an ordinary downward dependency), `CdcLease` (same shape), `SqliteSessionHandle`
(vendor crate; the engine names it through its vendor edge), `TypedCell`/`TypedRows` (core, ideally
renamed as neutral row vocabulary, with vendor decoding staying in each vendor), and `MaskKind`
(core, shared vocabulary).

**The lesson generalises past this document.** A file-granular walk answers "where does this file
go"; a crate boundary is decided symbol-by-symbol. Three of this plan's hardest questions -
`BackendHandle`, `error.rs`, `auth/` - were all "one file holds two tiers", and the walk found them
only because each happened to be big enough to notice.

#### `read_set.rs` is production-inert on BOTH ends, so live queries are coarse-grained today

Raised by review, verified independently here, and it is a behavioural fact about shipped code rather
than a placement question.

**Producer.** The only writer of the capture buffer is `Active::begin` (`read_set.rs:396`), and
`Active` is `#[cfg(test)]` (`:386`, `:391`). The one other `borrow_mut` (`:422`) is inside
`#[cfg(test)] impl Drop for Active` - a clear, not a write. So in any production build
`CURRENT_BUFFER` is always `None`, `is_active()` (`:430`) always returns false, and
`record_if_active` never records.

**Consumer.** `Subscription::set_read_set` (`broker.rs:267`) has NO caller outside `broker.rs`'s own
test module - every hit is `:1298`, `:1333`, `:1349`, `:1358`, all inside the `#[cfg(test)]` region.
So `Subscription.read_set` is always `None` in production and the filtering early-return always
fires.

**Therefore every subscriber receives every change for its collection today**, unfiltered by read
set. Whether that is a known trade or an unnoticed gap is not a question this document can answer -
but a plan that assigns `read_set.rs` (659 lines) to a crate is answering the wrong question about
it. It is a **fifth** built-tested-unreferenced cluster, alongside the four this document already
names, and it deserves the same disposition: delete or wire, not relocate.

**One correction it forces on Step 0.** The mask-liveness finding cites `read_set.rs:320` as one of
TWO live consumers proving `MaskKind` survives. That path is production-unreachable, so the citation
is bad. **The conclusion holds** on the other root - `crud/mask_pass.rs:81` and
`crud/write_pipeline.rs:237` are genuinely live - but it rests on one root rather than two, and this
document should not have counted a `cfg(test)`-gated path as evidence of liveness while making
exactly that argument about other modules.

#### A CRATE SPLIT ERASES `pub(crate)`, AND THIS CRATE USES IT AS A SHIPPED FENCE - and it has already been burned by exactly this

Rust has no cross-crate `pub(crate)`. Every `pub(crate)` symbol whose module ends up on the far side
of a cut becomes **unconditionally `pub`**. This crate uses that modifier deliberately as a
production fence: `lib.rs` carries THIRTEEN two-arm pairs of the shape

```rust
#[cfg(not(feature = "test-helpers"))]  pub(crate) mod crud;   // lib.rs:127-128
#[cfg(feature = "test-helpers")]       pub        mod crud;   // lib.rs:129-130
```

plus four modules `pub(crate)` with no widening arm at all (`context`, `v8_bridge`, `descriptor`,
`change_stream_pg`) and one fully private (`cdc_lifecycle`). Named casualties if their modules move:

| symbol | what it is |
| --- | --- |
| `crud/unmask.rs:311` `sanitize_app_actor` | **the DB-3 patch** AGENTS.md calls load-bearing |
| `tx_route.rs:92` `TxRoute::capture` | the ONLY constructor, V8-scoped on purpose; `:45-56` says a dispatcher that forgets it "**fails to compile**" |
| `binding.rs:37-40` `DbBinding::cold_start` | test-gated because "**this gate is what makes that checkable**" (`:30-36`) |
| `context.rs:1173`, `:1179` `with` / `with_mut` | hands out `&mut ThreadDbContext`, i.e. every co-resident tenant's live connection - the SEC-1 fence |

**AND THE PRECEDENT IS IN THE TREE, WITH A FALSE CLAIM ATTACHED.** `plugin-db/src/lib.rs:137-139`
says of the earlier `zeroship-schema` extraction: "The original `pub(crate)` vs `pub` (under
`test-helpers`) visibility **is preserved by the cfg gate**."

It is not. `crates/zeroship-schema/src/lib.rs:78` is `pub mod diff;` - unconditional. The gate
preserves the SPELLING `crate::diff` inside plugin-db; it does nothing about
`zeroship_schema::diff::compute_diff`, which any crate reaches with one dependency line. **The last
time this codebase pushed a module down into a leaf crate, the production fence evaporated and a
comment was written claiming it had not.**

**Prerequisite, not artefact:** before any module moves, produce the list of symbols going
`pub(crate) -> pub` and say for each whether the fence was load-bearing. Three of the four above are
named security controls elsewhere in this repository.

*This also compounds with the cargo-unification measurement above.* Today `test-helpers` gates
visibility inside ONE crate, so a bad unification widens one crate. After the split, `plugin-db`'s
test targets must enable `data-engine/test-helpers`, `data-core/test-helpers`,
`data-sqlite/test-helpers` - and the same unification widens FOUR crates at once.

#### The contract crate would depend on the relay, which this document's own rule forbids

`backend/mod.rs` declares `BrokerPauseGuard` (`:910`) and `SchemaPendingGuard` (`:1318`), and they
are the **return types of two `pub trait ChangeStream` methods** (`:863`, `:873`). Their bodies:

```rust
pub(crate) fn new(app_id: String) -> Self {
    crate::wal_consumer::suppress_app(&app_id);          // :931
impl Drop for BrokerPauseGuard {
    fn drop(&mut self) {
        crate::wal_consumer::unsuppress_app(&self.app_id);      // :940
        crate::broker::resume_app_with_resync(&self.app_id);    // :945
```

So the contract half of `backend/mod.rs` - bound for `data-core` - calls into `wal_consumer`
(`data-cdc-server`) and `broker` (`data-engine`). Against the declared arrows that is
`data-core -> data-cdc-server` and `data-core -> data-engine`: **two cycles.**

And it breaks the target block's own rule. `plugin-db` is declared "NOT -> data-cdc-server; the
worker must not link the relay". That holds on the DIRECT edge and is violated **transitively**,
twice: `plugin-db -> data-engine -> data-cdc-server` (via `exec.rs:485`, `:557`, `:588` and
`cdc_lifecycle.rs:270`) and `plugin-db -> data-engine -> data-core -> data-cdc-server` (via the
guard above).

> **RETRACTED, 2026-08-31, same day it was written. The paragraph above is CORRECT; the correction
> that briefly sat here was wrong, and it is the most instructive error in this document.**
>
> It claimed `exec.rs:485`, `:557` and `:588` were test-only, on the grounds that `exec.rs` "opens its
> `#[cfg(test)]` region at `:370`". It does not. `exec.rs:370` is a `#[cfg(test)]` attribute on a
> single *statement inside a production function*:
>
> ```rust
> if !route.in_tx() {
>     #[cfg(test)]
>     tests::record_sqlite_shared_route();      // <- :370, a test hook in live code
>     return backend.query_json(sql, params).await;
> ```
>
> The real test module is `#[cfg(test)] mod tests {` at **`:670-671`**. All three cited lines are
> **production**, the transitive cycle is real exactly as first written, and both cycles stand.
>
> **The instrument was "first `#[cfg(test)]` in the file", and that is not a test-region boundary.**
> It is wrong wherever a production function carries a test hook, which is common here. Measured
> across the crate, the lines it wrongly discards as test code:
>
> ```
>              first #[cfg(test)]   real `mod tests`   production lines wrongly hidden
> exec.rs             370                 671                    301
> crud/mask_policy    92                  463                    371
> broker.rs           209                1034                    825
> lib.rs              269                (none at all)          1088
> ```
>
> `crud/mask_policy.rs` is the one that bites: hidden inside its 371 discarded lines is
> **`dispatch_set_mask_policy_field` at `:431`, an 18th V8-signature dispatch function**, and a 44th
> engine-tier `to_op_error` call at `:449` - both missed by Phase 0.1's census below.
>
> **Why this is worth the space.** The retracted text was not a guess; it was measured, committed, and
> written up with a rule attached ("a check that costs one `awk`"). The awk ran correctly and answered
> a question adjacent to the one being asked - *where is the first cfg(test) attribute* rather than
> *where does the test module begin*. That is the identical failure this document catalogues in four
> other instruments, committed by the person cataloguing them, in a paragraph congratulating the
> catalogue. **Precision about a boundary is not the same as knowing where the boundary is.**

**"What to build NOW" measured the wrong direction.** It established the relay's OUT-edges - "Zero
`crate::backend`. Zero `crate::encryption`." - and concluded the tier extracts cleanly. It never
measured the worker's IN-edges to `wal_consumer`, which are five live production sites.

#### There are TWO suppression mechanisms, and Track B found only one

This is a direct extension of the dedup-fence blocker, discovered through the cycle above.
`wal_consumer`'s process-global `SUPPRESSED_APPS` has **two** writers, not one:

- `SuppressGuard::activate` (`wal_consumer.rs:781`) - held by the consumer for its whole lifetime.
  This is the one Track B analyses.
- `BrokerPauseGuard` (`backend/mod.rs:930`, `:940`) - taken by orchestrator code AROUND an operation,
  through `ChangeStream::pause_broker`, and it pairs its release with a `Resync` push to every
  active subscription.

**THREE, NOT TWO. This paragraph said "two writers, not one" and was ALSO an undercount** - corrected
within one cycle of correcting it from one to two. The third:

- `cdc_lifecycle.rs:270` - `let startup_suppression = SuppressGuard::activate(app_id)` bracketing
  `spawn_consumer`, dropped immediately after.

And the three are **not independent brackets; they are a protocol with a deliberate overlap**, which
that site states outright (`:266-269`): the snapshot "is emitted only after readiness, so writes in
this startup window are represented by that snapshot. **The consumer installs its own overlapping
guard before it reports ready; the overlap prevents a local-plus-WAL duplicate-delivery gap.**"

| bracket | lifetime |
| --- | --- |
| `wal_consumer.rs:781` `SuppressGuard` in `run_supervised_controlled` | the consumer's whole life, across reconnect backoff |
| `cdc_lifecycle.rs:270` `SuppressGuard` | the startup window, **deliberately overlapping the above** |
| `backend/mod.rs:930-945` `BrokerPauseGuard` | a bracket around an arbitrary operation; its release pushes a `Resync` to every active subscription |

**So the relay handshake must reproduce three bracket shapes AND their overlap invariant** - a
signal protocol that merely says "suppressed / not suppressed" loses the property the overlap exists
to guarantee, and loses it in the direction that duplicates rather than drops.

*Twice now this document has stated a count of suppression writers and been wrong. The first said
one, the second said two. Both were produced by tracing outward from a site I already knew rather
than enumerating the callers of `suppress_app` / `SuppressGuard::activate`.*

#### The last three are one cluster, and they are REWRITTEN rather than relocated

`cdc_lifecycle.rs` (523), `change_stream_pg.rs` (315) and `replication_ops.rs` (47) all exist for one
purpose: **the worker owning its own logical-decoding stream.** Full removes that purpose. So the
question is not which crate they move to; it is what replaces them.

`cdc_lifecycle.rs:1-15` states its job exactly: *"one consumer and one slot per `(app, worker
process)`. Every native Subscription owns a `CdcLease`; the first lease that reaches its readiness
handshake starts the consumer, all other isolates await the same startup result, and the last lease
signals shutdown."* It is a **refcounter over subscriptions**, holding process-wide state behind a
mutex that "protects only counters, state enums, and channel handles".

**That refcounting survives Full unchanged in shape.** A worker still needs to know whether it has
any subscribers for app X - not to start a local consumer any more, but to tell the relay to start
and stop feeding it. First lease opens a relay session, last lease closes it. Same structure, a
different thing being opened.

**And it is the obvious home for the suppression handshake**, which Track B identifies as the blocker
with no owner. `cdc_lifecycle` already knows first-lease and last-lease, and already holds
cross-thread state; "app X is now WAL-fed, stop emitting locally" and "app X is disconnected but its
slot is retaining, KEEP suppressing" are lifecycle transitions of exactly the kind it already
tracks. Putting the suppression signal anywhere else means a second component learning the same
subscription lifecycle.

So:

| module | after Full |
| --- | --- |
| `cdc_lifecycle.rs` | **stays and gains a job.** Becomes the relay-session lifecycle, and owns the suppression handshake. |
| `change_stream_pg.rs` | **replaced.** It is "the single ownership path for provisioning, starting, stopping and cleaning up a worker's logical-decoding consumer" (`:1-4`); after Full the worker has no consumer to own. Its logic is absorbed into the relay client. |
| `replication_ops.rs` | **decide, do not port.** A V8 diagnostic calling `replication::watchdog_query` (`:33`). Either it becomes a relay RPC or the diagnostic goes away; porting it verbatim buys a cross-process call for a debugging aid. |

**THAT CLAIM WAS FALSE WHEN FIRST WRITTEN. IT SAID "every one of the 57,427 lines has a destination
or an explicit decision", AND 782 LINES WERE NEVER MENTIONED** - found by adding up my own tables
rather than waiting for a reviewer to do it:

```
  adapter        5,821      contested      8,681      (was 5,679; +142 tx_scope.rs)
  engine        26,649      ---                       (was 26,791; -142 tx_scope.rs)
  encryption     1,591      accounted     56,645
  postgres       1,477      tree total    57,427
  sqlite         9,485      UNACCOUNTED      782
  cdc-server     2,941
```

**And this table went stale within hours of being held up as the fix for staleness.** When
`tx_scope.rs` moved to the adapter (`:165`), its 142 lines were struck from the placement table and
left in the arithmetic here: `5,679 = 3,455 + 867 + 1,357` counts no `tx_scope`, while
`26,791 = 26,649 + 142` counts it. One module, two inventories, edited independently - **the exact
failure this section exists to record**, recurring inside the correction for it, in the numbers offered
as proof that "totals are checked by adding, not by asserting". The 782 is unaffected; the two rows
above it were wrong for as long as the fix was in place. Corrected 2026-08-31 by a reviewer, not by the
addition.

The 782:

| module | lines | disposition |
| --- | --- | --- |
| `test_support/` | 336 | test scaffolding; follows whichever crate its subject lands in |
| `backend/lock_guard.rs` | 397 | `#[cfg(any(test, feature = "test-helpers"))]` (`backend/mod.rs:72`), so it is in NO production build; it follows `backend/` and needs no production home |
| `binding.rs` | 49 | **a real omission.** It defines `DbBinding` - the identity of the bound database, held by `Collection` (`v8_classes/collection.rs:36`) and named by both backends. Two rounds were spent arguing about this type's NAME and the assignment forgot to place it. |

`DbBinding` belongs in `data-core`: it is the vocabulary every tier uses to say WHICH database, it
carries no behaviour beyond two strings (`binding.rs:13`), and both vendors name it.

**The method failure is worth naming because it is the same one this document keeps finding in
others.** The walk enumerated modules and then wrote up the ones I had an OPINION about. Modules I
had nothing to say about silently left the ledger - and the arithmetic that would have caught it was
a claim I made rather than a sum I computed. **"All N lines are assigned" is a total; totals are
checked by adding, not by asserting.** Same shape as the table that summed to 13,163 under a printed
13,560, one section of this document away.

With those three placed, the enumeration is complete - and the total now reconciles.

#### `auth/` does not place because it is not one thing

Its callers say so. Every reference from outside the module, comment lines stripped:

| caller | reaches for | what that is |
| --- | --- | --- |
| `exec.rs:316` | `bootstrap::autocommit_local_session_setup_sql` | **session setup - hot path** |
| `transaction/mod.rs:219` | `bootstrap::tx_session_setup_sql` | **session setup - hot path** |
| `drop_namespace.rs:167` | `bootstrap::drop_per_app_role` | **role lifecycle** |
| `backend/sqlite/session_minter.rs:51`, `:626` | `util::{hex_decode, hex_encode, format_unix_millis}` | **not auth** |
| `backend/sqlite/mod.rs:1167`, `:1174`, `:1176` | `util::{DEFAULT_TOKEN_TTL_SECS, getrandom_or_fallback, iso_timestamp_after}` | **not auth** |

**`auth/util.rs` is not authentication.** Its whole public surface is
`getrandom_or_fallback`, `iso_timestamp_after`, `format_unix_millis`, `civil_from_days` (a calendar
algorithm), `hex_encode`, `hex_decode` and one TTL constant. Random bytes, date arithmetic and a hex
codec, filed under `auth/` for historical reasons.

**That looked like a live blocker for the SQLite extraction. IT IS TEST-ONLY, AND THIS PARAGRAPH
OVERSTATED IT** - corrected 2026-08-31 by review, verified. It said `data-sqlite` "would need
`hex_encode` and `getrandom_or_fallback`", implying a shipped production edge. Every SQLite caller of
`auth::util` is gated: `sqlite/mod.rs:1143` is `#[cfg(feature = "test-helpers")]` and the calls at
`:1167`, `:1174`, `:1176` sit under it; `session_minter.rs:36` and `:84` are
`#[cfg(any(test, feature = "test-helpers"))]`.

So the real statement is narrower and still worth acting on: **a `data-sqlite` crate's TEST surface
would depend on an auth module to encode hex.** Production builds would not. The helpers still belong
in `data-core` - `auth/util.rs` is random bytes, calendar arithmetic and a hex codec by its own
contents - but this is tidiness plus test-build correctness, not a production dependency inversion.

**And the same shape hides a sharper one.** `backend/sqlite/session_minter.rs:113` names
`PluginDbConsumer`, the ADAPTER-owned declared-env identity (`lib.rs:74`), also test-gated. Under
`--features test-helpers` a future `data-sqlite` would need a dependency on the adapter - the exact
inversion the split exists to prevent - and it would be invisible in a default build. Fix by giving
`data-sqlite` its own test consumer, or by deleting the test-only session-minter surface.

*Why this matters beyond the two modules:* it is the feature-unification hazard again, one layer
down. These edges are absent from the shipped graph and present under `test-helpers` - and this
document has already measured that cargo unifies features across packages in one invocation. A
dependency that only exists under a test feature is still a dependency the workspace has to satisfy.

So `auth/` splits three ways: session setup to `data-engine` (it is on the statement and transaction
paths), the generic helpers to `data-core`, and role lifecycle wherever teardown lands - noting that
#55 found teardown is itself unwired with test-only callers, so that third piece may not need a home
so much as a decision about whether it lives at all.

#### `service.rs` straddles two tiers, and its own header is the evidence

`DbService` is "process-wide ownership of the `env.db` primitive", constructed "at worker / CLI
composition, BEFORE any V8 isolate exists" (`service.rs:1-4`). It then lists the five things it owns,
and they do not belong to one crate:

| owned | tier |
| --- | --- |
| validated configuration - "Backend selection happens ONCE" | engine (it must know the vendors) |
| the plugin prototype | adapter (a `plugin-db` concept) |
| the stable thread-resource key | adapter (isolates and OS threads) |
| the process-wide live-metadata cache | engine |
| the neutral operator-lifecycle handle | engine (owns an operator pool) |

**It is a composition root, and composition roots straddle by nature** - that is what composing is.
The resolution is either to split it along the table above, or to leave it in the adapter and have
the adapter call an engine-side selector. What must NOT happen is assigning it wholesale on the
strength of one of its five jobs; I did exactly that a section ago, writing "it sits wherever backend
selection ends up", and a grep found it names no `BackendHandle` at all.

#### `BackendHandle` goes UP, not down - settled by measurement, 2026-08-31

This was named above as "the first thing to settle" and it took one grep. Inside `backend/mod.rs`,
`BackendHandle` occurs at exactly five places: the enum (`:1475`), its impl (`:1489`), and three
lines inside tests (`:1897`, `:1922`, `:1929`). **None of the file's 13 `pub trait` declarations
mentions it.** The contract is completely independent of the composition enum.

So the split is clean and the arrow is the opposite of the one that worried us:

```
  data-engine     BackendHandle { Postgres(Rc<..>), Sqlite(Rc<..>) }   -> both vendors
       |                                                                  (it composes them)
  data-core       the traits                                           -> NEITHER vendor
       |    \
  data-postgres  data-sqlite    impl the traits
```

The vendor-naming enum belongs in the layer that already depends on both vendors, which is exactly
the layer that dispatches on it. Putting it in core was never necessary; it only looked necessary
because it currently shares a FILE with the contract.

**And the repo already does this, in the family this plan keeps citing as precedent.**
`zeroship-migrate/src/lib.rs:61-65` names all three vendors in `SHIPPING: [&BackendVendor; 3]` - the
facade, at the top - while `zeroship-migrate-backend`, the contract crate, declares no driver at all.
Same shape, one layer up. The closed-enum choice also survives: `backend/mod.rs:1455-1464` records
that the enum exists for monomorphisation on the hot path, and nothing about moving it up changes
that.

**A clarification this document owes the reader.** Track B argues "the broker stays in the worker",
and that is about the PROCESS - V8 subscription objects hold `broker::Subscription` handles directly,
so it cannot move to another machine. **A crate boundary is not a process boundary.** The broker moves
DOWN into `data-engine` and still runs inside the worker process. The two statements are compatible,
and the earlier phrasing could be read as "the broker must stay in `plugin-db`". It must not.

**The five crates below are the DESTINATION. Only three of them should be built now, and the
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

### What to build NOW: three moves, whatever the final count is

*(This section predates the thin-adapter decision and survives it unchanged. Thin-adapter fixes the
DESTINATION; it does not argue that everything moves at once, and the sequencing below is about what
can move FIRST without prerequisites.)*

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
| operator, first proposal | five | as originally proposed |
| architecture | **three** | `data-core`/`-postgres`/`-sqlite` are a destination, not a plan; build the rename, the CDC tier, and the in-place feature gate |
| dependency graph | **six** | splits `data-encryption` OUT of `data-core`, so the core stays comparable to `migrate-backend` and the CDC tier never inherits crypto |
| operator, after the thin-adapter decision | **six** | adds `data-engine`, because a thin `plugin-db` leaves ~23,000 lines of pipeline and reducer with nowhere else to go |

**THE TWO SIXES ARE NOT THE SAME SIX, and that matters when the destination is finally drawn.** The
dependency-graph reviewer's sixth crate is `data-encryption`, split DOWNWARD out of the core. The
operator's sixth is `data-engine`, added UPWARD between the core and the adapter. They solve
different problems and neither subsumes the other, so **taking both arguments yields SEVEN**:

```
  plugin-db          thin adapter
  data-engine        crud, transactions, exec, broker      <- operator's sixth
  data-core          contract, shared vocabulary
  data-encryption    AEAD, keys, AAD, wire framing         <- reviewer's sixth
  data-postgres  data-sqlite
  data-query-builder
  data-cdc-server    separate process
```

Seven is not obviously wrong - the engine's family is nine - but it should be arrived at
deliberately rather than by accepting two independent "make it six" arguments in sequence.

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
  `zeroship_runtime::state::OpError`, `zeroship-runtime` declares `v8` (`Cargo.toml:94`), the
  encryption modules all use `crate::error::DbError`, and `backend/mod.rs` names `DbError` 48 times.
  So `data-core` would link V8, and `data-cdc-server` would inherit it - **the relay whose entire
  justification is that it does not execute creator code would link the V8 runtime.**

  **The remedy is much smaller than that framing suggests - measured 2026-08-31.** The V8 edge is
  carried by EXACTLY ONE METHOD: `DbError::to_op_error` (`error.rs:408`, body at `:421-429`), which
  converts a `DbError` into the V8 op error type. That is a boundary-crossing conversion and belongs
  at the V8 seam by rights, not in a contract crate. **`DbError` itself needs no V8.**

  **The paragraph above is correct about the TYPE and wrong about the FIX; see Phase 0.1 in the
  execution order, which was refuted by attempting it.** `DbError` does indeed need no V8, and
  `error.rs` carries exactly one production runtime edge (`:67`). But moving the method to an
  extension trait in the V8 tier cannot be done first: **43 of its production callers are engine-tier**,
  and an adapter-owned trait is unreachable from below. Those 43 callers all sit inside functions that
  already take a `v8::PinScope` - they are dispatch code mis-filed as engine - so the real prerequisite
  is relocating that dispatch surface, after which this becomes a one-method move. Read this bullet as
  *"the type is clean, the sequencing is not"*, never as a costed remedy.

  So of the three reasons `data-core` cannot hold its proposed contents yet, this one looked like it
  had a cheap and obviously-correct fix, and the appearance survived four review rounds because every
  round re-audited the count instead of the direction. The other two - row-to-JSON being two vendor
  converters, and `encryption`
  carrying `PluginDbConsumer` - do not, and neither does the absent `data-core -> data-query-builder`
  edge.
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
2. **The CDC rows were conditional on Full-vs-Partial. That is SETTLED: FULL**
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

```
  TODAY, one process:

    wal_consumer.rs:781   SuppressGuard::activate(app)
                                |
                                v
                   static SUPPRESSED_APPS          <-- process-global
                        (wal_consumer.rs:72)           Mutex<HashMap<..>>
                                ^
                                | is_app_suppressed(app)?
                                |
    exec.rs:485        mutation path: SKIP the local emit,
                       "the WAL consumer owns the publish path"

    => a change is delivered EXACTLY ONCE, via WAL.


  AFTER A NAIVE SPLIT:

    RELAY process                       WORKER process
    +--------------------+              +----------------------------+
    | SuppressGuard      |              | SUPPRESSED_APPS = EMPTY    |
    |  writes ITS OWN    |              |          ^                 |
    |  process's map     |              |          | returns false   |
    +--------------------+              | exec.rs:485 -> EMITS       |
             |                          |          |                 |
             | ALSO delivers            |          v                 |
             | the same change          |       broker               |
             | over WAL                 |          |                 |
             +------------------------> |          v                 |
                                        |   subscriber sees it TWICE |
                                        +----------------------------+

    no error. no refusal. FAILS OPEN.
```


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

**SETTLED BY THE OPERATOR 2026-08-31: FULL. The worker stops decoding WAL and loses `REPLICATION`.**
This question - does the worker keep decoding its own stream while only the privileged and
destructive parts move - was open through three review rounds. It is now closed, and the two options
are kept below because the reasoning is what makes the four edits non-negotiable.

Measured, and the reason Partial was rejected: `wal_consumer.rs:21-23` opens a `replication=database`
connection and issues `START_REPLICATION SLOT ... LOGICAL`, so REPLICATION is legitimately required
by today's design and moving the reaper ALONE does NOT let `db_posture.rs:123-126` narrow. The
attribute is the capability; relocating code that uses it removes nothing.

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

```
  PARTIAL                                FULL
  move slot_reaper.rs only               move the whole WAL side

  +---------------------+                +---------------------+
  | WORKER              |                | WORKER              |
  |  wal_consumer   *   |                |  broker             |
  |  broker             |                |  cdc_lifecycle      |
  |  REPLICATION    <---+-- still held   |  NOREPLICATION  <---+-- removed
  +---------------------+                +----------+----------+
  +---------------------+                           | wire protocol
  | RELAY               |                +----------v----------+
  |  slot_reaper        |                | RELAY               |
  +---------------------+                |  wal_consumer       |
                                         |  replication        |
  The worker can STILL see every         |  slot_reaper        |
  slot and drop any of them. The         |  REPLICATION        |
  code moved; the CAPABILITY did not.    +---------------------+

  => defence in depth                    => satisfies the invariant
```

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

## Execution order

Everything above is analysis. This section is the only part that says WHAT TO DO FIRST, and it exists
because the prerequisites were discovered across four review rounds and landed wherever they were
found. **Nothing here is new; it is the same findings, ordered by what blocks what.**

**The shape of the answer: almost nothing can be extracted until five in-place refactors land.** Every
one was found by a different lens, and none of them creates a crate.

### Phase 0 - refactors inside today's crate, no new crates, each independently shippable

| # | move | why it blocks things | measured cost |
| --- | --- | --- | --- |
| 0.1 | lift the **V8 dispatch surface** out of `crud/`, `transaction/`, `tx_scope.rs`, `tx_route.rs` and `crud/unmask.rs` | those modules are assigned to a v8-free engine crate and contain **39 production functions whose signatures carry `v8::`** - the whole `env.db` op surface. Nothing under them can move while they hold it | **39 functions / 117 production `v8::` refs**; `tx_scope.rs` moves whole |
| 0.2 | `auth/util.rs` helpers (hex, random, calendar) to a neutral home | a database driver crate would otherwise depend on an `auth` module to encode hex (test-tier today, but test builds must compile) | 7 exports, callers in 2 SQLite files |
| 0.3 | `encryption/` and row-to-JSON below the vendors | `encryption` is 7 edges from EACH backend; row-to-JSON is two vendor converters in one file | `encryption/` 1,591 lines; `keys.rs:318` also carries `PluginDbConsumer` |
| 0.4 | make `PgSqlExecutor` / `PgLockManager` driver-neutral | they name `compio_postgres::OwnedPooledClient` in their BOUNDS, so a contract crate built from them ships a vendor | `backend/mod.rs:755`, `:786` |
| 0.5 | resolve the `v8_bridge` two-way cycle | `v8_bridge.rs:34` imports SQLite types while both backends call back into it | 5 back-edges |

**Then, and only then, the split becomes file moves.** `BackendHandle` goes up with the engine
(settled); `context.rs` follows it.

**0.1 used to say something else, and it was refuted by trying to execute it.** Through four review
rounds this row read: *move `DbError::to_op_error` to an extension trait in the V8 tier, 53 call sites,
10 files.* The premise was that this method is the only thing making `DbError` link V8. On starting the
edit, the first measurement taken - which tier actually CALLS it - killed the whole item:

```
production `.to_op_error()` call sites, by tier
  43   ENGINE      crud/mod.rs, crud/unmask.rs, transaction/mod.rs
   4   ADAPTER     v8_classes/{db,masked_value}.rs
   2   CONTESTED   replication_ops.rs (the V8 diagnostic bridge)
```

An extension trait **cannot serve 43 engine-tier callers**. Put it in the adapter and the engine cannot
reach it - the engine is below the adapter, not above it. Put it in the engine and the engine still
names `OpError`, which is the exact coupling the item existed to remove. There is no third place while
those callers sit where they sit.

**The trait is not the error; its POSITION IN THE ORDER was.** An adapter-owned extension trait is
still the right end state, and it becomes both possible and nearly free - but only once the callers are
adapter-side, which is a move this document listed nowhere. Sequenced first, as written, 0.1 is not
expensive: it is unexecutable. That is a harder failure than a wrong estimate, and only attempting it
surfaced it.

**Why 43 engine-tier callers exist is the real finding.** They are not engine code. `crud/mod.rs:2108`
is representative:

```rust
pub(crate) fn dispatch_search<'s>(
    scope: &mut v8::PinScope<'s, '_>,          // a V8 scope
    binding: DbBinding, collection: &str, args: Value,
) -> v8::Local<'s, v8::Promise>                // a V8 promise
```

That is the runtime's op-dispatch layer, sitting in a module this document assigns wholesale to a
**v8-free engine crate**. Measured across every module assigned outside the adapter tier:

```
  117   production `v8::` references          (comments and #[cfg(test)] excluded)   <- WRONG, see below
   39   functions carrying `v8::` in their SIGNATURE                                 <- 33, see below
        crud/mod.rs         19   run_op, reject_op, and all 17 dispatch_*
        transaction/mod.rs  10   transaction_dispatch, the promise finalizer chain, resolve/reject
        tx_scope.rs          6   ALL SIX production functions - the file is 142 lines of V8 context-map
        crud/unmask.rs       2   dispatch_unmask_field, dispatch_bulk_unmask_field
        tx_route.rs          1   capture
        replication_ops.rs   1   already adapter-tier by nature; stays
   exec.rs holds 1 v8:: ref, none in a signature - it is genuinely engine, and is the control
   that shows this is a real distinction rather than a grep artefact.       <- NOT A VALID CONTROL
```

**That control is void, and it failed in the one way a control must not.** `exec.rs`'s single `v8::`
token is a `//!` doc comment at `:36`; it has **zero** production V8, so it is cleaner than claimed.
But it reaches the adapter twice - `:63 use crate::v8_bridge::rows_to_json_value` and
`:386 crate::v8_bridge::typed_rows_to_json_value(&typed)` - and `v8_bridge.rs` is adapter-tier with 58
`v8::` references. It is V8-free in **spelling** while pointing **upward**.

A control exists to show the instrument discriminates. This one was selected *by* the instrument, on
the instrument's own blind axis, so it could only ever return "clean". **A control chosen by the
measurement it is meant to validate is not independent** - it is the measurement, run twice. The
correct control would have been a module known by other means to be engine-only, then checked.

#### Round 5 corrected every number in that block. Two reviewers, independently, agree.

The census that produced those figures had four defects. Both round-5 reviewers found the first two
without contact; on the headline `v8::` count they independently produced the *same* corrected number.
All four are now fixed in `tests/lib/tier_signature_census.sh`.

| figure as published | corrected | what was wrong |
| --- | --- | --- |
| `117` production `v8::` refs, "comments and `#[cfg(test)]` excluded" | **152** refs / 107 lines comment-stripped | 117 is *lines containing* `v8::` **with comments left in**. It did neither thing the label claims. Both reviewers reached 152 separately. |
| `39` V8-signature functions | **39, different membership** | `replication_ops.rs:15` was counted but is contested and stays; `crud/mask_policy.rs:431` `dispatch_set_mask_policy_field` was missed. Swap them and 39 holds. Of those 39, **6 are `tx_scope.rs`** (moves whole, not split) and so **33 need lifting**. |
| `43 of 43` engine `to_op_error` callers | **44 of 44** | `crud/mask_policy.rs:449` was hidden by the test-boundary defect. The claim survives *strengthened*: all 44 sit inside the corrected 39. |
| `49` signature violations | **80** under the repaired instrument | 49 was right by CANCELLATION - an under-reading collector and a stale tier map erring in opposite directions. See below. |
| `exec.rs` is "the control" | **not a valid control** | Its sole `v8::` token is a doc comment at `:36`; it has zero production V8. But `:63` and `:386` import from `crate::v8_bridge`, which is adapter-tier and holds 58 `v8::` refs. It is V8-free in *spelling* while pointing upward. A control chosen by the instrument under test is not independent. |

**The four instrument defects, because each printed a plausible number:**

1. **The test boundary was "the first `#[cfg(test)]` in the file"** - which is usually a test hook
   inside a live function, not the test module. It discarded ~2,995 production lines across 8 files
   and caused the retracted false correction earlier in this document.
2. **Single-line signatures were never checked** - the collector's `fn` rule ended in `next`.
3. **The last function in a file was never checked** - no flush at EOF. (2 and 3 made `tx_scope.rs`
   report 4 while this document's prose correctly said 6, both printed in the same section.)
4. **The runtime marker was blind to ordinary Rust.** It matched only `zeroship_runtime::`, but every
   runtime type here is imported unqualified. It therefore missed `pub fn to_op_error(self) -> OpError`
   at `error.rs:408` - **the single edge this entire investigation started from.**

**And the position no instrument had: what a module hands UPWARD.** Four markers asked "what foreign
crate does this module NAME?"; none asked "what tier does it REACH INTO?". Adding `crate::v8_bridge` /
`crate::v8_classes` as a marker finds **17 upward references in 7 modules**:

```
ENGINE   crud/unmask.rs 4, crud/mod.rs 2, crud/mask_policy.rs 2, exec.rs 2, transaction/mod.rs 2
SQLITE   backend/sqlite/mod.rs 3      <- :232, :1440, :1571  v8_bridge::typed_rows_to_json_value
PG       backend/postgres.rs   2      <- :476, :572          v8_bridge::rows_to_json_value
```

**Both backends call up into the adapter to convert rows to JSON.** That is `data-postgres -> plugin-db`
and `data-sqlite -> plugin-db` while `plugin-db` depends on both: a cycle, not a layering violation.
It is the row-to-JSON finding (Phase 0.3) seen from the side that makes it structural. The engine rows
include `crud/mod.rs:554`, where an **engine** function names an **adapter** function pointer whose type
is `fn(&mut v8::PinScope, v8::Local<Value>) -> Option<v8::Local<Value>>` - behaviour crossing a boundary
while spelling no marker at all. It sits on the masked read path, so masking structurally requires
engine-to-adapter today.

**Two more field-position violations, in a module this document twice called settled:** `context.rs:80`
and `:178` hold Postgres in enum/struct fields, and `transaction/cancel.rs:89`/`:90` hold
`Rc<Pool>` and `CancelToken`. See the flag at `:147`.

**Still uncounted, and stated rather than silently missed:** `impl` headers.
`error.rs:797 impl From<compio_postgres::Error> for DbError` is a real vendor edge in a CORE-tier
module, and no `fn` line starts it. Both reviewers found it; the collector still cannot.

**Clean, and worth recording as clean:** proc-macro expansion. All six `#[v8_class]` invocations are in
`v8_classes/` (adapter). The macro generates 12 V8-bearing struct fields no source census can see, and
every one is correctly adapter-owned. The brief's biggest suspected blind spot is empty. Type aliases,
const/static types, `impl LocalTrait for ForeignType`, and `where` clauses past the collector's window
are all **0**.

#### Is 0.1 a MOVE or a REWRITE? It is a rewrite, and the seam is not where the census points

This is the question the whole item turns on, and the answer is not the comfortable one.

**The seam is real.** Every `dispatch_*` has the same shape: a short synchronous V8 prologue, then an
`async move` block pushed onto `spawned_ops`. Measured by tracking brace depth from each `async move`:

```
crud/mod.rs         21 async blocks    0 v8/scope references inside them
crud/unmask.rs       2 async blocks    0
transaction/mod.rs   3 async blocks    2   <- both at :407-408, a single site
tx_route.rs / exec.rs                  no async blocks
```

The `async move` future is `'static`, so it **cannot** hold a `v8::PinScope`. The compiler enforces the
prologue/body split; it is not a convention. `crud/mod.rs:966` says so in a comment: the routing
decision is "frozen HERE, while `scope` is live".

**And the split still does not deliver a V8-free engine.** After cutting at `spawned_ops.push`, the
engine half's signature is `async fn op_X(resolver: v8::Global<v8::PromiseResolver>, request_id, ...)
-> OpResult`. `v8::Global` is a rooted, scope-free handle - the very thing this document blesses at
`:219-221` as an ordinary downward dependency - but `data-engine` still *declares* `v8` and
`zeroship-runtime`, which is exactly what 0.1 exists to prevent. **A clean cut in the wrong place
completes the move and leaves the dependency.**

So the residue is a **protocol inversion**: the engine returns `Result<DomainValue, DbError>` and the
adapter maps it to `ResolveValue`. That is 47 early-return sites reshaped (43 in `crud/mod.rs`, 4 in
`crud/unmask.rs`) across ~987 async-body lines - mechanical and type-directed. Two constructions
currently run the wrong way and do not survive it:

- **`transaction/mod.rs:407`** builds `ResolveValue::Continuation(Box::new(move |scope, state| ...))` -
  a `Box<dyn FnOnce(&mut v8::PinScope, &SharedState)>`. The engine is *authoring V8 behaviour*, not
  passing a handle down. The transaction begin re-enters V8 to call the creator's callback; the module
  header at `:33` documents it.
- **`crud/mod.rs:554`** stores `transform: crate::v8_classes::masked_value::rehydrate_masked_values`,
  an adapter function pointer of V8 type, on the masked read path.

Neither is plumbing. Both are the engine handing behaviour upward, and both need a design answer -
most likely "the engine returns a description of what to continue with, the adapter builds the closure."

**What this changes about the plan.** 0.1 is not "lift 33 functions", and it is not independently
shippable in the strong sense - it rests on 0.3 (row-to-JSON below the vendors), because `exec.rs` and
both backends reach `v8_bridge` for row conversion. **Sequence 0.3 before 0.1.** The honest description
of 0.1 is *invert the op-completion protocol across 18 dispatch functions plus `run_op`*, and no figure
in this document prices that. It is still worth doing first, and it is still worth doing if the split
is cancelled - but it is a redesign of how `env.db` returns results, not a file move.

`run_op` (`:138`) and `reject_op` (`:236`) are the two of the 39 that take only `v8::Global`, never a
`PinScope`. They are the shape the rest should be converted TO, not more work to be done.

**And the original item then collapses into a rounding error.** Every engine-tier `to_op_error` call
site - **44 of 44, no exceptions** (43 as first counted, plus `crud/mask_policy.rs:449`, which the
census's broken test boundary hid) - sits inside one of those V8-signature functions. `error.rs`
carries exactly one production runtime edge to begin with (`:67`), which is why the item looked cheap
from the type side and was never checked from the caller side.

> **THE SENTENCE THAT USED TO FOLLOW WAS THE LOAD-BEARING ONE, AND IT WAS WRONG.** It read:
> *"Relocate the dispatch surface and the calls travel with it, landing adapter-side where the
> extension trait can legally serve them. What remains of 0.1 afterwards is relocating one method and
> adding one `use`."*
>
> Classified by position - **re-measured here rather than taken from the review that raised it, and
> the reviewer's figure was off by two:**
>
> ```
>                        async future    sync prologue
>   crud/mod.rs               28              6
>   crud/unmask.rs             4              0
>   crud/mask_policy.rs        1              0
>   transaction/mod.rs         1              4
>                        ----------------------------
>                             34             10   = 44
> ```
>
> **34 of the 44 sit inside the `async move` future**, not the V8 prologue. The review reported 36 of
> 43, counting `crud/mod.rs:2160`, `:2179` and `:2222` as async; they are not. Each is
> `let op_err: OpError = err.to_op_error();` on the line *immediately before* its
> `spawned_ops.push(...)` - the value is constructed synchronously and then moved into the future. They
> read as "inside the dispatch" and are lexically outside it. Of the 10 sync sites, 9 are in
> `PinScope`-bearing prologue code and the 10th, `crud/mod.rs:243`, is inside `reject_op`, a helper the
> futures call.
>
> **The conclusion is unchanged and slightly strengthened by the correction.** The split of interest is
> not 36/7 or 34/10 in particular - it is that the large majority live in the half that is supposed to
> become the engine.
>
> **So "the calls travel with it" depends entirely on where you cut.** Cut at the `v8::` signature -
> where the census points - and 36 of 43 stay engine-side, still needing `OpError`, and the extension
> trait is still unreachable from below. The only cut that carries them is moving the whole function
> body, which moves `crud/mod.rs:617-2479` into the adapter and leaves "engine" meaning the sibling
> modules.
>
> The refutation of 0.1 was itself refuted on its own remedy. See the move-or-rewrite section above:
> the answer is a protocol inversion, and "one method and one `use`" is not the residue of anything.

**Two instrument failures produced this, and both are worth naming.** First, the check that was run
instead of this one: *"if production relay code called `to_op_error`, an adapter-owned trait would drag
V8 into the relay."* That check passed, honestly and irrelevantly - the relay does not call it. It
examined the boundary the author was worried about rather than the boundary carrying the traffic, and
passing it was taken as clearance to proceed. Second, the grep that was supposed to find exactly this
was keyed to `->` and `Result<`, so it was **blind to parameter position** and reported `crud/` as
having no signature-level `OpError` at all. *A guard finds what it is keyed to; ask what it is keyed to
before believing what it did not find.*

**Fourth time in this document that a measurement has been reported as something it was not** - the
others being the SQLite census, the 782 unassigned lines, and 0.1's own earlier count. The first three
were counting errors. This one was a wrong remedy that four review rounds did not catch, because every
round audited the number and none re-derived the premise. The rule stands and gains a clause: *a grep
counts text; a cost needs the thing the text refers to - and a remedy needs the direction the
dependency actually runs.*

**Consequence for the assignment table above:** the `data-engine` row is wrong as printed. `crud/`,
`transaction/`, `tx_scope.rs`, `tx_route.rs` and `crud/unmask.rs` are not wholly engine; each is a
dispatch layer stacked on an engine layer, and the line between them is the 39 functions above.
`tx_scope.rs` is not a split at all - all six of its production functions are V8, so it moves to the
adapter whole.

#### The audit this should have been, run for every tier

The V8 finding was luck: it fell out of trying to execute an unrelated item. The instrument that
should have caught it is cheap and general - **for each module, does any function SIGNATURE name a
crate the module's assigned tier is forbidden to depend on?** Run across all four marker crates
(`v8`, `zeroship_runtime`, `compio_postgres`, `rusqlite`), comments stripped, `#[cfg(test)]` regions
excluded:

The table this section first printed is kept below **struck through**, because what it got wrong is
more useful than what it got right. Every figure in it is superseded by the repaired instrument:

```
SUPERSEDED - do not plan against this
TIER     FILE                       MARKER             SIGS      corrected
ENGINE   crud/mod.rs                v8                   19      19
ENGINE   transaction/mod.rs         v8                   10      10
ENGINE   tx_scope.rs                v8                    4      6, and it is ADAPTER - not reported
ENGINE   crud/unmask.rs             v8                    2      2
ENGINE   tx_route.rs                v8                    1      1
ENGINE   transaction/mod.rs         zeroship_runtime      1      3
CORE     error.rs                   compio_postgres       6      7   (+ impl header at :797, uncounted)
ENGINE   exec.rs                    compio_postgres       4      5
ENGINE   transaction/mod.rs         compio_postgres       1      1
ENGINE   transaction/driver.rs      compio_postgres       1      1
                                                  total   49      80  (see the corrected run below)
```

Current, from `tests/lib/tier_signature_census.sh` at `e81ff8783`:

```
ENGINE   crud/mod.rs           v8 19, zeroship_runtime 7, upward 2
ENGINE   transaction/mod.rs    v8 10, zeroship_runtime 3, compio_postgres 1, upward 2
ENGINE   crud/unmask.rs        v8 2,  upward 4
ENGINE   crud/mask_policy.rs   v8 1,  upward 2
ENGINE   exec.rs               compio_postgres 5, upward 2
ENGINE   transaction/driver.rs compio_postgres 1
ENGINE   crud/system_fields_pass.rs   zeroship_runtime 1
ENGINE   auth/bootstrap.rs     compio_postgres 1
CORE     error.rs              compio_postgres 7, zeroship_runtime 1   <- to_op_error itself
ADAPTER  v8_bridge.rs          compio_postgres 3                        <- names BOTH drivers
SQLITE   backend/sqlite/mod.rs upward 3
PG       backend/postgres.rs   upward 2
                                                            total 80
```

**Two of these are new findings that have nothing to do with V8, and four review rounds did not
surface either.**

- **`error.rs` names `compio_postgres` in six signatures** - `from_pg(e: &compio_postgres::Error)`
  at `:355`, `coded_sql(..., e: compio_postgres::Error)` at `:731`, and four more taking
  `&compio_postgres::Error` or `&SqlState`. `error.rs` is assigned to `data-core`, the crate whose
  entire premise is vendor neutrality and which every other crate depends on. As assigned,
  **`data-sqlite` - 9,485 lines with no Postgres in it - would link the Postgres driver**, because the
  shared error type classifies PG SQLSTATEs. The document searched for `from_pg` zero times before
  this; the "reasons `error.rs` does not place cleanly" list named only the V8 edge.
- **`exec.rs` names `compio_postgres` in four signatures and says so in its own header** - "the only
  consumer of `compio_postgres::Pool`" - returning `Vec<compio_postgres::Row>` from three functions.
  It is assigned to `data-engine`.

**Together with the V8 result this is the measured form of the "junk drawer" objection.** `data-engine`
as specified would link the V8 runtime *and* the Postgres driver. A crate that links both is not an
engine layer; it is the present plugin with a few files removed and a new name. That criticism was
raised in review as a judgement about cohesion and was answerable either way. It is now a fact about
what the crate would compile against, and it is not answerable by argument.

**The instrument itself needed one correction, which is the reason to report it rather than just its
output.** The first run flagged `backend/sqlite/mod.rs` for naming `compio_postgres` - the SQLite
backend importing the Postgres driver, easily the most alarming line in the table. All three hits were
**doc comments** describing a `Client = compio_postgres::Client` pin: the multi-line signature
collector absorbed continuation lines without stripping comments. Fixing it dropped the total from 51
to 49 and removed the one finding that would have been briefed first. *An audit built to catch a
class of error is not exempt from that class* - this one reported occurrences as signatures, which is
the same failure it exists to find.

#### The test tail, which the census excluded on purpose and should not have

The census skips `#[cfg(test)]` regions. That exclusion is defensible for shipped-code questions and
wrong for this one, because **a test build must compile** - and this document already argues exactly
that in Phase 0.2, where `auth/util.rs` is moved on the grounds that it is "test-tier today, but test
builds must compile". Applying the same standard to the test regions
(`tier_signature_census.sh --tests`) finds **39 more marker references** in modules whose crate would
forbid them:

```
CORE     error.rs             zeroship_runtime   16      <- all 16 are OpErrorKind matches
CORE     error.rs             compio_postgres     6
ENGINE   tx_route.rs          v8                  4
ENGINE   read_set.rs          zeroship_runtime    4
ENGINE   exec.rs              compio_postgres     3
ENGINE   crud/mask_policy.rs  v8 + runtime        3
ENGINE   transaction/mod.rs   zeroship_runtime    1
CDC      replication.rs       zeroship_runtime    1
                                          total  39
```

**The `error.rs` row lands directly on Phase 0.1 and nothing in this document accounts for it.** All
16 of its `zeroship_runtime` references are `OpErrorKind::CodedError` pattern matches, sitting beside
**17 `to_op_error` mentions** in the same region: they are the tests *for the one method 0.1
relocates*. Move the method and leave them, and `data-core` still carries a dev-dependency on the V8
runtime - the item completes and the edge it exists to cut survives, which is the most expensive
possible outcome because it looks like success. The tests move too, or 0.1 is not done.

**And this generalises past 0.1.** Every move in Phase 0 has a test tail, and none of the five items
costs one. That is not a reason to re-estimate them now - it is a reason to *count the tests* when
each is actually planned, and to treat "the lib compiles without the dependency" as a partial result
rather than the finish line.

**A test-only edge is harmless and binding at the same time, and the two must be said separately.**
Cargo permits dev-dependency cycles, and `cargo build --release --bins` never activates
dev-dependencies - so for the **shipped trust boundary** ("the worker must not link the relay") a
test-only edge proves nothing and costs nothing. For the **workspace build graph** it binds fully: the
crate must still declare the dependency, and `cargo test -p <crate>` links it. This document argues
both, correctly, in two places - Phase 0.2 moves `auth/util.rs` on the second ground, and the retracted
cycle correction leaned on the first. Whenever either is invoked, say which question is being answered,
or the two read as a contradiction and one of them gets "fixed".

### Phase 0.5 - three audits that must precede ANY crate boundary

- **The `pub(crate)` audit.** List every symbol that would go `pub(crate) -> pub`, and say for each
  whether the fence was load-bearing. Four are named security controls (`sanitize_app_actor`,
  `TxRoute::capture`, `DbBinding::cold_start`, `context::with_mut`). **This repository has already
  shipped this mistake once** and written a comment claiming it had not.
- **The dead-code decision.** ~1,000 lines are self-declared unreachable (`cross_app_fk.rs`,
  `drop_namespace.rs`, `crud/mask_backfill.rs`), plus `read_set.rs` (659) which is inert on both
  ends. **Giving dead code a crate is how the existing clusters got there.** Decide delete-or-wire
  BEFORE assigning, not after.
- **The tier-signature audit** - `tests/lib/tier_signature_census.sh`, added 2026-08-31, 49 violations
  on its first run (see Phase 0.1). For each module, does any function signature name a crate its
  assigned tier may not depend on? This is the check that catches what a module walk and a type walk
  both miss, and it is the one that should have caught 0.1 before four rounds reviewed it. It is a
  census, not a gate: it reports and does not rule, and it is deliberately not named `*_gate.sh` so
  `tests/gate_arm_census.sh` does not adopt it. It becomes a gate - with arms and floors - the moment
  the first crate boundary exists, so that "which crate may link V8" stops being a claim in a document
  and becomes something CI rules on. **Its `tier()` map is a copy of the assignment table above, so
  re-drawing any boundary invalidates every verdict it prints**; change both in the same commit or it
  reports on a shape nobody proposed.

### Phase 1 - the two things that need no new prerequisites

- **Step 0's deletion**, once its own prerequisite lands: migrate the 12 live security tests off the
  dead DDL builders and onto the engine's renderer (already reachable - `plugin-db` dev-depends on
  `migrate-server`, which pulls the facade).
- **`data-cdc-server`**, which needs neither `data-core` nor `data-postgres` (measured: zero
  `crate::backend`, zero `crate::encryption`). Its cost is the four Full edits, of which the
  suppression handshake is the hard one - **three brackets with an overlap invariant.**

### Phase 2 - the crates

`data-query-builder` (rename only), then `data-core`, `data-postgres`, `data-sqlite`, `data-engine`,
with `plugin-db` reduced to the adapter. **The count is not settled** - see the three-answers table
and the `data-engine` disagreement.

### What Phase 0 costs, and why it is the honest headline

Five refactors, no new crates, and **every one of them improves the current tree on its own terms** -
a query engine that does not link V8, contract traits that name no vendor, crypto that both backends
share from below rather than beside. **If the crate split were cancelled tomorrow, Phase 0 would still
be worth having.** That is the test a prerequisite should pass, and it is why this ordering is safe to
start before the count is decided.

**This section said "no visible architectural change" until 0.1 was measured, and that is no longer
true.** 0.1 is now the largest item in the phase, not the smallest: separating 39 dispatch functions
from the query engine across five modules is a visible, reviewable change to how `env.db` is
structured, and it should be planned as one. The claim it replaces - one method and ten `use` lines -
was the reason the phase read as cheap. **Phase 0 is still worth doing first and is still individually
shippable; it is not small.** Anyone sizing this work should take the 39 functions as the headline and
treat 0.2 through 0.5 as the tail.

The independent-value test survives the correction, and 0.1 arguably passes it hardest: a `dispatch_*`
layer that owns V8 promise plumbing, sitting in the same module as the query builder it calls, is worth
separating whether or not a single crate is ever created.

## Not decided

- **`zeroship-migrate-server`** is a service HOST, not engine. Left in the `migrate-*` family for
  now; a `-service` suffix is arguable.
- ~~**The `plugin-*` family breaks.**~~ **CLOSED - it does not.** This entry asked whether to accept
  an asymmetry or rename four crates, because db (57,427) was leaving while kv (3,019), storage
  (3,808) and workflow (10,847) stayed. Since `plugin-db` keeps its name, `AGENTS.md`'s "Adding a
  native primitive" table stays true and there is no asymmetry to declare. *The entry also said db is
  "15x kv"; 57,427/3,019 is 19.0. Fifteen is the db/storage ratio - two ratios, one sentence.*
- ~~**Whether the worker keeps decoding WAL.**~~ **SETTLED 2026-08-31: FULL.** The worker stops
  decoding and its role becomes `NOREPLICATION`. Every "moves to the relay" row in Track B's table is
  therefore unconditional now, and the four edits Full requires are committed work, not options.
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
placement table is the inventory, and the target block wins nothing.** Diagnosing every defect as a thinking error is its own bias - it
implies the remedy is to think harder, when the remedy here was to keep one list.

The practical rule: grep answers spelling, the compiler answers "is this named", and neither answers
"does production reach this". Only walking outward from a real entry point does. Every correction
above arrived when someone walked that path - which is the argument for doing it BEFORE writing the
claim, not after review returns.
