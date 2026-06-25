//! **Migration-first P1 — the offline ops→schema fold (the keystone).**
//!
//! [`fold_ops`] replays an ordered [`Op`] list into a [`SchemaSnapshot`] — PURE,
//! offline, NO database I/O. It is the offline companion of the live
//! [`snapshot_schema`](crate::drift::snapshot_schema): the SAME `SchemaSnapshot`
//! output, sourced from the migration set instead of `pg_catalog`. The
//! migration-first design (`docs/proposals/2026-06-25-migration-first-schema.md`
//! §2.1) makes the `op.*` migrations the SOLE source of truth, and "the current
//! schema" is the fold of that set; later phases (`gen-types`) emit the `env.db`
//! types + the runtime descriptor from this snapshot.
//!
//! # Why it agrees with introspection (the load-bearing invariant)
//!
//! The fold does NOT re-implement column / type / default / sentinel shaping. It
//! REUSES the SHARED snapshot-builder the differ + the IR lower both use
//! ([`build_table_snapshot`](crate::declarative::build_table_snapshot),
//! [`ir_fk_constraint_snapshot`](crate::declarative::ir_fk_constraint_snapshot),
//! [`ir_column_to_field`](crate::ir_author::ir_column_to_field),
//! [`create_index_snapshot`](crate::ir_author::create_index_snapshot), …). Because
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
//! never a silently-wrong snapshot (P1 deliverable 2). A real `.ir.json` set the
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

use crate::declarative::{
    build_table_snapshot, ir_fk_constraint_snapshot, CollectionDescriptor, DeclarativeError,
};
use crate::drift::{ColumnSnapshot, ConstraintSnapshot, IndexSnapshot, SchemaSnapshot, TableSnapshot};
use crate::ir::{
    ColType, IrColumn, IrConstraint, IrConstraintKind, IrDefault, IrIndex, Op, RefAction,
};
use crate::ir_author::{
    create_index_snapshot, derived_constraint_name, index_method_access, ir_column_to_field,
    quote_cols,
};
use zeroship_schema::query::SqlDialect;

/// The owner-app stamp the fold gives every `CollectionDescriptor`. `owner_app` is
/// drift-irrelevant — it never enters `SchemaSnapshot` equality (the snapshot only
/// carries columns/indexes/constraints, none of which embed it), so a fold-internal
/// constant is correct. (Ownership is a deploy-time concern handled elsewhere; the
/// project-union phase P7 re-derives ownership from the migrations directly.)
const FOLD_OWNER_APP: &str = "__fold__";

