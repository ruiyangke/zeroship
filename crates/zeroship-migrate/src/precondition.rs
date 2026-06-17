//! Preconditions — state/data-conditional apply (v3 Plan D, Liquibase-style).
//!
//! A migration may carry **preconditions**: assertions evaluated against the
//! live DB *before* its `up` runs, gating whether the migration applies. This is
//! the engine's "only run this if the database is in shape X" primitive — the
//! peer of Liquibase `<preConditions>`.
//!
//! # Two evaluation paths, by trust
//!
//! - **Structured checks** ([`Precondition::TableExists`],
//!   [`Precondition::ColumnExists`], [`Precondition::RowCount`], …) are
//!   ENGINE-BUILT, fully parameterized catalog queries
//!   (`information_schema` / `pg_catalog`). The project schema is bound as `$1`;
//!   the table/column identifiers are validated with [`validate_ident`] (bare
//!   `[A-Za-z_][A-Za-z0-9_]*` only — no schema qualifier, no quotes, no
//!   punctuation) and then bound as parameters too. There is **no string
//!   interpolation of user input into SQL**, so these are injection-safe by
//!   construction.
//! - **[`Precondition::SqlBoolean`]** is UNTRUSTED creator/AI SQL. It is the
//!   escape hatch for assertions the structured checks cannot express, and it is
//!   confined three ways before it is allowed to run:
//!   1. it MUST pass the [`SqlGuard`](crate::guard::SqlGuard) (read-only SELECT;
//!      a cross-schema / file / network / dangerous precondition is denied — the
//!      same line-1 defense the `up` gets);
//!   2. it MUST be a **single `SELECT`** returning exactly **one boolean** column
//!      (multi-statement / non-SELECT / wrong-shape is rejected up-front, before
//!      it touches the DB — a precondition can NEVER mutate state);
//!   3. it runs under the least-privilege **`migrator` role** (`SET LOCAL ROLE`,
//!      transaction-scoped, inside a read-only transaction) — the same line-2
//!      DB-privilege confinement the `up` gets.
//!
//! # Where this is evaluated
//!
//! [`crate::executor::apply`] evaluates a pending migration's preconditions
//! **inside the apply flow, under the project advisory lock**, immediately before
//! the migration's `up` — so the state a precondition checks is stable for the
//! apply. All evaluation is read-only (catalog reads + a single read-only
//! `SELECT`); a precondition can never write.

use compio_postgres::Client;
use pg_query::protobuf::node::Node as NodeEnum;

use crate::db::ExecutorConfig;
use crate::guard::{GuardConfig, GuardError, SqlGuard};

/// A comparison operator for a [`Precondition::RowCount`] assertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CmpOp {
    /// `=`
    Eq,
    /// `<>`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

impl CmpOp {
    /// Apply the operator to two i64 operands (`lhs <op> rhs`).
    #[must_use]
    pub const fn apply(self, lhs: i64, rhs: i64) -> bool {
        match self {
            Self::Eq => lhs == rhs,
            Self::Ne => lhs != rhs,
            Self::Lt => lhs < rhs,
            Self::Le => lhs <= rhs,
            Self::Gt => lhs > rhs,
            Self::Ge => lhs >= rhs,
        }
    }
}

/// A single precondition assertion evaluated against the live DB before a
/// migration's `up` runs.
///
/// Structured variants are engine-built parameterized catalog queries
/// (injection-safe); [`Precondition::SqlBoolean`] is untrusted SQL run behind
/// the guard + migrator role + single-read-only-SELECT shape gate. See the
/// module docs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Precondition {
    /// The project schema contains a table named `table` (a base table or view).
    TableExists {
        /// The bare table name (no schema qualifier).
        table: String,
    },
    /// The project schema does NOT contain a table named `table`.
    TableNotExists {
        /// The bare table name (no schema qualifier).
        table: String,
    },
    /// The project-schema table `table` has a column named `column`.
    ColumnExists {
        /// The bare table name (no schema qualifier).
        table: String,
        /// The bare column name.
        column: String,
    },
    /// The project-schema table `table` does NOT have a column named `column`.
    ColumnNotExists {
        /// The bare table name (no schema qualifier).
        table: String,
        /// The bare column name.
        column: String,
    },
    /// `count(*)` of the project-schema table `table` compares to `value` under
    /// `op` (e.g. `RowCount { table, op: Eq, value: 0 }` = "the table is empty").
    RowCount {
        /// The bare table name (no schema qualifier).
        table: String,
        /// The comparison operator (`count(*) <op> value`).
        op: CmpOp,
        /// The right-hand operand.
        value: i64,
    },
    /// An UNTRUSTED single read-only `SELECT` returning one boolean column. Run
    /// behind the guard + migrator role + shape gate. The escape hatch for
    /// assertions the structured checks cannot express.
    SqlBoolean {
        /// The read-only `SELECT … ` returning exactly one boolean.
        sql: String,
    },
}

