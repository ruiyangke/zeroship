//! A backend crate that does not own a closed dialect enum can still say who it is.
//!
//! # What this test is for
//!
//! `DmlRenderer::dialect` and `SchemaRenderer::dialect` used to return
//! a closed enum owned by `zero-migrate-ir`. A vendor crate cannot construct a
//! variant of a closed enum it does not own, so the only body that type-checked
//! in a fourth backend was
//! `todo!()`: the crate compiled and then panicked the first time anything asked
//! it who it was. That is not a registry problem - a stub fourth backend already
//! registers and is REACHED through the real registry - it is a signature
//! problem, and it is the one thing that stopped a vendor crate from lowering a
//! migration.
//!
//! Both methods return [`DialectId`] now. This file is the proof: `DuckDb` below
//! is a complete outsider. It is declared in a test binary, while the contract
//! crate owns no shipping-descriptor list at all, and the whole file contains no
//! mention of that removed closed enum - the assertion at the bottom of the
//! module enforces that by reading this source file back.
//!
//! The spelling bodies are deliberately thin. The claim under test is IDENTITY,
//! not fidelity: a fourth backend's `dialect()` has a real body, and everything
//! that asks a renderer who it is gets an honest answer instead of a panic.

use std::collections::BTreeMap;
use zeroship_migrate_backend::dml::{
    BindCtx, DmlError, LimitedDeleteRenderRequest, OnConflict, OnConflictRenderRequest,
};
use zeroship_migrate_backend::error::IrLowerError;
use zeroship_migrate_backend::existence_probe::ExistenceProbePolicy;
use zeroship_migrate_backend::fold::{
    AuthorTypeOverride, CatalogFoldPolicy, CatalogFoldRefusal, FoldCursorColumnContract,
    FoldDatabaseFeature, ReferenceTextStorage, SnapshotProvenanceStrength,
};
use zeroship_migrate_backend::renderer::{DmlRenderer, FeatureSupportKey, MaterializedNamedTypeOp};
use zeroship_migrate_backend::schema::{
    AddColumnIfNotExistsRequest, CreateIndexIfNotExistsRequest, SchemaRenderer,
};
use zeroship_migrate_backend::snapshot::{
    ColumnCollationSnapshot, ColumnSnapshot, IdDefaultSnapshot, PartitionSnapshot,
    SequenceSnapshot, TableSnapshot, ViewSnapshot,
};
use zeroship_migrate_backend::step::BindValue;
use zeroship_migrate_backend::validation::{Disposition, ValidationPolicy, ValidationRefusal};
use zeroship_migrate_backend::value_format::{
    CatalogSqlContext, LiteralCastKind, ValueFormatColumnMetadata, ValueFormatRenderer,
};
use zeroship_migrate_backend::vendor::VendorStatement;
use zeroship_migrate_ir::backend::{
    BackendDescriptor, Capability, CapabilitySet, IdentifierLimit, Limits,
};
use zeroship_migrate_ir::dialect::DialectId;
use zeroship_migrate_ir::expr::{AggFunc, CastTarget, Duration, Expr, ExtractField, ScalarFn};
use zeroship_migrate_ir::ir::{ColType, IrScalar, IrValue, Op, TableRef, ValueFormat};
use zeroship_migrate_ir::precondition::PreconditionCheck;
use zeroship_migrate_ir::validate::{
    validate_expr, ExprDialectFeature, ExprDialectRejection, ExprDialectValidator,
    ExprDialectValidatorSet, TargetScope,
};

/// The outsider's own identity, declared at item scope in a crate that owns
/// neither the closed dialect enum nor the shipping registry. `DialectId::new` is `const`,
/// which is what makes this line possible at all.
const DUCKDB: DialectId = DialectId::new("duckdb");

/// The outsider's own descriptor - the ONE thing it declares about itself, and
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
        reserved_identifier_prefixes: &[],
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

#[derive(Debug)]
struct DuckDbValidationPolicy;

impl ValidationPolicy for DuckDbValidationPolicy {
    fn op_disposition(&self, _kind: &str, _variant: &str) -> Disposition {
        // This stub deliberately supports no engine operation shape. Its own
        // required answer is explicit and fail-closed for every current or
        // future token; it never borrows a shipping backend's table.
        Disposition::Unsupported
    }

    fn canonical_identifier(&self, identifier: &str) -> String {
        identifier.to_ascii_lowercase()
    }

