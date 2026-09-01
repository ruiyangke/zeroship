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

## End to end, through a real app

`examples/db-hitcounter` is the whole problem at one-column scale.

**The creator writes one column**
(`migrations/20260711000000_create_hits.ts`):

```ts
table("hits").create({ columns: { path: t.text().notNull() } });
```

**The fold emits eight fields** (`generated/zeroship/schema.runtime.json`):

```
id           string  required=true   default=undefined
created_at   date    required=true   default=undefined
updated_at   date    required=true   default=undefined
created_by   string  required=undefined
updated_by   string  required=undefined
version      int     required=true   default=1
deleted_at   date    required=undefined
path         string  required=true
```

The `required=true, default=undefined` on the first three is what breaks inserts
the moment those fields enter the SDK schema. `version` is the sole exception,
and only because of its default.

**The generated types declare one field and index three others**
(`generated/zeroship/env.db.ts`):

```ts
hits: defineSchema({ path: t.string().required() })
  .index("hits_deleted_at_idx", ["deleted_at"])
  .index("hits_updated_at_idx", ["updated_at"])
  .index("hits_created_by_idx", ["created_by"]),
```

Three system columns named in the index specs of a schema that does not declare
them. The incoherence this proposal is about, visible in a single generated file.

**What an insert actually does today**, traced end to end:

| Step | Site | Effect |
| --- | --- | --- |
| strip | `install-schema.ts:1072` | `_schema` becomes `{ path }` |
| validate | `validate.ts:497` | iterates the schema, so only `path` survives |
| outbound | `crud.ts:201` | `{ path }` reaches the wire |
| Rust | `system_fields_pass.rs:257` | mints `id`; stamps `created_by`/`updated_by` from the actor |
| DB | column defaults | fills `created_at`, `updated_at`, `version` |
| return | `utils.ts:28` | copies every key - all eight come back |

**So the app's own handler already reads a field its schema does not declare.**
`src/index.ts` returns `inserted.data?.id`, and it works, because reads are not
filtered and `Row<S>` carries all seven (`types.ts:195`).

**The four things a creator can try, and what happens:**

| Attempt | Today |
| --- | --- |
| read `row.created_at` | works, and typechecks |
| filter / sort on `created_at` | works |
| `insert({ id: "hit_MINE" })` | banned by `RowInput` (`types.ts:200-208`); cast past it and the value is **silently dropped** |
| declare `created_at` in the migration | refused by the engine (`table_shape.rs:361-376`) |

Reads and types already work. Only writes are blocked, and one of them fails
silently. That is the entire delta this proposal addresses - and it is why the
change is an INSERT-write change wearing a visibility description.

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
4. **Nine copies of the list, not seven** - I predicted a reviewer would find an
   eighth and then found two myself. The seven declared ones
   (`types.ts:174-182`, `types.ts:200-208`, `collection.ts:208-216`,
   `install-schema.ts:249`, `render-env-db.ts:73-81`,
   `zeroship-schema/src/query.rs:756`, `data-plan/src/projection.rs:58`) all
   become readers of one declared property.

   **The two I missed are in PROSE, and they are the dangerous ones.**
   `crates/zeroship-plugin-db/src/error.rs:728-731` spells all seven inside a
   user-facing remediation string - *"System fields (id, created_at,
   updated_at, created_by, updated_by, version, deleted_at) are managed by the
   platform and cannot be overridden"* - and `:743-745` spells the write-once
   three - *"Fields `id`, `created_at`, `created_by` are write-once and set
   automatically on INSERT"*. Both go stale the moment `writeClass` reclassifies
   `created_by` as `serverAuthored`, and **no grep for the constant will find
   them**, because they are sentences. A creator would be told the platform
   manages a field the platform had just stopped managing.

   Count the tally as a FLOOR. The search that found these two keyed on files
   naming `created_by`, `updated_by` and `deleted_at` together; a copy that
   omits any one of the three is invisible to it.

