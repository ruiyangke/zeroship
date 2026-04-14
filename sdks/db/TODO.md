# @zeroship/db — TODO

## Critical — SDK fixes (done)

- [x] `sort`/`skip` key mismatch — SDK sends `sort`/`skip`, Rust reads `orderBy`/`offset` → rename in query.ts + globals.d.ts
- [x] Native error envelope not detected — Rust resolves with `{"error":"..."}`, SDK treats as valid doc → add detection in `parseRaw`
- [x] Dead code: `validatePartial` replaced by `checkPartial` → delete

## Critical — Rust fixes needed

- [x] `updatedAt` never updated on UPDATE — inject `updated_at = NOW()` in `build_set_clauses` (`query.rs:474`)
- [x] `$first` aggregator — `(array_agg(col))[1]` in `build_aggregate` (`query.rs:786`). TODO: thread `$sort` order into `array_agg ORDER BY` for guaranteed first-in-sort-order
- [x] Transaction errors — SDK now detects error envelope from begin/commit. Proper fix: add `OpResult::Failed` variant to Rust runtime so promises reject instead of resolving with error JSON (`runtime/src/state.rs:331`, `callbacks.rs:1069`)

## Important — Rust fixes needed

- [x] `$push`/`$addToSet` — use `$N::jsonb` instead of `to_jsonb($N::text)`, preserves number/boolean types (`query.rs:443-457`)
- [x] `$pull` — use `jsonb_array_elements` subquery to remove by value instead of `jsonb - text` key removal (`query.rs:447-452`)
- [x] `$set` top-level — flatten inline, process all keys together (`query.rs:399-410`)
- [x] `insertMany` — union all columns across all docs via BTreeSet (`query.rs:548-567`)
- [x] Numeric enum values in DDL CHECK — handle `as_i64()`/`as_f64()` (`query.rs:232-252`)

## Minor

- [ ] `naming.asIs` + Rust hardcoded `created_at`/`updated_at` = mismatch — document that `asIs` requires Rust-side quoted identifiers
- [ ] Spec "deferred" list is stale — update `docs/specs/db.md` to reflect implemented features
