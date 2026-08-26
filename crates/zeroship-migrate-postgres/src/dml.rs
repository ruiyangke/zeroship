//! PostgreSQL SQL spelling.

use std::collections::BTreeMap;

use zero_migrate_backend::dml::{
    self, BindCtx, DmlError, LimitedDeleteRenderRequest, OnConflictRenderRequest,
};
use zero_migrate_backend::error::IrLowerError;
use zero_migrate_backend::renderer::{
    Capability, DmlRenderer, FeatureSupportKey, MaterializedNamedTypeOp,
};
use zero_migrate_backend::step::BindValue;
use zero_migrate_ir::backend::BackendDescriptor;
use zero_migrate_ir::dialect::DialectId;
use zero_migrate_ir::expr::{CastTarget, Duration, ExtractField, ScalarFn};
use zero_migrate_ir::ir::TableRef;
use zero_migrate_ir::ir::{
    ColType, CommentTarget, ExistenceGuard, IrScalar, IrValue, Op, SafeI64, SequenceOwnedBy,
    TriggerAction,
};
use zero_migrate_ir::validate::{ExprDialectFeature, ExprDialectRejection, ExprDialectValidator};

// This module's vendor identity. It names NO dialect literal of its own - the one
// declaration is `crate::DIALECT` in `lib.rs`, and the one-dialect rule is a
// per-CRATE rule now rather than a per-module one. See `render/backends/mod.rs`.
use crate::DIALECT;

/// A validated text operand in this backend's explicitly-typed spelling.
///
/// The `::text` cast is not decoration. Both positions that reach this - a regex
/// pattern and an `IN`-list element - put the literal next to an operator whose
/// overload resolution would otherwise see an untyped literal, so the cast is what
/// pins which operator runs.
///
/// Core owns whether the operand is LEGAL (non-empty, no NUL) and the two checks below
/// mirror `zero_migrate_backend::dml::in_list_text_literal`, which applies the same two
/// to the operand a caller's own backend then spells. This function was
/// `zero_migrate_backend::dml::pg_text_literal`, a vendor name in the neutral contract
/// whose only callers were this file and [`in_list_elem`] beside it.
fn text_literal(s: &str, what: &'static str) -> Result<String, DmlError> {
    if s.is_empty() {
        return Err(DmlError::UnrenderableExpr(format!(
            "{what} must be non-empty"
        )));
    }
    if s.contains('\0') {
        return Err(DmlError::UnrenderableExpr(format!(
            "{what} contains a NUL byte"
        )));
    }
    Ok(format!("{}::text", dml::sql_string_literal(s)))
}

/// One `IN`-list element in this backend's spelling.
///
/// It takes no backend, and that is the difference from the shared
/// `zero_migrate_backend::dml::render_in_list_elem_portable`: every spelling here is
/// FIXED - `'x'::text` for a string, the decimal verbatim - so there is no vendor to
/// resolve. The two backends whose in-list needs one (a quoted decimal, a hex string)
/// call the portable helper and hand it `self`.
///
/// It lived in the contract crate under a PostgreSQL-named spelling, and its only
/// caller was [`PostgresDmlRenderer::render_in_list`] below. The `inList` NODE is
/// portable; this backend's SPELLING of one is not, and that is the distinction the
/// old home lost.
fn in_list_elem(elem: &IrScalar) -> Result<String, DmlError> {
    Ok(match elem {
        IrScalar::Str(s) => text_literal(s, "inList element")?,
        IrScalar::Int(i) | IrScalar::Int64(i) => i.to_string(),
        IrScalar::Decimal(d) => d.clone(),
        IrScalar::Bool(b) => {
            if *b {
                "TRUE".to_string()
            } else {
                "FALSE".to_string()
            }
        }
        IrScalar::Null => "NULL".to_string(),
        IrScalar::Bytes(_) => {
            return Err(DmlError::UnrenderableExpr(
                "inList elements must be string, number, boolean, or null; bytes are not allowed"
                    .to_string(),
            ));
        }
    })
}

