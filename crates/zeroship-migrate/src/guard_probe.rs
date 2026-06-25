//! **PR10 Part B** — executor-side existence-guard catalog probe (§2.7).
//!
//! PR10 (Part A) shipped the existence-guard IR/JS SHAPE in `ir_version 2`
//! (`ExistenceGuard::{IfNotExists,IfExists}` on every DDL op) but DEFERRED guard
//! EXECUTION — every guarded op was REFUSED fail-closed at lower. This module is
//! the execution: a render-time-resolved, dialect-neutral [`GuardProbe`] is stamped
//! onto each lowered [`Migration`](crate::migration::Migration); at apply time the
//! executor reads the LIVE catalog under the ALREADY-HELD project advisory lock +
//! the open per-step transaction, and [`decide`] returns a [`GuardVerdict`]:
//!
//! - [`GuardVerdict::RunBare`] — the guard's precondition is met; run the op's `up`.
//! - [`GuardVerdict::SatisfiedNoop`] — the object already has the declared shape
//!   (`ifNotExists`) or is already absent (`ifExists`); SKIP the `up` but STILL
//!   journal the `completed` row so the version lands (a re-deploy sees it
//!   net-applied and skips it via normal pending computation).
//! - [`GuardVerdict::FailDrift`] — the object EXISTS with a shape that DIVERGES
//!   from the declared one (`ifNotExists`) and cannot be proven equal. This is a
//!   HARD error (`ApplyError::ExistenceGuardDrift`), never a silent skip. The
//!   transaction is rolled back; nothing is applied or journaled.
//!
//! # No-TOCTOU
//! The probe is a `&Client` read issued INSIDE the same open transaction that will
//! run the `up`, under the project advisory lock the whole plan already holds. No
//! lock is acquired or released across probe→decide→act, so there is no window for
//! the catalog to change between the verdict and the action.
//!
//! # Fail-closed shape verification (the point of this module)
//! A guard whose precondition CANNOT be fully proven from the catalog fails CLOSED
//! (`FailDrift`), never optimistically `SatisfiedNoop`. The non-obvious cases:
//!
//! - **Constraint `ifNotExists`** — a present same-name constraint is `FailDrift`,
//!   NOT `SatisfiedNoop`, unless its KIND clashes (also `FailDrift`, with a clearer
//!   `kind` field). The live `pg_get_constraintdef` definition cannot be
//!   byte-compared against the IR's un-normalized constraint body, so a present
//!   constraint's definitional equality cannot be PROVEN — and a same-name +
//!   same-kind constraint with a DIFFERENT predicate (a rewritten CHECK / a
//!   different FK target) is a real divergence the PG catalog DOES expose. We refuse
//!   rather than skip. (The realistic `ifNotExists` use — the constraint is ABSENT —
//!   still `RunBare`.)
//! - **Index `ifNotExists` over an expression / partial predicate** — `FailDrift`
//!   naming `expression`: the live index carries a non-empty `pg_get_expr`
//!   predicate the IR `createIndex` (a column-list AST) cannot render to a
//!   byte-comparable form, so equivalence cannot be proven.
//! - **SQLite TEXT-affinity collision (H1)** — SQLite stores only the TEXT
//!   affinity for a `text`-family column, and the engine's declared snapshot
//!   data_type is the PG spelling (`field_data_type` maps via the PG dialect), so
//!   the facets that collapse to the literal `text` in BOTH the snapshot and the
//!   SQLite live catalog (`string`/`ref`/`actor`/`id` + a string `literal`) become
//!   indistinguishable on the SQLite leg: a same-name column whose true SDK facet
//!   changed within that group (live authored `string` vs declared `ref`) is
//!   INVISIBLE to a plain affinity compare. On the SQLite leg an `ifNotExists`
//!   column/table verify therefore CANNOT prove full-shape equality for a
//!   `text`-affinity column; it fails CLOSED (`FailDrift` naming `data_type`)
//!   rather than `SatisfiedNoop` on an affinity-only match. The probe carries the
//!   un-collapsed declared facet token ([`ExpectColumn::sqlite_text_facet`] /
//!   `GuardProbe::Column::sqlite_text_facet`), set ONLY on the SQLite leg when the
//!   declared snapshot data_type is `text`, to drive this. Facets whose snapshot
//!   spelling is DISTINCT (`json`→`jsonb`, `date`→`timestamp with time zone`) and
//!   the non-TEXT affinities (INTEGER/REAL/BLOB) are unambiguous and compare
//!   exactly (the flag is `None`).
//!
//! The expected `data_type`/`nullable`/`unique`/`columns`/`kind` values are built
//! by the SAME shared snapshot builders the lowering arms call
//! (`build_table_snapshot` / `add_column_snapshot` / `create_index_snapshot` / the
//! addConstraint kind), so they are byte-comparable against the introspected
//! [`SchemaSnapshot`](crate::drift::SchemaSnapshot).

