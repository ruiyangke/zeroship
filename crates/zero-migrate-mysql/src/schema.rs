//! MySQL schema/DDL spelling. The future `zero-migrate-mysql`.

use crate::collation::{mysql_pin_collation, mysql_type_without_collation};
use zero_migrate_backend::renderer::DmlRenderer;
use zero_migrate_backend::schema::{
    char_len, decimal_precision_scale, def_case_sensitive, SchemaRenderer,
};
use zero_migrate_backend::snapshot::ColumnSnapshot;
use zero_migrate_ir::dialect::{DialectId, MYSQL};

/// This module's own vendor identity.
const DIALECT: DialectId = MYSQL;

#[derive(Debug)]
pub(super) struct MysqlSchemaRenderer;

pub(super) static RENDERER: MysqlSchemaRenderer = MysqlSchemaRenderer;

impl SchemaRenderer for MysqlSchemaRenderer {
    fn dialect(&self) -> DialectId {
        DIALECT
    }

    fn quote_ident(&self, ident: &str) -> String {
        crate::dml::RENDERER.quote_ident(ident)
    }

    fn ident_quote_char(&self) -> char {
        '`'
    }

    /// MySQL snapshots expose structured catalog facts rather than retaining a
    /// vendor CREATE statement for surgical rewrites.
    fn stored_ddl(&self) -> Option<&'static dyn zero_migrate_backend::stored_ddl::StoredDdl> {
        None
    }

    fn foreign_key_target(&self, app_id: &str, target: &str) -> String {
        format!("{}.{}", self.quote_ident(app_id), self.quote_ident(target))
    }

    fn column_type(&self, c: &ColumnSnapshot, _inline_pk: bool) -> String {
        if let Some(ty) = &c.ddl_type_override {
            return mysql_pin_native_enum_collation(ty, c.case_sensitive);
        }

        let rendered = if c.unbounded_text {
            "text".to_string()
        } else if let Some(def) = &c.type_def {
            mysql_base_column_type_for_def(def)
        } else if matches!(c.case_sensitive, Some(false))
            && (c.authored_type || c.data_type.eq_ignore_ascii_case("text"))
        {
            "text".to_string()
        } else {
            mysql_ddl_type(&c.data_type)
        };

        if c.authored_type || c.type_def.is_some() {
            let case_sensitive = c
                .type_def
                .as_ref()
                .and_then(def_case_sensitive)
                .or(c.case_sensitive);
            self.pin_collation(&rendered, case_sensitive)
        } else {
            mysql_pin_native_enum_collation(&rendered, c.case_sensitive)
        }
    }

    fn canonical_type(&self, raw: &str) -> String {
        mysql_canonical_type(raw)
    }

    fn create_table_target(&self, app_id: &str, collection: &str, _unqualified: bool) -> String {
        format!(
            "{}.{}",
            self.quote_ident(app_id),
            self.quote_ident(collection)
        )
    }

    fn injected_index_statement(
        &self,
        app_id: &str,
        collection: &str,
        index_name: &str,
        unique: bool,
        columns: &[&str],
        _unqualified: bool,
    ) -> String {
        let unique_clause = if unique { "UNIQUE " } else { "" };
        let rendered_columns = columns
            .iter()
            .map(|column| self.quote_ident(column))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "CREATE {unique_clause}INDEX {} ON {}.{} ({rendered_columns})",
            self.quote_ident(index_name),
            self.quote_ident(app_id),
            self.quote_ident(collection),
        )
    }

    fn schema_string_literal(&self, value: &str) -> String {
        format!("_utf8mb4 X'{}'", hex::encode(value.as_bytes()))
    }

    fn injected_column_ident(&self, name: &str, _canonical_bare: bool) -> String {
        self.quote_ident(name)
    }

    /// InnoDB has no deferred constraint checks, so its `RESTRICT` and
    /// `NO ACTION` catalog/render forms collapse to the same default.
    fn canonical_fk_action(&self, action: &'static str) -> &'static str {
        if matches!(action, "RESTRICT" | "NO ACTION") {
            "NO ACTION"
        } else {
            action
        }
    }

    fn suppress_string_enum_check(&self, def: &serde_json::Value) -> bool {
        zero_migrate_backend::schema::string_enum_values(def).is_some()
    }

    fn pin_collation(&self, rendered: &str, case_sensitive: Option<bool>) -> String {
        mysql_pin_collation(rendered, case_sensitive)
    }

    fn strip_collation<'a>(&self, rendered: &'a str) -> &'a str {
        mysql_type_without_collation(rendered)
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