#[derive(Debug)]
pub(super) struct PostgresDmlRenderer;

pub(super) static RENDERER: PostgresDmlRenderer = PostgresDmlRenderer;

fn quote_engine_ident_as_dml(what: &'static str, ident: &str) -> Result<String, IrLowerError> {
    dml::quote_ident_checked_for_backend(ident, &RENDERER)
        .map_err(|e| DmlError::InvalidIdentifier {
            what,
            value: e.value,
        })
        .map_err(IrLowerError::DmlAssemble)
}

impl ExprDialectValidator for PostgresDmlRenderer {
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
                zero_migrate_ir::expr::AggFunc::Count
                | zero_migrate_ir::expr::AggFunc::Sum
                | zero_migrate_ir::expr::AggFunc::Avg
                | zero_migrate_ir::expr::AggFunc::Min
                | zero_migrate_ir::expr::AggFunc::Max
                | zero_migrate_ir::expr::AggFunc::StringAgg
                | zero_migrate_ir::expr::AggFunc::ArrayAgg
                | zero_migrate_ir::expr::AggFunc::BoolAnd
                | zero_migrate_ir::expr::AggFunc::BoolOr => Ok(()),
            },
            ExprDialectFeature::ConcatWs { .. }
            | ExprDialectFeature::SplitPart { .. }
            | ExprDialectFeature::UuidV7Generation
            | ExprDialectFeature::RegexMatch
            | ExprDialectFeature::StorageSize
            | ExprDialectFeature::Interval => Ok(()),
            // PostgreSQL renders the whole EXTRACT field set, so every part is
            // accepted - asserted through its own renderer rather than restated,
            // so the two can never disagree.
            ExprDialectFeature::Extract(field) => DmlRenderer::render_extract(self, field, "x")
                .map(|_| ())
                .map_err(|e| ExprDialectRejection {
                    code: zero_migrate_ir::validate::CODE_UNSUPPORTED,
                    kind: Some(zero_migrate_ir::validate::UnsupportedKind::Expr),
                    reason: format!("{e}"),
                    suggested_fix: None,
                }),
        }
    }
}

impl DmlRenderer for PostgresDmlRenderer {
    fn dialect(&self) -> DialectId {
        DIALECT
    }

    fn expr_validator(&self) -> &dyn ExprDialectValidator {
        self
    }

