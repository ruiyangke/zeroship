//! P1/P2 cross-feature END-TO-END lifecycle tests against a REAL Postgres (no
//! shims) — the sequel to `lifecycle_pg` (P0).
//!
//! Where the per-feature suites (`baseline_pg`, `squash_pg`, `repeatable_pg`,
//! `precondition_pg`, `manifest_pg`, `declarative_pg`, `analyze`, `shadow_pg`,
//! `expand_contract_pg`) each prove one feature in isolation, this suite proves
//! the features COMPOSE across multi-deploy lifecycles, driving the PUBLIC engine
//! API end-to-end the way the control plane will — and using two scenarios to
//! probe a specific integration hypothesis (H5: squash over a baselined version;
//! H6: an expand carrying an unmet precondition vs. the contract gate).
//!
//! FAITHFUL: every test drives the public API on the real project DB. No internal
//! shims, no partial hand-applies of a sequence. A baseline goes through
//! [`baseline`]; a squash through [`squash`]; a destructive set through
//! `analyze_migration` → `dry_run` → the two-layer approval gate → `apply`; a
//! manifest tamper through `apply_verified`; a multi-app union through
//! `plan_declarative` + `apply` under per-app ownership.
//!
//! Requires the dedicated database `zeroship_migrate_test` on :5440 and a
//! connecting role with `CREATEROLE`. Each test runs in its OWN meta + project
//! schema + project id (unique token) for isolation.

use std::collections::HashMap;

use compio_postgres::Client;
use zeroship_migrate::executor::ApplyError;
use zeroship_migrate::{
    analyze_migration, baseline, check_checksum_drift, compute_manifest, desired_snapshot,
    diff_snapshots, history, migrator_role_name, provision_migrator, role::deprovision_migrator,
    snapshot_schema, squash, status, Approval, Checksum, ChecksumInput, CollectionDescriptor,
    DeclarativeError, EngineError, ExecutorConfig, FieldDescriptor, GuardConfig, HistoryKind,
    ManifestHash, Migration, MigrationEngine, MigrationFlags, MigrationId, OnUnmet, Precondition,
    PreconditionCheck, RenameHint, SchemaSnapshot, Severity, ShadowConfig, SquashError,
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

/// A config + matching least-privilege migrator role for an isolated project.
fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(format!("prj_{tok}"), format!("proj_{tok}"));
    c.meta_schema = format!("meta_{tok}");
    let role = migrator_role_name(&c.project_id).unwrap();
    c.with_migrator_role(role)
}

fn guard_cfg(cfg: &ExecutorConfig) -> GuardConfig {
    GuardConfig {
        project_schema: cfg.project_schema.clone(),
        extension_allowlist: Vec::new(),
    }
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
            cfg.project_schema, cfg.meta_schema
        ))
        .await;
}