/// A structured, fail-closed fold error (P1 deliverable 2). Every incoherent op
/// stream maps to a typed variant — never a silently-wrong snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoldError {
    /// An op targeted a table not present in the folded schema.
    MissingTable(String),
    /// A `createTable` named a table already present.
    DuplicateTable(String),
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
    /// A CHECK predicate (or other closed-AST `Expr`) whose SQL rendering is the
    /// deferred Expr→SQL wave — parity with `IrLowerError::ExprRenderDeferred`. The
    /// fold cannot materialize the constraint `definition`, so it fails closed
    /// rather than fold a partial / wrong CHECK body.
    ExprDeferred(&'static str),
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
            FoldError::ExprDeferred(what) => {
                write!(f, "fold: {what} carries a closed-AST predicate the offline fold cannot render (deferred Expr→SQL wave)")
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

/// Replay an ordered [`Op`] list into the current logical [`SchemaSnapshot`].
/// Pure, offline, NO DB I/O — the offline companion of the live
/// [`snapshot_schema`](crate::drift::snapshot_schema).
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

    for op in ops {
        match op {
            Op::CreateTable { name, columns, constraints, indexes, .. } => {
                if tables.contains_key(name) {
                    return Err(FoldError::DuplicateTable(name.clone()));
                }
                let desc = create_table_descriptor(name, columns);
                let mut snap = build_table_snapshot(project_schema, &desc, dialect)?;
                fold_create_table_specs(name, project_schema, &mut snap, constraints, indexes)?;
                tables.insert(name.clone(), snap);
            }
            Op::DropTable { table, .. } => {
                if tables.remove(table).is_none() {
                    return Err(FoldError::MissingTable(table.clone()));
                }
            }
            Op::AddColumn { table, column, ty, nullable, default, .. } => {
                let snap = table_mut(&mut tables, table)?;
                if snap.columns.iter().any(|c| &c.name == column) {
                    return Err(FoldError::DuplicateColumn {
                        table: table.clone(),
                        column: column.clone(),
                    });
                }
                let col = add_column_snapshot(
                    table,
                    column,
                    ty,
                    *nullable,
                    default.as_ref(),
                    project_schema,
                    dialect,
                )?;
                snap.columns.push(col);
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
            }
            Op::RenameColumn { table, from, to, .. } => {
                let snap = table_mut(&mut tables, table)?;
                // A pure rename keeps the column's type/nullable/default/sentinels;
                // only the NAME changes (the IR carries `ty` for the live-rename type
                // reconciliation the lower does, but the fold trusts the EXISTING
                // column's type — a pure rename cannot change type, the same stance
                // `lower_rename` takes: "the live column is the single authoritative
                // type source"). So we do NOT re-derive from `ty`.
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
            Op::AlterColumnType { table, column, ty, .. } => {
                // Re-derive ONLY `data_type` from the new type via the shared builder
                // (so `vector(N)` / `geography(...)` / encrypted-BYTEA spellings match
                // introspection's `canonical_extension_type`). Keep the live
                // `nullable`. The `using` cast is fold-irrelevant (it casts DATA, not
                // shape). We build a throwaway one-field snapshot to spell the type,
                // then transplant only `data_type` onto the existing column.
                let new_type =
                    add_column_snapshot(table, column, ty, None, None, project_schema, dialect)?
                        .data_type;
                let snap = table_mut(&mut tables, table)?;
                let col = snap
                    .columns
                    .iter_mut()
                    .find(|c| &c.name == column)
                    .ok_or_else(|| FoldError::MissingColumn {
                        table: table.clone(),
                        column: column.clone(),
                    })?;
                col.data_type = new_type;
            }
            Op::AlterColumnNullability { table, column, nullable, .. } => {
                let snap = table_mut(&mut tables, table)?;
                let col = snap
                    .columns
                    .iter_mut()
                    .find(|c| &c.name == column)
                    .ok_or_else(|| FoldError::MissingColumn {
                        table: table.clone(),
                        column: column.clone(),
                    })?;
                col.nullable = *nullable;
            }
            Op::AddConstraint { table, constraint, .. } => {
                // Build the ConstraintSnapshot the SAME way the lower's snapshot half
                // does (`lower_add_constraint`): FK via the shared `ir_fk_*`, UNIQUE
                // via `UNIQUE (cols)` + derived `_key` name, PK via `PRIMARY KEY (cols)`
                // + derived `_pkey` name, CHECK deferred. We must verify the target
                // table FIRST (fail-closed) before stamping.
                let cs = add_constraint_snapshot(table, project_schema, constraint)?;
                let snap = table_mut(&mut tables, table)?;
                if snap.constraints.iter().any(|c| c.name == cs.name) {
                    return Err(FoldError::DuplicateConstraint {
                        table: table.clone(),
                        name: cs.name,
                    });
                }
                snap.constraints.push(cs);
                snap.constraints.sort_by(|a, b| a.name.cmp(&b.name));
            }
            Op::DropConstraint { table, name, .. } => {
                let snap = table_mut(&mut tables, table)?;
                let before = snap.constraints.len();
                snap.constraints.retain(|c| &c.name != name);
                if snap.constraints.len() == before {
                    return Err(FoldError::MissingConstraint {
                        table: table.clone(),
                        name: name.clone(),
                    });
                }
            }
            Op::CreateIndex { table, columns, name, unique, using, .. } => {
                let idx = create_index_snapshot(table, columns, name.as_deref(), *unique, *using);
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
            // DML: schema no-ops (rows, not shape).
            Op::Insert { .. } | Op::Update { .. } | Op::Delete { .. } | Op::Backfill { .. } => {}
        }
    }

    Ok(SchemaSnapshot { tables })
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

/// The `CollectionDescriptor` for a `createTable` op — the SAME bridge
/// `IrAuthor::create_table_descriptor` builds (columns only; the table-level
/// constraints/indexes are folded on separately by [`fold_create_table_specs`],
/// exactly as the lower does). `owner_app` is the fold-internal constant (drift-
/// irrelevant).
fn create_table_descriptor(name: &str, columns: &[IrColumn]) -> CollectionDescriptor {
    CollectionDescriptor {
        name: name.to_string(),
        owner_app: FOLD_OWNER_APP.to_string(),
        fields: columns.iter().map(ir_column_to_field).collect(),
        indexes: Vec::new(),
    }
}

/// The `ColumnSnapshot` for a single field — routes ONE field through the shared
/// `build_table_snapshot` (a one-field descriptor) and pulls the matching column
/// out, so the default / encryption / comment sentinel is built by the shared
/// kernel, never re-spelled. Mirrors `IrAuthor::add_column_snapshot`.
fn add_column_snapshot(
    table: &str,
    column: &str,
    ty: &ColType,
    nullable: Option<bool>,
    default: Option<&IrDefault>,
    project_schema: &str,
    dialect: SqlDialect,
) -> Result<ColumnSnapshot, FoldError> {
    let field = ir_column_to_field(&IrColumn {
        name: column.to_string(),
        ty: ty.clone(),
        nullable,
        default: default.cloned(),
        unique: None,
    });
    let desc = CollectionDescriptor {
        name: table.to_string(),
        owner_app: FOLD_OWNER_APP.to_string(),
        fields: vec![field],
        indexes: Vec::new(),
    };
    let snap = build_table_snapshot(project_schema, &desc, dialect)?;
    snap.columns
        .into_iter()
        .find(|c| c.name == column)
        .ok_or(FoldError::Unsupported("addColumn (column folded away)"))
}

/// Fold a `createTable`'s TABLE-LEVEL constraints + indexes onto the
/// `build_table_snapshot`-built [`TableSnapshot`]. Byte-identical to
/// `IrAuthor::fold_create_table_specs` (each spec built the SAME way as its
/// stand-alone-op equivalent), so an op-authored table folds to the SAME snapshot
/// the apply path produces (and the differ re-diffs clean).
///
/// Fail-closed parity with the lower:
/// - a user composite / per-column PRIMARY KEY is refused (the platform owns the
///   synthetic `id` PK — a second PK is never satisfiable);
/// - a CHECK is refused with [`FoldError::ExprDeferred`] (closed-AST predicate);
/// - a multi-column / non-`id`-referencing FK is refused;
/// - a partial-index `where` is refused (closed-AST predicate).
///
/// Note: the lower's SQLite-specific refusals (table-level UNIQUE/FK not threaded
/// into the SQLite emitter) are an APPLY-render limitation, NOT a logical-shape
/// limitation — the folded snapshot is dialect-shaped by `build_table_snapshot`
/// (which already handles the FTS divergence). The fold's job is the LOGICAL shape;
/// the round-trip oracle exercises the supported corpus per dialect.
fn fold_create_table_specs(
    table: &str,
    project_schema: &str,
    snap: &mut TableSnapshot,
    constraints: &[IrConstraint],
    indexes: &[IrIndex],
) -> Result<(), FoldError> {
    for c in constraints {
        match &c.kind {
            IrConstraintKind::Pk { .. } => {
                return Err(FoldError::Unsupported(
                    "createTable user PRIMARY KEY (the platform owns the `id` primary key)",
                ));
            }
            IrConstraintKind::Check { .. } => {
                return Err(FoldError::ExprDeferred("createTable check"));
            }
            IrConstraintKind::Fk {
                columns,
                references_table,
                references_columns,
                on_delete,
                on_update,
            } => {
                let local = columns
                    .first()
                    .ok_or(FoldError::Unsupported("createTable FOREIGN KEY with no local column"))?;
                if columns.len() != 1 {
                    return Err(FoldError::Unsupported(
                        "createTable multi-column FOREIGN KEY (later wave)",
                    ));
                }
                if !(references_columns.is_empty()
                    || (references_columns.len() == 1 && references_columns[0] == "id"))
                {
                    return Err(FoldError::Unsupported(
                        "createTable FOREIGN KEY referencing a non-`id` column (later wave)",
                    ));
                }
                let fk = ir_fk_constraint_snapshot(
                    project_schema,
                    c.name.as_deref(),
                    local,
                    references_table,
                    on_delete.map(RefAction::as_token),
                    on_update.map(RefAction::as_token),
                );
                push_constraint(table, snap, fk)?;
            }
            IrConstraintKind::Unique { columns } => {
                let name = c.name.as_deref().map_or_else(
                    || derived_constraint_name(table, columns, "key"),
                    str::to_string,
                );
                push_constraint(
                    table,
                    snap,
                    ConstraintSnapshot {
                        name,
                        kind: "UNIQUE".to_string(),
                        definition: format!("UNIQUE ({})", quote_cols(columns)),
                    },
                )?;
            }
        }
    }
    for ix in indexes {
        if ix.r#where.is_some() {
            return Err(FoldError::ExprDeferred("createTable index where"));
        }
        let access = ix.using.map_or("btree", index_method_access);
        let name = ix.name.clone().unwrap_or_else(|| {
            crate::author::cap_ident_name(&format!("{table}_{}_idx", ix.columns.join("_")))
        });
        let mut snap_idx =
            IndexSnapshot::btree(name, ix.unique.unwrap_or(false), ix.columns.clone());
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

/// Push a constraint onto the table, fail-closed on a name collision.
fn push_constraint(
    table: &str,
    snap: &mut TableSnapshot,
    cs: ConstraintSnapshot,
) -> Result<(), FoldError> {
    if snap.constraints.iter().any(|c| c.name == cs.name) {
        return Err(FoldError::DuplicateConstraint {
            table: table.to_string(),
            name: cs.name,
        });
    }
    snap.constraints.push(cs);
    Ok(())
}

/// The `ConstraintSnapshot` a stand-alone `addConstraint` op produces — built the
/// SAME way as `IrAuthor::lower_add_constraint`'s snapshot half (FK via the shared
/// `ir_fk_*`, UNIQUE/PK via the derived name + `quote_cols` body, CHECK deferred).
fn add_constraint_snapshot(
    table: &str,
    project_schema: &str,
    constraint: &IrConstraint,
) -> Result<ConstraintSnapshot, FoldError> {
    let name = constraint.name.as_deref();
    match &constraint.kind {
        IrConstraintKind::Fk {
            columns,
            references_table,
            references_columns,
            on_delete,
            on_update,
        } => {
            let local = columns
                .first()
                .ok_or(FoldError::Unsupported("addConstraint(fk) with no local column"))?;
            if columns.len() != 1 {
                return Err(FoldError::Unsupported("addConstraint(fk) multi-column (later wave)"));
            }
            if !(references_columns.is_empty()
                || (references_columns.len() == 1 && references_columns[0] == "id"))
            {
                return Err(FoldError::Unsupported(
                    "addConstraint(fk) referencing a non-`id` column (later wave)",
                ));
            }
            Ok(ir_fk_constraint_snapshot(
                project_schema,
                name,
                local,
                references_table,
                on_delete.map(RefAction::as_token),
                on_update.map(RefAction::as_token),
            ))
        }
        IrConstraintKind::Unique { columns } => {
            let cname = name.map_or_else(
                || derived_constraint_name(table, columns, "key"),
                str::to_string,
            );
            Ok(ConstraintSnapshot {
                name: cname,
                kind: "UNIQUE".to_string(),
                definition: format!("UNIQUE ({})", quote_cols(columns)),
            })
        }
        IrConstraintKind::Pk { columns } => {
            let cname = name.map_or_else(
                || derived_constraint_name(table, columns, "pkey"),
                str::to_string,
            );
            Ok(ConstraintSnapshot {
                name: cname,
                kind: "PRIMARY KEY".to_string(),
                definition: format!("PRIMARY KEY ({})", quote_cols(columns)),
            })
        }
        IrConstraintKind::Check { .. } => Err(FoldError::ExprDeferred("addConstraint(check)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::Expr;
    use crate::ir::IrScalar;

    const SCHEMA: &str = "proj_test";

    fn fold(ops: &[Op]) -> Result<SchemaSnapshot, FoldError> {
        fold_ops(ops, SqlDialect::Postgres, SCHEMA)
    }

    fn col(name: &str, ty: ColType, nullable: bool) -> IrColumn {
        IrColumn {
            name: name.to_string(),
            ty,
            nullable: Some(nullable),
            default: None,
            unique: None,
        }
    }

    fn create(name: &str, columns: Vec<IrColumn>) -> Op {
        Op::CreateTable {
            name: name.to_string(),
            columns,
            constraints: Vec::new(),
            indexes: Vec::new(),
            schema: None,
            existence_guard: None,
        }
    }

    /// A folded table carries the platform system columns (`id`, timestamps, …)
    /// PLUS the user columns — proving the fold routes through the SHARED builder
    /// (`build_table_snapshot` injects the system fields), not a re-implementation.
    #[test]
    fn create_table_injects_system_fields_via_shared_builder() {
        let snap = fold(&[create("users", vec![col("email", ColType::Text, false)])]).unwrap();
        let t = snap.tables.get("users").expect("users table folded");
        let names: Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"id"), "system `id` column injected by the shared builder");
        assert!(names.contains(&"email"), "user column present");
        // The `<table>_pkey` PK constraint the shared builder injects.
        assert!(
            t.constraints.iter().any(|c| c.name == "users_pkey" && c.kind == "PRIMARY KEY"),
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
        assert!(snap.tables.is_empty(), "dropped table removed from the fold");
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
        assert!(t.columns.iter().any(|c| c.name == "score"), "added column present");
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
            FoldError::DuplicateColumn { table: "users".to_string(), column: "name".to_string() }
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
            FoldError::MissingColumn { table: "users".to_string(), column: "ghost".to_string() }
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
        assert!(t.columns.iter().any(|c| c.name == "handle"), "renamed-to column present");
        assert!(!t.columns.iter().any(|c| c.name == "nickname"), "old name gone");
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
        let m = renamed.tables["users"].columns.iter().find(|c| c.name == "m").unwrap();
        assert_eq!(m.data_type, int_type, "rename keeps the existing column type");
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
            FoldError::MissingColumn { table: "users".to_string(), column: "ghost".to_string() }
        );
    }

    #[test]
    fn rename_to_existing_errors() {
        let err = fold(&[
            create("users", vec![col("a", ColType::Text, true), col("b", ColType::Text, true)]),
            rename("users", "a", "b"),
        ])
        .unwrap_err();
        assert_eq!(err, FoldError::RenameCollision { table: "users".to_string(), to: "b".to_string() });
    }

    #[test]
    fn alter_column_type_rederives_data_type() {
        let snap = fold(&[
            create("users", vec![col("n", ColType::Int, false)]),
            Op::AlterColumnType {
                table: "users".to_string(),
                column: "n".to_string(),
                ty: ColType::Text,
                using: None,
                schema: None,
                existence_guard: None,
            },
        ])
        .unwrap();
        let n = snap.tables["users"].columns.iter().find(|c| c.name == "n").unwrap();
        let text_n = fold(&[create("users", vec![col("n", ColType::Text, false)])]).unwrap();
        let want = text_n.tables["users"].columns.iter().find(|c| c.name == "n").unwrap();
        assert_eq!(n.data_type, want.data_type, "alterColumnType re-derives the new data_type");
        assert!(!n.nullable, "alterColumnType keeps existing nullability");
    }

    #[test]
    fn alter_column_type_missing_column_errors() {
        let err = fold(&[
            create("users", vec![col("n", ColType::Int, true)]),
            Op::AlterColumnType {
                table: "users".to_string(),
                column: "ghost".to_string(),
                ty: ColType::Text,
                using: None,
                schema: None,
                existence_guard: None,
            },
        ])
        .unwrap_err();
        assert_eq!(
            err,
            FoldError::MissingColumn { table: "users".to_string(), column: "ghost".to_string() }
        );
    }

    #[test]
    fn alter_column_nullability_flips() {
        let snap = fold(&[
            create("users", vec![col("n", ColType::Int, false)]),
            Op::AlterColumnNullability {
                table: "users".to_string(),
                column: "n".to_string(),
                nullable: true,
                schema: None,
                existence_guard: None,
            },
        ])
        .unwrap();
        let n = snap.tables["users"].columns.iter().find(|c| c.name == "n").unwrap();
        assert!(n.nullable, "alterColumnNullability set NULL");
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
        let c = named.tables["users"].constraints.iter().find(|c| c.name == "u_handle").unwrap();
        assert_eq!(c.kind, "UNIQUE");
        assert_eq!(c.definition, "UNIQUE (\"handle\")");

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
            derived.tables["users"].constraints.iter().any(|c| c.name == "users_handle_key"),
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
            FoldError::MissingConstraint { table: "users".to_string(), name: "ghost".to_string() }
        );
    }

    #[test]
    fn add_check_constraint_is_deferred() {
        let chk = IrConstraint {
            name: Some("age_pos".to_string()),
            kind: IrConstraintKind::Check {
                expr: Expr::Literal { value: IrScalar::Bool(true) },
            },
        };
        let err = fold(&[
            create("users", vec![col("age", ColType::Int, false)]),
            Op::AddConstraint {
                table: "users".to_string(),
                constraint: chk,
                schema: None,
                existence_guard: None,
            },
        ])
        .unwrap_err();
        assert_eq!(err, FoldError::ExprDeferred("addConstraint(check)"));
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
        let c = snap.tables["members"].constraints.iter().find(|c| c.name == "m_team_fk").unwrap();
        assert_eq!(c.kind, "FOREIGN KEY");
        assert!(c.definition.contains("ON DELETE CASCADE"), "FK definition carries ON DELETE: {}", c.definition);
        assert!(c.definition.contains("teams"), "FK references the target table: {}", c.definition);
    }

    fn create_index(table: &str, name: Option<&str>, cols: &[&str], unique: bool) -> Op {
        Op::CreateIndex {
            table: table.to_string(),
            columns: cols.iter().map(ToString::to_string).collect(),
            name: name.map(ToString::to_string),
            unique: Some(unique),
            using: None,
            r#where: None,
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
        let idx = snap.tables["users"].indexes.iter().find(|i| i.name == "u_email_idx").unwrap();
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
            !snap.tables["users"].indexes.iter().any(|i| i.name == "u_email_idx"),
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
                rows: vec![vec![IrScalar::Str("alice".to_string())]],
                on_conflict: None,
                schema: None,
            },
            Op::Delete {
                table: "users".to_string(),
                r#where: Expr::Literal { value: IrScalar::Bool(true) },
                limit: None,
                schema: None,
            },
        ])
        .unwrap();
        assert_eq!(with_dml, schema_only, "DML ops leave the folded schema unchanged");
    }

    #[test]
    fn create_table_level_unique_fk_and_index_fold() {
        // Mirrors the PG oracle corpus's table-level specs (a named UNIQUE + a
        // single-`id` FK + an extra index) — proves they fold onto the snapshot.
        let teams = create("teams", vec![col("label", ColType::Text, false)]);
        let memberships = Op::CreateTable {
            name: "memberships".to_string(),
            columns: vec![col("team_id", ColType::Text, false), col("slot", ColType::Text, false)],
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
                    },
                },
            ],
            indexes: vec![IrIndex {
                name: Some("m_team_idx".to_string()),
                columns: vec!["team_id".to_string()],
                unique: None,
                using: None,
                r#where: None,
            }],
            schema: None,
            existence_guard: None,
        };
        let snap = fold(&[teams, memberships]).unwrap();
        let t = &snap.tables["memberships"];
        assert!(t.constraints.iter().any(|c| c.name == "m_slot_uq" && c.kind == "UNIQUE"));
        assert!(t.constraints.iter().any(|c| c.name == "m_team_fk" && c.kind == "FOREIGN KEY"));
        assert!(t.indexes.iter().any(|i| i.name == "m_team_idx"));
    }

    /// PURITY: `fold_ops` is a plain synchronous `fn` — it takes NO DSN / `Client`,
    /// runs OUTSIDE any compio runtime, and opens no connection. This test is a
    /// non-async `#[test]` (no `#[compio::test]`): it executes a representative fold
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

    #[test]
    fn create_table_user_pk_is_unsupported() {
        let pk = IrConstraint {
            name: None,
            kind: IrConstraintKind::Pk { columns: vec!["a".to_string(), "b".to_string()] },
        };
        let op = Op::CreateTable {
            name: "t".to_string(),
            columns: vec![col("a", ColType::Text, false), col("b", ColType::Text, false)],
            constraints: vec![pk],
            indexes: Vec::new(),
            schema: None,
            existence_guard: None,
        };
        let err = fold(&[op]).unwrap_err();
        assert!(matches!(err, FoldError::Unsupported(m) if m.contains("PRIMARY KEY")));
    }
}