    fn descriptor(&self) -> &'static BackendDescriptor {
        &crate::descriptor::POSTGRES_DESCRIPTOR
    }

    fn supports(&self, cap: zero_migrate_ir::backend::Capability) -> bool {
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

    fn op_support_refusal(&self, op: &Op, _variant: &str) -> Option<&'static str> {
        match op {
            Op::CreateTrigger {
                action: TriggerAction::Body { .. },
                ..
            } => Some("Postgres triggers must execute a named trigger function"),
            _ => None,
        }
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
            | FeatureSupportKey::NonIdForeignKey
            | FeatureSupportKey::SequenceDefault
            | FeatureSupportKey::ConstraintNotValid
            | FeatureSupportKey::ExclusionConstraint
            | FeatureSupportKey::ExistenceGuardProbe
            | FeatureSupportKey::InsertOnConflict
            | FeatureSupportKey::MaterializedView
            | FeatureSupportKey::TriggerMultipleEvents
            | FeatureSupportKey::TriggerExecuteFunction
            | FeatureSupportKey::TriggerTruncateEvent
            | FeatureSupportKey::TriggerInsteadOfTiming
            | FeatureSupportKey::TriggerStatementForEach
            | FeatureSupportKey::TriggerWhen
            | FeatureSupportKey::Comment
            | FeatureSupportKey::Sequence
            | FeatureSupportKey::RawViewBody
            | FeatureSupportKey::RawSql
            | FeatureSupportKey::PartitionDdl => None,
            FeatureSupportKey::ForeignKeyNoLocalColumn => {
                Some("foreign keys need at least one local column")
            }
            FeatureSupportKey::AlterColumnUsing => Some(
                "setColumnType.using expression rendering is deferred in the current engine",
            ),
            FeatureSupportKey::RenameColumnGuard => Some(
                "renameColumn ifExists guards cannot be attributed to a single migration unit today",
            ),
            FeatureSupportKey::CreateOrReplaceMaterializedView => Some(
                "Postgres has no CREATE OR REPLACE MATERIALIZED VIEW and the other dialects have no materialized views",
            ),
            FeatureSupportKey::TriggerBody => {
                Some("Postgres triggers must execute a named trigger function")
            }
            FeatureSupportKey::TriggerRaiseIgnore => Some(
                "Postgres trigger bodies are unsupported; named functions must be used",
            ),
        }
    }

    fn quote_ident(&self, ident: &str) -> String {
        zero_migrate_backend::spelling::ansi_double_quote_ident(ident)
    }

    fn qualify_table(&self, project_schema: &str, table: &str) -> Result<String, DmlError> {
        let t = dml::quote_bare_ident_for_backend("table", table, &RENDERER)?;
        Ok(format!(
            "{}.{}",
            dml::quote_ident_checked_for_backend(project_schema, &RENDERER).map_err(|e| {
                DmlError::InvalidIdentifier {
                    what: "schema",
                    value: e.value,
                }
            })?,
            t
        ))
    }

    fn cast_target(&self, target: CastTarget) -> &'static str {
        match target {
            CastTarget::Text => "text",
            CastTarget::Int => "integer",
            CastTarget::Real => "real",
            CastTarget::Boolean => "boolean",
            CastTarget::Bytes => "bytea",
            CastTarget::Uuid => "uuid",
        }
    }

    fn placeholder(&self, n: usize) -> String {
        format!("${n}")
    }

    fn inline_string_literal(&self, s: &str) -> String {
        dml::sql_string_literal(s)
    }

    fn inline_decimal_literal(&self, d: &str) -> String {
        d.to_string()
    }

    fn inline_bytes_literal(&self, bytes: &[u8]) -> String {
        let encoded = zero_migrate_backend::spelling::base64_standard(bytes);
        format!("decode({}, 'base64')", dml::sql_string_literal(&encoded))
    }

    /// PostgreSQL's schema-blind DML seam takes the canonical base64 as a TEXT
    /// bind and decodes it inside the statement, so the bound spelling is the
    /// inline one with a placeholder where the literal would be.
    fn bind_bytes(&self, bytes: &[u8], push: &mut dyn FnMut(BindValue) -> String) -> String {
        let placeholder = push(BindValue::Text(
            zero_migrate_backend::spelling::base64_standard(bytes),
        ));
        format!("decode({placeholder}, 'base64')")
    }

    fn validate_assignment_semantics(
        &self,
        _op: &'static str,
        _table: &str,
        _set: &BTreeMap<String, IrValue>,
    ) -> Result<(), DmlError> {
        // PostgreSQL evaluates every assignment RHS from the row before the SET
        // list, so cross-assignment reads already have the authored simultaneous
        // semantics.
        Ok(())
    }

    fn render_on_conflict(
        &self,
        request: OnConflictRenderRequest<'_>,
        ctx: &mut BindCtx<'_>,
    ) -> Result<String, DmlError> {
        let target = format!("ON CONFLICT ({})", request.quoted_target_columns.join(", "));
        match &request.on_conflict.do_update {
            None => Ok(format!(" {target} DO NOTHING")),
            Some(set) => {
                if set.is_empty() {
                    return Ok(format!(" {target} DO NOTHING"));
                }
                // BTreeMap => deterministic column order (canonical).
                let mut assigns = Vec::with_capacity(set.len());
                for (col, val) in set {
                    let qc = dml::quote_ident_for_backend("column", col, self)?;
                    let ph = dml::render_value_bound(val, ctx)?;
                    assigns.push(format!("{qc} = {ph}"));
                }
                Ok(format!(" {target} DO UPDATE SET {}", assigns.join(", ")))
            }
        }
    }

    fn render_limited_delete(
        &self,
        request: LimitedDeleteRenderRequest<'_>,
        ctx: &mut BindCtx<'_>,
    ) -> Result<String, DmlError> {
        let ph = ctx.push_bind(BindValue::Int(
            i64::try_from(request.limit).unwrap_or(i64::MAX),
        ));
        Ok(format!(
            "DELETE FROM {} WHERE (tableoid, ctid) IN \
             (SELECT tableoid, ctid FROM {} WHERE {} LIMIT {ph})",
            request.qualified_table, request.qualified_table, request.rendered_where
        ))
    }

    fn render_in_list(
        &self,
        expr: &str,
        elems: &[IrScalar],
        negated: bool,
        joiner: &str,
    ) -> Result<String, DmlError> {
        let rendered: Result<Vec<_>, _> = elems.iter().map(in_list_elem).collect();
        let (cmp, quantifier) = if negated { ("<>", "ALL") } else { ("=", "ANY") };
        Ok(format!(
            "({expr} {cmp} {quantifier} (ARRAY[{}]))",
            rendered?.join(joiner)
        ))
    }

    fn render_regex_match(&self, expr: &str, pattern: &str) -> Result<String, DmlError> {
        Ok(format!(
            "({expr} ~ {})",
            text_literal(pattern, "regex pattern")?
        ))
    }

    fn render_storage_size(&self, expr: &str) -> Result<String, DmlError> {
        // PostgreSQL measures the bytes a value actually occupies, TOAST and
        // compression included, which is why this is not `length()`.
        Ok(format!("pg_column_size({expr})"))
    }

    fn render_interval(&self, duration: &Duration) -> Result<String, DmlError> {
        // PostgreSQL takes the whole duration as ONE quoted parts string
        // (`INTERVAL '1 year 2 days'`), and pluralises each unit.
        let mut parts = Vec::new();
        for (value, singular, plural) in [
            (duration.years, "year", "years"),
            (duration.months, "month", "months"),
            (duration.days, "day", "days"),
            (duration.hours, "hour", "hours"),
            (duration.minutes, "minute", "minutes"),
            (duration.seconds, "second", "seconds"),
        ] {
            if let Some(value) = value {
                let unit = if value == 1 || value == -1 {
                    singular
                } else {
                    plural
                };
                parts.push(format!("{value} {unit}"));
            }
        }
        if parts.is_empty() {
            return Err(DmlError::UnrenderableExpr(
                "an interval duration must include at least one field".to_string(),
            ));
        }
        Ok(format!(
            "INTERVAL {}",
            dml::sql_string_literal(&parts.join(" "))
        ))
    }

    fn render_extract(&self, field: ExtractField, expr: &str) -> Result<String, DmlError> {
        // PostgreSQL implements the whole field set, so there is no refusal arm
        // here - and that is a fact about PostgreSQL, stated by PostgreSQL,
        // rather than a shape the IR was built around.
        Ok(format!(
            "EXTRACT({} FROM {expr})",
            dml::extract_field_name(field)
        ))
    }

    fn render_concat(&self, l: &str, r: &str) -> String {
        format!("({l} || {r})")
    }

    fn render_distinct_from(&self, l: &str, r: &str) -> String {
        format!("({l} IS DISTINCT FROM {r})")
    }

    fn render_scalar_fn_override(&self, _f: ScalarFn, _args: &[String]) -> Option<String> {
        // PostgreSQL spells every allow-listed scalar the way the shared table
        // does; the portable INTENT and the native name coincide here.
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
        let d = format!("'{}'", delim.replace('\'', "''"));
        Ok(format!("split_part({col_sql}, {d}, {n})"))
    }

    fn synth_now(&self) -> String {
        "now()".to_string()
    }

    fn uuid_v4(&self) -> String {
        "gen_random_uuid()".to_string()
    }

    fn uuid_v7(&self) -> Result<String, DmlError> {
        Ok("uuidv7()".to_string())
    }

    fn view_create_prefix(
        &self,
        materialized: bool,
        replace: bool,
    ) -> Result<String, IrLowerError> {
        // Postgres has no `CREATE OR REPLACE MATERIALIZED VIEW`. Rather than
        // silently dropping `replace` (which would leave a populated matview in
        // place under a "replace" request) or destructively DROP+CREATE it, fail
        // closed so the author resolves the contradiction explicitly.
        if materialized && replace {
            return Err(IrLowerError::UnsupportedOp(
                "validated createView replace+materialized reached lower",
            ));
        }
        let mut create = String::from("CREATE ");
        if materialized {
            create.push_str("MATERIALIZED VIEW ");
        } else if replace && self.supports(Capability::CreateOrReplaceView) {
            create.push_str("OR REPLACE VIEW ");
        } else {
            create.push_str("VIEW ");
        }
        Ok(create)
    }

    fn view_replace_prelude(&self, _qname: &str, _replace: bool) -> Vec<String> {
        Vec::new()
    }

    fn view_object_name(&self, name: &str, eff_schema: &str) -> Result<String, IrLowerError> {
        Ok(format!(
            "{}.{}",
            quote_engine_ident_as_dml("schema", eff_schema)?,
            dml::quote_bare_ident_for_backend("view", name, &RENDERER)?
        ))
    }

    fn render_table_ref(&self, table: &TableRef, eff_schema: &str) -> Result<String, IrLowerError> {
        let mut sql = {
            let schema = table.schema.as_deref().unwrap_or(eff_schema);
            format!(
                "{}.{}",
                quote_engine_ident_as_dml("schema", schema)?,
                dml::quote_bare_ident_for_backend("table", &table.name, &RENDERER)?
            )
        };
        if let Some(alias) = table.alias.as_deref() {
            sql.push_str(" AS ");
            sql.push_str(&dml::quote_bare_ident_for_backend(
                "table alias",
                alias,
                &RENDERER,
            )?);
        }
        Ok(sql)
    }

    fn render_sequence_op(
        &self,
        op: &Op,
        eff_schema: &str,
    ) -> Result<zero_migrate_backend::vendor::VendorStatement, IrLowerError> {
        render_sequence_op(op, eff_schema)
    }

    fn render_materialized_named_type_op(
        &self,
        op: MaterializedNamedTypeOp<'_>,
    ) -> Result<zero_migrate_backend::vendor::VendorStatement, IrLowerError> {
        render_materialized_named_type_op(op)
    }

    fn render_comment_op(
        &self,
        op: &Op,
        eff_schema: &str,
    ) -> Result<zero_migrate_backend::vendor::VendorStatement, IrLowerError> {
        render_comment_op(op, eff_schema)
    }

    /// This vendor OWNS the vendor-op surface: every op kind [`crate::vendor`]
    /// renders is this crate's spelling, and it is the only registered backend that
    /// renders them - so an artifact carrying one measures a `DialectScope::Only`
    /// reach naming this dialect, and apply declines it against every other target.
    ///
    /// A one-line delegation to the module that already held them. Nothing about the
    /// rendering changed when the engine stopped naming it; this method exists so
    /// that reaching them requires going through the registry.
    fn render_vendor_op(
        &self,
        op: &Op,
        eff_schema: &str,
    ) -> Result<
        Vec<zero_migrate_backend::vendor::VendorStatement>,
        zero_migrate_backend::vendor::VendorError,
    > {
        crate::vendor::render_vendor_op(op, eff_schema)
    }

    fn render_trigger_op(
        &self,
        op: &Op,
        eff_schema: &str,
    ) -> Result<Vec<zero_migrate_backend::vendor::VendorStatement>, IrLowerError> {
        if let Op::CreateTrigger {
            action: TriggerAction::Body { .. },
            ..
        } = op
        {
            if !self.supports(Capability::TriggerBody) {
                return Err(IrLowerError::TriggerUnsupported {
                    kind: "triggerBody",
                    dialect: DIALECT,
                });
            }
        }
        let stmts = match crate::vendor::render_vendor_op(op, eff_schema) {
            Ok(stmts) => stmts,
            Err(zero_migrate_backend::vendor::VendorError::UnsupportedTriggerAction {
                kind,
                ..
            }) => {
                return Err(IrLowerError::TriggerUnsupported {
                    kind,
                    dialect: DIALECT,
                });
            }
            Err(e) => return Err(IrLowerError::Vendor(e)),
        };
        Ok(stmts)
    }
}