use crate::drift::SchemaSnapshot;
use crate::ir::ExistenceGuard;

/// One declared column's verifiable shape for a `createTable ifNotExists`
/// [`GuardProbe::Table`] probe. Built from the SAME shared snapshot the CREATE
/// renders from, so the `data_type`/`nullable` strings are byte-comparable against
/// introspection.
///
/// `sqlite_text_facet` carries the **un-collapsed SDK facet token** (e.g. `date`,
/// `json`, `ref`) and is `Some` ONLY when the authoring dialect is SQLite AND this
/// column collapses to the ambiguous **TEXT affinity** (string/date/json/ref → the
/// literal `TEXT`). The SQLite catalog stores only the affinity (`text`), so a
/// same-name column whose SDK facet changed WITHIN the TEXT affinity (live `string`
/// vs declared `date`) is INVISIBLE to a plain affinity compare. When this is
/// `Some`, [`column_shape_divergence`] fails CLOSED (`FailDrift` naming
/// `data_type`) on a TEXT-affinity column rather than optimistically
/// `SatisfiedNoop`-ing — matching the module's documented fail-closed contract. On
/// PG (and for non-TEXT SQLite affinities — INTEGER/REAL/BLOB, which are
/// unambiguous) it is `None` and the plain affinity compare is already
/// facet-precise.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExpectColumn {
    /// Column name.
    pub name: String,
    /// The introspectable data-type spelling (PG type / SQLite affinity).
    pub data_type: String,
    /// Declared nullability.
    pub nullable: bool,
    /// The un-collapsed SDK facet token for a SQLite TEXT-affinity column; `None`
    /// otherwise (see the type docs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sqlite_text_facet: Option<String>,
}

/// The guard DIRECTION carried on a probe (a 1:1 copy of [`ExistenceGuard`], kept
/// local so the probe module owns its decision vocabulary; `ExistenceGuard` is
/// `Copy` and converts in via [`From`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GuardDir {
    /// Run only if the target object is ABSENT (`create*`/`add*`).
    IfNotExists,
    /// Run only if the target object is PRESENT (`drop*`/`rename`/`alter*`).
    IfExists,
}

impl From<ExistenceGuard> for GuardDir {
    fn from(g: ExistenceGuard) -> Self {
        match g {
            ExistenceGuard::IfNotExists => GuardDir::IfNotExists,
            ExistenceGuard::IfExists => GuardDir::IfExists,
        }
    }
}

/// A render-time-resolved, dialect-neutral descriptor of WHAT to probe and WHICH
/// shape to verify. Built in `lower_one_op` from the op (which already has the
/// columns/type/nullable/eff_schema in hand) and stamped onto each lowered
/// [`Migration`](crate::migration::Migration). NOT folded into the migration
/// checksum (the IR-path anchor `Checksum::of_ir` over the op-list already covers
/// the guard); see `migration.rs`.
///
/// Derives `Serialize`/`Deserialize` only so `Migration` can keep its derives —
/// the field is `skip_serializing_if = "Option::is_none"`, so the on-disk `.sql`
/// / golden wire is unchanged (it is never set on those paths) and the in-memory
/// plan round-trips.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GuardProbe {
    /// `createTable ifNotExists` (`expect_columns` = the declared
    /// `(name, data_type, nullable)`) or `dropTable ifExists` (empty
    /// `expect_columns` — presence-only).
    Table {
        /// The effective schema the table lives in.
        schema: String,
        /// The table name.
        table: String,
        /// Guard direction.
        direction: GuardDir,
        /// The declared columns to shape-verify (`ifNotExists`); empty for the
        /// presence-only `ifExists` drop.
        expect_columns: Vec<ExpectColumn>,
    },
    /// `addColumn ifNotExists` (`expect` = `Some((data_type, nullable))`) or
    /// `dropColumn ifExists` (`expect` = `None`, presence-only).
    Column {
        /// The effective schema.
        schema: String,
        /// The table the column belongs to.
        table: String,
        /// The column name.
        column: String,
        /// Guard direction.
        direction: GuardDir,
        /// The declared `(data_type, nullable)` to verify (`ifNotExists`); `None`
        /// for the presence-only `ifExists` drop.
        expect: Option<(String, bool)>,
        /// The un-collapsed SDK facet token for a SQLite TEXT-affinity column (see
        /// [`ExpectColumn::sqlite_text_facet`]); `None` on PG / non-TEXT affinity /
        /// the presence-only `ifExists` drop. When `Some`, a present TEXT-affinity
        /// column fails CLOSED rather than `SatisfiedNoop` (H1 fail-closed).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sqlite_text_facet: Option<String>,
    },
    /// `createIndex ifNotExists` (`expect` = `Some((unique, columns))`) or
    /// `dropIndex ifExists` (`expect` = `None`, presence-only).
    Index {
        /// The effective schema.
        schema: String,
        /// The table the index covers.
        table: String,
        /// The index name.
        name: String,
        /// Guard direction.
        direction: GuardDir,
        /// The declared `(unique, columns)` to verify (`ifNotExists`); `None` for
        /// the presence-only `ifExists` drop.
        expect: Option<(bool, Vec<String>)>,
    },
    /// `addConstraint … ifNotExists` (`expect_kind` = the declared catalog kind:
    /// `"PRIMARY KEY"` / `"FOREIGN KEY"` / `"UNIQUE"` / `"CHECK"`) or
    /// `dropConstraint ifExists` (`expect_kind` = `None`, presence-only).
    Constraint {
        /// The effective schema.
        schema: String,
        /// The table the constraint is on.
        table: String,
        /// The constraint name.
        name: String,
        /// Guard direction.
        direction: GuardDir,
        /// The declared catalog kind to compare (`ifNotExists`); `None` for the
        /// presence-only `ifExists` drop. NOTE: a PRESENT same-name constraint
        /// under `ifNotExists` is `FailDrift` even on a kind MATCH — the live
        /// `pg_get_constraintdef` body cannot be proven equal to the IR's
        /// un-normalized constraint, so we refuse rather than skip (see the module
        /// docs).
        expect_kind: Option<String>,
    },
    /// `alterColumnType` / `alterColumnNullability` / `renameColumn` `ifExists`:
    /// the SOURCE column must EXIST (presence-only — an alter/rename intentionally
    /// CHANGES the shape, so there is no shape to verify).
    ColumnPresence {
        /// The effective schema.
        schema: String,
        /// The table the column belongs to.
        table: String,
        /// The source column name.
        column: String,
        /// Guard direction (always `IfExists` for this variant).
        direction: GuardDir,
    },
}

