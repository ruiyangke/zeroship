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
    build_table_snapshot, constraintdef_cols, ir_fk_constraint_snapshot, CollectionDescriptor,
    DeclarativeError,
};
use crate::drift::{ColumnSnapshot, ConstraintSnapshot, IndexSnapshot, SchemaSnapshot, TableSnapshot};
use crate::ir::{
    ColType, IrColumn, IrConstraint, IrConstraintKind, IrDefault, IrIndex, Op, RefAction,
};
use crate::ir_author::{
    create_index_snapshot, derived_constraint_name, index_method_access, ir_column_to_field,
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
            Op::DropTable { table, .. } => {
                // Remove ONLY the target table. We do NOT cascade-drop FK constraints
                // on OTHER tables that reference it, and that is faithful (not a hole):
                // the lower IGNORES the op's `cascade` flag (`ir_author.rs`
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
                // PG's `ALTER TABLE … DROP COLUMN` AUTO-CASCADES to every dependent
                // index and UNIQUE/FK constraint that references the dropped column
                // (and a multi-column index/constraint that merely PARTIALLY covers
                // it is dropped whole, identically). Live introspection therefore
                // shows none of them after the drop, so the fold MUST mirror the
                // cascade — otherwise a phantom index/constraint survives in the
                // fold and `fold_ops != snapshot_schema(live)`, corrupting P2
                // gen-types and producing permanent phantom drift.
                //
                // (1) Drop every index covering the column. `IndexSnapshot::columns`
                //     is the raw key-column list, so an exact name compare suffices;
                //     a multi-column index partially covering it is dropped too.
                snap.indexes.retain(|i| !i.columns.iter().any(|c| c == column));
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
            Op::RenameColumn { table, from, to, .. } => {
                let snap = table_mut(&mut tables, table)?;
                // A pure rename keeps the column's type/nullable/default/sentinels;
                // only the NAME changes (the IR carries `ty` for the live-rename type
                // reconciliation the lower does, but the fold trusts the EXISTING
                // column's type — a pure rename cannot change type, the same stance
                // `lower_rename` takes: "the live column is the single authoritative
                // type source"). So we do NOT re-derive from `ty`.
                //
                // P2 gen-types BOUNDARY (review LOW finding — tracked, no P1 change):
                // the IR rename lowers to an online expand-contract whose CONTRACT
                // (drop the `from` column) is a SEPARATE later deploy. Between expand
                // and contract, live PG carries BOTH the `from` and `to` columns while
                // this fold (which collapses the rename to the final `to` name) shows
                // only `to`. That divergence is correctly EXCLUDED from the fold==live
                // equality oracle and is acceptable for gen-types (the `to` column
                // exists post-expand), but in the migration-first model the fold is the
                // SOLE source of truth for gen-types — so generated types over a
                // mid-expand migration set reflect the POST-EXPAND logical shape (final
                // `to` name). P2 must add an e2e running gen-types against a live
                // mid-expand DB to confirm reads/writes resolve. No action here.
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
                // Re-derive the new column shape from the new type via the shared
                // builder (so `vector(N)` / `geography(...)` / encrypted-BYTEA
                // spellings match introspection's `canonical_extension_type`). Keep
                // the live `nullable`. The `using` cast is fold-irrelevant (it casts
                // DATA, not shape).
                //
                // FAIL-CLOSED on an encryption-contract change. A plain↔encrypted
                // (or masked) type change rewrites the column's emission contract:
                // its `encryption_sentinel` / `comment_sentinel` — the EXACT fields
                // P2 gen-types reads to drive the AEAD encrypt/decrypt pass. The
                // apply path (`render_alter_column_type`) emits ONLY `ALTER COLUMN
                // … TYPE bytea`, never the `COMMENT ON COLUMN … zsenc` an encrypted
                // column needs, so the LIVE DB also lacks the metadata after such an
                // alter. Folding only `data_type` here would carry the OLD (now
                // wrong / stale) sentinel — a silently-wrong snapshot, which P1
                // deliverable 2 forbids. Until the apply path can faithfully
                // re-stamp the sentinel, refuse the change (parity with the lower's
                // `using` / SQLite alter refusals). Detection is symmetric:
                //   - the TARGET type carries a sentinel (plain→encrypted/masked), OR
                //   - the SOURCE column carries one (encrypted/masked→anything).
                let new_col =
                    add_column_snapshot(table, column, ty, None, None, project_schema, dialect)?;
                let target_has_sentinel = new_col.encryption_sentinel.is_some()
                    || new_col.comment_sentinel.is_some();
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
                        "alterColumnType to/from an encrypted (or masked) column \
                         (the apply path cannot re-stamp the zsenc/zsmask sentinel; \
                         fail-closed rather than fold a stale encryption contract)",
                    ));
                }
                col.data_type = new_col.data_type;
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
                // Build the constraint (+ its implicit index for UNIQUE/PK, which PG
                // MATERIALIZES and live introspection reports) the SAME way the lower's
                // snapshot half does: FK via the shared `ir_fk_*`, UNIQUE/PK with a
                // `pg_get_constraintdef`-matching body + the implicit unique index PG
                // names after the constraint, CHECK deferred. Verify the target table
                // FIRST (fail-closed) before stamping.
                let folded = add_constraint_snapshot(table, project_schema, constraint)?;
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
/// `build_table_snapshot`-built [`TableSnapshot`].
///
/// This fold and the lower ([`IrAuthor::fold_create_table_specs`]) agree on every
/// constraint/index NAME and on the UNIQUE definition body (both route through the
/// shared `constraintdef_cols` speller), so an op-authored table re-diffs clean
/// against the apply path. They DELIBERATELY differ on one point — NOT "byte
/// identical": for a table-level UNIQUE the fold ALSO materializes the implicit
/// unique index, whereas the lower pushes only the `ConstraintSnapshot`
/// (`ir_author.rs` ~2057-2070, no index). The reason is that the two snapshots
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
/// - a user composite / per-column PRIMARY KEY is refused (the platform owns the
///   synthetic `id` PK — a second PK is never satisfiable);
/// - a CHECK is refused with [`FoldError::ExprDeferred`] (closed-AST predicate);
/// - a multi-column / non-`id`-referencing FK is refused;
/// - a partial-index `where` is refused (closed-AST predicate);
/// - on **SQLite**, a table-level FOREIGN KEY, a table-level UNIQUE, and a
///   non-btree index `using` are refused — BYTE-FOR-BYTE parity with the lower
///   ([`crate::ir_author::IrAuthor::fold_create_table_specs`]).
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
    let is_sqlite = matches!(dialect, SqlDialect::Sqlite);
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
                if is_sqlite {
                    return Err(FoldError::Unsupported(
                        "createTable table-level FOREIGN KEY on SQLite (the SQLite \
                         CREATE renders from the descriptor; a table-level FK is not \
                         threaded into the emitter)",
                    ));
                }
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
                // A FOREIGN KEY materializes no index.
                push_folded_constraint(table, snap, FoldedConstraint { constraint: fk, index: None })?;
            }
            IrConstraintKind::Unique { columns } => {
                if is_sqlite {
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
        }
    }
    for ix in indexes {
        if ix.r#where.is_some() {
            return Err(FoldError::ExprDeferred("createTable index where"));
        }
        let access = ix.using.map_or("btree", index_method_access);
        if is_sqlite && access != "btree" {
            return Err(FoldError::Unsupported(
                "createTable non-btree index `using` on SQLite (later wave)",
            ));
        }
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
    if snap.constraints.iter().any(|c| c.name == folded.constraint.name) {
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
        },
        index: Some(IndexSnapshot::btree(name.to_string(), true, columns.to_vec())),
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
) -> Result<FoldedConstraint, FoldError> {
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
            Ok(FoldedConstraint {
                constraint: ir_fk_constraint_snapshot(
                    project_schema,
                    name,
                    local,
                    references_table,
                    on_delete.map(RefAction::as_token),
                    on_update.map(RefAction::as_token),
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
        IrConstraintKind::Pk { .. } => {
            // Byte-for-byte parity with the createTable Pk refusal
            // (`fold_create_table_specs`): the platform owns the synthetic
            // `<table>_pkey` PK, so a SECOND user PK — NAMED or derived — is never
            // satisfiable. PG errors `multiple primary keys for table not allowed`
            // at apply, so a two-PK snapshot is UNREACHABLE by introspection;
            // accepting it would be fail-OPEN relative to apply (a named user PK
            // would otherwise slip past the DuplicateConstraint net the derived
            // `<table>_pkey` incidentally trips).
            Err(FoldError::Unsupported(
                "addConstraint user PRIMARY KEY (the platform owns the `id` primary key)",
            ))
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
            create("t", vec![col("a", ColType::Text, false), col("b", ColType::Text, true)]),
            create_index("t", Some("t_b_idx"), &["b"], false),
        ])
        .unwrap();
        assert!(
            with_idx.tables["t"].indexes.iter().any(|i| i.name == "t_b_idx"),
            "precondition: index present before the drop"
        );
        let dropped = fold(&[
            create("t", vec![col("a", ColType::Text, false), col("b", ColType::Text, true)]),
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
        assert_eq!(dropped, base, "drop-column-with-index folds back to the bare table");
    }

    #[test]
    fn drop_column_cascades_dependent_unique_constraint_and_index() {
        // PG auto-drops a UNIQUE constraint (AND its implicit index) over a dropped
        // column. Pre-fix the fold retained BOTH, leaving fold != introspect.
        let dropped = fold(&[
            create("t", vec![col("a", ColType::Text, false), col("b", ColType::Text, false)]),
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
        assert_eq!(dropped, base, "drop-column-with-unique folds back to the bare table");
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
            !dropped.tables["members"].constraints.iter().any(|c| c.name == "m_team_fk"),
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
        assert!(t.indexes.iter().any(|i| i.name == "t_c_idx"), "unrelated index kept");
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
            create("t", vec![col("a", ColType::Text, false), col("b", ColType::Text, true)]),
            create_index("t", Some("t_ab_idx"), &["a", "b"], false),
            drop_column("t", "b"),
        ])
        .unwrap();
        assert!(
            !snap.tables["t"].indexes.iter().any(|i| i.name == "t_ab_idx"),
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
        let t = &named.tables["users"];
        let c = t.constraints.iter().find(|c| c.name == "u_handle").unwrap();
        assert_eq!(c.kind, "UNIQUE");
        // `pg_get_constraintdef`-matching: conditional quoting (bare for a safe
        // lowercase ident), NOT unconditional `UNIQUE ("handle")`.
        assert_eq!(c.definition, "UNIQUE (handle)");
        // A UNIQUE constraint also materializes the implicit unique index PG names
        // after the constraint — live introspection reports it, so the fold must too.
        let idx = t.indexes.iter().find(|i| i.name == "u_handle").unwrap();
        assert!(idx.unique, "implicit unique index for the UNIQUE constraint");
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
    fn add_constraint_user_pk_is_refused_fail_closed() {
        // A standalone addConstraint(Pk) — named OR derived — must be REFUSED
        // fail-closed (byte-for-byte parity with the createTable Pk refusal): the
        // platform owns the synthetic `<table>_pkey` PK, so a SECOND PK is never
        // satisfiable. PG errors `multiple primary keys for table not allowed` at
        // apply, so a snapshot with two PKs is UNREACHABLE by introspection — a
        // fail-OPEN the fold's contract forbids. A NAMED user PK (distinct from
        // `<table>_pkey`) must not slip past the DuplicateConstraint net.
        for name in [Some("my_custom_pk"), None] {
            let pk = IrConstraint {
                name: name.map(ToString::to_string),
                kind: IrConstraintKind::Pk { columns: vec!["a".to_string()] },
            };
            let err = fold(&[
                create("t", vec![col("a", ColType::Text, false)]),
                Op::AddConstraint {
                    table: "t".to_string(),
                    constraint: pk,
                    schema: None,
                    existence_guard: None,
                },
            ])
            .unwrap_err();
            assert_eq!(
                err,
                FoldError::Unsupported(
                    "addConstraint user PRIMARY KEY (the platform owns the `id` primary key)"
                ),
                "named={name:?} user PK must be refused, not fold to a two-PK snapshot",
            );
        }
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

    // -----------------------------------------------------------------------
    // Finding #1 (MED) — alterColumnType to/from an encrypted column must NOT
    // silently lose / carry a stale encryption sentinel. The fold fails closed
    // (the apply path cannot re-stamp the zsenc sentinel today).
    // -----------------------------------------------------------------------

    fn encrypted_text() -> ColType {
        ColType::Encrypted { of: Box::new(ColType::Text) }
    }

    fn alter_type(table: &str, column: &str, ty: ColType) -> Op {
        Op::AlterColumnType {
            table: table.to_string(),
            column: column.to_string(),
            ty,
            using: None,
            schema: None,
            existence_guard: None,
        }
    }

    /// A FRESH `t.encrypted(text)` column folds WITH an encryption sentinel (the
    /// shared builder stamps the `zsenc:` contract P2 gen-types reads). This is the
    /// baseline the alter path must preserve — assert the sentinel is present so the
    /// "alter loses it" regression below is meaningful.
    #[test]
    fn fresh_encrypted_column_carries_sentinel() {
        let snap = fold(&[create("v", vec![col("secret", encrypted_text(), true)])]).unwrap();
        let c = snap.tables["v"].columns.iter().find(|c| c.name == "secret").unwrap();
        assert!(
            c.encryption_sentinel.is_some() || c.comment_sentinel.is_some(),
            "a fresh encrypted column carries the zsenc sentinel (the P2 contract)"
        );
    }

    /// REGRESSION (Finding #1): plain→encrypted via `alterColumnType` is FAIL-CLOSED.
    /// Pre-fix the fold transplanted ONLY `data_type` (bytea), keeping the OLD
    /// `encryption_sentinel=None` — so the folded encrypted column carried NO
    /// sentinel (a silently-wrong snapshot, since the oracle excludes the sentinel
    /// from Eq). The apply path likewise never emits the `COMMENT … zsenc`, so live
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
            "plain→encrypted alterColumnType must fail closed, got {err:?}"
        );
    }

    /// REGRESSION (Finding #1, symmetric): encrypted→plain via `alterColumnType` is
    /// also FAIL-CLOSED. The SOURCE column carries the sentinel; transplanting only
    /// `data_type` would leave the now-stale `zsenc` sentinel on a plaintext column.
    #[test]
    fn alter_column_type_from_encrypted_is_unsupported() {
        let err = fold(&[
            create("v", vec![col("secret", encrypted_text(), true)]),
            alter_type("v", "secret", ColType::Text),
        ])
        .unwrap_err();
        assert!(
            matches!(err, FoldError::Unsupported(m) if m.contains("encrypted")),
            "encrypted→plain alterColumnType must fail closed, got {err:?}"
        );
    }

    /// A PLAIN→PLAIN `alterColumnType` (neither side encrypted) still works — the
    /// fail-closed guard is scoped to the encryption-contract change only.
    #[test]
    fn alter_column_type_plain_to_plain_still_folds() {
        let snap = fold(&[
            create("v", vec![col("n", ColType::Int, false)]),
            alter_type("v", "n", ColType::BigInt),
        ])
        .unwrap();
        let n = snap.tables["v"].columns.iter().find(|c| c.name == "n").unwrap();
        let want = fold(&[create("v", vec![col("n", ColType::BigInt, false)])]).unwrap();
        let want_n = want.tables["v"].columns.iter().find(|c| c.name == "n").unwrap();
        assert_eq!(n.data_type, want_n.data_type, "plain→plain re-derives data_type");
    }

    // -----------------------------------------------------------------------
    // Finding #2 (MED) — the fold must mirror the lower's SQLite refusals so it
    // never emits types for a schema that can never deploy on SQLite (fail-OPEN).
    // -----------------------------------------------------------------------

    fn fold_sqlite(ops: &[Op]) -> Result<SchemaSnapshot, FoldError> {
        fold_ops(ops, SqlDialect::Sqlite, SCHEMA)
    }

    fn create_with(name: &str, columns: Vec<IrColumn>, constraints: Vec<IrConstraint>, indexes: Vec<IrIndex>) -> Op {
        Op::CreateTable {
            name: name.to_string(),
            columns,
            constraints,
            indexes,
            schema: None,
            existence_guard: None,
        }
    }

    /// REGRESSION (Finding #2): a createTable TABLE-LEVEL FOREIGN KEY is refused on
    /// SQLite — byte-for-byte parity with the lower (`ir_author.rs`), which never
    /// threads a table-level FK into the SQLite emitter. Pre-fix the fold ACCEPTED
    /// it (fail-open: a schema the engine can never apply on SQLite).
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
                },
            }],
            Vec::new(),
        );
        let err = fold_sqlite(&[parents, kids]).unwrap_err();
        assert!(
            matches!(err, FoldError::Unsupported(m) if m.contains("FOREIGN KEY") && m.contains("SQLite")),
            "table-level FK on SQLite must fail closed, got {err:?}"
        );
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
                },
            }],
            Vec::new(),
        );
        assert!(fold(&[parents, kids]).is_ok(), "table-level FK folds on Postgres");
    }

    /// REGRESSION (Finding #2): a createTable TABLE-LEVEL UNIQUE is refused on SQLite.
    #[test]
    fn create_table_level_unique_unsupported_on_sqlite() {
        let op = create_with(
            "t",
            vec![col("handle", ColType::Text, false)],
            vec![unique_constraint(Some("t_handle_uq"), &["handle"])],
            Vec::new(),
        );
        let err = fold_sqlite(&[op]).unwrap_err();
        assert!(
            matches!(err, FoldError::Unsupported(m) if m.contains("UNIQUE") && m.contains("SQLite")),
            "table-level UNIQUE on SQLite must fail closed, got {err:?}"
        );
    }

    /// REGRESSION (Finding #2): a createTable non-btree index `using` is refused on
    /// SQLite.
    #[test]
    fn create_table_non_btree_index_using_unsupported_on_sqlite() {
        let op = create_with(
            "t",
            vec![col("doc", ColType::Json, false)],
            Vec::new(),
            vec![IrIndex {
                name: Some("t_doc_idx".to_string()),
                columns: vec!["doc".to_string()],
                unique: None,
                using: Some(crate::ir::IndexMethod::Gin),
                r#where: None,
            }],
        );
        let err = fold_sqlite(&[op]).unwrap_err();
        assert!(
            matches!(err, FoldError::Unsupported(m) if m.contains("non-btree") && m.contains("SQLite")),
            "non-btree index `using` on SQLite must fail closed, got {err:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Finding #3 (LOW) — the round-trip oracle's ColumnSnapshot Eq excludes
    // default / encryption_sentinel / comment_sentinel, so it structurally CANNOT
    // validate the fold's emission metadata. These NO-DB goldens assert the fold's
    // emitted default / sentinels match build_table_snapshot DIRECTLY (not via the
    // Eq-blind oracle) — the fields P2 gen-types depends on.
    // -----------------------------------------------------------------------

    /// The `ColumnSnapshot` build_table_snapshot produces for ONE field — the ground
    /// truth the fold's emission metadata must match.
    fn builder_column(table: &str, column: &str, ty: ColType, nullable: bool, default: Option<IrDefault>) -> ColumnSnapshot {
        let field = ir_column_to_field(&IrColumn {
            name: column.to_string(),
            ty,
            nullable: Some(nullable),
            default,
            unique: None,
        });
        let desc = CollectionDescriptor {
            name: table.to_string(),
            owner_app: FOLD_OWNER_APP.to_string(),
            fields: vec![field],
            indexes: Vec::new(),
        };
        build_table_snapshot(SCHEMA, &desc, SqlDialect::Postgres)
            .unwrap()
            .columns
            .into_iter()
            .find(|c| c.name == column)
            .unwrap()
    }

    /// GOLDEN (Finding #3): the fold's emitted default + sentinels for a createTable
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
                    default: Some(IrDefault::Literal { value: IrScalar::Str("beta".to_string()) }),
                    unique: None,
                },
                // An `int` column with a literal default — the snapshot's
                // emission-only `default` IS what P2 gen-types reads, so it MUST
                // render (regression: int defaults were silently dropped — the
                // shared `field_default_expr` had no `int` arm).
                IrColumn {
                    name: "rank".to_string(),
                    ty: ColType::Int,
                    nullable: Some(false),
                    default: Some(IrDefault::Literal { value: IrScalar::Int(7) }),
                    unique: None,
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
            Some(IrDefault::Literal { value: IrScalar::Str("beta".to_string()) }),
        );
        assert_eq!(tier.default, want_tier.default, "fold's emitted string default matches the shared builder");
        assert!(want_tier.default.is_some(), "the string default golden is non-trivial");

        let rank = t.columns.iter().find(|c| c.name == "rank").unwrap();
        let want_rank = builder_column(
            "g",
            "rank",
            ColType::Int,
            false,
            Some(IrDefault::Literal { value: IrScalar::Int(7) }),
        );
        assert_eq!(rank.default, want_rank.default, "fold's emitted int default matches the shared builder");
        assert_eq!(
            want_rank.default.as_deref(),
            Some("7"),
            "an int column's default DOES render into the snapshot (regression: it was dropped)"
        );

        let meta = t.columns.iter().find(|c| c.name == "meta").unwrap();
        let want_meta = builder_column("g", "meta", ColType::Json, true, None);
        assert_eq!(meta.default, want_meta.default, "fold's emitted json default matches the shared builder");
        assert!(want_meta.default.is_some(), "the json default golden is non-trivial ('{{}}'::jsonb)");
    }

    /// GOLDEN (Finding #3, addColumn): the fold's emitted default + sentinels for an
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
                schema: None,
                existence_guard: None,
            },
        ])
        .unwrap();
        let secret = snap.tables["g"].columns.iter().find(|c| c.name == "secret").unwrap();
        let want = builder_column("g", "secret", encrypted_text(), true, None);
        assert_eq!(secret.encryption_sentinel, want.encryption_sentinel, "addColumn encryption_sentinel parity");
        assert_eq!(secret.comment_sentinel, want.comment_sentinel, "addColumn comment_sentinel parity");
        assert!(
            want.encryption_sentinel.is_some() || want.comment_sentinel.is_some(),
            "the addColumn encrypted golden is non-trivial"
        );
    }

    // -----------------------------------------------------------------------
    // Finding #4 (LOW) — the fold and the lower must agree on the UNIQUE
    // constraint `definition` body spelling (shared `constraintdef_cols`), so the
    // two copies of the createTable-spec folding cannot drift.
    // -----------------------------------------------------------------------

    /// REGRESSION (Finding #4): the fold and the lower spell the UNIQUE `definition`
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
        let folded = snap.tables["t"].constraints.iter().find(|c| c.name == "t_handle_uq").unwrap();
        // The lower's snapshot half spells it via the SAME shared helper now.
        let cols = vec!["handle".to_string()];
        let lower_body = format!("UNIQUE ({})", crate::declarative::constraintdef_cols(&cols));
        assert_eq!(
            folded.definition, lower_body,
            "fold and lower must spell the UNIQUE definition identically"
        );
        assert_eq!(folded.definition, "UNIQUE (handle)", "bare spelling matches pg_get_constraintdef");
    }
}
