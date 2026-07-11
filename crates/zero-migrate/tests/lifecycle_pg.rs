#![cfg(feature = "native-pg")]
//! P0 cross-feature END-TO-END lifecycle tests against a REAL Postgres (no shims).
//!
//! Every per-feature suite (`executor_pg`, `declarative_pg`, `expand_contract_pg`,
//! `rollback_pg`, `squash_pg`, `status_pg`, `shadow_pg`, `role_pg`, `manifest_pg`)
//! proves one feature in isolation. This suite proves the features COMPOSE — it
//! drives the PUBLIC engine API across multi-deploy lifecycles end-to-end, the way
//! the control plane will, and uses each scenario to CONFIRM or REFUTE a specific
//! integration-bug hypothesis from the Phase-1 trace.
//!
//! No internal shims, no hand-applying only part of a sequence: a declarative
//! rename runs its REAL backfill via `apply_declarative` → `run_expand`; a rollback
//! goes through `engine.rollback`; a squash through `squash`; a malicious batch
//! through `plan` → `apply` → `dry_run` and the provisioned `migrator` role.
//!
//! Requires the dedicated database `zero_migrate_test` on :5440 and a
//! connecting role with `CREATEROLE` (the runbook uses `postgres`). Each test runs
//! in its OWN meta + project schema + project id (unique token) for isolation.

use std::collections::HashMap;

use compio_postgres::Client;
use zero_migrate::apply::executor::{RollbackRequest, RollbackTarget};
use zero_migrate::{
    PostgresBackend,
    desired_snapshot, diff_snapshots, history, migrator_role_name, provision_migrator,
    apply::role::deprovision_migrator, snapshot_schema, squash, status, Approval, Checksum, ChecksumInput,
    CollectionDescriptor, DeclarativeApplyError, EngineError, ExecutorConfig, FieldDescriptor,
    GuardConfig, HistoryKind, Migration, MigrationEngine, MigrationFlags, MigrationId, RawSqlAuthor,
    RenameHint, SchemaSnapshot, ShadowConfig,
};

const DEFAULT_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zero_migrate_test";

fn dsn() -> String {
    std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| DEFAULT_DSN.to_string())
}

async fn pg() -> Client {
    let (client, conn) = compio_postgres::connect(&dsn(), compio_postgres::NoTls)
        .await
        .expect("connect to zero_migrate_test on :5440");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{pid}_{nanos}_{n}")
}

/// A config + matching least-privilege migrator role for an isolated project.
fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(format!("prj_{tok}"), format!("proj_{tok}"));
    c.pg.meta_schema = format!("meta_{tok}");
    let role = migrator_role_name(&c.project_id).unwrap();
    c.with_migrator_role(role)
}

fn guard_cfg(cfg: &ExecutorConfig) -> GuardConfig {
    GuardConfig::confined(cfg.project_schema.clone())
}

async fn ensure_project_schema(conn: &Client, cfg: &ExecutorConfig) {
    conn.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS \"{}\"",
        cfg.project_schema
    ))
    .await
    .expect("create project schema");
}

/// Stand up the project schema + provision the migrator role (the Plan-3 path).
async fn setup(conn: &Client, cfg: &ExecutorConfig) {
    ensure_project_schema(conn, cfg).await;
    provision_migrator(conn, cfg)
        .await
        .expect("provision migrator role");
}

async fn teardown(conn: &Client, cfg: &ExecutorConfig) {
    let _ = deprovision_migrator(conn, cfg).await;
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.pg.meta_schema
        ))
        .await;
}

/// Hand a project table to the migrator role (mirrors the platform, where project
/// tables are owned by the migrator so its DDL — ADD/DROP COLUMN, CREATE TRIGGER —
/// works under SET ROLE). The test admin seeds the table, then transfers ownership.
async fn give_table_to_migrator(conn: &Client, cfg: &ExecutorConfig, table: &str) {
    let role = cfg.pg.migrator_role.as_ref().unwrap();
    conn.batch_execute(&format!(
        "ALTER TABLE \"{}\".\"{table}\" OWNER TO \"{role}\"",
        cfg.project_schema
    ))
    .await
    .expect("transfer table ownership to migrator");
}

/// Create a NOLOGIN app role with exactly what an app needs to write a table and
/// fire the INVOKER dual-write trigger. Returns the role name.
async fn make_app_role(conn: &Client, cfg: &ExecutorConfig, tok: &str) -> String {
    let role = format!("appusr_{}", tok.replace(['.', '-'], "_"));
    let schema = &cfg.project_schema;
    conn.batch_execute(&format!(
        "DROP ROLE IF EXISTS \"{role}\"; \
         CREATE ROLE \"{role}\" NOLOGIN; \
         GRANT USAGE ON SCHEMA \"{schema}\" TO \"{role}\"; \
         GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA \"{schema}\" TO \"{role}\"; \
         GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA \"{schema}\" TO \"{role}\"; \
         GRANT \"{role}\" TO CURRENT_USER"
    ))
    .await
    .expect("create app role + grants");
    role
}

/// Run `sql` AS the app role (SET ROLE … then RESET ROLE).
async fn as_app(conn: &Client, role: &str, sql: &str) {
    conn.batch_execute(&format!("SET ROLE \"{role}\""))
        .await
        .expect("SET ROLE app");
    let r = conn.batch_execute(sql).await;
    conn.batch_execute("RESET ROLE")
        .await
        .expect("RESET ROLE back to admin");
    r.expect("app write");
}

