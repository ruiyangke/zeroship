//! The STRUCTURAL expression-AST validator + the structured-error envelope.
//!
//! The closed expression AST ([`crate::model::expr::Expr`]) is **constructed in JS and
//! serialized to IR — never parsed from text**. So validation is a
//! purely STRUCTURAL allow-list walk over the deserialized tree:
//!
//! - **(a)** every node is in the allow-listed set — the serde deserializer
//!   already rejects an unknown node *tag* (`UNSUPPORTED { kind: "expr" }` at
//!   load); this walk additionally rejects the structural shapes that *are*
//!   well-typed nodes but out of policy (an out-of-envelope `FnSynth(splitPart)`,
//!   a non-portable cast target).
//! - **(b)** `c.fn.splitPart` args are in-envelope — `delim` is a single ASCII
//!   character `Literal` (one byte, code point `< 0x80`), `n` is a positive
//!   integer `Literal` with `1 ≤ n ≤ 8`, and the column arg is a `ColRef` /
//!   in-AST sub-expression.
//! - **(c)** every `ColRef` resolves to a column on the ENCLOSING target table —
//!   an apply/render-time check scoped to the single target table of the
//!   enclosing op (an apply-time check). A cross-table reference is impossible by
//!   construction (`c` is single-table-scoped), and any reference to a
//!   column not on the target table is a hard error (injection defense + the
//!   capability boundary).
//! - **(d)** a `Cast` target is a portable type — guaranteed by the closed
//!   [`crate::model::expr::CastTarget`] enum, so this is structurally total.
//!
//! There is **NO lexer, NO Pratt/precedence parser, NO `libpg_query`, NO
//! differential fuzzer** — the injection risk is dissolved, not mitigated. The
//! Rust validator here is the authoritative STRUCTURAL gate (checks (a), (b),
//! (d) — node allow-list, `FnSynth` arity/envelope, portable cast target); the
//! JS side runs an optional best-effort structural hint over the SAME schemars
//! schema. Rule (c) — `ColRef` resolution against the live target table — runs
//! at the apply/render seam (an apply-time check): at IR load the
//! live column set is generally unknown for the DML ops, `setColumnType`,
//! `addConstraint` and `createIndex`, so those positions validate
//! [`TargetScope::structural_only`] here and the seam re-runs the walk with a
//! resolved column set. A self-contained `createTable` DOES resolve (c) against
//! its own declared columns at load.
//!
//! LAYERING EXCEPTION: raw view-body validation calls the guard's read-only
//! body scanner after the structural `SELECT` checks. That scanner is real
//! deny-list security logic, so moving it down into `model` would put guard policy
//! in the data layer. Until a separate analysis pass above `model` + `guard`
//! walks raw view bodies, this is the one deliberate `model -> guard` edge.
//!
//! # Structural vs. policy split
//!
//! The STRUCTURAL, policy-free validator — the closed-`Expr` allow-list walk, the
//! structured-error envelope ([`AuthoringError`]), the `Dialect`/`UnsupportedKind`
//! vocabulary, the `CODE_*` codes, [`TargetScope`], and [`validate_expr`] — now
//! lives in the [`zero_migrate_ir::validate`] leaf crate. It carries no
//! [`SchemaScope`](crate::model::policy::SchemaScope) dependency and no `pg_query`.
//! THIS module keeps the policy-bound layer: the `SchemaScope`-threaded op/IR
//! validators, the vendor-capability gate, the raw-view-body `pg_query` scan, and
//! the pure primary-key validation. (Author-PK CONFORMANCE against the operator's
//! injected shape is owned by the injection resolver, not this validator.) The
//! structural surface is re-exported below so callers name it unchanged.

use crate::model::expr::{CaseBranch, Expr, ScalarFn};
use pg_query::protobuf::node::Node as NodeEnum;
use std::collections::{BTreeMap, BTreeSet};

// The structural, policy-free validator moved to the `zero-migrate-ir` leaf crate.
// Re-export its full surface so this policy-bound module (and the engine root)
// name `Dialect`, `AuthoringError`, `validate_expr`, the `CODE_*` codes,
// `TargetScope`, `validate_immutable_expr_context`, etc. exactly as before.
pub use zero_migrate_ir::validate::*;

/// Walk an entire [`MigrationIr`](crate::model::ir::MigrationIr) and validate EVERY
/// embedded expression-AST node against `target_dialect` — the "the
/// Rust validator is the authoritative STRUCTURAL gate" obligation made
/// operative. Checks (a)/(b)/(d) run at load for every Expr slot; check (c)
/// (`ColRef` resolution) runs here only for a self-contained `createTable`, and
/// otherwise at the apply/render seam (see the module note).
///
/// This is the walker that enumerates each [`Op`](crate::model::ir::Op) variant's
/// expression positions and calls [`validate_expr`] per node with the enclosing
/// op's index + single target table as scope:
///
/// - `createTable` — each `IrIndex` element expression, each `IrIndex.where`
///   partial-index predicate + each `Check` constraint `expr` (scoped to the
///   table's own declared columns, so rule (c) `ColRef` resolution runs against
///   them).
/// - `createIndex` — each index element expression + the `where` partial-index
///   predicate (closed AST since the property-A fix).
/// - `setColumnType` — the `using` cast expression (closed AST since the
///   property-A fix).
/// - `addConstraint` — a `Check` constraint `expr`.
/// - `update` — every `set` RHS + the optional `where`.
/// - `delete` — the mandatory `where`.
/// - `backfill` — every `set` RHS + the optional `filter`.
///
/// Ops with no expression slot (e.g. `dropTable`, `addColumn`, `insert`) walk to
/// `Ok(())`. For the DML ops (`update`/`delete`/`backfill`) and `setColumnType`
/// the live-schema column set is generally not known at IR-load time, so the
/// scope is [`TargetScope::structural_only`] — the structural checks (a),(b),(d)
/// still run; the apply/render seam re-runs the walk with a
/// resolved column set to enforce (c). A `createTable` is self-contained, so its
/// embedded predicates ARE resolved against the table's own columns here.
///
/// Returns the FIRST [`AuthoringError`] encountered, or `Ok(())`.
///
/// `ts_locations`, when supplied, maps a 0-based op index to its `.ts` source
/// location for the structured-error payload; a missing entry yields `None`.
///
/// # Errors
/// Returns the first [`AuthoringError`] any embedded expression produces.
pub fn validate_ir(
    ir: &crate::model::ir::MigrationIr,
    target_dialect: Dialect,
    ts_locations: &[Option<String>],
) -> Result<(), AuthoringError> {
    validate_ir_scoped(ir, target_dialect, ts_locations, None)
}

/// [`validate_ir`] threaded with the active schema confinement scope.
/// `schema_scope`:
/// - `None` ⇒ omitted/default public capability: no project schema is known, so
///   cross-schema checks are not applied, but vendor capabilities stay confined.
/// - `Some(SchemaScope::Single(project_schema))` ⇒ the **Confined** creator
///   profile: an explicit `schema != project_schema` is REFUSED fail-closed
///   ([`CODE_CROSS_SCHEMA`]).
/// - `Some(SchemaScope::Allowlist([...]))` ⇒ the **Platform** profile: an explicit
///   `schema` must be a member of the allow-list.
/// - `Some(SchemaScope::Unconfined)` ⇒ the explicit **Trusted** operator profile:
///   no cross-schema confinement and full vendor capability.
///
/// # Errors
/// The first [`AuthoringError`] any op produces (cross-schema, invalid schema ident,
/// illegal guard direction, or an embedded-expression rejection).
pub fn validate_ir_scoped(
    ir: &crate::model::ir::MigrationIr,
    target_dialect: Dialect,
    ts_locations: &[Option<String>],
    schema_scope: Option<&crate::model::policy::SchemaScope>,
) -> Result<(), AuthoringError> {
    for (op_index, op) in ir.ops.iter().enumerate() {
        let ts = ts_locations.get(op_index).and_then(Option::as_deref);
        validate_op_scoped(op, target_dialect, op_index, ts, schema_scope)?;
    }
    validate_column_references(ir, target_dialect, ts_locations)?;
    validate_table_foreign_keys(ir, target_dialect, ts_locations)?;
    validate_per_row_destinations(ir, target_dialect, ts_locations)?;
    validate_online_rename_sequence(ir, target_dialect, ts_locations)?;
    validate_partition_recording(ir, target_dialect, ts_locations)?;
    Ok(())
}

#[derive(Debug)]
struct TableOperationTarget<'a> {
    schema: Option<&'a str>,
    table: &'a str,
    op_index: usize,
    is_online_rename: bool,
}

fn schemas_may_name_same_table(left: Option<&str>, right: Option<&str>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
        // Validation does not always know which project schema an unqualified op
        // will use. Treat it as potentially matching an explicit qualifier so the
        // online-rename safety gate fails closed.
        _ => true,
    }
}

/// Stable logical identity of one project-schema column.
///
/// This key deliberately records only declarations authored by migration IR.
/// Catalog introspection cannot reconstruct semantic value-format contracts such
/// as TypeID prefixes or ULID casing, so it must never populate this map.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LogicalColumnKey {
    pub schema: Option<String>,
    pub table: String,
    pub column: String,
}

/// Logical column contract retained across ordered migration artifacts.
#[derive(Debug, Clone)]
pub struct LogicalColumnContract {
    pub ty: crate::model::ir::ColType,
    pub value_format: Option<crate::model::ir::ValueFormat>,
    /// Authored collation intent. `None` and `Some(true)` both mean the
    /// bytewise/default comparison behavior; `Some(false)` requests the
    /// portable case-insensitive text shape.
    pub case_sensitive: Option<bool>,
    /// Whether this authored column is independently eligible as the target of
    /// a single-column foreign key: a one-column primary key or UNIQUE key.
    /// Composite keys deliberately do not set this bit for their components.
    pub single_column_reference_key: bool,
    /// Ordered primary/unique candidate-key tuples declared for this column's
    /// table. Every column contract for a freshly declared table carries the
    /// same set so ordered composite-key eligibility survives across migration
    /// artifacts without being inferred from physical catalog storage.
    pub candidate_keys: BTreeSet<Vec<String>>,
    /// Object-identity-preserving sources for `candidate_keys`. This lets ordered
    /// artifacts replay a later `dropIndex` / `dropConstraint` / primary-key
    /// lifecycle operation without erasing an equivalent tuple still backed by
    /// another UNIQUE object. Column-level UNIQUE declarations remain intrinsic;
    /// the primary key has its own source so explicit drop/replace can remove it
    /// without weakening an equivalent UNIQUE source.
    candidate_key_sources: CandidateKeySources,
}

#[derive(Debug, Clone, Default)]
struct CandidateKeySources {
    primary_key: Option<Vec<String>>,
    intrinsic: BTreeSet<Vec<String>>,
    indexes: BTreeMap<String, Vec<String>>,
    constraints: BTreeMap<String, Vec<String>>,
}

impl CandidateKeySources {
    fn tuples(&self) -> BTreeSet<Vec<String>> {
        self.primary_key
            .iter()
            .chain(self.intrinsic.iter())
            .chain(self.indexes.values())
            .chain(self.constraints.values())
            .cloned()
            .collect()
    }
}

/// Cumulative logical project-schema declarations used by strict per-row and
/// typed-reference validation at lowering time.
pub type LogicalColumnContracts = BTreeMap<LogicalColumnKey, LogicalColumnContract>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MissingLogicalDeclaration {
    DeferToLower,
    Reject,
}

#[derive(Debug, Clone, Copy)]
enum LogicalSchemaMode<'a> {
    Authored,
    Effective {
        project_schema: &'a str,
        default_schema: Option<&'a str>,
    },
}

impl LogicalSchemaMode<'_> {
    fn resolve(self, schema: Option<&str>) -> Option<String> {
        match self {
            Self::Authored => schema.map(str::to_string),
            Self::Effective {
                project_schema,
                default_schema,
            } => {
                let resolved = schema.or(default_schema).unwrap_or(project_schema);
                Some(
                    if resolved.eq_ignore_ascii_case(project_schema) {
                        project_schema
                    } else {
                        resolved
                    }
                    .to_string(),
                )
            }
        }
    }

    fn declarations_match(self, left: Option<&str>, right: Option<&str>) -> bool {
        match self {
            Self::Authored => schemas_name_same_declared_table(left, right),
            Self::Effective { .. } => left == right,
        }
    }

    fn destination_matches(self, left: Option<&str>, right: Option<&str>) -> bool {
        match self {
            Self::Authored => schemas_may_name_same_table(left, right),
            Self::Effective { .. } => left == right,
        }
    }
}

fn schemas_name_same_declared_table(left: Option<&str>, right: Option<&str>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
        (None, None) => true,
        _ => false,
    }
}

fn declare_logical_column(
    declared: &mut LogicalColumnContracts,
    schema_mode: LogicalSchemaMode<'_>,
    schema: Option<&str>,
    table: &str,
    column: &str,
    ty: crate::model::ir::ColType,
    value_format: Option<crate::model::ir::ValueFormat>,
    case_sensitive: Option<bool>,
    candidate_key_sources: CandidateKeySources,
) {
    declared.retain(|candidate, _| {
        candidate.table != table
            || candidate.column != column
            || !schema_mode.declarations_match(candidate.schema.as_deref(), schema)
    });
    let candidate_keys = candidate_key_sources.tuples();
    declared.insert(
        LogicalColumnKey {
            schema: schema.map(str::to_string),
            table: table.to_string(),
            column: column.to_string(),
        },
        LogicalColumnContract {
            ty,
            value_format,
            case_sensitive,
            single_column_reference_key: candidate_keys.contains(&vec![column.to_string()]),
            candidate_keys,
            candidate_key_sources,
        },
    );
}

fn create_table_candidate_key_sources(
    table: &str,
    columns: &[crate::model::ir::IrColumn],
    primary_key: Option<&[String]>,
    constraints: &[crate::model::ir::IrConstraint],
    indexes: &[crate::model::ir::IrIndex],
) -> CandidateKeySources {
    use crate::model::ir::{IndexElement, IndexMethod, IrConstraintKind};

    let mut sources = CandidateKeySources {
        intrinsic: columns
            .iter()
            .filter(|column| column.unique == Some(true))
            .map(|column| vec![column.name.clone()])
            .collect::<BTreeSet<_>>(),
        ..CandidateKeySources::default()
    };
    if let Some(primary_key) = primary_key.filter(|primary_key| !primary_key.is_empty()) {
        sources.primary_key = Some(primary_key.to_vec());
    }
    for constraint in constraints {
        if let IrConstraintKind::Unique { columns } = &constraint.kind {
            if !columns.is_empty() {
                let name = constraint.name.clone().unwrap_or_else(|| {
                    crate::plan::author::cap_ident_name(&format!(
                        "{table}_{}_key",
                        columns.join("_")
                    ))
                });
                sources.constraints.insert(name, columns.clone());
            }
        }
    }
    for index in indexes {
        if index.unique != Some(true)
            || index.r#where.is_some()
            || index.only == Some(true)
            || !matches!(index.using, None | Some(IndexMethod::Btree))
        {
            continue;
        }
        let key = index
            .columns
            .iter()
            .map(|element| match element {
                IndexElement::Column {
                    name,
                    opclass: None,
                    collation: None,
                    ..
                } => Some(name.clone()),
                _ => None,
            })
            .collect::<Option<Vec<_>>>();
        if let Some(key) = key.filter(|key| !key.is_empty()) {
            let name = index.name.clone().unwrap_or_else(|| {
                crate::plan::author::cap_ident_name(&format!("{table}_{}_idx", key.join("_")))
            });
            sources.indexes.insert(name, key);
        }
    }
    sources
}

fn existing_candidate_key_sources(
    declared: &LogicalColumnContracts,
    schema_mode: LogicalSchemaMode<'_>,
    schema: Option<&str>,
    table: &str,
    column: &str,
) -> CandidateKeySources {
    declared
        .iter()
        .find(|(candidate, _)| {
            candidate.table == table
                && candidate.column == column
                && schema_mode.declarations_match(candidate.schema.as_deref(), schema)
        })
        .map(|(_, contract)| contract.candidate_key_sources.clone())
        .unwrap_or_default()
}

fn refresh_candidate_keys(column: &str, contract: &mut LogicalColumnContract) {
    contract.candidate_keys = contract.candidate_key_sources.tuples();
    contract.single_column_reference_key =
        contract.candidate_keys.contains(&vec![column.to_string()]);
}

fn mutate_table_candidate_keys(
    declared: &mut LogicalColumnContracts,
    schema_mode: LogicalSchemaMode<'_>,
    schema: Option<&str>,
    table: &str,
    mut mutate: impl FnMut(&mut CandidateKeySources),
) {
    for (key, contract) in declared.iter_mut().filter(|(candidate, _)| {
        candidate.table == table
            && schema_mode.declarations_match(candidate.schema.as_deref(), schema)
    }) {
        mutate(&mut contract.candidate_key_sources);
        refresh_candidate_keys(&key.column, contract);
    }
}

fn eligible_unique_index_tuple(
    columns: &[crate::model::ir::IndexElement],
    unique: Option<bool>,
    using: Option<crate::model::ir::IndexMethod>,
    predicate: Option<&crate::model::expr::Expr>,
    only: Option<bool>,
) -> Option<Vec<String>> {
    use crate::model::ir::{IndexElement, IndexMethod};

    if unique != Some(true)
        || predicate.is_some()
        || only == Some(true)
        || !matches!(using, None | Some(IndexMethod::Btree))
    {
        return None;
    }
    columns
        .iter()
        .map(|element| match element {
            IndexElement::Column {
                name,
                opclass: None,
                collation: None,
                ..
            } => Some(name.clone()),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .filter(|tuple| !tuple.is_empty())
}

fn reset_create_table_candidate_keys(
    declared: &mut LogicalColumnContracts,
    schema_mode: LogicalSchemaMode<'_>,
    schema: Option<&str>,
    table: &str,
    columns: &[crate::model::ir::IrColumn],
    primary_key: Option<&[String]>,
    constraints: &[crate::model::ir::IrConstraint],
    indexes: &[crate::model::ir::IrIndex],
) {
    let sources =
        create_table_candidate_key_sources(table, columns, primary_key, constraints, indexes);
    mutate_table_candidate_keys(declared, schema_mode, schema, table, |candidate_sources| {
        candidate_sources.clone_from(&sources);
    });
}

fn add_index_candidate_key(
    declared: &mut LogicalColumnContracts,
    schema_mode: LogicalSchemaMode<'_>,
    schema: Option<&str>,
    table: &str,
    columns: &[crate::model::ir::IndexElement],
    name: Option<&str>,
    unique: Option<bool>,
    using: Option<crate::model::ir::IndexMethod>,
    predicate: Option<&crate::model::expr::Expr>,
    only: Option<bool>,
) {
    let Some(tuple) = eligible_unique_index_tuple(columns, unique, using, predicate, only) else {
        return;
    };
    let name = name.map_or_else(
        || crate::plan::author::cap_ident_name(&format!("{table}_{}_idx", tuple.join("_"))),
        str::to_string,
    );
    mutate_table_candidate_keys(declared, schema_mode, schema, table, |sources| {
        sources.indexes.insert(name.clone(), tuple.clone());
    });
}

fn drop_index_candidate_key(
    declared: &mut LogicalColumnContracts,
    schema_mode: LogicalSchemaMode<'_>,
    schema: Option<&str>,
    table: &str,
    name: &str,
) {
    mutate_table_candidate_keys(declared, schema_mode, schema, table, |sources| {
        sources.indexes.remove(name);
    });
}

fn add_unique_constraint_candidate_key(
    declared: &mut LogicalColumnContracts,
    schema_mode: LogicalSchemaMode<'_>,
    schema: Option<&str>,
    table: &str,
    constraint: &crate::model::ir::IrConstraint,
) {
    let crate::model::ir::IrConstraintKind::Unique { columns } = &constraint.kind else {
        return;
    };
    if columns.is_empty() {
        return;
    }
    let name = constraint.name.clone().unwrap_or_else(|| {
        crate::plan::author::cap_ident_name(&format!("{table}_{}_key", columns.join("_")))
    });
    mutate_table_candidate_keys(declared, schema_mode, schema, table, |sources| {
        sources.constraints.insert(name.clone(), columns.clone());
    });
}

fn drop_constraint_candidate_key(
    declared: &mut LogicalColumnContracts,
    schema_mode: LogicalSchemaMode<'_>,
    schema: Option<&str>,
    table: &str,
    name: &str,
) {
    mutate_table_candidate_keys(declared, schema_mode, schema, table, |sources| {
        sources.constraints.remove(name);
    });
}

fn alter_primary_key_candidate_key(
    declared: &mut LogicalColumnContracts,
    schema_mode: LogicalSchemaMode<'_>,
    schema: Option<&str>,
    table: &str,
    action: &crate::model::ir::AlterPrimaryKeyAction,
) {
    use crate::model::ir::AlterPrimaryKeyAction;

    let known_columns = declared
        .keys()
        .filter(|candidate| {
            candidate.table == table
                && schema_mode.declarations_match(candidate.schema.as_deref(), schema)
        })
        .map(|candidate| candidate.column.as_str())
        .collect::<BTreeSet<_>>();
    let target_is_known = action.target_columns().is_none_or(|columns| {
        columns
            .iter()
            .all(|column| known_columns.contains(column.as_str()))
    });
    if !target_is_known {
        // A lifecycle op never declares a column. If the authored graph cannot
        // resolve every target component, keep its candidate-key state unchanged;
        // the locked live preflight is authoritative and will reject an absent
        // column rather than this replay inventing one.
        return;
    }

    mutate_table_candidate_keys(declared, schema_mode, schema, table, |sources| {
        let has_alternate = |columns: &[String]| {
            sources.intrinsic.contains(columns)
                || sources.indexes.values().any(|key| key == columns)
                || sources.constraints.values().any(|key| key == columns)
        };
        match action {
            AlterPrimaryKeyAction::Add { columns }
                if sources.primary_key.is_none() && has_alternate(columns) =>
            {
                sources.primary_key = Some(columns.clone());
            }
            AlterPrimaryKeyAction::Replace {
                expected_columns,
                columns,
                ..
            } if sources.primary_key.as_ref() == Some(expected_columns)
                && has_alternate(columns) =>
            {
                sources.primary_key = Some(columns.clone());
            }
            AlterPrimaryKeyAction::Drop {
                expected_columns, ..
            } if sources.primary_key.as_ref() == Some(expected_columns) => {
                sources.primary_key = None;
            }
            _ => {
                // Unknown/mismatched authored state remains unchanged. The op's
                // expectedColumns is a locked apply precondition, never permission
                // for an offline replay to discover or assume a different key.
            }
        }
    });
}

fn remove_declared_per_row_table(
    declared: &mut LogicalColumnContracts,
    schema_mode: LogicalSchemaMode<'_>,
    schema: Option<&str>,
    table: &str,
) {
    declared.retain(|candidate, _| {
        candidate.table != table
            || !schema_mode.declarations_match(candidate.schema.as_deref(), schema)
    });
}

fn per_row_validation_error(
    target_dialect: Dialect,
    op_index: usize,
    ts_locations: &[Option<String>],
    reason: String,
    suggested_fix: String,
) -> AuthoringError {
    AuthoringError {
        code: CODE_OP_INVALID.to_string(),
        kind: Some(UnsupportedKind::Op),
        op_index,
        ts_location: ts_locations.get(op_index).cloned().flatten(),
        dialect: target_dialect,
        reason,
        suggested_fix: Some(suggested_fix),
    }
}

const MAX_EXTERNAL_CURSOR_INVARIANT_NAME_CHARS: usize = 255;

fn validate_backfill_cursor_fields(
    cursor_columns: &[String],
    cursor_stability: &crate::model::ir::CursorStability,
    set: &BTreeMap<String, crate::model::ir::BackfillSetValue>,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    let error = |reason: String, suggested_fix: String| AuthoringError {
        code: CODE_OP_INVALID.to_string(),
        kind: Some(UnsupportedKind::Op),
        op_index,
        ts_location: ts_location.map(str::to_string),
        dialect: target_dialect,
        reason,
        suggested_fix: Some(suggested_fix),
    };

    if cursor_columns.is_empty() {
        return Err(error(
            "backfill cursorColumns must be a non-empty ordered tuple".to_string(),
            "provide the full ordered primary/unique candidate key, for example cursorColumns: [\"id\"]"
                .to_string(),
        ));
    }

    let mut seen = BTreeSet::new();
    for column in cursor_columns {
        if !is_safe_schema_ident(column) {
            return Err(error(
                format!(
                    "backfill cursorColumns contains {column:?}, which is not a safe non-empty bare column identifier"
                ),
                "use plain column identifiers containing only ASCII letters, digits, and underscores"
                    .to_string(),
            ));
        }
        let comparison_name = if target_dialect == Dialect::Postgres {
            column.clone()
        } else {
            column.to_ascii_lowercase()
        };
        if !seen.insert(comparison_name) {
            return Err(error(
                format!(
                    "backfill cursorColumns repeats component {column:?}; a cursor tuple cannot contain the same column twice"
                ),
                "list each cursor component exactly once in candidate-key order".to_string(),
            ));
        }
        if set.keys().any(|destination| {
            if target_dialect == Dialect::Postgres {
                destination == column
            } else {
                destination.eq_ignore_ascii_case(column)
            }
        }) {
            return Err(error(
                format!(
                    "backfill assignment targets cursor component {column:?}; changing any cursor component while paging would make row selection unstable"
                ),
                "remove every cursorColumns component from the backfill set assignment"
                    .to_string(),
            ));
        }
    }

    if let crate::model::ir::CursorStability::ExternalInvariant { name } = cursor_stability {
        let chars = name.chars().count();
        if name.trim().is_empty() || chars > MAX_EXTERNAL_CURSOR_INVARIANT_NAME_CHARS {
            return Err(error(
                format!(
                    "backfill cursorStability externalInvariant name must contain non-whitespace text and be at most {MAX_EXTERNAL_CURSOR_INVARIANT_NAME_CHARS} characters; found {chars} characters"
                ),
                "provide a short, explicit application or maintenance invariant name that operators can recognize in preview and status"
                    .to_string(),
            ));
        }
    }

    Ok(())
}

fn validate_per_row_destination(
    table: &str,
    schema: Option<&str>,
    cursor_columns: &[String],
    column: &str,
    generator: &crate::model::ir::PerRowGenerator,
    declared: &LogicalColumnContracts,
    schema_mode: LogicalSchemaMode<'_>,
    missing: MissingLogicalDeclaration,
    target_dialect: Dialect,
    op_index: usize,
    ts_locations: &[Option<String>],
) -> Result<(), AuthoringError> {
    use crate::model::ir::{ColType, PerRowGenerator, ValueFormat};

    if let PerRowGenerator::TypeId { prefix } = generator {
        if let Err(error) = crate::model::ir::validate_type_id_prefix(prefix) {
            return Err(AuthoringError {
                code: CODE_INVALID_TYPE_ID_PREFIX.to_string(),
                kind: Some(UnsupportedKind::Op),
                op_index,
                ts_location: ts_locations.get(op_index).cloned().flatten(),
                dialect: target_dialect,
                reason: format!(
                    "backfill perRow.typeId({{ prefix: {prefix:?} }}) carries an invalid TypeID prefix: {error}"
                ),
                suggested_fix: Some(
                    "use an empty prefix, or at most 63 lowercase ASCII letters and underscores, starting and ending with a letter"
                        .to_string(),
                ),
            });
        }
    }

    if cursor_columns.iter().any(|cursor| {
        if target_dialect == Dialect::Postgres {
            cursor == column
        } else {
            cursor.eq_ignore_ascii_case(column)
        }
    }) {
        return Err(per_row_validation_error(
            target_dialect,
            op_index,
            ts_locations,
            format!(
                "backfill per-row generation targets cursor column {column:?}; changing the cursor while paging would make row selection unstable"
            ),
            "choose a destination column that is not a component of cursorColumns".to_string(),
        ));
    }

    let matches: Vec<&LogicalColumnContract> = declared
        .iter()
        .filter(|(candidate, _)| {
            candidate.table == table
                && candidate.column == column
                && schema_mode.destination_matches(candidate.schema.as_deref(), schema)
        })
        .map(|(_, contract)| contract)
        .collect();
    let qualified_table =
        schema.map_or_else(|| table.to_string(), |schema| format!("{schema}.{table}"));
    let destination = match matches.as_slice() {
        [] => {
            if missing == MissingLogicalDeclaration::DeferToLower {
                return Ok(());
            }
            return Err(per_row_validation_error(
                target_dialect,
                op_index,
                ts_locations,
                format!(
                    "backfill per-row destination {qualified_table}.{column} has no logical column declaration in the project schema available to this migration"
                ),
                "declare the destination in createTable/addColumn before this backfill and use a matching logical UUID, TypeID, or ULID column"
                    .to_string(),
            ));
        }
        [destination] => *destination,
        _ => {
            return Err(per_row_validation_error(
                target_dialect,
                op_index,
                ts_locations,
                format!(
                    "backfill per-row destination {qualified_table}.{column} is ambiguous across {} project-schema declarations",
                    matches.len()
                ),
                "qualify the declarations and backfill with one exact schema so the logical destination contract is unambiguous"
                    .to_string(),
            ));
        }
    };

    let valid = match generator {
        PerRowGenerator::UuidV4 | PerRowGenerator::UuidV7 => {
            matches!(destination.ty, ColType::Uuid)
        }
        PerRowGenerator::TypeId { prefix } => matches!(
            (&destination.ty, &destination.value_format),
            (ColType::Text, Some(ValueFormat::TypeId { prefix: declared_prefix }))
                if declared_prefix == prefix
        ),
        PerRowGenerator::Ulid => matches!(
            (&destination.ty, &destination.value_format),
            (ColType::Text, Some(ValueFormat::Ulid))
        ),
    };
    if valid {
        return Ok(());
    }

    let expected = match generator {
        PerRowGenerator::UuidV4 => "a logical UUID column for perRow.uuidV4()".to_string(),
        PerRowGenerator::UuidV7 => "a logical UUID column for perRow.uuidV7()".to_string(),
        PerRowGenerator::TypeId { prefix } => format!(
            "a TypeID column whose declared stored prefix is exactly {prefix:?} for perRow.typeId(...)"
        ),
        PerRowGenerator::Ulid => "a declared ULID column for perRow.ulid()".to_string(),
    };
    let actual = match (&destination.ty, &destination.value_format) {
        (ColType::Text, Some(ValueFormat::TypeId { prefix })) => {
            format!("a TypeID column with stored prefix {prefix:?}")
        }
        (ColType::Text, Some(ValueFormat::Ulid)) => "a ULID column".to_string(),
        (ColType::Text, None) => "generic text with no value-format contract".to_string(),
        (ty, Some(format)) => format!("logical type {ty:?} with value format {format:?}"),
        (ty, None) => format!("logical type {ty:?}"),
    };
    Err(per_row_validation_error(
        target_dialect,
        op_index,
        ts_locations,
        format!(
            "backfill per-row destination {qualified_table}.{column} is {actual}; this generator requires {expected}"
        ),
        "use the generator matching the destination's declared logical value format; generic text is not inferred as TypeID or ULID"
            .to_string(),
    ))
}

fn validate_per_row_op(
    op: &crate::model::ir::Op,
    target_dialect: Dialect,
    op_index: usize,
    ts_locations: &[Option<String>],
    declared: &mut LogicalColumnContracts,
    missing: MissingLogicalDeclaration,
    schema_mode: LogicalSchemaMode<'_>,
) -> Result<(), AuthoringError> {
    use crate::model::ir::{BackfillSetValue, Op};

    match op {
        Op::Dialectal {
            default,
            pg,
            sqlite,
            mysql,
        } => {
            let own = match target_dialect {
                Dialect::Postgres => pg.as_deref(),
                Dialect::Sqlite => sqlite.as_deref(),
                Dialect::Mysql => mysql.as_deref(),
            };
            if let Some(ops) = own.or(default.as_deref()) {
                for inner in ops {
                    validate_per_row_op(
                        inner,
                        target_dialect,
                        op_index,
                        ts_locations,
                        declared,
                        missing,
                        schema_mode,
                    )?;
                }
            }
        }
        Op::CreateTable {
            name,
            columns,
            primary_key,
            constraints,
            indexes,
            schema,
            ..
        } => {
            let schema = schema_mode.resolve(schema.as_deref());
            remove_declared_per_row_table(declared, schema_mode, schema.as_deref(), name);
            let reference_keys = create_table_candidate_key_sources(
                name,
                columns,
                primary_key.as_deref(),
                constraints,
                indexes,
            );
            for column in columns {
                declare_logical_column(
                    declared,
                    schema_mode,
                    schema.as_deref(),
                    name,
                    &column.name,
                    column.ty.clone(),
                    column.value_format.clone(),
                    column.case_sensitive,
                    reference_keys.clone(),
                );
            }
        }
        Op::AddColumn {
            table,
            column,
            ty,
            value_format,
            case_sensitive,
            schema,
            ..
        } => {
            let schema = schema_mode.resolve(schema.as_deref());
            declare_logical_column(
                declared,
                schema_mode,
                schema.as_deref(),
                table,
                column,
                ty.clone(),
                value_format.clone(),
                *case_sensitive,
                CandidateKeySources::default(),
            );
        }
        Op::SetColumnType {
            table,
            column,
            to_type,
            schema,
            ..
        } => {
            let schema = schema_mode.resolve(schema.as_deref());
            let reference_keys = existing_candidate_key_sources(
                declared,
                schema_mode,
                schema.as_deref(),
                table,
                column,
            );
            declare_logical_column(
                declared,
                schema_mode,
                schema.as_deref(),
                table,
                column,
                to_type.clone(),
                None,
                None,
                reference_keys,
            );
        }
        Op::DropColumn {
            table,
            column,
            schema,
            ..
        } => {
            let schema = schema_mode.resolve(schema.as_deref());
            declared.retain(|candidate, _| {
                candidate.table != *table
                    || candidate.column != *column
                    || !schema_mode
                        .declarations_match(candidate.schema.as_deref(), schema.as_deref())
            });
        }
        Op::DropTable { table, schema, .. } => {
            let schema = schema_mode.resolve(schema.as_deref());
            remove_declared_per_row_table(declared, schema_mode, schema.as_deref(), table);
        }
        Op::RenameTable {
            table, to, schema, ..
        } => {
            let schema = schema_mode.resolve(schema.as_deref());
            let renamed = declared
                .keys()
                .filter(|candidate| {
                    candidate.table == *table
                        && schema_mode
                            .declarations_match(candidate.schema.as_deref(), schema.as_deref())
                })
                .cloned()
                .collect::<Vec<_>>();
            for from in renamed {
                if let Some(contract) = declared.remove(&from) {
                    declared.insert(
                        LogicalColumnKey {
                            schema: from.schema,
                            table: to.clone(),
                            column: from.column,
                        },
                        contract,
                    );
                }
            }
        }
        Op::RenameColumn {
            table,
            from,
            to,
            ty,
            schema,
            ..
        } => {
            let schema = schema_mode.resolve(schema.as_deref());
            let renamed = declared
                .keys()
                .filter(|candidate| {
                    candidate.table == *table
                        && candidate.column == *from
                        && schema_mode
                            .declarations_match(candidate.schema.as_deref(), schema.as_deref())
                })
                .cloned()
                .collect::<Vec<_>>();
            let found = !renamed.is_empty();
            for old_key in renamed {
                if let Some(mut contract) = declared.remove(&old_key) {
                    contract.ty = ty.clone();
                    declared.insert(
                        LogicalColumnKey {
                            schema: old_key.schema,
                            table: old_key.table,
                            column: to.clone(),
                        },
                        contract,
                    );
                }
            }
            if !found {
                declare_logical_column(
                    declared,
                    schema_mode,
                    schema.as_deref(),
                    table,
                    to,
                    ty.clone(),
                    None,
                    None,
                    CandidateKeySources::default(),
                );
            }
        }
        Op::CreateIndex {
            table,
            columns,
            name,
            unique,
            using,
            r#where,
            only,
            schema,
            ..
        } => {
            let schema = schema_mode.resolve(schema.as_deref());
            add_index_candidate_key(
                declared,
                schema_mode,
                schema.as_deref(),
                table,
                columns,
                name.as_deref(),
                *unique,
                *using,
                r#where.as_ref(),
                *only,
            );
        }
        Op::DropIndex {
            table: Some(table),
            name,
            schema,
            ..
        } => {
            let schema = schema_mode.resolve(schema.as_deref());
            drop_index_candidate_key(declared, schema_mode, schema.as_deref(), table, name);
        }
        Op::AddConstraint {
            table,
            constraint,
            schema,
            ..
        } => {
            let schema = schema_mode.resolve(schema.as_deref());
            add_unique_constraint_candidate_key(
                declared,
                schema_mode,
                schema.as_deref(),
                table,
                constraint,
            );
        }
        Op::DropConstraint {
            table,
            name,
            schema,
            ..
        } => {
            let schema = schema_mode.resolve(schema.as_deref());
            drop_constraint_candidate_key(declared, schema_mode, schema.as_deref(), table, name);
        }
        Op::AlterPrimaryKey {
            table,
            action,
            schema,
        } => {
            let schema = schema_mode.resolve(schema.as_deref());
            alter_primary_key_candidate_key(
                declared,
                schema_mode,
                schema.as_deref(),
                table,
                action,
            );
        }
        Op::Backfill {
            table,
            cursor_columns,
            set,
            schema,
            ..
        } => {
            let schema = schema_mode.resolve(schema.as_deref());
            for (column, value) in set {
                if let BackfillSetValue::PerRow { per_row } = value {
                    validate_per_row_destination(
                        table,
                        schema.as_deref(),
                        cursor_columns,
                        column,
                        per_row,
                        declared,
                        schema_mode,
                        missing,
                        target_dialect,
                        op_index,
                        ts_locations,
                    )?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Load-time validation of apply-engine per-row generators. Exact declarations
/// available earlier in this artifact are enforced immediately, including
/// mismatch and ambiguity failures. A genuinely missing declaration is deferred
/// because it may live in an earlier ordered artifact; strict lowering resolves
/// it from [`LogicalColumnContracts`] before an executable backfill exists.
pub(crate) fn validate_per_row_destinations(
    ir: &crate::model::ir::MigrationIr,
    target_dialect: Dialect,
    ts_locations: &[Option<String>],
) -> Result<(), AuthoringError> {
    let mut declared = LogicalColumnContracts::new();
    for (op_index, op) in ir.ops.iter().enumerate() {
        validate_per_row_op(
            op,
            target_dialect,
            op_index,
            ts_locations,
            &mut declared,
            MissingLogicalDeclaration::DeferToLower,
            LogicalSchemaMode::Authored,
        )?;
    }
    Ok(())
}

/// Strict lower-time validation seeded by logical declarations accumulated from
/// earlier ordered artifacts. Returns the declarations after this artifact so a
/// caller can carry the semantic project schema forward without catalog inference.
/// Unqualified declarations and destinations are normalized through the same
/// project/default-schema rule as SQL lowering; strict matching is then exact.
pub(crate) fn validate_per_row_destinations_for_lower(
    ir: &crate::model::ir::MigrationIr,
    target_dialect: Dialect,
    ts_locations: &[Option<String>],
    seed: &LogicalColumnContracts,
    project_schema: &str,
    default_schema: Option<&str>,
) -> Result<LogicalColumnContracts, AuthoringError> {
    let mut declared = seed.clone();
    let schema_mode = LogicalSchemaMode::Effective {
        project_schema,
        default_schema,
    };
    for (op_index, op) in ir.ops.iter().enumerate() {
        validate_per_row_op(
            op,
            target_dialect,
            op_index,
            ts_locations,
            &mut declared,
            MissingLogicalDeclaration::Reject,
            schema_mode,
        )?;
    }
    Ok(declared)
}

/// Replay only the operations that change authored logical column contracts.
///
/// Reference validation uses this as its first pass so a reference may target a
/// table declared later in the same artifact. The selected dialectal leg is the
/// only leg that contributes declarations. Catalog state is intentionally absent:
/// this graph is deterministic authored metadata.
fn collect_logical_declarations_op(
    op: &crate::model::ir::Op,
    target_dialect: Dialect,
    declared: &mut LogicalColumnContracts,
    schema_mode: LogicalSchemaMode<'_>,
) {
    use crate::model::ir::Op;

    match op {
        Op::Dialectal {
            default,
            pg,
            sqlite,
            mysql,
        } => {
            let own = match target_dialect {
                Dialect::Postgres => pg.as_deref(),
                Dialect::Sqlite => sqlite.as_deref(),
                Dialect::Mysql => mysql.as_deref(),
            };
            if let Some(ops) = own.or(default.as_deref()) {
                for inner in ops {
                    collect_logical_declarations_op(inner, target_dialect, declared, schema_mode);
                }
            }
        }
        Op::CreateTable {
            name,
            columns,
            primary_key,
            constraints,
            indexes,
            schema,
            ..
        } => {
            let schema = schema_mode.resolve(schema.as_deref());
            remove_declared_per_row_table(declared, schema_mode, schema.as_deref(), name);
            let reference_keys = create_table_candidate_key_sources(
                name,
                columns,
                primary_key.as_deref(),
                constraints,
                indexes,
            );
            for column in columns {
                declare_logical_column(
                    declared,
                    schema_mode,
                    schema.as_deref(),
                    name,
                    &column.name,
                    column.ty.clone(),
                    column.value_format.clone(),
                    column.case_sensitive,
                    reference_keys.clone(),
                );
            }
        }
        Op::AddColumn {
            table,
            column,
            ty,
            value_format,
            case_sensitive,
            schema,
            ..
        } => {
            let schema = schema_mode.resolve(schema.as_deref());
            declare_logical_column(
                declared,
                schema_mode,
                schema.as_deref(),
                table,
                column,
                ty.clone(),
                value_format.clone(),
                *case_sensitive,
                CandidateKeySources::default(),
            );
        }
        Op::SetColumnType {
            table,
            column,
            to_type,
            schema,
            ..
        } => {
            let schema = schema_mode.resolve(schema.as_deref());
            let reference_keys = existing_candidate_key_sources(
                declared,
                schema_mode,
                schema.as_deref(),
                table,
                column,
            );
            declare_logical_column(
                declared,
                schema_mode,
                schema.as_deref(),
                table,
                column,
                to_type.clone(),
                None,
                None,
                reference_keys,
            );
        }
        Op::DropColumn {
            table,
            column,
            schema,
            ..
        } => {
            let schema = schema_mode.resolve(schema.as_deref());
            declared.retain(|candidate, _| {
                candidate.table != *table
                    || candidate.column != *column
                    || !schema_mode
                        .declarations_match(candidate.schema.as_deref(), schema.as_deref())
            });
        }
        Op::DropTable { table, schema, .. } => {
            let schema = schema_mode.resolve(schema.as_deref());
            remove_declared_per_row_table(declared, schema_mode, schema.as_deref(), table);
        }
        Op::RenameTable {
            table, to, schema, ..
        } => {
            let schema = schema_mode.resolve(schema.as_deref());
            let renamed = declared
                .keys()
                .filter(|candidate| {
                    candidate.table == *table
                        && schema_mode
                            .declarations_match(candidate.schema.as_deref(), schema.as_deref())
                })
                .cloned()
                .collect::<Vec<_>>();
            for from in renamed {
                if let Some(contract) = declared.remove(&from) {
                    declared.insert(
                        LogicalColumnKey {
                            schema: from.schema,
                            table: to.clone(),
                            column: from.column,
                        },
                        contract,
                    );
                }
            }
        }
        Op::RenameColumn {
            table,
            from,
            to,
            ty,
            schema,
            ..
        } => {
            let schema = schema_mode.resolve(schema.as_deref());
            let renamed = declared
                .keys()
                .filter(|candidate| {
                    candidate.table == *table
                        && candidate.column == *from
                        && schema_mode
                            .declarations_match(candidate.schema.as_deref(), schema.as_deref())
                })
                .cloned()
                .collect::<Vec<_>>();
            let found = !renamed.is_empty();
            for old_key in renamed {
                if let Some(mut contract) = declared.remove(&old_key) {
                    contract.ty = ty.clone();
                    declared.insert(
                        LogicalColumnKey {
                            schema: old_key.schema,
                            table: old_key.table,
                            column: to.clone(),
                        },
                        contract,
                    );
                }
            }
            if !found {
                declare_logical_column(
                    declared,
                    schema_mode,
                    schema.as_deref(),
                    table,
                    to,
                    ty.clone(),
                    None,
                    None,
                    CandidateKeySources::default(),
                );
            }
        }
        _ => {}
    }
}

fn logical_reference_types_match(
    local: &crate::model::ir::ColType,
    target: &crate::model::ir::ColType,
) -> bool {
    use crate::model::ir::ColType;

    match (local, target) {
        // These two legacy neutral variants share one unformatted text storage
        // contract. Public `t.text()` records `Text`.
        (ColType::String | ColType::Text, ColType::String | ColType::Text) => true,
        _ => local == target,
    }
}

fn integer_width(ty: &crate::model::ir::ColType) -> Option<u8> {
    use crate::model::ir::ColType;

    match ty {
        ColType::SmallInt => Some(16),
        ColType::Int => Some(32),
        ColType::BigInt => Some(64),
        _ => None,
    }
}

/// Canonical physical storage spelling used only for deterministic authored-side
/// compatibility. Exact logical matching still runs first, so SQLite's broad
/// `INTEGER` and `TEXT` storage classes can never erase UUID semantics, integer
/// width, char length, decimal precision, or named-type identity.
fn lowered_reference_storage(ty: &crate::model::ir::ColType, dialect: Dialect) -> String {
    use crate::model::ir::ColType;

    match ty {
        ColType::String | ColType::Text | ColType::Ref { .. } => "text".to_string(),
        ColType::SmallInt => match dialect {
            Dialect::Sqlite => "integer".to_string(),
            _ => "smallint".to_string(),
        },
        ColType::Int => "integer".to_string(),
        ColType::BigInt => match dialect {
            Dialect::Sqlite => "integer".to_string(),
            _ => "bigint".to_string(),
        },
        ColType::Double => match dialect {
            Dialect::Mysql => "double".to_string(),
            Dialect::Sqlite => "real".to_string(),
            Dialect::Postgres => "double precision".to_string(),
        },
        ColType::Real => "real".to_string(),
        ColType::Boolean => match dialect {
            Dialect::Sqlite => "integer".to_string(),
            _ => "boolean".to_string(),
        },
        ColType::Json => match dialect {
            Dialect::Postgres => "jsonb".to_string(),
            Dialect::Mysql => "json".to_string(),
            Dialect::Sqlite => "text".to_string(),
        },
        ColType::Timestamp => match dialect {
            Dialect::Postgres => "timestamp with time zone".to_string(),
            Dialect::Mysql => "datetime".to_string(),
            Dialect::Sqlite => "text".to_string(),
        },
        ColType::Date => match dialect {
            Dialect::Sqlite => "text".to_string(),
            _ => "date".to_string(),
        },
        ColType::Uuid => match dialect {
            Dialect::Postgres => "uuid".to_string(),
            Dialect::Mysql | Dialect::Sqlite => "text".to_string(),
        },
        ColType::Inet => match dialect {
            Dialect::Postgres => "inet".to_string(),
            Dialect::Mysql | Dialect::Sqlite => "text".to_string(),
        },
        ColType::TextArray => match dialect {
            Dialect::Postgres => "text[]".to_string(),
            Dialect::Mysql | Dialect::Sqlite => "text".to_string(),
        },
        ColType::Bytes | ColType::Encrypted { .. } => match dialect {
            Dialect::Postgres => "bytea".to_string(),
            Dialect::Mysql | Dialect::Sqlite => "blob".to_string(),
        },
        ColType::Char { length } => match dialect {
            Dialect::Sqlite => "text".to_string(),
            _ => format!("char({length})"),
        },
        ColType::Vector { vector } => format!("vector({vector})"),
        ColType::GeoPoint => match dialect {
            Dialect::Postgres => "geography(point,4326)".to_string(),
            Dialect::Mysql | Dialect::Sqlite => "text".to_string(),
        },
        ColType::Decimal { precision, scale } => match dialect {
            Dialect::Postgres => format!("numeric({precision},{scale})"),
            Dialect::Mysql => format!("decimal({precision},{scale})"),
            Dialect::Sqlite => "text".to_string(),
        },
        ColType::Enum { name, schema } => match dialect {
            Dialect::Postgres => schema
                .as_deref()
                .map_or_else(|| name.clone(), |schema| format!("{schema}.{name}")),
            Dialect::Mysql => format!("enum:{name}"),
            Dialect::Sqlite => "text".to_string(),
        },
        ColType::Domain { name, schema } => match dialect {
            Dialect::Postgres => schema
                .as_deref()
                .map_or_else(|| name.clone(), |schema| format!("{schema}.{name}")),
            Dialect::Mysql | Dialect::Sqlite => format!("domain:{name}"),
        },
    }
}

fn reference_is_format_bearing(contract: &LogicalColumnContract) -> bool {
    contract.value_format.is_some() || matches!(contract.ty, crate::model::ir::ColType::Uuid)
}

fn reference_format_description(contract: &LogicalColumnContract) -> String {
    match &contract.value_format {
        Some(crate::model::ir::ValueFormat::TypeId { prefix }) => {
            format!("TypeID(prefix={prefix:?})")
        }
        Some(crate::model::ir::ValueFormat::Ulid) => "ULID".to_string(),
        None if matches!(contract.ty, crate::model::ir::ColType::Uuid) => {
            "canonical UUID".to_string()
        }
        None => "no value format".to_string(),
    }
}

fn reference_validation_error(
    local: &LogicalColumnKey,
    reference: &crate::model::ir::ColumnReference,
    target_dialect: Dialect,
    op_index: usize,
    ts_locations: &[Option<String>],
    reason: String,
    suggested_fix: String,
) -> AuthoringError {
    let local_table = local.schema.as_deref().map_or_else(
        || local.table.clone(),
        |schema| format!("{schema}.{}", local.table),
    );
    let target_table = local.schema.as_deref().map_or_else(
        || reference.table.clone(),
        |schema| format!("{schema}.{}", reference.table),
    );
    AuthoringError {
        code: CODE_OP_INVALID.to_string(),
        kind: Some(UnsupportedKind::Op),
        op_index,
        ts_location: ts_locations.get(op_index).cloned().flatten(),
        dialect: target_dialect,
        reason: format!(
            "typed reference {local_table}.{} -> {target_table}.{} is incompatible: {reason}",
            local.column, reference.column
        ),
        suggested_fix: Some(suggested_fix),
    }
}

fn validate_one_column_reference(
    local: &LogicalColumnKey,
    local_contract: &LogicalColumnContract,
    reference: &crate::model::ir::ColumnReference,
    declared: &LogicalColumnContracts,
    schema_mode: LogicalSchemaMode<'_>,
    missing: MissingLogicalDeclaration,
    target_dialect: Dialect,
    op_index: usize,
    ts_locations: &[Option<String>],
) -> Result<(), AuthoringError> {
    let matches = declared
        .iter()
        .filter(|(candidate, _)| {
            candidate.table == reference.table
                && candidate.column == reference.column
                && schema_mode
                    .destination_matches(candidate.schema.as_deref(), local.schema.as_deref())
        })
        .map(|(_, contract)| contract)
        .collect::<Vec<_>>();

    let target = match matches.as_slice() {
        [] => {
            if missing == MissingLogicalDeclaration::Reject
                && reference_is_format_bearing(local_contract)
            {
                return Err(reference_validation_error(
                    local,
                    reference,
                    target_dialect,
                    op_index,
                    ts_locations,
                    format!(
                        "the local column carries {}, but the referenced target has no authored value-format metadata in the project graph",
                        reference_format_description(local_contract)
                    ),
                    "declare or import the referenced key with the exact same value format; a live catalog may validate recorded metadata but cannot supply it"
                        .to_string(),
                ));
            }
            // Plain primitive references may be proved physically from the live
            // catalog by lower. Load also defers any genuinely cross-artifact
            // target until the ordered graph is available.
            return Ok(());
        }
        [target] => *target,
        _ => {
            return Err(reference_validation_error(
                local,
                reference,
                target_dialect,
                op_index,
                ts_locations,
                format!(
                    "the target resolves to {} authored column declarations",
                    matches.len()
                ),
                "qualify the surrounding createTable schema so the referenced target has one deterministic authored contract"
                    .to_string(),
            ));
        }
    };

    if !target.single_column_reference_key {
        return Err(reference_validation_error(
            local,
            reference,
            target_dialect,
            op_index,
            ts_locations,
            "the declared target is not an eligible single-column primary or unique key"
                .to_string(),
            "mark the referenced target column primaryKey()/unique(), or declare a one-column primaryKey/UNIQUE table constraint; a component of a composite key is not independently referenceable"
                .to_string(),
        ));
    }

    if let (Some(local_width), Some(target_width)) =
        (integer_width(&local_contract.ty), integer_width(&target.ty))
    {
        if local_width != target_width {
            return Err(reference_validation_error(
                local,
                reference,
                target_dialect,
                op_index,
                ts_locations,
                format!(
                    "logical integer width differs ({local_width}-bit local vs {target_width}-bit target), even if this dialect lowers both to INTEGER"
                ),
                "use the same explicit integer builder on both sides (for example, t.bigInt() on both columns)"
                    .to_string(),
            ));
        }
    }

    let local_storage = lowered_reference_storage(&local_contract.ty, target_dialect);
    let target_storage = lowered_reference_storage(&target.ty, target_dialect);
    if !logical_reference_types_match(&local_contract.ty, &target.ty) {
        return Err(reference_validation_error(
            local,
            reference,
            target_dialect,
            op_index,
            ts_locations,
            format!(
                "logical column types differ ({:?} local vs {:?} target; lowered storage {local_storage:?} vs {target_storage:?})",
                local_contract.ty, target.ty
            ),
            "declare the local reference with the same explicit logical type as the referenced key"
                .to_string(),
        ));
    }
    if local_storage != target_storage {
        return Err(reference_validation_error(
            local,
            reference,
            target_dialect,
            op_index,
            ts_locations,
            format!(
                "lowered storage types differ ({local_storage:?} local vs {target_storage:?} target)"
            ),
            "choose a local type whose lowered storage exactly matches the referenced key on this dialect"
                .to_string(),
        ));
    }

    if local_contract.value_format != target.value_format {
        return Err(reference_validation_error(
            local,
            reference,
            target_dialect,
            op_index,
            ts_locations,
            format!(
                "value formats differ ({} local vs {} target)",
                reference_format_description(local_contract),
                reference_format_description(target)
            ),
            "use the exact same value-format helper and TypeID prefix on both sides".to_string(),
        ));
    }

    let local_case_sensitive = local_contract.case_sensitive.unwrap_or(true);
    let target_case_sensitive = target.case_sensitive.unwrap_or(true);
    if local_case_sensitive != target_case_sensitive {
        return Err(reference_validation_error(
            local,
            reference,
            target_dialect,
            op_index,
            ts_locations,
            format!(
                "collation intent differs (caseSensitive={local_case_sensitive} local vs caseSensitive={target_case_sensitive} target)"
            ),
            "use the same caseSensitive/collation intent on both sides of the reference"
                .to_string(),
        ));
    }

    Ok(())
}

fn validate_column_references_op(
    op: &crate::model::ir::Op,
    target_dialect: Dialect,
    op_index: usize,
    ts_locations: &[Option<String>],
    declared: &LogicalColumnContracts,
    schema_mode: LogicalSchemaMode<'_>,
    missing: MissingLogicalDeclaration,
) -> Result<(), AuthoringError> {
    use crate::model::ir::Op;

    match op {
        Op::Dialectal {
            default,
            pg,
            sqlite,
            mysql,
        } => {
            let own = match target_dialect {
                Dialect::Postgres => pg.as_deref(),
                Dialect::Sqlite => sqlite.as_deref(),
                Dialect::Mysql => mysql.as_deref(),
            };
            if let Some(ops) = own.or(default.as_deref()) {
                for inner in ops {
                    validate_column_references_op(
                        inner,
                        target_dialect,
                        op_index,
                        ts_locations,
                        declared,
                        schema_mode,
                        missing,
                    )?;
                }
            }
        }
        Op::CreateTable {
            name,
            columns,
            schema,
            ..
        } => {
            let schema = schema_mode.resolve(schema.as_deref());
            for column in columns {
                let Some(reference) = &column.references else {
                    continue;
                };
                let local = LogicalColumnKey {
                    schema: schema.clone(),
                    table: name.clone(),
                    column: column.name.clone(),
                };
                let local_contract = LogicalColumnContract {
                    ty: column.ty.clone(),
                    value_format: column.value_format.clone(),
                    case_sensitive: column.case_sensitive,
                    single_column_reference_key: false,
                    candidate_keys: BTreeSet::new(),
                    candidate_key_sources: CandidateKeySources::default(),
                };
                validate_one_column_reference(
                    &local,
                    &local_contract,
                    reference,
                    declared,
                    schema_mode,
                    missing,
                    target_dialect,
                    op_index,
                    ts_locations,
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Load-time two-pass validation for typed single-column references.
///
/// Every declaration in the selected artifact leg is collected before any
/// reference is checked, so targets declared later in the artifact are visible.
/// A missing target is deferred because it may be declared by an earlier ordered
/// artifact whose graph is available only at lower time.
fn validate_column_references(
    ir: &crate::model::ir::MigrationIr,
    target_dialect: Dialect,
    ts_locations: &[Option<String>],
) -> Result<(), AuthoringError> {
    let schema_mode = LogicalSchemaMode::Authored;
    let mut declared = LogicalColumnContracts::new();
    for op in &ir.ops {
        collect_logical_declarations_op(op, target_dialect, &mut declared, schema_mode);
    }
    for (op_index, op) in ir.ops.iter().enumerate() {
        validate_column_references_op(
            op,
            target_dialect,
            op_index,
            ts_locations,
            &declared,
            schema_mode,
            MissingLogicalDeclaration::DeferToLower,
        )?;
    }
    Ok(())
}

/// Strict lower-time two-pass validation for typed single-column references.
///
/// `seed` contains authored logical declarations retained from earlier ordered
/// artifacts. Current declarations are overlaid before validation, so forward
/// references within this artifact are deterministic. Missing format-bearing
/// targets are rejected because a catalog cannot recover `ValueFormat`; missing
/// plain primitives are left for lower's physical catalog compatibility check.
///
/// # Errors
/// Returns [`CODE_OP_INVALID`] for an ambiguous target, a logical/storage/format/
/// collation mismatch, or a formatted reference with no authored target metadata.
pub(crate) fn validate_column_references_for_lower(
    ir: &crate::model::ir::MigrationIr,
    target_dialect: Dialect,
    ts_locations: &[Option<String>],
    seed: &LogicalColumnContracts,
    project_schema: &str,
    default_schema: Option<&str>,
) -> Result<(), AuthoringError> {
    let schema_mode = LogicalSchemaMode::Effective {
        project_schema,
        default_schema,
    };
    let mut declared = seed.clone();
    for op in &ir.ops {
        collect_logical_declarations_op(op, target_dialect, &mut declared, schema_mode);
    }
    for (op_index, op) in ir.ops.iter().enumerate() {
        validate_column_references_op(
            op,
            target_dialect,
            op_index,
            ts_locations,
            &declared,
            schema_mode,
            MissingLogicalDeclaration::Reject,
        )?;
    }
    Ok(())
}

fn table_foreign_key_error(
    table: &str,
    constraint_name: Option<&str>,
    target_dialect: Dialect,
    op_index: usize,
    ts_locations: &[Option<String>],
    reason: String,
    suggested_fix: String,
) -> AuthoringError {
    let name = constraint_name.unwrap_or("<derived>");
    AuthoringError {
        code: CODE_OP_INVALID.to_string(),
        kind: Some(UnsupportedKind::Op),
        op_index,
        ts_location: ts_locations.get(op_index).cloned().flatten(),
        dialect: target_dialect,
        reason: format!("table-level foreign key {table}.{name} is invalid: {reason}"),
        suggested_fix: Some(suggested_fix),
    }
}

fn logical_column_matches<'a>(
    declared: &'a LogicalColumnContracts,
    schema_mode: LogicalSchemaMode<'_>,
    schema: Option<&str>,
    table: &str,
    column: &str,
) -> Vec<&'a LogicalColumnContract> {
    declared
        .iter()
        .filter(|(candidate, _)| {
            candidate.table == table
                && candidate.column == column
                && schema_mode.destination_matches(candidate.schema.as_deref(), schema)
        })
        .map(|(_, contract)| contract)
        .collect()
}

fn logical_table_is_declared(
    declared: &LogicalColumnContracts,
    schema_mode: LogicalSchemaMode<'_>,
    schema: Option<&str>,
    table: &str,
) -> bool {
    declared.keys().any(|candidate| {
        candidate.table == table
            && schema_mode.destination_matches(candidate.schema.as_deref(), schema)
    })
}

#[allow(clippy::too_many_arguments)]
fn validate_table_foreign_key_constraint(
    local_schema: Option<&str>,
    local_table: &str,
    constraint: &crate::model::ir::IrConstraint,
    declared: &LogicalColumnContracts,
    schema_mode: LogicalSchemaMode<'_>,
    missing: MissingLogicalDeclaration,
    target_dialect: Dialect,
    op_index: usize,
    ts_locations: &[Option<String>],
) -> Result<(), AuthoringError> {
    use crate::model::ir::IrConstraintKind;

    let IrConstraintKind::Fk {
        columns,
        references_table,
        references_columns,
        deferrable,
        initially_deferred,
        ..
    } = &constraint.kind
    else {
        return Ok(());
    };

    let error = |reason: String, fix: &str| {
        table_foreign_key_error(
            local_table,
            constraint.name.as_deref(),
            target_dialect,
            op_index,
            ts_locations,
            reason,
            fix.to_string(),
        )
    };

    if columns.is_empty() || references_columns.is_empty() {
        return Err(error(
            format!(
                "local and referenced column lists must both be nonempty (got {} local and {} referenced)",
                columns.len(),
                references_columns.len()
            ),
            "name one or more ordered local columns and the same number of ordered referenced columns",
        ));
    }
    if columns.len() != references_columns.len() {
        return Err(error(
            format!(
                "local/referenced arity differs ({} local columns vs {} referenced columns)",
                columns.len(),
                references_columns.len()
            ),
            "make columns and references.columns equal-length ordered tuples",
        ));
    }
    for (label, names) in [
        ("local", columns.as_slice()),
        ("referenced", references_columns.as_slice()),
    ] {
        let mut seen = BTreeSet::new();
        if let Some(duplicate) = names.iter().find(|name| !seen.insert(name.as_str())) {
            return Err(error(
                format!("{label} column {duplicate:?} appears more than once"),
                "remove duplicate columns while preserving the intended positional order",
            ));
        }
    }
    if target_dialect == Dialect::Mysql
        && (deferrable == &Some(true) || initially_deferred == &Some(true))
    {
        return Err(error(
            "MySQL does not support deferrable foreign-key constraints".to_string(),
            "omit deferrable/initiallyDeferred for MySQL, or use a dialectal PostgreSQL/SQLite leg",
        ));
    }

    let local_table_declared =
        logical_table_is_declared(declared, schema_mode, local_schema, local_table);
    let target_table_declared =
        logical_table_is_declared(declared, schema_mode, local_schema, references_table);
    let mut target_contracts = Vec::with_capacity(references_columns.len());

    for (position, (local_column, target_column)) in
        columns.iter().zip(references_columns).enumerate()
    {
        let local_matches = logical_column_matches(
            declared,
            schema_mode,
            local_schema,
            local_table,
            local_column,
        );
        let target_matches = logical_column_matches(
            declared,
            schema_mode,
            local_schema,
            references_table,
            target_column,
        );

        let local = match local_matches.as_slice() {
            [contract] => Some(*contract),
            [] if !local_table_declared => None,
            [] => {
                return Err(error(
                    format!("local column {local_column:?} is missing from the declared table"),
                    "name only columns present on the local table",
                ));
            }
            _ => {
                return Err(error(
                    format!("local column {local_column:?} resolves ambiguously"),
                    "qualify the table schema so each local column has one deterministic declaration",
                ));
            }
        };
        let target = match target_matches.as_slice() {
            [contract] => Some(*contract),
            [] if !target_table_declared => None,
            [] => {
                return Err(error(
                    format!(
                        "referenced column {references_table}.{target_column} is missing from the declared target table"
                    ),
                    "name only columns present on the referenced table",
                ));
            }
            _ => {
                return Err(error(
                    format!(
                        "referenced column {references_table}.{target_column} resolves ambiguously"
                    ),
                    "qualify the table schema so each referenced column has one deterministic declaration",
                ));
            }
        };

        if let (Some(local), None) = (local, target) {
            if missing == MissingLogicalDeclaration::Reject && reference_is_format_bearing(local) {
                return Err(error(
                    format!(
                        "position {} local column {local_column:?} carries {}, but the referenced target has no authored value-format metadata",
                        position + 1,
                        reference_format_description(local)
                    ),
                    "declare or import the referenced candidate key with the exact same value format",
                ));
            }
        }

        if let (Some(local), Some(target)) = (local, target) {
            if let (Some(local_width), Some(target_width)) =
                (integer_width(&local.ty), integer_width(&target.ty))
            {
                if local_width != target_width {
                    return Err(error(
                        format!(
                            "position {} integer width differs ({local_width}-bit local {local_column:?} vs {target_width}-bit referenced {target_column:?})",
                            position + 1
                        ),
                        "use the same explicit integer builder at each corresponding tuple position",
                    ));
                }
            }
            let local_storage = lowered_reference_storage(&local.ty, target_dialect);
            let target_storage = lowered_reference_storage(&target.ty, target_dialect);
            if !logical_reference_types_match(&local.ty, &target.ty)
                || local_storage != target_storage
            {
                return Err(error(
                    format!(
                        "position {} logical/storage type differs ({:?}/{local_storage} local {local_column:?} vs {:?}/{target_storage} referenced {target_column:?})",
                        position + 1,
                        local.ty,
                        target.ty
                    ),
                    "declare positionally corresponding columns with the same logical storage type",
                ));
            }
            if local.value_format != target.value_format {
                return Err(error(
                    format!(
                        "position {} value format differs ({} local vs {} referenced)",
                        position + 1,
                        reference_format_description(local),
                        reference_format_description(target)
                    ),
                    "use the exact same ValueFormat, TypeID prefix, or ULID declaration at each tuple position",
                ));
            }
            let local_case_sensitive = local.case_sensitive.unwrap_or(true);
            let target_case_sensitive = target.case_sensitive.unwrap_or(true);
            if local_case_sensitive != target_case_sensitive {
                return Err(error(
                    format!(
                        "position {} collation intent differs (caseSensitive={local_case_sensitive} local vs caseSensitive={target_case_sensitive} referenced)",
                        position + 1
                    ),
                    "use matching collation/caseSensitive intent at each tuple position",
                ));
            }
        }
        target_contracts.push(target);
    }

    if target_table_declared {
        let Some(target) = target_contracts.first().copied().flatten() else {
            return Err(error(
                "the referenced tuple could not be resolved".to_string(),
                "declare every referenced tuple column",
            ));
        };
        if !target.candidate_keys.contains(references_columns) {
            return Err(error(
                format!(
                    "referenced ordered tuple {references_table}({}) is not backed by an exact PRIMARY KEY or UNIQUE candidate key",
                    references_columns.join(", ")
                ),
                "reference an exact ordered primary/unique candidate key; a reordered, partial, or wider key is not equivalent",
            ));
        }
    }

    Ok(())
}

fn validate_table_foreign_keys_op(
    op: &crate::model::ir::Op,
    target_dialect: Dialect,
    op_index: usize,
    ts_locations: &[Option<String>],
    declared: &mut LogicalColumnContracts,
    schema_mode: LogicalSchemaMode<'_>,
    missing: MissingLogicalDeclaration,
) -> Result<(), AuthoringError> {
    use crate::model::ir::Op;

    match op {
        Op::Dialectal {
            default,
            pg,
            sqlite,
            mysql,
        } => {
            let own = match target_dialect {
                Dialect::Postgres => pg.as_deref(),
                Dialect::Sqlite => sqlite.as_deref(),
                Dialect::Mysql => mysql.as_deref(),
            };
            if let Some(ops) = own.or(default.as_deref()) {
                for inner in ops {
                    validate_table_foreign_keys_op(
                        inner,
                        target_dialect,
                        op_index,
                        ts_locations,
                        declared,
                        schema_mode,
                        missing,
                    )?;
                }
            }
        }
        Op::CreateTable {
            name,
            columns,
            primary_key,
            constraints,
            indexes,
            schema,
            ..
        } => {
            let schema = schema_mode.resolve(schema.as_deref());
            reset_create_table_candidate_keys(
                declared,
                schema_mode,
                schema.as_deref(),
                name,
                columns,
                primary_key.as_deref(),
                constraints,
                indexes,
            );
            for constraint in constraints {
                validate_table_foreign_key_constraint(
                    schema.as_deref(),
                    name,
                    constraint,
                    declared,
                    schema_mode,
                    missing,
                    target_dialect,
                    op_index,
                    ts_locations,
                )?;
            }
        }
        Op::AddConstraint {
            table,
            constraint,
            schema,
            ..
        } => {
            let schema = schema_mode.resolve(schema.as_deref());
            add_unique_constraint_candidate_key(
                declared,
                schema_mode,
                schema.as_deref(),
                table,
                constraint,
            );
            validate_table_foreign_key_constraint(
                schema.as_deref(),
                table,
                constraint,
                declared,
                schema_mode,
                missing,
                target_dialect,
                op_index,
                ts_locations,
            )?;
        }
        Op::CreateIndex {
            table,
            columns,
            name,
            unique,
            using,
            r#where,
            only,
            schema,
            ..
        } => {
            let schema = schema_mode.resolve(schema.as_deref());
            add_index_candidate_key(
                declared,
                schema_mode,
                schema.as_deref(),
                table,
                columns,
                name.as_deref(),
                *unique,
                *using,
                r#where.as_ref(),
                *only,
            );
        }
        Op::DropIndex {
            table: Some(table),
            name,
            schema,
            ..
        } => {
            let schema = schema_mode.resolve(schema.as_deref());
            drop_index_candidate_key(declared, schema_mode, schema.as_deref(), table, name);
        }
        Op::DropConstraint {
            table,
            name,
            schema,
            ..
        } => {
            let schema = schema_mode.resolve(schema.as_deref());
            drop_constraint_candidate_key(declared, schema_mode, schema.as_deref(), table, name);
        }
        Op::AlterPrimaryKey {
            table,
            action,
            schema,
        } => {
            let schema = schema_mode.resolve(schema.as_deref());
            alter_primary_key_candidate_key(
                declared,
                schema_mode,
                schema.as_deref(),
                table,
                action,
            );
        }
        _ => {}
    }
    Ok(())
}

fn validate_table_foreign_keys(
    ir: &crate::model::ir::MigrationIr,
    target_dialect: Dialect,
    ts_locations: &[Option<String>],
) -> Result<(), AuthoringError> {
    let schema_mode = LogicalSchemaMode::Authored;
    let mut declared = LogicalColumnContracts::new();
    for op in &ir.ops {
        collect_logical_declarations_op(op, target_dialect, &mut declared, schema_mode);
    }
    for (op_index, op) in ir.ops.iter().enumerate() {
        validate_table_foreign_keys_op(
            op,
            target_dialect,
            op_index,
            ts_locations,
            &mut declared,
            schema_mode,
            MissingLogicalDeclaration::DeferToLower,
        )?;
    }
    Ok(())
}

/// Strict ordered-tuple validation for table-level foreign keys. The authored
/// declaration graph is authoritative for logical types and value formats; an
/// unmanaged primitive target may still be proved from the live catalog by the
/// lowerer.
pub(crate) fn validate_table_foreign_keys_for_lower(
    ir: &crate::model::ir::MigrationIr,
    target_dialect: Dialect,
    ts_locations: &[Option<String>],
    seed: &LogicalColumnContracts,
    project_schema: &str,
    default_schema: Option<&str>,
) -> Result<(), AuthoringError> {
    let schema_mode = LogicalSchemaMode::Effective {
        project_schema,
        default_schema,
    };
    let mut declared = seed.clone();
    for op in &ir.ops {
        collect_logical_declarations_op(op, target_dialect, &mut declared, schema_mode);
    }
    for (op_index, op) in ir.ops.iter().enumerate() {
        validate_table_foreign_keys_op(
            op,
            target_dialect,
            op_index,
            ts_locations,
            &mut declared,
            schema_mode,
            MissingLogicalDeclaration::Reject,
        )?;
    }
    Ok(())
}

fn validate_online_rename_isolation_op<'a>(
    op: &'a crate::model::ir::Op,
    target_dialect: Dialect,
    op_index: usize,
    ts_locations: &[Option<String>],
    seen: &mut Vec<TableOperationTarget<'a>>,
) -> Result<(), AuthoringError> {
    use crate::model::ir::Op;

    match op {
        Op::Dialectal {
            default,
            pg,
            sqlite,
            mysql,
        } => {
            let own = match target_dialect {
                Dialect::Postgres => pg.as_deref(),
                Dialect::Sqlite => sqlite.as_deref(),
                Dialect::Mysql => mysql.as_deref(),
            };
            if let Some(ops) = own.or(default.as_deref()) {
                for inner in ops {
                    validate_online_rename_isolation_op(
                        inner,
                        target_dialect,
                        op_index,
                        ts_locations,
                        seen,
                    )?;
                }
            }
        }
        table_op => {
            let Some(table) = table_op.touched_table() else {
                return Ok(());
            };
            let schema = table_op.schema();
            let is_online_rename = matches!(table_op, Op::RenameColumn { .. });
            if let Some(previous) = seen.iter().find(|previous| {
                previous.table == table
                    && schemas_may_name_same_table(previous.schema, schema)
                    && (previous.is_online_rename || is_online_rename)
            }) {
                let rename_schema = if is_online_rename {
                    schema
                } else {
                    previous.schema
                };
                let qualified_table = rename_schema
                    .map_or_else(|| table.to_string(), |schema| format!("{schema}.{table}"));
                return Err(AuthoringError {
                    code: CODE_OP_INVALID.to_string(),
                    kind: Some(UnsupportedKind::Op),
                    op_index,
                    ts_location: ts_locations.get(op_index).cloned().flatten(),
                    dialect: target_dialect,
                    reason: format!(
                        "renameColumn must be the only operation targeting table \
                         {qualified_table:?} in a migration; it conflicts with another operation \
                         at op index {}",
                        previous.op_index
                    ),
                    suggested_fix: Some(format!(
                        "keep the renameColumn for {qualified_table:?} in its own migration; move \
                         every other operation on that table into a later migration and apply it \
                         only after the rename is resolved"
                    )),
                });
            }
            seen.push(TableOperationTarget {
                schema,
                table,
                op_index,
                is_online_rename,
            });
        }
    }
    Ok(())
}

fn validate_online_rename_sequence(
    ir: &crate::model::ir::MigrationIr,
    target_dialect: Dialect,
    ts_locations: &[Option<String>],
) -> Result<(), AuthoringError> {
    // PostgreSQL keeps an online rename open across deploys, so every other
    // operation on that table must wait for resolution. SQLite performs the
    // rename as one rebuild and has no pending obligation; MySQL refuses the
    // rename through its dialect-support gate.
    if target_dialect != Dialect::Postgres {
        return Ok(());
    }
    let mut seen = Vec::new();
    for (op_index, op) in ir.ops.iter().enumerate() {
        validate_online_rename_isolation_op(op, target_dialect, op_index, ts_locations, &mut seen)?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct PartitionUniqueEntry {
    op_index: usize,
    label: &'static str,
    columns: Vec<String>,
}

#[derive(Debug, Clone)]
struct PartitionParentFold {
    op_index: usize,
    spec: crate::model::ir::PartitionSpec,
    not_null_columns: std::collections::BTreeSet<String>,
    unique_entries: Vec<PartitionUniqueEntry>,
    children: std::collections::BTreeMap<String, (usize, crate::model::ir::PartitionBounds)>,
}

fn partition_error(
    code: &'static str,
    op_index: usize,
    ts_locations: &[Option<String>],
    dialect: Dialect,
    reason: impl Into<String>,
    suggested_fix: impl Into<String>,
) -> AuthoringError {
    AuthoringError {
        code: code.to_string(),
        kind: None,
        op_index,
        ts_location: ts_locations.get(op_index).cloned().flatten(),
        dialect,
        reason: reason.into(),
        suggested_fix: Some(suggested_fix.into()),
    }
}

fn partition_spec_label(spec: &crate::model::ir::PartitionSpec) -> &'static str {
    match spec {
        crate::model::ir::PartitionSpec::Range { .. } => "range",
        crate::model::ir::PartitionSpec::List { .. } => "list",
        crate::model::ir::PartitionSpec::Hash { .. } => "hash",
    }
}

fn index_column_names(index_columns: &[crate::model::ir::IndexElement]) -> Vec<String> {
    index_columns
        .iter()
        .filter_map(|element| match element {
            crate::model::ir::IndexElement::Column { name, .. } => Some(name.clone()),
            crate::model::ir::IndexElement::Expr { .. } => None,
        })
        .collect()
}

fn exclusion_column_names(elements: &[crate::model::ir::ExclusionElement]) -> Vec<String> {
    elements
        .iter()
        .filter_map(|element| match &element.target {
            crate::model::ir::ColumnOrExpr::Column { name } => Some(name.clone()),
            crate::model::ir::ColumnOrExpr::Expr { .. } => None,
        })
        .collect()
}

fn partition_bound_key(value: &crate::model::ir::PartitionBoundValue) -> String {
    match value {
        crate::model::ir::PartitionBoundValue::String { value } => format!("s:{value}"),
        crate::model::ir::PartitionBoundValue::Int { value } => format!("i:{}", value.get()),
        crate::model::ir::PartitionBoundValue::MinValue => "min".to_string(),
        crate::model::ir::PartitionBoundValue::MaxValue => "max".to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PartitionComparableBound<'a> {
    Min,
    Int(i64),
    String(&'a str),
    Max,
}

fn comparable_bound(value: &crate::model::ir::PartitionBoundValue) -> PartitionComparableBound<'_> {
    match value {
        crate::model::ir::PartitionBoundValue::String { value } => {
            PartitionComparableBound::String(value)
        }
        crate::model::ir::PartitionBoundValue::Int { value } => {
            PartitionComparableBound::Int(value.get())
        }
        crate::model::ir::PartitionBoundValue::MinValue => PartitionComparableBound::Min,
        crate::model::ir::PartitionBoundValue::MaxValue => PartitionComparableBound::Max,
    }
}

fn compare_bound_tuple(
    lhs: &[crate::model::ir::PartitionBoundValue],
    rhs: &[crate::model::ir::PartitionBoundValue],
) -> Option<std::cmp::Ordering> {
    if lhs.len() != rhs.len() {
        return None;
    }
    for (l, r) in lhs.iter().zip(rhs) {
        let l = comparable_bound(l);
        let r = comparable_bound(r);
        match (l, r) {
            (PartitionComparableBound::Int(_), PartitionComparableBound::String(_))
            | (PartitionComparableBound::String(_), PartitionComparableBound::Int(_)) => {
                return None;
            }
            _ => {}
        }
        let ord = l.cmp(&r);
        if !ord.is_eq() {
            return Some(ord);
        }
    }
    Some(std::cmp::Ordering::Equal)
}

fn hash_gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        let r = a % b;
        a = b;
        b = r;
    }
    a
}

fn hash_lcm(a: u128, b: u128) -> Option<u128> {
    if a == 0 || b == 0 {
        return None;
    }
    a.checked_div(hash_gcd(a, b))?.checked_mul(b)
}

fn validate_partition_recording(
    ir: &crate::model::ir::MigrationIr,
    target_dialect: Dialect,
    ts_locations: &[Option<String>],
) -> Result<(), AuthoringError> {
    use crate::model::ir::{IrConstraintKind, Op, PartitionSpec};

    let mut parents: std::collections::BTreeMap<String, PartitionParentFold> =
        std::collections::BTreeMap::new();

    for (op_index, op) in ir.ops.iter().enumerate() {
        match op {
            Op::CreateTable {
                name,
                columns,
                primary_key,
                constraints,
                indexes,
                partition_by: Some(spec),
                ..
            } => {
                let mut not_null_columns = std::collections::BTreeSet::new();
                for column in columns {
                    if column.nullable == Some(false) {
                        not_null_columns.insert(column.name.clone());
                    }
                }
                let mut unique_entries = Vec::new();
                if let Some(pk) = primary_key {
                    for column in pk {
                        not_null_columns.insert(column.clone());
                    }
                    unique_entries.push(PartitionUniqueEntry {
                        op_index,
                        label: "primary key",
                        columns: pk.clone(),
                    });
                }
                for column in columns {
                    if column.unique.unwrap_or(false) {
                        unique_entries.push(PartitionUniqueEntry {
                            op_index,
                            label: "column unique",
                            columns: vec![column.name.clone()],
                        });
                    }
                }
                for constraint in constraints {
                    match &constraint.kind {
                        IrConstraintKind::Unique { columns } => {
                            unique_entries.push(PartitionUniqueEntry {
                                op_index,
                                label: "unique constraint",
                                columns: columns.clone(),
                            });
                        }
                        IrConstraintKind::Exclusion { elements, .. } => {
                            unique_entries.push(PartitionUniqueEntry {
                                op_index,
                                label: "exclusion constraint",
                                columns: exclusion_column_names(elements),
                            });
                        }
                        _ => {}
                    }
                }
                for index in indexes {
                    if index.unique.unwrap_or(false) {
                        unique_entries.push(PartitionUniqueEntry {
                            op_index,
                            label: "unique index",
                            columns: index_column_names(&index.columns),
                        });
                    }
                }
                parents.insert(
                    name.clone(),
                    PartitionParentFold {
                        op_index,
                        spec: spec.clone(),
                        not_null_columns,
                        unique_entries,
                        children: std::collections::BTreeMap::new(),
                    },
                );
            }
            Op::CreateTable { name, .. } => {
                parents.remove(name);
            }
            Op::DropTable { table, .. } => {
                parents.remove(table);
            }
            Op::RenameTable { table, to, .. } => {
                if let Some(parent) = parents.remove(table) {
                    parents.insert(to.clone(), parent);
                }
            }
            Op::CreatePartition {
                name, of, bounds, ..
            } => {
                if let Some(parent) = parents.get_mut(of) {
                    parent
                        .children
                        .insert(name.clone(), (op_index, bounds.clone()));
                } else if !matches!(target_dialect, Dialect::Postgres) {
                    return Err(partition_error(
                        CODE_DIALECT_UNSUPPORTED,
                        op_index,
                        ts_locations,
                        target_dialect,
                        format!(
                            "createPartition {name:?} targets parent {of:?}, but this recording does not contain a collapse-affirmed partitioned parent to authorize the no-DDL leg"
                        ),
                        "record the partitioned parent with partitionBy.whenUnsupported: \"collapse\" in the same fold, or target Postgres for native partition DDL",
                    ));
                }
            }
            Op::AttachPartition {
                parent,
                name,
                bound,
                ..
            } => {
                if let Some(parent) = parents.get_mut(parent) {
                    parent
                        .children
                        .insert(name.clone(), (op_index, bound.clone()));
                } else if !matches!(target_dialect, Dialect::Postgres) {
                    return Err(partition_error(
                        CODE_DIALECT_UNSUPPORTED,
                        op_index,
                        ts_locations,
                        target_dialect,
                        format!(
                            "attachPartition {name:?} targets parent {parent:?}, but attachPartition is PostgreSQL-only"
                        ),
                        "target Postgres for native partition attach",
                    ));
                }
            }
            Op::DropPartition { parent, name, .. } => {
                if let Some(parent_state) = parents.get(parent) {
                    if parent_state.spec.collapse()
                        && parent_state.children.get(name).is_some_and(|(_, bounds)| {
                            matches!(bounds, crate::model::ir::PartitionBounds::Hash { .. })
                        })
                    {
                        return Err(partition_error(
                            CODE_PARTITION_HASH_DROP_UNDERIVABLE,
                            op_index,
                            ts_locations,
                            target_dialect,
                            format!(
                                "dropping hash partition {name:?} from collapse-affirmed parent {parent:?} has no portable row predicate"
                            ),
                            "omit partitionBy.whenUnsupported for PG-only hash repartitioning, or avoid dropping hash children under collapse",
                        ));
                    }
                }
                if let Some(parent_state) = parents.get_mut(parent) {
                    parent_state.children.remove(name);
                }
            }
            Op::SetColumnNotNull { table, column, .. } => {
                if let Some(parent) = parents.get_mut(table) {
                    parent.not_null_columns.insert(column.clone());
                }
            }
            Op::DropColumnNotNull { table, column, .. } => {
                if let Some(parent) = parents.get_mut(table) {
                    parent.not_null_columns.remove(column);
                }
            }
            Op::AddConstraint {
                table, constraint, ..
            } => {
                if let Some(parent) = parents.get_mut(table) {
                    match &constraint.kind {
                        IrConstraintKind::Unique { columns } => {
                            parent.unique_entries.push(PartitionUniqueEntry {
                                op_index,
                                label: "unique constraint",
                                columns: columns.clone(),
                            });
                        }
                        IrConstraintKind::Exclusion { elements, .. } => {
                            parent.unique_entries.push(PartitionUniqueEntry {
                                op_index,
                                label: "exclusion constraint",
                                columns: exclusion_column_names(elements),
                            });
                        }
                        _ => {}
                    }
                }
            }
            Op::CreateIndex {
                table,
                columns,
                unique,
                ..
            } if unique.unwrap_or(false) => {
                if let Some(parent) = parents.get_mut(table) {
                    parent.unique_entries.push(PartitionUniqueEntry {
                        op_index,
                        label: "unique index",
                        columns: index_column_names(columns),
                    });
                }
            }
            _ => {}
        }
    }

    for (table, parent) in &parents {
        let key_columns = parent.spec.columns();
        for entry in &parent.unique_entries {
            let cols: std::collections::BTreeSet<&str> =
                entry.columns.iter().map(String::as_str).collect();
            if let Some(missing) = key_columns.iter().find(|key| !cols.contains(key.as_str())) {
                return Err(partition_error(
                    CODE_PARTITION_KEY_COVERAGE,
                    entry.op_index,
                    ts_locations,
                    target_dialect,
                    format!(
                        "partitioned table {table:?} has a {} that does not include partition key column {missing:?}",
                        entry.label
                    ),
                    "include every partition key column in each primary key, unique constraint, unique index, and exclusion constraint on the partitioned table",
                ));
            }
        }

        validate_partition_bounds_well_formed(table, parent, target_dialect, ts_locations)?;

        if parent.spec.collapse() {
            if matches!(parent.spec, PartitionSpec::Range { .. }) && key_columns.len() != 1 {
                return Err(partition_error(
                    CODE_PARTITION_COMPOSITE_KEY_UNSUPPORTED,
                    parent.op_index,
                    ts_locations,
                    target_dialect,
                    format!(
                        "collapse-affirmed range partitioning on table {table:?} has {} partition key columns; v1 collapse supports exactly one",
                        key_columns.len()
                    ),
                    "use a single range partition key for collapse, or omit whenUnsupported and target Postgres only",
                ));
            }
            for key in key_columns {
                if !parent.not_null_columns.contains(key) {
                    return Err(partition_error(
                        CODE_PARTITION_KEY_NULLABLE_UNDER_COLLAPSE,
                        parent.op_index,
                        ts_locations,
                        target_dialect,
                        format!(
                            "collapse-affirmed partitioned table {table:?} has nullable partition key column {key:?}"
                        ),
                        "mark every partition key column notNull, or omit whenUnsupported and target Postgres only",
                    ));
                }
            }
            validate_partition_bounds_total(table, parent, target_dialect, ts_locations)?;
        }
    }

    Ok(())
}

fn validate_partition_bounds_well_formed(
    table: &str,
    parent: &PartitionParentFold,
    dialect: Dialect,
    ts_locations: &[Option<String>],
) -> Result<(), AuthoringError> {
    use crate::model::ir::{PartitionBounds, PartitionSpec};

    match &parent.spec {
        PartitionSpec::Range { columns, .. } => {
            let mut ranges: Vec<(
                usize,
                &[crate::model::ir::PartitionBoundValue],
                &[crate::model::ir::PartitionBoundValue],
            )> = Vec::new();
            for (op_index, bounds) in parent.children.values() {
                match bounds {
                    PartitionBounds::Range { from, to } => {
                        if from.len() != columns.len() || to.len() != columns.len() {
                            return Err(partition_error(
                                CODE_PARTITION_BOUNDS_ILL_FORMED,
                                *op_index,
                                ts_locations,
                                dialect,
                                format!(
                                    "range partition child on table {table:?} has bound arity from={} to={} for {} partition key columns",
                                    from.len(),
                                    to.len(),
                                    columns.len()
                                ),
                                "make each range bound tuple match the partition key arity",
                            ));
                        }
                        if !matches!(
                            compare_bound_tuple(from, to),
                            Some(std::cmp::Ordering::Less)
                        ) {
                            return Err(partition_error(
                                CODE_PARTITION_BOUNDS_ILL_FORMED,
                                *op_index,
                                ts_locations,
                                dialect,
                                format!(
                                    "range partition child on table {table:?} has an empty, reversed, or incomparable FROM/TO bound"
                                ),
                                "use non-empty range bounds with comparable value kinds and FROM < TO",
                            ));
                        }
                        ranges.push((*op_index, from, to));
                    }
                    PartitionBounds::Default => {}
                    _ => {
                        return Err(partition_error(
                            CODE_PARTITION_BOUNDS_ILL_FORMED,
                            *op_index,
                            ts_locations,
                            dialect,
                            format!(
                                "range partitioned table {table:?} has a non-range child bound"
                            ),
                            "use range bounds or a default child under a range-partitioned parent",
                        ));
                    }
                }
            }
            for i in 0..ranges.len() {
                for j in (i + 1)..ranges.len() {
                    let (_, a_from, a_to) = ranges[i];
                    let (b_op, b_from, b_to) = ranges[j];
                    let overlaps = matches!(
                        compare_bound_tuple(a_from, b_to),
                        Some(std::cmp::Ordering::Less)
                    ) && matches!(
                        compare_bound_tuple(b_from, a_to),
                        Some(std::cmp::Ordering::Less)
                    );
                    if overlaps {
                        return Err(partition_error(
                            CODE_PARTITION_BOUNDS_ILL_FORMED,
                            b_op,
                            ts_locations,
                            dialect,
                            format!("range partition bounds on table {table:?} overlap"),
                            "make sibling range partition bounds pairwise non-overlapping",
                        ));
                    }
                }
            }
        }
        PartitionSpec::List { .. } => {
            let mut seen = std::collections::BTreeSet::new();
            for (op_index, bounds) in parent.children.values() {
                match bounds {
                    PartitionBounds::List { values } => {
                        for value in values {
                            let key = partition_bound_key(value);
                            if !seen.insert(key) {
                                return Err(partition_error(
                                    CODE_PARTITION_BOUNDS_ILL_FORMED,
                                    *op_index,
                                    ts_locations,
                                    dialect,
                                    format!(
                                        "list partition value {} appears more than once on table {table:?}",
                                        partition_bound_key(value)
                                    ),
                                    "ensure each list-bound value appears at most once across all sibling partitions",
                                ));
                            }
                        }
                    }
                    PartitionBounds::Default => {}
                    _ => {
                        return Err(partition_error(
                            CODE_PARTITION_BOUNDS_ILL_FORMED,
                            *op_index,
                            ts_locations,
                            dialect,
                            format!("list partitioned table {table:?} has a non-list child bound"),
                            "use list bounds or a default child under a list-partitioned parent",
                        ));
                    }
                }
            }
        }
        PartitionSpec::Hash { .. } => {
            let mut classes = Vec::new();
            for (op_index, bounds) in parent.children.values() {
                match bounds {
                    PartitionBounds::Default => {
                        return Err(partition_error(
                            CODE_PARTITION_BOUNDS_ILL_FORMED,
                            *op_index,
                            ts_locations,
                            dialect,
                            format!("hash partitioned table {table:?} cannot have a default child"),
                            "remove the default child from hash partitioning and use modulus/remainder bounds",
                        ));
                    }
                    PartitionBounds::Hash { modulus, remainder } => {
                        if *modulus == 0 || *remainder >= *modulus {
                            return Err(partition_error(
                            CODE_PARTITION_BOUNDS_ILL_FORMED,
                            *op_index,
                            ts_locations,
                            dialect,
                            format!(
                                "hash partition on table {table:?} has modulus {modulus} and remainder {remainder}; remainder must be less than a non-zero modulus"
                            ),
                            "use hash bounds with modulus > 0 and remainder < modulus",
                        ));
                        }
                        classes.push((*op_index, u128::from(*modulus), u128::from(*remainder)));
                    }
                    _ => {
                        return Err(partition_error(
                            CODE_PARTITION_BOUNDS_ILL_FORMED,
                            *op_index,
                            ts_locations,
                            dialect,
                            format!("hash partitioned table {table:?} has a non-hash child bound"),
                            "use modulus/remainder bounds under a hash-partitioned parent",
                        ));
                    }
                }
            }
            for i in 0..classes.len() {
                for j in (i + 1)..classes.len() {
                    let (op_index, m1, r1) = classes[i];
                    let (op_index2, m2, r2) = classes[j];
                    let (small_m, small_r, large_m, large_r, err_op) = if m1 <= m2 {
                        (m1, r1, m2, r2, op_index2)
                    } else {
                        (m2, r2, m1, r1, op_index)
                    };
                    if large_m % small_m != 0 {
                        return Err(partition_error(
                            CODE_PARTITION_BOUNDS_ILL_FORMED,
                            err_op,
                            ts_locations,
                            dialect,
                            format!(
                                "hash partition moduli {m1} and {m2} on table {table:?} are not comparable by divisibility"
                            ),
                            "use hash partition moduli where every pair is comparable by divisibility",
                        ));
                    }
                    if large_r % small_m == small_r {
                        return Err(partition_error(
                            CODE_PARTITION_BOUNDS_ILL_FORMED,
                            err_op,
                            ts_locations,
                            dialect,
                            format!(
                                "hash partition congruence classes ({m1},{r1}) and ({m2},{r2}) overlap on table {table:?}"
                            ),
                            "use non-overlapping hash remainder classes",
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn validate_partition_bounds_total(
    table: &str,
    parent: &PartitionParentFold,
    dialect: Dialect,
    ts_locations: &[Option<String>],
) -> Result<(), AuthoringError> {
    use crate::model::ir::{PartitionBounds, PartitionSpec};

    match &parent.spec {
        PartitionSpec::Range { .. } | PartitionSpec::List { .. } => {
            if !parent
                .children
                .values()
                .any(|(_, bounds)| matches!(bounds, PartitionBounds::Default))
            {
                return Err(partition_error(
                    CODE_PARTITION_BOUNDS_NOT_TOTAL,
                    parent.op_index,
                    ts_locations,
                    dialect,
                    format!(
                        "collapse-affirmed {} partitioned table {table:?} has no default child",
                        partition_spec_label(&parent.spec)
                    ),
                    "add a .partition(...).create({ default: true }) child, or omit whenUnsupported and target Postgres only",
                ));
            }
        }
        PartitionSpec::Hash { .. } => {
            let mut lcm = 1_u128;
            let mut classes = Vec::new();
            for (op_index, bounds) in parent.children.values() {
                if let PartitionBounds::Hash { modulus, remainder } = bounds {
                    lcm = hash_lcm(lcm, u128::from(*modulus)).ok_or_else(|| {
                        partition_error(
                            CODE_PARTITION_BOUNDS_NOT_TOTAL,
                            *op_index,
                            ts_locations,
                            dialect,
                            format!(
                                "hash partition modulus set on table {table:?} overflows the validator's exact lcm arithmetic"
                            ),
                            "use smaller hash moduli or avoid collapse affirmation for this hash partition set",
                        )
                    })?;
                    classes.push((u128::from(*modulus), u128::from(*remainder)));
                }
            }
            let covered: u128 = classes.iter().map(|(m, _)| lcm / *m).sum();
            if covered != lcm {
                return Err(partition_error(
                    CODE_PARTITION_BOUNDS_NOT_TOTAL,
                    parent.op_index,
                    ts_locations,
                    dialect,
                    format!(
                        "collapse-affirmed hash partitioned table {table:?} covers {covered} of {lcm} residue classes"
                    ),
                    "declare hash children whose modulus/remainder classes cover every residue in 0..lcm(moduli)-1",
                ));
            }
        }
    }
    Ok(())
}

/// Validate every expression slot of a single [`Op`](crate::model::ir::Op) at
/// `op_index`. The per-variant Expr enumeration the SOLE-gate property needs;
/// see [`validate_ir`] for the slot map.
///
/// # Errors
/// Returns the first [`AuthoringError`] any embedded expression produces.
pub fn validate_op(
    op: &crate::model::ir::Op,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    // The bare entry keeps the Trusted posture (no cross-schema confinement); the
    // schema-ident + guard-direction checks still run (trust-independent).
    validate_op_scoped(op, target_dialect, op_index, ts_location, None)
}

fn validate_dialectal_op(
    default: Option<&[crate::model::ir::Op]>,
    pg: Option<&[crate::model::ir::Op]>,
    sqlite: Option<&[crate::model::ir::Op]>,
    mysql: Option<&[crate::model::ir::Op]>,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    schema_scope: Option<&crate::model::policy::SchemaScope>,
) -> Result<(), AuthoringError> {
    fn mk(
        target_dialect: Dialect,
        op_index: usize,
        ts_location: Option<&str>,
        reason: impl Into<String>,
        suggested_fix: impl Into<String>,
    ) -> AuthoringError {
        AuthoringError {
            code: CODE_OP_INVALID.to_string(),
            kind: Some(UnsupportedKind::Op),
            op_index,
            ts_location: ts_location.map(str::to_string),
            dialect: target_dialect,
            reason: reason.into(),
            suggested_fix: Some(suggested_fix.into()),
        }
    }

    let legs = [
        ("default", default),
        ("pg", pg),
        ("sqlite", sqlite),
        ("mysql", mysql),
    ];
    if legs.iter().all(|(_, leg)| leg.is_none()) {
        return Err(mk(
            target_dialect,
            op_index,
            ts_location,
            "dialectal op carries no legs; at least one of default/pg/sqlite/mysql must be present",
            "supply at least one dialectal op leg, or remove the dialect() statement",
        ));
    }
    for (label, leg) in legs {
        let Some(ops) = leg else {
            continue;
        };
        if ops
            .iter()
            .any(|op| matches!(op, crate::model::ir::Op::Dialectal { .. }))
        {
            return Err(mk(
                target_dialect,
                op_index,
                ts_location,
                format!("dialectal op leg {label:?} contains a nested dialectal op"),
                "flatten the inner dialect() into the outer leg; nested op-level dialect() is not supported",
            ));
        }
    }

    if let Some(ops) = default {
        for dialect in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
            for op in ops {
                validate_op_scoped(op, dialect, op_index, ts_location, schema_scope)?;
            }
        }
    }
    if let Some(ops) = pg {
        for op in ops {
            validate_op_scoped(op, Dialect::Postgres, op_index, ts_location, schema_scope)?;
        }
    }
    if let Some(ops) = sqlite {
        for op in ops {
            validate_op_scoped(op, Dialect::Sqlite, op_index, ts_location, schema_scope)?;
        }
    }
    if let Some(ops) = mysql {
        for op in ops {
            validate_op_scoped(op, Dialect::Mysql, op_index, ts_location, schema_scope)?;
        }
    }
    Ok(())
}

/// [`validate_op`] threaded with the active
/// [`SchemaScope`](crate::model::policy::SchemaScope). Runs the schema/guard gate
/// FIRST, then the per-op expression-slot checks.
///
/// # Errors
/// Returns the first [`AuthoringError`] the gate or any embedded expression produces.
pub fn validate_op_scoped(
    op: &crate::model::ir::Op,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    schema_scope: Option<&crate::model::policy::SchemaScope>,
) -> Result<(), AuthoringError> {
    use crate::model::ir::{
        ColumnOrExpr, IndexElement, IrConstraintKind, Op, TriggerAction, ViewQuery,
    };

    if let Op::Dialectal {
        default,
        pg,
        sqlite,
        mysql,
    } = op
    {
        return validate_dialectal_op(
            default.as_deref(),
            pg.as_deref(),
            sqlite.as_deref(),
            mysql.as_deref(),
            target_dialect,
            op_index,
            ts_location,
            schema_scope,
        );
    }

    // schema confinement + guard-direction gate, BEFORE any expression
    // walk. Fail-closed: a Confined cross-schema op never reaches lower.
    validate_op_schema_and_guard(op, target_dialect, op_index, ts_location, schema_scope)?;

    // **VENDOR (`zero-migrate`)** — the capability-composition gate,
    // BEFORE any expression walk. A privileged vendor op is
    // refused fail-closed when (a) the target is SQLite (every vendor op is
    // `PgOnly`), or (b) the active capability set — derived from the threaded
    // [`SchemaScope`] — does not GRANT the op's required capability. The Confined
    // creator/AI posture (`Single` scope) grants nothing, so every vendor op dies
    // here; Platform/Trusted (`Allowlist`/`Unconfined`) grant the operator preset.
    validate_vendor_op(op, target_dialect, op_index, ts_location, schema_scope)?;
    validate_create_table_primary_key_policy(op, target_dialect, op_index, ts_location)?;
    validate_op_support(op, target_dialect, op_index, ts_location)?;
    validate_sequence_options(op, target_dialect, op_index, ts_location)?;
    validate_function_type_refs(op, target_dialect, op_index, ts_location)?;

    // Constraint-embedded expressions validate against the given table scope.
    let check_constraint =
        |kind: &IrConstraintKind, scope: &TargetScope<'_>| -> Result<(), AuthoringError> {
            match kind {
                IrConstraintKind::Check { expr, .. } => {
                    validate_expr(expr, target_dialect, scope, op_index, ts_location)?;
                    validate_immutable_expr_context(
                        expr,
                        "CHECK constraint",
                        target_dialect,
                        op_index,
                        ts_location,
                    )?;
                }
                IrConstraintKind::Exclusion {
                    elements,
                    where_predicate,
                    ..
                } => {
                    for element in elements {
                        match &element.target {
                            ColumnOrExpr::Column { name } => {
                                let col = crate::model::expr::Expr::ColRef {
                                    name: name.clone(),
                                    table: None,
                                };
                                validate_expr(&col, target_dialect, scope, op_index, ts_location)?;
                            }
                            ColumnOrExpr::Expr { expr } => {
                                validate_expr(expr, target_dialect, scope, op_index, ts_location)?;
                            }
                        }
                    }
                    if let Some(pred) = where_predicate {
                        validate_expr(pred, target_dialect, scope, op_index, ts_location)?;
                    }
                }
                _ => {}
            }
            Ok(())
        };

    let check_index_element =
        |element: &IndexElement, scope: &TargetScope<'_>| -> Result<(), AuthoringError> {
            match element {
                IndexElement::Column { name, .. } => {
                    let col = crate::model::expr::Expr::ColRef {
                        name: name.clone(),
                        table: None,
                    };
                    validate_expr(&col, target_dialect, scope, op_index, ts_location)?;
                }
                IndexElement::Expr { expr } => {
                    validate_expr(expr, target_dialect, scope, op_index, ts_location)?;
                    validate_immutable_expr_context(
                        expr,
                        "index expression",
                        target_dialect,
                        op_index,
                        ts_location,
                    )?;
                }
            }
            Ok(())
        };

    match op {
        Op::CreateTable { name, columns, primary_key, constraints, indexes, .. } => {
            // A resolved createTable is self-contained: ColRefs resolve against
            // the op's explicit columns. Confined record/build paths stamp the
            // seven system fields before checksum; Platform paths do not.
            let cols: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
            let scope = TargetScope::new(name, &cols);
            let value_format_columns = columns
                .iter()
                .filter_map(|column| {
                    let format = match column.value_format.as_ref()? {
                        crate::model::ir::ValueFormat::TypeId { .. } => "TypeID",
                        crate::model::ir::ValueFormat::Ulid => "ULID",
                    };
                    Some((column.name.as_str(), format))
                })
                .collect::<std::collections::BTreeMap<_, _>>();
            for ix in indexes {
                for element in &ix.columns {
                    let weakening = match element {
                        IndexElement::Column {
                            name,
                            collation: Some(collation),
                            ..
                        } if collation != "C" => value_format_columns
                            .get(name.as_str())
                            .map(|format| (name, collation, format)),
                        _ => None,
                    };
                    if let Some((name, collation, format)) = weakening {
                        return Err(AuthoringError {
                            code: CODE_COLUMN_FACET_CONFLICT.to_string(),
                            kind: None,
                            op_index,
                            ts_location: ts_location.map(str::to_string),
                            dialect: target_dialect,
                            reason: format!(
                                "column {name:?} declares a {format} value format but index {:?} selects collation {collation:?}; {format} requires the bytewise C collation",
                                ix.name
                            ),
                            suggested_fix: Some(
                                "remove the index collation override or use collation \"C\""
                                    .to_string(),
                            ),
                        });
                    }
                    check_index_element(element, &scope)?;
                }
                if let Some(pred) = &ix.r#where {
                    validate_expr(pred, target_dialect, &scope, op_index, ts_location)?;
                    validate_immutable_expr_context(
                        pred,
                        "index predicate",
                        target_dialect,
                        op_index,
                        ts_location,
                    )?;
                }
            }
            for c in constraints {
                check_constraint(&c.kind, &scope)?;
            }
            let pk_cols = primary_key.as_deref();
            // The per-column declared-only facets
            // (`id_prefix` / `vector_metric`) carry validate-time bounds: the IR's
            // threat model is a hand-crafted IR envelope, so a malformed/reserved
            // prefix or a misplaced metric is refused fail-closed BEFORE lower /
            // checksum, never deferred to a render surprise.
            for col in columns {
                if let Some(generated) = &col.generated {
                    validate_expr(
                        &generated.expr,
                        target_dialect,
                        &scope,
                        op_index,
                        ts_location,
                    )?;
                    validate_immutable_expr_context(
                        &generated.expr,
                        "generated column expression",
                        target_dialect,
                        op_index,
                        ts_location,
                    )?;
                }
                validate_column_facets(col, target_dialect, op_index, ts_location)?;
                validate_identity_placement(
                    col,
                    target_dialect,
                    pk_cols,
                    false,
                    op_index,
                    ts_location,
                )?;
            }
            Ok(())
        }
        Op::SetTableOptions { .. } => Ok(()),
        Op::CreateIndex { table, columns, r#where, .. } => {
            // The index elements and partial-index predicate. The live column set
            // is not known at load (the table pre-exists), so structural-only here.
            let scope = TargetScope::structural_only(table);
            for element in columns {
                check_index_element(element, &scope)?;
            }
            if let Some(pred) = r#where {
                validate_expr(pred, target_dialect, &scope, op_index, ts_location)?;
                validate_immutable_expr_context(
                    pred,
                    "index predicate",
                    target_dialect,
                    op_index,
                    ts_location,
                )?;
            }
            Ok(())
        }
        Op::SetColumnType { table, to_type, using, .. } => {
            validate_col_type_position(
                to_type,
                "setColumnType.toType",
                false,
                target_dialect,
                op_index,
                ts_location,
            )?;
            if let Some(cast) = using {
                let scope = TargetScope::structural_only(table);
                validate_expr(cast, target_dialect, &scope, op_index, ts_location)?;
            }
            Ok(())
        }
        Op::AddConstraint { table, constraint, .. } => {
            let scope = TargetScope::structural_only(table);
            check_constraint(&constraint.kind, &scope)
        }
        Op::CreateDomain { as_type, check, default, .. } => {
            validate_col_type_position(
                as_type,
                "createDomain.as",
                true,
                target_dialect,
                op_index,
                ts_location,
            )?;
            if let Some(default) = default {
                validate_default_for_type(
                    "createDomain.default",
                    as_type,
                    default,
                    target_dialect,
                    op_index,
                    ts_location,
                )?;
            }
            if let Some(check) = check {
                let cols = vec!["VALUE".to_string()];
                let scope = TargetScope::new("domain", &cols);
                validate_expr(check, target_dialect, &scope, op_index, ts_location)?;
                validate_immutable_expr_context(
                    check,
                    "CHECK constraint",
                    target_dialect,
                    op_index,
                    ts_location,
                )?;
            }
            Ok(())
        }
        Op::Update { table, set, r#where, .. } => {
            let scope = TargetScope::structural_only(table);
            for value in set.values() {
                if let crate::model::ir::IrValue::Expr(expr) = value {
                    validate_expr(expr, target_dialect, &scope, op_index, ts_location)?;
                }
            }
            if let Some(pred) = r#where {
                validate_expr(pred, target_dialect, &scope, op_index, ts_location)?;
            }
            Ok(())
        }
        Op::Delete { table, r#where, .. } => {
            let scope = TargetScope::structural_only(table);
            validate_expr(r#where, target_dialect, &scope, op_index, ts_location)
        }
        Op::Backfill {
            table,
            cursor_columns,
            cursor_stability,
            set,
            filter,
            ..
        } => {
            validate_backfill_cursor_fields(
                cursor_columns,
                cursor_stability,
                set,
                target_dialect,
                op_index,
                ts_location,
            )?;
            let scope = TargetScope::structural_only(table);
            for value in set.values() {
                if let crate::model::ir::BackfillSetValue::Value(
                    crate::model::ir::IrValue::Expr(expr),
                ) = value
                {
                    validate_expr(expr, target_dialect, &scope, op_index, ts_location)?;
                }
            }
            if let Some(pred) = filter {
                validate_expr(pred, target_dialect, &scope, op_index, ts_location)?;
                validate_immutable_expr_context(
                    pred,
                    "backfill filter",
                    target_dialect,
                    op_index,
                    ts_location,
                )?;
            }
            Ok(())
        }
        Op::Insert { table, rows, on_conflict, .. } => {
            let scope = TargetScope::structural_only(table);
            for row in rows {
                for value in row {
                    if let crate::model::ir::IrValue::Expr(expr) = value {
                        validate_expr(expr, target_dialect, &scope, op_index, ts_location)?;
                    }
                }
            }
            if let Some(on_conflict) = on_conflict {
                if let Some(do_update) = &on_conflict.do_update {
                    for value in do_update.values() {
                        if let crate::model::ir::IrValue::Expr(expr) = value {
                            validate_expr(expr, target_dialect, &scope, op_index, ts_location)?;
                        }
                    }
                }
            }
            Ok(())
        }
        Op::DropIndex { name, table, .. } => {
            // fail-closed: a DropIndex carries an index `name` and an
            // OPTIONAL owning-table hint. The ownership gate
            // ([`crate::model::load::enforce_ir_ownership`]) checks the op's TARGET
            // TABLE — but a bare-name DropIndex (`table: None`) has no
            // ownership-checkable target, so the gate would SKIP it, letting a
            // hostile IR envelope `{op:"dropIndex", name:"<other_app_index>"}` drop
            // ANOTHER app's index cross-tenant. Until a name→owning-table registry
            // resolver exists, we refuse a bare-name DropIndex fail-closed: the
            // author must carry the owning-table hint, which makes the drop
            // ownership-checkable. (A name-only drop is also intrinsically
            // dialect-ambiguous on PG, where an index lives in a schema, not a
            // table.) An `UNSUPPORTED { kind: "op" }` so the AI/author loop's
            // remedy is "carry the owning table".
            if table.is_none() {
                return Err(AuthoringError {
                    code: CODE_UNSUPPORTED.to_string(),
                    kind: Some(UnsupportedKind::Op),
                    op_index,
                    ts_location: ts_location.map(str::to_string),
                    dialect: target_dialect,
                    reason: format!(
                        "dropIndex of {name:?} omits its owning table, so the \
                         ownership check cannot resolve the index's owner — a \
                         bare-name index drop is refused fail-closed (it would let a \
                         migration drop another app's index by name)"
                    ),
                    suggested_fix: Some(format!(
                        "name the owning table, e.g. op.dropIndex({name:?}, {{ table: \
                         \"<owning_table>\" }}), so the drop is ownership-checked"
                    )),
                });
            }
            Ok(())
        }
        // AddColumn carries the same per-column declared facets (`value_format` /
        // `vector_metric` / standalone `mask`) `createTable` columns do, so it gets
        // the SAME fail-closed facet validation. Build a synthetic single-column
        // `IrColumn` view and route it through the shared [`validate_column_facets`].
        // (`id_prefix` cannot reach here — `Op::AddColumn` has no slot; the recorder
        // fail-closes it — so the legacy-prefix arm is a no-op for this view.)
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
            if let Some(generated) = generated {
                let scope = TargetScope::structural_only(table);
                validate_expr(
                    &generated.expr,
                    target_dialect,
                    &scope,
                    op_index,
                    ts_location,
                )?;
                validate_immutable_expr_context(
                    &generated.expr,
                    "generated column expression",
                    target_dialect,
                    op_index,
                    ts_location,
                )?;
            }
            let view = crate::model::ir::IrColumn {
                name: column.clone(),
                ty: ty.clone(),
                nullable: *nullable,
                default: default.clone(),
                unique: None,
                value_format: value_format.clone(),
                references: None,
                id_prefix: None,
                vector_metric: *vector_metric,
                case_sensitive: *case_sensitive,
                mask: *mask,
                generated: generated.clone(),
                identity: *identity,
            };
            validate_column_facets(&view, target_dialect, op_index, ts_location)?;
            validate_identity_placement(
                &view,
                target_dialect,
                None,
                true,
                op_index,
                ts_location,
            )
        }
        // VENDOR — a `createPolicy`'s `USING`/`WITH CHECK` predicates are CLOSED
        // `(c) => Expr` ASTs: validate them STRUCTURALLY (the
        // (a)/(b)/(d) checks) against the policy's target table. The live column set
        // is unknown at load (the table pre-exists), so structural-only here.
        Op::CreatePolicy { table, using, with_check, .. } => {
            let scope = TargetScope::structural_only(table);
            validate_expr(using, target_dialect, &scope, op_index, ts_location)?;
            if let Some(wc) = with_check {
                validate_expr(wc, target_dialect, &scope, op_index, ts_location)?;
            }
            Ok(())
        }
        // CROSS-DIALECT CORE — trigger `WHEN` + body statements are CLOSED ASTs.
        // Dialect-impossible actions/facets are refused per facet, not by a
        // whole-construct vendor gate.
        Op::CreateTrigger { table, events, for_each, when, action, .. } => {
            validate_trigger_dialect(
                events,
                *for_each,
                action,
                target_dialect,
                op_index,
                ts_location,
            )?;
            if let Some(w) = when {
                let scope = TargetScope::structural_only(table);
                validate_expr(w, target_dialect, &scope, op_index, ts_location)?;
            }
            if let TriggerAction::Body { statements } = action {
                for stmt in statements {
                    validate_trigger_stmt(
                        stmt,
                        table,
                        target_dialect,
                        op_index,
                        ts_location,
                        schema_scope,
                    )?;
                }
            }
            Ok(())
        }
        // CROSS-DIALECT CORE views. A structured view body is the closed SelectAst
        // subset and needs no vendor capability. A raw body is operator-gated above,
        // then asserted to be exactly one read-only SELECT and re-scanned with the
        // function-body deny-list before admission.
        Op::CreateView { query, .. } => {
            match query {
                ViewQuery::Structured { select } => {
                    validate_select_ast(
                        select,
                        target_dialect,
                        op_index,
                        ts_location,
                        schema_scope,
                    )?;
                }
                ViewQuery::Raw { sql } => {
                    validate_raw_view_body_sql(
                        sql,
                        target_dialect,
                        op_index,
                        ts_location,
                        schema_scope,
                    )?;
                }
            }
            Ok(())
        }
        Op::PgRaw { reason, .. } if reason.trim().is_empty() => Err(AuthoringError {
            code: CODE_PGRAW_REASON_REQUIRED.to_string(),
            kind: Some(UnsupportedKind::Op),
            op_index,
            ts_location: ts_location.map(str::to_string),
            dialect: target_dialect,
            reason: "pgRaw requires a non-empty reason for auditability".to_string(),
            suggested_fix: Some(
                "pass pg.raw({ sql, reason }) with a short explanation for why raw SQL is required"
                    .to_string(),
            ),
        }),
        Op::CreateRole { superuser, if_not_exists, .. }
            if superuser.unwrap_or(false) && if_not_exists.unwrap_or(false) =>
        {
            Err(AuthoringError {
                code: CODE_UNSUPPORTED.to_string(),
                kind: Some(UnsupportedKind::Op),
                op_index,
                ts_location: ts_location.map(str::to_string),
                dialect: target_dialect,
                reason: "createRole cannot combine superuser:true with ifNotExists:true; \
                         the idempotent form requires a PL/pgSQL DO wrapper and SUPERUSER \
                         must never be hidden inside an opaque body"
                    .to_string(),
                suggested_fix: Some(
                    "remove superuser:true; Platform migrations may create bounded \
                     roles, but must not mint Postgres superusers"
                        .to_string(),
                ),
            })
        }
        // Ops with no embedded expression slot. (`RenameTable` carries only its
        // old/new table NAMES — no Expr — so the schema-ident + guard-direction
        // gate in `validate_op_schema_and_guard` above is the whole check, and the
        // render-time `quote_ident` is the injection-safe identifier seam.) The
        // remaining VENDOR ops carry no embedded Expr — their privileged payload is
        // closed sub-enums (`Privilege`/`TriggerTiming`/…) or the capability-gated
        // raw `body`/`sql` strings (parse-scanned by the guard deny-list at lower).
        Op::RenameColumn { ty, .. } => validate_col_type_position(
            ty,
            "renameColumn.type",
            false,
            target_dialect,
            op_index,
            ts_location,
        ),
        Op::AlterPrimaryKey { action, .. } => {
            validate_alter_primary_key_action(action).map_err(|reason| AuthoringError {
                code: CODE_PRIMARY_KEY_INVALID.to_string(),
                kind: Some(UnsupportedKind::Op),
                op_index,
                ts_location: ts_location.map(str::to_string),
                dialect: target_dialect,
                reason,
                suggested_fix: Some(
                    "provide exact non-empty ordered columns; replace must change the tuple, and dropIdentityFrom must be a non-empty subset of expectedColumns"
                        .to_string(),
                ),
            })
        }
        Op::SetColumnDefault { value, .. } => {
            if let crate::model::ir::IrDefault::Expr { expr } = value {
                validate_default_expr(
                    "setColumnDefault.value",
                    expr,
                    target_dialect,
                    op_index,
                    ts_location,
                )?;
            }
            Ok(())
        }
        Op::SetRls { enabled, forced, .. } if enabled.is_none() && forced.is_none() => {
            Err(AuthoringError {
                code: CODE_OP_INVALID.to_string(),
                kind: Some(UnsupportedKind::Op),
                op_index,
                ts_location: ts_location.map(str::to_string),
                dialect: target_dialect,
                reason: "setRls needs at least one of { enabled, forced }".to_string(),
                suggested_fix: Some(
                    "set enabled, forced, or both on the setRls op".to_string(),
                ),
            })
        }
        Op::DropTable { .. }
        | Op::CreatePartition { .. }
        | Op::AttachPartition { .. }
        | Op::DetachPartition { .. }
        | Op::DropPartition { .. }
        | Op::RenameTable { .. }
        | Op::DropColumn { .. }
        | Op::SetColumnNotNull { .. }
        | Op::DropColumnNotNull { .. }
        | Op::DropColumnDefault { .. }
        | Op::DropConstraint { .. }
        // ValidateConstraint carries no embedded Expr; its PG-only dialect refusal
        // runs in the op-level `error_from_decision` gate above.
        | Op::ValidateConstraint { .. }
        | Op::CreateEnum { .. }
        | Op::DropEnum { .. }
        | Op::DropDomain { .. }
        | Op::CreateSequence { .. }
        | Op::AlterSequence { .. }
        | Op::DropSequence { .. }
        | Op::Comment { .. }
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
        | Op::DropPolicy { .. }
        | Op::DropTrigger { .. }
        | Op::DropView { .. }
        | Op::CreateFunction { .. }
        | Op::DropFunction { .. }
        | Op::PgRaw { .. }
        | Op::Dialectal { .. } => Ok(()),
    }
}

fn validate_create_table_primary_key_policy(
    op: &crate::model::ir::Op,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    use crate::model::ir::Op;

    let Op::CreateTable {
        name,
        columns,
        primary_key,
        ..
    } = op
    else {
        return Ok(());
    };

    let err = |code: &str, reason: String, suggested_fix: String| AuthoringError {
        code: code.to_string(),
        kind: None,
        op_index,
        ts_location: ts_location.map(str::to_string),
        dialect: target_dialect,
        reason,
        suggested_fix: Some(suggested_fix),
    };

    if let Some(pk_columns) = primary_key {
        if pk_columns.is_empty() {
            return Err(err(
                CODE_PRIMARY_KEY_INVALID,
                format!("createTable {name:?} declares an empty primaryKey"),
                "omit primaryKey for no primary key, or name one or more table columns".to_string(),
            ));
        }

        let mut seen = std::collections::BTreeSet::new();
        let table_columns = columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        for column in pk_columns {
            if !seen.insert(column.as_str()) {
                return Err(err(
                    CODE_PRIMARY_KEY_INVALID,
                    format!(
                        "createTable {name:?} primaryKey names column {column:?} more than once"
                    ),
                    "remove duplicate primaryKey columns".to_string(),
                ));
            }
            if !table_columns.contains(column.as_str()) {
                return Err(err(
                    CODE_PRIMARY_KEY_INVALID,
                    format!(
                        "createTable {name:?} primaryKey names column {column:?}, but that column \
                         is absent from the resolved table"
                    ),
                    "name only columns present in the resolved createTable columns".to_string(),
                ));
            }
        }
    }

    // NOTE (Cut 3 — de-thread PolicyProfile): the author-PK CONFORMANCE re-check
    // (does the resolved table carry the operator's injected system shape / PK?) is
    // no longer a hardcoded confined-profile gate here. That conformance is owned by
    // the injection resolver ([`crate::model::table_shape::resolve_create_table_policy`]),
    // which is the `EffectivePolicy`/`injects_for` evaluator: a `createTable` in a
    // mandatory-inject scope whose author declares its own PK is refused there with
    // `AuthorPrimaryKeyForbidden`. The generic engine no longer bakes zeroship's
    // shape into validate-time; only the PURE primaryKey validation (empty / dup /
    // absent-column, above) stays. See the design doc §"7-column system_shape → one
    // inject rule".
    Ok(())
}

fn validate_op_support(
    op: &crate::model::ir::Op,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    use crate::model::ir::{
        IndexElement, IndexMethod, IrConstraintKind, IrDefault, Op, TriggerAction, TriggerEvent,
        TriggerStmt,
    };
    use crate::model::support::{Feature, Support, SupportDecision};

    fn error_from_decision(
        decision: SupportDecision,
        kind: UnsupportedKind,
        target_dialect: Dialect,
        op_index: usize,
        ts_location: Option<&str>,
    ) -> Option<AuthoringError> {
        let SupportDecision::Unsupported { code, reason } = decision else {
            return None;
        };
        let suggested_fix = match kind {
            UnsupportedKind::Expr => {
                "remove the expression-bearing option for now, or defer this migration until the expression/default renderer lands"
            }
            _ => {
                "remove this unsupported shape, or target a dialect/op shape the current engine declares supported"
            }
        };
        Some(AuthoringError {
            code: code.to_string(),
            kind: Some(kind),
            op_index,
            ts_location: ts_location.map(str::to_string),
            dialect: target_dialect,
            reason: reason.to_string(),
            suggested_fix: Some(suggested_fix.to_string()),
        })
    }

    fn feature_kind(feature: Feature) -> UnsupportedKind {
        match feature {
            Feature::TableLevelCheck | Feature::AlterColumnUsing => UnsupportedKind::Expr,
            _ => UnsupportedKind::Op,
        }
    }

    fn op_kind(op: &Op) -> UnsupportedKind {
        match op {
            Op::CreateTable { columns, .. } if columns.iter().any(|col| col.identity.is_some()) => {
                UnsupportedKind::Identity
            }
            Op::AddColumn {
                identity: Some(_), ..
            } => UnsupportedKind::Identity,
            Op::SetColumnType { using: Some(_), .. } => UnsupportedKind::Expr,
            Op::AddConstraint { constraint, .. }
                if matches!(constraint.kind, IrConstraintKind::Check { .. }) =>
            {
                UnsupportedKind::Expr
            }
            _ => UnsupportedKind::Op,
        }
    }

    fn check_feature(
        support: &Support,
        feature: Feature,
        target_dialect: Dialect,
        op_index: usize,
        ts_location: Option<&str>,
    ) -> Result<(), AuthoringError> {
        let Some(feature_support) = support.features.iter().find(|decl| decl.feature == feature)
        else {
            return Ok(());
        };
        if let Some(err) = error_from_decision(
            feature_support.decision(target_dialect),
            feature_kind(feature),
            target_dialect,
            op_index,
            ts_location,
        ) {
            return Err(err);
        }
        Ok(())
    }

    fn default_is_nextval(default: Option<&IrDefault>) -> bool {
        matches!(default, Some(IrDefault::Nextval { .. }))
    }

    fn non_btree_index_method(using: Option<IndexMethod>) -> bool {
        !matches!(using, None | Some(IndexMethod::Btree))
    }

    fn with_storage_params(with: &Option<crate::model::ir::IndexStorageParams>) -> bool {
        with.as_ref().is_some_and(|params| !params.is_empty())
    }

    fn index_elements_have_opclass(columns: &[IndexElement]) -> bool {
        columns.iter().any(|element| {
            matches!(
                element,
                IndexElement::Column {
                    opclass: Some(_),
                    ..
                }
            )
        })
    }

    fn index_elements_have_collation(columns: &[IndexElement]) -> bool {
        columns.iter().any(|element| {
            matches!(
                element,
                IndexElement::Column {
                    collation: Some(_),
                    ..
                }
            )
        })
    }

    fn constraint_kind_not_valid(kind: &IrConstraintKind) -> bool {
        matches!(
            kind,
            IrConstraintKind::Fk {
                not_valid: Some(true),
                ..
            } | IrConstraintKind::Check {
                not_valid: Some(true),
                ..
            }
        )
    }

    fn fk_features(
        columns: &[String],
        references_columns: &[String],
        mut check: impl FnMut(Feature) -> Result<(), AuthoringError>,
    ) -> Result<(), AuthoringError> {
        if columns.is_empty() {
            check(Feature::ForeignKeyNoLocalColumn)?;
        } else if columns.len() != 1 {
            check(Feature::CompositeForeignKey)?;
        }
        if !(references_columns.is_empty()
            || (references_columns.len() == 1 && references_columns[0] == "id"))
        {
            check(Feature::NonIdForeignKey)?;
        }
        Ok(())
    }

    let fk_deferrable_consistency =
        |deferrable: &Option<bool>, initially_deferred: &Option<bool>| {
            if *initially_deferred == Some(true) && *deferrable != Some(true) {
                return Err(AuthoringError {
                    code: CODE_OP_INVALID.to_string(),
                    kind: None,
                    op_index,
                    ts_location: ts_location.map(str::to_string),
                    dialect: target_dialect,
                    reason: "initiallyDeferred requires deferrable".to_string(),
                    suggested_fix: Some(
                        "set deferrable: true when initiallyDeferred is true, or omit initiallyDeferred"
                            .to_string(),
                    ),
                });
            }
            Ok(())
        };

    let support = crate::model::op_support::support(op);
    match op {
        Op::CreateTable {
            name,
            partition_by: Some(partition_by),
            ..
        } if !matches!(target_dialect, Dialect::Postgres) && !partition_by.collapse() => {
            return Err(AuthoringError {
                code: CODE_DIALECT_UNSUPPORTED.to_string(),
                kind: Some(UnsupportedKind::Op),
                op_index,
                ts_location: ts_location.map(str::to_string),
                dialect: target_dialect,
                reason: format!(
                    "partitioned table {name:?} is native only on Postgres unless partitionBy.whenUnsupported is affirmed as \"collapse\""
                ),
                suggested_fix: Some(
                    "add partitionBy.whenUnsupported: \"collapse\" and satisfy the partition collapse validation rules, or target Postgres only"
                        .to_string(),
                ),
            });
        }
        _ => {}
    }
    if let Some(err) = error_from_decision(
        support.decision(target_dialect),
        op_kind(op),
        target_dialect,
        op_index,
        ts_location,
    ) {
        return Err(err);
    }

    let mut check =
        |feature| check_feature(&support, feature, target_dialect, op_index, ts_location);

    match op {
        Op::CreateTable {
            columns,
            partition_by,
            constraints,
            indexes,
            ..
        } => {
            if partition_by.is_some() {
                check(Feature::PartitionDdl)?;
            }
            if columns
                .iter()
                .any(|col| default_is_nextval(col.default.as_ref()))
            {
                check(Feature::SequenceDefault)?;
            }
            for constraint in constraints {
                // `NOT VALID` is meaningless at create-time (there are no existing
                // rows to defer, and PostgreSQL rejects `NOT VALID` in `CREATE TABLE`).
                // Refuse it fail-closed on the create-time inline constraint so a
                // hand-crafted IR cannot smuggle it into a silently-dropped slot; it
                // is only authorable via addForeignKey/addCheck (ALTER TABLE ADD
                // CONSTRAINT).
                if constraint_kind_not_valid(&constraint.kind) {
                    return Err(AuthoringError {
                        code: CODE_OP_INVALID.to_string(),
                        kind: None,
                        op_index,
                        ts_location: ts_location.map(str::to_string),
                        dialect: target_dialect,
                        reason: "notValid is only valid on addForeignKey/addCheck (ALTER TABLE ADD CONSTRAINT); a create-time constraint cannot be NOT VALID".to_string(),
                        suggested_fix: Some(
                            "drop notValid from the create() constraint, or add the constraint after createTable via addForeignKey/addCheck with { notValid: true }".to_string(),
                        ),
                    });
                }
                match &constraint.kind {
                    IrConstraintKind::Check { .. } => check(Feature::TableLevelCheck)?,
                    IrConstraintKind::Fk {
                        columns,
                        references_columns,
                        deferrable,
                        initially_deferred,
                        ..
                    } => {
                        check(Feature::TableLevelForeignKey)?;
                        fk_features(columns, references_columns, &mut check)?;
                        fk_deferrable_consistency(deferrable, initially_deferred)?;
                    }
                    IrConstraintKind::Unique { .. } => check(Feature::TableLevelUnique)?,
                    IrConstraintKind::Exclusion { .. } => check(Feature::ExclusionConstraint)?,
                }
            }
            for index in indexes {
                if index
                    .columns
                    .iter()
                    .any(|element| matches!(element, IndexElement::Expr { .. }))
                {
                    check(Feature::ExpressionIndex)?;
                }
                if index.r#where.is_some() {
                    check(Feature::PartialIndex)?;
                }
                if !index.include.is_empty() {
                    check(Feature::IndexInclude)?;
                }
                if with_storage_params(&index.with) {
                    check(Feature::IndexStorageParams)?;
                }
                if index.only.unwrap_or(false) {
                    check(Feature::IndexOnly)?;
                }
                if index.nulls_not_distinct.unwrap_or(false) {
                    check(Feature::IndexNullsNotDistinct)?;
                }
                if index_elements_have_opclass(&index.columns) {
                    check(Feature::IndexOpclass)?;
                }
                if index_elements_have_collation(&index.columns) {
                    check(Feature::IndexCollation)?;
                }
                if non_btree_index_method(index.using) {
                    check(Feature::NonBtreeIndexMethod)?;
                }
            }
        }
        Op::CreatePartition { .. }
        | Op::AttachPartition { .. }
        | Op::DetachPartition { .. }
        | Op::DropPartition { .. } => check(Feature::PartitionDdl)?,
        Op::AddColumn { default, .. } => {
            if default_is_nextval(default.as_ref()) {
                check(Feature::SequenceDefault)?;
            }
        }
        Op::SetColumnType { using: Some(_), .. } => check(Feature::AlterColumnUsing)?,
        Op::SetColumnDefault {
            value: IrDefault::Nextval { .. },
            ..
        } => check(Feature::SequenceDefault)?,
        Op::CreateIndex {
            columns,
            using,
            r#where,
            include,
            with,
            only,
            nulls_not_distinct,
            ..
        } => {
            if columns
                .iter()
                .any(|element| matches!(element, IndexElement::Expr { .. }))
            {
                check(Feature::ExpressionIndex)?;
            }
            if r#where.is_some() {
                check(Feature::PartialIndex)?;
            }
            if !include.is_empty() {
                check(Feature::IndexInclude)?;
            }
            if with_storage_params(with) {
                check(Feature::IndexStorageParams)?;
            }
            if only.unwrap_or(false) {
                check(Feature::IndexOnly)?;
            }
            if nulls_not_distinct.unwrap_or(false) {
                check(Feature::IndexNullsNotDistinct)?;
            }
            if index_elements_have_opclass(columns) {
                check(Feature::IndexOpclass)?;
            }
            if index_elements_have_collation(columns) {
                check(Feature::IndexCollation)?;
            }
            if non_btree_index_method(*using) {
                check(Feature::NonBtreeIndexMethod)?;
            }
        }
        Op::RenameColumn {
            existence_guard: Some(_),
            ..
        } => check(Feature::RenameColumnGuard)?,
        Op::AddConstraint { constraint, .. } => match &constraint.kind {
            IrConstraintKind::Fk {
                columns,
                references_columns,
                deferrable,
                initially_deferred,
                not_valid,
                ..
            } => {
                fk_features(columns, references_columns, &mut check)?;
                fk_deferrable_consistency(deferrable, initially_deferred)?;
                if *not_valid == Some(true) {
                    check(Feature::ConstraintNotValid)?;
                }
            }
            IrConstraintKind::Check { not_valid, .. } => {
                check(Feature::TableLevelCheck)?;
                if *not_valid == Some(true) {
                    check(Feature::ConstraintNotValid)?;
                }
            }
            IrConstraintKind::Exclusion { .. } => check(Feature::ExclusionConstraint)?,
            IrConstraintKind::Unique { .. } => {}
        },
        Op::Insert {
            on_conflict: Some(_),
            ..
        } => check(Feature::InsertOnConflict)?,
        Op::CreateView {
            query,
            replace,
            materialized,
            ..
        } => {
            if matches!(query, crate::model::ir::ViewQuery::Raw { .. }) {
                check(Feature::RawViewBody)?;
            }
            if materialized.unwrap_or(false) {
                check(Feature::MaterializedView)?;
                if replace.unwrap_or(false) {
                    check(Feature::CreateOrReplaceMaterializedView)?;
                }
            }
        }
        Op::DropView { materialized, .. } if materialized.unwrap_or(false) => {
            check(Feature::MaterializedView)?;
        }
        Op::CreateTrigger {
            timing,
            events,
            for_each,
            action,
            when,
            ..
        } => {
            if events.len() > 1 {
                check(Feature::TriggerMultipleEvents)?;
            }
            if events
                .iter()
                .any(|event| matches!(event, TriggerEvent::Truncate))
            {
                check(Feature::TriggerTruncateEvent)?;
            }
            if matches!(timing, crate::model::ir::TriggerTiming::InsteadOf) {
                check(Feature::TriggerInsteadOfTiming)?;
            }
            if matches!(for_each, crate::model::ir::ForEach::Statement) {
                check(Feature::TriggerStatementForEach)?;
            }
            if when.is_some() {
                check(Feature::TriggerWhen)?;
            }
            match action {
                TriggerAction::ExecuteFunction { .. } => check(Feature::TriggerExecuteFunction)?,
                TriggerAction::Body { statements } => {
                    check(Feature::TriggerBody)?;
                    if statements.iter().any(|stmt| {
                        matches!(
                            stmt,
                            TriggerStmt::Raise {
                                level: crate::model::ir::RaiseLevel::Ignore,
                                ..
                            }
                        )
                    }) {
                        check(Feature::TriggerRaiseIgnore)?;
                    }
                }
            }
        }
        Op::Comment { .. } => check(Feature::Comment)?,
        Op::CreateSequence { .. } | Op::AlterSequence { .. } | Op::DropSequence { .. } => {
            check(Feature::Sequence)?;
        }
        Op::PgRaw { .. } => check(Feature::RawSql)?,
        _ => {}
    }

    Ok(())
}

/// **VENDOR (`zero-migrate`)** — the capability-composition gate.
/// For every VENDOR [`Op`](crate::model::ir::Op) variant:
///
/// 1. **SQLite refusal** — every vendor op is `dialect_scope = PgOnly` (no SQLite
///    analogue); a SQLite target is refused [`CODE_UNSUPPORTED`] `{kind:"op"}`
///    at load, never silently skipped.
/// 2. **Capability gate** — the active
///    [`VendorCapabilities`](crate::model::capability::VendorCapabilities), derived from the
///    threaded [`SchemaScope`](crate::model::policy::SchemaScope), must GRANT the op's
///    required [`VendorCapability`](crate::model::capability::VendorCapability). The
///    Confined `Single` scope grants nothing ⇒ every vendor op is
///    [`CODE_VENDOR_OP_DENIED`]. The gate keys on the CAPABILITY FLAG
///    (`caps.grants(cap)`), not on a hard-coded profile name.
///
/// A non-vendor op is a no-op here.
fn validate_vendor_op(
    op: &crate::model::ir::Op,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    schema_scope: Option<&crate::model::policy::SchemaScope>,
) -> Result<(), AuthoringError> {
    let caps = crate::model::op_support::vendor_capabilities(op);
    if caps.is_empty() {
        return Ok(()); // portable-core op — not gated here.
    }

    // (1) SQLite — every vendor op except RawViewBody is PgOnly. Refuse
    // fail-closed at load. RawViewBody is a raw surface but not PgOnly; SQLite can
    // create plain views from a SELECT body.
    if matches!(target_dialect, Dialect::Sqlite)
        && caps
            .iter()
            .any(|cap| !matches!(cap, crate::model::capability::VendorCapability::RawViewBody))
    {
        let cap = caps
            .iter()
            .find(|cap| !matches!(cap, crate::model::capability::VendorCapability::RawViewBody))
            .copied()
            .expect("non-raw-view cap exists");
        let (reason, fix) = if matches!(
            cap,
            crate::model::capability::VendorCapability::MaterializedView
        ) {
            (
                "materializedView: SQLite has no materialized views; materialized:true is PostgreSQL-only"
                    .to_string(),
                "drop materialized:true for SQLite, or target Postgres for this view".to_string(),
            )
        } else {
            (
                format!(
                    "the zero-migrate vendor op (capability {:?}) is Postgres-only — \
                     roles/grants/RLS/partitions/policies/triggers/functions/extensions/schemas/pgRaw have \
                     no SQLite analogue (PgOnly)",
                    cap.as_token()
                ),
                "vendor primitives target Postgres only — deploy this migration against a \
                 Postgres backend, or remove the privileged Postgres op"
                    .to_string(),
            )
        };
        return Err(AuthoringError {
            code: CODE_UNSUPPORTED.to_string(),
            kind: Some(UnsupportedKind::Op),
            op_index,
            ts_location: ts_location.map(str::to_string),
            dialect: target_dialect,
            reason,
            suggested_fix: Some(fix),
        });
    }

    // (2) The capability-composition gate. Derive the active capability set from the
    // threaded scope (the operator-gated, non-spoofable trust signal) and key on the
    // capability FLAG — never a hard-coded profile name.
    let caps = crate::model::capability::VendorCapabilities::from_scope(schema_scope);
    for cap in crate::model::op_support::vendor_capabilities(op) {
        if !caps.grants(cap) {
            return Err(AuthoringError {
                code: CODE_VENDOR_OP_DENIED.to_string(),
                kind: None,
                op_index,
                ts_location: ts_location.map(str::to_string),
                dialect: target_dialect,
                reason: format!(
                    "vendor PG primitive (op capability {:?}) requires the {} capability, which \
                     the active (Confined creator) capability set does not grant — the privileged \
                     zero-migrate primitives are unreachable from a confined migration by \
                     construction",
                    cap.as_token(),
                    cap.flag_name(),
                ),
                suggested_fix: Some(format!(
                    "author this privileged migration under the operator/platform capability set \
                     (which composes {}), not the confined creator profile",
                    cap.flag_name(),
                )),
            });
        }
    }
    Ok(())
}

fn validate_function_type_refs(
    op: &crate::model::ir::Op,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    let reject = |slot: &'static str, value: &str| AuthoringError {
        code: CODE_UNSUPPORTED.to_string(),
        kind: Some(UnsupportedKind::Op),
        op_index,
        ts_location: ts_location.map(str::to_string),
        dialect: target_dialect,
        reason: format!(
            "{slot} must be a conservative PostgreSQL type reference (bare or \
             schema-qualified name with optional precision and [] suffixes), not \
             a SQL fragment: {value:?}"
        ),
        suggested_fix: Some(
            "use a type like text, int[], numeric(10,2), or myschema.mytype; \
             function attributes such as SECURITY DEFINER must be explicit \
             structured fields, not smuggled through a type string"
                .to_string(),
        ),
    };

    match op {
        crate::model::ir::Op::CreateFunction { args, returns, .. } => {
            if !crate::model::ir::is_valid_pg_type_ref(returns) {
                return Err(reject("createFunction.returns", returns));
            }
            if let Some(args) = args {
                for arg in args {
                    if !crate::model::ir::is_valid_pg_type_ref(&arg.ty) {
                        return Err(reject("createFunction.args[].type", &arg.ty));
                    }
                }
            }
        }
        crate::model::ir::Op::DropFunction {
            arg_types: Some(arg_types),
            ..
        } => {
            for ty in arg_types {
                if !crate::model::ir::is_valid_pg_type_ref(ty) {
                    return Err(reject("dropFunction.argTypes[]", ty));
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn view_body_error(
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    reason: String,
    suggested_fix: &'static str,
) -> AuthoringError {
    AuthoringError {
        code: CODE_UNSUPPORTED.to_string(),
        kind: Some(UnsupportedKind::Op),
        op_index,
        ts_location: ts_location.map(str::to_string),
        dialect: target_dialect,
        reason,
        suggested_fix: Some(suggested_fix.to_string()),
    }
}

/// Validate a raw view body before it is admitted by `ViewQuery::Raw`.
///
/// The raw surface is deliberately narrow: it must be exactly one
/// top-level `SELECT` (no DDL/DML utility statement, no semicolon-chained second
/// statement, no `SELECT INTO`) and then it is fed through the same body
/// reparse/string-literal/token deny-list used for function bodies.
pub(crate) fn validate_raw_view_body_sql(
    sql: &str,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    schema_scope: Option<&crate::model::policy::SchemaScope>,
) -> Result<(), AuthoringError> {
    let parsed = pg_query::parse(sql).map_err(|e| {
        view_body_error(
            target_dialect,
            op_index,
            ts_location,
            format!("raw viewBody SQL must parse as exactly one top-level SELECT: {e}"),
            "rewrite the view body as a single SELECT, or use the structured SelectAst builder",
        )
    })?;
    if parsed.protobuf.stmts.len() != 1 {
        return Err(view_body_error(
            target_dialect,
            op_index,
            ts_location,
            format!(
                "raw viewBody SQL must contain exactly one top-level SELECT statement; parsed {} statements",
                parsed.protobuf.stmts.len()
            ),
            "remove semicolon-chained statements from the view body",
        ));
    }
    let stmt = parsed
        .protobuf
        .stmts
        .first()
        .and_then(|raw| raw.stmt.as_ref())
        .and_then(|stmt| stmt.node.as_ref());
    let Some(NodeEnum::SelectStmt(select)) = stmt else {
        return Err(view_body_error(
            target_dialect,
            op_index,
            ts_location,
            "raw viewBody SQL must be a single top-level SELECT; DDL, DML, COPY, and utility statements are refused".to_string(),
            "rewrite the view body as a SELECT, or use the structured SelectAst builder",
        ));
    };
    if select.into_clause.is_some() {
        return Err(view_body_error(
            target_dialect,
            op_index,
            ts_location,
            "raw viewBody SQL uses SELECT INTO, which creates a table and is not a read-only view body".to_string(),
            "drop the INTO clause; a view body must be read-only",
        ));
    }
    // LAYERING EXCEPTION (A3): keep the deny-list scanner in `guard`; duplicating
    // or moving that security policy into `model` would be the worse boundary.
    crate::guard::check_raw_view_body_text(sql, sql, schema_scope).map_err(|e| {
        view_body_error(
            target_dialect,
            op_index,
            ts_location,
            format!("raw viewBody SQL failed the read-only body scanner: {e}"),
            "remove host/file/network/dynamic-SQL escape tokens from the view body",
        )
    })?;
    Ok(())
}

fn validate_table_ref(
    table: &crate::model::ir::TableRef,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    schema_scope: Option<&crate::model::policy::SchemaScope>,
) -> Result<(), AuthoringError> {
    if let Some(schema) = table.schema.as_deref() {
        if !is_safe_schema_ident(schema) {
            return Err(AuthoringError {
                code: CODE_INVALID_SCHEMA_IDENT.to_string(),
                kind: None,
                op_index,
                ts_location: ts_location.map(str::to_string),
                dialect: target_dialect,
                reason: format!(
                    "view SELECT table reference names schema {schema:?}, which is not a safe bare SQL identifier"
                ),
                suggested_fix: Some("use a plain identifier for the table schema".to_string()),
            });
        }
        if let Some(scope) = schema_scope {
            if !scope.permits(schema) {
                return Err(AuthoringError {
                    code: CODE_CROSS_SCHEMA.to_string(),
                    kind: None,
                    op_index,
                    ts_location: ts_location.map(str::to_string),
                    dialect: target_dialect,
                    reason: format!(
                        "view SELECT table reference names schema {schema:?}, which the active schema scope does not permit"
                    ),
                    suggested_fix: Some(
                        "drop the table schema qualifier or use a schema permitted by the active capability scope"
                            .to_string(),
                    ),
                });
            }
        }
    }
    Ok(())
}

fn validate_select_ast(
    select: &crate::model::ir::SelectAst,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    schema_scope: Option<&crate::model::policy::SchemaScope>,
) -> Result<(), AuthoringError> {
    use crate::model::ir::{OrderItem, SelectItem};

    validate_table_ref(
        &select.from,
        target_dialect,
        op_index,
        ts_location,
        schema_scope,
    )?;
    let scope = TargetScope::structural_only(&select.from.name);

    for item in &select.projection {
        if let SelectItem::Expr { expr, .. } = item {
            validate_expr(expr, target_dialect, &scope, op_index, ts_location)?;
        }
    }
    for join in &select.joins {
        validate_table_ref(
            &join.table,
            target_dialect,
            op_index,
            ts_location,
            schema_scope,
        )?;
        validate_expr(&join.on, target_dialect, &scope, op_index, ts_location)?;
    }
    if let Some(pred) = &select.r#where {
        validate_expr(pred, target_dialect, &scope, op_index, ts_location)?;
    }
    for expr in &select.group_by {
        validate_no_aggregate_expr_context(
            expr,
            "view SELECT GROUP BY item",
            target_dialect,
            op_index,
            ts_location,
        )?;
        validate_expr(expr, target_dialect, &scope, op_index, ts_location)?;
    }
    if let Some(pred) = &select.having {
        validate_expr(pred, target_dialect, &scope, op_index, ts_location)?;
    }
    if let Some(order_by) = &select.order_by {
        for item in order_by {
            if let OrderItem::Expr { expr, .. } = item {
                validate_expr(expr, target_dialect, &scope, op_index, ts_location)?;
            }
        }
    }
    Ok(())
}

/// Validate an op's `schema` qualifier + existence-guard direction,
/// BEFORE the per-op expression-slot checks. Three fail-closed checks:
///
/// 1. **Schema identifier safety** — if a `schema` is present it MUST be a safe bare
///    identifier ([`is_safe_schema_ident`], mirroring `dml.rs`'s `quote_ident`
///    shape); an injection-shaped value is rejected ([`CODE_INVALID_SCHEMA_IDENT`])
///    REGARDLESS of profile (the engine double-quotes it, but a fail-closed
///    validate-time reject is the defense the names-are-strings stance needs).
/// 2. **Cross-schema confinement** — under a `Some(scope)` (Confined/Platform) an
///    explicit `schema` the scope does not `permit` is refused
///    ([`CODE_CROSS_SCHEMA`]). Absent schema, or a permitted one, passes.
///    `SchemaScope::Unconfined` skips this for the explicit Trusted operator
///    profile; `None` means default public validation without vendor capabilities.
/// 3. **Existence-guard direction** — a guard whose direction is illegal for the op
///    variant is refused ([`CODE_GUARD_DIRECTION`]).
fn validate_op_schema_and_guard(
    op: &crate::model::ir::Op,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    schema_scope: Option<&crate::model::policy::SchemaScope>,
) -> Result<(), AuthoringError> {
    let mk = |code: &str, reason: String, fix: String| AuthoringError {
        code: code.to_string(),
        kind: None,
        op_index,
        ts_location: ts_location.map(str::to_string),
        dialect: target_dialect,
        reason,
        suggested_fix: Some(fix),
    };

    let check_schema = |schema: &str, what: &str| -> Result<(), AuthoringError> {
        if !is_safe_schema_ident(schema) {
            return Err(mk(
                CODE_INVALID_SCHEMA_IDENT,
                format!(
                    "{what} schema qualifier {schema:?} is not a safe bare SQL identifier \
                     (must be non-empty, start with a letter or '_', and contain only \
                     letters, digits, or '_')"
                ),
                "use a plain identifier for the schema, e.g. schema: \"app2\"".to_string(),
            ));
        }
        if let Some(scope) = schema_scope {
            if !scope.permits(schema) {
                let (reason, fix) = match scope {
                    crate::model::policy::SchemaScope::Single(project) => (
                        format!(
                            "this migration is CONFINED to its project schema {project:?}, but \
                             {what} names a different schema {schema:?} — a cross-schema \
                             migration is refused fail-closed (the creator profile pins the \
                             project schema; the migrator role would also reject it, but this \
                             is the earlier, friendlier gate)"
                        ),
                        format!(
                            "drop the schema qualifier (it defaults to {project:?}) or set \
                             schema: {project:?}"
                        ),
                    ),
                    crate::model::policy::SchemaScope::Allowlist(allowed) => (
                        format!(
                            "{what} names schema {schema:?}, which is not in the permitted \
                             platform schema allow-list {allowed:?}"
                        ),
                        format!("name one of the permitted schemas {allowed:?}"),
                    ),
                    crate::model::policy::SchemaScope::Unconfined => (
                        format!(
                            "internal error: unconfined operator scope unexpectedly refused \
                             schema {schema:?}"
                        ),
                        "report this migrate engine bug".to_string(),
                    ),
                };
                return Err(mk(CODE_CROSS_SCHEMA, reason, fix));
            }
        }
        Ok(())
    };

    // (1) + (2) — the top-level schema qualifier.
    if let Some(schema) = op.schema() {
        check_schema(schema, "op")?;
    }

    // GRANT/REVOKE table targets carry an inner schema that is
    // not `Op::schema()`. Surface it to the same validate-time allowlist gate so
    // an out-of-scope table grant is refused before lower/render.
    match op {
        crate::model::ir::Op::Grant {
            on:
                crate::model::ir::GrantTarget::Table {
                    schema: Some(schema),
                    ..
                },
            ..
        }
        | crate::model::ir::Op::Revoke {
            on:
                crate::model::ir::GrantTarget::Table {
                    schema: Some(schema),
                    ..
                },
            ..
        } => {
            check_schema(schema, "grant table target")?;
        }
        _ => {}
    }

    // (3) — the existence-guard direction.
    if let Some(guard) = op.existence_guard() {
        match op.legal_existence_guard() {
            Some(legal) if legal == guard => {}
            Some(_) => {
                let (got, want, family) = match guard {
                    crate::model::ir::ExistenceGuard::IfExists => {
                        ("ifExists", "ifNotExists", "create*/add*")
                    }
                    crate::model::ir::ExistenceGuard::IfNotExists => {
                        ("ifNotExists", "ifExists", "drop*/rename/alter")
                    }
                };
                return Err(mk(
                    CODE_GUARD_DIRECTION,
                    format!(
                        "existence guard {got:?} is not legal on this op (the {family} family \
                         takes {want:?})"
                    ),
                    format!("use {want:?} on this op, or drop the guard"),
                ));
            }
            None => {
                // A DML op carries no guard slot, so `existence_guard()` is `None`
                // there and this arm is unreachable; defensively refuse.
                return Err(mk(
                    CODE_GUARD_DIRECTION,
                    "this op admits no existence guard".to_string(),
                    "remove the existence guard from this op".to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn sequence_option_error(
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    reason: String,
    suggested_fix: String,
) -> AuthoringError {
    AuthoringError {
        code: CODE_SEQUENCE_OPTION_INVALID.to_string(),
        kind: None,
        op_index,
        ts_location: ts_location.map(str::to_string),
        dialect: target_dialect,
        reason,
        suggested_fix: Some(suggested_fix),
    }
}

fn validate_sequence_numeric_options(
    increment: Option<crate::model::ir::SafeI64>,
    min_value: &Option<Option<crate::model::ir::SafeI64>>,
    max_value: &Option<Option<crate::model::ir::SafeI64>>,
    cache: Option<crate::model::ir::SafeU64>,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    if matches!(increment, Some(n) if n.get() == 0) {
        return Err(sequence_option_error(
            target_dialect,
            op_index,
            ts_location,
            "sequence increment must not be 0".to_string(),
            "use a non-zero sequence increment".to_string(),
        ));
    }
    if matches!(cache, Some(n) if n.get() < 1) {
        return Err(sequence_option_error(
            target_dialect,
            op_index,
            ts_location,
            "sequence cache must be at least 1".to_string(),
            "set cache to 1 or a larger integer".to_string(),
        ));
    }
    if let (Some(Some(min)), Some(Some(max))) = (min_value, max_value) {
        if min.get() > max.get() {
            return Err(sequence_option_error(
                target_dialect,
                op_index,
                ts_location,
                format!(
                    "sequence minValue ({}) must be less than or equal to maxValue ({})",
                    min.get(),
                    max.get()
                ),
                "set minValue <= maxValue, or use null to request the PostgreSQL default bound"
                    .to_string(),
            ));
        }
    }
    Ok(())
}

fn validate_sequence_options(
    op: &crate::model::ir::Op,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    use crate::model::ir::Op;
    match op {
        Op::CreateSequence {
            increment,
            min_value,
            max_value,
            cache,
            ..
        }
        | Op::AlterSequence {
            increment,
            min_value,
            max_value,
            cache,
            ..
        } => validate_sequence_numeric_options(
            *increment,
            min_value,
            max_value,
            *cache,
            target_dialect,
            op_index,
            ts_location,
        ),
        _ => Ok(()),
    }
}

fn unsupported_trigger(
    kind: &'static str,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    reason: String,
    suggested_fix: String,
) -> AuthoringError {
    AuthoringError {
        code: CODE_UNSUPPORTED.to_string(),
        kind: Some(UnsupportedKind::Op),
        op_index,
        ts_location: ts_location.map(str::to_string),
        dialect: target_dialect,
        reason: format!("{kind}: {reason}"),
        suggested_fix: Some(suggested_fix),
    }
}

fn validate_trigger_dialect(
    events: &[crate::model::ir::TriggerEvent],
    for_each: crate::model::ir::ForEach,
    action: &crate::model::ir::TriggerAction,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    match (target_dialect, action) {
        (Dialect::Postgres, crate::model::ir::TriggerAction::Body { .. }) => {
            return Err(unsupported_trigger(
                "triggerBody",
                target_dialect,
                op_index,
                ts_location,
                "Postgres triggers must execute a named trigger function; the closed inline body form renders only on SQLite".to_string(),
                "use action: { kind: \"executeFunction\", name: \"...\" } and create the trigger function separately".to_string(),
            ));
        }
        (
            Dialect::Sqlite | Dialect::Mysql,
            crate::model::ir::TriggerAction::ExecuteFunction { .. },
        ) => {
            let dialect_name = target_dialect.as_str();
            return Err(unsupported_trigger(
                "executeFunction",
                target_dialect,
                op_index,
                ts_location,
                format!("{dialect_name} has no CREATE TRIGGER EXECUTE FUNCTION form"),
                format!("use action: {{ kind: \"body\", statements: [...] }} for {dialect_name} triggers"),
            ));
        }
        _ => {}
    }

    if matches!(target_dialect, Dialect::Sqlite | Dialect::Mysql)
        && events
            .iter()
            .any(|e| matches!(e, crate::model::ir::TriggerEvent::Truncate))
    {
        let dialect_name = target_dialect.as_str();
        return Err(unsupported_trigger(
            "triggerEventTruncate",
            target_dialect,
            op_index,
            ts_location,
            format!("{dialect_name} has no TRUNCATE trigger event"),
            format!(
                "remove the truncate event for {dialect_name}, or target Postgres for this trigger"
            ),
        ));
    }

    if matches!(target_dialect, Dialect::Mysql) && events.len() > 1 {
        return Err(unsupported_trigger(
            "triggerMultipleEvents",
            target_dialect,
            op_index,
            ts_location,
            "MySQL CREATE TRIGGER accepts exactly one trigger event".to_string(),
            "split this into one trigger per event when targeting MySQL".to_string(),
        ));
    }

    if matches!(target_dialect, Dialect::Sqlite | Dialect::Mysql)
        && matches!(for_each, crate::model::ir::ForEach::Statement)
    {
        let dialect_name = target_dialect.as_str();
        return Err(unsupported_trigger(
            "forEachStatement",
            target_dialect,
            op_index,
            ts_location,
            format!("{dialect_name} triggers are row-level only"),
            format!("use forEach: \"row\" for {dialect_name}, or target Postgres for statement-level triggers"),
        ));
    }

    Ok(())
}

fn validate_trigger_stmt(
    stmt: &crate::model::ir::TriggerStmt,
    outer_table: &str,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    schema_scope: Option<&crate::model::policy::SchemaScope>,
) -> Result<(), AuthoringError> {
    let validate_schema = |schema: Option<&str>| -> Result<(), AuthoringError> {
        let Some(schema) = schema else {
            return Ok(());
        };
        if !is_safe_schema_ident(schema) {
            return Err(AuthoringError {
                code: CODE_INVALID_SCHEMA_IDENT.to_string(),
                kind: None,
                op_index,
                ts_location: ts_location.map(str::to_string),
                dialect: target_dialect,
                reason: format!(
                    "trigger body statement schema qualifier {schema:?} is not a safe bare SQL identifier"
                ),
                suggested_fix: Some("use a plain schema identifier or omit the nested schema qualifier".to_string()),
            });
        }
        if let Some(scope) = schema_scope {
            if !scope.permits(schema) {
                return Err(AuthoringError {
                    code: CODE_CROSS_SCHEMA.to_string(),
                    kind: None,
                    op_index,
                    ts_location: ts_location.map(str::to_string),
                    dialect: target_dialect,
                    reason: format!(
                        "trigger body statement names schema {schema:?}, which is outside the active schema scope"
                    ),
                    suggested_fix: Some(
                        "omit the nested schema qualifier or use the permitted project schema".to_string(),
                    ),
                });
            }
        }
        Ok(())
    };

    match stmt {
        crate::model::ir::TriggerStmt::Insert {
            table,
            rows,
            schema,
            ..
        } => {
            validate_schema(schema.as_deref())?;
            let scope = TargetScope::structural_only(table);
            for row in rows {
                for value in row {
                    if let crate::model::ir::IrValue::Expr(expr) = value {
                        validate_expr(expr, target_dialect, &scope, op_index, ts_location)?;
                    }
                }
            }
            Ok(())
        }
        crate::model::ir::TriggerStmt::Update {
            table,
            set,
            r#where,
            schema,
        } => {
            validate_schema(schema.as_deref())?;
            let scope = TargetScope::structural_only(table);
            for value in set.values() {
                if let crate::model::ir::IrValue::Expr(expr) = value {
                    validate_expr(expr, target_dialect, &scope, op_index, ts_location)?;
                }
            }
            if let Some(pred) = r#where {
                validate_expr(pred, target_dialect, &scope, op_index, ts_location)?;
            }
            Ok(())
        }
        crate::model::ir::TriggerStmt::Delete {
            table,
            r#where,
            schema,
            ..
        } => {
            validate_schema(schema.as_deref())?;
            let scope = TargetScope::structural_only(table);
            validate_expr(r#where, target_dialect, &scope, op_index, ts_location)
        }
        crate::model::ir::TriggerStmt::Select { expr } => {
            let scope = TargetScope::structural_only(outer_table);
            validate_expr(expr, target_dialect, &scope, op_index, ts_location)
        }
        crate::model::ir::TriggerStmt::Raise { errcode, .. } => {
            if let Some(code) = errcode {
                let valid = code.len() == 5 && code.chars().all(|c| c.is_ascii_alphanumeric());
                if !valid {
                    return Err(AuthoringError {
                        code: CODE_UNSUPPORTED.to_string(),
                        kind: Some(UnsupportedKind::Op),
                        op_index,
                        ts_location: ts_location.map(str::to_string),
                        dialect: target_dialect,
                        reason: format!(
                            "raise errcode {code:?} is not a five-character SQLSTATE token"
                        ),
                        suggested_fix: Some(
                            "use a five-character SQLSTATE such as \"P0001\", or omit errcode"
                                .to_string(),
                        ),
                    });
                }
            }
            Ok(())
        }
    }
}

/// A safe bare SQL identifier for a schema qualifier: non-empty,
/// alpha/`_`-leading, all chars `[A-Za-z0-9_]`. Mirrors `dml.rs`'s `quote_ident`
/// shape so the validate-time gate and the emitter's double-quoting agree.
#[must_use]
fn is_safe_schema_ident(s: &str) -> bool {
    !s.is_empty()
        && s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn validate_default_for_type(
    position: &str,
    ty: &crate::model::ir::ColType,
    default: &crate::model::ir::IrDefault,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    use crate::model::ir::{ColType, EmptyContainerKind, IrDefault};

    if let IrDefault::Expr { expr } = default {
        validate_default_expr(position, expr, target_dialect, op_index, ts_location)?;
        return Ok(());
    }

    if let IrDefault::Nextval { .. } = default {
        if !matches!(target_dialect, Dialect::Postgres) {
            return Err(AuthoringError {
                code: CODE_UNSUPPORTED.to_string(),
                kind: Some(UnsupportedKind::Op),
                op_index,
                ts_location: ts_location.map(str::to_string),
                dialect: target_dialect,
                reason: format!(
                    "{position} declares a nextval sequence default, but standalone \
                     sequences and nextval defaults are PostgreSQL-only"
                ),
                suggested_fix: Some(
                    "target PostgreSQL, use an identity/auto-increment shape for this dialect, or remove `.default(nextval(...))`"
                        .to_string(),
                ),
            });
        }
        if matches!(ty, ColType::Int | ColType::BigInt | ColType::SmallInt) {
            return Ok(());
        }
        return Err(AuthoringError {
            code: CODE_COLUMN_DEFAULT_TYPE.to_string(),
            kind: None,
            op_index,
            ts_location: ts_location.map(str::to_string),
            dialect: target_dialect,
            reason: format!(
                "{position} declares a nextval sequence default on type {ty:?}; \
                 nextval defaults require an integer column"
            ),
            suggested_fix: Some(
                "use nextval only on int, bigInt, or smallInt columns, or remove `.default(nextval(...))`"
                    .to_string(),
            ),
        });
    }

    if let IrDefault::Json { .. } = default {
        if matches!(ty, ColType::Json) {
            return Ok(());
        }
        return Err(AuthoringError {
            code: CODE_COLUMN_DEFAULT_TYPE.to_string(),
            kind: None,
            op_index,
            ts_location: ts_location.map(str::to_string),
            dialect: target_dialect,
            reason: format!(
                "{position} declares a JSON value default on type {ty:?}; \
                 JSON value defaults are valid only on json columns in v1"
            ),
            suggested_fix: Some(
                "use this default only on json columns, or remove the non-empty object/array default"
                    .to_string(),
            ),
        });
    }

    let IrDefault::Container { kind } = default else {
        return Ok(());
    };
    let ok = matches!(
        (kind, ty),
        (EmptyContainerKind::Object, ColType::Json)
            | (
                EmptyContainerKind::Array,
                ColType::Json | ColType::TextArray
            )
    );
    if ok {
        return Ok(());
    }

    let expected = match kind {
        EmptyContainerKind::Object => "json",
        EmptyContainerKind::Array => "json or textArray",
    };
    Err(AuthoringError {
        code: CODE_COLUMN_DEFAULT_TYPE.to_string(),
        kind: None,
        op_index,
        ts_location: ts_location.map(str::to_string),
        dialect: target_dialect,
        reason: format!(
            "{position} declares an empty {kind:?} container default on type {ty:?}; \
             empty object defaults require json, and empty array defaults require \
             json or textArray"
        ),
        suggested_fix: Some(format!(
            "use this default only on {expected} columns, or remove `.default({{}})` / `.default([])`"
        )),
    })
}

fn validate_default_expr(
    position: &str,
    expr: &Expr,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    let scope = TargetScope::structural_only(position);
    validate_expr(expr, target_dialect, &scope, op_index, ts_location)?;
    validate_no_aggregate_expr_context(expr, position, target_dialect, op_index, ts_location)?;

    fn mk_err(
        reason: String,
        target_dialect: Dialect,
        op_index: usize,
        ts_location: Option<&str>,
    ) -> AuthoringError {
        AuthoringError {
            code: CODE_OP_INVALID.to_string(),
            kind: Some(UnsupportedKind::Expr),
            op_index,
            ts_location: ts_location.map(str::to_string),
            dialect: target_dialect,
            reason,
            suggested_fix: Some(
                "use only literals, CASE, immutable scalar helpers, now(), uuidV4(), and uuidV7() in column defaults"
                    .to_string(),
            ),
        }
    }

    fn walk(
        expr: &Expr,
        target_dialect: Dialect,
        op_index: usize,
        ts_location: Option<&str>,
    ) -> Result<(), AuthoringError> {
        match expr {
            Expr::ColRef { .. } => Err(mk_err(
                "a column default cannot reference a column".to_string(),
                target_dialect,
                op_index,
                ts_location,
            )),
            Expr::Agg { .. } => Err(mk_err(
                "a column default cannot use an aggregate".to_string(),
                target_dialect,
                op_index,
                ts_location,
            )),
            Expr::FnCall { r#fn, args } => {
                if matches!(r#fn, ScalarFn::CurrentSetting | ScalarFn::CurrentUser) {
                    return Err(mk_err(
                        "a column default cannot use volatile or vendor-only functions".to_string(),
                        target_dialect,
                        op_index,
                        ts_location,
                    ));
                }
                for arg in args {
                    walk(arg, target_dialect, op_index, ts_location)?;
                }
                Ok(())
            }
            Expr::PgRegexMatch { .. }
            | Expr::PgColumnSize { .. }
            | Expr::PgExtract { .. }
            | Expr::PgInterval { .. }
            | Expr::Dialectal { .. } => Err(mk_err(
                "a column default cannot use volatile, dialect-specific, or vendor-only expression nodes"
                    .to_string(),
                target_dialect,
                op_index,
                ts_location,
            )),
            Expr::Extract { .. } => Err(mk_err(
                "a column default cannot use an EXTRACT expression".to_string(),
                target_dialect,
                op_index,
                ts_location,
            )),
            Expr::Literal { .. } | Expr::UuidV4 | Expr::UuidV7 => Ok(()),
            Expr::BinOp { lhs, rhs, .. } => {
                walk(lhs, target_dialect, op_index, ts_location)?;
                walk(rhs, target_dialect, op_index, ts_location)
            }
            Expr::UnaryOp { operand, .. } => walk(operand, target_dialect, op_index, ts_location),
            Expr::Case { branches, r#else } => {
                for CaseBranch { when, then } in branches {
                    walk(when, target_dialect, op_index, ts_location)?;
                    walk(then, target_dialect, op_index, ts_location)?;
                }
                if let Some(expr) = r#else {
                    walk(expr, target_dialect, op_index, ts_location)?;
                }
                Ok(())
            }
            Expr::FnSynth { args, .. } => {
                for arg in args {
                    walk(arg, target_dialect, op_index, ts_location)?;
                }
                Ok(())
            }
            Expr::Cast { operand, .. } => walk(operand, target_dialect, op_index, ts_location),
            Expr::Between { operand, low, high } => {
                walk(operand, target_dialect, op_index, ts_location)?;
                walk(low, target_dialect, op_index, ts_location)?;
                walk(high, target_dialect, op_index, ts_location)
            }
            Expr::Like { operand, pattern } => {
                walk(operand, target_dialect, op_index, ts_location)?;
                walk(pattern, target_dialect, op_index, ts_location)
            }
            Expr::DistinctFrom { left, right } => {
                walk(left, target_dialect, op_index, ts_location)?;
                walk(right, target_dialect, op_index, ts_location)
            }
            Expr::InList { expr, .. } => walk(expr, target_dialect, op_index, ts_location),
        }
    }

    walk(expr, target_dialect, op_index, ts_location)
}

/// Validate one [`IrColumn`](crate::model::ir::IrColumn)'s
/// declared-only facets (`value_format` / `id_prefix` / `vector_metric`) against
/// their bounds.
///
/// Three fail-closed checks, with the IR's hand-crafted-IR envelope threat model in
/// mind (the closed-enum + `deny_unknown_fields` design):
///
/// 1. **`id_prefix`** — a legacy internal platform-ID prefix, distinct from
///    TypeID, which must obey the internal `^[a-z][a-z0-9_]*$`
///    charset rule + reserved-prefix deny-list (`usr`, …) the runtime enforces via
///    [`crate::schema::query::validate_id_prefix`] (the SINGLE source of truth,
///    mirroring `crates/core/src/typed_id.rs` + `system_fields_pass`'s
///    `RESERVED_AUTO_PREFIXES`), PLUS a [`MAX_ID_PREFIX_LEN`] length bound so a
///    hand-authored prefix keeps the compact `<prefix>_<22 base62 UUIDv7>` shape.
///    A reserved/malformed/over-long prefix is [`CODE_INVALID_ID_PREFIX`], refused
///    BEFORE lower — never a render-time surprise minting colliding `usr_…` ids.
/// 2. **`value_format`** — TypeID prefixes obey the distinct TypeID 0.3 grammar;
///    TypeID and ULID formats co-occur only with exact
///    [`ColType::Text`](crate::model::ir::ColType::Text) storage and never with
///    `caseSensitive:false`.
/// 3. **`vector_metric`** — structurally bounded by the closed
///    [`crate::model::ir::VectorMetric`] enum at deserialize; the only authoring error
///    left is CO-OCCURRENCE: a metric carried on a non-`Vector` column is
///    meaningless (the opclass has no vector to apply to) and is refused
///    ([`CODE_VECTOR_METRIC_MISPLACED`]) so a hand-crafted artifact cannot ride a
///    dead field in.
///
/// # Errors
/// [`CODE_INVALID_ID_PREFIX`] / [`CODE_INVALID_TYPE_ID_PREFIX`] /
/// [`CODE_VECTOR_METRIC_MISPLACED`] as above.
fn validate_column_facets(
    col: &crate::model::ir::IrColumn,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    validate_col_type_position(
        &col.ty,
        "column.type",
        false,
        target_dialect,
        op_index,
        ts_location,
    )?;

    let mk = |code: &str, reason: String, fix: String| AuthoringError {
        code: code.to_string(),
        kind: None,
        op_index,
        ts_location: ts_location.map(str::to_string),
        dialect: target_dialect,
        reason,
        suggested_fix: Some(fix),
    };
    let unsupported = |kind: UnsupportedKind, reason: String, fix: String| AuthoringError {
        code: CODE_UNSUPPORTED.to_string(),
        kind: Some(kind),
        op_index,
        ts_location: ts_location.map(str::to_string),
        dialect: target_dialect,
        reason,
        suggested_fix: Some(fix),
    };

    if col.generated.is_some() && col.default.is_some() {
        return Err(mk(
            CODE_COLUMN_FACET_CONFLICT,
            format!(
                "column {:?} is generated and also declares a default; generated columns \
                 cannot have DEFAULT values",
                col.name
            ),
            "remove either `.generated(...)` or `.default(...)` from the column".to_string(),
        ));
    }
    if col.identity.is_some() && col.default.is_some() {
        return Err(mk(
            CODE_COLUMN_FACET_CONFLICT,
            format!(
                "column {:?} is an identity column and also declares a default; identity \
                 columns cannot have DEFAULT values",
                col.name
            ),
            "remove either `.identity(...)` or `.default(...)` from the column".to_string(),
        ));
    }
    if col.identity.is_some() && col.generated.is_some() {
        return Err(mk(
            CODE_COLUMN_FACET_CONFLICT,
            format!(
                "column {:?} declares both identity and generated facets; SQL identity \
                 and generated/computed columns are mutually exclusive",
                col.name
            ),
            "remove either `.identity(...)` or `.generated(...)` from the column".to_string(),
        ));
    }

    if let Some(default) = &col.default {
        validate_default_for_type(
            &format!("column {:?}.default", col.name),
            &col.ty,
            default,
            target_dialect,
            op_index,
            ts_location,
        )?;
    }

    if matches!(target_dialect, Dialect::Postgres)
        && matches!(col.generated.as_ref(), Some(generated) if !generated.stored)
    {
        return Err(unsupported(
            UnsupportedKind::VirtualColumn,
            format!(
                "column {:?} requests a VIRTUAL generated column, but Postgres supports \
                 generated columns only as STORED",
                col.name
            ),
            "use `.generated(expr)` / `{ virtual: false }` for Postgres, or target SQLite"
                .to_string(),
        ));
    }

    if col.identity.is_some()
        && !matches!(
            col.ty,
            crate::model::ir::ColType::SmallInt
                | crate::model::ir::ColType::Int
                | crate::model::ir::ColType::BigInt
        )
    {
        return Err(unsupported(
            UnsupportedKind::Identity,
            format!(
                "column {:?} declares identity on a non-integer type; identity is only \
                 supported on smallInt/int/bigInt columns",
                col.name
            ),
            "declare the column as `t.smallInt().identity(...)`, `t.int().identity(...)`, \
             or `t.bigInt().identity(...)`"
                .to_string(),
        ));
    }

    if let Some(prefix) = &col.id_prefix {
        // Charset + reserved deny-list — the runtime's single source of truth.
        if let Err(e) = crate::schema::query::validate_id_prefix(prefix) {
            return Err(mk(
                CODE_INVALID_ID_PREFIX,
                format!(
                    "column {:?} declares an invalid internal platform-ID prefix {prefix:?}: {e}",
                    col.name
                ),
                "use a prefix matching ^[a-z][a-z0-9_]*$ that is not platform-reserved \
                 (e.g. \"post\", \"org\")"
                    .to_string(),
            ));
        }
        // Length bound — keep the compact typed-id shape (charset already checked).
        if prefix.len() > MAX_ID_PREFIX_LEN {
            return Err(mk(
                CODE_INVALID_ID_PREFIX,
                format!(
                    "column {:?} declares an internal platform-ID prefix {prefix:?} of {} bytes; the \
                     maximum is {MAX_ID_PREFIX_LEN} (the legacy prefix is kept short so \
                     the minted `<prefix>_<22 base62 UUIDv7>` id stays compact)",
                    col.name,
                    prefix.len()
                ),
                format!("shorten the prefix to at most {MAX_ID_PREFIX_LEN} characters"),
            ));
        }
    }

    if let Some(value_format) = &col.value_format {
        let format_name = match value_format {
            crate::model::ir::ValueFormat::TypeId { prefix } => {
                if let Err(error) = crate::model::ir::validate_type_id_prefix(prefix) {
                    return Err(mk(
                        CODE_INVALID_TYPE_ID_PREFIX,
                        format!(
                            "column {:?} declares an invalid TypeID prefix {prefix:?}: {error}",
                            col.name
                        ),
                        "use an empty prefix, or at most 63 lowercase ASCII letters and underscores, starting and ending with a letter"
                            .to_string(),
                    ));
                }
                "TypeID"
            }
            crate::model::ir::ValueFormat::Ulid => "ULID",
        };

        if !matches!(col.ty, crate::model::ir::ColType::Text) {
            return Err(mk(
                CODE_COLUMN_FACET_CONFLICT,
                format!(
                    "column {:?} declares a {format_name} value format on a non-text storage type; {format_name} requires exact text storage",
                    col.name,
                ),
                format!(
                    "declare the column with text storage, or remove the {format_name} value format"
                ),
            ));
        }

        if matches!(col.case_sensitive, Some(false)) {
            return Err(mk(
                CODE_COLUMN_FACET_CONFLICT,
                format!(
                    "column {:?} declares both a {format_name} value format and caseSensitive:false; {format_name} requires bytewise, case-sensitive comparison",
                    col.name,
                ),
                format!(
                    "remove caseSensitive:false so {format_name} storage keeps bytewise comparison semantics"
                ),
            ));
        }
    }

    if col.vector_metric.is_some() && !matches!(col.ty, crate::model::ir::ColType::Vector { .. }) {
        return Err(mk(
            CODE_VECTOR_METRIC_MISPLACED,
            format!(
                "column {:?} carries a vector_metric but is not a vector column; a \
                 distance metric only applies to a t.vector(n) column",
                col.name
            ),
            "drop the metric, or declare the column as t.vector(n, { metric })".to_string(),
        ));
    }

    if matches!(col.case_sensitive, Some(false))
        && !matches!(col.ty, crate::model::ir::ColType::Text)
    {
        return Err(mk(
            CODE_UNSUPPORTED,
            format!(
                "column {:?} declares caseSensitive:false but is not a text column; \
                 caseSensitive:false is only valid on a text column",
                col.name
            ),
            "drop the caseSensitive facet, or declare the column as t.text({ caseSensitive: false })"
                .to_string(),
        ));
    }

    Ok(())
}

fn validate_col_type_position(
    ty: &crate::model::ir::ColType,
    position: &'static str,
    _allow_pg_domain_date_base: bool,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    use crate::model::ir::ColType;

    if matches!(ty, ColType::Char { length: 0 }) {
        return Err(AuthoringError {
            code: CODE_UNSUPPORTED.to_string(),
            kind: Some(UnsupportedKind::Op),
            op_index,
            ts_location: ts_location.map(str::to_string),
            dialect: target_dialect,
            reason: format!(
                "{position} uses `char(0)`; fixed-length char requires a positive length"
            ),
            suggested_fix: Some("use `t.char(1)` or larger".to_string()),
        });
    }

    Ok(())
}

fn validate_identity_placement(
    col: &crate::model::ir::IrColumn,
    target_dialect: Dialect,
    pk_cols: Option<&[String]>,
    is_add_column: bool,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    let Some(identity) = col.identity else {
        return Ok(());
    };
    if !matches!(target_dialect, Dialect::Sqlite | Dialect::Mysql) {
        return Ok(());
    }
    let err = |reason: String| AuthoringError {
        code: CODE_UNSUPPORTED.to_string(),
        kind: Some(UnsupportedKind::Identity),
        op_index,
        ts_location: ts_location.map(str::to_string),
        dialect: target_dialect,
        reason,
        suggested_fix: Some(
            "use identity only on the sole integer primary key for this dialect, or remove \
             `.identity(...)`"
                .to_string(),
        ),
    };
    if identity.always {
        return Err(err(
            "identity({ always: true }) is PostgreSQL-only; SQLite/MySQL support \
             only identity({ always: false }) / autoIncrement() on the sole integer \
             primary key"
                .to_string(),
        ));
    }
    if is_add_column {
        return Err(err(
            "autoIncrement identity: non-PK identity has no sound target-dialect \
             render; SQLite AUTOINCREMENT and MySQL AUTO_INCREMENT are only sound \
             on the sole integer primary key"
                .to_string(),
        ));
    }
    let Some(pk_cols) = pk_cols else {
        return Err(err(format!(
            "autoIncrement identity: column {:?} is not the declared primary key; \
             non-PK identity has no sound target-dialect render",
            col.name
        )));
    };
    if pk_cols.len() == 1 && pk_cols[0] == col.name {
        return Ok(());
    }
    Err(err(format!(
        "autoIncrement identity: column {:?} is part of {:?}, but this dialect's \
         identity is only sound for the sole integer primary key",
        col.name, pk_cols
    )))
}

/// **Apply/render-seam ColRef resolution (rule (c)).** Re-run the
/// expression-AST walk for the ops whose live-schema column set was NOT known at
/// IR-load time — the DML ops (`update`/`delete`/`backfill`) and `setColumnType`
/// — now that the render/apply seam HAS the live columns. For each such op whose
/// target table appears in `live_columns`, the embedded predicates / set RHS /
/// cast are re-validated with a **RESOLVING** [`TargetScope`], so an unresolved
/// `ColRef` is rejected with the structured [`AuthoringError`] (rule (c)) at apply
/// — NOT as an opaque raw DB error mid-statement.
///
/// `live_columns` maps a target table → its live column names (system fields
/// included). An op whose table is absent from the map keeps the structural-only
/// scope (the (c) check is skipped — the caller could not resolve that table).
/// Non-DML / non-`setColumnType` ops are revalidated structurally (a),(b),(d)
/// — harmless and keeps the walk total.
///
/// This is the seam the `validate_ir` doc ("the apply/render seam re-runs
/// the walk with a resolved column set to enforce (c)") names. The apply path
/// calls this BEFORE rendering the DML statement.
///
/// # Errors
/// The first [`AuthoringError`] any embedded expression produces — incl. a rule
/// (c) `ColRef`-resolution failure now that the column set is known.
pub fn validate_ir_resolved(
    ir: &crate::model::ir::MigrationIr,
    target_dialect: Dialect,
    live_columns: &std::collections::BTreeMap<String, Vec<String>>,
    ts_locations: &[Option<String>],
) -> Result<(), AuthoringError> {
    for (op_index, op) in ir.ops.iter().enumerate() {
        let ts = ts_locations.get(op_index).and_then(Option::as_deref);
        validate_op_resolved(op, target_dialect, live_columns, op_index, ts)?;
    }
    validate_per_row_destinations(ir, target_dialect, ts_locations)?;
    validate_online_rename_sequence(ir, target_dialect, ts_locations)?;
    validate_partition_recording(ir, target_dialect, ts_locations)?;
    Ok(())
}

/// **Single-op apply/render-seam ColRef resolution (rule (c)).** The per-op
/// peer of [`validate_ir_resolved`]: re-run the expression-AST walk for ONE op with
/// a RESOLVING [`TargetScope`] when its target table's live column set is known.
///
/// This is the seam the DML LOWER calls ([`crate::render::lower::IrAuthor::lower_dml_op`]):
/// at lower/apply the live schema HAS been introspected, so each DML op
/// (`update`/`delete`/`backfill`) / `setColumnType` resolves its embedded
/// `ColRef`s against the live target-table columns BEFORE the SQL template is
/// assembled. A `ColRef` to a column NOT on the enclosing target table (or a
/// synthesized cross-table reference) is rejected with the structured
/// [`AuthoringError`] (`UNSUPPORTED { kind: "expr" }`, rule (c)) at apply — NOT as
/// an opaque raw DB `column does not exist` error mid-statement (the (c) check
/// runs "at apply/render time").
///
/// `live_columns` maps a target table → its live column names (system fields
/// included). An op whose table is ABSENT from the map keeps the structural-only
/// scope (the (c) check is skipped — the caller could not resolve that table; the
/// (a)/(b)/(d) structural checks still run). A non-DML / non-`setColumnType` op
/// re-runs the structural [`validate_op`] (harmless; keeps the walk total).
///
/// # Errors
/// The first [`AuthoringError`] the op's embedded expressions produce — incl. a
/// rule (c) `ColRef`-resolution failure now that the column set is known.
pub fn validate_op_resolved(
    op: &crate::model::ir::Op,
    target_dialect: Dialect,
    live_columns: &std::collections::BTreeMap<String, Vec<String>>,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    use crate::model::ir::Op;
    let ts = ts_location;
    validate_op_support(op, target_dialect, op_index, ts)?;
    // The op's target table (for the DML / setColumnType ops we resolve).
    let resolved_scope = |table: &str| -> Option<Vec<String>> { live_columns.get(table).cloned() };
    match op {
        Op::Update {
            table,
            set,
            r#where,
            ..
        } => {
            if let Some(cols) = resolved_scope(table) {
                let scope = TargetScope::new(table, &cols);
                for value in set.values() {
                    if let crate::model::ir::IrValue::Expr(expr) = value {
                        validate_expr(expr, target_dialect, &scope, op_index, ts)?;
                    }
                }
                if let Some(pred) = r#where {
                    validate_expr(pred, target_dialect, &scope, op_index, ts)?;
                }
            } else {
                validate_op(op, target_dialect, op_index, ts)?;
            }
        }
        Op::Delete { table, r#where, .. } => {
            if let Some(cols) = resolved_scope(table) {
                let scope = TargetScope::new(table, &cols);
                validate_expr(r#where, target_dialect, &scope, op_index, ts)?;
            } else {
                validate_op(op, target_dialect, op_index, ts)?;
            }
        }
        Op::Backfill {
            table,
            cursor_columns,
            cursor_stability,
            set,
            filter,
            ..
        } => {
            validate_backfill_cursor_fields(
                cursor_columns,
                cursor_stability,
                set,
                target_dialect,
                op_index,
                ts,
            )?;
            if let Some(cols) = resolved_scope(table) {
                let scope = TargetScope::new(table, &cols);
                for value in set.values() {
                    if let crate::model::ir::BackfillSetValue::Value(
                        crate::model::ir::IrValue::Expr(expr),
                    ) = value
                    {
                        validate_expr(expr, target_dialect, &scope, op_index, ts)?;
                    }
                }
                if let Some(pred) = filter {
                    validate_expr(pred, target_dialect, &scope, op_index, ts)?;
                }
            } else {
                validate_op(op, target_dialect, op_index, ts)?;
            }
        }
        Op::SetColumnType {
            table,
            to_type,
            using,
            ..
        } => {
            validate_col_type_position(
                to_type,
                "setColumnType.toType",
                false,
                target_dialect,
                op_index,
                ts,
            )?;
            if let (Some(cols), Some(cast)) = (resolved_scope(table), using) {
                let scope = TargetScope::new(table, &cols);
                validate_expr(cast, target_dialect, &scope, op_index, ts)?;
            } else {
                validate_op(op, target_dialect, op_index, ts)?;
            }
        }
        // SA-18: insert row cells and `on_conflict.do_update` values can carry a
        // closed Expr (a DB-evaluated synth scalar or `DO UPDATE SET n = n + 1`).
        // When the target table resolves, walk every `IrValue::Expr` through a real
        // resolving `TargetScope` so a ColRef to a non-existent column is rejected
        // here, not as an opaque mid-statement DB error — symmetric with the
        // Update/Delete/Backfill/SetColumnType arms above.
        Op::Insert {
            table,
            rows,
            on_conflict,
            ..
        } => {
            if let Some(cols) = resolved_scope(table) {
                let scope = TargetScope::new(table, &cols);
                for row in rows {
                    for cell in row {
                        if let crate::model::ir::IrValue::Expr(e) = cell {
                            validate_expr(e, target_dialect, &scope, op_index, ts)?;
                        }
                    }
                }
                if let Some(do_update) = on_conflict.as_ref().and_then(|oc| oc.do_update.as_ref()) {
                    for v in do_update.values() {
                        if let crate::model::ir::IrValue::Expr(e) = v {
                            validate_expr(e, target_dialect, &scope, op_index, ts)?;
                        }
                    }
                }
            } else {
                validate_op(op, target_dialect, op_index, ts)?;
            }
        }
        // Every other op: revalidate structurally (its own scope is already
        // resolved or has no Expr slot).
        other => validate_op(other, target_dialect, op_index, ts)?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::expr::{
        AggFunc, BinaryOp, CastTarget, Expr, ExtractField, PgExtractField, ScalarFn, SynthFn,
        UnaryOp,
    };
    use crate::model::ir::{IndexElement, IrScalar, IrValue};

    fn cols() -> Vec<String> {
        vec![
            "name".into(),
            "first".into(),
            "last".into(),
            "total".into(),
            "active".into(),
        ]
    }

    fn scope<'a>(table: &'a str, cols: &'a [String]) -> TargetScope<'a> {
        TargetScope::new(table, cols)
    }

    // ── DoS guard: explicit walk depth bound ────────────────────────────────
    // The validator OWNS the recursion bound (MAX_EXPR_DEPTH), not an
    // implicit serde_json::recursion_limit. Build the AST in Rust (bypassing
    // serde entirely, exactly as a future streaming/custom deserializer or a
    // raised serde limit would) and assert the walker still refuses an
    // over-deep tree as CODE_UNSUPPORTED rather than recursing to a stack
    // overflow.

    /// Wrap `inner` in `depth` nested `UnaryOp::Not` nodes.
    fn nest_not(depth: u32, inner: Expr) -> Expr {
        let mut e = inner;
        for _ in 0..depth {
            e = Expr::UnaryOp {
                op: UnaryOp::Not,
                operand: Box::new(e),
            };
        }
        e
    }

    #[test]
    fn walk_refuses_over_deep_expression_as_unsupported() {
        let c = cols();
        let sc = scope("users", &c);
        // Comfortably past the bound — would stack-overflow a naive walker.
        let deep = nest_not(MAX_EXPR_DEPTH + 50, Expr::col("name"));
        let err = validate_expr(&deep, Dialect::Postgres, &sc, 0, None)
            .expect_err("an over-deep expression must be refused, not recursed");
        assert_eq!(err.code, CODE_UNSUPPORTED, "got: {err}");
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        assert!(
            err.reason.contains("nesting"),
            "the error must name the depth bound, got: {}",
            err.reason
        );
    }

    #[test]
    fn walk_accepts_expression_within_the_depth_bound() {
        let c = cols();
        let sc = scope("users", &c);
        // A legitimately-shallow tree (well under the bound) still validates —
        // the bound never narrows the realistic accepted set.
        let ok = nest_not(MAX_EXPR_DEPTH - 2, Expr::col("name"));
        assert!(
            validate_expr(&ok, Dialect::Postgres, &sc, 0, None).is_ok(),
            "a tree within the depth bound must validate"
        );
    }

    #[test]
    fn current_setting_and_current_user_are_pg_only_rejected_off_postgres() {
        // Regression: current_setting / current_user are PG-only VENDOR scalars
        // (they render as PG built-ins with no SQLite/MySQL form). A portable op
        // carrying them must be REFUSED at validate on SQLite/MySQL — not sail
        // through and break at apply.
        let c = cols();
        let sc = scope("users", &c);
        for f in [ScalarFn::CurrentUser, ScalarFn::CurrentSetting] {
            let e = Expr::FnCall {
                r#fn: f,
                args: vec![],
            };
            assert!(
                validate_expr(&e, Dialect::Postgres, &sc, 0, None).is_ok(),
                "{f:?} must validate on Postgres"
            );
            for d in [Dialect::Sqlite, Dialect::Mysql] {
                let err = validate_expr(&e, d, &sc, 0, None)
                    .expect_err("a PG-only vendor scalar must be refused off Postgres");
                assert_eq!(err.code, CODE_UNSUPPORTED, "{f:?} on {d:?}: {err}");
                assert_eq!(err.kind, Some(UnsupportedKind::Expr));
            }
        }
    }

    // ── (a) every allow-listed node validates ──────────────────────────────

    #[test]
    fn all_allow_listed_nodes_validate() {
        let c = cols();
        let sc = scope("users", &c);
        // A representative tree using each node kind.
        let e = Expr::BinOp {
            op: BinaryOp::And,
            lhs: Box::new(Expr::UnaryOp {
                op: UnaryOp::IsNotNull,
                operand: Box::new(Expr::col("name")),
            }),
            rhs: Box::new(Expr::BinOp {
                op: BinaryOp::Gt,
                lhs: Box::new(Expr::Cast {
                    operand: Box::new(Expr::FnCall {
                        r#fn: ScalarFn::Length,
                        args: vec![Expr::col("name")],
                    }),
                    target: CastTarget::Int,
                }),
                rhs: Box::new(Expr::lit(IrScalar::Int(0))),
            }),
        };
        assert!(validate_expr(&e, Dialect::Sqlite, &sc, 0, None).is_ok());

        // Case + FnCall(coalesce) + concat.
        let case = Expr::Case {
            branches: vec![CaseBranch {
                when: Expr::UnaryOp {
                    op: UnaryOp::IsNull,
                    operand: Box::new(Expr::col("first")),
                },
                then: Expr::lit(IrScalar::Str("none".into())),
            }],
            r#else: Some(Box::new(Expr::FnCall {
                r#fn: ScalarFn::Coalesce,
                args: vec![Expr::col("first"), Expr::lit(IrScalar::Str("".into()))],
            })),
        };
        assert!(validate_expr(&case, Dialect::Postgres, &sc, 1, None).is_ok());
    }

    fn in_list(expr: Expr, elems: Vec<&str>) -> Expr {
        Expr::InList {
            expr: Box::new(expr),
            elems: elems
                .into_iter()
                .map(|s| IrScalar::Str(s.to_string()))
                .collect(),
            negated: false,
        }
    }

    fn not_in_list(expr: Expr, elems: Vec<&str>) -> Expr {
        Expr::InList {
            expr: Box::new(expr),
            elems: elems
                .into_iter()
                .map(|s| IrScalar::Str(s.to_string()))
                .collect(),
            negated: true,
        }
    }

    #[test]
    fn pg_only_and_pg_mysql_expr_nodes_validate_on_supported_dialects() {
        let c = cols();
        let sc = scope("users", &c);
        let regex = Expr::PgRegexMatch {
            expr: Box::new(Expr::col("name")),
            pattern: "^[a-z]+$".to_string(),
        };
        for d in [Dialect::Postgres, Dialect::Mysql] {
            validate_expr(&regex, d, &sc, 0, None)
                .unwrap_or_else(|err| panic!("regex expression must validate on {d:?}: {err}"));
        }
        for e in [
            Expr::BinOp {
                op: BinaryOp::Le,
                lhs: Box::new(Expr::PgColumnSize {
                    expr: Box::new(Expr::col("name")),
                }),
                rhs: Box::new(Expr::lit(IrScalar::Int(8192))),
            },
            Expr::PgExtract {
                field: PgExtractField::Epoch,
                from: Box::new(Expr::col("total")),
            },
        ] {
            validate_expr(&e, Dialect::Postgres, &sc, 0, None)
                .unwrap_or_else(|err| panic!("PG-only expression must validate on PG: {err}"));
        }
    }

    #[test]
    fn portable_predicate_and_extract_nodes_validate_on_all_three_dialects() {
        // between / like / distinctFrom / inList / extract are PORTABLE:
        // they render on all three dialects (the engine owns each per-dialect
        // lowering), so the walk accepts them with NO dialect gate — including on
        // SQLite/MySQL, exactly where the PG-only nodes are refused.
        let c = cols();
        let sc = scope("users", &c);
        let nodes = [
            Expr::Between {
                operand: Box::new(Expr::col("total")),
                low: Box::new(Expr::lit(IrScalar::Int(0))),
                high: Box::new(Expr::lit(IrScalar::Int(100))),
            },
            Expr::Like {
                operand: Box::new(Expr::col("name")),
                pattern: Box::new(Expr::lit(IrScalar::Str("A%".into()))),
            },
            Expr::DistinctFrom {
                left: Box::new(Expr::col("first")),
                right: Box::new(Expr::col("last")),
            },
            in_list(Expr::col("name"), vec!["active", "past_due"]),
            not_in_list(Expr::col("name"), vec!["suspended"]),
            in_list(Expr::col("name"), vec![]),
            Expr::InList {
                expr: Box::new(Expr::col("total")),
                elems: vec![IrScalar::Int(200), IrScalar::Int(404), IrScalar::Int(500)],
                negated: false,
            },
            Expr::InList {
                expr: Box::new(Expr::col("active")),
                elems: vec![IrScalar::Bool(true), IrScalar::Bool(false)],
                negated: false,
            },
            Expr::Extract {
                field: ExtractField::Year,
                from: Box::new(Expr::col("total")),
            },
            Expr::Extract {
                field: ExtractField::Month,
                from: Box::new(Expr::col("total")),
            },
            Expr::Extract {
                field: ExtractField::Day,
                from: Box::new(Expr::col("total")),
            },
            Expr::Extract {
                field: ExtractField::Hour,
                from: Box::new(Expr::col("total")),
            },
            Expr::Extract {
                field: ExtractField::Minute,
                from: Box::new(Expr::col("total")),
            },
            Expr::Extract {
                field: ExtractField::Dow,
                from: Box::new(Expr::col("total")),
            },
        ];
        for e in &nodes {
            for d in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
                validate_expr(e, d, &sc, 0, None).unwrap_or_else(|err| {
                    panic!("portable predicate/extract must validate on {d:?}: {err}")
                });
            }
        }
    }

    #[test]
    fn portable_aggregate_nodes_validate_on_all_three_dialects() {
        use crate::model::expr::AggFunc;
        // count(*) / count(DISTINCT col) / sum/avg/min/max(col) are portable:
        // byte-identical SQL on PG/SQLite/MySQL, so the walk accepts them with NO
        // dialect gate. PG-first aggregate variants are covered by the next test.
        let c = cols();
        let sc = scope("users", &c);
        let nodes = [
            Expr::Agg {
                func: AggFunc::Count,
                arg: None,
                delimiter: None,
                distinct: false,
            },
            Expr::Agg {
                func: AggFunc::Count,
                arg: Some(Box::new(Expr::col("total"))),
                delimiter: None,
                distinct: true,
            },
            Expr::Agg {
                func: AggFunc::Sum,
                arg: Some(Box::new(Expr::col("total"))),
                delimiter: None,
                distinct: false,
            },
            Expr::Agg {
                func: AggFunc::Avg,
                arg: Some(Box::new(Expr::col("total"))),
                delimiter: None,
                distinct: false,
            },
            Expr::Agg {
                func: AggFunc::Min,
                arg: Some(Box::new(Expr::col("total"))),
                delimiter: None,
                distinct: false,
            },
            Expr::Agg {
                func: AggFunc::Max,
                arg: Some(Box::new(Expr::col("total"))),
                delimiter: None,
                distinct: false,
            },
        ];
        for e in &nodes {
            for d in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
                validate_expr(e, d, &sc, 0, None).unwrap_or_else(|err| {
                    panic!("portable aggregate must validate on {d:?}: {err}")
                });
            }
        }
        // A bogus column inside the aggregate arg is still caught by the recursive
        // colref check (the node isn't a blind accept).
        let bad = Expr::Agg {
            func: AggFunc::Sum,
            arg: Some(Box::new(Expr::col("does_not_exist"))),
            delimiter: None,
            distinct: false,
        };
        assert!(
            validate_expr(&bad, Dialect::Postgres, &sc, 0, None).is_err(),
            "aggregate must still validate its argument's column ref"
        );
    }

    #[test]
    fn pg_first_aggregate_nodes_validate_only_on_postgres() {
        use crate::model::expr::AggFunc;
        let c = cols();
        let sc = scope("users", &c);
        let nodes = [
            Expr::Agg {
                func: AggFunc::StringAgg,
                arg: Some(Box::new(Expr::col("name"))),
                delimiter: Some(Box::new(Expr::lit(IrScalar::Str(", ".to_string())))),
                distinct: false,
            },
            Expr::Agg {
                func: AggFunc::ArrayAgg,
                arg: Some(Box::new(Expr::col("name"))),
                delimiter: None,
                distinct: false,
            },
            Expr::Agg {
                func: AggFunc::BoolAnd,
                arg: Some(Box::new(Expr::col("active"))),
                delimiter: None,
                distinct: false,
            },
            Expr::Agg {
                func: AggFunc::BoolOr,
                arg: Some(Box::new(Expr::col("active"))),
                delimiter: None,
                distinct: false,
            },
        ];

        for e in &nodes {
            validate_expr(e, Dialect::Postgres, &sc, 0, None).unwrap_or_else(|err| {
                panic!("PG-first aggregate must validate on Postgres: {err}")
            });
            for d in [Dialect::Sqlite, Dialect::Mysql] {
                let err = validate_expr(e, d, &sc, 0, None)
                    .expect_err("PG-first aggregate must fail closed off Postgres");
                assert_eq!(err.code, CODE_DIALECT_UNSUPPORTED, "{d:?}: {err}");
                assert_eq!(err.kind, Some(UnsupportedKind::Expr));
            }
        }
    }

    #[test]
    fn portable_scalar_fns_validate_on_all_three_dialects() {
        // mod / round / floor / ceil / substr / replace are PORTABLE ScalarFns:
        // identical spelling on PG/SQLite/MySQL (mod renders as the `%`
        // operator), so the walk accepts them with NO dialect gate — unlike the
        // PG-only currentSetting/currentUser vendor scalars.
        let c = cols();
        let sc = scope("users", &c);
        let nodes = [
            Expr::FnCall {
                r#fn: ScalarFn::Mod,
                args: vec![Expr::col("total"), Expr::lit(IrScalar::Int(3))],
            },
            Expr::FnCall {
                r#fn: ScalarFn::Round,
                args: vec![Expr::col("total")],
            },
            Expr::FnCall {
                r#fn: ScalarFn::Round,
                args: vec![Expr::col("total"), Expr::lit(IrScalar::Int(2))],
            },
            Expr::FnCall {
                r#fn: ScalarFn::Floor,
                args: vec![Expr::col("total")],
            },
            Expr::FnCall {
                r#fn: ScalarFn::Ceil,
                args: vec![Expr::col("total")],
            },
            Expr::FnCall {
                r#fn: ScalarFn::Substr,
                args: vec![
                    Expr::col("name"),
                    Expr::lit(IrScalar::Int(1)),
                    Expr::lit(IrScalar::Int(3)),
                ],
            },
            Expr::FnCall {
                r#fn: ScalarFn::Replace,
                args: vec![
                    Expr::col("name"),
                    Expr::lit(IrScalar::Str("a".into())),
                    Expr::lit(IrScalar::Str("b".into())),
                ],
            },
        ];
        for e in &nodes {
            for d in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
                validate_expr(e, d, &sc, 0, None).unwrap_or_else(|err| {
                    panic!("portable scalar fn must validate on {d:?}: {err}")
                });
            }
        }
    }

    #[test]
    fn pg_only_expr_nodes_reject_on_sqlite_and_mysql() {
        let c = cols();
        let sc = scope("users", &c);
        for e in [
            Expr::PgColumnSize {
                expr: Box::new(Expr::col("name")),
            },
            Expr::PgExtract {
                field: PgExtractField::Epoch,
                from: Box::new(Expr::col("total")),
            },
        ] {
            for d in [Dialect::Sqlite, Dialect::Mysql] {
                let err = validate_expr(&e, d, &sc, 0, None)
                    .expect_err("PG-only expression must reject on non-PG");
                assert_eq!(err.code, CODE_UNSUPPORTED);
                assert_eq!(err.kind, Some(UnsupportedKind::Expr));
                assert_eq!(err.dialect, d);
                assert!(err.reason.contains("PostgreSQL-only"), "got: {err}");
            }
        }
    }

    #[test]
    fn regex_match_rejects_only_on_sqlite() {
        let c = cols();
        let sc = scope("users", &c);
        let expr = Expr::PgRegexMatch {
            expr: Box::new(Expr::col("name")),
            pattern: "^[a-z]+$".to_string(),
        };
        for d in [Dialect::Postgres, Dialect::Mysql] {
            validate_expr(&expr, d, &sc, 0, None)
                .unwrap_or_else(|err| panic!("regex match must validate on {d:?}: {err}"));
        }
        let err = validate_expr(&expr, Dialect::Sqlite, &sc, 0, None)
            .expect_err("regex match must fail closed on SQLite");
        assert_eq!(err.code, CODE_DIALECT_UNSUPPORTED);
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        assert_eq!(err.dialect, Dialect::Sqlite);
        assert!(err.reason.contains("SQLite"), "got: {err}");
    }

    #[test]
    fn text_literal_shapes_are_checked() {
        let c = cols();
        let sc = scope("users", &c);
        let empty_membership = in_list(Expr::col("name"), vec![]);
        for d in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
            validate_expr(&empty_membership, d, &sc, 0, None)
                .unwrap_or_else(|err| panic!("empty inList must validate on {d:?}: {err}"));
        }

        let nul_elem = in_list(Expr::col("name"), vec!["ok", "bad\0value"]);
        let err = validate_expr(&nul_elem, Dialect::Postgres, &sc, 0, None).unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert!(err.reason.contains("NUL"));

        let mixed_elem = Expr::InList {
            expr: Box::new(Expr::col("name")),
            elems: vec![IrScalar::Str("ok".into()), IrScalar::Int(200)],
            negated: false,
        };
        let err = validate_expr(&mixed_elem, Dialect::Postgres, &sc, 0, None).unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert!(err.reason.contains("homogeneous"));

        let bytes_elem = Expr::InList {
            expr: Box::new(Expr::col("name")),
            elems: vec![IrScalar::Bytes(vec![1, 2, 3])],
            negated: false,
        };
        let err = validate_expr(&bytes_elem, Dialect::Postgres, &sc, 0, None).unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert!(err.reason.contains("bytes are not allowed"));

        let empty_pattern = Expr::PgRegexMatch {
            expr: Box::new(Expr::col("name")),
            pattern: String::new(),
        };
        let err = validate_expr(&empty_pattern, Dialect::Postgres, &sc, 0, None).unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert!(err.reason.contains("non-empty"));
    }

    // ── (b) splitPart envelope ─────────────────────────────────────────────

    fn split(delim: &str, n: i64) -> Expr {
        Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![
                Expr::col("name"),
                Expr::lit(IrScalar::Str(delim.into())),
                Expr::lit(IrScalar::Int(n)),
            ],
        }
    }

    #[test]
    fn split_part_in_envelope_validates() {
        let c = cols();
        let sc = scope("users", &c);
        for n in 1..=SPLIT_PART_MAX_N {
            assert!(
                validate_expr(&split(" ", n), Dialect::Sqlite, &sc, 0, None).is_ok(),
                "n={n} single-ASCII delim must be in-envelope"
            );
        }
    }

    #[test]
    fn split_part_multichar_delim_rejected() {
        let c = cols();
        let sc = scope("users", &c);
        let err =
            validate_expr(&split(", ", 1), Dialect::Sqlite, &sc, 2, Some("m.ts:9")).unwrap_err();
        assert_eq!(err.code, CODE_EXPR_NOT_PORTABLE);
        assert_eq!(err.op_index, 2);
        assert_eq!(err.dialect, Dialect::Sqlite);
        assert_eq!(err.ts_location.as_deref(), Some("m.ts:9"));
        assert!(err.suggested_fix.is_some());
        // The structured payload leads with suggested_fix.
        let json = err.to_json();
        let obj = json.as_object().unwrap();
        assert_eq!(obj.keys().next().unwrap(), "suggested_fix");
        assert_eq!(obj["code"], CODE_EXPR_NOT_PORTABLE);
    }

    // ── the splitPart envelope verdict is DIALECT-GATED ─────────────────────
    // An OUT-OF-ENVELOPE-but-PG-renderable c.fn.splitPart (multi-char delim,
    // n>8, …) is renderable on Postgres (`split_part` accepts it) and only a
    // hard reject on the SQLite leg. The SAME node must therefore
    // validate OK on a Postgres target and be EXPR_NOT_PORTABLE on a SQLite
    // target. RED before check_split_part branches on target_dialect.

    #[test]
    fn out_of_envelope_split_part_loads_on_pg_rejected_on_sqlite() {
        let c = cols();
        let sc = scope("users", &c);
        // The loads-on-PG / rejected-on-SQLite fixture: multi-char delim.
        let node = split(", ", 1);
        assert!(
            validate_expr(&node, Dialect::Postgres, &sc, 0, None).is_ok(),
            "an out-of-envelope-but-PG-renderable splitPart must VALIDATE on a Postgres target"
        );
        let err = validate_expr(&node, Dialect::Sqlite, &sc, 0, None).unwrap_err();
        assert_eq!(
            err.code, CODE_EXPR_NOT_PORTABLE,
            "the same node must be EXPR_NOT_PORTABLE on a SQLite target"
        );
        assert_eq!(err.dialect, Dialect::Sqlite);

        // Likewise n>8 and a non-ASCII delim: PG-renderable, SQLite-rejected.
        for node in [split(" ", 9), split("·", 1)] {
            assert!(
                validate_expr(&node, Dialect::Postgres, &sc, 0, None).is_ok(),
                "out-of-envelope splitPart loads on PG"
            );
            assert_eq!(
                validate_expr(&node, Dialect::Sqlite, &sc, 0, None)
                    .unwrap_err()
                    .code,
                CODE_EXPR_NOT_PORTABLE
            );
        }
    }

    // ── the GRAMMAR is dialect-NEUTRAL ──────────────────────────────────────
    // A grammar-broken splitPart — a NON-literal / non-string delim, or a
    // non-literal / non-positive-int n — is not renderable on EITHER dialect (the
    // renderer enforces the same grammar fail-closed on PG and SQLite). The
    // validator (the AI loop's primary structured-feedback signal) must
    // therefore reject it on a Postgres target too, BEFORE the dialect early-return —
    // not defer the only rejection to render time. RED before check_split_part lifts
    // the grammar checks above the `if Postgres { return Ok(()) }`.
    #[test]
    fn grammar_broken_split_part_rejected_on_pg_too() {
        let c = cols();
        let sc = scope("users", &c);

        // (1) delim is a COLUMN REFERENCE (a runtime/computed delimiter) — not a
        //     string literal. Grammar-broken on BOTH dialects.
        let runtime_delim = Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![
                Expr::col("name"),
                Expr::col("first"),
                Expr::lit(IrScalar::Int(1)),
            ],
        };
        for d in [Dialect::Postgres, Dialect::Sqlite] {
            let err = validate_expr(&runtime_delim, d, &sc, 0, None).unwrap_err();
            assert_eq!(
                err.code, CODE_EXPR_NOT_PORTABLE,
                "a non-literal delim must reject on {d:?} (grammar is dialect-neutral)"
            );
        }

        // (2) delim is a NON-STRING literal (an integer). Grammar-broken on both.
        let int_delim = Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![
                Expr::col("name"),
                Expr::lit(IrScalar::Int(7)),
                Expr::lit(IrScalar::Int(1)),
            ],
        };
        for d in [Dialect::Postgres, Dialect::Sqlite] {
            assert_eq!(
                validate_expr(&int_delim, d, &sc, 0, None).unwrap_err().code,
                CODE_EXPR_NOT_PORTABLE,
                "a non-string-literal delim must reject on {d:?}"
            );
        }

        // (3) n is a COLUMN REFERENCE (a runtime n) — not a literal. Both dialects.
        let runtime_n = Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![
                Expr::col("name"),
                Expr::lit(IrScalar::Str(",".into())),
                Expr::col("total"),
            ],
        };
        for d in [Dialect::Postgres, Dialect::Sqlite] {
            assert_eq!(
                validate_expr(&runtime_n, d, &sc, 0, None).unwrap_err().code,
                CODE_EXPR_NOT_PORTABLE,
                "a non-literal n must reject on {d:?}"
            );
        }

        // (4) n is a non-POSITIVE integer literal (n<1) — grammar-broken on both.
        for d in [Dialect::Postgres, Dialect::Sqlite] {
            assert_eq!(
                validate_expr(&split(",", 0), d, &sc, 0, None)
                    .unwrap_err()
                    .code,
                CODE_EXPR_NOT_PORTABLE,
                "n<1 must reject on {d:?}"
            );
        }

        // GUARD: a grammar-VALID but out-of-ENVELOPE node (multi-char string-literal
        // delim, or n>8) is still PG-renderable — the envelope stays SQLite-gated.
        assert!(
            validate_expr(&split(", ", 1), Dialect::Postgres, &sc, 0, None).is_ok(),
            "a multi-char STRING-LITERAL delim is grammar-valid → still loads on PG"
        );
        assert!(
            validate_expr(&split(",", 9), Dialect::Postgres, &sc, 0, None).is_ok(),
            "n>8 is grammar-valid (positive int literal) → still loads on PG"
        );
    }

    #[test]
    fn malformed_split_part_arity_is_unconditional_unsupported() {
        // A genuinely-MALFORMED splitPart — wrong arity (not exactly 3 args) — is
        // broken on BOTH dialects (`split_part` is ternary on PG too), so it is
        // an unconditional CODE_UNSUPPORTED, NOT a dialect-gated portability
        // reject. Rejected on PG AND SQLite.
        let c = cols();
        let sc = scope("users", &c);
        let two_arg = Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![Expr::col("name"), Expr::lit(IrScalar::Str(" ".into()))],
        };
        for d in [Dialect::Postgres, Dialect::Sqlite] {
            let err = validate_expr(&two_arg, d, &sc, 0, None).unwrap_err();
            assert_eq!(
                err.code, CODE_UNSUPPORTED,
                "wrong arity is broken on both dialects ({d:?})"
            );
            assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        }
    }

    #[test]
    fn split_part_non_ascii_delim_rejected() {
        let c = cols();
        let sc = scope("users", &c);
        let err = validate_expr(&split("·", 1), Dialect::Sqlite, &sc, 0, None).unwrap_err();
        assert_eq!(err.code, CODE_EXPR_NOT_PORTABLE);
    }

    #[test]
    fn split_part_n_out_of_range_rejected() {
        let c = cols();
        let sc = scope("users", &c);
        for n in [0_i64, -1, 9, 100] {
            let err = validate_expr(&split(" ", n), Dialect::Sqlite, &sc, 0, None).unwrap_err();
            assert_eq!(err.code, CODE_EXPR_NOT_PORTABLE, "n={n} must reject");
        }
        // n=8 is the boundary that PASSES.
        assert!(validate_expr(&split(" ", 8), Dialect::Sqlite, &sc, 0, None).is_ok());
    }

    // ── (b') the remaining SynthFn arities — structural backstop ───────────
    // now takes ZERO args; concatWs takes >=2 (a delimiter + >=1
    // value). Independent of the (not-yet-existing) render seam, the validator
    // is the structural backstop. RED before the check_synth arity fix.

    fn synth(f: SynthFn, args: Vec<Expr>) -> Expr {
        Expr::FnSynth { r#fn: f, args }
    }

    #[test]
    fn now_with_args_is_rejected() {
        // now(arg) is a genuinely-MALFORMED synth — `now()` is nullary on
        // BOTH dialects — so it is an unconditional CODE_UNSUPPORTED, on PG AND
        // SQLite (not a dialect-gated portability reject).
        let sc = TargetScope::structural_only("t");
        let e = synth(SynthFn::Now, vec![Expr::lit(IrScalar::Int(1))]);
        for d in [Dialect::Postgres, Dialect::Sqlite] {
            let err = validate_expr(&e, d, &sc, 0, None).unwrap_err();
            assert_eq!(
                err.code, CODE_UNSUPPORTED,
                "now(arg) is broken on both dialects ({d:?})"
            );
            assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        }
        // zero-arg form passes on both.
        assert!(validate_expr(
            &synth(SynthFn::Now, vec![]),
            Dialect::Postgres,
            &sc,
            0,
            None
        )
        .is_ok());
        assert!(validate_expr(&synth(SynthFn::Now, vec![]), Dialect::Sqlite, &sc, 0, None).is_ok());
    }

    #[test]
    fn exact_uuid_generator_dialect_support_is_validated() {
        let sc = TargetScope::structural_only("t");
        for dialect in [Dialect::Postgres, Dialect::Mysql, Dialect::Sqlite] {
            assert!(
                validate_expr(&Expr::UuidV4, dialect, &sc, 0, None).is_ok(),
                "UUIDv4 has an exact database lowering on {dialect:?}"
            );
        }

        assert!(validate_expr(&Expr::UuidV7, Dialect::Postgres, &sc, 0, None).is_ok());
        for dialect in [Dialect::Mysql, Dialect::Sqlite] {
            let error = validate_expr(&Expr::UuidV7, dialect, &sc, 0, None).unwrap_err();
            assert_eq!(error.code, CODE_EXPR_NOT_PORTABLE);
            assert!(error.reason.contains("PostgreSQL 18+"), "got: {error}");
        }
    }

    #[test]
    fn concat_ws_arity_is_enforced() {
        // concatWs with <2 args is genuinely malformed (no valid join on
        // EITHER dialect) → unconditional CODE_UNSUPPORTED on PG and SQLite.
        let c = cols();
        let sc = scope("users", &c);
        // 0 args and 1 arg (delimiter only, no values) are out of shape.
        for bad in [vec![], vec![Expr::lit(IrScalar::Str(",".into()))]] {
            for d in [Dialect::Postgres, Dialect::Sqlite] {
                let err = validate_expr(&synth(SynthFn::ConcatWs, bad.clone()), d, &sc, 0, None)
                    .unwrap_err();
                assert_eq!(
                    err.code, CODE_UNSUPPORTED,
                    "concatWs needs delim + >=1 value ({d:?})"
                );
            }
        }
        // delim + 1 value is the minimum valid shape; the value still recurses.
        let ok = synth(
            SynthFn::ConcatWs,
            vec![Expr::lit(IrScalar::Str(",".into())), Expr::col("name")],
        );
        assert!(validate_expr(&ok, Dialect::Sqlite, &sc, 0, None).is_ok());
    }

    #[test]
    fn concat_ws_non_literal_delim_rejected_on_sqlite_loads_on_pg() {
        // the SQLite render's NULL-skip head-trim (`substr(fold,
        // length(delim)+1)`) is only correct for a FIXED literal delimiter. A
        // non-literal delimiter (here a ColRef to an existing column, so rule (c)
        // is satisfied and the ONLY objection is the literal-delim gate) must be a
        // HARD reject on SQLite and load fine on PG (`concat_ws` takes any expr),
        // mirroring the splitPart delim-literal gate.
        let c = cols();
        let sc = scope("users", &c);
        let e = synth(
            SynthFn::ConcatWs,
            vec![Expr::col("name"), Expr::col("first")],
        );
        // PG: a non-literal delimiter is fine.
        assert!(
            validate_expr(&e, Dialect::Postgres, &sc, 0, None).is_ok(),
            "a non-literal concatWs delimiter must LOAD on a Postgres target"
        );
        // SQLite: the structural literal-delim gate rejects it.
        let err = validate_expr(&e, Dialect::Sqlite, &sc, 0, None).unwrap_err();
        assert_eq!(
            err.code, CODE_EXPR_NOT_PORTABLE,
            "a non-literal concatWs delimiter must reject on SQLite (literal-delim gate); got: {err}"
        );
    }

    #[test]
    fn concat_ws_recurses_into_a_bad_nested_value() {
        // The arity gate must not short-circuit recursion: a nested
        // out-of-envelope splitPart inside a well-shaped concatWs still rejects.
        let c = cols();
        let sc = scope("users", &c);
        let e = synth(
            SynthFn::ConcatWs,
            vec![Expr::lit(IrScalar::Str(",".into())), split(", ", 1)],
        );
        let err = validate_expr(&e, Dialect::Sqlite, &sc, 0, None).unwrap_err();
        assert_eq!(err.code, CODE_EXPR_NOT_PORTABLE);
    }

    // ── the Layer-2 dialect() per-dialect value escape ──────────────────────

    fn dialectal(
        default: Option<Expr>,
        pg: Option<Expr>,
        sqlite: Option<Expr>,
        mysql: Option<Expr>,
    ) -> Expr {
        Expr::Dialectal {
            default: default.map(Box::new),
            pg: pg.map(Box::new),
            sqlite: sqlite.map(Box::new),
            mysql: mysql.map(Box::new),
        }
    }

    #[test]
    fn dialectal_missing_leg_no_default_accepted_on_own_target_refused_off_target() {
        // dialect({ pg: A }) — no default. Its covered set is exactly {pg}: it is
        // ACCEPTED targeting PG (its own leg), REFUSED targeting SQLite/MySQL
        // (neither own leg nor default) — the per-TARGET scope math.
        let sc = TargetScope::structural_only("t");
        let e = dialectal(None, Some(Expr::lit(IrScalar::Str("A".into()))), None, None);

        assert!(
            validate_expr(&e, Dialect::Postgres, &sc, 0, None).is_ok(),
            "a pg-only dialect() covers the PG target"
        );
        for d in [Dialect::Sqlite, Dialect::Mysql] {
            let err = validate_expr(&e, d, &sc, 0, None).unwrap_err();
            assert_eq!(
                err.code, CODE_EXPR_NOT_PORTABLE,
                "a pg-only dialect() must refuse the {d:?} target (no own leg, no default); got: {err}"
            );
            assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        }
    }

    #[test]
    fn dialectal_default_covers_every_off_target() {
        // dialect({ default: D, pg: A }) covers ALL dialects: PG via its own leg,
        // SQLite/MySQL via the default. Accepted on every target.
        let sc = TargetScope::structural_only("t");
        let e = dialectal(
            Some(Expr::lit(IrScalar::Int(0))),
            Some(Expr::lit(IrScalar::Str("A".into()))),
            None,
            None,
        );
        for d in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
            assert!(
                validate_expr(&e, d, &sc, 0, None).is_ok(),
                "a default leg covers the {d:?} target"
            );
        }
    }

    #[test]
    fn dialectal_pg_vendor_node_in_pg_leg_validates_on_all_covered_targets() {
        // Regression: the PG-only gate must validate each dialect() leg as the
        // dialect that owns that leg. A PG-vendor node in the pg leg is fine even
        // while validating a SQLite/MySQL target, because those targets render
        // their own portable legs and never render the pg leg.
        let c = cols();
        let sc = scope("users", &c);
        let e = dialectal(
            None,
            Some(Expr::PgColumnSize {
                expr: Box::new(Expr::col("name")),
            }),
            Some(Expr::col("name")),
            Some(Expr::col("name")),
        );
        for d in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
            validate_expr(&e, d, &sc, 0, None).unwrap_or_else(|err| {
                panic!("pgColumnSize in the pg leg must validate on covered {d:?}: {err}")
            });
        }
    }

    #[test]
    fn dialectal_pg_vendor_node_in_pg_leg_does_not_cover_missing_mysql_leg() {
        // The per-leg PG-only fix must not weaken the existing coverage rule:
        // pg+sqlite with no default still cannot target MySQL.
        let c = cols();
        let sc = scope("users", &c);
        let e = dialectal(
            None,
            Some(Expr::PgColumnSize {
                expr: Box::new(Expr::col("name")),
            }),
            Some(Expr::col("name")),
            None,
        );
        assert!(validate_expr(&e, Dialect::Postgres, &sc, 0, None).is_ok());
        assert!(validate_expr(&e, Dialect::Sqlite, &sc, 0, None).is_ok());
        let err = validate_expr(&e, Dialect::Mysql, &sc, 0, None).unwrap_err();
        assert_eq!(
            err.code, CODE_EXPR_NOT_PORTABLE,
            "a dialect() with no mysql/default leg must still refuse MySQL; got: {err}"
        );
    }

    #[test]
    fn dialectal_default_leg_must_remain_portable() {
        // `default` is not a vendor bucket. It may be selected for any target, so
        // a PG-only node in default is refused even when the current target is PG.
        let c = cols();
        let sc = scope("users", &c);
        let e = dialectal(
            Some(Expr::PgColumnSize {
                expr: Box::new(Expr::col("name")),
            }),
            None,
            Some(Expr::col("name")),
            Some(Expr::col("name")),
        );
        for d in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
            let err = validate_expr(&e, d, &sc, 0, None).unwrap_err();
            assert_eq!(
                err.code, CODE_UNSUPPORTED,
                "a PG-only node in default must be refused on {d:?}; got: {err}"
            );
            assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        }
    }

    #[test]
    fn dialectal_with_no_legs_is_refused_on_every_target() {
        // dialect({}) — zero legs — is malformed on EVERY target (dialect-neutral
        // CODE_UNSUPPORTED), enforced at validate (serde deserializes the empty
        // node, the structural gate refuses it).
        let sc = TargetScope::structural_only("t");
        let e = dialectal(None, None, None, None);
        for d in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
            let err = validate_expr(&e, d, &sc, 0, None).unwrap_err();
            assert_eq!(
                err.code, CODE_UNSUPPORTED,
                "legless dialect() refused on {d:?}; got: {err}"
            );
            assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        }
    }

    #[test]
    fn dialectal_recurses_into_every_present_leg() {
        // The scope check must not short-circuit recursion: a malformed nested
        // node in ANY leg rejects, dialect-neutrally, even on a target the leg
        // does not select. Here an unresolved ColRef sits in the (unselected)
        // mysql leg while targeting PG.
        let c = cols();
        let sc = scope("users", &c);
        let e = dialectal(
            Some(Expr::lit(IrScalar::Int(0))),
            Some(Expr::col("name")),
            None,
            Some(Expr::col("ghost")), // not a column on `users`
        );
        let err = validate_expr(&e, Dialect::Postgres, &sc, 0, None).unwrap_err();
        assert_eq!(
            err.code, CODE_UNSUPPORTED,
            "an unresolved ColRef in ANY leg must reject (rule c), even off-target; got: {err}"
        );
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
    }

    #[test]
    fn split_part_non_literal_args_rejected() {
        let c = cols();
        let sc = scope("users", &c);
        // Non-literal delimiter (a column ref) is not portable.
        let e = Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![
                Expr::col("name"),
                Expr::col("first"),
                Expr::lit(IrScalar::Int(1)),
            ],
        };
        let err = validate_expr(&e, Dialect::Sqlite, &sc, 0, None).unwrap_err();
        assert_eq!(err.code, CODE_EXPR_NOT_PORTABLE);
    }

    // ── item-4 regression: rule (c) ColRef resolution must cover EVERY splitPart
    // arg, on PG too. check_split_part returns Ok early on a Postgres target
    // (the envelope is PG-renderable); but the structural ColRef-resolution walk
    // (rule c) must STILL run over args[1]/args[2]. Before the fix, check_synth
    // recursed only args.first() (the column), so a ColRef to a nonexistent
    // column hidden in the delim/n slot slipped past on PG and deferred the
    // failure to render/execute. RED before walking every arg unconditionally.

    #[test]
    fn split_part_colref_in_delim_slot_rejected_on_pg() {
        let c = cols();
        let sc = scope("users", &c);
        // delim slot is a ColRef to a column NOT on `users` — rule (c) must fire,
        // even on a Postgres target (the structural resolution is dialect-neutral).
        let e = Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![
                Expr::col("name"),
                Expr::col("nonexistent"),
                Expr::lit(IrScalar::Int(1)),
            ],
        };
        let err = validate_expr(&e, Dialect::Postgres, &sc, 0, None).unwrap_err();
        assert_eq!(
            err.code, CODE_UNSUPPORTED,
            "an unresolved ColRef in the delim slot must reject on PG (rule c), got: {err}"
        );
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
    }

    #[test]
    fn split_part_colref_in_n_slot_rejected_on_pg() {
        let c = cols();
        let sc = scope("users", &c);
        // n slot is a ColRef to a nonexistent column — rule (c), on PG.
        let e = Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![
                Expr::col("name"),
                Expr::lit(IrScalar::Str(" ".into())),
                Expr::col("ghost"),
            ],
        };
        let err = validate_expr(&e, Dialect::Postgres, &sc, 0, None).unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED, "got: {err}");
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
    }

    #[test]
    fn split_part_nested_bad_synth_in_delim_slot_rejected_on_pg() {
        // A NESTED out-of-... no: on PG the inner splitPart envelope is fine, but
        // a nested splitPart with WRONG ARITY (malformed on both dialects) hidden
        // in the delim slot must still be reached by the walk on PG.
        let c = cols();
        let sc = scope("users", &c);
        let bad_inner = Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![Expr::col("name")], // arity 1 → malformed on both dialects
        };
        let e = Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![Expr::col("name"), bad_inner, Expr::lit(IrScalar::Int(1))],
        };
        let err = validate_expr(&e, Dialect::Postgres, &sc, 0, None).unwrap_err();
        assert_eq!(
            err.code, CODE_UNSUPPORTED,
            "a malformed nested splitPart in the delim slot must be reached on PG, got: {err}"
        );
    }

    #[test]
    fn validate_ir_rejects_split_part_colref_in_n_slot_on_pg() {
        // The production-path proof: drive a hostile IR through validate_ir on a
        // Postgres target. A createTable Check whose splitPart hides a ColRef to a
        // nonexistent column in the n slot must reject (rule c), not pass on PG.
        let ir = ir_with(vec![Op::CreateTable {
            name: "users".into(),
            columns: vec![IrColumn {
                name: "first".into(),
                ty: ColType::Text,
                nullable: None,
                default: None,
                unique: None,
                value_format: None,
                references: None,
                id_prefix: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
            primary_key: None,
            constraints: vec![IrConstraint {
                name: None,
                kind: IrConstraintKind::Check {
                    expr: Expr::UnaryOp {
                        op: UnaryOp::IsNotNull,
                        operand: Box::new(Expr::FnSynth {
                            r#fn: SynthFn::SplitPart,
                            args: vec![
                                Expr::col("first"),
                                Expr::lit(IrScalar::Str(" ".into())),
                                Expr::col("ghost"), // not a column of users
                            ],
                        }),
                    },

                    not_valid: None,
                },
            }],
            indexes: vec![],

            partition_by: None,

            runtime_options: None,
            schema: None,
            existence_guard: None,
        }]);
        let err = validate_ir_platform(&ir, Dialect::Postgres).unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED, "got: {err}");
        assert_eq!(err.op_index, 0);
    }

    // ── (c) ColRef resolution against the target table ─────────────────────

    #[test]
    fn colref_on_target_table_validates() {
        let c = cols();
        let sc = scope("users", &c);
        assert!(validate_expr(&Expr::col("name"), Dialect::Postgres, &sc, 0, None).is_ok());
    }

    #[test]
    fn colref_not_on_target_table_rejected() {
        let c = cols();
        let sc = scope("users", &c);
        let err = validate_expr(
            &Expr::col("nope"),
            Dialect::Postgres,
            &sc,
            3,
            Some("m.ts:4"),
        )
        .unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        assert_eq!(err.op_index, 3);
        assert!(err.reason.contains("cross-table") || err.reason.contains("does not resolve"));
    }

    #[test]
    fn synthesized_cross_table_reference_is_rejected() {
        // A node a buggy/malicious builder might synthesize: a ColRef carrying a
        // qualified "other.col" name. `c` is single-table-scoped, so "other.col"
        // is not a column on `users` → rejected (cross-table is not expressible).
        let c = cols();
        let sc = scope("users", &c);
        let err = validate_expr(
            &Expr::col("customers.name"),
            Dialect::Postgres,
            &sc,
            0,
            None,
        )
        .unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
    }

    #[test]
    fn structural_only_scope_skips_colref_resolution() {
        let sc = TargetScope::structural_only("users");
        // A col not in any set still validates structurally (resolution deferred).
        assert!(validate_expr(&Expr::col("anything"), Dialect::Sqlite, &sc, 0, None).is_ok());
        // …but an out-of-envelope splitPart STILL rejects (structural).
        let err = validate_expr(&split(", ", 1), Dialect::Sqlite, &sc, 0, None).unwrap_err();
        assert_eq!(err.code, CODE_EXPR_NOT_PORTABLE);
    }

    // ── validate_ir / validate_op — the SOLE-gate walker over a whole IR ────
    //
    // These pin the obligation that the validator is actually INVOKED
    // over every embedded Expr slot of every Op.

    use crate::model::ir::{
        BackfillSetValue, ColType, ColumnReference, IrColumn, IrConstraint, IrConstraintKind,
        IrIndex, MigrationIr, Op, PartitionBoundValue, PartitionBounds, PartitionSpec,
        PerRowGenerator, SafeI64,
    };
    use std::collections::BTreeMap;

    fn ir_with(ops: Vec<Op>) -> MigrationIr {
        MigrationIr {
            ir_version: 1,
            name: "n".into(),
            owner_app: String::new(),
            ops,
            flags: Default::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        }
    }

    fn op_json(json: &str) -> Op {
        serde_json::from_str(json).expect("test op JSON")
    }

    fn validate_ir_platform(ir: &MigrationIr, dialect: Dialect) -> Result<(), AuthoringError> {
        validate_ir_scoped(ir, dialect, &[], None)
    }

    fn part_col(name: &str, ty: ColType, not_null: bool) -> IrColumn {
        IrColumn {
            name: name.into(),
            ty,
            nullable: not_null.then_some(false),
            default: None,
            unique: None,
            value_format: None,
            references: None,
            id_prefix: None,
            vector_metric: None,
            case_sensitive: None,
            mask: None,
            generated: None,
            identity: None,
        }
    }

    fn create_with_column(name: &str, ty: ColType) -> Op {
        Op::CreateTable {
            name: "typed_columns".into(),
            columns: vec![part_col(name, ty, true)],
            primary_key: None,
            constraints: vec![],
            indexes: vec![],
            partition_by: None,
            runtime_options: None,
            schema: None,
            existence_guard: None,
        }
    }

    fn idx_col(name: &str) -> IndexElement {
        IndexElement::Column {
            name: name.into(),
            order: None,
            opclass: None,
            collation: None,
        }
    }

    fn unique_idx(columns: &[&str]) -> IrIndex {
        IrIndex {
            name: None,
            columns: columns.iter().map(|name| idx_col(name)).collect(),
            unique: Some(true),
            using: None,
            r#where: None,
            include: Vec::new(),
            with: None,
            only: None,
            nulls_not_distinct: None,
        }
    }

    fn safe_i(value: i64) -> PartitionBoundValue {
        PartitionBoundValue::Int {
            value: SafeI64::new(value).expect("test partition bound is JS-safe"),
        }
    }

    fn str_b(value: &str) -> PartitionBoundValue {
        PartitionBoundValue::String {
            value: value.into(),
        }
    }

    fn create_parent(
        name: &str,
        spec: PartitionSpec,
        columns: Vec<IrColumn>,
        primary_key: Option<&[&str]>,
        constraints: Vec<IrConstraint>,
        indexes: Vec<IrIndex>,
    ) -> Op {
        Op::CreateTable {
            name: name.into(),
            columns,
            primary_key: primary_key.map(|cols| cols.iter().map(|col| (*col).into()).collect()),
            constraints,
            indexes,
            partition_by: Some(spec),
            runtime_options: None,
            schema: None,
            existence_guard: None,
        }
    }

    fn create_part(name: &str, of: &str, bounds: PartitionBounds) -> Op {
        Op::CreatePartition {
            name: name.into(),
            of: of.into(),
            bounds,
            schema: None,
            existence_guard: None,
        }
    }

    fn drop_part(parent: &str, name: &str) -> Op {
        Op::DropPartition {
            parent: parent.into(),
            name: name.into(),
            schema: None,
            existence_guard: None,
            cascade: None,
        }
    }

    #[test]
    fn partitioned_table_without_collapse_is_dialect_unsupported_off_postgres() {
        let ir = ir_with(vec![create_parent(
            "events",
            PartitionSpec::Range {
                columns: vec!["ts".into()],
                collapse: false,
            },
            vec![part_col("ts", ColType::Timestamp, true)],
            None,
            vec![],
            vec![],
        )]);

        assert!(validate_ir_platform(&ir, Dialect::Postgres).is_ok());
        let err = validate_ir_platform(&ir, Dialect::Sqlite)
            .expect_err("non-affirmed partitioning must fail closed off Postgres");
        assert_eq!(err.code, CODE_DIALECT_UNSUPPORTED, "got: {err}");
    }

    #[test]
    fn partition_key_coverage_refuses_non_covering_unique_and_accepts_covering() {
        let base_cols = || {
            vec![
                part_col("tenant_id", ColType::Uuid, true),
                part_col("ts", ColType::Timestamp, true),
            ]
        };
        let spec = || PartitionSpec::Range {
            columns: vec!["ts".into()],
            collapse: false,
        };

        let bad = ir_with(vec![create_parent(
            "events",
            spec(),
            base_cols(),
            None,
            vec![],
            vec![unique_idx(&["tenant_id"])],
        )]);
        let err = validate_ir_platform(&bad, Dialect::Postgres)
            .expect_err("unique indexes on partitioned parents must cover the key");
        assert_eq!(err.code, CODE_PARTITION_KEY_COVERAGE, "got: {err}");

        let ok = ir_with(vec![create_parent(
            "events",
            spec(),
            base_cols(),
            None,
            vec![],
            vec![unique_idx(&["tenant_id", "ts"])],
        )]);
        assert!(validate_ir_platform(&ok, Dialect::Postgres).is_ok());
    }

    #[test]
    fn collapse_requires_total_range_list_and_hash_bounds() {
        let range_missing_default = ir_with(vec![
            create_parent(
                "events",
                PartitionSpec::Range {
                    columns: vec!["ts".into()],
                    collapse: true,
                },
                vec![part_col("ts", ColType::Timestamp, true)],
                None,
                vec![],
                vec![],
            ),
            create_part(
                "events_0",
                "events",
                PartitionBounds::Range {
                    from: vec![safe_i(0)],
                    to: vec![safe_i(10)],
                },
            ),
        ]);
        let err = validate_ir_platform(&range_missing_default, Dialect::Postgres)
            .expect_err("collapse range without default must refuse");
        assert_eq!(err.code, CODE_PARTITION_BOUNDS_NOT_TOTAL, "got: {err}");

        let list_missing_default = ir_with(vec![
            create_parent(
                "orders",
                PartitionSpec::List {
                    columns: vec!["region".into()],
                    collapse: true,
                },
                vec![part_col("region", ColType::Text, true)],
                None,
                vec![],
                vec![],
            ),
            create_part(
                "orders_us",
                "orders",
                PartitionBounds::List {
                    values: vec![str_b("US")],
                },
            ),
        ]);
        let err = validate_ir_platform(&list_missing_default, Dialect::Postgres)
            .expect_err("collapse list without default must refuse");
        assert_eq!(err.code, CODE_PARTITION_BOUNDS_NOT_TOTAL, "got: {err}");

        let hash_partial = ir_with(vec![
            create_parent(
                "sessions",
                PartitionSpec::Hash {
                    columns: vec!["tenant_id".into()],
                    collapse: true,
                },
                vec![part_col("tenant_id", ColType::Uuid, true)],
                None,
                vec![],
                vec![],
            ),
            create_part(
                "sessions_0",
                "sessions",
                PartitionBounds::Hash {
                    modulus: 2,
                    remainder: 0,
                },
            ),
        ]);
        let err = validate_ir_platform(&hash_partial, Dialect::Postgres)
            .expect_err("collapse hash must cover every residue");
        assert_eq!(err.code, CODE_PARTITION_BOUNDS_NOT_TOTAL, "got: {err}");

        let hash_total = ir_with(vec![
            create_parent(
                "sessions",
                PartitionSpec::Hash {
                    columns: vec!["tenant_id".into()],
                    collapse: true,
                },
                vec![part_col("tenant_id", ColType::Uuid, true)],
                None,
                vec![],
                vec![],
            ),
            create_part(
                "sessions_0",
                "sessions",
                PartitionBounds::Hash {
                    modulus: 2,
                    remainder: 0,
                },
            ),
            create_part(
                "sessions_1",
                "sessions",
                PartitionBounds::Hash {
                    modulus: 2,
                    remainder: 1,
                },
            ),
        ]);
        assert!(validate_ir_platform(&hash_total, Dialect::Postgres).is_ok());
    }

    #[test]
    fn collapse_hash_child_drop_is_underivable_but_pg_only_hash_drop_is_valid() {
        let parent = |collapse| {
            create_parent(
                "sessions",
                PartitionSpec::Hash {
                    columns: vec!["tenant_id".into()],
                    collapse,
                },
                vec![part_col("tenant_id", ColType::Uuid, true)],
                None,
                vec![],
                vec![],
            )
        };
        let child_0 = || {
            create_part(
                "sessions_0",
                "sessions",
                PartitionBounds::Hash {
                    modulus: 2,
                    remainder: 0,
                },
            )
        };
        let child_1 = || {
            create_part(
                "sessions_1",
                "sessions",
                PartitionBounds::Hash {
                    modulus: 2,
                    remainder: 1,
                },
            )
        };

        let collapse_drop = ir_with(vec![
            parent(true),
            child_0(),
            child_1(),
            drop_part("sessions", "sessions_0"),
        ]);
        for dialect in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
            let err = validate_ir_platform(&collapse_drop, dialect)
                .expect_err("collapse hash child drop must be recording-level underivable");
            assert_eq!(err.code, CODE_PARTITION_HASH_DROP_UNDERIVABLE, "got: {err}");
        }

        let pg_only_drop = ir_with(vec![
            parent(false),
            child_0(),
            child_1(),
            drop_part("sessions", "sessions_0"),
        ]);
        assert!(validate_ir_platform(&pg_only_drop, Dialect::Postgres).is_ok());
    }

    #[test]
    fn collapse_refuses_composite_range_key() {
        let ir = ir_with(vec![
            create_parent(
                "events",
                PartitionSpec::Range {
                    columns: vec!["tenant_id".into(), "ts".into()],
                    collapse: true,
                },
                vec![
                    part_col("tenant_id", ColType::Uuid, true),
                    part_col("ts", ColType::Timestamp, true),
                ],
                None,
                vec![],
                vec![],
            ),
            create_part("events_default", "events", PartitionBounds::Default),
        ]);

        let err = validate_ir_platform(&ir, Dialect::Postgres)
            .expect_err("range collapse v1 supports one key column");
        assert_eq!(
            err.code, CODE_PARTITION_COMPOSITE_KEY_UNSUPPORTED,
            "got: {err}"
        );
    }

    #[test]
    fn collapse_refuses_nullable_key_and_later_drop_not_null() {
        let nullable = ir_with(vec![
            create_parent(
                "events",
                PartitionSpec::Range {
                    columns: vec!["ts".into()],
                    collapse: true,
                },
                vec![part_col("ts", ColType::Timestamp, false)],
                None,
                vec![],
                vec![],
            ),
            create_part("events_default", "events", PartitionBounds::Default),
        ]);
        let err = validate_ir_platform(&nullable, Dialect::Postgres)
            .expect_err("collapse partition keys must be not null");
        assert_eq!(
            err.code, CODE_PARTITION_KEY_NULLABLE_UNDER_COLLAPSE,
            "got: {err}"
        );

        let dropped_later = ir_with(vec![
            create_parent(
                "events",
                PartitionSpec::Range {
                    columns: vec!["ts".into()],
                    collapse: true,
                },
                vec![part_col("ts", ColType::Timestamp, true)],
                None,
                vec![],
                vec![],
            ),
            create_part("events_default", "events", PartitionBounds::Default),
            Op::DropColumnNotNull {
                table: "events".into(),
                column: "ts".into(),
                schema: None,
                existence_guard: None,
            },
        ]);
        let err = validate_ir_platform(&dropped_later, Dialect::Postgres)
            .expect_err("later dropNotNull on a collapse key must refuse");
        assert_eq!(
            err.code, CODE_PARTITION_KEY_NULLABLE_UNDER_COLLAPSE,
            "got: {err}"
        );
    }

    #[test]
    fn partition_bounds_refuse_overlapping_range_and_accept_disjoint() {
        let parent = || {
            create_parent(
                "events",
                PartitionSpec::Range {
                    columns: vec!["bucket".into()],
                    collapse: false,
                },
                vec![part_col("bucket", ColType::Int, true)],
                None,
                vec![],
                vec![],
            )
        };
        let range = |name: &str, from: i64, to: i64| {
            create_part(
                name,
                "events",
                PartitionBounds::Range {
                    from: vec![safe_i(from)],
                    to: vec![safe_i(to)],
                },
            )
        };

        let bad = ir_with(vec![
            parent(),
            range("events_a", 0, 10),
            range("events_b", 5, 20),
        ]);
        let err = validate_ir_platform(&bad, Dialect::Postgres)
            .expect_err("overlapping range siblings must refuse");
        assert_eq!(err.code, CODE_PARTITION_BOUNDS_ILL_FORMED, "got: {err}");

        let ok = ir_with(vec![
            parent(),
            range("events_a", 0, 10),
            range("events_b", 10, 20),
        ]);
        assert!(validate_ir_platform(&ok, Dialect::Postgres).is_ok());
    }

    #[test]
    fn partition_bounds_refuse_duplicate_list_value_and_accept_unique() {
        let parent = || {
            create_parent(
                "orders",
                PartitionSpec::List {
                    columns: vec!["region".into()],
                    collapse: false,
                },
                vec![part_col("region", ColType::Text, true)],
                None,
                vec![],
                vec![],
            )
        };

        let bad = ir_with(vec![
            parent(),
            create_part(
                "orders_a",
                "orders",
                PartitionBounds::List {
                    values: vec![str_b("US"), str_b("US")],
                },
            ),
        ]);
        let err = validate_ir_platform(&bad, Dialect::Postgres)
            .expect_err("duplicate list values must refuse");
        assert_eq!(err.code, CODE_PARTITION_BOUNDS_ILL_FORMED, "got: {err}");

        let ok = ir_with(vec![
            parent(),
            create_part(
                "orders_us",
                "orders",
                PartitionBounds::List {
                    values: vec![str_b("US")],
                },
            ),
            create_part(
                "orders_eu",
                "orders",
                PartitionBounds::List {
                    values: vec![str_b("EU")],
                },
            ),
        ]);
        assert!(validate_ir_platform(&ok, Dialect::Postgres).is_ok());
    }

    #[test]
    fn partition_bounds_refuse_non_factor_chain_hash_and_accept_factor_chain() {
        let parent = || {
            create_parent(
                "sessions",
                PartitionSpec::Hash {
                    columns: vec!["tenant_id".into()],
                    collapse: false,
                },
                vec![part_col("tenant_id", ColType::Uuid, true)],
                None,
                vec![],
                vec![],
            )
        };

        let bad = ir_with(vec![
            parent(),
            create_part(
                "sessions_2_0",
                "sessions",
                PartitionBounds::Hash {
                    modulus: 2,
                    remainder: 0,
                },
            ),
            create_part(
                "sessions_3_1",
                "sessions",
                PartitionBounds::Hash {
                    modulus: 3,
                    remainder: 1,
                },
            ),
        ]);
        let err = validate_ir_platform(&bad, Dialect::Postgres)
            .expect_err("hash moduli must be comparable by divisibility");
        assert_eq!(err.code, CODE_PARTITION_BOUNDS_ILL_FORMED, "got: {err}");

        let ok = ir_with(vec![
            parent(),
            create_part(
                "sessions_2_0",
                "sessions",
                PartitionBounds::Hash {
                    modulus: 2,
                    remainder: 0,
                },
            ),
            create_part(
                "sessions_4_1",
                "sessions",
                PartitionBounds::Hash {
                    modulus: 4,
                    remainder: 1,
                },
            ),
        ]);
        assert!(validate_ir_platform(&ok, Dialect::Postgres).is_ok());
    }

    // ── schema confinement + guard direction + schema-ident safety ──────────────

    /// CONFINED — an explicit `schema != project_schema` is REFUSED fail-closed at
    /// validate-time with the structured `CROSS_SCHEMA` code. RED before the
    /// gate (the op would have lowered cross-schema). An op whose schema EQUALS the
    /// project schema, or omits it, passes.
    #[test]
    fn confined_cross_schema_op_is_refused_at_validate() {
        use crate::model::policy::SchemaScope;
        let cross = ir_with(vec![Op::DropTable {
            table: "t".into(),
            cascade: None,
            schema: Some("other_app".into()),
            existence_guard: None,
        }]);
        let scope = SchemaScope::Single("app_a".into());
        let err = validate_ir_scoped(&cross, Dialect::Postgres, &[], Some(&scope)).unwrap_err();
        assert_eq!(err.code, CODE_CROSS_SCHEMA, "got: {err}");

        // schema == project schema (case-insensitive) passes.
        let same = ir_with(vec![Op::DropTable {
            table: "t".into(),
            cascade: None,
            schema: Some("APP_A".into()),
            existence_guard: None,
        }]);
        assert!(validate_ir_scoped(&same, Dialect::Postgres, &[], Some(&scope)).is_ok());

        // Absent schema passes.
        let none = ir_with(vec![Op::DropTable {
            table: "t".into(),
            cascade: None,
            schema: None,
            existence_guard: None,
        }]);
        assert!(validate_ir_scoped(&none, Dialect::Postgres, &[], Some(&scope)).is_ok());
    }

    /// Defaulted public validation (`None` scope) has no project schema available,
    /// so it honors any schema for non-vendor ops; PLATFORM (`Allowlist`) refuses a
    /// schema outside its allow-list.
    #[test]
    fn trusted_honors_any_schema_platform_gates_to_allowlist() {
        use crate::model::policy::SchemaScope;
        let foreign = ir_with(vec![Op::DropTable {
            table: "t".into(),
            cascade: None,
            schema: Some("anything".into()),
            existence_guard: None,
        }]);
        // Defaulted public validation: permitted for non-vendor schema qualifiers.
        assert!(validate_ir_scoped(&foreign, Dialect::Postgres, &[], None).is_ok());
        // Platform allow-list excluding "anything": refused.
        let scope = SchemaScope::Allowlist(vec!["zero_migrate".into(), "public".into()]);
        let err = validate_ir_scoped(&foreign, Dialect::Postgres, &[], Some(&scope)).unwrap_err();
        assert_eq!(err.code, CODE_CROSS_SCHEMA);
        // A schema IN the allow-list passes.
        let ok = ir_with(vec![Op::DropTable {
            table: "t".into(),
            cascade: None,
            schema: Some("zero_migrate".into()),
            existence_guard: None,
        }]);
        assert!(validate_ir_scoped(&ok, Dialect::Postgres, &[], Some(&scope)).is_ok());
    }

    /// A `schema` qualifier that is not a safe bare identifier (injection-shaped) is
    /// REFUSED with `INVALID_SCHEMA_IDENT` — REGARDLESS of profile. RED before
    /// `is_safe_schema_ident` guards the author-controlled identifier position.
    #[test]
    fn injection_shaped_schema_ident_is_refused() {
        for bad in ["a\"; DROP TABLE x;--", "1bad", "has space", "", "a-b"] {
            let ir = ir_with(vec![Op::DropTable {
                table: "t".into(),
                cascade: None,
                schema: Some(bad.into()),
                existence_guard: None,
            }]);
            // Even defaulted public validation (None scope) rejects an injection-shaped ident.
            let err = validate_ir_scoped(&ir, Dialect::Postgres, &[], None).unwrap_err();
            assert_eq!(
                err.code, CODE_INVALID_SCHEMA_IDENT,
                "schema {bad:?} got: {err}"
            );
        }
    }

    /// A guard whose DIRECTION is illegal for the op variant is an authoring error
    /// (`GUARD_DIRECTION`): `ifExists` on a create*/add* op, `ifNotExists` on a
    /// drop*/rename op. RED before the legal-direction check.
    #[test]
    fn wrong_direction_existence_guard_is_an_authoring_error() {
        // ifExists on createTable — illegal.
        let bad_create = ir_with(vec![Op::CreateTable {
            name: "t".into(),
            columns: vec![],
            primary_key: None,
            constraints: vec![],
            indexes: vec![],

            partition_by: None,

            runtime_options: None,
            schema: None,
            existence_guard: Some(crate::model::ir::ExistenceGuard::IfExists),
        }]);
        let err = validate_ir_scoped(&bad_create, Dialect::Postgres, &[], None).unwrap_err();
        assert_eq!(err.code, CODE_GUARD_DIRECTION, "got: {err}");

        // ifNotExists on dropTable — illegal.
        let bad_drop = ir_with(vec![Op::DropTable {
            table: "t".into(),
            cascade: None,
            schema: None,
            existence_guard: Some(crate::model::ir::ExistenceGuard::IfNotExists),
        }]);
        let err2 = validate_ir_scoped(&bad_drop, Dialect::Postgres, &[], None).unwrap_err();
        assert_eq!(err2.code, CODE_GUARD_DIRECTION);

        // The LEGAL directions pass.
        let ok_create = ir_with(vec![Op::CreateTable {
            name: "t".into(),
            columns: vec![],
            primary_key: None,
            constraints: vec![],
            indexes: vec![],

            partition_by: None,

            runtime_options: None,
            schema: None,
            existence_guard: Some(crate::model::ir::ExistenceGuard::IfNotExists),
        }]);
        assert!(validate_ir_scoped(&ok_create, Dialect::Postgres, &[], None).is_ok());
    }

    #[test]
    fn alter_primary_key_structural_contract_is_portable_and_order_exact() {
        use crate::model::ir::AlterPrimaryKeyAction;

        for dialect in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
            for action in [
                AlterPrimaryKeyAction::Add {
                    columns: vec!["tenant_id".into(), "order_id".into()],
                },
                AlterPrimaryKeyAction::Replace {
                    expected_columns: vec!["id".into()],
                    columns: vec!["tenant_id".into(), "order_id".into()],
                    drop_identity_from: Some(vec!["id".into()]),
                },
                AlterPrimaryKeyAction::Drop {
                    expected_columns: vec!["tenant_id".into(), "order_id".into()],
                    drop_identity_from: None,
                },
            ] {
                validate_op(
                    &Op::AlterPrimaryKey {
                        table: "orders".into(),
                        action,
                        schema: None,
                    },
                    dialect,
                    4,
                    Some("migration.ts:10:3"),
                )
                .unwrap_or_else(|error| {
                    panic!("{dialect:?} rejected a portable lifecycle action: {error}")
                });
            }
        }
    }

    #[test]
    fn alter_primary_key_rejects_malformed_order_and_identity_transition_tuples() {
        use crate::model::ir::AlterPrimaryKeyAction;

        for action in [
            AlterPrimaryKeyAction::Add {
                columns: vec!["id".into(), "id".into()],
            },
            AlterPrimaryKeyAction::Replace {
                expected_columns: vec!["id".into()],
                columns: vec!["id".into()],
                drop_identity_from: None,
            },
            AlterPrimaryKeyAction::Drop {
                expected_columns: vec!["id".into()],
                drop_identity_from: Some(vec!["other".into()]),
            },
        ] {
            let error = validate_op(
                &Op::AlterPrimaryKey {
                    table: "orders".into(),
                    action,
                    schema: None,
                },
                Dialect::Postgres,
                7,
                Some("migration.ts:12:5"),
            )
            .expect_err("malformed lifecycle tuple must fail closed");
            assert_eq!(error.code, CODE_PRIMARY_KEY_INVALID);
            assert_eq!(error.op_index, 7);
            assert_eq!(error.ts_location.as_deref(), Some("migration.ts:12:5"));
        }
    }

    #[test]
    fn logical_candidate_replay_removes_dropped_primary_key_but_preserves_exact_unique() {
        let target = op_json(
            r#"{
              "op":"createTable", "name":"parents",
              "columns":[{"name":"id","type":"bigInt","nullable":false}],
              "primaryKey":["id"], "constraints":[], "indexes":[]
            }"#,
        );
        let drop_pk = Op::AlterPrimaryKey {
            table: "parents".into(),
            action: crate::model::ir::AlterPrimaryKeyAction::Drop {
                expected_columns: vec!["id".into()],
                drop_identity_from: None,
            },
            schema: None,
        };
        let child = op_json(
            r#"{
              "op":"createTable", "name":"children",
              "columns":[{"name":"parent_id","type":"bigInt","nullable":false}],
              "primaryKey":null,
              "constraints":[{
                "kind":{
                  "kind":"fk", "columns":["parent_id"],
                  "referencesTable":"parents", "referencesColumns":["id"]
                }
              }],
              "indexes":[]
            }"#,
        );

        let error = validate_ir_platform(
            &ir_with(vec![target.clone(), drop_pk.clone(), child.clone()]),
            Dialect::Postgres,
        )
        .expect_err("drop removes the primary key as a logical FK candidate");
        assert!(error
            .reason
            .contains("not backed by an exact PRIMARY KEY or UNIQUE"));

        let alternate = op_json(
            r#"{
              "op":"createIndex", "table":"parents", "name":"parents_id_key",
              "columns":[{"kind":"column","name":"id"}], "unique":true,
              "include":[]
            }"#,
        );
        validate_ir_platform(
            &ir_with(vec![target, alternate, drop_pk, child]),
            Dialect::Postgres,
        )
        .expect("an exact alternate unique key survives the primary-key drop");
    }

    #[test]
    fn platform_profile_accepts_create_table_composite_primary_key() {
        let ir = ir_with(vec![op_json(
            r#"{
              "op": "createTable",
              "name": "memberships",
              "columns": [
                { "name": "account_id", "type": "uuid", "nullable": false },
                { "name": "team", "type": "text", "nullable": false }
              ],
              "primaryKey": ["account_id", "team"],
              "constraints": [],
              "indexes": []
            }"#,
        )]);

        for dialect in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
            validate_ir_scoped(&ir, dialect, &[], None).unwrap_or_else(|error| {
                panic!(
                    "{dialect:?} must accept an ordered author-owned composite primary key: {error}"
                )
            });
        }
    }

    #[test]
    fn create_table_primary_key_rejects_empty_duplicate_and_missing_columns_on_every_dialect() {
        let invalid = [
            (Vec::<&str>::new(), "empty"),
            (vec!["account_id", "account_id"], "more than once"),
            (
                vec!["account_id", "missing"],
                "absent from the resolved table",
            ),
        ];

        for dialect in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
            for (primary_key, reason) in &invalid {
                let ir = ir_with(vec![Op::CreateTable {
                    name: "memberships".into(),
                    columns: vec![
                        part_col("account_id", ColType::Uuid, false),
                        part_col("team", ColType::Text, false),
                    ],
                    primary_key: Some(
                        primary_key
                            .iter()
                            .map(|column| (*column).to_string())
                            .collect(),
                    ),
                    constraints: vec![],
                    indexes: vec![],
                    partition_by: None,
                    runtime_options: None,
                    schema: None,
                    existence_guard: None,
                }]);

                let error = validate_ir_scoped(&ir, dialect, &[], None).unwrap_err();
                assert_eq!(error.code, CODE_PRIMARY_KEY_INVALID, "got: {error}");
                assert!(
                    error.reason.contains(reason),
                    "{dialect:?} rejection must identify {reason:?}: {error}"
                );
            }
        }
    }

    #[test]
    fn platform_profile_accepts_create_table_null_primary_key() {
        let ir = ir_with(vec![op_json(
            r#"{
              "op": "createTable",
              "name": "events",
              "columns": [
                { "name": "stream", "type": "text", "nullable": false },
                { "name": "payload", "type": "json", "nullable": false }
              ],
              "primaryKey": null,
              "constraints": [],
              "indexes": []
            }"#,
        )]);

        validate_ir_scoped(&ir, Dialect::Postgres, &[], None)
            .expect("platform profile accepts no primary key");
    }

    // Cut 3 — the author-PK CONFORMANCE refusal is now owned by the injection
    // resolver (`resolve_create_table_policy` over the operator's `EffectivePolicy`),
    // NOT a hardcoded confined-profile gate in `validate_ir_scoped`. A createTable in
    // a mandatory-inject scope (author_primary_key = "forbid") that declares its own
    // PK is refused there with `AuthorPrimaryKeyForbidden`.
    #[test]
    fn confined_inject_refuses_create_table_author_primary_key() {
        let ir = ir_with(vec![op_json(
            r#"{
              "op": "createTable",
              "name": "memberships",
              "columns": [
                { "name": "account_id", "type": "uuid", "nullable": false },
                { "name": "team", "type": "text", "nullable": false }
              ],
              "primaryKey": ["account_id", "team"],
              "constraints": [],
              "indexes": []
            }"#,
        )]);

        // The pure PK-column validation still passes (the columns exist); the shape
        // conformance is the injection resolver's job.
        validate_ir_scoped(&ir, Dialect::Postgres, &[], None)
            .expect("pure primaryKey validation passes (columns present)");

        let err = crate::model::table_shape::resolve_create_table_policy(
            &ir,
            &crate::model::table_shape::zeroship_confined_ceiling(),
        )
        .expect_err("a mandatory-inject scope must refuse an author-owned createTable primaryKey");
        assert!(
            matches!(
                err,
                crate::model::table_shape::TableShapeError::AuthorPrimaryKeyForbidden { .. }
            ),
            "expected AuthorPrimaryKeyForbidden, got {err:?}"
        );
    }

    #[test]
    fn confined_profile_accepts_resolved_system_shape_create_table() {
        let raw = ir_with(vec![op_json(
            r#"{
              "op": "createTable",
              "name": "users",
              "columns": [
                { "name": "email", "type": "text", "nullable": false }
              ],
              "constraints": [],
              "indexes": []
            }"#,
        )]);
        let resolved = crate::model::table_shape::resolve_create_table_policy(
            &raw,
            &crate::model::table_shape::zeroship_confined_ceiling(),
        )
        .expect("confined table-shape resolution succeeds");

        validate_ir_scoped(&resolved, Dialect::Postgres, &[], None)
            .expect("resolved confined system shape remains valid");
    }

    // ColRef resolution at the apply/render seam. At LOAD the DML scope
    // is structural-only (the live column set is unknown), so an unresolved ColRef
    // PASSES the load walk. At APPLY, `validate_ir_resolved` re-runs the walk with
    // the resolved live columns and REJECTS a ColRef that does not resolve — with
    // the structured (c) error, NOT a raw DB error.
    #[test]
    fn validate_ir_resolved_rejects_unresolved_colref_in_update_set() {
        use std::collections::BTreeMap;
        // An update whose SET RHS references `ghost` — a column that does NOT exist
        // on the live `users` table.
        let ir = ir_with(vec![Op::Update {
            table: "users".into(),
            set: [("name".to_string(), IrValue::Expr(Expr::col("ghost")))]
                .into_iter()
                .collect(),
            r#where: None,
            schema: None,
        }]);

        // At LOAD: structural-only scope ⇒ the unresolved ColRef is NOT caught.
        assert!(
            validate_ir(&ir, Dialect::Postgres, &[]).is_ok(),
            "load-time validation is structural-only for DML (column set unknown)"
        );

        // At APPLY: resolve against the live columns of `users` (no `ghost`).
        let mut live: BTreeMap<String, Vec<String>> = BTreeMap::new();
        live.insert(
            "users".to_string(),
            vec!["id".to_string(), "name".to_string()],
        );
        let err = validate_ir_resolved(&ir, Dialect::Postgres, &live, &[])
            .expect_err("an unresolved ColRef must be rejected at the resolved apply seam");
        assert_eq!(
            err.code, CODE_UNSUPPORTED,
            "rule (c) failure is structured, not a raw DB error"
        );
        assert_eq!(err.op_index, 0);
    }

    #[test]
    fn validate_ir_resolved_rejects_unresolved_colref_in_insert_on_conflict_do_update() {
        use crate::model::ir::{IrOnConflict, IrValue};
        use std::collections::BTreeMap;
        // SA-18: an insert whose ON CONFLICT DO UPDATE assigns an Expr that
        // references `ghost` — a column that does NOT exist on live `users`.
        let mut do_update: BTreeMap<String, IrValue> = BTreeMap::new();
        do_update.insert("name".to_string(), IrValue::Expr(Expr::col("ghost")));
        let ir = ir_with(vec![Op::Insert {
            table: "users".into(),
            columns: vec!["name".into()],
            rows: vec![vec![IrValue::Scalar(crate::model::ir::IrScalar::Str(
                "x".into(),
            ))]],
            on_conflict: Some(IrOnConflict {
                columns: vec!["id".into()],
                do_update: Some(do_update),
            }),
            schema: None,
        }]);

        // At LOAD: structural-only ⇒ the unresolved ColRef is NOT caught (this is
        // the asymmetry SA-18 closes — pre-fix the resolved seam also missed it).
        assert!(validate_ir(&ir, Dialect::Postgres, &[]).is_ok());

        // At APPLY: resolve against the live columns of `users` (no `ghost`).
        let mut live: BTreeMap<String, Vec<String>> = BTreeMap::new();
        live.insert(
            "users".to_string(),
            vec!["id".to_string(), "name".to_string()],
        );
        let err = validate_ir_resolved(&ir, Dialect::Postgres, &live, &[])
            .expect_err("an unresolved ColRef in DO UPDATE must be rejected at the resolved seam");
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert_eq!(err.op_index, 0);
    }

    #[test]
    fn validate_ir_resolved_accepts_resolvable_colref_in_update_set() {
        use std::collections::BTreeMap;
        // The SAME shape but the ColRef references a column that DOES exist.
        let ir = ir_with(vec![Op::Update {
            table: "users".into(),
            set: [("name".to_string(), IrValue::Expr(Expr::col("name")))]
                .into_iter()
                .collect(),
            r#where: None,
            schema: None,
        }]);
        let mut live: BTreeMap<String, Vec<String>> = BTreeMap::new();
        live.insert(
            "users".to_string(),
            vec!["id".to_string(), "name".to_string()],
        );
        assert!(
            validate_ir_resolved(&ir, Dialect::Postgres, &live, &[]).is_ok(),
            "a ColRef that resolves to a live column passes the apply-seam (c) check"
        );
    }

    #[test]
    fn validate_ir_passes_a_clean_migration() {
        let ir = ir_with(vec![
            Op::CreateTable {
                name: "users".into(),
                columns: vec![
                    IrColumn {
                        name: "first".into(),
                        ty: ColType::Text,
                        nullable: None,
                        default: None,
                        unique: None,
                        value_format: None,
                        references: None,
                        id_prefix: None,
                        case_sensitive: None,
                        vector_metric: None,
                        mask: None,
                        generated: None,
                        identity: None,
                    },
                    IrColumn {
                        name: "total".into(),
                        ty: ColType::Int,
                        nullable: None,
                        default: None,
                        unique: None,
                        value_format: None,
                        references: None,
                        id_prefix: None,
                        case_sensitive: None,
                        vector_metric: None,
                        mask: None,
                        generated: None,
                        identity: None,
                    },
                ],
                primary_key: None,
                constraints: vec![],
                indexes: vec![IrIndex {
                    name: None,
                    columns: vec![IndexElement::Column {
                        name: "first".into(),
                        order: None,
                        opclass: None,
                        collation: None,
                    }],
                    unique: None,
                    using: None,
                    r#where: Some(Expr::UnaryOp {
                        op: UnaryOp::IsNotNull,
                        operand: Box::new(Expr::col("first")),
                    }),
                    include: Vec::new(),
                    with: None,
                    only: None,
                    nulls_not_distinct: None,
                }],

                partition_by: None,

                runtime_options: Default::default(),
                schema: None,
                existence_guard: None,
            },
            Op::Delete {
                table: "users".into(),
                r#where: Expr::lit(IrScalar::Bool(true)),
                limit: None,
                schema: None,
            },
        ]);
        assert!(validate_ir_platform(&ir, Dialect::Postgres).is_ok());
        assert!(validate_ir_platform(&ir, Dialect::Sqlite).is_ok());
    }

    fn rename_column(table: &str, from: &str, to: &str) -> Op {
        Op::RenameColumn {
            table: table.into(),
            from: from.into(),
            to: to.into(),
            ty: ColType::Text,
            schema: None,
            existence_guard: None,
        }
    }

    #[test]
    fn validate_ir_rejects_chained_online_renames_on_one_table() {
        let ir = ir_with(vec![
            rename_column("users", "display_name", "name"),
            rename_column("users", "name", "full_name"),
        ]);

        let err = validate_ir(&ir, Dialect::Postgres, &[])
            .expect_err("a migration cannot safely open two rename contracts on one table");
        assert_eq!(err.code, CODE_OP_INVALID);
        assert_eq!(err.kind, Some(UnsupportedKind::Op));
        assert_eq!(err.op_index, 1);
        assert!(err.reason.contains("only operation"), "got: {err}");
    }

    #[test]
    fn validate_ir_rejects_independent_online_renames_on_one_table() {
        let ir = ir_with(vec![
            rename_column("users", "first", "first_name"),
            rename_column("users", "last", "last_name"),
        ]);

        let err = validate_ir(&ir, Dialect::Postgres, &[])
            .expect_err("independent renames on one table have the same contract conflict");
        assert_eq!(err.code, CODE_OP_INVALID);
        assert_eq!(err.op_index, 1);
    }

    #[test]
    fn validate_ir_rejects_ddl_before_online_rename_on_same_table() {
        let ir = ir_with(vec![
            Op::DropColumn {
                table: "users".into(),
                column: "legacy_flag".into(),
                schema: None,
                existence_guard: None,
            },
            rename_column("users", "name", "display_name"),
        ]);

        let err = validate_ir(&ir, Dialect::Postgres, &[])
            .expect_err("a same-table DDL step before a rename must be rejected");
        assert_eq!(err.code, CODE_OP_INVALID);
        assert_eq!(err.op_index, 1);
        assert!(err.reason.contains("only operation"), "got: {err}");
    }

    #[test]
    fn validate_ir_rejects_dml_after_online_rename_on_same_table() {
        let ir = ir_with(vec![
            rename_column("users", "name", "display_name"),
            Op::Delete {
                table: "users".into(),
                r#where: Expr::lit(IrScalar::Bool(false)),
                limit: None,
                schema: None,
            },
        ]);

        let err = validate_ir(&ir, Dialect::Postgres, &[])
            .expect_err("a same-table DML step after a rename must be rejected");
        assert_eq!(err.code, CODE_OP_INVALID);
        assert_eq!(err.op_index, 1);
        assert!(err.reason.contains("only operation"), "got: {err}");
    }

    #[test]
    fn validate_ir_allows_same_table_companion_after_sqlite_rename() {
        let ir = ir_with(vec![
            rename_column("users", "name", "display_name"),
            Op::Delete {
                table: "users".into(),
                r#where: Expr::lit(IrScalar::Bool(false)),
                limit: None,
                schema: None,
            },
        ]);

        validate_ir(&ir, Dialect::Sqlite, &[])
            .expect("SQLite applies renameColumn as one rebuild without a pending contract");
        validate_ir(&ir, Dialect::Postgres, &[])
            .expect_err("PostgreSQL must still isolate its online rename contract");
    }

    #[test]
    fn validate_ir_allows_online_renames_on_different_tables() {
        let ir = ir_with(vec![
            rename_column("users", "name", "display_name"),
            rename_column("accounts", "label", "display_label"),
        ]);

        for dialect in [Dialect::Postgres, Dialect::Sqlite] {
            validate_ir(&ir, dialect, &[]).unwrap_or_else(|err| {
                panic!("renames on different tables should remain valid on {dialect:?}: {err}")
            });
        }
    }

    #[test]
    fn validate_ir_allows_companion_operations_on_different_table() {
        let ir = ir_with(vec![
            Op::DropColumn {
                table: "accounts".into(),
                column: "legacy_flag".into(),
                schema: None,
                existence_guard: None,
            },
            rename_column("users", "name", "display_name"),
            Op::Delete {
                table: "accounts".into(),
                r#where: Expr::lit(IrScalar::Bool(false)),
                limit: None,
                schema: None,
            },
        ]);

        validate_ir(&ir, Dialect::Postgres, &[])
            .expect("DDL and DML on a different table remain valid companions");
    }

    #[test]
    fn validate_ir_checks_only_the_selected_dialectal_rename_sequence() {
        let ir = ir_with(vec![Op::Dialectal {
            default: None,
            pg: Some(vec![
                rename_column("users", "first", "first_name"),
                rename_column("users", "last", "last_name"),
            ]),
            sqlite: Some(vec![rename_column("users", "name", "display_name")]),
            mysql: None,
        }]);

        let err = validate_ir(&ir, Dialect::Postgres, &[])
            .expect_err("the two renames in the selected PostgreSQL leg must be rejected");
        assert_eq!(err.code, CODE_OP_INVALID);
        assert_eq!(err.op_index, 0);

        validate_ir(&ir, Dialect::Sqlite, &[])
            .expect("mutually exclusive dialect legs do not run in one migration");
    }

    #[test]
    fn validate_ir_accepts_date_columns_on_all_dialects() {
        let ir = ir_with(vec![create_with_column("business_day", ColType::Date)]);
        for dialect in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
            validate_ir_platform(&ir, dialect)
                .unwrap_or_else(|err| panic!("{dialect:?} should accept date columns: {err:?}"));
        }
    }

    #[test]
    fn validate_ir_rejects_initially_deferred_without_deferrable() {
        let create_table = op_json(
            r#"{
                "op":"createTable",
                "name":"orders",
                "columns":[{"name":"user_id","type":"text"}],
                "constraints":[{
                    "name":"orders_user_fk",
                    "kind":{
                        "kind":"fk",
                        "columns":["user_id"],
                        "referencesTable":"users",
                        "referencesColumns":["id"],
                        "initiallyDeferred":true
                    }
                }]
            }"#,
        );
        let add_constraint = op_json(
            r#"{
                "op":"addConstraint",
                "table":"orders",
                "constraint":{
                    "name":"orders_user_fk",
                    "kind":{
                        "kind":"fk",
                        "columns":["user_id"],
                        "referencesTable":"users",
                        "referencesColumns":["id"],
                        "initiallyDeferred":true
                    }
                }
            }"#,
        );

        for op in [create_table, add_constraint] {
            let ir = ir_with(vec![op]);
            let err = validate_ir_platform(&ir, Dialect::Postgres)
                .expect_err("initiallyDeferred without deferrable must be rejected");
            assert_eq!(err.code, CODE_OP_INVALID);
            assert_eq!(err.reason, "initiallyDeferred requires deferrable");
        }
    }

    #[test]
    fn validate_ir_rejects_sequence_increment_zero() {
        let ir = ir_with(vec![op_json(
            r#"{"op":"createSequence","name":"s","increment":0}"#,
        )]);
        let err = validate_ir_platform(&ir, Dialect::Postgres).unwrap_err();
        assert_eq!(err.code, CODE_SEQUENCE_OPTION_INVALID);
        assert!(err.reason.contains("increment"));
    }

    #[test]
    fn validate_ir_rejects_sequence_cache_zero() {
        let ir = ir_with(vec![op_json(
            r#"{"op":"alterSequence","name":"s","cache":0}"#,
        )]);
        let err = validate_ir_platform(&ir, Dialect::Postgres).unwrap_err();
        assert_eq!(err.code, CODE_SEQUENCE_OPTION_INVALID);
        assert!(err.reason.contains("cache"));
    }

    #[test]
    fn validate_ir_rejects_sequence_min_greater_than_max() {
        let ir = ir_with(vec![op_json(
            r#"{"op":"createSequence","name":"s","minValue":10,"maxValue":9}"#,
        )]);
        let err = validate_ir_platform(&ir, Dialect::Postgres).unwrap_err();
        assert_eq!(err.code, CODE_SEQUENCE_OPTION_INVALID);
        assert!(err.reason.contains("minValue"));
    }

    #[test]
    fn validate_ir_create_table_partial_index_resolves_system_fields_in_scope() {
        // The profile resolver materializes the seven platform system fields
        // before validation/lowering. A legitimate soft-delete partial-unique index
        // `WHERE deleted_at IS NULL` references the resolved column and MUST
        // resolve in rule (c) scope, not be rejected.
        let ir = ir_with(vec![Op::CreateTable {
            name: "users".into(),
            columns: vec![IrColumn {
                name: "first".into(),
                ty: ColType::Text,
                nullable: None,
                default: None,
                unique: None,
                value_format: None,
                references: None,
                id_prefix: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
            primary_key: None,
            constraints: vec![],
            indexes: vec![IrIndex {
                name: None,
                columns: vec![IndexElement::Column {
                    name: "first".into(),
                    order: None,
                    opclass: None,
                    collation: None,
                }],
                unique: Some(true),
                using: None,
                // the canonical soft-delete partial-unique predicate
                r#where: Some(Expr::UnaryOp {
                    op: UnaryOp::IsNull,
                    operand: Box::new(Expr::col("deleted_at")),
                }),
                include: Vec::new(),
                with: None,
                only: None,
                nulls_not_distinct: None,
            }],

            partition_by: None,

            runtime_options: Default::default(),
            schema: None,
            existence_guard: None,
        }]);
        let ir = crate::model::table_shape::resolve_create_table_policy(
            &ir,
            &crate::model::table_shape::zeroship_confined_ceiling(),
        )
        .expect("resolve confined table shape");
        assert!(
            validate_ir(&ir, Dialect::Postgres, &[]).is_ok(),
            "a partial index on `deleted_at` must resolve system fields (PG)"
        );
        assert!(
            validate_ir(&ir, Dialect::Sqlite, &[]).is_ok(),
            "a partial index on `deleted_at` must resolve system fields (SQLite)"
        );
    }

    #[test]
    fn validate_ir_create_table_still_rejects_truly_unknown_column() {
        // The system-field union must NOT loosen the gate for a genuinely unknown
        // column — `ghost` is neither declared nor a system field.
        let ir = ir_with(vec![Op::CreateTable {
            name: "users".into(),
            columns: vec![IrColumn {
                name: "first".into(),
                ty: ColType::Text,
                nullable: None,
                default: None,
                unique: None,
                value_format: None,
                references: None,
                id_prefix: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
            primary_key: None,
            constraints: vec![IrConstraint {
                name: None,
                kind: IrConstraintKind::Check {
                    expr: Expr::UnaryOp {
                        op: UnaryOp::IsNotNull,
                        operand: Box::new(Expr::col("ghost")),
                    },

                    not_valid: None,
                },
            }],
            indexes: vec![],

            partition_by: None,

            runtime_options: None,
            schema: None,
            existence_guard: None,
        }]);
        let err = validate_ir_platform(&ir, Dialect::Postgres).unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
    }

    #[test]
    fn validate_ir_rejects_check_colref_to_nonexistent_column() {
        // A createTable whose Check references a column NOT on the table — rule
        // (c). The walker resolves the createTable's own columns, so this fails.
        let ir = ir_with(vec![Op::CreateTable {
            name: "users".into(),
            columns: vec![IrColumn {
                name: "first".into(),
                ty: ColType::Text,
                nullable: None,
                default: None,
                unique: None,
                value_format: None,
                references: None,
                id_prefix: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
            primary_key: None,
            constraints: vec![IrConstraint {
                name: None,
                kind: IrConstraintKind::Check {
                    expr: Expr::UnaryOp {
                        op: UnaryOp::IsNotNull,
                        operand: Box::new(Expr::col("ghost")),
                    },

                    not_valid: None,
                },
            }],
            indexes: vec![],

            partition_by: None,

            runtime_options: None,
            schema: None,
            existence_guard: None,
        }]);
        let err = validate_ir_platform(&ir, Dialect::Postgres).unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        assert_eq!(err.op_index, 0);
    }

    #[test]
    fn validate_ir_rejects_out_of_envelope_split_part_in_update_set() {
        // The Update is the SECOND op — the walker must stamp op_index = 1, and
        // it must reach the `set` RHS (the splitPart) to reject it.
        let mut set = BTreeMap::new();
        set.insert("name".to_string(), IrValue::Expr(split(", ", 1))); // multi-char delim
        let ir = ir_with(vec![
            Op::DropColumn {
                table: "t".into(),
                column: "x".into(),
                schema: None,
                existence_guard: None,
            },
            Op::Update {
                table: "users".into(),
                set,
                r#where: None,
                schema: None,
            },
        ]);
        let ts = vec![None, Some("m.ts:9".to_string())];
        let err = validate_ir(&ir, Dialect::Sqlite, &ts).unwrap_err();
        assert_eq!(err.code, CODE_EXPR_NOT_PORTABLE);
        assert_eq!(
            err.op_index, 1,
            "the walker must stamp the enclosing op's index"
        );
        assert_eq!(err.ts_location.as_deref(), Some("m.ts:9"));
    }

    #[test]
    fn validate_ir_walks_create_index_where_predicate() {
        // The property-A fix made createIndex.where a closed Expr — the walker
        // must now reach it. An out-of-envelope splitPart there must reject.
        let ir = ir_with(vec![Op::CreateIndex {
            table: "users".into(),
            columns: vec![IndexElement::Column {
                name: "a".into(),
                order: None,
                opclass: None,
                collation: None,
            }],
            name: None,
            unique: None,
            using: None,
            r#where: Some(split(", ", 1)),

            include: Vec::new(),
            with: None,
            only: None,
            nulls_not_distinct: None,
            concurrently: None,
            schema: None,
            existence_guard: None,
        }]);
        let err = validate_ir(&ir, Dialect::Sqlite, &[]).unwrap_err();
        assert_eq!(err.code, CODE_EXPR_NOT_PORTABLE);
        assert_eq!(err.op_index, 0);
    }

    #[test]
    fn validate_ir_rejects_aggregate_count_in_index_predicate() {
        // Regression: moving aggregates to ExprChain methods makes aggregate
        // nodes type-reachable in immutable/scalar slots. The Rust validator is
        // the authoritative backstop; before this check, this createIndex.where
        // node passed validate cleanly.
        let ir = ir_with(vec![Op::CreateIndex {
            table: "users".into(),
            columns: vec![IndexElement::Column {
                name: "a".into(),
                order: None,
                opclass: None,
                collation: None,
            }],
            name: None,
            unique: None,
            using: None,
            r#where: Some(Expr::Agg {
                func: AggFunc::Count,
                arg: None,
                delimiter: None,
                distinct: false,
            }),
            include: Vec::new(),
            with: None,
            only: None,
            nulls_not_distinct: None,
            concurrently: None,
            schema: None,
            existence_guard: None,
        }]);
        let err = validate_ir(&ir, Dialect::Postgres, &[]).unwrap_err();
        assert_eq!(err.code, CODE_AGGREGATE_IN_SCALAR_CONTEXT);
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        assert_eq!(err.op_index, 0);
        assert!(
            err.reason.contains("count()"),
            "reason names offending aggregate: {err}"
        );
        assert!(
            err.reason.contains("index predicate"),
            "reason names scalar context: {err}"
        );
    }

    #[test]
    fn validate_ir_rejects_volatile_now_in_index_predicate() {
        // Regression: moving now()/genRandomUuid() to top-level imports makes
        // volatile nodes type-reachable in immutable slots. The Rust validator is
        // the authoritative backstop; before this check, this createIndex.where
        // node passed validate cleanly.
        let ir = ir_with(vec![Op::CreateIndex {
            table: "users".into(),
            columns: vec![IndexElement::Column {
                name: "a".into(),
                order: None,
                opclass: None,
                collation: None,
            }],
            name: None,
            unique: None,
            using: None,
            r#where: Some(Expr::FnSynth {
                r#fn: SynthFn::Now,
                args: vec![],
            }),

            include: Vec::new(),
            with: None,
            only: None,
            nulls_not_distinct: None,
            concurrently: None,
            schema: None,
            existence_guard: None,
        }]);
        let err = validate_ir(&ir, Dialect::Postgres, &[]).unwrap_err();
        assert_eq!(err.code, CODE_IMMUTABLE_CONTEXT_VOLATILE);
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        assert_eq!(err.op_index, 0);
        assert!(
            err.reason.contains("now()"),
            "reason names offending function: {err}"
        );
        assert!(
            err.reason.contains("index predicate"),
            "reason names immutable context: {err}"
        );
    }

    #[test]
    fn validate_ir_refuses_set_column_type_using_until_expr_renderer_lands() {
        let ir = ir_with(vec![Op::SetColumnType {
            table: "users".into(),
            column: "a".into(),
            to_type: ColType::Int,
            using: Some(split(", ", 1)),
            schema: None,
            existence_guard: None,
        }]);
        let err = validate_ir(&ir, Dialect::Sqlite, &[]).unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        assert!(err.reason.contains("setColumnType.using"));
        assert_eq!(err.op_index, 0);
    }

    #[test]
    fn validate_ir_walks_backfill_filter_and_set() {
        let mut set = BTreeMap::new();
        set.insert(
            "name".to_string(),
            BackfillSetValue::from(IrValue::Expr(Expr::col("first"))),
        ); // fine structurally
        let ir = ir_with(vec![Op::Backfill {
            table: "users".into(),
            cursor_columns: vec!["id".into()],
            cursor_stability: crate::model::ir::CursorStability::GuardUpdates,
            batch_size: serde_json::from_str("100").unwrap(),
            set,
            filter: Some(split(", ", 1)), // out-of-envelope → reject
            name: "bf".into(),
            schema: None,
        }]);
        let err = validate_ir(&ir, Dialect::Sqlite, &[]).unwrap_err();
        assert_eq!(err.code, CODE_EXPR_NOT_PORTABLE);
    }

    #[test]
    fn validate_ir_rejects_volatile_backfill_filter() {
        let ir = ir_with(vec![Op::Backfill {
            table: "users".into(),
            cursor_columns: vec!["id".into()],
            cursor_stability: crate::model::ir::CursorStability::GuardUpdates,
            batch_size: serde_json::from_str("100").unwrap(),
            set: [(
                "name".to_string(),
                BackfillSetValue::from(IrValue::Expr(Expr::col("first"))),
            )]
            .into_iter()
            .collect(),
            filter: Some(Expr::FnSynth {
                r#fn: SynthFn::Now,
                args: vec![],
            }),
            name: "bf".into(),
            schema: None,
        }]);

        let err = validate_ir(&ir, Dialect::Sqlite, &[]).unwrap_err();
        assert_eq!(err.code, CODE_IMMUTABLE_CONTEXT_VOLATILE);
        assert!(err.reason.contains("backfill filter"), "{err}");
        assert!(err.reason.contains("now()"), "{err}");
    }

    #[test]
    fn validate_ir_rejects_aggregate_backfill_filter() {
        let ir = ir_with(vec![Op::Backfill {
            table: "users".into(),
            cursor_columns: vec!["id".into()],
            cursor_stability: crate::model::ir::CursorStability::GuardUpdates,
            batch_size: serde_json::from_str("100").unwrap(),
            set: [(
                "name".to_string(),
                BackfillSetValue::from(IrValue::Expr(Expr::col("first"))),
            )]
            .into_iter()
            .collect(),
            filter: Some(Expr::Agg {
                func: AggFunc::Count,
                arg: None,
                delimiter: None,
                distinct: false,
            }),
            name: "bf".into(),
            schema: None,
        }]);

        let err = validate_ir(&ir, Dialect::Postgres, &[]).unwrap_err();
        assert_eq!(err.code, CODE_AGGREGATE_IN_SCALAR_CONTEXT);
        assert!(err.reason.contains("backfill filter"), "{err}");
        assert!(err.reason.contains("count()"), "{err}");
    }

    #[test]
    fn backfill_rejects_assignment_to_any_composite_cursor_component() {
        for assigned in ["tenant_id", "sequence"] {
            let ir = ir_with(vec![Op::Backfill {
                table: "events".into(),
                cursor_columns: vec!["tenant_id".into(), "sequence".into()],
                cursor_stability: crate::model::ir::CursorStability::GuardUpdates,
                batch_size: serde_json::from_str("100").unwrap(),
                set: [(
                    assigned.to_string(),
                    BackfillSetValue::from(IrValue::Scalar(IrScalar::Int(1))),
                )]
                .into_iter()
                .collect(),
                filter: None,
                name: "bf".into(),
                schema: None,
            }]);
            let error = validate_ir(&ir, Dialect::Postgres, &[])
                .expect_err("cursor components are immutable destinations");
            assert_eq!(error.code, CODE_OP_INVALID);
            assert!(error.reason.contains(assigned), "{error}");
            assert!(error.reason.contains("cursor component"), "{error}");
        }
    }

    #[test]
    fn case_insensitive_dialects_reject_case_variant_cursor_mutation() {
        for dialect in [Dialect::Sqlite, Dialect::Mysql] {
            let ir = ir_with(vec![Op::Backfill {
                table: "events".into(),
                cursor_columns: vec!["event_id".into()],
                cursor_stability: crate::model::ir::CursorStability::GuardUpdates,
                batch_size: serde_json::from_str("100").unwrap(),
                set: [(
                    "EVENT_ID".to_string(),
                    BackfillSetValue::from(IrValue::Scalar(IrScalar::Int(1))),
                )]
                .into_iter()
                .collect(),
                filter: None,
                name: "bf".into(),
                schema: None,
            }]);
            let error = validate_ir(&ir, dialect, &[])
                .expect_err("case-only spelling still targets the cursor component");
            assert_eq!(error.code, CODE_OP_INVALID);
            assert!(error.reason.contains("cursor component"), "{error}");
        }
    }

    #[test]
    fn external_cursor_invariant_requires_a_bounded_visible_name() {
        for name in ["   ".to_string(), "x".repeat(256)] {
            let ir = ir_with(vec![Op::Backfill {
                table: "events".into(),
                cursor_columns: vec!["id".into()],
                cursor_stability: crate::model::ir::CursorStability::ExternalInvariant { name },
                batch_size: serde_json::from_str("100").unwrap(),
                set: [(
                    "payload".to_string(),
                    BackfillSetValue::from(IrValue::Scalar(IrScalar::Str("ready".into()))),
                )]
                .into_iter()
                .collect(),
                filter: None,
                name: "bf".into(),
                schema: None,
            }]);
            let error = validate_ir(&ir, Dialect::Postgres, &[])
                .expect_err("external invariant name is operator-visible metadata");
            assert_eq!(error.code, CODE_OP_INVALID);
            assert!(error.reason.contains("externalInvariant"), "{error}");
        }

        let accepted = ir_with(vec![Op::Backfill {
            table: "events".into(),
            cursor_columns: vec!["id".into()],
            cursor_stability: crate::model::ir::CursorStability::ExternalInvariant {
                name: "events_id_updates_disabled_during_migration".into(),
            },
            batch_size: serde_json::from_str("100").unwrap(),
            set: [(
                "payload".to_string(),
                BackfillSetValue::from(IrValue::Scalar(IrScalar::Str("ready".into()))),
            )]
            .into_iter()
            .collect(),
            filter: None,
            name: "bf".into(),
            schema: None,
        }]);
        validate_ir(&accepted, Dialect::Postgres, &[])
            .expect("a named external invariant is explicitly authorable");
    }

    // ── the names-stay-strings BINDING corollary ───────────────────────────
    //
    // This is the apply-time HALF of the guarantee. The OTHER half lives in
    // the JS type-level suite (`packages/zero-migrate/tests/types/type-tests.ts`): a
    // migration whose table/column NAMES are plain strings type-checks cleanly
    // EVEN WHEN those names are not in the current generated db schema (the
    // anti-rot guarantee — names are NOT live-schema-bound, so an immutable
    // historical migration never rots as the schema evolves).
    //
    // The corollary this test pins: because tsc CANNOT see the name (it is a
    // plain string), a migration that references a NON-EXISTENT column must fail
    // at APPLY — never silently mis-apply — with the STRUCTURED error. Load is
    // structural-only (the name is accepted, mirroring tsc accepting the string),
    // and the resolved apply seam is the SOLE place a bad name is caught.
    #[test]
    fn pr5_nonexistent_column_name_fails_at_apply_not_at_load_with_structured_error() {
        use std::collections::BTreeMap;

        // A migration whose `where` and `set` reference `column_that_was_dropped`
        // — a plain-string name the JS DSL type-checks (it is NOT live-schema-
        // bound) and that does NOT exist on the live `users` table.
        let ir = ir_with(vec![Op::Update {
            table: "users".into(),
            set: [(
                "name".to_string(),
                IrValue::Expr(Expr::col("column_that_was_dropped")),
            )]
            .into_iter()
            .collect(),
            r#where: Some(Expr::BinOp {
                op: BinaryOp::Eq,
                lhs: Box::new(Expr::col("column_that_was_dropped")),
                rhs: Box::new(Expr::lit(IrScalar::Int(1))),
            }),
            schema: None,
        }]);

        // LOAD-time (the tsc-analog): structural-only — the plain-string name is
        // ACCEPTED, exactly as tsc accepts the string literal. NOT rejected here.
        assert!(
            validate_ir(&ir, Dialect::Postgres, &[]).is_ok(),
            "a plain-string column name is accepted at load (the tsc-analog), never name-bound"
        );

        // APPLY-time (resolved against the REAL live columns): the missing name is
        // the SOLE place it is caught — with the STRUCTURED `UNSUPPORTED { expr }`
        // error, not a raw DB \"column does not exist\" surprise.
        let mut live: BTreeMap<String, Vec<String>> = BTreeMap::new();
        live.insert(
            "users".to_string(),
            vec!["id".to_string(), "name".to_string()],
        );
        let err = validate_ir_resolved(&ir, Dialect::Postgres, &live, &[])
            .expect_err("a non-existent column name must FAIL at the resolved apply seam");
        assert_eq!(
            err.code, CODE_UNSUPPORTED,
            "the apply-time name reject is structured (the error envelope), not a raw DB error"
        );
        assert_eq!(
            err.kind,
            Some(UnsupportedKind::Expr),
            "an unknown column is a rule-(c) expr-kind capability-boundary reject"
        );
        assert_eq!(
            err.op_index, 0,
            "the structured error attributes the failing op"
        );
    }

    // ── column-facet validate-time bounds ───────────────────────────────────
    // RED before the `validate_column_facets` wiring: a hand-crafted IR envelope
    // carrying a malformed/reserved/over-long id_prefix or a misplaced metric would
    // have passed validate and deferred the blow-up to render / mint colliding ids.

    use crate::model::ir::{EmptyContainerKind, IrJsonValue, ValueFormat, VectorMetric};

    /// Build a createTable Op with a single `id` column carrying `id_prefix`.
    fn create_with_id_prefix(prefix: &str) -> Op {
        Op::CreateTable {
            name: "things".into(),
            columns: vec![IrColumn {
                name: "id".into(),
                ty: ColType::Uuid,
                nullable: None,
                default: None,
                unique: None,
                value_format: None,
                references: None,
                id_prefix: Some(prefix.to_string()),
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
            primary_key: None,
            constraints: vec![],
            indexes: vec![],

            partition_by: None,

            runtime_options: None,
            schema: None,
            existence_guard: None,
        }
    }

    fn create_with_type_id(prefix: &str, ty: ColType, case_sensitive: Option<bool>) -> Op {
        Op::CreateTable {
            name: "things".into(),
            columns: vec![IrColumn {
                name: "id".into(),
                ty,
                nullable: None,
                default: None,
                unique: None,
                value_format: Some(ValueFormat::TypeId {
                    prefix: prefix.to_string(),
                }),
                references: None,
                id_prefix: None,
                case_sensitive,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
            primary_key: None,
            constraints: vec![],
            indexes: vec![],
            partition_by: None,
            runtime_options: None,
            schema: None,
            existence_guard: None,
        }
    }

    fn create_with_ulid(ty: ColType, case_sensitive: Option<bool>) -> Op {
        Op::CreateTable {
            name: "things".into(),
            columns: vec![IrColumn {
                name: "id".into(),
                ty,
                nullable: None,
                default: None,
                unique: None,
                value_format: Some(ValueFormat::Ulid),
                references: None,
                id_prefix: None,
                case_sensitive,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
            primary_key: None,
            constraints: vec![],
            indexes: vec![],
            partition_by: None,
            runtime_options: None,
            schema: None,
            existence_guard: None,
        }
    }

    fn create_with_default(ty: ColType, default: crate::model::ir::IrDefault) -> Op {
        Op::CreateTable {
            name: "docs".into(),
            columns: vec![IrColumn {
                name: "body".into(),
                ty,
                nullable: None,
                default: Some(default),
                unique: None,
                value_format: None,
                references: None,
                id_prefix: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
            primary_key: None,
            constraints: vec![],
            indexes: vec![],
            partition_by: None,
            runtime_options: None,
            schema: None,
            existence_guard: None,
        }
    }

    fn json_value_default() -> crate::model::ir::IrDefault {
        crate::model::ir::IrDefault::Json {
            value: IrJsonValue::Object(
                [("a".to_string(), IrJsonValue::Int(1))]
                    .into_iter()
                    .collect(),
            ),
        }
    }

    #[test]
    fn p2a_create_table_accepts_a_valid_id_prefix() {
        let ir = ir_with(vec![create_with_id_prefix("post")]);
        assert!(
            validate_ir_platform(&ir, Dialect::Postgres).is_ok(),
            "a well-formed, unreserved, in-length id prefix must validate"
        );
    }

    #[test]
    fn type_id_value_format_accepts_canonical_prefixes_on_exact_text() {
        let max_prefix = "a".repeat(crate::model::ir::TYPE_ID_MAX_PREFIX_LEN);
        for prefix in ["", "a", "my__type", max_prefix.as_str()] {
            for dialect in [Dialect::Postgres, Dialect::Mysql, Dialect::Sqlite] {
                let ir = ir_with(vec![create_with_type_id(prefix, ColType::Text, None)]);
                assert!(
                    validate_ir_platform(&ir, dialect).is_ok(),
                    "canonical TypeID prefix {prefix:?} must validate for {dialect:?}"
                );
            }
        }
    }

    #[test]
    fn type_id_value_format_rejects_noncanonical_prefixes() {
        let overlong = "a".repeat(crate::model::ir::TYPE_ID_MAX_PREFIX_LEN + 1);
        for prefix in [
            "_user",
            "user_",
            "User",
            "user1",
            "us-er",
            "týpe",
            overlong.as_str(),
        ] {
            let ir = ir_with(vec![create_with_type_id(prefix, ColType::Text, None)]);
            let error = validate_ir_platform(&ir, Dialect::Postgres)
                .expect_err("a noncanonical TypeID prefix must fail closed");
            assert_eq!(error.code, CODE_INVALID_TYPE_ID_PREFIX, "got: {error}");
        }
    }

    #[test]
    fn type_id_value_format_requires_exact_text_storage() {
        for ty in [
            ColType::Uuid,
            ColType::String,
            ColType::Encrypted {
                of: Box::new(ColType::Text),
            },
        ] {
            let ir = ir_with(vec![create_with_type_id("user", ty, None)]);
            let error = validate_ir_platform(&ir, Dialect::Postgres)
                .expect_err("TypeID metadata on non-text storage must fail closed");
            assert_eq!(error.code, CODE_COLUMN_FACET_CONFLICT, "got: {error}");
            assert!(error.reason.contains("exact text storage"), "got: {error}");
        }
    }

    #[test]
    fn type_id_value_format_rejects_case_insensitive_text() {
        let ir = ir_with(vec![create_with_type_id(
            "user",
            ColType::Text,
            Some(false),
        )]);
        let error = validate_ir_platform(&ir, Dialect::Postgres)
            .expect_err("TypeID plus caseSensitive:false must fail closed");
        assert_eq!(error.code, CODE_COLUMN_FACET_CONFLICT, "got: {error}");
        assert!(error.reason.contains("bytewise"), "got: {error}");
    }

    #[test]
    fn type_id_value_format_rejects_a_weakening_index_collation() {
        let mut op = create_with_type_id("user", ColType::Text, None);
        let Op::CreateTable { indexes, .. } = &mut op else {
            unreachable!("helper always returns createTable");
        };
        indexes.push(IrIndex {
            name: Some("things_id_ci".into()),
            columns: vec![crate::model::ir::IndexElement::Column {
                name: "id".into(),
                order: None,
                opclass: None,
                collation: Some("und-x-icu".into()),
            }],
            unique: Some(true),
            using: None,
            r#where: None,
            include: vec![],
            with: None,
            only: None,
            nulls_not_distinct: None,
        });

        let error = validate_ir_platform(&ir_with(vec![op]), Dialect::Postgres)
            .expect_err("a non-bytewise TypeID index collation must fail closed");
        assert_eq!(error.code, CODE_COLUMN_FACET_CONFLICT, "got: {error}");
        assert!(error.reason.contains("collation"), "got: {error}");
        assert!(error.reason.contains("bytewise"), "got: {error}");
    }

    #[test]
    fn add_column_type_id_value_format_uses_the_same_policy_gate() {
        let valid = ir_with(vec![op_json(
            r#"{"op":"addColumn","table":"things","column":"id","type":"text","valueFormat":{"typeId":{"prefix":"thing"}}}"#,
        )]);
        assert!(validate_ir_platform(&valid, Dialect::Postgres).is_ok());

        let wrong_storage = ir_with(vec![op_json(
            r#"{"op":"addColumn","table":"things","column":"id","type":"uuid","valueFormat":{"typeId":{"prefix":"thing"}}}"#,
        )]);
        let error = validate_ir_platform(&wrong_storage, Dialect::Postgres)
            .expect_err("addColumn TypeID metadata on UUID storage must fail closed");
        assert_eq!(error.code, CODE_COLUMN_FACET_CONFLICT, "got: {error}");
    }

    #[test]
    fn ulid_value_format_accepts_exact_text_on_every_dialect() {
        for dialect in [Dialect::Postgres, Dialect::Mysql, Dialect::Sqlite] {
            let ir = ir_with(vec![create_with_ulid(ColType::Text, None)]);
            assert!(
                validate_ir_platform(&ir, dialect).is_ok(),
                "ULID text storage must validate for {dialect:?}"
            );
        }
    }

    #[test]
    fn ulid_value_format_requires_exact_text_storage() {
        for ty in [
            ColType::Uuid,
            ColType::String,
            ColType::Encrypted {
                of: Box::new(ColType::Text),
            },
        ] {
            let ir = ir_with(vec![create_with_ulid(ty, None)]);
            let error = validate_ir_platform(&ir, Dialect::Postgres)
                .expect_err("ULID metadata on non-text storage must fail closed");
            assert_eq!(error.code, CODE_COLUMN_FACET_CONFLICT, "got: {error}");
            assert!(error.reason.contains("exact text storage"), "got: {error}");
            assert!(error.reason.contains("ULID"), "got: {error}");
        }
    }

    #[test]
    fn ulid_value_format_rejects_case_insensitive_text() {
        let ir = ir_with(vec![create_with_ulid(ColType::Text, Some(false))]);
        let error = validate_ir_platform(&ir, Dialect::Postgres)
            .expect_err("ULID plus caseSensitive:false must fail closed");
        assert_eq!(error.code, CODE_COLUMN_FACET_CONFLICT, "got: {error}");
        assert!(error.reason.contains("ULID"), "got: {error}");
        assert!(error.reason.contains("bytewise"), "got: {error}");
    }

    #[test]
    fn ulid_value_format_rejects_a_weakening_index_collation() {
        let mut op = create_with_ulid(ColType::Text, None);
        let Op::CreateTable { indexes, .. } = &mut op else {
            unreachable!("helper always returns createTable");
        };
        indexes.push(IrIndex {
            name: Some("things_id_ci".into()),
            columns: vec![crate::model::ir::IndexElement::Column {
                name: "id".into(),
                order: None,
                opclass: None,
                collation: Some("und-x-icu".into()),
            }],
            unique: Some(true),
            using: None,
            r#where: None,
            include: vec![],
            with: None,
            only: None,
            nulls_not_distinct: None,
        });

        let error = validate_ir_platform(&ir_with(vec![op]), Dialect::Postgres)
            .expect_err("a non-bytewise ULID index collation must fail closed");
        assert_eq!(error.code, CODE_COLUMN_FACET_CONFLICT, "got: {error}");
        assert!(error.reason.contains("ULID"), "got: {error}");
        assert!(error.reason.contains("collation"), "got: {error}");
        assert!(error.reason.contains("bytewise"), "got: {error}");
    }

    #[test]
    fn add_column_ulid_value_format_uses_the_same_policy_gate() {
        let valid = ir_with(vec![op_json(
            r#"{"op":"addColumn","table":"things","column":"id","type":"text","valueFormat":"ulid"}"#,
        )]);
        assert!(validate_ir_platform(&valid, Dialect::Postgres).is_ok());

        let wrong_storage = ir_with(vec![op_json(
            r#"{"op":"addColumn","table":"things","column":"id","type":"uuid","valueFormat":"ulid"}"#,
        )]);
        let error = validate_ir_platform(&wrong_storage, Dialect::Postgres)
            .expect_err("addColumn ULID metadata on UUID storage must fail closed");
        assert_eq!(error.code, CODE_COLUMN_FACET_CONFLICT, "got: {error}");
    }

    #[test]
    fn legacy_uuid_id_prefix_remains_valid() {
        let ir = ir_with(vec![create_with_id_prefix("post")]);
        assert!(validate_ir_platform(&ir, Dialect::Postgres).is_ok());
    }

    #[test]
    fn p2a_create_table_rejects_a_reserved_id_prefix() {
        // `usr` is the platform user-id prefix (RESERVED_ID_PREFIXES); a creator
        // prefix that collides with it would mint ids colliding with platform users.
        let ir = ir_with(vec![create_with_id_prefix("usr")]);
        let err = validate_ir_platform(&ir, Dialect::Postgres)
            .expect_err("a reserved id prefix must be refused at validate, fail-closed");
        assert_eq!(err.code, CODE_INVALID_ID_PREFIX, "got: {err}");
        assert_eq!(err.op_index, 0);
    }

    #[test]
    fn p2a_create_table_rejects_a_malformed_id_prefix() {
        // An upper-case / non-`[a-z0-9_]` prefix is not a valid typed-id segment.
        let ir = ir_with(vec![create_with_id_prefix("Po-st")]);
        let err = validate_ir_platform(&ir, Dialect::Postgres)
            .expect_err("a malformed id prefix must be refused at validate");
        assert_eq!(err.code, CODE_INVALID_ID_PREFIX, "got: {err}");
    }

    #[test]
    fn p2a_create_table_rejects_an_over_long_id_prefix() {
        // Charset-valid but longer than MAX_ID_PREFIX_LEN — refused so the minted
        // `<prefix>_<22 base62>` typed-id keeps the compact platform shape.
        let ir = ir_with(vec![create_with_id_prefix("toolong")]);
        let err = validate_ir_platform(&ir, Dialect::Postgres)
            .expect_err("an over-long id prefix must be refused at validate");
        assert_eq!(err.code, CODE_INVALID_ID_PREFIX, "got: {err}");
        assert!(
            err.reason.contains("maximum"),
            "the error names the length bound: {err}"
        );
    }

    #[test]
    fn p2a_create_table_rejects_vector_metric_on_non_vector_column() {
        // A metric on a non-Vector column is the co-occurrence violation — the
        // closed enum already bounds the metric token at deserialize; this catches a
        // dead metric a hand-crafted artifact rides in on a text column.
        let ir = ir_with(vec![Op::CreateTable {
            name: "docs".into(),
            columns: vec![IrColumn {
                name: "body".into(),
                ty: ColType::Text,
                nullable: None,
                default: None,
                unique: None,
                value_format: None,
                references: None,
                id_prefix: None,
                case_sensitive: None,
                vector_metric: Some(VectorMetric::Cosine),
                mask: None,
                generated: None,
                identity: None,
            }],
            primary_key: None,
            constraints: vec![],
            indexes: vec![],

            partition_by: None,

            runtime_options: None,
            schema: None,
            existence_guard: None,
        }]);
        let err = validate_ir_platform(&ir, Dialect::Postgres)
            .expect_err("a vector_metric on a non-vector column must be refused");
        assert_eq!(err.code, CODE_VECTOR_METRIC_MISPLACED, "got: {err}");
    }

    #[test]
    fn case_sensitive_false_rejects_non_text_columns() {
        for ty in [ColType::Int, ColType::Json] {
            let ir = ir_with(vec![Op::CreateTable {
                name: "docs".into(),
                columns: vec![IrColumn {
                    name: "body".into(),
                    ty,
                    nullable: None,
                    default: None,
                    unique: None,
                    value_format: None,
                    references: None,
                    id_prefix: None,
                    case_sensitive: Some(false),
                    vector_metric: None,
                    mask: None,
                    generated: None,
                    identity: None,
                }],
                primary_key: None,
                constraints: vec![],
                indexes: vec![],
                partition_by: None,
                runtime_options: None,
                schema: None,
                existence_guard: None,
            }]);
            let err = validate_ir_platform(&ir, Dialect::Postgres)
                .expect_err("caseSensitive:false on a non-text column must be refused");
            assert_eq!(err.code, CODE_UNSUPPORTED, "got: {err}");
            assert!(
                err.reason
                    .contains("caseSensitive:false is only valid on a text column"),
                "error should explain the text-only bound: {err}"
            );
        }
    }

    #[test]
    fn container_default_object_on_text_array_is_rejected() {
        let ir = ir_with(vec![create_with_default(
            ColType::TextArray,
            crate::model::ir::IrDefault::Container {
                kind: EmptyContainerKind::Object,
            },
        )]);
        let err = validate_ir_platform(&ir, Dialect::Postgres)
            .expect_err("empty object defaults are valid only on json columns");
        assert_eq!(err.code, CODE_COLUMN_DEFAULT_TYPE, "got: {err}");
        assert!(
            err.reason.contains("empty object defaults require json"),
            "error should explain the allowed type: {err}"
        );
    }

    #[test]
    fn container_default_array_on_int_is_rejected() {
        let ir = ir_with(vec![create_with_default(
            ColType::Int,
            crate::model::ir::IrDefault::Container {
                kind: EmptyContainerKind::Array,
            },
        )]);
        let err = validate_ir_platform(&ir, Dialect::Postgres)
            .expect_err("empty array defaults are valid only on json/textArray columns");
        assert_eq!(err.code, CODE_COLUMN_DEFAULT_TYPE, "got: {err}");
        assert!(
            err.reason
                .contains("empty array defaults require json or textArray"),
            "error should explain the allowed types: {err}"
        );
    }

    #[test]
    fn json_value_default_on_int_is_rejected() {
        let ir = ir_with(vec![create_with_default(
            ColType::Int,
            json_value_default(),
        )]);
        let err = validate_ir_platform(&ir, Dialect::Postgres)
            .expect_err("JSON value defaults are valid only on json columns");
        assert_eq!(err.code, CODE_COLUMN_DEFAULT_TYPE, "got: {err}");
        assert!(
            err.reason
                .contains("JSON value defaults are valid only on json columns"),
            "error should explain the json-only bound: {err}"
        );
    }

    #[test]
    fn json_value_default_on_text_array_is_rejected() {
        let ir = ir_with(vec![create_with_default(
            ColType::TextArray,
            json_value_default(),
        )]);
        let err = validate_ir_platform(&ir, Dialect::Postgres)
            .expect_err("JSON value defaults are valid only on json columns");
        assert_eq!(err.code, CODE_COLUMN_DEFAULT_TYPE, "got: {err}");
        assert!(
            err.reason
                .contains("JSON value defaults are valid only on json columns"),
            "error should explain the json-only bound: {err}"
        );
    }

    #[test]
    fn p2a_create_table_accepts_vector_metric_on_a_vector_column() {
        let ir = ir_with(vec![Op::CreateTable {
            name: "docs".into(),
            columns: vec![IrColumn {
                name: "embedding".into(),
                ty: ColType::Vector { vector: 1536 },
                nullable: None,
                default: None,
                unique: None,
                value_format: None,
                references: None,
                id_prefix: None,
                case_sensitive: None,
                vector_metric: Some(VectorMetric::Cosine),
                mask: None,
                generated: None,
                identity: None,
            }],
            primary_key: None,
            constraints: vec![],
            indexes: vec![],

            partition_by: None,

            runtime_options: None,
            schema: None,
            existence_guard: None,
        }]);
        assert!(
            validate_ir_platform(&ir, Dialect::Postgres).is_ok(),
            "a metric on a t.vector(n) column is the legitimate co-occurrence"
        );
    }

    fn per_row_create_op(ty: ColType, value_format: Option<ValueFormat>) -> Op {
        let columns = vec![
            part_col("cursor", ColType::Int, true),
            IrColumn {
                name: "generated".into(),
                ty,
                nullable: Some(true),
                default: None,
                unique: None,
                value_format,
                references: None,
                id_prefix: None,
                vector_metric: None,
                case_sensitive: None,
                mask: None,
                generated: None,
                identity: None,
            },
        ];
        Op::CreateTable {
            name: "per_row_values".into(),
            columns,
            primary_key: Some(vec!["cursor".into()]),
            constraints: vec![],
            indexes: vec![],
            partition_by: None,
            runtime_options: None,
            schema: None,
            existence_guard: None,
        }
    }

    fn per_row_backfill_op(generator: PerRowGenerator) -> Op {
        Op::Backfill {
            table: "per_row_values".into(),
            cursor_columns: vec!["cursor".into()],
            cursor_stability: crate::model::ir::CursorStability::GuardUpdates,
            batch_size: serde_json::from_str("10").unwrap(),
            set: [("generated".to_string(), BackfillSetValue::from(generator))]
                .into_iter()
                .collect(),
            filter: None,
            name: "generate_values".into(),
            schema: None,
        }
    }

    fn per_row_validation_ir(
        ty: ColType,
        value_format: Option<ValueFormat>,
        generator: PerRowGenerator,
    ) -> MigrationIr {
        ir_with(vec![
            per_row_create_op(ty, value_format),
            per_row_backfill_op(generator),
        ])
    }

    #[test]
    fn per_row_destination_validation_accepts_exact_logical_families() {
        for (ty, format, generator) in [
            (ColType::Uuid, None, PerRowGenerator::UuidV4),
            (ColType::Uuid, None, PerRowGenerator::UuidV7),
            (
                ColType::Text,
                Some(ValueFormat::TypeId {
                    prefix: "order".into(),
                }),
                PerRowGenerator::TypeId {
                    prefix: "order".into(),
                },
            ),
            (
                ColType::Text,
                Some(ValueFormat::Ulid),
                PerRowGenerator::Ulid,
            ),
        ] {
            let ir = per_row_validation_ir(ty, format, generator);
            validate_ir_platform(&ir, Dialect::Sqlite)
                .expect("an exact declared per-row destination family must validate");
        }
    }

    #[test]
    fn per_row_type_id_requires_the_exact_declared_prefix() {
        let ir = per_row_validation_ir(
            ColType::Text,
            Some(ValueFormat::TypeId {
                prefix: "invoice".into(),
            }),
            PerRowGenerator::TypeId {
                prefix: "order".into(),
            },
        );
        let error = validate_ir_platform(&ir, Dialect::Sqlite)
            .expect_err("a mismatched TypeID prefix must fail before lowering");
        assert_eq!(error.code, CODE_OP_INVALID);
        assert!(
            error.reason.contains("stored prefix \"invoice\"")
                && error.reason.contains("exactly \"order\""),
            "got: {error}"
        );
    }

    #[test]
    fn per_row_type_id_and_ulid_never_infer_generic_text() {
        for generator in [
            PerRowGenerator::TypeId {
                prefix: "order".into(),
            },
            PerRowGenerator::Ulid,
        ] {
            let ir = per_row_validation_ir(ColType::Text, None, generator);
            let error = validate_ir_platform(&ir, Dialect::Postgres)
                .expect_err("generic text must not infer a TypeID or ULID contract");
            assert_eq!(error.code, CODE_OP_INVALID);
            assert!(
                error
                    .reason
                    .contains("generic text with no value-format contract"),
                "got: {error}"
            );
        }
    }

    #[test]
    fn per_row_uuid_rejects_text_even_without_a_value_format() {
        let ir = per_row_validation_ir(ColType::Text, None, PerRowGenerator::UuidV7);
        let error = validate_ir_platform(&ir, Dialect::Sqlite)
            .expect_err("a UUID generator requires logical UUID, not text storage");
        assert_eq!(error.code, CODE_OP_INVALID);
        assert!(
            error.reason.contains("logical UUID column")
                && error
                    .reason
                    .contains("generic text with no value-format contract"),
            "got: {error}"
        );
    }

    #[test]
    fn per_row_load_defers_a_missing_cross_artifact_declaration_but_lower_rejects_it() {
        let ir = ir_with(vec![per_row_backfill_op(PerRowGenerator::UuidV4)]);
        validate_ir_platform(&ir, Dialect::Postgres)
            .expect("load cannot know whether an earlier ordered artifact declared the column");
        let error = validate_per_row_destinations_for_lower(
            &ir,
            Dialect::Postgres,
            &[],
            &LogicalColumnContracts::new(),
            "app",
            None,
        )
        .expect_err(
            "strict lower must not replace missing logical metadata with catalog inference",
        );
        assert_eq!(error.code, CODE_OP_INVALID);
        assert!(
            error.reason.contains("no logical column declaration"),
            "got: {error}"
        );
    }

    #[test]
    fn per_row_load_rejects_a_malformed_generator_even_when_its_destination_is_missing() {
        let ir = ir_with(vec![per_row_backfill_op(PerRowGenerator::TypeId {
            prefix: "Not_Canonical".into(),
        })]);
        let error = validate_ir_platform(&ir, Dialect::Postgres)
            .expect_err("missing destination metadata must not defer generator validation");
        assert_eq!(error.code, CODE_INVALID_TYPE_ID_PREFIX);
        assert!(
            error.reason.contains("invalid TypeID prefix"),
            "got: {error}"
        );
    }

    #[test]
    fn per_row_destination_rejects_an_ambiguous_unqualified_declaration() {
        let mut schema_a = per_row_create_op(ColType::Uuid, None);
        let mut schema_b = per_row_create_op(ColType::Uuid, None);
        if let Op::CreateTable { schema, .. } = &mut schema_a {
            *schema = Some("schema_a".into());
        }
        if let Op::CreateTable { schema, .. } = &mut schema_b {
            *schema = Some("schema_b".into());
        }
        let ir = ir_with(vec![
            schema_a,
            schema_b,
            per_row_backfill_op(PerRowGenerator::UuidV4),
        ]);

        let error = validate_ir_platform(&ir, Dialect::Postgres)
            .expect_err("an unqualified target must not guess between declarations");
        assert_eq!(error.code, CODE_OP_INVALID);
        assert!(error.reason.contains("is ambiguous"), "got: {error}");
    }

    #[test]
    fn per_row_destination_tracking_uses_only_the_selected_dialectal_leg() {
        let dialectal_declaration = Op::Dialectal {
            default: Some(vec![per_row_create_op(ColType::Text, None)]),
            pg: Some(vec![per_row_create_op(ColType::Uuid, None)]),
            sqlite: None,
            mysql: None,
        };
        let ir = ir_with(vec![
            dialectal_declaration,
            per_row_backfill_op(PerRowGenerator::UuidV4),
        ]);

        validate_ir_platform(&ir, Dialect::Postgres)
            .expect("the selected PG declaration is logical UUID");
        let error = validate_ir_platform(&ir, Dialect::Sqlite)
            .expect_err("SQLite must use the generic-text default leg, not the PG leg");
        assert!(
            error
                .reason
                .contains("generic text with no value-format contract"),
            "got: {error}"
        );
    }

    fn typed_reference_column(
        name: &str,
        ty: ColType,
        value_format: Option<ValueFormat>,
        case_sensitive: Option<bool>,
        target: Option<(&str, &str)>,
    ) -> IrColumn {
        IrColumn {
            name: name.into(),
            ty,
            nullable: Some(true),
            default: None,
            unique: None,
            value_format,
            references: target.map(|(table, column)| ColumnReference {
                table: table.into(),
                column: column.into(),
                on_delete: None,
                on_update: None,
            }),
            id_prefix: None,
            vector_metric: None,
            case_sensitive,
            mask: None,
            generated: None,
            identity: None,
        }
    }

    fn typed_reference_table(name: &str, column: IrColumn) -> Op {
        Op::CreateTable {
            name: name.into(),
            columns: vec![column],
            primary_key: None,
            constraints: vec![],
            indexes: vec![],
            partition_by: None,
            runtime_options: None,
            schema: None,
            existence_guard: None,
        }
    }

    fn typed_reference_key_table(name: &str, column: IrColumn) -> Op {
        let key = column.name.clone();
        let mut op = typed_reference_table(name, column);
        let Op::CreateTable { primary_key, .. } = &mut op else {
            unreachable!("typed reference test helper always creates a table");
        };
        *primary_key = Some(vec![key]);
        op
    }

    #[test]
    fn typed_reference_load_sees_a_target_declared_later_in_the_artifact() {
        let child = typed_reference_table(
            "memberships",
            typed_reference_column(
                "account_id",
                ColType::Uuid,
                None,
                None,
                Some(("accounts", "id")),
            ),
        );
        let target = typed_reference_key_table(
            "accounts",
            typed_reference_column("id", ColType::Uuid, None, None, None),
        );
        let ir = ir_with(vec![child, target]);

        for dialect in [Dialect::Postgres, Dialect::Mysql, Dialect::Sqlite] {
            validate_ir_platform(&ir, dialect).unwrap_or_else(|error| {
                panic!("forward target must validate on {dialect:?}: {error}")
            });
        }
    }

    #[test]
    fn typed_reference_rejects_logical_integer_width_mismatch_on_sqlite() {
        let child = typed_reference_table(
            "events",
            typed_reference_column(
                "account_id",
                ColType::Int,
                None,
                None,
                Some(("accounts", "id")),
            ),
        );
        let target = typed_reference_key_table(
            "accounts",
            typed_reference_column("id", ColType::BigInt, None, None, None),
        );
        let error = validate_ir_platform(&ir_with(vec![child, target]), Dialect::Sqlite)
            .expect_err("SQLite INTEGER lowering must not erase int-vs-bigInt width");
        assert!(
            error.reason.contains("logical integer width differs"),
            "got: {error}"
        );
    }

    #[test]
    fn typed_reference_rejects_type_id_prefix_and_ulid_format_mismatches() {
        let type_id_child = typed_reference_table(
            "memberships",
            typed_reference_column(
                "account_id",
                ColType::Text,
                Some(ValueFormat::TypeId {
                    prefix: "account".into(),
                }),
                None,
                Some(("accounts", "id")),
            ),
        );
        let type_id_target = typed_reference_key_table(
            "accounts",
            typed_reference_column(
                "id",
                ColType::Text,
                Some(ValueFormat::TypeId {
                    prefix: "acct".into(),
                }),
                None,
                None,
            ),
        );
        let type_id_error = validate_ir_platform(
            &ir_with(vec![type_id_child, type_id_target]),
            Dialect::Postgres,
        )
        .expect_err("different stored TypeID prefixes must fail closed");
        assert!(
            type_id_error.reason.contains("value formats differ")
                && type_id_error.reason.contains("account")
                && type_id_error.reason.contains("acct"),
            "got: {type_id_error}"
        );

        let ulid_child = typed_reference_table(
            "memberships",
            typed_reference_column(
                "account_id",
                ColType::Text,
                Some(ValueFormat::Ulid),
                None,
                Some(("accounts", "id")),
            ),
        );
        let plain_target = typed_reference_key_table(
            "accounts",
            typed_reference_column("id", ColType::Text, None, None, None),
        );
        let ulid_error =
            validate_ir_platform(&ir_with(vec![ulid_child, plain_target]), Dialect::Mysql)
                .expect_err("ULID references must target the same exact value format");
        assert!(
            ulid_error.reason.contains("value formats differ")
                && ulid_error.reason.contains("ULID"),
            "got: {ulid_error}"
        );
    }

    #[test]
    fn typed_reference_rejects_collation_intent_mismatch() {
        let child = typed_reference_table(
            "memberships",
            typed_reference_column(
                "account_name",
                ColType::Text,
                None,
                Some(false),
                Some(("accounts", "name")),
            ),
        );
        let target = typed_reference_key_table(
            "accounts",
            typed_reference_column("name", ColType::Text, None, None, None),
        );
        let error = validate_ir_platform(&ir_with(vec![child, target]), Dialect::Postgres)
            .expect_err("reference collations must match exactly");
        assert!(
            error.reason.contains("collation intent differs"),
            "got: {error}"
        );
    }

    #[test]
    fn strict_reference_validation_rejects_unmanaged_format_but_defers_primitive() {
        let formatted = ir_with(vec![typed_reference_table(
            "memberships",
            typed_reference_column(
                "account_id",
                ColType::Text,
                Some(ValueFormat::TypeId {
                    prefix: "account".into(),
                }),
                None,
                Some(("accounts", "id")),
            ),
        )]);
        validate_ir_platform(&formatted, Dialect::Postgres)
            .expect("load defers a target that may live in an earlier artifact");
        let error = validate_column_references_for_lower(
            &formatted,
            Dialect::Postgres,
            &[],
            &LogicalColumnContracts::new(),
            "app",
            None,
        )
        .expect_err("a catalog cannot invent missing TypeID metadata");
        assert!(
            error.reason.contains("no authored value-format metadata"),
            "got: {error}"
        );

        let primitive = ir_with(vec![typed_reference_table(
            "memberships",
            typed_reference_column(
                "account_id",
                ColType::Int,
                None,
                None,
                Some(("accounts", "id")),
            ),
        )]);
        validate_column_references_for_lower(
            &primitive,
            Dialect::Sqlite,
            &[],
            &LogicalColumnContracts::new(),
            "app",
            None,
        )
        .expect("a missing primitive target is left for physical catalog validation");
    }
}
