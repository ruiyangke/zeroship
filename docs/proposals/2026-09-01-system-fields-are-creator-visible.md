# System fields are creator-visible, and read-only where they must be

Status: proposal, 2026-09-01. Supersedes the hide-by-default posture in
`sdks/bootstrap/src/install-schema.ts`.

Operator directive, 2026-09-01: *"for the system fields, we should be
transparent, we can make some fields readonly from creator code, just like
salesforce, but we should expose these fields to creator, less restrictions."*

## The finding: the runtime already implements this, and the SDK undoes it

The two layers disagree, and the restrictive one is the SDK.

**The Rust runtime is already Salesforce-shaped.** Of the seven system fields,
`crates/zeroship-plugin-db/src/crud/system_fields_pass.rs:46` marks exactly
three immutable:

```rust
pub(crate) const IMMUTABLE_SYSTEM_FIELDS: &[&str] = &["id", "created_at", "created_by"];
```

An UPDATE naming one of those three is refused at `:425` (and again at `:395`
for the `$set`-nested form) with `ImmutableSystemField`. The other four are
deliberately writable: `version`, `updated_at` and `updated_by` set
`creator_supplied_*` hints that *suppress* the auto-bump, so a creator who
supplies them wins; `deleted_at` is owned by `delete()` / `restore()`. That is
the Salesforce split already, decided and enforced, with the audit fields
overridable the way Salesforce's "Set Audit Fields" permission allows.

**The SDK then hides all seven.** `install-schema.ts:308` refuses any creator
schema declaring one of `SYSTEM_FIELD_NAMES` (`:249`), with a single sanctioned
exception for `id: t.id("prefix")`. And `:1072` strips all seven out of the
descriptor-derived field list before it reaches `model()`.

**The strip is an INSERT-VALIDATION GUARD.** This took three answers to get
right, and the first two were mine.

*Answer 1 (wrong).* The first draft said the strip was a workaround for the
declaration refusal - that without it the platform's own descriptor would throw
`RESERVED_SYSTEM_FIELD_NAME` on boot. **Refuted from the code.**
`normalizeSchema` copies any raw `FieldDef` and `continue`s at `:297-300`,
*before* the refusal at `:308`, and the comment at `:288-292` says exactly why.
Nothing throws at boot.

*Answer 2 (incomplete).* "Then it must be an intentional visibility policy."
Also wrong, because there is nothing left to make visible: the read path and the
row types already expose all seven (see the costs section below).

*Answer 3, and it is the strip's actual job.* **It stops `validateDoc`
rejecting every insert.** The committed descriptors mark `id`, `created_at` and
`updated_at` as `"required": true` **with no `default`**
(`examples/db-hitcounter/generated/zeroship/schema.runtime.json:9,20,31`; only
`version` carries `default: 1` at `:62-65`). `validateDoc` errors on exactly
that shape - `else if (def.required)` at `sdks/db/src/validate.ts:511-512`.

**MEASURED, with a control.** Two arms through `validateDoc`, differing only in
whether the descriptor's system fields are present in the schema. The field defs
were copied verbatim from the committed
`examples/db-hitcounter/generated/zeroship/schema.runtime.json`, so the arms test
the shape that ships rather than one invented for the probe:

| Schema | `validateDoc({ path: "/x" })` |
| --- | --- |
| stripped (today) | accepted, keeps `path` |
| unstripped (proposed) | **`Validation failed: id is required, created_at is required, updated_at is required`** |

`version` survives only because it is the one system field carrying a `default`
(`:62-65`), which the `if (def.default !== undefined)` branch at `:509` consumes
before the `required` branch is reached. That is the whole mechanism, and it is
why the fix belongs at the requiredness check rather than at the field list.

The strip's own introducing commit says so. `363b0edcb` added it alongside a
test named **"does not require runtime descriptor system fields in insert
input"** (`sdks/bootstrap/tests/install-schema.test.ts:223`), and never touched
`normalizeSchema`. The guard is about insert *requiredness*, not visibility and
not the refusal.

