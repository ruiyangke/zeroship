#![cfg(feature = "native-pg")]
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
use futures::FutureExt;
use zeroship_migrate::guard::GuardConfig;
use zeroship_migrate::model::migration::Checksum;
use zeroship_migrate::{
    PostgresBackend,
    deprovision_migrator, dry_run, dry_run_declarative, migrator_role_name, provision_migrator,
    snapshot_schema, sweep_leaked_shadows, Approval, CollectionDescriptor, DeclarativeAuthor,
    DeclarativeDeployPlan, DryRunError, ExecutorConfig, FieldDescriptor, Migration, MigrationEngine,
    MigrationFlags, MigrationId, RenameHint, SchemaSnapshot, ShadowConfig, desired_snapshot,
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
    c.pg.meta_schema = format!("meta_{tok}");
    c.pg.statement_timeout = Duration::from_secs(30);
    c.pg.lock_timeout = Duration::from_secs(10);
    c.pg.migrator_role = Some(role);
    c
}

fn shadow_cfg() -> ShadowConfig {
    ShadowConfig {
        admin_dsn: dsn(),
        db_name_prefix: "zsmig_shadow_".to_string(),
    }
}

/// A shadow config with a UNIQUE db-name prefix, so a teardown assertion that
/// counts `<prefix>%` databases is scoped to ONE test and never races with
/// sibling tests that share the default `zsmig_shadow_` prefix.
fn unique_shadow_cfg() -> (ShadowConfig, String) {
    let prefix: String = format!("zsmig_u_{}_", token().replace('_', ""))
        .chars()
        .take(40)
        .collect();
    (
        ShadowConfig {
            admin_dsn: dsn(),
            db_name_prefix: prefix.clone(),
        },
        prefix,
    )
}

