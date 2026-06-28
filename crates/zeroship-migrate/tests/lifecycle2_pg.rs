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

// The migration `up` SQL in this file uses a `{s}` placeholder substituted with
// the project schema by the `mig`/`with_up` helpers (`.replace("{s}", schema)`),
// so the `{s}` in those string literals is a deliberate template token, not a
// stray format argument.
#![allow(clippy::literal_string_with_formatting_args)]

use std::collections::HashMap;

use compio_postgres::Client;
use zeroship_migrate::apply::executor::ApplyError;
use zeroship_migrate::apply::baseline::BaselineOutcome;
use zeroship_migrate::{
    MigrationBackend, PostgresBackend,
    analyze_migration, check_checksum_drift, compute_manifest, desired_snapshot,
    diff_snapshots, history, migrator_role_name, provision_migrator, apply::role::deprovision_migrator,
    snapshot_schema, squash, status, Approval, Checksum, ChecksumInput, CollectionDescriptor,
    DeclarativeError, EngineError, ExecutorConfig, FieldDescriptor, GuardConfig, HistoryKind,
    ManifestHash, Migration, MigrationEngine, MigrationFlags, MigrationId, OnUnmet, Precondition,
    PreconditionCheck, SchemaSnapshot, Severity, ShadowConfig, SquashError,
};

