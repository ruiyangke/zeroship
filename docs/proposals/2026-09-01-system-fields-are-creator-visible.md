# System fields are creator-visible and platform-owned

Status: settled design, 2026-09-01. Supersedes the hide-by-default posture in
`sdks/bootstrap/src/install-schema.ts`.

Operator directive: be transparent about system fields, expose them to creators,
make them read-only where they must be, fewer restrictions. Refined across five
review rounds and settled by three operator decisions:

1. **The descriptor is transparent and hides nothing.**
2. **No hardcoded system-field lists**, in TypeScript or Rust. Values derive from
   named generators referenced from data.
3. **`assign` is not overridable; `default` is.**

Everything below is the design those decisions produce. Claims carry file:line;
where something is unverified it says so.

---

## The defect this starts from

The descriptor lies by omission, and inconsistently. Measured:

```
DDL:        created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()   (zeroship-schema/src/query.rs:212)
descriptor: created_at  required=true  default=undefined
            version     required=true  default=1
```

Same DDL line, same `DEFAULT` keyword: `version`'s literal default survives into
the descriptor, `created_at`'s expression default is dropped. `required=true` is
TRUE - the column really is NOT NULL. `default=undefined` is FALSE.

That single omission is load-bearing. `validateDoc`'s missing-value branch reads
the presence of a default as "the caller need not supply this"
(`sdks/db/src/validate.ts:507-515`), so a field with a real default that the
descriptor forgets becomes a field the caller is forced to supply. The SDK works
around it by deleting the seven system fields from every collection schema
before the validator sees them - which is what made them invisible.

**The fix is to stop omitting, not to hide more.**

---

## The shape: one property

A field declares **who computes its value, and when**:

```toml
{ name = "created_at", assign = { by = "now",          on = "insert" } }
{ name = "updated_at", assign = { by = "now",          on = "write"  } }
{ name = "version",    assign = { by = "increment(1)", on = "write"  } }
{ name = "id",         assign = { by = "typedId",      on = "insert" } }
{ name = "created_by", assign = { by = "actor",        on = "insert" } }
{ name = "updated_by", assign = { by = "actor",        on = "write"  } }
{ name = "deleted_at", assign = { by = "now",          on = "delete" } }
```

`on` is a closed vocabulary: `insert`, `write`, `delete`.

### What `by` may carry, and what it may not

**`by` names a generator and may carry CHARTER-LEVEL CONSTANT arguments only.**
`increment(1)` is legal: the seed `1` is one value for every table of every app,
which is exactly what a `scope = "all"` inject can attest.

**Per-collection creator data is never a charter argument.** An earlier draft
wrote `typedId(...)`, implying the prefix rides in the charter. It cannot, and
the contradiction was internal to this document: the inject rule is
`scope = "all"` (`confined-system-shape.inject.toml:60-64`), one identical line
for every table, while the prefix is per-collection creator data folded in by the
resolver (`crates/zeroship-migrate-core/src/model/table_shape.rs:357`). This
document said both things in two places and reconciled them nowhere. It killed
the first implementation attempt at the design stage, which is where it was
cheapest.

So the charter writes bare `typedId`. What it attests is the **generator identity
and its timing** - this column is minted by the typed-id generator on insert -
and nothing about the prefix. The prefix stays per-collection input, resolved at
generation time and validated at the pass boundary against
`RESERVED_ID_PREFIXES`, which is the gap documented below. That division is not a
concession: it is the same one the charter already makes, and the reason the
prefix needs its own fence rather than a charter line.

**`restore` is not an assignment event.** An earlier draft listed it as a fourth,
which forced the vocabulary to contain an event no generator could serve -
`restore()` writes `deleted_at = NULL` (`query.rs:4726`) and nothing in the set
yields nothing. Restore is instead defined as **the inverse of delete**: it
clears every field whose `assign` carries `on = "delete"`. One rule, no
null-producing generator, and no field bound to an event that cannot fire.

### `assign` vs `default`: the slot is the override policy

| slot | meaning | creator-supplied value |
| --- | --- | --- |
| `assign` | the platform computes the value | **not accepted** |
| `default` | a fallback when the caller supplies nothing | **wins** |

This is why the design needs no third property for override policy, and why
`updatable` / `generatorRuns` (an earlier draft) is not needed. All seven system
fields carry an `assign`. Ordinary creator columns keep `default` and stay
overridable, which is what `t.string().default("x")` already means.

### "System field" stops being a concept

This is the point of the change, and it is easy to miss while the document still
says "the seven".