impl GuardProbe {
    /// The effective schema this probe reads — the `snapshot_schema` argument the
    /// executor passes so the catalog read targets the op's schema.
    #[must_use]
    pub fn schema(&self) -> &str {
        match self {
            GuardProbe::Table { schema, .. }
            | GuardProbe::Column { schema, .. }
            | GuardProbe::Index { schema, .. }
            | GuardProbe::Constraint { schema, .. }
            | GuardProbe::ColumnPresence { schema, .. } => schema,
        }
    }
}

/// A single same-name object whose shape DIVERGES from the declared one — the
/// payload of [`GuardVerdict::FailDrift`]. Names + values only, never DDL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    /// The object the divergence is on, e.g. `table users`, `column users.email`,
    /// `index users_email_idx`, `constraint users_age_chk`.
    pub object: String,
    /// The attribute that diverged: `data_type`, `nullable`, `unique`, `columns`,
    /// `expression`, `kind`, or `definition`.
    pub field: String,
    /// The DECLARED (expected) value for `field`.
    pub expected: String,
    /// The LIVE value for `field`.
    pub actual: String,
}

/// The executor's decision for a guarded op, computed in Rust from the live
/// catalog snapshot — NEVER a SQL-level conditional.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardVerdict {
    /// Run the op's `up` bare (the guard's precondition is met).
    RunBare,
    /// Skip the `up` but journal the `completed` row (the declared shape is already
    /// present for `ifNotExists`, or the object is already absent for `ifExists`).
    SatisfiedNoop,
    /// The object exists with a divergent / unprovable shape — fail closed.
    FailDrift(Divergence),
}

/// Decide the verdict for `probe` against the LIVE catalog `live`. Pure (no DB
/// access — the caller has already taken the snapshot under the held lock + open
/// txn). See the module docs for the per-variant fail-closed rules.
#[must_use]
pub fn decide(probe: &GuardProbe, live: &SchemaSnapshot) -> GuardVerdict {
    match probe {
        GuardProbe::Table { table, direction, expect_columns, .. } => {
            decide_table(table, *direction, expect_columns, live)
        }
        GuardProbe::Column { table, column, direction, expect, sqlite_text_facet, .. } => {
            decide_column(
                table,
                column,
                *direction,
                expect.as_ref(),
                sqlite_text_facet.as_deref(),
                live,
            )
        }
        GuardProbe::Index { table, name, direction, expect, .. } => {
            decide_index(table, name, *direction, expect.as_ref(), live)
        }
        GuardProbe::Constraint { table, name, direction, expect_kind, .. } => {
            decide_constraint(table, name, *direction, expect_kind.as_deref(), live)
        }
        GuardProbe::ColumnPresence { table, column, .. } => {
            // Always IfExists. Source column must EXIST → RunBare; absent → Noop.
            if column_present(live, table, column) {
                GuardVerdict::RunBare
            } else {
                GuardVerdict::SatisfiedNoop
            }
        }
    }
}

