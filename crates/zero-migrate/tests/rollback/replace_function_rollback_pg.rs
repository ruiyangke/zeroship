//! Rolling back a function REPLACE, proven against live `PostgreSQL`.
//!
//! The same shape as the view case (`replace_view_rollback_sqlite.rs`): a flag turns a
//! create into a MODIFY, and the inverse still assumes the create brought the object into
//! being. `crates/zero-migrate/src/render/vendor.rs` renders the `createFunction` down as
//! `DROP FUNCTION IF EXISTS` unconditionally, with `replace` reaching only the up.
//!
//! It differs from the view case in two ways that both matter.
//!
//! It was NEVER guarded by accident. The view defect was unreachable across deploys until
//! a1fe1047 removed the fold's duplicate-name check; functions have no duplicate-name
//! refusal because overloads share names and differ by input signature. So an across-deploys
//! function replace has always applied, and its rollback has always been destructive.
//!
//! And it must be measured HERE rather than on SQLite, where the cheaper view harness
//! lives. `createFunction` is PostgreSQL-only - `dialect-table.ts:47` marks it
//! `sqlite: "unsupported", mysql: "unsupported"`, and `model/op_support.rs:271` refuses it
//! with "function vendor primitives are PostgreSQL-only". A SQLite copy of this test would
//! fail for a reason that has nothing to do with the defect.
//!
//! GATED behind `ZERO_MIGRATE_TEST_PG_URL` like every other live suite here. Every
//! assertion reads `pg_proc` through the live session, never the plan the engine intended.

use crate::support;

use std::collections::BTreeMap;

use crate::support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::apply::executor::{rollback, LockMode, RollbackRequest, RollbackTarget};
use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::Op;
use zero_migrate::model::migration::Migration;
use zero_migrate::render::step::PlanStep;
use zero_migrate::{
    fold_ops, guard_for, Approval, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema,
    MigrationEngine, PostgresBackend, SqlDialect,
};

const OWNER: &str = "app_replace_function_rollback_pg";
const FUNCTION: &str = "greet";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "replace_fn_rollback_pg_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// The original body.
fn create_doc() -> String {
    serde_json::json!({
        "ir_version": 1,
        "name": "create_greet",
        "owner_app": OWNER,
        "ops": [{
            "op": "createFunction",
            "name": FUNCTION,
            "args": [],
            "returns": "text",
            "language": "sql",
            "body": "SELECT 'hello'::text"
        }]
    })
    .to_string()
}

/// The replacing body. Same signature - a different signature would create a SECOND
/// function by overloading rather than replacing the first, which is a different question.
fn replace_doc() -> String {
    serde_json::json!({
        "ir_version": 1,
        "name": "replace_greet",
        "owner_app": OWNER,
        "ops": [{
            "op": "createFunction",
            "name": FUNCTION,
            "replace": true,
            "args": [],
            "returns": "text",
            "language": "sql",
            "body": "SELECT 'goodbye'::text"
        }]
    })
    .to_string()
}

async fn apply_doc(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
    ir: &str,
    reg: &BTreeMap<String, String>,
    history: &mut Vec<Op>,
    approval: Approval,
) -> Result<Vec<Migration>, String> {
    let backend = PostgresBackend::new_generic(session);
    let policy = support::operator_charter(&cfg.project_schema);
    let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Postgres, &policy);
    // The charter is threaded as vendor authority rather than left to the scope-derived
    // fallback, which answers schema confinement and grants nothing outside an operator
    // posture. `createFunction` is a privileged primitive, so the gate has to read the
    // grant off the charter or it refuses before the rollback can be measured at all.
    let document = zero_migrate::model::load::load_ir_document_authorized(
        ir,
        OWNER,
        zero_migrate::model::validate::Dialect::Postgres,
        reg,
        None,
        Some(zero_migrate::model::validate::VendorAuthority {
            effective: &policy,
            default_schema: &cfg.project_schema,
        }),
    )
    .map_err(|error| format!("load gate (postgres): {error}"))?;
    let folded = fold_ops(history, SqlDialect::Postgres, &cfg.project_schema, &policy)
        .map_err(|error| format!("fold the applied history: {error}"))?;
    let live = LiveSchema::from_catalog_snapshot(folded, OWNER);
    history.extend(document.ops.iter().cloned());
    let plan = author
        .lower_plan(&document, &live)
        .map_err(|error| format!("lower the doc plan on PostgreSQL: {error}"))?;
    MigrationEngine::new()
        .apply_plan(
            &plan.steps,
            approval,
            &backend,
            cfg,
            OWNER,
            LockMode::Acquire,
        )
        .await
        .map_err(|error| format!("apply the authored plan on PostgreSQL: {error}"))?;
    Ok(plan
        .steps
        .iter()
        .filter_map(|step| match step {
            PlanStep::Ddl(m) => Some(m.clone()),
            _ => None,
        })
        .collect())
}

