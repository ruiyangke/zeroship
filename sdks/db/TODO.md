# @zeroship/db — TODO

All critical and important issues have been resolved. Remaining items are future enhancements.

## Completed

- [x] `sort`/`skip` key mismatch → `orderBy`/`offset` (matches Rust)
- [x] Native error envelope detection in `parseRaw` and transaction begin/commit
- [x] Dead `validatePartial` removed (replaced by `checkPartial`)
- [x] `updatedAt` auto-update: `"updated_at" = NOW()` injected in every UPDATE
- [x] `$first` aggregator: `(array_agg(col))[1]`
- [x] Transaction error detection via envelope parsing
- [x] `$push`/`$addToSet`: `$N::jsonb` preserves number/boolean types
- [x] `$pull`: `jsonb_array_elements` subquery removes by value
- [x] `$set` top-level: flatten inline, process all keys
- [x] `insertMany`: union all columns across all docs via BTreeSet
- [x] Numeric enum values in DDL CHECK constraint
- [x] Spec updated: `model()` → `createDb()`, deferred list refreshed

## Future Enhancements

- [ ] `$first` sort-order threading — inject preceding `$sort` into `array_agg ORDER BY` for guaranteed order
- [ ] `OpResult::Failed` in Rust runtime — proper promise rejection instead of error envelope (`runtime/src/state.rs:331`)
- [ ] `naming.asIs` requires Rust-side quoted identifiers for `created_at`/`updated_at` auto-columns
- [ ] `findOneAndUpdate` / `findOneAndDelete` — `UPDATE ... RETURNING *`
- [ ] Cursor pagination — `{ after: lastId }` → `WHERE id > $1 LIMIT $2`
- [ ] Upsert — `INSERT ... ON CONFLICT DO UPDATE`
- [ ] Type-safe `select()` return type narrowing (Prisma-style)
- [ ] Transaction isolation levels (`SERIALIZABLE`, `REPEATABLE READ`)
- [ ] Populate / lookup (JOINs)
- [ ] Soft delete (`deletedAt` + automatic filtering)
