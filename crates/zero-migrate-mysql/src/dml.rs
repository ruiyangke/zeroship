//! MySQL SQL spelling. The future `zero-migrate-mysql`.
//!
//! This was the only backend module whose trigger spelling actually lived here, and
//! it is therefore the worked example step 3 followed: SQLite's now sits in
//! `backends/sqlite.rs` in the same shape. PostgreSQL's is still in `render::vendor`.

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
use zero_migrate_ir::dialect::{DialectId, MYSQL};
use zero_migrate_ir::expr::{AggFunc, CastTarget, Expr, ExtractField, ScalarFn};
use zero_migrate_ir::ir::{
    ForEach, IrScalar, IrValue, Op, RaiseLevel, TableRef, TriggerAction, TriggerEvent, TriggerStmt,
    TriggerTiming,
};
use zero_migrate_ir::validate::{
    ExprDialectFeature, ExprDialectRejection, ExprDialectValidator, UnsupportedKind,
    CODE_DIALECT_UNSUPPORTED, CODE_EXPR_NOT_PORTABLE, CODE_UNSUPPORTED,
};

/// This module's own vendor identity — the ONE dialect literal it is allowed to
/// name. See `backends/mod.rs`.
///
/// Every `dml::*_for_dialect(.., DIALECT)` call below is core's "validate, then
/// ask the vendor how to spell it" seam: `dml` owns whether an identifier is
/// LEGAL (semantics), this module owns how it is WRITTEN (spelling), and the
/// round trip goes back out through the `DmlRenderer` trait object. The const is
/// what keeps that from being a hard-coded vendor name inside a vendor module.
const DIALECT: DialectId = MYSQL;

const PREVIEW_SESSION_PROLOGUE: &[&str] = &[
    "SET @__zero_migrate_preview_saved_sql_mode = @@SESSION.sql_mode;",
    "SET SESSION sql_mode = CONCAT_WS(',', @@SESSION.sql_mode, 'NO_BACKSLASH_ESCAPES', 'NO_AUTO_VALUE_ON_ZERO');",
];
const PREVIEW_SESSION_EPILOGUE: &[&str] =
    &["SET SESSION sql_mode = @__zero_migrate_preview_saved_sql_mode;"];

#[derive(Debug)]
pub(super) struct MysqlDmlRenderer;

pub(super) static RENDERER: MysqlDmlRenderer = MysqlDmlRenderer;

fn unsupported_expr(name: &'static str) -> ExprDialectRejection {
    ExprDialectRejection {
        code: CODE_UNSUPPORTED,
        kind: Some(UnsupportedKind::Expr),
        // This backend speaks only for itself, and it names itself from its own
        // DialectId rather than from a hard-coded vendor word.
        reason: format!("{name} has no {} renderer", DIALECT.as_str()),
        suggested_fix: Some(format!(
            "rewrite the predicate using portable expression nodes, or give {} its own leg with dialect({{ ... }})",
            DIALECT.as_str()
        )),
    }
}

fn postgres_first_aggregate(name: &'static str) -> ExprDialectRejection {
    ExprDialectRejection {
        code: CODE_DIALECT_UNSUPPORTED,
        kind: Some(UnsupportedKind::Expr),
        reason: format!(
            "{name} aggregate is PostgreSQL-first and has no native SQLite/MySQL renderer"
        ),
        suggested_fix: Some(
            "wrap this aggregate in dialect({ postgres: ..., sqlite: ..., mysql: ... }) with explicit non-Postgres legs, or target Postgres only"
                .to_string(),
        ),
    }
}

impl ExprDialectValidator for MysqlDmlRenderer {
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
                | ScalarFn::Replace => Ok(()),
                ScalarFn::CurrentSetting | ScalarFn::CurrentUser => {
                    Err(unsupported_expr("current_setting / current_user"))
                }
            },
            ExprDialectFeature::Aggregate(function) => match function {
                AggFunc::Count | AggFunc::Sum | AggFunc::Avg | AggFunc::Min | AggFunc::Max => Ok(()),
                AggFunc::StringAgg => Err(postgres_first_aggregate("stringAgg")),
                AggFunc::ArrayAgg => Err(postgres_first_aggregate("arrayAgg")),
                AggFunc::BoolAnd => Err(postgres_first_aggregate("boolAnd")),
                AggFunc::BoolOr => Err(postgres_first_aggregate("boolOr")),
            },
            ExprDialectFeature::ConcatWs { .. }
            | ExprDialectFeature::SplitPart { .. }
            | ExprDialectFeature::RegexMatch => Ok(()),
            ExprDialectFeature::UuidV7Generation => Err(ExprDialectRejection {
                code: CODE_EXPR_NOT_PORTABLE,
                kind: None,
                reason: "uuidV7 database generation requires PostgreSQL 18+; MySQL and SQLite have no exact database UUIDv7 generator"
                    .to_string(),
                suggested_fix: Some(
                    "use an externally supplied UUIDv7 on this target, or use uuidV4() when database-generated random UUIDs are acceptable"
                        .to_string(),
                ),
            }),
            ExprDialectFeature::StorageSize => Err(unsupported_expr("storageSize")),
            ExprDialectFeature::PgExtract => Err(unsupported_expr("PG EXTRACT")),
            ExprDialectFeature::PgInterval => Err(unsupported_expr("PG interval literal")),
        }
    }
}