**Consequence: the two-edit design in this proposal does not work.** Deleting
`:192` and `:1072` red-bars every insert in the tree, including this proposal's
own acceptance criterion 6. The change additionally requires either the fold to
emit system fields as non-required, or `validateDoc` to understand that
platform-populated fields are never required *on input*. That is the real centre
of this work, and neither reviewer round found it until the second.

## Three defects found while establishing the above

**1. The refusal is keyed on the declared field name, not the resolved column.**
`:308` tests `SYSTEM_FIELD_NAMES.includes(key)` against the name as authored.
The default naming strategy is `naming.asIs` (`:1008`), but `naming.snakeCase`
is a supported opt-in (`:936`). Under `snakeCase`, a creator declaring
`createdAt` is **not refused** - `createdAt` is not in the list - and it resolves
to column `created_at`, the system column. Whatever policy replaces it must key
on the **resolved column name**.

**MEASURED.** `naming.snakeCase.toColumn` is
`s.replace(/[A-Z]/g, c => '_' + c.toLowerCase())` (`sdks/db/src/types.ts:347`),
and through `normalizeSchema`:

| Declared | Resolved column | Refused? |
| --- | --- | --- |
| `created_at` | `created_at` | **yes**, `RESERVED_SYSTEM_FIELD_NAME` |
| `createdAt` | `created_at` | **no** |
| `updatedBy` | `updated_by` | **no** |

The framing in the first draft was too generous to the current code. It is not
that the fence is "too strict for one spelling and blind to another": **both
spellings resolve to the same column**, so both collide identically, and the
fence catches exactly one of them.

**But no creator can reach this today, and the draft that said one could was
overstating it.** A reviewer traced reachability: both installers pass only
`{ descriptor }` (`runtime-entry.ts:152-156`, `dev-entry.ts:279-286`), the
default is `asIs` (`install-schema.ts:1008`, `collection.ts:200`), and
`installSchema` is framework-internal, so creators cannot pass `naming` at all.
**No production path activates `snakeCase`.** The only routes to the collision
are direct `model()` / `normalizeSchema` callers - i.e. tests.

Nor is the SDK the authoritative fence. Creator schemas are *migrations*, and
the engine refuses a colliding declaration twice
(`crates/zeroship-migrate-core/src/model/table_shape.rs:361-376`
`SystemColumnCollision`, and `.../schema/query.rs:511-531`). `createdAt` passes
both and becomes its own physical column `"createdAt"`, coexisting with
`created_at`. Two columns, no collision.

So defect 1 is a **latent footgun in an unreachable configuration**, not a live
hole. It stays in scope because the fix is cheap and the configuration is
supported-in-name; it drops out of the security argument entirely. If it is ever
wired, the failure is nastier than the draft said: writes land in the system
column, an UPDATE is refused with an error naming a field the creator never
typed, and reads never populate the creator's field, because the `autoFields`
loop overwrites the mapping (`collection.ts:203-221`, last write wins).

**2. Two system fields are already exposed, under different spellings.**
`model()` injects `deletedAt` at `:583` when soft-delete is on and `version` at
`:588` when versioning is on. `SYSTEM_FIELD_NAMES` is snake_case, so `deletedAt`
never collides with the `deleted_at` in the strip list. Creators can already see
and write two system fields today; the hiding is not even uniform.

**3. The refusal is bypassable by shape, so it is not a fence at all.** The
descriptor bypass at `:297` is gated on `!isTypeBuilder(rawVal) &&
isFieldDef(rawVal)`, and `isFieldDef` (`:233`) is *purely structural*:

```ts
function isFieldDef(value: unknown): value is FieldDef {
  return isPlainRecord(value) && typeof value.type === "string";
}
```

There is no provenance marker distinguishing a platform-generated descriptor
field from a creator-authored object literal. A creator who writes
`created_at: { type: "date" }` instead of `created_at: t.date()` takes the
bypass and is **never refused**. The comment at `:294-297` asserts the guard
keeps "user schemas on the strict path", but it only does so for users who
happen to use the `t.*` API.

