//! SQLite value-format spelling and catalog normalization.

use zero_migrate_backend::dml::sql_string_literal;
use zero_migrate_backend::snapshot::{ColumnCollationSnapshot, IdDefaultSnapshot};
use zero_migrate_backend::value_format::{
    CatalogSqlContext, LiteralCastKind, ValueFormatColumnMetadata, ValueFormatRenderer,
};
use zero_migrate_ir::dialect::{DialectId, SQLITE};
use zero_migrate_ir::expr::Expr;
use zero_migrate_ir::ir::{validate_type_id_prefix, ValueFormat};

const DIALECT: DialectId = SQLITE;

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

    fn recovery_candidates(
        &self,
        literals: &[String],
        type_id_alphabet: &str,
        ulid_alphabet: &str,
    ) -> Vec<ValueFormat> {
        let mut candidates = Vec::new();
        let lower_guard = format!("*[^{type_id_alphabet}]*");
        let upper_guard = format!("*[^{ulid_alphabet}]*");
        if literals.iter().any(|literal| literal == &upper_guard) {
            candidates.push(ValueFormat::Ulid);
        }
        if literals.iter().any(|literal| literal == &lower_guard) {
            let stored_prefix = literals.iter().find(|literal| {
                literal.ends_with('_') && !literal.starts_with('*') && literal.as_str() != "[0-7]"
            });
            let prefix = stored_prefix.map_or_else(String::new, |stored| {
                stored.strip_suffix('_').unwrap_or(stored).to_string()
            });
            if validate_type_id_prefix(&prefix).is_ok() {
                candidates.push(ValueFormat::TypeId { prefix });
            }
        }
        candidates
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

    fn ulid_column_metadata(
        &self,
        quoted: &str,
        _regex: &str,
        len: usize,
    ) -> ValueFormatColumnMetadata {
        ValueFormatColumnMetadata {
            ddl_type: "TEXT COLLATE BINARY".to_string(),
            collation: None,
            inline_check: format!(
                "CHECK ({quoted} IS NULL OR (typeof({quoted}) = 'text' AND \
                 length({quoted}) = {len} AND \
                 length(CAST({quoted} AS BLOB)) = {len} AND \
                 substr({quoted}, 1, 1) GLOB '[0-7]' AND \
                 substr({quoted}, 1, {len}) NOT GLOB \
                 '*[^0123456789ABCDEFGHJKMNPQRSTVWXYZ]*'))"
            ),
        }
    }

    fn type_id_column_metadata(
        &self,
        quoted: &str,
        stored_prefix: &str,
        suffix_start: usize,
        total_len: usize,
        suffix_len: usize,
        alphabet: &str,
        _regex: &str,
    ) -> ValueFormatColumnMetadata {
        let prefix_predicate = if stored_prefix.is_empty() {
            String::new()
        } else {
            format!(
                " AND substr({quoted}, 1, {}) = {} COLLATE BINARY",
                stored_prefix.len(),
                sql_string_literal(stored_prefix)
            )
        };
        ValueFormatColumnMetadata {
            ddl_type: "TEXT COLLATE BINARY".to_string(),
            collation: None,
            inline_check: format!(
                "CHECK ({quoted} IS NULL OR (typeof({quoted}) = 'text' AND \
                 length({quoted}) = {total_len} AND \
                 length(CAST({quoted} AS BLOB)) = {total_len}{prefix_predicate} AND \
                 substr({quoted}, {suffix_start}, 1) GLOB '[0-7]' AND \
                 substr({quoted}, {suffix_start}, {suffix_len}) NOT GLOB \
                 '*[^{alphabet}]*'))"
            ),
        }
    }

    fn bytewise_column_metadata(
        &self,
        rendered_type: &str,
    ) -> (String, Option<ColumnCollationSnapshot>) {
        (format!("{rendered_type} COLLATE BINARY"), None)
    }
}