`assign` is not a system-field property. The creator API already promises
assignment on ordinary columns: `t.actor()` is documented as "available to
creators who want their own actor-tracking columns (e.g. `last_edited_by`)"
(`sdks/db/src/types.ts:1881-1886`), and `t.timestamp()` carries `.auto_now()` /
`.auto_now_on_update()` (`:1044-1052`).

So the boundary is no longer *system vs creator*. It is **assigned vs
defaulted**, and both are available to both. No code branches on "is this a
system field?"; it branches on "does this have an `assign`?".

**What makes the platform's seven different is PROVENANCE, not identity.**
`created_by` differs from a creator's own `last_edited_by` because its `assign`
was declared in the root charter and the other's was not - which is exactly why
`assign` must be root-only at load. The property is available to everyone; the
authority to declare it on a platform column is not.

**Three things survive, and pretending otherwise would be dishonest:**

1. **The columns.** The charter still injects the same seven into every table
   (`scope = "all"`, `mandatory = true`). That is a list - but one list, in one
   operator-shipped file, which is the whole point.
2. **A hardcoded list in the DDL emitters.** `zeroship-schema` is a declared leaf
   with no TOML parser; the charter header says "THE SEVENTH PRODUCER CANNOT TAKE
   THIS FILE" (`:47-51`) and the mirror gate repeats it. So the goal is never
   "zero lists": it is collapse the ergonomic consumers, keep the trusted
   producers, and gate that they agree.
3. **A generated list in TypeScript**, since TS has no `include_str!` (charter
   `:32-35`).

### What follows from `assign`, and what does not

**Derived:**

| property | from |
| --- | --- |
| not required of the caller | the field has an `assign` |
| the client must not materialise a value | the field has an `assign` |
| immutable | `on = "insert"` |
| a creator-supplied value is refused | the field has an `assign` |

**NOT derived - the DDL default.** An earlier draft claimed it renders from `by`.
That is false and dangerous:

- `SynthFn` is closed at `ConcatWs`, `SplitPart`, `Now`
  (`crates/zeroship-migrate-ir/src/expr.rs:156-163`), so only `now` has any
  rendering.
- `version`'s `DEFAULT 1` is a literal seed, which `increment` does not express.
- **`deleted_at` is the kill.** It is `TIMESTAMPTZ NULL` with no default
  (`query.rs:217`), and its `by` is `now` - which *does* render. Derive it and
  every row is born soft-deleted: reads append `AND "deleted_at" IS NULL`
  (`query.rs:3405-3416`) and return zero rows, `delete()`'s inner SELECT carries
  the same predicate (`:4752`) so soft delete affects nothing, and `restore()`'s
  `IS NOT NULL` guard (`:4648`) succeeds on live rows.

So the DDL default stays declared in the charter beside `assign`. The two are
layers, not duplicates: **the generator is the normal path, the DDL default is
the backstop** for writes that never reach the runtime - migration DML, CDC
backfill, raw SQL. `created_by` already works this way: runtime assigns it, the
DDL says `NULL`.

---

## Where the bindings live, and what makes that safe