/// Hand a project table to the migrator role (mirrors the platform, where project
/// tables are owned by the migrator so its DDL works under SET ROLE).
async fn give_table_to_migrator(conn: &Client, cfg: &ExecutorConfig, table: &str) {
    let role = cfg.migrator_role.as_ref().unwrap();
    conn.batch_execute(&format!(
        "ALTER TABLE \"{}\".\"{table}\" OWNER TO \"{role}\"",
        cfg.project_schema
    ))
    .await
    .expect("transfer table ownership to migrator");
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

/// A `ShadowConfig` for the dry-run path: clones into a throwaway database (same
/// admin DSN, a unique prefix so parallel tests never collide).
fn shadow_cfg(tok: &str) -> ShadowConfig {
    ShadowConfig {
        admin_dsn: dsn(),
        db_name_prefix: format!("zsmig_life2_{}_", tok.replace(['.', '-'], "_")),
    }
}

/// Build a transactional versioned migration with a correct checksum.
fn mig(version: MigrationId, name: &str, up: &str, schema: &str) -> Migration {
    let up = up.replace("{s}", schema);
    let mut m = Migration {
        version,
        name: name.to_string(),
        up,
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
        supersedes: Vec::new(),
        preconditions: Vec::new(),
    };
    m.recompute_checksum();
    m
}

/// Net-applied (completed) versions, sorted.
async fn applied_versions(conn: &Client, cfg: &ExecutorConfig) -> Vec<String> {
    let mut v: Vec<String> = zeroship_migrate::applied(conn, cfg)
        .await
        .expect("read journal")
        .into_iter()
        .filter(|e| matches!(e.phase, zeroship_migrate::journal::Phase::Completed))
        .map(|e| e.version)
        .collect();
    v.sort();
    v
}

/// The journaled `kind` of the LATEST completed event for a version, read straight
/// from the meta journal (so we anchor on what the ENGINE recorded, not flags).
async fn journaled_kind(conn: &Client, cfg: &ExecutorConfig, version: &str) -> Option<String> {
    let rows = conn
        .query(
            &format!(
                "SELECT kind FROM \"{}\".schema_migrations \
                 WHERE version = $1 AND phase = 'completed' \
                 ORDER BY event_seq DESC LIMIT 1",
                cfg.meta_schema
            ),
            &[&version],
        )
        .await
        .expect("journaled_kind");
    rows.first().map(|r| r.get::<_, String>("kind"))
}

/// Count RAW `completed` events for a version (every re-apply appends one).
async fn completed_events(conn: &Client, cfg: &ExecutorConfig, version: &str) -> i64 {
    conn.query_one(
        &format!(
            "SELECT count(*)::bigint AS n FROM \"{}\".schema_migrations \
             WHERE version = $1 AND phase = 'completed'",
            cfg.meta_schema
        ),
        &[&version],
    )
    .await
    .expect("completed_events")
    .get("n")
}

// ===========================================================================
// L-5 — baseline → apply-on-top → drift (#31), probes H5 squash-over-baseline.
//
// Hand-create a legacy table, baseline() it (kind='baseline', no DDL), then apply
// a NEW additive set that INCLUDES the baselined version → the baseline is NOT
// re-run (drift exemption / no double-apply), only the new migration applies,
// checksum-drift clean. H5 probe: squash() a prefix INCLUDING the baselined
// version → assert the correct supersession behavior.
// ===========================================================================

#[compio::test]
async fn l5_baseline_apply_on_top_then_squash_over_baseline() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let engine = MigrationEngine::new();
    let schema = cfg.project_schema.clone();

    // --- LEGACY DB: a table created OUTSIDE the engine (the adoption path). ---
    conn.batch_execute(&format!(
        "CREATE TABLE \"{schema}\".legacy_users (id bigint primary key, email text)"
    ))
    .await
    .expect("hand-create legacy table");
    give_table_to_migrator(&conn, &cfg, "legacy_users").await;

    // --- baseline(): a migration whose `up` DOCUMENTS the legacy schema. It is
    //     journaled kind='baseline' WITHOUT running its up (the table already
    //     exists; re-running CREATE TABLE would error). ---
    let baseline_mig = mig(
        MigrationId::generate(),
        "baseline_v0",
        "CREATE TABLE \"{s}\".legacy_users (id bigint primary key, email text)",
        &schema,
    );
    let out = baseline(&conn, &cfg, &baseline_mig, "operator")
        .await
        .expect("baseline the legacy DB");
    assert!(!out.already_present, "first baseline records the event");
    assert_eq!(out.version, baseline_mig.version.as_str());
    assert_eq!(
        journaled_kind(&conn, &cfg, baseline_mig.version.as_str()).await.as_deref(),
        Some("baseline"),
        "the baseline event is stamped kind='baseline'"
    );

    // --- apply a NEW additive set that INCLUDES the baseline migration in the
    //     supplied set (the control plane re-supplies the full known history). The
    //     baseline must NOT re-run; only the genuinely-new migration applies. ---
    let new_mig = mig(
        MigrationId::generate(),
        "add_orders",
        "CREATE TABLE \"{s}\".orders (id bigint primary key)",
        &schema,
    );
    // version order: baseline minted first, so it sorts before new_mig.
    assert!(
        baseline_mig.version.as_str() < new_mig.version.as_str(),
        "baseline must sort before the new migration"
    );
    let set = vec![baseline_mig.clone(), new_mig.clone()];

    let plan = engine.plan(&set, &guard_cfg(&cfg));
    let apply_out = engine
        .apply(&plan, Approval::None, &conn, &cfg, "app_acme")
        .await
        .expect("apply the new set on top of the baseline");

    // ONLY the new migration applied; the baselined version was already net-applied
    // (kind='baseline') so the executor's pending computation excluded it.
    assert_eq!(
        apply_out.applied,
        vec![new_mig.version.as_str().to_string()],
        "only the genuinely-new migration applies; the baseline is NOT re-run"
    );
    assert!(table_exists(&conn, &schema, "orders").await, "the new table applied");
    // The baseline still has exactly ONE completed event (it was not re-run).
    assert_eq!(
        completed_events(&conn, &cfg, baseline_mig.version.as_str()).await,
        1,
        "the baseline must not accrue a second completed event"
    );

    // --- DRIFT: the checksum-drift check is clean across the supplied set (the
    //     baseline's journaled checksum matches what we re-supply). ---
    let drift = check_checksum_drift(&conn, &cfg, &set)
        .await
        .expect("checksum-drift check");
    assert!(
        drift.is_clean(),
        "no checksum drift after baseline + apply-on-top: {drift:?}"
    );

    // === H5 probe: squash a prefix that INCLUDES the baselined version. ===
    // The squash S supersedes [baseline_v0, add_orders] — both are net-applied
    // (one as kind='baseline', one as kind='apply'). squash()'s all-or-none rule
    // counts NET-APPLIED state regardless of kind: ALL superseded versions are
    // net-applied ⇒ this is the existing-DB path ⇒ record the supersession WITHOUT
    // running S.up (NOT a partial-overlap error, NOT a re-run).
    let mut s = mig(
        MigrationId::generate(),
        "squash_baseline_and_orders",
        "CREATE TABLE \"{s}\".legacy_users (id bigint primary key, email text); \
         CREATE TABLE \"{s}\".orders (id bigint primary key)",
        &schema,
    );
    s.supersedes = vec![baseline_mig.version.clone(), new_mig.version.clone()];
    s.recompute_checksum();

    let sq = squash(&conn, &cfg, &s, "operator")
        .await
        .expect("squash over a prefix that includes the baselined version must record cleanly");
    assert!(
        !sq.already_present,
        "the first squash records the supersession (clean supersession, not partial-overlap)"
    );
    assert_eq!(
        sq.superseded.len(),
        2,
        "the squash supersedes both the baseline and the additive migration"
    );
    assert_eq!(
        journaled_kind(&conn, &cfg, s.version.as_str()).await.as_deref(),
        Some("squash"),
        "the squash event is stamped kind='squash'"
    );

    // The squash did NOT re-run its up (both tables already existed; no dup error).
    assert!(table_exists(&conn, &schema, "legacy_users").await);
    assert!(table_exists(&conn, &schema, "orders").await);

    // status: the squash is net-applied; the superseded versions (incl. the
    // baseline) are NOT pending.
    let mut full = set.clone();
    full.push(s.clone());
    let st = status(&conn, &cfg, &full).await.expect("status post-squash");
    let applied: std::collections::HashSet<&str> =
        st.applied.iter().map(|e| e.version.as_str()).collect();
    assert!(applied.contains(s.version.as_str()), "the squash is net-applied");
    let pending: std::collections::HashSet<&str> =
        st.pending.iter().map(MigrationId::as_str).collect();
    assert!(
        !pending.contains(baseline_mig.version.as_str()),
        "the baselined version must NOT be pending after the squash supersedes it; pending={pending:?}"
    );
    assert!(
        !pending.contains(new_mig.version.as_str()),
        "the additive version must NOT be pending after the squash; pending={pending:?}"
    );

    teardown(&conn, &cfg).await;
}

