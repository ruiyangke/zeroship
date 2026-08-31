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

Roughly **1,966 lines** in `query.rs`, plus the dead half of `diff.rs`, delete before any porting
starts. Detail and per-symbol evidence in **#91** and **#92**. Summary:

| region | lines | why dead |
| --- | --- | --- |
| `query.rs:1057-1755` DDL builders | 699 | no callers; engine uses its own `crate::schema::query` |
| `query.rs:1756-3022` index builders | 1,267 | engine calls its own `index_name`; `migrate-backend/src/ddl.rs:123` says "i.e. the ENGINE's" |
| `diff.rs` differ + introspection | ~2,000 | `compute_diff` has no production caller; `read_live_schema` / `estimate_row_count` reachable only from `tests/sqlite_integration.rs` |

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
- **`SqliteEmitScope` is defined in the DDL region and used by the LIVE renderer** at `query.rs:139`,
  `:225`, `:339`, `:469`. It must be kept and rehomed, not deleted with its neighbours.

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
`crate::broker::publish(&event)` directly. Moving it would turn every local `db.insert(...)` into a
network round trip to observe your own write.

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

**Full is cheaper than an earlier draft of this document claimed.** That draft said full extraction
"adds a hop to every subscription". It does not: the broker stays, so LOCAL writes still short-
circuit in-process with no network at all. The hop lands only on changes originating in OTHER
processes - which already travel through WAL today. And the worker needs no new client machinery,
because the relay would feed `broker::publish` over a transport exactly where `wal_consumer` feeds it
in-process now. The broker is already the merge point and does not care which side an event came
from.

## The rename

`zeroship-plugin-db` -> `zeroship-data-binding`: **1,467 references across 159 files**, mostly
mechanical. Two things make it safer than it sounds: `deploy/Dockerfile` uses `COPY crates/ crates/`
wholesale rather than a curated list, so #64's failure mode is already gone; and
`doc_citation_gate.sh` checks 139 `AGENTS.md` paths, so broken doc references fail loudly.

**Ride it with Track B's extraction, never alone.** Renaming first means 1,467 edits to a crate you
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
