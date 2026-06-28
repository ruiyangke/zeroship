# SQLite divergences

`plugin-db` keeps the creator-facing CRUD surface aligned across Postgres and SQLite. The remaining intentional differences are in engine-specific search, transaction isolation, locking, scoring, and text-ordering behavior.

For the **migration** authoring surface, the equivalent boundary — which DML transforms are portable across both backends, the `c.fn.splitPart` portable-expression envelope, and the `EXPR_NOT_PORTABLE` hard error out of envelope — is documented in [migrate-op-dsl.md](./migrate-op-dsl.md#the-dml-portability-boundary).

## Current differences

| Area | Postgres | SQLite | Contract |
| --- | --- | --- | --- |
| Vector metrics | `pgvector` supports cosine, L2, and inner product | `sqlite-vec` `vec0` supports cosine and L2 only | `VectorMetric::InnerProduct` returns `vector_unsupported_metric` on SQLite. |
| Full-text language | `language` selects the `tsvector` configuration | FTS5 ignores the `language` parameter | Matching behavior should line up; do not depend on language-specific tokenization on SQLite. |
| Full-text score | `ts_rank`-style descending score | FTS5 hidden `rank` / bm25 score, surfaced verbatim as `_rank` | Match on documents, not exact cross-engine score values. |
| Spatial search | PostGIS + GIST-backed `ST_DWithin` | no index build; `spatial_near` does a haversine flat scan over the stored geopoint BLOB | Result semantics aim to match, but the SQLite path is a dev-scale scan. |
| Transaction isolation | `env.db.transaction(..., { isolationLevel })` emits `BEGIN ISOLATION LEVEL ...` | validates the supplied isolation level but always runs plain `BEGIN` | Commit/rollback/savepoint behavior should line up; do not depend on SQLite honoring PG isolation-level distinctions. |
| Write contention | backend-native locking | WAL mode + single writer actor; busy-family SQLite errors collapse to the same typed lock-contention surface | Callers should branch on the typed error code, not raw SQLite error text. |
| Text ordering | backend ordering plus the database collation | emulates PG NULL placement with `IS NULL` buckets, but does not inject a cross-engine collation | Do not depend on locale-sensitive or Unicode string ordering matching exactly across backends. |
| Migration column-shape verify (existence-guard / drift) | compares the full `information_schema` type spelling | compares only the SQLite **type affinity** (`text`/`integer`/`real`/`numeric`/`blob`) | Several distinct SDK facets fold to the `text` affinity on SQLite (`string`/`ref`/`date`/`json`/…). A within-text-affinity facet change (e.g. `string`→`ref`) is invisible to SQLite introspection and is treated as **no change** by both the differ and the `ifNotExists` existence-guard probe (a `ref` adds no FK via `ALTER` on SQLite — it is physically the same `text` column). A genuine affinity change (`text`↔`real`, i.e. string↔number) IS detected. |
| Migration enum/domain types | `CREATE TYPE ... AS ENUM` and `CREATE DOMAIN` objects | inline column type plus `CHECK`/default/nullability at each use site | The logical constraint must match. SQLite does not create standalone named type objects. |
| Migration table `CHECK` constraints | intended closed-AST `CHECK` rendering | intended closed-AST `CHECK` rendering | Current op.* lower fails closed with `ExprRenderDeferred` for table-level `CHECK`; enum/domain `CHECK` emulations are supported. |
| Migration table FK/UNIQUE constraints | table constraints render in `CREATE TABLE` | descriptor-backed `CREATE TABLE` path does not yet thread table-level FK/UNIQUE constraints | SQLite op.* lower fails closed for these table-level constraints until the emitter carries them; use this as a migration-engine gap, not a runtime divergence. |
| Migration identity columns | `GENERATED {ALWAYS\|BY DEFAULT} AS IDENTITY` | `INTEGER PRIMARY KEY AUTOINCREMENT` only for the sole integer primary-key case | Other SQLite identity placements fail closed instead of emulating an unsound sequence. |
| Migration generated columns | STORED generated columns | STORED or VIRTUAL generated columns | The shared expression AST renders on both backends. Postgres virtual generated columns fail closed. |
| Migration triggers | function-backed triggers (`EXECUTE FUNCTION`) | closed inline `Body` triggers | The two trigger action forms are intentionally dialect-specific: SQLite `Body` triggers fail closed on Postgres, and Postgres `EXECUTE FUNCTION` triggers fail closed on SQLite. |
| Migration sequences/exclusion constraints | native standalone sequences and exclusion constraints | no sound general equivalent | SQLite fails closed on these PG-only facets. |

## Source of truth

- [crates/plugin-db/src/backend/mod.rs](../../crates/plugin-db/src/backend/mod.rs) — cross-backend trait contracts for vector, FTS, and spatial search
- [crates/plugin-db/src/backend/sqlite/mod.rs](../../crates/plugin-db/src/backend/sqlite/mod.rs) — SQLite implementations
- [crates/plugin-db/src/backend/sqlite/vector.rs](../../crates/plugin-db/src/backend/sqlite/vector.rs) — `vector_unsupported_metric`
- [crates/plugin-db/src/backend/sqlite/fts.rs](../../crates/plugin-db/src/backend/sqlite/fts.rs) — SQLite `_rank` / FTS5 SQL shape
- [crates/plugin-db/src/backend/sqlite/spatial.rs](../../crates/plugin-db/src/backend/sqlite/spatial.rs) — haversine helper
- [crates/plugin-db/src/backend/sqlite/session.rs](../../crates/plugin-db/src/backend/sqlite/session.rs) — WAL + `busy_timeout`
- [crates/plugin-db/src/backend/sqlite/error.rs](../../crates/plugin-db/src/backend/sqlite/error.rs) — `SQLITE_BUSY*` → typed lock contention mapping
- [crates/plugin-db/src/v8_classes/transaction.rs](../../crates/plugin-db/src/v8_classes/transaction.rs) — SQLite `transaction()` begin path
- [crates/plugin-db/src/query.rs](../../crates/plugin-db/src/query.rs) — cross-backend `ORDER BY` shaping

## Test coverage

SQLite-specific backend coverage lives in [crates/plugin-db/tests/sqlite_integration.rs](../../crates/plugin-db/tests/sqlite_integration.rs). The parity matrix helpers live in [crates/plugin-db/tests/parity/mod.rs](../../crates/plugin-db/tests/parity/mod.rs).
