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

**But that argument currently has no reader, and the shape makes the gap
invisible. OPEN, and it needs a decision before another generator takes an
argument.** Measured 2026-09-01 across the whole tree.
`AssignmentGenerator::Increment(i64)` is declared at
`crates/zeroship-migrate-policy/src/rule.rs:153`, parsed at `:220`,
`Display`-round-tripped at `:190`, and folded into the policy seal at
`crates/zeroship-migrate-policy/src/seal.rs:517`. That is the complete set of
readers. The write pass matches it as `Increment(_)` and DISCARDS the amount
(`crates/zeroship-plugin-db/src/crud/system_fields_pass.rs:383`); the actual
step is a literal `+ 1` emitted from three places in `zeroship-schema`
(`query.rs:4279` update, `:4780` soft delete, `:4825` restore) plus the PG
upsert's `COALESCE(..., 0) + 1` at `:6476`.

So `increment(2)` in the charter parses, passes the root-only fence, **changes
the sealed policy identity** - and the runtime still adds 1. Typed, sealed,
mirrored, and false. Note what does NOT catch it: `inject_policy_mirror_gate.sh`
arm 5 compares column NAMES, so a charter whose only edit is the increment
argument passes every arm.

This is a defect the data-driven pass CREATED. Before it, `by` never reached the
runtime, so there was no argument to ignore.

**Recommended: honour the amount rather than fence it.** Threading `n` into
those four emitters is small and bounded, and it is the end state; a fence
refusing `n != 1` is a second intermediate shape to throw away later, which this
project's own pre-launch stance argues against. The fence is only the right
answer if the amount turns out to need per-dialect care that makes honouring it
expensive - decide that by trying, not by assuming. Whichever is chosen, the
rule generalises: **an argument the charter can express and the runtime cannot
honour must be refused at parse time, never silently dropped**, because the seal
makes it look attested.

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
and nothing about the prefix.

### An integer identity `id` substitutes the generator, not the policy

The resolver lets a creator-authored integer identity column REPLACE the injected
`id` wholesale: `is_id_identity_replacement` accepts an `id` column carrying
`identity` of `SmallInt | Int | BigInt`
(`crates/zeroship-migrate-core/src/model/table_shape.rs:615-622`), and the fold
then does `col = author_col.clone()` (`:380`), which drops any `assign` the
charter put there. Read naively that is a creator overriding a non-overridable
root binding, and it looks like it forces a choice between refusing the
capability and abandoning the fence.

**It forces neither, because `assign` declares two separable things:**

| | what it declares | does identity replacement change it? |
| --- | --- | --- |
| policy | the value is platform-computed; a caller-supplied value is refused | **no** |
| generator | `typedId` mints it | **yes** - the database's own sequence does |

A creator writing `t.id()` as a bigserial is not asking to supply ids. They are
asking a *different non-creator* to compute them. So the fold must **preserve the
`assign` and rewrite its `by`**, not clone the column and lose it.

The generator set therefore gains `identity`, meaning *this column is assigned by
its own DDL identity; the runtime emits nothing for it*. Consequences, both
required:

- `system_fields_pass.rs:253` mints a typed-id string unconditionally when `id`
  is absent. It must consult `by` instead - which is step 5's work anyway ("stops
  naming fields; it iterates and invokes"). Under `by = "identity"` it emits no
  column and lets the INSERT default fire.
- The refusal of a caller-supplied `id` is unchanged in both shapes, because it
  derives from the presence of an `assign`, not from which generator it names.

This keeps a working creator capability, keeps the security property uniform, and
costs one generator whose whole meaning is "not me". Refusing integer identity
instead would delete a feature to make the charter tidier, which is the platform
serving itself.

### An `assign` may not outlive the thing its generator depends on

The resolution above is only sound if `by = "identity"` stays true. It does not,
today, and the gap is general rather than specific to identity: **charter
resolution runs on `CreateTable` only** (`table_shape.rs:310`), so no later
operation is re-resolved against the charter.