/// Baseline through the neutral [`MigrationBackend::baseline_one`] seam (multi-engine
/// abstraction L5) — the PG free `baseline()` is now the `pub(crate)` body behind
/// `PostgresBackend::baseline_one`, keeping the `&Client`/advisory-lock confined.
async fn baseline(
    conn: &Client,
    cfg: &ExecutorConfig,
    m: &Migration,
    applied_by: &str,
) -> Result<BaselineOutcome, zeroship_migrate::apply::baseline::BaselineError> {
    PostgresBackend::new(conn)
        .baseline_one(cfg, m, applied_by)
        .await
}

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
/// tables are owned by the migrator so its DDL works under SET ROLE).
async fn give_table_to_migrator(conn: &Client, cfg: &ExecutorConfig, table: &str) {
    let role = cfg.pg.migrator_role.as_ref().unwrap();
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
        existence_guard: None,
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
        .filter(|e| matches!(e.phase, zeroship_migrate::apply::journal::Phase::Completed))
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
                cfg.pg.meta_schema
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
            cfg.pg.meta_schema
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
        .apply(&plan, Approval::None, &PostgresBackend::new(&conn), &cfg, "app_acme")
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

    let sq = squash(&PostgresBackend::new(&conn), &cfg, &s, "operator")
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

    let err = squash(&PostgresBackend::new(&conn), &cfg, &s, "operator")
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

// ===========================================================================
// L-6 — repeatable + versioned interleaved across deploys + tamper anchor
//   (#46/#53).
//
// Deploy 1: a versioned table + a repeatable CREATE OR REPLACE VIEW (depends_on
// the table) → the view applies AFTER the versioned. Deploy 2: the unchanged view
// is SKIPPED, a versioned ADD COLUMN applies. Deploy 3: the CHANGED view is
// RE-APPLIED (new completed event, kind='repeatable'). Then the flip-flag tamper:
// supply the once-only ADD COLUMN as repeatable=true → ChecksumDrift abort.
// status/history reflect the repeatable re-events. Composes the partition + the
// re-run oracle + the kind-anchor.
// ===========================================================================

/// A REPEATABLE migration (stable identity, re-applies on checksum change).
fn repeatable(version: MigrationId, name: &str, up: &str, schema: &str) -> Migration {
    let mut m = mig(version, name, up, schema);
    m.flags.repeatable = true;
    m.recompute_checksum();
    m
}

/// Re-checksum a migration after editing its `up`.
fn with_up(mut m: Migration, up: &str, schema: &str) -> Migration {
    m.up = up.replace("{s}", schema);
    m.recompute_checksum();
    m
}

#[compio::test]
async fn l6_repeatable_versioned_interleaved_across_deploys_and_tamper_anchor() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let engine = MigrationEngine::new();
    let schema = cfg.project_schema.clone();

    // --- DEPLOY 1: a versioned table `widgets(n int)` + a repeatable view over it
    //     (depends_on the table). The view's CREATE references the table, so a
    //     wrong order (view before table) would fail — proving the partition puts
    //     repeatables AFTER versioned. ---
    let table_v = MigrationId::generate();
    let table = mig(
        table_v.clone(),
        "create_widgets",
        "CREATE TABLE \"{s}\".widgets (n int); INSERT INTO \"{s}\".widgets VALUES (1)",
        &schema,
    );
    let view_v = MigrationId::generate();
    let mut view1 = repeatable(
        view_v.clone(),
        "v_widgets",
        "CREATE OR REPLACE VIEW \"{s}\".v AS SELECT n FROM \"{s}\".widgets",
        &schema,
    );
    view1.depends_on = vec![table_v.clone()];
    view1.recompute_checksum();

    // Supply the repeatable FIRST in the slice to prove ordering is by phase +
    // depends_on, not input order.
    let plan1 = engine.plan(&[view1.clone(), table.clone()], &guard_cfg(&cfg));
    let out1 = engine
        .apply(&plan1, Approval::None, &PostgresBackend::new(&conn), &cfg, "app_acme")
        .await
        .expect("deploy 1");
    assert_eq!(
        out1.applied,
        vec![table_v.as_str().to_string(), view_v.as_str().to_string()],
        "the versioned table applies BEFORE the repeatable view"
    );
    // The view reads the seeded row.
    let n: i32 = conn
        .query_one(&format!("SELECT n FROM \"{schema}\".v"), &[])
        .await
        .expect("read view")
        .get(0);
    assert_eq!(n, 1, "the view returns the seeded row");
    assert_eq!(
        journaled_kind(&conn, &cfg, view_v.as_str()).await.as_deref(),
        Some("repeatable"),
        "the view is journaled kind='repeatable'"
    );
    assert_eq!(completed_events(&conn, &cfg, view_v.as_str()).await, 1);

    // --- DEPLOY 2: the UNCHANGED view is SKIPPED; a NEW versioned ADD COLUMN
    //     applies. ---
    let addcol_v = MigrationId::generate();
    let addcol = mig(
        addcol_v.clone(),
        "add_widgets_label",
        "ALTER TABLE \"{s}\".widgets ADD COLUMN label text",
        &schema,
    );
    // Supply the full known set: table (applied) + view (unchanged) + addcol (new).
    let set2 = vec![table.clone(), view1.clone(), addcol.clone()];
    let plan2 = engine.plan(&set2, &guard_cfg(&cfg));
    let out2 = engine
        .apply(&plan2, Approval::None, &PostgresBackend::new(&conn), &cfg, "app_acme")
        .await
        .expect("deploy 2");
    assert_eq!(
        out2.applied,
        vec![addcol_v.as_str().to_string()],
        "only the new versioned ADD COLUMN applies in deploy 2"
    );
    assert!(
        out2.skipped.contains(&view_v.as_str().to_string()),
        "the unchanged repeatable view is SKIPPED in deploy 2; skipped={:?}",
        out2.skipped
    );
    assert!(column_exists(&conn, &schema, "widgets", "label").await, "ADD COLUMN landed");
    // The view still has exactly one completed event (a skip appends nothing).
    assert_eq!(
        completed_events(&conn, &cfg, view_v.as_str()).await,
        1,
        "a skipped repeatable does not append a completed event"
    );

    // --- DEPLOY 3: the CHANGED view (new definition) RE-APPLIES — a new completed
    //     event stamped kind='repeatable'. ---
    let view2 = with_up(
        view1.clone(),
        "CREATE OR REPLACE VIEW \"{s}\".v AS SELECT n + 100 AS n FROM \"{s}\".widgets",
        &schema,
    );
    assert_ne!(view1.checksum, view2.checksum, "the edit changes the checksum");
    let set3 = vec![table.clone(), view2.clone(), addcol.clone()];
    let plan3 = engine.plan(&set3, &guard_cfg(&cfg));
    let out3 = engine
        .apply(&plan3, Approval::None, &PostgresBackend::new(&conn), &cfg, "app_acme")
        .await
        .expect("deploy 3");
    assert_eq!(
        out3.applied,
        vec![view_v.as_str().to_string()],
        "only the changed repeatable re-applies in deploy 3"
    );
    let n: i32 = conn
        .query_one(&format!("SELECT n FROM \"{schema}\".v"), &[])
        .await
        .expect("read view")
        .get(0);
    assert_eq!(n, 101, "the view now returns the NEW definition's value");
    // The re-run anchor: a SECOND completed event for the repeatable, still
    // kind='repeatable'.
    assert_eq!(
        completed_events(&conn, &cfg, view_v.as_str()).await,
        2,
        "the re-applied repeatable accrues a second completed event (append-only)"
    );
    assert_eq!(
        journaled_kind(&conn, &cfg, view_v.as_str()).await.as_deref(),
        Some("repeatable"),
        "the re-event is still kind='repeatable' (the kind anchor holds across re-runs)"
    );

    // --- status / history reflect the re-events. ---
    let st = status(&conn, &cfg, &set3).await.expect("status");
    let applied: std::collections::HashSet<&str> =
        st.applied.iter().map(|e| e.version.as_str()).collect();
    for v in [table_v.as_str(), view_v.as_str(), addcol_v.as_str()] {
        assert!(applied.contains(v), "{v} must be net-applied; applied={applied:?}");
    }
    assert!(st.pending.is_empty(), "nothing pending after deploy 3: {:?}", st.pending);

    // history shows TWO completed events for the repeatable view (deploy 1 + 3),
    // in monotonic event order, plus the versioned applies.
    let hist = history(&conn, &cfg).await.expect("history");
    let view_completed = hist
        .iter()
        .filter(|e| e.version == view_v.as_str() && e.kind == HistoryKind::Completed)
        .count();
    assert_eq!(view_completed, 2, "history records the view's apply + re-apply");
    let seqs: Vec<i64> = hist.iter().map(|e| e.event_seq).collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted, "history is in monotonic event order");

    // --- TAMPER ANCHOR: supply the once-only ADD COLUMN as repeatable=true with a
    //     MUTATED up. The journal stamped addcol kind='apply' (once-only). Flipping
    //     it to repeatable is a KIND MISMATCH → ChecksumDrift abort, NOT a silent
    //     re-run. The drift exemption anchors on the JOURNALED kind, never the
    //     supplied flag. ---
    let mut tampered = with_up(
        addcol.clone(),
        "ALTER TABLE \"{s}\".widgets DROP COLUMN label",
        &schema,
    );
    tampered.flags.repeatable = true;
    tampered.recompute_checksum();
    assert_ne!(addcol.checksum, tampered.checksum, "the attack mutates the up");

    // Apply through the executor directly (the drift guard lives there) with the
    // full known set so the tampered version is the only delta.
    let set4 = vec![table.clone(), view2.clone(), tampered.clone()];
    let err = zeroship_migrate::apply(&conn, &cfg, &set4, Approval::None, "attacker")
        .await
        .expect_err("flipping an applied once-only to repeatable must ABORT as tamper");
    assert!(
        matches!(err, ApplyError::ChecksumDrift { ref version, .. } if version == addcol_v.as_str()),
        "expected ChecksumDrift on the flipped version, got {err:?}"
    );
    // The mutated DROP COLUMN never ran (the column survives) and no second
    // completed event was appended.
    assert!(
        column_exists(&conn, &schema, "widgets", "label").await,
        "the tamper's DROP COLUMN must NOT have run"
    );
    assert_eq!(
        completed_events(&conn, &cfg, addcol_v.as_str()).await,
        1,
        "the tamper attempt appends no completed event"
    );

    teardown(&conn, &cfg).await;
}

