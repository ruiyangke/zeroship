# plugin-db Completion — Native Primitives

Complete the `appbase.db.*` native primitives layer to match the full spec (`docs/specs/db.md`).

## Context

The DB plugin (`crates/plugin-db/`) currently implements 6 of 12 native primitives. The remaining 5 primitives (insertMany, updateMany, deleteMany, aggregate, distinct) and missing operators need to be added to provide full CRUD, aggregation, and query capabilities. `registerModel` is deferred to the auto-migration system.

## Files

- `crates/plugin-db/src/query.rs` — SQL builders
- `crates/plugin-db/src/callbacks.rs` — V8 callbacks
- `crates/plugin-db/src/lib.rs` — registration

## 1. New Primitives

### insertMany(collection, docsJson)

Returns array of inserted rows.

```sql
INSERT INTO "app_id"."table" ("col1", "col2")
VALUES ($1, $2), ($3, $4), ($5, $6)
RETURNING *
```

All docs must have the same keys. First doc defines the column set.

### updateMany(collection, filterJson, updateJson)

Returns `{ updated: N }`.

```sql
UPDATE "app_id"."table" SET "col" = $1
WHERE "filter_col" = $2
```

Same as `updateOne` but without the `ctid` LIMIT 1 subquery. Count affected rows from RETURNING.

### deleteMany(collection, filterJson)

Returns `{ deleted: N }`.

```sql
DELETE FROM "app_id"."table"
WHERE "filter_col" = $1
```

Same as `deleteOne` but without the `ctid` LIMIT 1 subquery. Count affected rows from RETURNING.

### distinct(collection, field, filterJson)

Returns flat array of unique values: `["val1", "val2", ...]`.

```sql
SELECT DISTINCT "field" FROM "app_id"."table"
WHERE ... ORDER BY "field"
```

### aggregate(collection, pipelineJson)

Pipeline is a JSON array of stages:

```json
[
  { "$match": { "status": "active" } },
  { "$group": { "by": "category", "count": { "$count": true }, "total": { "$sum": "price" } } },
  { "$having": { "count": { "$gt": 5 } } },
  { "$sort": { "total": -1 } },
  { "$limit": 10 }
]
```

Translates to one SQL query:

```sql
SELECT "category", COUNT(*) AS "count", SUM("price") AS "total"
FROM "app_id"."products"
WHERE "status" = $1
GROUP BY "category"
HAVING COUNT(*) > $2
ORDER BY "total" DESC
LIMIT 10
```

**Stage mapping:**

| Stage | SQL Clause | Implementation |
|---|---|---|
| `$match` | `WHERE` | Reuse `build_where()` |
| `$group` | `SELECT aggs GROUP BY cols` | Parse `by` + aggregation functions |
| `$having` | `HAVING` | Reuse `build_where()` on aggregate aliases |
| `$sort` | `ORDER BY` | Reuse `build_order_by()` |
| `$limit` | `LIMIT N` | Integer |

**Aggregation functions:**

| Operator | SQL |
|---|---|
| `{ "$count": true }` | `COUNT(*)` |
| `{ "$sum": "field" }` | `SUM("field")` |
| `{ "$avg": "field" }` | `AVG("field")` |
| `{ "$min": "field" }` | `MIN("field")` |
| `{ "$max": "field" }` | `MAX("field")` |

**`$group.by`**: string for single field, array for multiple fields.

## 2. Update Operators

Currently only `$set` and plain `field: value`. Add:

| Operator | SQL Expression |
|---|---|
| `$set` / plain | `"col" = $N` |
| `$inc` | `"col" = "col" + $N` |
| `$dec` | `"col" = "col" - $N` |
| `$mul` | `"col" = "col" * $N` |
| `$push` | `"col" = "col" \|\| to_jsonb($N::text)` |
| `$pull` | `"col" = "col" - $N` (JSONB minus) |
| `$addToSet` | `"col" = CASE WHEN "col" @> to_jsonb($N::text) THEN "col" ELSE "col" \|\| to_jsonb($N::text) END` |

**Parsing logic**: walk each key in the update object. If value is `{ $op: val }` where `$op` is a known operator, use the operator-specific SQL expression. Otherwise treat as plain `$set`.

Shared between `build_update_one` and `build_update_many` via a common `build_set_clauses()` helper.

## 3. Missing Filter Operators

Added to `build_field_condition()` in query.rs:

| Operator | SQL |
|---|---|
| `$ilike` | `"col" ILIKE $N` |
| `$search` | `to_tsvector('english', "col") @@ plainto_tsquery('english', $N)` |
| `$not` | Top-level operator: `NOT (sub_filter)`, reuses `build_where()` |

## 4. Projection (select)

`find(collection, filter, { select: ["name", "email"] })` generates:

```sql
SELECT "name", "email" FROM "app_id"."table" WHERE ...
```

Instead of `SELECT *`. Parsed from opts JSON in the `find` callback.

## 5. Implementation Order

Each step compiles and can be tested independently:

1. **Filter operators** — add `$ilike`, `$search`, `$not` to `query.rs` + tests
2. **Update operators** — extract `build_set_clauses()`, add `$inc/$dec/$mul/$push/$pull/$addToSet` to `query.rs` + tests
3. **insertMany** — `query.rs` builder + `callbacks.rs` callback + register in `lib.rs`
4. **updateMany** — `query.rs` builder + `callbacks.rs` callback + register
5. **deleteMany** — `query.rs` builder + `callbacks.rs` callback + register
6. **Projection** — add `select` param to `build_find()` + parse in `find` callback
7. **distinct** — `query.rs` builder + `callbacks.rs` callback + register
8. **aggregate** — `query.rs` pipeline builder + `callbacks.rs` callback + register

## 6. Verification

1. `cargo test -p appbase-plugin-db` — unit tests for all query builders
2. Build: `cargo build --release -p appbase-worker`
3. E2E test against running platform:
   - insertMany: batch insert 3 notes, verify all returned
   - updateMany: update all notes, verify `{ updated: 3 }`
   - deleteMany: delete by filter, verify `{ deleted: N }`
   - Update operators: `$inc` a counter, `$push` to array, verify
   - Aggregate: group + sum + count, verify results
   - Distinct: get unique values from a column
   - Projection: find with select, verify only requested fields returned
   - Filter: `$ilike`, `$search`, `$not` queries

## Deferred

- `registerModel` — requires schema manager / auto-migration system (Phase 2)
- Transactions — requires connection-level state management
- Populate/lookup — requires schema awareness (SDK layer)
- Soft delete — SDK wraps deleteOne with filter
- Cursor pagination — SDK adds cursor logic around find
- findOneAndUpdate/findOneAndDelete — convenience wrappers, add later
- Date functions ($dateTrunc, $extract) — add after core aggregation works
- Advanced aggregation ($first, $last, $collect, $countDistinct) — add after core
