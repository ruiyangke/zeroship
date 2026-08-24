//! SQLite SQL spelling. The future `zero-migrate-sqlite`.
//!
//! The trigger spelling at the bottom of this file arrived here in step 3 of
//! `docs/proposals/pluggable-backends.md`, from `render::lower`. It is a DIRECTORY
//! MOVE and nothing else: same functions, same bytes emitted, one const renamed. It
//! was the last SQLite render path living in the 17k-line core lowerer, and its own
//! doc had already said so — `backends/sqlite.rs`'s `render_trigger_op` carried a
//! note calling the delegation "a POINTER to work that `lower.rs`'s own step-3 pass
//! has to finish, not a boundary that is done". This is that pass. MySQL's trigger
//! spelling was the worked example of where it lands.

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
use zero_migrate_ir::expr::{AggFunc, CastTarget, Duration, Expr, ExtractField, ScalarFn};
use zero_migrate_ir::ir::{
    ForEach, IrScalar, IrValue, Op, RaiseLevel, TableRef, TriggerAction, TriggerEvent, TriggerStmt,
};
use zero_migrate_ir::validate::{
    ExprDialectFeature, ExprDialectRejection, ExprDialectValidator, UnsupportedKind,
    CODE_DIALECT_UNSUPPORTED, CODE_EXPR_NOT_PORTABLE, CODE_UNSUPPORTED,
};

const SPLIT_PART_MAX_N: i64 = 8;

/// This backend's numbered placeholder spelling.
///
/// `pub(crate)` rather than private because the batched-backfill executor
/// (`crate::backend::backfill_sql`) assembles its own per-batch statements and has to
/// bind through the same spelling the one-shot assembler reaches via
/// [`SqliteDmlRenderer::placeholder`]. Two paths, one spelling, in the crate that owns
/// it — it used to be `zero_migrate_backend::dml::sqlite_placeholder`, a vendor name in
/// the neutral contract whose only two callers were both here.
pub(crate) fn placeholder(n: usize) -> String {
    format!("?{n}")
}

// This module's vendor identity, read from `crate::DIALECT` — this module names no
// dialect literal of its own. See `render/backends/mod.rs`.
//
// # It absorbed the trigger renderer's const on the way in
//
// `render_sqlite_trigger_op` and the two helpers below it named this vendor
// nineteen times between them while they lived in `render::lower`: thirteen
// capability and inline-render arguments that were already right, and six identifier
// quotes that were NOT. Those six called the PostgreSQL-pinned
// `dml::quote_bare_ident`, so every identifier in a rendered SQLite trigger was
// spelled by `PostgresDmlRenderer::quote_ident` — correct only because both vendors
// spell an identifier `"x"`, and a hard blocker on extracting a `zero-migrate-sqlite`
// crate that does not need `zero-migrate-postgres` at RUNTIME. A crate-extraction
// spike proved the reach was live rather than theoretical: it rendered a
// `createTrigger` from inside the extracted crate and got PostgreSQL's marker back
// in the SQLite trigger SQL.
//
// Routing those six through `quote_bare_ident_for_backend` was the fix; folding the
// other thirteen into a single name is what made it stay fixed. That name was
// `SQLITE_TRIGGER_DIALECT`, a `lower.rs`-local stand-in for the rule this file
// already obeyed, and its whole purpose was to make the eventual move of those three
// functions a RELOCATION rather than an edit. It worked: the move renamed one
// identifier and touched nothing else, and `DIALECT` is what it was renamed to.
//
// Pinned by `tests/dialect_matrix/sqlite_trigger_quoting_reaches_postgres.rs`, whose
// count went 6 → 0 when the fix landed and whose subject-anchor followed the three
// functions here.
use crate::DIALECT;

#[derive(Debug)]
pub(super) struct SqliteDmlRenderer;

pub(super) static RENDERER: SqliteDmlRenderer = SqliteDmlRenderer;

fn unsupported_expr(name: &'static str) -> ExprDialectRejection {
    unsupported_expr_owned(name.to_string())
}