**The engine is already the shape this design proposes, and that is the
strongest argument for it.** `table_shape.rs:360` iterates `&inject.columns`
from a `ResolvedInject` (`:150`) rather than matching a hardcoded literal, so
the migration engine already treats system columns as *data resolved once and
passed in*. The proposal is not inventing a pattern; it is extending the one the
authoritative layer already uses to the layers that hardcode instead.

~~**Enforcement stays in Rust.** The descriptor is produced by the migration
service and consumed by the worker, so `writeClass` is state a separate service
writes and the worker only reads - the one shape the privilege invariant
permits.~~

## ROUND 3 KILLED THIS TOO, AND THE FIRST DEFECT IS A SECURITY INVERSION

**The descriptor is CREATOR-SUPPLIED. The paragraph above is backwards in both
halves, and the tree says so in its own words.**

`crates/zeroship-migrate-server/src/apply.rs:61-65`:

```
/// WHAT THIS PROVES IS ORDERING, NOT TRUTH. The value is client-declared: a
/// creator who hand-edits both generated files can make them agree about a
/// lie. Closing that needs the server to re-render the descriptor from the
/// documents it just applied and refuse a mismatch, which is a separate
/// change and owes a byte-identity gate first.
```

`schema.runtime.json` is generated on the creator's machine
(`sdks/vite-plugin/src/gen-types/index.ts:43`), packed into the `.zship`
(`sdks/vite-plugin/src/zship.ts:527`), and the deploy gate is hash equality
against that same client-declared value
(`crates/zeroship-control/src/registry.rs:551`). The migration server never
re-derives it. `crates/zeroship-migrate-core/src/render/declarative.rs:160`
labels `FieldDescriptor` **"Untrusted"** in its own doc comment.

So putting the fence on `writeClass` **hands the attacker the fence's
configuration**. A creator edits one JSON file to
`"created_by": { ..., "writeClass": "creator" }`, the overwrite never runs, and
the audit column is forgeable. That is the DB-3 *shape* from AGENTS.md,
reintroduced by the fix for DB-3's sibling.

**Severity, stated honestly rather than at its most alarming.** The second
reviewer moderated this and the moderation is correct: the forgery is available
at **deploy time**, not at runtime. Running app JS cannot reach the descriptor -
it is bound natively before creator modules evaluate (`descriptor.rs:34-46`,
`lib.rs:383-397`) - so this is not DB-3's live privilege escalation. And nothing
privileged consumes `created_by` today. It is **audit-integrity**, and the reason
it still blocks the design is that an audit column a tenant can author is not an
audit column, which is precisely what the `serverAuthored` class was introduced
to establish.

The precedent for descriptor-trusting behaviour is already live:
`prefix_for_collection` (`system_fields_pass.rs:128-135`) honours a
descriptor-declared `idPrefix` **unvalidated**, so a descriptor claiming
`idPrefix: "usr"` mints platform-shaped user ids from the worker today.

**The correction is MONOTONIC, not replacement.** Rust keeps a hardcoded floor -
`IMMUTABLE_SYSTEM_FIELDS` plus a hardcoded `serverAuthored` set - and the
descriptor's `writeClass` may only **narrow** capability, never widen it. There
is precedent for exactly this: `render/gen_types.rs:356-359` stamps
`readable`/`filterable`/`sortable`/`projectable` as descriptor-declared booleans
that are unconditionally `true`, so a tampered `false` can only restrict.

**And the cost of that correction must be stated, because it deletes the
design's headline benefit.** Under intersection, the two readers that are
*fences* keep their own hardcoded lists - the duplication does not collapse for
them. That is the right outcome, not a regression: **a security list must be
duplicated in the trusted layer; only the ergonomic ones may be derived.** The
proposal's "seven lists become readers of one property" is therefore wrong as
stated, and the honest version is "the ergonomic lists collapse; the fences stay
hardcoded, deliberately, and gain a comment saying why".

**Second defect: the predicate is fail-open.** `writeClass !== "creator"` was the
gate. Nothing stamps `"creator"` - `TypeBuilder.toFieldDef()` (`types.ts:1142`)
carries no write class, nor do the injections at `install-schema.ts:583`/`:588`,
nor the raw-object bypass at `:297`. So `undefined !== "creator"` is `true` for
every creator field and `required` stops being enforced anywhere. It must gate on
the explicit non-creator classes. The proposal also contradicted itself: `:387`
says the fold emits it, and the requiredness item said "No fold change".