    fn catalog_proves_uuid_format(
        &self,
        native_uuid: bool,
        catalog_uuid_format_check: bool,
    ) -> bool {
        native_uuid || catalog_uuid_format_check
    }

    fn lowered_reference_storage(&self, ty: &ColType) -> String {
        format!("{ty:?}")
    }

    fn tracks_constraint_names(&self) -> bool {
        true
    }

    fn tracks_relation_type_namespace(&self) -> bool {
        false
    }

    fn vendor_capability_refusal(
        &self,
        capability: zeroship_migrate_ir::capability::VendorCapability,
    ) -> Option<ValidationRefusal> {
        Some(ValidationRefusal {
            reason: format!(
                "DuckDB stub refuses vendor capability {:?}",
                capability.as_token()
            ),
            suggested_fix: "use a DuckDB-supported operation".to_string(),
        })
    }

    fn deferrable_foreign_key_refusal(&self) -> ValidationRefusal {
        ValidationRefusal {
            reason: "DuckDB stub does not validate deferrable foreign keys".to_string(),
            suggested_fix: "omit the deferrable options".to_string(),
        }
    }

    fn inline_trigger_body_refusal(&self) -> ValidationRefusal {
        ValidationRefusal {
            reason: "DuckDB stub does not validate inline trigger bodies".to_string(),
            suggested_fix: "omit the trigger".to_string(),
        }
    }

    fn trigger_event_count_refusal(&self, event_count: usize) -> Option<ValidationRefusal> {
        (event_count > 1).then(|| ValidationRefusal {
            reason: "DuckDB stub accepts one trigger event".to_string(),
            suggested_fix: "split the trigger".to_string(),
        })
    }

    fn sequence_default_refusal(&self, position: &str) -> ValidationRefusal {
        ValidationRefusal {
            reason: format!("DuckDB stub refuses a sequence default at {position}"),
            suggested_fix: "remove the sequence default".to_string(),
        }
    }

    fn virtual_generated_column_refusal(&self, column: &str) -> ValidationRefusal {
        ValidationRefusal {
            reason: format!("DuckDB stub refuses virtual generated column {column:?}"),
            suggested_fix: "use a stored generated column".to_string(),
        }
    }

    fn identity_placement_refusal(
        &self,
        column: &str,
        _always: bool,
        _primary_key_columns: Option<&[String]>,
        _is_add_column: bool,
    ) -> Option<ValidationRefusal> {
        Some(ValidationRefusal {
            reason: format!("DuckDB stub refuses identity column {column:?}"),
            suggested_fix: "remove identity".to_string(),
        })
    }

    /// The outsider states its own raw-view-body posture. It owns no parser, and
    /// unlike the two shipping descriptor backends it declines rather than trusts -
    /// which is the point of the method being required: BOTH answers are writable,
    /// and neither is inheritable. A fourth backend that supplied nothing would fail
    /// to compile here, in its own file, naming this method.
    fn raw_view_body_refusal(
        &self,
        _sql: &str,
        _scope: Option<&zeroship_migrate_ir::policy::SchemaScope>,
    ) -> Option<ValidationRefusal> {
        Some(ValidationRefusal {
            reason: "DuckDB stub has no parser and refuses raw view bodies".to_string(),
            suggested_fix: "use the structured SelectAst view builder".to_string(),
        })
    }
}

impl CatalogFoldPolicy for DuckDbCatalogFoldPolicy {
    fn snapshot_provenance_strength(
        &self,
        _table: &TableSnapshot,
    ) -> Option<SnapshotProvenanceStrength> {
        None
    }

    fn implicit_primary_key_name(&self, table: &str) -> String {
        format!("duck_pk_{table}")
    }

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

