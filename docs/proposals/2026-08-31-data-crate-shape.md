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
   hardcode `dialect: "postgres"` (`sdks/vite-plugin/src/gen-types/index.ts:271`, `:326`), and every
   `mysql` occurrence in `crates/zeroship-migrate-node/src/` is a doc comment, never a dispatch. The
   dialect string resolves through a *registry* - `preview_dialect` searches `shipping_backends()`
   and returns `Err("unknown dialect …")` at `crates/zeroship-migrate-node/src/verbs.rs:105` - so a
   default-off `mysql` feature degrades to a runtime rejection, not a compile error, and
   `--all-features` (what `clippy_gate.sh` lints under) still covers the code.

   **The cost is two hardcoded counts, and it lands in the one file designed to name vendors once.**
   `crates/zeroship-migrate/src/lib.rs:65` is `static SHIPPING: [&BackendVendor; 3]` - a fixed-size
   array, so the length becomes conditional (or the const becomes a slice), and
   `tests/dialect_matrix/vendor_registry_owns_shipping_descriptors.rs:15,:21` hardcode
   `REGISTERED_VENDOR_FLOOR = 3` and `-> [&'static BackendVendor; 3]`. Both must become
   feature-conditional or a default-feature build fails to compile that test.

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

zeroship-query-plan             the typed query grammar. ZERO dependencies, and that stays
                                load-bearing. (today's zeroship-data-plan, renamed)
zeroship-plugin-db              env.db surface, worker tier. impl NativePlugin. KEEPS ITS NAME.
zeroship-db-relay               service tier: WAL stream, publication and slot authority, reaper.
                                Peer of zeroship-workflow-scheduler, named the same way.

zeroship-core::change_event     the cross-process event type, beside usage_event and
                                replication_names, which are already there.

zeroship-migrate-*              the engine, dialect-complete, untouched
zeroship-migrate-server         the migration service host
```

**This block names crates. It does NOT enumerate modules** - that is Track B's table, and holding the
inventory in two places is what produced two contradictions in two rounds.

**The `zeroship-data-*` prefix is DROPPED - corrected 2026-08-31 by review.** Once `plugin-db` kept
its name, the two remaining `data-*` members shared no production dependency edge, no type, no trait
and no process. Measured: across `wal_consumer.rs`, `replication.rs` and `slot_reaper.rs` there is
exactly ONE reference to any query-building symbol, `replication.rs:674`, and it is inside the
`#[cfg(test)]` module opened at `:598`. Zero references to `DbPlan` in any of the three.

Every other prefix in this workspace denotes something checkable. `plugin-*` means "implements
`NativePlugin` (`zeroship-runtime/src/core/plugin.rs:36`) and is composed at
`zeroship-worker/src/cache.rs:246`" - you can ask and get a yes. `migrate-*` names a
dependency-closed stack: every member declares `zeroship-migrate-ir`. Ask the same of `data-*` and
there is no predicate to ask. A prefix worn by two of roughly ten members of the data system, with
the four largest excluded by this very plan, tells a reader "these two are related to each other" -
the one thing that is false.

**Two names follow from that, and both come from precedent already in the tree:**

- **`zeroship-db-relay`, not `zeroship-data-cdc`.** The repo has spelled worker-tier plus service-tier
  of one domain twice already: `zeroship-plugin-workflow` beside `zeroship-workflow-scheduler`
  (whose `src/lib.rs:1-10` describes "the standalone process is a deferred extraction target" - the
  same situation Track B is in), and `zeroship-migrate-server`. `data-cdc` invents a third axis and
  reads like a library when the thing is a process tier - exactly the distinction the "privilege
  follows the PROCESS" invariant turns on.
- **`zeroship-query-plan`, not `zeroship-data-query-ir`.** `-ir` is already taken in this workspace
  and means the OPPOSITE: `zeroship-migrate-ir` is "the zeroship-migrate **wire contract**"
  (`Cargo.toml:6`), whose `MigrationIr` derives `Serialize, Deserialize, JsonSchema` and whose first
  dependency is serde. The data-plane crate is defined by forbidding exactly that. Naming it `-ir`
  hands a reader that expectation and then bans it. "Plan" is the crate's own word (`DbPlan`), it
  matches the repo's shape for shared leaves (`zeroship-core`, `zeroship-bundle`, `zeroship-schema`),
  and it is a smaller rename than the one this plan proposed.

`zeroship-schema` still DISSOLVES: its live query building is replaced by the IR rather than moved,
its `MaskKind` and sentinel codec go to `plugin-db`, and its dead regions are deleted (below).

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
(`v8_classes/collection.rs:32`) and mints its change stream through it (`:565`). A change stream is
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

**The dev-only backend is 6.4x the production one, and the production worker compiles all of it.**
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
runtime refusal for a backend it has no reason to contain. Gating it removes 9,485 lines from the
production binary and from the attack surface of the process that executes creator code, and it
strengthens exactly the guard that already exists rather than duplicating it.

### The split is right, and it is blocked by a measurable cycle

The target is the engine's own shape - `zeroship-migrate-backend` is a contract that
`-postgres`/`-sqlite`/`-mysql` implement without depending on the engine or each other. The data
plane should match it: `zeroship-data-backend` + `zeroship-data-postgres` + `zeroship-data-sqlite`.

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

**The cheap win does not wait for any of it.** Feature-gating the SQLite backend inside today's
`plugin-db` is a much smaller change than extraction and delivers the whole production-binary
benefit. Do that first; it is also the forcing function that will surface every place the dev backend
is reachable from a production path.

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
  `zeroship-data-plan` and `zeroship-plugin-db`. The engine carries parallel copies of the same
  code. So the `data-*` family has no cross-family edge.
- **`zeroship-data-plan` has zero production consumers.** It is a `[dev-dependencies]` entry in both
  dependants, and the `zeroship-schema` uses of it sit after `#[cfg(test)]` at `query.rs:6446`.
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

So the survivor set is `MaskKind` alone, and the 407-line `mask_codec.rs` needs the same
production-root test before it is carried across rather than deleted. Cutting on the region boundary
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

Delete the tests that reach the dead code in the same change. They are the only thing making it look
alive, and keeping them is how the next reader concludes it still runs.

**This qualifies #1 (decision 10, "remove all DDL from plugin-db").** The DDL left plugin-db's own
source but stayed reachable through `zeroship-schema`, which plugin-db depends on and re-exports as
`crate::query` (`lib.rs:94`). Nothing called it, so nothing failed.

## Track A - one query builder

Merge `zeroship-schema` + `zeroship-data-plan` into `zeroship-data-query`, then replace the string
builder from inside one crate.

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
purge paths call them with no mandatory bound (`crud/mod.rs:1372`, `:1617`). The typed `Update` and
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

**A3 (writes) is gated on #45.** The `RETURNING` projection is exactly where column grants and the
unmask primitive meet. Porting writes before #45 is decided means building against a contract that
may be deleted. Reads carry no such entanglement.

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

So the seam is one module lower. **THIS TABLE IS THE ONE MODULE INVENTORY. The target block at the
top of this document does not enumerate modules, by rule** - see the correction below.

| stays in the worker | moves to the relay |
| --- | --- |
| `broker.rs` - the subscriber registry V8 holds handles into | `wal_consumer.rs` - decodes pgoutput |
| `cdc_lifecycle.rs` - bridges V8 subscription leases to one consumer per worker process (`:1`, `:69`) | `replication.rs` - slot and publication lifecycle, **rewritten not moved** (below) |
| `replication_ops.rs` - V8 bridge for replication diagnostics; calls `replication::watchdog_query` (`:32`), which moves | `slot_reaper.rs` - the privileged, destructive part |
| `change_stream_pg.rs` - "the single ownership path for provisioning, starting, stopping and cleaning up a worker's logical-decoding consumer" (`:1-4`); `backend/mod.rs` names it 7 times | |

**Corrected 2026-08-31 by review. Three defects in the previous version of this table:**

- **`cdc_lifecycle.rs` was listed as moving while the target block said it stays.** Both sentences
  were mine. The tree settles it against the table: it is worker-side, and the V8 wrapper owns the
  lease it hands out (`subscription.rs:42`).
- **`change_stream_pg.rs` (315 lines) and `replication_ops.rs` (47 lines) were unassigned entirely**,
  and the boundary forces both. Moving `wal_consumer.rs` orphans `cdc_lifecycle`'s handle type
  (`cdc_lifecycle.rs:22` imports `WalConsumerHandle` from `change_stream_pg`); moving `replication.rs`
  orphans a V8 dispatch. Both become new cross-process calls this plan had not priced.
- **`replication.rs` cannot be extracted, only rewritten.** It is keyed per app per worker (`:1`,
  `:96`), while the settled target is one slot and publication per Datastore with relay fan-out
  (`2026-08-28-app-database-decoupling.md:955`). Moving the file would move the wrong data model.

Extract the right-hand column into the relay crate, hosted by a process that never executes creator
code - plausibly the relay that #5 builds, which would make the extraction its prerequisite rather
than a separate job.

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
- **Full** - the whole WAL side moves and the worker loses REPLICATION outright, satisfying the
  invariant rather than approximating it.

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

## The rename

`zeroship-plugin-db` -> `zeroship-data-binding`. Re-measured 2026-08-31 with the pattern stated, so
the next reader can reproduce it rather than inherit it:

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

**Ride it with Track B's extraction, never alone.** Renaming first means 1,520 edits to a crate you
are about to reshape.

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
