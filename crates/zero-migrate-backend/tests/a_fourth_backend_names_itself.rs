//! A backend crate that does not own a closed dialect enum can still say who it is.
//!
//! # What this test is for
//!
//! `DmlRenderer::dialect` and `SchemaRenderer::dialect` used to return
//! a closed enum owned by `zero-migrate-ir`. A vendor crate cannot construct a
//! variant of a closed enum it does not own, so the only body that type-checked
//! in a fourth backend was
//! `todo!()`: the crate compiled and then panicked the first time anything asked
//! it who it was. That is not a registry problem — a stub fourth backend already
//! registers and is REACHED through the real registry — it is a signature
//! problem, and it is the one thing that stopped a vendor crate from lowering a
//! migration.
//!
//! Both methods return [`DialectId`] now. This file is the proof: `DuckDb` below
//! is a complete outsider. It is declared in a test binary, while the contract
//! crate owns no shipping-descriptor list at all, and the whole file contains no
//! mention of that removed closed enum — the assertion at the bottom of the
//! module enforces that by reading this source file back.
//!
//! The spelling bodies are deliberately thin. The claim under test is IDENTITY,
//! not fidelity: a fourth backend's `dialect()` has a real body, and everything
//! that asks a renderer who it is gets an honest answer instead of a panic.

use std::collections::BTreeMap;
use zero_migrate_backend::dml::DmlError;
use zero_migrate_backend::error::IrLowerError;
use zero_migrate_backend::existence_probe::ExistenceProbePolicy;
use zero_migrate_backend::fold::{
    AuthorTypeOverride, CatalogFoldPolicy, FoldCursorColumnContract, FoldDatabaseFeature,
    ReferenceTextStorage,
};
use zero_migrate_backend::renderer::DmlRenderer;
use zero_migrate_backend::schema::SchemaRenderer;
use zero_migrate_backend::snapshot::{
    ColumnCollationSnapshot, ColumnSnapshot, IdDefaultSnapshot, PartitionSnapshot,
    SequenceSnapshot, TableSnapshot, ViewSnapshot,
};
use zero_migrate_backend::step::BindValue;
use zero_migrate_backend::value_format::{
    CatalogSqlContext, LiteralCastKind, ValueFormatColumnMetadata, ValueFormatRenderer,
};
use zero_migrate_backend::vendor::VendorStatement;
use zero_migrate_ir::backend::{
    BackendDescriptor, Capability, CapabilitySet, IdentifierLimit, Limits,
};
use zero_migrate_ir::dialect::DialectId;
use zero_migrate_ir::expr::{AggFunc, CastTarget, Expr, ExtractField, ScalarFn};
use zero_migrate_ir::ir::{ColType, IrScalar, Op, TableRef, ValueFormat};
use zero_migrate_ir::precondition::PreconditionCheck;
use zero_migrate_ir::validate::{
    validate_expr, ExprDialectFeature, ExprDialectRejection, ExprDialectValidator,
    ExprDialectValidatorSet, TargetScope,
};

/// The outsider's own identity, declared at item scope in a crate that owns
/// neither the closed dialect enum nor the shipping registry. `DialectId::new` is `const`,
/// which is what makes this line possible at all.
const DUCKDB: DialectId = DialectId::new("duckdb");

/// The outsider's own descriptor — the ONE thing it declares about itself, and
/// the value both its identity and its capability answers are read off. Every
/// item on the right-hand side is `const`, so an out-of-tree crate writes this at
/// item scope exactly as it appears here.
static DUCKDB_DESCRIPTOR: BackendDescriptor = BackendDescriptor {
    id: DUCKDB,
    display_name: "DuckDB",
    capabilities: CapabilitySet::empty()
        .with(Capability::TableLevelForeignKey)
        .with(Capability::TableLevelUnique)
        .with(Capability::CreateOrReplaceView)
        .with(Capability::Sequence),
    limits: Limits {
        identifier: IdentifierLimit::Unbounded,
    },
};

#[derive(Debug)]
struct DuckDbDmlRenderer;

#[derive(Debug)]
struct DuckDbSchemaRenderer;

#[derive(Debug)]
struct DuckDbValueFormatRenderer;

