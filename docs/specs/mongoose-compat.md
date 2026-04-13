# Mongoose Compatibility Spec

What @appbase/db must match from Mongoose so LLM-generated code works on first try.

## Schema Definition

### Two forms (both must work)

```js
// Shorthand — bare constructor
const users = model("users", {
  name: String,
  age: Number,
  active: Boolean,
  tags: [String],
});

// Explicit — object with type + options
const users = model("users", {
  name: { type: String, required: true, minlength: 1, maxlength: 100 },
  email: { type: String, required: true, unique: true, match: /^[^@]+@[^@]+$/ },
  age: { type: Number, min: 0, max: 150 },
  role: { type: String, enum: ["user", "admin"], default: "user" },
  bio: { type: String, trim: true },
  tags: { type: [String] },
  settings: { type: Object },
  birthday: { type: Date },
});
```

### Type mapping

| Mongoose | @appbase/db | Postgres |
|---|---|---|
| `String` | `"string"` | TEXT |
| `Number` | `"number"` | NUMERIC |
| `Boolean` | `"boolean"` | BOOLEAN |
| `Date` | `"date"` | TIMESTAMPTZ |
| `Object` / `Mixed` | `"json"` | JSONB |
| `[String]` / `[Number]` | `"array"` | JSONB |

### Validators (per Mongoose docs)

| Validator | Applies to | Mongoose syntax | @appbase/db status |
|---|---|---|---|
| `required` | all types | `required: true` | Implemented |
| `default` | all types | `default: value` or `default: fn` | Implemented |
| `min` | Number | `min: 0` | Implemented |
| `max` | Number | `max: 150` | Implemented |
| `minlength` | String | `minlength: 1` | Map to `min` |
| `maxlength` | String | `maxlength: 100` | Map to `max` |
| `match` | String | `match: /regex/` | Map to `pattern` |
| `enum` | String | `enum: ["a", "b"]` | Implemented |
| `trim` | String | `trim: true` | Deferred |
| `lowercase` | String | `lowercase: true` | Deferred |
| `uppercase` | String | `uppercase: true` | Deferred |
| `unique` | all types | `unique: true` | Implemented (schema only, enforced by DB) |
| `index` | all types | `index: true` | Implemented (schema only) |
| `immutable` | all types | `immutable: true` | Deferred |
| `select` | all types | `select: false` | Deferred |

### Auto-generated fields

Mongoose adds `_id`, `createdAt`, `updatedAt` automatically when `timestamps: true`.

@appbase/db always adds these (mapped from Postgres columns):
- `_id` ← `id` column
- `createdAt` ← `created_at` column (number, Unix ms)
- `updatedAt` ← `updated_at` column (number, Unix ms)

## Model Methods

### Must match Mongoose exactly

| Method | Mongoose signature | @appbase/db status |
|---|---|---|
| `create(doc)` | `Model.create(doc) → Promise<doc>` | Implemented |
| `insertMany(docs)` | `Model.insertMany(docs) → Promise<docs[]>` | Implemented |
| `findOne(filter)` | `Model.findOne(filter) → Query` | Implemented (returns Promise, not Query) |
| `find(filter)` | `Model.find(filter) → Query` | Implemented (returns Query thenable) |
| `findById(id)` | `Model.findById(id) → Query` | **Not implemented** — use `findOne({ _id: id })` |
| `updateOne(filter, update)` | `Model.updateOne(filter, update) → Promise<result>` | Implemented |
| `updateMany(filter, update)` | `Model.updateMany(filter, update) → Promise<result>` | Implemented |
| `deleteOne(filter)` | `Model.deleteOne(filter) → Promise<result>` | Implemented |
| `deleteMany(filter)` | `Model.deleteMany(filter) → Promise<result>` | Implemented |
| `countDocuments(filter)` | `Model.countDocuments(filter) → Promise<number>` | Implemented |
| `distinct(field, filter)` | `Model.distinct(field, filter) → Promise<arr>` | Implemented |
| `aggregate(pipeline)` | `Model.aggregate(pipeline) → Aggregate` | Implemented (returns Promise, not Aggregate) |
| `exists(filter)` | `Model.exists(filter) → Promise<{_id}|null>` | **Not implemented** |
| `findOneAndUpdate(filter, update, opts)` | Returns the document | **Not implemented** |
| `findOneAndDelete(filter, opts)` | Returns the document | **Not implemented** |
| `findByIdAndUpdate(id, update, opts)` | Shorthand | **Not implemented** |
| `findByIdAndDelete(id, opts)` | Shorthand | **Not implemented** |
| `bulkWrite(ops)` | Mixed operations | **Not implemented** |
| `where(path)` | Query builder chain | **Not implemented** |
| `populate(docs, opts)` | Eager load refs | **Not implemented** |