fn decide_table(
    table: &str,
    direction: GuardDir,
    expect_columns: &[ExpectColumn],
    live: &SchemaSnapshot,
) -> GuardVerdict {
    let present = live.tables.contains_key(table);
    match direction {
        GuardDir::IfExists => {
            // dropTable: presence-only.
            if present { GuardVerdict::RunBare } else { GuardVerdict::SatisfiedNoop }
        }
        GuardDir::IfNotExists => {
            let Some(t) = live.tables.get(table) else {
                return GuardVerdict::RunBare; // absent → create it.
            };
            // Present: prove EXACT shape equality of the declared columns. A missing
            // declared column, a `(data_type, nullable)` divergence, OR any EXTRA
            // live column not in the declared set → FailDrift (a wider live table is
            // not the declared shape — fail closed).
            for ec in expect_columns {
                match t.columns.iter().find(|c| c.name == ec.name) {
                    None => {
                        return drift(
                            &format!("table {table}"),
                            "data_type",
                            &format!("{}: {}", ec.name, ec.data_type),
                            &format!("{}: <absent>", ec.name),
                        );
                    }
                    Some(live_col) => {
                        if let Some(v) = column_shape_divergence(
                            table,
                            &ec.name,
                            &ec.data_type,
                            ec.nullable,
                            ec.sqlite_text_facet.as_deref(),
                            &live_col.data_type,
                            live_col.nullable,
                        ) {
                            return v;
                        }
                    }
                }
            }
            // Extra live column → FailDrift (the live table is wider than declared).
            for live_col in &t.columns {
                if !expect_columns.iter().any(|ec| ec.name == live_col.name) {
                    return drift(
                        &format!("table {table}"),
                        "columns",
                        "<declared column set>",
                        &format!("extra live column {}", live_col.name),
                    );
                }
            }
            GuardVerdict::SatisfiedNoop
        }
    }
}

fn decide_column(
    table: &str,
    column: &str,
    direction: GuardDir,
    expect: Option<&(String, bool)>,
    sqlite_text_facet: Option<&str>,
    live: &SchemaSnapshot,
) -> GuardVerdict {
    let present = column_present(live, table, column);
    match direction {
        GuardDir::IfExists => {
            // dropColumn: presence-only.
            if present { GuardVerdict::RunBare } else { GuardVerdict::SatisfiedNoop }
        }
        GuardDir::IfNotExists => {
            if !present {
                return GuardVerdict::RunBare; // absent → add it.
            }
            // Present: verify (data_type, nullable). `expect` is always Some on the
            // addColumn ifNotExists path; if it is somehow None (a presence-only
            // ifNotExists we never build), fail closed — we cannot prove the shape.
            let Some((dtype, nullable)) = expect else {
                return drift(
                    &format!("column {table}.{column}"),
                    "data_type",
                    "<declared but unverifiable>",
                    "<present>",
                );
            };
            let live_col = live
                .tables
                .get(table)
                .and_then(|t| t.columns.iter().find(|c| c.name == column));
            let Some(live_col) = live_col else {
                // column_present said present but the column vanished — fail closed.
                return drift(
                    &format!("column {table}.{column}"),
                    "data_type",
                    dtype,
                    "<absent>",
                );
            };
            column_shape_divergence(
                table,
                column,
                dtype,
                *nullable,
                sqlite_text_facet,
                &live_col.data_type,
                live_col.nullable,
            )
            .unwrap_or(GuardVerdict::SatisfiedNoop)
        }
    }
}

