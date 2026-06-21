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
use zeroship_migrate::executor::{ApplyError, LockMode, RollbackRequest, RollbackTarget};
use zeroship_migrate::{
    apply, migrator_role_name, provision_migrator, rollback, role::deprovision_migrator,
    run_backfill, Approval, BackfillSpec, Checksum, ExecutorConfig, ExpandContractAuthor,
    ExpandContractPlan, GuardConfig, Migration, MigrationBackend, MigrationFlags, MigrationId,
    MigrationEngine, OnlineIntent, OnlinePhase, PostgresBackend, RawSqlAuthor, SqlGuard,
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

/// Drive the EXPAND phase through the v1.3 orchestrator: apply E1+E2, run the
/// backfill, then journal E3 (the backfill marker) — in that order, so the gate
/// only sees the expand complete after the data is actually mirrored.
async fn apply_expand(conn: &Client, cfg: &ExecutorConfig, plan: &ExpandContractPlan) {
    MigrationEngine::new()
        .run_expand(plan, Approval::Approved, conn, cfg, "tester")
        .await
        .expect("run_expand orchestration");
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

    // Structural: C2 (DROP COLUMN) must declare C1 (DROP TRIGGER) as a dependency
    // so the trigger reading <from> is always torn down before the column it
    // reads — otherwise both are indeg-0 in a contract-only deploy and order only
    // by incidental UUIDv7 version.
    let (c1, c2) = (&plan.contract[0], &plan.contract[1]);
    assert!(
        c2.depends_on.contains(&c1.version),
        "C2 DROP COLUMN must depend on C1 DROP TRIGGER"
    );

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

/// A `phase: Contract` migration with EMPTY `depends_on` is MALFORMED and must
/// be refused fail-closed: the gate's expand/contract guarantee keys entirely on
/// the contract's declared expand dependency, so a contract that declares none
/// would vacuously pass (nothing to check) and could drop a column/trigger with
/// no journaled expand. Nothing is applied.
#[compio::test]
async fn contract_with_empty_depends_on_is_refused_fail_closed() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    seed_users(&conn, &cfg.project_schema).await;

    let schema = &cfg.project_schema;
    let up = format!("ALTER TABLE \"{schema}\".\"users\" DROP COLUMN email");
    let malformed = { let mut __mig = Migration {
        version: MigrationId::generate(),
        name: "contract_no_deps".into(),
        checksum: Checksum::of(&zeroship_migrate::ChecksumInput { up: "", down: None, flags: &MigrationFlags::default(), owner_app: "", depends_on: &[], supersedes: &[], preconditions: &[] }),
        up,
        down: None,
        flags: MigrationFlags {
            online: true,
            phase: Some(OnlinePhase::Contract),
            destructive: true,
            requires_approval: true,
            ..MigrationFlags::default()
        },
        owner_app: "app_acme".into(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
    }; __mig.recompute_checksum(); __mig };

    let err = apply(
        &conn,
        &cfg,
        std::slice::from_ref(&malformed),
        Approval::Approved,
        "tester",
    )
    .await
    .expect_err("a contract with no expand dependency must be refused");
    assert!(
        matches!(err, ApplyError::ExpandNotApplied { .. }),
        "expected ExpandNotApplied (malformed contract), got {err:?}"
    );
    // Nothing applied: the old column is intact.
    assert!(column_exists(&conn, &cfg.project_schema, "users", "email").await);

    drop_schemas(&conn, &cfg).await;
}

// ===========================================================================
// v1.3 — dual-write correctness, recursion, security, rollback (the MARQUEE)
// ===========================================================================

/// A cfg whose migrator role is the provisioned least-privilege role — so the
/// dual-write function is OWNED by the migrator and the trigger fires under the
/// (different) APP role at write time (the INVOKER-under-app-role path).
fn cfg_with_role(tok: &str) -> ExecutorConfig {
    let c = cfg_for(tok);
    let role = migrator_role_name(&c.project_id).unwrap();
    c.with_migrator_role(role)
}

/// Hand the seeded `users` table to the migrator role, mirroring the platform
/// where project tables are owned by the migrator (so its DDL — CREATE TRIGGER,
/// DROP COLUMN — works under SET ROLE). The test admin seeds the table, then
/// transfers ownership.
async fn give_table_to_migrator(conn: &Client, cfg: &ExecutorConfig) {
    let role = cfg.migrator_role.as_ref().unwrap();
    conn.batch_execute(&format!(
        "ALTER TABLE \"{}\".\"users\" OWNER TO \"{role}\"",
        cfg.project_schema
    ))
    .await
    .expect("transfer users ownership to migrator");
}

async fn full_teardown(conn: &Client, cfg: &ExecutorConfig, app_role: Option<&str>) {
    if let Some(r) = app_role {
        let _ = conn
            .batch_execute(&format!(
                "RESET ROLE; \
                 REASSIGN OWNED BY \"{r}\" TO CURRENT_USER; \
                 DROP OWNED BY \"{r}\"; DROP ROLE IF EXISTS \"{r}\""
            ))
            .await;
    }
    let _ = deprovision_migrator(conn, cfg).await;
    drop_schemas(conn, cfg).await;
}

/// Create a LOGIN app role and grant it exactly what an app needs to write the
/// table and fire the INVOKER dual-write trigger: USAGE on the schema, DML on
/// the table, EXECUTE on the dual-write function. Returns the role name.
async fn make_app_role(conn: &Client, cfg: &ExecutorConfig, tok: &str) -> String {
    let role = format!("app_{}", tok.replace(['.', '-'], "_"));
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

/// Run `sql` AS the app role (SET ROLE … then RESET ROLE), so the dual-write
/// trigger fires under the APP role's privileges (INVOKER), not the migrator's.
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

async fn row_pair(conn: &Client, schema: &str, id: i64) -> (Option<String>, Option<String>) {
    let row = conn
        .query_one(
            &format!("SELECT email, email_address FROM \"{schema}\".\"users\" WHERE id=$1"),
            &[&id],
        )
        .await
        .expect("row_pair");
    (row.get("email"), row.get("email_address"))
}

/// MARQUEE 1 — dual-write correctness during the transition, fired UNDER THE
/// APP ROLE (not the migrator): the INVOKER trigger fires for the writing role.
/// (a) insert via OLD only → NEW populated; (b) insert via NEW only → OLD
/// populated; (c) UPDATE old → new mirrors; (d) UPDATE new → old mirrors.
#[compio::test]
async fn dual_write_correctness_under_app_role() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_with_role(&tok);
    full_teardown(&conn, &cfg, None).await;
    ensure_project_schema(&conn, &cfg).await;
    provision_migrator(&conn, &cfg).await.expect("provision migrator");
    seed_users(&conn, &cfg.project_schema).await;
    give_table_to_migrator(&conn, &cfg).await;

    let plan = rename_plan(&cfg);
    apply_expand(&conn, &cfg, &plan).await;
    assert!(trigger_exists(&conn, &cfg.project_schema, "users").await);
    let app_role = make_app_role(&conn, &cfg, &tok).await;
    let schema = &cfg.project_schema;

    // (a) INSERT via the OLD column only → trigger fills the NEW column.
    as_app(
        &conn,
        &app_role,
        &format!("INSERT INTO \"{schema}\".\"users\" (id, email) VALUES (10, 'old-only@x.test')"),
    )
    .await;
    assert_eq!(
        row_pair(&conn, schema, 10).await,
        (Some("old-only@x.test".into()), Some("old-only@x.test".into())),
        "insert via OLD must populate NEW"
    );

    // (b) INSERT via the NEW column only → trigger fills the OLD column.
    as_app(
        &conn,
        &app_role,
        &format!(
            "INSERT INTO \"{schema}\".\"users\" (id, email_address) VALUES (11, 'new-only@x.test')"
        ),
    )
    .await;
    assert_eq!(
        row_pair(&conn, schema, 11).await,
        (Some("new-only@x.test".into()), Some("new-only@x.test".into())),
        "insert via NEW must populate OLD"
    );

    // (c) UPDATE the OLD column → NEW mirrors.
    as_app(
        &conn,
        &app_role,
        &format!("UPDATE \"{schema}\".\"users\" SET email='upd-old@x.test' WHERE id=10"),
    )
    .await;
    assert_eq!(
        row_pair(&conn, schema, 10).await,
        (Some("upd-old@x.test".into()), Some("upd-old@x.test".into())),
        "UPDATE old must mirror to new"
    );

    // (d) UPDATE the NEW column → OLD mirrors.
    as_app(
        &conn,
        &app_role,
        &format!("UPDATE \"{schema}\".\"users\" SET email_address='upd-new@x.test' WHERE id=11"),
    )
    .await;
    assert_eq!(
        row_pair(&conn, schema, 11).await,
        (Some("upd-new@x.test".into()), Some("upd-new@x.test".into())),
        "UPDATE new must mirror to old"
    );

    full_teardown(&conn, &cfg, Some(&app_role)).await;
}

/// L1/L2/H1 — the EXPAND drives byte-identically through the
/// `MigrationBackend::online()` capability seam (the `expand_conn() -> &Client`
/// escape hatch is GONE). `PostgresBackend::online()` is `Some`; we hand the seam
/// the NEUTRAL `OnlineIntent` + the pre-authored expand steps + backfill (never a
/// `Client`, never the PG `ExpandContractPlan` type), and it applies E1+E2, runs
/// the real backfill, and journals E3 LAST — exactly as `run_expand` does. The
/// pre-existing rows are mirrored into `<to>`, and the dual-write trigger is live.
#[compio::test]
async fn expand_drives_through_online_capability_seam_byte_identical() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_with_role(&tok);
    full_teardown(&conn, &cfg, None).await;
    ensure_project_schema(&conn, &cfg).await;
    provision_migrator(&conn, &cfg).await.expect("provision migrator");
    seed_users(&conn, &cfg.project_schema).await;
    give_table_to_migrator(&conn, &cfg).await;

    let plan = rename_plan(&cfg);
    let backend = PostgresBackend::new(&conn);

    // The PG backend exposes the online capability; SQLite returns None (proven in
    // engine_sqlite). The connection is owned INSIDE the capability, never returned.
    let online = backend
        .online()
        .expect("PostgresBackend exposes the OnlineSchemaChange capability (online() is Some)");

    // Drive the EXPAND through the NEUTRAL seam: intent + pre-authored expand +
    // backfill. LockMode::Acquire mirrors the standalone `run_expand` entry.
    let outcome = online
        .run_online(
            &plan.intent,
            &plan.expand,
            &plan.backfill,
            Approval::Approved,
            &cfg,
            "tester",
            LockMode::Acquire,
        )
        .await
        .expect("online().run_online drives the expand byte-identically");

    // E1, E2, E3 all journaled (the E3 marker LAST, after the real backfill).
    assert_eq!(
        outcome.applied.len(),
        3,
        "E1+E2 applied then E3 journaled last → 3 applied steps (byte-identical to run_expand)"
    );

    let schema = &cfg.project_schema;
    // The new column exists and the dual-write trigger is live.
    assert!(column_exists(&conn, schema, "users", "email_address").await, "<to> added");
    assert!(trigger_exists(&conn, schema, "users").await, "dual-write trigger live");

    // The real backfill mirrored EVERY pre-existing `<from>` into `<to>` — the
    // whole point of run_backfill running before E3 is journaled.
    let rows = conn
        .query(
            &format!("SELECT id, email, email_address FROM \"{schema}\".\"users\" ORDER BY id"),
            &[],
        )
        .await
        .expect("read rows after online expand");
    assert_eq!(rows.len(), 3, "all three pre-existing rows present");
    for row in &rows {
        let from: Option<String> = row.get("email");
        let to: Option<String> = row.get("email_address");
        assert_eq!(to, from, "backfill mirrored pre-existing <from> into <to> through the seam");
    }

    full_teardown(&conn, &cfg, None).await;
}

/// MARQUEE 1b — the dual-write trigger is TOTAL: a single statement that
/// changes BOTH columns to DIFFERENT values must NOT leave them divergent. The
/// "to wins" precedence (the new column wins, consistent with the contract end
/// state which keeps `to`) means after any such write the two columns are EQUAL
/// to the `to` value. Covers the central data-integrity hole: a divergent pair
/// would be silently destroyed by the contract's `DROP COLUMN <from>`.
#[compio::test]
async fn dual_write_is_total_to_wins_when_both_columns_change() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_with_role(&tok);
    full_teardown(&conn, &cfg, None).await;
    ensure_project_schema(&conn, &cfg).await;
    provision_migrator(&conn, &cfg).await.expect("provision");
    seed_users(&conn, &cfg.project_schema).await;
    give_table_to_migrator(&conn, &cfg).await;

    let plan = rename_plan(&cfg);
    apply_expand(&conn, &cfg, &plan).await;
    let app_role = make_app_role(&conn, &cfg, &tok).await;
    let schema = &cfg.project_schema;

    // (1) A single UPDATE setting BOTH columns to DIFFERENT values → after the
    //     trigger they are EQUAL, to the `to` (email_address) value.
    as_app(
        &conn,
        &app_role,
        &format!(
            "UPDATE \"{schema}\".\"users\" \
             SET email='from-side@x.test', email_address='to-side@x.test' WHERE id=1"
        ),
    )
    .await;
    assert_eq!(
        row_pair(&conn, schema, 1).await,
        (Some("to-side@x.test".into()), Some("to-side@x.test".into())),
        "UPDATE changing BOTH columns must converge to the `to` value (to wins)"
    );

    // (2) An INSERT setting BOTH columns to DIFFERENT values → EQUAL, to-wins.
    as_app(
        &conn,
        &app_role,
        &format!(
            "INSERT INTO \"{schema}\".\"users\" (id, email, email_address) \
             VALUES (20, 'ins-from@x.test', 'ins-to@x.test')"
        ),
    )
    .await;
    assert_eq!(
        row_pair(&conn, schema, 20).await,
        (Some("ins-to@x.test".into()), Some("ins-to@x.test".into())),
        "INSERT setting BOTH columns must converge to the `to` value (to wins)"
    );

    // (3) An INSERT setting BOTH to the SAME value stays that value (no-op arm).
    as_app(
        &conn,
        &app_role,
        &format!(
            "INSERT INTO \"{schema}\".\"users\" (id, email, email_address) \
             VALUES (21, 'same@x.test', 'same@x.test')"
        ),
    )
    .await;
    assert_eq!(
        row_pair(&conn, schema, 21).await,
        (Some("same@x.test".into()), Some("same@x.test".into())),
        "INSERT with both set to the same value is unchanged"
    );

    // (4) An INSERT with BOTH NULL stays both NULL (no-op arm, no error).
    as_app(
        &conn,
        &app_role,
        &format!("INSERT INTO \"{schema}\".\"users\" (id) VALUES (22)"),
    )
    .await;
    assert_eq!(
        row_pair(&conn, schema, 22).await,
        (None, None),
        "INSERT with both NULL leaves both NULL"
    );

    // (5) Single-column legacy cases STILL mirror correctly (regression guard):
    //     insert via OLD only, insert via NEW only, update OLD only, update NEW only.
    as_app(
        &conn,
        &app_role,
        &format!("INSERT INTO \"{schema}\".\"users\" (id, email) VALUES (23, 'a@x.test')"),
    )
    .await;
    assert_eq!(
        row_pair(&conn, schema, 23).await,
        (Some("a@x.test".into()), Some("a@x.test".into())),
        "(a) insert via OLD only still mirrors to NEW"
    );
    as_app(
        &conn,
        &app_role,
        &format!(
            "INSERT INTO \"{schema}\".\"users\" (id, email_address) VALUES (24, 'b@x.test')"
        ),
    )
    .await;
    assert_eq!(
        row_pair(&conn, schema, 24).await,
        (Some("b@x.test".into()), Some("b@x.test".into())),
        "(b) insert via NEW only still mirrors to OLD"
    );
    as_app(
        &conn,
        &app_role,
        &format!("UPDATE \"{schema}\".\"users\" SET email='c@x.test' WHERE id=23"),
    )
    .await;
    assert_eq!(
        row_pair(&conn, schema, 23).await,
        (Some("c@x.test".into()), Some("c@x.test".into())),
        "(c) update OLD only still mirrors to NEW"
    );
    as_app(
        &conn,
        &app_role,
        &format!("UPDATE \"{schema}\".\"users\" SET email_address='d@x.test' WHERE id=24"),
    )
    .await;
    assert_eq!(
        row_pair(&conn, schema, 24).await,
        (Some("d@x.test".into()), Some("d@x.test".into())),
        "(d) update NEW only still mirrors to OLD"
    );

    full_teardown(&conn, &cfg, Some(&app_role)).await;
}

