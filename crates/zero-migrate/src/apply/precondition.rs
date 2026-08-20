//! Preconditions — state/data-conditional apply.
//!
//! A migration may carry **preconditions**: assertions evaluated against the
//! live DB *before* its `up` runs, gating whether the migration applies. This is
//! the engine's "only run this if the database is in shape X" primitive — the
//! peer of Liquibase `<preConditions>`.
//!
//! # Two evaluation paths, by trust
//!
//! - **Structured checks** ([`Precondition::TableExists`],
//! [`Precondition::ColumnExists`], [`Precondition::RowCount`], …) are
//! ENGINE-BUILT, fully parameterized catalog queries
//! (`information_schema` / `pg_catalog`). The project schema is bound as `$1`;
//! the table/column identifiers are validated with `validate_ident` (bare
//! `[A-Za-z_][A-Za-z0-9_]*` only — no schema qualifier, no quotes, no
//! punctuation) and then bound as parameters too. There is **no string
//! interpolation of user input into SQL**, so these are injection-safe by
//! construction.
//! - **[`Precondition::SqlBoolean`]** is UNTRUSTED creator/AI SQL. It is the
//! escape hatch for assertions the structured checks cannot express, and it is
//! confined three ways before it is allowed to run:
//! 1. it MUST pass the [`SqlGuard`] (read-only SELECT;
//! a cross-schema / file / network / dangerous precondition is denied — the
//! same line-1 defense the `up` gets);
//! 2. it MUST pass the **shape gate** (`validate_single_select`): a SINGLE
//! `SELECT` (no second statement, no non-SELECT) with NO data-modifying
//! statement (`INSERT`/`UPDATE`/`DELETE`/`MERGE`) anywhere in its tree
//! (catches a data-modifying CTE at any nesting), NO locking clause (`FOR
//! UPDATE`/`FOR SHARE`/…), and NO sequence-mutating or advisory-lock builtin
//! (`nextval`/`setval`/`pg_advisory_*lock`/…). This gate is the REAL
//! pre-execution line: it is statically provable that a precondition can
//! **NEVER mutate state or acquire a leaking lock — it is rejected before it
//! touches the DB**. It returns exactly one boolean column (verified
//! result-side at execution);
//! 3. it runs under the least-privilege **`migrator` role** (`SET LOCAL ROLE`,
//! transaction-scoped) inside a **`BEGIN READ ONLY`** transaction — the same
//! line-2 DB-privilege confinement the `up` gets. These are
//! DEFENSE-IN-DEPTH: the shape gate (step 2) already proves no mutation, but
//! `READ ONLY` + the migrator role backstop it. (Note `READ ONLY` does NOT
//! block advisory locks or sequence mutation — which is exactly why the
//! shape gate, not `READ ONLY`, owns that confinement.)
//!
//! # Where this is evaluated — the backend seam (multi-engine abstraction)
//!
//! [`crate::apply::executor::apply`] evaluates a pending migration's preconditions
//! **inside the apply flow, under the project advisory lock**, immediately before
//! the migration's `up` — so the state a precondition checks is stable for the
//! apply. All evaluation is read-only (catalog reads + a single read-only
//! `SELECT`); a precondition can never write.
//!
//! **This module is the POSTGRES precondition impl.** Both the SQL VALIDATION
//! (`validate_single_select`, `pg_query::parse` — "is this a safe single
//! SELECT?") and the EVALUATION ([`evaluate`], `information_schema` catalog reads +
//! the `&Client`-bound `SqlBoolean` run) are PG-dialect-specific, so they live
//! BEHIND the [`MigrationBackend`] seam: the
//! generic apply path calls
//! [`backend.evaluate_preconditions`](crate::apply::backend::MigrationBackend::evaluate_preconditions),
//! and only [`PostgresBackend`] routes into this
//! module (via `evaluate_all`, which folds the per-check verdict loop). The
//! SQLite backend validates/evaluates in its own dialect (descriptor migrations
//! carry no preconditions, so it fails closed on a declared one). No `pg_query` /
//! `information_schema` / `&Client` appears in the generic executor body — it is
//! all contained here, the PG leaf.

#[cfg(pg_seam)]
use crate::apply::backend::{MigrationBackend, PostgresBackend};
#[cfg(pg_seam)]
use crate::driver::SqlSession;
use pg_query::protobuf::node::Node as NodeEnum;
use serde_json::Value;