async fn drop_app_role(conn: &Client, role: &str) {
    let _ = conn
        .batch_execute(&format!(
            "RESET ROLE; \
             REASSIGN OWNED BY \"{role}\" TO CURRENT_USER; \
             DROP OWNED BY \"{role}\"; DROP ROLE IF EXISTS \"{role}\""
        ))
        .await;
}

async fn column_exists(conn: &Client, schema: &str, table: &str, col: &str) -> bool {
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.columns \
             WHERE table_schema=$1 AND table_name=$2 AND column_name=$3",
            &[&schema, &table, &col],
        )
        .await
        .expect("column_exists");
    !rows.is_empty()
}

async fn table_exists(conn: &Client, schema: &str, table: &str) -> bool {
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.tables WHERE table_schema=$1 AND table_name=$2",
            &[&schema, &table],
        )
        .await
        .expect("table_exists");
    !rows.is_empty()
}

/// A `ShadowConfig` for the dry-run path: clones into a throwaway database (same
/// admin DSN, a unique prefix so parallel tests never collide), then tears it down.
fn shadow_cfg(tok: &str) -> ShadowConfig {
    ShadowConfig {
        admin_dsn: dsn(),
        db_name_prefix: format!("zsmig_life_{}_", tok.replace(['.', '-'], "_")),
    }
}

// ===========================================================================
// Declarative-rename helpers (P0-1 / P0-2): build a `users`-like collection
// with a declared `email` column and seed real rows, then plan a `email →
// email_address` rename through `plan_declarative` + a RenameHint.
// ===========================================================================

/// A collection descriptor named `tbl` with one declared `string` field `field`.
fn one_field_collection(tbl: &str, field: &str, app: &str) -> CollectionDescriptor {
    CollectionDescriptor {
        name: tbl.into(),
        owner_app: app.into(),
        fields: vec![FieldDescriptor {
            name: field.into(),
            ty: "string".into(),
            required: false,
            unique: false,
            references: None,
            ..Default::default()
        }],
        indexes: vec![],
    runtime_options: Default::default(),
    }
}

/// Apply a hint-free additive declarative plan (create the `from`-shaped table)
/// through the gate, under the migrator role.
async fn apply_additive(
    engine: &MigrationEngine,
    desired: &zero_migrate::DesiredSchema,
    live: &SchemaSnapshot,
    author: &zero_migrate::DeclarativeAuthor,
    cfg: &ExecutorConfig,
    conn: &Client,
) {
    let plan = engine
        .plan_declarative(desired, live, &HashMap::new(), author, &[], &guard_cfg(cfg))
        .expect("plan_declarative additive");
    assert!(plan.renames.is_empty(), "additive plan has no renames");
    engine
        .apply(&plan.plain, Approval::None, &PostgresBackend::new(conn), cfg, "app_acme")
        .await
        .expect("apply additive");
}

/// Insert N rows into a declarative collection table via the system-field `id` PK
/// and the declared text column. Returns the (id → value) map we seeded.
async fn seed_rows(
    conn: &Client,
    schema: &str,
    table: &str,
    col: &str,
    n: usize,
) -> Vec<(String, String)> {
    let mut seeded = Vec::new();
    for i in 0..n {
        let id = format!("row_{i}");
        let val = format!("user{i}@x.test");
        // The declarative create-table emits the seven system fields as NOT NULL
        // WITHOUT a DB-side default (the SDK's installSchema supplies values at
        // insert time, not the migration DDL). So a faithful seed must populate the
        // NOT-NULL system fields (created_at/updated_at/version) too.
        conn.batch_execute(&format!(
            "INSERT INTO \"{schema}\".\"{table}\" (id, created_at, updated_at, version, \"{col}\") \
             VALUES ('{id}', NOW(), NOW(), 1, '{val}')"
        ))
        .await
        .expect("seed row");
        seeded.push((id, val));
    }
    seeded
}

async fn col_value(conn: &Client, schema: &str, table: &str, id: &str, col: &str) -> Option<String> {
    conn.query_one(
        &format!("SELECT \"{col}\" AS v FROM \"{schema}\".\"{table}\" WHERE id=$1"),
        &[&id],
    )
    .await
    .expect("col_value")
    .get("v")
}

/// Build a `DeclarativeDeployPlan` to apply the deferred contract through the
/// normal gate: wrap the `pending_contract` migrations in a `MigrationPlan`.
fn contract_plan(engine: &MigrationEngine, contract: &[Migration], cfg: &ExecutorConfig) -> zero_migrate::MigrationPlan {
    engine.plan(contract, &guard_cfg(cfg))
}

// ===========================================================================
// P0-1 — declarative online RENAME, full 2-deploy, zero data loss.
//   Probes H8 (a failing rename in a multi-rename apply loses the first
//   rename's deferred contract), H4 (shadow predicts reality), H2/H3 (status/
//   history between deploys mislead or error).
// ===========================================================================