// ===========================================================================
// L-7 — preconditions across deploys (Skip→pending→apply) + Halt
//   (probes H6 expand+precondition).
//
// A migration with OnUnmet::Skip on an unmet precondition on deploy 1 → skipped,
// stays pending, dependents transitively skipped → seed the condition → deploy 2
// applies it. A Halt precondition unmet → fail-closed PreconditionFailed, nothing
// applied. H6 probe: an EXPAND migration (the online rename's E-phase) carrying an
// unmet Skip precondition → drive run_expand and observe what the contract gate
// then sees.
// ===========================================================================

/// A migration with the given preconditions + a correct checksum.
fn mig_pre(
    version: MigrationId,
    name: &str,
    up: &str,
    schema: &str,
    pre: Vec<PreconditionCheck>,
) -> Migration {
    let mut m = mig(version, name, up, schema);
    m.preconditions = pre;
    m.recompute_checksum();
    m
}

#[compio::test]
async fn l7_preconditions_across_deploys_skip_pending_apply_and_halt() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let engine = MigrationEngine::new();
    let schema = cfg.project_schema.clone();

    // --- SKIP across deploys + transitive dependent skip. ---
    // parent is gated on `dep` (Skip), which does not exist yet. child depends_on
    // parent. Deploy 1: parent skipped, child (dep unmet) does NOT run. Deploy 2:
    // seed `dep` → both apply in order.
    let parent_v = MigrationId::generate();
    let parent = mig_pre(
        parent_v.clone(),
        "parent",
        "CREATE TABLE \"{s}\".parent (id int primary key)",
        &schema,
        vec![PreconditionCheck::skip(Precondition::TableExists { table: "dep".into() })],
    );
    let child_v = MigrationId::generate();
    let mut child = mig(
        child_v.clone(),
        "child",
        "CREATE TABLE \"{s}\".child (id int, parent_id int references \"{s}\".parent(id))",
        &schema,
    );
    child.depends_on = vec![parent_v.clone()];
    child.recompute_checksum();

    let set = vec![parent.clone(), child.clone()];

    // Deploy 1: parent skipped; child (dependent of a not-yet-applied parent) does
    // not run. The apply SUCCEEDS (a skip is not an error), nothing journaled.
    let plan = engine.plan(&set, &guard_cfg(&cfg));
    let out1 = engine
        .apply(&plan, Approval::None, &PostgresBackend::new(&conn), &cfg, "app_acme")
        .await
        .expect("deploy 1 succeeds (skip is not an error)");
    assert!(out1.applied.is_empty(), "neither parent (skipped) nor child runs");
    assert!(out1.skipped.contains(&parent_v.as_str().to_string()), "parent is skipped");
    assert!(!table_exists(&conn, &schema, "parent").await);
    assert!(!table_exists(&conn, &schema, "child").await);
    // Both still pending.
    let st = status(&conn, &cfg, &set).await.expect("status after deploy 1");
    let pending: std::collections::HashSet<&str> =
        st.pending.iter().map(MigrationId::as_str).collect();
    assert!(
        pending.contains(parent_v.as_str()) && pending.contains(child_v.as_str()),
        "both parent and child stay pending after the skip; pending={pending:?}"
    );

    // Deploy 2: seed `dep` → the precondition is met → parent then child apply.
    conn.batch_execute(&format!("CREATE TABLE \"{schema}\".dep (id int)"))
        .await
        .expect("seed dep");
    let plan = engine.plan(&set, &guard_cfg(&cfg));
    let out2 = engine
        .apply(&plan, Approval::None, &PostgresBackend::new(&conn), &cfg, "app_acme")
        .await
        .expect("deploy 2 applies the now-unblocked set");
    assert_eq!(
        out2.applied,
        vec![parent_v.as_str().to_string(), child_v.as_str().to_string()],
        "parent then child apply once the precondition is met"
    );
    assert!(table_exists(&conn, &schema, "parent").await);
    assert!(table_exists(&conn, &schema, "child").await);

    // --- HALT (default) unmet → fail-closed PreconditionFailed, nothing applied. ---
    let halt_v = MigrationId::generate();
    let halt_mig = mig_pre(
        halt_v.clone(),
        "halt_gated",
        "CREATE TABLE \"{s}\".halt_t (id int)",
        &schema,
        vec![PreconditionCheck::halt(Precondition::TableExists {
            table: "absent_dep".into(),
        })],
    );
    let plan = engine.plan(std::slice::from_ref(&halt_mig), &guard_cfg(&cfg));
    let err = engine
        .apply(&plan, Approval::None, &PostgresBackend::new(&conn), &cfg, "app_acme")
        .await
        .expect_err("an unmet Halt precondition must fail closed");
    assert!(
        matches!(err, EngineError::Apply(ApplyError::PreconditionFailed { ref version, .. }) if version == halt_v.as_str()),
        "expected PreconditionFailed on the halt migration, got {err:?}"
    );
    assert!(!table_exists(&conn, &schema, "halt_t").await, "the halted migration applies nothing");
    assert!(
        !applied_versions(&conn, &cfg).await.contains(&halt_v.as_str().to_string()),
        "the halted migration journals nothing"
    );

    teardown(&conn, &cfg).await;
}