fn render_materialized_named_type_op(
    op: MaterializedNamedTypeOp<'_>,
) -> Result<zero_migrate_backend::vendor::VendorStatement, IrLowerError> {
    match op {
        MaterializedNamedTypeOp::CreateEnum {
            name,
            qualified_name,
            values,
        } => {
            let rendered_values = values
                .iter()
                .map(|value| dml::sql_string_literal(value))
                .collect::<Vec<_>>()
                .join(", ");
            let up = format!("CREATE TYPE {qualified_name} AS ENUM ({rendered_values})");
            let down = Some(format!("DROP TYPE {qualified_name}"));
            Ok(zero_migrate_backend::vendor::VendorStatement {
                name: format!("create_enum_{name}"),
                up,
                down,
            })
        }
        MaterializedNamedTypeOp::DropEnum {
            name,
            qualified_name,
        } => Ok(zero_migrate_backend::vendor::VendorStatement {
            name: format!("drop_enum_{name}"),
            up: format!("DROP TYPE {qualified_name}"),
            down: None,
        }),
        MaterializedNamedTypeOp::CreateDomain {
            name,
            qualified_name,
            base_type,
            default,
            not_null,
            check,
        } => {
            let mut up = format!("CREATE DOMAIN {qualified_name} AS {base_type}");
            if let Some(default) = default {
                up.push_str(" DEFAULT ");
                up.push_str(default);
            }
            if not_null {
                up.push_str(" NOT NULL");
            }
            if let Some(check) = check {
                up.push_str(" CHECK (");
                up.push_str(check);
                up.push(')');
            }
            let down = Some(format!("DROP DOMAIN {qualified_name}"));
            Ok(zero_migrate_backend::vendor::VendorStatement {
                name: format!("create_domain_{name}"),
                up,
                down,
            })
        }
        MaterializedNamedTypeOp::DropDomain {
            name,
            qualified_name,
        } => Ok(zero_migrate_backend::vendor::VendorStatement {
            name: format!("drop_domain_{name}"),
            up: format!("DROP DOMAIN {qualified_name}"),
            down: None,
        }),
    }
}