#[derive(Debug)]
struct DuckDbExistenceProbePolicy;

#[derive(Debug)]
struct DuckDbCatalogFoldPolicy;

impl CatalogFoldPolicy for DuckDbCatalogFoldPolicy {
    fn allocate_implicit_relation_name(
        &self,
        default_name: &str,
        _tables: &BTreeMap<String, TableSnapshot>,
        _partitions: &BTreeMap<String, PartitionSnapshot>,
        _views: &BTreeMap<String, ViewSnapshot>,
        _sequences: &BTreeMap<String, SequenceSnapshot>,
    ) -> String {
        default_name.to_string()
    }

    fn rowid_storage_generates(&self, _stored_create_sql: Option<&str>, _data_type: &str) -> bool {
        false
    }

    fn stored_table_allows_rowid(&self, _stored_create_sql: Option<&str>) -> bool {
        false
    }

    fn stored_primary_key_allows_rowid(
        &self,
        _stored_create_sql: Option<&str>,
        _column: &str,
    ) -> bool {
        false
    }

    fn primary_key_keeps_identity(&self, target_columns: Option<&[String]>, column: &str) -> bool {
        target_columns.is_some_and(|target| target.iter().any(|candidate| candidate == column))
    }

    fn reusable_primary_index(
        &self,
        _snapshot: &TableSnapshot,
        _columns: &[String],
        _current_primary_key_name: Option<&str>,
    ) -> Option<String> {
        None
    }

    fn rename_primary_key_after_table_rename(&self, _snapshot: &mut TableSnapshot, _to: &str) {}

    fn is_native_uuid_type(&self, data_type: &str) -> bool {
        data_type.eq_ignore_ascii_case("uuid")
    }

    fn materialized_named_type_metadata(
        &self,
        _ty: &ColType,
        _default_schema: &str,
    ) -> Result<Option<(String, String)>, IrLowerError> {
        Ok(None)
    }

    fn inline_enum_check(
        &self,
        _column: &str,
        _values: &[String],
    ) -> Result<Option<String>, IrLowerError> {
        Ok(None)
    }

    fn inline_enum_type(&self, _values: &[String]) -> Option<String> {
        None
    }

    fn folds_check_constraint_identity(&self) -> bool {
        false
    }

    fn physical_type_inputs_equal(&self, _left: &ColumnSnapshot, _right: &ColumnSnapshot) -> bool {
        false
    }

    fn reference_catalog_type<'a>(&self, column: &'a ColumnSnapshot) -> &'a str {
        &column.data_type
    }

    fn canonical_reference_catalog_type(
        &self,
        data_type: &str,
        _integer_width_is_logically_proven: bool,
    ) -> String {
        data_type.trim().to_ascii_lowercase()
    }

    fn explicit_reference_text_storage(&self, _ddl_type: &str) -> Option<ReferenceTextStorage> {
        None
    }

    fn catalog_reference_text_storage(
        &self,
        _column: &ColumnSnapshot,
    ) -> Option<ReferenceTextStorage> {
        None
    }

    fn compares_reference_named_collation(&self) -> bool {
        true
    }

    fn cursor_column_contract(
        &self,
        column: &ColumnSnapshot,
    ) -> Result<FoldCursorColumnContract, String> {
        Err(format!(
            "DuckDB stub has no cursor codec for {:?}",
            column.data_type
        ))
    }

    fn wrap_default_expr(&self, _expr: &Expr, rendered: String) -> String {
        rendered
    }

    fn database_requirement_for_column(
        &self,
        _ty: &ColType,
        _is_reference: bool,
    ) -> Option<FoldDatabaseFeature> {
        None
    }

    fn database_requirement_for_value_format(
        &self,
        _value_format: &ValueFormat,
    ) -> Option<FoldDatabaseFeature> {
        None
    }

    fn database_requirement_for_expr(&self, _expr: &Expr) -> Option<FoldDatabaseFeature> {
        None
    }

    fn drop_column_precondition(&self, _table: &str, _column: &str) -> Option<PreconditionCheck> {
        None
    }

    fn restates_column_type_at_apply(&self) -> bool {
        false
    }

    fn column_type_change_precondition(
        &self,
        _table: &str,
        _column: &str,
    ) -> Option<PreconditionCheck> {
        None
    }

    fn alter_column_refusal(&self, _op: &'static str) -> Result<(), IrLowerError> {
        Ok(())
    }

    fn partition_collapse_mirror_guard(
        &self,
        _table_sql: &str,
        _key_sql: &str,
        _predicate: &str,
    ) -> Result<String, IrLowerError> {
        Err(IrLowerError::UnsupportedOp(
            "DuckDB stub does not collapse partitions",
        ))
    }

    fn supports_expression_index(&self) -> bool {
        true
    }

    fn author_type_override(&self, _ty: &ColType) -> Option<AuthorTypeOverride> {
        None
    }
}