/// H6 probe — an EXPAND migration carrying an unmet Skip precondition.
///
/// The online rename's E2 (dual-write trigger) is the version the contract gate
/// keys on (`contract.depends_on = [trigger_version]`). We attach an UNMET Skip
/// precondition to E2 and drive `run_expand`. The faithful question: does the
/// expand get partially applied (E1 + the backfill marker E3 land while E2 is
/// skipped), and does a later contract deploy refuse with `ExpandNotApplied`?
///
/// VERDICT (H6): the system fails CLOSED — SOUND. With the Skip precondition on
/// E2: E1 (additive, no precondition) applies; E2 is precondition-skipped (not
/// net-applied); E3 (the backfill marker, `depends_on` E2) then fails its
/// dependency check, so `run_expand` returns `Err(Apply(MissingDependency{E3→E2}))`
/// and the expand never "completes". A later contract deploy keyed on E2 is then
/// refused with `ExpandNotApplied`. No data loss: the old column survives, the
/// contract's DROP COLUMN never runs.
///
/// MISLEADING-MESSAGE NOTE (quality, not a correctness bug): neither surfaced
/// error mentions the precondition that is the REAL root cause. `run_expand`
/// reports `MissingDependency` (E3 needs E2) and the contract gate reports
/// `ExpandNotApplied` (E2 not net-applied) — both technically true, but an
/// operator reading them would not learn that E2 was SKIPPED by an unmet
/// precondition. A clearer surfacing (e.g. "expand E2 was skipped by an unmet
/// precondition") would aid diagnosis. The behavior is correct; the diagnostics
/// could be friendlier.
#[compio::test]
async fn l7_h6_expand_with_unmet_skip_precondition_blocks_later_contract() {
    use zeroship_migrate::{ExpandContractAuthor, OnlineIntent};

    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let engine = MigrationEngine::new();
    let schema = cfg.project_schema.clone();

    // Seed a real `users(email text)` table the rename operates on, migrator-owned.
    let seed = mig(
        MigrationId::generate(),
        "create_users",
        "CREATE TABLE \"{s}\".users (id bigint primary key, email text)",
        &schema,
    );
    let plan_seed = engine.plan(std::slice::from_ref(&seed), &guard_cfg(&cfg));
    engine
        .apply(&plan_seed, Approval::None, &PostgresBackend::new(&conn), &cfg, "app_acme")
        .await
        .expect("seed users");
    give_table_to_migrator(&conn, &cfg, "users").await;

    // Author the online rename email → email_address.
    let mut plan = ExpandContractAuthor::new(&schema, "app_acme")
        .author(&OnlineIntent::RenameColumn {
            table: "users".into(),
            from: "email".into(),
            to: "email_address".into(),
            ty: "text".into(),
        })
        .expect("author rename");

    // Attach an UNMET Skip precondition to E2 (the dual-write trigger — the version
    // the contract gate keys on): require a table `go_signal` that does NOT exist.
    // Re-checksum E2 so the migration is internally consistent (the precondition
    // folds into the checksum).
    let trigger_version = plan.trigger_version.clone();
    {
        let e2 = &mut plan.expand[1];
        assert_eq!(e2.version, trigger_version, "expand[1] is the trigger (E2)");
        e2.preconditions = vec![PreconditionCheck {
            check: Precondition::TableExists { table: "go_signal".into() },
            on_unmet: OnUnmet::Skip,
        }];
        e2.recompute_checksum();
    }

    let e1_version = plan.expand[0].version.as_str().to_string();
    let e3_version = plan.expand[2].version.as_str().to_string();

    // Drive the EXPAND. run_expand applies E1+E2 (E2 is precondition-skipped),
    // runs the backfill, then journals E3. We capture whether it errors or returns.
    let expand_res = engine
        .run_expand(&plan, Approval::Approved, &conn, &cfg, "app_acme")
        .await;

    // Whatever run_expand returns, the SOUND invariant is: E2 (the trigger) is NOT
    // net-applied (it was precondition-skipped), so a later contract deploy keyed
    // on E2 MUST be refused. Drive the contract as a SEPARATE deploy and assert the
    // gate refuses it with ExpandNotApplied naming E2.
    let applied_now = applied_versions(&conn, &cfg).await;
    let e2_applied = applied_now.contains(&trigger_version.as_str().to_string());
    assert!(
        !e2_applied,
        "E2 (the trigger) carried an UNMET Skip precondition, so it must NOT be \
         net-applied; run_expand returned {expand_res:?}"
    );
    // Document the actual half-state run_expand leaves (the H6 verdict rests on
    // this): E1 (ADD COLUMN, no precondition) applies; E2 is skipped; E3 (the
    // backfill marker) — note whether the orchestrator journaled it despite E2
    // being skipped. The gate keys on E2, so even a journaled E3 does not let the
    // contract through; the column-level coexistence is preserved (email survives).
    let e1_applied = applied_now.contains(&e1_version);
    let e3_applied = applied_now.contains(&e3_version);
    eprintln!(
        "H6 half-state after run_expand({expand_res:?}): E1_applied={e1_applied} \
         E2_applied={e2_applied} E3_applied={e3_applied}"
    );
    // E1 has no precondition, so it applies; the new column exists (additive, safe).
    assert!(e1_applied, "E1 (ADD COLUMN, no precondition) applies");
    assert!(!e3_applied, "E3 (backfill marker) does NOT complete — its dep E2 was skipped");
    assert!(
        column_exists(&conn, &schema, "users", "email_address").await,
        "E1's additive column exists (harmless: nullable, no contract yet)"
    );
    // run_expand fails closed: the skipped E2 leaves E3's dependency unsatisfied,
    // so the orchestrator errors (MissingDependency E3→E2) rather than journaling a
    // bogus "expand complete". The error names the dependency, NOT the precondition
    // that is the real cause — the misleading-message note above.
    assert!(
        matches!(
            expand_res,
            Err(zeroship_migrate::OnlineError::Apply(ApplyError::MissingDependency { ref version, ref missing }))
                if version == &e3_version && missing == trigger_version.as_str()
        ),
        "run_expand must fail closed with E3's dependency on the skipped E2 unmet, got {expand_res:?}"
    );

    let contract_err = zeroship_migrate::apply(&conn, &cfg, &plan.contract, Approval::Approved, "app_acme")
        .await
        .expect_err("the contract must be refused while its expand (E2) is not net-applied");
    assert!(
        matches!(
            contract_err,
            ApplyError::ExpandNotApplied { ref expand, .. } if expand == trigger_version.as_str()
        ),
        "expected ExpandNotApplied naming E2 (the precondition-skipped trigger), got {contract_err:?}"
    );

    // The old column survives (the contract's DROP COLUMN never ran).
    assert!(
        column_exists(&conn, &schema, "users", "email").await,
        "the contract must not have dropped the old column"
    );

    teardown(&conn, &cfg).await;
}

