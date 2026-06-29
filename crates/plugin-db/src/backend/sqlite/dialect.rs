//! SQLite-flavoured `DialectBuilder` impl.
//!
//! **P1 PR 3** fills in the six P1-essential hooks declared by
//! [`crate::backend::DialectBuilder`] (see
//! `docs/proposals/p1-sqlite-implementation-plan.md` §5 for the hook
//! set rationale and §7.2 of the design doc for the engine-divergence
//! table). The shape is a Zero-Sized Type — every hook is a pure
//! function of its inputs, so there is no per-instance state.
//!
//! **No production caller yet**: PR 3 ships the trait impl alongside
//! the matching `PgDialect` impl so `query.rs`'s free-function string
//! builders can be retargeted onto a dialect-typed entry point in a
//! later PR without re-shaping their call sites. The
//! [`crate::backend::sqlite::SqliteBackend::ensure_app_schema`] impl
//! (PR 3) uses `quote_ident` to escape the ATTACH alias; the other
//! hooks have no PR-3 consumer.

use crate::backend::DialectBuilder;
#[cfg(any(test, feature = "test-helpers"))]
use crate::query::IndexSpec;

/// SQLite-flavoured dialect. Zero-sized — every method is pure.
///
/// Constructed implicitly by [`crate::backend::sqlite::SqliteBackend`]
/// (which `impl`s `DialectBuilder` directly via this ZST's behaviour;
/// see the impl block in `sqlite/mod.rs`).
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct SqliteDialect;

impl DialectBuilder for SqliteDialect {
    #[cfg(any(test, feature = "test-helpers"))]
    fn sql_dialect(&self) -> crate::query::SqlDialect {
        crate::query::SqlDialect::Sqlite
    }

    /// Double-quote the identifier, escaping any embedded `"` by
    /// doubling. Matches `crate::query::quote_ident` (the PG-side
    /// helper) — SQLite's identifier-quoting rules are a superset of
    /// PG's in this regard (both support the `"…""…"` escape).
    ///
    /// Validation of the input alphabet is **not** this hook's job —
    /// callers either pass identifiers already validated by
    /// `crate::audit::validate_app_id` / `validate_field_name`, or
    /// accept the SQL-injection risk consciously. The hook is a pure
    /// lexical transform.
    fn quote_ident(&self, name: &str) -> String {
        format!("\"{}\"", name.replace('"', "\"\""))
    }

    /// Build the SQL that idempotently provisions a per-app namespace.
    ///
    /// **NOTE**: this trait-level builder cannot produce the full
    /// SQLite ATTACH statement because the per-app file path is a
    /// backend-instance concern (it lives in
    /// [`crate::backend::sqlite::SqliteBackend::db_dir`]) — the
    /// dialect has no knowledge of `db_dir`. The
    /// `NamespaceManager::ensure_app_schema` impl on `SqliteBackend`
    /// constructs the ATTACH SQL inline using this hook only to quote
    /// the alias. This builder returns a *template* string with the
    /// alias quoted and a `:file_path` placeholder; PR 5 may decide
    /// whether to keep this template shape or fold the helper back
    /// into the backend impl.
    fn build_ensure_app_schema(&self, app_id: &str) -> String {
        // Template form — the `:file_path` placeholder is not a SQLite
        // bind parameter (ATTACH does not accept binds for path or
        // alias); the backend impl substitutes it via `format!` after
        // quoting the path string. Documented so a reader of the
        // template doesn't mistake it for a bound-parameter site.
        format!(
            "ATTACH DATABASE 'file::file_path' AS {}",
            self.quote_ident(app_id)
        )
    }

