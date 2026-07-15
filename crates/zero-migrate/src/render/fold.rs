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
//! ([`build_table_snapshot`](crate::render::declarative::build_table_snapshot),
//! [`ir_fk_constraint_snapshot_for_columns`](crate::render::declarative::ir_fk_constraint_snapshot_for_columns),
//! [`ir_column_to_field`](crate::render::lower::ir_column_to_field),
//! [`create_index_snapshot`](crate::render::lower::create_index_snapshot), …). Because
//! the engine APPLIES the same ops the fold replays through that builder, and the
//! differ's `desired_snapshot` is already round-trip-proven equal to
//! `snapshot_schema(live)` (the `declarative_pg` round-trip tests), the folded
//! snapshot is structurally identical to live introspection — transitively
//! `fold == introspect`. The headline correctness net is the round-trip oracle
//! (`tests/fold_roundtrip_pg.rs`): apply a corpus to real PG, introspect, assert
//! equality.
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
//! An op's `schema` qualifier governs WHICH schema the DDL renders into (cross-
//! schema confinement, an apply-time concern) and its `existence_guard` governs
//! apply-time presence; neither changes the final FOLDED logical shape. A folded
//! snapshot that already applied is coherent, so the fold ignores both. FK
//! definitions DO embed the `project_schema` (`REFERENCES <schema>.<target>(id)`),
//! so the caller passes the schema the live DB is introspected under (the oracle
//! passes `cfg.project_schema`) for the FK `definition` to compare equal.

use std::collections::BTreeMap;