### Return value formats (must match Mongoose)

**updateOne / updateMany:**
```js
{ acknowledged: true, matchedCount: 1, modifiedCount: 1, upsertedCount: 0, upsertedId: null }
```
@appbase/db currently returns `{ matchedCount, modifiedCount }` — missing `acknowledged`, `upsertedCount`, `upsertedId`.

**deleteOne / deleteMany:**
```js
{ acknowledged: true, deletedCount: 1 }
```
@appbase/db currently returns `{ deletedCount }` — missing `acknowledged`.

**create:**
```js
// Returns the full document with _id, createdAt, updatedAt
{ _id: "uuid", name: "Alice", role: "user", createdAt: ..., updatedAt: ... }
```
@appbase/db matches this.

## Query Chain (find returns thenable)

### Must support

```js
await Model.find(filter)
  .sort({ field: 1 })       // 1=ASC, -1=DESC
  .sort("-field")            // string shorthand (- = DESC)
  .select("name email")     // space-separated include
  .select("-password")       // exclude with -
  .select({ name: 1 })      // object projection
  .limit(10)
  .skip(20)
  .lean()                    // return plain objects (default in appbase)
```

| Method | Mongoose | @appbase/db status |
|---|---|---|
| `.sort(obj)` | `{ field: 1 }` or `"-field"` | Implemented (object only, string shorthand missing) |
| `.select(str)` | `"name email"` or `"-password"` | Implemented (include only, exclude missing) |
| `.select(obj)` | `{ name: 1, email: 1 }` | **Not implemented** |
| `.limit(n)` | number | Implemented |
| `.skip(n)` | number | Implemented |
| `.lean()` | returns plain objects | No-op (always plain objects) |
| `.populate(path)` | eager load refs | **Not implemented** |
| `.exec()` | explicit execute | **Not implemented** (await works) |
| `.cursor()` | streaming | **Not implemented** |
| `.where(path)` | condition builder | **Not implemented** |

## Filter Operators

| Operator | Mongoose | @appbase/db status |
|---|---|---|
| `{ field: value }` | implicit `$eq` | Implemented |
| `{ field: { $eq: val } }` | explicit eq | Implemented |
| `{ field: { $ne: val } }` | not equal | Implemented |
| `{ field: { $gt: val } }` | greater than | Implemented |
| `{ field: { $gte: val } }` | greater or equal | Implemented |
| `{ field: { $lt: val } }` | less than | Implemented |
| `{ field: { $lte: val } }` | less or equal | Implemented |
| `{ field: { $in: arr } }` | in array | Implemented |
| `{ field: { $nin: arr } }` | not in array | Implemented |
| `{ field: { $exists: bool } }` | null check | Implemented |
| `{ field: { $regex: /pat/ } }` | regex match | **Not implemented** (use `$like`/`$ilike` instead) |
| `{ $and: [f1, f2] }` | logical AND | Implemented |
| `{ $or: [f1, f2] }` | logical OR | Implemented |
| `{ $not: filter }` | logical NOT | Implemented |
| `{ $nor: [f1, f2] }` | NOR | **Not implemented** |