Bindings go in `policies/confined-system-shape.inject.toml`, which
`crates/zeroship-migrate-server/src/policy.rs:58` compiles in via `include_str!`.
The descriptor is creator-authored - `crates/zeroship-migrate-server/src/apply.rs:61-65`
says so outright ("the value is client-declared: a creator who hand-edits both
generated files can make them agree about a lie") - so it cannot hold the
authority.

**Two things break that, and both must be closed in the same change.**

**1. `assign` must be root-only at LOAD, not merely placed in a root file.**
**All three legs verified 2026-09-01.** `LoadContext`
(`crates/zeroship-migrate-policy/src/document.rs:27-44`) documents itself as
governing exactly two axes: `mandatory` injects are `RootCharter`-only, and
`extends` is trusted-only. Nothing gates an ordinary `[[inject]]`. Those
ordinary injects then UNION across layers - `compose.rs:1349` states
"require/inject/validate rule-sets UNION (each at its own scope)" and `:1377`
performs it. So an untrusted creator draft loaded as `NonRootLayer` may carry an
`[[inject]]`, and it reaches the effective policy. Widening `WireColumn` with an
`assign` key therefore lets a creator draft declare one.

**And the collision comparator would not catch it.** `inject_specs_collide`
(`compose.rs:1494`) compares exactly three column properties -
`ca.ty != cb.ty || ca.nullable != cb.nullable || ca.default != cb.default`
(`:1498-1499`). Two injects naming the SAME column with identical type,
nullability and default but DIFFERENT `assign` do not collide: they union, and
one wins arbitrarily. That is the attack, and it survives a comparator that is
never taught the new field. `PolicyDoc` retains rules but not their originating
`LoadContext`, so the check cannot be deferred - it belongs at load.

**That comparator is already wrong today, before `assign` exists.** `InjectColumn`
carries a `collation` (`rule.rs:92`), the charter pins it on three columns
(`id`, `created_by`, `updated_by` are `collation = "bytewise"`), and the
comparator does not compare it. Two injects on one column differing ONLY in
collation therefore union rather than collide - and for a typed-id column,
collation is what keeps base62 byte order equal to creation order. The doc
comment directly above the type claims otherwise: `rule.rs:73` describes the
check as "(name/type/nullable/default/collation)". The code checks three of those
five. Fix the live bug in the same change that teaches it `assign`, and treat the
stale comment as the warning it is - a comparator's doc is not its behaviour.

**2. The worker has no charter today.** Only migrate-server does. If plugin-db
takes bindings from the descriptor alone, a hand-edited `.zship` re-points them.
`include_str!`-ing the fragment into plugin-db closes it - no Cargo cycle,
`zeroship-migrate-policy` is a leaf - but the fragment cannot parse alone: it
lacks `policy_version` by design (`confined-system-shape.inject.toml:4-8`), so
plugin-db must concatenate a header exactly as the TypeScript ceiling already
does (`sdks/vite-plugin/src/gen-types/confined-ceiling.ts:28-31`), and parse once
at construction.

**One binding the charter still cannot attest.** The inject rule is
`scope = "all"`, `mandatory = true` (`confined-system-shape.inject.toml:60-64`) -
one identical line for every table of every app. `typedId`'s prefix is
per-collection creator data, read from the descriptor by
`prefix_for_collection` (`crud/system_fields_pass.rs:128-135`) **with no
validation** - the declared value flows straight through `.map(str::to_string)`.

**Verified 2026-09-01, and the asymmetry is sharper than "a missing check".**
A complete validator already exists: `validate_id_prefix`
(`zeroship-schema/src/query.rs:935`, reserved check at `:948`, backed by
`RESERVED_ID_PREFIXES` at `:922`). It is called from four sites, and **every one
of them is on the migration/DDL side** - `zeroship-schema/src/query.rs:1295`,
`migrate-core/src/model/table_shape.rs:643`, `model/validate.rs:9112`,
`render/declarative.rs:2173`. `zeroship-plugin-db` calls it **zero times**. The
*derived*-prefix path has its own duplicate one-element list
(`RESERVED_AUTO_PREFIXES`, `system_fields_pass.rs:60`, applied `:97-111`); the
descriptor-declared path reaches neither.

So the producer validates and the consumer does not, across an artifact boundary
where the two need not agree: the descriptor is a separate file from the
migration, and `apply.rs:61-65` says it is client-declared. A migration may
declare `blog` and pass, while the descriptor the worker actually reads declares
`usr`. That shape is already exercised in-tree -
`crates/zeroship-bundle/tests/runtime_descriptor_ingest_test.rs:86` ingests a
runtime descriptor carrying `{"type":"id","idPrefix":"usr"}`.

A descriptor claiming `idPrefix: "usr"` therefore mints platform-user-shaped ids
from the worker. The fix is one call to the existing validator at the pass
boundary, and it belongs to this change, because this change is what claims the
worker stops trusting descriptor bindings.

---

## What the lists become

`assign` in the charter is what lets the hardcoded lists go: `SYSTEM_FIELD_NAMES`
and `IMMUTABLE_SYSTEM_FIELDS` in Rust, the SDK list in `install-schema.ts`, three
copies in `sdks/db/src/types.ts`, two prose strings in
`crates/zeroship-plugin-db/src/error.rs:728-745`, and the field-naming inside
`system_fields_pass.rs`, which iterates the descriptor and invokes the named
generator instead. Adding an eighth system field becomes a charter line.

**Two consumers cannot be converted, and the tree says so.** The three
per-dialect DDL emitters live in `zeroship-schema` (`query.rs:208`, `:322`,
`:449`), a declared leaf with no TOML parser; the charter header states "THE
SEVENTH PRODUCER CANNOT TAKE THIS FILE" (`:47-51`) and
`tests/inject_policy_mirror_gate.sh:60-74` repeats it. TypeScript cannot
`include_str!` either (charter `:32-35`), so "no hardcoded lists in TS" resolves
to a **committed generated** list, `confined-system-shape.generated.ts`.

So the honest goal is: **collapse the ergonomic consumers; keep the trusted
producers, generated or hardcoded, with a gate proving they agree.** A fence
list must be duplicated in the trusted layer; only the derived ones may be
merged. The gate cannot assert equality - the lists are different projections
(all seven, the three timestamps, the immutable subset, the indexed subset, DDL
tuples, row types, prose) - so it must define named contracts and register each
mirror against one, and state its limits as
`tests/inject_policy_mirror_gate.sh:55-89` does.

---

## The work, in dependency order

**Preconditions - these are live bugs and must land first.**

| # | what | why it blocks |
| --- | --- | --- |
| #132 | the timestamp round-trip - **scoped down: creator-declared timestamp columns only.** The read-side normalisation is TYPE-driven (`read_pipeline.rs:183`, `Some("date") \| Some("calendarDate")`), so a creator's own `t.timestamp()` column has the identical asymmetry; the platform's three are handled by assignment instead and never carry a caller value to the builder. **Route: convert in the emitted SQL per dialect**, not by a Rust value formatter - plugin-db declares no date library and hand-rolls (`session_minter.rs:393`), and its two existing parsers disagree about the accepted shape, so a third formatter is new correctness surface. `to_timestamp($n/1000.0)` on PG; `strftime` with an explicit T-form on SQLite, deliberately not `datetime()`, whose space-separated output IS #134. | reads emit Unix-ms (`crud/read_pipeline.rs:162`), writes demand a `Date` (`validate.ts:277-284`); measured on pg18, a 13-digit value errors `22008` and an 8-digit value is **silently accepted as a calendar date**. It cannot be fixed in `validate.ts`: that is not on every write path (`crud.ts:513-569` calls neither validator), and filters bypass it entirely (`utils.ts:98-103`, `query.rs:5714-5744`), so `find({created_at:{$gt: row.created_at}})` already fails today. |
| #134 | the SQLite spelling schism | the DDL default writes `CURRENT_TIMESTAMP` (space-separated, `query.rs:323`) and creator values arrive as `toISOString()` (a `T`, `v8_bridge.rs:263-281`); TEXT comparison is bytewise, so a row stamped `23:59:59` sorts **before** one written at midnight the same day. Measured. Moving timestamps to runtime-assigned dissolves it - one writer, one spelling - so the conversion and the DDL default must land on one spelling in one change. |
| #135 | the live PG fixture | declares the timestamps nullable where production is `NOT NULL`; a regression test written on it passes while production raises `23502`. Fixed in `b2515127d`. |

**Then, in this order:**

1. **Charter gains `assign`**, root-only at load. Cost, sized: `WireColumn` is
   `deny_unknown_fields` (`document.rs:321-337`) so every consumer breaks until
   they move together, including migrate-server which parses at startup and
   exits on failure (`main.rs:245-257`); `InjectColumn` maps only
   type/null/default/collation (`rule.rs:72-93`); the collision comparator is
   field-enumerated and must learn `assign` or two different bindings compare
   equal; the seal must encode it (`seal.rs:485-512`);
   `ResolvedInject` converts straight to `IrColumn` with no assignment carrier
   (`table_shape.rs:150-201`); artifact projection discards synthesized defaults
   (`lower.rs:10066-10102`); plus regenerating the committed TS fragment
   (`policies/codegen.mjs`) and rebuilding the `.node` addon.
2. **The worker takes the charter**, with the synthetic header, parsed once.
3. **Validate the `typedId` prefix** at the pass boundary.
4. **The descriptor carries the binding** as a mirror the worker verifies. Note
   this is a **v3** descriptor by the rule at `install-schema.ts:68-74`: a
   consumer that stops deriving something itself and depends on a property being
   present on every field is exactly the situation that moved v1 to v2.
5. **`system_fields_pass.rs` stops naming fields** - it iterates and invokes.
   A supplied value for an assigned field is **removed**, not ignored.
6. **The SDK stops requiring and stops materialising** for assigned fields, and
   `stripRuntimeSystemFields` goes.
7. **Soft-delete routes through the native op** (`crud.ts:513-532`, `:556-573`)
   BEFORE any `deleted_at` refusal, or the refusal rejects the platform's own
   `delete()`.
8. **Migration DML gains a destination fence** for `created_by`/`updated_by`
   only - see the import note below.

---

## Constraints that will be got wrong if not stated

**"Not accepted" means removed, not ignored.** The raw `zeroship.db.*` path does
not pass through `validateDoc` (`system_fields_pass.rs:29-32` names it
first-class), so a supplied `version` can still arrive. If the auto-bump is made
unconditional while the key still reaches `build_upsert`, the generic loop emits
`"version" = EXCLUDED."version"` (`query.rs:6295-6304`) AND the bump emits a
second assignment to the same column (`:6313-6333`) - two assignments to one
column in one `DO UPDATE SET`, which PostgreSQL refuses. Strip at the pass and
make the bump unconditional as ONE change.

**`insertMany` needs a batch-shape rule.** `build_insert_many` unions the column
set across all documents (`query.rs:4390`) then binds
`obj.get(key).unwrap_or(&Value::Null)` for missing cells (`:4445`). Removing a
key per-document does not survive the union: a batch where one row supplies
`created_at` and another omits it drives an explicit `NULL` into a `NOT NULL`
column. Normalise the batch, group by shape, or emit `DEFAULT`.

**`on` is a lattice, not a set of disjoint events.** The native soft-delete and
restore builders bump `version`, `updated_at` and `updated_by` in the same
statement (`query.rs:4693-4714`, `:4720-4741`), so delete and restore must count
as "write" for those three while remaining distinct events for `deleted_at`.
And "write" must cover insert, or `updated_at` and `version` lose their DDL
defaults. This is what makes `restore`-as-inverse cheap: it is not a fourth arm
of the lattice, it is `write` plus the clearing of the `on = "delete"` fields.

**Anonymous writes resolve to NULL, not to a stale actor.** `created_by`/
`updated_by` are injected only when an actor is bound
(`system_fields_pass.rs:266`), and the update builder omits the SET clause the
same way (`query.rs:4228-4232`), leaving `updated_by` naming an actor who did
not make the last write. **Decided: the generator runs on every write and yields
NULL when unauthenticated.** Stale attribution is worse than absent attribution -
it is a false claim about who touched a row, and anything reading `updated_by`
for audit or authorization is entitled to believe it. This is a behaviour change
and needs its own test. The naive patch reproduces today's staleness, and the
existing test `insert_leaves_created_by_absent_when_no_actor` (`:707-719`) stays
green through it, because its input never had the key.

---

## Acceptance

0. `insert({ path: "/x" })` on a descriptor-installed collection succeeds. It
   does NOT today once the strip is removed: `id`/`created_at`/`updated_at` are
   `required: true` with no default, so `validateDoc:511` raises. **Measured.**
1. A creator schema declaring `created_at: t.timestamp()` is accepted and reads
   return the platform's value. Note `t.date` does not exist - the builders are
   `t.timestamp()` and `t.calendarDate()`.
2. An UPDATE naming `created_at` is refused. *(Regression guard: already true.)*
3. An UPDATE naming `updated_at` is **refused**. **Inverted by the decision** -
   it is accepted today.
4. Under `naming.snakeCase`, declaring `createdAt` is refused as a collision.
   Fails today. But the collision is built in `sdks/db/src/collection.ts:203-221`
   (the `autoFields` loop overwrites the declared mapping), not at
   `install-schema.ts:308`, and `normalizeSchema` has no access to the naming
   strategy - so the check must move to where the strategy is known.
5. Generated types carry all seven, with assigned fields unassignable in an
   insert payload. Note `Row<S>` already carries them (`types.ts:195`); the work
   is `render-env-db.ts:73-81`/`:129` and `RowInput`'s bans (`types.ts:200-208`).
   **Un-eliding also erases `| null` from `created_by`/`updated_by`/`deleted_at`
   by intersection narrowing, silently** - those three are nullable in the DDL
   (`query.rs:212-217`) and the renderer emits them optional.
6. `insert({ id: "post_abc" })` is **refused**. **Inverted by the decision** -
   #126 asked for a round-trip and does not get one; what it gets is a signal
   instead of silence.
7. `insert({ created_by: "usr_SOMEONE_ELSE" })` is refused, INCLUDING on an
   anonymous request. The naive patch flips the guards inside
   `if let Some(actor)` and leaves the `None` arm a passthrough.
8. A descriptor whose `encrypted` marker was removed does not silently produce
   plaintext writes. Unrelated to system fields, same mechanism - see #133.

---

## Open

- **Where history import lives.** The decision moves external-id / original-
  timestamp import out of `env.db`; migration DML is the only remaining path, so
  step 8's fence must refuse `created_by`/`updated_by` and never `id` or
  `created_at`.
- **The mirror gate's contracts** - named projections, and what it admits it
  cannot prove.

## Related live defects, tracked separately

`#132` timestamp round-trip · `#133` descriptor-gated encryption · `#134` SQLite
spelling schism · `#136` plaintext DDL default on an encrypted column (fixed,
`01a2b4ba1`).