use crate::model::ir::{
    ColType, CommentTarget, IrColumn, IrConstraint, IrConstraintKind, IrDefault, IrIndex, Op,
    RefAction, SafeI64, SafeU64, SequenceOwnedBy, TableRuntimeOptions,
};
use crate::model::snapshot::{
    normalize_sequence_max_value, normalize_sequence_min_value, sequence_default_start_value,
    ColumnSnapshot, ConstraintSnapshot, ExtensionSnapshot, IndexSnapshot, NamedTypeSnapshot,
    PartitionSnapshot, RoleSnapshot, SchemaObjectSnapshot, SchemaSnapshot,
    SequenceDataTypeSnapshot, SequenceSnapshot, TableSnapshot, ViewSnapshot,
};
use crate::render::declarative::{
    build_resolved_table_snapshot, build_table_snapshot, constraintdef_cols,
    ir_fk_constraint_snapshot_for_columns, push_primary_key_snapshot, quote_ident_if_needed,
    CollectionDescriptor, DeclarativeError,
};
use crate::render::lower::{
    create_index_snapshot, derived_check_constraint_name, derived_constraint_name,
    derived_exclusion_constraint_name, enum_inline_check, index_method_access, ir_column_to_field,
    ir_column_to_field_resolved_create, mysql_enum_type, render_container_default_for_data_type,
    render_domain_check, render_exclusion_constraint_body, render_ir_default,
    render_ir_default_for_type, render_json_default_for_data_type, IrLowerError, NamedTypeRegistry,
};
use crate::render::renderer::{Capability, DialectSupports};
use crate::schema::query::SqlDialect;

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
    /// A `renameColumn` `to` name already exists on the table.
    RenameCollision {
        /// The table.
        table: String,
        /// The `to` name that already exists.
        to: String,
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
            FoldError::RenameCollision { table, to } => {
                write!(f, "fold: rename target `{table}.{to}` already exists")
            }
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

fn selected_dialectal_leg<'a>(
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

fn flatten_dialectal_ops(ops: &[Op], dialect: SqlDialect) -> Result<Vec<&Op>, FoldError> {
    let mut out = Vec::new();
    for op in ops {
        push_fold_op(&mut out, op, dialect, false)?;
    }
    Ok(out)
}

/// Replay an ordered [`Op`] list into the current logical [`SchemaSnapshot`].
/// Pure, offline, NO DB I/O — the offline companion of the live
/// [`snapshot_schema`](crate::apply::drift::snapshot_schema).
///
/// `dialect` selects the per-dialect shaping the shared builder applies (PG vs
/// SQLite FTS folding, etc.); `project_schema` is embedded in FK `definition`s
/// (`REFERENCES <schema>.<target>(id)`) — pass the schema the live DB is
/// introspected under for the round-trip equality to hold.
///
/// DML ops fold to no-ops; an incoherent stream is a structured [`FoldError`].
///
/// # Errors
/// See [`FoldError`] for the closed set of fail-closed conditions.
pub fn fold_ops(
    ops: &[Op],
    dialect: SqlDialect,
    project_schema: &str,
) -> Result<SchemaSnapshot, FoldError> {
    let mut tables: BTreeMap<String, TableSnapshot> = BTreeMap::new();
    let mut partitions: BTreeMap<String, PartitionSnapshot> = BTreeMap::new();
    let mut views: BTreeMap<String, ViewSnapshot> = BTreeMap::new();
    let mut sequences: BTreeMap<String, SequenceSnapshot> = BTreeMap::new();
    let mut named_types = NamedTypeRegistry::default();
    let mut named_type_snapshots: BTreeMap<String, NamedTypeSnapshot> = BTreeMap::new();
    let mut roles: BTreeMap<String, RoleSnapshot> = BTreeMap::new();
    let mut schemas: BTreeMap<String, SchemaObjectSnapshot> = BTreeMap::new();
    let mut extensions: BTreeMap<String, ExtensionSnapshot> = BTreeMap::new();

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
                ..
            } => {
                if tables.contains_key(name) {
                    return Err(FoldError::DuplicateTable(name.clone()));
                }
                if views.contains_key(name) {
                    return Err(FoldError::DuplicateView(name.clone()));
                }
                if partitions.contains_key(name) {
                    return Err(FoldError::DuplicateTable(name.clone()));
                }
                let desc = create_table_descriptor(name, columns, runtime_options.as_ref());
                let mut snap = build_resolved_table_snapshot(project_schema, &desc, dialect)?;
                snap.partition_by = partition_by.clone();
                if let Some(pk) = primary_key {
                    push_primary_key_snapshot(name, &mut snap, pk);
                }
                apply_fold_author_type_overrides_to_snapshot(name, columns, &mut snap, dialect)?;
                apply_fold_structured_defaults_to_snapshot(name, columns, &mut snap, dialect)?;
                apply_fold_named_type_metadata(
                    name,
                    columns,
                    &mut snap,
                    &named_types,
                    dialect,
                    project_schema,
                )?;
                fold_create_table_specs(
                    name,
                    project_schema,
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
                partitions.insert(
                    name.clone(),
                    PartitionSnapshot {
                        of: of.clone(),
                        bounds: bounds.clone(),
                    },
                );
            }
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
                tables.remove(name);
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
                partitions.remove(name);
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
            }
            Op::RenameTable { table, to, .. } => {
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
                let snap = tables
                    .remove(table)
                    .ok_or_else(|| FoldError::MissingTable(table.clone()))?;
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
                )?;
                let source_col = IrColumn {
                    name: column.clone(),
                    ty: ty.clone(),
                    nullable: *nullable,
                    default: default.clone(),
                    unique: None,
                    id_prefix: None,
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
                )?;
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
                // (1) Drop every index covering the column. `IndexSnapshot::columns`
                //     is the raw key-column list, so an exact name compare suffices;
                //     a multi-column index partially covering it is dropped too.
                snap.indexes
                    .retain(|i| !i.columns.iter().any(|c| c == column));
                // (2) Drop every constraint whose LOCAL column list contains the
                //     column. UNIQUE (`UNIQUE (cols)`) and FOREIGN KEY
                //     (`FOREIGN KEY (cols) REFERENCES …`) both carry their local
                //     columns as the leading parenthesized group; the system
                //     `<table>_pkey` is `PRIMARY KEY (id)` and CHECK is never folded,
                //     so neither false-matches a non-`id` user column. Collect the
                //     dropped constraint names first to cascade their implicit unique
                //     indexes (mirror the DropConstraint index-cascade below).
                //
                //     Capture (name, kind) so the implicit-index cascade can
                //     discriminate by kind: only UNIQUE / PRIMARY KEY back a same-named
                //     index PG cascades; a FOREIGN KEY backs none. Cascading the index
                //     by a FK's name would wrongly phantom-drop an INDEPENDENT user
                //     index that merely shares the FK's name (PG allows a FK and an
                //     index to share a name — see the DropConstraint arm), so the
                //     implicit-index retain below is kind-gated for safety.
                let dropped_constraints: Vec<(String, String)> = snap
                    .constraints
                    .iter()
                    .filter(|c| constraint_local_columns_contain(&c.definition, column))
                    .map(|c| (c.name.clone(), c.kind.clone()))
                    .collect();
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
                                id_prefix: None,
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
                col.data_type = new_col.data_type;
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
                        render_container_default_for_data_type(*kind, &col.data_type)
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
                col.default = Some(rendered);
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
            }
            Op::AddConstraint {
                table, constraint, ..
            } => {
                // Build the constraint (+ its implicit index for UNIQUE/PK, which PG
                // MATERIALIZES and live introspection reports) the SAME way the lower's
                // snapshot half does: FK via the shared `ir_fk_*`, UNIQUE/PK with a
                // `pg_get_constraintdef`-matching body + the implicit unique index PG
                // names after the constraint, CHECK deferred. Verify the target table
                // FIRST (fail-closed) before stamping.
                let folded = add_constraint_snapshot(table, project_schema, constraint, dialect)?;
                let snap = table_mut(&mut tables, table)?;
                push_folded_constraint(table, snap, folded)?;
                snap.constraints.sort_by(|a, b| a.name.cmp(&b.name));
                snap.indexes.sort_by(|a, b| a.name.cmp(&b.name));
            }
            Op::DropConstraint { table, name, .. } => {
                let snap = table_mut(&mut tables, table)?;
                // Capture the dropped constraint's KIND before the retain so the
                // index-cascade below can discriminate. Only UNIQUE / PRIMARY KEY
                // constraints back an implicit same-named index that PG cascades on
                // drop; a FOREIGN KEY has NO backing index. PG lets a FK constraint
                // and an independent user INDEX share a name (verified live on :5440:
                // `ADD CONSTRAINT shared FOREIGN KEY …` + `CREATE INDEX shared …`
                // coexist, and `DROP CONSTRAINT shared` leaves the index intact), and
                // validate.rs does not forbid the coexistence — so an unconditional
                // `retain(|i| &i.name != name)` would WRONGLY phantom-drop the user
                // index here, breaking `fold_ops == snapshot_schema(live)`.
                let dropped_kind = snap
                    .constraints
                    .iter()
                    .find(|c| &c.name == name)
                    .map(|c| c.kind.clone());
                let before = snap.constraints.len();
                snap.constraints.retain(|c| &c.name != name);
                if snap.constraints.len() == before {
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
            Op::ValidateConstraint { .. } => {
                // Validating a previously NOT VALID constraint changes no structural
                // schema state — the constraint already exists in the folded snapshot
                // (added by its `ADD CONSTRAINT … NOT VALID`); VALIDATE only strengthens
                // it. Fold-invisible (see the addConstraint NOT VALID note).
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
                columns,
                materialized,
                ..
            } => {
                if tables.contains_key(name) {
                    return Err(FoldError::DuplicateTable(name.clone()));
                }
                if views.contains_key(name) {
                    return Err(FoldError::DuplicateView(name.clone()));
                }
                views.insert(
                    name.clone(),
                    ViewSnapshot {
                        materialized: materialized.unwrap_or(false),
                        columns: columns.clone(),
                        definition: None,
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
            // Remaining vendor ops either change unmodeled facets (role settings,
            // grants/RLS/policies/triggers/functions) or are raw statements, so
            // they do not contribute to this structural snapshot.
            Op::AlterRole { .. }
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
            | Op::PgRaw { .. } => {}
            Op::Dialectal { .. } => {}
        }
    }

    Ok(SchemaSnapshot {
        tables,
        partitions,
        views,
        named_types: named_type_snapshots,
        sequences,
        roles,
        schemas,
        extensions,
    })
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
/// shared `build_table_snapshot` (a one-field descriptor) and pulls the matching
/// column out, so the default / encryption / comment sentinel is built by the shared
/// kernel, never re-spelled. Mirrors `IrAuthor::add_column_snapshot_with_sibling`.
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
        id_prefix: None,
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
    let snap = build_table_snapshot(project_schema, &desc, dialect)?;
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
        if fold_author_data_type_override(&source.ty, dialect).is_none() {
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
    let Some(data_type) = fold_author_data_type_override(ty, dialect) else {
        return Ok(());
    };
    if col.name != column {
        return Err(FoldError::MissingColumn {
            table: table.to_string(),
            column: column.to_string(),
        });
    }
    col.data_type = data_type.to_string();
    Ok(())
}

fn fold_author_data_type_override(ty: &ColType, dialect: SqlDialect) -> Option<&'static str> {
    match (dialect, ty) {
        (SqlDialect::Postgres, ColType::Uuid) => Some("uuid"),
        _ => None,
    }
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
            | IrDefault::Nextval { .. },
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
        | IrDefault::Nextval { .. }),
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
        )?;
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
) -> Result<(), FoldError> {
    match &source.ty {
        ColType::Enum { name, .. } => match dialect {
            SqlDialect::Postgres => {
                let schema = named_types.enum_schema_or(name, project_schema);
                col.data_type = pg_type_data_type(schema, name);
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
                let schema = named_types.domain_schema_or(name, project_schema);
                col.data_type = pg_type_data_type(schema, name);
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
/// This fold and the lower ([`IrAuthor::fold_create_table_specs`]) agree on every
/// constraint/index NAME and on the UNIQUE definition body (both route through the
/// shared `constraintdef_cols` speller), so an op-authored table re-diffs clean
/// against the apply path. They DELIBERATELY differ on one point — NOT "byte
/// identical": for a table-level UNIQUE the fold ALSO materializes the implicit
/// unique index, whereas the lower pushes only the `ConstraintSnapshot`
/// (`render::lower` ~2057-2070, no index). The reason is that the two snapshots
/// model different things:
/// - the lower's is an EMISSION PLAN — `snap.indexes` drives `CREATE INDEX`, and PG
///   auto-creates the constraint's implicit index, so emitting it would duplicate;
/// - the fold's is a LOGICAL-STATE model — it must match what `snapshot_schema`
///   reports, and live introspection DOES return constraint-backed unique indexes
///   (the `pg_index` query has no constraint filter, drift.rs ~675).
///
/// So the fold's implicit-index materialization is REQUIRED for `fold == introspect`
/// to hold; do NOT "align" it with the lower by removing it — that would break the
/// round-trip oracle.
///
/// Fail-closed parity with the lower:
/// - a create-table PRIMARY KEY is carried by the op's top-level `primary_key`;
///   stale constraint-form PKs are ignored here after validation;
/// - table-level CHECK folds only on PostgreSQL until the non-PG renderers land;
/// - a multi-column / non-`id`-referencing FK is refused;
/// - a partial-index `where` is refused (closed-AST predicate);
/// - on **SQLite**, a table-level FOREIGN KEY, a table-level UNIQUE, and a
///   non-btree index `using` are refused — BYTE-FOR-BYTE parity with the lower
///   ([`crate::render::lower::IrAuthor::fold_create_table_specs`]).
///
/// The SQLite refusals are NOT cosmetic: in the migration-first model the fold is
/// the SOLE source of truth for gen-types. The lower (= the apply path) REFUSES
/// these shapes on SQLite (the SQLite CREATE renders from the descriptor; a
/// table-level FK/UNIQUE / non-btree `using` is not threaded into the emitter), so
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
                push_folded_constraint(
                    table,
                    snap,
                    FoldedConstraint {
                        constraint: ConstraintSnapshot {
                            name,
                            kind: "CHECK".to_string(),
                            definition: format!("CHECK ({rendered})"),
                            comment: None,
                        },
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
                        "createTable table-level FOREIGN KEY on SQLite (the SQLite \
                         CREATE renders from the descriptor; a table-level FK is not \
                         threaded into the emitter)",
                    ));
                }
                if columns.is_empty() {
                    return Err(FoldError::Unsupported(
                        "createTable FOREIGN KEY with no local column",
                    ));
                }
                let fk = ir_fk_constraint_snapshot_for_columns(
                    project_schema,
                    c.name.as_deref(),
                    columns,
                    references_table,
                    references_columns,
                    on_delete.map(RefAction::as_token),
                    on_update.map(RefAction::as_token),
                    deferrable.unwrap_or(false),
                    initially_deferred.unwrap_or(false),
                    dialect,
                );
                // A FOREIGN KEY materializes no index.
                push_folded_constraint(
                    table,
                    snap,
                    FoldedConstraint {
                        constraint: fk,
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
                push_folded_constraint(table, snap, unique_constraint(&name, columns))?;
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
                        constraint: ConstraintSnapshot {
                            name,
                            kind: "EXCLUDE".to_string(),
                            // PG canonicalizes exclusion bodies differently from the
                            // authored render. Drift tracks EXCLUDE by presence/name +
                            // kind only, matching `snapshot_schema`.
                            definition: String::new(),
                            comment: None,
                        },
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
    // Deterministic name ordering (build_table_snapshot sorts; live is name-sorted).
    snap.constraints.sort_by(|a, b| a.name.cmp(&b.name));
    snap.indexes.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(())
}

/// A folded constraint + the implicit unique INDEX (if any) PG MATERIALIZES for it.
///
/// A UNIQUE / PRIMARY KEY constraint creates a `pg_constraint` row AND an implicit
/// unique index of the SAME name (`pg_index` reports it), so live introspection
/// returns BOTH. The fold must mirror both for `fold == introspect` to hold (the
/// shared `build_table_snapshot` already does this for the system `<table>_pkey`).
/// A FOREIGN KEY creates no index, so `index` is `None`.
struct FoldedConstraint {
    constraint: ConstraintSnapshot,
    index: Option<IndexSnapshot>,
}

/// Push a folded constraint (+ its implicit index) onto the table, fail-closed on a
/// name collision against an existing constraint OR index of the same name.
fn push_folded_constraint(
    table: &str,
    snap: &mut TableSnapshot,
    folded: FoldedConstraint,
) -> Result<(), FoldError> {
    if snap
        .constraints
        .iter()
        .any(|c| c.name == folded.constraint.name)
    {
        return Err(FoldError::DuplicateConstraint {
            table: table.to_string(),
            name: folded.constraint.name,
        });
    }
    if let Some(idx) = &folded.index {
        if snap.indexes.iter().any(|i| i.name == idx.name) {
            return Err(FoldError::DuplicateIndex(idx.name.clone()));
        }
    }
    snap.constraints.push(folded.constraint);
    if let Some(idx) = folded.index {
        snap.indexes.push(idx);
    }
    Ok(())
}

/// True iff the LOCAL column list of a constraint `definition` contains `column`.
///
/// Used by the `DropColumn` cascade: PG auto-drops a UNIQUE/FK constraint when one
/// of its local columns is dropped, so the fold must too. Both the foldable
/// column-list constraints carry their LOCAL columns as the LEADING parenthesized
/// group — `UNIQUE (cols)` and `FOREIGN KEY (cols) REFERENCES <schema>.<tgt>(id)…`
/// — so we parse that first `(...)`. The FK's REFERENCED column list (`(id)`) comes
/// AFTER `REFERENCES`, never in the leading group, so a column named `id` on the
/// REFERENCING side is matched while the referenced `(id)` is correctly ignored.
/// The system `<table>_pkey` (`PRIMARY KEY (id)`) is the only PK and CHECK bodies
/// are never folded, so neither false-matches a non-`id` user column.
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

/// A UNIQUE constraint + its implicit unique index (PG names the index after the
/// constraint). The index covers the same columns, btree, unique.
fn unique_constraint(name: &str, columns: &[String]) -> FoldedConstraint {
    FoldedConstraint {
        constraint: ConstraintSnapshot {
            name: name.to_string(),
            kind: "UNIQUE".to_string(),
            definition: format!("UNIQUE ({})", constraintdef_cols(columns)),
            comment: None,
        },
        index: Some(IndexSnapshot::btree(
            name.to_string(),
            true,
            columns.to_vec(),
        )),
    }
}

/// The folded constraint (plus implicit index) a stand-alone `addConstraint` op
/// produces. FK via the shared `ir_fk_*` (no index); UNIQUE/PK via the derived name
/// with a `pg_get_constraintdef`-matching body and the implicit unique index; CHECK
/// deferred.
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
            // NOT VALID is fold-invisible: the folded DESIRED schema is the
            // eventual-validated constraint (a later VALIDATE CONSTRAINT is a
            // fold no-op), matching the live catalog after validation.
            not_valid: _,
        } => {
            if columns.is_empty() {
                return Err(FoldError::Unsupported(
                    "addConstraint(fk) with no local column",
                ));
            }
            Ok(FoldedConstraint {
                constraint: ir_fk_constraint_snapshot_for_columns(
                    project_schema,
                    name,
                    columns,
                    references_table,
                    references_columns,
                    on_delete.map(RefAction::as_token),
                    on_update.map(RefAction::as_token),
                    deferrable.unwrap_or(false),
                    initially_deferred.unwrap_or(false),
                    dialect,
                ),
                index: None,
            })
        }
        IrConstraintKind::Unique { columns } => {
            let cname = name.map_or_else(
                || derived_constraint_name(table, columns, "key"),
                str::to_string,
            );
            Ok(unique_constraint(&cname, columns))
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
            Ok(FoldedConstraint {
                constraint: ConstraintSnapshot {
                    name: cname,
                    kind: "CHECK".to_string(),
                    definition: format!("CHECK ({rendered})"),
                    comment: None,
                },
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
                constraint: ConstraintSnapshot {
                    name: cname,
                    kind: "EXCLUDE".to_string(),
                    // PG canonicalizes exclusion bodies differently from the authored
                    // render. Drift tracks EXCLUDE by presence/name + kind only,
                    // matching `snapshot_schema`.
                    definition: String::new(),
                    comment: None,
                },
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

/// Convert a numeric [`IrScalar`] literal to `f64` for a `min`/`max` bound, or
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

/// Convert an [`IrScalar`] literal to the `serde_json::Value` an enum membership
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
        // `Null` / `Bytes` are not enum-member shapes the facet models.
        IrScalar::Null | IrScalar::Bytes(_) => None,
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
///   from the op `IrColumn` via [`ir_column_to_field`] (reusing the shared
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
/// be coherent first).
pub fn fold_to_field_defs(
    ops: &[Op],
    dialect: SqlDialect,
    project_schema: &str,
) -> Result<BTreeMap<String, serde_json::Value>, FoldError> {
    // 1. Fail-closed coherence. `fold_ops` is the structural-coherence oracle
    //    (add-to-missing-table, drop-absent-column, duplicate-create, …).
    fold_ops(ops, dialect, project_schema)?;

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
    // Per-table recovered FK policy (`onDelete`/`onUpdate`) to lift onto the ref
    // column at the end. The op.* model carries the FK target on the `ColType::Ref`
    // column AND the FK POLICY on a separate `IrConstraintKind::Fk` constraint
    // (mirroring how the lower/differ emit both); the column-only `ir_column_to_field`
    // recovers the `ref` brand but not the policy, so we lift policy from the Fk
    // constraint here — the "recover from the applied FK constraint" path.
    let mut fks: BTreeMap<String, Vec<RecoveredFk>> = BTreeMap::new();

    let replay_ops = flatten_dialectal_ops(ops, dialect)?;
    for op in replay_ops {
        match op {
            Op::CreateTable {
                name,
                columns,
                constraints,
                ..
            } => {
                let system_prefix_len = confined_resolved_system_prefix_len(columns);
                let mut cols: indexmap::IndexMap<
                    String,
                    crate::render::declarative::FieldDescriptor,
                > = indexmap::IndexMap::new();
                for (idx, c) in columns.iter().enumerate() {
                    cols.insert(
                        c.name.clone(),
                        fold_create_column_to_field(c, idx < system_prefix_len),
                    );
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
                            if let Some(recovered) =
                                recover_fk_policy(columns, *on_delete, *on_update)
                            {
                                fks.entry(name.clone()).or_default().push(recovered);
                            }
                        }
                        _ => {}
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
                if let Some(cols) = tables.remove(table) {
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
                        if f.ty == "ref" && f.references.as_deref() == Some(table.as_str()) {
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
                    let field = ir_column_to_field(&IrColumn {
                        name: column.clone(),
                        ty: ty.clone(),
                        nullable: *nullable,
                        default: default.clone(),
                        unique: None,
                        id_prefix: None,
                        case_sensitive: *case_sensitive,
                        vector_metric: *vector_metric,
                        mask: *mask,
                        generated: generated.clone(),
                        identity: *identity,
                    });
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
                    if let Some(recovered) = recover_fk_policy(columns, *on_delete, *on_update) {
                        fks.entry(table.clone()).or_default().push(recovered);
                    }
                }
                if let IrConstraintKind::Check { expr, .. } = &constraint.kind {
                    if let Some(facet) = recover_check_facet(expr) {
                        checks.entry(table.clone()).or_default().push(facet);
                    }
                }
            }
            // Every other op (DML, index, type/nullability alters, drop*) does not
            // change the reconstructed column-facet shape.
            _ => {}
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

fn fold_create_column_to_field(
    c: &IrColumn,
    in_confined_system_prefix: bool,
) -> crate::render::declarative::FieldDescriptor {
    let mut field = ir_column_to_field(c);
    if in_confined_system_prefix && c.name == "id" && c.id_prefix.is_some() {
        field.ty = "id".to_string();
        field.required = false;
    }
    field
}

fn confined_resolved_system_prefix_len(columns: &[IrColumn]) -> usize {
    let shape = crate::model::profile::PolicyProfile::confined().system_shape;
    if columns.len() < shape.columns.len() {
        return 0;
    }
    let matches = columns
        .iter()
        .zip(&shape.columns)
        .all(|(actual, expected)| resolved_system_column_matches(actual, expected));
    if matches {
        shape.columns.len()
    } else {
        0
    }
}

fn resolved_system_column_matches(
    actual: &IrColumn,
    expected: &crate::model::profile::InjectedSystemColumnPolicy,
) -> bool {
    if actual.name != expected.name {
        return false;
    }
    if actual.name == "id" && actual.identity.is_some() {
        return matches!(actual.ty, ColType::Int | ColType::BigInt);
    }
    actual.ty == system_column_type_token(&expected.data_type)
        && actual.nullable == Some(expected.nullable)
        && actual.default.is_none()
        && actual.unique.is_none()
        && actual.case_sensitive.is_none()
        && actual.vector_metric.is_none()
        && actual.mask.is_none()
        && actual.generated.is_none()
        && actual.identity.is_none()
}

fn system_column_type_token(data_type: &str) -> ColType {
    match data_type {
        "text" => ColType::Text,
        "timestamp with time zone" => ColType::Timestamp,
        "integer" => ColType::Int,
        _ => ColType::Text,
    }
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
        // `t.id({prefix})` re-declares the system `id` PK as a `uuid` carrying the
        // prefix; the inverse of `ir_column_to_field`'s `name=="id" && Uuid → "id"`.
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
///   [`recover_check_facet`] lifts back ([`facet_check_constraints`]).
///
/// One resolved `Op::CreateTable` per descriptor, in descriptor order. The columns
/// include the confined profile's resolved system fields, the top-level
/// `primary_key` is explicit, and system indexes are carried as ordinary IR
/// indexes before checksum/fold.
///
/// # Errors
/// [`ProduceError`] for an unmappable type token, an unrepresentable CHECK facet
/// value, or a table-shape resolution error — fail closed, never an op whose fold
/// would silently drop the facet.
pub fn descriptors_to_create_ops(
    descriptors: &[crate::render::declarative::CollectionDescriptor],
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
            columns.push(IrColumn {
                name: f.name.clone(),
                ty,
                nullable,
                default,
                unique: if f.unique { Some(true) } else { None },
                id_prefix: f.id_prefix.clone(),
                vector_metric,
                case_sensitive: f.case_sensitive,
                mask,
                generated: f.generated.clone(),
                identity: f.identity,
            });
            // A `ref` column carries the FK target on its `ColType::Ref` (the brand)
            // AND its POLICY on a separate `Fk` constraint — the SAME split the
            // lower/differ emit, and the shape `fold_to_field_defs` recovers policy
            // from (`recover_fk_policy`). Emit it so onDelete/onUpdate round-trip.
            if f.ty == "ref" {
                if let Some(target) = &f.references {
                    constraints.push(IrConstraint {
                        name: Some(format!("{}_{}_fkey", d.name, f.name)),
                        kind: IrConstraintKind::Fk {
                            columns: vec![f.name.clone()],
                            references_table: target.clone(),
                            references_columns: vec!["id".to_string()],
                            on_delete: f.on_delete.as_deref().and_then(parse_ref_action),
                            on_update: f.on_update.as_deref().and_then(parse_ref_action),
                            deferrable: None,
                            initially_deferred: None,
                            not_valid: None,
                        },
                    });
                }
            }
            constraints.extend(facet_check_constraints(&d.name, f)?);
        }
        let op = Op::CreateTable {
            name: d.name.clone(),
            columns,
            primary_key: None,
            constraints,
            indexes: Vec::new(),
            partition_by: None,
            runtime_options: Some(d.runtime_options.clone()),
            schema: None,
            existence_guard: None,
        };
        let ir = crate::model::ir::MigrationIr {
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
        let resolved = crate::model::table_shape::resolve_create_table_policy(&ir, effective)
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
        TableStrictness, CURRENT_IR_VERSION,
    };
    use crate::model::policy::SchemaScope;
    
    use crate::model::table_shape::{resolve_create_table_policy, zeroship_confined_ceiling};
    use crate::model::validate::{validate_ir_scoped, Dialect, UnsupportedKind, CODE_UNSUPPORTED};

    const SCHEMA: &str = "proj_test";

    fn fold(ops: &[Op]) -> Result<SchemaSnapshot, FoldError> {
        fold_ops(ops, SqlDialect::Postgres, SCHEMA)
    }

    fn validate_ops(ops: Vec<Op>, dialect: Dialect) -> crate::model::validate::AuthoringError {
        let ir = crate::model::ir::MigrationIr {
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
        validate_ir_scoped(
            &ir,
            dialect,
            &[],
            Some(&SchemaScope::Unconfined)
        )
        .unwrap_err()
    }

    fn assert_validate_ops_ok(ops: Vec<Op>, dialect: Dialect) {
        let ir = crate::model::ir::MigrationIr {
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
        validate_ir_scoped(
            &ir,
            dialect,
            &[],
            Some(&SchemaScope::Unconfined)
        )
        .expect("ops validate");
    }

    fn col(name: &str, ty: ColType, nullable: bool) -> IrColumn {
        IrColumn {
            name: name.to_string(),
            ty,
            nullable: Some(nullable),
            default: None,
            unique: None,
            id_prefix: None,
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
        resolve_create_table_policy(&ir, &zeroship_confined_ceiling())
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
            create("members", vec![col("id", ColType::Uuid, false)]),
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
            id_prefix: None,
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
        let snap = fold_ops(&ops, SqlDialect::Postgres, SCHEMA)
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

    /// REGRESSION: a createTable TABLE-LEVEL FOREIGN KEY is refused at
    /// validate-time on SQLite.
    #[test]
    fn create_table_level_fk_unsupported_on_sqlite() {
        let parents = create("teams", vec![col("label", ColType::Text, false)]);
        let kids = create_with(
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
        );
        let err = validate_ops(vec![parents, kids], Dialect::Sqlite);
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert_eq!(err.kind, Some(UnsupportedKind::Op));
        assert!(err.reason.contains("foreign keys"));
        // The SAME shape FOLDS on Postgres (the parity is dialect-scoped).
        let parents = create("teams", vec![col("label", ColType::Text, false)]);
        let kids = create_with(
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
        );
        assert!(
            fold(&[parents, kids]).is_ok(),
            "table-level FK folds on Postgres"
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
            id_prefix: None,
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
        build_table_snapshot(SCHEMA, &desc, SqlDialect::Postgres)
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
                    id_prefix: None,
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
                    id_prefix: None,
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
        fold_to_field_defs(ops, SqlDialect::Postgres, SCHEMA).unwrap()
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
            id_prefix: Some("post".into()),
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
            id_prefix: None,
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
            id_prefix: None,
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
            id_prefix: None,
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
            id_prefix: None,
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
            id_prefix: None,
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

    #[test]
    fn producer_emits_fk_constraint_with_policy_for_ref_columns() {
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
        let ops = descriptors_to_create_ops(&[d], &zeroship_confined_ceiling()).unwrap();
        let Op::CreateTable { constraints, .. } = &ops[0] else {
            panic!("expected a createTable")
        };
        let fk = constraints
            .iter()
            .find_map(|c| match &c.kind {
                IrConstraintKind::Fk {
                    columns,
                    on_delete,
                    on_update,
                    references_table,
                    ..
                } => Some((
                    columns.clone(),
                    *on_delete,
                    *on_update,
                    references_table.clone(),
                )),
                _ => None,
            })
            .expect("the ref column emits an Fk constraint carrying the policy");
        assert_eq!(fk.0, vec!["owner".to_string()]);
        assert_eq!(fk.1, Some(RefAction::Cascade));
        assert_eq!(fk.2, Some(RefAction::Restrict));
        assert_eq!(fk.3, "orgs");
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
        let ops = descriptors_to_create_ops(&[d], &zeroship_confined_ceiling()).unwrap();
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
        let ops = descriptors_to_create_ops(&[d], &zeroship_confined_ceiling()).unwrap();
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
                .take(7)
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "id",
                "created_at",
                "updated_at",
                "created_by",
                "updated_by",
                "version",
                "deleted_at"
            ],
            "producer emits the resolved confined system-field prefix"
        );
        assert_eq!(
            primary_key.as_deref(),
            Some(&["id".to_string()][..]),
            "producer carries the resolved top-level primary key"
        );
        assert_eq!(indexes.len(), 3, "producer carries resolved system indexes");
        let defs = fold_to_field_defs(&ops, SqlDialect::Postgres, SCHEMA).unwrap();
        let keys: Vec<&str> = defs["t"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec![
                "id",
                "created_at",
                "updated_at",
                "created_by",
                "updated_by",
                "version",
                "deleted_at",
                "zeta",
                "alpha",
                "mid"
            ],
            "resolved system prefix is carried and declared order is preserved, not sorted"
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
        let err = descriptors_to_create_ops(&[d], &zeroship_confined_ceiling()).unwrap_err();
        assert!(
            matches!(err, ProduceError::UnknownType { .. }),
            "unmappable token fails closed"
        );
    }
}