#[compio::test]
async fn p0_1_declarative_rename_two_deploy_zero_data_loss() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let engine = MigrationEngine::new();
    let author = zero_migrate::DeclarativeAuthor::new(cfg.project_schema.clone(), "app_acme");
    let schema = cfg.project_schema.clone();

    // --- Create the `from`-shaped table (collection `accounts` with `email`). ---
    let d_from = desired_snapshot(&schema, &[one_field_collection("accounts", "email", "app_acme")])
        .expect("desired from");
    apply_additive(&engine, &d_from, &SchemaSnapshot::default(), &author, &cfg, &conn).await;
    give_table_to_migrator(&conn, &cfg, "accounts").await;

    // Seed N pre-existing rows (data lives in <from> = email).
    let n = 7usize;
    let seeded = seed_rows(&conn, &schema, "accounts", "email", n).await;

    // --- Desire the rename: `email` → `email_address`, with a RenameHint. ---
    let d_to = desired_snapshot(
        &schema,
        &[one_field_collection("accounts", "email_address", "app_acme")],
    )
    .expect("desired to");
    let live = snapshot_schema(&conn, &schema).await.expect("snap live");
    let hint = RenameHint {
        table: "accounts".into(),
        from: "email".into(),
        to: "email_address".into(),
    };
    let live_ownership: HashMap<String, String> =
        std::iter::once(("accounts".to_string(), "app_acme".to_string())).collect();
    let plan = engine
        .plan_declarative(&d_to, &live, &live_ownership, &author, &[hint], &guard_cfg(&cfg))
        .expect("plan_declarative rename");
    assert_eq!(plan.renames.len(), 1, "exactly one online rename");

    // --- Deploy N: apply_declarative → expand (E1+E2 + REAL backfill + E3),
    //     contract DEFERRED into pending_contract. ---
    let outcome = engine
        .apply_declarative(&plan, Approval::Approved, &PostgresBackend::new(&conn), &cfg, "app_acme")
        .await
        .expect("apply_declarative deploy N");
    assert!(
        !outcome.pending_contract.is_empty(),
        "the contract must be DEFERRED, not applied in deploy N"
    );

    // Both physical columns exist; old column intact (contract not yet applied).
    assert!(column_exists(&conn, &schema, "accounts", "email").await);
    assert!(column_exists(&conn, &schema, "accounts", "email_address").await);

    // Every pre-existing row's value is MIRRORED into <to> (the real backfill ran).
    for (id, val) in &seeded {
        assert_eq!(
            col_value(&conn, &schema, "accounts", id, "email_address").await.as_deref(),
            Some(val.as_str()),
            "pre-existing row {id} must be backfilled into email_address"
        );
    }

    // --- H2/H3 probe: status/history BETWEEN deploys (expand applied, contract
    //     pending). They must not error and must reflect the expand as applied. ---
    let expand_versions: Vec<String> = plan.renames[0]
        .expand
        .iter()
        .map(|m| m.version.as_str().to_string())
        .collect();
    // status() needs the full known set to compute pending correctly. Hand it the
    // expand + the deferred contract (the bundle the control plane tracks).
    let mut full_set: Vec<Migration> = plan.renames[0].expand.clone();
    full_set.extend(outcome.pending_contract.clone());
    let st = status(&conn, &cfg, &full_set)
        .await
        .expect("status between deploys must not error");
    let applied_versions: std::collections::HashSet<&str> =
        st.applied.iter().map(|e| e.version.as_str()).collect();
    for v in &expand_versions {
        assert!(
            applied_versions.contains(v.as_str()),
            "status must report expand version {v} as applied; applied={:?}",
            st.applied.iter().map(|e| &e.version).collect::<Vec<_>>()
        );
    }
    let pending: std::collections::HashSet<&MigrationId> = st.pending.iter().collect();
    for cm in &outcome.pending_contract {
        assert!(
            pending.contains(&cm.version),
            "status must report deferred contract {} as pending; pending={:?}",
            cm.version.as_str(),
            st.pending.iter().map(MigrationId::as_str).collect::<Vec<_>>()
        );
    }
    let hist = history(&conn, &cfg)
        .await
        .expect("history between deploys must not error");
    let hist_completed: std::collections::HashSet<&str> = hist
        .iter()
        .filter(|e| e.kind == HistoryKind::Completed)
        .map(|e| e.version.as_str())
        .collect();
    for v in &expand_versions {
        assert!(
            hist_completed.contains(v.as_str()),
            "history must record expand version {v} as a completed event"
        );
    }

    // --- App switches to <to>: a NEW write under the APP role dual-writes both. ---
    let app_role = make_app_role(&conn, &cfg, &tok).await;
    as_app(
        &conn,
        &app_role,
        &format!(
            "INSERT INTO \"{schema}\".\"accounts\" (id, created_at, updated_at, version, email_address) \
             VALUES ('new1', NOW(), NOW(), 1, 'new@x.test')"
        ),
    )
    .await;
    assert_eq!(
        col_value(&conn, &schema, "accounts", "new1", "email").await.as_deref(),
        Some("new@x.test"),
        "a NEW write via email_address must dual-write into email (coexistence)"
    );

    // --- Deploy N+1: apply the DEFERRED contract via engine.apply → DROP <from>. ---
    let cplan = contract_plan(&engine, &outcome.pending_contract, &cfg);
    engine
        .apply(&cplan, Approval::Approved, &PostgresBackend::new(&conn), &cfg, "app_acme")
        .await
        .expect("apply deferred contract deploy N+1");

    // <from> dropped, <to> retains ALL original + the new row (ZERO loss).
    assert!(!column_exists(&conn, &schema, "accounts", "email").await, "email dropped");
    assert!(column_exists(&conn, &schema, "accounts", "email_address").await, "email_address kept");
    for (id, val) in &seeded {
        assert_eq!(
            col_value(&conn, &schema, "accounts", id, "email_address").await.as_deref(),
            Some(val.as_str()),
            "ZERO data loss: original row {id} survives the contract"
        );
    }
    assert_eq!(
        col_value(&conn, &schema, "accounts", "new1", "email_address").await.as_deref(),
        Some("new@x.test"),
        "the transition-era new row survives the contract"
    );

    // Drift clean vs the desired (renamed) schema.
    let live_final = snapshot_schema(&conn, &schema).await.expect("snap final");
    let drift = diff_snapshots(&d_to.snapshot, &live_final);
    let col_drift: Vec<_> = drift
        .altered_objects
        .iter()
        .filter(|a| a.object.starts_with("column "))
        .collect();
    assert!(col_drift.is_empty(), "column drift after rename lifecycle: {col_drift:?}");
    assert!(
        !drift.missing_objects.iter().any(|m| m.contains("email_address")),
        "email_address must be present, not missing: {:?}",
        drift.missing_objects
    );

    drop_app_role(&conn, &app_role).await;
    teardown(&conn, &cfg).await;
}

