//! SQLite schema/DDL spelling.

use zero_migrate_backend::ddl::ExclusionConstraintRequest;
use zero_migrate_backend::renderer::DmlRenderer;
use zero_migrate_backend::schema::{
    decimal_precision_scale, AddColumnIfNotExistsRequest, CreateIndexIfNotExistsRequest,
    SchemaRenderer,
};
use zero_migrate_backend::snapshot::ColumnSnapshot;
use zero_migrate_ir::dialect::DialectId;

// This module's vendor identity, read from the crate's ONE declaration of it.
use crate::DIALECT;

#[derive(Debug)]
pub(super) struct SqliteSchemaRenderer;

pub(super) static RENDERER: SqliteSchemaRenderer = SqliteSchemaRenderer;

/// Whether this column is `SQLite`'s rowid-alias `INTEGER PRIMARY KEY
/// AUTOINCREMENT` shape - an auto-increment identity, inline PK, over one of the
/// integer storage classes.
#[must_use]
pub(super) fn sqlite_auto_increment_identity_pk(c: &ColumnSnapshot, inline_pk: bool) -> bool {
    matches!(c.identity, Some(identity) if !identity.always)
        && inline_pk
        && matches!(
            c.data_type.to_ascii_lowercase().as_str(),
            "integer" | "bigint" | "smallint" | "int" | "int2" | "int4" | "int8"
        )
}

impl SchemaRenderer for SqliteSchemaRenderer {
    fn dialect(&self) -> DialectId {
        DIALECT
    }

    fn quote_ident(&self, ident: &str) -> String {
        crate::dml::RENDERER.quote_ident(ident)
    }

    fn ident_quote_char(&self) -> char {
        '"'
    }