The concrete instance: `AlterPrimaryKey` accepts `dropIdentityFrom: ["id"]`
(`validate.rs:11789`), and the fold removes `column.identity` while leaving
`assign.by = "identity"` untouched (`single_fold.rs:714`). After that migration
the descriptor claims the database generates `id` and the database does not.
Step 5's pass would omit the column from the INSERT and every insert would fail.

**Production is protected only by accident, which is the reason to fix it rather
than note it.** Destructive primary-key changes require approval and
migrate-server supplies `Approval::None` (`apply.rs:495`) - so the operation is
refused for a reason that has nothing to do with assignment policy. Local dev
supplies `approved: true` (`dev-apply.ts:172`), so the stale binding is reachable
there now. A protection that holds for an unrelated reason is not a protection;
it is a coincidence with good PR.

**Decided: refuse the operation, and state the rule generally.** An operation
that would invalidate a column's `assign.by` is refused at the policy-aware IR
boundary, for every backend. For `by = "identity"` that is `dropIdentityFrom`;
the same guard catches any future operation that removes a DDL feature an
`assign` depends on.

The narrower alternatives are all incoherent, which is what makes this the
answer rather than a preference. Clearing the `assign` hands the column to
caller-supplied values, which the policy refuses - so the column becomes
unwritable by anyone. Reverting `by` to `typedId` cannot produce an integer.
Changing the column's type is a new destructive design. **Dropping identity from
an assigned `id` does not produce a working table under any reading**, so
refusing it removes no capability; a creator's own integer column carries no
`assign` and may still drop its identity freely. The prefix stays per-collection input, resolved at
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

`assign` in the charter is what lets the ERGONOMIC lists go: `IMMUTABLE_SYSTEM_FIELDS`
in Rust, the SDK list in `install-schema.ts`, three copies in
`sdks/db/src/types.ts`, two prose strings in
`crates/zeroship-plugin-db/src/error.rs:728-745`, and the field-naming inside
`system_fields_pass.rs`, which iterates the charter and invokes the named
generator instead.

**`SYSTEM_FIELD_NAMES` is NOT one of them, and this paragraph said it was.** It
is a trusted producer, for a reason that only became visible once the write pass
was converted: `implicit_read_projection_parts`
(`crates/zeroship-schema/src/query.rs:3644-3656`) runs two loops, and only the
SECOND consults `readable`. The seven are projected unconditionally; everything
else is projected at the creator-authored descriptor's discretion. Deleting the
const would move the platform's own columns into the discretionary loop, where
`readable: false` suppresses them. The const is the mechanism that makes them
unhideable, not a convenience copy of the charter.

So "adding an eighth system field becomes a charter line" is true of the DDL and
the write pass and false of the system as a whole. An eighth column would be
created, assigned and projected - but only through the descriptor, so unlike the
seven it could be hidden by hand-editing one JSON key.

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
`tests/inject_policy_mirror_gate.sh` does.

**Two of those contracts are now built.** `inject_policy_mirror_gate.sh` arm 5
compares the charter's assigned columns against `SYSTEM_FIELD_NAMES` in content
and order; arm 6 compares the charter's three system indexes against all three
per-dialect `SYSTEM_INDEXED_COLS` copies (`query.rs:227`, `:341`, `:471`),
separately rather than deduplicated, so two dialects agreeing while a third does
not is caught. Both are mutation-proved in both directions and against a blinded
extractor.

The projections still unguarded, in the order they would hurt: the DDL tuples
(types, nullability, collation - the charter header records that PostgreSQL and
the engine already disagree on `id`/`created_by`/`updated_by`, so this one is
known-drifted rather than merely unchecked), the row types in
`sdks/db/src/types.ts`, and the prose in `error.rs`. Types need a rendered-DDL
comparison rather than a textual one, which is why they are last: the check is a
different kind of instrument, not a bigger version of arms 5 and 6.

---

## The work, in dependency order

