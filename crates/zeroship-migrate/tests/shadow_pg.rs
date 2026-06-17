//! Faithful shadow-DATABASE dry-run tests (v3 Plan C) against a REAL Postgres.
//!
//! The dry-run runs the UNMODIFIED [`zeroship_migrate::apply`] path against a
//! throwaway DATABASE clone (same `project_schema` name, confined migrator role)
//! and tears it down on every path. These tests prove:
//!
//! - a good additive set dry-runs `ok == true`, all `applied_ok` (Phase 1.1);
//! - a deliberately-broken migration dry-runs `ok == false`, the offender
//!   `applied_ok == false`, AND — the load-bearing proof — the REAL project /
//!   meta schemas are NEVER created in the admin DB (Phase 1.2);
//! - the shadow DB is dropped after BOTH the ok and error paths (Phase 1.3);
//! - a clean declarative diff yields a clean resulting-drift + ok; a wrong
//!   generated op yields non-empty resulting-drift + ok == false (Phase 2);
//! - teardown survives a failure injected after CREATE DATABASE (Phase 3);
//! - `sweep_leaked_shadows` reaps a stale clone + leaves a fresh one (Phase 3).
//!
//! Requires `CREATEDB` on the connecting role (the test `postgres` role is a
//! superuser, so CREATEDB is available).

use std::collections::HashMap;
use std::time::Duration;

use compio_postgres::Client;
use zeroship_migrate::guard::GuardConfig;
use zeroship_migrate::migration::Checksum;
use zeroship_migrate::{
    deprovision_migrator, dry_run, dry_run_declarative, migrator_role_name, sweep_leaked_shadows,
    CollectionDescriptor, DeclarativeAuthor, DryRunError, ExecutorConfig, FieldDescriptor,
    Migration, MigrationEngine, MigrationFlags, MigrationId, SchemaSnapshot, ShadowConfig,
    desired_snapshot,
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

/// A config whose migrator role matches what `provision_migrator` creates on the
/// shadow — so the shadow apply runs under the SAME least-privilege confinement
/// as a real apply.
fn cfg_for(tok: &str) -> ExecutorConfig {
    let project_id = format!("prj_{tok}");
    let role = migrator_role_name(&project_id).expect("role name");
    let mut c = ExecutorConfig::new(project_id, format!("proj_{tok}"));
    c.meta_schema = format!("meta_{tok}");
    c.statement_timeout = Duration::from_secs(30);
    c.lock_timeout = Duration::from_secs(10);
    c.migrator_role = Some(role);
    c
}

fn shadow_cfg() -> ShadowConfig {
    ShadowConfig {
        admin_dsn: dsn(),
        db_name_prefix: "zsmig_shadow_".to_string(),
    }
}

fn mig(version: MigrationId, name: &str, up: &str) -> Migration {
    Migration {
        version,
        name: name.to_string(),
        up: up.to_string(),
        down: None,
        checksum: Checksum::of(up, None),
        flags: MigrationFlags::default(),
        owner_app: "app_test".to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
    }
}

async fn schema_exists(conn: &Client, schema: &str) -> bool {
    !conn
        .query(
            "SELECT 1 FROM information_schema.schemata WHERE schema_name = $1",
            &[&schema],
        )
        .await
        .expect("query schema existence")
        .is_empty()
}

async fn shadow_db_count(conn: &Client, prefix: &str) -> i64 {
    let like = format!("{prefix}%");
    let rows = conn
        .query("SELECT datname FROM pg_database WHERE datname LIKE $1", &[&like])
        .await
        .expect("count shadow dbs");
    i64::try_from(rows.len()).unwrap()
}

/// Drop the cluster-global migrator role this test provisioned on the shadow, so
/// it does not leak across the run (the role survives the shadow DB's drop).
async fn cleanup_role(conn: &Client, cfg: &ExecutorConfig) {
    let _ = deprovision_migrator(conn, cfg).await;
}

// ---------------------------------------------------------------------------
// Phase 1.1 — a good additive set dry-runs ok.
// ---------------------------------------------------------------------------

#[compio::test]
async fn dry_run_good_additive_set_is_ok() {
    let admin = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);

    let m1 = mig(
        MigrationId::generate(),
        "create_orders",
        &format!(
            "CREATE TABLE \"{}\".\"orders\" (id bigint PRIMARY KEY, note text)",
            cfg.project_schema
        ),
    );
    let m2 = mig(
        MigrationId::generate(),
        "add_total",
        &format!(
            "ALTER TABLE \"{}\".\"orders\" ADD COLUMN total numeric",
            cfg.project_schema
        ),
    );
    let set = vec![m1.clone(), m2.clone()];

    let report = dry_run(&admin, &set, &cfg, &shadow_cfg(), "actor")
        .await
        .expect("dry_run harness");

    assert!(report.ok, "good additive set must dry-run ok: {report:?}");
    assert_eq!(report.per_migration.len(), 2);
    assert!(report.per_migration.iter().all(|r| r.applied_ok));
    assert!(report.resulting_drift.is_none(), "plain dry_run has no drift");

    cleanup_role(&admin, &cfg).await;
}

