//! A MySQL trigger body may not RETURN A RESULT SET, and the engine must say so
//! before the statement reaches a server.
//!
//! MEASURED, by the live MySQL leg of `dialect_matrix/dialect_conformance_live.rs`
//! the day that leg was written. `createTrigger/bodySimple` is declared `portable`
//! on MySQL; the corpus representative's body is one `SELECT <expr>` statement; the
//! plan cleared validate, cleared the guard, cleared lower, and MySQL 8.4.11 then
//! answered
//!
//! ```text
//!   [0A000] Not allowed to return a result set from a trigger
//! ```
//!
//! which is the `Outcome::ServerError` class that layer exists to catch: a migration
//! that dies PART WAY THROUGH applying. `SELECT <expr>` has no result-set-free MySQL
//! spelling - `SELECT ... INTO` needs a declared target this closed trigger-body IR
//! has no way to name - so the statement is refused at lower, beside `RAISE IGNORE`,
//! the other trigger statement MySQL cannot render.
//!
//! NOT A DIALECT-TABLE CELL, and this is the whole reason the gate lives in the
//! renderer rather than in `dialect-support.toml`. MySQL body triggers WORK: the
//! same conformance sweep's `dropTrigger/base` prelude creates one with a `DELETE`
//! body and it applies. Flipping `createTrigger/bodySimple` to `unsupported` on
//! MySQL would refuse every MySQL body trigger to reject the one statement MySQL
//! cannot host - the over-refusal `sqlite_declaration_flip_over_refusal_control.rs`
//! demonstrated for `dropConstraint`, in the same words: a refusal measured from ONE
//! representative bounds what that SHAPE does, not what the OP does.
//!
//! THE OVER-REFUSAL CONTROLS ARE THE POINT, and there are two of them:
//!
//!   - A MySQL body trigger whose statement is a `DELETE` still applies AND STILL
//!     FIRES - the insert into `t` is proven to have emptied `audit` through the
//!     trigger, not merely to have been accepted.
//!   - SQLite still accepts a `SELECT` body, because a SQLite trigger genuinely may
//!     contain one. The gate is MySQL's, not the statement's.

use crate::support;

use std::collections::BTreeMap;

use crate::support::mysql::{quote_ident, DatabaseGuard, MysqlDevSession};
use serde_json::{json, Value};
use zero_migrate::apply::backend::{MigrationBackend, MysqlBackend};
use zero_migrate::apply::executor::LockMode;
use zero_migrate::driver::SqlSession;
use zero_migrate::{
    resolve_create_table_policy, Approval, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema,
    MigrationEngine, MigrationIr, SqlDialect, SqliteBackend,
};

const OWNER: &str = "app_mysql_trigger_result_set";

/// `t` and `audit` belong to this app, so an ownership refusal can never stand in
/// for the answer this file is asking for.
fn registry() -> BTreeMap<String, String> {
    ["t", "audit"]
        .into_iter()
        .map(|name| (name.to_string(), OWNER.to_string()))
        .collect()
}

fn envelope(name: &str, ops: Vec<Value>) -> String {
    json!({ "ir_version": 1, "name": name, "owner_app": OWNER, "ops": ops }).to_string()
}

/// The two tables. A MySQL trigger may not touch the table it is attached to, so the
/// `DELETE` control needs a second one.
fn base_ops() -> Vec<Value> {
    ["t", "audit"]
        .into_iter()
        .map(|name| {
            json!({ "op": "createTable", "name": name,
                    "columns": [
                        { "name": "id", "type": "bigInt", "nullable": false },
                        { "name": "x", "type": "boolean", "nullable": true },
                    ],
                    "primaryKey": ["id"], "constraints": [], "indexes": [] })
        })
        .collect()
}

/// A `BEFORE INSERT ... FOR EACH ROW` trigger on `t` carrying one body statement.
fn trigger(name: &str, statement: Value) -> Value {
    json!({
        "op": "createTrigger", "name": name, "table": "t",
        "timing": "before", "events": ["insert"], "forEach": "row",
        "action": { "kind": "body", "statements": [statement] },
    })
}

/// `SELECT x` - the corpus representative's body, and the shape MySQL refuses.
fn select_statement() -> Value {
    json!({ "stmt": "select", "expr": { "node": "colRef", "name": "x" } })
}

/// `DELETE FROM audit WHERE x` - a body statement MySQL genuinely hosts.
fn delete_statement() -> Value {
    json!({ "stmt": "delete", "table": "audit",
            "where": { "node": "colRef", "name": "x" } })
}