This reframes the whole proposal. The restriction being removed **is not a
restriction**: it stops well-behaved creators and waves through anyone who types
a plain object. Removing it costs nothing that was being enforced, and the
argument for exposure no longer has to weigh against a real fence - because
there isn't one. The only real fence is `system_fields_pass.rs`, which is where
this proposal says it belongs.

Found by a reviewer, not by me, and it is the most valuable finding in the
round.

**MEASURED, not inferred.** Two pairs through `normalizeSchema`, each differing
in exactly one variable - builder versus plain object literal, same field name,
same declared type:

| Declaration | Result |
| --- | --- |
| `{ version: t.number() }` | throws `RESERVED_SYSTEM_FIELD_NAME` |
| `{ version: { type: "number" } }` | **accepted**, `{"version":{"type":"number"}}` |
| `{ created_at: t.number() }` | throws `RESERVED_SYSTEM_FIELD_NAME` |
| `{ created_at: { type: "number" } }` | **accepted**, `{"created_at":{"type":"number"}}` |

The first probe was asymmetric - it asserted rather than printed - and reported
a failing control with no way to say why. The cause was that `t.date()` does not
exist (the builders are `string, number, boolean, timestamp, json, array, ref,
object, vector, geoPoint, calendarDate, bytes, encrypted, literal, id, actor,
union`), so the control was testing a `TypeError`, not the fence. Printing both
arms instead of asserting one is what surfaced it.

## The design

**Expose all seven on the Collection.** Delete `stripRuntimeSystemFields`
(`:192`) and its call site (`:1072`).

~~System fields appear in generated types, in query results, in filters and in
sorts, like every other column.~~ **All four clauses are false, and a reviewer
refuted every one.** They are already in query results (the read path passes
them through, above), already in `Row<S>` (`types.ts:195`), and therefore
already in `Filter<S>` (`types.ts:285-291`) for filtering and sorting. Generated
`env.db.ts` omits them for an unrelated reason in an unrelated file:
`render-env-db.ts` keeps its **own private copy** of the seven names at `:73-81`
and elides them at `:129`, reading `schema.runtime.json` at build time and never
consulting the strip at all. Its comment at `:67-72` states the intent - they
are elided *because* `@zeroship/db` already infers them onto every row.

So the two edits above deliver exactly one observable change: system fields
reach the INSERT wire. Everything else the first draft promised was already
true. **This is an INSERT-write-classification change, not a visibility change**,
and it must be scoped, reviewed and sequenced as one.

**Three further edits the first draft missed**, each in a different file:

- `model()` still injects `deletedAt` at `install-schema.ts:582-584` under the
  guard `!normalized.deletedAt`, while the descriptor supplies `deleted_at`.
  With the strip gone, **both spellings land in `_schema`**, and under the
  default `naming.asIs` the camelCase one maps to a column that does not exist.
  The injections at `:582-589` must be deleted or re-keyed. (This also corrects
  defect 2's explanation: `version` is spelled identically to the system name,
  so the strip *does* remove it - it survives only because the injection runs
  afterwards. Only `deletedAt` escapes by spelling.)
- The `createdAt`/`created_at` collision is built in `collection.ts:203-221`,
  **not** at `install-schema.ts:308`: the schema loop maps declared fields, then
  the `autoFields` loop overwrites with the system names. Fixing the refusal
  alone does not fix it, and the overwrite means the creator's declared field is
  unreachable on read as well as on write.
- `RowInput<S>` bans the write-once fields with `id?: never` / `deleted_at?:
  never` (`types.ts:200-208`). The `writeOnce` class requires relaxing those.

**The system-field list exists in SEVEN places.** `zeroship-schema/src/query.rs:756`,
`zeroship-data-plan/src/projection.rs:58`, `install-schema.ts:249`,
`types.ts:174-182`, `types.ts:200-208`, `collection.ts:208-216`,
`render-env-db.ts:73-81` - plus the three-name subset at
`system_fields_pass.rs:46`. "Named in one place" is the right goal; this
proposal must state that there are seven to reconcile, not assume one.