/// MARQUEE 2 — old + new shape coexist after expand, before contract: both
/// physical columns present and CONSISTENT after arbitrary writes through either.
#[compio::test]
async fn old_and_new_shape_coexist_consistently() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_with_role(&tok);
    full_teardown(&conn, &cfg, None).await;
    ensure_project_schema(&conn, &cfg).await;
    provision_migrator(&conn, &cfg).await.expect("provision");
    seed_users(&conn, &cfg.project_schema).await;
    give_table_to_migrator(&conn, &cfg).await;

    let plan = rename_plan(&cfg);
    apply_expand(&conn, &cfg, &plan).await;
    let app_role = make_app_role(&conn, &cfg, &tok).await;
    let schema = &cfg.project_schema;

    // Both columns physically present.
    assert!(column_exists(&conn, schema, "users", "email").await);
    assert!(column_exists(&conn, schema, "users", "email_address").await);

    // Pre-existing rows were backfilled (email_address == email everywhere).
    let mismatched: i64 = conn
        .query_one(
            &format!(
                "SELECT count(*) AS c FROM \"{schema}\".\"users\" \
                 WHERE email IS DISTINCT FROM email_address"
            ),
            &[],
        )
        .await
        .expect("consistency count")
        .get("c");
    assert_eq!(mismatched, 0, "every row must be consistent after backfill");

    // Arbitrary writes through EITHER name keep them consistent.
    as_app(
        &conn,
        &app_role,
        &format!("UPDATE \"{schema}\".\"users\" SET email='z@x.test' WHERE id=1"),
    )
    .await;
    as_app(
        &conn,
        &app_role,
        &format!("UPDATE \"{schema}\".\"users\" SET email_address='y@x.test' WHERE id=2"),
    )
    .await;
    let still_mismatched: i64 = conn
        .query_one(
            &format!(
                "SELECT count(*) AS c FROM \"{schema}\".\"users\" \
                 WHERE email IS DISTINCT FROM email_address"
            ),
            &[],
        )
        .await
        .expect("consistency count 2")
        .get("c");
    assert_eq!(still_mismatched, 0, "writes through either name stay consistent");

    full_teardown(&conn, &cfg, Some(&app_role)).await;
}

