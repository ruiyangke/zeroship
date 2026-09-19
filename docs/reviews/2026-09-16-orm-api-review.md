# ORM API review — bug log

Findings from the 2026-09-16 review of `zeroship-data-orm` / `@zeroship/db` /
`@zeroship/migrate`, focused on API consistency and ergonomics. Each item names
the owning code, quotes the evidence, and proposes a fix. Ordered by priority;
P1 items are correctness hazards on the creator-facing surface.

---

## P1 — correctness hazards

### BUG-1: `t.string()` means two different physical types in the two schema DSLs

**Status:** decided; the first implementation step has landed in the working tree
(not yet committed), with two recorded deviations. The design is
`docs/proposals/2026-09-16-shared-schema-builder.md`.
Landed: one shared lexicon in `@zeroship/schema`, the vendored copy in
`packages/zero-migrate/src/db-types.ts` deleted, and the builder-aliasing bug
this entry led to fixed by clone-on-modify. Deviations from that proposal's
step 1 as it was first written: (a) `packages/zero-migrate/src/db-lexicon.ts`
stays in migrate - it returns migrate's own `ColType`, so moving it into the leaf
would make the two packages reference each other; it moves with the token rename
instead. (b) "No public spelling changes" holds for CREATOR-FACING builders but
not for migrate's re-exported bridge surface (`dbType.date()` and `.optional()`
are gone, `TypeName` lost a member) - pre-launch, and nothing in the repo called
them. The spelling renames this entry is actually about are rollout step 3 and
remain open.

**Where**: `packages/db/src/types.ts` (`t.string()` factory) vs
`packages/zero-migrate/src/types.ts` (`TypeLexicon.string`).

**What**: `@zeroship/db`'s `t.string()` is unbounded `TEXT`.
`@zeroship/migrate`'s `t.string(opts)` is a **bounded `VARCHAR(N)`** with
`length` defaulting to 255; unbounded text there is `t.text()`. Same name,
same conceptual family, different physical column.

**Also colliding**: `t.integer()` exists in `@zeroship/db` but was deleted in
`@zeroship/migrate` in favor of `t.int()`; `t.calendarDate()` (db) vs `t.date()`
(migrate) — and `docs/reference/db.md` explicitly says "`t.date()` is not in
the surface" while migrate's lexicon has exactly that factory. Nullability is
spelled `.required()` in one and `.notNull()` in the other. Foreign keys are
`t.ref("users", { column, relation })` in one and
`.references("users", "id", { relation })` in the other.

`packages/zero-migrate/src/db-lexicon.ts` proves the underlying `FieldDef`
lexicon is shared and calls the db-side `ref` arm "legacy", so the divergence
is known but unreconciled.

**Why it hurts**: a creator who learns one dialect writes the other and gets a
silent physical-type difference. This is the first surface every creator
touches.

**Fix**: converge on one spelling per concept across both packages (facet sets
may differ — DDL vs validation). Rename on one side: either db's `t.string` →
`t.text`, or migrate's `t.string` → `t.varchar`. Unify `integer`/`int` and
`calendarDate`/`date`. Pick one of `.required()` / `.notNull()`.

---

### BUG-2: `update(filter, patch)` updates exactly one row; the name hides it

**Where**: `packages/db/src/collection.ts` (`Collection.update`),
`docs/reference/db.md` ("By filter (returns the lowest-id match, or null)").

**What**: `db.users.update({ email: "alice@..." }, patch)` updates the
lowest-id matching row, not all matches. `delete({ filter })` behaves the same
way. SQL `UPDATE … WHERE` hits all rows; Mongo distinguishes
`updateOne`/`updateMany` by name; Prisma's `update` requires a unique filter.

**Why it hurts**: every reference frame a user brings predicts different
behavior. Most likely source of a creator shipping a silent partial-update
data bug.

**Fix**: rename the filter forms to `updateOne` / `deleteOne` (matching the
Mongo grammar the SDK already borrows), or refuse non-unique filters. No
back-compat alias — pre-launch rename in one PR.

---

### BUG-3: JS timestamp reads are lossy on PostgreSQL; read-then-filter misses rows