// ===========================================================================
// L-8 — destructive: analyzer advisory → dry-run → gate → apply (#7/#8/#47).
//
// A DROP COLUMN set → analyze_migration emits a DESTRUCTIVE_DROP advisory
// (non-gating) → dry_run on the shadow surfaces it (the dry-run batch is
// self-seeding: it CREATEs the table then DROPs the column, so the shadow has the
// table to drop from) → apply with Approval::None → ApprovalRequired (nothing
// applied) → apply with Approved → succeeds, drift clean. Composes the advisory
// (non-gating) + dry-run + the two-layer approval gate (engine gate + executor's
// own gate).
// ===========================================================================

#[compio::test]
async fn l8_destructive_analyzer_advisory_dry_run_gate_apply() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let engine = MigrationEngine::new();
    let schema = cfg.project_schema.clone();

    // --- DEPLOY 1: create a real table with a `legacy_col` to later drop. ---
    let create = mig(
        MigrationId::generate(),
        "create_accounts",
        "CREATE TABLE \"{s}\".accounts (id bigint primary key, keep_col text, legacy_col text)",
        &schema,
    );
    let p = engine.plan(std::slice::from_ref(&create), &guard_cfg(&cfg));
    engine
        .apply(&p, Approval::None, &PostgresBackend::new(&conn), &cfg, "app_acme")
        .await
        .expect("create accounts");
    give_table_to_migrator(&conn, &cfg, "accounts").await;
    assert!(column_exists(&conn, &schema, "accounts", "legacy_col").await);

    // --- The DROP COLUMN migration (destructive). Author it as a destructive,
    //     approval-requiring migration (the declarative differ marks its own
    //     drops; here we mint it directly with the same flags). ---
    let mut drop_col = mig(
        MigrationId::generate(),
        "drop_legacy_col",
        "ALTER TABLE \"{s}\".accounts DROP COLUMN legacy_col",
        &schema,
    );
    drop_col.flags.destructive = true;
    drop_col.flags.requires_approval = true;
    drop_col.recompute_checksum();

    // --- LAYER 0: the analyzer emits a non-gating DESTRUCTIVE_DROP advisory. ---
    let advisories = analyze_migration(&drop_col);
    assert!(
        advisories
            .iter()
            .any(|a| a.rule == "DESTRUCTIVE_DROP" && a.severity == Severity::Warning),
        "analyze_migration must emit a DESTRUCTIVE_DROP advisory, got {advisories:?}"
    );
    // The advisory is ADVISORY ONLY: the plan still plans it (not denied), and the
    // DB is untouched by analysis.
    assert!(column_exists(&conn, &schema, "accounts", "legacy_col").await, "analysis is read-only");

    // --- LAYER 1: dry_run on a throwaway shadow surfaces the advisory + previews
    //     the destructive op without touching the real DB. The batch is
    //     self-seeding (CREATE table + DROP COLUMN in one batch). ---
    let create_for_shadow = mig(
        MigrationId::generate(),
        "shadow_create_accounts",
        "CREATE TABLE \"{s}\".accounts (id bigint primary key, keep_col text, legacy_col text)",
        &schema,
    );
    let mut drop_for_shadow = mig(
        MigrationId::generate(),
        "shadow_drop_legacy_col",
        "ALTER TABLE \"{s}\".accounts DROP COLUMN legacy_col",
        &schema,
    );
    drop_for_shadow.depends_on = vec![create_for_shadow.version.clone()];
    drop_for_shadow.flags.destructive = true;
    drop_for_shadow.flags.requires_approval = true;
    drop_for_shadow.recompute_checksum();
    let shadow_batch = vec![create_for_shadow.clone(), drop_for_shadow.clone()];

    let report = engine
        .dry_run(&PostgresBackend::new(&conn), &shadow_batch, &cfg, &shadow_cfg(&tok), "app_acme")
        .await
        .expect("dry_run harness");
    // The dry-run applies cleanly on the shadow (the destructive op is valid SQL).
    assert!(report.ok, "dry_run must apply the self-seeding batch cleanly on the shadow: {report:?}");
    // The destructive advisory is surfaced for the DROP COLUMN migration.
    let drop_advisories: Vec<_> = report
        .advisories
        .iter()
        .filter(|(v, _)| v == drop_for_shadow.version.as_str())
        .flat_map(|(_, a)| a.iter())
        .collect();
    assert!(
        drop_advisories.iter().any(|a| a.rule == "DESTRUCTIVE_DROP"),
        "dry_run must surface the DESTRUCTIVE_DROP advisory for the DROP COLUMN; advisories={:?}",
        report.advisories
    );
    // The dry-run did NOT touch the real project DB (the real DROP has not run).
    assert!(
        column_exists(&conn, &schema, "accounts", "legacy_col").await,
        "dry_run must not touch the real project schema"
    );

    // --- LAYER 2a: apply the real DROP with Approval::None → ApprovalRequired,
    //     nothing applied (the column survives). The engine's gate refuses. ---
    let plan_drop = engine.plan(std::slice::from_ref(&drop_col), &guard_cfg(&cfg));
    assert!(plan_drop.destructive, "the plan is destructive");
    assert!(plan_drop.requires_approval, "the plan requires approval");
    let err = engine
        .apply(&plan_drop, Approval::None, &PostgresBackend::new(&conn), &cfg, "app_acme")
        .await
        .expect_err("a destructive plan without approval must be refused");
    assert!(
        matches!(err, EngineError::ApprovalRequired),
        "expected ApprovalRequired, got {err:?}"
    );
    assert!(
        column_exists(&conn, &schema, "accounts", "legacy_col").await,
        "the refused destructive apply must drop NOTHING"
    );

    // --- LAYER 2b: apply with Approval::Approved → succeeds, the column is gone. ---
    let out = engine
        .apply(&plan_drop, Approval::Approved, &PostgresBackend::new(&conn), &cfg, "app_acme")
        .await
        .expect("an approved destructive apply must run");
    assert_eq!(out.applied, vec![drop_col.version.as_str().to_string()]);
    assert!(
        !column_exists(&conn, &schema, "accounts", "legacy_col").await,
        "the approved drop removed the column"
    );
    assert!(column_exists(&conn, &schema, "accounts", "keep_col").await, "the kept column survives");

    // --- DRIFT clean: the desired (post-drop) schema matches live. We build the
    //     desired snapshot from a descriptor that mirrors the post-drop shape and
    //     assert no column drift on `accounts`. ---
    let drift = check_checksum_drift(&conn, &cfg, &[create.clone(), drop_col.clone()])
        .await
        .expect("checksum-drift check");
    assert!(drift.is_clean(), "no checksum drift after the destructive lifecycle: {drift:?}");

    teardown(&conn, &cfg).await;
}