**Replace the declaration refusal with a write classification.** Three tiers,
named in one place and enforced on both sides of the V8 boundary:

| Class | Fields | Creator may |
| --- | --- | --- |
| `writeOnce` | `id`, `created_at`, `created_by` | read, filter, sort, and **set on INSERT**; never on UPDATE |
| `defaulted` | `updated_at`, `updated_by`, `version` | the above, and supply a value on UPDATE, which wins over the auto-bump |
| ~~`managed`~~ | `deleted_at` | **this row was fiction - see below** |

**The `managed` tier does not exist in Rust, and a reviewer caught it.**
`check_keys_for_immutable_and_overrides` refuses the three immutables and hints
the three defaulted, then lets `deleted_at` fall through `_ => {}`
(`system_fields_pass.rs:438`) as an ordinary SET column. No fence exists in
`crud/mod.rs` either. So an UPDATE patch naming `deleted_at` soft-deletes or
resurrects a row today, bypassing `delete()` / `restore()` entirely. The claim
below that this table is "exactly the split the runtime already enforces" is
therefore false for one field of seven: `deleted_at` is `defaulted`-like and
unfenced. Either the `managed` tier needs Rust work this proposal must name, or
the table must describe what ships.

This is exactly the split the runtime already enforces. The proposal does not
invent a policy; it makes the SDK stop contradicting the one that ships.

**The first class is `writeOnce`, not `readonly`, and that correction is load-
bearing.** `inject_into_object` (`system_fields_pass.rs:250`) auto-mints `id`
only when absent (`:257`), injects `created_by` / `updated_by` only when absent
(`:268`, `:271`), and never injects `created_at` / `updated_at` / `version` /
`deleted_at` at all - its own comment at `:276-278` says "creator overrides flow
through when present". **The INSERT path accepts creator-supplied values for all
seven fields.** `IMMUTABLE_SYSTEM_FIELDS` is a post-INSERT fence, exactly as its
doc comment says at `:42-46` ("write-once on INSERT").

That is Salesforce's model precisely: audit fields are settable at create time
(their "Set Audit Fields" permission) and never updatable afterwards. Calling
these fields `readonly` would have described a restriction the runtime does not
implement, and would have invited someone to add one.

**Keep `id: t.id("prefix")`.** It is a prefix declaration, not an override, and
it is already sanctioned at `:309-321`.

**Refuse only genuine collisions, on the resolved column name.** A creator
declaring a field that resolves to a system column with a *different type or
meaning* is an error; a creator declaring `id: t.id("post")` is not. The check
moves after naming-strategy resolution so defect 1 cannot recur.

**Read-only is enforced in Rust, not TypeScript.** The SDK class drives types
and a fast error message; the fence stays `system_fields_pass.rs`. Per the
privilege invariant, a check that only exists in code the worker executes is not
a boundary - the TS side is ergonomics, the Rust side is the rule.

## What the strip actually costs, measured

The read path is **unaffected**. `mapResultDoc` (`sdks/db/src/utils.ts:28-34`)
iterates `Object.keys(doc)` - every key the native layer returned - and renames
each through `_toField`, which falls back to the identity
(`sdks/db/src/collection.ts:224`). It filters nothing. **System field values
already reach creator code today**; what the strip removes is the declaration
that says so.

**The strip costs exactly ONE thing, and both other candidates were my errors.**

1. ~~The type surface.~~ **WRONG, and a reviewer refuted it.** `Row<S>` is
   `InferSchema<S> & SystemFields` (`sdks/db/src/types.ts:195`), and
   `SystemFields` (`:174-182`) declares all seven unconditionally. Its own doc
   comment says "system fields are always present at read time". They are
   already typed, and `Filter<S>` keys on `keyof Row<S>` (`types.ts:285-291`),
   so they are already filterable and sortable too. The strip never touched any
   of this.