// ---------------------------------------------------------------------------
// Phase 1.2 — a broken migration dry-runs NOT ok AND never touches prod.
// ---------------------------------------------------------------------------

#[compio::test]
async fn dry_run_broken_migration_fails_and_never_touches_prod() {
    let admin = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);

    let good = mig(
        MigrationId::generate(),
        "create_t",
        &format!(
            "CREATE TABLE \"{}\".\"t\" (id bigint PRIMARY KEY)",
            cfg.project_schema
        ),
    );
    // Deliberately broken: an FK to a non-existent table fails at apply time.
    let bad = mig(
        MigrationId::generate(),
        "bad_fk",
        &format!(
            "ALTER TABLE \"{schema}\".\"t\" ADD COLUMN parent bigint \
             REFERENCES \"{schema}\".\"missing\"(id)",
            schema = cfg.project_schema
        ),
    );
    let bad_version = bad.version.as_str().to_string();
    let set = vec![good, bad];

    let report = dry_run(&admin, &set, &cfg, &shadow_cfg(), "actor")
        .await
        .expect("dry_run harness");

    assert!(!report.ok, "broken set must dry-run NOT ok");
    let offender = report
        .per_migration
        .iter()
        .find(|r| r.version == bad_version)
        .expect("offending migration in report");
    assert!(!offender.applied_ok, "the bad migration must be applied_ok=false");
    assert!(offender.error.is_some(), "the bad migration carries an error");

    // THE LOAD-BEARING PROOF: the real project + meta schemas were NEVER created
    // in the ADMIN database, and no journal exists there.
    assert!(
        !schema_exists(&admin, &cfg.project_schema).await,
        "dry_run must NOT create the real project schema in the admin DB"
    );
    assert!(
        !schema_exists(&admin, &cfg.meta_schema).await,
        "dry_run must NOT create the real meta schema in the admin DB"
    );

    cleanup_role(&admin, &cfg).await;
}

// ---------------------------------------------------------------------------
// Phase 1.3 — teardown: no shadow DB remains after ok OR error paths.
// ---------------------------------------------------------------------------

#[compio::test]
async fn dry_run_tears_down_shadow_on_ok_and_error_paths() {
    let admin = pg().await;
    let prefix = "zsmig_shadow_";

    // OK path.
    let tok_ok = token();
    let cfg_ok = cfg_for(&tok_ok);
    let good = mig(
        MigrationId::generate(),
        "ok",
        &format!(
            "CREATE TABLE \"{}\".\"x\" (id bigint PRIMARY KEY)",
            cfg_ok.project_schema
        ),
    );
    let _ = dry_run(&admin, &[good], &cfg_ok, &shadow_cfg(), "actor")
        .await
        .expect("ok dry_run");
    cleanup_role(&admin, &cfg_ok).await;

    // ERROR path.
    let tok_err = token();
    let cfg_err = cfg_for(&tok_err);
    let bad = mig(
        MigrationId::generate(),
        "bad",
        &format!(
            "ALTER TABLE \"{schema}\".\"nope\" ADD COLUMN c int",
            schema = cfg_err.project_schema
        ),
    );
    let _ = dry_run(&admin, &[bad], &cfg_err, &shadow_cfg(), "actor")
        .await
        .expect("err dry_run harness still returns Ok(report)");
    cleanup_role(&admin, &cfg_err).await;

    // No shadow database from EITHER path remains.
    assert_eq!(
        shadow_db_count(&admin, prefix).await,
        0,
        "no <prefix>% shadow database may remain after teardown"
    );
}

// ---------------------------------------------------------------------------
// Phase 1.x — CONCURRENTLY non-txn dry-run runs in the shadow (no deadlock).
// (Plan calls this optional — included since it is quick + load-bearing.)
// ---------------------------------------------------------------------------

#[compio::test]
async fn dry_run_concurrently_index_runs_in_shadow() {
    let admin = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);

    let create = mig(
        MigrationId::generate(),
        "create_u",
        &format!(
            "CREATE TABLE \"{}\".\"u\" (id bigint PRIMARY KEY, email text)",
            cfg.project_schema
        ),
    );
    let mut idx = mig(
        MigrationId::generate(),
        "idx_concurrent",
        &format!(
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS u_email_idx \
             ON \"{}\".\"u\" (email)",
            cfg.project_schema
        ),
    );
    idx.flags.transactional = false; // non-txn two-phase path
    let set = vec![create, idx];

    let report = dry_run(&admin, &set, &cfg, &shadow_cfg(), "actor")
        .await
        .expect("dry_run harness");
    assert!(report.ok, "CONCURRENTLY index must dry-run ok: {report:?}");
    assert!(report.per_migration.iter().all(|r| r.applied_ok));

    cleanup_role(&admin, &cfg).await;
}