    fn canonical_rename_type_spelling(&self, ty: &str) -> String {
        ty.trim().to_ascii_lowercase()
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

    fn refusal_message(&self, refusal: CatalogFoldRefusal) -> &'static str {
        match refusal {
            CatalogFoldRefusal::AlterPrimaryKeyRowidGeneration => {
                "DuckDB refuses primary-key rowid generation in its catalog fold"
            }
            CatalogFoldRefusal::AddColumnIdentity => {
                "DuckDB refuses added-column identity in its catalog fold"
            }
            CatalogFoldRefusal::CreateTableCheckConstraint => {
                "DuckDB refuses a create-table CHECK in its catalog fold"
            }
            CatalogFoldRefusal::CreateTableUniqueConstraint => {
                "DuckDB refuses a create-table UNIQUE in its catalog fold"
            }
            CatalogFoldRefusal::CreateTableExclusionConstraint => {
                "DuckDB refuses a create-table exclusion constraint in its catalog fold"
            }
            CatalogFoldRefusal::CreateTableNonBtreeIndex => {
                "DuckDB refuses a non-btree create-table index in its catalog fold"
            }
            CatalogFoldRefusal::AddCheckConstraint => {
                "DuckDB refuses an added CHECK in its catalog fold"
            }
            CatalogFoldRefusal::AddExclusionConstraint => {
                "DuckDB refuses an added exclusion constraint in its catalog fold"
            }
        }
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
            | ExprDialectFeature::StorageSize
            | ExprDialectFeature::Extract(_)
            | ExprDialectFeature::Interval => Ok(()),
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

    fn supports(&self, cap: zeroship_migrate_ir::backend::Capability) -> bool {
        self.descriptor().capabilities.contains(cap)
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

    fn feature_support_refusal(&self, feature: FeatureSupportKey) -> Option<&'static str> {
        match feature {
            FeatureSupportKey::PartialIndex
            | FeatureSupportKey::IndexInclude
            | FeatureSupportKey::IndexStorageParams
            | FeatureSupportKey::IndexOnly
            | FeatureSupportKey::IndexNullsNotDistinct
            | FeatureSupportKey::IndexOpclass
            | FeatureSupportKey::IndexCollation
            | FeatureSupportKey::ExpressionIndex
            | FeatureSupportKey::NonBtreeIndexMethod
            | FeatureSupportKey::TableLevelForeignKey
            | FeatureSupportKey::TableLevelUnique
            | FeatureSupportKey::TableLevelCheckExpression
            | FeatureSupportKey::CompositeForeignKey
            | FeatureSupportKey::ForeignKeyNoLocalColumn
            | FeatureSupportKey::NonIdForeignKey
            | FeatureSupportKey::SequenceDefault
            | FeatureSupportKey::ConstraintNotValid
            | FeatureSupportKey::ExclusionConstraint
            | FeatureSupportKey::AlterColumnUsing
            | FeatureSupportKey::RenameColumnGuard
            | FeatureSupportKey::ExistenceGuardProbe
            | FeatureSupportKey::InsertOnConflict
            | FeatureSupportKey::MaterializedView
            | FeatureSupportKey::CreateOrReplaceMaterializedView
            | FeatureSupportKey::TriggerMultipleEvents
            | FeatureSupportKey::TriggerExecuteFunction
            | FeatureSupportKey::TriggerTruncateEvent
            | FeatureSupportKey::TriggerInsteadOfTiming
            | FeatureSupportKey::TriggerStatementForEach
            | FeatureSupportKey::TriggerBody
            | FeatureSupportKey::TriggerWhen
            | FeatureSupportKey::TriggerRaiseIgnore
            | FeatureSupportKey::Comment
            | FeatureSupportKey::Sequence
            | FeatureSupportKey::RawViewBody
            | FeatureSupportKey::RawSql
            | FeatureSupportKey::PartitionDdl => Some("DuckDB backend test feature refusal"),
        }
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

    fn validate_assignment_semantics(
        &self,
        _op: &'static str,
        _table: &str,
        _set: &BTreeMap<String, IrValue>,
    ) -> Result<(), DmlError> {
        // This stub states its own simultaneous-assignment policy instead of
        // inheriting one from a shipping backend.
        Ok(())
    }

    fn render_on_conflict(
        &self,
        _request: OnConflictRenderRequest<'_>,
        _ctx: &mut BindCtx<'_>,
    ) -> Result<String, DmlError> {
        Err(DmlError::UnrenderableExpr(
            "DuckDB backend test does not implement structured onConflict".to_string(),
        ))
    }

    fn render_limited_delete(
        &self,
        _request: LimitedDeleteRenderRequest<'_>,
        _ctx: &mut BindCtx<'_>,
    ) -> Result<String, DmlError> {
        Err(DmlError::UnrenderableExpr(
            "DuckDB backend test does not implement row-limited DELETE".to_string(),
        ))
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

    fn render_storage_size(&self, _expr: &str) -> Result<String, DmlError> {
        Err(DmlError::UnrenderableExpr(
            "DuckDB exposes no per-value stored-size function".to_string(),
        ))
    }

    fn render_interval(&self, _duration: &Duration) -> Result<String, DmlError> {
        Err(DmlError::UnrenderableExpr(
            "this fourth backend declines interval literals".to_string(),
        ))
    }

    fn render_extract(&self, field: ExtractField, expr: &str) -> Result<String, DmlError> {
        Ok(format!("date_part('{field:?}', {expr})"))
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

    fn render_sequence_op(
        &self,
        _op: &Op,
        _eff_schema: &str,
    ) -> Result<VendorStatement, IrLowerError> {
        Err(IrLowerError::SequenceUnsupported {
            kind: "sequence",
            dialect: DUCKDB,
        })
    }

    /// The outsider must write its own answer for materialized named-type DDL;
    /// this no-capability stub refuses rather than inheriting a default.
    fn render_materialized_named_type_op(
        &self,
        _op: MaterializedNamedTypeOp<'_>,
    ) -> Result<VendorStatement, IrLowerError> {
        Err(IrLowerError::UnsupportedOp(
            "DuckDB backend test does not materialize named types",
        ))
    }

    fn render_comment_op(
        &self,
        _op: &Op,
        _eff_schema: &str,
    ) -> Result<VendorStatement, IrLowerError> {
        Err(IrLowerError::UnsupportedOp(
            "validated COMMENT ON unsupported dialect reached lower",
        ))
    }

    /// The newcomer WRITES ITS OWN REFUSAL, and that is the point of the method
    /// having no default body.
    ///
    /// `render_vendor_op` covers sixteen op kinds that are PostgreSQL-only. A
    /// default body would have let this backend inherit somebody else's answer
    /// silently; a required method makes the omission `E0046` in the newcomer's
    /// own crate, so the only way to compile is to state a position. DuckDb has no
    /// vendor-op surface, so it refuses, naming ITSELF - exactly as the shipping
    /// SQLite and MySQL renderers do.
    fn render_vendor_op(
        &self,
        _op: &zeroship_migrate_ir::ir::Op,
        _eff_schema: &str,
    ) -> Result<
        Vec<zeroship_migrate_backend::vendor::VendorStatement>,
        zeroship_migrate_backend::vendor::VendorError,
    > {
        Err(zeroship_migrate_backend::vendor::VendorError::VendorOpsUnsupported(DUCKDB))
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
    fn stored_ddl(&self) -> Option<&'static dyn zeroship_migrate_backend::stored_ddl::StoredDdl> {
        None
    }

    fn table_rebuild_policy(
        &self,
    ) -> Option<&'static dyn zeroship_migrate_backend::table_rebuild::TableRebuildPolicy> {
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
        _index: &mut zeroship_migrate_backend::snapshot::IndexSnapshot,
    ) -> bool {
        false
    }

    fn validate_key_storage(
        &self,
        _desired: &zeroship_migrate_backend::snapshot::SchemaSnapshot,
        _live: &zeroship_migrate_backend::snapshot::SchemaSnapshot,
    ) -> Result<(), String> {
        Ok(())
    }

    fn unprefixed_key_storage_refusal(
        &self,
        _position: &str,
        _table: &str,
        _column: &str,
        _evidence: zeroship_migrate_backend::schema::KeyStorageEvidence<'_>,
    ) -> Option<zeroship_migrate_backend::schema::StorageValidationRefusal> {
        None
    }

    fn literal_default_storage_refusal(
        &self,
        _column: &str,
        _rendered_type: &str,
        _rendered_default: &str,
    ) -> Option<zeroship_migrate_backend::schema::StorageValidationRefusal> {
        None
    }

    fn dual_write_trigger(
        &self,
        _spec: &zeroship_migrate_backend::schema::DualWriteTriggerSpec<'_>,
    ) -> Option<zeroship_migrate_backend::schema::DualWriteTriggerSql> {
        None
    }

    fn existing_column_change_strategy(
        &self,
    ) -> zeroship_migrate_backend::schema::ExistingColumnChangeStrategy {
        zeroship_migrate_backend::schema::ExistingColumnChangeStrategy::Refuse
    }

    fn column_rename_strategy(&self) -> zeroship_migrate_backend::schema::ColumnRenameStrategy {
        zeroship_migrate_backend::schema::ColumnRenameStrategy::Refuse(
            "DuckDB rename lowering is outside this stub",
        )
    }

    fn supports_forward_inline_foreign_key(&self) -> bool {
        false
    }

    fn identity_column_type_allowed(&self, _data_type: &str) -> bool {
        true
    }

    fn identity_column_type_confinement(&self) -> &'static str {
        "any type a column of this target may have"
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

    fn add_foreign_key_statement(
        &self,
        _schema: &str,
        _table: &str,
        _clause: &str,
    ) -> Result<String, &'static str> {
        Err("DuckDB test backend does not implement additive foreign keys")
    }

    fn drop_foreign_key_if_exists_statement(
        &self,
        _schema: &str,
        _table: &str,
        _name: &str,
    ) -> Result<String, &'static str> {
        Err("DuckDB test backend does not implement idempotent foreign-key drops")
    }

    fn add_column_if_not_exists_statements(
        &self,
        _request: AddColumnIfNotExistsRequest<'_>,
    ) -> Result<Vec<String>, &'static str> {
        Err("DuckDB test backend does not implement idempotent additive columns")
    }

