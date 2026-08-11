//! Rolling back a dropped extension restores it, proven against live `PostgreSQL`.
//!
//! `Op::DropExtension` lowers with no `down`, so a rollback leaves the extension
//! gone. The history already records what is needed to put it back:
//! `ExtensionSnapshot` carries the placement schema, which is the only argument
//! `CREATE EXTENSION ... WITH SCHEMA` takes beyond the name.
//!
//! The guard clause here is NOT the `ExistenceGuard` mechanism the view and
//! sequence arms use. `Op::existence_guard()` returns `None` for every vendor op
//! by design, so a guarded drop has to be recognised from the op's own
//! `if_exists` field. Reading it the other way would classify `ifExists` as
//! unguarded and re-create an extension that may never have been dropped.
//!
//! GATED behind `ZERO_MIGRATE_TEST_PG_URL`. Assertions read the live catalog
//! through `snapshot_schema`, which introspects `pg_extension`.

mod support;

use std::collections::BTreeMap;

use support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::apply::executor::{
    rollback, LockMode, RollbackError, RollbackRequest, RollbackTarget,
};
use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::Op;
use zero_migrate::model::migration::Migration;
use zero_migrate::model::snapshot::ExtensionSnapshot;
use zero_migrate::render::step::PlanStep;
use zero_migrate::{
    fold_ops, guard_for, snapshot_schema, Approval, EffectivePolicy, ExecutorConfig, GuardConfig,
    IrAuthor, LiveSchema, MigrationEngine, PostgresBackend, SqlDialect,
};

const OWNER: &str = "app_drop_extension_rollback_pg";
/// Extension names are unique per DATABASE, not per schema, so the two tests here
/// cannot share one: `pg_extension_name_index` rejects the second creator with
/// "duplicate key value violates unique constraint". Per-schema isolation, which
/// is enough for tables and sequences, does not isolate an extension.
const EXT: &str = "citext";
const EXT_GUARDED: &str = "pgcrypto";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "drop_extension_rollback_pg_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// Vendor ops need the operator/platform capability set, not a creator profile.
/// Granting `code.extension` alone is not enough: the load gate refuses the op
/// outright under a confined profile with "vendor PG primitive (op capability
/// \"extension\") requires the allowExtension capability, which the active
/// (Confined creator) capability set does not grant". `operator_charter` carries
/// both the profile and the extension allowlist.
fn policy(schema: &str) -> EffectivePolicy {
    support::operator_charter(schema)
}

fn create_doc(ext: &str, schema: &str) -> String {
    serde_json::json!({
        "ir_version": 1,
        "name": format!("create_{ext}"),
        "owner_app": OWNER,
        "ops": [{"op": "createExtension", "name": ext, "schema": schema}]
    })
    .to_string()
}

fn drop_doc(ext: &str, guarded: bool) -> String {
    let mut op = serde_json::json!({"op": "dropExtension", "name": ext});
    if guarded {
        op["ifExists"] = serde_json::json!(true);
    }
    serde_json::json!({
        "ir_version": 1,
        "name": if guarded { format!("drop_{ext}_if_present") } else { format!("drop_{ext}") },
        "owner_app": OWNER,
        "ops": [op]
    })
    .to_string()
}

/// Apply one IR doc through the real lower + apply path against live `PostgreSQL`.
///
/// `history` accumulates the applied op stream and is folded into the live schema
/// the way the deploy path does it (`refresh_historical_live`, engine.rs:390-392).
async fn apply_doc(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
    ir: &str,
    history: &mut Vec<Op>,
    approval: Approval,
) -> Result<Vec<Migration>, String> {
    let backend = PostgresBackend::new_generic(session);
    let pol = policy(&cfg.project_schema);
    let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Postgres, &pol);
    let guard = GuardConfig::from_policy(pol.clone(), SqlDialect::Postgres);
    let folded = fold_ops(history, SqlDialect::Postgres, &cfg.project_schema, &pol)
        .map_err(|error| format!("fold the applied history: {error}"))?;
    let live = LiveSchema::from_catalog_snapshot(folded, OWNER);
    // Go through the guarded deploy entry rather than hand-rolling load + lower.
    // A vendor op needs the author's own `VendorAuthority`, which only these
    // entries pass; a bare `load_ir_document` refuses every privileged primitive
    // as "unreachable from a confined migration by construction".
    let artifact = author
        .load_and_lower_guarded(ir, OWNER, &BTreeMap::new(), &live, &guard)
        .map_err(|error| format!("load and lower the guarded plan: {error}"))?;
    let authored: zero_migrate::MigrationIr =
        serde_json::from_str(ir).map_err(|error| format!("parse the authored IR: {error}"))?;
    history.extend(authored.ops);
    MigrationEngine::new()
        .apply_plan(
            &artifact.plan.steps,
            approval,
            &backend,
            cfg,
            OWNER,
            LockMode::Acquire,
        )
        .await
        .map_err(|error| format!("apply the authored plan on PostgreSQL: {error}"))?;
    Ok(artifact
        .plan
        .steps
        .iter()
        .filter_map(|step| match step {
            PlanStep::Ddl(m) => Some(m.clone()),
            _ => None,
        })
        .collect())
}

