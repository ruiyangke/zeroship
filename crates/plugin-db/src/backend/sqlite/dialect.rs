//! SQLite-flavoured `DialectBuilder` impl — stub.
//!
//! **P1 PR 1**: ZST + skeletal method bodies (every method either
//! returns a trivial string or `todo!()`-style placeholder). The full
//! implementation lands in PR 3 alongside the matching `PgDialect`
//! impl. See `docs/proposals/p1-sqlite-implementation-plan.md` §5 for
//! the 6-hook P1 surface; design §7.2 for the divergence rationale.

use crate::backend::DialectBuilder;
use crate::query::IndexSpec;

/// SQLite-flavoured dialect. Zero-sized; one instance lives behind
/// [`super::SqliteBackend::dialect`].
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct SqliteDialect;

impl DialectBuilder for SqliteDialect {
    fn quote_ident(&self, _name: &str) -> String {
        // P1 PR 3 will install the real SQLite quoting (doubled `"`
        // characters inside the identifier, with a reject path for
        // embedded NULs). Stub returns a sentinel so any accidental
        // PR1 consumer fails loud at integration time.
        unimplemented!("SqliteDialect::quote_ident — P1 PR3 stub")
    }

    fn build_ensure_app_schema(&self, _app_id: &str) -> String {
        unimplemented!("SqliteDialect::build_ensure_app_schema — P1 PR3 stub")
    }

    fn build_create_index(&self, _spec: &IndexSpec, _online: bool) -> String {
        unimplemented!("SqliteDialect::build_create_index — P1 PR3 stub")
    }

    fn map_zs_type(&self, _zs_type: &str, _opts: &serde_json::Value) -> String {
        unimplemented!("SqliteDialect::map_zs_type — P1 PR3 stub")
    }

    fn now_fn(&self) -> &'static str {
        // SQLite uses `CURRENT_TIMESTAMP` (PG uses `NOW()`). Trivial
        // string — fine to ship live in PR 1 because it has no
        // dispatch cost and no consumer yet routes through it.
        "CURRENT_TIMESTAMP"
    }

    fn last_insert_rowid_sql(&self) -> Option<&'static str> {
        // SQLite exposes `last_insert_rowid()` as a SQL function; PG
        // returns `Some(None)`-equivalent by routing through
        // `RETURNING`. The trait default returns `None`; we override.
        Some("SELECT last_insert_rowid()")
    }
}
