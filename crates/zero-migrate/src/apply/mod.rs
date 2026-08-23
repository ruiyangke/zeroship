pub mod backend;
// The adoption path's dialect-neutral vocabulary moved down to the backend
// contract, beside the `MigrationBackend::baseline_one` signature that is its
// only reason to exist. Re-exported so every `crate::apply::baseline::…`
// reference resolves unchanged.
pub use zero_migrate_backend::baseline;
pub mod drift;
pub mod executor;
// The journal's dialect-neutral vocabulary moved down to the backend contract,
// where the three per-vendor journal writers can see it. Re-exported so every
// `crate::apply::journal::…` reference resolves unchanged.
pub use zero_migrate_backend::journal;
pub mod plan_precondition;
// The precondition EVALUATOR was PostgreSQL's alone — `pg_query` shape validation,
// `information_schema` catalog reads, and a `&Client`-bound `SqlBoolean` run, with
// `PostgresBackend` named in its own body. It is
// `zero_migrate_postgres::backend::precondition` now, beside the backend that was its
// only caller. This comment used to say it COULD NOT follow the renderers into the
// PostgreSQL crate, because it needs `SqlSession`/`ExecutorConfig`/`ApplyError`/
// `Migration` and that crate must not depend on the engine. All four of those moved
// down to `zero-migrate-backend` afterwards, which is what let the whole execution
// half go.
// The least-privilege `migrator` role name derivation was PostgreSQL's alone — the
// `NOLOGIN` + `SET ROLE` model, the `[a-z0-9_]` charset, and the 63-BYTE cap this
// vendor declares (MySQL's is 64 CHARACTERS; SQLite has no roles at all). It lives
// in the PostgreSQL backend crate now and the engine no longer surfaces it.
// The finite-timeout-budget rule moved down to the backend contract, where the
// vendors that resolve a budget can see it. Re-exported so
// `crate::apply::timeout::{resolve_timeout_ms, IndefiniteTimeoutError, TimeoutOrigin}`
// still resolve.
pub use zero_migrate_backend::timeout;