fn parse_character_type_len(data_type: &str) -> Option<u64> {
    let lower = data_type.trim().to_ascii_lowercase();
    let inner = lower
        .strip_prefix("character(")
        .or_else(|| lower.strip_prefix("char("))
        .or_else(|| lower.strip_prefix("bpchar("))?
        .strip_suffix(')')?;
    inner.parse::<u64>().ok().filter(|len| *len > 0)
}

/// Canonicalise MySQL `information_schema.COLUMNS.COLUMN_TYPE` / rendered DDL
/// type strings for drift/probe comparison.
#[must_use]
fn mysql_canonical_type(data_type: &str) -> String {
    let lower = data_type.trim().to_ascii_lowercase();
    // Strip an explicit `CHARACTER SET ... COLLATE ...` clause: it is the column's
    // collation, which the base-family canonicalization ignores (charset/collation
    // is compared independently). `VARCHAR(255) CHARACTER SET utf8mb4 COLLATE
    // utf8mb4_0900_as_cs` and a bare `varchar(255)` canonicalize to the same base family.
    let lower = lower
        .split(" character set ")
        .next()
        .and_then(|head| head.split(" collate ").next())
        .unwrap_or(&lower)
        .trim()
        .to_string();
    let no_width = strip_mysql_int_display_width(&lower);
    if no_width.starts_with("enum(") {
        return no_width;
    }
    if no_width == "varchar(43)" || no_width == "inet" {
        return "inet".to_string();
    }
    if let Some(len) = parse_character_type_len(&no_width) {
        return format!("character({len})");
    }
    // `character varying(n)` is the DIALECT-NEUTRAL spelling a bounded `t.string({
    // length })` carries in `data_type`; `varchar(n)` is what MySQL's catalog reports for
    // the same column. Both must fold to the same family or the differ sees a phantom
    // type change on every bounded string and refuses the deploy. Measured: a live
    // MySQL declarative re-deploy of a `character varying(191)` id column was refused
    // with `MysqlAlterColumnUnsupported { change: "type" }` until this arm existed.
    if no_width.starts_with("varchar(")
        || no_width.starts_with("character varying(")
        || no_width == "character varying"
        || no_width.ends_with("text")
        || no_width == "char"
    {
        return "text".to_string();
    }
    if no_width.starts_with("varbinary(") || no_width.ends_with("blob") || no_width == "bytea" {
        return "blob".to_string();
    }
    if no_width.starts_with("datetime")
        || no_width.starts_with("timestamp")
        || matches!(
            no_width.as_str(),
            "timestamp with time zone" | "timestamptz"
        )
    {
        return "datetime".to_string();
    }
    if no_width.starts_with("decimal") || no_width == "numeric" {
        return "decimal".to_string();
    }
    if no_width.starts_with("double") || matches!(no_width.as_str(), "double precision" | "float8")
    {
        return "double".to_string();
    }
    if matches!(no_width.as_str(), "float" | "real" | "float4") {
        return "real".to_string();
    }
    if no_width.starts_with("tinyint(1)") || no_width == "boolean" {
        return "boolean".to_string();
    }
    match no_width.as_str() {
        "smallint" | "int2" => "smallint".to_string(),
        "int" | "integer" | "int4" => "int".to_string(),
        "bigint" | "int8" => "bigint".to_string(),
        "json" | "jsonb" | "text[]" => "json".to_string(),
        "date" => "date".to_string(),
        "point" | "point srid 4326" | "geography(point, 4326)" | "geography(POINT, 4326)" => {
            "point".to_string()
        }
        other => other.to_string(),
    }
}

fn strip_mysql_int_display_width(input: &str) -> String {
    for ty in [
        "tinyint",
        "smallint",
        "mediumint",
        "int",
        "integer",
        "bigint",
    ] {
        if let Some(rest) = input.strip_prefix(ty) {
            if let Some(after_open) = rest.strip_prefix('(') {
                if let Some((digits, after_close)) = after_open.split_once(')') {
                    if digits.chars().all(|c| c.is_ascii_digit()) {
                        return format!("{ty}{after_close}");
                    }
                }
            }
        }
    }
    input.to_string()
}

/// Pin an explicit collation onto a rendered MySQL `ENUM(...)` spelling.
///
/// Snapshot-native catalog carriers historically pin only native enums here;
/// authored character columns take the broader pass in `column_type`. Keeping the
/// distinction preserves catalog-carrier bytes while the neutral authored marker
/// replaces the vendor-spelled override that core used to precompute.
fn mysql_pin_native_enum_collation(rendered: &str, case_sensitive: Option<bool>) -> String {
    if !rendered.trim().to_ascii_lowercase().starts_with("enum(") {
        return rendered.to_string();
    }
    mysql_pin_collation(rendered, case_sensitive)
}

