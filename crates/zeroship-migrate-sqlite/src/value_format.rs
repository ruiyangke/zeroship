//! SQLite value-format spelling and catalog normalization.

use zeroship_migrate_backend::snapshot::{ColumnCollationSnapshot, IdDefaultSnapshot};
use zeroship_migrate_backend::value_format::{
    CatalogSqlContext, LiteralCastKind, ValueFormatColumnMetadata, ValueFormatRenderer,
};
use zeroship_migrate_ir::dialect::DialectId;
use zeroship_migrate_ir::expr::Expr;

use crate::DIALECT;

fn literal_cast_kind(compact: &str) -> Option<LiteralCastKind> {
    if compact == "uuid" {
        return Some(LiteralCastKind::Uuid);
    }
    if matches!(compact, "text" | "charactervarying" | "varchar") {
        return Some(LiteralCastKind::Text);
    }
    let numeric_kind = match compact {
        "smallint" | "int2" => Some(LiteralCastKind::SignedInteger { bits: 16 }),
        "integer" | "int" | "int4" => Some(LiteralCastKind::SignedInteger { bits: 32 }),
        "bigint" | "int8" | "signed" => Some(LiteralCastKind::SignedInteger { bits: 64 }),
        "unsigned" => Some(LiteralCastKind::UnsignedInteger { bits: 64 }),
        "numeric" | "decimal" => Some(LiteralCastKind::ExactNumeric),
        "real" | "double" | "doubleprecision" => Some(LiteralCastKind::Real),
        _ => None,
    };
    if numeric_kind.is_some() {
        return numeric_kind;
    }
    matches!(compact, "boolean" | "bool").then_some(LiteralCastKind::Boolean)
}

fn is_catalog_cast_target(compact: &str) -> bool {
    matches!(
        compact,
        "text"
            | "character"
            | "charactervarying"
            | "varchar"
            | "char"
            | "smallint"
            | "integer"
            | "bigint"
            | "int"
            | "int2"
            | "int4"
            | "int8"
            | "numeric"
            | "decimal"
            | "real"
            | "double"
            | "doubleprecision"
            | "boolean"
            | "bool"
            | "bytea"
            | "blob"
            | "binary"
            | "uuid"
    ) || [
        "character",
        "charactervarying",
        "varchar",
        "char",
        "numeric",
        "decimal",
    ]
    .iter()
    .any(|prefix| {
        compact.starts_with(prefix)
            && compact[prefix.len()..]
                .bytes()
                .all(|byte| byte.is_ascii_digit())
    })
}

#[derive(Debug)]
pub(super) struct SqliteValueFormatRenderer;

pub(super) static RENDERER: SqliteValueFormatRenderer = SqliteValueFormatRenderer;

impl ValueFormatRenderer for SqliteValueFormatRenderer {
    fn dialect(&self) -> DialectId {
        DIALECT
    }

    fn normalize_authored_default_expr(&self, _expr: &Expr) -> Option<Expr> {
        None
    }

    fn normalize_text_literal_snapshot(&self, snapshot: IdDefaultSnapshot) -> IdDefaultSnapshot {
        snapshot
    }

    fn normalize_uuid_literal_snapshot(&self, snapshot: IdDefaultSnapshot) -> IdDefaultSnapshot {
        snapshot
    }

    fn catalog_default_is_unquoted_literal(&self, _expression_default: Option<bool>) -> bool {
        false
    }

    fn catalog_default_marker_is_authoritative(&self) -> bool {
        false
    }

    fn authored_storage_uses_rendered_literal(&self) -> bool {
        true
    }

    fn literal_cast_kind(&self, compact_target: &str) -> Option<LiteralCastKind> {
        literal_cast_kind(compact_target)
    }

    fn is_catalog_cast_target(&self, compact_target: &str) -> bool {
        is_catalog_cast_target(compact_target)
    }

    fn canonical_catalog_cast_target(&self, compact_target: &str) -> String {
        compact_target.to_string()
    }

    fn canonical_unattributed_catalog_cast_target(&self, _compact_target: &str) -> Option<String> {
        None
    }

    fn catalog_literal_hex_carrier<'a>(&self, _tokens: &'a [String]) -> Option<&'a str> {
        None
    }

    fn is_catalog_string_introducer(&self, _word: &str, _followed_by_quote: bool) -> bool {
        false
    }

    fn normalize_catalog_tokens(&self, _context: CatalogSqlContext, _tokens: &mut Vec<String>) {}

    fn normalizes_trim_both_from(&self) -> bool {
        false
    }

    fn canonical_catalog_function_name<'a>(&self, name: &'a str) -> &'a str {
        name
    }

    fn canonical_unattributed_catalog_function_name<'a>(&self, _name: &'a str) -> Option<&'a str> {
        None
    }

    fn uuid_generator_candidates(&self, rendered: &str) -> Vec<String> {
        vec![rendered.to_string()]
    }

    fn uuid_column_metadata(&self, quoted: &str) -> Option<ValueFormatColumnMetadata> {
        Some(ValueFormatColumnMetadata {
            ddl_type: "TEXT COLLATE BINARY".to_string(),
            // BINARY is SQLite's canonical default and is represented by None.
            collation: None,
            inline_check: format!(
                "CHECK ({quoted} IS NULL OR (typeof({quoted}) = 'text' AND \
                 length({quoted}) = 36 AND \
                 length(CAST({quoted} AS BLOB)) = 36 AND \
                 substr({quoted}, 9, 1) = '-' AND substr({quoted}, 14, 1) = '-' AND \
                 substr({quoted}, 19, 1) = '-' AND substr({quoted}, 24, 1) = '-' AND \
                 length({quoted}) - length(replace({quoted}, '-', '')) = 4 AND \
                 replace({quoted}, '-', '') NOT GLOB '*[^0-9a-f]*'))"
            ),
        })
    }

    fn bytewise_column_metadata(
        &self,
        rendered_type: &str,
    ) -> (String, Option<ColumnCollationSnapshot>) {
        (format!("{rendered_type} COLLATE BINARY"), None)
    }
}
