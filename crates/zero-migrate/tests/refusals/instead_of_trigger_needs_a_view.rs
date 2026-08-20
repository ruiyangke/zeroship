//! An `INSTEAD OF` trigger must target a VIEW, and the engine must say so before
//! the statement reaches a server.
//!
//! `INSTEAD OF` exists to make a view writable. Both dialects that have it refuse
//! it on a table, in their own words, MEASURED through the engine's own emitted
//! SQL:
//!
//! ```text
//!   sqlite      cannot create INSTEAD OF trigger on table: t
//!   postgres    "t" is a table; ... INSTEAD OF triggers may only be used on views
//! ```
//!
//! So this is NOT a dialect capability - both dialects support the op, on a view -
//! and it is not a `dialect-support.toml` cell. It is a structural fact about the
//! op: an `INSTEAD OF` trigger whose target is a TABLE has no valid form anywhere.
//! The gate is therefore dialect-neutral and lives in the lowerer, where the folded
//! live schema knows which names are tables.
//!
//! NOT SQLITE-ONLY, which is the part the live conformance sweep could not see.
//! `createTrigger/bodyInsteadOf` is the only corpus row carrying `INSTEAD OF`, and
//! it is declared unsupported on PostgreSQL because PostgreSQL has no trigger
//! BODIES. But `create_trigger_variant` routes EVERY `executeFunction` action to
//! the `executeFunction` variant regardless of timing, so a PostgreSQL
//! `INSTEAD OF ... EXECUTE FUNCTION` trigger on a table selects a `portable` cell,
//! clears validate, clears lower, and dies against PostgreSQL exactly the way the
//! SQLite row dies against SQLite. The corpus never drives that combination. This
//! file does.
//!
//! THE OVER-REFUSAL CONTROL IS THE POINT. Refusing `INSTEAD OF` outright would
//! break the only thing it is for, so both dialects apply an `INSTEAD OF` trigger
//! on a real VIEW here, and the trigger is then exercised: an INSERT into the view
//! must land in the base table through the trigger body.
//!
//! The gate keys on a POSITIVE fact - the target is a KNOWN LIVE TABLE - rather
//! than on the absence of a view. Catalog introspection is what fills that set
//! (`relkind IN ('r','p')` on PostgreSQL, `type='table'` on SQLite), so a view is
//! never in it. A target the engine has never seen is left alone, which is the
//! same fail-open posture `lower_dml_op` documents for its resolved-ColRef rule.

use crate::support;

use std::collections::BTreeMap;

use crate::support::PgDevSession;
use serde_json::json;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::apply::executor::LockMode;
use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::Op;
use zero_migrate::{
    fold_ops, Approval, ExecutorConfig, IrAuthor, LiveSchema, MigrationEngine, PostgresBackend,
    SqlDialect, SqliteBackend,
};

const OWNER: &str = "app_instead_of_trigger_needs_a_view";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "insteadofview_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn envelope(name: &str, ops: Vec<serde_json::Value>) -> String {
    json!({ "ir_version": 1, "name": name, "owner_app": OWNER, "ops": ops }).to_string()
}

/// `t` and `v` belong to this app, so an ownership refusal can never stand in for
/// the answer this file is asking for.
fn registry() -> BTreeMap<String, String> {
    ["t", "v"]
        .into_iter()
        .map(|name| (name.to_string(), OWNER.to_string()))
        .collect()
}

/// `t(id text pk, a text)` plus a view over it.
fn base_ops() -> Vec<serde_json::Value> {
    vec![
        json!({ "op": "createTable", "name": "t",
                "columns": [
                    { "name": "id", "type": "text", "nullable": false },
                    { "name": "a", "type": "text", "nullable": true },
                ],
                "primaryKey": ["id"], "constraints": [], "indexes": [] }),
        json!({ "op": "createView", "name": "v",
                "query": { "kind": "structured",
                           "select": { "from": { "name": "t" }, "projection": [] } } }),
    ]
}

/// A body-action `INSTEAD OF INSERT` trigger on `target`. SQLite's form.
fn body_instead_of(name: &str, target: &str) -> serde_json::Value {
    json!({
        "op": "createTrigger", "name": name, "table": target,
        "timing": "insteadOf", "events": ["insert"], "forEach": "row",
        "action": { "kind": "body", "statements": [
            { "stmt": "insert", "table": "t", "columns": ["id", "a"],
              "rows": [["seeded", "from the trigger"]] },
        ]},
    })
}