/// Return whether this backend's selected leg of an expression reads `column`.
/// This mirrors the renderer's recursive closed-AST walk so an unsafe dependency
/// cannot hide inside CASE, a helper, or a dialect branch.
fn expr_references_column(expr: &Expr, column: &str) -> Result<bool, DmlError> {
    Ok(match expr {
        Expr::ColRef { name, .. } => name == column,
        Expr::Literal { .. } | Expr::UuidV4 | Expr::UuidV7 | Expr::PgInterval { .. } => false,
        Expr::BinOp { lhs, rhs, .. } => {
            expr_references_column(lhs, column)? || expr_references_column(rhs, column)?
        }
        Expr::UnaryOp { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::StorageSize { expr: operand }
        | Expr::Extract { from: operand, .. }
        | Expr::PgExtract { from: operand, .. }
        | Expr::RegexMatch { expr: operand, .. }
        | Expr::InList { expr: operand, .. } => expr_references_column(operand, column)?,
        Expr::Case { branches, r#else } => {
            let mut found = false;
            for branch in branches {
                if expr_references_column(&branch.when, column)?
                    || expr_references_column(&branch.then, column)?
                {
                    found = true;
                    break;
                }
            }
            if !found {
                if let Some(r#else) = r#else {
                    found = expr_references_column(r#else, column)?;
                }
            }
            found
        }
        Expr::FnCall { args, .. } | Expr::FnSynth { args, .. } => {
            let mut found = false;
            for arg in args {
                if expr_references_column(arg, column)? {
                    found = true;
                    break;
                }
            }
            found
        }
        Expr::Between { operand, low, high } => {
            expr_references_column(operand, column)?
                || expr_references_column(low, column)?
                || expr_references_column(high, column)?
        }
        Expr::Like { operand, pattern } => {
            expr_references_column(operand, column)? || expr_references_column(pattern, column)?
        }
        Expr::DistinctFrom { left, right } => {
            expr_references_column(left, column)? || expr_references_column(right, column)?
        }
        Expr::Agg { arg, delimiter, .. } => {
            if let Some(arg) = arg {
                if expr_references_column(arg, column)? {
                    return Ok(true);
                }
            }
            if let Some(delimiter) = delimiter {
                expr_references_column(delimiter, column)?
            } else {
                false
            }
        }
        Expr::Dialectal { legs } => {
            expr_references_column(dml::select_dialect_leg(DIALECT, legs)?, column)?
        }
    })
}

impl DmlRenderer for MysqlDmlRenderer {
    fn dialect(&self) -> DialectId {
        DIALECT
    }

    fn expr_validator(&self) -> &dyn ExprDialectValidator {
        self
    }

    fn descriptor(&self) -> &'static BackendDescriptor {
        &crate::descriptor::MYSQL_DESCRIPTOR
    }

    fn supports(&self, cap: zero_migrate_ir::backend::Capability) -> bool {
        self.descriptor().capabilities.contains(cap)
    }

    fn preview_session_prologue(&self) -> &'static [&'static str] {
        PREVIEW_SESSION_PROLOGUE
    }

    fn preview_session_epilogue(&self) -> &'static [&'static str] {
        PREVIEW_SESSION_EPILOGUE
    }

    fn guarded_ddl_preview_limitation(&self) -> Option<&'static str> {
        Some(
            "present createTable/addColumn is refused until MySQL column-type equality is implemented; ",
        )
    }

    fn alter_ops_require_live_schema(&self) -> bool {
        false
    }

    fn op_support_refusal(&self, op: &Op, variant: &str) -> Option<&'static str> {
        match op {
            Op::CreateTable { .. } if variant == "partitioned" => {
                Some("partitioned tables are PostgreSQL-only")
            }
            Op::CreateTable { .. } if variant == "pgOnlyIndexFeature" => Some(
                "createTable BRIN/INCLUDE/WITH/ONLY index features are PostgreSQL-only",
            ),
            Op::CreateTable { .. } if variant == "nextvalDefault" => Some(
                "nextval sequence defaults are PostgreSQL-only; SQLite/MySQL have no standalone sequences",
            ),
            Op::CreateTable { .. } if variant == "identityAlways" => Some(
                "identity({ always: true }) is PostgreSQL-only; SQLite/MySQL support only \
                 identity({ always: false }) / autoIncrement() on the sole integer primary key",
            ),
            Op::CreateTable { .. } if variant == "nonportableByDefaultIdentity" => Some(
                "identity({ always: false }) / autoIncrement() must be the sole primary-key \
                 column on SQLite/MySQL",
            ),
            Op::CreatePartition { .. }
            | Op::AttachPartition { .. }
            | Op::DetachPartition { .. }
            | Op::DropPartition { .. } => {
                Some("partition lifecycle operations are PostgreSQL-only")
            }
            Op::AddColumn { .. } if variant == "identity" => Some(
                "addColumn identity is PostgreSQL-only; SQLite/MySQL auto-increment \
                 identity requires a createTable sole primary key",
            ),
            Op::AddColumn { .. } if variant == "nextvalDefault" => Some(
                "nextval sequence defaults are PostgreSQL-only; SQLite/MySQL have no standalone sequences",
            ),
            Op::CreateIndex {
                columns, r#where, ..
            } => {
                if columns.iter().any(|element| {
                    matches!(element, zero_migrate_ir::ir::IndexElement::Expr { .. })
                }) {
                    Some("createIndex expression elements are not supported on MySQL")
                } else if r#where.is_some() {
                    Some(
                        "createIndex partial predicates require partial-index support; MySQL does not support partial indexes",
                    )
                } else {
                    Some("createIndex BRIN/INCLUDE/WITH/ONLY features are unsupported on MySQL")
                }
            }
            Op::Comment { .. } => Some("COMMENT ON is PostgreSQL-only in the current engine"),
            Op::SetColumnDefault { .. } if variant == "nextval" => Some(
                "nextval sequence defaults are PostgreSQL-only; SQLite/MySQL have no standalone sequences",
            ),
            Op::SetColumnType { .. }
            | Op::SetColumnDefault { .. }
            | Op::SetColumnNotNull { .. }
            | Op::DropColumnNotNull { .. }
            | Op::DropColumnDefault { .. } => Some(
                "MySQL restates the whole column definition in MODIFY COLUMN, which this op \
                 does not carry, and this engine does not yet read the live definition back for \
                 a nullability change the way it does for a retype — express it as a schema \
                 change rather than a stand-alone op",
            ),
            Op::ValidateConstraint { .. } => Some(
                "VALIDATE CONSTRAINT is PostgreSQL-only online constraint adoption (the second \
                 half of NOT VALID → VALIDATE CONSTRAINT); SQLite and MySQL have no such \
                 statement, so there is nothing to validate",
            ),
            Op::RenameColumn { .. } if variant != "existenceGuard" => {
                Some("renameColumn is render-only for MySQL, not live-rendered")
            }
            Op::AddConstraint { .. } if variant == "check" => Some(
                "addConstraint(check) expression rendering is PostgreSQL-only in the current engine",
            ),
            Op::AddConstraint { .. } if variant == "exclusion" => {
                Some("exclusion constraints are PostgreSQL-only in the current engine")
            }
            Op::AddConstraint { .. } if variant == "fkNotValid" => Some(
                "NOT VALID online constraint adoption (addForeignKey { notValid }) is PostgreSQL-only in the current engine",
            ),
            Op::Insert { .. } if variant == "onConflictDoNothing" => Some(
                "MySQL cannot express targeted onConflict DO NOTHING without firing update triggers or suppressing unrelated errors",
            ),
            Op::CreateView { .. } if variant != "materializedReplace" => {
                Some("materialized views are PostgreSQL-only in the current engine")
            }
            Op::DropView { .. } => {
                Some("materialized views are PostgreSQL-only in the current engine")
            }
            Op::CreateDomain { .. } => Some(
                "nextval sequence defaults are PostgreSQL-only; SQLite/MySQL have no standalone sequences",
            ),
            Op::CreateSequence { .. } | Op::AlterSequence { .. } | Op::DropSequence { .. } => {
                Some("standalone sequence objects are PostgreSQL-only in the current engine")
            }
            Op::CreateSchema { .. } | Op::DropSchema { .. } => {
                Some("schema vendor primitives are PostgreSQL-only")
            }
            Op::CreateExtension { .. } | Op::DropExtension { .. } => {
                Some("extension vendor primitives are PostgreSQL-only")
            }
            Op::CreateRole { .. } if variant != "superuserIfNotExists" => {
                Some("role vendor primitives are PostgreSQL-only")
            }
            Op::AlterRole { .. } | Op::DropRole { .. } | Op::DropOwnedBy { .. } => {
                Some("role vendor primitives are PostgreSQL-only")
            }
            Op::Grant { .. } | Op::Revoke { .. } => {
                Some("grant vendor primitives are PostgreSQL-only")
            }
            Op::SetRls { .. } => Some("RLS vendor primitives are PostgreSQL-only"),
            Op::CreatePolicy { .. } | Op::DropPolicy { .. } => {
                Some("policy vendor primitives are PostgreSQL-only")
            }
            Op::CreateFunction { .. } | Op::DropFunction { .. } => {
                Some("function vendor primitives are PostgreSQL-only")
            }
            Op::Raw { .. } => Some("raw statements are PostgreSQL-only"),
            Op::CreateTrigger {
                timing,
                events,
                for_each,
                action,
                when,
                ..
            } => {
                if matches!(action, TriggerAction::ExecuteFunction { .. }) {
                    Some("MySQL has no CREATE TRIGGER EXECUTE FUNCTION form")
                } else if events.len() != 1 {
                    Some("MySQL CREATE TRIGGER accepts exactly one trigger event")
                } else if events
                    .iter()
                    .any(|event| matches!(event, TriggerEvent::Truncate))
                {
                    Some("MySQL has no TRUNCATE trigger event")
                } else if matches!(timing, TriggerTiming::InsteadOf) {
                    Some("MySQL does not support INSTEAD OF triggers")
                } else if matches!(for_each, ForEach::Statement) {
                    Some("MySQL triggers are row-level only")
                } else if when.is_some() {
                    Some("MySQL triggers do not support WHEN predicates")
                } else if let TriggerAction::Body { statements } = action {
                    statements
                        .iter()
                        .any(|statement| {
                            matches!(
                                statement,
                                TriggerStmt::Raise {
                                    level: RaiseLevel::Ignore,
                                    ..
                                }
                            )
                        })
                        .then_some("MySQL cannot render RAISE IGNORE")
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn feature_support_refusal(&self, feature: FeatureSupportKey) -> Option<&'static str> {
        match feature {
            FeatureSupportKey::TableLevelForeignKey
            | FeatureSupportKey::TableLevelUnique
            | FeatureSupportKey::CompositeForeignKey
            | FeatureSupportKey::NonIdForeignKey
            | FeatureSupportKey::InsertOnConflict
            | FeatureSupportKey::TriggerBody
            | FeatureSupportKey::RawViewBody
            | FeatureSupportKey::PartitionDdl => None,
            FeatureSupportKey::ExistenceGuardProbe => Some(
                "MySQL catalog probes enforce presence-only and non-column-type decisions, but any decision requiring column-type equality is refused until modifier-preserving equality is implemented",
            ),
            FeatureSupportKey::ForeignKeyNoLocalColumn => {
                Some("foreign keys need at least one local column")
            }
            FeatureSupportKey::ExpressionIndex => {
                Some("createIndex expression elements are not supported on MySQL")
            }
            FeatureSupportKey::PartialIndex => Some("MySQL does not support partial indexes"),
            FeatureSupportKey::AlterColumnUsing => Some(
                "setColumnType.using expression rendering is deferred in the current engine",
            ),
            FeatureSupportKey::RenameColumnGuard => Some(
                "renameColumn ifExists guards cannot be attributed to a single migration unit today",
            ),
            FeatureSupportKey::CreateOrReplaceMaterializedView => Some(
                "Postgres has no CREATE OR REPLACE MATERIALIZED VIEW and the other dialects have no materialized views",
            ),
            FeatureSupportKey::TriggerMultipleEvents => {
                Some("MySQL CREATE TRIGGER accepts exactly one trigger event")
            }
            FeatureSupportKey::TriggerInsteadOfTiming => {
                Some("MySQL does not support INSTEAD OF triggers")
            }
            FeatureSupportKey::TriggerWhen => {
                Some("MySQL triggers do not support WHEN predicates")
            }
            FeatureSupportKey::TriggerRaiseIgnore => Some("MySQL cannot render RAISE IGNORE"),
            FeatureSupportKey::TableLevelCheckExpression => {
                Some("table-level CHECK expression rendering is PostgreSQL-only in the current engine")
            }
            FeatureSupportKey::SequenceDefault => {
                Some("nextval sequence defaults are PostgreSQL-only; SQLite/MySQL have no standalone sequences")
            }
            FeatureSupportKey::ConstraintNotValid => {
                Some("NOT VALID online constraint adoption (addForeignKey/addCheck { notValid }) is PostgreSQL-only; SQLite/MySQL have no NOT VALID / VALIDATE CONSTRAINT")
            }
            FeatureSupportKey::Sequence => {
                Some("standalone sequence objects are PostgreSQL-only in the current engine")
            }
            FeatureSupportKey::Comment => {
                Some("COMMENT ON is PostgreSQL-only in the current engine")
            }
            FeatureSupportKey::MaterializedView => {
                Some("materialized views are PostgreSQL-only in the current engine")
            }
            FeatureSupportKey::ExclusionConstraint => {
                Some("exclusion constraints are PostgreSQL-only in the current engine")
            }
            FeatureSupportKey::IndexInclude => {
                Some("index INCLUDE columns are PostgreSQL-only")
            }
            FeatureSupportKey::IndexStorageParams => {
                Some("index WITH storage parameters are PostgreSQL-only")
            }
            FeatureSupportKey::IndexOnly => Some("CREATE INDEX ON ONLY is PostgreSQL-only"),
            FeatureSupportKey::IndexNullsNotDistinct => {
                Some("UNIQUE INDEX NULLS NOT DISTINCT is PostgreSQL-only (PG 15+)")
            }
            FeatureSupportKey::IndexOpclass => {
                Some("per-column index operator classes are PostgreSQL-only")
            }
            FeatureSupportKey::IndexCollation => {
                Some("per-column index collations are PostgreSQL-only")
            }
            FeatureSupportKey::NonBtreeIndexMethod => {
                Some("non-btree index methods are unsupported on SQLite/MySQL")
            }
            FeatureSupportKey::TriggerExecuteFunction => {
                Some("SQLite/MySQL have no CREATE TRIGGER EXECUTE FUNCTION form")
            }
            FeatureSupportKey::TriggerTruncateEvent => {
                Some("SQLite/MySQL have no TRUNCATE trigger event")
            }
            FeatureSupportKey::TriggerStatementForEach => {
                Some("SQLite/MySQL triggers are row-level only")
            }
            FeatureSupportKey::RawSql => Some("raw statements are PostgreSQL-only"),
        }
    }

    /// THE single physical home of MySQL's backtick identifier spelling: double
    /// every embedded backtick, then wrap the result in backticks.
    ///
    /// It lives HERE, in the vendor's own module, and no longer in core. It used
    /// to be `schema::query::mysql_quote_ident` — `pub`, in the schema kernel —
    /// with this method reaching INTO core to get its own spelling: the exact
    /// mirror image of the ANSI arrangement, where `ansi_double_quote_ident` is
    /// `pub(in crate::render::backends)` so that core CANNOT reach it un-named.
    ///
    /// NOTHING WAS EMITTED WRONGLY BEFORE THE MOVE, and that is the point. Every
    /// call site named MySQL in the callee's name, so no vendor was unnamed, and
    /// `backend_modules_name_one_dialect` passed because the reach was by function
    /// name rather than by a dialect-enum literal. What it blocked was step 4: the
    /// future `zero-migrate-mysql` would have needed core at RUNTIME to spell its
    /// own identifier — the core-to-backend cycle the backend split exists to
    /// break, and the same shape as the extraction spike's finding that `-sqlite`
    /// needed `-postgres` to quote a trigger name.
    ///
    /// (This doc may not spell the dialect-enum literal, even in prose.
    /// `backend_modules_name_one_dialect`'s second half collects EVERY line in this
    /// file mentioning that path and demands the list be exactly the `DIALECT`
    /// const, so a comment is a carrier like any other line. An earlier draft of
    /// this paragraph named it and turned that test red, which is the rule working
    /// as intended.)
    ///
    /// MEASURED on the 1231-test `--lib` binary by neutering each candidate with a
    /// single appended token:
    ///
    /// | tree | neutered | red |
    /// |------|----------|-----|
    /// | before | this method | 21 |
    /// | before | `schema::query::mysql_quote_ident` | 30 |
    /// | after | this method | 54 |
    ///
    /// The two before-sets NEST rather than being disjoint — the inverse of the
    /// ANSI case, and exactly what "the backend delegates into core" means
    /// operationally: NOTHING reddened by neutering this method was missed by
    /// neutering core. The 9 in the difference (`render::lower::tests` ×5,
    /// `schema::query::hostile_identifier_quoting` ×3, and
    /// `policy_keyword_and_quoted_identifiers_are_quoted_in_injected_sql`) are the
    /// tests whose MySQL identifier bytes this backend had NO say in.
    ///
    /// AND THE 54 IS NOT A TYPO FOR THE 30 THAT WAS PREDICTED. Routing the two
    /// SECOND homes found during the change — `zero_migrate_mysql::backend::journal_sql`
    /// and `::backfill_sql`, each of which carried its own copy of the spelling and
    /// so could not be reached by the core neuter at all — added 24
    /// `zero_migrate_mysql::backend` tests on top of the 30. Nothing was lost at any
    /// step: the 54 is a strict superset of the 30, and the binary held at 1232
    /// tests throughout. The prediction was wrong because it was formed from the
    /// two sets measured FIRST, before those homes were known to exist.
    ///
    /// Like the two ANSI impls, this spells the bytes DIRECTLY rather than through
    /// the `*_for_dialect` seam its sibling methods use: it IS this dialect's
    /// `quote_ident`, so routing through the dispatch would recurse.
    fn quote_ident(&self, ident: &str) -> String {
        format!("`{}`", ident.replace('`', "``"))
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
            },)?,
            t
        ))
    }

    fn cast_target(&self, target: CastTarget) -> &'static str {
        match target {
            CastTarget::Text => "char",
            CastTarget::Int => "signed",
            CastTarget::Real => "double",
            CastTarget::Boolean => "unsigned",
            CastTarget::Bytes => "binary",
            CastTarget::Uuid => "char(36)",
        }
    }

    fn placeholder(&self, _n: usize) -> String {
        "?".to_string()
    }

    fn inline_string_literal(&self, s: &str) -> String {
        // A UTF-8 hex literal, so `NO_BACKSLASH_ESCAPES` (present or absent)
        // cannot change either the value or the statement shape.
        format!("_utf8mb4 X'{}'", hex::encode(s.as_bytes()))
    }

    fn inline_decimal_literal(&self, d: &str) -> String {
        d.to_string()
    }

    fn inline_bytes_literal(&self, bytes: &[u8]) -> String {
        // MySQL requires expression defaults for BLOB columns. Parentheses
        // keep the same literal valid in defaults and ordinary expressions.
        format!("(X'{}')", hex::encode(bytes))
    }

    /// mysql2 carries a raw binary bind as text and would corrupt it, so the
    /// value goes over as canonical base64 and the server decodes it. The apply
    /// backend enforces the other half of this contract: a raw binary bind that
    /// reaches the MySQL session without a `FROM_BASE64` wrapper is refused.
    fn bind_bytes(&self, bytes: &[u8], push: &mut dyn FnMut(BindValue) -> String) -> String {
        let placeholder = push(BindValue::Text(
            zero_migrate_backend::spelling::base64_standard(bytes),
        ));
        format!("FROM_BASE64({placeholder})")
    }

    /// MySQL evaluates a SET list from left to right. Refuse cross-assignment
    /// reads rather than silently changing the simultaneous RHS semantics authors
    /// get on PostgreSQL and SQLite. A self-reference remains safe because its RHS
    /// is read before that column's sole assignment.
    fn validate_assignment_semantics(
        &self,
        op: &'static str,
        table: &str,
        set: &BTreeMap<String, IrValue>,
    ) -> Result<(), DmlError> {
        for (column, value) in set {
            let IrValue::Expr(expr) = value else {
                continue;
            };
            for referenced_column in set.keys() {
                if referenced_column != column && expr_references_column(expr, referenced_column)? {
                    return Err(DmlError::MySqlCrossAssignmentDependency {
                        op,
                        table: table.to_string(),
                        column: column.clone(),
                        referenced_column: referenced_column.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Render MySQL 8's closest safe native equivalent to a named conflict
    /// target. MySQL has no syntax for choosing one unique key. `DO NOTHING` is
    /// refused because neither a no-op update nor `INSERT IGNORE` has the same
    /// behavior.
    fn render_on_conflict(
        &self,
        request: OnConflictRenderRequest<'_>,
        ctx: &mut BindCtx<'_>,
    ) -> Result<String, DmlError> {
        let table = request.table;
        let qtable = request.qualified_table;
        let insert_columns = request.insert_columns;
        let oc = request.on_conflict;
        let qtarget_columns = request.quoted_target_columns;
        let Some(set) = oc.do_update.as_ref().filter(|set| !set.is_empty()) else {
            return Err(DmlError::MySqlConflictDoNothingNotExact);
        };

        for target in &oc.columns {
            if set.contains_key(target) {
                return Err(DmlError::MySqlConflictTargetUpdated {
                    table: table.to_string(),
                    column: target.clone(),
                });
            }
        }
        self.validate_assignment_semantics("onConflict.doUpdate", table, set)?;

        let row_alias = dml::escape_quote_ident_for_backend("zero-migrate-incoming", ctx.backend);
        let value_aliases = insert_columns
            .iter()
            .enumerate()
            .map(|(index, _)| {
                dml::escape_quote_ident_for_backend(
                    &format!("zero-migrate-value-{index}"),
                    ctx.backend,
                )
            })
            .collect::<Vec<_>>();

        let mut target_match = Vec::with_capacity(oc.columns.len());
        for (target, qtarget) in oc.columns.iter().zip(qtarget_columns) {
            let Some(index) = insert_columns.iter().position(|column| column == target) else {
                return Err(DmlError::MySqlConflictTargetNotInserted {
                    table: table.to_string(),
                    column: target.clone(),
                });
            };
            target_match.push(format!(
                "({qtable}.{qtarget} = {row_alias}.{})",
                value_aliases[index]
            ));
        }
        let target_match = target_match.join(" AND ");
        // Ordinary equality is intentional: MySQL UNIQUE indexes do not conflict
        // on NULL, so NULL/NULL must never establish an authored-target match. The
        // false branch is selected for both FALSE and NULL. Invalid JSON text is
        // specified by MySQL as a statement error independently of `sql_mode`;
        // division by zero is not, and under a permissive mode silently yields
        // NULL. CONCAT keeps IF's false branch string-compatible with legitimate
        // text, decimal, or binary assignment values. IF evaluates only the
        // selected branch, so a genuine authored-target collision never touches
        // the invalid document.
        let non_target_conflict_error =
            "CONCAT('', JSON_EXTRACT('zero-migrate conflict target mismatch', '$'))";

        let mut assigns = Vec::with_capacity(set.len());
        for (column, value) in set {
            let qcolumn = dml::quote_ident_for_backend("column", column, ctx.backend)?;
            let desired = dml::render_value_bound(value, ctx)?;
            assigns.push(format!(
                "{qcolumn} = IF({target_match}, {desired}, {non_target_conflict_error})"
            ));
        }

        Ok(format!(
            " AS {row_alias}({}) ON DUPLICATE KEY UPDATE {}",
            value_aliases.join(", "),
            assigns.join(", ")
        ))
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
            "DELETE FROM {} WHERE {} LIMIT {ph}",
            request.qualified_table, request.rendered_where
        ))
    }

    fn render_in_list(
        &self,
        expr: &str,
        elems: &[IrScalar],
        negated: bool,
        joiner: &str,
    ) -> Result<String, DmlError> {
        let rendered = elems
            .iter()
            .map(|elem| dml::render_in_list_elem_portable(elem, self))
            .collect::<Result<Vec<_>, _>>()?;
        let op = if negated { "NOT IN" } else { "IN" };
        Ok(format!("({expr} {op} ({}))", rendered.join(joiner)))
    }

    fn render_regex_match(&self, expr: &str, pattern: &str) -> Result<String, DmlError> {
        Ok(format!(
            "({expr} REGEXP {})",
            dml::in_list_text_literal(pattern, "regex pattern", self)?
        ))
    }

    fn render_storage_size(&self, _expr: &str) -> Result<String, DmlError> {
        Err(DmlError::UnrenderableExpr(
            "MySQL exposes no per-value stored-size function; use dialect({...}) to port"
                .to_string(),
        ))
    }

    fn render_extract(&self, field: ExtractField, expr: &str) -> String {
        match field {
            ExtractField::Dow => format!("(DAYOFWEEK({expr}) - 1)"),
            _ => format!(
                "EXTRACT({} FROM {expr})",
                dml::extract_field_name(field).to_ascii_uppercase()
            ),
        }
    }

    fn render_concat(&self, l: &str, r: &str) -> String {
        // MySQL's `||` is *logical OR* absent the non-default `PIPES_AS_CONCAT`
        // sql_mode, so a `Concat` rendered as `a || b` would silently corrupt to
        // a boolean.
        format!("CONCAT({l}, {r})")
    }

    fn render_distinct_from(&self, l: &str, r: &str) -> String {
        // MySQL has NO `IS DISTINCT FROM`. `<=>` is its NULL-safe equality
        // operator, so its negation is exactly the predicate.
        format!("(NOT ({l} <=> {r}))")
    }

    fn render_scalar_fn_override(&self, f: ScalarFn, args: &[String]) -> Option<String> {
        match f {
            // The portable `length()` intent is CHARACTER length (PG + SQLite
            // `length(text)`). MySQL's `LENGTH()` is *byte* length — wrong for
            // any multibyte string — so MySQL must use `CHAR_LENGTH()`.
            ScalarFn::Length => Some(format!("char_length({})", args.join(", "))),
            _ => None,
        }
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
        let d = self.inline_string_literal(delim);
        Ok(format!(
            "substring_index(substring_index({col_sql}, {d}, {n}), {d}, -1)"
        ))
    }

    fn synth_now(&self) -> String {
        "CURRENT_TIMESTAMP(6)".to_string()
    }

    fn uuid_v4(&self) -> String {
        // UUID() is UUIDv1 on MySQL and must never implement the UUIDv4
        // contract. Consume 16 random bytes across the canonical groups and
        // mask the relevant octets: version = 0100xxxx, variant = 10xxxxxx.
        // Parentheses make this valid in MySQL's expression-default grammar.
        "(lower(concat(hex(random_bytes(4)), '-', hex(random_bytes(2)), '-', \
         hex((ord(random_bytes(1)) & 15) | 64), hex(random_bytes(1)), '-', \
         hex((ord(random_bytes(1)) & 63) | 128), hex(random_bytes(1)), '-', \
         hex(random_bytes(6)))))"
            .to_string()
    }

    fn uuid_v7(&self) -> Result<String, DmlError> {
        Err(DmlError::UnrenderableExpr(
            "uuidV7 database generation is unsupported on MySQL".to_string(),
        ))
    }

    fn view_create_prefix(
        &self,
        materialized: bool,
        replace: bool,
    ) -> Result<String, IrLowerError> {
        // Kept as a SELF-check, not restored as a trait method. Core refuses a
        // materialized view before it ever resolves a renderer, but it refuses the
        // OP's `materialized`; the `down` of a `dropView` re-creates from the
        // recorded view's own `materialized`, which core never validated. A
        // backend asking its own `DIALECT` a capability question is legal here in
        // a way that core asking it through a registry was not.
        if materialized && !self.supports(Capability::MaterializedView) {
            return Err(IrLowerError::ViewUnsupported {
                kind: "materializedView",
                dialect: DIALECT,
            });
        }
        let mut create = String::from("CREATE ");
        if replace && self.supports(Capability::CreateOrReplaceView) {
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
            dml::quote_ident_checked_for_backend(eff_schema, &RENDERER).map_err(|e| {
                DmlError::InvalidIdentifier {
                    what: "schema",
                    value: e.value,
                }
            })?,
            dml::quote_bare_ident_for_backend("view", name, &RENDERER)?
        ))
    }

    fn render_table_ref(&self, table: &TableRef, eff_schema: &str) -> Result<String, IrLowerError> {
        let mut sql = {
            let schema = table.schema.as_deref().unwrap_or(eff_schema);
            format!(
                "{}.{}",
                dml::quote_ident_checked_for_backend(schema, &RENDERER).map_err(|e| {
                    DmlError::InvalidIdentifier {
                        what: "schema",
                        value: e.value,
                    }
                },)?,
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

    fn render_trigger_op(
        &self,
        op: &Op,
        eff_schema: &str,
    ) -> Result<Vec<zero_migrate_backend::vendor::VendorStatement>, IrLowerError> {
        match op {
            Op::CreateTrigger {
                name,
                table,
                timing,
                events,
                for_each,
                when,
                action,
                ..
            } => Ok(vec![render_mysql_trigger_create(
                name,
                table,
                *timing,
                events,
                *for_each,
                when.as_ref(),
                action,
                eff_schema,
            )?]),
            Op::DropTrigger {
                name,
                table,
                if_exists,
                ..
            } => {
                let qname = mysql_trigger_name(name, eff_schema)?;
                let mut up = String::from("DROP TRIGGER ");
                if if_exists.unwrap_or(false) {
                    up.push_str("IF EXISTS ");
                }
                up.push_str(&qname);
                Ok(vec![zero_migrate_backend::vendor::VendorStatement {
                    name: format!("drop_trigger_{name}_{table}"),
                    up,
                    down: None,
                }])
            }
            _ => Err(IrLowerError::UnsupportedOp(
                "non-trigger op routed to trigger renderer",
            )),
        }
    }

    /// MySQL declares no sequence capability. Core refuses before calling this,
    /// and this required body states the same fail-closed answer for direct users
    /// of the backend contract.
    fn render_sequence_op(
        &self,
        _op: &Op,
        _eff_schema: &str,
    ) -> Result<zero_migrate_backend::vendor::VendorStatement, IrLowerError> {
        Err(IrLowerError::SequenceUnsupported {
            kind: "sequence",
            dialect: DIALECT,
        })
    }

    /// MySQL represents authored enums/domains inline rather than as standalone
    /// schema objects. Core therefore never calls this required method for the
    /// shipping descriptor, and this body refuses any direct misuse explicitly.
    fn render_materialized_named_type_op(
        &self,
        _op: MaterializedNamedTypeOp<'_>,
    ) -> Result<zero_migrate_backend::vendor::VendorStatement, IrLowerError> {
        Err(IrLowerError::UnsupportedOp(
            "validated materialized named type unsupported by MySQL reached lower",
        ))
    }

    /// MySQL declares no `COMMENT ON` capability. Column/table comment syntax is
    /// a different DDL shape and must not be borrowed as an implicit fallback.
    fn render_comment_op(
        &self,
        _op: &Op,
        _eff_schema: &str,
    ) -> Result<zero_migrate_backend::vendor::VendorStatement, IrLowerError> {
        Err(IrLowerError::UnsupportedOp(
            "validated COMMENT ON unsupported dialect reached lower",
        ))
    }

    /// This vendor renders NO vendor ops, and that is written here rather than
    /// inherited.
    ///
    /// The sixteen privileged op kinds — roles, grants, RLS, policies, functions,
    /// extensions, schemas, `raw` — are every one of them `dialect_scope = PgOnly`.
    /// MySQL has no analogue for any of them, so there is nothing to render. The
    /// engine refuses earlier and more informatively (the lower seam checks
    /// `Capability::PostgresVendorPrimitives` and reports the op KIND), so nothing in
    /// the shipping paths reaches this.
    ///
    /// It is written out anyway because
    /// [`zero_migrate_backend::renderer::DmlRenderer`] gives no method a default
    /// body. An `Option` or an inherited refusal would let the NEXT backend acquire
    /// this posture by omitting something, which is the exact failure
    /// [`zero_migrate_backend::registry::BackendVendor::guard`] exists to prevent.
    fn render_vendor_op(
        &self,
        _op: &Op,
        _eff_schema: &str,
    ) -> Result<
        Vec<zero_migrate_backend::vendor::VendorStatement>,
        zero_migrate_backend::vendor::VendorError,
    > {
        Err(zero_migrate_backend::vendor::VendorError::VendorOpsUnsupported(DIALECT))
    }
}

fn mysql_trigger_name(name: &str, eff_schema: &str) -> Result<String, IrLowerError> {
    Ok(format!(
        "{}.{}",
        dml::quote_ident_checked_for_backend(eff_schema, &RENDERER).map_err(|e| {
            DmlError::InvalidIdentifier {
                what: "schema",
                value: e.value,
            }
        },)?,
        dml::quote_bare_ident_for_backend("trigger", name, &RENDERER)?
    ))
}

fn mysql_trigger_table_ref(
    table: &str,
    schema: Option<&str>,
    eff_schema: &str,
) -> Result<String, IrLowerError> {
    let schema = schema.unwrap_or(eff_schema);
    Ok(format!(
        "{}.{}",
        dml::quote_ident_checked_for_backend(schema, &RENDERER).map_err(|e| {
            DmlError::InvalidIdentifier {
                what: "schema",
                value: e.value,
            }
        },)?,
        dml::quote_bare_ident_for_backend("table", table, &RENDERER)?
    ))
}

#[allow(clippy::too_many_arguments)]
fn render_mysql_trigger_create(
    name: &str,
    table: &str,
    timing: TriggerTiming,
    events: &[TriggerEvent],
    for_each: ForEach,
    when: Option<&zero_migrate_ir::expr::Expr>,
    action: &TriggerAction,
    eff_schema: &str,
) -> Result<zero_migrate_backend::vendor::VendorStatement, IrLowerError> {
    if events.is_empty() {
        return Err(IrLowerError::Vendor(
            zero_migrate_backend::vendor::VendorError::EmptyList {
                what: "trigger events",
            },
        ));
    }
    if events.len() != 1 {
        return Err(IrLowerError::TriggerUnsupported {
            kind: "triggerMultipleEvents",
            dialect: DIALECT,
        });
    }
    if matches!(events[0], TriggerEvent::Truncate) {
        return Err(IrLowerError::TriggerUnsupported {
            kind: "triggerEventTruncate",
            dialect: DIALECT,
        });
    }
    if matches!(timing, TriggerTiming::InsteadOf) {
        return Err(IrLowerError::TriggerUnsupported {
            kind: "triggerTimingInsteadOf",
            dialect: DIALECT,
        });
    }
    if matches!(for_each, ForEach::Statement) {
        return Err(IrLowerError::TriggerUnsupported {
            kind: "forEachStatement",
            dialect: DIALECT,
        });
    }
    if when.is_some() {
        return Err(IrLowerError::TriggerUnsupported {
            kind: "triggerWhen",
            dialect: DIALECT,
        });
    }
    let TriggerAction::Body { statements } = action else {
        return Err(IrLowerError::TriggerUnsupported {
            kind: "executeFunction",
            dialect: DIALECT,
        });
    };
    if statements.is_empty() {
        return Err(IrLowerError::Vendor(
            zero_migrate_backend::vendor::VendorError::EmptyList {
                what: "trigger body statements",
            },
        ));
    }

    let qname = mysql_trigger_name(name, eff_schema)?;
    let qtable = mysql_trigger_table_ref(table, None, eff_schema)?;
    let body: Result<Vec<_>, _> = statements
        .iter()
        .map(|stmt| render_mysql_trigger_stmt(stmt, eff_schema))
        .collect();
    let mut up = format!(
        "CREATE TRIGGER {qname} {} {} ON {qtable} FOR EACH ROW BEGIN ",
        timing.as_sql(),
        events[0].as_sql()
    );
    up.push_str(
        &body?
            .into_iter()
            .map(|s| format!("{s};"))
            .collect::<Vec<_>>()
            .join(" "),
    );
    up.push_str(" END");
    Ok(zero_migrate_backend::vendor::VendorStatement {
        name: format!("create_trigger_{name}_{table}"),
        up,
        down: Some(format!("DROP TRIGGER IF EXISTS {qname}")),
    })
}

fn render_mysql_trigger_stmt(stmt: &TriggerStmt, eff_schema: &str) -> Result<String, IrLowerError> {
    match stmt {
        TriggerStmt::Insert {
            table,
            columns,
            rows,
            schema,
        } => {
            if columns.is_empty() {
                return Err(IrLowerError::DmlAssemble(
                    zero_migrate_backend::dml::DmlError::MalformedInsert {
                        table: table.clone(),
                        reason: "no columns".to_string(),
                    },
                ));
            }
            if rows.is_empty() {
                return Err(IrLowerError::DmlAssemble(
                    zero_migrate_backend::dml::DmlError::MalformedInsert {
                        table: table.clone(),
                        reason: "no rows".to_string(),
                    },
                ));
            }
            let qtable = mysql_trigger_table_ref(table, schema.as_deref(), eff_schema)?;
            let qcols: Result<Vec<_>, _> = columns
                .iter()
                .map(|c| dml::quote_bare_ident_for_backend("column", c, &RENDERER))
                .collect();
            let mut groups = Vec::with_capacity(rows.len());
            for (ri, row) in rows.iter().enumerate() {
                if row.len() != columns.len() {
                    return Err(IrLowerError::DmlAssemble(
                        zero_migrate_backend::dml::DmlError::MalformedInsert {
                            table: table.clone(),
                            reason: format!(
                                "row {ri} has {} value(s) but {} column(s) were named",
                                row.len(),
                                columns.len()
                            ),
                        },
                    ));
                }
                let vals: Result<Vec<_>, _> = row
                    .iter()
                    .map(|value| {
                        zero_migrate_backend::dml::render_value_inline_for_backend(value, &RENDERER)
                    })
                    .collect();
                groups.push(format!("({})", vals?.join(", ")));
            }
            Ok(format!(
                "INSERT INTO {qtable} ({}) VALUES {}",
                qcols?.join(", "),
                groups.join(", ")
            ))
        }
        TriggerStmt::Update {
            table,
            set,
            r#where,
            schema,
        } => {
            if set.is_empty() {
                return Err(IrLowerError::DmlAssemble(
                    zero_migrate_backend::dml::DmlError::EmptySet {
                        op: "update",
                        table: table.clone(),
                    },
                ));
            }
            let qtable = mysql_trigger_table_ref(table, schema.as_deref(), eff_schema)?;
            let mut assigns = Vec::with_capacity(set.len());
            for (col, rhs) in set {
                assigns.push(format!(
                    "{} = {}",
                    dml::quote_bare_ident_for_backend("column", col, &RENDERER)?,
                    zero_migrate_backend::dml::render_value_inline_for_backend(rhs, &RENDERER)?
                ));
            }
            let mut sql = format!("UPDATE {qtable} SET {}", assigns.join(", "));
            if let Some(pred) = r#where {
                sql.push_str(&format!(
                    " WHERE {}",
                    zero_migrate_backend::dml::render_expr_inline_for_backend(pred, &RENDERER)?
                ));
            }
            Ok(sql)
        }
        TriggerStmt::Delete {
            table,
            r#where,
            limit,
            schema,
        } => {
            let qtable = mysql_trigger_table_ref(table, schema.as_deref(), eff_schema)?;
            let pred =
                zero_migrate_backend::dml::render_expr_inline_for_backend(r#where, &RENDERER)?;
            Ok(match limit {
                None => format!("DELETE FROM {qtable} WHERE {pred}"),
                Some(n) => format!("DELETE FROM {qtable} WHERE {pred} LIMIT {}", n.get()),
            })
        }
        // MySQL forbids a trigger body that RETURNS A RESULT SET, and it says so when
        // the `CREATE TRIGGER` is executed, not when the trigger fires: MySQL 8.4.11
        // answers `[0A000] Not allowed to return a result set from a trigger`. Before
        // this arm existed, the plan cleared validate, the guard and lower, and died
        // there - the one outcome class `dialect_conformance_live.rs` exists to catch,
        // and the row that measured it is `createTrigger/bodySimple` on MySQL.
        //
        // Refused rather than rewritten: `SELECT <expr>` has no result-set-free MySQL
        // spelling, because `SELECT ... INTO` needs a declared target and this closed
        // trigger-body IR has no way to name one. The refusal sits HERE, beside
        // `RAISE IGNORE`, and NOT in `dialect-support.toml`, because MySQL body
        // triggers work - an `INSERT` / `UPDATE` / `DELETE` body applies and fires.
        // Declaring the whole `bodySimple` cell unsupported would refuse all of them
        // to reject this one statement. Pinned, with both over-refusal controls, by
        // `tests/refusals/mysql_trigger_body_cannot_return_a_result_set.rs`.
        TriggerStmt::Select { .. } => Err(IrLowerError::TriggerUnsupported {
            kind: "selectStatement",
            dialect: DIALECT,
        }),
        TriggerStmt::Raise {
            level: RaiseLevel::Ignore,
            ..
        } => Err(IrLowerError::TriggerUnsupported {
            kind: "raiseIgnore",
            dialect: DIALECT,
        }),
        TriggerStmt::Raise {
            level: _,
            message,
            errcode,
        } => {
            let errcode = errcode.as_deref().unwrap_or("45000");
            Ok(format!(
                "SIGNAL SQLSTATE {} SET MESSAGE_TEXT = {}",
                zero_migrate_backend::dml::mysql_grammar_string_literal(errcode),
                RENDERER.inline_string_literal(message)
            ))
        }
    }
}