**Third defect: gating `:511` is not enough.** `validate.ts:508-510` materialises
`def.default` *before* the required check, so `version` (the one system field
with a default) would ship `version: 1` on the wire from every SDK insert, for a
column the pass deliberately leaves to the DDL default
(`system_fields_pass.rs:274-278`). And `validateUnionDoc` is a **second**
`required` site (`validate.ts:578`) that the fix never reaches: it builds its
result from the matched *variant* map (`:567`), and system fields are top-level
entries, never variant entries, so **for union collections #126 stays broken
after the strip is removed**.

**Third-and-a-half, and it is a DATA-INTEGRITY regression: every upsert would
reset `version` to 1.** This follows from the `:508` defect above and is worth
stating separately because it is silent corruption rather than a failed call.
Once `validateDoc` materialises `version: 1` onto the payload:

- `zeroship-schema/src/query.rs:6297` excludes only `id | created_at |
  created_by` from the `DO UPDATE SET` list, so `version` gets
  `"version" = EXCLUDED."version"` - that is, `1`.
- `:6313` guards the auto-bump `"version" = COALESCE(...) + 1` behind
  `if !doc_has_version`, which is now false.

The comment at `:6322-6326` states the invariant being broken, verbatim: *"Every
PG upsert took this branch: the insert-side system-fields pass deliberately
leaves `version` to the DDL default, so `doc_has_version` is false on the
dispatch path."* So every SDK upsert onto an existing row would reset the
optimistic-concurrency counter, making a stale CAS predicate match again. This
is the sharpest argument that the gate must cover `:508` as well as `:511`: a
non-`creator` field must neither be required nor have its default materialised
client-side, **because the platform's default is the DDL's**.

**Fourth: this is a v3 descriptor, not a v2.** `install-schema.ts:68-74` states
the rule the version number encodes - a consumer that stops deriving something
itself, and instead depends on a property being present on every field, is
exactly the situation that moved v1 to v2. A layer that deletes its list of seven
and relies on `writeClass` being present is that situation. The proposal
specified no bump.

**Fifth: at least 24 lists, not nine.** Including a *third* in `types.ts`
(`:2196-2205`, `_knownFieldNames`), three per-dialect DDL emitters in
`zeroship-schema/src/query.rs` (`:208`, `:322`, `:449`), an unnamed second copy
of `IMMUTABLE_SYSTEM_FIELDS` in the upsert exclusion (`query.rs:6296`), and the
migration engine's charter `policies/confined-system-shape.inject.toml:96-102`.
Two of them **cannot** read a descriptor property even in principle:
`data-plan/src/projection.rs:53-56` deliberately takes no dependency on
`zeroship-schema`, and the charter is consumed at migration-apply time, before a
descriptor exists.

## The defect that outlives every version of this design

**The three timestamp columns cannot be written back at the type they are read
out as, and no version of `writeClass` touches the code that refuses them.**

- Read emits numbers: `backend/pg_row_json.rs:121-137` decodes TIMESTAMP to
  `Value::Number(unix_ms)`, and `crud/read_pipeline.rs:162` normalises
  `created_at` / `updated_at` / `deleted_at` by **hardcoded name** on every
  backend.
- The types agree: `sdks/db/src/types.ts:175-176` declares
  `created_at: number; updated_at: number`.
- Write refuses numbers: `sdks/db/src/validate.ts:277-284` - a `date` or
  `timestamp` field requires `value instanceof Date` or a parseable ISO string,
  else *"must be a Date or ISO 8601 date string"*.

The committed descriptors type all three as `"type": "date"`. So the moment the
strip is gone, reading a row and writing it back throws. This invalidates
acceptance criterion 1's own example (`created_at: t.timestamp()` typechecks as
`number` and fails at runtime) and **falsifies the "CANNOT FAIL" annotation on
criterion 3**: that annotation rests on `checkPartial` skipping unknown keys
(`validate.ts:613-614`), which is true only while the strip exists. Once
`updated_at` is a known key, `checkField` runs at `:616` and refuses the number.