**Preconditions - these are live bugs and must land first.**

| # | what | why it blocks |
| --- | --- | --- |
| #132 | the timestamp round-trip - **scoped down: creator-declared timestamp columns only.** The read-side normalisation is TYPE-driven (`read_pipeline.rs:183`, `Some("date") \| Some("calendarDate")`), so a creator's own `t.timestamp()` column has the identical asymmetry; the platform's three are handled by assignment instead and never carry a caller value to the builder. **Route: convert in the emitted SQL per dialect**, not by a Rust value formatter - plugin-db declares no date library and hand-rolls (`session_minter.rs:393`), and its two existing parsers disagree about the accepted shape, so a third formatter is new correctness surface. `to_timestamp($n/1000.0)` on PG; `strftime` with an explicit T-form on SQLite, deliberately not `datetime()`, whose space-separated output IS #134. | reads emit Unix-ms (`crud/read_pipeline.rs:162`), writes demand a `Date` (`validate.ts:277-284`); measured on pg18, a 13-digit value errors `22008` and an 8-digit value is **silently accepted as a calendar date**. It cannot be fixed in `validate.ts`: that is not on every write path (`crud.ts:513-569` calls neither validator), and filters bypass it entirely (`utils.ts:98-103`, `query.rs:5714-5744`), so `find({created_at:{$gt: row.created_at}})` already fails today. |
| #134 | the SQLite spelling schism | the DDL default writes `CURRENT_TIMESTAMP` (space-separated, `query.rs:323`) and creator values arrive as `toISOString()` (a `T`, `v8_bridge.rs:263-281`); TEXT comparison is bytewise, so a row stamped `23:59:59` sorts **before** one written at midnight the same day. Measured. Moving timestamps to runtime-assigned dissolves it - one writer, one spelling - so the conversion and the DDL default must land on one spelling in one change. |
| #135 | the live PG fixture | declares the timestamps nullable where production is `NOT NULL`; a regression test written on it passes while production raises `23502`. Fixed in `b2515127d`. |

**Then, in this order.** Steps 1-3 have landed on `feat/dbbind-impl`; what
follows each is what was actually verified, not what was intended.

**1-3 DONE.**

- **Step 1** - the charter declares all seven bindings beside their existing
  `default` and `collation`, and the loader fences `assign` to the root layer.
  Verified: `cargo test -p zeroship-migrate-policy` green across every target;
  `cargo test -p zeroship-migrate-server` green, which is the oracle that
  matters because it `include_str!`s the shipped charter and parses at startup;
  `tests/inject_policy_mirror_gate.sh` **caught real drift** in the generated
  TypeScript view, regenerated via `policies/codegen.mjs`, then 4 arms / 0
  refusals. The collision comparator also learned `collation`, which it had
  never compared despite its own doc comment claiming it did.
- **Step 2** - the worker compiles the charter in and parses it once in
  `DbService::new`, beside `select_backend`, so a malformed authority fails at
  composition rather than inside an app's first write. `zeroship-migrate-policy`
  was confirmed a genuine leaf (no zeroship dependencies) before the edge was
  added, so it introduces no cycle.

  **The field is parsed and retained but NOT YET READ**, marked with a scoped
  `allow(dead_code)` naming step 5 as what removes it. Parsed-at-startup is live
  behaviour; read-by-a-consumer is not yet, and conflating the two is how this
  crate accumulated four other built-tested-unreferenced regions. The first
  attempt at this step shipped a loader with **zero** production callers and a
  commit message claiming otherwise.
- **Step 3** - the descriptor-declared `typedId` prefix now routes through the
  same validator as the derived path, with a red-first test that minted
  `usr_034HQyaJ0C11GCzHMMrWwz` before the fix, a control proving ordinary
  prefixes still mint, and coverage of every `RESERVED_ID_PREFIXES` entry rather
  than `usr` alone.

**Remaining, in this order:**