fn unsupported_expr_owned(name: String) -> ExprDialectRejection {
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

/// The truthful remedy for an expression that is out of this backend's envelope,
/// stated ONCE so the three messages below cannot drift apart.
///
/// # What the old text promised, and why none of it was true
///
/// These three rejections advised the operator to "mark the migration PG-only
/// (`dialect_scope=PgOnly`)". `PgOnly` names no variant: the pinned arm is
/// `DialectScope::Only(DialectId)` and has been since the enum stopped growing one
/// variant per vendor, so a MySQL-only artifact could describe itself. `dialect_scope`
/// names no authorable field either — it is DERIVED from the op list at lowering,
/// precisely so a declared reach can never disagree with the ops — so there was
/// nothing for an author to set. And it spelled another vendor's product name in a
/// message this backend emits about itself.
///
/// What an author actually does is put the expression in a `dialect({ ... })` leg.
/// The leg set IS the pin: a set covering one registered backend makes that backend
/// the plan's whole measured reach, and every other deploy target is then refused
/// before a step runs.
///
/// The dialect named is this crate's own, read from `DIALECT`.
fn or_give_it_a_dialect_leg(instead: &str) -> String {
    format!(
        "{instead}, or move the expression into a dialect({{ ... }}) leg — a leg set \
         covering ONE backend pins the migration to that dialect, and every other \
         deploy target is refused before anything is applied (leaving {} uncovered is \
         how this expression stops being asked of it)",
        DIALECT.as_str()
    )
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

impl ExprDialectValidator for SqliteDmlRenderer {
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
            ExprDialectFeature::ConcatWs {
                delimiter: Expr::Literal { .. },
            } => {
                Ok(())
            }
            ExprDialectFeature::ConcatWs { delimiter } => Err(ExprDialectRejection {
                code: CODE_EXPR_NOT_PORTABLE,
                kind: None,
                reason: format!(
                    "c.fn.concatWs delimiter must be a literal on SQLite (a runtime/computed delimiter is not portable — the NULL-skip head-trim needs a fixed delimiter length); got {delimiter:?}"
                ),
                suggested_fix: Some(or_give_it_a_dialect_leg(
                    "pass a string literal as the concatWs delimiter",
                )),
            }),
            ExprDialectFeature::SplitPart {
                delimiter,
                part_index,
            } => {
                let bytes = delimiter.as_bytes();
                if bytes.len() != 1 || bytes[0] >= 0x80 {
                    return Err(ExprDialectRejection {
                        code: CODE_EXPR_NOT_PORTABLE,
                        kind: None,
                        reason: format!(
                            "c.fn.splitPart delimiter must be a single ASCII character (one byte, code point < 0x80); got {delimiter:?}"
                        ),
                        suggested_fix: Some(or_give_it_a_dialect_leg(
                            "use a single-ASCII delimiter with 1<=n<=8, restructure to stay in-envelope (split into <=8 parts)",
                        )),
                    });
                }
                if part_index > SPLIT_PART_MAX_N {
                    return Err(ExprDialectRejection {
                        code: CODE_EXPR_NOT_PORTABLE,
                        kind: None,
                        reason: format!(
                            "c.fn.splitPart part index n must be <= {SPLIT_PART_MAX_N} (the proven inline-unroll bound); got {part_index}"
                        ),
                        suggested_fix: Some(or_give_it_a_dialect_leg(
                            "use a single-ASCII delimiter with 1<=n<=8, restructure to stay in-envelope (split into <=8 parts)",
                        )),
                    });
                }
                Ok(())
            }
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
            ExprDialectFeature::RegexMatch => Err(ExprDialectRejection {
                code: CODE_DIALECT_UNSUPPORTED,
                kind: Some(UnsupportedKind::Expr),
                reason: "regex match is supported on PostgreSQL and MySQL, but SQLite has no stock REGEXP"
                    .to_string(),
                suggested_fix: Some(
                    "use dialect({ postgres: ..., sqlite: ..., mysql: ... }) to provide an explicit SQLite leg, or avoid regex on SQLite"
                        .to_string(),
                ),
            }),
            ExprDialectFeature::StorageSize => Err(unsupported_expr("storageSize")),
            // Answered by asking this backend's OWN renderer, so the validator
            // and the render seam cannot drift apart about which parts exist here.
            ExprDialectFeature::Extract(field) => DmlRenderer::render_extract(self, field, "x")
                .map(|_| ())
                .map_err(|_| {
                    unsupported_expr_owned(format!(
                        "the {} extract field",
                        dml::extract_field_name(field)
                    ))
                }),
            ExprDialectFeature::Interval => Err(unsupported_expr("PG interval literal")),
        }
    }
}

impl DmlRenderer for SqliteDmlRenderer {
    fn dialect(&self) -> DialectId {
        DIALECT
    }

    fn expr_validator(&self) -> &dyn ExprDialectValidator {
        self
    }

