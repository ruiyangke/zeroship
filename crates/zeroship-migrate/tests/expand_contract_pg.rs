//! Faithful expand-contract (Plan 8) tests against a REAL Postgres (no shims).
//!
//! Covers the online column-RENAME dual-write sequence end to end:
//!   - v1.2: the expand/contract GATE (contract refused until expand net-applied
//!     in the journal; cross-deploy partition via the journal);
//!   - v1.3: dual-write CORRECTNESS during the transition (under the APP role,
//!     not the migrator), old+new shape coexistence, NO trigger recursion /
//!     write-amplification, the guard/role security denials on the trigger body,
//!     and structural ROLLBACK of a half-done expand (before the backfill).
//!
//! Requires `zeroship_migrate_test` on :5440 (recreated by the runbook). Each
//! test runs in its OWN meta + project schema (unique token) for isolation.

use compio_postgres::Client;
use zeroship_migrate::executor::ApplyError;
use zeroship_migrate::{
    apply, run_backfill, Approval, ExecutorConfig, ExpandContractAuthor, ExpandContractPlan,
    OnlineIntent,
};

const DEFAULT_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test";

fn dsn() -> String {
    std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| DEFAULT_DSN.to_string())
}

async fn pg() -> Client {
    let (client, conn) = compio_postgres::connect(&dsn(), compio_postgres::NoTls)
        .await
        .expect("connect to zeroship_migrate_test on :5440");
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

fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(format!("prj_{tok}"), format!("proj_{tok}"));
    c.meta_schema = format!("meta_{tok}");
    c
}

async fn ensure_project_schema(conn: &Client, cfg: &ExecutorConfig) {
    conn.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS \"{}\"",
        cfg.project_schema
    ))
    .await
    .expect("create project schema");
}

async fn drop_schemas(conn: &Client, cfg: &ExecutorConfig) {
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.meta_schema
        ))
        .await;
}

/// Seed a `users` table with `id` PK + an `email` column and a few rows.
async fn seed_users(conn: &Client, schema: &str) {
    conn.batch_execute(&format!(
        "CREATE TABLE \"{schema}\".\"users\" (id bigint PRIMARY KEY, email text)"
    ))
    .await
    .expect("create users");
    conn.batch_execute(&format!(
        "INSERT INTO \"{schema}\".\"users\" (id, email) VALUES \
         (1,'a@x.test'),(2,'b@x.test'),(3,'c@x.test')"
    ))
    .await
    .expect("seed rows");
}

/// The canonical online rename of `users.email` → `users.email_address`.
fn rename_plan(cfg: &ExecutorConfig) -> ExpandContractPlan {
    ExpandContractAuthor::new(&cfg.project_schema, "app_acme")
        .author(&OnlineIntent::RenameColumn {
            table: "users".into(),
            from: "email".into(),
            to: "email_address".into(),
            ty: "text".into(),
        })
        .expect("author rename")
}