/// MARQUEE 3 — no trigger recursion / write-amplification: an UPDATE does not
/// infinitely re-fire (a BEFORE trigger assigning NEW.* never re-issues a write),
/// and an UPDATE that does not change either column is a no-op (the IS DISTINCT
/// FROM guards). Asserted via the row's `xmin` (system version): a re-firing or
/// amplifying trigger would bump it more than once / on a no-op update.
#[compio::test]
async fn no_trigger_recursion_or_write_amplification() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_with_role(&tok);
    full_teardown(&conn, &cfg, None).await;
    ensure_project_schema(&conn, &cfg).await;
    provision_migrator(&conn, &cfg).await.expect("provision");
    seed_users(&conn, &cfg.project_schema).await;
    give_table_to_migrator(&conn, &cfg).await;

    let plan = rename_plan(&cfg);
    apply_expand(&conn, &cfg, &plan).await;
    let app_role = make_app_role(&conn, &cfg, &tok).await;
    let schema = &cfg.project_schema;

    // A single UPDATE to the old column completes (does not hang / recurse) and
    // produces exactly one new row version.
    let xmin_before: String = conn
        .query_one(
            &format!("SELECT xmin::text AS x FROM \"{schema}\".\"users\" WHERE id=1"),
            &[],
        )
        .await
        .expect("xmin before")
        .get("x");
    as_app(
        &conn,
        &app_role,
        &format!("UPDATE \"{schema}\".\"users\" SET email='r@x.test' WHERE id=1"),
    )
    .await;
    let (e, ea) = row_pair(&conn, schema, 1).await;
    assert_eq!(e.as_deref(), Some("r@x.test"));
    assert_eq!(ea.as_deref(), Some("r@x.test"), "mirrored once, correctly");

    // A no-op UPDATE (writing the SAME value already present) must NOT amplify:
    // the trigger's IS DISTINCT FROM guards leave NEW untouched. The row still
    // gets a new xmin from the UPDATE itself (Postgres writes the tuple), but the
    // mirror column is unchanged and the statement returns without error / loop.
    let xmin_mid: String = conn
        .query_one(
            &format!("SELECT xmin::text AS x FROM \"{schema}\".\"users\" WHERE id=1"),
            &[],
        )
        .await
        .expect("xmin mid")
        .get("x");
    assert_ne!(xmin_before, xmin_mid, "the real update bumped the version once");
    as_app(
        &conn,
        &app_role,
        &format!("UPDATE \"{schema}\".\"users\" SET email='r@x.test' WHERE id=1"),
    )
    .await;
    // Both columns still equal — no amplification corrupted them.
    assert_eq!(
        row_pair(&conn, schema, 1).await,
        (Some("r@x.test".into()), Some("r@x.test".into())),
        "no-op re-update leaves a consistent, single-mirrored row"
    );

    full_teardown(&conn, &cfg, Some(&app_role)).await;
}