impl ExistenceProbePolicy for DuckDbExistenceProbePolicy {
    fn unique_index_carries_constraint_identity(&self) -> bool {
        false
    }

    fn unresolved_constraint_drop_reason(&self) -> Option<&'static str> {
        None
    }

    fn normalize_constraint_definition(&self, definition: &str) -> String {
        definition.to_string()
    }

    fn truncated_identifier(&self, _authored: &str) -> Option<String> {
        None
    }
}

impl ExprDialectValidator for DuckDbDmlRenderer {
    fn validate_expr_feature(
        &self,
        feature: ExprDialectFeature<'_>,
    ) -> Result<(), ExprDialectRejection> {
        match feature {
            ExprDialectFeature::ScalarFunction(function) => match function {
                ScalarFn::Coalesce
                | ScalarFn::Nullif
                | ScalarFn::Lower
                | ScalarFn::Upper
                | ScalarFn::Trim
                | ScalarFn::Length
                | ScalarFn::Abs
                | ScalarFn::Mod
                | ScalarFn::Round
                | ScalarFn::Floor
                | ScalarFn::Ceil
                | ScalarFn::Substr
                | ScalarFn::Replace
                | ScalarFn::CurrentSetting
                | ScalarFn::CurrentUser => Ok(()),
            },
            ExprDialectFeature::Aggregate(function) => match function {
                AggFunc::Count
                | AggFunc::Sum
                | AggFunc::Avg
                | AggFunc::Min
                | AggFunc::Max
                | AggFunc::StringAgg
                | AggFunc::ArrayAgg
                | AggFunc::BoolAnd
                | AggFunc::BoolOr => Ok(()),
            },
            ExprDialectFeature::ConcatWs { .. }
            | ExprDialectFeature::SplitPart { .. }
            | ExprDialectFeature::UuidV7Generation
            | ExprDialectFeature::RegexMatch
            | ExprDialectFeature::PgColumnSize
            | ExprDialectFeature::PgExtract
            | ExprDialectFeature::PgInterval => Ok(()),
        }
    }
}

struct DuckDbValidators;

impl ExprDialectValidatorSet for DuckDbValidators {
    fn get(&self, dialect: &DialectId) -> Option<&dyn ExprDialectValidator> {
        (dialect == &DUCKDB).then_some(&DuckDbDmlRenderer)
    }
}

impl DmlRenderer for DuckDbDmlRenderer {
    fn dialect(&self) -> DialectId {
        DUCKDB
    }

    fn expr_validator(&self) -> &dyn ExprDialectValidator {
        self
    }