    fn create_index_if_not_exists_statement(
        &self,
        _request: CreateIndexIfNotExistsRequest<'_>,
    ) -> Result<String, &'static str> {
        Err("DuckDB test backend does not implement idempotent additive indexes")
    }

    /// REFUSED: this fourth backend has no exclusion constraint.
    ///
    /// The compiler asked for this, which is the point of the method being required
    /// with no default. A default body would have handed this backend PostgreSQL's
    /// `EXCLUDE USING gist (...)` for free, and the fixture would have compiled while
    /// claiming a constraint kind it cannot enforce.
    fn exclusion_constraint_body(
        &self,
        _req: &zeroship_migrate_backend::ddl::ExclusionConstraintRequest<'_>,
    ) -> Option<String> {
        None
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
/// the one the outsider declared - not one of the three the core enum knows.
#[test]
fn a_fourth_backend_answers_dialect_with_its_own_id() {
    let dml: &dyn DmlRenderer = &DuckDbDmlRenderer;
    let schema: &dyn SchemaRenderer = &DuckDbSchemaRenderer;
    let value_format: &dyn ValueFormatRenderer = &DuckDbValueFormatRenderer;
    let existence_probe: &dyn ExistenceProbePolicy = &DuckDbExistenceProbePolicy;
    let catalog_fold: &dyn CatalogFoldPolicy = &DuckDbCatalogFoldPolicy;
    let validation: &dyn ValidationPolicy = &DuckDbValidationPolicy;

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
    assert_eq!(validation.canonical_identifier("MixedCase"), "mixedcase");
    assert_eq!(
        validation.op_disposition("createTable", "base"),
        Disposition::Unsupported,
        "the outsider explicitly refuses every current engine operation shape"
    );
    assert_eq!(
        validation.op_disposition("futureOperation", "futureVariant"),
        Disposition::Unsupported,
        "an unfamiliar operation shape fails closed in the outsider's own policy"
    );
    assert_eq!(
        validation
            .raw_view_body_refusal("SELECT 1", None)
            .map(|refusal| refusal.reason),
        Some("DuckDB stub has no parser and refuses raw view bodies".to_string()),
        "the outsider states its OWN raw-view-body posture; it cannot inherit \
         PostgreSQL's parser-backed gate, and it cannot inherit the two shipping \
         descriptor backends' blanket trust either"
    );
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
    // ones it declared - not the "no to everything" a core-owned id->capability
    // table would have to give a name it does not recognise.
    assert!(dml.supports(Capability::CreateOrReplaceView));
    assert!(!dml.supports(Capability::PrivilegedCatalogObjects));
    assert!(!dml.supports(Capability::MaterializedView));
    // The outsider's PARTITION posture, stated in the same one place it states
    // everything else about itself. This used to be a required
    // a native-partitioning predicate on ValidationPolicy - a method the outsider had
    // to implement but whose answer nothing ever checked. It is a descriptor row
    // now, which means the outsider's NO is the ordinary consequence of not
    // claiming a capability rather than a separate contract to satisfy, and it
    // fails closed if a future author forgets it exists.
    assert!(!dml.supports(Capability::PartitionRelationDdl));

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

/// Required DML policy methods make a newcomer write its own behavior rather
/// than falling through to one of the three shipping SQL shapes.
#[test]
fn a_fourth_backend_writes_its_own_dml_policy() {
    let renderer: &dyn DmlRenderer = &DuckDbDmlRenderer;
    renderer
        .validate_assignment_semantics("update", "items", &BTreeMap::new())
        .expect("the outsider states its own simultaneous-assignment policy");

    let conflict = OnConflict {
        columns: vec!["id".to_string()],
        do_update: None,
    };
    let mut conflict_ctx = BindCtx::new(renderer);
    let conflict_error = renderer
        .render_on_conflict(
            OnConflictRenderRequest {
                table: "items",
                qualified_table: "\"main\".\"items\"",
                insert_columns: &["id".to_string()],
                on_conflict: &conflict,
                quoted_target_columns: &["\"id\"".to_string()],
            },
            &mut conflict_ctx,
        )
        .expect_err("the outsider explicitly refuses its unimplemented conflict grammar");
    assert!(conflict_error
        .to_string()
        .contains("DuckDB backend test does not implement structured onConflict"));

    let mut delete_ctx = BindCtx::new(renderer);
    let delete_error = renderer
        .render_limited_delete(
            LimitedDeleteRenderRequest {
                table: "items",
                qualified_table: "\"main\".\"items\"",
                rendered_where: "TRUE",
                limit: 1,
                catalog_identity_columns: None,
            },
            &mut delete_ctx,
        )
        .expect_err("the outsider explicitly refuses its unimplemented limited-delete grammar");
    assert!(delete_error
        .to_string()
        .contains("DuckDB backend test does not implement row-limited DELETE"));
}

/// The stub is only a proof if it never touches the closed enum.
///
/// A test that demonstrated a fourth backend by naming the removed closed enum somewhere
/// would be demonstrating the opposite thing. This reads its own source back and
/// refuses the mention, so the proof cannot rot into one by a later edit.
///
/// The needle is assembled from two halves on purpose. Spelled whole, the
/// detector's own line is the first thing it finds and the test fails on itself -
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

/// The name a backend's catalog gives a table's IMPLICIT primary-key relation is a
/// vendor fact, and the outsider states it the same way it states every other one.
///
/// The three shipping backends disagree on this in the strongest possible way.
/// PostgreSQL derives a name from the table (`<table>_pkey`) and stores it. MySQL
/// stores no name at all and reports the fixed catalog name `PRIMARY`, which is the
/// only name a MySQL primary key can have. SQLite has neither, and its reader
/// synthesizes one. There is no shared convention here to inherit, which is exactly
/// why an outsider must be asked rather than assumed.
///
/// `DuckDb` below answers `duck_pk_<table>` - a spelling no shipping backend uses,
/// chosen so that a predicate carrying any one vendor's convention cannot pass this
/// by accident.
#[test]
fn a_fourth_backend_names_its_own_implicit_primary_key() {
    let policy: &dyn CatalogFoldPolicy = &DuckDbCatalogFoldPolicy;
    let pk_name = policy.implicit_primary_key_name("items");
    assert_eq!(
        pk_name, "duck_pk_items",
        "the outsider writes its own implicit primary-key spelling instead of \
         inheriting one shipping backend's convention"
    );

    let snapshot = TableSnapshot {
        columns: Vec::new(),
        indexes: vec![zeroship_migrate_backend::snapshot::IndexSnapshot::btree(
            pk_name.clone(),
            true,
            vec!["id".to_string()],
        )],
        constraints: Vec::new(),
        runtime_options: Default::default(),
        attributes: zeroship_migrate_ir::attribute::Attributes::new(),
        partition_by: None,
        comment: None,
        stored_create_sql: None,
    };

    assert!(
        zeroship_migrate_backend::ddl::is_pk_index(policy, "items", &pk_name),
        "the contract crate's primary-key-index predicate must recognise the name \
         the REGISTERED backend gives the relation; a predicate that spells one \
         vendor's convention forces every other backend to report a name its own \
         catalog does not have"
    );
    assert_eq!(
        zeroship_migrate_backend::ddl::primary_key_columns(policy, "items", &snapshot),
        Some(&["id".to_string()][..]),
        "the contract crate reads a table's primary-key columns off the index the \
         REGISTERED backend named, so an outsider's inline-versus-table-level PK \
         rendering is decided by its own catalog spelling"
    );
}