fn render_sequence_op(
    op: &Op,
    eff_schema: &str,
) -> Result<zero_migrate_backend::vendor::VendorStatement, IrLowerError> {
    match op {
        Op::CreateSequence {
            name,
            as_type,
            increment,
            start,
            min_value,
            max_value,
            cache,
            cycle,
            owned_by,
            ..
        } => {
            let qname = pg_sequence_qname(eff_schema, name)?;
            let mut up = format!("CREATE SEQUENCE {qname}");
            if let Some(as_type) = as_type {
                up.push_str(" AS ");
                up.push_str(render_sequence_as_type(as_type)?);
            }
            if let Some(n) = increment {
                up.push_str(" INCREMENT BY ");
                up.push_str(&n.to_string());
            }
            if let Some(n) = start {
                up.push_str(" START WITH ");
                up.push_str(&n.to_string());
            }
            render_sequence_optional_bound(&mut up, "MINVALUE", "NO MINVALUE", min_value);
            render_sequence_optional_bound(&mut up, "MAXVALUE", "NO MAXVALUE", max_value);
            if let Some(n) = cache {
                up.push_str(" CACHE ");
                up.push_str(&n.to_string());
            }
            if let Some(cycle) = cycle {
                up.push_str(if *cycle { " CYCLE" } else { " NO CYCLE" });
            }
            if let Some(owned_by) = owned_by {
                up.push_str(" OWNED BY ");
                up.push_str(&render_sequence_owned_by(owned_by.as_ref(), eff_schema)?);
            }
            Ok(zero_migrate_backend::vendor::VendorStatement {
                name: format!("create_sequence_{name}"),
                up,
                down: Some(format!("DROP SEQUENCE {qname}")),
            })
        }
        Op::AlterSequence {
            name,
            increment,
            restart,
            min_value,
            max_value,
            cache,
            cycle,
            owned_by,
            ..
        } => {
            // An alter that asks for nothing renders `ALTER SEQUENCE <name>` with no
            // action clause, which is not a statement. PRESENCE of the option is the
            // test, never its inner value: `restart: null` is a bare RESTART,
            // `min_value: null` is NO MINVALUE and `owned_by: null` is OWNED BY NONE.
            if increment.is_none()
                && restart.is_none()
                && min_value.is_none()
                && max_value.is_none()
                && cache.is_none()
                && cycle.is_none()
                && owned_by.is_none()
            {
                return Err(IrLowerError::AlterSequenceHasNoAction { name: name.clone() });
            }
            let qname = pg_sequence_qname(eff_schema, name)?;
            let mut up = format!("ALTER SEQUENCE {qname}");
            if let Some(n) = increment {
                up.push_str(" INCREMENT BY ");
                up.push_str(&n.to_string());
            }
            if let Some(restart) = restart {
                up.push_str(" RESTART");
                if let Some(n) = restart {
                    up.push_str(" WITH ");
                    up.push_str(&n.to_string());
                }
            }
            render_sequence_optional_bound(&mut up, "MINVALUE", "NO MINVALUE", min_value);
            render_sequence_optional_bound(&mut up, "MAXVALUE", "NO MAXVALUE", max_value);
            if let Some(n) = cache {
                up.push_str(" CACHE ");
                up.push_str(&n.to_string());
            }
            if let Some(cycle) = cycle {
                up.push_str(if *cycle { " CYCLE" } else { " NO CYCLE" });
            }
            if let Some(owned_by) = owned_by {
                up.push_str(" OWNED BY ");
                up.push_str(&render_sequence_owned_by(owned_by.as_ref(), eff_schema)?);
            }
            Ok(zero_migrate_backend::vendor::VendorStatement {
                name: format!("alter_sequence_{name}"),
                up,
                down: None,
            })
        }
        Op::DropSequence {
            name,
            existence_guard,
            ..
        } => {
            let qname = pg_sequence_qname(eff_schema, name)?;
            let mut up = String::from("DROP SEQUENCE ");
            if matches!(existence_guard, Some(ExistenceGuard::IfExists)) {
                up.push_str("IF EXISTS ");
            }
            up.push_str(&qname);

            // Refuse to synthesize an inverse: the definition is half the object
            // and its runtime position is the other half. No IR history knows the
            // position, so recreation could reissue values.
            Ok(zero_migrate_backend::vendor::VendorStatement {
                name: format!("drop_sequence_{name}"),
                up,
                down: None,
            })
        }
        _ => Err(IrLowerError::UnsupportedOp(
            "non-sequence op routed to sequence renderer",
        )),
    }
}