/// An `INSTEAD OF INSERT ... EXECUTE FUNCTION` trigger on `target`. PostgreSQL's
/// form, and the combination the dialect corpus never drives.
fn execute_function_instead_of(name: &str, target: &str) -> serde_json::Value {
    json!({
        "op": "createTrigger", "name": name, "table": target,
        "timing": "insteadOf", "events": ["insert"], "forEach": "row",
        "action": { "kind": "executeFunction", "name": "f" },
    })
}

/// The body is schema-qualified because a trigger function runs under whatever
/// `search_path` the firing session has, and this probe's schema is not on it.
fn trigger_function(schema: &str) -> serde_json::Value {
    let table = format!("{}.\"t\"", quote_ident(schema));
    json!({
        "op": "createFunction", "name": "f", "returns": "trigger",
        "language": "plpgsql",
        "body": format!(
            "BEGIN INSERT INTO {table} (\"id\", \"a\") VALUES ('seeded', 'from the trigger'); RETURN NEW; END"
        ),
    })
}

// ---------------------------------------------------------------------------
// PostgreSQL
// ---------------------------------------------------------------------------

async fn pg_apply(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
    ir: &str,
    history: &mut Vec<Op>,
) -> Result<(), String> {
    let backend = PostgresBackend::new_generic(session);
    let policy = support::operator_charter(&cfg.project_schema);
    let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Postgres, &policy);
    // The charter is threaded as vendor authority: `createFunction` is a privileged
    // primitive, and the scope-derived fallback grants nothing outside an operator
    // posture, so without this the gate refuses before the trigger is reached.
    let document = zero_migrate::model::load::load_ir_document_authorized(
        ir,
        OWNER,
        zero_migrate::model::validate::Dialect::Postgres,
        &registry(),
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
    let plan = author
        .lower_plan(&document, &live)
        .map_err(|error| format!("lower: {error}"))?;
    MigrationEngine::new()
        .apply_plan(
            &plan.steps,
            Approval::Approved,
            &backend,
            cfg,
            OWNER,
            LockMode::Acquire,
        )
        .await
        .map_err(|error| format!("apply: {error}"))?;
    history.extend(document.ops.iter().cloned());
    Ok(())
}

#[compio::test]
async fn postgres_refuses_an_instead_of_trigger_on_a_table_and_keeps_one_on_a_view() {
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
        let mut setup = base_ops();
        setup.push(trigger_function(&cfg.project_schema));
        pg_apply(&session, &cfg, &envelope("setup", setup), &mut history).await?;

        // THE DEFECT, in the shape the corpus never drives: INSTEAD OF timing with
        // an EXECUTE FUNCTION action selects the `executeFunction` variant, which
        // PostgreSQL declares portable.
        let on_table = envelope(
            "instead_of_on_table",
            vec![execute_function_instead_of("tg_bad", "t")],
        );
        match pg_apply(&session, &cfg, &on_table, &mut history.clone()).await {
            Ok(()) => {
                return Err(
                    "an INSTEAD OF trigger on a TABLE must be refused by the engine, but \
                     PostgreSQL was handed the statement"
                        .into(),
                )
            }
            Err(refusal) => {
                if !refusal.contains("INSTEAD OF") || !refusal.contains("view") {
                    return Err(format!(
                        "the refusal must name INSTEAD OF and the view requirement; got: {refusal}"
                    ));
                }
                if refusal.contains("apply:") {
                    return Err(format!(
                        "the refusal must arrive at lower, before the server: {refusal}"
                    ));
                }
            }
        }

        // OVER-REFUSAL CONTROL - the same trigger on the VIEW still applies, and
        // still fires.
        pg_apply(
            &session,
            &cfg,
            &envelope(
                "instead_of_on_view",
                vec![execute_function_instead_of("tg_good", "v")],
            ),
            &mut history,
        )
        .await?;
        session
            .batch(&format!(
                "INSERT INTO {quoted_schema}.\"v\" (\"id\", \"a\") VALUES ('ignored', 'ignored')"
            ))
            .await
            .map_err(|error| format!("insert through the view: {error}"))?;
        let seeded: i64 = session
            .query_one(
                &format!("SELECT count(*) AS n FROM {quoted_schema}.\"t\" WHERE \"id\" = 'seeded'"),
                &[],
            )
            .await
            .map_err(|error| format!("count the trigger's row: {error}"))?
            .try_get("n")
            .map_err(|error| format!("decode the trigger's row count: {error}"))?;
        if seeded != 1 {
            return Err(format!(
                "the INSTEAD OF trigger on the view must have written the base row, got {seeded}"
            ));
        }
        Ok(())
    }
    .await;

    session
        .batch(&format!("DROP SCHEMA IF EXISTS {quoted_schema} CASCADE"))
        .await
        .expect("drop the isolated test schema");
    work.expect("the PostgreSQL INSTEAD OF gate and its over-refusal control");
}