/// Compare a declared column shape against the live one. Returns a `FailDrift`
/// verdict on a divergence, or `None` if they match exactly.
///
/// **SQLite TEXT-affinity fail-closed (H1)**: the engine's declared snapshot
/// data_type is the PG spelling, and the facets that collapse to the literal `text`
/// in BOTH the snapshot and the SQLite live catalog (`string`/`ref`/`actor`/`id` +
/// a string `literal`) are indistinguishable on SQLite — the live catalog stores
/// only the affinity (`text`), the un-collapsed SDK facet is NOT recoverable. So a
/// same-name column whose SDK facet changed within that group (live authored
/// `string` vs declared `ref`) is INVISIBLE to a plain affinity compare: both spell
/// `text`, the plain `expect_dtype != live_dtype` branch passes, and we would
/// SILENTLY `SatisfiedNoop` over a divergent column.
///
/// `sqlite_text_facet` is `Some(<declared SDK facet>)` ONLY when the authoring
/// dialect is SQLite AND this column's declared snapshot data_type is `text`. When
/// it is `Some` and the affinity tokens MATCH (`text` == `text`), we CANNOT prove
/// the full SDK facet is equal from the catalog, so we fail CLOSED (`FailDrift`
/// naming `data_type`, carrying the declared facet as `expected`) rather than
/// optimistically matching — exactly the module's documented contract. On PG (the
/// live type IS the distinct facet), for facets whose snapshot spelling is distinct
/// (`json`→`jsonb`, `date`→`timestamp with time zone`), and for non-TEXT SQLite
/// affinities (INTEGER/REAL/BLOB — unambiguous), `sqlite_text_facet` is `None` and
/// the plain affinity compare is already facet-precise. A genuine `text` != `text`
/// mismatch still trips the plain inequality branch above regardless.
fn column_shape_divergence(
    table: &str,
    column: &str,
    expect_dtype: &str,
    expect_nullable: bool,
    sqlite_text_facet: Option<&str>,
    live_dtype: &str,
    live_nullable: bool,
) -> Option<GuardVerdict> {
    if expect_dtype != live_dtype {
        return Some(drift(
            &format!("column {table}.{column}"),
            "data_type",
            expect_dtype,
            live_dtype,
        ));
    }
    // **H1** — the affinity tokens MATCH, but on the SQLite leg a TEXT-affinity
    // column's true SDK facet is NOT introspectable: a live `string` and a declared
    // `date` BOTH read back as `text`. We cannot PROVE the facet is equal from the
    // catalog, so fail closed (matching the module-doc contract) rather than noop.
    if let Some(facet) = sqlite_text_facet {
        return Some(drift(
            &format!("column {table}.{column}"),
            "data_type",
            // The declared facet (un-collapsed) makes the message precise; the live
            // side is the affinity the catalog reports — the full facet is unprovable.
            &format!("{facet} (TEXT affinity — SDK facet unprovable from SQLite catalog)"),
            &format!("{live_dtype} (affinity only)"),
        ));
    }
    if expect_nullable != live_nullable {
        return Some(drift(
            &format!("column {table}.{column}"),
            "nullable",
            &expect_nullable.to_string(),
            &live_nullable.to_string(),
        ));
    }
    None
}

fn decide_index(
    table: &str,
    name: &str,
    direction: GuardDir,
    expect: Option<&(bool, Vec<String>)>,
    live: &SchemaSnapshot,
) -> GuardVerdict {
    // Look up the index under the table hint first; for the presence-only
    // `ifExists` (dropIndex) path the table hint may be absent/empty, so fall back
    // to scanning ALL tables for the index name (indexes are unique per schema).
    let live_idx = live
        .tables
        .get(table)
        .and_then(|t| t.indexes.iter().find(|i| i.name == name))
        .or_else(|| live.tables.values().flat_map(|t| &t.indexes).find(|i| i.name == name));
    match direction {
        GuardDir::IfExists => {
            // dropIndex: presence-only on the index name.
            if live_idx.is_some() { GuardVerdict::RunBare } else { GuardVerdict::SatisfiedNoop }
        }
        GuardDir::IfNotExists => {
            let Some(live_idx) = live_idx else {
                return GuardVerdict::RunBare; // absent → create it.
            };
            // Present: prove (unique, columns) equality. An expression / partial
            // index fails closed (`expression`): the IR createIndex carries a
            // column-list (no rendered `pg_get_expr` form) so equivalence cannot be
            // proven.
            if live_idx.expression.is_some() {
                return drift(
                    &format!("index {name}"),
                    "expression",
                    "<plain column-list index>",
                    "<expression/partial index — cannot prove equivalence>",
                );
            }
            let Some((unique, columns)) = expect else {
                return drift(
                    &format!("index {name}"),
                    "unique",
                    "<declared but unverifiable>",
                    "<present>",
                );
            };
            if *unique != live_idx.unique {
                return drift(
                    &format!("index {name}"),
                    "unique",
                    &unique.to_string(),
                    &live_idx.unique.to_string(),
                );
            }
            if columns != &live_idx.columns {
                return drift(
                    &format!("index {name}"),
                    "columns",
                    &columns.join(","),
                    &live_idx.columns.join(","),
                );
            }
            GuardVerdict::SatisfiedNoop
        }
    }
}