fn mysql_ddl_type(data_type: &str) -> String {
    let lower = data_type.trim().to_ascii_lowercase();
    if lower.starts_with("enum(") {
        return data_type.to_string();
    }
    if lower.starts_with("vector(") {
        return "BLOB".to_string();
    }
    if let Some(len) = char_len_from_data_type(&lower) {
        return format!("CHAR({len})");
    }
    if let Some(len) = varchar_len_from_data_type(&lower) {
        return format!("VARCHAR({len})");
    }
    match lower.as_str() {
        "text" => "VARCHAR(191)".to_string(),
        "double precision" | "float8" => "DOUBLE".to_string(),
        "real" | "float4" => "FLOAT".to_string(),
        "boolean" => "TINYINT(1)".to_string(),
        "timestamp with time zone" | "timestamptz" => "DATETIME(6)".to_string(),
        "date" => "DATE".to_string(),
        "jsonb" | "json" => "JSON".to_string(),
        "text[]" => "JSON".to_string(),
        "bytea" | "blob" => "LONGBLOB".to_string(),
        "numeric" | "decimal" => "DECIMAL(65, 30)".to_string(),
        "integer" | "int" | "int4" => "INT".to_string(),
        "smallint" | "int2" => "SMALLINT".to_string(),
        "bigint" | "int8" => "BIGINT".to_string(),
        "inet" => "VARCHAR(43)".to_string(),
        "geography(point, 4326)" | "geography(POINT, 4326)" => "POINT SRID 4326".to_string(),
        other => other.to_string(),
    }
}

fn char_len_from_data_type(data_type: &str) -> Option<u32> {
    let lower = data_type.trim().to_ascii_lowercase();
    let inner = lower
        .strip_prefix("character(")
        .or_else(|| lower.strip_prefix("char("))
        .or_else(|| lower.strip_prefix("bpchar("))?
        .strip_suffix(')')?;
    inner.parse::<u32>().ok().filter(|len| *len > 0)
}

fn varchar_len_from_data_type(data_type: &str) -> Option<u32> {
    let lower = data_type.trim().to_ascii_lowercase();
    let inner = lower
        .strip_prefix("character varying(")
        .or_else(|| lower.strip_prefix("varchar("))?
        .strip_suffix(')')?;
    inner.parse::<u32>().ok().filter(|len| *len > 0)
}

fn mysql_native_enum_values(def: &serde_json::Value) -> Option<Vec<String>> {
    let values = def.get("enum")?.as_array()?;
    let mut rendered = Vec::with_capacity(values.len());
    for value in values {
        let s = value.as_str()?;
        // MySQL's ENUM value grammar accepts a bare hex literal but rejects the
        // `_utf8mb4 X'…'` introduced form used in expression positions. The
        // column's utf8mb4 character set consumes these UTF-8 bytes while the hex
        // spelling remains independent of `NO_BACKSLASH_ESCAPES`.
        rendered.push(format!("X'{}'", hex::encode(s.as_bytes())));
    }
    if rendered.is_empty() {
        None
    } else {
        Some(rendered)
    }
}

