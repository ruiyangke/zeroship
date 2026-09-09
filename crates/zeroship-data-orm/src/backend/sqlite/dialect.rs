//! SQLite-flavoured `DialectBuilder` impl.
//!
//! Fills in the six hooks declared by
//! [`zeroship_data_orm::storage::DialectBuilder`] (see
//! `docs/archive/p1-sqlite-implementation-plan.md` §5 for the hook
//! set rationale and §7.2 of the design doc for the engine-divergence
//! table). The shape is a Zero-Sized Type — every hook is a pure
//! function of its inputs, so there is no per-instance state.
//!
//! **No production caller yet**: this trait impl ships alongside
//! the matching `PgDialect` impl so `query.rs`'s free-function string
//! builders can be retargeted onto a dialect-typed entry point in a
//! later change without re-shaping their call sites. The
//! [`crate::backend::sqlite::SqliteBackend::attach_app_file`] impl
//! uses `quote_ident` to escape the ATTACH alias; the other
//! hooks have no consumer yet.

use zeroship_data_orm::storage::DialectBuilder;

/// SQLite-flavoured dialect. Zero-sized — every method is pure.
///
/// Constructed implicitly by [`crate::backend::sqlite::SqliteBackend`]
/// (which `impl`s `DialectBuilder` directly via this ZST's behaviour;
/// see the impl block in `sqlite/mod.rs`).
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct SqliteDialect;

impl DialectBuilder for SqliteDialect {
    // UNGATED since 2026-09-04, with the trait member in data-core. Gated, this
    // impl and the one on `SqliteBackend` were `error[E0046]: not all trait
    // items implemented, missing: sql_dialect` in any build that turned on
    // `zeroship-data-core/test-helpers` without this crate's own feature.
    fn sql_dialect(&self) -> zeroship_data_sql::compile::SqlDialect {
        zeroship_data_sql::compile::SqlDialect::Sqlite
    }

    /// Double-quote the identifier, escaping any embedded `"` by
    /// doubling. Matches `zeroship_data_sql::compile::quote_ident` (the PG-side
    /// helper) — SQLite's identifier-quoting rules are a superset of
    /// PG's in this regard (both support the `"…""…"` escape).
    ///
    /// Validation of the input alphabet is **not** this hook's job —
    /// callers either pass identifiers already validated at the SDK
    /// boundary, or accept the SQL-injection risk consciously. The hook
    /// is a pure lexical transform.
    fn quote_ident(&self, name: &str) -> String {
        format!("\"{}\"", name.replace('"', "\"\""))
    }

    /// Map a Zeroship-level type string to SQLite's storage-class
    /// vocabulary (`TEXT` / `INTEGER` / `REAL` / `BLOB` / `NUMERIC`).
    ///
    /// SQLite's type system is dynamic ("type affinity") — the column
    /// declaration is a hint, not a constraint. We pick affinities
    /// that round-trip cleanly through the SDK:
    ///
    /// - Booleans store as `INTEGER` 0/1 (SQLite has no boolean class).
    /// - Timestamps store as `TEXT` (ISO 8601) — the SDK already emits
    ///   ISO-8601 strings on the wire; staying in TEXT avoids the
    ///   floating-point Julian day surprise from `REAL`.
    /// - JSON/JSONB store as `TEXT` — SQLite ships a JSON1 extension
    ///   that operates on TEXT columns; a future change can teach the
    ///   diff engine to recognise the affinity.
    /// - Unknown types fall through to `TEXT` (the most permissive
    ///   affinity) with a debug log warning so the operator sees the
    ///   miss; production runs should hit only the typed branches.
    fn map_zs_type(&self, zs_type: &str, _opts: &zeroship_data_sql::value::Value) -> String {
        match zs_type {
            "text" => "TEXT",
            "bigint" | "int8" | "integer" | "int" | "int4" => "INTEGER",
            "double" | "real" => "REAL",
            "bytes" | "blob" => "BLOB",
            "numeric" | "decimal" => "NUMERIC",
            // `t.encrypted(...)`-declared columns always
            // store the ciphertext wire blob (`[version_flag | nonce |
            // ct+tag]`) as BLOB regardless of `wraps`. The DDL emitter
            // (`zeroship_data_sql::compile::field_to_column`) inspects `def.encrypted`
            // BEFORE calling `map_zs_type` and shortcuts to BLOB on the
            // SQLite arm — but if a future path reaches this branch
            // with `zs_type = "encrypted"`, BLOB is the safe answer.
            // Mirrors the PG arm's BYTEA override in `query.rs`.
            "encrypted" => "BLOB",
            // SQLite stores booleans as 0/1 — keeps the on-disk size
            // compact and lets equality / index comparisons stay
            // INTEGER-fast.
            "boolean" | "bool" => "INTEGER",
            // ISO-8601 string; PG would map these to `TIMESTAMP` /
            // `TIMESTAMPTZ`. The SDK serialises Date as ISO 8601 on
            // the wire so the affinity matches the producer.
            "timestamp" | "timestamptz" => "TEXT",
            // JSON1 extension operates on TEXT columns.
            "json" | "jsonb" => "TEXT",
            other => {
                tracing::debug!(
                    zs_type = other,
                    "SqliteDialect::map_zs_type: unknown type — defaulting to TEXT"
                );
                "TEXT"
            }
        }
        .to_string()
    }

