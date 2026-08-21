//! SQLite schema/DDL spelling. The future `zero-migrate-sqlite`.

use zero_migrate_backend::ddl::sqlite_auto_increment_identity_pk;
use zero_migrate_backend::schema::{
    decimal_precision_scale, quote_ident_for_backend, SchemaRenderer,
};
use zero_migrate_backend::snapshot::ColumnSnapshot;
use zero_migrate_ir::dialect::{DialectId, SqlDialect};

/// This module's own vendor identity — the ONE dialect literal it is allowed to
/// name. See `backends/mod.rs`.
const DIALECT: SqlDialect = SqlDialect::Sqlite;

#[derive(Debug)]
pub(super) struct SqliteSchemaRenderer;

pub(super) static RENDERER: SqliteSchemaRenderer = SqliteSchemaRenderer;

impl SchemaRenderer for SqliteSchemaRenderer {
    fn dialect(&self) -> DialectId {
        DIALECT.id()
    }

    fn foreign_key_target(&self, _app_id: &str, target: &str) -> String {
        quote_ident_for_backend(target, &crate::dml::RENDERER)
    }

    fn column_type(&self, c: &ColumnSnapshot, inline_pk: bool) -> String {
        if let Some(ty) = &c.ddl_type_override {
            ty.clone()
        } else if let Some(def) = &c.type_def {
            column_type_for_def(def)
        } else if matches!(c.case_sensitive, Some(false))
            && c.data_type.eq_ignore_ascii_case("text")
        {
            "text COLLATE NOCASE".to_string()
        } else if sqlite_auto_increment_identity_pk(c, inline_pk) {
            "INTEGER".to_string()
        } else {
            sqlite_ddl_type(&c.data_type).to_string()
        }
    }

    /// SQLite's `column_type` owns its `text COLLATE NOCASE` spelling directly;
    /// there is no separate character-set/collation suffix for this hook to add.
    fn pin_collation(&self, rendered: &str, _case_sensitive: Option<bool>) -> String {
        rendered.to_string()
    }

    /// SQLite rebuilds carry the complete type spelling selected above, so there is
    /// no MySQL-style engine pin to strip before a retype.
    fn strip_collation<'a>(&self, rendered: &'a str) -> &'a str {
        rendered
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

fn column_type_for_def(def: &serde_json::Value) -> String {
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
            Some(serde_json::Value::Number(_)) => "TEXT".to_string(),
            Some(serde_json::Value::Bool(_)) => "INTEGER".to_string(),
            _ => "TEXT".to_string(),
        },
        Some("bigInt") | Some("bigint") | Some("int8") | Some("integer") | Some("int")
        | Some("int4") => "INTEGER".to_string(),
        Some("smallInt") => "INTEGER".to_string(),
        Some("bytes") => "BLOB".to_string(),
        Some("inet") => "TEXT".to_string(),
        _ => "TEXT".to_string(),
    }
}

fn sqlite_ddl_type(data_type: &str) -> &'static str {
    let lower = data_type.to_ascii_lowercase();
    if lower.starts_with("vector(") || lower.starts_with("geography(") {
        return "BLOB";
    }
    match lower.as_str() {
        "integer" | "int" | "int4" | "smallint" | "int2" | "bigint" | "int8" => "INTEGER",
        "real" | "double precision" => "REAL",
        // `numeric`/`decimal` are EXACT types. SQLite has no fixed-precision decimal
        // storage class; REAL (or NUMERIC) affinity coerces a sufficiently wide
        // decimal through a binary float, silently losing precision and diverging
        // from the documented "exact decimal text" guarantee (dialects.md). Store as
        // TEXT affinity — byte-for-byte decimal text — matching the typed
        // `ColType::Decimal` SQLite override.
        "numeric" | "decimal" => "TEXT",
        "bytea" | "blob" | "geography(point, 4326)" => "BLOB",
        "boolean" => "INTEGER",
        _ => "TEXT",
    }
}

#[cfg(test)]
mod tests {
    use super::{SchemaRenderer, RENDERER};
    use zero_migrate_backend::snapshot::ColumnSnapshot;

    #[test]
    fn collation_hooks_are_explicit_sqlite_pass_throughs() {
        let rendered = "  text COLLATE NOCASE  ";
        assert_eq!(RENDERER.pin_collation(rendered, Some(false)), rendered);
        assert_eq!(RENDERER.strip_collation(rendered), rendered);
    }

    #[test]
    fn every_supported_snapshot_type_has_sqlite_owned_add_column_spelling() {
        let cases = [
            ("text", "TEXT"),
            ("character(8)", "TEXT"),
            ("character varying(64)", "TEXT"),
            ("double precision", "REAL"),
            ("real", "REAL"),
            ("boolean", "INTEGER"),
            ("integer", "INTEGER"),
            ("smallint", "INTEGER"),
            ("bigint", "INTEGER"),
            ("timestamp with time zone", "TEXT"),
            ("date", "TEXT"),
            ("jsonb", "TEXT"),
            ("text[]", "TEXT"),
            ("bytea", "BLOB"),
            ("numeric", "TEXT"),
            ("numeric(20, 4)", "TEXT"),
            ("inet", "TEXT"),
            ("vector(3)", "BLOB"),
            ("geography(POINT, 4326)", "BLOB"),
        ];

        for (data_type, expected) in cases {
            let column = ColumnSnapshot {
                data_type: data_type.to_string(),
                ..Default::default()
            };
            assert_eq!(
                RENDERER.column_type(&column, false),
                expected,
                "{data_type}"
            );
        }
    }
}