`writeClass` cannot help: it gates the `required` branch, and `checkField` fires
from `:517` whenever a value is *present*, independent of `required` entirely.

**The strip has been masking a pre-existing `date` round-trip asymmetry in
`@zeroship/db`.** Fixing that asymmetry is a precondition for this proposal, not
a consequence of it, and it is the one defect that survives every redesign so far
because it lives in neither the strip nor the descriptor.

## The `serverAuthored` fix has an invisible hole, one authentication state over

**Patched where the code lives, the overwrite leaves anonymous requests
forgeable - and the test that looks like it guards this stays green.**

`inject_into_object:263-273` wraps the whole actor block in `if let Some(actor)
= actor_id`, with the comment *"No actor -> leave absent so the DDL's `NULL`
default fires."* The natural reading of "overwrite rather than inject-when-absent"
is to flip the `!obj.contains_key(...)` guards **inside that Some-arm**. Do that
and the `None` arm is still a passthrough: an app serving an unauthenticated
request delivers a creator-supplied `created_by: "usr_VICTIM"` to the wire
untouched.

**And the existing test cannot catch it.**
`insert_leaves_created_by_absent_when_no_actor` (`:707-719`) asserts the doc has
no `created_by` after the pass - but it feeds in `{"title": "hi"}`, which never
had one. It pins *"no actor -> no injection"*, not *"no actor -> a supplied
value is removed"*. It stays green through the naive patch, and criterion 7's
new test will be written with an actor bound, because every test in that file
binds one.

**So the specification must be explicit:** for a `serverAuthored` field the
supplied key is **removed unconditionally**, and then the actor - or nothing -
replaces it. "Overwrite" is not a sufficient instruction.

## The gate must DELETE the key, not skip the check

A second implementation constraint, from the same review.
`validateDoc` copies the value **before** classifying it missing
(`validate.ts:497-498`: `if (key in doc) result[key] = doc[key]`, then
`const value = result[key]`). The `missing` branch ends in `continue`
(`:515`), which leaves that copy in `result`.

So a gate that merely *skips the required check* lets a creator-supplied
`created_at: null` ride the wire as an explicit NULL into a `NOT NULL` column
(`query.rs:212` emits `created_at TIMESTAMPTZ NOT NULL`), turning today's silent
drop into a 23502 at the database. The gate must remove the key from `result`.

## And criterion 5 would silently delete `| null` from three columns

Un-eliding the seven from the generated `env.db.ts` - which criterion 5 describes
as a two-line change at `render-env-db.ts:73-81`/`:129` - **erases nullability
from `created_by`, `updated_by` and `deleted_at`, with no compiler diagnostic.**

Today `Row<S> = InferSchema<S> & SystemFields` (`types.ts:195`), the seven are
elided from the generated literal, and `SystemFields` (`types.ts:174-182`) is
their sole source: `created_by: string | null`, `updated_by: string | null`,
`deleted_at: number | null`.

Un-elide, and the renderer emits builders from the descriptor. Those three carry
**no `required`** in the committed descriptor, so they render `t.string()` /
`t.timestamp()` - optional keys. The intersection then computes:

```
(string | undefined) & (string | null)
  = (string & string) | (string & null) | (undefined & string) | (undefined & null)
  = string
```

`| null` is gone. **Intersection narrows rather than errors**, so nothing fires
anywhere in the build.

The columns really are nullable - `zeroship-schema/src/query.rs:212-217` emits
`created_by TEXT ... NULL`, `updated_by TEXT ... NULL`, `deleted_at TIMESTAMPTZ
NULL`, and the charter agrees (`policies/confined-system-shape.inject.toml:99-102`,
`nullable = true` for exactly those three). So every
`if (row.deleted_at === null)` soft-delete check in creator code becomes a
comparison TypeScript believes is impossible, against a column that is NULL in
practice for every live row.

Making criterion 5 correct requires either dropping `SystemFields` from `Row<S>`
when the schema declares them, or emitting a nullable marker the renderer does
not currently have. It is not a two-line deletion.

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