## Update Operators

| Operator | Mongoose | @appbase/db status |
|---|---|---|
| `{ $set: { field: val } }` | set field | Implemented |
| `{ $unset: { field: "" } }` | remove field | **Not implemented** |
| `{ $inc: { field: n } }` | increment | Implemented |
| `{ $mul: { field: n } }` | multiply | Implemented |
| `{ $push: { field: val } }` | array append | Implemented |
| `{ $pull: { field: val } }` | array remove | Implemented |
| `{ $addToSet: { field: val } }` | array add unique | Implemented |
| `{ $pop: { field: 1 } }` | array pop | **Not implemented** |
| `{ $rename: { old: "new" } }` | rename field | **Not implemented** |

Note: @appbase/db also supports `$dec` (decrement) which Mongoose does not have. Mongoose uses `{ $inc: { field: -1 } }` for decrement.

## Aggregate Pipeline

| Stage | Mongoose | @appbase/db status |
|---|---|---|
| `{ $match: filter }` | filter docs | Implemented |
| `{ $group: { _id, ...accumulators } }` | group + aggregate | Implemented |
| `{ $sort: { field: 1 } }` | sort | Implemented |
| `{ $limit: n }` | limit | Implemented |
| `{ $skip: n }` | skip | **Not implemented** |
| `{ $project: { field: 1 } }` | projection | **Not implemented** |
| `{ $unwind: "$field" }` | flatten arrays | **Not implemented** |
| `{ $lookup: {...} }` | join | **Not implemented** |
| `{ $addFields: {...} }` | computed fields | **Not implemented** |

### Accumulator operators

| Operator | Mongoose | @appbase/db status |
|---|---|---|
| `{ $sum: "$field" }` | sum | Implemented |
| `{ $sum: 1 }` | count | Implemented (translated to `$count`) |
| `{ $avg: "$field" }` | average | Implemented |
| `{ $min: "$field" }` | minimum | Implemented |
| `{ $max: "$field" }` | maximum | Implemented |
| `{ $first: "$field" }` | first value | **Not implemented** |
| `{ $last: "$field" }` | last value | **Not implemented** |
| `{ $push: "$field" }` | collect into array | **Not implemented** |

## Error Format

Mongoose throws:
```js
// Validation error
{ name: "ValidationError", errors: { field: { message, path, kind, value } } }

// Duplicate key
{ name: "MongoServerError", code: 11000, keyPattern: { field: 1 } }
```

@appbase/db throws:
```js
// Validation — matches Mongoose shape
{ name: "ValidationError", errors: { field: { message, path } } }

// Duplicate key — code matches
{ code: 11000, message: "..." }
```

Missing from @appbase/db: `kind`, `value` in validation errors, `keyPattern` in duplicate errors.

## Gaps Summary (Priority Order)

### High (LLMs generate these frequently)
1. `findById(id)` — trivial alias for `findOne({ _id: id })`
2. `exists(filter)` — trivial wrapper around `countDocuments`
3. `minlength`/`maxlength`/`match` in schema — map to existing `min`/`max`/`pattern`
4. `acknowledged` in return values — add to updateOne/deleteOne results
5. `.sort("-field")` string shorthand
6. `.select({ name: 1 })` object projection
7. `.lean()` as no-op
8. `.exec()` as alias for then()

### Medium (common but not critical)
9. `findOneAndUpdate` / `findOneAndDelete` — atomic read-modify-return
10. `$unset` update operator
11. `$regex` filter (map to `$like`/`$ilike`)

### Low (advanced usage)
12. `populate()` — requires schema refs
13. `where()` builder chain
14. `bulkWrite()`
15. Aggregate: `$skip`, `$project`, `$unwind`, `$lookup`
16. `trim`/`lowercase`/`uppercase` schema options