2. ~~Input validation.~~ **ALSO WRONG, and mine.** I wrote that `_knownFields`
   is built from the stripped schema, so `distinct("created_at")` would be
   refused. It is not: `collection.ts:208-221` re-adds all seven to
   `fieldToCol` / `colToField` unconditionally, *after* the schema loop, and
   `_knownFields` is built from the result at `:222`. `distinct` works today.
3. **Silent discard on write** - the `validateDoc` chain above. This is the
   whole of it.

**That single remaining cost is on the INSERT path, and it cuts both ways.**
`validateDoc` is not only dropping the `id` a creator wanted to set; it is the
only thing stopping a creator setting `created_by`. See the safety section
below, which the first draft got backwards.

## This is NOT safe to widen on its own. The first draft had it backwards.

The section this replaces said "exposure adds no write capability that does not
already exist" and "creators can already do so today, so this proposal does not
open the hole". **Both sentences are false, and a reviewer refuted them with a
measurement.** They confused *what the Rust layer accepts* with *what creator
code can reach*.

The Rust layer accepts creator values for all seven on INSERT - that part was
right. But creator code cannot get them there, because `validateDoc` drops
every key absent from `_schema`, and the strip is what removes them from
`_schema`. The reviewer's two arms, differing only in whether the seven are in
the field map handed to `model()`:

```
ARM A (today, strip applied):  system fields on the wire: []
ARM B (strip deleted):         system fields on the wire: all seven, including
                               "created_by":"usr_VICTIM" and "id":"post_FORGED"
```

So deleting the strip **is** a write-capability change, on `insert`,
`insertMany` and `upsert` (`crud.ts:200`, `:216`, `:416`). And there is no
second line of defence: `apply_system_fields_on_insert` returns `()`
(`system_fields_pass.rs:177-183`) and therefore **has no refusal arm at all**,
unlike its UPDATE sibling which returns `Result<_, DbError>` (`:340-344`). The
INSERT fence count goes from one weak in-process gate to **zero**.

`created_by` is the one that matters. It is the only value the platform stamps
from the request actor (`system_fields_pass.rs:153-162`), so a creator-nameable
`created_by` is a forgeable audit signal - the same shape as the DB-3 counter-
example in AGENTS.md.

**Consequence for sequencing.** The `created_by` narrowing that the last section
defers is not optional follow-up work; it is a precondition. `inject_into_object`
must stop honouring a creator-supplied `created_by` (and `id`, unless #126 is
deliberately granted) **in the same change** that removes the strip. Widening
first and narrowing later leaves a window in which the audit column is forgeable
by design, and "pre-launch" is not a reason to open it - the ground rules put
security first and forbid intermediate states built to be thrown away.

What remains true: the strip was never a *security* boundary by design - it is
an accident that it functions as one, and an accidental fence in code the worker
executes is exactly what the privilege invariant says not to rely on. The answer
is to build the real fence in Rust, not to keep the accidental one.

## This also explains task #126

#126 recorded that `insert()` silently discards a caller-supplied `id`, with the
observation solid and the mechanism unknown. The mechanism is now bounded: the
Rust path does **not** discard it. `system_fields_pass.rs:257` mints only
`if !obj.contains_key("id")`, so a supplied `id` survives the runtime untouched.
The discard therefore happens ABOVE Rust, in the SDK - and the only thing in the
SDK that removes system fields from a collection's shape is the refusal/strip
pair this proposal deletes.

**The path has now been traced end to end, and it is four steps with no gap.**

1. `stripRuntimeSystemFields` removes the seven system fields from the field
   record handed to `model()` (`install-schema.ts:1072`), so the `Collection`'s
   `_schema` does not contain `id`.
2. `insert()` calls `validateDoc(row, self._schema)` at
   `sdks/db/src/collection/crud.ts:200`.
3. `validateDoc` iterates **the schema, not the document**
   (`sdks/db/src/validate.ts:497`), under a comment that states the intent
   outright at `:496`: *"Only copy schema-defined fields - unknown fields are
   stripped for safety"*. A supplied `id` is not in the schema, so it is
   dropped here, silently and with no error.
