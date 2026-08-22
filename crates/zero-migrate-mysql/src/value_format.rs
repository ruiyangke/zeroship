//! MySQL value-format spelling and catalog normalization.

use zero_migrate_backend::dml::mysql_grammar_string_literal;
use zero_migrate_backend::snapshot::{ColumnCollationSnapshot, IdDefaultSnapshot};
use zero_migrate_backend::value_format::{
    CatalogSqlContext, LiteralCastKind, ValueFormatColumnMetadata, ValueFormatRenderer,
};
use zero_migrate_ir::dialect::{DialectId, MYSQL};
use zero_migrate_ir::expr::Expr;
use zero_migrate_ir::ir::ValueFormat;

const DIALECT: DialectId = MYSQL;

fn literal_cast_kind(compact: &str) -> Option<LiteralCastKind> {
    let compact = compact
        .split_once("charset")
        .map_or(compact, |(base, _)| base);
    if compact == "uuid" {
        return Some(LiteralCastKind::Uuid);
    }
    if matches!(
        compact,
        "text" | "charactervarying" | "varchar" | "character" | "char"
    ) {
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
    let compact = compact
        .split_once("charset")
        .map_or(compact, |(base, _)| base);
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
pub(super) struct MysqlValueFormatRenderer;

pub(super) static RENDERER: MysqlValueFormatRenderer = MysqlValueFormatRenderer;

impl ValueFormatRenderer for MysqlValueFormatRenderer {
    fn dialect(&self) -> DialectId {
        DIALECT
    }

    fn normalize_authored_default_expr(&self, _expr: &Expr) -> Option<Expr> {
        None
    }

    fn normalize_text_literal_snapshot(&self, snapshot: IdDefaultSnapshot) -> IdDefaultSnapshot {
        let IdDefaultSnapshot::Literal(fingerprint) = snapshot else {
            return snapshot;
        };
        let stored = if let Ok(value) = serde_json::from_str::<String>(&fingerprint) {
            value
        } else if fingerprint == "true" {
            "1".to_string()
        } else if fingerprint == "false" {
            "0".to_string()
        } else if zero_migrate_ir::ir::is_decimal_string(&fingerprint) {
            fingerprint
                .strip_prefix('+')
                .unwrap_or(&fingerprint)
                .to_string()
        } else {
            return IdDefaultSnapshot::Literal(fingerprint);
        };
        IdDefaultSnapshot::Literal(
            serde_json::to_string(&stored).expect("string serialization is infallible"),
        )
    }

    fn normalize_uuid_literal_snapshot(&self, snapshot: IdDefaultSnapshot) -> IdDefaultSnapshot {
        snapshot
    }

    fn catalog_default_is_unquoted_literal(&self, expression_default: Option<bool>) -> bool {
        expression_default == Some(false)
    }

    fn catalog_default_marker_is_authoritative(&self) -> bool {
        true
    }

    fn authored_storage_uses_rendered_literal(&self) -> bool {
        false
    }

    fn literal_cast_kind(&self, compact_target: &str) -> Option<LiteralCastKind> {
        literal_cast_kind(compact_target)
    }

    fn is_catalog_cast_target(&self, compact_target: &str) -> bool {
        is_catalog_cast_target(compact_target)
    }

    fn canonical_catalog_cast_target(&self, compact_target: &str) -> String {
        compact_target
            .split_once("charset")
            .map_or(compact_target, |(base, _)| base)
            .to_string()
    }

    fn canonical_unattributed_catalog_cast_target(&self, compact_target: &str) -> Option<String> {
        Some(self.canonical_catalog_cast_target(compact_target))
    }

    fn catalog_literal_hex_carrier<'a>(&self, tokens: &'a [String]) -> Option<&'a str> {
        (tokens.len() == 3 && tokens[0].starts_with('_') && tokens[1] == "x")
            .then(|| tokens[2].as_str())
    }

    fn is_catalog_string_introducer(&self, word: &str, followed_by_quote: bool) -> bool {
        followed_by_quote && word.starts_with('_')
    }

    fn normalize_catalog_tokens(&self, _context: CatalogSqlContext, _tokens: &mut Vec<String>) {}

    fn normalizes_trim_both_from(&self) -> bool {
        false
    }

    fn canonical_catalog_function_name<'a>(&self, name: &'a str) -> &'a str {
        match name {
            "now" | "current_timestamp" => "current_timestamp",
            "ceil" | "ceiling" => "ceil",
            _ => name,
        }
    }

    fn canonical_unattributed_catalog_function_name<'a>(&self, _name: &'a str) -> Option<&'a str> {
        None
    }

    fn uuid_generator_candidates(&self, rendered: &str) -> Vec<String> {
        vec![rendered.to_string()]
    }

    fn recovery_candidates(
        &self,
        _literals: &[String],
        _type_id_alphabet: &str,
        _ulid_alphabet: &str,
    ) -> Vec<ValueFormat> {
        Vec::new()
    }

    fn uuid_column_metadata(&self, quoted: &str) -> Option<ValueFormatColumnMetadata> {
        let regex = mysql_grammar_string_literal(
            "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$",
        );
        Some(ValueFormatColumnMetadata {
            ddl_type: "VARCHAR(36) CHARACTER SET ascii COLLATE ascii_bin".to_string(),
            collation: None,
            inline_check: format!(
                "CHECK ({quoted} IS NULL OR (CHAR_LENGTH({quoted}) = 36 AND \
                 REGEXP_LIKE({quoted}, {regex}, 'c')))"
            ),
        })
    }

    fn ulid_column_metadata(
        &self,
        quoted: &str,
        regex: &str,
        len: usize,
    ) -> ValueFormatColumnMetadata {
        let regex = mysql_grammar_string_literal(regex);
        ValueFormatColumnMetadata {
            ddl_type: "VARCHAR(191) CHARACTER SET ascii COLLATE ascii_bin".to_string(),
            collation: None,
            inline_check: format!(
                "CHECK ({quoted} IS NULL OR (CHAR_LENGTH({quoted}) = {len} AND \
                 REGEXP_LIKE({quoted}, {regex}, 'c')))"
            ),
        }
    }

    fn type_id_column_metadata(
        &self,
        quoted: &str,
        _stored_prefix: &str,
        _suffix_start: usize,
        total_len: usize,
        _suffix_len: usize,
        _alphabet: &str,
        regex: &str,
    ) -> ValueFormatColumnMetadata {
        let regex = mysql_grammar_string_literal(regex);
        ValueFormatColumnMetadata {
            ddl_type: "VARCHAR(191) CHARACTER SET ascii COLLATE ascii_bin".to_string(),
            collation: None,
            inline_check: format!(
                "CHECK ({quoted} IS NULL OR (CHAR_LENGTH({quoted}) = {total_len} AND \
                 REGEXP_LIKE({quoted}, {regex}, 'c')))"
            ),
        }
    }

    fn bytewise_column_metadata(
        &self,
        rendered_type: &str,
    ) -> (String, Option<ColumnCollationSnapshot>) {
        let ddl_type = format!("{} CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin", {
            // MySQL character types already carry an explicit charset+collation
            // suffix: `MysqlSchemaRenderer::column_type` pins one on every rendered
            // character spelling via `collation::mysql_collation_clause`. Replace it
            // rather than append a second one.
            rendered_type
                .split_once(" CHARACTER SET ")
                .map_or(rendered_type, |(base, _)| base)
        });
        (ddl_type, None)
    }
}
