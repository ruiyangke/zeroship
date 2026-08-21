//! Neutral contract for vendor-owned value-format rendering and catalog normalization.
//!
//! Logical UUID, ULID and TypeID intent belongs to the IR. The exact storage type,
//! collation, `CHECK` spelling, and catalog deparser normalization belong to the
//! backend that emits or reads them. This module names that boundary without
//! supplying a shared vendor answer.

use crate::snapshot::{ColumnCollationSnapshot, IdDefaultSnapshot};
use zero_migrate_ir::dialect::DialectId;
use zero_migrate_ir::expr::Expr;
use zero_migrate_ir::ir::ValueFormat;

/// The physical column details implied by one logical value format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueFormatColumnMetadata {
    /// Exact backend DDL type, including the format's bytewise collation.
    pub ddl_type: String,
    /// Exact non-default catalog collation identity, when the backend exposes
    /// one independently from its DDL type spelling.
    pub collation: Option<ColumnCollationSnapshot>,
    /// Null-tolerant canonical spelling check, including its `CHECK` wrapper.
    pub inline_check: String,
}

/// The scalar meaning of a catalog cast target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiteralCastKind {
    Text,
    SignedInteger { bits: u8 },
    UnsignedInteger { bits: u8 },
    ExactNumeric,
    Real,
    Boolean,
    Uuid,
}

/// Where catalog SQL tokens are being normalized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogSqlContext {
    Literal,
    Expression,
    Check,
}

/// Vendor facts used by the engine's value-format comparison and rendering logic.
///
/// Every method is required. A backend must state each spelling and normalization
/// rule in its own crate; it cannot inherit one from a shipping vendor or from the
/// contract crate.
pub trait ValueFormatRenderer: std::fmt::Debug + Sync {
    fn dialect(&self) -> DialectId;
    fn normalize_authored_default_expr(&self, expr: &Expr) -> Option<Expr>;
    fn normalize_text_literal_snapshot(&self, snapshot: IdDefaultSnapshot) -> IdDefaultSnapshot;
    fn normalize_uuid_literal_snapshot(&self, snapshot: IdDefaultSnapshot) -> IdDefaultSnapshot;
    fn catalog_default_is_unquoted_literal(&self, expression_default: Option<bool>) -> bool;
    fn catalog_default_marker_is_authoritative(&self) -> bool;
    fn authored_storage_uses_rendered_literal(&self) -> bool;

    fn literal_cast_kind(&self, compact_target: &str) -> Option<LiteralCastKind>;
    fn is_catalog_cast_target(&self, compact_target: &str) -> bool;
    fn canonical_catalog_cast_target(&self, compact_target: &str) -> String;
    fn canonical_unattributed_catalog_cast_target(&self, compact_target: &str) -> Option<String>;
    fn catalog_literal_hex_carrier<'a>(&self, tokens: &'a [String]) -> Option<&'a str>;
    fn is_catalog_string_introducer(&self, word: &str, followed_by_quote: bool) -> bool;
    fn normalize_catalog_tokens(&self, context: CatalogSqlContext, tokens: &mut Vec<String>);
    fn normalizes_trim_both_from(&self) -> bool;
    fn canonical_catalog_function_name<'a>(&self, name: &'a str) -> &'a str;
    fn canonical_unattributed_catalog_function_name<'a>(&self, name: &'a str) -> Option<&'a str>;
    fn uuid_generator_candidates(&self, rendered: &str) -> Vec<String>;
    fn recovery_candidates(
        &self,
        literals: &[String],
        type_id_alphabet: &str,
        ulid_alphabet: &str,
    ) -> Vec<ValueFormat>;

    fn uuid_column_metadata(&self, quoted: &str) -> Option<ValueFormatColumnMetadata>;
    fn ulid_column_metadata(
        &self,
        quoted: &str,
        regex: &str,
        len: usize,
    ) -> ValueFormatColumnMetadata;
    fn type_id_column_metadata(
        &self,
        quoted: &str,
        stored_prefix: &str,
        suffix_start: usize,
        total_len: usize,
        suffix_len: usize,
        alphabet: &str,
        regex: &str,
    ) -> ValueFormatColumnMetadata;
    fn bytewise_column_metadata(
        &self,
        rendered_type: &str,
    ) -> (String, Option<ColumnCollationSnapshot>);
}