use crate::analysis::tree_walk::first_dml_node;
use crate::apply::executor::{ApplyError, PreconditionVerdict};
use crate::conn::ExecutorConfig;
use crate::guard::{GuardError, SqlGuard};
use crate::model::migration::Migration;
use crate::model::precondition::{OnUnmet, Precondition};

/// Sequence-mutating and lock-acquiring builtins a read-only precondition may
/// NEVER call. `nextval`/`setval` mutate a sequence (NOT blocked by `READ
/// ONLY`); the `pg_advisory_*lock`/`unlock` family acquire/release advisory
/// locks (also NOT blocked by `READ ONLY`) — a SESSION-scoped advisory lock
/// taken in a precondition LEAKS onto the pooled connection and can collide with
/// the engine's own apply lock. `currval`/`lastval` are read-only (they only
/// read the session's last value), so they are deliberately ABSENT here. Matched
/// case-insensitively against the trailing (bare) function-name part, so a
/// `pg_catalog.`-qualified spelling resolves to the same builtin.
const MUTATING_OR_LOCK_BUILTINS: &[&str] = &[
    "nextval",
    "setval",
    "pg_advisory_lock",
    "pg_advisory_lock_shared",
    "pg_advisory_xact_lock",
    "pg_advisory_xact_lock_shared",
    "pg_advisory_unlock",
    "pg_advisory_unlock_all",
    "pg_try_advisory_lock",
    "pg_try_advisory_lock_shared",
    "pg_try_advisory_xact_lock",
    "pg_try_advisory_xact_lock_shared",
];