/// MARQUEE 4 — security: the EXISTING guard denies a dual-write trigger fn body
/// that reaches `control.*`, an `EXECUTE FUNCTION control.x()` trigger, and a
/// SECURITY DEFINER fn. plan() records the denial AND apply() refuses.
#[compio::test]
async fn security_guard_denies_malicious_trigger_variants() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let gcfg = GuardConfig::confined(cfg.project_schema.clone());
    let guard = SqlGuard::new(gcfg.clone());
    let raw = RawSqlAuthor::new(&cfg.project_schema, "app_acme");

    // (i) trigger fn body referencing control.* → cross-schema denial.
    let body_xtenant = format!(
        "CREATE FUNCTION \"{s}\".\"evilfn\"() RETURNS trigger AS $$ \
         BEGIN PERFORM 1 FROM control.creator_billing; RETURN NEW; END; $$ LANGUAGE plpgsql",
        s = cfg.project_schema
    );
    assert!(guard.check(&body_xtenant).is_err(), "control.* in body must be denied");

    // (ii) EXECUTE FUNCTION control.x() in the trigger → CrossSchema.
    let trg_xtenant = format!(
        "CREATE TRIGGER t BEFORE INSERT ON \"{s}\".\"users\" \
         FOR EACH ROW EXECUTE FUNCTION control.x()",
        s = cfg.project_schema
    );
    assert!(
        guard.check(&trg_xtenant).is_err(),
        "EXECUTE FUNCTION control.x() must be denied"
    );

    // (iii) SECURITY DEFINER fn → denied (escalation).
    let secdef = format!(
        "CREATE FUNCTION \"{s}\".\"sd\"() RETURNS trigger AS $$ BEGIN RETURN NEW; END; $$ \
         LANGUAGE plpgsql SECURITY DEFINER",
        s = cfg.project_schema
    );
    assert!(guard.check(&secdef).is_err(), "SECURITY DEFINER must be denied");

    // And via the engine: plan() records the denial, apply() refuses (nothing runs).
    let m = raw.wrap("evil", &body_xtenant, None).expect("parseable");
    let plan = MigrationEngine::new().plan(std::slice::from_ref(&m), &gcfg);
    assert_eq!(plan.denied.len(), 1, "plan records the cross-schema denial");
    assert!(!plan.is_appliable());
    let err = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::Approved, "tester")
        .await
        .expect_err("apply must refuse the denied migration");
    assert!(matches!(err, ApplyError::Guard { .. }), "got {err:?}");

    drop_schemas(&conn, &cfg).await;
}