    fn stored_ddl(&self) -> Option<&'static dyn zero_migrate_backend::stored_ddl::StoredDdl> {
        Some(&crate::stored_ddl::PARSER)
    }

    fn table_rebuild_policy(
        &self,
    ) -> Option<&'static dyn zero_migrate_backend::table_rebuild::TableRebuildPolicy> {
        Some(&crate::table_rebuild::POLICY)
    }

    fn foreign_key_target(&self, _app_id: &str, target: &str) -> String {
        self.quote_ident(target)
    }

    /// SQLite REFERENCES grammar prohibits the database/schema qualifier.
    fn canonical_fk_target(&self, _schema: &str, target: &str) -> String {
        target.to_string()
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

    fn snapshot_data_type(&self, c: &ColumnSnapshot) -> String {
        let rendered = self.column_type(c, false);
        rendered
            .split_once(" COLLATE ")
            .map_or(rendered.as_str(), |(base, _)| base)
            .trim()
            .to_ascii_lowercase()
    }

    /// SQLite compares its declared type through `canonical_type`; it carries no
    /// separate vendor-only physical-type projection on the neutral snapshot.
    /// Consume the neutral definition after deriving `data_type`; otherwise its
    /// generic token spelling would outrank SQLite-only facets such as NOCASE.
    fn finalize_column_snapshot(&self, column: &mut ColumnSnapshot) {
        column.type_def = None;
    }

    fn project_derived_ann_index(
        &self,
        index: &mut zero_migrate_backend::snapshot::IndexSnapshot,
    ) -> bool {
        index.access_method = "btree".to_string();
        index.opclass = None;
        true
    }

    /// SQLite has no additional key-storage restriction at this seam.
    fn validate_key_storage(
        &self,
        _desired: &zero_migrate_backend::snapshot::SchemaSnapshot,
        _live: &zero_migrate_backend::snapshot::SchemaSnapshot,
    ) -> Result<(), String> {
        Ok(())
    }

    fn unprefixed_key_storage_refusal(
        &self,
        _position: &str,
        _table: &str,
        _column: &str,
        _evidence: zero_migrate_backend::schema::KeyStorageEvidence<'_>,
    ) -> Option<zero_migrate_backend::schema::StorageValidationRefusal> {
        // SQLite has no prefix-length syntax or corresponding storage-family
        // restriction for the key shapes this engine authors.
        None
    }

    fn literal_default_storage_refusal(
        &self,
        _column: &str,
        _rendered_type: &str,
        _rendered_default: &str,
    ) -> Option<zero_migrate_backend::schema::StorageValidationRefusal> {
        // SQLite accepts literal defaults independently of declared type affinity.
        None
    }

    fn dual_write_trigger(
        &self,
        _spec: &zero_migrate_backend::schema::DualWriteTriggerSpec<'_>,
    ) -> Option<zero_migrate_backend::schema::DualWriteTriggerSql> {
        // This backend reconciles a rename by REBUILDING the table
        // (`ColumnRenameStrategy::TableRebuild` below), so there is no window in which
        // two columns coexist and nothing for a dual-write trigger to keep equal.
        None
    }

    fn existing_column_change_strategy(
        &self,
    ) -> zero_migrate_backend::schema::ExistingColumnChangeStrategy {
        zero_migrate_backend::schema::ExistingColumnChangeStrategy::TableRebuild
    }

    fn column_rename_strategy(&self) -> zero_migrate_backend::schema::ColumnRenameStrategy {
        zero_migrate_backend::schema::ColumnRenameStrategy::TableRebuild
    }

    fn supports_forward_inline_foreign_key(&self) -> bool {
        true
    }

    fn identity_column_type_allowed(&self, _data_type: &str) -> bool {
        true
    }

    /// Never read: this backend admits every type above, so its refusal is
    /// unreachable. Stated rather than defaulted so the answer is deliberate.
    fn identity_column_type_confinement(&self) -> &'static str {
        "any type a column of this target may have"
    }

    fn canonical_type(&self, raw: &str) -> String {
        sqlite_canonical_type(raw).to_string()
    }

    fn create_table_target(&self, app_id: &str, collection: &str, unqualified: bool) -> String {
        let backend = self;
        let sqlite_table_unqualified = unqualified;
        let table = if sqlite_table_unqualified {
            backend.quote_ident(collection)
        } else {
            format!(
                "{}.{}",
                backend.quote_ident(app_id),
                backend.quote_ident(collection)
            )
        };
        table
    }

    fn injected_index_statement(
        &self,
        app_id: &str,
        collection: &str,
        index_name: &str,
        unique: bool,
        columns: &[&str],
        unqualified: bool,
    ) -> String {
        let unique_clause = if unique { "UNIQUE " } else { "" };
        let rendered_columns = columns
            .iter()
            .map(|column| self.quote_ident(column))
            .collect::<Vec<_>>()
            .join(", ");
        if unqualified {
            format!(
                "CREATE {unique_clause}INDEX IF NOT EXISTS {} ON {} ({rendered_columns})",
                self.quote_ident(index_name),
                self.quote_ident(collection),
            )
        } else {
            format!(
                "CREATE {unique_clause}INDEX IF NOT EXISTS {}.{} ON {} ({rendered_columns})",
                self.quote_ident(app_id),
                self.quote_ident(index_name),
                self.quote_ident(collection),
            )
        }
    }

    fn schema_string_literal(&self, value: &str) -> String {
        format!("'{}'", value.replace('\'', "''"))
    }

    fn schema_grammar_string_literal(&self, value: &str) -> String {
        zero_migrate_backend::dml::sql_string_literal(value)
    }

    fn empty_json_expr(&self, object: bool) -> &'static str {
        if object {
            "'{}'"
        } else {
            "'[]'"
        }
    }

    /// SQLite has no text-array storage type, so an empty text-array default is
    /// explicitly unavailable rather than inherited from another backend.
    fn empty_text_array_expr(&self) -> Option<&'static str> {
        None
    }

    fn json_value_default_expr(&self, json: &str) -> String {
        zero_migrate_backend::dml::sql_string_literal(json)
    }

    fn injected_column_ident(&self, name: &str, canonical_bare: bool) -> String {
        if canonical_bare {
            name.to_string()
        } else {
            self.quote_ident(name)
        }
    }

    /// SQLite preserves `RESTRICT` and `NO ACTION` as distinct stored-DDL
    /// spellings, so its explicit canonicalizer is an identity.
    fn canonical_fk_action(&self, action: &'static str) -> &'static str {
        action
    }

    /// SQLite string enums are TEXT columns whose membership CHECK carries the
    /// constraint, so it never suppresses that CHECK.
    fn suppress_string_enum_check(&self, _def: &serde_json::Value) -> bool {
        false
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

    fn add_foreign_key_statement(
        &self,
        _schema: &str,
        _table: &str,
        _clause: &str,
    ) -> Result<String, &'static str> {
        Err("SQLite cannot add a foreign key without rebuilding the table")
    }

    fn drop_foreign_key_if_exists_statement(
        &self,
        _schema: &str,
        _table: &str,
        _name: &str,
    ) -> Result<String, &'static str> {
        Err("SQLite cannot drop a foreign key without rebuilding the table")
    }

    fn add_column_if_not_exists_statements(
        &self,
        _request: AddColumnIfNotExistsRequest<'_>,
    ) -> Result<Vec<String>, &'static str> {
        Err("SQLite has no ADD COLUMN IF NOT EXISTS grammar")
    }

    fn create_index_if_not_exists_statement(
        &self,
        _request: CreateIndexIfNotExistsRequest<'_>,
    ) -> Result<String, &'static str> {
        Err("SQLite has no concurrent index-build grammar")
    }

    /// REFUSED: SQLite has no exclusion constraint.
    ///
    /// `None` rather than an approximation. The nearest SQLite shape is a unique
    /// index, which excludes only on equality and would silently accept an authored
    /// overlap constraint while enforcing something narrower. The engine turns this
    /// into its own refusal, naming this dialect.
    fn exclusion_constraint_body(&self, _req: &ExclusionConstraintRequest<'_>) -> Option<String> {
        None
    }
}