/// Error evaluating a precondition.
#[derive(Debug, thiserror::Error)]
pub enum PreconditionError {
    /// A database error while running a (structured or `SqlBoolean`) check.
    #[error("precondition db error: {0}")]
    #[cfg(pg_seam)]
    Db(#[from] crate::driver::DbError),
    /// A structured check named an identifier that is not a bare SQL identifier
    /// (`[A-Za-z_][A-Za-z0-9_]*`) — a schema-qualified name, a quoted-injection
    /// attempt, whitespace, or punctuation. Rejected before any query runs.
    #[error("precondition has an invalid {what} identifier: {value:?}")]
    InvalidIdentifier {
        /// What was being validated (`"table"` / `"column"`).
        what: &'static str,
        /// The offending value.
        value: String,
    },
    /// PostgreSQL could not describe the objects that block a bare column drop, or
    /// the ones that block a column retype. This query is part of the structured
    /// assertion, so failure to evaluate it must fail closed instead of treating
    /// the column as safe to change.
    #[error("precondition blocking-column dependency query failed: {0}")]
    BlockingColumnDependents(#[source] Box<ApplyError>),
    /// A [`Precondition::SqlBoolean`]'s SQL was denied by the guard (cross-schema
    /// / file / network / dangerous / unparseable). Same line-1 defense as `up`.
    #[error("SqlBoolean precondition denied by guard: {0}")]
    Guard(#[from] GuardError),
    /// A [`Precondition::SqlBoolean`]'s SQL is not a single read-only `SELECT`
    /// returning exactly one boolean column (multi-statement, non-SELECT, or the
    /// wrong result shape). Rejected before it touches the DB so a precondition
    /// can never mutate state.
    #[error("SqlBoolean precondition is not a single boolean-returning SELECT: {reason}")]
    NotABooleanSelect {
        /// What specifically is wrong with the shape.
        reason: String,
    },
    /// An engine-supplied identifier (project schema / migrator role) was not
    /// quotable (empty or NUL-bearing) at a render seam — fail-closed rather than
    /// interpolate it. Maps [`crate::render::dml::IdentQuoteError`].
    #[error("precondition: {0}")]
    IdentQuote(#[from] crate::render::dml::IdentQuoteError),
}

/// Validate a bare SQL identifier (mirrors `backfill::validate_ident`): non-empty,
/// starts with a letter/underscore, only `[A-Za-z0-9_]`. Rejects schema-qualified
/// names, quoted-injection, whitespace, punctuation — so the value is safe even
/// though structured checks BIND it (never interpolate it) into the catalog query.
fn validate_ident(what: &'static str, value: &str) -> Result<(), PreconditionError> {
    let mut chars = value.chars();
    let ok_first = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_');
    let ok_rest = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if value.is_empty() || !ok_first || !ok_rest {
        return Err(PreconditionError::InvalidIdentifier {
            what,
            value: value.to_string(),
        });
    }
    Ok(())
}

/// Double-quote a validated identifier (belt-and-suspenders; the value has passed
/// [`validate_ident`] so it has no `"`).
///
/// PostgreSQL, named rather than assumed: the only caller is the `#[cfg(pg_seam)]`
/// `row_count` probe, which is a PG-only path. It used to reach the crate's raw
/// escape primitive, which produced the right bytes for no stated dialect.
fn quote_ident(ident: &str) -> String {
    crate::render::dml::escape_quote_ident_for_dialect(
        ident,
        crate::schema::query::SqlDialect::Postgres,
    )
}

/// Evaluate a precondition against the live DB. Returns `true` if the assertion
/// HOLDS (the migration may apply), `false` if it is UNMET (the executor then
/// applies the [`OnUnmet`] policy).
///
/// All evaluation is **read-only**: structured checks are parameterized catalog
/// reads; [`Precondition::SqlBoolean`] is gated to a no-mutation, no-lock single
/// `SELECT` by `validate_single_select` (the real pre-execution line) and then
/// run under the migrator role inside a `READ ONLY` transaction
/// (defense-in-depth). A precondition can never mutate state or acquire a leaking
/// lock — it is rejected before it touches the DB.
///
/// # Errors
/// - [`PreconditionError::InvalidIdentifier`] — a structured check's table/column
/// is not a bare identifier.
/// - [`PreconditionError::BlockingColumnDependents`] - PostgreSQL could not query
/// the objects that block a bare column drop, or the ones that block a retype.
/// - [`PreconditionError::Guard`] — a `SqlBoolean` was guard-denied.
/// - [`PreconditionError::NotABooleanSelect`] — a `SqlBoolean` is not a single
/// boolean-returning `SELECT`.
/// - [`PreconditionError::Db`] — a query failed.
#[cfg(pg_seam)]
pub async fn evaluate<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    pre: &Precondition,
) -> Result<bool, PreconditionError> {
    match pre {
        Precondition::TableExists { table } => {
            validate_ident("table", table)?;
            table_exists(conn, cfg, table).await
        }
        Precondition::TableNotExists { table } => {
            validate_ident("table", table)?;
            Ok(!table_exists(conn, cfg, table).await?)
        }
        Precondition::ColumnExists { table, column } => {
            validate_ident("table", table)?;
            validate_ident("column", column)?;
            column_exists(conn, cfg, table, column).await
        }
        Precondition::ColumnNotExists { table, column } => {
            validate_ident("table", table)?;
            validate_ident("column", column)?;
            Ok(!column_exists(conn, cfg, table, column).await?)
        }
        Precondition::ColumnHasNoBlockingDependents { table, column } => {
            Ok(drop_column_blockers(conn, cfg, table, column)
                .await?
                .is_empty())
        }
        Precondition::ColumnTypeChangeHasNoBlockers { table, column } => {
            Ok(retype_column_blockers(conn, cfg, table, column)
                .await?
                .is_empty())
        }
        Precondition::RowCount { table, op, value } => {
            validate_ident("table", table)?;
            let n = row_count(conn, cfg, table).await?;
            Ok(op.apply(n, *value))
        }
        Precondition::SqlBoolean { sql } => evaluate_sql_boolean(conn, cfg, sql).await,
    }
}

/// Evaluate ALL of a migration's preconditions, in declaration order,
/// BEFORE its `up` runs — the **Postgres** precondition path behind
/// [`PostgresBackend::evaluate_preconditions`](crate::apply::backend::PostgresBackend),
/// reached only via the backend seam (multi-engine abstraction). Folds the
/// former generic-executor `evaluate_preconditions` loop into the PG leaf, so the
/// `&Client` + per-check `pg_query`/`information_schema` evaluation never sits in
/// the dialect-agnostic executor body.
///
/// All evaluation is read-only (parameterized catalog reads + a guarded single
/// read-only `SELECT` under the migrator role).
///
/// Returns:
/// - [`PreconditionVerdict::AllMet`] — every precondition held; apply.
/// - [`PreconditionVerdict::Skip`] — an `OnUnmet::Skip` check was unmet; skip.
///
/// # Errors
/// - [`ApplyError::PreconditionFailed`] — an `OnUnmet::Halt` check was UNMET (the
/// assertion evaluated false), or ANY check could not be evaluated (a
/// guard-denied / malformed `SqlBoolean`, an invalid identifier). Fail-closed:
/// an inevaluable precondition is treated as a hard failure regardless of its
/// `on_unmet`, so a precondition that cannot even be checked never silently
/// waves a migration through. The caller aborts the whole apply, applying
/// nothing for this migration.
///
/// `Halt` is evaluated first-failure-wins: the first unmet/inevaluable Halt check
/// stops evaluation and aborts. A `Skip` verdict is returned only when no Halt
/// check failed and at least one `Skip` check is unmet.
#[cfg(pg_seam)]
pub(crate) async fn evaluate_all<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    m: &Migration,
) -> Result<PreconditionVerdict, ApplyError> {
    let mut verdict = PreconditionVerdict::AllMet;
    for pc in &m.preconditions {
        let (met, blockers) = evaluate_one(conn, cfg, m.version.as_str(), &pc.check).await?;
        if met {
            continue;
        }
        match pc.on_unmet {
            OnUnmet::Halt => {
                return Err(unmet_halt_error(
                    m.version.as_str(),
                    &pc.check,
                    blockers.as_deref(),
                ));
            }
            OnUnmet::Skip => {
                // Record that we will skip, but keep scanning: a LATER Halt check
                // that is also unmet must still fail-close (Halt dominates Skip).
                verdict = PreconditionVerdict::Skip;
            }
        }
    }
    Ok(verdict)
}

/// Evaluate ONE precondition, returning both the verdict and - for the two
/// blocked-column assertions - the object descriptions the refusal names.
///
/// The shared body behind BOTH seams that ask this question: the per-migration
/// [`evaluate_all`] and the plan-wide preflight
/// ([`crate::apply::plan_precondition`], via
/// `MigrationBackend::evaluate_plan_precondition`). One body so the two can never
/// come to different conclusions, or word the same refusal differently.
///
/// An evaluation ERROR (guard denial, invalid identifier, malformed `SqlBoolean`,
/// DB error) is ALWAYS fatal - fail-closed, regardless of `on_unmet`. A
/// precondition that cannot be checked must never wave a migration through.
///
/// The blocked-column assertions need the returned object descriptions after
/// evaluation, not only a boolean, so their refusal can name what PostgreSQL says
/// depends on the column; projecting them through [`evaluate`] would discard the
/// reason for refusal. The two ask DIFFERENT questions of the catalog - see
/// [`Precondition::ColumnTypeChangeHasNoBlockers`].
///
/// # Errors
/// [`ApplyError::PreconditionFailed`] when the check cannot be evaluated.
#[cfg(pg_seam)]
pub(crate) async fn evaluate_one<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    version: &str,
    check: &Precondition,
) -> Result<(bool, Option<Vec<String>>), ApplyError> {
    let inevaluable = |e: PreconditionError| ApplyError::PreconditionFailed {
        version: version.to_string(),
        which: format!("{check:?} could not be evaluated: {e}"),
    };
    match check {
        Precondition::ColumnHasNoBlockingDependents { table, column } => {
            let blockers = drop_column_blockers(conn, cfg, table, column)
                .await
                .map_err(inevaluable)?;
            Ok((blockers.is_empty(), Some(blockers)))
        }
        Precondition::ColumnTypeChangeHasNoBlockers { table, column } => {
            let blockers = retype_column_blockers(conn, cfg, table, column)
                .await
                .map_err(inevaluable)?;
            Ok((blockers.is_empty(), Some(blockers)))
        }
        _ => {
            let met = evaluate(conn, cfg, check).await.map_err(inevaluable)?;
            Ok((met, None))
        }
    }
}

/// The refusal an unmet [`OnUnmet::Halt`] check produces.
///
/// Shared by the per-migration seam and the plan-wide preflight so that moving
/// WHEN a refusal fires never changes WHAT the operator reads.
pub(crate) fn unmet_halt_error(
    version: &str,
    check: &Precondition,
    blockers: Option<&[String]>,
) -> ApplyError {
    let blocker_detail = blockers.map_or_else(String::new, |blockers| {
        format!(": blocking dependents {blockers:?}")
    });
    ApplyError::PreconditionFailed {
        version: version.to_string(),
        which: format!("{check:?} is unmet (OnUnmet::Halt){blocker_detail}"),
    }
}

/// Ask the PostgreSQL backend's measured dependency predicate which objects block
/// a bare drop. Keeping this as a backend call avoids a third SQL spelling whose
/// behavior could drift independently from the rename guard and its live oracle.
#[cfg(pg_seam)]
async fn drop_column_blockers<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    table: &str,
    column: &str,
) -> Result<Vec<String>, PreconditionError> {
    validate_ident("table", table)?;
    validate_ident("column", column)?;
    PostgresBackend::new_generic(conn)
        .blocking_column_dependents(cfg, table, column)
        .await
        .map_err(|error| PreconditionError::BlockingColumnDependents(Box::new(error)))
}