fn decide_constraint(
    table: &str,
    name: &str,
    direction: GuardDir,
    expect_kind: Option<&str>,
    live: &SchemaSnapshot,
) -> GuardVerdict {
    let live_con = live
        .tables
        .get(table)
        .and_then(|t| t.constraints.iter().find(|c| c.name == name));
    match direction {
        GuardDir::IfExists => {
            // dropConstraint: presence-only on the constraint name.
            if live_con.is_some() { GuardVerdict::RunBare } else { GuardVerdict::SatisfiedNoop }
        }
        GuardDir::IfNotExists => {
            let Some(live_con) = live_con else {
                return GuardVerdict::RunBare; // absent → add it.
            };
            // Present. A kind clash is the clearest divergence.
            if let Some(kind) = expect_kind {
                if kind != live_con.kind {
                    return drift(
                        &format!("constraint {name}"),
                        "kind",
                        kind,
                        &live_con.kind,
                    );
                }
            }
            // **MED finding (fail-closed over a catalog-exposed divergence).** A
            // same-name + same-kind constraint is NOT a SatisfiedNoop: the live
            // `pg_get_constraintdef` definition cannot be byte-compared against the
            // IR's un-normalized constraint body, so we cannot PROVE the predicate
            // (a rewritten CHECK, a different FK target / column / ON-DELETE) is
            // equal. A same-name + same-kind + DIFFERENT-definition constraint is a
            // real divergence the PG catalog exposes via `pg_get_constraintdef`, so
            // we refuse rather than skip. The realistic `ifNotExists` use (the
            // constraint is ABSENT) still RunBare above.
            drift(
                &format!("constraint {name}"),
                "definition",
                "<declared constraint — cannot prove equal to live pg_get_constraintdef>",
                if live_con.definition.is_empty() { "<present>" } else { &live_con.definition },
            )
        }
    }
}

/// Whether `table.column` exists in the live snapshot.
fn column_present(live: &SchemaSnapshot, table: &str, column: &str) -> bool {
    live.tables
        .get(table)
        .is_some_and(|t| t.columns.iter().any(|c| c.name == column))
}