// ---------------------------------------------------------------------------
// Phase 2 — declarative dry-run with resulting-drift validation.
// ---------------------------------------------------------------------------

fn guard_cfg(cfg: &ExecutorConfig) -> GuardConfig {
    GuardConfig {
        project_schema: cfg.project_schema.clone(),
        extension_allowlist: Vec::new(),
    }
}

fn descriptor(name: &str, fields: Vec<FieldDescriptor>) -> CollectionDescriptor {
    CollectionDescriptor {
        name: name.into(),
        owner_app: "app_test".into(),
        fields,
        indexes: vec![],
    }
}

fn field(name: &str, ty: &str, required: bool) -> FieldDescriptor {
    FieldDescriptor {
        name: name.into(),
        ty: ty.into(),
        required,
        unique: false,
        references: None,
    }
}

/// Phase 2 test 1: a clean declarative diff (desired vs an EMPTY live) yields a
/// CLEAN resulting-drift + ok. The generated plain CREATE-TABLE plan, dry-run on
/// the shadow, realises EXACTLY the desired schema (zero drift).
#[compio::test]
async fn dry_run_declarative_clean_diff_is_ok_with_clean_drift() {
    let admin = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);

    let desc = descriptor(
        "members",
        vec![
            field("handle", "string", true),
            field("score", "number", false),
        ],
    );
    let desired = desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot");

    // Plan the diff against an EMPTY live snapshot (everything additive).
    let engine = MigrationEngine::new();
    let author = DeclarativeAuthor::new(cfg.project_schema.clone(), "app_test");
    let empty_live = SchemaSnapshot::default();
    let plan = engine
        .plan_declarative(
            &desired,
            &empty_live,
            &HashMap::new(),
            &author,
            &[],
            &guard_cfg(&cfg),
        )
        .expect("plan_declarative");

    let report =
        dry_run_declarative(&admin, &plan, &desired, &cfg, &shadow_cfg(), "actor")
            .await
            .expect("dry_run_declarative harness");

    let drift = report
        .resulting_drift
        .as_ref()
        .expect("declarative dry-run carries resulting_drift");
    assert!(
        drift.is_clean(),
        "the generated plan must realise the desired schema (clean drift): {drift:?}"
    );
    assert!(report.ok, "clean declarative diff must be ok: {report:?}");

    cleanup_role(&admin, &cfg).await;
}

/// Phase 2 test 2: a plan that does NOT realise the schema we validate against
/// yields a NON-empty resulting-drift + ok == false. We build the plan from
/// `desired_a` (table `widgets`) but validate the shadow result against
/// `desired_b` (table `gadgets`) — the shadow ends up with `widgets`, so diffing
/// against `gadgets` surfaces `gadgets` missing + `widgets` unexpected. This is
/// the faithful analogue of an intentionally-wrong generated op: the realised
/// schema diverges from the desired snapshot, caught before any real apply.
#[compio::test]
async fn dry_run_declarative_wrong_result_has_nonempty_drift_and_not_ok() {
    let admin = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);

    let desc_a = descriptor("widgets", vec![field("label", "string", true)]);
    let desired_a = desired_snapshot(&cfg.project_schema, &[desc_a]).expect("desired_a");

    let desc_b = descriptor("gadgets", vec![field("label", "string", true)]);
    let desired_b = desired_snapshot(&cfg.project_schema, &[desc_b]).expect("desired_b");

    // The plan is generated for desired_a (creates `widgets`)...
    let engine = MigrationEngine::new();
    let author = DeclarativeAuthor::new(cfg.project_schema.clone(), "app_test");
    let empty_live = SchemaSnapshot::default();
    let plan_a = engine
        .plan_declarative(
            &desired_a,
            &empty_live,
            &HashMap::new(),
            &author,
            &[],
            &guard_cfg(&cfg),
        )
        .expect("plan_declarative");

    // ...but we validate the shadow result against desired_b (expects `gadgets`).
    let report =
        dry_run_declarative(&admin, &plan_a, &desired_b, &cfg, &shadow_cfg(), "actor")
            .await
            .expect("dry_run_declarative harness");

    let drift = report
        .resulting_drift
        .as_ref()
        .expect("declarative dry-run carries resulting_drift");
    assert!(
        !drift.is_clean(),
        "the realised schema diverges from desired_b — drift must be non-empty"
    );
    assert!(
        drift.missing_objects.iter().any(|m| m == "gadgets"),
        "gadgets is desired but the shadow never got it: {drift:?}"
    );
    assert!(
        drift.unexpected_objects.iter().any(|m| m == "widgets"),
        "widgets was created but is not in desired_b: {drift:?}"
    );
    assert!(!report.ok, "a non-clean resulting drift must set ok=false");

    // Even on the not-ok path the shadow DB is gone.
    assert_eq!(shadow_db_count(&admin, "zsmig_shadow_").await, 0);

    cleanup_role(&admin, &cfg).await;
}