// ===========================================================================
// L-9 — manifest tamper rejected before apply (#36 set-level).
//
// compute_manifest over a trusted set → apply_verified rejects EACH tamper:
// (a) a depends_on edit that reorders execution, (b) a content edit, (c) an
// inserted migration, (d) a removed migration, (e) a supersedes/repeatable/
// requires_approval flag flip → EACH → EngineError::Manifest, the lock is never
// taken, the journal is untouched. A pure cosmetic slice-reorder of an additive
// set PASSES (M2 invariant) and applies. Asserts the manifest covers the full
// apply-relevant shape (Plan-F C1) + rejects before any side effect.
// ===========================================================================

/// True if the meta (journal) schema exists at all — proof `apply_verified` did
/// NOT take the lock / bootstrap the journal on a manifest-rejected call.
async fn meta_schema_exists(conn: &Client, cfg: &ExecutorConfig) -> bool {
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.schemata WHERE schema_name = $1",
            &[&cfg.pg.meta_schema],
        )
        .await
        .expect("meta_schema_exists");
    !rows.is_empty()
}

#[compio::test]
async fn l9_manifest_tamper_rejected_before_any_side_effect() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let engine = MigrationEngine::new();
    let schema = cfg.project_schema.clone();

    // A trusted additive set of two independent tables (a sorts before b).
    let a = mig(
        MigrationId::generate(),
        "create_a",
        "CREATE TABLE \"{s}\".a (id bigint primary key)",
        &schema,
    );
    std::thread::sleep(std::time::Duration::from_millis(2));
    let b = mig(
        MigrationId::generate(),
        "create_b",
        "CREATE TABLE \"{s}\".b (id bigint primary key)",
        &schema,
    );
    assert!(a.version.as_str() < b.version.as_str());
    let trusted = vec![a.clone(), b.clone()];
    let expected: ManifestHash = compute_manifest(&trusted);

    // --- (a) depends_on edit that REORDERS execution: make a depend on b → the
    //     canonical executed order flips to [b, a] AND a's checksum changes (C1). ---
    let mut a_dep = a.clone();
    a_dep.depends_on = vec![b.version.clone()];
    a_dep.recompute_checksum();
    let tamper_a = vec![a_dep, b.clone()];
    let err = engine
        .apply_verified(&tamper_a, &guard_cfg(&cfg), Some(&expected), Approval::Approved, &PostgresBackend::new(&conn), &cfg, "app_acme")
        .await
        .expect_err("(a) a depends_on reorder must be rejected");
    assert!(matches!(err, EngineError::Manifest(_)), "(a) got {err:?}");
    assert!(!meta_schema_exists(&conn, &cfg).await, "(a) lock/journal never taken");
    assert!(!table_exists(&conn, &schema, "a").await && !table_exists(&conn, &schema, "b").await, "(a) nothing applied");

    // --- (b) content edit (a's up changed, same version). ---
    let b_edit = with_up(a.clone(), "CREATE TABLE \"{s}\".a (id bigint primary key, extra text)", &schema);
    let tamper_b = vec![b_edit, b.clone()];
    let err = engine
        .apply_verified(&tamper_b, &guard_cfg(&cfg), Some(&expected), Approval::Approved, &PostgresBackend::new(&conn), &cfg, "app_acme")
        .await
        .expect_err("(b) a content edit must be rejected");
    assert!(matches!(err, EngineError::Manifest(_)), "(b) got {err:?}");
    assert!(!meta_schema_exists(&conn, &cfg).await, "(b) lock/journal never taken");

    // --- (c) inserted migration. ---
    let inserted = mig(MigrationId::generate(), "evil_insert", "CREATE TABLE \"{s}\".c (id int)", &schema);
    let tamper_c = vec![a.clone(), b.clone(), inserted];
    let err = engine
        .apply_verified(&tamper_c, &guard_cfg(&cfg), Some(&expected), Approval::Approved, &PostgresBackend::new(&conn), &cfg, "app_acme")
        .await
        .expect_err("(c) an inserted migration must be rejected");
    assert!(matches!(err, EngineError::Manifest(_)), "(c) got {err:?}");
    assert!(!meta_schema_exists(&conn, &cfg).await, "(c) lock/journal never taken");

    // --- (d) removed migration. ---
    let tamper_d = vec![a.clone()];
    let err = engine
        .apply_verified(&tamper_d, &guard_cfg(&cfg), Some(&expected), Approval::Approved, &PostgresBackend::new(&conn), &cfg, "app_acme")
        .await
        .expect_err("(d) a removed migration must be rejected");
    assert!(matches!(err, EngineError::Manifest(_)), "(d) got {err:?}");
    assert!(!meta_schema_exists(&conn, &cfg).await, "(d) lock/journal never taken");

    // --- (e) a flag flip (requires_approval) — the per-migration checksum folds
    //     flags, so flipping requires_approval changes a's checksum ⇒ the manifest.
    let mut a_flip = a.clone();
    a_flip.flags.requires_approval = true;
    a_flip.recompute_checksum();
    let tamper_e = vec![a_flip, b.clone()];
    let err = engine
        .apply_verified(&tamper_e, &guard_cfg(&cfg), Some(&expected), Approval::Approved, &PostgresBackend::new(&conn), &cfg, "app_acme")
        .await
        .expect_err("(e) a flag flip must be rejected");
    assert!(matches!(err, EngineError::Manifest(_)), "(e) got {err:?}");
    assert!(!meta_schema_exists(&conn, &cfg).await, "(e) lock/journal never taken");

    // --- (e') a supersedes flip (also folded by the per-migration checksum). ---
    let mut a_sup = a.clone();
    a_sup.supersedes = vec![MigrationId::generate()];
    a_sup.recompute_checksum();
    let tamper_e2 = vec![a_sup, b.clone()];
    let err = engine
        .apply_verified(&tamper_e2, &guard_cfg(&cfg), Some(&expected), Approval::Approved, &PostgresBackend::new(&conn), &cfg, "app_acme")
        .await
        .expect_err("(e') a supersedes flip must be rejected");
    assert!(matches!(err, EngineError::Manifest(_)), "(e') got {err:?}");

    // --- (e'') a repeatable flip (folded by the per-migration checksum). ---
    let mut a_rep = a.clone();
    a_rep.flags.repeatable = true;
    a_rep.recompute_checksum();
    let tamper_e3 = vec![a_rep, b.clone()];
    let err = engine
        .apply_verified(&tamper_e3, &guard_cfg(&cfg), Some(&expected), Approval::Approved, &PostgresBackend::new(&conn), &cfg, "app_acme")
        .await
        .expect_err("(e'') a repeatable flip must be rejected");
    assert!(matches!(err, EngineError::Manifest(_)), "(e'') got {err:?}");
    assert!(!meta_schema_exists(&conn, &cfg).await, "no tamper ever bootstrapped the journal");

    // --- INVARIANT: a pure cosmetic slice-reorder of the trusted additive set
    //     (no depends_on, same content) VERIFIES OK and APPLIES. The manifest is
    //     over the canonical EXECUTED order, identical for both slice orders. ---
    let reordered = vec![b.clone(), a.clone()];
    let out = engine
        .apply_verified(&reordered, &guard_cfg(&cfg), Some(&expected), Approval::Approved, &PostgresBackend::new(&conn), &cfg, "app_acme")
        .await
        .expect("a cosmetic slice-reorder of an additive set must verify OK and apply (M2)");
    // Both tables applied (canonical executed order is [a, b] regardless of slice).
    assert_eq!(out.applied, vec![a.version.as_str().to_string(), b.version.as_str().to_string()]);
    assert!(table_exists(&conn, &schema, "a").await && table_exists(&conn, &schema, "b").await);

    teardown(&conn, &cfg).await;
}