/// MARQUEE 5 — rollback of a half-done expand (E1, E2 applied, NO backfill yet):
/// order_rollback tears down the trigger (E2) BEFORE the column (E1); the table
/// returns to its pre-expand shape with no orphan trigger. Structural rollback
/// BEFORE the backfill is allowed (Plan 8); rollback ACROSS the backfill is NOT
/// (roll-forward-only) — asserted by E3 being irreversible (down: None).
#[compio::test]
async fn rollback_of_half_done_expand_tears_down_trigger_then_column() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_with_role(&tok);
    full_teardown(&conn, &cfg, None).await;
    ensure_project_schema(&conn, &cfg).await;
    provision_migrator(&conn, &cfg).await.expect("provision");
    seed_users(&conn, &cfg.project_schema).await;
    give_table_to_migrator(&conn, &cfg).await;

    let plan = rename_plan(&cfg);
    // Apply E1 + E2 ONLY (no backfill, no E3 marker) — a half-done expand.
    let head = &plan.expand[..2];
    apply(&conn, &cfg, head, Approval::Approved, "tester")
        .await
        .expect("apply E1+E2");
    assert!(column_exists(&conn, &cfg.project_schema, "users", "email_address").await);
    assert!(trigger_exists(&conn, &cfg.project_schema, "users").await);

    // E3 is roll-FORWARD-only: it carries no down (irreversible). Document/assert.
    assert!(
        plan.expand[2].down.is_none(),
        "E3 (backfill) must be irreversible — rollback across the backfill is not allowed"
    );

    // Roll back the half-done expand to before E1 (RollbackTarget::All). The
    // executor's order_rollback runs E2's down (drop trigger+fn) BEFORE E1's down
    // (drop column), so there is never an orphan trigger on a missing column.
    let req = RollbackRequest::new(RollbackTarget::All);
    rollback(&conn, &cfg, head, req, Approval::Approved, "tester")
        .await
        .expect("rollback half-done expand");

    // Back to pre-expand shape: old column present, new column gone, no trigger.
    assert!(column_exists(&conn, &cfg.project_schema, "users", "email").await);
    assert!(!column_exists(&conn, &cfg.project_schema, "users", "email_address").await);
    assert!(
        !trigger_exists(&conn, &cfg.project_schema, "users").await,
        "no orphan trigger after rollback"
    );

    full_teardown(&conn, &cfg, None).await;
}

/// MARQUEE 5b — the orchestrator's approval gate: `run_expand` with
/// `Approval::None` refuses (the backfill mutates data) and applies nothing.
#[compio::test]
async fn run_expand_without_approval_refuses() {
    use zeroship_migrate::OnlineError;
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_with_role(&tok);
    full_teardown(&conn, &cfg, None).await;
    ensure_project_schema(&conn, &cfg).await;
    provision_migrator(&conn, &cfg).await.expect("provision");
    seed_users(&conn, &cfg.project_schema).await;
    give_table_to_migrator(&conn, &cfg).await;

    let plan = rename_plan(&cfg);
    let err = MigrationEngine::new()
        .run_expand(&plan, Approval::None, &conn, &cfg, "tester")
        .await
        .expect_err("run_expand must refuse without approval");
    assert!(matches!(err, OnlineError::Approval), "got {err:?}");
    // Nothing applied: no new column, no trigger.
    assert!(!column_exists(&conn, &cfg.project_schema, "users", "email_address").await);
    assert!(!trigger_exists(&conn, &cfg.project_schema, "users").await);

    full_teardown(&conn, &cfg, None).await;
}

// ===========================================================================
// Matrix §6 #14 (split column) and #15 (merge columns) — AI-authored (RawSqlAuthor
// AI-seam) expand-contract sequences whose dual-write trigger fans out to / in
// from MORE THAN ONE column. The canonical author (`ExpandContractAuthor`) only
// covers single-column rename; these multi-column shapes are authored as raw SQL
// (the `RawSqlAuthor` seam an AI author would target), wired with explicit phase
// flags + `depends_on` so the engine gate, executor, and backfill drive them
// exactly like the rename. Dual-write correctness is asserted UNDER THE APP ROLE
// (the INVOKER path), identical to the rename marquees above.
// ===========================================================================

/// Author a raw `up`/`down` migration via the [`RawSqlAuthor`] AI-seam, then
/// stamp the online phase + `depends_on` an AI author would emit for a phased
/// expand-contract step, recomputing the checksum so the stamped flags/deps are
/// part of the migration's identity (exactly what the deterministic author does
/// internally via `make`). This is the multi-column analogue of `make`, used for
/// the split/merge shapes the deterministic author does not (yet) emit.
fn ai_step(
    raw: &RawSqlAuthor,
    name: &str,
    up: &str,
    down: Option<&str>,
    phase: OnlinePhase,
    destructive: bool,
    depends_on: Vec<MigrationId>,
) -> Migration {
    let mut m = raw.wrap(name, up, down).expect("author multi-column step");
    m.flags = MigrationFlags {
        online: true,
        phase: Some(phase),
        destructive,
        // Contract steps are gated + approval-required (they drop trigger/columns);
        // expand steps are additive. Mirror the deterministic author's flagging.
        requires_approval: matches!(phase, OnlinePhase::Contract),
        ..MigrationFlags::default()
    };
    m.depends_on = depends_on;
    m.recompute_checksum();
    m
}

/// Seed a `people` table with `id` PK + a single source column `full_name` and a
/// few rows — the pre-split shape for matrix #14.
async fn seed_people_split(conn: &Client, schema: &str) {
    conn.batch_execute(&format!(
        "CREATE TABLE \"{schema}\".\"people\" (id bigint PRIMARY KEY, full_name text)"
    ))
    .await
    .expect("create people");
    conn.batch_execute(&format!(
        "INSERT INTO \"{schema}\".\"people\" (id, full_name) VALUES \
         (1,'Ada Lovelace'),(2,'Alan Turing'),(3,'Grace Hopper')"
    ))
    .await
    .expect("seed people");
}

async fn person_split(
    conn: &Client,
    schema: &str,
    id: i64,
) -> (Option<String>, Option<String>, Option<String>) {
    let row = conn
        .query_one(
            &format!(
                "SELECT full_name, first_name, last_name FROM \"{schema}\".\"people\" WHERE id=$1"
            ),
            &[&id],
        )
        .await
        .expect("person_split");
    (
        row.get("full_name"),
        row.get("first_name"),
        row.get("last_name"),
    )
}

