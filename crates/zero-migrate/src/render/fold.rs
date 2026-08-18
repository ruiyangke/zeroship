//! The offline ops→schema fold.
//!
//! [`fold_ops`] replays an ordered [`Op`] list into a [`SchemaSnapshot`] — PURE,
//! offline, NO database I/O. It is the offline companion of the live
//! [`snapshot_schema`](crate::apply::drift::snapshot_schema): the SAME `SchemaSnapshot`
//! output, sourced from the migration set instead of `pg_catalog`. The
//! migration-first design makes the `op.*` migrations the SOLE source of truth, and "the current
//! schema" is the fold of that set; later phases (`gen-types`) emit the `env.db`
//! types + the runtime descriptor from this snapshot.
//!
//! # Why it agrees with introspection (the load-bearing invariant)
//!
//! The fold does NOT re-implement column / type / default / sentinel shaping. It
//! REUSES the SHARED snapshot-builder the differ + the IR lower both use
//! (`build_table_snapshot`, `ir_fk_constraint_snapshot_for_columns`,
//! `ir_column_to_field`, `create_index_snapshot`, …). Because
//! the engine APPLIES the same ops the fold replays through that builder, and the
//! differ's `desired_snapshot` is already round-trip-proven equal to
//! `snapshot_schema(live)` (the `declarative_pg` round-trip tests), the folded
//! snapshot is structurally identical to live introspection — transitively
//! `fold == introspect`. The headline correctness net is the round-trip oracle
//! (`tests/fold_roundtrip_pg.rs`): apply a corpus to real PG, introspect, assert
//! equality.
//!
//! ## The one exception: a partition on a collapsed dialect
//!
//! `fold == introspect` holds on PostgreSQL, which is what the round-trip oracle
//! above measures. It does NOT hold for `partitions` on SQLite or MySQL, and the
//! oracle cannot see that because it only runs against PostgreSQL.
//!
//! `Op::CreatePartition` records a child unconditionally here. Off PostgreSQL the
//! lower collapses that child into its parent behind a mirror guard and creates no
//! relation at all (`render/lower.rs`, "createPartition needs a collapse-affirmed
//! parent on SQLite/MySQL"), so those backends correctly snapshot no partition.
//! A folded snapshot therefore claims a relation live introspection cannot report,
//! and a dialect-blind comparison calls it missing.
//!
//! The recorded child is NOT removable: the bounds are read back out to derive the
//! collapsed deletes (`PartitionLowerState::from_live`), and a folded history is fed
//! straight back into lowering by `tests/partition_render.rs`. The fold has to keep
//! it. What follows from that is a comparison contract, stated on
//! [`diff_snapshots`](crate::apply::drift::diff_snapshots), not a change here.
//!
//! Partitions are the only structurally compared class where this bites. Function,
//! policy, and trigger definitions are also history-only, but `SchemaSnapshot`
//! excludes that rollback metadata from equality and drift just as `ViewSnapshot`
//! excludes its authored query. Named types are the near miss that shows the shape:
//! `createEnum` and `createDomain` are portable on MySQL, so the engine authors
//! them there, but the insert below is gated on
//! `Capability::MaterializedEnumType`, false off PostgreSQL, so both sides carry
//! nothing and agree. Sequences, roles, schemas and extensions are PostgreSQL-only
//! and cannot appear in a MySQL or SQLite history at all.
//!
//! # Fail-closed
//!
//! An incoherent op stream (add-column-to-missing-table, drop-absent-column,
//! duplicate-create-table, rename-to-existing, …) is a structured [`FoldError`] —
//! never a silently-wrong snapshot. A real IR envelope set the
//! engine already applied is internally consistent, so the fold agrees with apply.
//!
//! # DML is a schema no-op
//!
//! `Insert`/`Update`/`Delete`/`Backfill` mutate ROWS, not the structural shape, so
//! they fold to no-ops.
//!
//! # Schema qualifier / existence guard are fold-irrelevant
//!
//! An op's `schema` qualifier normally governs only where the DDL renders. Function,
//! policy, and trigger rollback histories are the exceptions: their keys resolve an
//! omitted schema to `project_schema` so definitions in different schemas cannot
//! collide. An `existence_guard` still governs only apply-time presence and does
//! not change the final folded logical shape. FK definitions also embed
//! `project_schema` (`REFERENCES <schema>.<target>(id)`), so the caller passes the
//! schema the live DB is introspected under for both uses.

use std::collections::BTreeMap;

use crate::model::ir::{
    AlterPrimaryKeyAction, ColType, ColumnCollation, ColumnOrExpr, ColumnReference, CommentTarget,
    ExclusionElement, IndexElement, IrColumn, IrConstraint, IrConstraintKind, IrDefault, IrIndex,
    Op, RefAction, SafeI64, SafeU64, SequenceOwnedBy, TableRuntimeOptions,
};
use crate::model::snapshot::{
    normalize_sequence_max_value, normalize_sequence_min_value, sequence_default_start_value,
    ColumnSnapshot, ConstraintSnapshot, ExtensionSnapshot, FunctionIdentity, FunctionKey,
    FunctionSnapshot, IndexElementSnapshot, IndexSnapshot, NamedTypeSnapshot, PartitionSnapshot,
    PolicyIdentity, PolicyKey, PolicySnapshot, RoleSnapshot, SchemaObjectSnapshot, SchemaSnapshot,
    SequenceDataTypeSnapshot, SequenceSnapshot, TableSnapshot, TriggerIdentity, TriggerKey,
    TriggerSnapshot, VendorObjectIdentities, ViewSnapshot,
};
use crate::model::table_shape::ResolvedInject;
#[cfg(test)]
use crate::render::declarative::build_table_snapshot;
use crate::render::declarative::{
    build_resolved_table_snapshot, constraintdef_cols, ir_fk_constraint_snapshot_for_columns,
    non_unique_index_name, push_primary_key_snapshot, quote_ident_if_needed, CollectionDescriptor,
    DeclarativeError,
};
use crate::render::lower::{
    author_type_override, create_index_snapshot, derived_check_constraint_name,
    derived_constraint_name, derived_exclusion_constraint_name, enum_inline_check,
    index_method_access, ir_column_to_field, ir_column_to_field_resolved_create, mysql_enum_type,
    postgres_named_type_metadata, render_container_default_for_data_type, render_domain_check,
    render_exclusion_constraint_body, render_ir_default, render_ir_default_for_type,
    render_json_default_for_data_type, IrLowerError, NamedTypeRegistry,
};
use crate::render::renderer::{Capability, DialectSupports};
use crate::render::value_format::{
    authored_id_default, authored_text_id_default, authored_uuid_id_default, catalog_id_default,
    catalog_uuid_id_default, column_metadata as value_format_column_metadata, uuid_column_metadata,
};
use crate::schema::query::SqlDialect;
use zero_migrate_policy::EffectivePolicy;

/// The owner-app stamp the fold gives every `CollectionDescriptor`. `owner_app` is
/// drift-irrelevant — it never enters `SchemaSnapshot` equality (the snapshot only
/// carries columns/indexes/constraints, none of which embed it), so a fold-internal
/// constant is correct. (Ownership is a deploy-time concern handled elsewhere; the
/// project-union phase re-derives ownership from the migrations directly.)
const FOLD_OWNER_APP: &str = "__fold__";

/// A structured, fail-closed fold error. Every incoherent op
/// stream maps to a typed variant — never a silently-wrong snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoldError {
    /// An op targeted a table not present in the folded schema.
    MissingTable(String),
    /// A `createTable` named a table already present.
    DuplicateTable(String),
    /// A `createView` named a view already present.
    DuplicateView(String),
    /// A `createView` with `replace` would change whether the existing view is
    /// materialized. `replace` licenses a new BODY under the same name, not a
    /// different kind of object, and no engine turns one into the other in place.
    ViewKindChanged {
        /// The view both declarations name.
        name: String,
        /// Whether the view already in the folded schema is materialized.
        existing_materialized: bool,
        /// Whether the replacing declaration asks for a materialized view.
        declared_materialized: bool,
    },
    /// A `dropView` named a view not present in the folded schema.
    MissingView(String),
    /// A `createSequence` named a sequence already present.
    DuplicateSequence(String),
    /// A `dropSequence`/`alterSequence` named a sequence not present.
    MissingSequence(String),
    /// A column references a named enum/domain that has not been registered.
    NamedTypeMissing {
        /// `"enum"` or `"domain"`.
        kind: &'static str,
        /// Referenced type name.
        name: String,
    },
    /// A named enum/domain cannot be folded soundly.
    NamedTypeUnsupported {
        /// `"enum"` or `"domain"`.
        kind: &'static str,
        /// Referenced type name.
        name: String,
        /// Why it cannot be folded.
        reason: &'static str,
    },
    /// Rendering named-type metadata failed.
    NamedTypeRender(String),
    /// Rendering a fold-visible body failed.
    Render(String),
    /// An op targeted a column not present on its table.
    MissingColumn {
        /// The table.
        table: String,
        /// The absent column.
        column: String,
    },
    /// An `addColumn` / `renameColumn` would create a column that already exists.
    DuplicateColumn {
        /// The table.
        table: String,
        /// The colliding column.
        column: String,
    },
    /// A `dropConstraint` named a constraint not present on its table.
    MissingConstraint {
        /// The table.
        table: String,
        /// The absent constraint.
        name: String,
    },
    /// An `addConstraint` / table-level `createTable` constraint name collided.
    DuplicateConstraint {
        /// The table.
        table: String,
        /// The colliding constraint.
        name: String,
    },
    /// A `dropIndex` named an index not present in the folded schema.
    MissingIndex(String),
    /// A `createIndex` / table-level `createTable` index name collided.
    DuplicateIndex(String),
    /// A `dropColumn` reached a folded CHECK constraint with no recorded
    /// `cascade_columns`, so whether PostgreSQL cascaded it away is unknowable.
    CheckCascadeColumnsMissing {
        /// The table.
        table: String,
        /// The CHECK constraint with no recorded columns.
        name: String,
    },
    /// A `renameColumn` `to` name already exists on the table.
    RenameCollision {
        /// The table.
        table: String,
        /// The `to` name that already exists.
        to: String,
    },
    /// `expectedColumns` (or add's expected absence) did not match the folded
    /// current primary key exactly and in order.
    PrimaryKeyPrecondition {
        /// The table.
        table: String,
        /// Expected current key (`None` for add).
        expected: Option<Vec<String>>,
        /// Folded current key (`None` when absent).
        actual: Option<Vec<String>>,
    },
    /// Add/replace named no exact pre-existing UNIQUE candidate key.
    MissingPrimaryKeyCandidate {
        /// The table.
        table: String,
        /// Exact ordered target key.
        columns: Vec<String>,
    },
    /// A malformed lifecycle payload reached the fold without authoring
    /// validation.
    InvalidPrimaryKeyAction(String),
    /// The declared identity transition is inconsistent with the folded column
    /// facets or omits a required identity removal.
    InvalidPrimaryKeyIdentityTransition {
        /// The table.
        table: String,
        /// The identity-bearing (or incorrectly declared) column.
        column: String,
        /// Stable reason.
        reason: &'static str,
    },
    /// The shared snapshot-builder rejected the shape (unknown type token, a bad
    /// `ref` target, a malformed `id` prefix, …). Carries the builder's own error.
    Shape(DeclarativeError),
    /// A structurally-unsupported op the IR lower also refuses (a composite /
    /// per-column user PRIMARY KEY — the platform owns the `id` PK; a multi-column
    /// or non-`id`-referencing FK; …). Parity with the lower's `UnsupportedOp`.
    Unsupported(&'static str),
}

impl std::fmt::Display for FoldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FoldError::MissingTable(t) => write!(f, "fold: table `{t}` does not exist"),
            FoldError::DuplicateTable(t) => write!(f, "fold: table `{t}` already exists"),
            FoldError::DuplicateView(v) => write!(f, "fold: view `{v}` already exists"),
            FoldError::ViewKindChanged {
                name,
                existing_materialized,
                declared_materialized,
            } => {
                let kind = |m: &bool| if *m { "materialized" } else { "plain" };
                write!(
                    f,
                    "fold: view `{name}` is {} and the replacing declaration is {}; \
                     replace changes a view's body, not its kind - drop it and create the \
                     other kind instead",
                    kind(existing_materialized),
                    kind(declared_materialized)
                )
            }
            FoldError::MissingView(v) => write!(f, "fold: view `{v}` does not exist"),
            FoldError::DuplicateSequence(s) => {
                write!(f, "fold: sequence `{s}` already exists")
            }
            FoldError::MissingSequence(s) => write!(f, "fold: sequence `{s}` does not exist"),
            FoldError::NamedTypeMissing { kind, name } => {
                write!(f, "fold: {kind} `{name}` is not registered")
            }
            FoldError::NamedTypeUnsupported { kind, name, reason } => {
                write!(f, "fold: {kind} `{name}` is unsupported: {reason}")
            }
            FoldError::NamedTypeRender(e) => write!(f, "fold: named type render error: {e}"),
            FoldError::Render(e) => write!(f, "fold: render error: {e}"),
            FoldError::MissingColumn { table, column } => {
                write!(f, "fold: column `{table}.{column}` does not exist")
            }
            FoldError::DuplicateColumn { table, column } => {
                write!(f, "fold: column `{table}.{column}` already exists")
            }
            FoldError::MissingConstraint { table, name } => {
                write!(f, "fold: constraint `{name}` does not exist on `{table}`")
            }
            FoldError::DuplicateConstraint { table, name } => {
                write!(f, "fold: constraint `{name}` already exists on `{table}`")
            }
            FoldError::MissingIndex(n) => write!(f, "fold: index `{n}` does not exist"),
            FoldError::DuplicateIndex(n) => write!(f, "fold: index `{n}` already exists"),
            FoldError::CheckCascadeColumnsMissing { table, name } => write!(
                f,
                "fold: CHECK constraint `{name}` on `{table}` records no cascade columns, \
                 so a column drop cannot tell whether PostgreSQL cascaded it away; the \
                 producer of this constraint must record them structurally"
            ),
            FoldError::RenameCollision { table, to } => {
                write!(f, "fold: rename target `{table}.{to}` already exists")
            }
            FoldError::PrimaryKeyPrecondition {
                table,
                expected,
                actual,
            } => write!(
                f,
                "fold: primary key precondition failed on `{table}`: expected {expected:?}, found {actual:?}"
            ),
            FoldError::MissingPrimaryKeyCandidate { table, columns } => write!(
                f,
                "fold: `{table}` has no exact pre-existing UNIQUE candidate for primary key {columns:?}"
            ),
            FoldError::InvalidPrimaryKeyAction(reason) => {
                write!(f, "fold: invalid primary-key lifecycle action: {reason}")
            }
            FoldError::InvalidPrimaryKeyIdentityTransition {
                table,
                column,
                reason,
            } => write!(
                f,
                "fold: invalid primary-key identity transition for `{table}.{column}`: {reason}"
            ),
            FoldError::Shape(e) => write!(f, "fold: shape error: {e}"),
            FoldError::Unsupported(what) => write!(f, "fold: unsupported op: {what}"),
        }
    }
}

impl std::error::Error for FoldError {}

impl From<DeclarativeError> for FoldError {
    fn from(e: DeclarativeError) -> Self {
        FoldError::Shape(e)
    }
}

fn fold_named_type_error(e: IrLowerError) -> FoldError {
    match e {
        IrLowerError::NamedTypeMissing { kind, name } => FoldError::NamedTypeMissing { kind, name },
        IrLowerError::NamedTypeUnsupported { kind, name, reason } => {
            FoldError::NamedTypeUnsupported { kind, name, reason }
        }
        other => FoldError::NamedTypeRender(other.to_string()),
    }
}

fn fold_lower_error(e: IrLowerError) -> FoldError {
    FoldError::Render(e.to_string())
}

fn safe_i64_one() -> SafeI64 {
    SafeI64::new(1).expect("1 is a safe integer")
}

fn safe_u64_one() -> SafeU64 {
    SafeU64::new(1).expect("1 is a safe integer")
}

fn fold_sequence_bound(
    as_type: SequenceDataTypeSnapshot,
    increment: SafeI64,
    value: &Option<Option<SafeI64>>,
    normalize: fn(SequenceDataTypeSnapshot, SafeI64, i64) -> Result<Option<SafeI64>, String>,
) -> Result<Option<SafeI64>, FoldError> {
    match value {
        None | Some(None) => Ok(None),
        Some(Some(n)) => normalize(as_type, increment, n.get()).map_err(FoldError::Render),
    }
}

struct CreateSequenceFoldInput<'a> {
    as_type: Option<&'a ColType>,
    increment: Option<SafeI64>,
    start: Option<SafeI64>,
    min_value: &'a Option<Option<SafeI64>>,
    max_value: &'a Option<Option<SafeI64>>,
    cache: Option<SafeU64>,
    cycle: Option<bool>,
    owned_by: &'a Option<Option<SequenceOwnedBy>>,
}

fn fold_create_sequence_snapshot(
    input: CreateSequenceFoldInput<'_>,
) -> Result<SequenceSnapshot, FoldError> {
    let as_type = SequenceDataTypeSnapshot::from_sequence_col_type(input.as_type)
        .map_err(FoldError::Unsupported)?;
    let increment = input.increment.unwrap_or_else(safe_i64_one);
    let min_value = fold_sequence_bound(
        as_type,
        increment,
        input.min_value,
        normalize_sequence_min_value,
    )?;
    let max_value = fold_sequence_bound(
        as_type,
        increment,
        input.max_value,
        normalize_sequence_max_value,
    )?;
    let start = match input.start {
        Some(start) => start,
        None => sequence_default_start_value(as_type, increment, min_value, max_value)
            .map_err(FoldError::Render)?,
    };
    Ok(SequenceSnapshot {
        as_type,
        increment,
        min_value,
        max_value,
        start,
        cache: input.cache.unwrap_or_else(safe_u64_one),
        cycle: input.cycle.unwrap_or(false),
        owned_by: input.owned_by.clone().flatten(),
        comment: None,
    })
}

fn apply_alter_sequence_snapshot(
    seq: &mut SequenceSnapshot,
    increment: Option<SafeI64>,
    min_value: &Option<Option<SafeI64>>,
    max_value: &Option<Option<SafeI64>>,
    cache: Option<SafeU64>,
    cycle: Option<bool>,
    owned_by: &Option<Option<SequenceOwnedBy>>,
) -> Result<(), FoldError> {
    if let Some(increment) = increment {
        seq.increment = increment;
    }
    if min_value.is_some() {
        seq.min_value = fold_sequence_bound(
            seq.as_type,
            seq.increment,
            min_value,
            normalize_sequence_min_value,
        )?;
    }
    if max_value.is_some() {
        seq.max_value = fold_sequence_bound(
            seq.as_type,
            seq.increment,
            max_value,
            normalize_sequence_max_value,
        )?;
    }
    if let Some(cache) = cache {
        seq.cache = cache;
    }
    if let Some(cycle) = cycle {
        seq.cycle = cycle;
    }
    if let Some(owned_by) = owned_by {
        seq.owned_by = owned_by.clone();
    }
    Ok(())
}

/// Rewrite every INCOMING FK `definition` in OTHER tables to follow a table
/// rename — the offline mirror of what live PG does on `ALTER TABLE … RENAME TO`.
///
/// **Why this is required.** A FK `definition` embeds the referenced
/// table by name (`FOREIGN KEY (col) REFERENCES <schema>.<target>(id) …`, built by
/// [`crate::render::declarative::ir_fk_constraint_snapshot_for_columns`]). Live PG renders that body
/// via `pg_get_constraintdef(oid)`, which resolves the referenced relation by OID,
/// so after `RENAME TO` the referencing FK reports the NEW name. If the fold left
/// the FK pointing at the OLD name, `fold_ops != snapshot_schema(live)` for EVERY
/// table that had an incoming FK — a permanent phantom drift, and `gen-types` would
/// emit a `ref` to a non-existent collection. SQLite ≥3.25 likewise auto-updates FK
/// references on table rename (the engine never sets `legacy_alter_table`), so the
/// rewrite is correct on both legs.
///
/// The referenced-table token is uniquely spelled `REFERENCES <schema_q>.<old_q>(`
/// (schema + target both `quote_ident_if_needed`-quoted, immediately followed by the
/// `(id)` column list), so a substring swap of that exact prefix is precise — it
/// cannot collide with the local-column list (which precedes `REFERENCES`) or with a
/// same-named column. Only `FOREIGN KEY` constraints carry a `REFERENCES`, so the
/// scan is scoped to them.
fn rewrite_incoming_fk_targets(
    tables: &mut BTreeMap<String, TableSnapshot>,
    project_schema: &str,
    renamed: &str,
    new_name: &str,
) {
    let schema_q = quote_ident_if_needed(project_schema);
    let old_ref = format!("REFERENCES {schema_q}.{}(", quote_ident_if_needed(renamed));
    let new_ref = format!("REFERENCES {schema_q}.{}(", quote_ident_if_needed(new_name));
    // Walk EVERY table — including the renamed table's own (already-moved) entry,
    // which may carry a SELF-FK (`REFERENCES <old>`) that live PG also re-targets
    // to the new name by OID. The `old_ref` token cannot appear in any other table
    // unless it references the renamed table, so a blanket scan is safe.
    for t in tables.values_mut() {
        for c in &mut t.constraints {
            if c.kind == "FOREIGN KEY" && c.definition.contains(&old_ref) {
                c.definition = c.definition.replace(&old_ref, &new_ref);
            }
        }
    }
}

/// Rewrite the REFERENCED column list of every INCOMING FK `definition` that targets
/// `renamed_table` so it follows a COLUMN rename - the offline mirror of what live PG
/// does on `ALTER TABLE ... RENAME COLUMN`.
///
/// **Why this is required.** The column-rename arm never leaves the renamed table's
/// own snapshot, but a FK `definition` in ANOTHER table embeds the referenced column
/// by name in its `REFERENCES <schema>.<target>(id)` tail. Live PG holds
/// `pg_constraint.confkey` as attribute NUMBERS, so `pg_get_constraintdef` deparses
/// the NEW name there the instant the rename commits (measured on PG 18.4). Leaving
/// the referencing table stale is a permanent phantom drift: the differ compares
/// `definition` for every kind but EXCLUDE and CHECK.
///
/// The referenced TABLE is matched FIRST, on the same uniquely spelled
/// `REFERENCES <schema_q>.<target_q>(` token [`rewrite_incoming_fk_targets`] uses -
/// schema and target both `quote_ident_if_needed`-quoted, immediately followed by the
/// referenced list. Without that match, renaming `id` in one table would rewrite an
/// unrelated FK that happens to reference a same-named `id` in a DIFFERENT table,
/// turning a stale definition into a corrupt one. The token also cannot collide with
/// the LOCAL column list, which precedes `REFERENCES`.
///
/// The walk covers EVERY table, including the renamed table's own entry, which may
/// carry a SELF-FK whose tail live PG re-targets the same way.
fn rewrite_incoming_fk_column_targets(
    tables: &mut BTreeMap<String, TableSnapshot>,
    project_schema: &str,
    renamed_table: &str,
    from: &str,
    to: &str,
) {
    let schema_q = quote_ident_if_needed(project_schema);
    let target_ref = format!(
        "REFERENCES {schema_q}.{}(",
        quote_ident_if_needed(renamed_table)
    );
    for table in tables.values_mut() {
        for constraint in &mut table.constraints {
            if constraint.kind != "FOREIGN KEY" {
                continue;
            }
            let Some(at) = constraint.definition.find(&target_ref) else {
                continue;
            };
            // `target_ref` ends WITH the `(`, so its last byte is the group's opener.
            let open = at + target_ref.len() - 1;
            if let Some(definition) =
                rename_definition_column_group(&constraint.definition, open, from, to)
            {
                constraint.definition = definition;
            }
        }
    }
}

/// The leg an `Op::Dialectal` contributes on `dialect`: its own, else `default`, else
/// nothing. `pub(crate)` so callers outside the fold select legs the SAME way rather
/// than re-deriving the own-then-default rule and drifting from it.
pub(crate) fn selected_dialectal_leg<'a>(
    dialect: SqlDialect,
    default: &'a Option<Vec<Op>>,
    pg: &'a Option<Vec<Op>>,
    sqlite: &'a Option<Vec<Op>>,
    mysql: &'a Option<Vec<Op>>,
) -> Option<&'a [Op]> {
    let own = match dialect {
        SqlDialect::Postgres => pg,
        SqlDialect::Sqlite => sqlite,
        SqlDialect::Mysql => mysql,
    };
    own.as_deref().or(default.as_deref())
}

fn push_fold_op<'a>(
    out: &mut Vec<&'a Op>,
    op: &'a Op,
    dialect: SqlDialect,
    inside_dialectal: bool,
) -> Result<(), FoldError> {
    match op {
        Op::Dialectal {
            default,
            pg,
            sqlite,
            mysql,
        } => {
            if inside_dialectal {
                return Err(FoldError::Unsupported("nested dialectal op reached fold"));
            }
            if let Some(leg) = selected_dialectal_leg(dialect, default, pg, sqlite, mysql) {
                for inner in leg {
                    push_fold_op(out, inner, dialect, true)?;
                }
            }
            Ok(())
        }
        other => {
            out.push(other);
            Ok(())
        }
    }
}

/// Does this op stream carry an op-level `dialect()` wrapper?
///
/// Answers "the dialect argument decides part of this history's content", which is
/// the fact a caller needs in order to know whether the dialect it supplied mattered.
/// Deliberately NOT "a leg was selected": a pg-only wrapper folded under SQLite
/// matches no leg and has no `default`, so it contributes nothing and the fold
/// succeeds - the dialect decided that op's entire content, and a selection-shaped
/// answer would report `false` at the one moment the answer carries weight.
///
/// A TOP-LEVEL scan is complete because a leg cannot itself hold a wrapper: nesting
/// is refused by the validator, and `push_fold_op` returns
/// `FoldError::Unsupported("nested dialectal op reached fold")` if one ever reaches
/// here. So an op stream that survives validation carries its wrappers at depth one.
///
/// Says NOTHING about whether the artifacts are dialect-independent. Other fold
/// rules key on the dialect too - see the [`crate::render_artifacts`] contract - so
/// `false` means "no `dialect()` wrapper", never "the target does not matter".
#[must_use]
pub fn history_carries_dialectal_ops(ops: &[Op]) -> bool {
    ops.iter().any(|op| matches!(op, Op::Dialectal { .. }))
}

pub(crate) fn flatten_dialectal_ops(
    ops: &[Op],
    dialect: SqlDialect,
) -> Result<Vec<&Op>, FoldError> {
    let mut out = Vec::new();
    for op in ops {
        push_fold_op(&mut out, op, dialect, false)?;
    }
    Ok(out)
}

fn constraint_ordered_columns(definition: &str) -> Option<Vec<String>> {
    let open = definition.find('(')?;
    let close = definition[open + 1..].find(')')? + open + 1;
    let columns = definition[open + 1..close]
        .split(',')
        .map(|column| column.trim().trim_matches('"').to_string())
        .collect::<Vec<_>>();
    (!columns.is_empty() && columns.iter().all(|column| !column.is_empty())).then_some(columns)
}

fn folded_primary_key(snap: &TableSnapshot) -> Result<Option<(String, Vec<String>)>, FoldError> {
    let mut primary_keys = snap
        .constraints
        .iter()
        .filter(|constraint| constraint.kind == "PRIMARY KEY");
    let Some(constraint) = primary_keys.next() else {
        return Ok(None);
    };
    if primary_keys.next().is_some() {
        return Err(FoldError::Unsupported(
            "table snapshot carries more than one PRIMARY KEY",
        ));
    }
    let columns = snap
        .indexes
        .iter()
        .find(|index| index.name == constraint.name && index.unique)
        .map(|index| index.columns.clone())
        .or_else(|| constraint_ordered_columns(&constraint.definition))
        .ok_or(FoldError::Unsupported(
            "PRIMARY KEY snapshot has no recoverable ordered columns",
        ))?;
    Ok(Some((constraint.name.clone(), columns)))
}

fn has_exact_unique_candidate(
    snap: &TableSnapshot,
    columns: &[String],
    current_primary_key_name: Option<&str>,
) -> bool {
    let eligible_index = snap.indexes.iter().any(|index| {
        index.name != current_primary_key_name.unwrap_or_default()
            && index.unique
            && index.columns == columns
            && index.access_method == "btree"
            && index.predicate.is_none()
            && !index.only
            && index.elements.len() == columns.len()
            && index.elements.iter().zip(columns).all(|(element, column)| {
                matches!(element, IndexElementSnapshot::Column { name, .. } if name == column)
            })
    });
    eligible_index
        || snap.constraints.iter().any(|constraint| {
            constraint.kind == "UNIQUE"
                && constraint_ordered_columns(&constraint.definition).as_deref() == Some(columns)
        })
}

fn reusable_postgres_primary_index(
    snap: &TableSnapshot,
    columns: &[String],
    current_primary_key_name: Option<&str>,
) -> Option<String> {
    snap.indexes
        .iter()
        .find(|index| {
            let constraint_owned = snap.constraints.iter().any(|constraint| {
                constraint.name == index.name
                    && matches!(
                        constraint.kind.as_str(),
                        "PRIMARY KEY" | "UNIQUE" | "EXCLUDE"
                    )
            });
            index.name != current_primary_key_name.unwrap_or_default()
                && !constraint_owned
                && index.unique
                && index.columns == columns
                && index.access_method == "btree"
                && index.predicate.is_none()
                && index.include.is_empty()
                && !index.only
                && index.elements.len() == columns.len()
                && index.elements.iter().all(|element| {
                    matches!(
                        element,
                        IndexElementSnapshot::Column {
                            order: None | Some(crate::model::ir::IndexSortOrder::Asc),
                            opclass: None,
                            collation: None,
                            ..
                        }
                    )
                })
        })
        .map(|index| index.name.clone())
}

fn sqlite_integer_storage_for_rowid(snap: &TableSnapshot, data_type: &str) -> bool {
    if snap.stored_create_sql.is_some() {
        data_type.trim().eq_ignore_ascii_case("INTEGER")
    } else {
        matches!(
            data_type.trim().to_ascii_lowercase().as_str(),
            "integer" | "int" | "bigint" | "smallint" | "boolean"
        )
    }
}

fn sqlite_folded_rowid_generation(
    snap: &TableSnapshot,
    old_columns: &[String],
    column: &str,
) -> bool {
    if old_columns != [column] {
        return false;
    }
    let Some(folded) = snap
        .columns
        .iter()
        .find(|candidate| candidate.name == column)
    else {
        return false;
    };
    if !sqlite_integer_storage_for_rowid(snap, &folded.data_type) {
        return false;
    }
    snap.stored_create_sql.as_deref().is_none_or(|stored| {
        !crate::render::declarative::sqlite_create_is_without_rowid(stored)
            && !crate::render::declarative::sqlite_inline_primary_key_is_desc(stored, column)
    })
}

/// Allocate a PostgreSQL-generated name against the modeled relation namespace.
///
/// HOLE: Relations created out of band are invisible to the folded IR, so this
/// cannot reserve a suffix they already occupy. This is the same accepted limit
/// as implicit primary-key allocation elsewhere in the fold.
fn allocate_implicit_relation_name(
    default_name: &str,
    dialect: SqlDialect,
    tables: &BTreeMap<String, TableSnapshot>,
    partitions: &BTreeMap<String, PartitionSnapshot>,
    views: &BTreeMap<String, ViewSnapshot>,
    sequences: &BTreeMap<String, SequenceSnapshot>,
) -> String {
    if dialect != SqlDialect::Postgres {
        return default_name.to_string();
    }

    let name_is_taken = |candidate: &str| {
        tables.contains_key(candidate)
            || partitions.contains_key(candidate)
            || views.contains_key(candidate)
            || sequences.contains_key(candidate)
            || tables
                .values()
                .any(|snapshot| snapshot.indexes.iter().any(|index| index.name == candidate))
    };
    let relation_count = tables.len()
        + partitions.len()
        + views.len()
        + sequences.len()
        + tables
            .values()
            .map(|snapshot| snapshot.indexes.len())
            .sum::<usize>();
    (0..=relation_count)
        .map(|suffix| {
            if suffix == 0 {
                default_name.to_string()
            } else {
                format!("{default_name}{suffix}")
            }
        })
        .find(|candidate| !name_is_taken(candidate))
        .expect("one more implicit relation-name candidate than relations must leave a free name")
}

/// Allocate the name PostgreSQL gives an implicit PRIMARY KEY relation.
///
/// This covers relation kinds represented by `SchemaSnapshot`. It does not
/// uniquify explicit constraint names or indexes adopted by `USING INDEX`.
fn implicit_primary_key_name(
    table: &str,
    dialect: SqlDialect,
    tables: &BTreeMap<String, TableSnapshot>,
    partitions: &BTreeMap<String, PartitionSnapshot>,
    views: &BTreeMap<String, ViewSnapshot>,
    sequences: &BTreeMap<String, SequenceSnapshot>,
) -> String {
    allocate_implicit_relation_name(
        &format!("{table}_pkey"),
        dialect,
        tables,
        partitions,
        views,
        sequences,
    )
}

fn apply_fold_alter_primary_key(
    table: &str,
    snap: &mut TableSnapshot,
    action: &AlterPrimaryKeyAction,
    dialect: SqlDialect,
    implicit_name: &str,
) -> Result<(), FoldError> {
    zero_migrate_ir::validate::validate_alter_primary_key_action(action)
        .map_err(FoldError::InvalidPrimaryKeyAction)?;

    let current = folded_primary_key(snap)?;
    let actual = current.as_ref().map(|(_, columns)| columns.clone());
    let expected = action.expected_columns().map(<[String]>::to_vec);
    if actual != expected {
        return Err(FoldError::PrimaryKeyPrecondition {
            table: table.to_string(),
            expected,
            actual,
        });
    }

    if let Some(target) = action.target_columns() {
        if dialect == SqlDialect::Sqlite
            && target.len() == 1
            && snap
                .columns
                .iter()
                .find(|column| column.name == target[0])
                .is_some_and(|column| sqlite_integer_storage_for_rowid(snap, &column.data_type))
            && snap.stored_create_sql.as_deref().is_none_or(|stored| {
                !crate::render::declarative::sqlite_create_is_without_rowid(stored)
            })
        {
            return Err(FoldError::Unsupported(
                "alterPrimaryKey cannot introduce SQLite INTEGER PRIMARY KEY rowid generation",
            ));
        }
        for column in target {
            let target_column = snap
                .columns
                .iter()
                .find(|candidate| candidate.name == *column)
                .ok_or_else(|| FoldError::MissingColumn {
                    table: table.to_string(),
                    column: column.clone(),
                })?;
            if target_column.nullable {
                return Err(FoldError::Unsupported(
                    "alterPrimaryKey target columns must already be NOT NULL",
                ));
            }
        }
        if !has_exact_unique_candidate(
            snap,
            target,
            current.as_ref().map(|(name, _)| name.as_str()),
        ) {
            return Err(FoldError::MissingPrimaryKeyCandidate {
                table: table.to_string(),
                columns: target.to_vec(),
            });
        }
    }

    let drop_identity_from = action.drop_identity_from();
    let old_columns = current
        .as_ref()
        .map(|(_, columns)| columns.as_slice())
        .unwrap_or_default();
    for column in drop_identity_from {
        let folded = snap
            .columns
            .iter()
            .find(|candidate| candidate.name == *column)
            .ok_or_else(|| FoldError::MissingColumn {
                table: table.to_string(),
                column: column.clone(),
            })?;
        let generated = folded.identity.is_some()
            || (dialect == SqlDialect::Sqlite
                && sqlite_folded_rowid_generation(snap, old_columns, column));
        if !generated {
            return Err(FoldError::InvalidPrimaryKeyIdentityTransition {
                table: table.to_string(),
                column: column.clone(),
                reason: "dropIdentityFrom names a column with no identity facet",
            });
        }
    }
    if let Some((_, old_columns)) = &current {
        for column in old_columns {
            let Some(folded) = snap
                .columns
                .iter()
                .find(|candidate| candidate.name == *column)
            else {
                return Err(FoldError::MissingColumn {
                    table: table.to_string(),
                    column: column.clone(),
                });
            };
            let generated = folded.identity.is_some()
                || (dialect == SqlDialect::Sqlite
                    && sqlite_folded_rowid_generation(snap, old_columns, column));
            let keeps_identity_contract = match dialect {
                SqlDialect::Postgres => action
                    .target_columns()
                    .is_some_and(|target| target.contains(column)),
                SqlDialect::Mysql | SqlDialect::Sqlite => action
                    .target_columns()
                    .is_some_and(|target| target == [column.as_str()]),
            };
            if generated && !keeps_identity_contract && !drop_identity_from.contains(column) {
                return Err(FoldError::InvalidPrimaryKeyIdentityTransition {
                    table: table.to_string(),
                    column: column.clone(),
                    reason: "generated column would no longer satisfy the target primary-key contract; list it in dropIdentityFrom",
                });
            }
        }
    }

    let reusable_candidate = (dialect == SqlDialect::Postgres)
        .then(|| {
            action.target_columns().and_then(|target| {
                reusable_postgres_primary_index(
                    snap,
                    target,
                    current.as_ref().map(|(name, _)| name.as_str()),
                )
            })
        })
        .flatten();
    let replacement_constraint_name = current.as_ref().map_or_else(
        || {
            reusable_candidate
                .clone()
                .unwrap_or_else(|| implicit_name.to_string())
        },
        |(name, _)| name.clone(),
    );
    if let Some((name, _)) = &current {
        snap.constraints
            .retain(|constraint| constraint.name != *name || constraint.kind != "PRIMARY KEY");
        snap.indexes.retain(|index| index.name != *name);
    }
    for column in drop_identity_from {
        if let Some(folded) = snap
            .columns
            .iter_mut()
            .find(|candidate| candidate.name == *column)
        {
            folded.identity = None;
            if folded.value_format.is_none()
                && !folded.data_type.eq_ignore_ascii_case("uuid")
                && !matches!(
                    folded.id_default,
                    Some(crate::model::snapshot::IdDefaultSnapshot::Nextval(_))
                )
            {
                folded.id_default = None;
            }
        }
    }
    if let Some(candidate) = reusable_candidate {
        snap.indexes.retain(|index| index.name != candidate);
    }
    if let Some(target) = action.target_columns() {
        push_primary_key_snapshot(snap, target, &replacement_constraint_name);
    }
    snap.constraints
        .sort_by(|left, right| left.name.cmp(&right.name));
    snap.indexes
        .sort_by(|left, right| left.name.cmp(&right.name));
    Ok(())
}

/// Replay an ordered [`Op`] list into the current logical [`SchemaSnapshot`].
/// Pure, offline, NO DB I/O — the offline companion of the live
/// [`snapshot_schema`](crate::apply::drift::snapshot_schema).
///
/// `dialect` selects the per-dialect shaping the shared builder applies (PG vs
/// SQLite FTS folding, etc.); `project_schema` is embedded in FK `definition`s
/// (`REFERENCES <schema>.<target>(id)`) — pass the schema the live DB is
/// introspected under for the round-trip equality to hold. `effective` is the
/// explicit policy used to recognize each resolved create-table injection prefix;
/// the fold never supplies an ambient system shape.
///
/// DML ops fold to no-ops; an incoherent stream is a structured [`FoldError`].
///
/// # Errors
/// See [`FoldError`] for the closed set of fail-closed conditions.
pub fn fold_ops(
    ops: &[Op],
    dialect: SqlDialect,
    project_schema: &str,
    effective: &EffectivePolicy,
) -> Result<SchemaSnapshot, FoldError> {
    fold_ops_onto(
        &SchemaSnapshot::default(),
        ops,
        dialect,
        project_schema,
        effective,
    )
}

/// Replay an ordered [`Op`] list on top of an existing logical snapshot.
///
/// This is the catalog-seeded form of [`fold_ops`]. It uses the same exhaustive
/// structural replay, including dialectal selection and DML no-ops, while
/// preserving objects that predate the supplied migration set.
///
/// # Errors
/// See [`FoldError`] for incoherent transitions relative to `base`.
pub fn fold_ops_onto(
    base: &SchemaSnapshot,
    ops: &[Op],
    dialect: SqlDialect,
    project_schema: &str,
    effective: &EffectivePolicy,
) -> Result<SchemaSnapshot, FoldError> {
    let mut tables: BTreeMap<String, TableSnapshot> = base.tables.clone();
    // Per-table RLS, carried alongside `tables` because it lives on the schema
    // snapshot rather than on TableSnapshot. Seeded from the base so a fold onto an
    // existing snapshot keeps what the base already knew.
    let mut table_rls: BTreeMap<String, bool> = base.table_rls.clone();
    let mut partitions: BTreeMap<String, PartitionSnapshot> = base.partitions.clone();
    let mut attached_partition_tables: BTreeMap<String, TableSnapshot> = BTreeMap::new();
    let mut created_partition_comments: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut views: BTreeMap<String, ViewSnapshot> = base.views.clone();
    let mut sequences: BTreeMap<String, SequenceSnapshot> = base.sequences.clone();
    let mut named_types = NamedTypeRegistry::default();
    let mut named_type_snapshots: BTreeMap<String, NamedTypeSnapshot> = base.named_types.clone();
    let mut roles: BTreeMap<String, RoleSnapshot> = base.roles.clone();
    let mut schemas: BTreeMap<String, SchemaObjectSnapshot> = base.schemas.clone();
    let mut extensions: BTreeMap<String, ExtensionSnapshot> = base.extensions.clone();
    let mut functions: BTreeMap<FunctionKey, FunctionSnapshot> = base.functions.clone();
    let mut policies: BTreeMap<PolicyKey, PolicySnapshot> = base.policies.clone();
    let mut triggers: BTreeMap<TriggerKey, TriggerSnapshot> = base.triggers.clone();

    let replay_ops = flatten_dialectal_ops(ops, dialect)?;
    for op in replay_ops {
        match op {
            Op::CreateEnum { name, values, .. } => {
                named_types
                    .create_enum(name, project_schema, values)
                    .map_err(fold_named_type_error)?;
                if dialect.supports(Capability::MaterializedEnumType) {
                    named_type_snapshots.insert(
                        name.clone(),
                        NamedTypeSnapshot {
                            kind: "enum".to_string(),
                            comment: None,
                        },
                    );
                }
            }
            Op::DropEnum { name, .. } => {
                named_types.drop_enum(name);
                named_type_snapshots.remove(name);
            }
            Op::CreateDomain {
                name,
                as_type,
                check,
                default,
                not_null,
                ..
            } => {
                named_types
                    .create_domain(
                        name,
                        project_schema,
                        as_type,
                        check,
                        default,
                        not_null.unwrap_or(false),
                    )
                    .map_err(fold_named_type_error)?;
                if dialect.supports(Capability::MaterializedDomainType) {
                    named_type_snapshots.insert(
                        name.clone(),
                        NamedTypeSnapshot {
                            kind: "domain".to_string(),
                            comment: None,
                        },
                    );
                }
            }
            Op::DropDomain { name, .. } => {
                named_types.drop_domain(name);
                named_type_snapshots.remove(name);
            }
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
                if sequences.contains_key(name) {
                    return Err(FoldError::DuplicateSequence(name.clone()));
                }
                let snapshot = fold_create_sequence_snapshot(CreateSequenceFoldInput {
                    as_type: as_type.as_ref(),
                    increment: *increment,
                    start: *start,
                    min_value,
                    max_value,
                    cache: *cache,
                    cycle: *cycle,
                    owned_by,
                })?;
                sequences.insert(name.clone(), snapshot);
            }
            Op::AlterSequence {
                name,
                increment,
                min_value,
                max_value,
                cache,
                cycle,
                owned_by,
                ..
            } => {
                let seq = sequences
                    .get_mut(name)
                    .ok_or_else(|| FoldError::MissingSequence(name.clone()))?;
                apply_alter_sequence_snapshot(
                    seq, *increment, min_value, max_value, *cache, *cycle, owned_by,
                )?;
            }
            Op::DropSequence { name, .. } => {
                if sequences.remove(name).is_none() {
                    return Err(FoldError::MissingSequence(name.clone()));
                }
            }
            Op::CreateTable {
                name,
                columns,
                primary_key,
                constraints,
                indexes,
                partition_by,
                runtime_options,
                schema,
                ..
            } => {
                // A new table starts with row-level security OFF. Seeded so the
                // expected map is shaped like the live one, which records every table
                // it sees - absent-vs-false would otherwise read as a change.
                //
                // POSTGRESQL ONLY. SQLite and MySQL have no row-level security and
                // their introspection leaves the map empty, so seeding there would
                // make the folded snapshot differ from the live one for every table.
                if matches!(dialect, SqlDialect::Postgres) {
                    table_rls.insert(name.clone(), false);
                }
                if tables.contains_key(name) {
                    return Err(FoldError::DuplicateTable(name.clone()));
                }
                if views.contains_key(name) {
                    return Err(FoldError::DuplicateView(name.clone()));
                }
                if partitions.contains_key(name) {
                    return Err(FoldError::DuplicateTable(name.clone()));
                }
                let effective_schema = schema.as_deref().unwrap_or(project_schema);
                let desc = create_table_descriptor(name, columns, runtime_options.as_ref());
                let resolved_inject = ResolvedInject::for_table(effective, effective_schema, name)
                    .map_err(|error| FoldError::Render(error.to_string()))?;
                let mut snap = build_resolved_table_snapshot(
                    effective_schema,
                    &desc,
                    dialect,
                    &resolved_inject,
                )?;
                snap.partition_by = partition_by.clone();
                if let Some(pk) = primary_key {
                    let primary_key_name = implicit_primary_key_name(
                        name,
                        dialect,
                        &tables,
                        &partitions,
                        &views,
                        &sequences,
                    );
                    push_primary_key_snapshot(&mut snap, pk, &primary_key_name);
                }
                apply_fold_author_type_overrides_to_snapshot(name, columns, &mut snap, dialect)?;
                apply_fold_structured_defaults_to_snapshot(name, columns, &mut snap, dialect)?;
                apply_fold_named_type_metadata(
                    name,
                    columns,
                    &mut snap,
                    &named_types,
                    dialect,
                    effective_schema,
                    effective,
                )?;
                apply_fold_uuid_metadata(columns, &mut snap, dialect, effective_schema)?;
                apply_fold_collation_metadata(columns, &mut snap, dialect)?;
                apply_fold_value_format_metadata(columns, &mut snap, dialect, effective_schema)?;
                apply_fold_id_default_metadata(columns, &mut snap, dialect, effective_schema)?;
                fold_create_table_specs(
                    name,
                    effective_schema,
                    &mut snap,
                    constraints,
                    indexes,
                    dialect,
                )?;
                tables.insert(name.clone(), snap);
            }
            Op::CreatePartition {
                name, of, bounds, ..
            } => {
                let parent = tables
                    .get(of)
                    .ok_or_else(|| FoldError::MissingTable(of.clone()))?;
                if parent.partition_by.is_none() {
                    return Err(FoldError::Unsupported(
                        "createPartition parent table is not partitioned",
                    ));
                }
                if tables.contains_key(name) || partitions.contains_key(name) {
                    return Err(FoldError::DuplicateTable(name.clone()));
                }
                attached_partition_tables.remove(name);
                created_partition_comments.remove(name);
                partitions.insert(
                    name.clone(),
                    PartitionSnapshot {
                        of: of.clone(),
                        bounds: bounds.clone(),
                    },
                );
            }
            // HOLE: the fold models attachment, not PostgreSQL's ATTACH preconditions.
            // The server additionally refuses a child that carries an identity column
            // ("The new partition may not contain an identity column"), whose column
            // names/types/order differ from the parent's, that lacks a NOT NULL the
            // parent has, that is missing a parent CHECK, that is itself partitioned,
            // or that is already a partition elsewhere. None of those is checked here.
            //
            // This is a hole with a floor rather than an omission: one ATTACH
            // precondition is row-level ("partition constraint is violated by some
            // row") and can never be decided offline, so the server stays the
            // enforcing layer regardless of how much the fold learns to model.
            //
            // Rejecting here was considered and declined. A fold-side identity check
            // would refuse a history PostgreSQL accepts, because `Op::PgRaw` folds to
            // nothing and `ALTER TABLE ... ALTER COLUMN ... DROP IDENTITY` is the only
            // way to clear identity from a column that is not part of a primary-key
            // change - `drop_identity_from` rides on `AlterPrimaryKeyAction` alone. The
            // fold error would fail the deploy, whereas letting the server refuse costs
            // only a late diagnosis: the apply rolls back and writes no journal row.
            //
            // The resulting asymmetry is deliberate. Detach STRIPS identity below,
            // because the server drops it on DETACH, so the fold asserts on the way out
            // an invariant it does not enforce on the way in.
            Op::AttachPartition {
                parent,
                name,
                bound,
                ..
            } => {
                let parent_snap = tables
                    .get(parent)
                    .ok_or_else(|| FoldError::MissingTable(parent.clone()))?;
                if parent_snap.partition_by.is_none() {
                    return Err(FoldError::Unsupported(
                        "attachPartition parent table is not partitioned",
                    ));
                }
                if partitions.contains_key(name) {
                    return Err(FoldError::DuplicateTable(name.clone()));
                }
                let attached = tables
                    .remove(name)
                    .ok_or_else(|| FoldError::MissingTable(name.clone()))?;
                attached_partition_tables.insert(name.clone(), attached);
                partitions.insert(
                    name.clone(),
                    PartitionSnapshot {
                        of: parent.clone(),
                        bounds: bound.clone(),
                    },
                );
            }
            Op::DetachPartition { parent, name, .. } => {
                let parent_snap = tables
                    .get(parent)
                    .ok_or_else(|| FoldError::MissingTable(parent.clone()))?;
                if parent_snap.partition_by.is_none() {
                    return Err(FoldError::Unsupported(
                        "detachPartition parent table is not partitioned",
                    ));
                }
                let partition = partitions
                    .get(name)
                    .ok_or_else(|| FoldError::MissingTable(name.clone()))?;
                if &partition.of != parent {
                    return Err(FoldError::Unsupported(
                        "detachPartition child belongs to a different parent",
                    ));
                }
                let attached = attached_partition_tables.remove(name);
                let created_comment = created_partition_comments.remove(name).flatten();
                partitions.remove(name);
                if let Some(mut detached) = attached {
                    detached.partition_by = None;
                    for column in &mut detached.columns {
                        if column.identity.take().is_some() {
                            column.id_default = None;
                        }
                    }
                    tables.insert(name.clone(), detached);
                } else {
                    let mut detached = parent_snap.clone();
                    detached.partition_by = None;
                    detached.stored_create_sql = None;
                    detached.comment = created_comment;
                    for column in &mut detached.columns {
                        if column.identity.take().is_some() {
                            column.id_default = None;
                        }
                        column.comment = None;
                    }
                    for constraint in &mut detached.constraints {
                        constraint.comment = None;
                    }

                    let parent_indexes = std::mem::take(&mut detached.indexes);
                    if parent_indexes.iter().any(|index| {
                        index
                            .elements
                            .iter()
                            .any(|element| matches!(element, IndexElementSnapshot::Expr(_)))
                    }) {
                        return Err(FoldError::Unsupported(
                            "detachPartition cannot derive a created partition's expression-index clone name",
                        ));
                    }

                    tables.insert(name.clone(), detached);
                    let mut renamed_constraint_indexes = BTreeMap::new();
                    for mut index in parent_indexes {
                        let constraint_kind = tables[name]
                            .constraints
                            .iter()
                            .find(|constraint| constraint.name == index.name)
                            .map(|constraint| constraint.kind.clone());
                        let generated_name = match constraint_kind.as_deref() {
                            Some("PRIMARY KEY") => {
                                let natural = format!("{name}_pkey");
                                if crate::plan::author::cap_ident_name(&natural) != natural {
                                    return Err(FoldError::Unsupported(
                                        "detachPartition cannot derive an overlong created-partition clone name",
                                    ));
                                }
                                implicit_primary_key_name(
                                    name,
                                    dialect,
                                    &tables,
                                    &partitions,
                                    &views,
                                    &sequences,
                                )
                            }
                            Some("UNIQUE") => {
                                let natural = format!("{name}_{}_key", index.columns.join("_"));
                                if crate::plan::author::cap_ident_name(&natural) != natural {
                                    return Err(FoldError::Unsupported(
                                        "detachPartition cannot derive an overlong created-partition clone name",
                                    ));
                                }
                                let base = derived_constraint_name(name, &index.columns, "key");
                                allocate_implicit_relation_name(
                                    &base,
                                    dialect,
                                    &tables,
                                    &partitions,
                                    &views,
                                    &sequences,
                                )
                            }
                            _ => {
                                let columns = index
                                    .elements
                                    .iter()
                                    .map(|element| match element {
                                        IndexElementSnapshot::Column { name, .. } => name.clone(),
                                        IndexElementSnapshot::Expr(_) => unreachable!(
                                            "expression indexes fail closed before clone naming"
                                        ),
                                    })
                                    .collect::<Vec<_>>();
                                let natural = format!("{name}_{}_idx", columns.join("_"));
                                if crate::plan::author::cap_ident_name(&natural) != natural {
                                    return Err(FoldError::Unsupported(
                                        "detachPartition cannot derive an overlong created-partition clone name",
                                    ));
                                }
                                let base = if let [column] = columns.as_slice() {
                                    non_unique_index_name(name, column)
                                } else {
                                    derived_constraint_name(name, &columns, "idx")
                                };
                                allocate_implicit_relation_name(
                                    &base,
                                    dialect,
                                    &tables,
                                    &partitions,
                                    &views,
                                    &sequences,
                                )
                            }
                        };
                        // Reject native PostgreSQL truncation rather than compare a
                        // hash-capped name. This does not cover overlong clone bases
                        // or collision suffixes that exceed NAMEDATALEN.
                        if crate::plan::author::cap_ident_name(&generated_name) != generated_name {
                            return Err(FoldError::Unsupported(
                                "detachPartition cannot derive an overlong created-partition clone name",
                            ));
                        }
                        if matches!(constraint_kind.as_deref(), Some("PRIMARY KEY" | "UNIQUE")) {
                            renamed_constraint_indexes
                                .insert(index.name.clone(), generated_name.clone());
                        }
                        index.name = generated_name;
                        index.comment = None;
                        tables
                            .get_mut(name)
                            .expect("detached child was inserted before cloning indexes")
                            .indexes
                            .push(index);
                    }

                    let detached = tables
                        .get_mut(name)
                        .expect("detached child remains present after cloning indexes");
                    for constraint in &mut detached.constraints {
                        if matches!(constraint.kind.as_str(), "PRIMARY KEY" | "UNIQUE") {
                            constraint.name = renamed_constraint_indexes
                                .get(&constraint.name)
                                .cloned()
                                .ok_or(FoldError::Unsupported(
                                    "detachPartition parent constraint has no backing index",
                                ))?;
                        }
                    }
                    detached
                        .constraints
                        .sort_by(|left, right| left.name.cmp(&right.name));
                    detached
                        .indexes
                        .sort_by(|left, right| left.name.cmp(&right.name));
                }
            }
            Op::DropPartition { parent, name, .. } => {
                let partition = partitions
                    .get(name)
                    .ok_or_else(|| FoldError::MissingTable(name.clone()))?;
                if &partition.of != parent {
                    return Err(FoldError::Unsupported(
                        "dropPartition child belongs to a different parent",
                    ));
                }
                if partitions.remove(name).is_none() {
                    return Err(FoldError::MissingTable(name.clone()));
                }
                attached_partition_tables.remove(name);
                created_partition_comments.remove(name);
            }
            // ROW-LEVEL SECURITY. Recorded on the EXPECTED side so it can be compared
            // with the live catalog; without this the live map would carry every
            // table and the expected map none, and the diff would report permanent
            // drift instead of real change.
            Op::SetRls { table, enabled, .. } => {
                if let Some(enabled) = enabled {
                    table_rls.insert(table.clone(), *enabled);
                }
            }
            Op::SetTableOptions { table, options, .. } => {
                let snap = table_mut(&mut tables, table)?;
                if let Some(soft_delete) = options.soft_delete {
                    snap.runtime_options.soft_delete = soft_delete;
                }
                if let Some(versioning) = options.versioning {
                    snap.runtime_options.versioning = versioning;
                }
                if let Some(strictness) = options.strictness {
                    snap.runtime_options.strictness = strictness;
                }
            }
            Op::DropTable { table, .. } => {
                // The table is gone, so its RLS entry goes with it - the same
                // obligation the snapshot map itself has.
                table_rls.remove(table);
                // Remove ONLY the target table. We do NOT cascade-drop FK constraints
                // on OTHER tables that reference it, and that is faithful (not a hole):
                // the lower IGNORES the op's `cascade` flag (`render::lower`
                // `lower_drop_table` emits `DROP TABLE <t>`, never `… CASCADE`), so a
                // drop of a still-referenced table FAILS at apply. A folded state with a
                // referencing FK left dangling is therefore UNREACHABLE — the engine
                // never produces it — so the fold needs no cascade here.
                //
                // REVISIT if/when `Op::DropTable.cascade` is ever threaded into the
                // render path: a real `DROP TABLE … CASCADE` WOULD drop referencing FKs
                // on other tables, and this arm would then have to mirror that cascade.
                if tables.remove(table).is_none() {
                    return Err(FoldError::MissingTable(table.clone()));
                }
                partitions.retain(|_, partition| &partition.of != table);
                attached_partition_tables.retain(|name, _| partitions.contains_key(name));
                created_partition_comments.retain(|name, _| partitions.contains_key(name));
            }
            Op::RenameTable { table, to, .. } => {
                // Re-key the RLS entry with the table, exactly as the snapshot map is
                // re-keyed below: RLS is a property of the relation, not of its name.
                if let Some(rls) = table_rls.remove(table) {
                    table_rls.insert(to.clone(), rls);
                }
                // A whole-table rename moves the snapshot WHOLESALE from the old key
                // to the new one — every column / index / constraint / facet is
                // preserved (a `TableSnapshot` carries no own `name`; the BTreeMap
                // KEY is the table name, so a re-key IS the rename). A later op
                // referencing the NEW name now resolves; one referencing the OLD name
                // errors (`MissingTable`), exactly the contract the column rename has.
                //
                // The two structural guards mirror the column-rename arm:
                //   - the SOURCE must exist (fail-closed `MissingTable`);
                //   - the TARGET must NOT already exist (fail-closed `DuplicateTable`)
                //     — a rename cannot collide with a live table.
                if tables.contains_key(to) {
                    return Err(FoldError::DuplicateTable(to.clone()));
                }
                if views.contains_key(to) {
                    return Err(FoldError::DuplicateView(to.clone()));
                }
                let mut snap = tables
                    .remove(table)
                    .ok_or_else(|| FoldError::MissingTable(table.clone()))?;
                // A generated expression may QUALIFY its column references with the
                // enclosing table (`line_items.qty_on_hand`). That qualifier is an
                // identity the rename moves, so it follows the table for the same
                // reason the column-rename arm follows a column - and through the same
                // AST walk, which cannot touch a string literal.
                crate::render::declarative::rename_table_in_generated_columns(
                    &mut snap, table, to, dialect,
                )
                .map_err(|error| FoldError::Render(error.to_string()))?;
                tables.insert(to.clone(), snap);
                // Live PG/SQLite re-target every INCOMING FK to the renamed table by
                // OID, so the FK `definition` in OTHER tables now reports the NEW
                // name. Mirror that, or the renamed table phantom-drifts for every
                // table that referenced it.
                rewrite_incoming_fk_targets(&mut tables, project_schema, table, to);
            }
            Op::AddColumn {
                table,
                column,
                ty,
                nullable,
                default,
                value_format,
                vector_metric,
                case_sensitive,
                mask,
                generated,
                identity,
                ..
            } => {
                let snap = table_mut(&mut tables, table)?;
                if snap.columns.iter().any(|c| &c.name == column) {
                    return Err(FoldError::DuplicateColumn {
                        table: table.clone(),
                        column: column.clone(),
                    });
                }
                // Thread the carried facets so the SNAPSHOT for a vector
                // / masked added column renders the metric opclass / `zero-migrate:mask` sentinel
                // (this snapshot feeds the `--sql` plan preview + the apply path), and grow
                // the `<col>_masked` sibling for a masked column so the offline fold matches
                // the live apply.
                let (col, masked_sibling) = add_column_snapshot(
                    table,
                    column,
                    ty,
                    *nullable,
                    default.as_ref(),
                    *vector_metric,
                    *case_sensitive,
                    *mask,
                    generated.as_ref(),
                    *identity,
                    project_schema,
                    dialect,
                    effective,
                )?;
                let source_col = IrColumn {
                    name: column.clone(),
                    ty: ty.clone(),
                    nullable: *nullable,
                    default: default.clone(),
                    unique: None,
                    value_format: value_format.clone(),
                    references: None,
                    id_prefix: None,
                    collation: None,
                    vector_metric: *vector_metric,
                    case_sensitive: *case_sensitive,
                    mask: *mask,
                    generated: generated.clone(),
                    identity: *identity,
                };
                let mut col = col;
                apply_fold_named_type_column_metadata(
                    table,
                    &source_col,
                    &mut col,
                    &named_types,
                    dialect,
                    project_schema,
                    effective,
                )?;
                apply_fold_uuid_column_metadata(&source_col, &mut col, dialect, project_schema)?;
                apply_fold_value_format_column_metadata(
                    &source_col,
                    &mut col,
                    dialect,
                    project_schema,
                )?;
                apply_fold_id_default_column_metadata(
                    &source_col,
                    &mut col,
                    dialect,
                    project_schema,
                );
                snap.columns.push(col);
                if let Some(sibling) = masked_sibling {
                    snap.columns.push(sibling);
                }
                snap.columns.sort_by(|a, b| a.name.cmp(&b.name));
            }
            Op::DropColumn { table, column, .. } => {
                let snap = table_mut(&mut tables, table)?;
                let before = snap.columns.len();
                snap.columns.retain(|c| &c.name != column);
                if snap.columns.len() == before {
                    return Err(FoldError::MissingColumn {
                        table: table.clone(),
                        column: column.clone(),
                    });
                }
                // PG's `ALTER TABLE … DROP COLUMN` AUTO-CASCADES to every dependent
                // index and UNIQUE/FK constraint that references the dropped column
                // (and a multi-column index/constraint that merely PARTIALLY covers
                // it is dropped whole, identically). Live introspection therefore
                // shows none of them after the drop, so the fold MUST mirror the
                // cascade — otherwise a phantom index/constraint survives in the
                // fold and `fold_ops != snapshot_schema(live)`, corrupting
                // gen-types and producing permanent phantom drift.
                //
                // (1) Drop every index covering the column. An index carries column
                //     names in three STRUCTURED places, and PG cascades on any of
                //     them: `columns` (the raw key-column list), the `Column` variant
                //     of `elements` (the same keys, carrying sort order / opclass),
                //     and `include` (the non-key payload, whose attributes are
                //     `indkey` entries past `indnkeyatts` and so depend on the column
                //     exactly as a key does - measured on PG 18.4:
                //     `CREATE INDEX i ON t (b) INCLUDE (a); ALTER TABLE t DROP COLUMN a`
                //     leaves no `i` in `pg_indexes`). All three are exact names, so an
                //     exact compare suffices; a multi-column index partially covering
                //     the column is dropped whole, identically. This mirrors the three
                //     fields the `RenameColumn` arm below rewrites.
                //
                //     The other two column-bearing sites - `IndexSnapshot::predicate`
                //     (a partial index's `WHERE`) and the `Expr` variant of `elements`
                //     (an expression key) - cascade in PG too: measured on PG 18.4,
                //     both `CREATE INDEX i ON t (b) WHERE (a > 0)` and
                //     `CREATE INDEX i ON t ((a + 1))` vanish from `pg_indexes` on
                //     `DROP COLUMN a`. They are RENDERED SQL TEXT here, not names, and
                //     matching a name inside rendered SQL is the trap the CHECK cascade
                //     below exists to avoid: measured on PG 18.4,
                //     `CREATE INDEX i ON t (note) WHERE (note <> 'a')` SURVIVES
                //     `DROP COLUMN a`, so a text match would drop an index PG KEPT -
                //     worse than the phantom it fixes. So they cascade off
                //     `IndexSnapshot::expr_cascade_columns`, the structural column set
                //     `create_index_snapshot` collects from the CLOSED `Expr` via
                //     `render::dml::expr_column_refs` - the same discipline
                //     `ConstraintSnapshot::cascade_columns` uses for a CHECK, and the
                //     same selected-leg-only walk, so a column named solely by an
                //     inactive `dialect()` leg never cascades.
                //
                //     The provenance covers ONLY those two sites and is UNIONED with
                //     the three exact-name lists rather than replacing them: it is
                //     `None` on every plain column-list index and on every producer
                //     that cannot record it (live introspection), and those must keep
                //     cascading on the names exactly as before.
                snap.indexes.retain(|i| {
                    !i.columns.iter().any(|c| c == column)
                        && !i.elements.iter().any(|e| {
                            matches!(e, IndexElementSnapshot::Column { name, .. } if name == column)
                        })
                        && !i.include.iter().any(|c| c == column)
                        && !i
                            .expr_cascade_columns
                            .as_ref()
                            .is_some_and(|cols| cols.iter().any(|c| c == column))
                });
                // (2) Drop every constraint whose LOCAL column list contains the
                //     column. A producer that recorded `cascade_columns` is believed
                //     verbatim - that list is structural provenance, collected from
                //     the closed AST rather than read back out of rendered SQL, and
                //     an empty list means the constraint reads no column at all and
                //     never cascades. Everything else falls back to parsing the
                //     leading parenthesized group of the definition: UNIQUE
                //     (`UNIQUE (cols)`) and FOREIGN KEY
                //     (`FOREIGN KEY (cols) REFERENCES …`) both carry their local
                //     columns there, and the system `<table>_pkey` is
                //     `PRIMARY KEY (id)`, so none false-matches a non-`id` user
                //     column. A CHECK cannot use that fallback - its leading group is
                //     the EXPRESSION, so the parse never matches and the constraint
                //     survives a drop PostgreSQL cascaded - hence the refusal below
                //     rather than a silent guess. Collect the dropped constraint names
                //     first to cascade their implicit unique indexes (mirror the
                //     DropConstraint index-cascade below).
                //
                //     Capture (name, kind) so the implicit-index cascade can
                //     discriminate by kind: only UNIQUE / PRIMARY KEY back a same-named
                //     index PG cascades; a FOREIGN KEY backs none. Cascading the index
                //     by a FK's name would wrongly phantom-drop an INDEPENDENT user
                //     index that merely shares the FK's name (PG allows a FK and an
                //     index to share a name — see the DropConstraint arm), so the
                //     implicit-index retain below is kind-gated for safety.
                let mut dropped_constraints: Vec<(String, String)> = Vec::new();
                for c in &snap.constraints {
                    let cascades = match &c.cascade_columns {
                        Some(cols) => cols.iter().any(|local| local == column),
                        None if c.kind == "CHECK" => {
                            return Err(FoldError::CheckCascadeColumnsMissing {
                                table: table.clone(),
                                name: c.name.clone(),
                            })
                        }
                        None => constraint_local_columns_contain(&c.definition, column),
                    };
                    if cascades {
                        dropped_constraints.push((c.name.clone(), c.kind.clone()));
                    }
                }
                snap.constraints
                    .retain(|c| !dropped_constraints.iter().any(|(n, _)| n == &c.name));
                // A UNIQUE/PK constraint backs an implicit unique index of the SAME
                // name (a FK backs none), which PG cascades with the constraint —
                // remove it identically to the DropConstraint arm, kind-gated so a
                // same-named independent user index behind a FK is NOT phantom-dropped.
                let implicit_index_names: Vec<&String> = dropped_constraints
                    .iter()
                    .filter(|(_, kind)| matches!(kind.as_str(), "UNIQUE" | "PRIMARY KEY"))
                    .map(|(n, _)| n)
                    .collect();
                snap.indexes
                    .retain(|i| !implicit_index_names.contains(&&i.name));
            }
            Op::RenameColumn {
                table, from, to, ..
            } => {
                let snap = table_mut(&mut tables, table)?;
                // A pure rename keeps the column's type/nullable/default/sentinels;
                // only the NAME changes (the IR carries `ty` for the live-rename type
                // reconciliation the lower does, but the fold trusts the EXISTING
                // column's type — a pure rename cannot change type, the same stance
                // `lower_rename` takes: "the live column is the single authoritative
                // type source"). So we do NOT re-derive from `ty`.
                //
                // gen-types BOUNDARY:
                // the IR rename lowers to an online expand-contract whose CONTRACT
                // (drop the `from` column) is a SEPARATE later deploy. Between expand
                // and contract, live PG carries BOTH the `from` and `to` columns while
                // this fold (which collapses the rename to the final `to` name) shows
                // only `to`. That divergence is correctly EXCLUDED from the fold==live
                // equality oracle and is acceptable for gen-types (the `to` column
                // exists post-expand), but in the migration-first model the fold is the
                // SOLE source of truth for gen-types — so generated types over a
                // mid-expand migration set reflect the POST-EXPAND logical shape (final
                // `to` name). A live mid-expand DB should be exercised e2e to
                // confirm gen-types reads/writes resolve. No action here.
                if snap.columns.iter().any(|c| &c.name == to) {
                    return Err(FoldError::RenameCollision {
                        table: table.clone(),
                        to: to.clone(),
                    });
                }
                let col = snap
                    .columns
                    .iter_mut()
                    .find(|c| &c.name == from)
                    .ok_or_else(|| FoldError::MissingColumn {
                        table: table.clone(),
                        column: from.clone(),
                    })?;
                col.name = to.clone();
                snap.columns.sort_by(|a, b| a.name.cmp(&b.name));
                // A generated expression READS other columns, so the rename has to
                // follow it. PostgreSQL holds the expression as a parse tree over
                // attribute NUMBERS, so `pg_get_expr` deparses the NEW name the
                // instant the rename commits (measured on PG 18.4:
                // `("qty_on_hand" + 1)` becomes `(quantity + 1)`), and the descriptor
                // fold has rewritten it all along. This snapshot used to keep the old
                // name, which is not merely cosmetic: on SQLite a rename is a table
                // REBUILD, and `render_create_table_sqlite_rebuild` renders the
                // new-table CREATE FROM THIS SNAPSHOT for exactly the tables that have
                // a generated column, so the stale body emitted
                // `GENERATED ALWAYS AS (("qty_on_hand" + 1))` over a table with no
                // such column.
                //
                // The rewrite walks the closed `Expr` the snapshot now keeps beside
                // the rendering, so it matches column REFERENCES and cannot corrupt a
                // string literal that spells the old name - the discrimination that
                // rules out the text substitution refused for the CHECK body and the
                // index predicate below.
                crate::render::declarative::rename_column_in_generated_columns(
                    snap, table, from, to, dialect,
                )
                .map_err(|error| FoldError::Render(error.to_string()))?;
                // An inline CHECK body names the column it guards, so the rename has
                // to follow it for the same reason. This one CANNOT be re-rendered
                // from an AST - `inline_checks` is rendered SQL text whose producers
                // keep none - so it is rewritten by a quoted-run walk that copies a
                // string literal through whole and leaves an unparseable body stale
                // (`rename_column_in_inline_checks`). Without it, a folded snapshot
                // carrying a rename is a broken rebuild source for every LATER
                // migration in the same history, which the rebuild's own rewrite
                // cannot repair: it only knows the rename it is lowering.
                crate::render::declarative::rename_column_in_inline_checks(snap, from, to, dialect);
                // `cascade_columns` names columns, so a rename has to follow it.
                // PostgreSQL renames the attribute in place and leaves every
                // constraint's `conkey` pointing at it, so a later drop of the NEW
                // name still cascades. Without this rewrite the provenance would
                // still name `from`, the drop of `to` would find no match, and
                // `rename qty -> amount; drop amount` would leave a phantom CHECK
                // behind - the exact drift this provenance exists to prevent.
                //
                // `ConstraintSnapshot::definition` names the column too, and drift
                // DOES compare it for every kind but EXCLUDE and CHECK
                // (`apply::drift::constraint_definition_is_comparable`). PG holds
                // `conkey` as attribute NUMBERS, so `pg_get_constraintdef` deparses
                // `UNIQUE (b)` / `FOREIGN KEY (b) REFERENCES ...` / `PRIMARY KEY (b)`
                // the moment the rename commits (measured on PG 18.4) while the fold
                // kept the old rendering and reported drift on the next introspection.
                //
                // Only the LEADING PARENTHESIZED GROUP is re-rendered, and only for the
                // three kinds whose leading group is a LOCAL COLUMN LIST. That group
                // cannot contain a string literal, so the trap that rules out text
                // matching for a CHECK body is structurally unreachable - see
                // `rename_constraint_definition_column`, which also carries the
                // round-trip guard that leaves a definition the parser mishandles STALE
                // rather than CORRUPT. A CHECK body stays stale here (it reads its
                // columns out of an expression, needs the `Expr` the snapshot
                // discarded, and reports nothing because the differ exempts it).
                for constraint in &mut snap.constraints {
                    if let Some(cascade_columns) = &mut constraint.cascade_columns {
                        for local in cascade_columns.iter_mut() {
                            if local == from {
                                local.clone_from(to);
                            }
                        }
                        cascade_columns.sort();
                    }
                    if matches!(
                        constraint.kind.as_str(),
                        "UNIQUE" | "PRIMARY KEY" | "FOREIGN KEY"
                    ) {
                        if let Some(definition) =
                            rename_constraint_definition_column(&constraint.definition, from, to)
                        {
                            constraint.definition = definition;
                        }
                    }
                }
                // An index names columns too, and a rename has to follow it for the
                // same reason. `pg_index` references the attribute by `attnum`, never
                // by name, so PG renames the attribute in place and every index over
                // it keeps working under the NEW name: `pg_get_indexdef` spells the new
                // name the instant the rename commits, and a later `DROP COLUMN` of the
                // new name cascades the index away. Without this rewrite the fold drifts
                // twice: the surviving index disagrees with live on its key columns, and
                // `createIndex on a; rename a -> b; drop b` leaves a PHANTOM INDEX (the
                // DropColumn cascade above matches on `IndexSnapshot::columns`, which
                // would still say `a`).
                //
                // The index NAME is deliberately NOT rewritten: PG does NOT rename an
                // index when a column is renamed (measured on PG 18.4 - an index created
                // as `t_a_idx` over `a` is still `t_a_idx` after `a` becomes `b`), so
                // rewriting it here would invent an index live does not have. Key and
                // INCLUDE lists are POSITIONAL, so they are rewritten in place and never
                // re-sorted - unlike `cascade_columns`, whose order carries no meaning.
                //
                // `IndexSnapshot::expr_cascade_columns` names columns too, and follows
                // the rename for the same reason the constraint provenance does: PG
                // keeps `indpred` / `indexprs` pointing at the renamed attribute, so a
                // later drop of the NEW name still cascades the index. Without this,
                // `createIndex WHERE a > 0; rename a -> b; drop b` leaves a PHANTOM
                // partial index - measured against live PG 18.4. Order carries no
                // meaning here (unlike the positional key and INCLUDE lists), so it is
                // re-sorted back to the canonical form `create_index_snapshot` emits.
                //
                // STILL NOT rewritten, and measured stale on PG 18.4: the rendered text
                // in `predicate` and in an `IndexElementSnapshot::Expr` key, both of
                // which drift DOES compare. After `rename a -> b` the fold keeps
                // `("a" > 0)` / `expr:("a" + 1)` while live reports `(b > 0)` /
                // `expr:(b + 1)`. A column LIST cannot fix that - re-rendering needs the
                // `Expr` the snapshot discarded - and swapping the name inside the text
                // is the exact false-positive the provenance above exists to avoid
                // (`WHERE (note <> 'a')` would become `WHERE (note <> 'b')`). That trap
                // is why the `ConstraintSnapshot::definition` rewrite above is confined
                // to a leading COLUMN LIST, where a string literal cannot appear: these
                // two sites are arbitrary expressions and admit no such argument.
                for index in &mut snap.indexes {
                    for column in &mut index.columns {
                        if column == from {
                            column.clone_from(to);
                        }
                    }
                    for element in &mut index.elements {
                        if let IndexElementSnapshot::Column { name, .. } = element {
                            if name == from {
                                name.clone_from(to);
                            }
                        }
                    }
                    for column in &mut index.include {
                        if column == from {
                            column.clone_from(to);
                        }
                    }
                    if let Some(expr_cascade_columns) = &mut index.expr_cascade_columns {
                        for local in expr_cascade_columns.iter_mut() {
                            if local == from {
                                local.clone_from(to);
                            }
                        }
                        expr_cascade_columns.sort();
                    }
                }
                // A FK in ANOTHER table names this column in its REFERENCES tail, and
                // PG follows the rename there too (`confkey` is attribute numbers).
                // That constraint lives outside `snap`, so it needs its own walk - the
                // column-rename analogue of what `rewrite_incoming_fk_targets` does for
                // a table rename.
                rewrite_incoming_fk_column_targets(&mut tables, project_schema, table, from, to);
            }
            Op::SetColumnType {
                table,
                column,
                to_type,
                ..
            } => {
                // Re-derive the new column shape from the new type via the shared
                // builder (so `vector(N)` / `geography(...)` / encrypted-BYTEA
                // spellings match introspection's `canonical_extension_type`). Keep
                // the live `nullable`. The `using` cast is fold-irrelevant (it casts
                // DATA, not shape).
                //
                // FAIL-CLOSED on an encryption-contract change. A plain↔encrypted
                // (or masked) type change rewrites the column's emission contract:
                // its `encryption_sentinel` / `comment_sentinel` — the EXACT fields
                // gen-types reads to drive the AEAD encrypt/decrypt pass. The
                // apply path (`render_alter_column_type`) emits ONLY `ALTER COLUMN
                // … TYPE bytea`, never the `COMMENT ON COLUMN … zero-migrate:enc` an encrypted
                // column needs, so the LIVE DB also lacks the metadata after such an
                // alter. Folding only `data_type` here would carry the OLD (now
                // wrong / stale) sentinel — a silently-wrong snapshot, which the
                // fail-closed contract forbids. Until the apply path can faithfully
                // re-stamp the sentinel, refuse the change (parity with the lower's
                // `using` / SQLite alter refusals). Detection is symmetric:
                //   - the TARGET type carries a sentinel (plain→encrypted/masked), OR
                //   - the SOURCE column carries one (encrypted/masked→anything).
                let (mut new_col, _sibling) = add_column_snapshot(
                    table,
                    column,
                    to_type,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    project_schema,
                    dialect,
                    effective,
                )?;
                if matches!(to_type, ColType::Enum { .. } | ColType::Domain { .. }) {
                    match to_type {
                        ColType::Enum { name, .. }
                            if !dialect.supports(Capability::MaterializedEnumType) =>
                        {
                            return Err(FoldError::NamedTypeUnsupported {
                                kind: "enum",
                                name: name.clone(),
                                reason: "unreachable use-site",
                            });
                        }
                        ColType::Domain { name, .. }
                            if !dialect.supports(Capability::MaterializedDomainType) =>
                        {
                            return Err(FoldError::NamedTypeUnsupported {
                                kind: "domain",
                                name: name.clone(),
                                reason: "unreachable use-site",
                            });
                        }
                        _ => {
                            let source_col = IrColumn {
                                name: column.clone(),
                                ty: to_type.clone(),
                                nullable: None,
                                default: None,
                                unique: None,
                                value_format: None,
                                references: None,
                                id_prefix: None,
                                collation: None,
                                case_sensitive: None,
                                vector_metric: None,
                                mask: None,
                                generated: None,
                                identity: None,
                            };
                            apply_fold_named_type_column_metadata(
                                table,
                                &source_col,
                                &mut new_col,
                                &named_types,
                                dialect,
                                project_schema,
                                effective,
                            )?;
                        }
                    }
                }
                let target_has_sentinel =
                    new_col.encryption_sentinel.is_some() || new_col.comment_sentinel.is_some();
                let snap = table_mut(&mut tables, table)?;
                let col = snap
                    .columns
                    .iter_mut()
                    .find(|c| &c.name == column)
                    .ok_or_else(|| FoldError::MissingColumn {
                        table: table.clone(),
                        column: column.clone(),
                    })?;
                let source_has_sentinel =
                    col.encryption_sentinel.is_some() || col.comment_sentinel.is_some();
                if target_has_sentinel || source_has_sentinel {
                    return Err(FoldError::Unsupported(
                        "setColumnType to/from an encrypted (or masked) column \
                         (the apply path cannot re-stamp the zero-migrate:enc/zero-migrate:mask sentinel; \
                         fail-closed rather than fold a stale encryption contract)",
                    ));
                }
                // FAIL-CLOSED on a VALUE-FORMAT column, for the same reason and by the
                // same test as the sentinel refusal directly above: the apply path
                // emits ONLY `ALTER COLUMN … TYPE`, never the `DROP CONSTRAINT` a
                // TypeID/ULID format contract would need, so the LIVE DB keeps a
                // contract this side can no longer describe either way.
                //
                // MEASURED on live PostgreSQL 18.4, through the real path
                // (`MigrationEngine::apply_plan`, not raw SQL), because the two halves
                // fail differently and neither is fixable by folding:
                //
                //   * To any NON-text target the SERVER REFUSES THE ALTER. The
                //     engine's own `ALTER TABLE … ALTER COLUMN "v" TYPE integer USING
                //     "v"::integer` dies with `function octet_length(integer) does not
                //     exist`, because PostgreSQL re-parses the format CHECK against the
                //     new type. `bigint` and `uuid` fail identically; `bytea` fails with
                //     `collations are not supported by type bytea`. The plan clears
                //     validate AND preview and then dies mid-deploy.
                //
                //   * To a TEXT-family target (`varchar(N)`, `char(N)`, `text`) the
                //     ALTER SUCCEEDS and the CHECK SURVIVES — but PostgreSQL re-parses
                //     it with casts injected (`octet_length((v)::text)`), a spelling
                //     `render::value_format::recover_format_check` does not recognise.
                //     So introspection never projects it back onto `value_format`, and
                //     after `typeId(usr) text → string(50)` structural drift reported
                //     THREE differences that do not exist, on a schema that was exactly
                //     what had been deployed:
                //         collation  expected "pg_catalog.C"  actual ""
                //         format     expected "typeId(usr)"   actual ""
                //         unexpected: constraint <table>_v_check
                //     CLEARING `value_format` does not fix that: the engine-owned CHECK
                //     is still in the database and still unaccounted for.
                //
                // So neither keeping nor clearing is truthful. Until the apply path can
                // drop the format CHECK alongside the type change, refuse. Detection is
                // SOURCE-ONLY, unlike the sentinel test: `Op::SetColumnType` carries no
                // `valueFormat` slot, so a target can never acquire one — the assertion
                // is `new_col.value_format.is_none()`, checked here rather than assumed.
                debug_assert!(new_col.value_format.is_none());
                if col.value_format.is_some() {
                    return Err(FoldError::Unsupported(
                        "setColumnType on a column carrying a value format \
                         (the apply path cannot drop the TypeID/ULID format CHECK; \
                         PostgreSQL refuses the ALTER outright for a non-text target, and \
                         keeps an unrecognisable rewritten CHECK for a text one; \
                         fail-closed rather than fold a stale format contract)",
                    ));
                }
                let source_was_native_uuid =
                    dialect == SqlDialect::Postgres && col.data_type.eq_ignore_ascii_case("uuid");
                col.data_type = new_col.data_type;
                col.ddl_type_override = new_col.ddl_type_override;
                // The remaining TYPE-BOUND facets, taken from `new_col` rather than
                // left behind. Each one is `None`/empty on `new_col` unless `to_type`
                // itself produces it, so this is "re-derive from the target type", not
                // "clear" — the same rule `retype_field_descriptor` states in
                // descriptor terms, and the reasons are recorded there.
                //
                //   * `inline_checks` — the enum / domain / UUID / format CHECKs of the
                //     type the column HAD. Emission-only but it is DDL: the SQLite
                //     rebuild joins it straight into the new table's column
                //     declaration, so a SQLite `enum → int` retype used to leave
                //     `CHECK ("v" IN ('ok', 'bad'))` sitting on an `integer` column —
                //     the shape that made a SQLite rename undeployable (3903a98e).
                //   * `collation` — DRIFT-COMPARED, and PostgreSQL RESETS it: measured,
                //     `text COLLATE "C" → character varying(40)` leaves the catalog
                //     reporting the DEFAULT collation, never `C`. BELT-AND-BRACES
                //     rather than the fix, and said plainly: there are now TWO
                //     fold-side writers of this field — `value_format`'s
                //     `bytewise_catalog_collation` and the `IrColumn::collation`
                //     facet's `apply_fold_collation_metadata`, which calls the same
                //     function (SQLite's `NOCASE` rides on `case_sensitive`). Both
                //     write the SAME bytewise identity, so re-deriving the field from
                //     the TARGET type still clears whichever of them put it there, and
                //     the refusal above still closes the only route a stale collation
                //     had. A retype AWAY from a collated column therefore drops the
                //     collation, which is what PostgreSQL itself does; the column must
                //     re-declare it, and drift says so rather than staying silent.
                //   * `case_sensitive` — DRIFT-COMPARED. On PostgreSQL
                //     case-insensitivity IS the `citext` type, so the retype destroys
                //     it; measured, `citext → character varying(40)` reported
                //     `case_sensitive expected "false" actual ""` forever after.
                col.inline_checks = new_col.inline_checks;
                col.collation = new_col.collation;
                col.case_sensitive = new_col.case_sensitive;
                if matches!(to_type, ColType::Uuid) {
                    // `setColumnType` preserves the live DEFAULT. Entering the UUID
                    // surface must therefore classify that existing default instead
                    // of copying `new_col`'s synthetic absent default.
                    col.id_default = Some(catalog_uuid_id_default(
                        col.default.as_deref(),
                        dialect,
                        None,
                    ));
                } else if source_was_native_uuid {
                    // PostgreSQL's native UUID type is itself an ID-default drift
                    // surface. Leaving it must not retain stale UUID-only metadata.
                    col.id_default = None;
                }
            }
            Op::SetColumnNotNull { table, column, .. } => {
                let snap = table_mut(&mut tables, table)?;
                let col = snap
                    .columns
                    .iter_mut()
                    .find(|c| &c.name == column)
                    .ok_or_else(|| FoldError::MissingColumn {
                        table: table.clone(),
                        column: column.clone(),
                    })?;
                col.nullable = false;
            }
            Op::DropColumnNotNull { table, column, .. } => {
                let snap = table_mut(&mut tables, table)?;
                let col = snap
                    .columns
                    .iter_mut()
                    .find(|c| &c.name == column)
                    .ok_or_else(|| FoldError::MissingColumn {
                        table: table.clone(),
                        column: column.clone(),
                    })?;
                col.nullable = true;
            }
            Op::SetColumnDefault {
                table,
                column,
                value,
                ..
            } => {
                let snap = table_mut(&mut tables, table)?;
                let col = snap
                    .columns
                    .iter_mut()
                    .find(|c| &c.name == column)
                    .ok_or_else(|| FoldError::MissingColumn {
                        table: table.clone(),
                        column: column.clone(),
                    })?;
                let rendered = match value {
                    IrDefault::Literal { .. } => {
                        render_ir_default(value, dialect).map_err(fold_named_type_error)?
                    }
                    IrDefault::Expr { .. } => {
                        render_ir_default(value, dialect).map_err(fold_named_type_error)?
                    }
                    IrDefault::Container { kind } => {
                        render_container_default_for_data_type(*kind, &col.data_type, dialect)
                            .map_err(fold_named_type_error)?
                    }
                    IrDefault::Json { value } => {
                        render_json_default_for_data_type(value, &col.data_type, dialect)
                            .map_err(fold_named_type_error)?
                    }
                    IrDefault::Nextval { .. } => {
                        render_ir_default(value, dialect).map_err(fold_named_type_error)?
                    }
                };
                let tracks_id_default =
                    col.id_default.is_some() || matches!(value, IrDefault::Nextval { .. });
                col.default = Some(rendered);
                if tracks_id_default {
                    // The arms below spell `character varying` BARE but never
                    // `character varying(N)`, which is what the desired side emits
                    // for a bounded string (`schema::query` renders
                    // `character varying({len})`; `render::declarative`'s
                    // `varchar_len_from_data_type` strips exactly that prefix). That
                    // asymmetry is deliberate and load-bearing on the fact that a
                    // BOUNDED string column never reaches here: `tracks_id_default`
                    // needs `id_default` set, which only the value-format path does,
                    // and both value-format builders declare base type `text`
                    // (`ids.typeId` and `ids.ulid` in the authoring DSL). `valueFormat`
                    // has no public setter, so it cannot be attached to a `t.string()`.
                    // The other route in, `identity`/`nextval`, is integer-typed.
                    //
                    // Widening the list would therefore add an arm nothing can select.
                    // If a bounded string ever DOES gain an ID default, this dispatch
                    // silently picks `authored_id_default` where lowering picked
                    // `authored_text_id_default` for the same column, and the fold
                    // diverges from the DDL it is supposed to mirror - so change the
                    // invariant and this comment together, or not at all.
                    let data_type = col.data_type.trim().to_ascii_lowercase();
                    let default = if data_type == "uuid" {
                        authored_uuid_id_default(
                            Some(value),
                            col.default.as_deref(),
                            dialect,
                            Some(project_schema),
                        )
                    } else if data_type == "text"
                        || data_type == "character varying"
                        || data_type == "character"
                        || data_type.starts_with("varchar")
                        || data_type.starts_with("char(")
                    {
                        authored_text_id_default(
                            Some(value),
                            col.default.as_deref(),
                            dialect,
                            Some(project_schema),
                        )
                    } else {
                        authored_id_default(
                            Some(value),
                            col.default.as_deref(),
                            dialect,
                            Some(project_schema),
                        )
                    };
                    col.id_default = Some(default);
                }
            }
            Op::DropColumnDefault { table, column, .. } => {
                let snap = table_mut(&mut tables, table)?;
                let col = snap
                    .columns
                    .iter_mut()
                    .find(|c| &c.name == column)
                    .ok_or_else(|| FoldError::MissingColumn {
                        table: table.clone(),
                        column: column.clone(),
                    })?;
                col.default = None;
                if col.id_default.is_some() {
                    col.id_default = Some(crate::model::snapshot::IdDefaultSnapshot::Absent);
                }
            }
            Op::AlterPrimaryKey { table, action, .. } => {
                let primary_key_name = implicit_primary_key_name(
                    table,
                    dialect,
                    &tables,
                    &partitions,
                    &views,
                    &sequences,
                );
                let snap = table_mut(&mut tables, table)?;
                apply_fold_alter_primary_key(table, snap, action, dialect, &primary_key_name)?;
            }
            Op::SynchronizeIdentity { table, column, .. } => {
                let snap = table_mut(&mut tables, table)?;
                if !snap
                    .columns
                    .iter()
                    .any(|candidate| candidate.name == *column)
                {
                    return Err(FoldError::MissingColumn {
                        table: table.clone(),
                        column: column.clone(),
                    });
                }
                // Generator state is runtime data, not structural schema state.
                // The fold validates the target and otherwise leaves the snapshot
                // byte-for-byte unchanged.
            }
            Op::AddConstraint {
                table, constraint, ..
            } => {
                // Build the dialect-canonical object(s) the SAME way the live catalog
                // reports them: FK via the shared `ir_fk_*`; PostgreSQL UNIQUE as a
                // constraint plus its implicit index; MySQL UNIQUE as the ordered unique
                // key alone (MySQL does not preserve constraint-vs-index provenance);
                // CHECK deferred. Verify the target table FIRST (fail-closed) before
                // stamping.
                let folded = add_constraint_snapshot(table, project_schema, constraint, dialect)?;
                let fk_support = match &constraint.kind {
                    IrConstraintKind::Fk { columns, .. } if columns.len() > 1 => {
                        let name = folded
                            .constraint
                            .as_ref()
                            .expect("a folded foreign key always has a constraint snapshot")
                            .name
                            .clone();
                        Some((name, columns.clone()))
                    }
                    _ => None,
                };
                let snap = table_mut(&mut tables, table)?;
                push_folded_constraint(table, snap, folded)?;
                if let Some((constraint_name, columns)) = fk_support {
                    // Stand-alone composite FK lowering emits an explicit child-side
                    // supporting index before the ALTER TABLE. Keep the folded desired
                    // snapshot in lockstep so post-apply introspection does not report
                    // that planned index as unexplained drift.
                    crate::render::declarative::ensure_fk_supporting_index(
                        table,
                        snap,
                        &constraint_name,
                        &columns,
                    )
                    .map_err(FoldError::Render)?;
                }
                snap.constraints.sort_by(|a, b| a.name.cmp(&b.name));
                snap.indexes.sort_by(|a, b| a.name.cmp(&b.name));
            }
            Op::DropConstraint { table, name, .. } => {
                let snap = table_mut(&mut tables, table)?;
                // Capture the dropped constraint's KIND before the retain so the
                // index-cascade below can discriminate. Only UNIQUE / PRIMARY KEY
                // constraints back an implicit same-named index that PG cascades on
                // drop; a FOREIGN KEY has NO backing index. PG lets a FK constraint
                // and an independent user INDEX share a name, and validate.rs does not
                // forbid the coexistence, so an unconditional
                // `retain(|i| &i.name != name)` would WRONGLY phantom-drop the user
                // index here, breaking `fold_ops == snapshot_schema(live)`.
                //
                // Re-verified on PostgreSQL 18.4: `ADD CONSTRAINT shared FOREIGN KEY
                // (...)` then `CREATE INDEX shared` leaves one row in `pg_constraint`
                // and one in `pg_class`, and `DROP CONSTRAINT shared` leaves the index
                // intact (0 constraints, 1 index). The VERSION is recorded rather than
                // the host and port the check ran against, because the version is what
                // the behaviour depends on and the port named a fixture this repository
                // no longer serves.
                let dropped_kind = snap
                    .constraints
                    .iter()
                    .find(|c| &c.name == name)
                    .map(|c| c.kind.clone());
                let before = snap.constraints.len();
                snap.constraints.retain(|c| &c.name != name);
                let removed_constraint = snap.constraints.len() != before;
                if !removed_constraint && matches!(dialect, SqlDialect::Mysql) {
                    // MySQL's catalog collapses a named table UNIQUE and its backing
                    // unique index into one key object. A catalog-seeded fold therefore
                    // has no ConstraintSnapshot to remove: DROP CONSTRAINT of that
                    // authored UNIQUE removes the same-name unique key instead.
                    let index_before = snap.indexes.len();
                    snap.indexes
                        .retain(|index| index.name != name.as_str() || !index.unique);
                    if snap.indexes.len() != index_before {
                        continue;
                    }
                }
                if !removed_constraint {
                    return Err(FoldError::MissingConstraint {
                        table: table.clone(),
                        name: name.clone(),
                    });
                }
                // Cascade the implicit index ONLY for UNIQUE / PRIMARY KEY (the kinds
                // that materialize a same-named index). For a FK this is skipped, so a
                // same-named user index survives — matching live introspection.
                if matches!(dropped_kind.as_deref(), Some("UNIQUE" | "PRIMARY KEY")) {
                    snap.indexes.retain(|i| &i.name != name);
                }
            }
            Op::ValidateConstraint { table, name, .. } => {
                // VALIDATE flips `convalidated` to true, and `pg_get_constraintdef`
                // stops rendering the ` NOT VALID` tail the moment it does. The folded
                // body has to lose the same tail or the fold would report an
                // unvalidated constraint against a catalog that has just validated it -
                // the mirror image of the drift the recorded tail exists to remove.
                //
                // BOTH lookups miss quietly, unlike `DropConstraint`, which errors with
                // `MissingConstraint`. This op does not need its target in the folded
                // set to have been a legal migration: validating a constraint an
                // EARLIER artifact added is ordinary, and `fold_ops` is routinely handed
                // one artifact's ops rather than the whole history, so a fatal lookup
                // would fail a fold on a history the server accepted. `DropConstraint`
                // is stricter because it has a delta to apply and cannot apply it;
                // this one's delta is already absent. Nothing to strip is equally the
                // case for a CHECK, whose fold never carries the tail.
                //
                // Exactly one suffix comes off. A doubled tail is malformed and stays
                // visible as drift rather than being quietly repaired here.
                if let Some(constraint) = tables
                    .get_mut(table)
                    .and_then(|snap| snap.constraints.iter_mut().find(|c| &c.name == name))
                {
                    if let Some(validated) = constraint
                        .definition
                        .strip_suffix(crate::render::declarative::NOT_VALID_DEFINITION_SUFFIX)
                    {
                        constraint.definition = validated.to_string();
                    }
                }
            }
            Op::CreateIndex {
                table,
                columns,
                name,
                unique,
                using,
                r#where,
                include,
                with,
                only,
                nulls_not_distinct,
                ..
            } => {
                let idx = create_index_snapshot(
                    table,
                    columns,
                    name.as_deref(),
                    *unique,
                    *using,
                    r#where.as_ref(),
                    include,
                    with.as_ref(),
                    *only,
                    *nulls_not_distinct,
                    dialect,
                )
                .map_err(fold_lower_error)?;
                let snap = table_mut(&mut tables, table)?;
                if snap.indexes.iter().any(|i| i.name == idx.name) {
                    return Err(FoldError::DuplicateIndex(idx.name));
                }
                snap.indexes.push(idx);
                snap.indexes.sort_by(|a, b| a.name.cmp(&b.name));
            }
            Op::DropIndex { name, table, .. } => {
                // The op carries an optional table hint. Scan the hinted table when
                // present, else every table, for the named index — fail-closed if
                // absent. The hint is advisory (a bare-name drop is rejected upstream
                // at validate; the fold accepts either since it has the whole schema).
                let removed = match table {
                    Some(t) => {
                        let snap = table_mut(&mut tables, t)?;
                        let before = snap.indexes.len();
                        snap.indexes.retain(|i| &i.name != name);
                        snap.indexes.len() != before
                    }
                    None => {
                        let mut any = false;
                        for snap in tables.values_mut() {
                            let before = snap.indexes.len();
                            snap.indexes.retain(|i| &i.name != name);
                            if snap.indexes.len() != before {
                                any = true;
                            }
                        }
                        any
                    }
                };
                if !removed {
                    return Err(FoldError::MissingIndex(name.clone()));
                }
            }
            Op::CreateView {
                name,
                schema,
                columns,
                query,
                materialized,
                replace,
                ..
            } => {
                // A view never replaces a TABLE, whatever `replace` says: the renderer
                // emits CREATE OR REPLACE VIEW, which PostgreSQL, MySQL and SQLite all
                // refuse against a table of that name. So this check stays
                // unconditional while the one below does not.
                if tables.contains_key(name) {
                    return Err(FoldError::DuplicateTable(name.clone()));
                }
                // `replace` is the authored way to change a view's body, and the fold
                // used to discard it with the rest of the struct, so re-declaring a
                // view any applied migration had created was refused before it ran.
                // The insert below overwrites, which is what replacing means.
                let replace = replace.unwrap_or(false);
                let declared_materialized = materialized.unwrap_or(false);
                match views.get(name) {
                    Some(_) if !replace => return Err(FoldError::DuplicateView(name.clone())),
                    // A replace may not turn a materialized view into a plain one or the
                    // reverse. Refusing here keeps the objection at plan time, where the
                    // accidental duplicate-name refusal used to put it: the engines all
                    // reject the statement, so letting it through only moves the failure
                    // into the middle of an apply.
                    Some(existing) if existing.materialized != declared_materialized => {
                        return Err(FoldError::ViewKindChanged {
                            name: name.clone(),
                            existing_materialized: existing.materialized,
                            declared_materialized,
                        })
                    }
                    _ => {}
                }
                views.insert(
                    name.clone(),
                    ViewSnapshot {
                        materialized: materialized.unwrap_or(false),
                        columns: columns.clone(),
                        definition: None,
                        // Keep the authored body so a later `dropView` can render the
                        // `CREATE VIEW` that undoes it. Folding is the only place the
                        // typed query and the drop meet: they are authored in
                        // different migrations, and only the accumulated history sees
                        // both.
                        authored_query: Some(query.clone()),
                        authored_schema: schema.clone(),
                        comment: None,
                    },
                );
            }
            Op::DropView { name, .. } => {
                if views.remove(name).is_none() {
                    return Err(FoldError::MissingView(name.clone()));
                }
            }
            // DML: schema no-ops (rows, not shape).
            Op::Insert { .. } | Op::Update { .. } | Op::Delete { .. } | Op::Backfill { .. } => {}
            Op::Comment { target, comment } => {
                if let CommentTarget::Table { name, .. } = target {
                    if let Some(attached) = attached_partition_tables.get_mut(name) {
                        attached.comment.clone_from(comment);
                        continue;
                    }
                    if partitions.contains_key(name) {
                        created_partition_comments.insert(name.clone(), comment.clone());
                        continue;
                    }
                }
                apply_comment(
                    &mut tables,
                    &mut views,
                    &mut named_type_snapshots,
                    &mut sequences,
                    target,
                    comment.clone(),
                )?;
            }
            // VENDOR privileged objects with modeled attributes contribute typed
            // catalog facts to the fold. The apply-time native/probed
            // IF-NOT-EXISTS decision remains presence-based; this snapshot compare
            // is the safety net that catches a same-name object with divergent
            // attributes after a skip.
            Op::CreateSchema {
                name,
                authorization,
                ..
            } => {
                schemas.insert(
                    name.clone(),
                    SchemaObjectSnapshot {
                        owner: authorization.clone(),
                    },
                );
            }
            Op::DropSchema { name, .. } => {
                schemas.remove(name);
            }
            Op::CreateExtension { name, schema, .. } => {
                extensions.insert(
                    name.clone(),
                    ExtensionSnapshot {
                        schema: schema.clone(),
                    },
                );
            }
            Op::DropExtension { name, .. } => {
                extensions.remove(name);
            }
            Op::CreateRole {
                name,
                login,
                bypass_rls,
                create_role,
                create_db,
                superuser,
                in_role,
                ..
            } => {
                let mut member_of = in_role.clone().unwrap_or_default();
                member_of.sort();
                member_of.dedup();
                roles.insert(
                    name.clone(),
                    RoleSnapshot {
                        login: login.unwrap_or(false),
                        superuser: superuser.unwrap_or(false),
                        create_db: create_db.unwrap_or(false),
                        create_role: create_role.unwrap_or(false),
                        bypass_rls: bypass_rls.unwrap_or(false),
                        member_of,
                        ..RoleSnapshot::default()
                    },
                );
            }
            Op::DropRole { name, .. } => {
                roles.remove(name);
            }
            Op::CreateFunction {
                name,
                schema,
                args,
                returns,
                language,
                replace,
                volatility,
                body,
            } => {
                let key = FunctionKey::from_create(
                    name,
                    schema.as_deref(),
                    args.as_deref(),
                    project_schema,
                );
                if replace.unwrap_or(false) && !functions.contains_key(&key) {
                    // PostgreSQL resolves aliases and discards type modifiers when
                    // identifying a function, while the offline IR has only the
                    // authored spellings. An alias-spelled OR REPLACE may therefore
                    // target an existing same-arity key that is not textually equal.
                    // Invalidate those candidates before recording the new body so
                    // no later drop can recover the stale pre-replace definition.
                    functions.retain(|existing, _| {
                        existing.schema != key.schema
                            || existing.name != key.name
                            || existing.arg_types.len() != key.arg_types.len()
                    });
                }
                functions.insert(
                    key,
                    FunctionSnapshot {
                        schema: schema.clone(),
                        args: args.clone(),
                        returns: returns.clone(),
                        language: *language,
                        volatility: *volatility,
                        body: body.clone(),
                    },
                );
            }
            Op::DropFunction {
                name,
                schema,
                arg_types,
                ..
            } => {
                let key = FunctionKey::from_drop(
                    name,
                    schema.as_deref(),
                    arg_types.as_deref(),
                    project_schema,
                );
                if functions.remove(&key).is_none() {
                    // A non-textual type alias or an all-arguments DROP signature
                    // can still identify a recorded function. Without catalog type
                    // resolution the exact overload is unknowable, so discard every
                    // same-name candidate rather than leave a definition that may
                    // already have been dropped.
                    functions.retain(|existing, _| {
                        existing.schema != key.schema || existing.name != key.name
                    });
                }
            }
            Op::CreatePolicy {
                name,
                table,
                schema,
                for_cmd,
                to,
                using,
                with_check,
            } => {
                let key = PolicyKey::new(name, table, schema.as_deref(), project_schema);
                policies.insert(
                    key,
                    PolicySnapshot {
                        for_cmd: *for_cmd,
                        to: to.clone(),
                        using: using.clone(),
                        with_check: with_check.clone(),
                    },
                );
            }
            Op::DropPolicy {
                name,
                table,
                schema,
                ..
            } => {
                let key = PolicyKey::new(name, table, schema.as_deref(), project_schema);
                policies.remove(&key);
            }
            Op::CreateTrigger {
                name,
                table,
                schema,
                timing,
                events,
                for_each,
                action,
                when,
            } => {
                let key = TriggerKey::new(name, table, schema.as_deref(), project_schema);
                triggers.insert(
                    key,
                    TriggerSnapshot {
                        timing: *timing,
                        events: events.clone(),
                        for_each: *for_each,
                        action: action.clone(),
                        when: when.clone(),
                    },
                );
            }
            Op::DropTrigger {
                name,
                table,
                schema,
                ..
            } => {
                let key = TriggerKey::new(name, table, schema.as_deref(), project_schema);
                triggers.remove(&key);
            }
            // Remaining vendor ops either change unmodeled facets (role settings,
            // grants/RLS) or are raw statements, so they do not
            // contribute to this structural snapshot.
            Op::AlterRole { .. }
            | Op::DropOwnedBy { .. }
            | Op::Grant { .. }
            | Op::Revoke { .. }
            | Op::PgRaw { .. } => {}
            Op::Dialectal { .. } => {}
        }
    }

    if dialect == SqlDialect::Sqlite {
        for snap in tables.values_mut() {
            apply_fold_sqlite_rowid_metadata(snap)?;
        }
    }

    // The expected side of the vendor-object comparison: what the authored history
    // says exists, reduced to the identity a catalog read can also produce. The
    // three maps this derives FROM keep their authored definitions for every
    // dialect - they are rollback history, and a SQLite trigger is still restorable.
    //
    // POSTGRESQL ONLY, for the reason the row-level-security seeding above is:
    // SQLite and MySQL introspection reads none of these catalogs and leaves the
    // field `None`, so filling it for them would make every folded snapshot differ
    // from the live one it is supposed to equal. Equality compares the field plainly
    // and cannot skip an absent side the way the diff does.
    //
    // OR THE BASE ALREADY SPOKE, which is the carry-through every other map here
    // gets. Folding onto a base is a continuation, so a base that asserted "this
    // schema holds no policies" must not have that assertion ERASED by a fold under
    // another dialect - a no-op op would then change the snapshot. Measured:
    // `synchronize_identity_fold_validates_target_without_changing_schema` folds a
    // PostgreSQL base under all three dialects and asserts the result is unchanged,
    // and the dialect test alone failed it on `None` against `Some({})`.
    let speaks = dialect == SqlDialect::Postgres || base.vendor_objects.is_some();
    let vendor_objects = speaks.then(|| VendorObjectIdentities {
        // The body comes from the SAME `functions` map the rollback history uses, so
        // a `CREATE OR REPLACE` that overwrote an entry above contributes the LAST
        // body rather than the first - which is what PostgreSQL holds too.
        functions: functions
            .iter()
            .map(|(key, snapshot)| (key.canonicalized(), FunctionIdentity::of(snapshot)))
            .collect(),
        policies: policies
            .iter()
            .map(|(key, snapshot)| (key.clone(), PolicyIdentity::of(snapshot)))
            .collect(),
        triggers: triggers
            .iter()
            .map(|(key, snapshot)| (key.clone(), TriggerIdentity::of(snapshot)))
            .collect(),
    });

    Ok(SchemaSnapshot {
        tables,
        table_rls,
        partitions,
        views,
        named_types: named_type_snapshots,
        sequences,
        roles,
        schemas,
        extensions,
        functions,
        policies,
        triggers,
        vendor_objects,
    })
}

fn apply_fold_sqlite_rowid_metadata(snap: &mut TableSnapshot) -> Result<(), FoldError> {
    for column in &mut snap.columns {
        column.sqlite_rowid = false;
    }
    let Some((_, columns)) = folded_primary_key(snap)? else {
        return Ok(());
    };
    let [column_name] = columns.as_slice() else {
        return Ok(());
    };
    let Some(column_index) = snap
        .columns
        .iter()
        .position(|candidate| candidate.name == *column_name)
    else {
        return Err(FoldError::MissingColumn {
            table: "<snapshot>".to_string(),
            column: column_name.clone(),
        });
    };
    let storage_generates =
        sqlite_integer_storage_for_rowid(snap, &snap.columns[column_index].data_type)
            || matches!(snap.columns[column_index].identity, Some(identity) if !identity.always);
    let stored_shape_allows_rowid = snap.stored_create_sql.as_deref().is_none_or(|stored| {
        !crate::render::declarative::sqlite_create_is_without_rowid(stored)
            && !crate::render::declarative::sqlite_inline_primary_key_is_desc(stored, column_name)
    });
    let sqlite_rowid = storage_generates && stored_shape_allows_rowid;
    let column = &mut snap.columns[column_index];
    column.sqlite_rowid = sqlite_rowid;
    if column.sqlite_rowid {
        column.id_default = Some(catalog_id_default(
            column.default.as_deref(),
            SqlDialect::Sqlite,
            None,
        ));
    }
    Ok(())
}

/// `&mut TableSnapshot` for `table`, or [`FoldError::MissingTable`] (fail-closed).
fn table_mut<'a>(
    tables: &'a mut BTreeMap<String, TableSnapshot>,
    table: &str,
) -> Result<&'a mut TableSnapshot, FoldError> {
    tables
        .get_mut(table)
        .ok_or_else(|| FoldError::MissingTable(table.to_string()))
}

fn apply_comment(
    tables: &mut BTreeMap<String, TableSnapshot>,
    views: &mut BTreeMap<String, ViewSnapshot>,
    named_types: &mut BTreeMap<String, NamedTypeSnapshot>,
    sequences: &mut BTreeMap<String, SequenceSnapshot>,
    target: &CommentTarget,
    comment: Option<String>,
) -> Result<(), FoldError> {
    match target {
        CommentTarget::Table { name, .. } => {
            table_mut(tables, name)?.comment = comment;
        }
        CommentTarget::Column { table, name, .. } => {
            let snap = table_mut(tables, table)?;
            let col = snap
                .columns
                .iter_mut()
                .find(|c| c.name == *name)
                .ok_or_else(|| FoldError::MissingColumn {
                    table: table.clone(),
                    column: name.clone(),
                })?;
            col.comment = comment;
        }
        CommentTarget::Index { name, .. } => {
            let mut found = false;
            for table in tables.values_mut() {
                if let Some(idx) = table.indexes.iter_mut().find(|i| i.name == *name) {
                    idx.comment = comment.clone();
                    found = true;
                }
            }
            if !found {
                return Err(FoldError::MissingIndex(name.clone()));
            }
        }
        CommentTarget::Constraint { table, name, .. } => {
            let snap = table_mut(tables, table)?;
            let constraint = snap
                .constraints
                .iter_mut()
                .find(|c| c.name == *name)
                .ok_or_else(|| FoldError::MissingConstraint {
                    table: table.clone(),
                    name: name.clone(),
                })?;
            constraint.comment = comment;
        }
        CommentTarget::View { name, .. } => {
            let view = views
                .get_mut(name)
                .ok_or_else(|| FoldError::MissingView(name.clone()))?;
            view.comment = comment;
        }
        CommentTarget::Type { name, .. } => {
            let ty = named_types
                .get_mut(name)
                .ok_or_else(|| FoldError::NamedTypeMissing {
                    kind: "type",
                    name: name.clone(),
                })?;
            ty.comment = comment;
        }
        CommentTarget::Sequence { name, .. } => {
            let seq = sequences
                .get_mut(name)
                .ok_or_else(|| FoldError::MissingSequence(name.clone()))?;
            seq.comment = comment;
        }
        // PostgreSQL function comments require a signature for overloaded
        // functions. The IR intentionally carries only a function name, so
        // function comments render but are not folded into SchemaSnapshot.
        CommentTarget::Function { .. } => {}
    }
    Ok(())
}

/// The `CollectionDescriptor` for a `createTable` op — the SAME bridge
/// `IrAuthor::create_table_descriptor` builds (columns only; the table-level
/// constraints/indexes are folded on separately by [`fold_create_table_specs`],
/// exactly as the lower does). `owner_app` is the fold-internal constant (drift-
/// irrelevant).
fn create_table_descriptor(
    name: &str,
    columns: &[IrColumn],
    runtime_options: Option<&TableRuntimeOptions>,
) -> CollectionDescriptor {
    CollectionDescriptor {
        name: name.to_string(),
        owner_app: FOLD_OWNER_APP.to_string(),
        fields: columns
            .iter()
            .map(ir_column_to_field_resolved_create)
            .collect(),
        indexes: Vec::new(),
        runtime_options: runtime_options.cloned().unwrap_or_default(),
    }
}

/// The `ColumnSnapshot`(s) for a single added field — routes ONE field through the
/// shared resolved snapshot builder (a one-field descriptor) and pulls the matching
/// column out, so the default / encryption / comment sentinel is built by the shared
/// kernel, never re-spelled. The table's active policy injection is explicit, but
/// the one-field descriptor never matches its complete resolved prefix, so no
/// column is injected or reshaped. Mirrors `IrAuthor::add_column_snapshot_with_sibling`.
///
/// Returns the MAIN column plus the hidden `<col>_masked TEXT` sibling the
/// shared builder injects for a masked column, so the OFFLINE fold snapshot grows the
/// SAME sibling the live apply path does (otherwise `fold_ops` would phantom-drift
/// against the introspected live table for a masked added column). A non-masked column
/// returns `(main, None)`.
#[allow(clippy::too_many_arguments)]
fn add_column_snapshot(
    table: &str,
    column: &str,
    ty: &ColType,
    nullable: Option<bool>,
    default: Option<&IrDefault>,
    vector_metric: Option<crate::model::ir::VectorMetric>,
    case_sensitive: Option<bool>,
    mask: Option<crate::model::ir::IrMask>,
    generated: Option<&crate::model::ir::GeneratedCol>,
    identity: Option<crate::model::ir::IdentityCol>,
    project_schema: &str,
    dialect: SqlDialect,
    effective: &EffectivePolicy,
) -> Result<(ColumnSnapshot, Option<ColumnSnapshot>), FoldError> {
    if !dialect.supports(Capability::NonPkIdentity) && identity.is_some() {
        return Err(FoldError::Unsupported(
            "addColumn identity on SQLite (non-PK identity has no sound SQLite emulation)",
        ));
    }
    let field = ir_column_to_field(&IrColumn {
        name: column.to_string(),
        ty: ty.clone(),
        nullable,
        default: default.cloned(),
        // `id_prefix` stays `None` (an added column is never the system PK);
        // the vector metric + standalone mask ARE threaded so the snapshot renders them.
        unique: None,
        value_format: None,
        references: None,
        id_prefix: None,
        collation: None,
        vector_metric,
        case_sensitive,
        mask,
        generated: generated.cloned(),
        identity,
    });
    let desc = CollectionDescriptor {
        name: table.to_string(),
        owner_app: FOLD_OWNER_APP.to_string(),
        fields: vec![field],
        indexes: Vec::new(),
        runtime_options: Default::default(),
    };
    let resolved_inject = ResolvedInject::for_table(effective, project_schema, table)
        .map_err(|error| FoldError::Render(error.to_string()))?;
    let snap = build_resolved_table_snapshot(project_schema, &desc, dialect, &resolved_inject)?;
    let sibling_name = format!("{column}_masked");
    let mut main = snap
        .columns
        .iter()
        .find(|c| c.name == column)
        .cloned()
        .ok_or(FoldError::Unsupported("addColumn (column folded away)"))?;
    apply_fold_author_type_override_to_column(table, column, ty, &mut main, dialect)?;
    apply_fold_structured_default_to_column(table, column, ty, default, &mut main, dialect)?;
    let sibling = snap.columns.into_iter().find(|c| c.name == sibling_name);
    Ok((main, sibling))
}

fn apply_fold_author_type_overrides_to_snapshot(
    table: &str,
    columns: &[IrColumn],
    snap: &mut TableSnapshot,
    dialect: SqlDialect,
) -> Result<(), FoldError> {
    for source in columns {
        if author_type_override(&source.ty, dialect).is_none() {
            continue;
        }
        let col = snap
            .columns
            .iter_mut()
            .find(|c| c.name == source.name)
            .ok_or_else(|| FoldError::MissingColumn {
                table: table.to_string(),
                column: source.name.clone(),
            })?;
        apply_fold_author_type_override_to_column(table, &source.name, &source.ty, col, dialect)?;
    }
    Ok(())
}

fn apply_fold_author_type_override_to_column(
    table: &str,
    column: &str,
    ty: &ColType,
    col: &mut ColumnSnapshot,
    dialect: SqlDialect,
) -> Result<(), FoldError> {
    let Some(type_override) = author_type_override(ty, dialect) else {
        return Ok(());
    };
    if col.name != column {
        return Err(FoldError::MissingColumn {
            table: table.to_string(),
            column: column.to_string(),
        });
    }
    col.data_type = type_override.data_type;
    col.ddl_type_override = type_override.ddl_type;
    if type_override.quote_literal_default_as_text {
        col.default = col
            .default
            .take()
            .map(|default| crate::render::dml::sql_string_literal(&default));
    }
    Ok(())
}

fn apply_fold_structured_defaults_to_snapshot(
    table: &str,
    columns: &[IrColumn],
    snap: &mut TableSnapshot,
    dialect: SqlDialect,
) -> Result<(), FoldError> {
    for source in columns {
        let Some(
            IrDefault::Expr { .. }
            | IrDefault::Container { .. }
            | IrDefault::Json { .. }
            | IrDefault::Nextval { .. }
            | IrDefault::Literal {
                value: crate::model::ir::IrScalar::Int64(_),
            },
        ) = source.default.as_ref()
        else {
            continue;
        };
        let col = snap
            .columns
            .iter_mut()
            .find(|c| c.name == source.name)
            .ok_or_else(|| FoldError::MissingColumn {
                table: table.to_string(),
                column: source.name.clone(),
            })?;
        apply_fold_structured_default_to_column(
            table,
            &source.name,
            &source.ty,
            source.default.as_ref(),
            col,
            dialect,
        )?;
    }
    Ok(())
}

fn apply_fold_structured_default_to_column(
    table: &str,
    column: &str,
    ty: &ColType,
    default: Option<&IrDefault>,
    col: &mut ColumnSnapshot,
    dialect: SqlDialect,
) -> Result<(), FoldError> {
    let Some(
        default @ (IrDefault::Expr { .. }
        | IrDefault::Container { .. }
        | IrDefault::Json { .. }
        | IrDefault::Nextval { .. }
        | IrDefault::Literal {
            value: crate::model::ir::IrScalar::Int64(_),
        }),
    ) = default
    else {
        return Ok(());
    };
    if col.name != column {
        return Err(FoldError::MissingColumn {
            table: table.to_string(),
            column: column.to_string(),
        });
    }
    col.default =
        Some(render_ir_default_for_type(default, ty, dialect).map_err(fold_named_type_error)?);
    Ok(())
}

fn apply_fold_named_type_metadata(
    table: &str,
    columns: &[IrColumn],
    snap: &mut TableSnapshot,
    named_types: &NamedTypeRegistry,
    dialect: SqlDialect,
    project_schema: &str,
    effective: &EffectivePolicy,
) -> Result<(), FoldError> {
    for source in columns {
        if !matches!(source.ty, ColType::Enum { .. } | ColType::Domain { .. }) {
            continue;
        }
        let col = snap
            .columns
            .iter_mut()
            .find(|c| c.name == source.name)
            .ok_or(FoldError::Unsupported("named type column folded away"))?;
        apply_fold_named_type_column_metadata(
            table,
            source,
            col,
            named_types,
            dialect,
            project_schema,
            effective,
        )?;
    }
    Ok(())
}

/// Stamp the per-column collation facet onto the folded snapshot.
///
/// Runs BEFORE the value-format pass on purpose. A column carrying both is refused
/// by the validator, so the two never contend in a valid migration; ordering them
/// this way means that if one ever slips through, the format's own bytewise
/// collation wins rather than a bare facet overwriting a storage contract.
///
/// This is the second fold-side writer of `ColumnSnapshot::collation`, and the
/// invariant `SetColumnType` relies on still holds: both writers write the SAME
/// bytewise identity, so a retype that re-derives the field from the target type
/// still clears whichever of them put it there.
fn apply_fold_collation_metadata(
    columns: &[IrColumn],
    snap: &mut TableSnapshot,
    dialect: SqlDialect,
) -> Result<(), FoldError> {
    for source in columns {
        let Some(ColumnCollation::Bytewise) = source.collation else {
            continue;
        };
        let col = snap
            .columns
            .iter_mut()
            .find(|col| col.name == source.name)
            .ok_or(FoldError::Unsupported("collated column folded away"))?;
        // The type spelling the renderer would have used, so the override REPLACES
        // that decision rather than guessing beside it.
        let rendered = crate::render::declarative::column_type_for_render(col, dialect, false);
        let (ddl_type, collation) =
            crate::render::value_format::bytewise_column_metadata(&rendered, dialect);
        col.ddl_type_override = Some(ddl_type);
        col.collation = collation;
    }
    Ok(())
}

fn apply_fold_value_format_metadata(
    columns: &[IrColumn],
    snap: &mut TableSnapshot,
    dialect: SqlDialect,
    project_schema: &str,
) -> Result<(), FoldError> {
    for source in columns {
        if source.value_format.is_none() {
            continue;
        }
        let col = snap
            .columns
            .iter_mut()
            .find(|col| col.name == source.name)
            .ok_or(FoldError::Unsupported("value-format column folded away"))?;
        apply_fold_value_format_column_metadata(source, col, dialect, project_schema)?;
    }
    Ok(())
}

fn apply_fold_id_default_metadata(
    columns: &[IrColumn],
    snap: &mut TableSnapshot,
    dialect: SqlDialect,
    project_schema: &str,
) -> Result<(), FoldError> {
    for source in columns {
        let Some(col) = snap.columns.iter_mut().find(|col| col.name == source.name) else {
            return Err(FoldError::Unsupported("ID-default column folded away"));
        };
        apply_fold_id_default_column_metadata(source, col, dialect, project_schema);
    }
    Ok(())
}

fn apply_fold_id_default_column_metadata(
    source: &IrColumn,
    col: &mut ColumnSnapshot,
    dialect: SqlDialect,
    project_schema: &str,
) {
    if source.identity.is_some() || matches!(source.default, Some(IrDefault::Nextval { .. })) {
        col.id_default = Some(authored_id_default(
            source.default.as_ref(),
            col.default.as_deref(),
            dialect,
            Some(project_schema),
        ));
    }
}

fn apply_fold_uuid_metadata(
    columns: &[IrColumn],
    snap: &mut TableSnapshot,
    dialect: SqlDialect,
    project_schema: &str,
) -> Result<(), FoldError> {
    for source in columns {
        if !matches!(source.ty, ColType::Uuid) {
            continue;
        }
        let col = snap
            .columns
            .iter_mut()
            .find(|col| col.name == source.name)
            .ok_or(FoldError::Unsupported("UUID column folded away"))?;
        apply_fold_uuid_column_metadata(source, col, dialect, project_schema)?;
    }
    Ok(())
}

fn apply_fold_uuid_column_metadata(
    source: &IrColumn,
    col: &mut ColumnSnapshot,
    dialect: SqlDialect,
    project_schema: &str,
) -> Result<(), FoldError> {
    if !matches!(source.ty, ColType::Uuid) {
        return Ok(());
    }
    col.id_default = Some(authored_uuid_id_default(
        source.default.as_ref(),
        col.default.as_deref(),
        dialect,
        Some(project_schema),
    ));
    let Some(metadata) = uuid_column_metadata(&source.name, dialect)
        .map_err(|error| FoldError::Shape(DeclarativeError::Invalid(error)))?
    else {
        return Ok(());
    };
    col.collation = metadata.collation;
    col.ddl_type_override = Some(metadata.ddl_type);
    if source.references.is_none() {
        col.inline_checks.push(metadata.inline_check);
    }
    Ok(())
}

fn apply_fold_value_format_column_metadata(
    source: &IrColumn,
    col: &mut ColumnSnapshot,
    dialect: SqlDialect,
    project_schema: &str,
) -> Result<(), FoldError> {
    let Some(value_format) = &source.value_format else {
        return Ok(());
    };
    let metadata = value_format_column_metadata(&source.name, value_format, dialect)
        .map_err(|error| FoldError::Shape(DeclarativeError::Invalid(error)))?;
    col.collation = metadata.collation;
    col.ddl_type_override = Some(metadata.ddl_type);
    col.id_default = Some(authored_text_id_default(
        source.default.as_ref(),
        col.default.as_deref(),
        dialect,
        Some(project_schema),
    ));
    if source.references.is_none() {
        col.value_format = Some(value_format.clone());
        col.inline_checks.push(metadata.inline_check);
    }
    Ok(())
}

fn pg_type_data_type(schema: &str, name: &str) -> String {
    format!("{schema}.{name}")
}

fn apply_fold_named_type_column_metadata(
    table: &str,
    source: &IrColumn,
    col: &mut ColumnSnapshot,
    named_types: &NamedTypeRegistry,
    dialect: SqlDialect,
    project_schema: &str,
    effective: &EffectivePolicy,
) -> Result<(), FoldError> {
    match &source.ty {
        ColType::Enum { name, .. } => match dialect {
            SqlDialect::Postgres => {
                let registry_schema = named_types.enum_schema_or(name, project_schema);
                let (data_type, ddl_type) =
                    postgres_named_type_metadata(&source.ty, registry_schema)
                        .map_err(fold_named_type_error)?
                        .ok_or(FoldError::Unsupported(
                            "named enum metadata was not resolved",
                        ))?;
                col.data_type = data_type;
                col.ddl_type_override = Some(ddl_type);
            }
            SqlDialect::Sqlite => {
                let def = named_types.enum_def(name).map_err(fold_named_type_error)?;
                col.data_type = "text".to_string();
                col.inline_checks.push(
                    enum_inline_check(&source.name, &def.values, dialect)
                        .map_err(fold_named_type_error)?,
                );
            }
            SqlDialect::Mysql => {
                let def = named_types.enum_def(name).map_err(fold_named_type_error)?;
                let ty = mysql_enum_type(&def.values);
                col.data_type = ty.clone();
                col.ddl_type_override = Some(ty);
            }
        },
        ColType::Domain { name, .. } => {
            if matches!(dialect, SqlDialect::Postgres) {
                let registry_schema = named_types.domain_schema_or(name, project_schema);
                let (data_type, ddl_type) =
                    postgres_named_type_metadata(&source.ty, registry_schema)
                        .map_err(fold_named_type_error)?
                        .ok_or(FoldError::Unsupported(
                            "named domain metadata was not resolved",
                        ))?;
                col.data_type = data_type;
                col.ddl_type_override = Some(ddl_type);
                return Ok(());
            }
            let def = named_types
                .domain_def(name)
                .map_err(fold_named_type_error)?;
            if matches!(def.as_type, ColType::Enum { .. } | ColType::Domain { .. }) {
                return Err(FoldError::NamedTypeUnsupported {
                    kind: "domain",
                    name: name.clone(),
                    reason: "nested named base type",
                });
            }
            let (base, _sibling) = add_column_snapshot(
                table,
                &source.name,
                &def.as_type,
                source.nullable,
                source.default.as_ref(),
                source.vector_metric,
                source.case_sensitive,
                source.mask,
                source.generated.as_ref(),
                source.identity,
                project_schema,
                dialect,
                effective,
            )?;
            col.data_type = base.data_type;
            col.ddl_type_override = base.ddl_type_override;
            if matches!(dialect, SqlDialect::Postgres) {
                col.data_type = pg_type_data_type(&def.schema, name);
            } else {
                if def.not_null {
                    col.nullable = false;
                }
                if col.default.is_none() {
                    if let Some(default) = &def.default {
                        col.default = Some(
                            render_ir_default_for_type(default, &def.as_type, dialect)
                                .map_err(fold_named_type_error)?,
                        );
                    }
                }
                if let Some(check) = &def.check {
                    let value_sql = crate::render::dml::quote_ident_for_dialect(
                        "column",
                        &source.name,
                        dialect,
                    )
                    .map_err(|e| FoldError::NamedTypeRender(e.to_string()))?;
                    let expr = render_domain_check(check, dialect, &value_sql)
                        .map_err(fold_named_type_error)?;
                    col.inline_checks.push(format!("CHECK ({expr})"));
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Fold a `createTable`'s TABLE-LEVEL constraints + indexes onto the
/// `build_table_snapshot`-built [`TableSnapshot`].
///
/// This fold and the lower
/// ([`crate::render::lower::IrAuthor::fold_create_table_specs`]) agree on every
/// constraint/index NAME and on the UNIQUE definition body (both route through the
/// shared `constraintdef_cols` speller), so an op-authored table re-diffs clean
/// against the apply path. They DELIBERATELY differ on one point — NOT "byte
/// identical": for a table-level UNIQUE the fold materializes its catalog unique
/// key, whereas the lower pushes only the `ConstraintSnapshot` used to emit the
/// inline clause. The reason is that the two snapshots model different things:
/// - the lower's is an EMISSION PLAN — `snap.indexes` drives `CREATE INDEX`, and PG
///   auto-creates the constraint's implicit index, so emitting it would duplicate;
/// - the fold's is a LOGICAL-STATE model — it must match what `snapshot_schema`
///   reports. PostgreSQL returns both a constraint and its backing index; MySQL
///   collapses constraint and explicit-index syntax to one ordered unique key and
///   cannot recover which spelling authored it.
///
/// The dialect-canonical unique-key materialization is therefore REQUIRED for
/// `fold == introspect` to hold; aligning it with the lower's emission-only shape
/// would break the round-trip oracle.
///
/// Fail-closed parity with the lower:
/// - a create-table PRIMARY KEY is carried by the op's top-level `primary_key`;
///   stale constraint-form PKs are ignored here after validation;
/// - table-level CHECK folds only on PostgreSQL until the non-PG renderers land;
/// - table-level single- and multi-column FKs fold on all three targets;
/// - target-specific partial/non-btree index features remain capability-gated;
/// - on **SQLite**, a table-level UNIQUE and a non-btree index `using` are
///   refused — BYTE-FOR-BYTE parity with the lower
///   ([`crate::render::lower::IrAuthor::fold_create_table_specs`]).
///
/// The SQLite refusals are NOT cosmetic: in the migration-first model the fold is
/// the SOLE source of truth for gen-types. The lower (= the apply path) REFUSES
/// these shapes on SQLite (a table-level UNIQUE / non-btree `using` is not
/// threaded into the emitter), so
/// such a `createTable` can NEVER be deployed on SQLite. A fold that ACCEPTED it
/// would emit types for a schema that never applies — fail-OPEN relative to apply,
/// breaking the fold's own contract ("a set the engine already applied is
/// internally consistent, so the fold agrees with apply"). So the fold mirrors the
/// lower's refusals exactly.
fn fold_create_table_specs(
    table: &str,
    project_schema: &str,
    snap: &mut TableSnapshot,
    constraints: &[IrConstraint],
    indexes: &[IrIndex],
    dialect: SqlDialect,
) -> Result<(), FoldError> {
    let mut table_foreign_keys: Vec<(String, Vec<String>)> = Vec::new();
    for c in constraints {
        match &c.kind {
            IrConstraintKind::Check { expr, .. } => {
                if !matches!(dialect, SqlDialect::Postgres) {
                    return Err(FoldError::Unsupported(
                        "createTable table-level CHECK is PostgreSQL-only",
                    ));
                }
                let name = c.name.as_deref().map_or_else(
                    || derived_check_constraint_name(table, expr),
                    str::to_string,
                );
                let rendered = crate::render::dml::render_expr_inline(expr, dialect)
                    .map_err(|e| FoldError::Render(e.to_string()))?;
                // Record the columns the CHECK reads structurally, from the same
                // AST the render above walked, so the `DropColumn` cascade never
                // has to guess them back out of the rendered text.
                let cascade_columns = crate::render::dml::expr_column_refs(expr, dialect)
                    .map_err(|e| FoldError::Render(e.to_string()))?;
                push_folded_constraint(
                    table,
                    snap,
                    FoldedConstraint {
                        constraint: Some(ConstraintSnapshot {
                            name,
                            kind: "CHECK".to_string(),
                            definition: format!("CHECK ({rendered})"),
                            comment: None,
                            cascade_columns: Some(cascade_columns),
                        }),
                        index: None,
                    },
                )?;
            }
            IrConstraintKind::Fk {
                columns,
                references_table,
                references_columns,
                on_delete,
                on_update,
                deferrable,
                initially_deferred,
                not_valid: _,
            } => {
                if !dialect.supports(Capability::TableLevelForeignKey) {
                    return Err(FoldError::Unsupported(
                        "createTable table-level FOREIGN KEY is unsupported by this dialect",
                    ));
                }
                if columns.is_empty() {
                    return Err(FoldError::Unsupported(
                        "createTable FOREIGN KEY with no local column",
                    ));
                }
                let fk = ir_fk_constraint_snapshot_for_columns(
                    project_schema,
                    table,
                    c.name.as_deref(),
                    columns,
                    references_table,
                    references_columns,
                    on_delete.map(RefAction::as_token),
                    on_update.map(RefAction::as_token),
                    deferrable.unwrap_or(false),
                    initially_deferred.unwrap_or(false),
                    // Measured against PostgreSQL 18: a CREATE TABLE foreign key
                    // spelled ` NOT VALID` is accepted and stored `convalidated =
                    // true`, and the catalog reports a plain body. Recording the tail
                    // here would phantom-diff a constraint the server considers valid.
                    false,
                    dialect,
                );
                table_foreign_keys.push((fk.name.clone(), columns.clone()));
                // A FOREIGN KEY materializes no index.
                push_folded_constraint(
                    table,
                    snap,
                    FoldedConstraint {
                        constraint: Some(fk),
                        index: None,
                    },
                )?;
            }
            IrConstraintKind::Unique { columns } => {
                if !dialect.supports(Capability::TableLevelUnique) {
                    return Err(FoldError::Unsupported(
                        "createTable table-level UNIQUE on SQLite (the SQLite CREATE \
                         renders from the descriptor; a table-level UNIQUE is not \
                         threaded into the emitter)",
                    ));
                }
                let name = c.name.as_deref().map_or_else(
                    || derived_constraint_name(table, columns, "key"),
                    str::to_string,
                );
                push_folded_constraint(table, snap, unique_constraint(&name, columns, dialect))?;
            }
            IrConstraintKind::Exclusion { elements, .. } => {
                if !dialect.supports(Capability::ExclusionConstraint) {
                    return Err(FoldError::Unsupported(
                        "createTable exclusion constraint is PostgreSQL-only",
                    ));
                }
                let name = c.name.as_deref().map_or_else(
                    || derived_exclusion_constraint_name(table, elements),
                    str::to_string,
                );
                render_exclusion_constraint_body(&c.kind, dialect).map_err(fold_lower_error)?;
                push_folded_constraint(
                    table,
                    snap,
                    FoldedConstraint {
                        constraint: Some(ConstraintSnapshot {
                            name,
                            kind: "EXCLUDE".to_string(),
                            // PG canonicalizes exclusion bodies differently from the
                            // authored render. Drift tracks EXCLUDE by presence/name +
                            // kind only, matching `snapshot_schema`.
                            definition: String::new(),
                            comment: None,
                            // The empty `definition` above leaves the DropColumn
                            // cascade's parsing fallback nothing to match, so the
                            // cascade set is recorded structurally - PG's own `conkey`
                            // predicate, plain column elements only.
                            cascade_columns: Some(exclusion_cascade_columns(elements)),
                        }),
                        index: None,
                    },
                )?;
            }
        }
    }
    for ix in indexes {
        let access = ix.using.map_or("btree", index_method_access);
        if !dialect.supports(Capability::NonBtreeIndexMethod) && access != "btree" {
            return Err(FoldError::Unsupported(
                "createTable non-btree index `using` on SQLite (not yet supported)",
            ));
        }
        let mut snap_idx = create_index_snapshot(
            table,
            &ix.columns,
            ix.name.as_deref(),
            ix.unique,
            ix.using,
            ix.r#where.as_ref(),
            &ix.include,
            ix.with.as_ref(),
            ix.only,
            ix.nulls_not_distinct,
            dialect,
        )
        .map_err(fold_lower_error)?;
        snap_idx.access_method = access.to_string();
        if snap.indexes.iter().any(|i| i.name == snap_idx.name) {
            return Err(FoldError::DuplicateIndex(snap_idx.name));
        }
        snap.indexes.push(snap_idx);
    }
    for (constraint_name, columns) in table_foreign_keys {
        crate::render::declarative::ensure_fk_supporting_index(
            table,
            snap,
            &constraint_name,
            &columns,
        )
        .map_err(FoldError::Render)?;
    }
    // Deterministic name ordering (build_table_snapshot sorts; live is name-sorted).
    snap.constraints.sort_by(|a, b| a.name.cmp(&b.name));
    snap.indexes.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(())
}

/// A folded catalog constraint (if recoverable) plus its implicit unique index.
///
/// A UNIQUE / PRIMARY KEY constraint creates a `pg_constraint` row AND an implicit
/// unique index of the SAME name (`pg_index` reports it), so live introspection
/// returns BOTH. The fold must mirror both for `fold == introspect` to hold (the
/// shared `build_table_snapshot` already does this for the system `<table>_pkey`).
/// MySQL cannot recover whether a named unique key was authored as a table
/// constraint or an explicit unique index, so its UNIQUE shape has no synthetic
/// constraint. A FOREIGN KEY creates no index, so `index` is `None`.
struct FoldedConstraint {
    constraint: Option<ConstraintSnapshot>,
    index: Option<IndexSnapshot>,
}

/// Push a folded constraint (+ its implicit index) onto the table, fail-closed on a
/// name collision against an existing constraint OR index of the same name.
fn push_folded_constraint(
    table: &str,
    snap: &mut TableSnapshot,
    folded: FoldedConstraint,
) -> Result<(), FoldError> {
    if let Some(constraint) = &folded.constraint {
        if snap
            .constraints
            .iter()
            .any(|candidate| candidate.name == constraint.name)
        {
            return Err(FoldError::DuplicateConstraint {
                table: table.to_string(),
                name: constraint.name.clone(),
            });
        }
    }
    if let Some(idx) = &folded.index {
        if snap.indexes.iter().any(|i| i.name == idx.name) {
            return Err(FoldError::DuplicateIndex(idx.name.clone()));
        }
    }
    if let Some(constraint) = folded.constraint {
        snap.constraints.push(constraint);
    }
    if let Some(idx) = folded.index {
        snap.indexes.push(idx);
    }
    Ok(())
}

/// The columns whose drop cascades an EXCLUDE away, collected structurally from the
/// constraint's elements.
///
/// PostgreSQL records an exclusion's PLAIN COLUMN elements in `pg_constraint.conkey`,
/// and `conkey` IS the catalog's cascade predicate: `ALTER TABLE ... DROP COLUMN`
/// removes every constraint whose `conkey` names the dropped attribute. So this is
/// the same list `snapshot_schema` recovers on the live side, built from the closed
/// IR instead of the catalog.
///
/// An EXCLUDE `definition` is deliberately EMPTY in the snapshot (PostgreSQL
/// canonicalizes exclusion bodies differently from the authored render, so
/// `apply::drift::constraint_definition_is_comparable` tracks EXCLUDE by presence and
/// name), which leaves the cascade's `definition`-parsing fallback nothing to match.
/// Without this provenance an EXCLUDE never cascaded and survived as a PHANTOM the
/// live catalog does not have.
///
/// EXPRESSION elements and the `WHERE` predicate are DELIBERATELY EXCLUDED, and this
/// is where an EXCLUDE parts ways with [`IndexSnapshot::expr_cascade_columns`]. Both
/// reach a column through a parse tree, but PostgreSQL treats the two dependencies
/// differently: an expression/partial INDEX is cascaded away silently, while an
/// EXCLUDE that names the column only through `indexprs` / `indpred` carries a NORMAL
/// dependency and PostgreSQL REFUSES the drop outright - measured on PostgreSQL 18.4:
///
/// ```text
/// ALTER TABLE t ADD CONSTRAINT x EXCLUDE USING btree (((a + 1)) WITH =);
/// ALTER TABLE t DROP COLUMN a;
/// -- ERROR:  cannot drop column a of table t because other objects depend on it
/// -- DETAIL:  constraint x on table t depends on column a of table t
/// ```
///
/// The engine only ever emits a plain `ALTER TABLE ... DROP COLUMN` (never
/// `CASCADE` - see `DdlEmitter::drop_column_up`), so that refusal is the real
/// behaviour: the statement aborts and the constraint is still there. Folding those
/// columns in would drop a constraint PostgreSQL did NOT drop, which is worse drift
/// than the phantom. The live side agrees by construction - `conkey` holds attnum `0`
/// for an expression element, which resolves to no name, and the predicate is not in
/// `conkey` at all.
///
/// A column reached through BOTH a plain element and an expression/predicate still
/// cascades, because the plain element's auto dependency is enough - measured on
/// PostgreSQL 18.4, `EXCLUDE USING gist (a WITH =, ((b + 1)) WITH =)` loses the whole
/// constraint to `DROP COLUMN a`. Matching on the plain elements alone gets that
/// right.
///
/// Sorted and deduplicated so the list is deterministic, matching what
/// `render::dml::expr_column_refs` gives the CHECK producers and the post-condition
/// the `RenameColumn` arm re-establishes.
fn exclusion_cascade_columns(elements: &[ExclusionElement]) -> Vec<String> {
    let mut columns = elements
        .iter()
        .filter_map(|element| match &element.target {
            ColumnOrExpr::Column { name } => Some(name.clone()),
            ColumnOrExpr::Expr { .. } => None,
        })
        .collect::<Vec<_>>();
    columns.sort();
    columns.dedup();
    columns
}

/// True iff the LOCAL column list of a constraint `definition` contains `column`.
///
/// The `DropColumn` cascade's FALLBACK, used only for a constraint whose producer
/// recorded no `cascade_columns`: PG auto-drops a UNIQUE/FK constraint when one of
/// its local columns is dropped, so the fold must too. Both column-list constraints
/// carry their LOCAL columns as the LEADING parenthesized group - `UNIQUE (cols)`
/// and `FOREIGN KEY (cols) REFERENCES <schema>.<tgt>(id)...` - so we parse that first
/// `(...)`. The FK's REFERENCED column list (`(id)`) comes AFTER `REFERENCES`, never
/// in the leading group, so a column named `id` on the REFERENCING side is matched
/// while the referenced `(id)` is correctly ignored. The system `<table>_pkey`
/// (`PRIMARY KEY (id)`) is the only PK, so it cannot false-match a non-`id` user
/// column.
///
/// NOT usable for a CHECK, whose leading group is the EXPRESSION rather than a
/// column list: `CHECK ((qty >= 0))` yields the token `qty >= 0`, which matches no
/// column, so the constraint would survive a drop PostgreSQL cascaded. Every CHECK
/// producer records `cascade_columns` instead, and the cascade REFUSES a CHECK that
/// reaches it without them rather than falling back here.
///
/// Columns are spelled by [`constraintdef_cols`] (conditional quoting), so each
/// comma-separated token is trimmed of whitespace and surrounding double-quotes
/// before the exact compare. This intentionally matches a column that PARTIALLY
/// covers a multi-column constraint — PG drops such a constraint whole.
fn constraint_local_columns_contain(definition: &str, column: &str) -> bool {
    let Some(open) = definition.find('(') else {
        return false;
    };
    let Some(close_rel) = definition[open + 1..].find(')') else {
        return false;
    };
    let cols = &definition[open + 1..open + 1 + close_rel];
    cols.split(',')
        .any(|tok| tok.trim().trim_matches('"') == column)
}

/// Re-render a constraint `definition`'s LEADING PARENTHESIZED GROUP with `from`
/// renamed to `to`, or `None` to leave the definition untouched.
///
/// Only sound for a kind whose leading group is a LOCAL COLUMN LIST - `UNIQUE`,
/// `PRIMARY KEY`, `FOREIGN KEY`. A string literal is not legal in that grammar, so
/// the trap that rules out text matching for a CHECK body
/// (`CHECK ((status <> 'qty'::text))` survives dropping `qty` - measured) is
/// structurally unreachable here. Callers must kind-gate; this does not.
///
/// The group is RE-RENDERED through [`constraintdef_cols`] rather than
/// substring-swapped because quoting is CONDITIONAL: `a` -> `order` has to produce
/// `UNIQUE ("order")`, and a naive swap gives `UNIQUE (order)`, trading one drift
/// for another. Everything outside the group - the `FOREIGN KEY` tail's
/// `REFERENCES ...`, MATCH, `ON UPDATE` / `ON DELETE`, DEFERRABLE and any ` NOT VALID`
/// suffix - is spliced through byte-identically.
///
/// ROUND-TRIP GUARD: the UNCHANGED parse is re-rendered first and must equal the
/// original group BYTE FOR BYTE. A definition the parser mishandles therefore stays
/// stale rather than becoming corrupt - `UNIQUE ("a""b")` parses to `a""b` (the
/// `trim_matches('"')` cannot see an embedded escaped quote), re-renders to
/// `"a""""b"`, fails the compare, and is left alone. The same guard catches a name
/// carrying a `,` or a `)`, both of which the split-on-comma / first-`)` parse gets
/// wrong. It lives in the shared [`rename_definition_column_group`] speller.
///
/// Also the SQLite rename REBUILD's rewrite, through
/// [`crate::render::declarative::rename_column_in_constraint_definitions`]. That
/// replay splices this same `definition` into the rebuilt table's `CREATE TABLE`, so
/// both replays that own a `TableSnapshot` now spell a moved column through THIS
/// function - not through the quoted-run walk the inline-CHECK rewrite uses, which
/// cannot tell the FK's LOCAL column list from its REFERENCED one.
pub(crate) fn rename_constraint_definition_column(
    definition: &str,
    from: &str,
    to: &str,
) -> Option<String> {
    rename_definition_column_group(definition, definition.find('(')?, from, to)
}

/// Re-render the parenthesized column list that OPENS at byte `open` with `from`
/// renamed to `to`, or `None` to leave `definition` untouched.
///
/// The single speller behind both column-list groups a constraint `definition` can
/// carry: the LEADING local list ([`rename_constraint_definition_column`]) and the
/// FOREIGN KEY tail's REFERENCED list ([`rewrite_incoming_fk_column_targets`]).
/// Both are CLOSED IDENTIFIER LISTS in the grammar, so neither can hold a string
/// literal - the trap that rules out text rewriting for a CHECK body is structurally
/// unreachable in either. Everything outside the group is spliced through
/// byte-identically.
///
/// ROUND-TRIP GUARD: the UNCHANGED parse is re-rendered through [`constraintdef_cols`]
/// first and must equal the original group BYTE FOR BYTE. A group the split-on-comma
/// / first-`)` parse mishandles - an embedded escaped quote, a `,` or a `)` inside a
/// name - therefore stays STALE rather than becoming CORRUPT.
fn rename_definition_column_group(
    definition: &str,
    open: usize,
    from: &str,
    to: &str,
) -> Option<String> {
    let close = definition[open + 1..].find(')')? + open + 1;
    let group = &definition[open + 1..close];
    let columns = group
        .split(',')
        .map(|column| column.trim().trim_matches('"').to_string())
        .collect::<Vec<_>>();
    if columns.is_empty() || columns.iter().any(String::is_empty) {
        return None;
    }
    if constraintdef_cols(&columns) != group {
        return None;
    }
    if !columns.iter().any(|column| column == from) {
        return None;
    }
    let renamed = columns
        .into_iter()
        .map(|column| {
            if column == from {
                to.to_string()
            } else {
                column
            }
        })
        .collect::<Vec<_>>();
    Some(format!(
        "{}{}{}",
        &definition[..=open],
        constraintdef_cols(&renamed),
        &definition[close..]
    ))
}

/// Canonical snapshot shape for a named UNIQUE key.
///
/// PostgreSQL exposes both the constraint and its implicit same-name index. MySQL
/// collapses `CONSTRAINT name UNIQUE (...)` and `UNIQUE KEY name (...)` to the same
/// `STATISTICS`/`SHOW INDEX` object, so its authoritative snapshot retains only the
/// ordered unique index. Keeping a synthetic MySQL constraint here would make a
/// clean apply report that constraint as missing on every re-introspection.
fn unique_constraint(name: &str, columns: &[String], dialect: SqlDialect) -> FoldedConstraint {
    FoldedConstraint {
        constraint: (!matches!(dialect, SqlDialect::Mysql)).then(|| ConstraintSnapshot {
            name: name.to_string(),
            kind: "UNIQUE".to_string(),
            definition: format!("UNIQUE ({})", constraintdef_cols(columns)),
            comment: None,
            cascade_columns: None,
        }),
        index: Some(IndexSnapshot::btree(
            name.to_string(),
            true,
            columns.to_vec(),
        )),
    }
}

/// The folded catalog shape a stand-alone `addConstraint` op produces. FK uses the
/// shared `ir_fk_*` (no index); UNIQUE uses the dialect-canonical key shape from
/// [`unique_constraint`]; CHECK is deferred.
fn add_constraint_snapshot(
    table: &str,
    project_schema: &str,
    constraint: &IrConstraint,
    dialect: SqlDialect,
) -> Result<FoldedConstraint, FoldError> {
    let name = constraint.name.as_deref();
    match &constraint.kind {
        IrConstraintKind::Fk {
            columns,
            references_table,
            references_columns,
            on_delete,
            on_update,
            deferrable,
            initially_deferred,
            // NOT VALID is recorded, because `pg_get_constraintdef` renders the tail
            // for as long as `convalidated` is false. Folding the eventual-validated
            // body instead made every unvalidated constraint report drift over the
            // whole window the facet exists to open - and the declarative differ
            // refuses a foreign-key body change outright, so it also refused the next
            // deploy. `Op::ValidateConstraint` removes the tail again.
            not_valid,
        } => {
            if columns.is_empty() {
                return Err(FoldError::Unsupported(
                    "addConstraint(fk) with no local column",
                ));
            }
            Ok(FoldedConstraint {
                constraint: Some(ir_fk_constraint_snapshot_for_columns(
                    project_schema,
                    table,
                    name,
                    columns,
                    references_table,
                    references_columns,
                    on_delete.map(RefAction::as_token),
                    on_update.map(RefAction::as_token),
                    deferrable.unwrap_or(false),
                    initially_deferred.unwrap_or(false),
                    *not_valid == Some(true),
                    dialect,
                )),
                index: None,
            })
        }
        IrConstraintKind::Unique { columns } => {
            let cname = name.map_or_else(
                || derived_constraint_name(table, columns, "key"),
                str::to_string,
            );
            Ok(unique_constraint(&cname, columns, dialect))
        }
        IrConstraintKind::Check { expr, .. } => {
            if !matches!(dialect, SqlDialect::Postgres) {
                return Err(FoldError::Unsupported(
                    "addConstraint(check) is PostgreSQL-only",
                ));
            }
            let cname = name.map_or_else(
                || derived_check_constraint_name(table, expr),
                str::to_string,
            );
            let rendered = crate::render::dml::render_expr_inline(expr, dialect)
                .map_err(|e| FoldError::Render(e.to_string()))?;
            // Same structural provenance as the table-level CHECK above.
            let cascade_columns = crate::render::dml::expr_column_refs(expr, dialect)
                .map_err(|e| FoldError::Render(e.to_string()))?;
            Ok(FoldedConstraint {
                constraint: Some(ConstraintSnapshot {
                    name: cname,
                    kind: "CHECK".to_string(),
                    definition: format!("CHECK ({rendered})"),
                    comment: None,
                    cascade_columns: Some(cascade_columns),
                }),
                index: None,
            })
        }
        IrConstraintKind::Exclusion { elements, .. } => {
            if !dialect.supports(Capability::ExclusionConstraint) {
                return Err(FoldError::Unsupported(
                    "addConstraint exclusion constraint is PostgreSQL-only",
                ));
            }
            let cname = name.map_or_else(
                || derived_exclusion_constraint_name(table, elements),
                str::to_string,
            );
            render_exclusion_constraint_body(&constraint.kind, dialect)
                .map_err(fold_lower_error)?;
            Ok(FoldedConstraint {
                constraint: Some(ConstraintSnapshot {
                    name: cname,
                    kind: "EXCLUDE".to_string(),
                    // PG canonicalizes exclusion bodies differently from the authored
                    // render. Drift tracks EXCLUDE by presence/name + kind only,
                    // matching `snapshot_schema`.
                    definition: String::new(),
                    comment: None,
                    // Same structural provenance as the createTable EXCLUDE above:
                    // the empty `definition` leaves the DropColumn cascade's parsing
                    // fallback nothing to match, so the plain column elements - PG's
                    // own `conkey` predicate - are recorded directly.
                    cascade_columns: Some(exclusion_cascade_columns(elements)),
                }),
                index: None,
            })
        }
    }
}

// ===========================================================================
// Fold-and-RECOVER: the seam the gen-types layer
// consumes. `fold_ops` produces the drift SchemaSnapshot (and correctly DEFERS a
// CHECK there — it cannot render the SQL `definition` offline); this seam
// reconstructs, per column, the FieldDescriptor / wire-`FieldDef` the SDK type
// inference consumes, by RECOVERING facets from the applied migration shape:
//   - type / vector dims / encrypted (default mode) / ref target / id_prefix /
//     vector_metric — already on the descriptor `ir_column_to_field` builds from
//     the op `IrColumn` (the carried fields + the structural ones);
//   - enum / min / max — LIFTED from the canonical closed-AST CHECK shapes
//     (`recover_check_facet`), bounded to recognized shapes (an unrecognized CHECK
//     is left unprojected — the column types as its base scalar; NEVER a panic).
// This is the offline analogue of `crud/introspect_schema.rs`'s runtime derive.
// ===========================================================================

/// A facet recovered by LIFTING a canonical closed-AST CHECK. Bounded
/// to the SDK-emitted shapes over a SINGLE column; an unrecognized CHECK yields
/// `None` from [`recover_check_facet`] and is left unprojected.
#[derive(Debug, Clone, PartialEq)]
pub enum RecoveredCheck {
    /// `col >= a [AND col <= b]` / `col <= b` → a numeric `min`/`max` bound.
    Range {
        /// The bounded column.
        column: String,
        /// Lower bound (`>=`), if present.
        min: Option<f64>,
        /// Upper bound (`<=`), if present.
        max: Option<f64>,
    },
    /// `col = v` or a left-folded OR-chain `col = v1 OR col = v2 OR …` → an enum
    /// membership over a single column. (The op.* closed AST has NO `IN` node, so
    /// the canonical enum shape is the eq/eq-OR-chain; this is the closed-AST
    /// analogue of the declarative `IN (...)` shape.)
    Enum {
        /// The constrained column.
        column: String,
        /// The accepted values, in source order.
        values: Vec<serde_json::Value>,
    },
}

/// Convert a numeric [`crate::model::ir::IrScalar`] literal to `f64` for a `min`/`max` bound, or
/// `None` for a non-numeric literal (which is not a recognized range bound).
///
/// **Precision note.** A `Decimal` bound is narrowed to `f64` here. This is
/// NOT a new precision loss: the reconstructed `FieldDescriptor.min`/`max` is itself
/// an `f64` (`declarative.rs`), so the recovered facet cannot be wider than `f64`
/// regardless; and an `Int` literal is wire-bounded to ±2^53 by `IrScalar` (the
/// `< 2^53` JS-safe-integer guard), so the `Int` arm is lossless. A large `Decimal`
/// CHECK bound narrows to the same `f64` the declarative path would carry — the two
/// sides stay byte-identical (the round-trip parity), they just share the `f64` model.
/// A future reader should NOT assume a lossless decimal bound here.
fn ir_scalar_as_f64(s: &crate::model::ir::IrScalar) -> Option<f64> {
    use crate::model::ir::IrScalar;
    match s {
        IrScalar::Int(i) => Some(*i as f64),
        // A decimal literal is carried as its lossless string; parse for the bound
        // (narrowed to f64 — see the precision note above; matches the f64 facet).
        IrScalar::Decimal(d) => d.parse::<f64>().ok(),
        _ => None,
    }
}

/// Convert a [`crate::model::ir::IrScalar`] literal to the `serde_json::Value` an enum membership
/// carries (string / number / bool), mirroring the declarative `enum_values`
/// domain. `None` for a non-scalar (`Bytes`) the enum facet does not model.
fn ir_scalar_to_json(s: &crate::model::ir::IrScalar) -> Option<serde_json::Value> {
    use crate::model::ir::IrScalar;
    match s {
        IrScalar::Bool(b) => Some(serde_json::Value::Bool(*b)),
        IrScalar::Int(i) => Some(serde_json::Value::from(*i)),
        IrScalar::Str(s) => Some(serde_json::Value::String(s.clone())),
        // A decimal enum member is rare; carry it as a JSON number when it parses
        // losslessly into the f64 domain `serde_json::Number` admits, else `None`
        // (an unparseable decimal is not a recognized enum member).
        IrScalar::Decimal(d) => d
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(serde_json::Value::Number),
        // `Null` / `Bytes` are not enum-member shapes the facet models. The
        // descriptor facet vocabulary also has no tagged int64 carrier: do not
        // project one to a JSON number and silently erase its exact-value tag.
        IrScalar::Null | IrScalar::Int64(_) | IrScalar::Bytes(_) => None,
    }
}

/// Match `BinOp{op, ColRef(col), Literal(value)}` — the canonical "column compared
/// to a literal" leaf the SDK emits for a bound / enum member. Returns
/// `(column, value)` only for this exact shape (literal on the RHS).
fn match_col_op_lit(
    expr: &crate::model::expr::Expr,
    want: crate::model::expr::BinaryOp,
) -> Option<(&str, &crate::model::ir::IrScalar)> {
    use crate::model::expr::Expr;
    if let Expr::BinOp { op, lhs, rhs } = expr {
        if *op == want {
            if let (Expr::ColRef { name, table: None }, Expr::Literal { value }) =
                (lhs.as_ref(), rhs.as_ref())
            {
                return Some((name.as_str(), value));
            }
        }
    }
    None
}

/// Lift a canonical closed-AST CHECK `Expr` back to a
/// [`RecoveredCheck`] facet, or `None` for an unrecognized shape (which stays
/// unprojected — the column types as its base scalar; NEVER a panic).
///
/// Recognized shapes (all over a SINGLE column):
/// - `col >= n` → `Range { min }`;
/// - `col <= n` → `Range { max }`;
/// - `col >= a AND col <= b` (same column) → `Range { min, max }`;
/// - `col = v` → `Enum { [v] }`;
/// - `col = v1 OR col = v2 OR …` (left-folded, same column) → `Enum { [v1, v2, …] }`.
///
/// The round-trip caveat applies: this is a RECOGNIZED-shape inverse, not a
/// total one. A hand-written `c('age').ge(0).and(c('age').le(120))` is
/// indistinguishable from a `min/max` facet (both reconstruct the same bound),
/// which is acceptable; an arbitrary boolean CHECK is NOT projectable and yields
/// `None`.
#[must_use]
pub fn recover_check_facet(expr: &crate::model::expr::Expr) -> Option<RecoveredCheck> {
    use crate::model::expr::{BinaryOp, Expr};

    // Range: `col >= a AND col <= b` over the SAME column.
    if let Expr::BinOp {
        op: BinaryOp::And,
        lhs,
        rhs,
    } = expr
    {
        let lo = match_col_op_lit(lhs, BinaryOp::Ge);
        let hi = match_col_op_lit(rhs, BinaryOp::Le);
        if let (Some((c1, lo_v)), Some((c2, hi_v))) = (lo, hi) {
            if c1 == c2 {
                if let (Some(min), Some(max)) = (ir_scalar_as_f64(lo_v), ir_scalar_as_f64(hi_v)) {
                    return Some(RecoveredCheck::Range {
                        column: c1.to_string(),
                        min: Some(min),
                        max: Some(max),
                    });
                }
            }
        }
    }
    // Range: a lone `col >= n`.
    if let Some((c, v)) = match_col_op_lit(expr, BinaryOp::Ge) {
        if let Some(min) = ir_scalar_as_f64(v) {
            return Some(RecoveredCheck::Range {
                column: c.to_string(),
                min: Some(min),
                max: None,
            });
        }
    }
    // Range: a lone `col <= n`.
    if let Some((c, v)) = match_col_op_lit(expr, BinaryOp::Le) {
        if let Some(max) = ir_scalar_as_f64(v) {
            return Some(RecoveredCheck::Range {
                column: c.to_string(),
                min: None,
                max: Some(max),
            });
        }
    }
    // Enum: a single `col = v` OR a left-folded OR-chain of `col = v` over one column.
    if let Some((column, values)) = recover_enum_chain(expr) {
        return Some(RecoveredCheck::Enum { column, values });
    }
    None
}

/// Walk a left-folded OR chain of `col = v` equalities over a SINGLE column,
/// returning `(column, [values])` in source (left-to-right) order, or `None` if
/// the chain mixes columns / contains a non-`(col = literal)` leaf.
fn recover_enum_chain(expr: &crate::model::expr::Expr) -> Option<(String, Vec<serde_json::Value>)> {
    use crate::model::expr::{BinaryOp, Expr};
    let mut column: Option<String> = None;
    let mut values: Vec<serde_json::Value> = Vec::new();

    // Collect leaves of the left-folded OR tree in source order.
    fn collect<'a>(e: &'a Expr, leaves: &mut Vec<&'a Expr>) -> bool {
        if let Expr::BinOp {
            op: BinaryOp::Or,
            lhs,
            rhs,
        } = e
        {
            collect(lhs, leaves) && collect(rhs, leaves)
        } else {
            leaves.push(e);
            true
        }
    }
    let mut leaves: Vec<&Expr> = Vec::new();
    if !collect(expr, &mut leaves) {
        return None;
    }
    for leaf in leaves {
        let (c, v) = match_col_op_lit(leaf, BinaryOp::Eq)?;
        match &column {
            Some(existing) if existing != c => return None, // mixed columns ⇒ not an enum
            Some(_) => {}
            None => column = Some(c.to_string()),
        }
        values.push(ir_scalar_to_json(v)?);
    }
    // A bare non-OR `col = v` yields exactly one leaf — still a valid singleton enum.
    column.map(|c| (c, values))
}

/// FK policy recovered from a single-column `IrConstraintKind::Fk` constraint, to
/// lift onto the matching `ref` column's descriptor (recover from the applied
/// FK constraint). `on_delete`/`on_update` are the camelCase SDK tokens. The
/// `deferrable` bit is not carried on the Fk constraint; omitted means the
/// SQL/Postgres default (`NOT DEFERRABLE`), so the folded descriptor leaves it
/// unset.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RecoveredFk {
    /// The single referencing column the policy attaches to.
    column: String,
    /// The constraint this policy came from, when it was authored with a name.
    ///
    /// Carried so a later `dropConstraint` can un-lift exactly this policy.
    /// Matching on the COLUMN instead would be wrong: two constraints can touch
    /// one column, and dropping either would strip the other one policy.
    /// `None` for an unnamed inline constraint, which `dropConstraint` cannot
    /// target anyway.
    name: Option<String>,
    /// `ON DELETE` policy token (`restrict`/`cascade`/…), if set.
    on_delete: Option<String>,
    /// `ON UPDATE` policy token, if set.
    on_update: Option<String>,
}

/// Lift the FK policy from a single-column FK constraint. Multi-column FKs (which the
/// platform never authors for a `t.ref()` — it is always a single `(col) → (id)` FK)
/// are not projected onto a column descriptor (`None`).
fn recover_fk_policy(
    columns: &[String],
    on_delete: Option<RefAction>,
    on_update: Option<RefAction>,
) -> Option<RecoveredFk> {
    if columns.len() != 1 {
        return None;
    }
    Some(RecoveredFk {
        column: columns[0].clone(),
        // Filled in by the caller, which is where the constraint name is in scope.
        name: None,
        on_delete: on_delete.map(|a| a.as_token().to_string()),
        on_update: on_update.map(|a| a.as_token().to_string()),
    })
}

/// The FieldDef reconstruction seam.
///
/// Replay `ops` into the coherent folded state (fail-closed via [`fold_ops`]),
/// then reconstruct, per table, the wire-`FieldDef` map (`{ <col>: { type, … } }`)
/// the built-in db type inference consumes — recovering each facet from the
/// applied shape:
///
/// - **type / vector dims / encrypted(default) / ref / id_prefix / vector_metric**
///   from the op `IrColumn` via `ir_column_to_field` (reusing the shared
///   descriptor machinery — the carried fields + structural ones);
/// - **enum / min / max** LIFTED from canonical CHECKs ([`recover_check_facet`]),
///   bounded to recognized shapes;
/// - the column SET (after `addColumn` / `dropColumn` / `renameColumn`) tracked so
///   the reconstructed map matches the FOLDED logical state, never a stale
///   createTable snapshot.
///
/// The returned `Value` per table is exactly what
/// [`descriptor_to_sdk_schema`](crate::render::declarative::descriptor_to_sdk_schema)
/// emits — the SAME shape the declarative differ consumes losslessly — so the
/// `.d.ts` emitter maps `descriptor → t.*()` builder calls off one facet table.
///
/// # Errors
/// Any structural-incoherence [`FoldError`] [`fold_ops`] raises (the stream must
/// be coherent first), or an unrepresentable inject rule in `effective`.
pub fn fold_to_field_defs(
    ops: &[Op],
    dialect: SqlDialect,
    project_schema: &str,
    effective: &EffectivePolicy,
) -> Result<BTreeMap<String, serde_json::Value>, FoldError> {
    // 1. Fail-closed coherence. `fold_ops` is the structural-coherence oracle
    //    (add-to-missing-table, drop-absent-column, duplicate-create, …).
    fold_ops(ops, dialect, project_schema, effective)?;

    // 2. Build a per-table FieldDescriptor map by replaying the ops' column shapes.
    //    We track FieldDescriptors (not snapshots) because the descriptor carries
    //    the recoverable facets (encrypted/vector*/ref/id_prefix) the snapshot
    //    flattens to a `data_type` string. Drops/renames keep it in lock-step with
    //    the folded state — this replay IS the live column set (no snapshot needed).
    //
    //    COLUMN ORDER is preserved with `IndexMap` so the reconstructed FieldDef map
    //    matches the createTable column order (the SAME order `descriptor_to_sdk_schema`
    //    emits from `descriptor.fields`) — the round-trip parity compares the
    //    serialized maps, so a sorted-vs-declared order would false-mismatch.
    let mut tables: BTreeMap<
        String,
        indexmap::IndexMap<String, crate::render::declarative::FieldDescriptor>,
    > = BTreeMap::new();
    // Per-table CHECK facets to lift onto the matching column at the end. A CHECK
    // over an unrecognized shape is left unprojected (the column types as its base
    // scalar) — NOT an error.
    let mut checks: BTreeMap<String, Vec<RecoveredCheck>> = BTreeMap::new();
    // Per-table recovered FK policy (`onDelete`/`onUpdate`) to lift onto the
    // referencing column at the end. A reference authored as a TABLE-LEVEL
    // `IrConstraintKind::Fk` (a `foreignKeys` entry, or a later `addConstraint`)
    // keeps its policy on the constraint, where `ir_column_to_field` cannot see it,
    // so we lift it here -- the "recover from the applied FK constraint" path. A
    // reference carried on the column itself (`ColType::Ref` plus, for the facets
    // the brand cannot express, `IrColumn.references`) recovers through
    // `ir_column_to_field` and needs no lift.
    let mut fks: BTreeMap<String, Vec<RecoveredFk>> = BTreeMap::new();
    // The named-type definitions the column types only NAME. `ColType::Enum` carries
    // `{ name, schema }` and nothing else, so the members a `t.enum("x")` column is
    // closed over are not on the column at all - they arrive in a separate
    // `Op::CreateEnum`. This is the SAME registry the DDL lower
    // (`apply_named_type_column_metadata`) and the snapshot fold
    // (`apply_fold_named_type_column_metadata`) resolve those names through, reused
    // rather than re-spelled so the three replays cannot disagree about which
    // definition a name resolves to after a drop-and-recreate.
    let mut named_types = NamedTypeRegistry::default();

    let replay_ops = flatten_dialectal_ops(ops, dialect)?;
    for op in replay_ops {
        match op {
            Op::CreateTable {
                name,
                columns,
                constraints,
                schema,
                ..
            } => {
                let effective_schema = schema.as_deref().unwrap_or(project_schema);
                let resolved_inject = ResolvedInject::for_table(effective, effective_schema, name)
                    .map_err(|error| FoldError::Render(error.to_string()))?;
                let injected_prefix_len = resolved_inject_prefix_len(columns, &resolved_inject);
                let mut cols: indexmap::IndexMap<
                    String,
                    crate::render::declarative::FieldDescriptor,
                > = indexmap::IndexMap::new();
                for (idx, c) in columns.iter().enumerate() {
                    let in_resolved_id_primary_key = idx < injected_prefix_len
                        && c.name == "id"
                        && resolved_inject.owns_id_primary_key();
                    let mut field = fold_create_column_to_field(c, in_resolved_id_primary_key);
                    lift_named_enum_membership(&mut field, &c.ty, &named_types);
                    cols.insert(c.name.clone(), field);
                }
                tables.insert(name.clone(), cols);
                for c in constraints {
                    match &c.kind {
                        IrConstraintKind::Check { expr, .. } => {
                            if let Some(facet) = recover_check_facet(expr) {
                                checks.entry(name.clone()).or_default().push(facet);
                            }
                        }
                        IrConstraintKind::Fk {
                            columns,
                            on_delete,
                            on_update,
                            ..
                        } => {
                            if let Some(mut recovered) =
                                recover_fk_policy(columns, *on_delete, *on_update)
                            {
                                recovered.name = c.name.clone();
                                fks.entry(name.clone()).or_default().push(recovered);
                            }
                        }
                        // A SINGLE-COLUMN UNIQUE IS A COLUMN FACET. Authoring it as
                        // `t.string().unique()` set `unique` on the descriptor;
                        // authoring the same constraint at table level did not, so
                        // two routes to one uniqueness produced different generated
                        // types. Multi-column unique is a table KEY, not a column
                        // facet - the same single-column line `recover_fk_policy`
                        // already draws for foreign keys.
                        IrConstraintKind::Unique { columns } if columns.len() == 1 => {
                            if let Some(field) = tables
                                .get_mut(name)
                                .and_then(|c| c.get_mut(&columns[0]))
                            {
                                field.unique = true;
                            }
                        }
                        // Exhaustive: a multi-column unique is a table key, and an
                        // exclusion constraint has no FieldDescriptor slot at all
                        // (its access method, elements and predicate are
                        // table-level).
                        IrConstraintKind::Unique { .. } | IrConstraintKind::Exclusion { .. } => {}
                    }
                }
            }
            Op::DropTable { table, .. } => {
                tables.remove(table);
                checks.remove(table);
                fks.remove(table);
            }
            Op::RenameTable { table, to, .. } => {
                // Re-key the table's reconstructed column map AND its pending
                // CHECK / FK facets from the old name to the new one, so gen-types
                // sees the RENAMED table (the same wholesale move the structural
                // `fold_ops` does). A column dropped/renamed/added under the new
                // name afterward resolves; the old name no longer does.
                if let Some(mut cols) = tables.remove(table) {
                    // A generated expression may QUALIFY its column references with
                    // the enclosing table. The `env.db.ts` replay in `gen_types`
                    // rewrites that qualifier on a table rename and this one did not,
                    // so the two artifacts shipped side by side described the same
                    // column under different table names - the runtime descriptor
                    // still naming the collection that no longer exists.
                    for field in cols.values_mut() {
                        if let Some(generated) = field.generated.as_mut() {
                            crate::render::gen_types::rename_expr_table(
                                &mut generated.expr,
                                table,
                                to,
                            );
                        }
                    }
                    tables.insert(to.clone(), cols);
                }
                if let Some(c) = checks.remove(table) {
                    checks.insert(to.clone(), c);
                }
                if let Some(f) = fks.remove(table) {
                    fks.insert(to.clone(), f);
                }
                // INCOMING references must follow the rename for gen-types too:
                // every OTHER table's `ref` column whose target is the renamed table
                // points at a now-dead collection name. Re-target it to the new name
                // so the emitted TS `ref` resolves (the gen-types twin
                // of the `fold_ops` incoming-FK rewrite). A self-ref (a `ref` column
                // in the renamed table pointing at itself) is re-targeted too.
                for cols in tables.values_mut() {
                    for f in cols.values_mut() {
                        if f.references.as_deref() == Some(table.as_str()) {
                            f.references = Some(to.clone());
                        }
                    }
                }
            }
            Op::AddColumn {
                table,
                column,
                ty,
                nullable,
                default,
                value_format,
                vector_metric,
                case_sensitive,
                mask,
                generated,
                identity,
                ..
            } => {
                if let Some(cols) = tables.get_mut(table) {
                    // AddColumn carries no `id_prefix` (an added column is never
                    // the system PK), but it DOES carry the `vector_metric` + standalone
                    // `mask` facets, so the reconstructed descriptor for an added vector /
                    // masked column round-trips the metric opclass / `zero-migrate:mask` mask
                    // through the offline fold.
                    let mut field = ir_column_to_field(&IrColumn {
                        name: column.clone(),
                        ty: ty.clone(),
                        nullable: *nullable,
                        default: default.clone(),
                        unique: None,
                        value_format: value_format.clone(),
                        references: None,
                        id_prefix: None,
                        collation: None,
                        case_sensitive: *case_sensitive,
                        vector_metric: *vector_metric,
                        mask: *mask,
                        generated: generated.clone(),
                        identity: *identity,
                    });
                    lift_named_enum_membership(&mut field, ty, &named_types);
                    cols.insert(column.clone(), field);
                }
            }
            Op::DropColumn { table, column, .. } => {
                if let Some(cols) = tables.get_mut(table) {
                    // `shift_remove` preserves the relative order of the surviving
                    // columns (vs `swap_remove`, which would reorder).
                    cols.shift_remove(column);
                }
            }
            // THE FACET OPS. These change a column's shape without adding or
            // removing one, and the replay used to drop them on its catch-all -
            // so a migration that widened a type or tightened a NOT NULL produced
            // generated types describing the OLD shape.
            //
            // The new shape comes from `retype_field_descriptor`, which builds the
            // target column through the SAME `ir_column_to_field` a createTable
            // column goes through. A second mapping here would drift from it
            // invisibly: the fold would emit a shape that no longer matches what a
            // create of the same ColType produces.
            //
            // This arm used to assign `col_type_to_token(to_type)` and NOTHING ELSE,
            // which is wrong in both directions, because the token is not the whole
            // type - `String { length }`, `Char { length }` and `Vector { vector }`
            // carry their parameter in a SIBLING descriptor field. It kept
            // `maxLength: 24` on a column widened to `varchar(40)`, and emitted a
            // bare `{"type":"char"}` for a retype INTO `char(8)`. On SQLite this map
            // is the DESIRED snapshot the 12-step rebuild renders `CREATE TABLE`
            // from (`engine`'s `sqlite_schemas`), so the shape is DDL, not just
            // codegen. The per-facet verdict and its measurements live on
            // `retype_field_descriptor`.
            Op::SetColumnType {
                table,
                column,
                to_type,
                ..
            } => {
                if let Some(field) = tables.get_mut(table).and_then(|c| c.get_mut(column)) {
                    crate::render::lower::retype_field_descriptor(field, to_type);
                    // `retype_field_descriptor` CLEARS `enum_values` - the old type's
                    // membership is a contract over storage the column no longer has.
                    // A retype INTO a named enum re-earns it from the target type's
                    // own definition, so a retype to `T` and a create of `T` still
                    // describe the same column.
                    lift_named_enum_membership(field, to_type, &named_types);
                }
            }
            Op::SetColumnNotNull { table, column, .. } => {
                if let Some(field) = tables.get_mut(table).and_then(|c| c.get_mut(column)) {
                    field.required = true;
                }
            }
            Op::DropColumnNotNull { table, column, .. } => {
                if let Some(field) = tables.get_mut(table).and_then(|c| c.get_mut(column)) {
                    field.required = false;
                }
            }
            // The DEFAULT facet, both directions. `ir_default_to_value` is the same
            // conversion `ir_column_to_field` applies to a createTable column, so a
            // default set by an op serialises identically to one declared inline.
            Op::SetColumnDefault {
                table,
                column,
                value,
                ..
            } => {
                if let Some(field) = tables.get_mut(table).and_then(|c| c.get_mut(column)) {
                    field.default = crate::render::lower::ir_default_to_value(value);
                }
            }
            Op::DropColumnDefault { table, column, .. } => {
                if let Some(field) = tables.get_mut(table).and_then(|c| c.get_mut(column)) {
                    field.default = None;
                }
            }
            // UN-LIFT the FK policy the dropped constraint granted. addConstraint
            // was already replayed (it FEEDS the lift); its inverse was not, so the
            // policy outlived the constraint and gen-types kept emitting an
            // ON DELETE the database no longer has.
            Op::DropConstraint { table, name, .. } => {
                if let Some(recovered) = fks.get_mut(table) {
                    recovered.retain(|fk| fk.name.as_deref() != Some(name.as_str()));
                }
            }
            Op::RenameColumn {
                table, from, to, ..
            } => {
                if let Some(cols) = tables.get_mut(table) {
                    // Preserve the renamed column's POSITION: find its index, remove,
                    // re-insert at the same slot under the new key.
                    if let Some(idx) = cols.get_index_of(from) {
                        if let Some((_, mut field)) = cols.shift_remove_index(idx) {
                            field.name = to.clone();
                            cols.shift_insert(idx, to.clone(), field);
                        }
                    }
                    // A generated expression reads OTHER columns, so the rename has
                    // to walk the whole table rather than just the renamed column.
                    // `FieldDescriptor.generated` keeps the closed `Expr` rather than
                    // rendered SQL, so the rewrite matches column references and
                    // cannot corrupt a string literal that spells the old name.
                    // Without it the descriptor this fold ships describes a column
                    // the database no longer has, while the authoring types emitted
                    // beside it - which run this same rewrite - describe the new one.
                    for field in cols.values_mut() {
                        if let Some(generated) = field.generated.as_mut() {
                            crate::render::gen_types::rename_expr_column(
                                &mut generated.expr,
                                table,
                                from,
                                to,
                                true,
                            );
                        }
                    }
                }
                // The CHECK and foreign-key facets recovered from this table's
                // constraints are lifted onto columns BY NAME once the op stream has
                // been walked. They still carry the pre-rename name, and the lift
                // looks the column up rather than failing, so leaving them would
                // silently drop a `min`/`max` bound or an `onDelete`/`onUpdate`
                // policy instead of reporting anything.
                if let Some(recovered) = checks.get_mut(table) {
                    for check in recovered.iter_mut() {
                        match check {
                            RecoveredCheck::Range { column, .. }
                            | RecoveredCheck::Enum { column, .. } => {
                                if column == from {
                                    column.clone_from(to);
                                }
                            }
                        }
                    }
                }
                if let Some(recovered) = fks.get_mut(table) {
                    for fk in recovered.iter_mut() {
                        if &fk.column == from {
                            fk.column.clone_from(to);
                        }
                    }
                }
            }
            Op::AlterPrimaryKey { table, action, .. } => {
                if let Some(columns) = tables.get_mut(table) {
                    for column in action.drop_identity_from() {
                        if let Some(field) = columns.get_mut(column) {
                            field.identity = None;
                        }
                    }
                }
            }
            Op::AddConstraint {
                table, constraint, ..
            } => {
                if let IrConstraintKind::Fk {
                    columns,
                    on_delete,
                    on_update,
                    ..
                } = &constraint.kind
                {
                    if let Some(mut recovered) = recover_fk_policy(columns, *on_delete, *on_update)
                    {
                        recovered.name = constraint.name.clone();
                        fks.entry(table.clone()).or_default().push(recovered);
                    }
                }
                if let IrConstraintKind::Check { expr, .. } = &constraint.kind {
                    if let Some(facet) = recover_check_facet(expr) {
                        checks.entry(table.clone()).or_default().push(facet);
                    }
                }
                // The addConstraint route to the same single-column uniqueness the
                // inline createTable route sets. Handling only one leaves two
                // authoring forms disagreeing in the generated types.
                if let IrConstraintKind::Unique { columns } = &constraint.kind {
                    if columns.len() == 1 {
                        if let Some(field) = tables
                            .get_mut(table)
                            .and_then(|c| c.get_mut(&columns[0]))
                        {
                            field.unique = true;
                        }
                    }
                }
            }
            // THE ENUM'S MEMBERS, which no column carries.
            //
            // `ColType::Enum { name, schema }` is a NAME. The closed set a
            // `t.enum("issue_status")` column is validated against arrives here, in a
            // separate op, and used to be dropped on the catch-all - so `envDbTs`
            // emitted `t.enum("issue_status")` while `runtimeJson`, folded from the
            // same op stream in the same call, described the column as a bare
            // `{"type":"string"}`. The database enforced the set (a native
            // `CREATE TYPE` on PostgreSQL, an inlined `CHECK (... IN (...))` on SQLite,
            // an inlined `ENUM(...)` on MySQL) and only the artifact the deployed app
            // installs `env.db` from forgot it.
            //
            // Registering the definition does NOT put a CHECK in the DDL: this replay
            // ends at `descriptor_to_sdk_schema`, and the membership becomes the wire
            // `FieldDef`'s `enum` key. The DDL comes from `fold_ops` / the lower,
            // which resolve the same registry into the storage each dialect wants.
            Op::CreateEnum { name, values, .. } => {
                named_types
                    .create_enum(name, project_schema, values)
                    .map_err(fold_named_type_error)?;
            }
            Op::DropEnum { name, .. } => {
                named_types.drop_enum(name);
            }
            // Every other op (DML, index, type/nullability alters, drop*) does not
            // change the reconstructed column-facet shape.
            // EXHAUSTIVE FROM HERE, and that is the point of this arm rather than a
            // `_`. Six stale facets shipped behind the catch-all this file used to
            // end with - a column type, nullability both ways, a default both
            // ways, and an FK policy that outlived its constraint. Each was legal,
            // accepted, and then dropped on the floor by the replay, so gen-types
            // described a schema the database no longer had. Listing every variant
            // makes the next op that touches a column a COMPILE ERROR here.
            //
            // `synchronizeIdentity` is a MEASURED no-op: it advances a live
            // sequence value to clear existing rows and does not alter the identity
            // DECLARATION, so the descriptor correctly does not move.
            Op::SynchronizeIdentity { .. }
            // `dialectal` never reaches here - `flatten_dialectal_ops` expands it
            // into the replay list above - but the match must still name it.
            | Op::Dialectal { .. }
            // Relation-level ops: they create, move or drop whole relations that
            // are not this map's tables, or alter table-level settings that carry
            // no column facet.
            | Op::CreatePartition { .. }
            | Op::AttachPartition { .. }
            | Op::DetachPartition { .. }
            | Op::DropPartition { .. }
            | Op::SetTableOptions { .. }
            | Op::CreateView { .. }
            | Op::DropView { .. }
            | Op::CreateSequence { .. }
            | Op::AlterSequence { .. }
            | Op::DropSequence { .. }
            // Index and comment ops: an index is not a column facet, and a comment
            // is not projected into the FieldDescriptor at all.
            | Op::CreateIndex { .. }
            | Op::DropIndex { .. }
            | Op::Comment { .. }
            // `validateConstraint` promotes a NOT VALID constraint to validated;
            // the policy it carries was already lifted when it was added.
            | Op::ValidateConstraint { .. }
            // DML moves ROWS, never column shape.
            | Op::Insert { .. }
            | Op::Update { .. }
            | Op::Delete { .. }
            | Op::Backfill { .. }
            // Named-type and vendor objects: they live outside the per-table
            // column map this replay builds.
            | Op::CreateDomain { .. }
            | Op::DropDomain { .. }
            | Op::CreateSchema { .. }
            | Op::DropSchema { .. }
            | Op::CreateExtension { .. }
            | Op::DropExtension { .. }
            | Op::CreateRole { .. }
            | Op::AlterRole { .. }
            | Op::DropRole { .. }
            | Op::DropOwnedBy { .. }
            | Op::Grant { .. }
            | Op::Revoke { .. }
            | Op::SetRls { .. }
            | Op::CreatePolicy { .. }
            | Op::DropPolicy { .. }
            | Op::CreateTrigger { .. }
            | Op::DropTrigger { .. }
            | Op::CreateFunction { .. }
            | Op::DropFunction { .. }
            // Raw SQL is opaque; nothing can be recovered from it by construction.
            | Op::PgRaw { .. } => {}
        }
    }

    // 3. Lift the recovered CHECK facets onto their columns, then emit the
    //    wire-FieldDef map. `cols` IS the folded logical column set — `dropColumn`
    //    removed its entry and `renameColumn` re-keyed it during the replay above —
    //    so a column dropped after a CHECK that referenced it never resurrects (the
    //    facet's `cols.get_mut` simply finds nothing).
    let mut out: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for (table, mut cols) in tables {
        for facet in checks.remove(&table).unwrap_or_default() {
            match facet {
                RecoveredCheck::Range { column, min, max } => {
                    if let Some(f) = cols.get_mut(&column) {
                        if min.is_some() {
                            f.min = min;
                        }
                        if max.is_some() {
                            f.max = max;
                        }
                    }
                }
                RecoveredCheck::Enum { column, values } => {
                    if let Some(f) = cols.get_mut(&column) {
                        f.enum_values = Some(values);
                    }
                }
            }
        }
        // Lift the recovered FK policy onto the ref column. `on_delete`/`on_update`
        // come from the Fk constraint; `deferrable` stays unset because the FK IR
        // has no explicit deferrable bit and omitted means the SQL/Postgres default.
        for fk in fks.remove(&table).unwrap_or_default() {
            if let Some(f) = cols.get_mut(&fk.column) {
                if f.ty == "ref" {
                    f.on_delete = fk.on_delete;
                    f.on_update = fk.on_update;
                }
            }
        }
        let desc = CollectionDescriptor {
            name: table.clone(),
            owner_app: FOLD_OWNER_APP.to_string(),
            fields: cols.into_values().collect(),
            indexes: Vec::new(),
            runtime_options: Default::default(),
        };
        out.insert(
            table,
            crate::render::declarative::descriptor_to_sdk_schema(&desc),
        );
    }
    Ok(out)
}

/// Lift a NAMED enum type's members onto the column descriptor that only names it.
///
/// # Why this is not a line in `col_type_to_token`
///
/// `ColType::Enum { name, schema }` carries the name and nothing else, so no
/// function whose whole input is one [`IrColumn`] can populate `enum_values` - the
/// members live in [`Op::CreateEnum`]. This runs where the op stream is in scope
/// and the registry has already seen the definition.
///
/// # Absent beats wrong
///
/// An unresolvable name leaves `enum_values` untouched rather than guessing. The
/// case is real and dialect-specific: `genArtifacts` is DB-free by contract and
/// never reads a live catalog, so a column whose `createEnum` is not in the folded
/// stream has no provable membership. PostgreSQL still folds it (the native type
/// reference needs only the NAME, so `apply_fold_named_type_column_metadata`
/// succeeds), which is exactly the case where a fabricated set would ship as fact.
/// SQLite and MySQL INLINE the value list into the column's storage, so their folds
/// already fail closed before reaching here.
///
/// # Not through `ColType::Encrypted`
///
/// Deliberately no recursion into the wrapped type. An encrypted column stores
/// ciphertext; a membership over the plaintext domain is not a contract its stored
/// bytes satisfy, and `field_check_constraints` would turn it into a CHECK no row
/// could pass. `ColType::Domain` is excluded for its own reason: a domain's
/// constraint is an arbitrary predicate, not a closed value set, and `enum_values`
/// asserts a closed set.
fn lift_named_enum_membership(
    field: &mut crate::render::declarative::FieldDescriptor,
    ty: &ColType,
    named_types: &NamedTypeRegistry,
) {
    let ColType::Enum { name, .. } = ty else {
        return;
    };
    let Ok(def) = named_types.enum_def(name) else {
        return;
    };
    // DECLARATION order, not sorted: PostgreSQL's enum declaration order IS its
    // sort order, and the SQLite/MySQL inlined value lists preserve it too.
    field.enum_values = Some(
        def.values
            .iter()
            .map(|value| serde_json::Value::String(value.clone()))
            .collect(),
    );
}

fn fold_create_column_to_field(
    c: &IrColumn,
    in_resolved_id_primary_key: bool,
) -> crate::render::declarative::FieldDescriptor {
    let mut field = ir_column_to_field_resolved_create(c);
    if in_resolved_id_primary_key && c.id_prefix.is_some() {
        field.ty = "id".to_string();
        field.required = false;
    }
    field
}

/// Length of the policy-resolved injected prefix carried by `columns`.
///
/// The create-table resolver prepends [`ResolvedInject::columns`] in canonical
/// sealed-policy order. A legacy ID-prefix declaration may retain `id_prefix` and
/// a typed reference on the injected `id`; an integer identity may replace that
/// slot. Those are the same two policy-approved folds accepted by
/// `resolve_create_table_policy`. Every other facet must match the canonical
/// injected [`IrColumn`] exactly. A no-inject policy has an empty prefix.
fn resolved_inject_prefix_len(columns: &[IrColumn], inject: &ResolvedInject) -> usize {
    if columns.len() < inject.columns().len() {
        return 0;
    }
    let matches = columns
        .iter()
        .zip(inject.columns())
        .all(|(actual, expected)| {
            resolved_injected_column_matches(
                actual,
                expected,
                inject.owns_id_primary_key() && expected.name == "id",
            )
        });
    if matches {
        inject.columns().len()
    } else {
        0
    }
}

fn resolved_injected_column_matches(
    actual: &IrColumn,
    expected: &IrColumn,
    is_id_primary_key: bool,
) -> bool {
    if actual == expected {
        return true;
    }
    if actual.name != expected.name || !is_id_primary_key {
        return false;
    }
    if actual.identity.is_some()
        && matches!(
            actual.ty,
            ColType::SmallInt | ColType::Int | ColType::BigInt
        )
    {
        return true;
    }
    let mut folded_base = actual.clone();
    folded_base.id_prefix = None;
    folded_base.references = None;
    folded_base == *expected
}

// ===========================================================================
// The createTable producer: descriptor → op.* `createTable`.
//
// `fold_to_field_defs` (above) is the RECOVERY direction (ops → FieldDef map);
// this is its faithful INVERSE over the authoring surface (descriptor → ops),
// the structural inverse of `ir_column_to_field` + `recover_check_facet`. It is
// the producer the round-trip parity test threads:
//
//   author (declarative)         descriptor_to_sdk_schema(descriptor)   ─┐
//        │                                                               ├─ MUST be byte-identical
//   descriptors_to_create_ops  → ops → fold_to_field_defs(ops)         ─┘
//
// WHY a NEW producer (closing the producer gap): the existing
// `generate_ops` (`scaffold.rs`) derives ops from a `SchemaSnapshot`, whose
// `ColumnSnapshot.data_type` has already FLATTENED away the declared-only facets
// (`idPrefix`/`vectorMetric`) and the CHECK-borne facets (`enum`/`min`/`max`) — it
// even fail-closes on vector/encrypted goodies (`col_type_for_data_type` →
// `UnsupportedColumnType`) and pins `id_prefix: None`. A snapshot-sourced producer
// therefore CANNOT round-trip the rich facets; the chain would be lossy. This
// descriptor-sourced producer threads every facet through, so the author→generate→
// fold chain is lossless for exactly the facets the SDK type inference consumes.
// ===========================================================================

/// A structured error from the [`descriptors_to_create_ops`] producer. The only
/// failure modes are an unmappable type token, an unrepresentable CHECK facet
/// value (a non-numeric `min`/`max`, an unscalar `enum` member), or a confined
/// table-shape resolution error — fail closed rather than emit an op whose fold
/// would silently drop the facet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProduceError {
    /// A descriptor field carried a `type` token with no closed [`ColType`].
    UnknownType {
        /// The table.
        table: String,
        /// The column.
        column: String,
        /// The unmapped token.
        token: String,
    },
    /// A `min`/`max`/`enum` facet value could not be represented as a closed-AST
    /// `Literal` (e.g. a non-numeric `min`, a `null`/object `enum` member) — the
    /// CHECK could not be authored faithfully, so the producer refuses rather than
    /// drop the bound.
    UnrepresentableFacet {
        /// The table.
        table: String,
        /// The column.
        column: String,
        /// Which facet (`min`/`max`/`enum`).
        facet: &'static str,
    },
    /// The confined table-shape resolver rejected the produced createTable op.
    TableShape {
        /// The table.
        table: String,
        /// Human-readable resolver error.
        message: String,
    },
}

impl std::fmt::Display for ProduceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProduceError::UnknownType {
                table,
                column,
                token,
            } => write!(
                f,
                "produce: `{table}.{column}` has unmappable type token `{token}`"
            ),
            ProduceError::UnrepresentableFacet {
                table,
                column,
                facet,
            } => write!(
                f,
                "produce: `{table}.{column}` has an unrepresentable `{facet}` facet value"
            ),
            ProduceError::TableShape { table, message } => {
                write!(
                    f,
                    "produce: `{table}` could not resolve table shape: {message}"
                )
            }
        }
    }
}

impl std::error::Error for ProduceError {}

/// Map a descriptor `(type_token, references?)` back to the closed [`ColType`] — the
/// structural inverse of [`col_type_to_token`](crate::render::lower). The token-set is the
/// SDK `FieldDef` spelling the descriptor carries (`ir_column_to_field` produces it).
///
/// Canonicalisation note (round-trip fidelity): the forward map is many-to-one
/// (`Int`/`BigInt`→`"int"`, `String`/`Text`/`Uuid`→`"string"`, `Float`/`Decimal`→
/// `"number"`). This inverse picks the canonical `ColType` whose forward token is the
/// SAME token, so a descriptor authored with token `t` round-trips to token `t`
/// (`"int"`→`Int`→`"int"`, `"string"`→`Text`→`"string"`, `"number"`→`Float`→
/// `"number"`). The fold compares FieldDef maps by these TOKENS, so the round-trip is
/// byte-identical for the type field.
fn token_to_col_type(f: &crate::render::declarative::FieldDescriptor) -> Option<ColType> {
    let inner = |token: &str| -> Option<ColType> {
        Some(match token {
            "string" => ColType::Text,
            "int" | "integer" => ColType::Int,
            "smallInt" => ColType::SmallInt,
            "bigInt" => ColType::BigInt,
            "number" | "float" => ColType::Double,
            "real" => ColType::Real,
            "boolean" => ColType::Boolean,
            "json" | "object" | "array" => ColType::Json,
            "date" | "timestamp" => ColType::Timestamp,
            "bytes" => ColType::Bytes,
            "inet" => ColType::Inet,
            "textArray" => ColType::TextArray,
            "char" => ColType::Char {
                length: u32::try_from(f.char_len?).ok().filter(|len| *len > 0)?,
            },
            "geoPoint" => ColType::GeoPoint,
            _ => return None,
        })
    };
    match f.ty.as_str() {
        // A legacy internal platform-ID descriptor represents the policy-owned
        // `id` slot as a UUID carrier with an `id_prefix`.
        "id" => Some(ColType::Uuid),
        // A `ref` column carries the FK target on `references`.
        "ref" => f
            .references
            .clone()
            .map(|references| ColType::Ref { references }),
        // A `vector(N)` column carries dims on `vector_dims`.
        "vector" => f.vector_dims.map(|d| ColType::Vector { vector: d as u32 }),
        other => {
            let base = inner(other)?;
            // An encrypted column carries the `encrypted` facet PLUS the inner token
            // as `ty`; wrap the inner ColType (the inverse of `col_type_to_token`'s
            // `Encrypted{of}` → inner token).
            if f.encrypted.is_some() {
                Some(ColType::Encrypted { of: Box::new(base) })
            } else {
                Some(base)
            }
        }
    }
}

/// Build a closed-AST `Literal` from a descriptor's JSON facet value (`enum` member /
/// `min`/`max` bound), or `None` if the value has no [`crate::model::ir::IrScalar`] image.
/// The inverse of [`ir_scalar_to_json`].
fn json_to_ir_scalar(v: &serde_json::Value) -> Option<crate::model::ir::IrScalar> {
    use crate::model::ir::IrScalar;
    match v {
        serde_json::Value::Bool(b) => Some(IrScalar::Bool(*b)),
        serde_json::Value::String(s) => Some(IrScalar::Str(s.clone())),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(IrScalar::Int(i))
            } else {
                // A non-integer number is carried as its canonical decimal string,
                // matching `ir_scalar_as_f64` / `ir_scalar_to_json`'s Decimal arm.
                n.as_f64().map(|f| IrScalar::Decimal(f.to_string()))
            }
        }
        // `null` / array / object are not scalar facet members.
        _ => None,
    }
}

/// A numeric bound (`min`/`max`) as a closed-AST `Literal`, or `None` for a value
/// `recover_check_facet` would not lift back (non-numeric). Mirrors
/// [`ir_scalar_as_f64`]'s numeric-only domain so the CHECK round-trips.
fn f64_to_ir_literal(n: f64) -> crate::model::ir::IrScalar {
    use crate::model::ir::IrScalar;
    // Prefer the exact integer image (the `Int` arm `recover_check_facet` lifts
    // losslessly) when the bound is integral and JS-safe; else carry the decimal.
    if n.fract() == 0.0 && n.abs() < 9.007_199_254_740_992e15 {
        IrScalar::Int(n as i64)
    } else {
        IrScalar::Decimal(n.to_string())
    }
}

/// Build the per-column CHECK constraints (`enum` membership + `min`/`max` range) a
/// descriptor field declares, as closed-AST `Expr`s in EXACTLY the shapes
/// [`recover_check_facet`] recognises:
/// - `min`+`max` → `col >= min AND col <= max`;
/// - `min`-only → `col >= min`;  `max`-only → `col <= max`;
/// - `enum` → `col = v` (singleton) / left-folded `col = v1 OR col = v2 OR …`.
///
/// Returns the constraints in a stable order (range before enum) so the produced op
/// is deterministic.
fn facet_check_constraints(
    table: &str,
    f: &crate::render::declarative::FieldDescriptor,
) -> Result<Vec<IrConstraint>, ProduceError> {
    use crate::model::expr::{BinaryOp, Expr};
    let mut out = Vec::new();
    let col = || Expr::col(f.name.clone());

    // Range: min/max → `col >= min [AND col <= max]`.
    let range_expr = match (f.min, f.max) {
        (Some(min), Some(max)) => Some(Expr::BinOp {
            op: BinaryOp::And,
            lhs: Box::new(Expr::BinOp {
                op: BinaryOp::Ge,
                lhs: Box::new(col()),
                rhs: Box::new(Expr::lit(f64_to_ir_literal(min))),
            }),
            rhs: Box::new(Expr::BinOp {
                op: BinaryOp::Le,
                lhs: Box::new(col()),
                rhs: Box::new(Expr::lit(f64_to_ir_literal(max))),
            }),
        }),
        (Some(min), None) => Some(Expr::BinOp {
            op: BinaryOp::Ge,
            lhs: Box::new(col()),
            rhs: Box::new(Expr::lit(f64_to_ir_literal(min))),
        }),
        (None, Some(max)) => Some(Expr::BinOp {
            op: BinaryOp::Le,
            lhs: Box::new(col()),
            rhs: Box::new(Expr::lit(f64_to_ir_literal(max))),
        }),
        (None, None) => None,
    };
    if let Some(expr) = range_expr {
        out.push(IrConstraint {
            name: Some(format!("{table}_{}_range_check", f.name)),
            kind: IrConstraintKind::Check {
                expr,
                not_valid: None,
            },
        });
    }

    // Enum: `col = v` singleton / left-folded `col = v1 OR col = v2 OR …`.
    if let Some(values) = &f.enum_values {
        if !values.is_empty() {
            let mut leaves = Vec::with_capacity(values.len());
            for v in values {
                let scalar =
                    json_to_ir_scalar(v).ok_or_else(|| ProduceError::UnrepresentableFacet {
                        table: table.to_string(),
                        column: f.name.clone(),
                        facet: "enum",
                    })?;
                leaves.push(Expr::BinOp {
                    op: BinaryOp::Eq,
                    lhs: Box::new(col()),
                    rhs: Box::new(Expr::lit(scalar)),
                });
            }
            // Left-fold the leaves into the OR chain `recover_enum_chain` walks.
            let mut iter = leaves.into_iter();
            let mut expr = iter.next().expect("non-empty (values not empty)");
            for leaf in iter {
                expr = Expr::BinOp {
                    op: BinaryOp::Or,
                    lhs: Box::new(expr),
                    rhs: Box::new(leaf),
                };
            }
            out.push(IrConstraint {
                name: Some(format!("{table}_{}_enum_check", f.name)),
                kind: IrConstraintKind::Check {
                    expr,
                    not_valid: None,
                },
            });
        }
    }

    Ok(out)
}

/// The op.* `createTable` producer. Build the op.*
/// `createTable` ops a `Vec<CollectionDescriptor>` (the declarative authoring shape)
/// generates, threading EVERY facet the SDK type inference consumes:
///
/// - **type / ref / vector dims / encrypted** — onto the [`IrColumn`]'s [`ColType`]
///   (the inverse of [`col_type_to_token`](crate::render::lower));
/// - **idPrefix / vectorMetric** — onto the carried [`IrColumn`] fields;
/// - **required / unique** — onto `nullable` / `unique`;
/// - **default** — onto `default` (a typed literal);
/// - **enum / min / max** — as CHECK constraints in the closed-AST shapes
///   [`recover_check_facet`] lifts back (`facet_check_constraints`).
///
/// One resolved `Op::CreateTable` per descriptor, in descriptor order. The columns,
/// top-level `primary_key`, and indexes include exactly the active policy's resolved
/// injection before checksum/fold; an uncovered table remains author-shaped.
///
/// # Errors
/// [`ProduceError`] for an unmappable type token, an unrepresentable CHECK facet
/// value, or a table-shape resolution error — fail closed, never an op whose fold
/// would silently drop the facet.
pub fn descriptors_to_create_ops(
    descriptors: &[crate::render::declarative::CollectionDescriptor],
    project_schema: &str,
    effective: &zero_migrate_policy::EffectivePolicy,
) -> Result<Vec<Op>, ProduceError> {
    let mut ops = Vec::with_capacity(descriptors.len());
    for d in descriptors {
        let mut columns = Vec::with_capacity(d.fields.len());
        let mut constraints = Vec::new();
        for f in &d.fields {
            let ty = token_to_col_type(f).ok_or_else(|| ProduceError::UnknownType {
                table: d.name.clone(),
                column: f.name.clone(),
                token: f.ty.clone(),
            })?;
            // `required` ⇒ `nullable: Some(false)`; the descriptor models the
            // inverse of `nullable`, matching `ir_column_to_field`'s
            // `required = !nullable.unwrap_or(true)`. A non-required field stays
            // `None` (the `t.*` default-nullable image), so the wire bytes match.
            let nullable = if f.required { Some(false) } else { None };
            let default = f
                .default
                .as_ref()
                .map(|v| crate::model::ir::IrDefault::Literal {
                    value: json_value_to_ir_scalar_default(v),
                });
            let vector_metric = f
                .vector_metric
                .as_deref()
                .and_then(parse_vector_metric_token);
            // Carry a STANDALONE mask onto the produced IrColumn so the
            // round-trip (descriptors → ops → fold) keeps it. The encrypted
            // auto-mask `{ full, pii }` is NOT carried — it is re-implied by the
            // `ColType::Encrypted` carrier in `ir_column_to_field` (carrying it would
            // double-emit). A descriptor whose mask IS the encrypted auto-mask on an
            // encrypted column is therefore dropped here (recovered downstream); a
            // standalone/non-default mask is carried.
            let mask = standalone_mask_facet(f);
            // A `ref` field carries its FK target on the `ColType::Ref` brand, which
            // the shared snapshot builder ALREADY materializes into the derived
            // `<table>_<column>_fkey` constraint. Only the reference facets the brand
            // cannot express ride on a second carrier, so a plain `ref` keeps the
            // brand-only column image the recorder emits and the two artifact sources
            // stay byte-identical.
            let references = if f.ty == "ref" {
                ref_brand_reference_facets(f)
            } else {
                f.references
                    .as_ref()
                    .map(|table| column_reference_for_field(f, table))
            };
            columns.push(IrColumn {
                name: f.name.clone(),
                ty,
                nullable,
                default,
                unique: if f.unique { Some(true) } else { None },
                value_format: None,
                references,
                id_prefix: f.id_prefix.clone(),
                collation: None,
                vector_metric,
                case_sensitive: f.case_sensitive,
                mask,
                generated: f.generated.clone(),
                identity: f.identity,
            });
            constraints.extend(facet_check_constraints(&d.name, f)?);
        }
        // Carry the author-declared named indexes onto the produced createTable so the
        // round-trip (descriptors → ops → fold) keeps them. `resolve_create_table_policy`
        // extends this vec with exactly the active policy's injected indexes — it never
        // replaces it — so author + policy-owned indexes both survive.
        // An empty `_indexes` yields `Vec::new()`, byte-identical to the pre-index shape.
        let indexes = d.indexes.iter().map(index_descriptor_to_ir).collect();
        let op = Op::CreateTable {
            name: d.name.clone(),
            columns,
            primary_key: None,
            constraints,
            indexes,
            partition_by: None,
            runtime_options: Some(d.runtime_options.clone()),
            schema: None,
            existence_guard: None,
        };
        let ir = crate::model::ir::MigrationIr {
            inverse_ops: None,
            irreversible: None,
            ir_version: crate::model::ir::CURRENT_IR_VERSION,
            name: format!("produce_{}", d.name),
            owner_app: d.owner_app.clone(),
            ops: vec![op],
            flags: Default::default(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            checksum: None,
        };
        let resolved =
            crate::model::table_shape::resolve_create_table_policy(&ir, effective, project_schema)
                .map_err(|source| ProduceError::TableShape {
                    table: d.name.clone(),
                    message: source.to_string(),
                })?;
        ops.push(
            resolved
                .ops
                .into_iter()
                .next()
                .expect("single-op IR resolves to single-op IR"),
        );
    }
    Ok(ops)
}

/// The [`ColumnReference`] a declared reference field carries onto its produced
/// [`IrColumn`]: the target table plus the target column (defaulting to the
/// historical `id`), the explicit constraint name, and the referential actions.
fn column_reference_for_field(
    f: &crate::render::declarative::FieldDescriptor,
    target: &str,
) -> ColumnReference {
    ColumnReference {
        table: target.to_string(),
        column: f
            .reference_column
            .clone()
            .unwrap_or_else(|| "id".to_string()),
        on_delete: f.on_delete.as_deref().and_then(parse_ref_action),
        on_update: f.on_update.as_deref().and_then(parse_ref_action),
        name: f.reference_name.clone(),
    }
}

/// The reference carrier for a `ref`-branded field, or `None` when the brand
/// already says everything the field declares.
///
/// `ColType::Ref` names the target table but cannot express an explicit target
/// column, an explicit constraint name, or `ON DELETE`/`ON UPDATE`. Those facets
/// therefore ride on the column's [`ColumnReference`], which
/// [`crate::render::lower::ir_column_to_field`] prefers over the brand, so the
/// foreign key is still declared exactly ONCE. A field that declares none of them
/// gets no second carrier, keeping the produced column byte-identical to the
/// brand-only image the recorder emits for the same schema.
fn ref_brand_reference_facets(
    f: &crate::render::declarative::FieldDescriptor,
) -> Option<ColumnReference> {
    let target = f.references.as_ref()?;
    let declares_facets = f.reference_column.is_some()
        || f.reference_name.is_some()
        || f.on_delete.is_some()
        || f.on_update.is_some();
    declares_facets.then(|| column_reference_for_field(f, target))
}

/// Map a declared [`IndexDescriptor`](crate::render::declarative::IndexDescriptor)
/// (the SDK `_indexes` entry: `{ name, columns, unique }`) onto the closed
/// [`IrIndex`] the `createTable` op carries. Each column becomes a plain
/// [`IndexElement::Column`] (default order / no opclass / no collation — the
/// author-declared index surface is column-name + uniqueness only); every other
/// `IrIndex` facet (`using`/`where`/`include`/…) stays `None`, byte-identical to a
/// hand-authored plain named index. `unique == false` maps to `None` (the SQL
/// default), not `Some(false)`, so a non-unique index serializes identically to the
/// pre-index wire shape.
fn index_descriptor_to_ir(d: &crate::render::declarative::IndexDescriptor) -> IrIndex {
    IrIndex {
        name: Some(d.name.clone()),
        columns: d
            .columns
            .iter()
            .map(|name| IndexElement::Column {
                name: name.clone(),
                order: None,
                opclass: None,
                collation: None,
            })
            .collect(),
        unique: if d.unique { Some(true) } else { None },
        using: None,
        r#where: None,
        include: Vec::new(),
        with: None,
        only: None,
        nulls_not_distinct: None,
    }
}

/// Parse a descriptor FK-action token (`cascade`/`restrict`/`setNull`/`setDefault`/
/// `noAction`) back to the closed [`RefAction`]. An out-of-set token yields `None`
/// (no action emitted — checksum-neutral, the SQL default). Mirrors the SDK's
/// camelCase `FkAction` spelling.
fn parse_ref_action(token: &str) -> Option<RefAction> {
    match token {
        "cascade" => Some(RefAction::Cascade),
        "restrict" => Some(RefAction::Restrict),
        "setNull" => Some(RefAction::SetNull),
        "setDefault" => Some(RefAction::SetDefault),
        "noAction" => Some(RefAction::NoAction),
        _ => None,
    }
}

/// Parse a descriptor `vector_metric` token (`cosine`/`l2`/`innerProduct`) back to
/// the closed [`crate::model::ir::VectorMetric`] enum. An out-of-set token yields `None`
/// (the column then carries no metric — the kernel default), never a panic.
fn parse_vector_metric_token(token: &str) -> Option<crate::model::ir::VectorMetric> {
    use crate::model::ir::VectorMetric;
    match token {
        "cosine" => Some(VectorMetric::Cosine),
        "l2" => Some(VectorMetric::L2),
        "innerProduct" => Some(VectorMetric::InnerProduct),
        _ => None,
    }
}

/// Extract a STANDALONE [`crate::model::ir::IrMask`] from a descriptor's
/// `mask` JSON (`{ kind, classification }`), to carry on the produced [`IrColumn`].
///
/// Returns `None` when the field carries no mask, OR when the mask is exactly the
/// ENCRYPTED auto-mask (`{ full, pii }`) ON AN ENCRYPTED column — that mask is RE-IMPLIED
/// by the `ColType::Encrypted` carrier in [`crate::render::lower::ir_column_to_field`], so
/// carrying it here would double-source it and perturb the round-trip (the encrypted
/// auto-mask must come from the carrier, not the mask facet). A standalone mask on a
/// plaintext column, or a NON-default mask on an encrypted column (an explicit override),
/// IS carried. An unparseable kind/classification token yields `None` (fail-soft — the
/// closed-enum producer never panics; the round-trip's own gate catches a genuine drop).
fn standalone_mask_facet(
    f: &crate::render::declarative::FieldDescriptor,
) -> Option<crate::model::ir::IrMask> {
    let mask = f.mask.as_ref()?;
    let kind = mask.get("kind").and_then(serde_json::Value::as_str)?;
    let classification = mask
        .get("classification")
        .and_then(serde_json::Value::as_str)?;
    // Suppress the encrypted auto-mask: only when the column is ACTUALLY encrypted and
    // the mask is the exact kernel default. (A plaintext column authored with
    // `.mask({ full, pii })` is a real standalone mask and IS carried.)
    let is_encrypted = f.encrypted.is_some();
    if is_encrypted && kind == "full" && classification == "pii" {
        return None;
    }
    Some(crate::model::ir::IrMask {
        kind: parse_mask_kind_token(kind)?,
        classification: parse_classification_token(classification)?,
    })
}

/// Parse an SDK/IR-wire mask `kind` token (kebab `date-year`/`date-decade`; camelCase
/// otherwise) back to the closed [`crate::model::ir::IrMaskKind`]. Out-of-set ⇒ `None`.
fn parse_mask_kind_token(token: &str) -> Option<crate::model::ir::IrMaskKind> {
    use crate::model::ir::IrMaskKind;
    Some(match token {
        "full" => IrMaskKind::Full,
        "last4" => IrMaskKind::Last4,
        "first4" => IrMaskKind::First4,
        "email" => IrMaskKind::Email,
        "name" => IrMaskKind::Name,
        "date-year" => IrMaskKind::DateYear,
        "date-decade" => IrMaskKind::DateDecade,
        "none" => IrMaskKind::None,
        _ => return None,
    })
}

/// Parse an SDK/IR-wire `classification` token back to the closed
/// [`crate::model::ir::IrClassification`]. Out-of-set ⇒ `None`.
fn parse_classification_token(token: &str) -> Option<crate::model::ir::IrClassification> {
    use crate::model::ir::IrClassification;
    Some(match token {
        "public" => IrClassification::Public,
        "pii" => IrClassification::Pii,
        "spi" => IrClassification::Spi,
        "phi" => IrClassification::Phi,
        "pci" => IrClassification::Pci,
        "internal" => IrClassification::Internal,
        _ => return None,
    })
}

/// Map a descriptor `default` JSON value to a closed-AST [`crate::model::ir::IrScalar`] for an
/// `IrDefault::Literal`. The inverse of `ir_default_to_value`; a non-scalar default
/// (array/object/null) maps to `IrScalar::Null` (the SDK never authors those as a
/// column default, and the round-trip fixtures use scalar defaults).
fn json_value_to_ir_scalar_default(v: &serde_json::Value) -> crate::model::ir::IrScalar {
    json_to_ir_scalar(v).unwrap_or(crate::model::ir::IrScalar::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::expr::Expr;
    use crate::model::ir::{
        IndexElement, IrScalar, MigrationIr, TableRuntimeOptions, TableRuntimeOptionsPatch,
        TableStrictness, ValueFormat, CURRENT_IR_VERSION,
    };
    use crate::model::policy::SchemaScope;
    use crate::model::snapshot::IdDefaultSnapshot;

    use crate::model::table_shape::{
        effective_policy_from_charter_toml, resolve_create_table_policy,
    };
    use crate::model::validate::{validate_ir_scoped, Dialect, UnsupportedKind, CODE_UNSUPPORTED};

    const SCHEMA: &str = "proj_test";

    fn fold(ops: &[Op]) -> Result<SchemaSnapshot, FoldError> {
        fold_ops(
            ops,
            SqlDialect::Postgres,
            SCHEMA,
            &crate::test_fixtures::confined_charter(),
        )
    }

    fn validate_ops(ops: Vec<Op>, dialect: Dialect) -> crate::model::validate::AuthoringError {
        let ir = crate::model::ir::MigrationIr {
            inverse_ops: None,
            irreversible: None,
            ir_version: crate::model::ir::CURRENT_IR_VERSION,
            name: "fold_validate".to_string(),
            owner_app: "app_fold".to_string(),
            ops,
            flags: Default::default(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            checksum: None,
        };
        validate_ir_scoped(&ir, dialect, Some(&SchemaScope::Unconfined)).unwrap_err()
    }

    fn assert_validate_ops_ok(ops: Vec<Op>, dialect: Dialect) {
        let ir = crate::model::ir::MigrationIr {
            inverse_ops: None,
            irreversible: None,
            ir_version: crate::model::ir::CURRENT_IR_VERSION,
            name: "fold_validate".to_string(),
            owner_app: "app_fold".to_string(),
            ops,
            flags: Default::default(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            checksum: None,
        };
        validate_ir_scoped(&ir, dialect, Some(&SchemaScope::Unconfined)).expect("ops validate");
    }

    fn col(name: &str, ty: ColType, nullable: bool) -> IrColumn {
        IrColumn {
            name: name.to_string(),
            ty,
            nullable: Some(nullable),
            default: None,
            unique: None,
            value_format: None,
            references: None,
            id_prefix: None,
            collation: None,
            case_sensitive: None,
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
        }
    }

    fn create(name: &str, columns: Vec<IrColumn>) -> Op {
        let op = Op::CreateTable {
            name: name.to_string(),
            columns,
            primary_key: None,
            constraints: Vec::new(),
            indexes: Vec::new(),
            partition_by: None,
            runtime_options: None,
            schema: None,
            existence_guard: None,
        };
        let ir = MigrationIr {
            inverse_ops: None,
            irreversible: None,
            ir_version: CURRENT_IR_VERSION,
            name: "fold_create".to_string(),
            owner_app: "app_fold".to_string(),
            ops: vec![op],
            flags: Default::default(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            checksum: None,
        };
        resolve_create_table_policy(&ir, &crate::test_fixtures::confined_charter(), "app")
            .expect("test createTable resolves")
            .ops
            .into_iter()
            .next()
            .expect("resolved op")
    }

    /// A folded table carries the platform system columns (`id`, timestamps, …)
    /// PLUS the user columns — proving the fold routes through the SHARED builder
    /// (`build_table_snapshot` injects the system fields), not a re-implementation.
    #[test]
    fn create_table_injects_system_fields_via_shared_builder() {
        let snap = fold(&[create("users", vec![col("email", ColType::Text, false)])]).unwrap();
        let t = snap.tables.get("users").expect("users table folded");
        let names: Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
        assert!(
            names.contains(&"id"),
            "system `id` column injected by the shared builder"
        );
        assert!(names.contains(&"email"), "user column present");
        // The `<table>_pkey` PK constraint the shared builder injects.
        assert!(
            t.constraints
                .iter()
                .any(|c| c.name == "users_pkey" && c.kind == "PRIMARY KEY"),
            "system PK constraint injected"
        );
    }

    #[test]
    fn no_inject_fold_preserves_author_updated_at_shape() {
        let op = Op::CreateTable {
            name: "events".to_string(),
            columns: vec![col("updated_at", ColType::Text, true)],
            primary_key: None,
            constraints: Vec::new(),
            indexes: Vec::new(),
            partition_by: None,
            runtime_options: None,
            schema: None,
            existence_guard: None,
        };
        let snap = fold_ops(
            &[op],
            SqlDialect::Postgres,
            SCHEMA,
            &crate::test_fixtures::no_inject("app"),
        )
        .expect("no-inject create folds verbatim");
        let events = &snap.tables["events"];
        assert_eq!(events.columns.len(), 1, "no ambient columns are injected");
        assert_eq!(events.columns[0].name, "updated_at");
        assert_eq!(events.columns[0].data_type, "text");
        assert!(events.columns[0].nullable);
        assert!(events.indexes.is_empty());
        assert!(events.constraints.is_empty());
    }

    #[test]
    fn no_inject_fold_preserves_uuid_columns_named_id() {
        let create_with_id = Op::CreateTable {
            name: "external_keys".to_string(),
            columns: vec![col("id", ColType::Uuid, true)],
            primary_key: None,
            constraints: Vec::new(),
            indexes: Vec::new(),
            partition_by: None,
            runtime_options: None,
            schema: None,
            existence_guard: None,
        };
        let create_then_add = Op::CreateTable {
            name: "imported_keys".to_string(),
            columns: vec![col("label", ColType::Text, true)],
            primary_key: None,
            constraints: Vec::new(),
            indexes: Vec::new(),
            partition_by: None,
            runtime_options: None,
            schema: None,
            existence_guard: None,
        };
        let add_id = Op::AddColumn {
            table: "imported_keys".to_string(),
            column: "id".to_string(),
            ty: ColType::Uuid,
            nullable: Some(true),
            default: None,
            value_format: None,
            case_sensitive: None,
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
            schema: None,
            existence_guard: None,
        };

        let snap = fold_ops(
            &[create_with_id, create_then_add, add_id],
            SqlDialect::Postgres,
            SCHEMA,
            &crate::test_fixtures::no_inject("app"),
        )
        .expect("no-inject UUID ids fold verbatim");

        for table in ["external_keys", "imported_keys"] {
            let id = snap.tables[table]
                .columns
                .iter()
                .find(|column| column.name == "id")
                .expect("author id remains present");
            assert_eq!(id.data_type, "uuid");
        }
    }

    #[test]
    fn schema_qualified_create_uses_the_same_scoped_inject_for_fold_and_recovery() {
        let effective = effective_policy_from_charter_toml(
            r#"policy_version = 1

[[inject]]
scope = { include = ["tenant_special.events"] }
mandatory = true
primary_key = ["id"]
author_primary_key = "forbid"
columns = [
  { name = "id", type = "text", nullable = false },
]
"#,
        )
        .expect("schema-scoped inject policy composes");
        let mut id = col("id", ColType::Uuid, true);
        id.id_prefix = Some("event".to_string());
        let raw = MigrationIr {
            inverse_ops: None,
            irreversible: None,
            ir_version: CURRENT_IR_VERSION,
            name: "scoped_create".to_string(),
            owner_app: "app_fold".to_string(),
            ops: vec![Op::CreateTable {
                name: "events".to_string(),
                columns: vec![id, col("payload", ColType::Json, true)],
                primary_key: Some(vec!["id".to_string()]),
                constraints: Vec::new(),
                indexes: Vec::new(),
                partition_by: None,
                runtime_options: None,
                schema: Some("tenant_special".to_string()),
                existence_guard: None,
            }],
            flags: Default::default(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            checksum: None,
        };
        let resolved = resolve_create_table_policy(&raw, &effective, SCHEMA)
            .expect("the explicit schema selects the scoped inject");

        let snapshot = fold_ops(&resolved.ops, SqlDialect::Postgres, SCHEMA, &effective)
            .expect("fold uses the create op's explicit schema");
        assert_eq!(
            snapshot.tables["events"]
                .columns
                .iter()
                .find(|column| column.name == "payload")
                .and_then(|column| column.default.as_deref()),
            Some("'{}'::jsonb"),
            "the scoped injected prefix is recognized by snapshot folding"
        );

        let fields = fold_to_field_defs(&resolved.ops, SqlDialect::Postgres, SCHEMA, &effective)
            .expect("runtime recovery uses the create op's explicit schema");
        assert_eq!(fields["events"]["id"]["type"], serde_json::json!("id"));
        assert_eq!(
            fields["events"]["id"]["idPrefix"],
            serde_json::json!("event")
        );
    }

    #[test]
    fn fold_ops_onto_preserves_base_and_advances_complete_table_shape() {
        let create_op = create("events", vec![col("payload", ColType::Json, true)]);
        let base = fold(std::slice::from_ref(&create_op)).expect("base schema folds");
        let tail = vec![
            Op::RenameColumn {
                table: "events".to_string(),
                from: "payload".to_string(),
                to: "body".to_string(),
                ty: ColType::Json,
                schema: None,
                existence_guard: None,
            },
            Op::SetColumnDefault {
                table: "events".to_string(),
                column: "body".to_string(),
                value: IrDefault::Container {
                    kind: crate::model::ir::EmptyContainerKind::Object,
                },
                schema: None,
                existence_guard: None,
            },
        ];

        let projected = fold_ops_onto(
            &base,
            &tail,
            SqlDialect::Postgres,
            SCHEMA,
            &crate::test_fixtures::confined_charter(),
        )
        .expect("tail projects onto catalog base");
        let expected = fold(&std::iter::once(create_op).chain(tail).collect::<Vec<_>>())
            .expect("combined replay folds");

        assert_eq!(projected, expected);
        let events = projected.tables.get("events").expect("events survives");
        assert!(events.columns.iter().all(|column| column.name != "payload"));
        let body = events
            .columns
            .iter()
            .find(|column| column.name == "body")
            .expect("renamed column is projected");
        assert_eq!(body.default.as_deref(), Some("'{}'::jsonb"));
    }

    #[test]
    fn set_column_default_preserves_id_surface_literal_semantics() {
        let upper_uuid = "AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA";
        for dialect in [SqlDialect::Postgres, SqlDialect::Mysql, SqlDialect::Sqlite] {
            let ops = vec![
                create("members", vec![col("member_key", ColType::Uuid, false)]),
                Op::SetColumnDefault {
                    table: "members".to_string(),
                    column: "member_key".to_string(),
                    value: IrDefault::Literal {
                        value: IrScalar::Str(upper_uuid.to_string()),
                    },
                    schema: None,
                    existence_guard: None,
                },
            ];
            let snapshot = fold_ops(
                &ops,
                dialect,
                SCHEMA,
                &crate::test_fixtures::confined_charter(),
            )
            .expect("UUID history folds");
            let member_key = snapshot.tables["members"]
                .columns
                .iter()
                .find(|column| column.name == "member_key")
                .expect("UUID column survives");
            let expected = if dialect == SqlDialect::Postgres {
                IdDefaultSnapshot::UuidLiteral(format!("\"{}\"", upper_uuid.to_ascii_lowercase()))
            } else {
                IdDefaultSnapshot::Literal(format!("\"{upper_uuid}\""))
            };
            assert_eq!(member_key.id_default.as_ref(), Some(&expected));
        }

        let mut type_id = col("type_key", ColType::Text, false);
        type_id.value_format = Some(ValueFormat::TypeId {
            prefix: String::new(),
        });
        let decimal = "12345678901234567890123456";
        let mysql = fold_ops(
            &[
                create("type_keys", vec![type_id]),
                Op::SetColumnDefault {
                    table: "type_keys".to_string(),
                    column: "type_key".to_string(),
                    value: IrDefault::Literal {
                        value: IrScalar::Decimal(decimal.to_string()),
                    },
                    schema: None,
                    existence_guard: None,
                },
            ],
            SqlDialect::Mysql,
            SCHEMA,
            &crate::test_fixtures::confined_charter(),
        )
        .expect("MySQL TypeID history folds");
        let type_key = mysql.tables["type_keys"]
            .columns
            .iter()
            .find(|column| column.name == "type_key")
            .expect("TypeID column survives");
        assert_eq!(
            type_key.id_default.as_ref(),
            Some(&IdDefaultSnapshot::Literal(format!("\"{decimal}\"")))
        );
    }

    #[test]
    fn synchronize_identity_fold_validates_target_without_changing_schema() {
        let create_op = create("orders", vec![col("description", ColType::Text, true)]);
        let base = fold(std::slice::from_ref(&create_op)).expect("base schema folds");
        let synchronize = Op::SynchronizeIdentity {
            table: "orders".to_string(),
            column: "id".to_string(),
            writes_quiesced: "orders_import_window".to_string(),
            schema: None,
        };

        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite, SqlDialect::Mysql] {
            let projected = fold_ops_onto(
                &base,
                std::slice::from_ref(&synchronize),
                dialect,
                SCHEMA,
                &crate::test_fixtures::confined_charter(),
            )
            .expect("synchronization target validates");
            assert_eq!(
                projected, base,
                "generator state must not become structural drift state"
            );
        }

        let missing = Op::SynchronizeIdentity {
            table: "orders".to_string(),
            column: "missing_id".to_string(),
            writes_quiesced: "orders_import_window".to_string(),
            schema: None,
        };
        assert!(matches!(
            fold_ops_onto(
                &base,
                &[missing],
                SqlDialect::Postgres,
                SCHEMA,
                &crate::test_fixtures::confined_charter(),
            ),
            Err(FoldError::MissingColumn { .. })
        ));
    }

    #[test]
    fn fold_retains_fixed_decimal_storage_for_mysql_and_sqlite_replay() {
        let snap = fold_ops(
            &[create(
                "ledger",
                vec![col(
                    "amount",
                    ColType::Decimal {
                        precision: 30,
                        scale: 10,
                    },
                    false,
                )],
            )],
            SqlDialect::Mysql,
            SCHEMA,
            &crate::test_fixtures::confined_charter(),
        )
        .expect("MySQL decimal create should fold");
        let amount = snap.tables["ledger"]
            .columns
            .iter()
            .find(|column| column.name == "amount")
            .expect("amount column");
        assert_eq!(amount.data_type, "numeric");
        assert_eq!(amount.ddl_type_override.as_deref(), Some("DECIMAL(30, 10)"));

        let sqlite = fold_ops(
            &[create(
                "ledger",
                vec![col(
                    "amount",
                    ColType::Decimal {
                        precision: 30,
                        scale: 10,
                    },
                    false,
                )],
            )],
            SqlDialect::Sqlite,
            SCHEMA,
            &crate::test_fixtures::confined_charter(),
        )
        .expect("SQLite decimal create should fold");
        let amount = sqlite.tables["ledger"]
            .columns
            .iter()
            .find(|column| column.name == "amount")
            .expect("amount column");
        assert_eq!(amount.data_type, "text");
        assert_eq!(amount.ddl_type_override.as_deref(), Some("TEXT"));
    }

    #[test]
    fn duplicate_create_table_errors() {
        let err = fold(&[
            create("users", vec![col("a", ColType::Text, true)]),
            create("users", vec![col("b", ColType::Text, true)]),
        ])
        .unwrap_err();
        assert_eq!(err, FoldError::DuplicateTable("users".to_string()));
    }

    #[test]
    fn drop_table_removes_it() {
        let snap = fold(&[
            create("users", vec![col("a", ColType::Text, true)]),
            Op::DropTable {
                table: "users".to_string(),
                cascade: None,
                schema: None,
                existence_guard: None,
            },
        ])
        .unwrap();
        assert!(
            snap.tables.is_empty(),
            "dropped table removed from the fold"
        );
    }

    #[test]
    fn drop_missing_table_errors() {
        let err = fold(&[Op::DropTable {
            table: "ghost".to_string(),
            cascade: None,
            schema: None,
            existence_guard: None,
        }])
        .unwrap_err();
        assert_eq!(err, FoldError::MissingTable("ghost".to_string()));
    }

    fn add_col(table: &str, column: &str, ty: ColType, nullable: bool) -> Op {
        Op::AddColumn {
            table: table.to_string(),
            column: column.to_string(),
            ty,
            nullable: Some(nullable),
            default: None,
            value_format: None,
            case_sensitive: None,
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
            schema: None,
            existence_guard: None,
        }
    }

    #[test]
    fn add_column_appears_sorted() {
        let snap = fold(&[
            create("users", vec![col("name", ColType::Text, true)]),
            add_col("users", "score", ColType::Int, false),
        ])
        .unwrap();
        let t = &snap.tables["users"];
        assert!(
            t.columns.iter().any(|c| c.name == "score"),
            "added column present"
        );
        // Columns are name-sorted (matches snapshot_schema + build_table_snapshot).
        let names: Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "columns name-sorted after add");
    }

    #[test]
    fn add_column_to_missing_table_errors() {
        let err = fold(&[add_col("ghost", "x", ColType::Text, true)]).unwrap_err();
        assert_eq!(err, FoldError::MissingTable("ghost".to_string()));
    }

    #[test]
    fn add_duplicate_column_errors() {
        let err = fold(&[
            create("users", vec![col("name", ColType::Text, true)]),
            add_col("users", "name", ColType::Text, true),
        ])
        .unwrap_err();
        assert_eq!(
            err,
            FoldError::DuplicateColumn {
                table: "users".to_string(),
                column: "name".to_string()
            }
        );
    }

    #[test]
    fn add_then_drop_column_is_empty_delta() {
        let with = fold(&[
            create("users", vec![col("name", ColType::Text, true)]),
            add_col("users", "tmp", ColType::Int, true),
        ])
        .unwrap();
        let without = fold(&[
            create("users", vec![col("name", ColType::Text, true)]),
            add_col("users", "tmp", ColType::Int, true),
            Op::DropColumn {
                table: "users".to_string(),
                column: "tmp".to_string(),
                schema: None,
                existence_guard: None,
            },
        ])
        .unwrap();
        let base = fold(&[create("users", vec![col("name", ColType::Text, true)])]).unwrap();
        assert_ne!(with, base, "the added column changes the snapshot");
        assert_eq!(without, base, "add-then-drop folds back to the base shape");
    }

    #[test]
    fn drop_absent_column_errors() {
        let err = fold(&[
            create("users", vec![col("name", ColType::Text, true)]),
            Op::DropColumn {
                table: "users".to_string(),
                column: "ghost".to_string(),
                schema: None,
                existence_guard: None,
            },
        ])
        .unwrap_err();
        assert_eq!(
            err,
            FoldError::MissingColumn {
                table: "users".to_string(),
                column: "ghost".to_string()
            }
        );
    }

    fn drop_column(table: &str, column: &str) -> Op {
        Op::DropColumn {
            table: table.to_string(),
            column: column.to_string(),
            schema: None,
            existence_guard: None,
        }
    }

    #[test]
    fn drop_column_cascades_dependent_index() {
        // PG auto-drops an index over a dropped column; the pure fold must too.
        // Pre-fix this RETAINED the stale `t_b_idx`, leaving fold != introspect.
        let with_idx = fold(&[
            create(
                "t",
                vec![
                    col("a", ColType::Text, false),
                    col("b", ColType::Text, true),
                ],
            ),
            create_index("t", Some("t_b_idx"), &["b"], false),
        ])
        .unwrap();
        assert!(
            with_idx.tables["t"]
                .indexes
                .iter()
                .any(|i| i.name == "t_b_idx"),
            "precondition: index present before the drop"
        );
        let dropped = fold(&[
            create(
                "t",
                vec![
                    col("a", ColType::Text, false),
                    col("b", ColType::Text, true),
                ],
            ),
            create_index("t", Some("t_b_idx"), &["b"], false),
            drop_column("t", "b"),
        ])
        .unwrap();
        let t = &dropped.tables["t"];
        assert!(!t.columns.iter().any(|c| c.name == "b"), "column gone");
        assert!(
            !t.indexes.iter().any(|i| i.name == "t_b_idx"),
            "the index over the dropped column cascades away (matches PG auto-drop)"
        );
        // Equals the schema that never had the index/column at all.
        let base = fold(&[create("t", vec![col("a", ColType::Text, false)])]).unwrap();
        assert_eq!(
            dropped, base,
            "drop-column-with-index folds back to the bare table"
        );
    }

    #[test]
    fn drop_column_cascades_dependent_unique_constraint_and_index() {
        // PG auto-drops a UNIQUE constraint (AND its implicit index) over a dropped
        // column. Pre-fix the fold retained BOTH, leaving fold != introspect.
        let dropped = fold(&[
            create(
                "t",
                vec![
                    col("a", ColType::Text, false),
                    col("b", ColType::Text, false),
                ],
            ),
            Op::AddConstraint {
                table: "t".to_string(),
                constraint: unique_constraint(Some("t_b_uq"), &["b"]),
                schema: None,
                existence_guard: None,
            },
            drop_column("t", "b"),
        ])
        .unwrap();
        let t = &dropped.tables["t"];
        assert!(
            !t.constraints.iter().any(|c| c.name == "t_b_uq"),
            "the UNIQUE constraint over the dropped column cascades away"
        );
        assert!(
            !t.indexes.iter().any(|i| i.name == "t_b_uq"),
            "the constraint's implicit unique index cascades away too"
        );
        let base = fold(&[create("t", vec![col("a", ColType::Text, false)])]).unwrap();
        assert_eq!(
            dropped, base,
            "drop-column-with-unique folds back to the bare table"
        );
    }

    #[test]
    fn drop_column_cascades_dependent_fk_constraint() {
        // PG auto-drops a FK constraint when its LOCAL (referencing) column is dropped.
        let fk = IrConstraint {
            name: Some("m_team_fk".to_string()),
            kind: IrConstraintKind::Fk {
                columns: vec!["team_id".to_string()],
                references_table: "teams".to_string(),
                references_columns: vec!["id".to_string()],
                on_delete: None,
                on_update: None,
                deferrable: None,
                initially_deferred: None,

                not_valid: None,
            },
        };
        let dropped = fold(&[
            create("teams", vec![col("label", ColType::Text, false)]),
            create("members", vec![col("team_id", ColType::Text, false)]),
            Op::AddConstraint {
                table: "members".to_string(),
                constraint: fk,
                schema: None,
                existence_guard: None,
            },
            drop_column("members", "team_id"),
        ])
        .unwrap();
        assert!(
            !dropped.tables["members"]
                .constraints
                .iter()
                .any(|c| c.name == "m_team_fk"),
            "the FK over the dropped local column cascades away"
        );
    }

    #[test]
    fn drop_column_keeps_unrelated_index_and_constraint() {
        // The cascade must NOT over-drop: an index/constraint on a DIFFERENT column,
        // and the system `<table>_pkey` (PRIMARY KEY (id)), survive the drop of `b`.
        let snap = fold(&[
            create(
                "t",
                vec![
                    col("a", ColType::Text, false),
                    col("b", ColType::Text, true),
                    col("c", ColType::Text, false),
                ],
            ),
            create_index("t", Some("t_c_idx"), &["c"], false),
            drop_column("t", "b"),
        ])
        .unwrap();
        let t = &snap.tables["t"];
        assert!(
            t.indexes.iter().any(|i| i.name == "t_c_idx"),
            "unrelated index kept"
        );
        assert!(
            t.constraints.iter().any(|c| c.name == "t_pkey"),
            "system PK (PRIMARY KEY (id)) not dropped by a non-id column drop"
        );
    }

    #[test]
    fn drop_column_cascades_multicolumn_index_partial_cover() {
        // A multi-column index that merely PARTIALLY covers the dropped column is
        // dropped whole by PG — the fold mirrors that.
        let snap = fold(&[
            create(
                "t",
                vec![
                    col("a", ColType::Text, false),
                    col("b", ColType::Text, true),
                ],
            ),
            create_index("t", Some("t_ab_idx"), &["a", "b"], false),
            drop_column("t", "b"),
        ])
        .unwrap();
        assert!(
            !snap.tables["t"]
                .indexes
                .iter()
                .any(|i| i.name == "t_ab_idx"),
            "a multi-column index partially covering the dropped column cascades away"
        );
    }

    fn rename(table: &str, from: &str, to: &str) -> Op {
        Op::RenameColumn {
            table: table.to_string(),
            from: from.to_string(),
            to: to.to_string(),
            ty: ColType::Text,
            schema: None,
            existence_guard: None,
        }
    }

    #[test]
    fn rename_collapses_to_final_name() {
        let snap = fold(&[
            create("users", vec![col("nickname", ColType::Text, true)]),
            rename("users", "nickname", "handle"),
        ])
        .unwrap();
        let t = &snap.tables["users"];
        assert!(
            t.columns.iter().any(|c| c.name == "handle"),
            "renamed-to column present"
        );
        assert!(
            !t.columns.iter().any(|c| c.name == "nickname"),
            "old name gone"
        );
    }

    #[test]
    fn rename_preserves_type_and_nullability() {
        // A renamed column keeps its EXISTING type (the fold trusts the live column,
        // not the op's `ty`). Author an int column, rename it (op carries `ty:text`),
        // and assert the folded column keeps the int data_type.
        let int_snap = fold(&[create("users", vec![col("n", ColType::Int, false)])]).unwrap();
        let int_type = int_snap.tables["users"]
            .columns
            .iter()
            .find(|c| c.name == "n")
            .unwrap()
            .data_type
            .clone();
        let renamed = fold(&[
            create("users", vec![col("n", ColType::Int, false)]),
            rename("users", "n", "m"),
        ])
        .unwrap();
        let m = renamed.tables["users"]
            .columns
            .iter()
            .find(|c| c.name == "m")
            .unwrap();
        assert_eq!(
            m.data_type, int_type,
            "rename keeps the existing column type"
        );
        assert!(!m.nullable, "rename keeps nullability");
    }

    #[test]
    fn rename_missing_from_errors() {
        let err = fold(&[
            create("users", vec![col("a", ColType::Text, true)]),
            rename("users", "ghost", "x"),
        ])
        .unwrap_err();
        assert_eq!(
            err,
            FoldError::MissingColumn {
                table: "users".to_string(),
                column: "ghost".to_string()
            }
        );
    }

    #[test]
    fn rename_to_existing_errors() {
        let err = fold(&[
            create(
                "users",
                vec![col("a", ColType::Text, true), col("b", ColType::Text, true)],
            ),
            rename("users", "a", "b"),
        ])
        .unwrap_err();
        assert_eq!(
            err,
            FoldError::RenameCollision {
                table: "users".to_string(),
                to: "b".to_string()
            }
        );
    }

    fn rename_table(table: &str, to: &str) -> Op {
        Op::RenameTable {
            table: table.to_string(),
            to: to.to_string(),
            schema: None,
            existence_guard: None,
        }
    }

    #[test]
    fn rename_table_moves_snapshot_wholesale() {
        // A table rename re-keys the snapshot from the old name to the new one,
        // PRESERVING every column + index. A later op referencing the NEW name
        // resolves; the old key is gone.
        let snap = fold(&[
            create(
                "accounts",
                vec![
                    col("email", ColType::Text, false),
                    col("balance", ColType::Int, true),
                ],
            ),
            create_index("accounts", Some("accounts_email_idx"), &["email"], true),
            rename_table("accounts", "members"),
        ])
        .unwrap();
        assert!(
            !snap.tables.contains_key("accounts"),
            "old table name is gone after rename"
        );
        let t = snap
            .tables
            .get("members")
            .expect("renamed table present under new name");
        assert!(
            t.columns.iter().any(|c| c.name == "email"),
            "columns preserved across table rename"
        );
        assert!(
            t.columns.iter().any(|c| c.name == "balance"),
            "all columns preserved"
        );
        assert!(
            t.indexes.iter().any(|i| i.name == "accounts_email_idx"),
            "indexes preserved across table rename"
        );
    }

    #[test]
    fn rename_table_then_reference_new_name_resolves() {
        // After the rename, an op on the NEW name (add a column) folds onto the
        // moved snapshot.
        let snap = fold(&[
            create("accounts", vec![col("email", ColType::Text, false)]),
            rename_table("accounts", "members"),
            Op::AddColumn {
                table: "members".to_string(),
                column: "nickname".to_string(),
                ty: ColType::Text,
                nullable: Some(true),
                default: None,
                value_format: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
                schema: None,
                existence_guard: None,
            },
        ])
        .unwrap();
        let t = &snap.tables["members"];
        assert!(
            t.columns.iter().any(|c| c.name == "nickname"),
            "op on the renamed-to name resolves"
        );
    }

    #[test]
    fn rename_table_then_reference_old_name_errors() {
        // After the rename, an op on the OLD name fails closed (the table no longer
        // exists under that key) — the same contract a column rename has.
        let err = fold(&[
            create("accounts", vec![col("email", ColType::Text, false)]),
            rename_table("accounts", "members"),
            drop_column("accounts", "email"),
        ])
        .unwrap_err();
        assert_eq!(err, FoldError::MissingTable("accounts".to_string()));
    }

    #[test]
    fn rename_table_missing_source_errors() {
        let err = fold(&[
            create("accounts", vec![col("email", ColType::Text, false)]),
            rename_table("ghost", "x"),
        ])
        .unwrap_err();
        assert_eq!(err, FoldError::MissingTable("ghost".to_string()));
    }

    #[test]
    fn rename_table_to_existing_errors() {
        // A rename cannot collide with a live table.
        let err = fold(&[
            create("accounts", vec![col("email", ColType::Text, false)]),
            create("members", vec![col("member_key", ColType::Uuid, false)]),
            rename_table("accounts", "members"),
        ])
        .unwrap_err();
        assert_eq!(err, FoldError::DuplicateTable("members".to_string()));
    }

    #[test]
    fn rename_table_rewrites_incoming_fk_definition() {
        // REGRESSION: a table rename must re-target every INCOMING FK
        // `definition` in OTHER tables to the new name — the offline mirror of live
        // PG re-rendering the FK by OID after `RENAME TO`. Pre-fix the rename re-keyed
        // only the renamed table's own entry, so `orders`'s FK kept the dead `accounts`
        // name and `fold_ops` phantom-drifted against live for every incoming FK.
        let fk = IrConstraint {
            name: Some("orders_account_fk".to_string()),
            kind: IrConstraintKind::Fk {
                columns: vec!["account_id".to_string()],
                references_table: "accounts".to_string(),
                references_columns: vec!["id".to_string()],
                on_delete: None,
                on_update: None,
                deferrable: None,
                initially_deferred: None,

                not_valid: None,
            },
        };
        let snap = fold(&[
            create("accounts", vec![col("email", ColType::Text, false)]),
            create("orders", vec![col("account_id", ColType::Text, true)]),
            Op::AddConstraint {
                table: "orders".to_string(),
                constraint: fk,
                schema: None,
                existence_guard: None,
            },
            rename_table("accounts", "members"),
        ])
        .unwrap();
        let def = &snap.tables["orders"]
            .constraints
            .iter()
            .find(|c| c.name == "orders_account_fk")
            .expect("the incoming FK survives the target rename")
            .definition;
        assert!(
            def.contains(&format!("REFERENCES {SCHEMA}.members(id)")),
            "the incoming FK definition re-targets the NEW name: {def}"
        );
        assert!(
            !def.contains("accounts"),
            "the dead OLD name no longer appears in the incoming FK: {def}"
        );

        // PARITY ORACLE (scoped to the FK body): the rewritten incoming FK
        // `definition` must be byte-identical to authoring the FK against `members`
        // in the first place — what live PG reports post-rename. (The whole-snapshot
        // is NOT compared: PG preserves the renamed table's INDEX NAMES across a
        // RENAME — `accounts_*_idx`, not `members_*_idx` — so the index buckets
        // legitimately differ from a fresh `members` create. See the round-trip test
        // `fold_equals_introspect_after_rename_table_pg`, which asserts the index name
        // survives the rename on live PG.)
        let direct = fold(&[
            create("members", vec![col("email", ColType::Text, false)]),
            create("orders", vec![col("account_id", ColType::Text, true)]),
            Op::AddConstraint {
                table: "orders".to_string(),
                constraint: IrConstraint {
                    name: Some("orders_account_fk".to_string()),
                    kind: IrConstraintKind::Fk {
                        columns: vec!["account_id".to_string()],
                        references_table: "members".to_string(),
                        references_columns: vec!["id".to_string()],
                        on_delete: None,
                        on_update: None,
                        deferrable: None,
                        initially_deferred: None,

                        not_valid: None,
                    },
                },
                schema: None,
                existence_guard: None,
            },
        ])
        .unwrap();
        let direct_def = &direct.tables["orders"]
            .constraints
            .iter()
            .find(|c| c.name == "orders_account_fk")
            .unwrap()
            .definition;
        assert_eq!(
            def, direct_def,
            "the rewritten FK body is byte-identical to authoring the FK against the new name"
        );
    }

    #[test]
    fn rename_table_rewrites_self_referencing_fk_definition() {
        // A SELF-FK on the renamed table is re-targeted too (live PG rewrites it by
        // OID). `nodes(parent_id → nodes)` renamed to `tree` must read `REFERENCES
        // tree`, not the dead `nodes`.
        let self_fk = IrConstraint {
            name: Some("nodes_parent_fk".to_string()),
            kind: IrConstraintKind::Fk {
                columns: vec!["parent_id".to_string()],
                references_table: "nodes".to_string(),
                references_columns: vec!["id".to_string()],
                on_delete: None,
                on_update: None,
                deferrable: None,
                initially_deferred: None,

                not_valid: None,
            },
        };
        let snap = fold(&[
            create("nodes", vec![col("parent_id", ColType::Text, true)]),
            Op::AddConstraint {
                table: "nodes".to_string(),
                constraint: self_fk,
                schema: None,
                existence_guard: None,
            },
            rename_table("nodes", "tree"),
        ])
        .unwrap();
        let def = &snap.tables["tree"]
            .constraints
            .iter()
            .find(|c| c.name == "nodes_parent_fk")
            .expect("the self-FK survives the rename")
            .definition;
        assert!(
            def.contains(&format!("REFERENCES {SCHEMA}.tree(id)")) && !def.contains("nodes"),
            "the self-FK re-targets the new name: {def}"
        );
    }

    #[test]
    fn rename_table_rewrites_incoming_ref_target_for_gen_types() {
        // REGRESSION (gen-types twin): `fold_to_field_defs` must re-target
        // the INCOMING `ref` column in OTHER tables to the renamed table's new name, or
        // gen-types emits a TS `ref` to a non-existent collection. Pre-fix the arm
        // re-keyed only the renamed table's own column map, leaving `orders.account_id`
        // pointing at the dead `accounts`.
        let account_ref = IrColumn {
            name: "account_id".into(),
            ty: ColType::Ref {
                references: "accounts".into(),
            },
            nullable: Some(true),
            default: None,
            unique: None,
            value_format: None,
            references: None,
            id_prefix: None,
            collation: None,
            case_sensitive: None,
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
        };
        let m = defs(&[
            create("accounts", vec![col("email", ColType::Text, false)]),
            create("orders", vec![account_ref]),
            rename_table("accounts", "members"),
        ]);
        let def = field_def(&m, "orders", "account_id");
        assert_eq!(def.get("type").and_then(|v| v.as_str()), Some("ref"));
        assert_eq!(
            def.get("refTarget").and_then(|v| v.as_str()),
            Some("members"),
            "the incoming ref column re-targets the renamed table's NEW name: {def}"
        );
    }

    #[test]
    fn alter_column_type_rederives_data_type() {
        let snap = fold(&[
            create("users", vec![col("n", ColType::Int, false)]),
            Op::SetColumnType {
                table: "users".to_string(),
                column: "n".to_string(),
                to_type: ColType::Text,
                using: None,
                schema: None,
                existence_guard: None,
            },
        ])
        .unwrap();
        let n = snap.tables["users"]
            .columns
            .iter()
            .find(|c| c.name == "n")
            .unwrap();
        let text_n = fold(&[create("users", vec![col("n", ColType::Text, false)])]).unwrap();
        let want = text_n.tables["users"]
            .columns
            .iter()
            .find(|c| c.name == "n")
            .unwrap();
        assert_eq!(
            n.data_type, want.data_type,
            "setColumnType re-derives the new data_type"
        );
        assert!(!n.nullable, "setColumnType keeps existing nullability");
    }

    #[test]
    fn alter_column_type_enters_and_leaves_uuid_default_drift_surface() {
        let to_uuid = fold(&[
            create("users", vec![col("external_id", ColType::Text, false)]),
            Op::SetColumnType {
                table: "users".to_string(),
                column: "external_id".to_string(),
                to_type: ColType::Uuid,
                using: None,
                schema: None,
                existence_guard: None,
            },
        ])
        .unwrap();
        let external_id = to_uuid.tables["users"]
            .columns
            .iter()
            .find(|column| column.name == "external_id")
            .unwrap();
        assert_eq!(
            external_id.id_default,
            Some(crate::model::snapshot::IdDefaultSnapshot::Absent)
        );

        let from_uuid = fold(&[
            create("users", vec![col("external_id", ColType::Uuid, false)]),
            Op::SetColumnType {
                table: "users".to_string(),
                column: "external_id".to_string(),
                to_type: ColType::Text,
                using: None,
                schema: None,
                existence_guard: None,
            },
        ])
        .unwrap();
        let external_id = from_uuid.tables["users"]
            .columns
            .iter()
            .find(|column| column.name == "external_id")
            .unwrap();
        assert_eq!(external_id.id_default, None);
    }

    #[test]
    fn alter_column_type_missing_column_errors() {
        let err = fold(&[
            create("users", vec![col("n", ColType::Int, true)]),
            Op::SetColumnType {
                table: "users".to_string(),
                column: "ghost".to_string(),
                to_type: ColType::Text,
                using: None,
                schema: None,
                existence_guard: None,
            },
        ])
        .unwrap_err();
        assert_eq!(
            err,
            FoldError::MissingColumn {
                table: "users".to_string(),
                column: "ghost".to_string()
            }
        );
    }

    #[test]
    fn drop_column_not_null_flips() {
        let snap = fold(&[
            create("users", vec![col("n", ColType::Int, false)]),
            Op::DropColumnNotNull {
                table: "users".to_string(),
                column: "n".to_string(),
                schema: None,
                existence_guard: None,
            },
        ])
        .unwrap();
        let n = snap.tables["users"]
            .columns
            .iter()
            .find(|c| c.name == "n")
            .unwrap();
        assert!(n.nullable, "dropColumnNotNull set NULL");
    }

    fn unique_constraint(name: Option<&str>, cols: &[&str]) -> IrConstraint {
        IrConstraint {
            name: name.map(ToString::to_string),
            kind: IrConstraintKind::Unique {
                columns: cols.iter().map(ToString::to_string).collect(),
            },
        }
    }

    /// A NOT VALID foreign key, its table, and the op that validates it.
    fn not_valid_fk(name: &str) -> IrConstraint {
        IrConstraint {
            name: Some(name.to_string()),
            kind: IrConstraintKind::Fk {
                columns: vec!["parent_id".to_string()],
                references_table: "parents".to_string(),
                references_columns: vec!["id".to_string()],
                on_delete: None,
                on_update: None,
                deferrable: None,
                initially_deferred: None,
                not_valid: Some(true),
            },
        }
    }

    fn validate_constraint(table: &str, name: &str) -> Op {
        Op::ValidateConstraint {
            table: table.to_string(),
            name: name.to_string(),
            schema: None,
            existence_guard: None,
        }
    }

    fn children_with_not_valid_fk(constraint: &str) -> Vec<Op> {
        // `id` is injected by the charter, so neither table declares one.
        vec![
            create("parents", vec![col("label", ColType::Text, false)]),
            create("children", vec![col("parent_id", ColType::Text, true)]),
            Op::AddConstraint {
                table: "children".to_string(),
                constraint: not_valid_fk(constraint),
                schema: None,
                existence_guard: None,
            },
        ]
    }

    fn folded_definition(snapshot: &SchemaSnapshot, table: &str, constraint: &str) -> String {
        snapshot.tables[table]
            .constraints
            .iter()
            .find(|c| c.name == constraint)
            .expect("the folded constraint exists")
            .definition
            .clone()
    }

    #[test]
    fn not_valid_foreign_key_carries_the_tail_until_it_is_validated() {
        let added = fold(&children_with_not_valid_fk("children_parent_fkey")).unwrap();
        assert!(
            folded_definition(&added, "children", "children_parent_fkey").ends_with(" NOT VALID"),
            "an unvalidated FK must fold with the tail `pg_get_constraintdef` renders"
        );

        let mut ops = children_with_not_valid_fk("children_parent_fkey");
        ops.push(validate_constraint("children", "children_parent_fkey"));
        let validated = fold(&ops).unwrap();
        let definition = folded_definition(&validated, "children", "children_parent_fkey");
        assert!(
            !definition.contains("NOT VALID"),
            "VALIDATE must remove the tail, leaving {definition:?}"
        );

        // Validating twice is the same as validating once: the strip is defined on
        // the suffix being there, not on a state flag that could be flipped twice.
        ops.push(validate_constraint("children", "children_parent_fkey"));
        assert_eq!(
            folded_definition(&fold(&ops).unwrap(), "children", "children_parent_fkey"),
            definition,
            "a second VALIDATE is idempotent"
        );
    }

    #[test]
    fn validate_constraint_is_a_no_op_on_a_table_or_constraint_the_fold_cannot_see() {
        // A VALIDATE naming a table an EARLIER artifact created is an ordinary
        // migration, and `fold_ops` is routinely handed one artifact's ops rather
        // than the whole history. Erroring here would fail a fold on a history the
        // server accepted, so both lookups miss quietly. `DropConstraint` is
        // deliberately stricter: it has a delta to apply and cannot apply it.
        let absent_table = fold(&[
            create("parents", vec![col("label", ColType::Text, false)]),
            validate_constraint("children", "children_parent_fkey"),
        ]);
        assert!(
            absent_table.is_ok(),
            "a VALIDATE on an unseen table must fold clean, got {absent_table:?}"
        );

        let mut ops = children_with_not_valid_fk("children_parent_fkey");
        ops.push(validate_constraint("children", "some_other_constraint"));
        let absent_constraint = fold(&ops).expect("a VALIDATE naming an unseen constraint folds");
        assert!(
            folded_definition(&absent_constraint, "children", "children_parent_fkey")
                .ends_with(" NOT VALID"),
            "validating a different constraint must not strip this one's tail"
        );
    }

    #[test]
    fn add_unique_constraint_named_and_derived() {
        let named = fold(&[
            create("users", vec![col("handle", ColType::Text, false)]),
            Op::AddConstraint {
                table: "users".to_string(),
                constraint: unique_constraint(Some("u_handle"), &["handle"]),
                schema: None,
                existence_guard: None,
            },
        ])
        .unwrap();
        let t = &named.tables["users"];
        let c = t.constraints.iter().find(|c| c.name == "u_handle").unwrap();
        assert_eq!(c.kind, "UNIQUE");
        // `pg_get_constraintdef`-matching: conditional quoting (bare for a safe
        // lowercase ident), NOT unconditional `UNIQUE ("handle")`.
        assert_eq!(c.definition, "UNIQUE (handle)");
        // A UNIQUE constraint also materializes the implicit unique index PG names
        // after the constraint — live introspection reports it, so the fold must too.
        let idx = t.indexes.iter().find(|i| i.name == "u_handle").unwrap();
        assert!(
            idx.unique,
            "implicit unique index for the UNIQUE constraint"
        );
        assert_eq!(idx.columns, vec!["handle".to_string()]);

        // An unnamed UNIQUE derives `<table>_<cols>_key`.
        let derived = fold(&[
            create("users", vec![col("handle", ColType::Text, false)]),
            Op::AddConstraint {
                table: "users".to_string(),
                constraint: unique_constraint(None, &["handle"]),
                schema: None,
                existence_guard: None,
            },
        ])
        .unwrap();
        assert!(
            derived.tables["users"]
                .constraints
                .iter()
                .any(|c| c.name == "users_handle_key"),
            "unnamed UNIQUE derives <table>_<cols>_key"
        );
    }

    #[test]
    fn add_then_drop_constraint_is_empty_delta() {
        let base = fold(&[create("users", vec![col("handle", ColType::Text, false)])]).unwrap();
        let round = fold(&[
            create("users", vec![col("handle", ColType::Text, false)]),
            Op::AddConstraint {
                table: "users".to_string(),
                constraint: unique_constraint(Some("u_handle"), &["handle"]),
                schema: None,
                existence_guard: None,
            },
            Op::DropConstraint {
                table: "users".to_string(),
                name: "u_handle".to_string(),
                schema: None,
                existence_guard: None,
            },
        ])
        .unwrap();
        assert_eq!(round, base, "add-then-drop constraint folds back to base");
    }

    #[test]
    fn add_constraint_to_missing_table_errors() {
        let err = fold(&[Op::AddConstraint {
            table: "ghost".to_string(),
            constraint: unique_constraint(Some("u"), &["x"]),
            schema: None,
            existence_guard: None,
        }])
        .unwrap_err();
        assert_eq!(err, FoldError::MissingTable("ghost".to_string()));
    }

    #[test]
    fn duplicate_constraint_errors() {
        let err = fold(&[
            create("users", vec![col("handle", ColType::Text, false)]),
            Op::AddConstraint {
                table: "users".to_string(),
                constraint: unique_constraint(Some("u_handle"), &["handle"]),
                schema: None,
                existence_guard: None,
            },
            Op::AddConstraint {
                table: "users".to_string(),
                constraint: unique_constraint(Some("u_handle"), &["handle"]),
                schema: None,
                existence_guard: None,
            },
        ])
        .unwrap_err();
        assert_eq!(
            err,
            FoldError::DuplicateConstraint {
                table: "users".to_string(),
                name: "u_handle".to_string()
            }
        );
    }

    #[test]
    fn drop_missing_constraint_errors() {
        let err = fold(&[
            create("users", vec![col("handle", ColType::Text, false)]),
            Op::DropConstraint {
                table: "users".to_string(),
                name: "ghost".to_string(),
                schema: None,
                existence_guard: None,
            },
        ])
        .unwrap_err();
        assert_eq!(
            err,
            FoldError::MissingConstraint {
                table: "users".to_string(),
                name: "ghost".to_string()
            }
        );
    }

    #[test]
    fn table_checks_fold_on_pg_and_validate_refuse_non_pg() {
        let create_chk = IrConstraint {
            name: Some("users_true".to_string()),
            kind: IrConstraintKind::Check {
                expr: Expr::Literal {
                    value: IrScalar::Bool(true),
                },

                not_valid: None,
            },
        };
        let add_chk = IrConstraint {
            name: Some("age_pos".to_string()),
            kind: IrConstraintKind::Check {
                expr: Expr::Literal {
                    value: IrScalar::Bool(true),
                },

                not_valid: None,
            },
        };
        let mut create_op = create("users", vec![col("age", ColType::Int, false)]);
        let Op::CreateTable { constraints, .. } = &mut create_op else {
            unreachable!("create helper returns createTable");
        };
        constraints.push(create_chk);

        let pg_ops = vec![
            create_op.clone(),
            Op::AddConstraint {
                table: "users".to_string(),
                constraint: add_chk.clone(),
                schema: None,
                existence_guard: None,
            },
        ];
        assert_validate_ops_ok(pg_ops.clone(), Dialect::Postgres);
        let folded = fold(&pg_ops).expect("PG table-level CHECK constraints fold");
        let users = folded.tables.get("users").expect("users table folded");
        for name in ["users_true", "age_pos"] {
            let constraint = users
                .constraints
                .iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("{name} CHECK should be folded"));
            assert_eq!(constraint.kind, "CHECK");
            assert_eq!(constraint.definition, "CHECK (TRUE)");
        }

        for dialect in [Dialect::Sqlite, Dialect::Mysql] {
            let err = validate_ops(
                vec![
                    create("users", vec![col("age", ColType::Int, false)]),
                    Op::AddConstraint {
                        table: "users".to_string(),
                        constraint: add_chk.clone(),
                        schema: None,
                        existence_guard: None,
                    },
                ],
                dialect,
            );
            assert_eq!(err.code, CODE_UNSUPPORTED);
            assert_eq!(err.kind, Some(UnsupportedKind::Expr));
            assert!(err.reason.contains("CHECK") || err.reason.contains("check"));
        }
    }

    #[test]
    fn add_fk_constraint_with_on_delete_renders_definition() {
        let fk = IrConstraint {
            name: Some("m_team_fk".to_string()),
            kind: IrConstraintKind::Fk {
                columns: vec!["team_id".to_string()],
                references_table: "teams".to_string(),
                references_columns: vec!["id".to_string()],
                on_delete: Some(RefAction::Cascade),
                on_update: None,
                deferrable: None,
                initially_deferred: None,

                not_valid: None,
            },
        };
        let snap = fold(&[
            create("teams", vec![col("label", ColType::Text, false)]),
            create("members", vec![col("team_id", ColType::Text, false)]),
            Op::AddConstraint {
                table: "members".to_string(),
                constraint: fk,
                schema: None,
                existence_guard: None,
            },
        ])
        .unwrap();
        let c = snap.tables["members"]
            .constraints
            .iter()
            .find(|c| c.name == "m_team_fk")
            .unwrap();
        assert_eq!(c.kind, "FOREIGN KEY");
        assert!(
            c.definition.contains("ON DELETE CASCADE"),
            "FK definition carries ON DELETE: {}",
            c.definition
        );
        assert!(
            c.definition.contains("teams"),
            "FK references the target table: {}",
            c.definition
        );
    }

    fn create_index(table: &str, name: Option<&str>, cols: &[&str], unique: bool) -> Op {
        Op::CreateIndex {
            table: table.to_string(),
            columns: cols
                .iter()
                .map(|col| IndexElement::Column {
                    name: (*col).to_string(),
                    order: None,
                    opclass: None,
                    collation: None,
                })
                .collect(),
            name: name.map(ToString::to_string),
            unique: Some(unique),
            using: None,
            r#where: None,
            include: Vec::new(),
            with: None,
            only: None,
            nulls_not_distinct: None,
            concurrently: None,
            schema: None,
            existence_guard: None,
        }
    }

    fn lifecycle_table(primary_key: Option<&[&str]>, identity_id: bool) -> Op {
        let mut id = col("id", ColType::BigInt, false);
        id.identity = identity_id.then_some(crate::model::ir::IdentityCol { always: false });
        Op::CreateTable {
            name: "orders".to_string(),
            columns: vec![
                id,
                col("tenant_id", ColType::BigInt, false),
                col("order_id", ColType::BigInt, false),
            ],
            primary_key: primary_key
                .map(|columns| columns.iter().map(|column| (*column).to_string()).collect()),
            constraints: Vec::new(),
            indexes: Vec::new(),
            partition_by: None,
            runtime_options: None,
            schema: None,
            existence_guard: None,
        }
    }

    #[test]
    fn alter_primary_key_fold_adds_replaces_and_drops_exact_ordered_keys() {
        let add = Op::AlterPrimaryKey {
            table: "orders".to_string(),
            action: AlterPrimaryKeyAction::Add {
                columns: vec!["tenant_id".to_string(), "order_id".to_string()],
            },
            schema: None,
        };
        let added = fold(&[
            lifecycle_table(None, false),
            create_index(
                "orders",
                Some("orders_tenant_order_key"),
                &["tenant_id", "order_id"],
                true,
            ),
            add,
        ])
        .expect("an exact staged unique candidate permits add");
        let table = &added.tables["orders"];
        assert!(table.constraints.iter().any(|constraint| {
            constraint.kind == "PRIMARY KEY"
                && constraint.name == "orders_tenant_order_key"
                && constraint.definition == "PRIMARY KEY (tenant_id, order_id)"
        }));
        assert!(
            table
                .indexes
                .iter()
                .any(|index| index.name == "orders_tenant_order_key"),
            "PostgreSQL USING INDEX retains the standalone index name as the unnamed add's constraint/index name"
        );
        assert!(!table
            .indexes
            .iter()
            .any(|index| index.name == "orders_pkey"));

        let replace_composite = Op::AlterPrimaryKey {
            table: "orders".to_string(),
            action: AlterPrimaryKeyAction::Replace {
                expected_columns: vec!["id".to_string()],
                columns: vec!["tenant_id".to_string(), "order_id".to_string()],
                drop_identity_from: Some(vec!["id".to_string()]),
            },
            schema: None,
        };
        let replace_single = Op::AlterPrimaryKey {
            table: "orders".to_string(),
            action: AlterPrimaryKeyAction::Replace {
                expected_columns: vec!["tenant_id".to_string(), "order_id".to_string()],
                columns: vec!["id".to_string()],
                drop_identity_from: None,
            },
            schema: None,
        };
        let dropped = fold(&[
            lifecycle_table(Some(&["id"]), true),
            create_index("orders", Some("orders_id_key"), &["id"], true),
            create_index(
                "orders",
                Some("orders_tenant_order_key"),
                &["tenant_id", "order_id"],
                true,
            ),
            replace_composite,
            replace_single,
            Op::AlterPrimaryKey {
                table: "orders".to_string(),
                action: AlterPrimaryKeyAction::Drop {
                    expected_columns: vec!["id".to_string()],
                    drop_identity_from: None,
                },
                schema: None,
            },
        ])
        .expect("exact single/composite transitions fold in order");
        let table = &dropped.tables["orders"];
        assert!(!table
            .constraints
            .iter()
            .any(|constraint| constraint.kind == "PRIMARY KEY"));
        assert_eq!(
            table
                .columns
                .iter()
                .find(|column| column.name == "id")
                .and_then(|column| column.identity),
            None,
            "dropIdentityFrom turns the old identity into an ordinary integer"
        );
        assert!(!table
            .indexes
            .iter()
            .any(|index| index.name == "orders_id_key"));
    }

    #[test]
    fn alter_primary_key_fold_preserves_constraint_owned_postgres_candidate() {
        let candidate_name = "orders_tenant_order_uq";
        let snapshot = fold(&[
            lifecycle_table(None, false),
            Op::AddConstraint {
                table: "orders".to_string(),
                constraint: IrConstraint {
                    name: Some(candidate_name.to_string()),
                    kind: IrConstraintKind::Unique {
                        columns: vec!["tenant_id".to_string(), "order_id".to_string()],
                    },
                },
                schema: None,
                existence_guard: None,
            },
            Op::AlterPrimaryKey {
                table: "orders".to_string(),
                action: AlterPrimaryKeyAction::Add {
                    columns: vec!["tenant_id".to_string(), "order_id".to_string()],
                },
                schema: None,
            },
        ])
        .expect("a UNIQUE constraint proves the candidate but cannot be adopted by USING INDEX");
        let table = &snapshot.tables["orders"];
        assert!(table
            .constraints
            .iter()
            .any(|constraint| constraint.name == candidate_name && constraint.kind == "UNIQUE"));
        assert!(table
            .indexes
            .iter()
            .any(|index| index.name == candidate_name));
        assert!(
            table
                .constraints
                .iter()
                .any(|constraint| constraint.name == "orders_pkey"
                    && constraint.kind == "PRIMARY KEY")
        );
        assert!(table
            .indexes
            .iter()
            .any(|index| index.name == "orders_pkey"));
    }

    #[test]
    fn alter_primary_key_fold_matches_dialect_identity_and_candidate_adoption() {
        let pg_composite = fold_ops(
            &[
                lifecycle_table(Some(&["id"]), true),
                create_index(
                    "orders",
                    Some("orders_tenant_id_key"),
                    &["tenant_id", "id"],
                    true,
                ),
                Op::AlterPrimaryKey {
                    table: "orders".to_string(),
                    action: AlterPrimaryKeyAction::Replace {
                        expected_columns: vec!["id".to_string()],
                        columns: vec!["tenant_id".to_string(), "id".to_string()],
                        drop_identity_from: None,
                    },
                    schema: None,
                },
            ],
            SqlDialect::Postgres,
            SCHEMA,
            &crate::test_fixtures::no_inject("app"),
        )
        .expect("PostgreSQL identity remains valid inside a composite key");
        assert!(pg_composite.tables["orders"]
            .columns
            .iter()
            .find(|column| column.name == "id")
            .and_then(|column| column.identity)
            .is_some());

        let sqlite_ops = [
            lifecycle_table(Some(&["id"]), false),
            create_index(
                "orders",
                Some("orders_tenant_order_key"),
                &["tenant_id", "order_id"],
                true,
            ),
            Op::AlterPrimaryKey {
                table: "orders".to_string(),
                action: AlterPrimaryKeyAction::Replace {
                    expected_columns: vec!["id".to_string()],
                    columns: vec!["tenant_id".to_string(), "order_id".to_string()],
                    drop_identity_from: Some(vec!["id".to_string()]),
                },
                schema: None,
            },
        ];
        let sqlite = fold_ops(
            &sqlite_ops,
            SqlDialect::Sqlite,
            SCHEMA,
            &crate::test_fixtures::no_inject("app"),
        )
        .expect("SQLite plain INTEGER PRIMARY KEY is a generated rowid contract");
        assert!(sqlite.tables["orders"]
            .constraints
            .iter()
            .any(|constraint| constraint.definition == "PRIMARY KEY (tenant_id, order_id)"));

        let mut missing_drop = sqlite_ops.to_vec();
        let Op::AlterPrimaryKey { action, .. } = missing_drop.last_mut().unwrap() else {
            unreachable!()
        };
        let AlterPrimaryKeyAction::Replace {
            drop_identity_from, ..
        } = action
        else {
            unreachable!()
        };
        *drop_identity_from = None;
        assert!(matches!(
            fold_ops(
                &missing_drop,
                SqlDialect::Sqlite,
                SCHEMA,
                &crate::test_fixtures::no_inject("app"),
            ),
            Err(FoldError::InvalidPrimaryKeyIdentityTransition { .. })
        ));

        let desc_candidate = Op::CreateIndex {
            table: "orders".to_string(),
            columns: vec![
                IndexElement::Column {
                    name: "tenant_id".to_string(),
                    order: Some(crate::model::ir::IndexSortOrder::Desc),
                    opclass: None,
                    collation: None,
                },
                IndexElement::Column {
                    name: "order_id".to_string(),
                    order: None,
                    opclass: None,
                    collation: None,
                },
            ],
            name: Some("orders_desc_candidate".to_string()),
            unique: Some(true),
            using: None,
            r#where: None,
            include: Vec::new(),
            with: None,
            only: None,
            nulls_not_distinct: None,
            concurrently: None,
            schema: None,
            existence_guard: None,
        };
        let pg_desc = fold(&[
            lifecycle_table(None, false),
            desc_candidate,
            Op::AlterPrimaryKey {
                table: "orders".to_string(),
                action: AlterPrimaryKeyAction::Add {
                    columns: vec!["tenant_id".to_string(), "order_id".to_string()],
                },
                schema: None,
            },
        ])
        .expect("DESC uniqueness proves the tuple but cannot be adopted as the PK index");
        assert!(pg_desc.tables["orders"]
            .indexes
            .iter()
            .any(|index| index.name == "orders_desc_candidate"));
        assert!(pg_desc.tables["orders"]
            .indexes
            .iter()
            .any(|index| index.name == "orders_pkey"));
    }

    #[test]
    fn alter_primary_key_fold_refuses_drift_missing_candidates_and_implicit_identity_drop() {
        let staged = vec![
            lifecycle_table(Some(&["id"]), true),
            create_index(
                "orders",
                Some("orders_tenant_order_key"),
                &["tenant_id", "order_id"],
                true,
            ),
        ];
        let mut drift = staged.clone();
        drift.push(Op::AlterPrimaryKey {
            table: "orders".to_string(),
            action: AlterPrimaryKeyAction::Replace {
                expected_columns: vec!["order_id".to_string(), "tenant_id".to_string()],
                columns: vec!["tenant_id".to_string(), "order_id".to_string()],
                drop_identity_from: Some(vec!["order_id".to_string()]),
            },
            schema: None,
        });
        assert!(matches!(
            fold(&drift),
            Err(FoldError::PrimaryKeyPrecondition { .. })
        ));

        let mut implicit_drop = staged;
        implicit_drop.push(Op::AlterPrimaryKey {
            table: "orders".to_string(),
            action: AlterPrimaryKeyAction::Replace {
                expected_columns: vec!["id".to_string()],
                columns: vec!["tenant_id".to_string(), "order_id".to_string()],
                drop_identity_from: None,
            },
            schema: None,
        });
        assert!(matches!(
            fold(&implicit_drop),
            Err(FoldError::InvalidPrimaryKeyIdentityTransition { .. })
        ));

        assert!(matches!(
            fold(&[
                lifecycle_table(None, false),
                Op::AlterPrimaryKey {
                    table: "orders".to_string(),
                    action: AlterPrimaryKeyAction::Add {
                        columns: vec!["tenant_id".to_string(), "order_id".to_string()],
                    },
                    schema: None,
                }
            ]),
            Err(FoldError::MissingPrimaryKeyCandidate { .. })
        ));
    }

    #[test]
    fn create_index_appears() {
        let snap = fold(&[
            create("users", vec![col("email", ColType::Text, false)]),
            create_index("users", Some("u_email_idx"), &["email"], true),
        ])
        .unwrap();
        let idx = snap.tables["users"]
            .indexes
            .iter()
            .find(|i| i.name == "u_email_idx")
            .unwrap();
        assert!(idx.unique, "unique index folded");
        assert_eq!(idx.columns, vec!["email".to_string()]);
    }

    #[test]
    fn add_then_drop_index_is_empty_delta() {
        let base = fold(&[create("users", vec![col("email", ColType::Text, false)])]).unwrap();
        let round = fold(&[
            create("users", vec![col("email", ColType::Text, false)]),
            create_index("users", Some("u_email_idx"), &["email"], true),
            Op::DropIndex {
                name: "u_email_idx".to_string(),
                table: Some("users".to_string()),
                unique: Some(true),
                concurrently: None,
                schema: None,
                existence_guard: None,
            },
        ])
        .unwrap();
        assert_eq!(round, base, "add-then-drop index folds back to base");
    }

    #[test]
    fn drop_index_without_table_hint_scans_all() {
        let snap = fold(&[
            create("users", vec![col("email", ColType::Text, false)]),
            create_index("users", Some("u_email_idx"), &["email"], false),
            Op::DropIndex {
                name: "u_email_idx".to_string(),
                table: None,
                unique: None,
                concurrently: None,
                schema: None,
                existence_guard: None,
            },
        ])
        .unwrap();
        assert!(
            !snap.tables["users"]
                .indexes
                .iter()
                .any(|i| i.name == "u_email_idx"),
            "bare-name drop scans all tables and removes the index"
        );
    }

    #[test]
    fn drop_missing_index_errors() {
        let err = fold(&[
            create("users", vec![col("email", ColType::Text, false)]),
            Op::DropIndex {
                name: "ghost_idx".to_string(),
                table: Some("users".to_string()),
                unique: None,
                concurrently: None,
                schema: None,
                existence_guard: None,
            },
        ])
        .unwrap_err();
        assert_eq!(err, FoldError::MissingIndex("ghost_idx".to_string()));
    }

    #[test]
    fn dml_ops_are_noops() {
        // A schema with one table, then a battery of DML ops — the folded schema is
        // byte-identical to the schema WITHOUT the DML (faithful no-op guard).
        let schema_only = fold(&[create("users", vec![col("name", ColType::Text, true)])]).unwrap();
        let with_dml = fold(&[
            create("users", vec![col("name", ColType::Text, true)]),
            Op::Insert {
                table: "users".to_string(),
                columns: vec!["name".to_string()],
                rows: vec![vec![IrScalar::Str("alice".to_string()).into()]],
                on_conflict: None,
                schema: None,
            },
            Op::Delete {
                table: "users".to_string(),
                r#where: Expr::Literal {
                    value: IrScalar::Bool(true),
                },
                limit: None,
                schema: None,
            },
        ])
        .unwrap();
        assert_eq!(
            with_dml, schema_only,
            "DML ops leave the folded schema unchanged"
        );
    }

    #[test]
    fn create_table_level_unique_fk_and_index_fold() {
        // Mirrors the PG oracle corpus's table-level specs (a named UNIQUE + a
        // single-`id` FK + an extra index) — proves they fold onto the snapshot.
        let teams = create("teams", vec![col("label", ColType::Text, false)]);
        let memberships = Op::CreateTable {
            name: "memberships".to_string(),
            columns: vec![
                col("team_id", ColType::Text, false),
                col("slot", ColType::Text, false),
            ],
            primary_key: None,
            constraints: vec![
                unique_constraint(Some("m_slot_uq"), &["slot"]),
                IrConstraint {
                    name: Some("m_team_fk".to_string()),
                    kind: IrConstraintKind::Fk {
                        columns: vec!["team_id".to_string()],
                        references_table: "teams".to_string(),
                        references_columns: vec!["id".to_string()],
                        on_delete: None,
                        on_update: None,
                        deferrable: None,
                        initially_deferred: None,

                        not_valid: None,
                    },
                },
            ],
            indexes: vec![IrIndex {
                name: Some("m_team_idx".to_string()),
                columns: vec![IndexElement::Column {
                    name: "team_id".to_string(),
                    order: None,
                    opclass: None,
                    collation: None,
                }],
                unique: None,
                using: None,
                r#where: None,
                include: Vec::new(),
                with: None,
                only: None,
                nulls_not_distinct: None,
            }],
            partition_by: None,
            runtime_options: Default::default(),
            schema: None,
            existence_guard: None,
        };
        let snap = fold(&[teams, memberships]).unwrap();
        let t = &snap.tables["memberships"];
        assert!(t
            .constraints
            .iter()
            .any(|c| c.name == "m_slot_uq" && c.kind == "UNIQUE"));
        assert!(t
            .constraints
            .iter()
            .any(|c| c.name == "m_team_fk" && c.kind == "FOREIGN KEY"));
        assert!(t.indexes.iter().any(|i| i.name == "m_team_idx"));
    }

    #[test]
    fn runtime_options_and_plain_indexes_fold_into_table_snapshot() {
        let ops = vec![
            Op::CreateTable {
                name: "posts".to_string(),
                columns: vec![
                    col("author_id", ColType::Text, false),
                    col("status", ColType::Text, false),
                ],
                primary_key: None,
                constraints: Vec::new(),
                indexes: Vec::new(),
                partition_by: None,
                runtime_options: Some(TableRuntimeOptions {
                    soft_delete: true,
                    versioning: true,
                    strictness: TableStrictness::Lenient,
                }),
                schema: None,
                existence_guard: None,
            },
            Op::CreateIndex {
                table: "posts".to_string(),
                columns: vec![
                    IndexElement::Column {
                        name: "author_id".to_string(),
                        order: None,
                        opclass: None,
                        collation: None,
                    },
                    IndexElement::Column {
                        name: "status".to_string(),
                        order: None,
                        opclass: None,
                        collation: None,
                    },
                ],
                name: Some("posts_author_status_idx".to_string()),
                unique: Some(false),
                using: None,
                r#where: None,
                include: Vec::new(),
                with: None,
                only: None,
                nulls_not_distinct: None,
                concurrently: None,
                schema: None,
                existence_guard: None,
            },
            Op::SetTableOptions {
                table: "posts".to_string(),
                options: TableRuntimeOptionsPatch {
                    soft_delete: None,
                    versioning: Some(false),
                    strictness: Some(TableStrictness::Off),
                },
                schema: None,
            },
        ];
        let snap = fold(&ops).unwrap();
        let posts = &snap.tables["posts"];
        assert!(posts.runtime_options.soft_delete);
        assert!(!posts.runtime_options.versioning);
        assert_eq!(posts.runtime_options.strictness, TableStrictness::Off);
        assert!(posts.indexes.iter().any(|idx| {
            idx.name == "posts_author_status_idx"
                && idx.columns == vec!["author_id".to_string(), "status".to_string()]
                && !idx.unique
        }));
    }

    /// PURITY: `fold_ops` is a plain synchronous `fn` — it takes NO DSN / `Client`,
    /// runs OUTSIDE any async runtime, and opens no connection. This test is a
    /// non-async `#[test]`: it executes a representative fold
    /// with no DB infrastructure in scope at all, which would be impossible if
    /// `fold_ops` performed I/O (it would need an async runtime + a connection). The
    /// signature itself is the structural proof; this exercises it to be sure.
    #[test]
    fn fold_ops_is_pure_offline_no_db() {
        let ops = vec![
            create("a", vec![col("x", ColType::Text, true)]),
            add_col("a", "y", ColType::Int, false),
            create_index("a", Some("a_y_idx"), &["y"], false),
        ];
        let snap = fold_ops(
            &ops,
            SqlDialect::Postgres,
            SCHEMA,
            &crate::test_fixtures::confined_charter(),
        )
        .expect("fold runs with no DB connection or async runtime");
        assert!(snap.tables.contains_key("a"));
    }

    // -----------------------------------------------------------------------
    // setColumnType to/from an encrypted column must NOT
    // silently lose / carry a stale encryption sentinel. The fold fails closed
    // (the apply path cannot re-stamp the zero-migrate:enc sentinel today).
    // -----------------------------------------------------------------------

    fn encrypted_text() -> ColType {
        ColType::Encrypted {
            of: Box::new(ColType::Text),
        }
    }

    fn alter_type(table: &str, column: &str, ty: ColType) -> Op {
        Op::SetColumnType {
            table: table.to_string(),
            column: column.to_string(),
            to_type: ty,
            using: None,
            schema: None,
            existence_guard: None,
        }
    }

    /// A FRESH `t.encrypted(text)` column folds WITH an encryption sentinel (the
    /// shared builder stamps the `zero-migrate:enc:` contract gen-types reads). This is the
    /// baseline the alter path must preserve — assert the sentinel is present so the
    /// "alter loses it" regression below is meaningful.
    #[test]
    fn fresh_encrypted_column_carries_sentinel() {
        let snap = fold(&[create("v", vec![col("secret", encrypted_text(), true)])]).unwrap();
        let c = snap.tables["v"]
            .columns
            .iter()
            .find(|c| c.name == "secret")
            .unwrap();
        assert!(
            c.encryption_sentinel.is_some() || c.comment_sentinel.is_some(),
            "a fresh encrypted column carries the zero-migrate:enc sentinel (the gen-types contract)"
        );
    }

    /// REGRESSION: plain→encrypted via `setColumnType` is FAIL-CLOSED.
    /// Pre-fix the fold transplanted ONLY `data_type` (bytea), keeping the OLD
    /// `encryption_sentinel=None` — so the folded encrypted column carried NO
    /// sentinel (a silently-wrong snapshot, since the oracle excludes the sentinel
    /// from Eq). The apply path likewise never emits the `COMMENT … zero-migrate:enc`, so live
    /// also lacks it. Until apply can re-stamp it, the fold refuses the change.
    #[test]
    fn alter_column_type_to_encrypted_is_unsupported() {
        let err = fold(&[
            create("v", vec![col("secret", ColType::Text, true)]),
            alter_type("v", "secret", encrypted_text()),
        ])
        .unwrap_err();
        assert!(
            matches!(err, FoldError::Unsupported(m) if m.contains("encrypted")),
            "plain→encrypted setColumnType must fail closed, got {err:?}"
        );
    }

    /// REGRESSION (symmetric): encrypted→plain via `setColumnType` is
    /// also FAIL-CLOSED. The SOURCE column carries the sentinel; transplanting only
    /// `data_type` would leave the now-stale `zero-migrate:enc` sentinel on a plaintext column.
    #[test]
    fn alter_column_type_from_encrypted_is_unsupported() {
        let err = fold(&[
            create("v", vec![col("secret", encrypted_text(), true)]),
            alter_type("v", "secret", ColType::Text),
        ])
        .unwrap_err();
        assert!(
            matches!(err, FoldError::Unsupported(m) if m.contains("encrypted")),
            "encrypted→plain setColumnType must fail closed, got {err:?}"
        );
    }

    /// A PLAIN→PLAIN `setColumnType` (neither side encrypted) still works — the
    /// fail-closed guard is scoped to the encryption-contract change only.
    #[test]
    fn alter_column_type_plain_to_plain_still_folds() {
        let snap = fold(&[
            create("v", vec![col("n", ColType::Int, false)]),
            alter_type("v", "n", ColType::BigInt),
        ])
        .unwrap();
        let n = snap.tables["v"]
            .columns
            .iter()
            .find(|c| c.name == "n")
            .unwrap();
        let want = fold(&[create("v", vec![col("n", ColType::BigInt, false)])]).unwrap();
        let want_n = want.tables["v"]
            .columns
            .iter()
            .find(|c| c.name == "n")
            .unwrap();
        assert_eq!(
            n.data_type, want_n.data_type,
            "plain→plain re-derives data_type"
        );
    }

    // -----------------------------------------------------------------------
    // The fold must mirror the lower's SQLite refusals so it
    // never emits types for a schema that can never deploy on SQLite (fail-OPEN).
    // -----------------------------------------------------------------------

    fn create_with(
        name: &str,
        columns: Vec<IrColumn>,
        constraints: Vec<IrConstraint>,
        indexes: Vec<IrIndex>,
    ) -> Op {
        Op::CreateTable {
            name: name.to_string(),
            columns,
            primary_key: None,
            constraints,
            indexes,

            partition_by: None,

            runtime_options: Default::default(),
            schema: None,
            existence_guard: None,
        }
    }

    /// REGRESSION: a createTable table-level FK validates and folds on SQLite.
    /// Before portable composite-FK support, this witness asserted the opposite.
    #[test]
    fn create_table_level_fk_supported_on_sqlite() {
        let ops = vec![
            create("teams", vec![col("label", ColType::Text, false)]),
            create_with(
                "memberships",
                vec![col("team_id", ColType::Text, false)],
                vec![IrConstraint {
                    name: Some("m_team_fk".to_string()),
                    kind: IrConstraintKind::Fk {
                        columns: vec!["team_id".to_string()],
                        references_table: "teams".to_string(),
                        references_columns: vec!["id".to_string()],
                        on_delete: None,
                        on_update: None,
                        deferrable: None,
                        initially_deferred: None,

                        not_valid: None,
                    },
                }],
                Vec::new(),
            ),
        ];
        assert_validate_ops_ok(ops.clone(), Dialect::Sqlite);
        let sqlite = fold_ops(
            &ops,
            SqlDialect::Sqlite,
            SCHEMA,
            &crate::test_fixtures::confined_charter(),
        )
        .expect("table-level FK folds on SQLite");
        let memberships = &sqlite.tables["memberships"];
        assert!(memberships.constraints.iter().any(|constraint| {
            constraint.name == "m_team_fk" && constraint.kind == "FOREIGN KEY"
        }));
        assert!(memberships
            .indexes
            .iter()
            .any(|index| index.columns == ["team_id"]));

        assert!(
            fold(&ops).is_ok(),
            "the same table-level FK folds on Postgres"
        );
    }

    /// REGRESSION: a createTable TABLE-LEVEL UNIQUE is refused at
    /// validate-time on SQLite.
    #[test]
    fn create_table_level_unique_unsupported_on_sqlite() {
        let op = create_with(
            "t",
            vec![col("handle", ColType::Text, false)],
            vec![unique_constraint(Some("t_handle_uq"), &["handle"])],
            Vec::new(),
        );
        let err = validate_ops(vec![op], Dialect::Sqlite);
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert_eq!(err.kind, Some(UnsupportedKind::Op));
        assert!(err.reason.contains("unique"));
    }

    /// REGRESSION: a createTable non-btree index `using` is refused at
    /// validate-time on SQLite.
    #[test]
    fn create_table_non_btree_index_using_unsupported_on_sqlite() {
        let op = create_with(
            "t",
            vec![col("doc", ColType::Json, false)],
            Vec::new(),
            vec![IrIndex {
                name: Some("t_doc_idx".to_string()),
                columns: vec![IndexElement::Column {
                    name: "doc".to_string(),
                    order: None,
                    opclass: None,
                    collation: None,
                }],
                unique: None,
                using: Some(crate::model::ir::IndexMethod::Gin),
                r#where: None,
                include: Vec::new(),
                with: None,
                only: None,
                nulls_not_distinct: None,
            }],
        );
        let err = validate_ops(vec![op], Dialect::Sqlite);
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert_eq!(err.kind, Some(UnsupportedKind::Op));
        assert!(err.reason.contains("non-btree"));
    }

    // -----------------------------------------------------------------------
    // The round-trip oracle's ColumnSnapshot Eq excludes
    // default / encryption_sentinel / comment_sentinel, so it structurally CANNOT
    // validate the fold's emission metadata. These NO-DB goldens assert the fold's
    // emitted default / sentinels match build_table_snapshot DIRECTLY (not via the
    // Eq-blind oracle) — the fields gen-types depends on.
    // -----------------------------------------------------------------------

    /// The `ColumnSnapshot` build_table_snapshot produces for ONE field — the ground
    /// truth the fold's emission metadata must match.
    fn builder_column(
        table: &str,
        column: &str,
        ty: ColType,
        nullable: bool,
        default: Option<IrDefault>,
    ) -> ColumnSnapshot {
        let field = ir_column_to_field(&IrColumn {
            name: column.to_string(),
            ty,
            nullable: Some(nullable),
            default,
            unique: None,
            value_format: None,
            references: None,
            id_prefix: None,
            collation: None,
            case_sensitive: None,
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
        });
        let desc = CollectionDescriptor {
            name: table.to_string(),
            owner_app: FOLD_OWNER_APP.to_string(),
            fields: vec![field],
            indexes: Vec::new(),
            runtime_options: Default::default(),
        };
        build_table_snapshot(
            SCHEMA,
            &desc,
            SqlDialect::Postgres,
            &crate::test_fixtures::confined_charter(),
        )
        .unwrap()
        .columns
        .into_iter()
        .find(|c| c.name == column)
        .unwrap()
    }

    /// GOLDEN: the fold's emitted default + sentinels for a createTable
    /// encrypted column + a defaulted column match build_table_snapshot's directly.
    /// The headline round-trip oracle CANNOT see these fields (excluded from Eq).
    #[test]
    fn fold_emission_metadata_matches_builder_golden() {
        let snap = fold(&[create(
            "g",
            vec![
                col("secret", encrypted_text(), true),
                // A `string` column with a literal default — the shared builder DOES
                // render a quoted `DEFAULT 'beta'` clause for the `string` token, so
                // the non-triviality assertion below is real.
                IrColumn {
                    name: "tier".to_string(),
                    ty: ColType::Text,
                    nullable: Some(false),
                    default: Some(IrDefault::Literal {
                        value: IrScalar::Str("beta".to_string()),
                    }),
                    unique: None,
                    value_format: None,
                    references: None,
                    id_prefix: None,
                    collation: None,
                    case_sensitive: None,
                    vector_metric: None,
                    mask: None,
                    generated: None,
                    identity: None,
                },
                // An `int` column with a literal default — the snapshot's
                // emission-only `default` IS what gen-types reads, so it MUST
                // render (regression: int defaults were silently dropped — the
                // shared `field_default_expr` had no `int` arm).
                IrColumn {
                    name: "rank".to_string(),
                    ty: ColType::Int,
                    nullable: Some(false),
                    default: Some(IrDefault::Literal {
                        value: IrScalar::Int(7),
                    }),
                    unique: None,
                    value_format: None,
                    references: None,
                    id_prefix: None,
                    collation: None,
                    case_sensitive: None,
                    vector_metric: None,
                    mask: None,
                    generated: None,
                    identity: None,
                },
                // A `json` column always carries the `'{}'::jsonb` default (even with
                // no explicit default) — a second independent emission-only golden.
                col("meta", ColType::Json, true),
            ],
        )])
        .unwrap();
        let t = &snap.tables["g"];

        let secret = t.columns.iter().find(|c| c.name == "secret").unwrap();
        let want_secret = builder_column("g", "secret", encrypted_text(), true, None);
        assert_eq!(
            secret.encryption_sentinel, want_secret.encryption_sentinel,
            "fold's encryption_sentinel matches the shared builder"
        );
        assert_eq!(
            secret.comment_sentinel, want_secret.comment_sentinel,
            "fold's comment_sentinel matches the shared builder"
        );
        // A fresh encrypted column DOES carry the sentinel (the contract is non-empty).
        assert!(
            want_secret.encryption_sentinel.is_some() || want_secret.comment_sentinel.is_some(),
            "the encrypted golden is non-trivial (a sentinel exists to compare)"
        );

        let tier = t.columns.iter().find(|c| c.name == "tier").unwrap();
        let want_tier = builder_column(
            "g",
            "tier",
            ColType::Text,
            false,
            Some(IrDefault::Literal {
                value: IrScalar::Str("beta".to_string()),
            }),
        );
        assert_eq!(
            tier.default, want_tier.default,
            "fold's emitted string default matches the shared builder"
        );
        assert!(
            want_tier.default.is_some(),
            "the string default golden is non-trivial"
        );

        let rank = t.columns.iter().find(|c| c.name == "rank").unwrap();
        let want_rank = builder_column(
            "g",
            "rank",
            ColType::Int,
            false,
            Some(IrDefault::Literal {
                value: IrScalar::Int(7),
            }),
        );
        assert_eq!(
            rank.default, want_rank.default,
            "fold's emitted int default matches the shared builder"
        );
        assert_eq!(
            want_rank.default.as_deref(),
            Some("7"),
            "an int column's default DOES render into the snapshot (regression: it was dropped)"
        );

        let meta = t.columns.iter().find(|c| c.name == "meta").unwrap();
        let want_meta = builder_column("g", "meta", ColType::Json, true, None);
        assert_eq!(
            meta.default, want_meta.default,
            "fold's emitted json default matches the shared builder"
        );
        assert!(
            want_meta.default.is_some(),
            "the json default golden is non-trivial ('{{}}'::jsonb)"
        );
    }

    /// GOLDEN (addColumn): the fold's emitted default + sentinels for an
    /// addColumn path match build_table_snapshot's directly too.
    #[test]
    fn fold_add_column_emission_metadata_matches_builder_golden() {
        let snap = fold(&[
            create("g", vec![col("x", ColType::Text, true)]),
            Op::AddColumn {
                table: "g".to_string(),
                column: "secret".to_string(),
                ty: encrypted_text(),
                nullable: Some(true),
                default: None,
                value_format: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
                schema: None,
                existence_guard: None,
            },
        ])
        .unwrap();
        let secret = snap.tables["g"]
            .columns
            .iter()
            .find(|c| c.name == "secret")
            .unwrap();
        let want = builder_column("g", "secret", encrypted_text(), true, None);
        assert_eq!(
            secret.encryption_sentinel, want.encryption_sentinel,
            "addColumn encryption_sentinel parity"
        );
        assert_eq!(
            secret.comment_sentinel, want.comment_sentinel,
            "addColumn comment_sentinel parity"
        );
        assert!(
            want.encryption_sentinel.is_some() || want.comment_sentinel.is_some(),
            "the addColumn encrypted golden is non-trivial"
        );
    }

    // -----------------------------------------------------------------------
    // The fold and the lower must agree on the UNIQUE
    // constraint `definition` body spelling (shared `constraintdef_cols`), so the
    // two copies of the createTable-spec folding cannot drift.
    // -----------------------------------------------------------------------

    /// REGRESSION: the fold and the lower spell the UNIQUE `definition`
    /// IDENTICALLY (both via the shared `constraintdef_cols`). Pre-fix the lower used
    /// `quote_cols` → `UNIQUE ("handle")` while the fold used the conditional-quote
    /// helper → `UNIQUE (handle)`; the catalog's `pg_get_constraintdef` spells it
    /// bare, so the fold's form is correct and the lower now matches it.
    #[test]
    fn fold_and_lower_agree_on_unique_definition_spelling() {
        // The fold's spelling for a single safe lowercase column.
        let snap = fold(&[create_with(
            "t",
            vec![col("handle", ColType::Text, false)],
            vec![unique_constraint(Some("t_handle_uq"), &["handle"])],
            Vec::new(),
        )])
        .unwrap();
        let folded = snap.tables["t"]
            .constraints
            .iter()
            .find(|c| c.name == "t_handle_uq")
            .unwrap();
        // The lower's snapshot half spells it via the SAME shared helper now.
        let cols = vec!["handle".to_string()];
        let lower_body = format!(
            "UNIQUE ({})",
            crate::render::declarative::constraintdef_cols(&cols)
        );
        assert_eq!(
            folded.definition, lower_body,
            "fold and lower must spell the UNIQUE definition identically"
        );
        assert_eq!(
            folded.definition, "UNIQUE (handle)",
            "bare spelling matches pg_get_constraintdef"
        );
    }

    // ===================================================================
    // Fold-and-RECOVER (`fold_to_field_defs` + the CHECK-lift recognizer).
    // The facet assertions (id_prefix, vector_metric, enum/min/max lift) all
    // depend on the carry + lift logic.
    // ===================================================================

    fn defs(ops: &[Op]) -> std::collections::BTreeMap<String, serde_json::Value> {
        fold_to_field_defs(
            ops,
            SqlDialect::Postgres,
            SCHEMA,
            &crate::test_fixtures::confined_charter(),
        )
        .unwrap()
    }

    /// The reconstructed wire-FieldDef object for `table.column`.
    fn field_def<'a>(
        m: &'a std::collections::BTreeMap<String, serde_json::Value>,
        table: &str,
        column: &str,
    ) -> &'a serde_json::Value {
        m.get(table)
            .and_then(|t| t.get(column))
            .unwrap_or_else(|| panic!("no reconstructed FieldDef for {table}.{column}"))
    }

    #[test]
    fn recover_id_prefix_facet() {
        // id_prefix is a DECLARED-ONLY facet the carry + reconstruction must
        // surface as `idPrefix` on the rebuilt FieldDef.
        let id = IrColumn {
            name: "id".into(),
            ty: ColType::Uuid,
            nullable: Some(false),
            default: None,
            unique: None,
            value_format: None,
            references: None,
            id_prefix: Some("post".into()),
            collation: None,
            case_sensitive: None,
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
        };
        let m = defs(&[create("posts", vec![id])]);
        let def = field_def(&m, "posts", "id");
        assert_eq!(
            def.get("idPrefix").and_then(|v| v.as_str()),
            Some("post"),
            "the typed-id prefix is recovered onto the FieldDef: {def}"
        );
    }

    #[test]
    fn recover_vector_metric_facet() {
        // vector_metric is the other DECLARED-ONLY facet; recovered as the
        // camelCase `vectorMetric` token + the dims.
        let embedding = IrColumn {
            name: "embedding".into(),
            ty: ColType::Vector { vector: 1536 },
            nullable: Some(true),
            default: None,
            unique: None,
            value_format: None,
            references: None,
            id_prefix: None,
            collation: None,
            case_sensitive: None,
            vector_metric: Some(crate::model::ir::VectorMetric::InnerProduct),
            mask: None,
            generated: None,
            identity: None,
        };
        let m = defs(&[create("docs", vec![embedding])]);
        let def = field_def(&m, "docs", "embedding");
        assert_eq!(
            def.get("vectorMetric").and_then(|v| v.as_str()),
            Some("innerProduct"),
            "the declared vector metric is recovered: {def}"
        );
        assert_eq!(
            def.get("vectorDims").and_then(|v| v.as_i64()),
            Some(1536),
            "the vector dims ride alongside the metric: {def}"
        );
    }

    /// A STANDALONE `.mask()` on a PLAINTEXT createTable column is CARRIED
    /// on `IrColumn.mask`, lowered to `FieldDescriptor.mask`, and RECOVERED onto the
    /// FieldDef.
    #[test]
    fn recover_standalone_mask_facet() {
        let ssn = IrColumn {
            name: "ssn".into(),
            ty: ColType::Text,
            nullable: Some(true),
            default: None,
            unique: None,
            value_format: None,
            references: None,
            id_prefix: None,
            collation: None,
            case_sensitive: None,
            vector_metric: None,
            mask: Some(crate::model::ir::IrMask {
                kind: crate::model::ir::IrMaskKind::Last4,
                classification: crate::model::ir::IrClassification::Spi,
            }),
            generated: None,
            identity: None,
        };
        let m = defs(&[create("people", vec![ssn])]);
        let def = field_def(&m, "people", "ssn");
        let mask = def
            .get("mask")
            .unwrap_or_else(|| panic!("mask must be recovered: {def}"));
        assert_eq!(mask.get("kind").and_then(|v| v.as_str()), Some("last4"));
        assert_eq!(
            mask.get("classification").and_then(|v| v.as_str()),
            Some("spi")
        );
    }

    /// Precedence — an EXPLICIT `.mask()` on an ENCRYPTED column OVERRIDES the
    /// fail-safe auto-mask `{ full, pii }` the `ColType::Encrypted` carrier implies.
    /// Without the override arm, an encrypted column would ALWAYS recover
    /// `{ full, pii }` and an explicit override would be impossible.
    #[test]
    fn explicit_mask_overrides_encrypted_auto_mask() {
        let secret = IrColumn {
            name: "secret".into(),
            ty: ColType::Encrypted {
                of: Box::new(ColType::Text),
            },
            nullable: Some(true),
            default: None,
            unique: None,
            value_format: None,
            references: None,
            id_prefix: None,
            collation: None,
            case_sensitive: None,
            vector_metric: None,
            mask: Some(crate::model::ir::IrMask {
                kind: crate::model::ir::IrMaskKind::Last4,
                classification: crate::model::ir::IrClassification::Pci,
            }),
            generated: None,
            identity: None,
        };
        let m = defs(&[create("vault", vec![secret])]);
        let def = field_def(&m, "vault", "secret");
        let mask = def
            .get("mask")
            .unwrap_or_else(|| panic!("mask must be recovered: {def}"));
        assert_eq!(
            mask.get("kind").and_then(|v| v.as_str()),
            Some("last4"),
            "the EXPLICIT mask wins over the encrypted auto-mask `full`: {def}"
        );
        assert_eq!(
            mask.get("classification").and_then(|v| v.as_str()),
            Some("pci")
        );
    }

    /// A `mask` facet carried on `Op::AddColumn` is recovered onto the added
    /// column's FieldDef (the addColumn fold arm threads the facet).
    #[test]
    fn recover_mask_on_added_column() {
        let ops = vec![
            create("people", vec![col("name", ColType::Text, false)]),
            Op::AddColumn {
                table: "people".into(),
                column: "card".into(),
                ty: ColType::Text,
                nullable: Some(true),
                default: None,
                value_format: None,
                case_sensitive: None,
                vector_metric: None,
                mask: Some(crate::model::ir::IrMask {
                    kind: crate::model::ir::IrMaskKind::First4,
                    classification: crate::model::ir::IrClassification::Pci,
                }),
                generated: None,
                identity: None,
                schema: None,
                existence_guard: None,
            },
        ];
        let m = defs(&ops);
        let def = field_def(&m, "people", "card");
        let mask = def
            .get("mask")
            .unwrap_or_else(|| panic!("added-column mask must be recovered: {def}"));
        assert_eq!(mask.get("kind").and_then(|v| v.as_str()), Some("first4"));
        assert_eq!(
            mask.get("classification").and_then(|v| v.as_str()),
            Some("pci")
        );
    }

    #[test]
    fn recover_ref_target_facet() {
        // The FK target → the `ref` brand, recovered from the Ref ColType.
        let owner = IrColumn {
            name: "owner".into(),
            ty: ColType::Ref {
                references: "orgs".into(),
            },
            nullable: Some(false),
            default: None,
            unique: None,
            value_format: None,
            references: None,
            id_prefix: None,
            collation: None,
            case_sensitive: None,
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
        };
        let m = defs(&[create("teams", vec![owner])]);
        let def = field_def(&m, "teams", "owner");
        assert_eq!(def.get("type").and_then(|v| v.as_str()), Some("ref"));
        assert_eq!(
            def.get("refTarget").and_then(|v| v.as_str()),
            Some("orgs"),
            "the FK target collection is recovered as the ref brand: {def}"
        );
    }

    #[test]
    fn recover_encrypted_default_mode_facet() {
        // An encrypted column is recovered structurally (default mode) — the
        // ONLY encrypted shape op.* can author (see the encrypted-mode finding test).
        let secret = col("secret", encrypted_text(), true);
        let m = defs(&[create("vaults", vec![secret])]);
        let def = field_def(&m, "vaults", "secret");
        assert!(
            def.get("encrypted").is_some(),
            "an encrypted column is recovered with the (default-mode) encrypted facet: {def}"
        );
    }

    #[test]
    fn recover_min_max_range_from_check() {
        // `age >= 0 AND age <= 120` lifts to min:0, max:120 on a numeric column.
        use crate::model::expr::{BinaryOp, Expr};
        let range = Expr::BinOp {
            op: BinaryOp::And,
            lhs: Box::new(Expr::BinOp {
                op: BinaryOp::Ge,
                lhs: Box::new(Expr::col("age")),
                rhs: Box::new(Expr::lit(IrScalar::Int(0))),
            }),
            rhs: Box::new(Expr::BinOp {
                op: BinaryOp::Le,
                lhs: Box::new(Expr::col("age")),
                rhs: Box::new(Expr::lit(IrScalar::Int(120))),
            }),
        };
        assert_eq!(
            recover_check_facet(&range),
            Some(RecoveredCheck::Range {
                column: "age".to_string(),
                min: Some(0.0),
                max: Some(120.0),
            }),
            "the min/max recognizer must stay ready for the CHECK renderer"
        );
    }

    #[test]
    fn recover_lone_min_from_check() {
        use crate::model::expr::{BinaryOp, Expr};
        let ge = Expr::BinOp {
            op: BinaryOp::Ge,
            lhs: Box::new(Expr::col("qty")),
            rhs: Box::new(Expr::lit(IrScalar::Int(1))),
        };
        assert_eq!(
            recover_check_facet(&ge),
            Some(RecoveredCheck::Range {
                column: "qty".to_string(),
                min: Some(1.0),
                max: None,
            }),
            "a lone >= lifts only the min"
        );
    }

    #[test]
    fn recover_enum_from_eq_or_chain_check() {
        // The op.* closed AST has no IN node; the canonical enum shape is the
        // left-folded `role = 'admin' OR role = 'user'` chain → ["admin","user"].
        use crate::model::expr::{BinaryOp, Expr};
        let eq = |v: &str| Expr::BinOp {
            op: BinaryOp::Eq,
            lhs: Box::new(Expr::col("role")),
            rhs: Box::new(Expr::lit(IrScalar::Str(v.into()))),
        };
        let chain = Expr::BinOp {
            op: BinaryOp::Or,
            lhs: Box::new(eq("admin")),
            rhs: Box::new(eq("user")),
        };
        assert_eq!(
            recover_check_facet(&chain),
            Some(RecoveredCheck::Enum {
                column: "role".to_string(),
                values: vec![
                    serde_json::Value::String("admin".to_string()),
                    serde_json::Value::String("user".to_string()),
                ],
            }),
            "the enum members are lifted in order"
        );
    }

    #[test]
    fn unrecognized_check_is_left_unprojected_not_a_panic() {
        // An arbitrary boolean CHECK (here `length(name) > 3`) is NOT one of
        // the recognized shapes, so it is left unprojected — the column types as its
        // base scalar, and the recovery does NOT panic / error.
        use crate::model::expr::{BinaryOp, Expr, ScalarFn};
        let weird = Expr::BinOp {
            op: BinaryOp::Gt,
            lhs: Box::new(Expr::FnCall {
                r#fn: ScalarFn::Length,
                args: vec![Expr::col("name")],
            }),
            rhs: Box::new(Expr::lit(IrScalar::Int(3))),
        };
        assert_eq!(
            recover_check_facet(&weird),
            None,
            "an unrecognized CHECK projects NO facet"
        );
    }

    #[test]
    fn recovery_respects_a_dropped_column() {
        // The reconstruction tracks the folded logical state: a column dropped after
        // creation must NOT appear in the rebuilt FieldDef map.
        let m = defs(&[
            create(
                "t",
                vec![
                    col("keep", ColType::Text, true),
                    col("gone", ColType::Int, true),
                ],
            ),
            Op::DropColumn {
                table: "t".into(),
                column: "gone".into(),
                schema: None,
                existence_guard: None,
            },
        ]);
        let t = m.get("t").expect("table reconstructed");
        assert!(t.get("keep").is_some(), "a surviving column is present");
        assert!(
            t.get("gone").is_none(),
            "a dropped column is absent from the reconstruction"
        );
    }

    // ── The encrypted-mode finding ───────────────────────────────────────────
    // op.* can author ONLY a DEFAULT-mode encrypted column: `ColType::Encrypted`
    // carries the inner type ONLY, and the recorder `t.encrypted({ of })` exposes
    // no mode/keyId/wraps surface. So a non-default-encrypted column is
    // UNREPRESENTABLE in the IR — fail-closed BY CONSTRUCTION, NOT a silently
    // wrong-mode sentinel.
    //
    // Recovery restores the KERNEL DEFAULTS the SDK's
    // `t.encrypted()` stamps (`mode:randomised, keyId:default, wraps:<inner>`) PLUS the
    // fail-safe auto-mask (`full/pii`), so the author→generate→fold chain is byte-
    // lossless over a default `t.encrypted()` (the round-trip). The fail-closed property
    // is UNCHANGED: that recovered triple is the ONLY shape op.* can produce — there is
    // no IR surface for a non-default mode/keyId.

    #[test]
    fn encrypted_via_op_star_is_default_mode_only_fail_closed_by_construction() {
        // The recorder/IR can build an encrypted column carrying ONLY the inner type.
        let enc = encrypted_text();
        // The descriptor the shared kernel reads back recovers the encrypted facet as
        // the SDK kernel default (`t.encrypted()` byte-image) + the fail-safe auto-mask
        // — there is NO mode/keyId/wraps field on `ColType::Encrypted` to make it carry
        // a NON-default mode, so this is the only representable encrypted shape.
        let field = ir_column_to_field(&IrColumn {
            name: "secret".into(),
            ty: enc,
            nullable: Some(true),
            default: None,
            unique: None,
            value_format: None,
            references: None,
            id_prefix: None,
            collation: None,
            case_sensitive: None,
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
        });
        assert_eq!(
            field.encrypted,
            Some(
                serde_json::json!({ "mode": "randomised", "keyId": "default", "wraps": "string" })
            ),
            "op.* encrypted recovers the SDK kernel-DEFAULT triple (the `t.encrypted()` \
             byte-image) — a non-default mode is unrepresentable in ColType::Encrypted, \
             so the path is fail-closed by construction, never a wrong-mode sentinel"
        );
        assert_eq!(
            field.mask,
            Some(serde_json::json!({ "kind": "full", "classification": "pii" })),
            "a default t.encrypted() carries the fail-safe auto-mask, recovered byte-exact"
        );
    }

    // ===================================================================
    // The createTable producer (`descriptors_to_create_ops`).
    // The FK-constraint + closed-AST CHECK emission it threads is what makes the
    // author→generate→fold chain lossless.
    // ===================================================================

    use crate::render::declarative::{CollectionDescriptor, FieldDescriptor};

    fn descriptor(name: &str, fields: Vec<FieldDescriptor>) -> CollectionDescriptor {
        CollectionDescriptor {
            name: name.into(),
            owner_app: "app_test".into(),
            fields,
            indexes: Vec::new(),
            runtime_options: Default::default(),
        }
    }

    fn custom_nonlegacy_inject_policy() -> EffectivePolicy {
        effective_policy_from_charter_toml(
            r#"policy_version = 1

[[inject]]
scope = "all"
mandatory = true
primary_key = ["tenant_key"]
author_primary_key = "forbid"
columns = [
  { name = "tenant_key",    type = "text",    nullable = false },
  { name = "id",            type = "text",    nullable = true  },
  { name = "audit_revision", type = "integer", nullable = false, default = "41" },
]
indexes = [
  { name = "audit_revision_lookup", columns = ["audit_revision"] },
]
"#,
        )
        .expect("custom non-legacy inject charter composes")
    }

    #[test]
    fn custom_policy_snapshot_converges_across_declarative_and_resolved_fold() {
        let effective = custom_nonlegacy_inject_policy();
        let inject = ResolvedInject::for_table(&effective, SCHEMA, "entries")
            .expect("custom inject resolves");
        assert_eq!(
            inject
                .columns()
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            vec!["tenant_key", "id", "audit_revision"]
        );
        assert_eq!(inject.primary_key(), Some(&["tenant_key".to_string()][..]));
        assert_eq!(inject.indexes().len(), 1);
        assert!(
            !inject.owns_id_primary_key(),
            "an ordinary injected `id` is not the policy-owned primary key"
        );

        let authored = descriptor(
            "entries",
            vec![FieldDescriptor {
                name: "title".into(),
                ty: "string".into(),
                required: true,
                ..Default::default()
            }],
        );
        let declarative = build_table_snapshot(SCHEMA, &authored, SqlDialect::Postgres, &effective)
            .expect("declarative snapshot builds from the custom policy");

        let resolved_ops =
            descriptors_to_create_ops(std::slice::from_ref(&authored), SCHEMA, &effective)
                .expect("migration producer resolves the same custom policy");
        let Op::CreateTable {
            columns,
            primary_key,
            indexes,
            ..
        } = &resolved_ops[0]
        else {
            panic!("descriptor produces a createTable op")
        };
        assert_eq!(
            columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            vec!["tenant_key", "id", "audit_revision", "title"]
        );
        assert_eq!(
            primary_key.as_deref(),
            Some(&["tenant_key".to_string()][..])
        );
        assert_eq!(indexes, inject.indexes());

        let folded = fold_ops(&resolved_ops, SqlDialect::Postgres, SCHEMA, &effective)
            .expect("resolved migration folds under the same custom policy");
        let migration = &folded.tables["entries"];
        assert_eq!(
            format!("{migration:#?}"),
            format!("{declarative:#?}"),
            "declarative and resolved-migration snapshots must match in every debug-visible byte"
        );

        let revision = declarative
            .columns
            .iter()
            .find(|column| column.name == "audit_revision")
            .expect("custom defaulted column is injected");
        assert_eq!(revision.default.as_deref(), Some("41"));
        assert!(declarative.indexes.iter().any(|index| {
            index.name == "entries_audit_revision_idx"
                && index.columns == ["audit_revision".to_string()]
        }));
        assert!(declarative.constraints.iter().any(|constraint| {
            constraint.name == "entries_pkey" && constraint.definition == "PRIMARY KEY (tenant_key)"
        }));
    }

    #[test]
    fn no_inject_snapshot_preserves_author_order_and_converges_with_resolved_fold() {
        let effective = crate::test_fixtures::no_inject("app");
        let authored = descriptor(
            "events",
            vec![
                FieldDescriptor {
                    name: "zeta".into(),
                    ty: "string".into(),
                    ..Default::default()
                },
                FieldDescriptor {
                    name: "updated_at".into(),
                    ty: "int".into(),
                    required: true,
                    ..Default::default()
                },
                FieldDescriptor {
                    name: "alpha".into(),
                    ty: "boolean".into(),
                    ..Default::default()
                },
            ],
        );

        let declarative = build_table_snapshot(SCHEMA, &authored, SqlDialect::Postgres, &effective)
            .expect("no-inject declarative snapshot builds");
        assert_eq!(
            declarative
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            vec!["zeta", "updated_at", "alpha"],
            "empty injection must preserve the author's column order"
        );
        let updated_at = declarative
            .columns
            .iter()
            .find(|column| column.name == "updated_at")
            .expect("author updated_at survives");
        assert_eq!(updated_at.data_type, "integer");
        assert!(!updated_at.nullable);

        let resolved_ops =
            descriptors_to_create_ops(std::slice::from_ref(&authored), SCHEMA, &effective)
                .expect("no-inject migration producer preserves author shape");
        let folded = fold_ops(&resolved_ops, SqlDialect::Postgres, SCHEMA, &effective)
            .expect("no-inject resolved migration folds");
        assert_eq!(
            format!("{:#?}", folded.tables["events"]),
            format!("{declarative:#?}"),
            "no-inject declarative and resolved-migration snapshots must be byte-identical"
        );
    }

    #[test]
    fn ordinary_injected_id_does_not_enable_legacy_id_or_identity_folding() {
        let effective = custom_nonlegacy_inject_policy();
        let inject = ResolvedInject::for_table(&effective, SCHEMA, "entries")
            .expect("custom inject resolves");
        assert!(inject.contains_column("id"));
        assert!(!inject.owns_id_primary_key());

        let prefixed_id = descriptor(
            "entries",
            vec![FieldDescriptor {
                name: "id".into(),
                ty: "id".into(),
                id_prefix: Some("entry".into()),
                ..Default::default()
            }],
        );
        let identity_id = descriptor(
            "entries",
            vec![FieldDescriptor {
                name: "id".into(),
                ty: "bigInt".into(),
                identity: Some(crate::model::ir::IdentityCol { always: false }),
                ..Default::default()
            }],
        );

        for (kind, authored) in [("legacy prefix", prefixed_id), ("identity", identity_id)] {
            let declarative_error =
                build_table_snapshot(SCHEMA, &authored, SqlDialect::Postgres, &effective)
                    .expect_err("an ordinary injected `id` must reject author collision");
            assert!(
                declarative_error
                    .to_string()
                    .contains("collides with an injected policy column"),
                "{kind} declaration unexpectedly used the declarative ID fold: {declarative_error}"
            );

            let producer_error = descriptors_to_create_ops(&[authored], SCHEMA, &effective)
                .expect_err("migration resolution must reject the same collision");
            assert!(
                matches!(
                    producer_error,
                    ProduceError::TableShape { ref message, .. }
                        if message.contains("column \"id\"")
                            && message.contains("collides with an injected")
                ),
                "{kind} declaration unexpectedly used the migration ID fold: {producer_error}"
            );
        }
    }

    /// A `ref` column declares its foreign key on ONE carrier. The policy the
    /// `ColType::Ref` brand cannot express rides on the column's
    /// `ColumnReference`; no table-level `Fk` twin is emitted, because both
    /// carriers derive the SAME `<table>_<column>_fkey` name and the shared
    /// snapshot builder would then declare that constraint twice.
    #[test]
    fn producer_emits_ref_policy_on_the_column_carrier_without_a_table_level_twin() {
        let d = descriptor(
            "teams",
            vec![FieldDescriptor {
                name: "owner".into(),
                ty: "ref".into(),
                references: Some("orgs".into()),
                on_delete: Some("cascade".into()),
                on_update: Some("restrict".into()),
                ..Default::default()
            }],
        );
        let ops = descriptors_to_create_ops(&[d], "app", &crate::test_fixtures::confined_charter())
            .unwrap();
        let Op::CreateTable {
            columns,
            constraints,
            ..
        } = &ops[0]
        else {
            panic!("expected a createTable")
        };
        assert!(
            !constraints
                .iter()
                .any(|c| matches!(c.kind, IrConstraintKind::Fk { .. })),
            "the ref column's foreign key is carried by the column, never duplicated \
             as a table-level constraint: {constraints:?}"
        );
        let owner = columns
            .iter()
            .find(|column| column.name == "owner")
            .expect("the ref column is produced");
        assert_eq!(
            owner.ty,
            ColType::Ref {
                references: "orgs".into()
            },
            "the FK target stays on the ref brand"
        );
        let reference = owner
            .references
            .as_ref()
            .expect("the declared reference policy is carried on the column");
        assert_eq!(reference.table, "orgs");
        assert_eq!(reference.column, "id");
        assert_eq!(reference.on_delete, Some(RefAction::Cascade));
        assert_eq!(reference.on_update, Some(RefAction::Restrict));
    }

    /// A `ref` field that declares no reference facets keeps the brand-only column
    /// image the recorder emits, so the manual and generated artifact sources stay
    /// byte-identical for the same logical schema.
    #[test]
    fn producer_leaves_a_plain_ref_column_on_the_brand_alone() {
        let d = descriptor(
            "teams",
            vec![FieldDescriptor {
                name: "owner".into(),
                ty: "ref".into(),
                references: Some("orgs".into()),
                ..Default::default()
            }],
        );
        let ops = descriptors_to_create_ops(&[d], "app", &crate::test_fixtures::confined_charter())
            .unwrap();
        let Op::CreateTable {
            columns,
            constraints,
            ..
        } = &ops[0]
        else {
            panic!("expected a createTable")
        };
        assert!(
            !constraints
                .iter()
                .any(|c| matches!(c.kind, IrConstraintKind::Fk { .. })),
            "a plain ref emits no table-level foreign key: {constraints:?}"
        );
        let owner = columns
            .iter()
            .find(|column| column.name == "owner")
            .expect("the ref column is produced");
        assert_eq!(
            owner.ty,
            ColType::Ref {
                references: "orgs".into()
            }
        );
        assert!(
            owner.references.is_none(),
            "a plain ref carries no second reference carrier: {:?}",
            owner.references
        );
    }

    #[test]
    fn producer_emits_recoverable_check_shapes_for_enum_and_range() {
        let d = descriptor(
            "users",
            vec![
                FieldDescriptor {
                    name: "age".into(),
                    ty: "number".into(),
                    min: Some(0.0),
                    max: Some(120.0),
                    ..Default::default()
                },
                FieldDescriptor {
                    name: "role".into(),
                    ty: "string".into(),
                    enum_values: Some(vec!["a".into(), "b".into()]),
                    ..Default::default()
                },
            ],
        );
        let ops = descriptors_to_create_ops(&[d], "app", &crate::test_fixtures::confined_charter())
            .unwrap();
        let Op::CreateTable { constraints, .. } = &ops[0] else {
            panic!("createTable")
        };
        // Each emitted CHECK must round-trip through `recover_check_facet` to the
        // facet that authored it (the round-trip bound, asserted at the unit level).
        let mut recovered_range = false;
        let mut recovered_enum = false;
        for c in constraints {
            if let IrConstraintKind::Check { expr, .. } = &c.kind {
                match recover_check_facet(expr) {
                    Some(RecoveredCheck::Range { column, min, max }) if column == "age" => {
                        assert_eq!(min, Some(0.0));
                        assert_eq!(max, Some(120.0));
                        recovered_range = true;
                    }
                    Some(RecoveredCheck::Enum { column, values }) if column == "role" => {
                        assert_eq!(values, vec![serde_json::json!("a"), serde_json::json!("b")]);
                        recovered_enum = true;
                    }
                    other => panic!("unexpected CHECK recovery: {other:?}"),
                }
            }
        }
        assert!(
            recovered_range && recovered_enum,
            "both CHECK shapes round-trip via recover_check_facet"
        );
    }

    #[test]
    fn producer_preserves_column_order_through_fold() {
        // The reconstructed FieldDef map must preserve the descriptor's declared
        // column order (the round-trip compares serialized maps).
        let effective = crate::test_fixtures::confined_charter();
        let inject =
            ResolvedInject::for_table(&effective, "app", "t").expect("confined injection resolves");
        let d = descriptor(
            "t",
            vec![
                FieldDescriptor {
                    name: "zeta".into(),
                    ty: "string".into(),
                    ..Default::default()
                },
                FieldDescriptor {
                    name: "alpha".into(),
                    ty: "string".into(),
                    ..Default::default()
                },
                FieldDescriptor {
                    name: "mid".into(),
                    ty: "string".into(),
                    ..Default::default()
                },
            ],
        );
        let ops = descriptors_to_create_ops(&[d], "app", &effective).unwrap();
        let Op::CreateTable {
            columns,
            primary_key,
            indexes,
            ..
        } = &ops[0]
        else {
            panic!("createTable")
        };
        assert_eq!(
            columns
                .iter()
                .take(inject.columns().len())
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            inject
                .columns()
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            "producer emits the resolved confined system-field prefix"
        );
        assert_eq!(
            primary_key.as_deref(),
            inject.primary_key(),
            "producer carries the resolved top-level primary key"
        );
        assert_eq!(
            indexes,
            inject.indexes(),
            "producer carries resolved indexes"
        );
        let defs = fold_to_field_defs(&ops, SqlDialect::Postgres, SCHEMA, &effective).unwrap();
        let keys: Vec<&str> = defs["t"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let mut expected_keys = inject
            .columns()
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>();
        expected_keys.extend(["zeta", "alpha", "mid"]);
        assert_eq!(
            keys, expected_keys,
            "resolved system prefix is carried and declared order is preserved, not sorted"
        );
    }

    #[test]
    fn recovery_recognizes_injected_prefix_from_active_policy() {
        let effective = effective_policy_from_charter_toml(
            r#"policy_version = 1

[[inject]]
scope = "all"
mandatory = true
primary_key = ["id"]
author_primary_key = "forbid"
columns = [
  { name = "id",           type = "text",    nullable = false },
  { name = "audit_marker", type = "integer", nullable = false, default = "1" },
]
"#,
        )
        .expect("custom inject charter composes");
        let d = descriptor(
            "entries",
            vec![
                FieldDescriptor {
                    name: "id".into(),
                    ty: "id".into(),
                    id_prefix: Some("entry".into()),
                    ..Default::default()
                },
                FieldDescriptor {
                    name: "title".into(),
                    ty: "string".into(),
                    ..Default::default()
                },
            ],
        );
        let ops = descriptors_to_create_ops(&[d], "app", &effective).expect("descriptor resolves");
        let defs = fold_to_field_defs(&ops, SqlDialect::Postgres, SCHEMA, &effective)
            .expect("custom policy prefix recovers");
        let entries = defs["entries"].as_object().expect("entries FieldDef map");
        assert_eq!(
            entries.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["id", "audit_marker", "title"]
        );
        assert_eq!(entries["id"]["type"], serde_json::json!("id"));
        assert_eq!(entries["id"]["idPrefix"], serde_json::json!("entry"));
    }

    #[test]
    fn producer_carries_author_declared_indexes_alongside_system_indexes() {
        use crate::render::declarative::IndexDescriptor;
        // WALL 2 regression: a `CollectionDescriptor` carrying author-declared named
        // indexes (a plain one AND a unique one) must have BOTH survive into the
        // produced `createTable` op — alongside the 3 injected confined system indexes
        // — and NOT be dropped. Pre-fix `descriptors_to_create_ops` hardcoded
        // `indexes: Vec::new()`, so the author indexes vanished.
        let mut d = descriptor(
            "articles",
            vec![
                FieldDescriptor {
                    name: "slug".into(),
                    ty: "string".into(),
                    ..Default::default()
                },
                FieldDescriptor {
                    name: "author".into(),
                    ty: "string".into(),
                    ..Default::default()
                },
            ],
        );
        d.indexes = vec![
            IndexDescriptor {
                name: "articles_author_idx".into(),
                columns: vec!["author".into()],
                unique: false,
            },
            IndexDescriptor {
                name: "articles_slug_key".into(),
                columns: vec!["slug".into()],
                unique: true,
            },
        ];

        let ops = descriptors_to_create_ops(
            &[d.clone()],
            "app",
            &crate::test_fixtures::confined_charter(),
        )
        .unwrap();
        let Op::CreateTable { indexes, .. } = &ops[0] else {
            panic!("createTable")
        };
        // 2 author indexes + 3 confined system indexes.
        assert_eq!(
            indexes.len(),
            5,
            "author indexes are carried alongside the 3 resolved system indexes: {indexes:?}"
        );
        let author_idx = indexes
            .iter()
            .find(|i| i.name.as_deref() == Some("articles_author_idx"))
            .expect("the plain author index survives production");
        assert_eq!(
            author_idx.unique, None,
            "a non-unique index is `None`, not Some(false)"
        );
        assert_eq!(
            author_idx.columns,
            vec![IndexElement::Column {
                name: "author".into(),
                order: None,
                opclass: None,
                collation: None,
            }],
        );
        let unique_idx = indexes
            .iter()
            .find(|i| i.name.as_deref() == Some("articles_slug_key"))
            .expect("the UNIQUE author index survives production");
        assert_eq!(
            unique_idx.unique,
            Some(true),
            "the unique flag is preserved"
        );

        // End-to-end: the author indexes appear in the emitted v1 schema.runtime.json.
        let artifacts = crate::render_artifacts_from_descriptors(
            &[d],
            SqlDialect::Postgres,
            SCHEMA,
            &crate::test_fixtures::confined_charter(),
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&artifacts.runtime_json).unwrap();
        let idx_names: Vec<String> = v["collections"]["articles"]["indexes"]
            .as_array()
            .expect("indexes array present")
            .iter()
            .filter_map(|i| i["name"].as_str().map(str::to_string))
            .collect();
        assert!(
            idx_names.iter().any(|n| n == "articles_author_idx"),
            "the plain author index is emitted into schema.runtime.json, not dropped: {idx_names:?}"
        );
        assert!(
            idx_names.iter().any(|n| n == "articles_slug_key"),
            "the UNIQUE author index is emitted into schema.runtime.json: {idx_names:?}"
        );
    }

    #[test]
    fn producer_rejects_unmappable_type_token() {
        let d = descriptor(
            "t",
            vec![FieldDescriptor {
                name: "x".into(),
                ty: "no_such_type".into(),
                ..Default::default()
            }],
        );
        let err = descriptors_to_create_ops(&[d], "app", &crate::test_fixtures::confined_charter())
            .unwrap_err();
        assert!(
            matches!(err, ProduceError::UnknownType { .. }),
            "unmappable token fails closed"
        );
    }

    /// The rename rewrites ONLY the leading parenthesized group. The FOREIGN KEY
    /// tail - `REFERENCES ...`, the referential actions, DEFERRABLE, ` NOT VALID` -
    /// is spliced through byte-identically, and quoting is re-derived rather than
    /// swapped (a bare `order` would be a syntax error live reports as `"order"`).
    #[test]
    fn constraint_definition_rename_rewrites_only_the_leading_column_group() {
        assert_eq!(
            rename_constraint_definition_column("UNIQUE (a)", "a", "b").as_deref(),
            Some("UNIQUE (b)")
        );
        assert_eq!(
            rename_constraint_definition_column("PRIMARY KEY (a)", "a", "b").as_deref(),
            Some("PRIMARY KEY (b)")
        );
        assert_eq!(
            rename_constraint_definition_column("UNIQUE (note, a)", "a", "b").as_deref(),
            Some("UNIQUE (note, b)"),
            "a composite list keeps its ORDER and rewrites in place"
        );
        assert_eq!(
            rename_constraint_definition_column("UNIQUE (a)", "a", "order").as_deref(),
            Some("UNIQUE (\"order\")"),
            "quoting is CONDITIONAL and re-derived, never carried over from the source"
        );
        assert_eq!(
            rename_constraint_definition_column(
                "FOREIGN KEY (a) REFERENCES proj.parent(id) ON UPDATE RESTRICT \
                 ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED NOT VALID",
                "a",
                "b",
            )
            .as_deref(),
            Some(
                "FOREIGN KEY (b) REFERENCES proj.parent(id) ON UPDATE RESTRICT \
                 ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED NOT VALID"
            ),
            "the FK tail survives byte-for-byte, including the referenced column"
        );
        assert_eq!(
            rename_constraint_definition_column("UNIQUE (note)", "a", "b"),
            None,
            "a constraint the rename does not touch is left alone"
        );
    }

    /// The round-trip guard. A definition whose leading group the parser mishandles
    /// is left STALE rather than rewritten into something CORRUPT: an embedded
    /// escaped quote, a `,` inside a quoted name, and a `)` inside a quoted name all
    /// survive the parse as the wrong tokens, and all three fail the byte-for-byte
    /// re-render compare before any swap happens.
    #[test]
    fn constraint_definition_rename_guard_refuses_a_mishandled_group() {
        for definition in [
            r#"UNIQUE ("a""b")"#,
            r#"UNIQUE ("a,b")"#,
            r#"UNIQUE ("a)b")"#,
            r#"UNIQUE ("a""b", c)"#,
        ] {
            assert_eq!(
                rename_constraint_definition_column(definition, "a", "b"),
                None,
                "the guard leaves `{definition}` untouched: stale is acceptable, corrupt is not"
            );
        }
        assert_eq!(
            rename_constraint_definition_column(r#"UNIQUE ("a""b")"#, r#"a""b"#, "c"),
            None,
            "the guard fires on the SHAPE, so even a parse that happens to name the \
             renamed column cannot get through it"
        );
    }

    fn incoming_fk_table(constraints: &[(&str, &str, &str)]) -> TableSnapshot {
        TableSnapshot {
            columns: Vec::new(),
            indexes: Vec::new(),
            constraints: constraints
                .iter()
                .map(|(name, kind, definition)| ConstraintSnapshot {
                    name: (*name).to_string(),
                    kind: (*kind).to_string(),
                    definition: (*definition).to_string(),
                    comment: None,
                    cascade_columns: None,
                })
                .collect(),
            runtime_options: TableRuntimeOptions::default(),
            partition_by: None,
            comment: None,
            stored_create_sql: None,
        }
    }

    /// The referenced TABLE is matched before anything is rewritten. A same-named
    /// column in a DIFFERENT parent keeps its FK byte-identical - matching on the
    /// column name alone would turn a stale definition into a corrupt one, naming a
    /// column its parent does not have.
    #[test]
    fn incoming_fk_column_rewrite_matches_the_referenced_table_first() {
        let mut tables = BTreeMap::new();
        tables.insert(
            "child".to_string(),
            incoming_fk_table(&[
                (
                    "child_p1_fkey",
                    "FOREIGN KEY",
                    "FOREIGN KEY (p1_k) REFERENCES proj.p1(k)",
                ),
                (
                    "child_p2_fkey",
                    "FOREIGN KEY",
                    "FOREIGN KEY (p2_k) REFERENCES proj.p2(k)",
                ),
                ("child_k_key", "UNIQUE", "UNIQUE (k)"),
            ]),
        );
        rewrite_incoming_fk_column_targets(&mut tables, "proj", "p1", "k", "uid");
        let definitions = tables["child"]
            .constraints
            .iter()
            .map(|constraint| constraint.definition.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            definitions,
            vec![
                "FOREIGN KEY (p1_k) REFERENCES proj.p1(uid)",
                "FOREIGN KEY (p2_k) REFERENCES proj.p2(k)",
                "UNIQUE (k)",
            ],
            "only the FK targeting the RENAMED table moves; the same-named column in \
             another parent and the local UNIQUE list are untouched"
        );
    }

    /// The referenced list is POSITIONAL, the LOCAL list is never touched by this
    /// walk (the rename arm owns it), and the tail after the group survives.
    #[test]
    fn incoming_fk_column_rewrite_moves_one_position_and_keeps_the_tail() {
        let mut tables = BTreeMap::new();
        tables.insert(
            "child".to_string(),
            incoming_fk_table(&[(
                "child_ab_fkey",
                "FOREIGN KEY",
                "FOREIGN KEY (a, b) REFERENCES proj.parent(a, b) ON DELETE CASCADE",
            )]),
        );
        rewrite_incoming_fk_column_targets(&mut tables, "proj", "parent", "a", "order");
        assert_eq!(
            tables["child"].constraints[0].definition,
            "FOREIGN KEY (a, b) REFERENCES proj.parent(\"order\", b) ON DELETE CASCADE",
            "the local `(a, b)` stays put, the referenced `a` moves in place, quoting is \
             re-derived, and the referential-action tail survives"
        );
    }
}