1. ~~**Charter gains `assign`**, root-only at load.~~ **DONE.** Cost, as sized
   before the work - every item held: `WireColumn` is
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
2. ~~**The worker takes the charter**, with the synthetic header, parsed once.~~
   **DONE.**
3. ~~**Validate the `typedId` prefix** at the pass boundary.~~ **DONE.**
4. **The descriptor carries the binding** as a mirror the worker verifies. Note
   this is a **v3** descriptor by the rule at `install-schema.ts:68-74`: a
   consumer that stops deriving something itself and depends on a property being
   present on every field is exactly the situation that moved v1 to v2.
5. **`system_fields_pass.rs` stops naming fields** - it iterates and invokes.
   A supplied value for an assigned field is **removed**, not ignored.

   **This is the next step, and step 2 unblocked it.** The pass can now iterate
   the CHARTER the worker holds rather than the creator-authored descriptor,
   which is the whole reason step 2 came first. It is also the step that closes
   the half of the id fence step 3 left open: minting is conditional on `id`
   being absent, so a supplied `usr_`-prefixed id is still written verbatim
   today. **The remaining half is the one an attacker reaches without touching a
   generated file**, so treat this as security work, not ergonomics.

   **THE REFUSAL CANNOT LIVE IN THE PASS, and this was measured by trying.**
   Implementing it inside `inject_into_object` breaks three tests, and one of
   them is an invariant rather than a stale expectation:
   `insert_pass_is_idempotent`. The pass documents itself as idempotent -
   "calling this twice on the same doc is a no-op the second time (every check
   is field absent -> inject)" - and a refusal keyed on `id` being PRESENT
   cannot distinguish a creator's value from one the pass minted on an earlier
   call. Presence is the only signal available inside the function and it means
   both things.

   So the refusal belongs at the **caller boundary**, where "supplied by the
   creator" is still knowable, and the pass keeps minting. The requirement is
   pinned as an `#[ignore]`d test naming that boundary
   (`insert_refuses_a_creator_supplied_id`), with a live control beside it so
   the ignored case cannot quietly become unreachable.

   **This is a correction to the constraint below, not an exception to it.**
   "Strip at the pass" is right for `version`, whose hazard is two assignments
   reaching one `DO UPDATE SET`. It is wrong for `id`, because the pass is the
   one place that cannot tell whose value it is looking at.
6. ~~**The SDK stops requiring and stops materialising** for assigned fields, and
   `stripRuntimeSystemFields` goes.~~ **DONE.** `validateDoc` reads `def.assign`
   and drops the key from its OWN result object - never from the caller's
   document. The seven-name lists in `install-schema.ts` and `collection.ts` now
   read a committed generated projection derived from the charter, and
   `policies/codegen.mjs` emits it to a second target so `@zeroship/db` can
   import it without a package cycle.

   Verified: bootstrap 77/1 -> **78/0**, `@zeroship/db` 517 -> **528**, mirror
   gate exit 2 -> **exit 0**. The stale-dist hazard was closed by MUTATION, not
   by mtime: deleting `assign` from `created_at` in the fragment, regenerating,
   and rebuilding turned bootstrap back to 77/1 with `created_at is required`;
   restoring returned 78/0. A stale dist cannot follow a source mutation.

   **It also exposed the second `version` producer** described above, which is
   now fixed: both post-normalization injections take the charter stamp, so the
   `DEFAULT 1` seed no longer reaches the write. Before the fix an insert
   arrived at the native op as `{"title":"hello","version":1}`; the regression
   test asserts against a recording native stub rather than an internal shape.
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

**THERE IS A SECOND PRODUCER OF `version`, AND THIS DOCUMENT NEVER NAMED IT.**
Found and measured 2026-09-01. `sdks/bootstrap/src/install-schema.ts:599-600`
injects `normalized.version = { type: "number", required: false, default: 1 }`
when versioning is enabled - and it does so AFTER `normalizeSchema` at `:591`,
which is where the charter stamp happens. So it carries no `assign`, never
passes through the assignment projection, and falls into `validateDoc`'s
`default` arm, which MATERIALISES the seed. A probe with `versioning: true` and
a descriptor lacking `version` produced the row `{"path":"/x","version":1}`
reaching the native op.