/// Ask the PostgreSQL backend's measured dependency predicate what would make the
/// server refuse an `ALTER COLUMN … TYPE`. A backend call for the same reason the
/// drop question is one: a second SQL spelling here could drift away from the live
/// oracle that certifies it.
///
/// Deliberately a DIFFERENT predicate from [`drop_column_blockers`]. The two
/// disagree in both directions on shapes measured against a live server, so
/// routing this variant through the drop question would refuse retypes PostgreSQL
/// accepts (a column an inbound FOREIGN KEY names) and admit retypes it rejects (a
/// partition-key column, an inherited column).
#[cfg(pg_seam)]
async fn retype_column_blockers<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    table: &str,
    column: &str,
) -> Result<Vec<String>, PreconditionError> {
    validate_ident("table", table)?;
    validate_ident("column", column)?;
    PostgresBackend::new_generic(conn)
        .column_type_change_blockers(cfg, table, column)
        .await
        .map_err(|error| PreconditionError::BlockingColumnDependents(Box::new(error)))
}

/// `information_schema.tables` lookup: a base table OR a view named `table` in the
/// project schema. Schema + table are BOUND ($1/$2), never interpolated.
#[cfg(pg_seam)]
async fn table_exists<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    table: &str,
) -> Result<bool, PreconditionError> {
    let row = conn
        .query_one(
            "SELECT EXISTS (
                 SELECT 1 FROM information_schema.tables
                  WHERE table_schema = $1 AND table_name = $2
             ) AS present",
            &[(&cfg.project_schema).into(), table.into()],
        )
        .await?;
    Ok(row.try_get("present")?)
}