**Where**: `docs/architecture/data-orm.md` ("The V8 adapter keeps JavaScript's
millisecond contract… a JavaScript caller that reads a PostgreSQL instant and
filters by equality on it can still miss the row it read"),
`docs/reference/sqlite-divergences.md` ("Timestamps in JavaScript" row).

**What**: PostgreSQL stores microseconds; the V8 adapter floors outbound
instants to whole milliseconds. `find({ createdAt: row.createdAt })` can miss
the very row `row` came from. SQLite is exact because it stores no finer
value, so dev never reproduces it.

**Why it hurts**: equality on a value you just read is the most natural query
imaginable, and it silently returns wrong answers — in production only.

**Fix**: return a branded opaque `Timestamp` in JS (comparable, round-trips
losslessly through filters) instead of a raw floored number, or refuse
point-equality filters on floored timestamp values and direct callers to
ranges. Silent misses are the worst of the three options.

---

### BUG-4: `Query` terminals mutate shared builder state; concurrent terminals race

**Where**: `packages/db/src/query.ts` — `first()`, `unique()`, `last()` each
assign `this._limit` / `this._sort` and restore in `finally`.

**What**: `first()` sets `_limit = 1`, `unique()` sets `_limit = 2`, `last()`
reverses `_sort`. All three mutate the same `Query` instance.

```ts
async first(): Promise<Result<P | null>> {
  const prevLimit = this._limit;
  this._limit = 1;
  ...
}
```

**Why it hurts**: `await Promise.all([q.first(), q.last()])` interleaves the
mutations and can return wrong windows. The failure is timing-dependent and
will look like a flaky query, not a race.

**Fix**: terminals should derive a frozen snapshot of the builder state
instead of mutating `this` (clone-and-execute).

---

### BUG-5: mask kinds `last4` / `first4` / `name` fail open on unexpected shapes

**Where**: `docs/reference/db.md` ("The eight mask kinds" — "On an unexpected
shape" table), implementation in
`crates/zeroship-data-orm/src/protection/mask_pass.rs`.

**What**: string-oriented mask kinds are not type-checked at deploy or at
write. `email`/`dateYear`/`dateDecade` fall back to full redaction (safe), but
`name` still emits initials and `first4`/`last4` perform no shape check at
all — a `last4` aimed at a date column reveals its trailing digits.

**Why it hurts**: a protection feature that reveals characters from whatever
column it is pointed at. "Verify masks by reading a masked row back" is not a
control.

**Fix**: validate mask kind against the declared field type at deploy; refuse
kinds whose shape contract the type cannot satisfy.

---

## P2 — consistency defects

### BUG-6: two error-code vocabularies; the docs disagree with each other

**Where**: `packages/db/src/errors.ts` (`canonicalErrorCode` +
`CANONICAL_CODE_OVERRIDES`), `docs/reference/db.md` (Errors table),
`docs/reference/sqlite-divergences.md`.

**What**: the native layer emits snake_case codes (`concurrency_mismatch`,
`unsupported_isolation_level`, `vector_unsupported_metric`); the SDK
canonicalizes to SCREAMING_CASE (`OPTIMISTIC_CONCURRENCY`, …) with a
hand-maintained override table. db.md's error table uses the canonical forms;
sqlite-divergences.md tells users to expect the snake_case originals. db.md
also demonstrates both `error?.name === "OptimisticLockError"` and
`error.code === "..."` as branching styles.

**Why it hurts**: users branch on `error.code` in production; the docs cannot
tell them which alphabet they are matching.

**Fix**: declare the canonical SCREAMING form the one public contract, sweep
every doc to it, teach `code` branching only, and mark `name`-matching an
anti-pattern (`instanceof` is cross-isolate-fragile; the `Symbol.for` brand
exists for that reason).

---

### BUG-7: bulk-write count shapes are asymmetric, and the doc is stale

**Where**: `packages/db/src/collection.ts` + `packages/db/src/collection/crud.ts`
(`updateManyCollection` returns `{ count: n }`), `docs/reference/db.md`
("counts = { matchedCount: N, modifiedCount: N }").

**What**: `updateMany` → `{ count }`, but `deleteMany` → `{ deletedCount }`,
`purgeMany` → `{ purgedCount }`, `restoreMany` → `{ restoredCount }`. The doc
describes a `{ matchedCount, modifiedCount }` shape the code never produces.

**Fix**: pick one convention — `{ updatedCount }` for family symmetry, or
`{ count }` everywhere — and correct the doc.

---

### BUG-8: `aggregate` pipeline looks like MongoDB but isn't

**Where**: `docs/reference/db.md` (Aggregate section).

**What**: the stage grammar borrows Mongo spelling (`$match`, `$group`,
`$sort`, `$limit`) but `$group` takes `{ by: "country", count: { $count: true } }`
instead of Mongo's `{ _id: "$country", count: { $sum: 1 } }`, and there is a
`$having` stage Mongo does not have (post-group filtering in Mongo is another
`$match`).

**Why it hurts**: the syntax courts exactly the users whose muscle memory it
breaks. A Mongo user writes `_id:` and fails.

**Fix**: either hew to real Mongo grammar (`_id`, drop `$having`) or rename
the stages so the grammar stops impersonating Mongo (`$groupBy` + `$having`
reads as SQL-in-JSON, which is what it is).

---

### BUG-9: Rust read-builder fallibility is scattered arbitrarily across the chain

**Where**: `crates/zeroship-data-orm/src/orm/read_builder.rs` —
`filter`/`order_by`/`group_by`/`having` return `Self`;
`limit`/`select`/`for_update`/`for_update_of` return `Result<Self, DbError>`;
predicate constructors (`Field::gte`, `in_values`, …) also return `Result`.

**What**: one expression carries three error surfaces:

```rust
posts.query()
    .filter(schema::posts::score.gte(Some(min))?.and(schema::posts::title.in_values(t)?))
    .order_by(schema::posts::score.desc().nulls_last())
    .limit(page_size)?
    .all().await?
```

**Why it hurts**: the `?` placement feels arbitrary; Diesel needs none
(type-level), SeaORM/sqlx need one (at execute). Eager validation is
defensible, but the rule is nowhere stated.

**Fix**: make `limit` infallible (validate at execution) so chaining is
uniformly infallible and constructors are the single eager-validation point;
document the rule either way.

---

### BUG-10: dev tier cannot reproduce production transaction behavior

**Where**: `docs/reference/sqlite-divergences.md` ("Transaction isolation
under contention", "Concurrent `db.transaction()` calls", "How a transaction
opens").

**What**: three invisible divergences —

- `SERIALIZATION_FAILURE` is unreachable on `pnpm dev` (one thread, one
  writer; contending pairs always both succeed locally, the loser aborts in
  production).
- A second concurrent `db.transaction()` is queued in production but refused
  instantly in dev with `TRANSACTION_CONNECTION_BUSY` — different ceiling and
  different response.
- SQLite opens explicit transactions with `BEGIN IMMEDIATE`, so a read-only
  transaction still takes the write lock in dev.

**Why it hurts**: the doc's own advice ("do not treat a clean local run as
evidence that a transaction is conflict-free") concedes `pnpm dev` cannot
validate transactional correctness. Bugs of this class ship invisible.

**Fix**: add a dev option that simulates serialization aborts (fail a
configurable fraction of contending commits), and scaffold the dual-tier test
pattern from `examples/db-todos/tests/database.test.ts` into
`create-zeroship-app` so it is a default, not a discovery.

---

## P3 — surface warts

### BUG-11: synthetic distance columns are inconsistent

`Collection.search` returns `_distance?: number` (optional, unitless);
`Collection.near` returns `_distance_m: number` (required, snake_case with a
unit suffix) — see `packages/db/src/collection.ts`. Pick one convention for
name, optionality, and unit suffix.

### BUG-12: `bulkUnmask` returns a `Map`

`Collection.bulkUnmask` → `Result<Map<RowId<S>, Record<string, unknown>>>` —
the only non-JSON-serializable return in the SDK, on a platform whose RPC
layer serializes results. Return a plain record (or an array of
`{ id, values }` pairs).

### BUG-13: three pagination APIs, one redundant

`skip`/`limit` offset, `.after(id)` id-seek, and `.paginate()` cursor envelope
coexist on `Query` (`packages/db/src/query.ts`). `.after` is a strict subset
of `paginate`. Deprecate and remove it.

### BUG-14: string-form `.sort("-score title")` is untyped

`Query.sort` accepts a space-separated string alongside the typed object form
(`packages/db/src/query.ts`). Typos in string form silently sort by nothing
useful. Remove the string form; the object form already covers multi-key
sorts.

### BUG-15: `aggregate()` result is fully untyped

`Collection.aggregate(pipeline)` → `Result<PlainObject[]>`
(`packages/db/src/collection.ts`). Parity with Mongoose, behind Prisma/Drizzle.
Type the result from the stage generics, at least for the terminal
`$group`/`$project` shape.

### BUG-16: type-only `declare` accessors vanish at runtime

`Collection` declares `declare readonly Id` / `declare readonly RowInput`
(`packages/db/src/collection.ts`) so `typeof db.users.Id` works as a type, but
`db.users.Id` is `undefined` at runtime. Documented ("do not read them at
runtime") but unguarded. Consider a runtime getter that throws with a clear
message.

### BUG-17: `t.number()` maps to `DOUBLE PRECISION`

`docs/reference/db.md` field-type table. A float is the classic money footgun;
Prisma defaults to `Decimal` for a reason. Keep the mapping but make
`t.numeric({ precision, scale })` the spelling used in every doc example that
stores amounts.

---

## Fix order

1. BUG-1 (two `t` dialects) — creator-facing correctness, first surface touched.
2. BUG-2 (`update` single-row semantics) — silent data corruption potential.
3. BUG-3 (timestamp round-trip) — silent wrong answers, production-only.
4. BUG-4 (Query terminal race) — real concurrency bug, small fix.
5. BUG-5 (fail-open masks) — security feature failing open.
6. BUG-6 (error-code vocabulary) — production branching contract.
7. BUG-7 (bulk count shapes + stale doc) — confirmed doc/code mismatch.
8. BUG-8, BUG-9, BUG-10 — grammar/ergonomics/testability.
9. BUG-11 through BUG-17 — surface polish, batch into one cleanup PR.