/// MATRIX #14 — SPLIT one column into two, online, via an AI-authored
/// expand-contract sequence. `people.full_name` → (`first_name`, `last_name`)
/// where the split rule is "first token / rest". Expand: E1 adds BOTH new
/// columns; E2 installs a dual-write trigger that derives both new columns from
/// the source AND re-derives the source from the two parts (so old + new shapes
/// coexist + stay consistent); E3 backfills the parts from the source. Contract:
/// C1 drops the trigger/fn, C2 drops the source column. Dual-write correctness is
/// asserted UNDER THE APP ROLE.
#[compio::test]
async fn split_column_expand_contract_dual_write_under_app_role() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_with_role(&tok);
    full_teardown(&conn, &cfg, None).await;
    ensure_project_schema(&conn, &cfg).await;
    provision_migrator(&conn, &cfg).await.expect("provision");
    seed_people_split(&conn, &cfg.project_schema).await;

    let schema = cfg.project_schema.clone();
    // The migrator must own the table it ALTERs / triggers under SET ROLE.
    conn.batch_execute(&format!(
        "ALTER TABLE \"{schema}\".\"people\" OWNER TO \"{}\"",
        cfg.migrator_role.as_ref().unwrap()
    ))
    .await
    .expect("chown people to migrator");

    let raw = RawSqlAuthor::new(schema.clone(), "app_acme");

    // ---- E1: ADD COLUMN first_name + last_name (both nullable, additive) ----
    let e1 = ai_step(
        &raw,
        "expand_split_add_name_parts",
        &format!(
            "ALTER TABLE \"{schema}\".\"people\" \
             ADD COLUMN first_name text, ADD COLUMN last_name text"
        ),
        Some(&format!(
            "ALTER TABLE \"{schema}\".\"people\" \
             DROP COLUMN first_name, DROP COLUMN last_name"
        )),
        OnlinePhase::Expand,
        false,
        Vec::new(),
    );

    // ---- E2: dual-write trigger (SECURITY INVOKER plpgsql) ----
    //
    // TOTAL for the SPLIT shape: after it runs the source and the two parts are
    // ALWAYS consistent, for every INSERT and UPDATE. Precedence by *which side
    // the app wrote*:
    //   - INSERT: if only the source is set (parts NULL), derive the parts from it;
    //     otherwise recompose the source from the parts (new shape wins).
    //   - UPDATE: if the source changed (and the parts did not), re-derive the
    //     parts; otherwise (parts changed, or both/neither changed) recompose the
    //     source from the parts (new shape wins, to-wins).
    // A BEFORE trigger assigning NEW.* never re-fires → no recursion.
    let e2_up = format!(
        "CREATE OR REPLACE FUNCTION \"{schema}\".\"zsdw_people_split_fn\"() \
         RETURNS trigger AS $zsdw$\n\
         BEGIN\n\
         \x20  IF (TG_OP = 'INSERT' \
                   AND NEW.first_name IS NULL AND NEW.last_name IS NULL \
                   AND NEW.full_name IS NOT NULL)\n\
         \x20     OR (TG_OP = 'UPDATE' \
                   AND NEW.full_name IS DISTINCT FROM OLD.full_name \
                   AND NEW.first_name IS NOT DISTINCT FROM OLD.first_name \
                   AND NEW.last_name IS NOT DISTINCT FROM OLD.last_name) THEN\n\
         \x20    NEW.first_name := split_part(NEW.full_name, ' ', 1);\n\
         \x20    NEW.last_name  := NULLIF(substring(NEW.full_name from position(' ' in NEW.full_name) + 1), '');\n\
         \x20  ELSE\n\
         \x20    NEW.full_name := NULLIF(trim(both ' ' from \
                 coalesce(NEW.first_name, '') || ' ' || coalesce(NEW.last_name, '')), '');\n\
         \x20  END IF;\n\
         \x20  RETURN NEW;\n\
         END;\n\
         $zsdw$ LANGUAGE plpgsql;\n\
         CREATE TRIGGER \"zsdw_people_split_trg\" BEFORE INSERT OR UPDATE \
         ON \"{schema}\".\"people\" FOR EACH ROW \
         EXECUTE FUNCTION \"{schema}\".\"zsdw_people_split_fn\"()"
    );
    let e2_down = format!(
        "DROP TRIGGER IF EXISTS \"zsdw_people_split_trg\" ON \"{schema}\".\"people\"; \
         DROP FUNCTION IF EXISTS \"{schema}\".\"zsdw_people_split_fn\"()"
    );
    let e2 = ai_step(
        &raw,
        "expand_split_dual_write",
        &e2_up,
        Some(&e2_down),
        OnlinePhase::Expand,
        false,
        vec![e1.version.clone()],
    );

    // Apply E1 + E2 (additive, no approval needed — they are Expand).
    apply(
        &conn,
        &cfg,
        &[e1.clone(), e2.clone()],
        Approval::Approved,
        "tester",
    )
    .await
    .expect("apply split expand E1+E2");
    assert!(column_exists(&conn, &schema, "people", "first_name").await);
    assert!(column_exists(&conn, &schema, "people", "last_name").await);
    assert!(trigger_exists(&conn, &schema, "people").await);

    // ---- E3: backfill the parts from the source for the pre-existing rows ----
    let backfill = BackfillSpec {
        table: "people".into(),
        cursor_column: "id".into(),
        batch_size: 1000,
        set_clause: "first_name = split_part(_bf.full_name, ' ', 1), \
                     last_name = NULLIF(substring(_bf.full_name from position(' ' in _bf.full_name) + 1), '')"
            .into(),
        filter: Some("first_name IS NULL AND last_name IS NULL".into()),
        name: "backfill_people_split".into(),
    };
    let out = run_backfill(&conn, &cfg, &backfill, Approval::Approved, "tester")
        .await
        .expect("split backfill");
    assert!(out.complete);
    assert_eq!(out.rows_updated, 3, "all 3 seed rows split");

    // Pre-existing rows: parts derived from the source.
    assert_eq!(
        person_split(&conn, &schema, 1).await,
        (
            Some("Ada Lovelace".into()),
            Some("Ada".into()),
            Some("Lovelace".into())
        ),
        "backfill split full_name into parts"
    );

    let app_role = make_app_role(&conn, &cfg, &tok).await;

    // (a) INSERT via the SOURCE only → trigger derives BOTH parts.
    as_app(
        &conn,
        &app_role,
        &format!(
            "INSERT INTO \"{schema}\".\"people\" (id, full_name) VALUES (10, 'Katherine Johnson')"
        ),
    )
    .await;
    assert_eq!(
        person_split(&conn, &schema, 10).await,
        (
            Some("Katherine Johnson".into()),
            Some("Katherine".into()),
            Some("Johnson".into())
        ),
        "insert via source must populate both parts"
    );

    // (b) INSERT via the NEW parts only → trigger recomposes the source.
    as_app(
        &conn,
        &app_role,
        &format!(
            "INSERT INTO \"{schema}\".\"people\" (id, first_name, last_name) \
             VALUES (11, 'Margaret', 'Hamilton')"
        ),
    )
    .await;
    assert_eq!(
        person_split(&conn, &schema, 11).await,
        (
            Some("Margaret Hamilton".into()),
            Some("Margaret".into()),
            Some("Hamilton".into())
        ),
        "insert via parts must recompose the source (parts win)"
    );

    // (c) UPDATE the SOURCE → both parts re-derive.
    as_app(
        &conn,
        &app_role,
        &format!("UPDATE \"{schema}\".\"people\" SET full_name='Dorothy Vaughan' WHERE id=10"),
    )
    .await;
    assert_eq!(
        person_split(&conn, &schema, 10).await,
        (
            Some("Dorothy Vaughan".into()),
            Some("Dorothy".into()),
            Some("Vaughan".into())
        ),
        "update source must re-derive both parts"
    );

    // (d) UPDATE a NEW part → the source recomposes (parts win).
    as_app(
        &conn,
        &app_role,
        &format!("UPDATE \"{schema}\".\"people\" SET last_name='Hamilton-Lickly' WHERE id=11"),
    )
    .await;
    assert_eq!(
        person_split(&conn, &schema, 11).await,
        (
            Some("Margaret Hamilton-Lickly".into()),
            Some("Margaret".into()),
            Some("Hamilton-Lickly".into())
        ),
        "update a part must recompose the source"
    );

    // ---- CONTRACT: drop the trigger/fn (C1), then the source column (C2) ----
    let c1 = ai_step(
        &raw,
        "contract_split_drop_dual_write",
        &e2_down,
        Some(&e2_up),
        OnlinePhase::Contract,
        false,
        vec![e2.version.clone()],
    );
    let c2 = ai_step(
        &raw,
        "contract_split_drop_source",
        &format!("ALTER TABLE \"{schema}\".\"people\" DROP COLUMN full_name"),
        None,
        OnlinePhase::Contract,
        true,
        // C2 depends on E2 (the journaled expand the gate keys on) AND C1 (drop the
        // trigger reading full_name before dropping the column it reads).
        vec![e2.version.clone(), c1.version.clone()],
    );
    apply(
        &conn,
        &cfg,
        &[c1, c2],
        Approval::Approved,
        "tester",
    )
    .await
    .expect("apply split contract");

    // End state: the source is gone, both parts remain + intact, no trigger.
    assert!(!column_exists(&conn, &schema, "people", "full_name").await);
    assert!(column_exists(&conn, &schema, "people", "first_name").await);
    assert!(column_exists(&conn, &schema, "people", "last_name").await);
    assert!(!trigger_exists(&conn, &schema, "people").await);
    let parts: (String, String) = {
        let r = conn
            .query_one(
                &format!("SELECT first_name, last_name FROM \"{schema}\".\"people\" WHERE id=1"),
                &[],
            )
            .await
            .expect("post-contract row");
        (r.get("first_name"), r.get("last_name"))
    };
    assert_eq!(parts, ("Ada".into(), "Lovelace".into()));

    full_teardown(&conn, &cfg, Some(&app_role)).await;
}