    fn descriptor(&self) -> &'static BackendDescriptor {
        &crate::descriptor::SQLITE_DESCRIPTOR
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
        true
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
            Op::CreateIndex { .. } => {
                Some("createIndex BRIN/INCLUDE/WITH/ONLY features are unsupported on SQLite")
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
                "SQLite has no ALTER COLUMN: this change is applied only by the declarative \
                 differ's 12-step table rebuild, which needs the whole table definition — \
                 express it as a schema change rather than a stand-alone op",
            ),
            Op::DropConstraint { .. } => Some(
                "SQLite cannot add or drop a table constraint in place: it is applied only by \
                 the declarative differ's 12-step table rebuild, which needs the whole table \
                 definition — express it as a schema change rather than a stand-alone op",
            ),
            Op::AddConstraint { .. } if variant == "unique" => Some(
                "SQLite cannot add or drop a table constraint in place: it is applied only by \
                 the declarative differ's 12-step table rebuild, which needs the whole table \
                 definition — express it as a schema change rather than a stand-alone op",
            ),
            Op::ValidateConstraint { .. } => Some(
                "VALIDATE CONSTRAINT is PostgreSQL-only online constraint adoption (the second \
                 half of NOT VALID → VALIDATE CONSTRAINT); SQLite and MySQL have no such \
                 statement, so there is nothing to validate",
            ),
            Op::AddConstraint { .. } if variant == "check" => Some(
                "addConstraint(check) expression rendering is PostgreSQL-only in the current engine",
            ),
            Op::AddConstraint { .. } if variant == "exclusion" => {
                Some("exclusion constraints are PostgreSQL-only in the current engine")
            }
            Op::AddConstraint { .. } if variant == "fkNotValid" => Some(
                "NOT VALID online constraint adoption (addForeignKey { notValid }) is PostgreSQL-only in the current engine",
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
                events,
                for_each,
                action,
                ..
            } => {
                if matches!(action, TriggerAction::ExecuteFunction { .. }) {
                    Some("SQLite has no CREATE TRIGGER EXECUTE FUNCTION form")
                } else if events.len() != 1 {
                    Some("SQLite CREATE TRIGGER accepts exactly one trigger event")
                } else if events
                    .iter()
                    .any(|event| matches!(event, TriggerEvent::Truncate))
                {
                    Some("SQLite has no TRUNCATE trigger event")
                } else if matches!(for_each, ForEach::Statement) {
                    Some("SQLite triggers are row-level only")
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn feature_support_refusal(&self, feature: FeatureSupportKey) -> Option<&'static str> {
        match feature {
            FeatureSupportKey::PartialIndex
            | FeatureSupportKey::ExpressionIndex
            | FeatureSupportKey::TableLevelForeignKey
            | FeatureSupportKey::CompositeForeignKey
            | FeatureSupportKey::NonIdForeignKey
            | FeatureSupportKey::ExistenceGuardProbe
            | FeatureSupportKey::InsertOnConflict
            | FeatureSupportKey::TriggerInsteadOfTiming
            | FeatureSupportKey::TriggerBody
            | FeatureSupportKey::TriggerWhen
            | FeatureSupportKey::TriggerRaiseIgnore
            | FeatureSupportKey::RawViewBody
            | FeatureSupportKey::PartitionDdl => None,
            FeatureSupportKey::ForeignKeyNoLocalColumn => {
                Some("foreign keys need at least one local column")
            }
            FeatureSupportKey::TableLevelUnique => Some(
                "SQLite createTable table-level unique constraints are not threaded into the emitter",
            ),
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
                Some("SQLite CREATE TRIGGER accepts exactly one trigger event")
            }
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

    fn quote_ident(&self, ident: &str) -> String {
        zero_migrate_backend::spelling::ansi_double_quote_ident(ident)
    }

    fn qualify_table(&self, _project_schema: &str, table: &str) -> Result<String, DmlError> {
        dml::quote_bare_ident_for_backend("table", table, &RENDERER)
    }

    fn cast_target(&self, target: CastTarget) -> &'static str {
        match target {
            CastTarget::Text => "text",
            CastTarget::Int => "integer",
            CastTarget::Real => "real",
            CastTarget::Boolean => "integer",
            CastTarget::Bytes => "blob",
            CastTarget::Uuid => "text",
        }
    }

    fn placeholder(&self, n: usize) -> String {
        placeholder(n)
    }

    fn inline_string_literal(&self, s: &str) -> String {
        dml::sql_string_literal(s)
    }

    fn inline_decimal_literal(&self, d: &str) -> String {
        // SQLite stores an exact decimal losslessly only as TEXT.
        dml::sql_string_literal(d)
    }

    fn inline_bytes_literal(&self, bytes: &[u8]) -> String {
        format!("X'{}'", hex::encode(bytes))
    }

    /// rusqlite binds a byte vector natively, so SQLite needs NO decoder around
    /// the placeholder and NO base64 detour — the bytes stay bytes end to end.
    fn bind_bytes(&self, bytes: &[u8], push: &mut dyn FnMut(BindValue) -> String) -> String {
        push(BindValue::Bytes(bytes.to_vec()))
    }

    fn validate_assignment_semantics(
        &self,
        _op: &'static str,
        _table: &str,
        _set: &BTreeMap<String, IrValue>,
    ) -> Result<(), DmlError> {
        // SQLite evaluates every assignment RHS from the row before the SET list,
        // so cross-assignment reads already have the authored simultaneous
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
                // BTreeMap ⇒ deterministic column order (canonical).
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
        let table = request.table;
        let qtable = request.qualified_table;
        let w = request.rendered_where;
        let n = request.limit;
        let identity_columns = request
            .catalog_identity_columns
            .filter(|columns| !columns.is_empty())
            .ok_or_else(|| DmlError::LimitedDeleteNeedsUniqueIdentity {
                dialect: DIALECT,
                table: table.to_string(),
            })?;
        let quoted_identity: Result<Vec<_>, _> = identity_columns
            .iter()
            .map(|column| {
                dml::quote_ident_checked_for_backend(column, self).map_err(|_| {
                    DmlError::InvalidIdentifier {
                        what: "catalog identity column",
                        value: column.clone(),
                    }
                })
            })
            .collect();
        let quoted_identity = quoted_identity?;
        let selected_identity = quoted_identity.join(", ");
        let compared_identity = if quoted_identity.len() == 1 {
            selected_identity.clone()
        } else {
            format!("({selected_identity})")
        };
        let ph = ctx.push_bind(BindValue::Int(i64::try_from(n).unwrap_or(i64::MAX)));
        Ok(format!(
            "DELETE FROM {qtable} WHERE {compared_identity} IN \
             (SELECT {selected_identity} FROM {qtable} WHERE {w} LIMIT {ph})"
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

    fn render_regex_match(&self, _expr: &str, _pattern: &str) -> Result<String, DmlError> {
        Err(DmlError::UnrenderableExpr(
            "regex is not supported on SQLite (no stock REGEXP); use dialect({...}) to port"
                .to_string(),
        ))
    }

    fn render_storage_size(&self, _expr: &str) -> Result<String, DmlError> {
        Err(DmlError::UnrenderableExpr(
            "SQLite exposes no per-value stored-size function; use dialect({...}) to port"
                .to_string(),
        ))
    }

    fn render_interval(&self, _duration: &Duration) -> Result<String, DmlError> {
        Err(DmlError::UnrenderableExpr(
            "SQLite has no interval type or literal (date maths goes through datetime() modifiers); use dialect({...}) to port"
                .to_string(),
        ))
    }

    fn render_extract(&self, field: ExtractField, expr: &str) -> Result<String, DmlError> {
        // SQLite has no EXTRACT at all; every part it can answer goes through a
        // strftime format, which is exactly why the keyword table in the shared
        // crate is an option rather than an instruction.
        let fmt = match field {
            ExtractField::Year => "%Y",
            ExtractField::Month => "%m",
            ExtractField::Day => "%d",
            ExtractField::Hour => "%H",
            ExtractField::Minute => "%M",
            ExtractField::Dow => "%w",
            ExtractField::Second
            | ExtractField::Doy
            | ExtractField::Epoch
            | ExtractField::Quarter
            | ExtractField::Week
            | ExtractField::Isodow
            | ExtractField::Isoyear
            | ExtractField::Century
            | ExtractField::Decade
            | ExtractField::Millennium
            | ExtractField::Microseconds
            | ExtractField::Milliseconds
            | ExtractField::Timezone
            | ExtractField::TimezoneHour
            | ExtractField::TimezoneMinute => {
                return Err(DmlError::UnrenderableExpr(format!(
                    "SQLite has no proven strftime form for the {} extract field; use dialect({{...}}) to port",
                    dml::extract_field_name(field)
                )))
            }
        };
        Ok(format!("CAST(strftime('{fmt}', {expr}) AS INTEGER)"))
    }

    fn render_concat(&self, l: &str, r: &str) -> String {
        format!("({l} || {r})")
    }

    fn render_distinct_from(&self, l: &str, r: &str) -> String {
        format!("({l} IS DISTINCT FROM {r})")
    }

    fn render_scalar_fn_override(&self, f: ScalarFn, args: &[String]) -> Option<String> {
        // SQLite exposes floor()/ceil() only when it was built with the optional
        // math extension. Lower both operations to core SQL so the portable DSL
        // behaves the same on every supported SQLite build. The builder enforces
        // one argument; a malformed hand-authored arity falls back to the generic
        // spelling, which validation rejects before render.
        match f {
            ScalarFn::Floor if args.len() == 1 => {
                let arg = &args[0];
                Some(format!(
                    "(CASE WHEN {arg} >= 9223372036854775808.0 OR {arg} <= -9223372036854775808.0 THEN {arg} ELSE CAST({arg} AS INTEGER) - (CAST({arg} AS INTEGER) > {arg}) END)"
                ))
            }
            ScalarFn::Ceil if args.len() == 1 => {
                let arg = &args[0];
                Some(format!(
                    "(CASE WHEN {arg} >= 9223372036854775808.0 OR {arg} <= -9223372036854775808.0 THEN {arg} ELSE CAST({arg} AS INTEGER) + (CAST({arg} AS INTEGER) < {arg}) END)"
                ))
            }
            _ => None,
        }
    }

    fn render_is_true(&self, operand: &str) -> String {
        // SQLite has no native boolean type (values are 0/1) and rejects the
        // `IS TRUE` / `IS FALSE` predicates at apply.
        format!("({operand} = 1)")
    }

    fn render_is_false(&self, operand: &str) -> String {
        format!("({operand} = 0)")
    }

    fn render_concat_ws(&self, rendered: &[String]) -> String {
        // rendered[0] is the delimiter; rendered[1..] are the values.
        // NULL-skipping join: coalesce each value with '' joined by the
        // delimiter would re-introduce empty fields; the pinned SQLite shape
        // for concat_ws is a fold that skips NULLs. We use the standard
        // equivalent: trim away the delimiter that a leading NULL would leave.
        // For the bounded value count we emit the explicit
        // `substr(<acc>, len(delim)+1)` head-trim of a `||`-fold where each
        // value contributes `delim || value` only when not NULL.
        let delim = &rendered[0];
        let values = &rendered[1..];
        // acc = '' ; for each v: acc = acc || (case when v is null then '' else delim||v end)
        // then strip the single leading delim.
        let mut fold = String::from("''");
        for v in values {
            fold = format!(
                "({fold} || (CASE WHEN ({v}) IS NULL THEN '' ELSE ({delim}) || ({v}) END))"
            );
        }
        // Strip the leading delimiter (length of the delim literal). Using
        // substr with instr-free fixed-prefix removal is only correct when the
        // delim is a fixed literal; concatWs's delim is a Literal by the op
        // shape, so this holds. We strip `length(delim)` leading chars.
        format!("substr({fold}, length({delim}) + 1)")
    }

    fn render_split_part(&self, col_sql: &str, delim: &str, n: i64) -> Result<String, DmlError> {
        // ENVELOPE (SQLite only): single-ASCII-byte delim, 1 <= n <= MAX_N.
        let bytes = delim.as_bytes();
        if bytes.len() != 1 || bytes[0] >= 0x80 {
            return Err(DmlError::UnrenderableExpr(format!(
                "c.fn.splitPart delimiter must be a single ASCII character (one byte, \
                 code point < 0x80) to lower portably on SQLite; got {delim:?}"
            )));
        }
        if n > SPLIT_PART_MAX_N {
            return Err(DmlError::UnrenderableExpr(format!(
                "c.fn.splitPart part index n must be in 1..={SPLIT_PART_MAX_N} \
                 (the proven inline-unroll bound) to lower portably on SQLite; got {n}"
            )));
        }
        let dc = char::from(bytes[0]);
        // Single-ASCII delimiter as an inline SQL string literal (`'` -> `''`).
        let d = if dc == '\'' {
            "''''".to_string()
        } else {
            format!("'{dc}'")
        };
        // cur0 = (col || 'd') - the sentinel-terminated string.
        let mut cur = format!("({col_sql} || {d})");
        // cur_i = substr(cur_i-1, instr(cur_i-1, 'd') + 1), i = 1..n-1.
        for _ in 1..n {
            cur = format!("substr({cur}, instr({cur}, {d}) + 1)");
        }
        // result = substr(cur_n-1, 1, instr(cur_n-1, 'd') - 1).
        Ok(format!("substr({cur}, 1, instr({cur}, {d}) - 1)"))
    }

    fn synth_now(&self) -> String {
        "CURRENT_TIMESTAMP".to_string()
    }

    fn uuid_v4(&self) -> String {
        // SQLite has no native UUID generator. Build each canonical group from
        // random bytes, pin the version nibble to `4`, and choose the variant
        // nibble from `8..b` (binary `10xx`). The outer lower() canonicalizes
        // hex(randomblob(...)), whose native spelling is uppercase.
        "(lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || \
         substr(hex(randomblob(2)), 2, 3) || '-' || \
         substr('89ab', ((instr('0123456789ABCDEF', \
         substr(hex(randomblob(1)), 1, 1)) - 1) % 4) + 1, 1) || \
         substr(hex(randomblob(2)), 2, 3) || '-' || hex(randomblob(6))))"
            .to_string()
    }

    fn uuid_v7(&self) -> Result<String, DmlError> {
        Err(DmlError::UnrenderableExpr(
            "uuidV7 database generation is unsupported on SQLite".to_string(),
        ))
    }

    fn view_create_prefix(
        &self,
        _materialized: bool,
        _replace: bool,
    ) -> Result<String, IrLowerError> {
        let mut create = String::from("CREATE ");
        create.push_str("VIEW ");
        Ok(create)
    }

    fn view_replace_prelude(&self, qname: &str, replace: bool) -> Vec<String> {
        if replace && !self.supports(Capability::CreateOrReplaceView) {
            vec![format!("DROP VIEW IF EXISTS {qname}")]
        } else {
            Vec::new()
        }
    }

    fn view_object_name(&self, name: &str, _eff_schema: &str) -> Result<String, IrLowerError> {
        Ok(dml::quote_bare_ident_for_backend("view", name, &RENDERER)?)
    }

    fn render_table_ref(&self, table: &TableRef, eff_schema: &str) -> Result<String, IrLowerError> {
        let mut sql = {
            if let Some(schema) = table.schema.as_deref() {
                if !schema.eq_ignore_ascii_case(eff_schema) {
                    return Err(IrLowerError::LowerCrossSchema(schema.to_string()));
                }
            }
            dml::quote_bare_ident_for_backend("table", &table.name, &RENDERER)?
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

    /// STEP 3, RESOLVED. The 315 lines this used to reach across the crate for now
    /// sit at the bottom of this file, and the delegation is a local call.
    ///
    /// The note that stood here said the SQLite trigger SPELLING still lived in
    /// `render::lower::render_sqlite_trigger_op`, inside the 17k-line core lowerer,
    /// and that this delegation was "a POINTER to work that `lower.rs`'s own step-3
    /// pass has to finish, not a boundary that is done". Nothing about the emitted
    /// SQL changed when it moved — that is what made it a move.
    ///
    /// PostgreSQL used to be STILL in the position SQLite just left, via
    /// `render::vendor`, and that one was not the same shape: `render::vendor` was
    /// PostgreSQL by CONSTRUCTION rather than by gate (it carried no dialect literal
    /// at all), so every dialect-match census scored it zero. RESOLVED as well now —
    /// see [`Self::render_vendor_op`] below and `zero_migrate::render::vendor`. The
    /// census that DOES see it is `core_names_no_vendor_crate.rs`, which counts crate
    /// idents rather than dialect literals.
    fn render_trigger_op(
        &self,
        op: &Op,
        eff_schema: &str,
    ) -> Result<Vec<zero_migrate_backend::vendor::VendorStatement>, IrLowerError> {
        Ok(vec![render_sqlite_trigger_op(op, eff_schema)?])
    }

    /// SQLite declares no sequence capability. Core refuses before calling this,
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

    /// SQLite lowers authored enum/domain intent into columns and CHECK clauses,
    /// not standalone schema objects. This required body makes that refusal
    /// explicit for any caller that bypasses core's capability gate.
    fn render_materialized_named_type_op(
        &self,
        _op: MaterializedNamedTypeOp<'_>,
    ) -> Result<zero_migrate_backend::vendor::VendorStatement, IrLowerError> {
        Err(IrLowerError::UnsupportedOp(
            "validated materialized named type unsupported by SQLite reached lower",
        ))
    }

    /// SQLite has no `COMMENT ON` statement. Its stored-DDL comments are not a
    /// substitute, so this required implementation refuses explicitly.
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
    /// This is the other half of the note above: PostgreSQL is no longer "in the
    /// position SQLite just left", because the engine no longer names
    /// `zero_migrate_postgres::render_vendor_op`. It asks whichever vendor it
    /// resolved, and this is what this one answers.
    ///
    /// The sixteen privileged op kinds are rendered by exactly one registered
    /// backend, so an artifact carrying any of them measures a `DialectScope::Only`
    /// reach that does not name this one. SQLite has no analogue for any of them, so
    /// there is nothing to render and no partial answer worth giving. The engine refuses
    /// earlier and more informatively — the lower seam checks
    /// `Capability::PrivilegedCatalogObjects` and reports the op KIND — so nothing in
    /// the shipping paths reaches this. It is here because
    /// [`zero_migrate_backend::renderer::DmlRenderer`] gives no method a default
    /// body: a vendor's posture has to be visible in that vendor's own diff.
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

fn render_sqlite_trigger_op(
    op: &Op,
    eff_schema: &str,
) -> Result<zero_migrate_backend::vendor::VendorStatement, IrLowerError> {
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
        } => {
            if events.is_empty() {
                return Err(IrLowerError::Vendor(
                    zero_migrate_backend::vendor::VendorError::EmptyList {
                        what: "trigger events",
                    },
                ));
            }
            if events.iter().any(|e| matches!(e, TriggerEvent::Truncate))
                && !RENDERER.supports(Capability::TriggerTruncateEvent)
            {
                return Err(IrLowerError::TriggerUnsupported {
                    kind: "triggerEventTruncate",
                    dialect: DIALECT,
                });
            }
            if matches!(for_each, ForEach::Statement)
                && !RENDERER.supports(Capability::TriggerStatementForEach)
            {
                return Err(IrLowerError::TriggerUnsupported {
                    kind: "forEachStatement",
                    dialect: DIALECT,
                });
            }
            let TriggerAction::Body { statements } = action else {
                if !RENDERER.supports(Capability::TriggerExecuteFunction) {
                    return Err(IrLowerError::TriggerUnsupported {
                        kind: "executeFunction",
                        dialect: DIALECT,
                    });
                }
                return Err(IrLowerError::UnsupportedOp(
                    "SQLite trigger action routed past capability check",
                ));
            };
            if !RENDERER.supports(Capability::TriggerBody) {
                return Err(IrLowerError::TriggerUnsupported {
                    kind: "triggerBody",
                    dialect: DIALECT,
                });
            }
            if statements.is_empty() {
                return Err(IrLowerError::Vendor(
                    zero_migrate_backend::vendor::VendorError::EmptyList {
                        what: "trigger body statements",
                    },
                ));
            }

            let qname = zero_migrate_backend::dml::quote_bare_ident_for_backend(
                "trigger", name, &RENDERER,
            )?;
            let qtable =
                zero_migrate_backend::dml::quote_bare_ident_for_backend("table", table, &RENDERER)?;
            let events_sql = events
                .iter()
                .map(|e| e.as_sql())
                .collect::<Vec<_>>()
                .join(" OR ");
            let mut up = format!(
                "CREATE TRIGGER {qname} {} {events_sql} ON {qtable}",
                timing.as_sql()
            );
            up.push_str(" FOR EACH ROW");
            if let Some(pred) = when {
                up.push_str(&format!(
                    " WHEN ({})",
                    zero_migrate_backend::dml::render_predicate(pred, &RENDERER)?
                ));
            }
            let body: Result<Vec<_>, _> = statements
                .iter()
                .map(|stmt| render_sqlite_trigger_stmt(stmt, eff_schema))
                .collect();
            up.push_str(" BEGIN ");
            up.push_str(
                &body?
                    .into_iter()
                    .map(|s| format!("{s};"))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
            up.push_str(" END;");
            Ok(zero_migrate_backend::vendor::VendorStatement {
                name: format!("create_trigger_{name}_{table}"),
                up,
                down: Some(format!("DROP TRIGGER IF EXISTS {qname}")),
            })
        }
        Op::DropTrigger {
            name,
            table,
            if_exists,
            ..
        } => {
            let qname = zero_migrate_backend::dml::quote_bare_ident_for_backend(
                "trigger", name, &RENDERER,
            )?;
            let mut up = String::from("DROP TRIGGER ");
            if if_exists.unwrap_or(false) {
                up.push_str("IF EXISTS ");
            }
            up.push_str(&qname);
            Ok(zero_migrate_backend::vendor::VendorStatement {
                name: format!("drop_trigger_{name}_{table}"),
                up,
                down: None,
            })
        }
        _ => Err(IrLowerError::UnsupportedOp(
            "non-trigger op routed to trigger renderer",
        )),
    }
}

fn sqlite_trigger_table_ref(
    table: &str,
    schema: Option<&str>,
    eff_schema: &str,
) -> Result<String, IrLowerError> {
    if let Some(schema) = schema {
        if !schema.eq_ignore_ascii_case(eff_schema) {
            return Err(IrLowerError::LowerCrossSchema(schema.to_string()));
        }
    }
    Ok(zero_migrate_backend::dml::quote_bare_ident_for_backend(
        "table", table, &RENDERER,
    )?)
}

fn render_sqlite_trigger_stmt(
    stmt: &TriggerStmt,
    eff_schema: &str,
) -> Result<String, IrLowerError> {
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
            let qtable = sqlite_trigger_table_ref(table, schema.as_deref(), eff_schema)?;
            let qcols: Result<Vec<_>, _> = columns
                .iter()
                .map(|c| {
                    zero_migrate_backend::dml::quote_bare_ident_for_backend("column", c, &RENDERER)
                })
                .collect();
            let qcols = qcols?;
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
                    .map(|v| {
                        zero_migrate_backend::dml::render_value_inline_for_backend(v, &RENDERER)
                    })
                    .collect();
                groups.push(format!("({})", vals?.join(", ")));
            }
            Ok(format!(
                "INSERT INTO {qtable} ({}) VALUES {}",
                qcols.join(", "),
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
            let qtable = sqlite_trigger_table_ref(table, schema.as_deref(), eff_schema)?;
            let mut assigns = Vec::with_capacity(set.len());
            for (col, rhs) in set {
                assigns.push(format!(
                    "{} = {}",
                    zero_migrate_backend::dml::quote_bare_ident_for_backend(
                        "column", col, &RENDERER
                    )?,
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
            let qtable = sqlite_trigger_table_ref(table, schema.as_deref(), eff_schema)?;
            let pred =
                zero_migrate_backend::dml::render_expr_inline_for_backend(r#where, &RENDERER)?;
            Ok(match limit {
                None => format!("DELETE FROM {qtable} WHERE {pred}"),
                // Trigger rendering has no live-catalog snapshot for the body
                // target. Refuse a limited delete instead of guessing at hidden
                // rowid; the one-shot DML path can use a proven PK/UNIQUE key.
                Some(_) => {
                    return Err(IrLowerError::DmlAssemble(
                        zero_migrate_backend::dml::DmlError::LimitedDeleteNeedsUniqueIdentity {
                            dialect: DIALECT,
                            table: table.clone(),
                        },
                    ));
                }
            })
        }
        TriggerStmt::Select { expr } => Ok(format!(
            "SELECT {}",
            zero_migrate_backend::dml::render_expr_inline_for_backend(expr, &RENDERER)?
        )),
        TriggerStmt::Raise {
            level: RaiseLevel::Ignore,
            ..
        } => Ok("SELECT RAISE(IGNORE)".to_string()),
        TriggerStmt::Raise { level, message, .. } => Ok(format!(
            "SELECT RAISE({},{})",
            raise_level_sql(*level),
            zero_migrate_backend::dml::sql_string_literal(message)
        )),
    }
}

/// The `RAISE(<action>, …)` action token for a neutral [`RaiseLevel`].
///
/// This spelling is THIS backend's, and lives here rather than on the IR enum for
/// the reason the enum's own comment records: a target without `RAISE` reads the
/// same level and answers in its own grammar — MySQL discards the level and emits
/// `SIGNAL SQLSTATE`. A shared `as_sql` would have implied one of the two is the
/// level's real spelling.
const fn raise_level_sql(level: RaiseLevel) -> &'static str {
    match level {
        RaiseLevel::Abort => "ABORT",
        RaiseLevel::Fail => "FAIL",
        RaiseLevel::Ignore => "IGNORE",
        RaiseLevel::Rollback => "ROLLBACK",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zero_migrate_ir::expr::Expr;
    use zero_migrate_ir::ir::SafeU64;

    /// Relocated from `render::lower`'s test module with the renderer it covers.
    /// A unit test for a private helper cannot outlive its module, and leaving it
    /// behind would have meant either widening the helper's visibility or dropping
    /// the only coverage of this refusal.
    #[test]
    fn sqlite_trigger_limited_delete_is_rejected_without_live_identity_facts() {
        let stmt = TriggerStmt::Delete {
            table: "events".to_string(),
            r#where: Expr::UnaryOp {
                op: zero_migrate_ir::expr::UnaryOp::IsNull,
                operand: Box::new(Expr::col("code")),
            },
            limit: Some(SafeU64::new(1).unwrap()),
            schema: None,
        };
        let err = render_sqlite_trigger_stmt(&stmt, "app")
            .expect_err("trigger body rendering cannot guess at hidden rowid");
        assert!(matches!(
            err,
            IrLowerError::DmlAssemble(
                zero_migrate_backend::dml::DmlError::LimitedDeleteNeedsUniqueIdentity {
                    ref table,
                    ..
                }
            ) if table == "events"
        ));
    }
}
