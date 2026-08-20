//! MySQL schema/DDL spelling. The future `zero-migrate-mysql`.

use crate::schema::query::{
    def_case_sensitive, mysql_base_column_type_for_def, quote_ident_for_dialect, SchemaRenderer,
    SqlDialect,
};

/// This module's own vendor identity — the ONE dialect literal it is allowed to
/// name. See `backends/mod.rs`.
const DIALECT: SqlDialect = SqlDialect::Mysql;

pub(super) struct MysqlSchemaRenderer;

pub(super) static RENDERER: MysqlSchemaRenderer = MysqlSchemaRenderer;

impl SchemaRenderer for MysqlSchemaRenderer {
    fn dialect(&self) -> SqlDialect {
        DIALECT
    }

    fn foreign_key_target(&self, app_id: &str, target: &str) -> String {
        format!(
            "{}.{}",
            quote_ident_for_dialect(app_id, self.dialect()),
            quote_ident_for_dialect(target, self.dialect())
        )
    }

    /// The MySQL column type for a field def, with an explicit collation on every
    /// CHARACTER spelling.
    ///
    /// The collation is pinned through
    /// [`crate::render::declarative::mysql_pin_collation`] - the same function the
    /// snapshot-carrier renderer pins through - so the two MySQL renderers cannot
    /// answer the same column with different comparison semantics. Without it every
    /// character column this arm emits inherits the table default, which on a stock
    /// MySQL 8 server is `utf8mb4_0900_ai_ci`: `'Active' = 'active'` compares TRUE and
    /// a UNIQUE index rejects the second of the two, where PostgreSQL and SQLite
    /// separate them. See [`mysql_base_column_type_for_def`] for which spellings are
    /// character types and which are deliberately left bare.
    fn column_type(&self, def: &serde_json::Value) -> String {
        crate::render::declarative::mysql_pin_collation(
            &mysql_base_column_type_for_def(def),
            def_case_sensitive(def),
        )
    }

    fn json_object_default(&self) -> String {
        "DEFAULT (JSON_OBJECT())".to_string()
    }

    fn json_array_default(&self) -> String {
        "DEFAULT (JSON_ARRAY())".to_string()
    }

    fn current_timestamp_expr(&self) -> &'static str {
        "CURRENT_TIMESTAMP(6)"
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
