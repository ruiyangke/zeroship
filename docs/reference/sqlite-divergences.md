# SQLite Divergences

This document lists the intentional SQLite-vs-Postgres differences for
`plugin-db` after the parity work. Tier-1 CRUD/read-shape/system-field
behavior should match; the items below are the remaining documented
exceptions.

## Tier 2

| Area | Postgres | SQLite | Contract |
| --- | --- | --- | --- |
| Vector ranking | `pgvector` operator classes | `sqlite-vec` `vec0` MATCH ranking | Assert top-k membership; near-tie order may differ. |
| Full-text ranking | `ts_rank` | FTS5 `bm25()` | Assert matching docs; ranking order may differ. |
| Geo `near` ordering | indexed geo operators | haversine flat scan | Assert result-set membership for clear separations; boundary ordering may differ. |

## Tier 3

| Area | Postgres | SQLite | Notes |
| --- | --- | --- | --- |
| Write concurrency | MVCC, multiple concurrent writers | single-writer WAL mode | SQLite may return `SQLITE_BUSY` where Postgres would serialize differently. |
| Isolation-level hint | honors `READ COMMITTED` / `REPEATABLE READ` / `SERIALIZABLE` | ignores the SDK isolation-level hint and issues plain `BEGIN` | Successful-path transaction semantics still match: commit, rollback, and savepoints. |
| Unicode collation | locale-aware database collation options | SQLite collation surface differs | ASCII ordering is normalized; full Unicode locale parity is not guaranteed. |
| Float edge precision | engine-native `float8` / `numeric` arithmetic | engine-native f64 arithmetic | Last-ULP differences, NaN, and infinity edges are not a parity target. |

## Test Coverage

- `crates/plugin-db/tests/sqlite_integration.rs` runs the SQLite parity
  leg in the default gate.
- `crates/plugin-db/tests/integration.rs` contains the live-Postgres leg
  and is marked `#[ignore]` until a Postgres listener is available.