/// Canonicalise a column type to the SQLite affinity token used when comparing
/// PG-spelled desired snapshots against live SQLite declared types.
#[must_use]
fn sqlite_canonical_type(data_type: &str) -> &'static str {
    let lower = data_type.trim().to_ascii_lowercase();
    // Parameterised extension types keep their DDL spelling in the snapshot
    // (`vector(384)`, `geography(POINT, 4326)`); both emit BLOB on SQLite.
    if lower.starts_with("vector(")
        || lower == "vector"
        || lower.starts_with("geography(")
        || lower.starts_with("geometry(")
    {
        return "blob";
    }
    match lower.as_str() {
        // TEXT affinity: PG `text`/`jsonb`/`timestamp with time zone`/`date`
        // (date->TIMESTAMPTZ, calendarDate->DATE on PG; both -> SQLite TEXT), and the
        // live SQLite `text` token itself.
        "text"
        | "text[]"
        | "jsonb"
        | "json"
        | "timestamp with time zone"
        | "timestamptz"
        | "date"
        | "inet"
        | "character"
        | "char"
        | "bpchar" => "text",
        // REAL affinity: PG `double precision` (`t.number()`), and live `real`.
        "double precision" | "float8" | "real" => "real",
        // INTEGER affinity: PG `boolean`/`integer` (and `bigint`), and live `integer`.
        "boolean" | "integer" | "bigint" | "smallint" | "int8" | "int4" | "int2" | "int" => {
            "integer"
        }
        // TEXT affinity: `numeric`/`decimal` (a numeric `t.literal()`, `t.numeric()`)
        // are stored as exact decimal TEXT on SQLite - no fixed-precision storage
        // class - matching the emitter and the `t.numeric()` override. A live
        // `numeric`/`decimal` declaration canonicalises the same way, so the model
        // and introspection agree instead of drifting (numeric-vs-real).
        "numeric" | "decimal" => "text",
        // BLOB affinity: PG `bytea` (encrypted / `t.bytes()`), and live `blob`.
        "bytea" | "blob" => "blob",
        // Unknown / future spelling: fall back to TEXT (SQLite's catch-all affinity,
        // matching the emitter's `_ => TEXT` arm). An unrecognised pair still
        // compares equal-to-equal by its own lowercased form first (see the caller),
        // so this fallback only collapses genuinely unmapped tokens.
        _ => "text",
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
        // TEXT affinity - byte-for-byte decimal text - matching the typed
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
    fn canonical_type_is_owned_by_sqlite() {
        assert_eq!(RENDERER.canonical_type("timestamp with time zone"), "text");
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