    fn descriptor(&self) -> &'static BackendDescriptor {
        &DUCKDB_DESCRIPTOR
    }

    fn preview_session_prologue(&self) -> &'static [&'static str] {
        &[]
    }

    fn preview_session_epilogue(&self) -> &'static [&'static str] {
        &[]
    }

    fn guarded_ddl_preview_limitation(&self) -> Option<&'static str> {
        None
    }

    fn alter_ops_require_live_schema(&self) -> bool {
        false
    }

    fn op_support_refusal(&self, _op: &Op, _variant: &str) -> Option<&'static str> {
        Some("DuckDB backend test refusal")
    }

    fn quote_ident(&self, ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }

    fn qualify_table(&self, project_schema: &str, table: &str) -> Result<String, DmlError> {
        Ok(format!(
            "{}.{}",
            self.quote_ident(project_schema),
            self.quote_ident(table)
        ))
    }

    fn cast_target(&self, target: CastTarget) -> &'static str {
        match target {
            CastTarget::Text => "VARCHAR",
            CastTarget::Int => "BIGINT",
            CastTarget::Real => "DOUBLE",
            CastTarget::Boolean => "BOOLEAN",
            CastTarget::Bytes => "BLOB",
            CastTarget::Uuid => "UUID",
        }
    }

    fn placeholder(&self, n: usize) -> String {
        format!("${n}")
    }

    fn inline_string_literal(&self, s: &str) -> String {
        format!("'{}'", s.replace('\'', "''"))
    }

    fn inline_decimal_literal(&self, d: &str) -> String {
        d.to_string()
    }

    fn inline_bytes_literal(&self, bytes: &[u8]) -> String {
        let mut out = String::from("'");
        for b in bytes {
            out.push_str(&format!("\\x{b:02X}"));
        }
        out.push_str("'::BLOB");
        out
    }

    fn bind_bytes(&self, bytes: &[u8], push: &mut dyn FnMut(BindValue) -> String) -> String {
        push(BindValue::Bytes(bytes.to_vec()))
    }

    fn render_in_list(
        &self,
        expr: &str,
        elems: &[IrScalar],
        negated: bool,
        joiner: &str,
    ) -> Result<String, DmlError> {
        let rendered: Vec<String> = elems.iter().map(|e| format!("{e:?}")).collect();
        let op = if negated { "NOT IN" } else { "IN" };
        Ok(format!("{expr} {op} ({})", rendered.join(joiner)))
    }

    fn render_regex_match(&self, expr: &str, pattern: &str) -> Result<String, DmlError> {
        Ok(format!(
            "regexp_matches({expr}, {})",
            self.inline_string_literal(pattern)
        ))
    }

    fn render_extract(&self, field: ExtractField, expr: &str) -> String {
        format!("date_part('{field:?}', {expr})")
    }

    fn render_concat(&self, l: &str, r: &str) -> String {
        format!("({l} || {r})")
    }

    fn render_distinct_from(&self, l: &str, r: &str) -> String {
        format!("({l} IS DISTINCT FROM {r})")
    }

    fn render_scalar_fn_override(&self, _f: ScalarFn, _args: &[String]) -> Option<String> {
        None
    }

    fn render_is_true(&self, operand: &str) -> String {
        format!("({operand} IS TRUE)")
    }

    fn render_is_false(&self, operand: &str) -> String {
        format!("({operand} IS FALSE)")
    }

    fn render_concat_ws(&self, rendered: &[String]) -> String {
        format!("concat_ws({})", rendered.join(", "))
    }

    fn render_split_part(&self, col_sql: &str, delim: &str, n: i64) -> Result<String, DmlError> {
        Ok(format!(
            "str_split({col_sql}, {})[{n}]",
            self.inline_string_literal(delim)
        ))
    }

    fn synth_now(&self) -> String {
        "now()".to_string()
    }

    fn uuid_v4(&self) -> String {
        "uuid()".to_string()
    }

    fn uuid_v7(&self) -> Result<String, DmlError> {
        Err(DmlError::UnrenderableExpr(
            "duckdb has no uuidv7 generator".to_string(),
        ))
    }

    fn view_create_prefix(
        &self,
        materialized: bool,
        replace: bool,
    ) -> Result<String, IrLowerError> {
        if materialized {
            return Err(IrLowerError::DmlAssemble(DmlError::UnrenderableExpr(
                "duckdb has no materialized views".to_string(),
            )));
        }
        Ok(if replace {
            "CREATE OR REPLACE VIEW".to_string()
        } else {
            "CREATE VIEW".to_string()
        })
    }

    fn view_replace_prelude(&self, _qname: &str, _replace: bool) -> Vec<String> {
        Vec::new()
    }

    fn view_object_name(&self, name: &str, eff_schema: &str) -> Result<String, IrLowerError> {
        Ok(format!(
            "{}.{}",
            self.quote_ident(eff_schema),
            self.quote_ident(name)
        ))
    }

    fn render_table_ref(&self, table: &TableRef, eff_schema: &str) -> Result<String, IrLowerError> {
        let schema = table.schema.as_deref().unwrap_or(eff_schema);
        Ok(format!(
            "{}.{}",
            self.quote_ident(schema),
            self.quote_ident(&table.name)
        ))
    }

    fn render_trigger_op(
        &self,
        _op: &Op,
        _eff_schema: &str,
    ) -> Result<Vec<VendorStatement>, IrLowerError> {
        Err(IrLowerError::DmlAssemble(DmlError::UnrenderableExpr(
            "duckdb has no triggers".to_string(),
        )))
    }

    /// The newcomer WRITES ITS OWN REFUSAL, and that is the point of the method
    /// having no default body.
    ///
    /// `render_vendor_op` covers sixteen op kinds that are PostgreSQL-only. A
    /// default body would have let this backend inherit somebody else's answer
    /// silently; a required method makes the omission `E0046` in the newcomer's
    /// own crate, so the only way to compile is to state a position. DuckDb has no
    /// vendor-op surface, so it refuses, naming ITSELF — exactly as the shipping
    /// SQLite and MySQL renderers do.
    fn render_vendor_op(
        &self,
        _op: &zero_migrate_ir::ir::Op,
        _eff_schema: &str,
    ) -> Result<
        Vec<zero_migrate_backend::vendor::VendorStatement>,
        zero_migrate_backend::vendor::VendorError,
    > {
        Err(zero_migrate_backend::vendor::VendorError::VendorOpsUnsupported(DUCKDB))
    }
}