// ===========================================================================
// L-10 — multi-app union lifecycle (#19/#20/#21/#23/#24).
//
// App A declares `products`, deploys → App B declares `orders`(FK→products) +
// uses `products`, deploys (the union grows; B's deploy emits only `orders`,
// products is a no-op) → B re-declares `products` with a DIFFERENT shape →
// ConflictingDeclaration → B deploys a union OMITTING A's `products` while it is
// live + A-owned → NotTableOwner / DropOfUnownedTable fail-closed (A's table
// persists). Asserts union growth + per-table ownership + fail-closed drop across
// real deploys.
// ===========================================================================

fn field(name: &str, ty: &str) -> FieldDescriptor {
    FieldDescriptor {
        name: name.into(),
        ty: ty.into(),
        required: false,
        unique: false,
        references: None,
        ..Default::default()
    }
}

fn fk_field(name: &str, target: &str) -> FieldDescriptor {
    FieldDescriptor {
        name: name.into(),
        ty: "ref".into(),
        required: false,
        unique: false,
        references: Some(target.into()),
        ..Default::default()
    }
}

fn collection(name: &str, owner: &str, fields: Vec<FieldDescriptor>) -> CollectionDescriptor {
    CollectionDescriptor {
        name: name.into(),
        owner_app: owner.into(),
        fields,
        indexes: vec![],
    }
}