/// `information_schema.columns` lookup: a column named `column` on the
/// project-schema table `table`. All three are BOUND, never interpolated.
#[cfg(pg_seam)]
async fn column_exists<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    table: &str,
    column: &str,
) -> Result<bool, PreconditionError> {
    let row = conn
        .query_one(
            "SELECT EXISTS (
                 SELECT 1 FROM information_schema.columns
                  WHERE table_schema = $1 AND table_name = $2 AND column_name = $3
             ) AS present",
            &[(&cfg.project_schema).into(), table.into(), column.into()],
        )
        .await?;
    Ok(row.try_get("present")?)
}

/// `count(*)` of the project-schema table `table`.
///
/// The schema + table are validated as bare identifiers then double-quoted into a
/// fully-qualified relation reference. They CANNOT be bound (a relation name is
/// not a bind parameter in Postgres), so the injection-safety here rests on
/// [`validate_ident`] (the schema is the engine-owned `cfg.project_schema`, the
/// table has passed `validate_ident`). This mirrors `backfill::build_batch_sql`,
/// which quotes the same validated identifiers into relation position.
#[cfg(pg_seam)]
async fn row_count<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    table: &str,
) -> Result<i64, PreconditionError> {
    let stmt = format!(
        "SELECT count(*)::bigint AS n FROM {}.{}",
        quote_ident(&cfg.project_schema),
        quote_ident(table),
    );
    let row = conn.query_one(&stmt, &[]).await?;
    Ok(row.try_get("n")?)
}