/// Seed a `contacts` table with `id` PK + TWO source columns and a few rows — the
/// pre-merge shape for matrix #15.
async fn seed_contacts_merge(conn: &Client, schema: &str) {
    conn.batch_execute(&format!(
        "CREATE TABLE \"{schema}\".\"contacts\" \
         (id bigint PRIMARY KEY, area_code text, local_number text)"
    ))
    .await
    .expect("create contacts");
    conn.batch_execute(&format!(
        "INSERT INTO \"{schema}\".\"contacts\" (id, area_code, local_number) VALUES \
         (1,'415','5551234'),(2,'212','5559876'),(3,NULL,'5550000')"
    ))
    .await
    .expect("seed contacts");
}

/// MATRIX #15 — MERGE two columns into one, online, via an AI-authored
/// expand-contract sequence. `contacts.(area_code, local_number)` → `phone` with
/// a PRECEDENCE / composition rule: `phone = area_code || '-' || local_number`
/// when an area code exists, else just `local_number`. Expand: E1 adds the single
/// new column; E2 a dual-write trigger that composes `phone` from the two sources
/// (and, when the app writes `phone` directly, leaves the sources as-is so the
/// new shape is authoritative); E3 backfills `phone`. Contract: C1 drops the
/// trigger/fn, C2 drops BOTH source columns. Asserted UNDER THE APP ROLE.
#[compio::test]
async fn merge_columns_expand_contract_dual_write_under_app_role() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_with_role(&tok);
    full_teardown(&conn, &cfg, None).await;
    ensure_project_schema(&conn, &cfg).await;
    provision_migrator(&conn, &cfg).await.expect("provision");
    seed_contacts_merge(&conn, &cfg.project_schema).await;

    let schema = cfg.project_schema.clone();
    conn.batch_execute(&format!(
        "ALTER TABLE \"{schema}\".\"contacts\" OWNER TO \"{}\"",
        cfg.migrator_role.as_ref().unwrap()
    ))
    .await
    .expect("chown contacts to migrator");

    let raw = RawSqlAuthor::new(schema.clone(), "app_acme");

    // ---- E1: ADD COLUMN phone (nullable, additive) ----
    let e1 = ai_step(
        &raw,
        "expand_merge_add_phone",
        &format!("ALTER TABLE \"{schema}\".\"contacts\" ADD COLUMN phone text"),
        Some(&format!(
            "ALTER TABLE \"{schema}\".\"contacts\" DROP COLUMN phone"
        )),
        OnlinePhase::Expand,
        false,
        Vec::new(),
    );

    // ---- E2: dual-write trigger composing phone from the two sources ----
    //
    // The precedence/composition rule: with an area code,
    // `phone = area_code-local_number`, else just `local_number`. TOTAL — after it
    // runs phone is ALWAYS consistent with the sources for the write the app made:
    //   - INSERT: if phone is NULL, compose it from the sources; else phone wins.
    //   - UPDATE: if a source changed (and phone did not), recompose phone from the
    //     sources; else (phone changed, or both/neither changed) phone wins (the
    //     app wrote the new shape directly, the sources are being retired).
    // A BEFORE trigger assigning NEW.* never re-fires → no recursion.
    let e2_up = format!(
        "CREATE OR REPLACE FUNCTION \"{schema}\".\"zsdw_contacts_merge_fn\"() \
         RETURNS trigger AS $zsdw$\n\
         BEGIN\n\
         \x20  IF (TG_OP = 'INSERT' AND NEW.phone IS NULL)\n\
         \x20     OR (TG_OP = 'UPDATE' \
                   AND (NEW.area_code IS DISTINCT FROM OLD.area_code \
                        OR NEW.local_number IS DISTINCT FROM OLD.local_number) \
                   AND NEW.phone IS NOT DISTINCT FROM OLD.phone) THEN\n\
         \x20    NEW.phone := CASE\n\
         \x20      WHEN NEW.area_code IS NOT NULL AND NEW.area_code <> '' \
                   THEN NEW.area_code || '-' || coalesce(NEW.local_number, '')\n\
         \x20      ELSE NEW.local_number\n\
         \x20    END;\n\
         \x20  END IF;\n\
         \x20  RETURN NEW;\n\
         END;\n\
         $zsdw$ LANGUAGE plpgsql;\n\
         CREATE TRIGGER \"zsdw_contacts_merge_trg\" BEFORE INSERT OR UPDATE \
         ON \"{schema}\".\"contacts\" FOR EACH ROW \
         EXECUTE FUNCTION \"{schema}\".\"zsdw_contacts_merge_fn\"()"
    );
    let e2_down = format!(
        "DROP TRIGGER IF EXISTS \"zsdw_contacts_merge_trg\" ON \"{schema}\".\"contacts\"; \
         DROP FUNCTION IF EXISTS \"{schema}\".\"zsdw_contacts_merge_fn\"()"
    );
    let e2 = ai_step(
        &raw,
        "expand_merge_dual_write",
        &e2_up,
        Some(&e2_down),
        OnlinePhase::Expand,
        false,
        vec![e1.version.clone()],
    );

    apply(
        &conn,
        &cfg,
        &[e1.clone(), e2.clone()],
        Approval::Approved,
        "tester",
    )
    .await
    .expect("apply merge expand E1+E2");
    assert!(column_exists(&conn, &schema, "contacts", "phone").await);
    assert!(trigger_exists(&conn, &schema, "contacts").await);

    // ---- E3: backfill phone from the two sources via the precedence rule ----
    let backfill = BackfillSpec {
        table: "contacts".into(),
        cursor_column: "id".into(),
        batch_size: 1000,
        set_clause: "phone = CASE \
            WHEN _bf.area_code IS NOT NULL AND _bf.area_code <> '' \
                THEN _bf.area_code || '-' || coalesce(_bf.local_number, '') \
            ELSE _bf.local_number END"
            .into(),
        filter: Some("phone IS NULL".into()),
        name: "backfill_contacts_merge".into(),
    };
    let out = run_backfill(&conn, &cfg, &backfill, Approval::Approved, "tester")
        .await
        .expect("merge backfill");
    assert!(out.complete);
    assert_eq!(out.rows_updated, 3, "all 3 seed rows merged");

    let phone_of = |id: i64| {
        let schema = schema.clone();
        let conn = &conn;
        async move {
            conn.query_one(
                &format!("SELECT phone FROM \"{schema}\".\"contacts\" WHERE id=$1"),
                &[&id],
            )
            .await
            .expect("phone_of")
            .get::<_, Option<String>>("phone")
        }
    };

    // Pre-existing rows: composed via the precedence rule (and the area-code-less
    // row uses local_number only).
    assert_eq!(phone_of(1).await, Some("415-5551234".into()));
    assert_eq!(phone_of(3).await, Some("5550000".into()), "no area code → local only");

    let app_role = make_app_role(&conn, &cfg, &tok).await;

    // (a) INSERT via the SOURCES only → trigger composes phone (with area code).
    as_app(
        &conn,
        &app_role,
        &format!(
            "INSERT INTO \"{schema}\".\"contacts\" (id, area_code, local_number) \
             VALUES (10, '650', '5552222')"
        ),
    )
    .await;
    assert_eq!(phone_of(10).await, Some("650-5552222".into()), "compose with area code");

    // (b) INSERT via the SOURCES with NO area code → local-number-only precedence.
    as_app(
        &conn,
        &app_role,
        &format!(
            "INSERT INTO \"{schema}\".\"contacts\" (id, local_number) VALUES (11, '5553333')"
        ),
    )
    .await;
    assert_eq!(phone_of(11).await, Some("5553333".into()), "no area code → local only");

    // (c) INSERT writing the NEW column directly → it wins (sources untouched).
    as_app(
        &conn,
        &app_role,
        &format!(
            "INSERT INTO \"{schema}\".\"contacts\" (id, phone) VALUES (12, '999-0000000')"
        ),
    )
    .await;
    assert_eq!(phone_of(12).await, Some("999-0000000".into()), "phone written directly wins");

    // (d) UPDATE a SOURCE → phone recomposes from the sources.
    as_app(
        &conn,
        &app_role,
        &format!("UPDATE \"{schema}\".\"contacts\" SET area_code='510' WHERE id=10"),
    )
    .await;
    assert_eq!(phone_of(10).await, Some("510-5552222".into()), "source update recomposes phone");

    // ---- CONTRACT: drop the trigger/fn (C1), then BOTH source columns (C2) ----
    let c1 = ai_step(
        &raw,
        "contract_merge_drop_dual_write",
        &e2_down,
        Some(&e2_up),
        OnlinePhase::Contract,
        false,
        vec![e2.version.clone()],
    );
    let c2 = ai_step(
        &raw,
        "contract_merge_drop_sources",
        &format!(
            "ALTER TABLE \"{schema}\".\"contacts\" \
             DROP COLUMN area_code, DROP COLUMN local_number"
        ),
        None,
        OnlinePhase::Contract,
        true,
        vec![e2.version.clone(), c1.version.clone()],
    );
    apply(
        &conn,
        &cfg,
        &[c1, c2],
        Approval::Approved,
        "tester",
    )
    .await
    .expect("apply merge contract");

    // End state: both sources gone, the merged column remains + intact, no trigger.
    assert!(!column_exists(&conn, &schema, "contacts", "area_code").await);
    assert!(!column_exists(&conn, &schema, "contacts", "local_number").await);
    assert!(column_exists(&conn, &schema, "contacts", "phone").await);
    assert!(!trigger_exists(&conn, &schema, "contacts").await);
    assert_eq!(phone_of(1).await, Some("415-5551234".into()));

    full_teardown(&conn, &cfg, Some(&app_role)).await;
}
