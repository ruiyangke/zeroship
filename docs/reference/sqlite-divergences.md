# SQLite divergences

`plugin-db` keeps the creator-facing CRUD surface aligned across Postgres and SQLite. The remaining intentional differences are in engine-specific search, locking, and scoring behavior.

## Current differences

| Area | Postgres | SQLite | Contract |
| --- | --- | --- | --- |
| Vector metrics | `pgvector` supports cosine, L2, and inner product | `sqlite-vec` `vec0` supports cosine and L2 only | `VectorMetric::InnerProduct` returns `vector_unsupported_metric` on SQLite. |
| Full-text language | `language` selects the `tsvector` configuration | FTS5 ignores the `language` parameter | Matching behavior should line up; do not depend on language-specific tokenization on SQLite. |
| Full-text score | `ts_rank`-style descending score | FTS5 hidden `rank` / bm25 score, surfaced verbatim as `_rank` | Match on documents, not exact cross-engine score values. |
| Spatial search | PostGIS + GIST-backed `ST_DWithin` | no index build; `spatial_near` does a haversine flat scan over the stored geopoint BLOB | Result semantics aim to match, but the SQLite path is a dev-scale scan. |
| Write contention | backend-native locking | WAL mode + single writer actor; busy-family SQLite errors collapse to the same typed lock-contention surface | Callers should branch on the typed error code, not raw SQLite error text. |

## Source of truth

- [crates/plugin-db/src/backend/mod.rs](crates/plugin-db/src/backend/mod.rs) — cross-backend trait contracts for vector, FTS, and spatial search
- [crates/plugin-db/src/backend/sqlite/mod.rs](crates/plugin-db/src/backend/sqlite/mod.rs) — SQLite implementations
- [crates/plugin-db/src/backend/sqlite/vector.rs](crates/plugin-db/src/backend/sqlite/vector.rs) — `vector_unsupported_metric`
- [crates/plugin-db/src/backend/sqlite/fts.rs](crates/plugin-db/src/backend/sqlite/fts.rs) — SQLite `_rank` / FTS5 SQL shape
- [crates/plugin-db/src/backend/sqlite/spatial.rs](crates/plugin-db/src/backend/sqlite/spatial.rs) — haversine helper
- [crates/plugin-db/src/backend/sqlite/session.rs](crates/plugin-db/src/backend/sqlite/session.rs) — WAL + `busy_timeout`
- [crates/plugin-db/src/backend/sqlite/error.rs](crates/plugin-db/src/backend/sqlite/error.rs) — `SQLITE_BUSY*` → typed lock contention mapping

## Test coverage

SQLite-specific backend coverage lives in [crates/plugin-db/tests/sqlite_integration.rs](crates/plugin-db/tests/sqlite_integration.rs). The parity matrix helpers live in [crates/plugin-db/tests/parity/mod.rs](crates/plugin-db/tests/parity/mod.rs).