impl SchemaRenderer for DuckDbSchemaRenderer {
    fn dialect(&self) -> DialectId {
        DUCKDB
    }

    fn quote_ident(&self, ident: &str) -> String {
        DuckDbDmlRenderer.quote_ident(ident)
    }

    fn ident_quote_char(&self) -> char {
        '"'
    }

    /// DuckDB deliberately offers no catalog-stored DDL parser in this stub.
    fn stored_ddl(&self) -> Option<&'static dyn zero_migrate_backend::stored_ddl::StoredDdl> {
        None
    }

    fn foreign_key_target(&self, app_id: &str, target: &str) -> String {
        format!("\"{app_id}\".\"{target}\"")
    }

    fn canonical_fk_target(&self, schema: &str, target: &str) -> String {
        format!("{schema}.{target}")
    }

    fn column_type(&self, column: &ColumnSnapshot, _inline_pk: bool) -> String {
        match column.data_type.as_str() {
            "double precision" => "DOUBLE".to_string(),
            "boolean" => "BOOLEAN".to_string(),
            _ => "VARCHAR".to_string(),
        }
    }

    fn snapshot_data_type(&self, column: &ColumnSnapshot) -> String {
        column.data_type.clone()
    }

    fn finalize_column_snapshot(&self, _column: &mut ColumnSnapshot) {}

    fn project_derived_ann_index(
        &self,
        _index: &mut zero_migrate_backend::snapshot::IndexSnapshot,
    ) -> bool {
        false
    }

    fn validate_key_storage(
        &self,
        _desired: &zero_migrate_backend::snapshot::SchemaSnapshot,
        _live: &zero_migrate_backend::snapshot::SchemaSnapshot,
    ) -> Result<(), String> {
        Ok(())
    }

    fn existing_column_change_strategy(
        &self,
    ) -> zero_migrate_backend::schema::ExistingColumnChangeStrategy {
        zero_migrate_backend::schema::ExistingColumnChangeStrategy::Refuse
    }

    fn column_rename_strategy(&self) -> zero_migrate_backend::schema::ColumnRenameStrategy {
        zero_migrate_backend::schema::ColumnRenameStrategy::Refuse(
            "DuckDB rename lowering is outside this stub",
        )
    }

    fn supports_forward_inline_foreign_key(&self) -> bool {
        false
    }

    fn identity_column_type_allowed(&self, _data_type: &str) -> bool {
        true
    }

    fn canonical_type(&self, raw: &str) -> String {
        raw.to_string()
    }

    fn create_table_target(&self, app_id: &str, collection: &str, unqualified: bool) -> String {
        if unqualified {
            self.quote_ident(collection)
        } else {
            format!(
                "{}.{}",
                self.quote_ident(app_id),
                self.quote_ident(collection)
            )
        }
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
        let table = self.create_table_target(app_id, collection, unqualified);
        let columns = columns
            .iter()
            .map(|column| self.quote_ident(column))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "CREATE {unique_clause}INDEX {} ON {table} ({columns})",
            self.quote_ident(index_name)
        )
    }

    fn schema_string_literal(&self, value: &str) -> String {
        format!("'{}'", value.replace('\'', "''"))
    }

    fn schema_grammar_string_literal(&self, value: &str) -> String {
        format!("'{}'", value.replace('\'', "''"))
    }

    fn empty_json_expr(&self, object: bool) -> &'static str {
        if object {
            "'{}'"
        } else {
            "'[]'"
        }
    }

    fn empty_text_array_expr(&self) -> Option<&'static str> {
        Some("[]")
    }

    fn json_value_default_expr(&self, json: &str) -> String {
        format!("'{}'", json.replace('\'', "''"))
    }

    fn injected_column_ident(&self, name: &str, canonical_bare: bool) -> String {
        if canonical_bare {
            name.to_string()
        } else {
            self.quote_ident(name)
        }
    }

    fn canonical_fk_action(&self, action: &'static str) -> &'static str {
        action
    }

    fn suppress_string_enum_check(&self, _def: &serde_json::Value) -> bool {
        false
    }

    /// This stub deliberately supports no collation spelling. The required method
    /// makes that refusal-to-transform explicit in the outsider's own crate.
    fn pin_collation(&self, rendered: &str, _case_sensitive: Option<bool>) -> String {
        rendered.to_string()
    }

    /// With no pin of its own, DuckDB has no suffix of its own to strip.
    fn strip_collation<'a>(&self, rendered: &'a str) -> &'a str {
        rendered
    }

    fn json_object_default(&self) -> String {
        "'{}'".to_string()
    }

    fn json_array_default(&self) -> String {
        "'[]'".to_string()
    }

    fn current_timestamp_expr(&self) -> &'static str {
        "now()"
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