That is exactly the key the paragraph above says must not reach `build_upsert`.
This document named only the `deletedAt` injection at `:594-595` as the ad-hoc
producer to delete - **`:599-600` is its unnamed twin, and it is the one that
actually writes a value.** `deletedAt` carries no default, so it is skipped;
`version` does, so it is not.

Consequence for the ordering inside `validate.ts`: the `assign` check must sit
ABOVE the `default` arm, because `version` is the one charter column carrying
both an `assign` and a `DEFAULT 1`. Reversed, the seed is written.

**"At the pass" holds for `version` and NOT for `id`** - see step 5. `version`
has no minting step whose output the pass could mistake for a caller's value;
`id` does, and the pass is documented and tested as idempotent, so a
presence-keyed refusal there rejects the pass's own earlier output. Removal and
refusal want different homes: removal belongs where the value would otherwise
reach the SQL builder, refusal belongs where provenance is still known.

**`insertMany` needs a batch-shape rule.** `build_insert_many` unions the column
set across all documents (`query.rs:4390`) then binds
`obj.get(key).unwrap_or(&Value::Null)` for missing cells (`:4445`). Removing a
key per-document does not survive the union: a batch where one row supplies
`created_at` and another omits it drives an explicit `NULL` into a `NOT NULL`
column. Normalise the batch, group by shape, or emit `DEFAULT`.

**`on` is a lattice, not a set of disjoint events.** The native soft-delete and
restore builders bump `version`, `updated_at` and `updated_by` in the same
statement (`build_soft_delete_set_clauses`, `query.rs:4772`;
`build_restore_set_clauses`, `:4817`), so delete and restore must count
as "write" for those three while remaining distinct events for `deleted_at`.
And "write" must cover insert, or `updated_at` and `version` lose their DDL
defaults. This is what makes `restore`-as-inverse cheap: it is not a fourth arm
of the lattice, it is `write` plus the clearing of the `on = "delete"` fields.

**The pass does not yet encode that lattice, and step 7 is where it will bite.**
`system_fields_pass.rs:104` answers only one question - does this event fire
while a row is being created - and returns `false` for `Delete`. That is correct
for what the pass does today, because delete and restore never reach it: they go
through the two builders above, which name `deleted_at`, `version`, `updated_at`
and `updated_by` literally. Those two functions are a FIFTH hand-written
restatement of charter semantics, and they are the ones that decide the lattice.

So step 7 cannot be a straight re-point of delete onto the pass. Done naively, a
soft delete would stamp `deleted_at` and stop bumping `version` - which is what
optimistic concurrency reads, so a stale-version update would start matching a
row that had been deleted underneath it. The behaviour is correct today and the
refactor is what would break it; a `deleted_at`-only regression would pass every
test that checks a delete marks the row deleted. Step 7 must carry the lattice
rule (`delete` implies `write`, plus the delete stamp) and a test that a soft
delete still increments `version`, before the builders are retired.

**Anonymous writes resolve to NULL, not to a stale actor. SHIPPED, `8e2f89759`.**
The rule as decided: the generator runs on every write and yields NULL when
unauthenticated, because stale attribution is worse than absent attribution - it
is a false claim about who touched a row, and anything reading `updated_by` for
audit or authorization is entitled to believe it.

What landed, and where, because the gate on the SET clause is not where a reader
would look for it. The update path keys on `autobump.dispatch_write`
(`query.rs:4302`) rather than on the actor's presence, so a direct builder caller
- which writes on nobody's behalf - still emits no clause at all, while every
CRUD dispatch write emits one. Delete and restore take the same rule through a
shared helper, `push_updated_by_clause` (`:4795`), unconditionally: those two
builders exist only for the dispatch path, so there is no third case to exclude.

