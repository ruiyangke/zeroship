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

And one shape converged in discussion: a `zeroship-data-*` family, with `zeroship-schema` and
`zeroship-data-plan` merged, and `zeroship-plugin-db` renamed.

## Target

```
libs/compio-postgres            driver, unchanged

zeroship-data-query             query IR, rendering, validators, naming
zeroship-data-binding           env.db surface: v8, crud, transaction, encryption, backends,
                                mask vocabulary, sentinel codec
zeroship-data-cdc               broker, wal_consumer, replication, cdc_lifecycle, slot_reaper

zeroship-migrate-*              the engine, dialect-complete, untouched
zeroship-migrate-server         the migration service host
```

`zeroship-schema` DISSOLVES rather than leaving a stub: its query building merges into
`data-query`, its mask vocabulary and sentinel codec into `data-binding`, and its dead regions are
deleted (below). Dependencies run `binding -> query`, acyclic.

**Why `binding` is the right word despite being the most overloaded term in this project** (the
proposal is *runtime-db-binding*; `binding.rs` defines `DbBinding`; #46 was about *binding
injection*): the crate's job IS "the bound database, as the app sees it". `DbBinding` is its core
concept and crud/transaction/encryption are what you do THROUGH a binding. That reading only holds
because CDC leaves - change streams are not something an app gets through its binding. **The rename
and the extraction are one change; taking the rename alone leaves the name lying.**

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
`def_to_column_type_for_dialect`. `diff.rs` holds five LIVE mask types (`MaskKind`, `MaskMeta`,
`EncryptionMeta`, `WrappedType`, `Classification`) inside a dead differ. Cutting on the boundary
breaks the read path twice.

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
soft-delete, HAVING, WHERE) and **35 `build_*` call sites** in the data plane - not 13,814. Of the
127 apparent `crate::query::` references, most are naming helpers: `quote_ident` (25),
`raw_column_name` (21), `field_to_column` (3).

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

So the seam is one module lower:

| stays in the worker | moves to the relay |
| --- | --- |
| `broker.rs` (76 KB) - merges local writes with remote changes | `wal_consumer.rs` - decodes pgoutput |
| | `replication.rs`, `cdc_lifecycle.rs` - slot and publication lifecycle |
| | `slot_reaper.rs` - the privileged, destructive part |

Extract those three-and-a-bit into `zeroship-data-cdc`, hosted by a process that never executes
creator code - plausibly the relay that #5 builds, which would make the extraction its prerequisite
rather than a separate job.

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
adds one process hop to a path already going through WAL. The worker needs no new client machinery,
because the relay would feed `broker::publish` over a transport exactly where `wal_consumer` feeds it
in-process now. The broker is already the merge point and does not care which side an event came
from.

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
- **The `plugin-*` family breaks.** kv (3,019), storage (3,808) and workflow (10,847) stay
  `plugin-*` while db (57,427) leaves. `AGENTS.md`'s "Adding a native primitive" table points at
  `crates/plugin-{db,kv,storage}/`, which stops being true. Either accept the asymmetry explicitly
  in `AGENTS.md` - db is 15x kv and is a family where the others are single crates - or move all
  four. An unstated asymmetry is the worse option.
- **Whether the worker keeps decoding WAL** (above).
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

The practical rule: grep answers spelling, the compiler answers "is this named", and neither answers
"does production reach this". Only walking outward from a real entry point does. Every correction
above arrived when someone walked that path - which is the argument for doing it BEFORE writing the
claim, not after review returns.