/// What to do when a precondition is **unmet** (evaluates false).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum OnUnmet {
    /// Abort the whole apply (fail-closed): return [`PreconditionFailed`] and
    /// apply NOTHING for this migration or the rest of the batch. The default —
    /// an unmet precondition usually means the world is not as the migration
    /// assumes, and silently skipping could leave the schema inconsistent.
    ///
    /// [`PreconditionFailed`]: crate::executor::ApplyError::PreconditionFailed
    #[default]
    Halt,
    /// Skip THIS migration this run (do not apply it, do not journal it — it
    /// stays pending and is re-evaluated on the next deploy), and continue with
    /// the rest of the batch. The "apply this once the DB reaches shape X"
    /// idempotent-deploy primitive. A skipped migration's dependents do not run
    /// this batch either (a dependent of a not-yet-applied migration is blocked
    /// by the dependency ordering — see [`crate::executor::order_pending`]).
    Skip,
}

/// One precondition + its unmet policy, carried by a [`Migration`](crate::migration::Migration).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PreconditionCheck {
    /// The assertion to evaluate against the live DB.
    pub check: Precondition,
    /// What to do if `check` is unmet (default [`OnUnmet::Halt`]).
    #[serde(default)]
    pub on_unmet: OnUnmet,
}

impl PreconditionCheck {
    /// A check with the default ([`OnUnmet::Halt`]) unmet policy.
    #[must_use]
    pub const fn halt(check: Precondition) -> Self {
        Self {
            check,
            on_unmet: OnUnmet::Halt,
        }
    }

    /// A check that SKIPs the migration when unmet.
    #[must_use]
    pub const fn skip(check: Precondition) -> Self {
        Self {
            check,
            on_unmet: OnUnmet::Skip,
        }
    }
}

/// Error evaluating a precondition.
#[derive(Debug, thiserror::Error)]
pub enum PreconditionError {
    /// A database error while running a (structured or `SqlBoolean`) check.
    #[error("precondition db error: {0}")]
    Db(#[from] compio_postgres::Error),
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
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Evaluate a precondition against the live DB. Returns `true` if the assertion
/// HOLDS (the migration may apply), `false` if it is UNMET (the executor then
/// applies the [`OnUnmet`] policy).
///
/// All evaluation is **read-only**: structured checks are parameterized catalog
/// reads; [`Precondition::SqlBoolean`] is gated to a single read-only `SELECT`
/// (guard + shape) and run under the migrator role inside a `READ ONLY`
/// transaction. A precondition can never mutate state.
///
/// # Errors
/// - [`PreconditionError::InvalidIdentifier`] — a structured check's table/column
///   is not a bare identifier.
/// - [`PreconditionError::Guard`] — a `SqlBoolean` was guard-denied.
/// - [`PreconditionError::NotABooleanSelect`] — a `SqlBoolean` is not a single
///   boolean-returning `SELECT`.
/// - [`PreconditionError::Db`] — a query failed.
pub async fn evaluate(
    conn: &Client,
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
        Precondition::RowCount { table, op, value } => {
            validate_ident("table", table)?;
            let n = row_count(conn, cfg, table).await?;
            Ok(op.apply(n, *value))
        }
        Precondition::SqlBoolean { sql } => evaluate_sql_boolean(conn, cfg, sql).await,
    }
}

/// `information_schema.tables` lookup: a base table OR a view named `table` in the
/// project schema. Schema + table are BOUND ($1/$2), never interpolated.
async fn table_exists(
    conn: &Client,
    cfg: &ExecutorConfig,
    table: &str,
) -> Result<bool, PreconditionError> {
    let row = conn
        .query_one(
            "SELECT EXISTS (
                 SELECT 1 FROM information_schema.tables
                  WHERE table_schema = $1 AND table_name = $2
             ) AS present",
            &[&cfg.project_schema, &table],
        )
        .await?;
    Ok(row.get("present"))
}

/// `information_schema.columns` lookup: a column named `column` on the
/// project-schema table `table`. All three are BOUND, never interpolated.
async fn column_exists(
    conn: &Client,
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
            &[&cfg.project_schema, &table, &column],
        )
        .await?;
    Ok(row.get("present"))
}

/// `count(*)` of the project-schema table `table`.
///
/// The schema + table are validated as bare identifiers then double-quoted into a
/// fully-qualified relation reference. They CANNOT be bound (a relation name is
/// not a bind parameter in Postgres), so the injection-safety here rests on
/// [`validate_ident`] (the schema is the engine-owned `cfg.project_schema`, the
/// table has passed `validate_ident`). This mirrors `backfill::build_batch_sql`,
/// which quotes the same validated identifiers into relation position.
async fn row_count(
    conn: &Client,
    cfg: &ExecutorConfig,
    table: &str,
) -> Result<i64, PreconditionError> {
    let stmt = format!(
        "SELECT count(*)::bigint AS n FROM {}.{}",
        quote_ident(&cfg.project_schema),
        quote_ident(table),
    );
    let row = conn.query_one(&stmt, &[]).await?;
    Ok(row.get("n"))
}