/// Statically validate that a `SqlBoolean`'s SQL is a no-mutation, no-lock
/// SELECT BEFORE it touches the DB — so a precondition can NEVER run DML/DDL,
/// smuggle a second statement, acquire a lock, or mutate a sequence. This gate
/// (not the `READ ONLY` transaction) is the REAL pre-execution confinement line;
/// `READ ONLY` + the migrator role are defense-in-depth on top of it.
///
/// Rejects (all as [`PreconditionError::NotABooleanSelect`], before any query
/// runs):
/// - more than one statement, or a single statement that is not a `SELECT`;
/// - a data-modifying statement (`INSERT`/`UPDATE`/`DELETE`/`MERGE`) ANYWHERE
/// in the parsed tree — a data-modifying CTE
/// (`WITH x AS (DELETE … RETURNING …) SELECT …`) hangs its `DeleteStmt` off
/// the `SelectStmt`'s `with_clause`, so a top-node check alone would miss it. We
/// walk the whole serialized tree (the same `serde_json` approach the guard
/// uses) and reject any DML node at any nesting;
/// - a non-empty locking clause (`FOR UPDATE`/`FOR SHARE`/`FOR NO KEY
/// UPDATE`/`FOR KEY SHARE`) — a `LockingClause` node acquires row locks, not
/// a read-only assertion;
/// - a sequence-mutating or advisory-lock builtin
/// ([`MUTATING_OR_LOCK_BUILTINS`]) — `READ ONLY` does NOT block these, and a
/// session-scoped advisory lock would leak onto the pooled connection.
///
/// Shape (single boolean column) is enforced at execution by reading exactly one
/// `bool` column from the single result row. Parse failure is rejected
/// (deny-by-default, though the guard's parse already covers it).
fn validate_single_select(sql: &str) -> Result<(), PreconditionError> {
    let parsed = pg_query::parse(sql).map_err(|e| PreconditionError::NotABooleanSelect {
        reason: format!("could not parse SQL: {e}"),
    })?;
    let raw_stmts = &parsed.protobuf.stmts;
    let stmts: Vec<_> = raw_stmts
        .iter()
        .filter_map(|s| s.stmt.as_ref().and_then(|n| n.node.as_ref()))
        .collect();
    if stmts.len() != 1 {
        return Err(PreconditionError::NotABooleanSelect {
            reason: format!("expected exactly one statement, found {}", stmts.len()),
        });
    }
    if !matches!(stmts[0], NodeEnum::SelectStmt(_)) {
        return Err(PreconditionError::NotABooleanSelect {
            reason: "the single statement is not a SELECT".to_string(),
        });
    }

    // Serialize the single statement's full subtree for the generic tree-walks
    // (the same pattern the SqlGuard uses): this visits EVERY node — CTE bodies,
    // sub-selects, expression args — not just the slots a hand-written traversal
    // would reach.
    let json = serde_json::to_value(&raw_stmts[0]).unwrap_or(Value::Null);

    // (a) No data-modifying statement anywhere (catches data-modifying CTEs).
    if let Some(kind) = first_dml_node(&json) {
        return Err(PreconditionError::NotABooleanSelect {
            reason: format!(
                "contains a data-modifying statement ({kind}) — a precondition must be read-only"
            ),
        });
    }

    // (b) No locking clause (`FOR UPDATE`/`FOR SHARE`/…).
    if tree_has_key(&json, "LockingClause") {
        return Err(PreconditionError::NotABooleanSelect {
            reason:
                "contains a locking clause (FOR UPDATE/SHARE) — a precondition must be lock-free"
                    .to_string(),
        });
    }

    // (c) No sequence-mutating / advisory-lock builtin.
    if let Some(name) = first_mutating_or_lock_builtin(&json) {
        return Err(PreconditionError::NotABooleanSelect {
            reason: format!("calls a mutating/lock builtin ({name}) — not blocked by READ ONLY, so denied at the gate"),
        });
    }

    Ok(())
}

/// True if any object node in the serialized tree carries `key` (used to detect a
/// `LockingClause` node anywhere — top-level or in a sub-select).
fn tree_has_key(v: &Value, key: &str) -> bool {
    match v {
        Value::Object(map) => map.contains_key(key) || map.values().any(|c| tree_has_key(c, key)),
        Value::Array(items) => items.iter().any(|i| tree_has_key(i, key)),
        _ => false,
    }
}

/// The first [`MUTATING_OR_LOCK_BUILTINS`] function name called anywhere in the
/// serialized tree, or `None`. Reuses the guard's `funcname`-array convention:
/// a `FuncCall`/`CallStmt` carries a `funcname` array of `String` nodes whose
/// trailing element is the bare function name.
fn first_mutating_or_lock_builtin(v: &Value) -> Option<String> {
    match v {
        Value::Object(map) => {
            if let Some(Value::Array(parts)) = map.get("funcname") {
                if let Some(name) = last_string_part(parts) {
                    if MUTATING_OR_LOCK_BUILTINS
                        .iter()
                        .any(|b| b.eq_ignore_ascii_case(&name))
                    {
                        return Some(name);
                    }
                }
            }
            map.values().find_map(first_mutating_or_lock_builtin)
        }
        Value::Array(items) => items.iter().find_map(first_mutating_or_lock_builtin),
        _ => None,
    }
}

/// The trailing `{"node":{"String":{"sval":"…"}}}` value of a `funcname` array
/// (the bare function name), mirroring the guard's `json_last_string_part`.
fn last_string_part(parts: &[Value]) -> Option<String> {
    parts.iter().rev().find_map(|v| {
        v.get("node")?
            .get("String")?
            .get("sval")?
            .as_str()
            .map(str::to_string)
    })
}