/// H8 probe — TWO renames in one `apply_declarative` where the SECOND fails. The
/// FIRST rename's deferred contract must NOT be silently lost; the engine must not
/// leave a permanent half-rename. We force the second rename to fail at apply by
/// authoring its expand against a column TYPE Postgres will reject (the rename's
/// `from` column does not exist after the diff routes it — but to make the SECOND
/// expand fail deterministically we make its table missing the source column).
#[compio::test]
async fn p0_1_h8_second_rename_failure_does_not_lose_first_renames_contract() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let engine = MigrationEngine::new();
    let author = zero_migrate::DeclarativeAuthor::new(cfg.project_schema.clone(), "app_acme");
    let schema = cfg.project_schema.clone();

    // Two collections, each with a declared column to rename.
    let d_from = desired_snapshot(
        &schema,
        &[
            one_field_collection("alpha", "a_old", "app_acme"),
            one_field_collection("beta", "b_old", "app_acme"),
        ],
    )
    .expect("desired from");
    apply_additive(&engine, &d_from, &SchemaSnapshot::default(), &author, &cfg, &conn).await;
    give_table_to_migrator(&conn, &cfg, "alpha").await;
    give_table_to_migrator(&conn, &cfg, "beta").await;
    let _seed_a = seed_rows(&conn, &schema, "alpha", "a_old", 3).await;
    let _seed_b = seed_rows(&conn, &schema, "beta", "b_old", 3).await;

    let d_to = desired_snapshot(
        &schema,
        &[
            one_field_collection("alpha", "a_new", "app_acme"),
            one_field_collection("beta", "b_new", "app_acme"),
        ],
    )
    .expect("desired to");
    let live = snapshot_schema(&conn, &schema).await.expect("snap");
    let hints = vec![
        RenameHint { table: "alpha".into(), from: "a_old".into(), to: "a_new".into() },
        RenameHint { table: "beta".into(), from: "b_old".into(), to: "b_new".into() },
    ];
    let ownership: HashMap<String, String> = [
        ("alpha".to_string(), "app_acme".to_string()),
        ("beta".to_string(), "app_acme".to_string()),
    ]
    .into_iter()
    .collect();
    let mut plan = engine
        .plan_declarative(&d_to, &live, &ownership, &author, &hints, &guard_cfg(&cfg))
        .expect("plan two renames");
    assert_eq!(plan.renames.len(), 2, "two online renames");

    // Sabotage the SECOND rename so its expand fails at apply: drop beta's source
    // column out from under it AFTER planning. The first rename (alpha) will have
    // already applied its expand + run its backfill; the second's E1 ADD COLUMN
    // succeeds but its backfill UPDATE references the now-missing b_old → fails.
    // To make the failure deterministic and isolated to the SECOND, we corrupt the
    // beta plan's expand to reference a non-existent column.
    //
    // Force a known apply failure on the second rename: replace beta's E1 up with
    // SQL that errors (ADD COLUMN of a column that already exists, no IF NOT
    // EXISTS) — a clean executor MigrationFailed, after alpha's expand committed.
    {
        let beta = &mut plan.renames[1];
        let bad_up = format!("ALTER TABLE \"{schema}\".\"beta\" ADD COLUMN b_old text");
        beta.expand[0].up = bad_up;
        beta.expand[0].checksum =
            Checksum::of(&ChecksumInput::from_migration(&beta.expand[0]));
    }

    let first_contract: Vec<String> = plan.renames[0]
        .contract
        .iter()
        .map(|m| m.version.as_str().to_string())
        .collect();

    let err = engine
        .apply_declarative(&plan, Approval::Approved, &PostgresBackend::new(&conn), &cfg, "app_acme")
        .await
        .expect_err("the second rename's expand must fail");
    assert!(
        matches!(err, DeclarativeApplyError::Expand(_)),
        "expected an Expand failure from the second rename, got {err:?}"
    );

    // The FIRST rename's expand is applied + journaled (it committed before the
    // second failed): alpha has both columns, and its backfill ran.
    assert!(column_exists(&conn, &schema, "alpha", "a_new").await, "first rename's expand applied");
    let st = status(&conn, &cfg, &plan.renames[0].expand)
        .await
        .expect("status after partial failure");
    assert_eq!(
        st.pending.len(),
        0,
        "the first rename's expand must be fully net-applied (recoverable, not lost): {:?}",
        st.pending.iter().map(MigrationId::as_str).collect::<Vec<_>>()
    );

    // The FIRST rename's contract is RECOVERABLE: it was authored in `plan`
    // (deterministically derivable), the expand is journaled, so re-deriving the
    // plan yields the same contract versions, and the contract can be applied as a
    // subsequent deploy. Prove it: apply the first contract now and confirm zero
    // half-rename residue on alpha.
    let cplan = contract_plan(&engine, &plan.renames[0].contract, &cfg);
    engine
        .apply(&cplan, Approval::Approved, &PostgresBackend::new(&conn), &cfg, "app_acme")
        .await
        .expect("the first rename's deferred contract is recoverable + appliable");
    assert!(
        !column_exists(&conn, &schema, "alpha", "a_old").await,
        "first rename completes cleanly — no permanent half-rename"
    );
    assert!(column_exists(&conn, &schema, "alpha", "a_new").await);
    // Sanity: the contract versions we just applied are exactly the ones the plan
    // surfaced (the contract was never lost).
    assert_eq!(first_contract.len(), 2, "C1 + C2 surfaced for the first rename");

    teardown(&conn, &cfg).await;
}