/// The extension as `pg_extension` reports it, or `None` when absent.
async fn live_extension(
    session: &PgDevSession,
    schema: &str,
    ext: &str,
) -> Result<Option<ExtensionSnapshot>, String> {
    let snapshot = snapshot_schema(session, schema)
        .await
        .map_err(|error| format!("snapshot the live PostgreSQL schema: {error}"))?;
    Ok(snapshot.extensions.get(ext).cloned())
}

fn pg_guard(cfg: &ExecutorConfig) -> Box<dyn zero_migrate::MigrationGuard> {
    guard_for(&GuardConfig::from_policy(
        policy(&cfg.project_schema),
        SqlDialect::Postgres,
    ))
}

#[compio::test]
async fn rolling_back_a_dropped_extension_restores_it() {
    let url = skip_if_no_pg!();
    let ext = EXT;
    let session = PgDevSession::connect(&url);
    let schema = token();
    let cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy(&schema));
    let quoted_schema = quote_ident(&cfg.project_schema);
    let quoted_meta_schema = quote_ident(&cfg.pg.meta_schema);
    // Both schemas, dropped on an unwind that skips the explicit cleanup below.
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
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
            &create_doc(ext, &cfg.project_schema),
            &mut history,
            Approval::None,
        )
        .await?;

        let before = live_extension(&session, &cfg.project_schema, ext)
            .await?
            .ok_or_else(|| "the extension must exist before it is dropped".to_string())?;

        migrations.extend(
            apply_doc(
                &session,
                &cfg,
                &drop_doc(ext, false),
                &mut history,
                Approval::Approved,
            )
            .await?,
        );
        if live_extension(&session, &cfg.project_schema, ext)
            .await?
            .is_some()
        {
            return Err("the drop must actually remove the extension".into());
        }

        let request = RollbackRequest::new(RollbackTarget::Steps(1));
        rollback(
            &backend,
            &cfg,
            &request,
            &migrations,
            Approval::Approved,
            OWNER,
            pg_guard(&cfg).as_ref(),
        )
        .await
        .map_err(|error| format!("rolling back the dropped extension must succeed: {error}"))?;

        let after = live_extension(&session, &cfg.project_schema, ext)
            .await?
            .ok_or_else(|| "rolling back the drop must put the extension back".to_string())?;
        if after != before {
            return Err(format!(
                "the restored extension must carry the placement it had before the drop\n  before: {before:?}\n   after: {after:?}"
            ));
        }
        Ok(())
    }
    .await;

    let cleanup = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {quoted_schema} CASCADE; \
             DROP SCHEMA IF EXISTS {quoted_meta_schema} CASCADE"
        ))
        .await;
    match (work, cleanup) {
        (Ok(()), Ok(())) => {}
        (Err(work), Ok(())) => panic!("{work}"),
        (Ok(()), Err(cleanup)) => panic!("drop PostgreSQL test schemas: {cleanup}"),
        (Err(work), Err(cleanup)) => panic!("{work}; cleanup failed: {cleanup}"),
    }
}

/// A guarded drop keeps no inverse. This is the case that would silently break if
/// the vendor arm read `Op::existence_guard()` the way the view and sequence arms
/// do: that accessor returns `None` for every vendor op, so `ifExists` would look
/// unguarded and earn an inverse it must not have.
#[compio::test]
async fn a_guarded_extension_drop_keeps_no_inverse() {
    let url = skip_if_no_pg!();
    let ext = EXT_GUARDED;
    let session = PgDevSession::connect(&url);
    let schema = token();
    let cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy(&schema));
    let quoted_schema = quote_ident(&cfg.project_schema);
    let quoted_meta_schema = quote_ident(&cfg.pg.meta_schema);
    // Both schemas, dropped on an unwind that skips the explicit cleanup below.
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
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
            &create_doc(ext, &cfg.project_schema),
            &mut history,
            Approval::None,
        )
        .await?;
        migrations.extend(
            apply_doc(
                &session,
                &cfg,
                &drop_doc(ext, true),
                &mut history,
                Approval::Approved,
            )
            .await?,
        );

        let request = RollbackRequest::new(RollbackTarget::Steps(1));
        let error = rollback(
            &backend,
            &cfg,
            &request,
            &migrations,
            Approval::Approved,
            OWNER,
            pg_guard(&cfg).as_ref(),
        )
        .await
        .err()
        .ok_or_else(|| "a guarded drop must not be reversible".to_string())?;

        if !matches!(error, RollbackError::Irreversible { .. }) {
            return Err(format!(
                "expected the planner to refuse the guarded drop as irreversible, got {error:?}"
            ));
        }
        Ok(())
    }
    .await;

    let cleanup = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {quoted_schema} CASCADE; \
             DROP SCHEMA IF EXISTS {quoted_meta_schema} CASCADE"
        ))
        .await;
    match (work, cleanup) {
        (Ok(()), Ok(())) => {}
        (Err(work), Ok(())) => panic!("{work}"),
        (Ok(()), Err(cleanup)) => panic!("drop PostgreSQL test schemas: {cleanup}"),
        (Err(work), Err(cleanup)) => panic!("{work}; cleanup failed: {cleanup}"),
    }
}