/// Statically validate that a `SqlBoolean`'s SQL is a SINGLE `SELECT` (no other
/// statement kind, no multi-statement) BEFORE it touches the DB — so a
/// precondition can never run DML/DDL or smuggle a second statement.
///
/// Shape (single boolean column) is enforced at execution by reading exactly one
/// `bool` column from the single result row; here we only enforce
/// single-statement-and-it-is-a-SELECT. Parse failure is rejected (deny-by-default,
/// though the guard's parse already covers it).
fn validate_single_select(sql: &str) -> Result<(), PreconditionError> {
    let parsed = pg_query::parse(sql).map_err(|e| PreconditionError::NotABooleanSelect {
        reason: format!("could not parse SQL: {e}"),
    })?;
    let stmts: Vec<_> = parsed
        .protobuf
        .stmts
        .iter()
        .filter_map(|s| s.stmt.as_ref().and_then(|n| n.node.as_ref()))
        .collect();
    if stmts.len() != 1 {
        return Err(PreconditionError::NotABooleanSelect {
            reason: format!(
                "expected exactly one statement, found {}",
                stmts.len()
            ),
        });
    }
    if !matches!(stmts[0], NodeEnum::SelectStmt(_)) {
        return Err(PreconditionError::NotABooleanSelect {
            reason: "the single statement is not a SELECT".to_string(),
        });
    }
    Ok(())
}

/// Evaluate an untrusted `SqlBoolean` precondition: guard → shape gate → run
/// under the migrator role in a READ ONLY transaction → read one boolean.
async fn evaluate_sql_boolean(
    conn: &Client,
    cfg: &ExecutorConfig,
    sql: &str,
) -> Result<bool, PreconditionError> {
    // 1. Line-1 guard: the SAME deny-list + cross-schema confinement the `up`
    //    gets. A precondition reaching `control.*` / a file/network func / a
    //    dangerous construct is denied here, before anything runs.
    let guard = SqlGuard::new(GuardConfig {
        project_schema: cfg.project_schema.clone(),
        extension_allowlist: Vec::new(),
    });
    guard.check(sql)?;

    // 2. Shape gate: a single SELECT (no DML/DDL, no second statement).
    validate_single_select(sql)?;

    // 3. Run read-only under the least-privilege migrator role. A READ ONLY
    //    transaction is a hard backstop on top of the SELECT-only shape gate and
    //    the migrator role: even if a write somehow slipped through, the txn
    //    rejects it. `SET LOCAL` (search_path/role) is transaction-scoped, so it
    //    vanishes at COMMIT and never leaks onto the session — the same H2
    //    discipline the executor uses.
    conn.batch_execute("BEGIN READ ONLY").await?;
    let result = run_sql_boolean_in_txn(conn, cfg, sql).await;
    // Always end the transaction. A read-only txn has nothing to persist, so we
    // COMMIT on success and ROLLBACK on error (both end the txn + revert the
    // SET LOCALs); failures here are surfaced behind the primary result.
    match &result {
        Ok(_) => {
            conn.batch_execute("COMMIT").await?;
        }
        Err(_) => {
            if let Err(rb) = conn.batch_execute("ROLLBACK").await {
                tracing::warn!(error = %rb, "zeroship-migrate: ROLLBACK failed after a SqlBoolean precondition error");
            }
        }
    }
    result
}

/// The body of [`evaluate_sql_boolean`] run INSIDE the `BEGIN READ ONLY`
/// transaction: pin the project `search_path`, drop to the migrator role (both
/// `SET LOCAL`, transaction-scoped), then run the `SELECT` and read one boolean.
async fn run_sql_boolean_in_txn(
    conn: &Client,
    cfg: &ExecutorConfig,
    sql: &str,
) -> Result<bool, PreconditionError> {
    // Pin the project schema (and only it) on the path, scoped to this txn.
    conn.batch_execute(&format!(
        "SET LOCAL search_path TO \"{}\"",
        cfg.project_schema.replace('"', "\"\"")
    ))
    .await?;
    // Drop to the migrator role for the read, scoped to this txn (line-2). No
    // role configured (tests / single-tenant dev) runs as the connecting role.
    if let Some(role) = &cfg.migrator_role {
        conn.batch_execute(&format!("SET LOCAL ROLE \"{}\"", role.replace('"', "\"\"")))
            .await?;
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
}