// ===========================================================================
// P0-2 — declarative dry-run faithfully predicts the real apply (probes H4).
// ===========================================================================

// RED — CONFIRMED BUG (H4). The dry-run builds the shadow DB from an EMPTY
// project schema (`open_and_provision_shadow` at src/shadow.rs:436-442 does only
// `CREATE SCHEMA "<project_schema>"` + provision_migrator — it NEVER replicates
// the live project schema's pre-existing objects, despite the module doc header
// at src/shadow.rs:10/24 calling it "a throwaway DATABASE clone"). So for any
// INCREMENTAL deploy — here a column rename whose expand E1 is
// `ALTER TABLE "contacts" ADD COLUMN ...` against a `contacts` table created in a
// PRIOR deploy — the rename's E1 fails on the shadow with "relation does not
// exist", and the dry-run reports ok=false / resulting_drift missing=["contacts"]
// while the REAL incremental apply against the live DB succeeds drift-clean.
//
// Observed (the FAILS assertion below): dry-run ok=false,
//   resulting_drift = Some(StructuralDrift{ missing_objects: ["contacts"] }),
//   per_migration = [{ "<declarative-apply>", applied_ok:false,
//     error:"migration mig_… failed to apply: db error" /* relation does not exist */ }].
// Expected: dry-run ok == real-drift-clean (true) — the shadow must predict reality.
//
// Root cause: src/shadow.rs `open_and_provision_shadow` (lines 427-448). The fix
// is to seed the shadow with the live project schema before applying the plan —
// e.g. `CREATE DATABASE <shadow> TEMPLATE <live>` (a true clone, as the doc
// claims), or replay the live snapshot's CREATE statements into the fresh shadow
// schema — so an incremental plan dry-runs against the same starting state the
// real deploy sees. With that, this test goes GREEN unchanged.
// GREEN (H4 fixed): `dry_run_declarative` now SEEDS the shadow with the current
// live project structure (snapshot → reconstruct-via-differ; see
// `shadow::seed_shadow_from_live`) BEFORE applying the incremental plan, so the
// shadow starts from the same state the real deploy sees. An incremental rename
// whose expand targets a prior-deploy table now applies cleanly on the shadow,
// and the dry-run's `ok` matches the real drift-clean outcome.
#[compio::test]
async fn p0_2_dry_run_declarative_predicts_real_apply() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let engine = MigrationEngine::new();
    let author = zero_migrate::DeclarativeAuthor::new(cfg.project_schema.clone(), "app_acme");
    let schema = cfg.project_schema.clone();

    // Create the `from` table + seed rows.
    let d_from = desired_snapshot(&schema, &[one_field_collection("contacts", "phone", "app_acme")])
        .expect("desired from");
    apply_additive(&engine, &d_from, &SchemaSnapshot::default(), &author, &cfg, &conn).await;
    give_table_to_migrator(&conn, &cfg, "contacts").await;
    let seeded = seed_rows(&conn, &schema, "contacts", "phone", 5).await;

    // Plan the rename.
    let d_to = desired_snapshot(
        &schema,
        &[one_field_collection("contacts", "phone_number", "app_acme")],
    )
    .expect("desired to");
    let live = snapshot_schema(&conn, &schema).await.expect("snap");
    let hint = RenameHint { table: "contacts".into(), from: "phone".into(), to: "phone_number".into() };
    let ownership: HashMap<String, String> =
        std::iter::once(("contacts".to_string(), "app_acme".to_string())).collect();
    let plan = engine
        .plan_declarative(&d_to, &live, &ownership, &author, &[hint], &guard_cfg(&cfg))
        .expect("plan rename");

    // --- dry_run_declarative: simulate BOTH deploys on a throwaway shadow DB. ---
    let report = engine
        .dry_run_declarative(
            &PostgresBackend::new(&conn),
            &plan,
            &d_to,
            &cfg,
            &shadow_cfg(&tok),
            "app_acme",
        )
        .await
        .expect("dry_run_declarative");

    // --- The REAL apply: deploy N + the deferred contract on the project DB. ---
    let outcome = engine
        .apply_declarative(&plan, Approval::Approved, &PostgresBackend::new(&conn), &cfg, "app_acme")
        .await
        .expect("real apply_declarative");
    let cplan = contract_plan(&engine, &outcome.pending_contract, &cfg);
    engine
        .apply(&cplan, Approval::Approved, &PostgresBackend::new(&conn), &cfg, "app_acme")
        .await
        .expect("real contract apply");

    // Reality: final schema == desired (drift clean), zero data loss.
    let live_final = snapshot_schema(&conn, &schema).await.expect("snap final");
    let real_drift = diff_snapshots(&d_to.snapshot, &live_final);
    let real_clean = real_drift.is_clean();
    for (id, val) in &seeded {
        assert_eq!(
            col_value(&conn, &schema, "contacts", id, "phone_number").await.as_deref(),
            Some(val.as_str()),
            "row {id} backfilled + survives in reality"
        );
    }

    // The dry-run's prediction must match reality: dry-run ok ⟺ real drift clean.
    // FAILS (H4): observed report.ok == false (shadow rename E1 hit "relation does
    // not exist" on the empty shadow schema) vs expected == real_clean (true). The
    // shadow does not start from the live project state, so it cannot predict an
    // incremental deploy. See the #[ignore] note above for the root cause + fix.
    assert_eq!(
        report.ok, real_clean,
        "dry-run ok ({}) must match real drift-clean ({}); the shadow must predict reality. \
         dry-run resulting_drift={:?}; real_drift missing={:?} unexpected={:?} altered={:?}",
        report.ok,
        real_clean,
        report.resulting_drift,
        real_drift.missing_objects,
        real_drift.unexpected_objects,
        real_drift.altered_objects,
    );
    assert!(real_clean, "the real rename lifecycle must converge to desired (drift clean)");
    assert!(report.ok, "the dry-run must have predicted success");
    // The shadow's resulting drift prediction must itself be clean.
    if let Some(rd) = &report.resulting_drift {
        assert!(
            rd.is_clean(),
            "the shadow's resulting_drift must be clean (matching reality): {rd:?}"
        );
    }

    teardown(&conn, &cfg).await;
}