fn mig(version: MigrationId, name: &str, up: &str) -> Migration {
    Migration {
        version,
        name: name.to_string(),
        up: up.to_string(),
        down: None,
        checksum: Checksum::of(&zeroship_migrate::ChecksumInput {
            up,
            down: None,
            flags: &MigrationFlags::default(),
            owner_app: "app_test",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        }),
        flags: MigrationFlags::default(),
        owner_app: "app_test".to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        existence_guard: None,
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

/// Count cluster-global roles whose name matches `<prefix>%` (used to prove the
/// shadow's migrator role does not leak past teardown; H1).
async fn role_count(conn: &Client, prefix: &str) -> i64 {
    let like = format!("{prefix}%");
    let rows = conn
        .query("SELECT rolname FROM pg_roles WHERE rolname LIKE $1", &[&like])
        .await
        .expect("count roles");
    i64::try_from(rows.len()).unwrap()
}

/// True iff a cluster-global role with this exact name exists.
async fn role_exists(conn: &Client, role: &str) -> bool {
    !conn
        .query("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&role])
        .await
        .expect("query role existence")
        .is_empty()
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
        !schema_exists(&admin, &cfg.pg.meta_schema).await,
        "dry_run must NOT create the real meta schema in the admin DB"
    );
}

// ---------------------------------------------------------------------------
// Phase 1.3 — teardown: no shadow DB remains after ok OR error paths.
// ---------------------------------------------------------------------------

#[compio::test]
async fn dry_run_tears_down_shadow_on_ok_and_error_paths() {
    let admin = pg().await;
    // A UNIQUE prefix scopes the count to THIS test (the default-prefix shadow
    // DBs of concurrent sibling tests must not race this assertion).
    let (scfg, prefix) = unique_shadow_cfg();

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
    let _ = dry_run(&admin, &[good], &cfg_ok, &scfg, "actor")
        .await
        .expect("ok dry_run");

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
    let _ = dry_run(&admin, &[bad], &cfg_err, &scfg, "actor")
        .await
        .expect("err dry_run harness still returns Ok(report)");

    // No shadow database from EITHER path remains (scoped to this test's prefix).
    assert_eq!(
        shadow_db_count(&admin, &prefix).await,
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
    idx.checksum = Checksum::of(&zeroship_migrate::ChecksumInput::from_migration(&idx));
    let set = vec![create, idx];

    let report = dry_run(&admin, &set, &cfg, &shadow_cfg(), "actor")
        .await
        .expect("dry_run harness");
    assert!(report.ok, "CONCURRENTLY index must dry-run ok: {report:?}");
    assert!(report.per_migration.iter().all(|r| r.applied_ok));
}

// ---------------------------------------------------------------------------
// Phase 2 — declarative dry-run with resulting-drift validation.
// ---------------------------------------------------------------------------

fn guard_cfg(cfg: &ExecutorConfig) -> GuardConfig {
    GuardConfig::confined(cfg.project_schema.clone())
}

fn descriptor(name: &str, fields: Vec<FieldDescriptor>) -> CollectionDescriptor {
    CollectionDescriptor {
        name: name.into(),
        owner_app: "app_test".into(),
        fields,
        indexes: vec![],
    runtime_options: Default::default(),
    }
}

fn field(name: &str, ty: &str, required: bool) -> FieldDescriptor {
    FieldDescriptor {
        name: name.into(),
        ty: ty.into(),
        required,
        unique: false,
        references: None,
        ..Default::default()
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
    // Use a unique prefix so the not-ok-path teardown count is race-free.
    let (scfg, prefix) = unique_shadow_cfg();
    let report = dry_run_declarative(&admin, &plan_a, &desired_b, &cfg, &scfg, "actor")
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

    // Even on the not-ok path the shadow DB is gone (scoped to this test's prefix).
    assert_eq!(shadow_db_count(&admin, &prefix).await, 0);
}

// ---------------------------------------------------------------------------
// Phase 3 — teardown-on-FAILURE hardening + leaked-shadow sweeper.
// ---------------------------------------------------------------------------

/// Phase 3 test 1: inject a HARNESS failure AFTER `CREATE DATABASE` but before
/// apply completes, and assert the shadow DB (+ role) is STILL dropped.
///
/// The injection: an EMPTY `project_schema` makes the in-body `CREATE SCHEMA ""`
/// fail (zero-length delimited identifier), which lands AFTER `CREATE DATABASE` +
/// the shadow connect — exactly the crash-before-teardown window. (Pre-H1 this
/// test injected via an empty `project_id`/`BadRoleName`; H1 now derives the
/// shadow's role from the shadow DB name, so the role name is always valid — the
/// injection moved to the schema-creation step, which is still after
/// `CREATE DATABASE`.) We use a UNIQUELY-prefixed shadow DB so we can prove no
/// `<unique_prefix>%` database survives.
#[compio::test]
async fn dry_run_drops_shadow_when_provision_fails_after_create() {
    let admin = pg().await;
    // A unique prefix so we observe ONLY this test's shadow DB.
    let unique = format!("zsmig_fail_{}_", token().replace('_', ""));
    let unique_prefix: String = unique.chars().take(40).collect();

    // project_id is valid (the shadow derives its own role from the DB name), but
    // project_schema is EMPTY → `CREATE SCHEMA ""` fails AFTER CREATE DATABASE +
    // shadow connect.
    let tok = token();
    let mut cfg = ExecutorConfig::new(format!("prj_{tok}"), String::new());
    cfg.pg.meta_schema = format!("meta_{tok}");
    cfg.pg.migrator_role = Some("migrator_unused".to_string());

    let m = mig(
        MigrationId::generate(),
        "create",
        "CREATE TABLE \"t\" (id bigint PRIMARY KEY)",
    );
    let shadow_cfg = ShadowConfig {
        admin_dsn: dsn(),
        db_name_prefix: unique_prefix.clone(),
    };

    let result = dry_run(&admin, &[m], &cfg, &shadow_cfg, "actor").await;

    // The harness surfaced the post-CREATE-DATABASE failure as an Err...
    assert!(
        matches!(result, Err(DryRunError::Admin(_))),
        "empty project_schema must fail CREATE SCHEMA after CREATE DATABASE: {result:?}"
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

/// H2(c): the sweeper is continue-on-error — it does NOT early-abort. With THREE
/// stale clones it reaps ALL of them in ONE call (returning a count of 3), proving
/// the loop continues across targets rather than `?`-bailing after the first.
/// (A genuinely-undroppable DB is hard to stage deterministically; this asserts
/// the loop's continue-and-count contract, which is the behavior H2(c) added.)
#[compio::test]
async fn sweep_leaked_shadows_continues_across_multiple_targets() {
    let admin = pg().await;
    let prefix = format!("zsmig_multi_{}_", token().replace('_', ""));
    let prefix: String = prefix.chars().take(36).collect();

    // Three STALE clones (all ~1970, so all qualify for an "older than 1h" sweep).
    let old_hex = format!("{:x}", 1_000_000_000u128);
    let stale: Vec<String> = (0..3).map(|i| format!("{prefix}1_{old_hex}_{i}")).collect();
    for name in &stale {
        admin
            .batch_execute(&format!("CREATE DATABASE \"{name}\""))
            .await
            .unwrap_or_else(|e| panic!("create {name}: {e}"));
    }

    let dropped = sweep_leaked_shadows(&admin, &prefix, Duration::from_secs(3600))
        .await
        .expect("sweep");
    assert_eq!(dropped, 3, "the sweep reaps ALL stale clones, not just the first");
    assert_eq!(
        shadow_db_count(&admin, &prefix).await,
        0,
        "no stale clone survives a continue-on-error sweep"
    );
}

// ---------------------------------------------------------------------------
// C1 (CRITICAL) — teardown is panic-safe: a panic in the dry-run body STILL
// drops the shadow DB (+ role), and the panic still propagates.
// ---------------------------------------------------------------------------

/// C1 regression. Arm the in-body panic seam (`arm_panic_after_provision`), run a
/// dry-run, and prove BOTH halves of the "always tear down" contract under a
/// panic: (a) the panic PROPAGATES (`catch_unwind` re-raises it), and (b) the
/// shadow DB created before the panic is STILL dropped — and the shadow role too.
///
/// Pre-fix (no `catch_unwind` around the body) this LEAKS the shadow DB: the
/// unwind skips `teardown_shadow`. The assertion that no `<prefix>%` DB remains
/// is RED.
#[compio::test]
async fn dry_run_teardown_survives_a_body_panic() {
    let admin = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    // A unique prefix so we observe ONLY this test's shadow DB.
    let unique_tail: String = token().replace('_', "").chars().rev().take(8).collect();
    let unique_prefix = format!("zp{unique_tail}");
    let scfg = ShadowConfig {
        admin_dsn: dsn(),
        db_name_prefix: unique_prefix.clone(),
    };

    let m = mig(
        MigrationId::generate(),
        "create",
        &format!(
            "CREATE TABLE \"{}\".\"t\" (id bigint PRIMARY KEY)",
            cfg.project_schema
        ),
    );

    // Arm the seam (per-thread, so parallel sibling tests are unaffected), run the
    // dry-run under catch_unwind at the TEST boundary, then disarm. The body panics
    // AFTER the shadow DB + role are provisioned — exactly the crash-before-teardown
    // window. We catch the unwind on the FUTURE (the same combinator the production
    // fix uses) so the test observes propagation without aborting.
    zeroship_migrate::arm_panic_after_provision(true);
    let caught = std::panic::AssertUnwindSafe(dry_run(&admin, &[m], &cfg, &scfg, "actor"))
        .catch_unwind()
        .await;
    zeroship_migrate::arm_panic_after_provision(false);

    // (a) The panic PROPAGATED — the body's panic was re-raised, not swallowed.
    assert!(
        caught.is_err(),
        "the injected body panic must propagate out of dry_run (resume_unwind)"
    );

    // (b) The shadow DB created before the panic was STILL dropped (teardown ran
    // on the unwind path). RED pre-fix (the unwind skipped teardown).
    assert_eq!(
        shadow_db_count(&admin, &unique_prefix).await,
        0,
        "teardown must drop the shadow DB even when the body PANICS"
    );
    // And the shadow's cluster-global role was dropped too (scoped to THIS test's
    // unique prefix so it never races a sibling's role).
    let shadow_role_prefix = format!("migrator_{unique_prefix}");
    assert_eq!(
        role_count(&admin, &shadow_role_prefix).await,
        0,
        "teardown must drop the shadow migrator role even when the body PANICS"
    );
}

// ---------------------------------------------------------------------------
// H1 (HIGH) — the shadow provisions a UNIQUELY-NAMED migrator role that is torn
// down, and never the real project's role.
// ---------------------------------------------------------------------------

/// H1 regression. A dry-run must NOT leave any shadow migrator role behind, and must
/// NEVER provision the REAL project's migrator role. Pre-fix the
/// shadow called `provision_migrator` with the real `cfg` (creating
/// the real project role, cluster-global, surviving DROP DATABASE) and never
/// dropped it.
#[compio::test]
async fn dry_run_provisions_unique_shadow_role_and_drops_it() {
    let admin = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    // The REAL project role that pre-fix would have leaked.
    let real_role = migrator_role_name(&cfg.project_id).expect("real role name");

    // A UNIQUE shadow prefix so the role-leak count is scoped to THIS test (the
    // global `migrator_%` count races with parallel sibling dry-runs).
    let unique_tail: String = token().replace('_', "").chars().rev().take(8).collect();
    let unique_prefix = format!("zh{unique_tail}");
    let scfg = ShadowConfig {
        admin_dsn: dsn(),
        db_name_prefix: unique_prefix.clone(),
    };
    // The role derivation keeps the first readable bytes of the shadow DB name
    // before its hash suffix, so a short unique DB prefix scopes this check.
    let shadow_role_prefix = format!("migrator_{unique_prefix}");

    let m = mig(
        MigrationId::generate(),
        "create",
        &format!(
            "CREATE TABLE \"{}\".\"t\" (id bigint PRIMARY KEY)",
            cfg.project_schema
        ),
    );
    let report = dry_run(&admin, &[m], &cfg, &scfg, "actor")
        .await
        .expect("dry_run harness");
    assert!(report.ok, "additive dry-run ok: {report:?}");
    // Teardown reported clean (H2 signal).
    assert!(report.teardown_ok, "teardown_ok must be true: {report:?}");
    assert!(report.teardown_error.is_none());

    // The REAL project's role was NEVER created (the shadow used its own).
    assert!(
        !role_exists(&admin, &real_role).await,
        "dry_run must NOT provision the real project's role {real_role}"
    );
    // No shadow role survives teardown (RED pre-fix: the role outlived DROP
    // DATABASE and was never dropped). Scoped to this test's unique prefix.
    assert_eq!(
        role_count(&admin, &shadow_role_prefix).await,
        0,
        "the shadow's migrator role must be dropped on teardown (H1)"
    );
}

// ---------------------------------------------------------------------------
// #5d (correctness) — a declarative RENAME dry-run validates the FINAL
// post-contract state and does NOT false-positive on drift.
// ---------------------------------------------------------------------------

/// #5d regression. A declarative rename's plan applies only the EXPAND on deploy
/// N (the `<from>` column stays; the contract is DEFERRED). If the dry-run
/// snapshots right after the expand, `<from>` is still present and a CORRECT
/// rename false-positives on drift (`desired` lacks `<from>`).
///
/// We construct a self-contained plan that simulates BOTH deploys in the shadow:
/// the plain set creates `users(email)`, and the rename's expand-contract turns
/// `email` → `email_address`. With the #5d fix, `dry_run_declarative` also applies
/// the DEFERRED contract in the shadow, so `email` is dropped and the final state
/// equals `desired` (`users(email_address)`) → CLEAN drift + ok.
///
/// Pre-fix (contract not applied) the shadow keeps BOTH `email` and
/// `email_address`, so diffing against `desired` flags `email` as unexpected →
/// non-clean drift + ok=false. RED.
#[compio::test]
async fn dry_run_declarative_rename_validates_final_state_no_false_drift() {
    let admin = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    let engine = MigrationEngine::new();
    let author = DeclarativeAuthor::new(cfg.project_schema.clone(), "app_test");

    let users_email = descriptor("users", vec![field("email", "string", false)]);
    let users_email_addr = descriptor("users", vec![field("email_address", "string", false)]);

    // The "live" intermediate the rename diffs against: users(email).
    let live_email = desired_snapshot(&cfg.project_schema, std::slice::from_ref(&users_email))
        .expect("live snapshot")
        .snapshot;
    // The DESIRED end state: users(email_address).
    let desired = desired_snapshot(&cfg.project_schema, &[users_email_addr]).expect("desired");

    // Plain part: create users(email) from empty (so the shadow has the table +
    // <from> column the rename's expand operates on — simulating deploy 0).
    let create_diff = author
        .diff(
            &desired_snapshot(&cfg.project_schema, &[users_email]).expect("create desired"),
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &[],
        )
        .expect("create-users diff");
    assert!(
        create_diff.renames.is_empty() && !create_diff.migrations.is_empty(),
        "the create part is a plain CREATE TABLE: {create_diff:?}"
    );
    let plain = engine.plan(&create_diff.migrations, &guard_cfg(&cfg));

    // Rename part: users(email_address) vs live users(email), with a hint →
    // structured expand-contract (the <from> drop is the DEFERRED contract).
    let hints = vec![RenameHint {
        table: "users".into(),
        from: "email".into(),
        to: "email_address".into(),
    }];
    let rename_diff = author
        .diff(&desired, &live_email, &HashMap::new(), &hints)
        .expect("rename diff");
    assert_eq!(rename_diff.renames.len(), 1, "exactly one structured rename");
    assert!(
        rename_diff.migrations.is_empty(),
        "a pure rename produces no plain migrations: {rename_diff:?}"
    );

    // Combine into ONE declarative deploy plan: create + rename, applied in the
    // same throwaway shadow (both deploys simulated).
    let plan = DeclarativeDeployPlan {
        plain,
        renames: rename_diff.renames,
        // PG path: never produces SQLite rebuilds.
        rebuilds: Vec::new(),
    };

    let report = dry_run_declarative(&admin, &plan, &desired, &cfg, &shadow_cfg(), "actor")
        .await
        .expect("dry_run_declarative harness");

    let drift = report
        .resulting_drift
        .as_ref()
        .expect("declarative dry-run carries resulting_drift");
    // The CRUX (#5d): <from> (`email`) must NOT show as unexpected — the deferred
    // contract dropped it in the shadow, so the final state equals desired.
    assert!(
        drift.is_clean(),
        "a correct rename must NOT false-positive on drift after the deferred \
         contract is applied; got: {drift:?}"
    );
    assert!(
        report.ok,
        "a correct declarative rename dry-run must be ok: {report:?}"
    );
}

// ---------------------------------------------------------------------------
// H4 (correctness) — an INCREMENTAL declarative dry-run is faithful: the shadow
// is SEEDED with the live project structure before the plan applies, so a plan
// that targets a PRIOR-deploy table dry-runs ok (not a false negative).
// ---------------------------------------------------------------------------

/// H4 regression. Deploy 1 creates `contacts(phone)` on the REAL project DB.
/// Deploy 2 is an INCREMENTAL declarative plan that ADDs a column `email` to the
/// existing `contacts` table — its `ALTER TABLE "contacts" ADD COLUMN …` targets
/// a table the plan itself does NOT create (it lives only in the live schema).
///
/// Pre-fix the shadow started EMPTY, so the ADD COLUMN hit `relation "contacts"
/// does not exist` → the dry-run reported ok=false / `missing_objects:["contacts"]`
/// for a plan the real apply runs cleanly (RED — a false negative that would
/// block a valid deploy). With the seed fix `dry_run_declarative` reconstructs
/// the live `contacts` table in the shadow FIRST, so the incremental ADD COLUMN
/// applies and the dry-run is ok with clean resulting drift (GREEN).
#[compio::test]
async fn dry_run_declarative_incremental_add_column_on_prior_table_is_ok() {
    let admin = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);

    // --- Stand up the LIVE project schema + migrator role, then DEPLOY 1:
    //     create `contacts(phone)` on the real project DB. ---
    admin
        .batch_execute(&format!(
            "CREATE SCHEMA IF NOT EXISTS \"{}\"",
            cfg.project_schema
        ))
        .await
        .expect("create live project schema");
    provision_migrator(&admin, &cfg)
        .await
        .expect("provision live migrator role");

    let engine = MigrationEngine::new();
    let author = DeclarativeAuthor::new(cfg.project_schema.clone(), "app_test");

    let contacts_phone = descriptor("contacts", vec![field("phone", "string", false)]);
    let d1 = desired_snapshot(&cfg.project_schema, std::slice::from_ref(&contacts_phone))
        .expect("deploy-1 desired");
    let plan1 = engine
        .plan_declarative(
            &d1,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &author,
            &[],
            &guard_cfg(&cfg),
        )
        .expect("plan deploy 1");
    engine
        .apply(&plan1.plain, Approval::None, &PostgresBackend::new(&admin), &cfg, "app_test")
        .await
        .expect("apply deploy 1 on live");

    // --- DEPLOY 2 (INCREMENTAL): desire `contacts(phone, email)`. The plan vs the
    //     live snapshot is a single ADD COLUMN `email` on the EXISTING table — it
    //     does NOT re-create `contacts`. ---
    let contacts_phone_email = descriptor(
        "contacts",
        vec![field("phone", "string", false), field("email", "string", false)],
    );
    let d2 = desired_snapshot(&cfg.project_schema, &[contacts_phone_email]).expect("deploy-2 desired");
    let live = snapshot_schema(&admin, &cfg.project_schema)
        .await
        .expect("snapshot live after deploy 1");
    assert!(
        live.tables.contains_key("contacts"),
        "deploy 1 must have created `contacts` in the live schema"
    );

    let plan2 = engine
        .plan_declarative(
            &d2,
            &live,
            &HashMap::new(),
            &author,
            &[],
            &guard_cfg(&cfg),
        )
        .expect("plan deploy 2 (incremental)");
    assert!(
        plan2.renames.is_empty() && !plan2.plain.items.is_empty(),
        "deploy 2 is a plain incremental ADD COLUMN: {plan2:?}"
    );

    // --- The incremental dry-run: seeds the live `contacts`, then applies the
    //     ADD COLUMN — ok with clean resulting drift. ---
    let report = dry_run_declarative(&admin, &plan2, &d2, &cfg, &shadow_cfg(), "app_test")
        .await
        .expect("dry_run_declarative incremental harness");

    let drift = report
        .resulting_drift
        .as_ref()
        .expect("declarative dry-run carries resulting_drift");
    assert!(
        drift.is_clean(),
        "the incremental ADD COLUMN must realise the desired schema on the SEEDED \
         shadow (clean drift, not a `relation does not exist` false negative): {drift:?}"
    );
    assert!(
        report.ok,
        "an incremental declarative dry-run against a prior-deploy table must be ok \
         (H4: the shadow is seeded with the live structure first): {report:?}"
    );

    // The seed never touched prod beyond the seeded deploy-1 state: `email` is NOT
    // present on the LIVE table (the dry-run only added it inside the throwaway
    // shadow). This guards the never-touches-prod invariant for the seed path.
    let live_after = snapshot_schema(&admin, &cfg.project_schema)
        .await
        .expect("snapshot live after dry-run");
    let contacts = live_after.tables.get("contacts").expect("contacts still live");
    assert!(
        contacts.columns.iter().any(|c| c.name == "phone"),
        "live `contacts.phone` survives"
    );
    assert!(
        !contacts.columns.iter().any(|c| c.name == "email"),
        "the dry-run must NOT have added `email` to the LIVE table (shadow-only)"
    );

    // Teardown the live schema + migrator role this test created.
    let _ = deprovision_migrator(&admin, &cfg).await;
    let _ = admin
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.pg.meta_schema
        ))
        .await;
}