// ---------------------------------------------------------------------------
// Phase 3 — teardown-on-FAILURE hardening + leaked-shadow sweeper.
// ---------------------------------------------------------------------------

/// Phase 3 test 1: inject a HARNESS failure AFTER `CREATE DATABASE` but before
/// apply completes, and assert the shadow DB is STILL dropped.
///
/// The injection: an EMPTY `project_id` makes the in-body `provision_migrator`
/// fail with `BadRoleName` (the role name derives from project_id), which lands
/// AFTER the shadow DB was created and the schema set up — exactly the
/// crash-before-teardown window. We use a UNIQUELY-prefixed shadow DB so we can
/// prove no `<unique_prefix>%` database survives.
#[compio::test]
async fn dry_run_drops_shadow_when_provision_fails_after_create() {
    let admin = pg().await;
    // A unique prefix so we observe ONLY this test's shadow DB.
    let unique = format!("zsmig_fail_{}_", token().replace('_', ""));
    let unique_prefix: String = unique.chars().take(40).collect();

    // project_schema is valid (CREATE SCHEMA succeeds) but project_id is EMPTY,
    // so provision_migrator fails AFTER CREATE DATABASE + CREATE SCHEMA.
    let tok = token();
    let mut cfg = ExecutorConfig::new(String::new(), format!("proj_{tok}"));
    cfg.meta_schema = format!("meta_{tok}");
    cfg.migrator_role = Some("migrator_unused".to_string());

    let m = mig(
        MigrationId::generate(),
        "create",
        &format!(
            "CREATE TABLE \"{}\".\"t\" (id bigint PRIMARY KEY)",
            cfg.project_schema
        ),
    );
    let shadow_cfg = ShadowConfig {
        admin_dsn: dsn(),
        db_name_prefix: unique_prefix.clone(),
    };

    let result = dry_run(&admin, &[m], &cfg, &shadow_cfg, "actor").await;

    // The harness surfaced the provisioning failure as an Err...
    assert!(
        matches!(result, Err(DryRunError::Provision(_))),
        "empty project_id must fail provisioning: {result:?}"
    );
    // ...AND the shadow DB created before the failure is STILL dropped.
    assert_eq!(
        shadow_db_count(&admin, &unique_prefix).await,
        0,
        "teardown must drop the shadow DB even when the body fails after CREATE DATABASE"
    );
}

/// Phase 3 test 2: `sweep_leaked_shadows` drops a STALE crash-leaked clone and
/// leaves a FRESH one (and a non-matching DB) untouched.
#[compio::test]
async fn sweep_leaked_shadows_drops_stale_and_keeps_fresh() {
    let admin = pg().await;
    let prefix = format!("zsmig_sweep_{}_", token().replace('_', ""));
    let prefix: String = prefix.chars().take(38).collect();

    // A STALE clone: name carries an OLD embedded nanos timestamp (1s past epoch),
    // matching fresh_shadow_name's `<prefix><pid>_<nanos_hex>_<n>` shape.
    let old_nanos_hex = format!("{:x}", 1_000_000_000u128); // ~1970, definitely stale
    let stale = format!("{prefix}1_{old_nanos_hex}_0");
    // A FRESH clone: a recent embedded timestamp (now).
    let now_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let fresh = format!("{prefix}1_{now_nanos:x}_1");
    // A non-matching DB sharing the prefix but NOT the timestamp shape — never reaped.
    let unparseable = format!("{prefix}plain");

    for name in [&stale, &fresh, &unparseable] {
        admin
            .batch_execute(&format!("CREATE DATABASE \"{name}\""))
            .await
            .unwrap_or_else(|e| panic!("create {name}: {e}"));
    }

    // Sweep anything older than 1 hour: only the ~1970 stale clone qualifies.
    let dropped = sweep_leaked_shadows(&admin, &prefix, Duration::from_secs(3600))
        .await
        .expect("sweep");
    assert_eq!(dropped, 1, "exactly the one stale clone is dropped");

    let remaining = shadow_db_count(&admin, &prefix).await;
    assert_eq!(remaining, 2, "the fresh + the unparseable DBs survive");

    // Cleanup the two survivors.
    for name in [&fresh, &unparseable] {
        let _ = admin
            .batch_execute(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
            .await;
    }
}