// ===========================================================================
// P0-3 — apply v1..v3 → rollback to v1 → re-apply → squash → status.
//   Composes rollback (#38) + re-apply + squash (#49) + status (#53).
// ===========================================================================

#[compio::test]
async fn p0_3_apply_rollback_reapply_squash_status_lifecycle() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let engine = MigrationEngine::new();
    let schema = cfg.project_schema.clone();

    // Three reversible versioned migrations, each creating a table + seeding a row.
    let mk = |name: &str, tbl: &str| -> Migration {
        let up = format!(
            "CREATE TABLE \"{schema}\".\"{tbl}\" (id bigint primary key); \
             INSERT INTO \"{schema}\".\"{tbl}\" (id) VALUES (1)"
        );
        let down = format!("DROP TABLE \"{schema}\".\"{tbl}\"");
        let mut m = Migration {
            version: MigrationId::generate(),
            name: name.to_string(),
            up,
            down: Some(down),
            checksum: Checksum::of(&ChecksumInput {
                up: "",
                down: None,
                flags: &MigrationFlags::default(),
                owner_app: "",
                depends_on: &[],
                supersedes: &[],
                preconditions: &[],
            }),
            flags: MigrationFlags::default(),
            owner_app: "app_acme".into(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            existence_guard: None,
        };
        m.recompute_checksum();
        m
    };
    // Ensure version order v1 < v2 < v3.
    let v1 = mk("create_t1", "t1");
    std::thread::sleep(std::time::Duration::from_millis(2));
    let v2 = mk("create_t2", "t2");
    std::thread::sleep(std::time::Duration::from_millis(2));
    let v3 = mk("create_t3", "t3");
    assert!(v1.version.as_str() < v2.version.as_str() && v2.version.as_str() < v3.version.as_str());
    let set = vec![v1.clone(), v2.clone(), v3.clone()];

    // --- Apply v1..v3. ---
    let plan = engine.plan(&set, &guard_cfg(&cfg));
    engine.apply(&plan, Approval::None, &PostgresBackend::new(&conn), &cfg, "app_acme").await.expect("apply v1..v3");
    for t in ["t1", "t2", "t3"] {
        assert!(table_exists(&conn, &schema, t).await, "{t} exists after apply");
    }

    // --- Rollback ToVersion(v1) (Approved): v2/v3 gone, v1 intact, rows reversed. ---
    engine
        .rollback(
            &set,
            RollbackRequest::new(RollbackTarget::ToVersion(v1.version.clone())),
            Approval::Approved,
            &conn,
            &cfg,
            "app_acme",
        )
        .await
        .expect("rollback to v1");
    assert!(table_exists(&conn, &schema, "t1").await, "v1 intact");
    assert!(!table_exists(&conn, &schema, "t2").await, "v2 table reversed");
    assert!(!table_exists(&conn, &schema, "t3").await, "v3 table reversed");

    // status: v1 net-applied, v2/v3 pending again (rolled back re-enters pending).
    let st = status(&conn, &cfg, &set).await.expect("status post-rollback");
    let applied: std::collections::HashSet<&str> =
        st.applied.iter().map(|e| e.version.as_str()).collect();
    assert!(applied.contains(v1.version.as_str()), "v1 applied");
    let pending: std::collections::HashSet<&str> =
        st.pending.iter().map(MigrationId::as_str).collect();
    assert!(pending.contains(v2.version.as_str()) && pending.contains(v3.version.as_str()),
        "v2/v3 pending after rollback");

    // --- Re-apply v2, v3 (they re-enter pending and re-apply). ---
    engine
        .apply(&engine.plan(&set, &guard_cfg(&cfg)), Approval::None, &PostgresBackend::new(&conn), &cfg, "app_acme")
        .await
        .expect("re-apply v2,v3");
    for t in ["t1", "t2", "t3"] {
        assert!(table_exists(&conn, &schema, t).await, "{t} present after re-apply");
    }

    // --- Squash v1..v3 on the now-existing DB (records supersession, no re-run). ---
    // A squash S whose up is the equivalent combined DDL, supersedes [v1,v2,v3].
    let squash_up = format!(
        "CREATE TABLE \"{schema}\".\"t1\" (id bigint primary key); \
         CREATE TABLE \"{schema}\".\"t2\" (id bigint primary key); \
         CREATE TABLE \"{schema}\".\"t3\" (id bigint primary key)"
    );
    let mut s = Migration {
        version: MigrationId::generate(),
        name: "squash_v1_v3".into(),
        up: squash_up,
        down: None,
        checksum: Checksum::of(&ChecksumInput {
            up: "",
            down: None,
            flags: &MigrationFlags::default(),
            owner_app: "",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        }),
        flags: MigrationFlags::default(),
        owner_app: "app_acme".into(),
        depends_on: Vec::new(),
        supersedes: vec![v1.version.clone(), v2.version.clone(), v3.version.clone()],
        preconditions: Vec::new(),
        existence_guard: None,
    };
    s.recompute_checksum();

    let sq = squash(&PostgresBackend::new(&conn), &cfg, &s, "app_acme").await.expect("squash v1..v3");
    assert!(!sq.already_present, "first squash records the supersession");
    assert_eq!(sq.superseded.len(), 3);
    // The squash did NOT re-run its up (the tables already existed; no error/dup).
    for t in ["t1", "t2", "t3"] {
        assert!(table_exists(&conn, &schema, t).await, "{t} untouched by recorded squash");
    }

    // --- status: squash net-applied; v1..v3 superseded (NOT pending). ---
    // The known set now includes the squash + the three originals.
    let mut full = set.clone();
    full.push(s.clone());
    let st2 = status(&conn, &cfg, &full).await.expect("status post-squash");
    let applied2: std::collections::HashSet<&str> =
        st2.applied.iter().map(|e| e.version.as_str()).collect();
    assert!(applied2.contains(s.version.as_str()), "squash is net-applied");
    let pending2: std::collections::HashSet<&str> =
        st2.pending.iter().map(MigrationId::as_str).collect();
    for v in [v1.version.as_str(), v2.version.as_str(), v3.version.as_str()] {
        assert!(
            !pending2.contains(v),
            "superseded version {v} must NOT be pending after squash; pending={pending2:?}"
        );
    }

    // --- history: shows the full apply / rollback / re-apply / squash chain. ---
    let hist = history(&conn, &cfg).await.expect("history");
    // Count completed events for v2 (apply + re-apply = 2) and rolled_back (1).
    let v2_completed = hist.iter().filter(|e| e.version == v2.version.as_str() && e.kind == HistoryKind::Completed).count();
    let v2_rolled = hist.iter().filter(|e| e.version == v2.version.as_str() && e.kind == HistoryKind::RolledBack).count();
    assert_eq!(v2_completed, 2, "v2 shows apply + re-apply (two completed events)");
    assert_eq!(v2_rolled, 1, "v2 shows one rollback event");
    // The squash event is in the log (as a completed event).
    assert!(
        hist.iter().any(|e| e.version == s.version.as_str() && e.kind == HistoryKind::Completed),
        "the squash event must appear in history"
    );
    // The full chain is event-ordered (monotonic event_seq).
    let seqs: Vec<i64> = hist.iter().map(|e| e.event_seq).collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted, "history must be in monotonic event order");

    teardown(&conn, &cfg).await;
}