/// The function's live body as `pg_proc` records it, or `None` when the function is gone.
/// Read from the catalog rather than from the plan, so a rollback that removed the
/// function is distinguishable from one that rewrote it.
async fn live_function_body(
    session: &PgDevSession,
    schema: &str,
) -> Result<Option<String>, String> {
    let rows = session
        .query(
            "SELECT p.prosrc AS src FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname = $1 AND p.proname = $2",
            &[schema.into(), FUNCTION.into()],
        )
        .await
        .map_err(|error| format!("read pg_proc: {error}"))?;
    Ok(rows
        .first()
        .and_then(|row| row.try_get::<_, String>("src").ok()))
}

/// BLOCKED, not skipped: this instrument reaches the load gate and is REFUSED before it can
/// measure anything. `createFunction` is unreachable from the confined creator capability set:
///
///     author this privileged migration under the operator/platform capability set (which
///     composes allowFunction), not the confined creator profile [VENDOR_OP_DENIED
///     op_index=0 dialect=postgres]
///
/// Reaching the defect needs `load_ir_document_authorized` with a `VendorAuthority`
/// (`model/load.rs:70`), which no test in this crate constructs today. That plumbing is the
/// work, and it is worth doing deliberately rather than inside a probe.
///
/// The refusal is itself the useful result: it narrows #211 from "always reachable" to
/// "reachable only from an operator/platform migration". Kept rather than deleted because
/// the harness is correct up to that gate and states what the measurement needs.
#[compio::test]
async fn rolling_back_a_function_replace_on_postgres() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let schema = token();
    let cfg = ExecutorConfig::new(
        format!("project_{schema}"),
        &schema,
        support::operator_charter(&schema),
    );
    let quoted_schema = quote_ident(&cfg.project_schema);
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [
            cfg.project_schema.clone(),
            cfg.confinement.meta_schema.clone(),
        ],
    );
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated test schema");

    let work: Result<(), String> = async {
        let backend = PostgresBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .map_err(|error| format!("ensure migration journal: {error}"))?;

        let mut history: Vec<Op> = Vec::new();
        let mut migrations = apply_doc(
            &session,
            &cfg,
            &create_doc(),
            &BTreeMap::new(),
            &mut history,
            Approval::None,
        )
        .await?;

        let original = live_function_body(&session, &cfg.project_schema).await?;
        let original = original.ok_or_else(|| {
            "the function must exist after its create, or the rest proves nothing".to_string()
        })?;

        migrations.extend(
            apply_doc(
                &session,
                &cfg,
                &replace_doc(),
                &BTreeMap::new(),
                &mut history,
                Approval::Approved,
            )
            .await?,
        );

        let replaced = live_function_body(&session, &cfg.project_schema).await?;
        if replaced.as_deref() == Some(original.as_str()) {
            return Err(
                "the replace did not change the stored body, so the rollback below \
                        would have nothing to undo"
                    .to_string(),
            );
        }

        // Measured before it was asserted. The recorded run reported
        // `rollback=ok RollbackOutcome { rolled_back: [..], skipped_irreversible: [] }`
        // with `function_after=None`: undoing a body change deleted the function and
        // called it a success. The expectation below is that outcome corrected, not a
        // guess written ahead of the evidence.
        let request = RollbackRequest::new(RollbackTarget::Steps(1));
        let outcome = rollback(
            &backend,
            &cfg,
            &request,
            &migrations,
            Approval::Approved,
            OWNER,
            guard_for(&GuardConfig::from_policy(
                support::operator_charter(&cfg.project_schema),
                SqlDialect::Postgres,
            ))
            .as_ref(),
        )
        .await;

        let error = match outcome {
            Ok(outcome) => {
                return Err(format!(
                    "the rollback must refuse a replace it cannot invert, and it succeeded: \
                     {outcome:?}"
                ));
            }
            Err(error) => format!("{error:?}"),
        };
        if !error.contains("Irreversible") {
            return Err(format!(
                "the refusal must be the irreversible one the operator can force past, got: \
                 {error}"
            ));
        }

        // A refusal that still destroyed the object would be the same defect wearing an
        // error message, so the catalog is read again rather than trusting the verdict.
        let after = live_function_body(&session, &cfg.project_schema).await?;
        match after {
            None => {
                return Err(
                    "the refused rollback left no function behind, so it dropped the object \
                     it declined to restore"
                        .to_string(),
                );
            }
            Some(body) if Some(body.as_str()) != replaced.as_deref() => {
                return Err(format!(
                    "the refused rollback rewrote the body it declined to restore: {body:?}"
                ));
            }
            Some(_) => {}
        }

        Ok(())
    }
    .await;

    session
        .batch(&format!("DROP SCHEMA IF EXISTS {quoted_schema} CASCADE"))
        .await
        .ok();
    work.expect("the function replace rollback arm");
}