fn render_comment_op(
    op: &Op,
    eff_schema: &str,
) -> Result<zero_migrate_backend::vendor::VendorStatement, IrLowerError> {
    let Op::Comment { target, comment } = op else {
        return Err(IrLowerError::UnsupportedOp(
            "non-comment op routed to comment renderer",
        ));
    };
    let object = render_comment_target(target, eff_schema)?;
    let value = comment
        .as_deref()
        .map(dml::sql_string_literal)
        .unwrap_or_else(|| "NULL".to_string());
    Ok(zero_migrate_backend::vendor::VendorStatement {
        name: format!("comment_{}", comment_target_name_part(target)),
        up: format!("COMMENT ON {object} IS {value}"),
        down: None,
    })
}

fn comment_target_name_part(target: &CommentTarget) -> String {
    match target {
        CommentTarget::Table { name, .. }
        | CommentTarget::Index { name, .. }
        | CommentTarget::View { name, .. }
        | CommentTarget::Type { name, .. }
        | CommentTarget::Sequence { name, .. }
        | CommentTarget::Function { name, .. } => name.clone(),
        CommentTarget::Column { table, name, .. }
        | CommentTarget::Constraint { table, name, .. } => format!("{table}_{name}"),
    }
}

fn pg_comment_qname(kind: &'static str, schema: &str, name: &str) -> Result<String, IrLowerError> {
    Ok(format!(
        "{}.{}",
        quote_engine_ident_as_dml("schema", schema)?,
        quote_engine_ident_as_dml(kind, name)?
    ))
}