/// Apply the EXPAND phase (E1, E2, E3) and drive the backfill (E3's real work),
/// recording the backfill completion the gate reads — the v1.3 orchestration.
/// Returns the applied plan for later contract use.
async fn apply_expand(conn: &Client, cfg: &ExecutorConfig, plan: &ExpandContractPlan) {
    // E1, E2, E3 are minted with online + Expand; E3's `up` is the no-op backfill
    // marker, so a normal apply journals all three.
    apply(conn, cfg, &plan.expand, Approval::Approved, "tester")
        .await
        .expect("apply expand");
    // Drive the real backfill (E3's work). The marker migration is already
    // journaled completed; the backfill mutates the rows (cursor on the PK).
    run_backfill(conn, cfg, &plan.backfill, Approval::Approved, "tester")
        .await
        .expect("run backfill");
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

async fn trigger_exists(conn: &Client, schema: &str, table: &str) -> bool {
    let rows = conn
        .query(
            "SELECT 1 FROM pg_trigger t \
             JOIN pg_class c ON c.oid=t.tgrelid \
             JOIN pg_namespace n ON n.oid=c.relnamespace \
             WHERE n.nspname=$1 AND c.relname=$2 AND NOT t.tgisinternal",
            &[&schema, &table],
        )
        .await
        .expect("trigger_exists");
    !rows.is_empty()
}

// ===========================================================================
// v1.2 — the expand/contract gate
// ===========================================================================

/// A bundle with ONLY contract migrations, expand NOT journaled → clean refusal,
/// nothing applied.
#[compio::test]
async fn contract_before_expand_is_refused_and_applies_nothing() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    seed_users(&conn, &cfg.project_schema).await;

    let plan = rename_plan(&cfg);
    // Apply ONLY the contract (C1, C2); the expand was never applied/journaled.
    let err = apply(&conn, &cfg, &plan.contract, Approval::Approved, "tester")
        .await
        .expect_err("contract-before-expand must be refused");
    assert!(
        matches!(err, ApplyError::ExpandNotApplied { .. }),
        "expected ExpandNotApplied, got {err:?}"
    );
    // Nothing applied: the old column is intact, no trigger, no new column.
    assert!(column_exists(&conn, &cfg.project_schema, "users", "email").await);
    assert!(!column_exists(&conn, &cfg.project_schema, "users", "email_address").await);
    assert!(!trigger_exists(&conn, &cfg.project_schema, "users").await);

    drop_schemas(&conn, &cfg).await;
}

/// Apply expand (separate deploy), THEN a SEPARATE apply of contract → succeeds.
/// Proves cross-deploy partition: the gate reads the expand's completion off the
/// JOURNAL, not the in-batch set.
#[compio::test]
async fn expand_then_separate_contract_apply_succeeds_cross_deploy() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    seed_users(&conn, &cfg.project_schema).await;

    let plan = rename_plan(&cfg);
    // Deploy N: expand only.
    apply_expand(&conn, &cfg, &plan).await;
    assert!(trigger_exists(&conn, &cfg.project_schema, "users").await);
    assert!(column_exists(&conn, &cfg.project_schema, "users", "email_address").await);

    // Deploy N+1: contract ONLY (a fresh bundle that does not include the
    // expand). The gate sees E1/E2 net-applied in the journal and allows it.
    apply(&conn, &cfg, &plan.contract, Approval::Approved, "tester")
        .await
        .expect("contract apply after expand journaled");

    // The old column + trigger are gone; the new column remains.
    assert!(!column_exists(&conn, &cfg.project_schema, "users", "email").await);
    assert!(!trigger_exists(&conn, &cfg.project_schema, "users").await);
    assert!(column_exists(&conn, &cfg.project_schema, "users", "email_address").await);

    drop_schemas(&conn, &cfg).await;
}

/// A single bundle carrying BOTH phases is REFUSED: the gate's single source of
/// truth is the JOURNAL, and within one locked apply the expand is not yet
/// net-applied when the contract is examined. The contract MUST be a separate
/// deploy (bundling into deploys is the control plane's job, out of scope here).
/// Nothing is applied.
#[compio::test]
async fn full_bundle_both_phases_in_one_apply_is_refused() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    seed_users(&conn, &cfg.project_schema).await;

    let plan = rename_plan(&cfg);
    // One apply of the whole set (E1..C2). The contract's expand deps are
    // pending (not journaled), so the gate refuses before ANY execution.
    let err = apply(&conn, &cfg, &plan.all(), Approval::Approved, "tester")
        .await
        .expect_err("both phases in one apply must be refused");
    assert!(
        matches!(err, ApplyError::ExpandNotApplied { .. }),
        "expected ExpandNotApplied, got {err:?}"
    );
    // Nothing applied (the gate runs before any execution): no new column.
    assert!(!column_exists(&conn, &cfg.project_schema, "users", "email_address").await);
    assert!(!trigger_exists(&conn, &cfg.project_schema, "users").await);
    assert!(column_exists(&conn, &cfg.project_schema, "users", "email").await);

    drop_schemas(&conn, &cfg).await;
}