impl ValueFormatRenderer for DuckDbValueFormatRenderer {
    fn dialect(&self) -> DialectId {
        DUCKDB
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

    fn literal_cast_kind(&self, _compact_target: &str) -> Option<LiteralCastKind> {
        None
    }

    fn is_catalog_cast_target(&self, _compact_target: &str) -> bool {
        false
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
        _literals: &[String],
        _type_id_alphabet: &str,
        _ulid_alphabet: &str,
    ) -> Vec<ValueFormat> {
        Vec::new()
    }

    fn uuid_column_metadata(&self, _quoted: &str) -> Option<ValueFormatColumnMetadata> {
        None
    }

    fn ulid_column_metadata(
        &self,
        _quoted: &str,
        _regex: &str,
        _len: usize,
    ) -> ValueFormatColumnMetadata {
        ValueFormatColumnMetadata {
            ddl_type: "VARCHAR".to_string(),
            collation: None,
            inline_check: String::new(),
        }
    }

    fn type_id_column_metadata(
        &self,
        _quoted: &str,
        _stored_prefix: &str,
        _suffix_start: usize,
        _total_len: usize,
        _suffix_len: usize,
        _alphabet: &str,
        _regex: &str,
    ) -> ValueFormatColumnMetadata {
        ValueFormatColumnMetadata {
            ddl_type: "VARCHAR".to_string(),
            collation: None,
            inline_check: String::new(),
        }
    }

    fn bytewise_column_metadata(
        &self,
        rendered_type: &str,
    ) -> (String, Option<ColumnCollationSnapshot>) {
        (rendered_type.to_string(), None)
    }
}

/// The claim, stated as a value: an outsider's `dialect()` returns its OWN id.
///
/// Before the signature change the only body that type-checked here was
/// `todo!()`, so this call panicked. It returns a value now, and the value is
/// the one the outsider declared — not one of the three the core enum knows.
#[test]
fn a_fourth_backend_answers_dialect_with_its_own_id() {
    let dml: &dyn DmlRenderer = &DuckDbDmlRenderer;
    let schema: &dyn SchemaRenderer = &DuckDbSchemaRenderer;
    let value_format: &dyn ValueFormatRenderer = &DuckDbValueFormatRenderer;
    let existence_probe: &dyn ExistenceProbePolicy = &DuckDbExistenceProbePolicy;
    let catalog_fold: &dyn CatalogFoldPolicy = &DuckDbCatalogFoldPolicy;

    assert_eq!(dml.dialect(), DUCKDB);
    assert_eq!(schema.dialect(), DUCKDB);
    assert_eq!(dml.dialect().as_str(), "duckdb");
    assert!(dml.dialect().is_well_formed());
    assert_eq!(
        schema.pin_collation("VARCHAR", Some(false)),
        "VARCHAR",
        "the outsider writes its own pass-through instead of inheriting one"
    );
    assert_eq!(schema.strip_collation("VARCHAR"), "VARCHAR");
    assert_eq!(
        value_format.bytewise_column_metadata("VARCHAR"),
        ("VARCHAR".to_string(), None),
        "the outsider writes its own value-format refusal/pass-through"
    );
    assert!(!existence_probe.unique_index_carries_constraint_identity());
    assert_eq!(existence_probe.unresolved_constraint_drop_reason(), None);
    assert_eq!(
        existence_probe.normalize_constraint_definition("CHECK (x > 0)"),
        "CHECK (x > 0)"
    );
    assert_eq!(existence_probe.truncated_identifier("duckdb_name"), None);
    assert_eq!(
        catalog_fold.allocate_implicit_relation_name(
            "items_pkey",
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        ),
        "items_pkey",
        "the outsider writes its own catalog-fold policy instead of inheriting one"
    );

    // Capabilities come off the outsider's OWN descriptor, so the answers are the
    // ones it declared — not the "no to everything" a core-owned id->capability
    // table would have to give a name it does not recognise.
    assert!(dml.supports(Capability::CreateOrReplaceView));
    assert!(!dml.supports(Capability::PostgresVendorPrimitives));
    assert!(!dml.supports(Capability::MaterializedView));

    let refusal = IrLowerError::ViewUnsupported {
        kind: "materializedView",
        dialect: dml.dialect(),
    };
    assert_eq!(
        refusal.to_string(),
        "IrAuthor::lower of view facet \"materializedView\" is unsupported on duckdb",
        "provenance errors must let an outsider name itself with its open id"
    );

    // And the leaf contract has no shipping list to edit: declaring this row is
    // sufficient for an outsider to answer its own identity and capabilities.
}

#[test]
fn a_fourth_backend_validates_expressions_under_its_own_id() {
    let expr = Expr::FnCall {
        r#fn: ScalarFn::CurrentUser,
        args: Vec::new(),
    };
    validate_expr(
        &expr,
        &DUCKDB,
        &DuckDbValidators,
        &TargetScope::structural_only("items"),
        0,
    )
    .expect("the outsider's own exhaustive validator must answer for its id");
}

/// The stub is only a proof if it never touches the closed enum.
///
/// A test that demonstrated a fourth backend by naming the removed closed enum somewhere
/// would be demonstrating the opposite thing. This reads its own source back and
/// refuses the mention, so the proof cannot rot into one by a later edit.
///
/// The needle is assembled from two halves on purpose. Spelled whole, the
/// detector's own line is the first thing it finds and the test fails on itself —
/// which it did, on the first run. A scanner that matches its own source is the
/// standard failure of this shape, and the fix has to be in the LITERAL rather
/// than in an exclusion rule, because any "skip line N" carve-out would also skip
/// a real offender that later lands on that line.
#[test]
fn the_stub_never_names_the_closed_enum() {
    let source = include_str!("a_fourth_backend_names_itself.rs");
    let needle = concat!("Sql", "Dialect");
    let offenders: Vec<(usize, &str)> = source
        .lines()
        .enumerate()
        .filter(|(_, line)| line.contains(needle))
        .filter(|(_, line)| !line.trim_start().starts_with("//"))
        .map(|(i, line)| (i + 1, line.trim()))
        .collect();
    assert!(
        offenders.is_empty(),
        "the fourth-backend stub must name no closed dialect enum, but it does: {offenders:#?}"
    );
}