/// `eff_schema` ALREADY accounts for the target's own qualifier: `Op::schema()` for a
/// comment returns `target.schema()`, and `IrAuthor::effective_schema` canonicalizes a
/// case-variant of the project schema before handing it here.
///
/// So do not re-read `target.schema()`. It is the author's casing, and the render seam
/// quotes byte-verbatim while `SchemaScope::permits` matches case-insensitively - a
/// target qualified `APP` under project `app` was blessed as `app` by the confinement
/// gate and rendered `"APP"`, which PostgreSQL treats as a different schema entirely.
fn render_comment_target(target: &CommentTarget, eff_schema: &str) -> Result<String, IrLowerError> {
    let schema = eff_schema;
    Ok(match target {
        CommentTarget::Table { name, .. } => {
            format!("TABLE {}", pg_comment_qname("table", schema, name)?)
        }
        CommentTarget::Column { table, name, .. } => format!(
            "COLUMN {}.{}",
            pg_comment_qname("table", schema, table)?,
            quote_engine_ident_as_dml("column", name)?
        ),
        CommentTarget::Index { name, .. } => {
            format!("INDEX {}", pg_comment_qname("index", schema, name)?)
        }
        CommentTarget::Constraint { table, name, .. } => format!(
            "CONSTRAINT {} ON {}",
            quote_engine_ident_as_dml("constraint", name)?,
            pg_comment_qname("table", schema, table)?
        ),
        CommentTarget::View { name, .. } => {
            format!("VIEW {}", pg_comment_qname("view", schema, name)?)
        }
        CommentTarget::Type { name, .. } => {
            format!("TYPE {}", pg_comment_qname("type", schema, name)?)
        }
        CommentTarget::Sequence { name, .. } => {
            format!("SEQUENCE {}", pg_comment_qname("sequence", schema, name)?)
        }
        CommentTarget::Function { name, .. } => {
            format!("FUNCTION {}", pg_comment_qname("function", schema, name)?)
        }
    })
}