// ---------------------------------------------------------------------------
// SQLite
// ---------------------------------------------------------------------------

async fn sqlite_apply(
    backend: &SqliteBackend,
    cfg: &ExecutorConfig,
    ir: &str,
    history: &mut Vec<Op>,
) -> Result<(), String> {
    let policy = support::operator_charter(&cfg.project_schema);
    let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Sqlite, &policy);
    let document = zero_migrate::model::load::load_ir_document(
        ir,
        OWNER,
        zero_migrate::model::validate::Dialect::Sqlite,
        &registry(),
        None,
    )
    .map_err(|error| format!("load gate (sqlite): {error}"))?;
    let folded = fold_ops(history, SqlDialect::Sqlite, &cfg.project_schema, &policy)
        .map_err(|error| format!("fold the applied history: {error}"))?;
    let live = LiveSchema::from_catalog_snapshot(folded, OWNER);
    let plan = author
        .lower_plan(&document, &live)
        .map_err(|error| format!("lower: {error}"))?;
    MigrationEngine::new()
        .apply_plan(
            &plan.steps,
            Approval::Approved,
            backend,
            cfg,
            OWNER,
            LockMode::Acquire,
        )
        .await
        .map_err(|error| format!("apply: {error}"))?;
    history.extend(document.ops.iter().cloned());
    Ok(())
}

#[compio::test]
async fn sqlite_refuses_an_instead_of_trigger_on_a_table_and_keeps_one_on_a_view() {
    let dir = tempfile::tempdir().expect("temp dir");
    let app = dir.path().join("probe.sqlite");
    let journal = dir.path().join("probe.migrations.sqlite");
    let backend = SqliteBackend::open(&app, &journal).expect("open the probe database");
    let cfg = ExecutorConfig::new("main", "main", support::operator_charter("main"));
    backend
        .ensure_journal(&cfg)
        .await
        .expect("ensure migration journal");

    let mut history: Vec<Op> = Vec::new();
    sqlite_apply(&backend, &cfg, &envelope("setup", base_ops()), &mut history)
        .await
        .expect("create the base table and the view");

    // THE DEFECT: declared portable, clears validate, clears lower, and before the
    // gate below existed SQLite answered
    // `cannot create INSTEAD OF trigger on table: t`.
    let refusal = sqlite_apply(
        &backend,
        &cfg,
        &envelope("instead_of_on_table", vec![body_instead_of("tg_bad", "t")]),
        &mut history.clone(),
    )
    .await
    .expect_err("an INSTEAD OF trigger on a TABLE must be refused by the engine");
    assert!(
        refusal.contains("INSTEAD OF") && refusal.contains("view"),
        "the refusal must name INSTEAD OF and the view requirement; got: {refusal}"
    );
    assert!(
        !refusal.contains("apply:"),
        "the refusal must arrive at lower, before the server: {refusal}"
    );

    // OVER-REFUSAL CONTROL - SQLite genuinely supports this on a view, and it is
    // the only reason INSTEAD OF exists there.
    sqlite_apply(
        &backend,
        &cfg,
        &envelope("instead_of_on_view", vec![body_instead_of("tg_good", "v")]),
        &mut history,
    )
    .await
    .expect("an INSTEAD OF trigger on a VIEW must still apply on SQLite");

    backend
        .actor()
        .exec("INSERT INTO \"v\" (\"id\", \"a\") VALUES ('ignored', 'ignored')")
        .await
        .expect("insert through the view");
    let rows = backend
        .actor()
        .query("SELECT count(*) FROM \"t\" WHERE \"id\" = 'seeded'")
        .await
        .expect("count the trigger's row");
    let seeded = rows
        .first()
        .and_then(|row| row.first())
        .cloned()
        .flatten()
        .unwrap_or_default();
    assert_eq!(
        seeded, "1",
        "the INSTEAD OF trigger on the view must have written the base row"
    );
}