/// Evaluate an untrusted `SqlBoolean` precondition: guard → shape gate → run
/// under the migrator role in a READ ONLY transaction → read one boolean.
#[cfg(pg_seam)]
async fn evaluate_sql_boolean<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    sql: &str,
) -> Result<bool, PreconditionError> {
    // 1. Line-1 guard: the SAME deny-list + cross-schema confinement the `up`
    // gets. A precondition reaching `control.*` / a file/network func / a
    // dangerous construct is denied here, before anything runs.
    // Trust-aware. Confined ⇒ `confined(project_schema)`;
    // Platform ⇒ the operator allowlist. Latent for the port (the loader sets
    // `preconditions = []`), but threaded so it is correct the day a platform
    // precondition is written.
    let guard = SqlGuard::new(cfg.guard_config());
    guard.check(sql)?;

    // 2. Shape gate (THE pre-execution line): a single SELECT with no DML
    // anywhere (incl. data-modifying CTEs), no locking clause, and no
    // sequence-mutating / advisory-lock builtin. After this gate it is
    // statically provable the SQL cannot mutate state or take a leaking lock —
    // before it touches the DB.
    validate_single_select(sql)?;

    // 3. Run read-only under the least-privilege migrator role — DEFENSE-IN-DEPTH
    // on top of the shape gate (step 2 already proves no mutation). A READ ONLY
    // transaction backstops any write; the migrator role backstops privilege.
    // NOTE READ ONLY does NOT block advisory locks or sequence mutation —
    // those are denied by the shape gate above, which is why the gate (not
    // READ ONLY) is the real line. `SET LOCAL` (search_path/role) is
    // transaction-scoped, so it vanishes at COMMIT and never leaks onto the
    // session — the same discipline the executor uses.
    conn.batch("BEGIN READ ONLY").await?;
    let result = run_sql_boolean_in_txn(conn, cfg, sql).await;
    // Always end the transaction. A read-only txn has nothing to persist, so we
    // COMMIT on success and ROLLBACK on error (both end the txn + revert the
    // SET LOCALs); failures here are surfaced behind the primary result.
    match &result {
        Ok(_) => {
            conn.batch("COMMIT").await?;
        }
        Err(_) => {
            if let Err(rb) = conn.batch("ROLLBACK").await {
                tracing::warn!(error = %rb, "zero-migrate: ROLLBACK failed after a SqlBoolean precondition error");
            }
        }
    }
    result
}

