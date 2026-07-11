#![cfg(feature = "native-pg")]
//! Faithful repeatable-migration tests (v3 Plan E — Flyway `R__` / Liquibase
//! `runOnChange`) against a REAL Postgres (no shims).
//!
//! A repeatable migration ([`MigrationFlags::repeatable`]) has a STABLE identity
//! (its `version`/name never changes per edit) and RE-APPLIES whenever its
//! definition checksum changes — for views/functions/triggers edited over time.
//!
//! Requires the dedicated database `zero_migrate_test` on :5440 (recreated by
//! the test runbook). Each test runs in its own meta + project schema.

use compio_postgres::Client;
use zero_migrate::model::migration::Checksum;
use zero_migrate::{
    apply, check_checksum_drift, apply::executor::ApplyError, Approval, ExecutorConfig, Migration,
    MigrationFlags, MigrationId,
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

fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(format!("prj_{tok}"), format!("proj_{tok}"));
    c.pg.meta_schema = format!("meta_{tok}");
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
            cfg.project_schema, cfg.pg.meta_schema
        ))
        .await;
}

/// Build an ordinary (versioned, run-once) transactional migration.
fn versioned(version: MigrationId, name: &str, up: &str) -> Migration {
    Migration {
        version,
        name: name.to_string(),
        up: up.to_string(),
        down: None,
        checksum: Checksum::of(&zero_migrate::ChecksumInput {
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

/// Build a REPEATABLE migration with a stable identity and a correct checksum.
fn repeatable(version: MigrationId, name: &str, up: &str) -> Migration {
    let mut m = versioned(version, name, up);
    m.flags.repeatable = true;
    m.checksum = Checksum::of(&zero_migrate::ChecksumInput::from_migration(&m));
    m
}

/// Re-checksum a migration after editing its `up` (the author would re-derive it
/// from the new definition; here we recompute it the same way `versioned` does).
fn with_up(mut m: Migration, up: &str) -> Migration {
    m.up = up.to_string();
    m.checksum = Checksum::of(&zero_migrate::ChecksumInput::from_migration(&m));
    m
}

/// Count RAW `completed` events for a version (every re-apply appends one), as
/// opposed to net-applied state — a repeatable accrues multiple over re-applies.
async fn completed_events(conn: &Client, cfg: &ExecutorConfig, version: &str) -> i64 {
    let row = conn
        .query_one(
            &format!(
                "SELECT count(*)::bigint AS n FROM \"{}\".schema_migrations \
                 WHERE version = $1 AND phase = 'completed'",
                cfg.pg.meta_schema
            ),
            &[&version],
        )
        .await
        .expect("count completed events");
    row.get("n")
}

/// Read the latest completed journal kind for one migration identity.
async fn latest_completed_kind(
    conn: &Client,
    cfg: &ExecutorConfig,
    version: &str,
) -> Option<String> {
    let row = conn
        .query_opt(
            &format!(
                "SELECT kind FROM \"{}\".schema_migrations \
                 WHERE version = $1 AND phase = 'completed' \
                 ORDER BY event_seq DESC LIMIT 1",
                cfg.pg.meta_schema
            ),
            &[&version],
        )
        .await
        .expect("read latest completed kind");
    row.and_then(|r| r.get::<_, Option<String>>("kind"))
}

/// Run a one-row scalar `int` (`int4`) query against the project (returns the
/// view's value widened to `i64` for convenient comparison).
async fn scalar_int(conn: &Client, sql: &str) -> i64 {
    i64::from(
        conn.query_one(sql, &[])
            .await
            .expect("scalar query")
            .get::<_, i32>(0),
    )
}

// ---------------------------------------------------------------------------
// COMMIT 1 — repeatable phase: apply, skip-unchanged, re-apply-on-change, ordering
// ---------------------------------------------------------------------------

#[compio::test]
async fn repeatable_view_applies_on_first_deploy() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    let schema = &cfg.project_schema;

    let r = repeatable(
        MigrationId::generate(),
        "v_view",
        &format!("CREATE OR REPLACE VIEW \"{schema}\".v AS SELECT 1 AS n"),
    );
    let out = apply(&conn, &cfg, std::slice::from_ref(&r), Approval::None, "actor")
        .await
        .expect("apply repeatable");

    assert_eq!(out.applied, vec![r.version.as_str().to_string()]);
    assert_eq!(
        scalar_int(&conn, &format!("SELECT n FROM \"{schema}\".v")).await,
        1,
        "the view must exist and return 1"
    );
    assert_eq!(
        completed_events(&conn, &cfg, r.version.as_str()).await,
        1,
        "one completed event after first apply"
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn repeatable_unchanged_redeploy_is_skipped() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    let schema = &cfg.project_schema;

    let r = repeatable(
        MigrationId::generate(),
        "v_view",
        &format!("CREATE OR REPLACE VIEW \"{schema}\".v AS SELECT 1 AS n"),
    );
    apply(&conn, &cfg, std::slice::from_ref(&r), Approval::None, "actor")
        .await
        .expect("first apply");

    // Redeploy with the SAME definition (same checksum) ⇒ SKIPPED, not re-applied.
    let out = apply(&conn, &cfg, std::slice::from_ref(&r), Approval::None, "actor")
        .await
        .expect("redeploy unchanged");

    assert!(
        out.applied.is_empty(),
        "an unchanged repeatable must not re-apply, got {:?}",
        out.applied
    );
    assert!(
        out.skipped.contains(&r.version.as_str().to_string()),
        "an unchanged repeatable must be reported skipped"
    );
    assert_eq!(
        completed_events(&conn, &cfg, r.version.as_str()).await,
        1,
        "still exactly one completed event (no new row on a skip)"
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn repeatable_changed_redeploy_is_reapplied() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    let schema = &cfg.project_schema;

    let v = MigrationId::generate();
    let r1 = repeatable(
        v.clone(),
        "v_view",
        &format!("CREATE OR REPLACE VIEW \"{schema}\".v AS SELECT 1 AS n"),
    );
    apply(&conn, &cfg, std::slice::from_ref(&r1), Approval::None, "actor")
        .await
        .expect("first apply");
    assert_eq!(
        scalar_int(&conn, &format!("SELECT n FROM \"{schema}\".v")).await,
        1
    );

    // Same identity (version/name), CHANGED definition (SELECT 2) ⇒ RE-APPLIED.
    let r2 = with_up(
        r1.clone(),
        &format!("CREATE OR REPLACE VIEW \"{schema}\".v AS SELECT 2 AS n"),
    );
    assert_ne!(r1.checksum, r2.checksum, "the edit must change the checksum");

    let out = apply(&conn, &cfg, std::slice::from_ref(&r2), Approval::None, "actor")
        .await
        .expect("redeploy changed");

    assert_eq!(
        out.applied,
        vec![v.as_str().to_string()],
        "a changed repeatable must re-apply"
    );
    assert_eq!(
        scalar_int(&conn, &format!("SELECT n FROM \"{schema}\".v")).await,
        2,
        "the view must now return the NEW definition's value"
    );
    assert_eq!(
        completed_events(&conn, &cfg, v.as_str()).await,
        2,
        "two completed events after a re-apply (append-only)"
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn repeatables_run_after_versioned_pending() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    let schema = &cfg.project_schema;

    // A versioned migration creates the table; the repeatable view reads it. If the
    // repeatable ran BEFORE the versioned migration, the CREATE VIEW would fail
    // (relation does not exist). Ordering after-versioned is what makes this pass.
    let table = versioned(
        MigrationId::generate(),
        "create_nums",
        &format!("CREATE TABLE \"{schema}\".nums (n int); INSERT INTO \"{schema}\".nums VALUES (7)"),
    );
    let view = repeatable(
        MigrationId::generate(),
        "v_over_nums",
        &format!("CREATE OR REPLACE VIEW \"{schema}\".v AS SELECT n FROM \"{schema}\".nums"),
    );

    // Supply the repeatable FIRST in the slice to prove ordering is by phase, not
    // by input order.
    let set = vec![view.clone(), table.clone()];
    let out = apply(&conn, &cfg, &set, Approval::None, "actor")
        .await
        .expect("apply mixed set");

    // The versioned migration applies first, then the repeatable.
    assert_eq!(
        out.applied,
        vec![table.version.as_str().to_string(), view.version.as_str().to_string()],
        "versioned must apply before the repeatable"
    );
    assert_eq!(
        scalar_int(&conn, &format!("SELECT n FROM \"{schema}\".v")).await,
        7
    );

    drop_schemas(&conn, &cfg).await;
}

/// P2a regression: repeatables are ALWAYS applied transactionally, even if the
/// loaded SQL is classified as non-transactional (`flags.transactional=false`).
/// The apply must journal `kind='repeatable'`, never the two-phase path's
/// hardcoded `kind='apply'`, or the next drift check treats the repeatable as a
/// kind-mismatch tamper.
#[compio::test]
async fn classified_non_transactional_repeatable_is_journaled_repeatable_and_redeploys_clean() {
    use zero_migrate::plan::loader::load_dir_migrations;

    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    let schema = cfg.project_schema.clone();

    let dir = loader_tempdir();
    write_loader_file(
        &dir,
        "V1__create_enum.sql",
        &format!("CREATE TYPE \"{schema}\".mood AS ENUM ('sad');"),
    );
    write_loader_file(
        &dir,
        "R__add_enum_value.sql",
        &format!("ALTER TYPE \"{schema}\".mood ADD VALUE IF NOT EXISTS 'ok';"),
    );

    let migs = load_dir_migrations(&dir).expect("load enum repeatable");
    let rep = migs
        .iter()
        .find(|m| m.flags.repeatable)
        .expect("loaded repeatable present");
    assert!(
        !rep.flags.transactional,
        "ALTER TYPE ADD VALUE must be classified as non-transactional"
    );
    let rep_id = rep.version.as_str().to_string();

    let out1 = apply(&conn, &cfg, &migs, Approval::None, "actor")
        .await
        .expect("first apply with non-transactional-classified repeatable");
    assert!(
        out1.applied.contains(&rep_id),
        "first deploy must apply the repeatable"
    );
    assert_eq!(
        latest_completed_kind(&conn, &cfg, &rep_id).await.as_deref(),
        Some("repeatable"),
        "repeatable apply must journal kind='repeatable', not kind='apply'"
    );

    let drift = check_checksum_drift(&conn, &cfg, &migs)
        .await
        .expect("checksum drift check after repeatable apply");
    assert!(
        drift.is_clean(),
        "a correctly journaled repeatable must not report tamper drift: {drift:?}"
    );

    let out2 = apply(&conn, &cfg, &migs, Approval::None, "actor")
        .await
        .expect("unchanged redeploy must pass the drift gate");
    assert!(
        !out2.applied.contains(&rep_id),
        "unchanged repeatable must be skipped on redeploy"
    );
    assert!(
        out2.skipped.contains(&rep_id),
        "unchanged repeatable must be reported skipped"
    );
    assert_eq!(
        completed_events(&conn, &cfg, &rep_id).await,
        1,
        "unchanged redeploy must not append another repeatable journal row"
    );

    drop_schemas(&conn, &cfg).await;
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// COMMIT 2 — drift exemption (once-only STILL aborts), ordering, security
// ---------------------------------------------------------------------------

/// CRITICAL: a repeatable's CHANGED checksum re-runs (no tamper-abort) WHILE a
/// once-only migration's CHANGED checksum STILL aborts with `ChecksumDrift`. This
/// proves the drift exemption is scoped strictly to repeatables and does NOT
/// weaken the once-only tamper guard.
#[compio::test]
async fn repeatable_changed_checksum_reruns_but_once_only_still_aborts() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    let schema = &cfg.project_schema;

    // First deploy: a once-only versioned migration + a repeatable view.
    let once_v = MigrationId::generate();
    let once = versioned(
        once_v.clone(),
        "create_t",
        &format!("CREATE TABLE \"{schema}\".t (id int)"),
    );
    let rep_v = MigrationId::generate();
    let rep1 = repeatable(
        rep_v.clone(),
        "v_view",
        &format!("CREATE OR REPLACE VIEW \"{schema}\".v AS SELECT 1 AS n"),
    );
    apply(&conn, &cfg, &[once.clone(), rep1.clone()], Approval::None, "actor")
        .await
        .expect("first deploy");

    // Part A — the repeatable's checksum CHANGES: it must re-run, NOT abort.
    let rep2 = with_up(
        rep1.clone(),
        &format!("CREATE OR REPLACE VIEW \"{schema}\".v AS SELECT 2 AS n"),
    );
    assert_ne!(rep1.checksum, rep2.checksum);
    let out = apply(&conn, &cfg, &[once.clone(), rep2.clone()], Approval::None, "actor")
        .await
        .expect("a changed repeatable must re-run, never abort on drift");
    assert_eq!(out.applied, vec![rep_v.as_str().to_string()]);
    assert_eq!(
        scalar_int(&conn, &format!("SELECT n FROM \"{schema}\".v")).await,
        2
    );

    // Part B — the ONCE-ONLY migration's checksum CHANGES (same identity, mutated
    // `up`): this STILL aborts with `ChecksumDrift`. We pair it with the (now
    // unchanged) repeatable to show the repeatable path does not mask the abort.
    let once_tampered = with_up(
        once.clone(),
        &format!("CREATE TABLE \"{schema}\".t (id bigint)"),
    );
    assert_ne!(once.checksum, once_tampered.checksum);
    let err = apply(
        &conn,
        &cfg,
        &[once_tampered.clone(), rep2.clone()],
        Approval::None,
        "actor",
    )
    .await
    .expect_err("a once-only migration's changed checksum must STILL abort");
    assert!(
        matches!(err, ApplyError::ChecksumDrift { ref version, .. } if version == once_v.as_str()),
        "expected `ChecksumDrift` on the once-only migration, got {err:?}"
    );

    drop_schemas(&conn, &cfg).await;
}

/// CRITICAL tamper bypass (v3 Plan E re-critic): a once-only migration applied as
/// `kind='apply'` cannot be turned into a repeatable by an attacker simply flipping
/// `flags.repeatable=true` and mutating `up`. The drift exemption must anchor on the
/// JOURNALED kind, not the attacker-supplied flag. Journaled `kind='apply'` +
/// supplied `repeatable=true` is a KIND MISMATCH ⇒ tamper ⇒ `ChecksumDrift` abort,
/// NOT a silent re-run of the mutated `up`.
#[compio::test]
async fn flip_to_repeatable_on_an_applied_once_only_is_tamper_not_a_rerun() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    let schema = &cfg.project_schema;

    // Deploy 1 — X applied ONCE-ONLY (kind='apply', checksum csA).
    let x_v = MigrationId::generate();
    let x_csa = versioned(
        x_v.clone(),
        "create_secret",
        &format!("CREATE TABLE \"{schema}\".secret (id int)"),
    );
    apply(&conn, &cfg, std::slice::from_ref(&x_csa), Approval::None, "actor")
        .await
        .expect("deploy 1: once-only apply");
    assert_eq!(
        completed_events(&conn, &cfg, x_v.as_str()).await,
        1,
        "one completed event after the once-only apply"
    );

    // Deploy 2 — ATTACK: re-ship X with the SAME version, flags.repeatable=true,
    // and a MUTATED `up` (csB). The supplied flag claims "repeatable", but the
    // journal stamped this version 'apply' (once-only). Kind mismatch ⇒ TAMPER.
    let mut x_csb = with_up(
        x_csa.clone(),
        &format!("CREATE TABLE \"{schema}\".secret (id bigint); DROP TABLE \"{schema}\".secret"),
    );
    x_csb.flags.repeatable = true;
    x_csb.checksum = Checksum::of(&zero_migrate::ChecksumInput::from_migration(&x_csb));
    assert_ne!(x_csa.checksum, x_csb.checksum, "the attack mutates the up");

    let err = apply(&conn, &cfg, std::slice::from_ref(&x_csb), Approval::None, "actor")
        .await
        .expect_err("flipping an applied once-only to repeatable must ABORT as tamper");
    assert!(
        matches!(err, ApplyError::ChecksumDrift { ref version, .. } if version == x_v.as_str()),
        "expected `ChecksumDrift` on the flipped version, got {err:?}"
    );

    // The mutated up NEVER ran (no second completed event, table unchanged).
    assert_eq!(
        completed_events(&conn, &cfg, x_v.as_str()).await,
        1,
        "the tamper attempt must not append a second completed event"
    );

    drop_schemas(&conn, &cfg).await;
}

/// The REVERSE re-classification is also tamper: a genuinely-repeatable version
/// (journaled `kind='repeatable'`) re-supplied as a once-only (`repeatable=false`)
/// with a changed checksum must ABORT, not silently re-route through the once-only
/// drift path. Journaled `kind='repeatable'` + supplied `repeatable=false` is a
/// kind mismatch ⇒ `ChecksumDrift`.
#[compio::test]
async fn flip_repeatable_to_once_only_is_tamper_not_a_rerun() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    let schema = &cfg.project_schema;

    // Deploy 1 — a genuine repeatable view (journaled kind='repeatable').
    let r_v = MigrationId::generate();
    let r1 = repeatable(
        r_v.clone(),
        "v_view",
        &format!("CREATE OR REPLACE VIEW \"{schema}\".v AS SELECT 1 AS n"),
    );
    apply(&conn, &cfg, std::slice::from_ref(&r1), Approval::None, "actor")
        .await
        .expect("deploy 1: genuine repeatable");

    // Deploy 2 — re-supply the SAME version as a once-only with a changed up.
    let mut r2 = with_up(
        r1.clone(),
        &format!("CREATE OR REPLACE VIEW \"{schema}\".v AS SELECT 2 AS n"),
    );
    r2.flags.repeatable = false;
    r2.checksum = Checksum::of(&zero_migrate::ChecksumInput::from_migration(&r2));
    assert_ne!(r1.checksum, r2.checksum);

    let err = apply(&conn, &cfg, std::slice::from_ref(&r2), Approval::None, "actor")
        .await
        .expect_err("re-classifying a repeatable to once-only must ABORT as tamper");
    assert!(
        matches!(err, ApplyError::ChecksumDrift { ref version, .. } if version == r_v.as_str()),
        "expected `ChecksumDrift` on the re-classified version, got {err:?}"
    );

    // The view keeps its deploy-1 definition (the abort ran nothing).
    assert_eq!(
        scalar_int(&conn, &format!("SELECT n FROM \"{schema}\".v")).await,
        1,
        "the abort must leave the original repeatable definition intact"
    );

    drop_schemas(&conn, &cfg).await;
}

/// Repeatables are ordered among themselves by `depends_on` (topo), regardless of
/// input order: `b` depends on `a`, so `a` re-applies before `b` even when `b` is
/// supplied first. `b`'s view reads `a`'s view, so a wrong order would fail.
#[compio::test]
async fn repeatables_ordered_by_depends_on() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    let schema = &cfg.project_schema;

    let a_v = MigrationId::generate();
    let a = repeatable(
        a_v.clone(),
        "v_a",
        &format!("CREATE OR REPLACE VIEW \"{schema}\".a AS SELECT 5 AS n"),
    );
    let b_v = MigrationId::generate();
    let mut b = repeatable(
        b_v.clone(),
        "v_b",
        // b reads a — if b ran before a, the relation would not exist.
        &format!("CREATE OR REPLACE VIEW \"{schema}\".b AS SELECT n FROM \"{schema}\".a"),
    );
    b.depends_on = vec![a_v.clone()];
    b.checksum = Checksum::of(&zero_migrate::ChecksumInput::from_migration(&b));

    // Supply b BEFORE a in the slice — ordering must come from depends_on, not input.
    let out = apply(&conn, &cfg, &[b.clone(), a.clone()], Approval::None, "actor")
        .await
        .expect("dependency-ordered repeatables must apply a before b");

    assert_eq!(
        out.applied,
        vec![a_v.as_str().to_string(), b_v.as_str().to_string()],
        "a (the dependency) must re-apply before b"
    );
    assert_eq!(
        scalar_int(&conn, &format!("SELECT n FROM \"{schema}\".b")).await,
        5
    );

    drop_schemas(&conn, &cfg).await;
}

/// SECURITY: a repeatable's `up` is held to the SAME guard deny-list as any `up`.
/// A cross-schema reference (the `control` schema — cross-tenant) is guard-denied;
/// nothing is applied and nothing is journaled.
#[compio::test]
async fn repeatable_cross_schema_up_is_guard_denied() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let rv = MigrationId::generate();
    let evil = repeatable(
        rv.clone(),
        "evil_view",
        // References the `control` schema — cross-tenant, must be denied.
        "CREATE OR REPLACE VIEW control.leak AS SELECT 1 AS n",
    );
    let err = apply(&conn, &cfg, std::slice::from_ref(&evil), Approval::None, "actor")
        .await
        .expect_err("a cross-schema repeatable up must be guard-denied");
    assert!(
        matches!(err, ApplyError::Guard { ref version, .. } if version == rv.as_str()),
        "expected a guard denial on the repeatable, got {err:?}"
    );

    // Nothing journaled (the denial fires before any execution).
    let applied = zero_migrate::apply::journal::applied(&conn, &cfg)
        .await
        .unwrap_or_default();
    assert!(applied.is_empty(), "a guard-denied repeatable journals nothing");

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// GROUP 2 — malformed-combination pre-flight rejections (fail-closed)
// ---------------------------------------------------------------------------

/// #4a — a `repeatable=true` migration that ALSO carries `supersedes` is malformed
/// (a repeatable cannot be a squash). It must be rejected up-front, before the
/// partition silently drops its supersedes into the repeatable phase. Nothing runs.
#[compio::test]
async fn repeatable_with_supersedes_is_rejected() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    let schema = &cfg.project_schema;

    let collapsed = MigrationId::generate();
    let mut bad = repeatable(
        MigrationId::generate(),
        "v_view",
        &format!("CREATE OR REPLACE VIEW \"{schema}\".v AS SELECT 1 AS n"),
    );
    bad.supersedes = vec![collapsed];

    let err = apply(&conn, &cfg, std::slice::from_ref(&bad), Approval::None, "actor")
        .await
        .expect_err("a repeatable with supersedes must be rejected");
    assert!(
        matches!(err, ApplyError::RepeatableCannotSquash { ref version } if version == bad.version.as_str()),
        "expected `RepeatableCannotSquash`, got {err:?}"
    );

    // Nothing journaled — the rejection precedes any execution.
    let applied = zero_migrate::apply::journal::applied(&conn, &cfg)
        .await
        .unwrap_or_default();
    assert!(applied.is_empty(), "a rejected malformed set journals nothing");

    drop_schemas(&conn, &cfg).await;
}

/// #3d — a VERSIONED (once-only) migration whose `depends_on` names a REPEATABLE in
/// the SAME set is malformed: a once-only migration must not depend on a repeatable
/// (a repeatable runs AFTER all versioned, so the dependency can never be satisfied
/// in order). Reject with a DEDICATED, accurate error — NOT the misleading
/// `MissingDependency` the partition would otherwise produce.
#[compio::test]
async fn versioned_depends_on_repeatable_is_dedicated_error() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    let schema = &cfg.project_schema;

    let rep_v = MigrationId::generate();
    let rep = repeatable(
        rep_v.clone(),
        "v_view",
        &format!("CREATE OR REPLACE VIEW \"{schema}\".v AS SELECT 1 AS n"),
    );
    let once_v = MigrationId::generate();
    let mut once = versioned(
        once_v.clone(),
        "create_t",
        &format!("CREATE TABLE \"{schema}\".t (id int)"),
    );
    once.depends_on = vec![rep_v.clone()];
    once.checksum = Checksum::of(&zero_migrate::ChecksumInput::from_migration(&once));

    let err = apply(&conn, &cfg, &[rep.clone(), once.clone()], Approval::None, "actor")
        .await
        .expect_err("a versioned migration depending on a repeatable must be rejected");
    assert!(
        matches!(
            err,
            ApplyError::OnceOnlyDependsOnRepeatable { ref version, ref dependency }
                if version == once_v.as_str() && dependency == rep_v.as_str()
        ),
        "expected the dedicated `OnceOnlyDependsOnRepeatable`, NOT MissingDependency, got {err:?}"
    );

    drop_schemas(&conn, &cfg).await;
}

/// #4c — a `repeatable=true` migration with a `down` violates the invariant that a
/// repeatable is replace-style (no true reverse). Reject up-front, fail-closed.
#[compio::test]
async fn repeatable_with_down_is_rejected() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    let schema = &cfg.project_schema;

    let up = format!("CREATE OR REPLACE VIEW \"{schema}\".v AS SELECT 1 AS n");
    let down = format!("DROP VIEW \"{schema}\".v");
    let mut bad = repeatable(MigrationId::generate(), "v_view", &up);
    bad.down = Some(down.clone());
    bad.checksum = Checksum::of(&zero_migrate::ChecksumInput::from_migration(&bad));

    let err = apply(&conn, &cfg, std::slice::from_ref(&bad), Approval::None, "actor")
        .await
        .expect_err("a repeatable with a down must be rejected");
    assert!(
        matches!(err, ApplyError::RepeatableHasDown { ref version } if version == bad.version.as_str()),
        "expected `RepeatableHasDown`, got {err:?}"
    );

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// COMMIT 3 — LOADER repeatable identity is STABLE across reloads (regression)
// ---------------------------------------------------------------------------

/// A repeatable loaded from disk via `load_dir` must keep a STABLE identity across
/// SEPARATE loads, so an unchanged `R__` file is SKIPPED on redeploy — not
/// re-applied. RED before the deterministic `repeatable_id_for_name` fix: the
/// loader minted a fresh random `MigrationId` per load, so the version-keyed
/// re-run oracle never matched the prior journaled row and the repeatable
/// re-applied on EVERY load (accruing a phantom journal event each time).
#[compio::test]
async fn loaded_repeatable_is_stable_across_reloads() {
    use zero_migrate::plan::loader::load_dir_migrations;

    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    let schema = cfg.project_schema.clone();

    let dir = loader_tempdir();
    let v1 = format!("CREATE OR REPLACE VIEW \"{schema}\".v AS SELECT 1 AS n");
    write_loader_file(&dir, "R__a_view.sql", &v1);

    // Load 1 (fresh) + apply: the repeatable applies once.
    let migs1 = load_dir_migrations(&dir).expect("load 1");
    let rep_id = migs1
        .iter()
        .find(|m| m.name == "a_view")
        .expect("loaded repeatable present")
        .version
        .as_str()
        .to_string();
    let out1 = apply(&conn, &cfg, &migs1, Approval::None, "actor")
        .await
        .expect("apply load 1");
    assert!(out1.applied.contains(&rep_id), "first load applies the repeatable");
    assert_eq!(completed_events(&conn, &cfg, &rep_id).await, 1);

    // Load 2 (a SEPARATE fresh load), file UNCHANGED ⇒ the derived id must be
    // IDENTICAL ⇒ the re-run oracle SKIPS it (no second journal event).
    let migs2 = load_dir_migrations(&dir).expect("load 2");
    let rep_id2 = migs2
        .iter()
        .find(|m| m.name == "a_view")
        .unwrap()
        .version
        .as_str()
        .to_string();
    assert_eq!(
        rep_id, rep_id2,
        "the loader must derive a STABLE repeatable id across separate loads"
    );
    let out2 = apply(&conn, &cfg, &migs2, Approval::None, "actor")
        .await
        .expect("apply load 2");
    assert!(
        out2.applied.is_empty(),
        "an unchanged reloaded repeatable must NOT re-apply, got {:?}",
        out2.applied
    );
    assert!(
        out2.skipped.contains(&rep_id),
        "an unchanged reloaded repeatable must be reported skipped"
    );
    assert_eq!(
        completed_events(&conn, &cfg, &rep_id).await,
        1,
        "no phantom second journal event on an unchanged reload"
    );

    // Change the file body ⇒ same id, new checksum ⇒ RE-APPLIES.
    let v2 = format!("CREATE OR REPLACE VIEW \"{schema}\".v AS SELECT 2 AS n");
    write_loader_file(&dir, "R__a_view.sql", &v2);
    let migs3 = load_dir_migrations(&dir).expect("load 3");
    let out3 = apply(&conn, &cfg, &migs3, Approval::None, "actor")
        .await
        .expect("apply load 3");
    assert!(out3.applied.contains(&rep_id), "a changed repeatable must re-apply");
    assert_eq!(
        completed_events(&conn, &cfg, &rep_id).await,
        2,
        "a re-apply adds exactly one completed event"
    );
    assert_eq!(
        scalar_int(&conn, &format!("SELECT n FROM \"{schema}\".v")).await,
        2,
        "the changed view definition is live"
    );

    drop_schemas(&conn, &cfg).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A throwaway temp directory for the loader test, unique per call.
fn loader_tempdir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("zsmig_loader_pg_{pid}_{nanos}_{n}"));
    std::fs::create_dir_all(&dir).expect("create loader temp dir");
    dir
}

fn write_loader_file(dir: &std::path::Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).expect("write loader fixture");
}