fn pg_sequence_qname(schema: &str, name: &str) -> Result<String, IrLowerError> {
    Ok(format!(
        "{}.{}",
        quote_engine_ident_as_dml("schema", schema)?,
        quote_engine_ident_as_dml("sequence", name)?
    ))
}

fn render_sequence_optional_bound(
    sql: &mut String,
    value_kw: &'static str,
    none_kw: &'static str,
    value: &Option<Option<SafeI64>>,
) {
    if let Some(value) = value {
        sql.push(' ');
        match value {
            Some(n) => {
                sql.push_str(value_kw);
                sql.push(' ');
                sql.push_str(&n.to_string());
            }
            None => sql.push_str(none_kw),
        }
    }
}

fn render_sequence_as_type(as_type: &ColType) -> Result<&'static str, IrLowerError> {
    match as_type {
        ColType::SmallInt => Ok("smallint"),
        ColType::Int => Ok("integer"),
        ColType::BigInt => Ok("bigint"),
        _ => Err(IrLowerError::UnsupportedOp(
            "sequence AS type must be smallInt, int, or bigInt",
        )),
    }
}

fn render_sequence_owned_by(
    owned_by: Option<&SequenceOwnedBy>,
    eff_schema: &str,
) -> Result<String, IrLowerError> {
    let Some(owned_by) = owned_by else {
        return Ok("NONE".to_string());
    };
    Ok(format!(
        "{}.{}.{}",
        quote_engine_ident_as_dml("schema", eff_schema)?,
        quote_engine_ident_as_dml("table", &owned_by.table)?,
        quote_engine_ident_as_dml("column", &owned_by.column)?
    ))
}
