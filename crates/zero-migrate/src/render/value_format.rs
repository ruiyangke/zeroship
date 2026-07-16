//! Dialect-specific physical metadata for validated textual value formats.
//!
//! A [`ValueFormat`] is logical schema metadata carried separately from the
//! physical [`ColType`](crate::model::ir::ColType). This module is the one seam
//! that turns that metadata into the exact column collation and inline format
//! `CHECK` consumed by the shared DDL emitters.

use crate::model::ir::{validate_type_id_prefix, ValueFormat};
use crate::render::dml::{
    mysql_grammar_string_literal, quote_ident_for_dialect, sql_string_literal,
};
use crate::schema::query::SqlDialect;

const TYPE_ID_SUFFIX_LEN: usize = 26;
const TYPE_ID_ALPHABET: &str = "0123456789abcdefghjkmnpqrstvwxyz";
const ULID_LEN: usize = 26;
const ULID_ALPHABET: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The physical column details implied by one logical value format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValueFormatColumnMetadata {
    /// Exact dialect DDL type, including the format's bytewise collation.
    pub ddl_type: String,
    /// Null-tolerant canonical spelling check, including its `CHECK` wrapper.
    pub inline_check: String,
}

/// Lower one logical value format to its dialect-specific text representation.
///
/// Prefixes are validated here as well as in the policy validator because some
/// internal tests and trusted callers exercise lowering directly. Malformed
/// hand-built IR must fail closed at either entry point.
pub(crate) fn column_metadata(
    column: &str,
    value_format: &ValueFormat,
    dialect: SqlDialect,
) -> Result<ValueFormatColumnMetadata, String> {
    match value_format {
        ValueFormat::TypeId { prefix } => type_id_column_metadata(column, prefix, dialect),
        ValueFormat::Ulid => ulid_column_metadata(column, dialect),
    }
}

fn ulid_column_metadata(
    column: &str,
    dialect: SqlDialect,
) -> Result<ValueFormatColumnMetadata, String> {
    let quoted = quote_ident_for_dialect("ULID column", column, dialect)
        .map_err(|error| error.to_string())?;
    let regex = format!("^[0-7][{ULID_ALPHABET}]{{{}}}$", ULID_LEN - 1);

    let (ddl_type, inline_check) = match dialect {
        SqlDialect::Postgres => {
            let regex = sql_string_literal(&regex);
            (
                "text COLLATE \"C\"".to_string(),
                format!(
                    "CHECK ({quoted} IS NULL OR (octet_length({quoted}) = {ULID_LEN} AND \
                     ({quoted} COLLATE \"C\") ~ {regex}))"
                ),
            )
        }
        SqlDialect::Mysql => {
            let regex = mysql_grammar_string_literal(&regex);
            (
                "VARCHAR(191) CHARACTER SET ascii COLLATE ascii_bin".to_string(),
                format!(
                    "CHECK ({quoted} IS NULL OR (CHAR_LENGTH({quoted}) = {ULID_LEN} AND \
                     REGEXP_LIKE({quoted}, {regex}, 'c')))"
                ),
            )
        }
        SqlDialect::Sqlite => (
            "TEXT COLLATE BINARY".to_string(),
            format!(
                "CHECK ({quoted} IS NULL OR (typeof({quoted}) = 'text' AND \
                 length({quoted}) = {ULID_LEN} AND \
                 length(CAST({quoted} AS BLOB)) = {ULID_LEN} AND \
                 substr({quoted}, 1, 1) GLOB '[0-7]' AND \
                 substr({quoted}, 1, {ULID_LEN}) NOT GLOB \
                 '*[^{ULID_ALPHABET}]*'))"
            ),
        ),
    };

    Ok(ValueFormatColumnMetadata {
        ddl_type,
        inline_check,
    })
}

fn type_id_column_metadata(
    column: &str,
    prefix: &str,
    dialect: SqlDialect,
) -> Result<ValueFormatColumnMetadata, String> {
    validate_type_id_prefix(prefix)?;

    let quoted = quote_ident_for_dialect("TypeID column", column, dialect)
        .map_err(|error| error.to_string())?;
    let stored_prefix = if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}_")
    };
    let suffix_start = stored_prefix.len() + 1; // SQL strings are one-indexed.
    let total_len = stored_prefix.len() + TYPE_ID_SUFFIX_LEN;
    let regex = format!(
        "^{stored_prefix}[0-7][{TYPE_ID_ALPHABET}]{{{}}}$",
        TYPE_ID_SUFFIX_LEN - 1
    );

    let (ddl_type, inline_check) = match dialect {
        SqlDialect::Postgres => {
            let regex = sql_string_literal(&regex);
            (
                "text COLLATE \"C\"".to_string(),
                format!(
                    "CHECK ({quoted} IS NULL OR (octet_length({quoted}) = {total_len} AND \
                     ({quoted} COLLATE \"C\") ~ {regex}))"
                ),
            )
        }
        SqlDialect::Mysql => {
            let regex = mysql_grammar_string_literal(&regex);
            (
                "VARCHAR(191) CHARACTER SET ascii COLLATE ascii_bin".to_string(),
                format!(
                    "CHECK ({quoted} IS NULL OR (CHAR_LENGTH({quoted}) = {total_len} AND \
                     REGEXP_LIKE({quoted}, {regex}, 'c')))"
                ),
            )
        }
        SqlDialect::Sqlite => {
            let prefix_predicate = if stored_prefix.is_empty() {
                String::new()
            } else {
                format!(
                    " AND substr({quoted}, 1, {}) = {} COLLATE BINARY",
                    stored_prefix.len(),
                    sql_string_literal(&stored_prefix)
                )
            };
            (
                "TEXT COLLATE BINARY".to_string(),
                format!(
                    "CHECK ({quoted} IS NULL OR (typeof({quoted}) = 'text' AND \
                     length({quoted}) = {total_len} AND \
                     length(CAST({quoted} AS BLOB)) = {total_len}{prefix_predicate} AND \
                     substr({quoted}, {suffix_start}, 1) GLOB '[0-7]' AND \
                     substr({quoted}, {suffix_start}, {TYPE_ID_SUFFIX_LEN}) NOT GLOB \
                     '*[^{TYPE_ID_ALPHABET}]*'))"
                ),
            )
        }
    };

    Ok(ValueFormatColumnMetadata {
        ddl_type,
        inline_check,
    })
}
