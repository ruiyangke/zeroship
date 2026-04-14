# @zeroship/db — TODO

## Critical — SDK fixes (done)

- [x] `sort`/`skip` key mismatch — SDK sends `sort`/`skip`, Rust reads `orderBy`/`offset` → rename in query.ts + globals.d.ts
- [x] Native error envelope not detected — Rust resolves with `{"error":"..."}`, SDK treats as valid doc → add detection in `parseRaw`
- [x] Dead code: `validatePartial` replaced by `checkPartial` → delete

## Critical — Rust fixes needed

- [ ] `updatedAt` never updated on UPDATE — inject `updated_at = NOW()` in `build_set_clauses` or create BEFORE UPDATE trigger (`query.rs:477-599`)
- [ ] `$first` aggregator missing — add `"$first" => (array_agg(col))[1]` in `build_aggregate` (`query.rs:748-786`)
- [ ] Transaction errors resolve as `{"error":"..."}` instead of rejecting — change `beginTransaction`/`commitTransaction`/`rollbackTransaction` to reject on error, or at minimum have SDK detect the envelope (`callbacks.rs:1069`)

## Important — Rust fixes needed

- [ ] `$push`/`$addToSet` casts all values as text JSONB — `$push: 42` stores `"42"` not `42`. Use proper type-aware casting (`query.rs:444-457`)
- [ ] `$pull` uses `jsonb - text` (key removal) not element-by-value removal — need subquery: `(SELECT jsonb_agg(elem) FROM jsonb_array_elements(col) elem WHERE elem != to_jsonb($N))` (`query.rs:448-449`)
- [ ] `$set` top-level in native update drops all other keys — don't early-return, process all keys (`query.rs:400-407`)
- [ ] `insertMany` derives columns from first doc only — union all columns across all docs (`query.rs:534-570`)
- [ ] Numeric enum values skipped in DDL CHECK constraint — handle `as_i64()`/`as_f64()` (`query.rs:232-244`)

## Minor

- [ ] `naming.asIs` + Rust hardcoded `created_at`/`updated_at` = mismatch — document that `asIs` requires Rust-side quoted identifiers
- [ ] Spec "deferred" list is stale — update `docs/specs/db.md` to reflect implemented features