/// Legacy SDK-token-to-MySQL-spelling table, moved out of the neutral contract.
///
/// Snapshot rendering no longer calls this JSON carrier. It remains vendor-owned
/// while the remaining SDK-definition producers lower to neutral snapshots.
pub fn mysql_base_column_type_for_def(def: &serde_json::Value) -> String {
    if def.get("encrypted").is_some() {
        return "LONGBLOB".to_string();
    }

    if let Some(values) = mysql_native_enum_values(def) {
        return format!("ENUM({})", values.join(", "));
    }

    let zs_type = def.get("type").and_then(|t| t.as_str());

    if zs_type == Some("vector") {
        return "BLOB".to_string();
    }

    if zs_type == Some("geoPoint") {
        return "POINT SRID 4326".to_string();
    }

    // The decimal half of the shared `number` token. `DOUBLE` is right for the
    // float and wrong for `t.numeric({ precision, scale })`; the MySQL arm of
    // `render::lower::author_type_override` already spells this column
    // `DECIMAL(p, s)` on the snapshot carrier, so this is the field-def carrier
    // catching up rather than a second opinion. Note that a BARE `DECIMAL` would
    // not do: MySQL reads it as `DECIMAL(10, 0)` and silently truncates the
    // scale, which is why the parameters have to reach this emitter at all.
    if zs_type == Some("number") {
        if let Some((precision, scale)) = decimal_precision_scale(def) {
            return format!("DECIMAL({precision}, {scale})");
        }
    }

    match zs_type {
        Some("string") => {
            let max = def
                .get("maxLength")
                .or_else(|| def.get("max"))
                .and_then(serde_json::Value::as_u64)
                .filter(|n| *n > 0 && *n <= 65_535);
            match max {
                Some(n) if n <= 16_383 => format!("VARCHAR({n})"),
                Some(_) => "LONGTEXT".to_string(),
                None => "VARCHAR(191)".to_string(),
            }
        }
        Some("char") => match char_len(def) {
            Some(len) => format!("CHAR({len})"),
            None => "CHAR(1)".to_string(),
        },
        Some("number") => "DOUBLE".to_string(),
        Some("real") => "FLOAT".to_string(),
        Some("boolean") => "TINYINT(1)".to_string(),
        Some("date") => "DATETIME(6)".to_string(),
        Some("calendarDate") => "DATE".to_string(),
        Some("json") | Some("object") | Some("array") | Some("union") => "JSON".to_string(),
        Some("textArray") => "JSON".to_string(),
        Some("ref") => "VARCHAR(191)".to_string(),
        Some("bytes") => "LONGBLOB".to_string(),
        Some("literal") => match def.get("literalValue") {
            Some(serde_json::Value::Number(_)) => "DECIMAL(65, 30)".to_string(),
            Some(serde_json::Value::Bool(_)) => "TINYINT(1)".to_string(),
            _ => "VARCHAR(191)".to_string(),
        },
        Some("bigInt") | Some("bigint") | Some("int8") => "BIGINT".to_string(),
        Some("integer") | Some("int") | Some("int4") => "INT".to_string(),
        Some("smallInt") => "SMALLINT".to_string(),
        Some("inet") => "VARCHAR(43)".to_string(),
        _ => "VARCHAR(191)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{SchemaRenderer, RENDERER};
    use zero_migrate_backend::snapshot::ColumnSnapshot;

    const PIN: &str = "CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_as_cs";

    #[test]
    fn both_unbounded_text_carriers_converge_on_text() {
        let ir = ColumnSnapshot {
            data_type: "text".to_string(),
            unbounded_text: true,
            authored_type: true,
            ..Default::default()
        };
        let descriptor = ColumnSnapshot {
            data_type: "text".to_string(),
            unbounded_text: true,
            type_def: Some(serde_json::json!({ "type": "string" })),
            authored_type: true,
            ..Default::default()
        };

        let expected = format!("text {PIN}");
        assert_eq!(RENDERER.column_type(&ir, false), expected);
        assert_eq!(RENDERER.column_type(&descriptor, false), expected);
    }

    #[test]
    fn a_bounded_string_remains_varchar() {
        let bounded = ColumnSnapshot {
            data_type: "character varying(64)".to_string(),
            authored_type: true,
            ..Default::default()
        };
        assert_eq!(
            RENDERER.column_type(&bounded, false),
            format!("VARCHAR(64) {PIN}")
        );
    }

    #[test]
    fn collation_contract_pins_and_strips_the_vendor_suffix() {
        let case_sensitive = RENDERER.pin_collation("VARCHAR(128)", None);
        assert_eq!(case_sensitive, format!("VARCHAR(128) {PIN}"));
        assert_eq!(RENDERER.strip_collation(&case_sensitive), "VARCHAR(128)");

        let case_insensitive = RENDERER.pin_collation("TEXT", Some(false));
        assert_eq!(
            case_insensitive,
            "TEXT CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci"
        );
        assert_eq!(RENDERER.strip_collation(&case_insensitive), "TEXT");

        assert_eq!(RENDERER.pin_collation("JSON", None), "JSON");
        assert_eq!(RENDERER.strip_collation("JSON"), "JSON");
    }

    #[test]
    fn catalog_carrier_preserves_the_old_snapshot_dispatch_domain() {
        let bounded = ColumnSnapshot {
            data_type: "character varying(64)".to_string(),
            ..Default::default()
        };
        assert_eq!(RENDERER.column_type(&bounded, false), "VARCHAR(64)");

        let non_text_case_facet = ColumnSnapshot {
            data_type: "integer".to_string(),
            case_sensitive: Some(false),
            ..Default::default()
        };
        assert_eq!(RENDERER.column_type(&non_text_case_facet, false), "INT");

        let whitespace_override = ColumnSnapshot {
            ddl_type_override: Some("  JSON  ".to_string()),
            authored_type: true,
            ..Default::default()
        };
        assert_eq!(
            RENDERER.column_type(&whitespace_override, false),
            "  JSON  "
        );

        let native_enum = ColumnSnapshot {
            data_type: "ENUM('open', 'closed')".to_string(),
            ..Default::default()
        };
        assert_eq!(
            RENDERER.column_type(&native_enum, false),
            format!("ENUM('open', 'closed') {PIN}")
        );
    }
}