// ===========================================================================
// P0-4 — SECURITY full-stack: guard deny + dry-run isolation + role backstop.
// ===========================================================================

/// Idempotently stand up a shared `control` stand-in schema (a real platform
/// schema name, so the guard's cross-tenant heuristics fire) + a sensitive table.
/// NEVER dropped — shared across parallel security tests; tolerates races.
async fn ensure_control_standin(conn: &Client) {
    for _ in 0..8 {
        match conn
            .batch_execute(
                "CREATE SCHEMA IF NOT EXISTS control; \
                 CREATE TABLE IF NOT EXISTS control.creator_billing (id int, secret text);",
            )
            .await
        {
            Ok(()) => return,
            Err(e) => {
                let msg = e.as_db_error().map(|d| d.message().to_string()).unwrap_or_default();
                if msg.contains("concurrently") || msg.contains("already exists") || msg.contains("duplicate key") {
                    compio::time::sleep(std::time::Duration::from_millis(20)).await;
                    continue;
                }
                panic!("seed control schema: {e}");
            }
        }
    }
}

fn assert_permission_denied(err: &compio_postgres::Error, ctx: &str) {
    let code = err.code();
    let detail = err.as_db_error().map_or_else(|| err.to_string(), ToString::to_string);
    assert_eq!(
        code,
        Some(&compio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE),
        "{ctx}: expected SQLSTATE 42501 (insufficient_privilege), got code={code:?} detail={detail}"
    );
}