/// H5 sub-probe (negative control): a squash whose superseded set has a PARTIAL
/// overlap with the journal — only the baselined version is applied, the other
/// superseded version is NOT — is correctly refused as a partial overlap, NOT
/// silently recorded. This confirms the all-or-none rule still holds when one of
/// the members is a baseline (the baseline counts as net-applied, the absent one
/// does not, so it is a genuine partial state).
#[compio::test]
async fn l5_h5_squash_partial_overlap_over_baseline_is_refused() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let schema = cfg.project_schema.clone();

    // Legacy table + baseline it.
    conn.batch_execute(&format!(
        "CREATE TABLE \"{schema}\".legacy (id bigint primary key)"
    ))
    .await
    .expect("legacy");
    give_table_to_migrator(&conn, &cfg, "legacy").await;
    let baseline_mig = mig(
        MigrationId::generate(),
        "baseline_v0",
        "CREATE TABLE \"{s}\".legacy (id bigint primary key)",
        &schema,
    );
    baseline(&conn, &cfg, &baseline_mig, "operator")
        .await
        .expect("baseline");

    // A squash that supersedes [baseline_v0, never_applied] — the second version
    // was NEVER applied. The baseline IS net-applied ⇒ partial overlap ⇒ refused.
    let never = MigrationId::generate();
    let mut s = mig(
        MigrationId::generate(),
        "bad_squash",
        "CREATE TABLE \"{s}\".legacy (id bigint primary key)",
        &schema,
    );
    s.supersedes = vec![baseline_mig.version.clone(), never];
    s.recompute_checksum();

    let err = squash(&conn, &cfg, &s, "operator")
        .await
        .expect_err("a partial-overlap squash (baseline applied, other not) must be refused");
    assert!(
        matches!(err, SquashError::PartialOverlap { applied: 1, total: 2, .. }),
        "expected PartialOverlap{{applied:1,total:2}}, got {err:?}"
    );
    // Nothing recorded for the squash.
    assert!(
        !applied_versions(&conn, &cfg).await.contains(&s.version.as_str().to_string()),
        "the refused squash must not be journaled"
    );

    teardown(&conn, &cfg).await;
}