4. `mapDocOutbound` (`crud.ts:201`) therefore never sees `id`, the native call
   never receives it, and `inject_into_object:257` mints a fresh one precisely
   because the key is absent.

Neither mapping function can be the culprit: `_toColumn` and `_toField`
(`sdks/db/src/collection.ts:223-224`) both fall back to the identity
(`?? field`), and `mapResultDoc` (`sdks/db/src/utils.ts:28-34`) iterates every
key of the returned row. Nothing else in the chain drops a key.

So #126 is a **side-effect fix of removing the strip**, by mechanism rather than
by hope: put the system fields back in `_schema` and step 3 stops dropping them.
This is the strongest practical argument for the change - the current design does
not merely hide the audit columns, it silently discards a value the creator
supplied. Acceptance criterion 6 still demonstrates it end to end; a traced
mechanism is not a passing test.

## The revised design, after two review rounds

The two-edit version is dead. What replaces it is one idea, not four patches:
**the write class becomes a property of the field in the descriptor, and every
layer reads it instead of keeping its own list of seven names.**

Add `writeClass` to the descriptor's `FieldDef`, emitted by the fold:

| `writeClass` | Fields | Input on INSERT | Input on UPDATE | Required on input |
| --- | --- | --- | --- | --- |
| `creator` | everything else | yes | yes | per `required` |
| `writeOnce` | `id`, `created_at` | yes | **refused** | never |
| `serverAuthored` | `created_by`, `updated_by` | **overwritten** | **overwritten** | never |
| `defaulted` | `updated_at`, `version` | yes | yes, wins over auto-bump | never |
| `lifecycle` | `deleted_at` | no | **refused** | never |

This resolves all four problems at once, each in the layer that owns it:

1. **Requiredness (criterion 0).** `validateDoc` skips the `required` check when
   `writeClass !== "creator"` (`validate.ts:511`). `required: true` keeps
   meaning "NOT NULL in storage", which is what the fold means by it; it stops
   meaning "the caller must supply it", which it never should have meant for a
   platform-populated column. No fold change, no per-name list.
2. **`created_by` forgery.** `inject_into_object` **overwrites** rather than
   injecting-when-absent for `serverAuthored`
   (`system_fields_pass.rs:266-273`). This is the narrowing that must land in
   the same change as the widening.
3. **`deleted_at`.** `check_keys_for_immutable_and_overrides` gains a
   `lifecycle` arm instead of falling through `_ => {}` (`:438`), so the
   `delete()` / `restore()` contract becomes real rather than documented.
4. **Seven copies of the list.** `types.ts:174-182`, `types.ts:200-208`,
   `collection.ts:208-216`, `install-schema.ts:249`, `render-env-db.ts:73-81`,
   `zeroship-schema/src/query.rs:756`, `data-plan/src/projection.rs:58` all
   become readers of one declared property. This is the part worth doing
   properly; a fifth reviewer would otherwise find an eighth copy.

**Enforcement stays in Rust.** The descriptor is produced by the migration
service and consumed by the worker, so `writeClass` is state a separate service
writes and the worker only reads - the one shape the privilege invariant permits.
The TS side mirrors it for types and error messages, and is not a fence.

**What this does not do.** It does not make `created_at` declarable by a creator
in a migration; the engine refuses that at `table_shape.rs:361-376` and this
proposal does not touch the engine. Criterion 1 is therefore scoped to the SDK
surface until a separate change decides whether creators may re-declare a system
column at all.

## Acceptance

0. **NEW, and it gates everything else.** `insert({ path: "/x" })` on a
   descriptor-installed collection still succeeds after the strip is removed.
   It does **not** today: `id` / `created_at` / `updated_at` are
   `"required": true` with no default, so `validateDoc:511` raises
   `ValidationError`. This is the criterion that proves the design's central
   edit is survivable, and the two-edit version of this proposal fails it.