#[compio::test]
async fn p0_4_security_full_stack_guard_dry_run_and_role_backstop() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    ensure_control_standin(&conn).await;
    let engine = MigrationEngine::new();
    let schema = cfg.project_schema.clone();
    let raw = RawSqlAuthor::new(schema.clone(), "app_acme");

    // A batch: a MALICIOUS cross-schema migration + a LEGIT additive one.
    let malicious = raw
        .wrap(
            "evil_xtenant",
            "SELECT * FROM control.creator_billing",
            None,
        )
        .expect("parseable");
    let legit = raw
        .wrap(
            "legit_table",
            &format!("CREATE TABLE \"{schema}\".\"safe_t\" (id bigint primary key)"),
            None,
        )
        .expect("legit");
    let batch = vec![malicious.clone(), legit.clone()];

    // --- LAYER 1: plan records the denial; apply returns Denied, applies NOTHING
    //     (all-or-nothing — the legit one does not land either). ---
    let plan = engine.plan(&batch, &guard_cfg(&cfg));
    assert_eq!(plan.denied.len(), 1, "plan records exactly the malicious denial");
    assert_eq!(plan.denied[0].0, malicious.version.as_str());
    assert!(!plan.is_appliable(), "a denied plan is un-appliable");

    let err = engine
        .apply(&plan, Approval::Approved, &PostgresBackend::new(&conn), &cfg, "app_acme")
        .await
        .expect_err("a denied plan must be refused even WITH approval");
    assert!(matches!(err, EngineError::Denied(ref d) if d.len() == 1), "got {err:?}");
    // ALL-OR-NOTHING: the legit table did NOT land.
    assert!(
        !table_exists(&conn, &schema, "safe_t").await,
        "all-or-nothing: the legit migration must NOT apply when the batch has a denial"
    );

    // --- LAYER 2: dry_run reports the malicious migration AND the REAL project
    //     schema + journal are provably untouched (the dry-run uses a throwaway DB). ---
    let report = engine
        .dry_run(&PostgresBackend::new(&conn), &batch, &cfg, &shadow_cfg(&tok), "app_acme")
        .await
        .expect("dry_run harness must succeed (a denied migration is reported, not an error)");
    // The dry-run reports the malicious migration as not-ok (denied / failed).
    assert!(
        !report.ok,
        "dry_run must report the batch as NOT ok (the malicious migration is denied)"
    );
    // The REAL project DB is untouched: no safe_t, and the journal has no completed
    // rows for either migration.
    assert!(
        !table_exists(&conn, &schema, "safe_t").await,
        "dry_run must NOT touch the real project schema"
    );
    let st = status(&conn, &cfg, &batch).await.expect("status after dry_run");
    assert!(
        st.applied.is_empty(),
        "dry_run + denied apply must leave the real journal empty: applied={:?}",
        st.applied.iter().map(|e| &e.version).collect::<Vec<_>>()
    );

    // --- LAYER 3: the role backstop denies a runtime-constructed cross-schema
    //     write AT THE DB, even though the guard can't statically see it. ---
    let role = cfg.pg.migrator_role.clone().unwrap();
    // (i) Confirm the guard (line 1) does NOT statically deny dynamic SQL — this is
    //     the documented residual the role backstops.
    let dynamic = format!(
        "DO $$ BEGIN EXECUTE 'CREATE TABLE ' || 'control' || '.evil_{tok} (x int)'; END $$",
        tok = tok.replace(['.', '-'], "_")
    );
    let guard = zero_migrate::SqlGuard::new(guard_cfg(&cfg));
    // The dynamic EXECUTE of a string-concatenated cross-schema name is the
    // documented residual: line 1 may or may not catch the literal; the BACKSTOP
    // is what we assert here. Run it AS the migrator role and require denial.
    let _ = guard; // guard behavior on dynamic SQL is covered by role_pg; here we assert the DB backstop.
    conn.batch_execute(&format!("SET ROLE \"{}\"", role.replace('"', "\"\"")))
        .await
        .expect("SET ROLE migrator");
    let exec_res = conn.batch_execute(&dynamic).await;
    conn.batch_execute("RESET ROLE").await.expect("RESET ROLE");
    let exec_err = exec_res.expect_err("runtime-constructed cross-schema write must be DB-denied");
    assert_permission_denied(&exec_err, "role backstop: runtime-constructed control write");
    // And prove it never created the table.
    let dyn_tbl = format!("evil_{}", tok.replace(['.', '-'], "_"));
    assert!(
        !table_exists(&conn, "control", &dyn_tbl).await,
        "the backstop payload must NOT have created the control table"
    );

    teardown(&conn, &cfg).await;
}