#[compio::test]
async fn l10_multi_app_union_ownership_and_fail_closed_drop_lifecycle() {
    use zeroship_migrate::DeclarativeAuthor;

    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let engine = MigrationEngine::new();
    let schema = cfg.project_schema.clone();
    let author_a = DeclarativeAuthor::new(schema.clone(), "app_a");
    let author_b = DeclarativeAuthor::new(schema.clone(), "app_b");

    // --- DEPLOY 1 (app_a): declare `products`. The union is { products(app_a) }. ---
    let products = || collection("products", "app_a", vec![field("name", "string")]);
    let union_v1 = desired_snapshot(&schema, &[products()]).expect("union v1");
    let plan1 = engine
        .plan_declarative(&union_v1, &SchemaSnapshot::default(), &HashMap::new(), &author_a, &[], &guard_cfg(&cfg))
        .expect("app_a plans products");
    engine
        .apply(&plan1.plain, Approval::None, &PostgresBackend::new(&conn), &cfg, "app_a")
        .await
        .expect("app_a deploys products");
    assert!(table_exists(&conn, &schema, "products").await, "products is live");

    // --- DEPLOY 2 (app_b): declare `orders`(FK→products) + USE products (re-declare
    //     the IDENTICAL products shape). The union grows to
    //     { products(app_a), orders(app_b) }; B's deploy emits ONLY `orders`
    //     (products == live ⇒ no op, no owner violation). ---
    let orders = collection(
        "orders",
        "app_b",
        vec![field("qty", "number"), fk_field("product", "products")],
    );
    let union_v2 = desired_snapshot(&schema, &[products(), orders]).expect("union v2");
    let live = snapshot_schema(&conn, &schema).await.expect("snap after deploy 1");
    let plan2 = engine
        .plan_declarative(&union_v2, &live, &HashMap::new(), &author_b, &[], &guard_cfg(&cfg))
        .expect("app_b plans orders (products is a no-op)");
    // Every emitted op targets `orders` only — products is untouched (no-op).
    assert!(
        plan2.plain.items.iter().all(|m| m.migration.up.contains("orders")),
        "app_b's deploy must emit ONLY orders ops; got {:?}",
        plan2.plain.items.iter().map(|m| &m.migration.name).collect::<Vec<_>>()
    );
    engine
        .apply(&plan2.plain, Approval::None, &PostgresBackend::new(&conn), &cfg, "app_b")
        .await
        .expect("app_b deploys orders with a cross-app FK to products");
    assert!(table_exists(&conn, &schema, "orders").await, "orders is live");
    // The cross-app FK materialised (orders → products).
    let live2 = snapshot_schema(&conn, &schema).await.expect("snap after deploy 2");
    assert!(
        live2.tables["orders"].constraints.iter().any(|c| c.kind == "FOREIGN KEY"),
        "orders must carry the cross-app FK to products"
    );
    // The whole union re-diffs clean.
    let drift = diff_snapshots(&union_v2.snapshot, &live2);
    assert!(drift.missing_objects.is_empty(), "union drift after deploy 2: {:?}", drift.missing_objects);

    // --- CONFLICT: app_b re-declares `products` with a DIFFERENT shape (an extra
    //     field) → ConflictingDeclaration (one table, one owner; a conflicting
    //     re-declaration is a hard deploy error, never a silent last-writer merge). ---
    let products_b_shape = collection("products", "app_b", vec![field("name", "string"), field("sku", "string")]);
    let conflicting_union = desired_snapshot(&schema, &[products_b_shape]).expect("conflicting union builds");
    // The conflict is detected at diff time (two apps, differing shapes for the
    // same table). We supply BOTH declarers so the union merge sees the conflict.
    let conflict_union = desired_snapshot(
        &schema,
        &[products(), collection("products", "app_b", vec![field("name", "string"), field("sku", "string")])],
    );
    match conflict_union {
        Ok(_) => panic!("desired_snapshot must reject two differing declarations of the same table"),
        Err(DeclarativeError::ConflictingDeclaration { ref table, ref apps }) => {
            assert_eq!(table, "products");
            assert!(
                apps.contains(&"app_a".to_string()) && apps.contains(&"app_b".to_string()),
                "the conflict must name both declarers (sorted, deduped); got {apps:?}"
            );
        }
        Err(other) => panic!("expected ConflictingDeclaration, got {other:?}"),
    }
    // (A single-declarer conflicting shape still builds a snapshot; the union-level
    // conflict only fires when two apps declare the same table differently.)
    let _ = conflicting_union;

    // --- FAIL-CLOSED DROP: app_b deploys a PARTIAL union OMITTING A's products
    //     while products is live + A-owned. With live_ownership{products: app_a},
    //     the differ refuses to drop it under app_b's authority → NotTableOwner. ---
    let live_now = snapshot_schema(&conn, &schema).await.expect("snap before partial deploy");
    let partial = desired_snapshot(&schema, &[collection("orders", "app_b", vec![field("qty", "number"), fk_field("product", "products")])])
        .expect("partial union (only app_b's orders)");
    let ownership: HashMap<String, String> = [
        ("products".to_string(), "app_a".to_string()),
        ("orders".to_string(), "app_b".to_string()),
    ]
    .into_iter()
    .collect();
    let err = engine
        .plan_declarative(&partial, &live_now, &ownership, &author_b, &[], &guard_cfg(&cfg))
        .unwrap_err();
    assert!(
        matches!(
            err,
            DeclarativeError::NotTableOwner { ref table, ref owner, ref deploying_app }
                if table == "products" && owner == "app_a" && deploying_app == "app_b"
        ),
        "a partial-union deploy omitting a foreign-owned live table must fail closed (NotTableOwner); got {err:?}"
    );

    // --- FAIL-CLOSED DROP (unknown ownership): the same partial deploy with NO
    //     ownership entry for products fails closed with DropOfUnownedTable. ---
    let err = engine
        .plan_declarative(&partial, &live_now, &HashMap::new(), &author_b, &[], &guard_cfg(&cfg))
        .unwrap_err();
    assert!(
        matches!(err, DeclarativeError::DropOfUnownedTable { ref table } if table == "products"),
        "a drop of an ownership-unknown live table must fail closed (DropOfUnownedTable); got {err:?}"
    );

    // A's table persists through every refused deploy (no foreign drop ran).
    assert!(table_exists(&conn, &schema, "products").await, "app_a's products survives the fail-closed refusals");
    assert!(table_exists(&conn, &schema, "orders").await);

    teardown(&conn, &cfg).await;
}