    /// Build a `CREATE INDEX` statement for a SQLite collection.
    ///
    /// SQLite has no `CREATE INDEX CONCURRENTLY`; the `online` flag is a
    /// no-op. Only `IndexKind::BTree` routes through this builder —
    /// vector/FTS/spatial index kinds have dedicated backend hooks.
    ///
    /// `IndexSpec` does not carry the collection name separately, so we
    /// recover the attached-schema/table target from the deterministic PG
    /// `spec.sql` shape emitted by `query.rs`.
    #[cfg(any(test, feature = "test-helpers"))]
    fn build_create_index(&self, spec: &IndexSpec, _online: bool) -> String {
        if !matches!(spec.kind, crate::query::IndexKind::BTree) {
            tracing::debug!(
                kind = ?spec.kind,
                "SqliteDialect::build_create_index: non-BTree index delegated to backend hook"
            );
            return String::new();
        }

        let (app_id, collection) = match extract_pg_index_target(&spec.sql) {
            Some(target) => target,
            None => {
                tracing::warn!(
                    sql = %spec.sql,
                    "SqliteDialect::build_create_index: could not parse PG index target"
                );
                return String::new();
            }
        };

        let cols = spec
            .columns
            .iter()
            .map(|c| self.quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        let unique_kw = if spec.unique { "UNIQUE " } else { "" };
        format!(
            "CREATE {unique_kw}INDEX IF NOT EXISTS {}.{} ON {} ({cols})",
            self.quote_ident(&app_id),
            self.quote_ident(&spec.name),
            self.quote_ident(&collection),
        )
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
    ///   that operates on TEXT columns; PR 5+ can teach the diff
    ///   engine to recognise the affinity.
    /// - Unknown types fall through to `TEXT` (the most permissive
    ///   affinity) with a debug log warning so the operator sees the
    ///   miss; production runs should hit only the typed branches.
    fn map_zs_type(&self, zs_type: &str, _opts: &serde_json::Value) -> String {
        match zs_type {
            "text" => "TEXT",
            "bigint" | "int8" | "integer" | "int" | "int4" => "INTEGER",
            "double" | "real" => "REAL",
            "bytes" | "blob" => "BLOB",
            "numeric" | "decimal" => "NUMERIC",
            // **P5 PR 3** — `t.encrypted(...)`-declared columns always
            // store the ciphertext wire blob (`[version_flag | nonce |
            // ct+tag]`) as BLOB regardless of `wraps`. The DDL emitter
            // (`crate::query::field_to_column`) inspects `def.encrypted`
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

    /// SQLite's "now" function. Returns the same ISO-8601-shaped string
    /// the SDK uses on the wire (`YYYY-MM-DD HH:MM:SS`). PG returns
    /// `NOW()` (timestamp with tz).
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

#[cfg(any(test, feature = "test-helpers"))]
fn extract_pg_index_target(sql: &str) -> Option<(String, String)> {
    let (_, on_tail) = sql.split_once(" ON ")?;
    let (target, _) = on_tail.split_once(" (")?;
    let (app, collection) = target.split_once('.')?;
    Some((unquote_ident(app)?, unquote_ident(collection)?))
}

#[cfg(any(test, feature = "test-helpers"))]
fn unquote_ident(ident: &str) -> Option<String> {
    let inner = ident.strip_prefix('"')?.strip_suffix('"')?;
    Some(inner.replace("\"\"", "\""))
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
    fn build_ensure_app_schema_quotes_alias() {
        let d = SqliteDialect;
        // The template carries a `:file_path` placeholder the backend
        // substitutes inline (ATTACH does not accept bound params for
        // path or alias). The alias is quoted via `quote_ident`.
        let sql = d.build_ensure_app_schema("app_demo");
        assert!(
            sql.contains("ATTACH DATABASE 'file::file_path'"),
            "missing path placeholder: {sql}"
        );
        assert!(
            sql.contains("AS \"app_demo\""),
            "alias not double-quoted: {sql}"
        );
    }

    #[test]
    fn map_zs_type_covers_p1_vocabulary() {
        let d = SqliteDialect;
        let no_opts = serde_json::json!({});
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
        // **P5 PR 3** — `encrypted` falls through to BLOB so a path
        // that bypasses the `def.encrypted` check still emits the right
        // affinity. Mirrors the PG dialect's BYTEA override.
        assert_eq!(d.map_zs_type("encrypted", &no_opts), "BLOB");
    }

    #[test]
    fn now_fn_and_last_insert_rowid() {
        let d = SqliteDialect;
        assert_eq!(d.now_fn(), "CURRENT_TIMESTAMP");
        assert_eq!(d.last_insert_rowid_sql(), Some("SELECT last_insert_rowid()"));
    }

    #[test]
    fn build_create_index_emits_plain_sqlite_index() {
        let d = SqliteDialect;
        let sql = d.build_create_index(
            &IndexSpec {
                name: "users_email_key".to_string(),
                columns: vec!["email".to_string()],
                unique: true,
                sql: "CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS \"users_email_key\" ON \"app_demo\".\"users\" (\"email\")".to_string(),
                kind: crate::query::IndexKind::BTree,
            },
            false,
        );
        assert_eq!(
            sql,
            "CREATE UNIQUE INDEX IF NOT EXISTS \"app_demo\".\"users_email_key\" ON \"users\" (\"email\")"
        );
    }

    #[test]
    fn extract_pg_index_target_round_trips_quoted_identifiers() {
        assert_eq!(
            extract_pg_index_target(
                "CREATE INDEX CONCURRENTLY IF NOT EXISTS \"idx\" ON \"app\".\"users\" (\"name\")"
            ),
            Some(("app".to_string(), "users".to_string()))
        );
        assert_eq!(
            extract_pg_index_target(
                "CREATE INDEX CONCURRENTLY IF NOT EXISTS \"idx\" ON \"a\"\"pp\".\"us\"\"ers\" (\"name\")"
            ),
            Some(("a\"pp".to_string(), "us\"ers".to_string()))
        );
    }
}