    /// SQLite's "now" function, and the source of a MEASURED tier
    /// divergence rather than a neutral spelling of PG's `NOW()`.
    ///
    /// `CURRENT_TIMESTAMP` renders at WHOLE-SECOND resolution
    /// (`2026-08-10 16:08:32`), so every row written inside the same second
    /// shares one `created_at`. Measured standalone: six back-to-back
    /// inserts into a table defaulting to `CURRENT_TIMESTAMP` gave **1
    /// distinct value of 6**, against 6 of 6 on the deployed Postgres tier
    /// (`tests/e2e_dev_vs_deployed_db.sh`, the `tsres` row). SQLite offers
    /// millisecond forms — `strftime('%Y-%m-%d %H:%M:%f','now')` and
    /// `unixepoch('now','subsec')` — so this is a resolution the injected
    /// DDL gives up, not one the engine lacks.
    ///
    /// This doc comment previously justified the choice by claiming the
    /// returned string is "the same ISO-8601-shaped string the SDK uses on
    /// the wire (`YYYY-MM-DD HH:MM:SS`)". That is FALSE and was the reason
    /// the resolution loss read as intentional: `created_at` crosses the
    /// wire as NUMERIC epoch milliseconds on BOTH tiers. Measured over a
    /// full dev-vs-deployed capture, every `created_at` in every response
    /// body was numeric and NOT ONE was an ISO string. The shape returned
    /// here is an internal storage detail that something downstream
    /// converts; it is not the creator-facing contract, so matching it was
    /// never an argument for second resolution.
    ///
    /// Do not raise the resolution here without tracing that conversion
    /// first — this function's output is parsed downstream, and adding a
    /// fractional part is only safe once that path is known to accept one.
    /// The divergence and its creator-facing consequences are recorded in
    /// `docs/reference/sqlite-divergences.md` ("System timestamp
    /// resolution").
    fn now_fn(&self) -> &'static str {
        "CURRENT_TIMESTAMP"
    }

    /// SQLite exposes the last-inserted rowid as a SQL function. PG
    /// returns `None` here because it routes through `RETURNING id`
    /// instead. The trait default is `None`; we override.
    fn last_insert_rowid_sql(&self) -> Option<&'static str> {
        Some("SELECT last_insert_rowid()")
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the pure-function dialect hooks. The hooks
    //! have no I/O — each test is a string-compare against the
    //! expected SQL fragment.

    use super::*;

    #[test]
    fn quote_ident_doubles_embedded_quote() {
        let d = SqliteDialect;
        assert_eq!(d.quote_ident("plain"), "\"plain\"");
        assert_eq!(d.quote_ident("with\"quote"), "\"with\"\"quote\"");
        assert_eq!(d.quote_ident(""), "\"\"");
    }

    #[test]
    fn map_zs_type_covers_p1_vocabulary() {
        let d = SqliteDialect;
        let no_opts = zeroship_data_sql::value!({});
        assert_eq!(d.map_zs_type("text", &no_opts), "TEXT");
        assert_eq!(d.map_zs_type("bigint", &no_opts), "INTEGER");
        assert_eq!(d.map_zs_type("int8", &no_opts), "INTEGER");
        assert_eq!(d.map_zs_type("integer", &no_opts), "INTEGER");
        assert_eq!(d.map_zs_type("double", &no_opts), "REAL");
        assert_eq!(d.map_zs_type("real", &no_opts), "REAL");
        assert_eq!(d.map_zs_type("bytes", &no_opts), "BLOB");
        assert_eq!(d.map_zs_type("blob", &no_opts), "BLOB");
        assert_eq!(d.map_zs_type("numeric", &no_opts), "NUMERIC");
        assert_eq!(d.map_zs_type("decimal", &no_opts), "NUMERIC");
        assert_eq!(d.map_zs_type("boolean", &no_opts), "INTEGER");
        assert_eq!(d.map_zs_type("bool", &no_opts), "INTEGER");
        assert_eq!(d.map_zs_type("timestamp", &no_opts), "TEXT");
        assert_eq!(d.map_zs_type("timestamptz", &no_opts), "TEXT");
        assert_eq!(d.map_zs_type("json", &no_opts), "TEXT");
        assert_eq!(d.map_zs_type("jsonb", &no_opts), "TEXT");
        // Unknown types fall through to TEXT with a debug log warning.
        assert_eq!(d.map_zs_type("nonsense_type", &no_opts), "TEXT");
        // `encrypted` falls through to BLOB so a path
        // that bypasses the `def.encrypted` check still emits the right
        // affinity. Mirrors the PG dialect's BYTEA override.
        assert_eq!(d.map_zs_type("encrypted", &no_opts), "BLOB");
    }

    #[test]
    fn now_fn_and_last_insert_rowid() {
        let d = SqliteDialect;
        assert_eq!(d.now_fn(), "CURRENT_TIMESTAMP");
        assert_eq!(
            d.last_insert_rowid_sql(),
            Some("SELECT last_insert_rowid()")
        );
    }
}