1. A creator schema declaring `created_at: t.timestamp()` is accepted, and
   reads return the platform's value. (`t.date` does not exist - the date-ish
   builders are `t.timestamp()` and `t.calendarDate()`.) **Passable only as an
   SDK unit test as written**: a real creator declares schema in a *migration*,
   and the engine refuses the name at `table_shape.rs:361-376`. Making this true
   for an actual creator requires engine changes this proposal does not specify.
2. An UPDATE patch naming `created_at` is refused by the runtime, with the
   error surfacing through the SDK. **REGRESSION GUARD ONLY - it cannot fail.**
   `checkPartial` skips unknown keys (`validate.ts:613-614`, `if (!def)
   continue`), so the patch already reaches Rust today and `system_fields_pass.rs:425`
   already refuses it. Keep it; do not count it as evidence for the change.
3. An UPDATE patch naming `updated_at` is accepted and the supplied value wins
   over the auto-bump. **ALSO CANNOT FAIL**, for the same reason. Same status.
4. Under `naming.snakeCase`, a creator declaring `createdAt` is refused as a
   collision. Fails on today's code (defect 1, measured), but **not passable as
   written**: `normalizeSchema` has no access to the naming strategy - `model()`
   takes it at `:559` and calls `normalizeSchema(schema)` at `:579` without it.
   The check must move to where the strategy is known
   (`collection.ts:203-221`, where the collision is actually built), which is a
   signature change this proposal must specify rather than assume.
5. Generated types for a collection include all seven system fields, with the
   three `writeOnce` ones accepted in an INSERT payload and rejected in an
   UPDATE patch at the type level. **Not passable from the two edits named**:
   the row type already has all seven (`types.ts:195`), so the work is
   `render-env-db.ts:73-81`/`:129` for the generated literal and
   `types.ts:200-208` for the `RowInput` bans.
6. `insert({ id: "post_abc..." })` round-trips: the row is stored under the
   supplied id and `find` returns it. This is #126. Mechanism traced above;
   still to be demonstrated end to end.
7. **NEW, and the one that must gate the change.** `insert({ created_by:
   "usr_SOMEONE_ELSE" })` is **refused**, or the value is overwritten by the
   request actor. This must pass in the same change that removes the strip.
   Without it, criteria 1-6 can all pass while the audit column becomes
   forgeable - which is the state the first draft would have shipped.

## Where the safety argument is weakest

Widening a surface is easy to justify one field at a time and hard to justify
in aggregate, so state the residual plainly: after this change, creator code
can set `created_at` and `created_by` to any value on INSERT.

~~It can already do so today, so this proposal does not open the hole - but it
does make it discoverable.~~ **Refuted. It cannot do so today, and this proposal
DOES open the hole.** `system_fields_pass.rs:267` injects the actor only when
the key is absent, but the key can never be present, because `validateDoc`
removed it upstream. The distinction between "the Rust function would accept it"
and "creator code can deliver it" is the whole of the safety question, and the
first draft collapsed the two.

**The decision this forces, which the first draft deferred and must not.**
`created_by` should be *server-authored*, not `writeOnce`. The actor is known to
the runtime (`system_fields_pass.rs:153-162`), and letting app code name a
different one makes the column unusable as an audit signal. The first draft
argued this was separable - "worth doing separately so it is not smuggled in
under a proposal whose stated purpose is to widen". That reasoning was sound
only under the false premise that the hole was already open. It is not open, so
the narrowing is not a tidy-up that can follow; it is the precondition for the
widening being safe at all.

Revised classification, and the reason the table above lists `created_by` under
`writeOnce` with a caveat: `id` and `created_at` are genuinely write-once and
carry no trust; `created_by` is a platform assertion about identity and belongs
in a fourth class:

| Class | Fields | Creator may |
| --- | --- | --- |
| `serverAuthored` | `created_by` | read, filter, sort - never write |

`inject_into_object` must therefore **overwrite** `created_by` from the actor
rather than injecting only when absent, in the same change that removes the
strip. `updated_by` deserves the same treatment and the same argument; it is
listed as `defaulted` today only because the UPDATE path already honours a
creator value, which is a pre-existing hole this proposal should close rather
than inherit.