/// Author + lower + apply one envelope through the production guarded path, against
/// the CATALOG the previous envelopes left behind.
async fn apply<B: MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    dialect: SqlDialect,
    ir: &str,
) -> Result<(), String> {
    let policy = support::operator_charter(&cfg.project_schema);
    let authored: MigrationIr =
        serde_json::from_str(ir).map_err(|error| format!("parse test IR: {error}"))?;
    let resolved = resolve_create_table_policy(&authored, &policy, &cfg.project_schema)
        .map_err(|error| format!("resolve create-table policy: {error}"))?;
    let source = serde_json::to_string(&resolved)
        .map_err(|error| format!("serialize the resolved IR: {error}"))?;
    let live = match backend.snapshot_schema(cfg).await {
        Ok(snapshot) => LiveSchema::from_catalog_snapshot(snapshot, OWNER),
        Err(error) => return Err(format!("snapshot the live schema: {error}")),
    };
    let author = IrAuthor::new(&cfg.project_schema, OWNER, dialect, &policy);
    let guard = GuardConfig::from_policy(policy.clone(), dialect);
    let artifact = author
        .load_and_lower_guarded(&source, OWNER, &registry(), &live, &guard)
        .map_err(|error| format!("lower: {error}"))?;
    MigrationEngine::new()
        .apply_plan(
            &artifact.plan.steps,
            Approval::Approved,
            backend,
            cfg,
            OWNER,
            LockMode::Acquire,
        )
        .await
        .map(|_| ())
        .map_err(|error| format!("apply: {error}"))
}

#[compio::test]
async fn mysql_refuses_a_select_trigger_body_and_keeps_a_delete_one() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("trigsel");
    let cfg = ExecutorConfig::new(
        format!("project_{database}"),
        &database,
        support::operator_charter(&database),
    );
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);
    session
        .batch(&format!("CREATE DATABASE {}", quote_ident(&database)))
        .await
        .expect("create the isolated probe database");
    let backend = MysqlBackend::new_generic(&session);
    backend
        .ensure_journal(&cfg)
        .await
        .expect("ensure the migration journal");
    let qdb = quote_ident(&database);

    let work: Result<(), String> = async {
        apply(
            &backend,
            &cfg,
            SqlDialect::Mysql,
            &envelope("setup", base_ops()),
        )
        .await?;

        // THE DEFECT. Before this gate existed the engine emitted the CREATE TRIGGER
        // and MySQL answered `[0A000] Not allowed to return a result set from a
        // trigger`, mid-apply.
        let refusal = match apply(
            &backend,
            &cfg,
            SqlDialect::Mysql,
            &envelope("select_body", vec![trigger("tg_bad", select_statement())]),
        )
        .await
        {
            Ok(()) => {
                return Err(
                    "a SELECT trigger body must be refused by the engine on MySQL, \
                            but the server was handed the statement"
                        .into(),
                )
            }
            Err(refusal) => refusal,
        };
        if !refusal.contains("selectStatement") {
            return Err(format!(
                "the refusal must name the trigger statement it cannot render; got: {refusal}"
            ));
        }
        if refusal.contains("apply:") {
            return Err(format!(
                "the refusal must arrive at lower, before the server: {refusal}"
            ));
        }

        // OVER-REFUSAL CONTROL ONE: a MySQL body trigger still applies...
        apply(
            &backend,
            &cfg,
            SqlDialect::Mysql,
            &envelope("delete_body", vec![trigger("tg_good", delete_statement())]),
        )
        .await?;

        // ... and still FIRES. Accepting the DDL is not the claim; the row leaving
        // `audit` because a row entered `t` is.
        session
            .batch(&format!(
                "INSERT INTO {qdb}.`audit` (`id`, `x`) VALUES (1, 1)"
            ))
            .await
            .map_err(|error| format!("seed the audit row: {error}"))?;
        session
            .batch(&format!("INSERT INTO {qdb}.`t` (`id`, `x`) VALUES (1, 1)"))
            .await
            .map_err(|error| format!("insert the row that fires the trigger: {error}"))?;
        let left: i64 = session
            .query_one(&format!("SELECT count(*) AS n FROM {qdb}.`audit`"), &[])
            .await
            .map_err(|error| format!("count the audit rows: {error}"))?
            .try_get("n")
            .map_err(|error| format!("decode the audit row count: {error}"))?;
        if left != 0 {
            return Err(format!(
                "the DELETE trigger body must have emptied `audit` when a row entered \
                 `t`, but {left} row(s) remain - the trigger was created and did nothing"
            ));
        }
        Ok(())
    }
    .await;

    work.expect("the MySQL SELECT-trigger-body gate and its over-refusal control");
}

#[compio::test]
async fn sqlite_still_accepts_a_select_trigger_body() {
    let dir = tempfile::tempdir().expect("temp dir");
    let app = dir.path().join("probe.sqlite");
    let journal = dir.path().join("probe.migrations.sqlite");
    let backend = SqliteBackend::open(&app, &journal).expect("open the probe database");
    let cfg = ExecutorConfig::new("main", "main", support::operator_charter("main"));
    backend
        .ensure_journal(&cfg)
        .await
        .expect("ensure the migration journal");

    apply(
        &backend,
        &cfg,
        SqlDialect::Sqlite,
        &envelope("setup", base_ops()),
    )
    .await
    .expect("create the base tables on SQLite");

    // THE CROSS-DIALECT CONTROL. A SQLite trigger body may contain a SELECT, so the
    // gate above has to be MySQL's and not the statement's.
    apply(
        &backend,
        &cfg,
        SqlDialect::Sqlite,
        &envelope("select_body", vec![trigger("tg_ok", select_statement())]),
    )
    .await
    .expect("a SELECT trigger body must still apply on SQLite");
}