/// The body of [`evaluate_sql_boolean`] run INSIDE the `BEGIN READ ONLY`
/// transaction: pin the project `search_path`, drop to the migrator role (both
/// `SET LOCAL`, transaction-scoped), then run the `SELECT` and read one boolean.
#[cfg(pg_seam)]
async fn run_sql_boolean_in_txn<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    sql: &str,
) -> Result<bool, PreconditionError> {
    // Pin the project schema (and only it) on the path, scoped to this txn.
    // Routed through the ONE engine seam so it fails closed on an empty/NUL
    // schema, byte-identical to the prior `escape_quote_ident` on every real
    // (quote-free / `-`-bearing UUIDv7) schema.
    let schema_q = crate::render::dml::quote_ident_checked(&cfg.project_schema)?;
    conn.batch(&format!("SET LOCAL search_path TO {schema_q}"))
        .await?;
    // Drop to the migrator role for the read, scoped to this txn (line-2). No
    // role configured (tests / single-tenant dev) runs as the connecting role.
    if let Some(role) = &cfg.pg.migrator_role {
        let role_q = crate::render::dml::quote_ident_checked(role)?;
        conn.batch(&format!("SET LOCAL ROLE {role_q}")).await?;
    }
    let row = conn.query_one(sql, &[]).await?;
    // Shape gate (result side): exactly one column, of boolean type. `try_get`
    // surfaces a wrong column count / non-boolean type as an error we map to
    // NotABooleanSelect rather than a raw Db error.
    if row.len() != 1 {
        return Err(PreconditionError::NotABooleanSelect {
            reason: format!("SELECT returned {} columns, expected exactly 1", row.len()),
        });
    }
    row.try_get::<_, bool>(0).map_or_else(
        |_| {
            Err(PreconditionError::NotABooleanSelect {
                reason: "the single column is not a boolean".to_string(),
            })
        },
        Ok,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::precondition::{CmpOp, PreconditionCheck};

    #[test]
    fn cmp_op_applies() {
        assert!(CmpOp::Eq.apply(3, 3));
        assert!(!CmpOp::Eq.apply(3, 4));
        assert!(CmpOp::Ne.apply(3, 4));
        assert!(CmpOp::Lt.apply(2, 3));
        assert!(CmpOp::Le.apply(3, 3));
        assert!(CmpOp::Gt.apply(4, 3));
        assert!(CmpOp::Ge.apply(3, 3));
        assert!(!CmpOp::Gt.apply(3, 3));
    }

    #[test]
    fn on_unmet_defaults_to_halt() {
        assert_eq!(OnUnmet::default(), OnUnmet::Halt);
        // serde default for the struct field, too.
        let c: PreconditionCheck =
            serde_json::from_str(r#"{"check":{"TableExists":{"table":"t"}}}"#).unwrap();
        assert_eq!(c.on_unmet, OnUnmet::Halt);
    }

    #[test]
    fn validate_ident_rejects_injection() {
        assert!(validate_ident("table", "users").is_ok());
        assert!(validate_ident("table", "_x9").is_ok());
        assert!(validate_ident("table", "").is_err());
        assert!(validate_ident("table", "control.users").is_err());
        assert!(validate_ident("table", "t\"; DROP TABLE x; --").is_err());
        assert!(validate_ident("table", "9abc").is_err());
        assert!(validate_ident("column", "a b").is_err());
    }

    #[test]
    fn single_select_shape_gate() {
        assert!(validate_single_select("SELECT true").is_ok());
        assert!(validate_single_select("SELECT count(*) = 0 FROM t").is_ok());
        // Multi-statement.
        assert!(validate_single_select("SELECT true; SELECT false").is_err());
        // Non-SELECT.
        assert!(validate_single_select("DELETE FROM t").is_err());
        assert!(validate_single_select("UPDATE t SET x = 1").is_err());
        assert!(validate_single_select("DROP TABLE t").is_err());
        // A SELECT smuggling a second DML statement.
        assert!(validate_single_select("SELECT true; DELETE FROM t").is_err());
    }

    #[test]
    fn shape_gate_rejects_data_modifying_cte() {
        // The DeleteStmt hangs off the SelectStmt's with_clause — a top-node-only
        // gate would miss it; the tree walk catches it.
        assert!(validate_single_select(
            "WITH x AS (DELETE FROM t RETURNING 1) SELECT count(*)=0 FROM x"
        )
        .is_err());
        assert!(validate_single_select(
            "WITH x AS (INSERT INTO t VALUES (1) RETURNING 1) SELECT count(*)=0 FROM x"
        )
        .is_err());
        assert!(validate_single_select(
            "WITH x AS (UPDATE t SET a=1 RETURNING 1) SELECT count(*)=0 FROM x"
        )
        .is_err());
    }

    #[test]
    fn shape_gate_rejects_locking_clause() {
        assert!(validate_single_select("SELECT * FROM t FOR UPDATE").is_err());
        assert!(validate_single_select("SELECT * FROM t FOR SHARE").is_err());
        assert!(validate_single_select("SELECT * FROM t FOR NO KEY UPDATE").is_err());
        assert!(validate_single_select("SELECT * FROM t FOR KEY SHARE").is_err());
        // A locking clause nested in a sub-select is still caught.
        assert!(
            validate_single_select("SELECT count(*)=0 FROM (SELECT id FROM t FOR UPDATE) s")
                .is_err()
        );
    }

    #[test]
    fn shape_gate_rejects_mutating_and_lock_builtins() {
        assert!(validate_single_select("SELECT nextval('s')").is_err());
        assert!(validate_single_select("SELECT setval('s', 1)").is_err());
        assert!(validate_single_select("SELECT pg_advisory_lock(1)").is_err());
        assert!(validate_single_select("SELECT pg_advisory_xact_lock(1)").is_err());
        assert!(validate_single_select("SELECT pg_try_advisory_lock(1)").is_err());
        assert!(validate_single_select("SELECT pg_advisory_unlock_all()").is_err());
        // pg_catalog-qualified resolves to the same builtin.
        assert!(validate_single_select("SELECT pg_catalog.nextval('s')").is_err());
        // Nested inside an expression / sub-select.
        assert!(validate_single_select("SELECT (SELECT nextval('s')) > 0").is_err());
    }

    #[test]
    fn shape_gate_allows_read_only_selects_and_read_only_builtins() {
        assert!(validate_single_select("SELECT count(*) = 0 FROM t").is_ok());
        // currval / lastval are READ-ONLY (read the session's last value) — allowed.
        assert!(validate_single_select("SELECT currval('s') > 0").is_ok());
        assert!(validate_single_select("SELECT lastval() > 0").is_ok());
        // A CTE with no DML is fine.
        assert!(
            validate_single_select("WITH x AS (SELECT 1 AS a) SELECT count(*)=1 FROM x").is_ok()
        );
    }
}