/// Build a `FailDrift` verdict.
fn drift(object: &str, field: &str, expected: &str, actual: &str) -> GuardVerdict {
    GuardVerdict::FailDrift(Divergence {
        object: object.to_string(),
        field: field.to_string(),
        expected: expected.to_string(),
        actual: actual.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drift::{ColumnSnapshot, ConstraintSnapshot, IndexSnapshot, TableSnapshot};
    use std::collections::BTreeMap;

    fn col(name: &str, dtype: &str, nullable: bool) -> ColumnSnapshot {
        ColumnSnapshot {
            name: name.to_string(),
            data_type: dtype.to_string(),
            nullable,
            default: None,
            encryption_sentinel: None,
            comment_sentinel: None,
        }
    }

    fn ec(name: &str, dtype: &str, nullable: bool) -> ExpectColumn {
        ExpectColumn {
            name: name.to_string(),
            data_type: dtype.to_string(),
            nullable,
            sqlite_text_facet: None,
        }
    }

    fn snapshot_with(table: &str, t: TableSnapshot) -> SchemaSnapshot {
        let mut tables = BTreeMap::new();
        tables.insert(table.to_string(), t);
        SchemaSnapshot { tables }
    }

    fn empty_table() -> TableSnapshot {
        TableSnapshot {
            columns: Vec::new(),
            indexes: Vec::new(),
            constraints: Vec::new(),
            stored_create_sql: None,
        }
    }

    // -- Column ifNotExists ------------------------------------------------

    #[test]
    fn column_ifnotexists_absent_runs_bare() {
        let probe = GuardProbe::Column {
            schema: "app".into(),
            table: "users".into(),
            column: "email".into(),
            direction: GuardDir::IfNotExists,
            expect: Some(("text".into(), true)),
            sqlite_text_facet: None,
        };
        let live = SchemaSnapshot::default();
        assert_eq!(decide(&probe, &live), GuardVerdict::RunBare);
    }

    #[test]
    fn column_ifnotexists_present_matching_is_noop() {
        let probe = GuardProbe::Column {
            schema: "app".into(),
            table: "users".into(),
            column: "email".into(),
            direction: GuardDir::IfNotExists,
            expect: Some(("text".into(), true)),
            sqlite_text_facet: None,
        };
        let mut t = empty_table();
        t.columns.push(col("email", "text", true));
        let live = snapshot_with("users", t);
        assert_eq!(decide(&probe, &live), GuardVerdict::SatisfiedNoop);
    }

    #[test]
    fn column_ifnotexists_present_divergent_type_fails() {
        let probe = GuardProbe::Column {
            schema: "app".into(),
            table: "users".into(),
            column: "email".into(),
            direction: GuardDir::IfNotExists,
            expect: Some(("text".into(), true)),
            sqlite_text_facet: None,
        };
        let mut t = empty_table();
        t.columns.push(col("email", "integer", true));
        let live = snapshot_with("users", t);
        match decide(&probe, &live) {
            GuardVerdict::FailDrift(d) => assert_eq!(d.field, "data_type"),
            v => panic!("expected FailDrift(data_type), got {v:?}"),
        }
    }

    #[test]
    fn column_ifnotexists_present_divergent_nullability_fails() {
        let probe = GuardProbe::Column {
            schema: "app".into(),
            table: "users".into(),
            column: "email".into(),
            direction: GuardDir::IfNotExists,
            expect: Some(("text".into(), true)),
            sqlite_text_facet: None,
        };
        let mut t = empty_table();
        t.columns.push(col("email", "text", false));
        let live = snapshot_with("users", t);
        match decide(&probe, &live) {
            GuardVerdict::FailDrift(d) => assert_eq!(d.field, "nullable"),
            v => panic!("expected FailDrift(nullable), got {v:?}"),
        }
    }

    // -- H1: SQLite TEXT-affinity facet fail-closed ------------------------

    #[test]
    fn column_ifnotexists_sqlite_text_affinity_facet_change_fails_closed() {
        // H1: on SQLite the live column reads back as the `text` affinity for a
        // string column; the guard declares `date` (also TEXT affinity). A plain
        // affinity compare (`text` == `text`) would SatisfiedNoop and silently skip
        // the add over a divergent column. With the un-collapsed declared facet
        // carried in the probe, the decider fails CLOSED instead.
        let probe = GuardProbe::Column {
            schema: "app".into(),
            table: "users".into(),
            column: "happened".into(),
            direction: GuardDir::IfNotExists,
            // declared `date` collapses to the `text` affinity on SQLite.
            expect: Some(("text".into(), true)),
            sqlite_text_facet: Some("date".into()),
        };
        let mut t = empty_table();
        // live column authored as `string` → introspected affinity `text`.
        t.columns.push(col("happened", "text", true));
        match decide(&probe, &snapshot_with("users", t)) {
            GuardVerdict::FailDrift(d) => {
                assert_eq!(d.field, "data_type");
                assert!(
                    d.expected.contains("date"),
                    "names the declared facet, got {}",
                    d.expected
                );
            }
            v => panic!("expected FailDrift(data_type) on a SQLite TEXT-affinity facet, got {v:?}"),
        }
    }

    #[test]
    fn column_ifnotexists_no_facet_text_match_is_noop() {
        // The PG / non-TEXT-affinity path: `sqlite_text_facet` is None, so a `text`
        // == `text` match is a legitimate SatisfiedNoop (the live type IS the facet).
        let probe = GuardProbe::Column {
            schema: "app".into(),
            table: "users".into(),
            column: "email".into(),
            direction: GuardDir::IfNotExists,
            expect: Some(("text".into(), true)),
            sqlite_text_facet: None,
        };
        let mut t = empty_table();
        t.columns.push(col("email", "text", true));
        assert_eq!(decide(&probe, &snapshot_with("users", t)), GuardVerdict::SatisfiedNoop);
    }

    #[test]
    fn table_ifnotexists_sqlite_text_affinity_facet_fails_closed() {
        // The Table (createTable) leg of H1: a per-column `sqlite_text_facet` flag on
        // a present TEXT-affinity column fails the whole-table verify closed.
        let probe = GuardProbe::Table {
            schema: "app".into(),
            table: "events".into(),
            direction: GuardDir::IfNotExists,
            expect_columns: vec![ExpectColumn {
                name: "at".into(),
                data_type: "text".into(),
                nullable: true,
                sqlite_text_facet: Some("date".into()),
            }],
        };
        let mut t = empty_table();
        t.columns.push(col("at", "text", true));
        match decide(&probe, &snapshot_with("events", t)) {
            GuardVerdict::FailDrift(d) => assert_eq!(d.field, "data_type"),
            v => panic!("expected FailDrift(data_type) for a SQLite TEXT-affinity table column, got {v:?}"),
        }
    }

    // -- Column ifExists (dropColumn) --------------------------------------

    #[test]
    fn column_ifexists_present_runs_absent_noops() {
        let probe = GuardProbe::Column {
            schema: "app".into(),
            table: "users".into(),
            column: "legacy".into(),
            direction: GuardDir::IfExists,
            expect: None,
            sqlite_text_facet: None,
        };
        let mut t = empty_table();
        t.columns.push(col("legacy", "text", true));
        assert_eq!(decide(&probe, &snapshot_with("users", t)), GuardVerdict::RunBare);
        assert_eq!(decide(&probe, &SchemaSnapshot::default()), GuardVerdict::SatisfiedNoop);
    }

    // -- Table ifNotExists -------------------------------------------------

    #[test]
    fn table_ifnotexists_present_extra_live_column_fails() {
        let probe = GuardProbe::Table {
            schema: "app".into(),
            table: "users".into(),
            direction: GuardDir::IfNotExists,
            expect_columns: vec![ec("id", "integer", false)],
        };
        let mut t = empty_table();
        t.columns.push(col("id", "integer", false));
        t.columns.push(col("sneaky", "text", true)); // extra live column
        match decide(&probe, &snapshot_with("users", t)) {
            GuardVerdict::FailDrift(d) => assert_eq!(d.field, "columns"),
            v => panic!("expected FailDrift(columns) for extra live column, got {v:?}"),
        }
    }

    #[test]
    fn table_ifnotexists_present_matching_is_noop() {
        let probe = GuardProbe::Table {
            schema: "app".into(),
            table: "users".into(),
            direction: GuardDir::IfNotExists,
            expect_columns: vec![ec("id", "integer", false)],
        };
        let mut t = empty_table();
        t.columns.push(col("id", "integer", false));
        assert_eq!(decide(&probe, &snapshot_with("users", t)), GuardVerdict::SatisfiedNoop);
    }

    // -- Index ifNotExists -------------------------------------------------

    #[test]
    fn index_ifnotexists_present_unique_flip_fails() {
        let probe = GuardProbe::Index {
            schema: "app".into(),
            table: "users".into(),
            name: "users_email_idx".into(),
            direction: GuardDir::IfNotExists,
            expect: Some((true, vec!["email".into()])),
        };
        let mut t = empty_table();
        t.indexes
            .push(IndexSnapshot::btree("users_email_idx".to_string(), false, vec!["email".to_string()]));
        match decide(&probe, &snapshot_with("users", t)) {
            GuardVerdict::FailDrift(d) => assert_eq!(d.field, "unique"),
            v => panic!("expected FailDrift(unique), got {v:?}"),
        }
    }

    #[test]
    fn index_ifnotexists_present_expression_index_fails_closed() {
        let probe = GuardProbe::Index {
            schema: "app".into(),
            table: "users".into(),
            name: "users_lower_idx".into(),
            direction: GuardDir::IfNotExists,
            expect: Some((false, vec!["email".into()])),
        };
        let mut t = empty_table();
        let mut idx = IndexSnapshot::btree("users_lower_idx".to_string(), false, Vec::new());
        idx.expression = Some("lower(email)".into());
        t.indexes.push(idx);
        match decide(&probe, &snapshot_with("users", t)) {
            GuardVerdict::FailDrift(d) => assert_eq!(d.field, "expression"),
            v => panic!("expected FailDrift(expression) for partial/expression index, got {v:?}"),
        }
    }

    // -- Constraint ifNotExists (MED finding) ------------------------------

    fn constraint(name: &str, kind: &str, definition: &str) -> ConstraintSnapshot {
        ConstraintSnapshot {
            name: name.to_string(),
            kind: kind.to_string(),
            definition: definition.to_string(),
        }
    }

    #[test]
    fn constraint_ifnotexists_absent_runs_bare() {
        let probe = GuardProbe::Constraint {
            schema: "app".into(),
            table: "users".into(),
            name: "users_email_key".into(),
            direction: GuardDir::IfNotExists,
            expect_kind: Some("UNIQUE".into()),
        };
        assert_eq!(decide(&probe, &SchemaSnapshot::default()), GuardVerdict::RunBare);
    }

    #[test]
    fn constraint_ifnotexists_present_different_kind_fails_kind() {
        let probe = GuardProbe::Constraint {
            schema: "app".into(),
            table: "users".into(),
            name: "users_pk".into(),
            direction: GuardDir::IfNotExists,
            expect_kind: Some("UNIQUE".into()),
        };
        let mut t = empty_table();
        t.constraints
            .push(constraint("users_pk", "PRIMARY KEY", "PRIMARY KEY (id)"));
        match decide(&probe, &snapshot_with("users", t)) {
            GuardVerdict::FailDrift(d) => assert_eq!(d.field, "kind"),
            v => panic!("expected FailDrift(kind), got {v:?}"),
        }
    }

    #[test]
    fn constraint_ifnotexists_present_same_kind_fails_definition_not_noop() {
        // MED finding: a same-name + same-kind constraint must NOT be SatisfiedNoop
        // — the live pg_get_constraintdef body cannot be proven equal to the IR's
        // un-normalized constraint, so a possibly-divergent CHECK/FK is refused.
        let probe = GuardProbe::Constraint {
            schema: "app".into(),
            table: "users".into(),
            name: "users_age_chk".into(),
            direction: GuardDir::IfNotExists,
            expect_kind: Some("CHECK".into()),
        };
        let mut t = empty_table();
        t.constraints
            .push(constraint("users_age_chk", "CHECK", "CHECK ((age > 18))"));
        match decide(&probe, &snapshot_with("users", t)) {
            GuardVerdict::FailDrift(d) => assert_eq!(d.field, "definition"),
            v => panic!("expected FailDrift(definition) on same-name+same-kind, got {v:?}"),
        }
    }

    // -- ColumnPresence (alter/rename ifExists) ----------------------------

    #[test]
    fn column_presence_present_runs_absent_noops() {
        let probe = GuardProbe::ColumnPresence {
            schema: "app".into(),
            table: "users".into(),
            column: "name".into(),
            direction: GuardDir::IfExists,
        };
        let mut t = empty_table();
        t.columns.push(col("name", "text", true));
        assert_eq!(decide(&probe, &snapshot_with("users", t)), GuardVerdict::RunBare);
        assert_eq!(decide(&probe, &SchemaSnapshot::default()), GuardVerdict::SatisfiedNoop);
    }
}
