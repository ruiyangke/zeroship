# SQLite divergences

`plugin-db` keeps the creator-facing CRUD surface aligned across Postgres and SQLite. The remaining differences are in engine-specific search, transaction isolation, locking, scoring, ordering, and system-column timestamp resolution.

Most rows below are deliberate. The last two — ordering by `id` and system timestamp resolution — are **consequences** of the storage each backend uses for a shared column, not choices anyone made, and both are things a creator can hit without doing anything unusual. They are documented here because the behaviour is real today, not because it is the behaviour we want.

For the **migration** authoring surface, the equivalent boundary — which DML transforms are portable across both backends, the `.splitPart` portable-expression envelope, and the `EXPR_NOT_PORTABLE` hard error out of envelope — is documented in [migrate-op-dsl.md](./migrate-op-dsl.md#the-dml-portability-boundary).

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
| Ordering by `id` | `id` is `text`, so it sorts under the database collation (`en_US.utf8` by default) | sorts `BINARY`, i.e. byte order | **`sort({ id: -1 })` is not "newest first" on Postgres, and the two backends disagree.** `id` is a base62 typed-id whose alphabet spans digits, uppercase and lowercase, and the two collations order those runs differently — measured on the same six ids, SQLite returns `c,a,Z,Y,W,U` and Postgres `Z,Y,W,U,c,a`; adding `COLLATE "C"` to the Postgres query reproduces SQLite's order exactly, which isolates the collation as the only variable. UUIDv7 makes an `id` time-ordered *as bytes*, and that property does not survive a locale collation. Cursor pagination keyed on `id` inherits this, because the cursor's `WHERE` and `ORDER BY` share the comparison. Sort on an explicit column you control. |
| System timestamp resolution | `created_at` / `updated_at` are `timestamptz` defaulting to `NOW()` — microsecond | the injected DDL is `TEXT ... DEFAULT CURRENT_TIMESTAMP`, which SQLite renders at **whole-second** resolution | Rows written inside the same second share one `created_at` on SQLite and have distinct values on Postgres — measured at six back-to-back inserts giving 6 distinct values deployed and 1 in dev. Both tiers return epoch milliseconds, so the type matches and only the granularity differs; SQLite's always end in `000`. Do not use `created_at` as a tiebreak or an ordering key on the dev tier, and do not expect a local ordering bug to reproduce in production — the local tier is the degenerate one. |
| Migration column-shape verify (existence-guard / drift) | compares the full `information_schema` type spelling | compares only the SQLite **type affinity** (`text`/`integer`/`real`/`numeric`/`blob`) | Several distinct SDK facets fold to the `text` affinity on SQLite (`string`/`ref`/`date`/`json`/…). A within-text-affinity facet change (e.g. `string`→`ref`) is invisible to SQLite introspection and is treated as **no change** by both the differ and the `ifNotExists` existence-guard probe (a `ref` adds no FK via `ALTER` on SQLite — it is physically the same `text` column). A genuine affinity change (`text`↔`real`, i.e. string↔number) IS detected. |
| Migration enum/domain types | `CREATE TYPE ... AS ENUM` and `CREATE DOMAIN` objects | inline column type plus `CHECK`/default/nullability at each use site | The logical constraint must match. SQLite does not create standalone named type objects. |
| Migration table `CHECK` constraints | intended closed-AST `CHECK` rendering | intended closed-AST `CHECK` rendering | Current op.* validate-refuses table-level `CHECK` until the expression renderer lands; enum/domain `CHECK` emulations are supported. |
| Migration table FK/UNIQUE constraints | table constraints render in `CREATE TABLE` | descriptor-backed `CREATE TABLE` path does not yet thread table-level FK/UNIQUE constraints | SQLite op.* validate-refuses these table-level constraints until the emitter carries them; use this as a migration-engine gap, not a runtime divergence. |
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
- [crates/plugin-db/src/crud/mod.rs](../../crates/plugin-db/src/crud/mod.rs) — reads the `orderBy` option and threads it to the backend
- [crates/plugin-db/src/backend/sqlite/dialect.rs](../../crates/plugin-db/src/backend/sqlite/dialect.rs) — `now_fn()` returns `CURRENT_TIMESTAMP`, which is where the whole-second system-timestamp resolution comes from

## Test coverage

SQLite-specific backend coverage lives in [crates/plugin-db/tests/sqlite_integration.rs](../../crates/plugin-db/tests/sqlite_integration.rs). The parity matrix helpers live in [crates/plugin-db/tests/parity/mod.rs](../../crates/plugin-db/tests/parity/mod.rs).
