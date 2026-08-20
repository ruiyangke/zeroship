//! SQLite schema/DDL spelling. The future `zero-migrate-sqlite`.

use crate::schema::query::{
    decimal_precision_scale, quote_ident_for_dialect, SchemaRenderer, SqlDialect,
};

/// This module's own vendor identity — the ONE dialect literal it is allowed to
/// name. See `backends/mod.rs`.
const DIALECT: SqlDialect = SqlDialect::Sqlite;

pub(super) struct SqliteSchemaRenderer;

pub(super) static RENDERER: SqliteSchemaRenderer = SqliteSchemaRenderer;

impl SchemaRenderer for SqliteSchemaRenderer {
    fn dialect(&self) -> SqlDialect {
        DIALECT
    }

    fn foreign_key_target(&self, _app_id: &str, target: &str) -> String {
        quote_ident_for_dialect(target, self.dialect())
    }

    fn column_type(&self, def: &serde_json::Value) -> String {
        if def.get("encrypted").is_some() {
            return "BLOB".to_string();
        }

        let zs_type = def.get("type").and_then(|t| t.as_str());

        if zs_type == Some("vector") {
            return "BLOB".to_string();
        }

        if zs_type == Some("geoPoint") {
            return "BLOB".to_string();
        }

        // A `number` carrying `precision` is a FIXED-PRECISION decimal, and SQLite
        // has no storage class for one. `REAL` is the right answer for the float that
        // shares this token and a lossy one for the decimal: REAL affinity converts a
        // stored decimal STRING to a binary double on the way in, and the 12-step
        // rebuild re-renders `CREATE TABLE` from this map and copies every existing
        // row through it - so a rebuild triggered by an unrelated column rewrote
        // `12345678901234.5678` to `12345678901234.6`, measured in
        // `tests/fold_live/sqlite_decimal_rebuild_live.rs`.
        //
        // `TEXT` is not a new opinion: `render::lower::author_type_override` already
        // answers `text`/`TEXT` for `ColType::Decimal` on SQLite, with this same
        // reason written beside it, and `sqlite_canonical_type` already folds
        // `numeric`/`decimal` to `text` affinity. The two carriers disagreed only
        // because the token map had no way to say "decimal"; it does now.
        if zs_type == Some("number") && decimal_precision_scale(def).is_some() {
            return "TEXT".to_string();
        }

        match zs_type {
            Some("string") => "TEXT".to_string(),
            Some("char") => "TEXT".to_string(),
            Some("number") => "REAL".to_string(),
            Some("real") => "REAL".to_string(),
            Some("boolean") => "INTEGER".to_string(),
            Some("date") => "TEXT".to_string(),
            Some("calendarDate") => "TEXT".to_string(),
            Some("json") | Some("object") | Some("array") | Some("union") => "TEXT".to_string(),
            Some("textArray") => "TEXT".to_string(),
            Some("ref") => "TEXT".to_string(),
            Some("literal") => match def.get("literalValue") {
                // Exact decimal TEXT affinity, matching the numeric/decimal column
                // mapping and the `t.numeric()` SQLite override; NUMERIC/REAL affinity
                // would coerce a wide literal through a binary float.
                Some(serde_json::Value::Number(_)) => "TEXT".to_string(),
                Some(serde_json::Value::Bool(_)) => "INTEGER".to_string(),
                _ => "TEXT".to_string(),
            },
            // `bigInt` is the DSL token — the one `render::lower::col_type_to_token`
            // emits for `ColType::BigInt` and the one the SDK's `t.bigInt()` writes.
            // Only the SQL type NAMES (`bigint`, `int8`, `int4`) were listed here, so
            // the token the rest of this codebase actually produces reached the
            // `_ => "TEXT"` arm below and the column was declared TEXT — TEXT affinity,
            // which converts an integer to its decimal string on the way in. The SQLite
            // 12-step rebuild re-renders `CREATE TABLE` from this map and copies every
            // existing row through it, so a `renameColumn` on an unrelated column of
            // the same table silently rewrote a `t.bigInt()` column's values to
            // strings. Measured against a live database in
            // `tests/fold_live/sqlite_field_def_type_tokens_live.rs`.
            //
            // The two sibling emitters already spelled it this way and this arm was the
            // odd one out: `def_to_pg_type` maps `Some("bigInt") => "BIGINT"`, the MySQL
            // arm lists `Some("bigInt") | Some("bigint") | Some("int8")`, and
            // `def_to_constraints_for_dialect` — over in `schema::query`, which is where
            // both of those live too — already lists `Some("bigInt")` among the numeric
            // tokens whose DEFAULT it renders. A `t.bigInt()` column therefore kept its
            // DEFAULT while losing its type.
            Some("bigInt") | Some("bigint") | Some("int8") | Some("integer") | Some("int")
            | Some("int4") => "INTEGER".to_string(),
            Some("smallInt") => "INTEGER".to_string(),
            // `ColType::Bytes`'s own doc-comment is "`BYTEA` on PG, `BLOB` on SQLite",
            // and the MySQL arm maps it to `LONGBLOB`. SQLite had no arm at all, so a
            // `bytes` column was re-declared TEXT by a rebuild. TEXT affinity leaves an
            // already-stored BLOB value alone, so this did not corrupt existing rows the
            // way the `bigInt` gap did; what it broke is the declared type every later
            // write and index build resolves against.
            Some("bytes") => "BLOB".to_string(),
            Some("inet") => "TEXT".to_string(),
            _ => "TEXT".to_string(),
        }
    }

    fn json_object_default(&self) -> String {
        "DEFAULT '{}'".to_string()
    }

    fn json_array_default(&self) -> String {
        "DEFAULT '[]'".to_string()
    }

    fn current_timestamp_expr(&self) -> &'static str {
        "CURRENT_TIMESTAMP"
    }

    fn column_comment_statements(
        &self,
        _app_id: &str,
        _collection: &str,
        _schema: &serde_json::Value,
    ) -> Vec<String> {
        Vec::new()
    }
}