The four production sites that now stamp NULL are `dispatch_update_one`,
`dispatch_update_many`, `dispatch_restore_one` and `dispatch_restore_many`
(`crud/mod.rs:1001`, `:1113`, `:1528`, `:1590`), plus soft delete via the shared
helper. Checked against the platform's own reads: nothing in `plugin-db` or
`zeroship-schema` makes an authorization decision from `updated_by`, so clearing
it cannot widen access; it can only stop a creator's own query from believing a
name that was already wrong.

`version` had been gated on `dispatch_write` all along (`query.rs:4278`), so this
change makes attribution follow the rule the version bump already followed rather
than inventing a gate for it.

---

## Acceptance

**Acceptance item 0a, and it is the one currently missing: a test must register
a `DbPlugin` and assert the context carries a plan.** `system_shape_charter::plan()`
returns the context-stamped projection if one is present and otherwise derives
its own from the same `include_str!` bytes and stamps that. Every test in
`--lib` and in `sqlite_integration` reaches the pass WITHOUT a registered
plugin, so all of them exercise the fallback. Delete the `set_assignment_plan`
call from `DbPlugin::register` and the whole suite stays green; what you would
get instead is a `dead_code` warning, in a crate that already emits dozens, so
in practice nothing.

That matters because the fallback is what makes the deletion invisible, and the
fallback is deliberate - it is what lets unit tests and the `test-helpers`
targets drive the pass at all. The `grep -c 'allow(dead_code'` check proves the
attribute is gone from the field; it does not prove the field is READ on the
path a production worker takes. Only a registering test binds that, and none
exists.

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

   **The platform trips this guard itself, and that must be fixed first.**
   Verified 2026-09-01: `sdks/bootstrap/src/install-schema.ts:569-571` injects a
   **camelCase** `deletedAt` when soft delete is on -
   `normalized.deletedAt = { type: "date", required: false }` - while
   `collection.ts`'s `autoFields` list carries the **snake_case** `deleted_at`.
   Under `naming.snakeCase` both resolve to the column `deleted_at`, the second
   loop overwrites `colToField["deleted_at"]`, and the injected field loses its
   read mapping. So the platform already performs the exact collision this
   criterion forbids, and a guard added naively fires on the platform's own
   soft-delete path rather than on creator code.

   The fix is not to special-case it. Under this design `deleted_at` is
   charter-injected with `assign = { by = "now", on = "delete" }`, so the ad-hoc
   JS injection should not exist at all - it is a second producer of a column the
   charter already owns. Delete it as part of step 6, before the guard lands.
5. Generated types carry all seven, with assigned fields unassignable in an
   insert payload. Note `Row<S>` already carries them (`types.ts:195`); the work
   is `render-env-db.ts:73-81`/`:129` and `RowInput`'s bans (`types.ts:200-208`).
   **Un-eliding also erases `| null` from `created_by`/`updated_by`/`deleted_at`
   by intersection narrowing, silently** - those three are nullable in the DDL
   (`query.rs:212-217`) and the renderer emits them optional.
6. `insert({ id: "post_abc" })` is **refused**. **Inverted by the decision** -
   #126 asked for a round-trip and does not get one; what it gets is a signal
   instead of silence.

   **This is the other half of a security fix, not an ergonomic preference.**
   The prefix fence closes the DESCRIPTOR vector: a descriptor can no longer
   declare `idPrefix: "usr"` and have the worker mint platform-user-shaped ids.
   It does not close the DIRECT vector, because minting is conditional -
   `system_fields_pass.rs:256` is `if !obj.contains_key("id")`, so a
   creator-supplied `id` is preserved verbatim and `insert({ id: "usr_SOMEONE" })`
   writes that value today. Verified 2026-09-01. Until an assigned field's
   supplied value is REMOVED at the pass (step 5), the fence is half built, and
   the half that is missing is the one an attacker reaches without touching a
   generated file.
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
