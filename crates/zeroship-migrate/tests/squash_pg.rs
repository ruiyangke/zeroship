#![cfg(feature = "native-pg")]
//! Faithful squash (Plan 9 sub-feature B) tests against a REAL Postgres.
//!
//! Squash collapses a contiguous prefix `[v1..vN]` of applied history into a
//! single supersession migration `S`. The journal is immutable, so squash NEVER
//! deletes `v1..vN`; it models squash as a SUPERSESSION. These tests prove:
//! (1) existing-DB squash records `S` without running its up; (2) fresh-DB squash
//! runs `S.up` once and skips the superseded versions (no double-apply);
//! (3) a later `depends_on` a superseded version is satisfied by `S`;
//! (4) partial-overlap is refused; (5) `S.up` is guard-denied if dangerous.

use std::time::Duration;

use compio_postgres::Client;
use zeroship_migrate::apply::executor::ApplyError;
use zeroship_migrate::model::migration::Checksum;
use zeroship_migrate::ops::squash::SquashError;
use zeroship_migrate::{
    apply, ensure_journal, squash, status, Approval, ExecutorConfig, Migration,
    MigrationFlags, MigrationId, PostgresBackend,
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
    c.pg.meta_schema = format!("meta_{tok}");
    c.pg.statement_timeout = Duration::from_secs(30);
    c.pg.lock_timeout = Duration::from_secs(10);
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

/// A squash migration superseding `sup`.
fn squash_mig(version: MigrationId, name: &str, up: &str, sup: Vec<MigrationId>) -> Migration {
    let mut m = mig(version, name, up);
    m.supersedes = sup;
    m.checksum = Checksum::of(&zeroship_migrate::ChecksumInput::from_migration(&m));
    m
}

async fn table_exists(conn: &Client, schema: &str, table: &str) -> bool {
    conn.query_one(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_schema = $1 AND table_name = $2) AS e",
        &[&schema, &table],
    )
    .await
    .expect("table_exists")
    .get::<_, bool>("e")
}

/// Three ordered migrations v1<v2<v3 each creating a table, plus a squash S that
/// supersedes them with a combined `up`.
async fn three_plus_squash(
    schema: &str,
) -> (Migration, Migration, Migration, Migration) {
    let v1 = MigrationId::generate();
    compio::time::sleep(Duration::from_millis(2)).await;
    let v2 = MigrationId::generate();
    compio::time::sleep(Duration::from_millis(2)).await;
    let v3 = MigrationId::generate();
    compio::time::sleep(Duration::from_millis(2)).await;
    let s = MigrationId::generate();
    let m1 = mig(v1.clone(), "t1", &format!("CREATE TABLE \"{schema}\".\"t1\" (id bigint)"));
    let m2 = mig(v2.clone(), "t2", &format!("CREATE TABLE \"{schema}\".\"t2\" (id bigint)"));
    let m3 = mig(v3.clone(), "t3", &format!("CREATE TABLE \"{schema}\".\"t3\" (id bigint)"));
    // S's combined up = all three tables (the equivalent net effect for a fresh DB).
    let s_up = format!(
        "CREATE TABLE \"{schema}\".\"t1\" (id bigint); \
         CREATE TABLE \"{schema}\".\"t2\" (id bigint); \
         CREATE TABLE \"{schema}\".\"t3\" (id bigint);"
    );
    let s = squash_mig(s, "squash_t1_t2_t3", &s_up, vec![v1, v2, v3]);
    (m1, m2, m3, s)
}

/// Test 1: existing-DB squash — apply v1,v2,v3 normally; squash S → S journaled
/// `completed` (kind='squash') WITHOUT running its up; pending against
/// {v1,v2,v3,S,v4} = just v4.
#[compio::test]
async fn existing_db_squash_records_without_running_up() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let (m1, m2, m3, s) = three_plus_squash(&cfg.project_schema).await;
    apply(
        &conn,
        &cfg,
        &[m1.clone(), m2.clone(), m3.clone()],
        Approval::None,
        "ci",
    )
    .await
    .expect("apply v1..v3");

    // Squash on the EXISTING DB — records S without running its up (which would
    // fail: the tables already exist and S.up has no IF NOT EXISTS).
    let out = squash(&PostgresBackend::new(&conn), &cfg, &s, "operator")
        .await
        .expect("squash on existing db");
    assert!(!out.already_present);
    assert_eq!(out.superseded.len(), 3);

    // S is journaled completed with kind='squash'.
    let kind: String = conn
        .query_one(
            &format!(
                "SELECT kind FROM \"{}\".schema_migrations WHERE version = $1",
                cfg.pg.meta_schema
            ),
            &[&s.version.as_str()],
        )
        .await
        .expect("kind")
        .get("kind");
    assert_eq!(kind, "squash");

    // pending against {v1,v2,v3,S,v4} = just v4.
    let v4 = MigrationId::generate();
    let m4 = mig(
        v4.clone(),
        "t4",
        &format!("CREATE TABLE \"{}\".\"t4\" (id bigint)", cfg.project_schema),
    );
    let st = status(
        &conn,
        &cfg,
        &[m1.clone(), m2.clone(), m3.clone(), s.clone(), m4.clone()],
    )
    .await
    .expect("status");
    let pending: Vec<String> = st.pending.iter().map(|v| v.as_str().to_string()).collect();
    assert_eq!(pending, vec![v4.as_str().to_string()], "only v4 is pending");

    drop_schemas(&conn, &cfg).await;
}

/// Test 2: fresh-DB squash — on a clean schema, apply {S(supersedes v1,v2,v3), v4}
/// → S.up runs once, v1,v2,v3 are NOT run (superseded), v4 runs; final schema
/// correct.
#[compio::test]
async fn fresh_db_squash_runs_up_once_and_skips_superseded() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("journal");

    let (m1, m2, m3, s) = three_plus_squash(&cfg.project_schema).await;
    let v4 = MigrationId::generate();
    let m4 = mig(
        v4.clone(),
        "t4",
        &format!("CREATE TABLE \"{}\".\"t4\" (id bigint)", cfg.project_schema),
    );

    // Fresh apply of the FULL set {v1,v2,v3,S,v4}. S supersedes v1..v3, which are
    // NOT applied → S runs its up; v1..v3 are skipped (superseded); v4 runs.
    let out = apply(
        &conn,
        &cfg,
        &[m1.clone(), m2.clone(), m3.clone(), s.clone(), m4.clone()],
        Approval::None,
        "ci",
    )
    .await
    .expect("fresh apply with squash");

    // Applied = S then v4 (v1..v3 superseded → skipped, never in `applied`).
    assert_eq!(
        out.applied,
        vec![s.version.as_str().to_string(), v4.as_str().to_string()],
        "only S and v4 run; v1..v3 are superseded"
    );
    // v1..v3 are in `skipped` (superseded), proving they did not run.
    for v in [&m1, &m2, &m3] {
        assert!(
            out.skipped.contains(&v.version.as_str().to_string()),
            "{} should be skipped (superseded)",
            v.version.as_str()
        );
    }

    // Final schema is correct: all four tables exist (S.up built t1..t3, v4 built t4).
    for t in ["t1", "t2", "t3", "t4"] {
        assert!(table_exists(&conn, &cfg.project_schema, t).await, "{t} missing");
    }
    // S.up ran exactly ONCE: the journal has a single completed row for S, and
    // none for v1..v3 (they were superseded, never journaled).
    let applied = apply::journal::applied(&conn, &cfg).await.expect("applied");
    let versions: Vec<&str> = applied.iter().map(|e| e.version.as_str()).collect();
    assert!(versions.contains(&s.version.as_str()));
    assert!(versions.contains(&v4.as_str()));
    assert!(!versions.contains(&m1.version.as_str()), "v1 not journaled");
    assert!(!versions.contains(&m2.version.as_str()), "v2 not journaled");
    assert!(!versions.contains(&m3.version.as_str()), "v3 not journaled");

    // Re-apply is a no-op (idempotent).
    let out2 = apply(
        &conn,
        &cfg,
        &[m1.clone(), m2.clone(), m3.clone(), s.clone(), m4.clone()],
        Approval::None,
        "ci",
    )
    .await
    .expect("re-apply");
    assert!(out2.is_noop(), "re-apply of a squashed set must be a no-op");

    drop_schemas(&conn, &cfg).await;
}

/// Test 3: a later migration `depends_on` a superseded version (`v2`) → satisfied
/// by `S` applied. On a fresh DB, applying `{S(supersedes v1,v2,v3), v5 depends_on
/// v2}` must NOT fail with a missing dependency: `S` satisfies `v2`.
#[compio::test]
async fn later_depends_on_superseded_version_satisfied_by_squash() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("journal");

    let (m1, m2, m3, s) = three_plus_squash(&cfg.project_schema).await;
    let v5 = MigrationId::generate();
    // v5 depends_on v2 (which S supersedes).
    let mut m5 = mig(
        v5.clone(),
        "t5",
        &format!("CREATE TABLE \"{}\".\"t5\" (id bigint)", cfg.project_schema),
    );
    m5.depends_on = vec![m2.version.clone()];
    m5.checksum = Checksum::of(&zeroship_migrate::ChecksumInput::from_migration(&m5));

    let out = apply(
        &conn,
        &cfg,
        &[m1.clone(), m2.clone(), m3.clone(), s.clone(), m5.clone()],
        Approval::None,
        "ci",
    )
    .await
    .expect("apply: S satisfies v5's depends_on v2");
    // S runs, then v5 (its dep v2 is satisfied by S).
    assert_eq!(
        out.applied,
        vec![s.version.as_str().to_string(), v5.as_str().to_string()]
    );
    assert!(table_exists(&conn, &cfg.project_schema, "t5").await);

    drop_schemas(&conn, &cfg).await;
}

/// Test 4: partial-overlap — `v1,v2` applied but NOT `v3`, then squash
/// `S[v1,v2,v3]` → refused with a clear error, both via `squash()` and via
/// `apply()`.
#[compio::test]
async fn partial_overlap_is_refused() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let (m1, m2, m3, s) = three_plus_squash(&cfg.project_schema).await;
    // Apply only v1, v2 (NOT v3).
    apply(&conn, &cfg, &[m1.clone(), m2.clone()], Approval::None, "ci")
        .await
        .expect("apply v1,v2");

    // squash() refuses the partial overlap (2 of 3 applied).
    let err = squash(&PostgresBackend::new(&conn), &cfg, &s, "operator")
        .await
        .expect_err("squash must refuse a partial overlap");
    assert!(
        matches!(err, SquashError::PartialOverlap { applied: 2, total: 3, .. }),
        "got {err:?}"
    );

    // apply() also refuses: a pending squash with a partial overlap.
    let err2 = apply(
        &conn,
        &cfg,
        &[m1.clone(), m2.clone(), m3.clone(), s.clone()],
        Approval::None,
        "ci",
    )
    .await
    .expect_err("apply must refuse a partial-overlap squash");
    assert!(
        matches!(err2, ApplyError::SquashPartialOverlap { applied: 2, total: 3, .. }),
        "got {err2:?}"
    );

    drop_schemas(&conn, &cfg).await;
}

/// Test 5: a squash whose `up` contains a denied construct is guard-denied (via
/// `squash()` on an existing DB).
#[compio::test]
async fn squash_up_guard_denied() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    // Apply v1..v3 first so the existing-DB squash() reaches the guard.
    let (m1, m2, m3, _good_s) = three_plus_squash(&cfg.project_schema).await;
    apply(
        &conn,
        &cfg,
        &[m1.clone(), m2.clone(), m3.clone()],
        Approval::None,
        "ci",
    )
    .await
    .expect("apply v1..v3");

    // An evil squash with a cross-schema up.
    let sv = MigrationId::generate();
    let evil = squash_mig(
        sv.clone(),
        "evil_squash",
        "CREATE TABLE control.users (id bigint)",
        vec![m1.version.clone(), m2.version.clone(), m3.version.clone()],
    );
    let err = squash(&PostgresBackend::new(&conn), &cfg, &evil, "operator")
        .await
        .expect_err("squash with a cross-schema up must be guard-denied");
    assert!(
        matches!(err, SquashError::Guard { ref version, .. } if version == sv.as_str()),
        "got {err:?}"
    );

    drop_schemas(&conn, &cfg).await;
}

/// Extra: `squash()` refuses when NONE of the superseded versions are applied
/// (that is the FRESH path — use `apply()`).
#[compio::test]
async fn squash_refuses_when_none_applied() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("journal");

    let (_m1, _m2, _m3, s) = three_plus_squash(&cfg.project_schema).await;
    let err = squash(&PostgresBackend::new(&conn), &cfg, &s, "operator")
        .await
        .expect_err("squash must refuse when none of the superseded are applied");
    assert!(
        matches!(err, SquashError::NotAllApplied { applied: 0, total: 3, .. }),
        "got {err:?}"
    );

    drop_schemas(&conn, &cfg).await;
}

/// Extra: re-squash of an already-recorded squash is idempotent.
#[compio::test]
async fn re_squash_is_idempotent() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let (m1, m2, m3, s) = three_plus_squash(&cfg.project_schema).await;
    apply(&conn, &cfg, &[m1, m2, m3], Approval::None, "ci")
        .await
        .expect("apply");
    let o1 = squash(&PostgresBackend::new(&conn), &cfg, &s, "operator").await.expect("squash 1");
    assert!(!o1.already_present);
    let o2 = squash(&PostgresBackend::new(&conn), &cfg, &s, "operator").await.expect("squash 2");
    assert!(o2.already_present, "re-squash is idempotent");

    drop_schemas(&conn, &cfg).await;
}

/// #1 regression — a CHAINED squash over a prefix already covered by an EARLIER
/// net-applied squash must NOT double-apply on a clean run.
///
/// Fresh DB: apply `{v1, v2, S1(supersedes[v1,v2])}` → `S1.up` runs once; `v1`/`v2`
/// are superseded and NEVER journaled (they live only as supersession edges). Then
/// apply a set containing `S2(supersedes[v1,v2])`. `v1`/`v2` are not in `completed`
/// (only in `superseded`, covered by net-applied `S1`), so the old all-or-none gate
/// counted `applied=0` → "fresh path" → re-ran `S2.up` → re-created already-built
/// objects (double-apply / MigrationFailed). The satisfied-set classification must
/// see `v1`/`v2` as SATISFIED (covered by net-applied `S1`) and refuse `S2` with
/// `SquashAlreadyApplied`.
#[compio::test]
async fn chained_squash_over_superseded_prefix_is_already_applied_not_double_run() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("journal");

    // v1, v2 each create a table; S1 supersedes [v1,v2] with the combined up.
    let v1 = MigrationId::generate();
    compio::time::sleep(Duration::from_millis(2)).await;
    let v2 = MigrationId::generate();
    compio::time::sleep(Duration::from_millis(2)).await;
    let s1v = MigrationId::generate();
    let schema = &cfg.project_schema;
    let m1 = mig(v1.clone(), "t1", &format!("CREATE TABLE \"{schema}\".\"t1\" (id bigint)"));
    let m2 = mig(v2.clone(), "t2", &format!("CREATE TABLE \"{schema}\".\"t2\" (id bigint)"));
    let s1_up = format!(
        "CREATE TABLE \"{schema}\".\"t1\" (id bigint); \
         CREATE TABLE \"{schema}\".\"t2\" (id bigint);"
    );
    let s1 = squash_mig(s1v.clone(), "squash_t1_t2", &s1_up, vec![v1.clone(), v2.clone()]);

    // Fresh apply of {v1,v2,S1} — S1.up runs, v1/v2 superseded (never journaled).
    let out1 = apply(
        &conn,
        &cfg,
        &[m1.clone(), m2.clone(), s1.clone()],
        Approval::None,
        "ci",
    )
    .await
    .expect("fresh apply with S1");
    assert_eq!(
        out1.applied,
        vec![s1.version.as_str().to_string()],
        "only S1 runs; v1/v2 superseded"
    );
    // Confirm v1/v2 are NOT journaled (they live only as supersession edges).
    let applied = apply::journal::applied(&conn, &cfg).await.expect("applied");
    let versions: Vec<&str> = applied.iter().map(|e| e.version.as_str()).collect();
    assert!(versions.contains(&s1.version.as_str()));
    assert!(!versions.contains(&v1.as_str()), "v1 not journaled");
    assert!(!versions.contains(&v2.as_str()), "v2 not journaled");

    // S2 supersedes the SAME prefix [v1,v2]. Its up (with no IF NOT EXISTS) would
    // FAIL if re-run, because t1/t2 already exist. The all-or-none gate must
    // classify v1/v2 against the satisfied set (completed ∪ superseded): both are
    // covered by net-applied S1 → fully satisfied → SquashAlreadyApplied, NOT a
    // fresh re-run.
    let s2v = MigrationId::generate();
    let s2 = squash_mig(s2v.clone(), "squash_t1_t2_again", &s1_up, vec![v1.clone(), v2.clone()]);

    let err = apply(
        &conn,
        &cfg,
        &[m1.clone(), m2.clone(), s1.clone(), s2.clone()],
        Approval::None,
        "ci",
    )
    .await
    .expect_err("S2 over an already-superseded prefix must be SquashAlreadyApplied");
    assert!(
        matches!(err, ApplyError::SquashAlreadyApplied { ref version } if version == s2v.as_str()),
        "expected SquashAlreadyApplied for S2, got {err:?}"
    );

    drop_schemas(&conn, &cfg).await;
}

/// Two distinct squashes in the SAME apply set superseding the same prefix is
/// malformed: on a fresh DB neither is net-applied, so the all-or-none gate sees
/// nothing satisfied and (without this check) would let BOTH run → the second's
/// up re-creates what the first built. Must be refused UP-FRONT (fail-closed),
/// not error mid-batch.
#[compio::test]
async fn two_squashes_over_same_prefix_in_one_batch_is_refused_up_front() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("journal");

    let v1 = MigrationId::generate();
    compio::time::sleep(Duration::from_millis(2)).await;
    let v2 = MigrationId::generate();
    compio::time::sleep(Duration::from_millis(2)).await;
    let s1v = MigrationId::generate();
    compio::time::sleep(Duration::from_millis(2)).await;
    let s2v = MigrationId::generate();
    let schema = &cfg.project_schema;
    let m1 = mig(v1.clone(), "t1", &format!("CREATE TABLE \"{schema}\".\"t1\" (id bigint)"));
    let m2 = mig(v2.clone(), "t2", &format!("CREATE TABLE \"{schema}\".\"t2\" (id bigint)"));
    let up = format!(
        "CREATE TABLE \"{schema}\".\"t1\" (id bigint); CREATE TABLE \"{schema}\".\"t2\" (id bigint);"
    );
    // Two distinct squashes, BOTH superseding [v1, v2], in one fresh set.
    let s1 = squash_mig(s1v.clone(), "squash_a", &up, vec![v1.clone(), v2.clone()]);
    let s2 = squash_mig(s2v.clone(), "squash_b", &up, vec![v1.clone(), v2.clone()]);

    let err = apply(
        &conn,
        &cfg,
        &[m1, m2, s1, s2],
        Approval::None,
        "ci",
    )
    .await
    .expect_err("two squashes over the same prefix must be refused up-front");
    assert!(
        matches!(&err, ApplyError::OverlappingSquashes { shared, .. } if shared == v1.as_str() || shared == v2.as_str()),
        "expected OverlappingSquashes, got {err:?}"
    );
    // Nothing applied — neither t1 nor t2 exists.
    let n: i64 = conn
        .query_one(
            &format!(
                "SELECT count(*)::bigint AS n FROM information_schema.tables \
                 WHERE table_schema = '{schema}' AND table_name IN ('t1','t2')"
            ),
            &[],
        )
        .await
        .expect("count tables")
        .get::<_, i64>("n");
    assert_eq!(n, 0, "the refusal ran nothing");

    drop_schemas(&conn, &cfg).await;
}

/// Count `completed` rows for a version in the journal.
async fn completed_count(conn: &Client, meta: &str, version: &str) -> i64 {
    conn.query_one(
        &format!(
            "SELECT count(*)::bigint AS n FROM \"{meta}\".schema_migrations \
             WHERE version = $1 AND phase = 'completed'"
        ),
        &[&version],
    )
    .await
    .expect("completed_count")
    .get::<_, i64>("n")
}

/// Count supersession edges recorded for a squash version.
async fn edge_count(conn: &Client, meta: &str, squash_version: &str) -> i64 {
    conn.query_one(
        &format!(
            "SELECT count(*)::bigint AS n FROM \"{meta}\".schema_migrations_supersedes \
             WHERE squash_version = $1"
        ),
        &[&squash_version],
    )
    .await
    .expect("edge_count")
    .get::<_, i64>("n")
}

/// #2 — a fresh-path (transactional) squash records its `completed` row AND every
/// supersession edge ATOMICALLY (same transaction). Happy path: after apply, BOTH
/// the completed row and all N edges are present.
#[compio::test]
async fn fresh_squash_records_completed_row_and_edges_atomically() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("journal");

    let (_m1, _m2, _m3, s) = three_plus_squash(&cfg.project_schema).await;
    apply(&conn, &cfg, std::slice::from_ref(&s), Approval::None, "ci")
        .await
        .expect("fresh apply of squash S");

    assert_eq!(
        completed_count(&conn, &cfg.pg.meta_schema, s.version.as_str()).await,
        1,
        "S has exactly one completed row"
    );
    assert_eq!(
        edge_count(&conn, &cfg.pg.meta_schema, s.version.as_str()).await,
        3,
        "all three supersession edges present"
    );

    drop_schemas(&conn, &cfg).await;
}

/// #2 — the supersession edges share the SAME transaction as S's `completed` row.
///
/// Discriminator for the old separate-txn bug: inject a `BEFORE INSERT` trigger on
/// `schema_migrations_supersedes` that always raises. A fresh-path (transactional)
/// squash apply then FAILS on the edge insert. Because the edge insert now lives in
/// the SAME transaction as S's `completed` row, the failure rolls BOTH back: NO
/// completed row for S and NO edges. (Under the old post-commit edge write, S's
/// `completed` row was already committed when the edge insert failed — the crash
/// window this fix closes.)
#[compio::test]
async fn fresh_squash_edge_failure_rolls_back_completed_row_same_txn() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("journal");

    // Poison every INSERT into the supersedes table for this meta schema.
    let trg_fn = format!("poison_sup_{tok}");
    conn.batch_execute(&format!(
        "CREATE OR REPLACE FUNCTION \"{meta}\".\"{trg_fn}\"() RETURNS trigger AS $fn$
         BEGIN RAISE EXCEPTION 'poison: supersedes insert blocked'; END;
         $fn$ LANGUAGE plpgsql;
         CREATE TRIGGER poison_sup_trg BEFORE INSERT ON \"{meta}\".schema_migrations_supersedes
             FOR EACH ROW EXECUTE FUNCTION \"{meta}\".\"{trg_fn}\"();",
        meta = cfg.pg.meta_schema
    ))
    .await
    .expect("install poison trigger");

    let (_m1, _m2, _m3, s) = three_plus_squash(&cfg.project_schema).await;

    // Fresh apply: S.up succeeds, the completed-row insert succeeds, but the edge
    // insert (same txn) raises → the WHOLE txn rolls back.
    let err = apply(&conn, &cfg, std::slice::from_ref(&s), Approval::None, "ci")
        .await
        .expect_err("edge insert must fail and roll back the apply");
    assert!(
        matches!(err, ApplyError::Journal(_)),
        "expected a Journal error from the edge insert, got {err:?}"
    );

    // ATOMICITY: neither the completed row nor any edge survived (same txn).
    assert_eq!(
        completed_count(&conn, &cfg.pg.meta_schema, s.version.as_str()).await,
        0,
        "S's completed row must NOT survive an edge-insert failure (same txn)"
    );
    assert_eq!(
        edge_count(&conn, &cfg.pg.meta_schema, s.version.as_str()).await,
        0,
        "no edges survive"
    );
    // S.up's DDL also rolled back (transactional migration): t1 must not exist.
    assert!(
        !table_exists(&conn, &cfg.project_schema, "t1").await,
        "S.up rolled back with the failed txn"
    );

    drop_schemas(&conn, &cfg).await;
}

/// #3 — the existing-DB `squash()` path (`record_baseline`) writes S's `completed`
/// row AND its supersession edges ATOMICALLY (one transaction).
///
/// Discriminator for the old non-atomic `record_baseline` (a bare INSERT followed by
/// an autocommit edge loop, with no BEGIN around them): poison every insert into the
/// supersedes table. Apply v1..v3 normally, then `squash(S)`. The edge insert fails;
/// because `record_baseline` now brackets the row + edges in one `BEGIN…COMMIT`, the
/// completed row rolls back too: NO completed row for S and NO edges. (Old code:
/// S's completed row was already committed when the edge loop failed — partial-edge
/// corruption.)
#[compio::test]
async fn existing_db_squash_record_baseline_is_atomic() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let (m1, m2, m3, s) = three_plus_squash(&cfg.project_schema).await;
    apply(
        &conn,
        &cfg,
        &[m1.clone(), m2.clone(), m3.clone()],
        Approval::None,
        "ci",
    )
    .await
    .expect("apply v1..v3");

    // Poison every INSERT into the supersedes table so the edge write in
    // record_baseline fails.
    let trg_fn = format!("poison_sup_{tok}");
    conn.batch_execute(&format!(
        "CREATE OR REPLACE FUNCTION \"{meta}\".\"{trg_fn}\"() RETURNS trigger AS $fn$
         BEGIN RAISE EXCEPTION 'poison: supersedes insert blocked'; END;
         $fn$ LANGUAGE plpgsql;
         CREATE TRIGGER poison_sup_trg BEFORE INSERT ON \"{meta}\".schema_migrations_supersedes
             FOR EACH ROW EXECUTE FUNCTION \"{meta}\".\"{trg_fn}\"();",
        meta = cfg.pg.meta_schema
    ))
    .await
    .expect("install poison trigger");

    // squash() on the existing DB: the completed-row insert succeeds, the edge
    // insert raises → the whole record_baseline txn rolls back.
    let err = squash(&PostgresBackend::new(&conn), &cfg, &s, "operator")
        .await
        .expect_err("edge insert must fail and roll back record_baseline");
    assert!(
        matches!(err, SquashError::Journal(_)),
        "expected a Journal error from the edge insert, got {err:?}"
    );

    // ATOMICITY: neither the completed row nor any edge survived (same txn).
    assert_eq!(
        completed_count(&conn, &cfg.pg.meta_schema, s.version.as_str()).await,
        0,
        "S's completed row must NOT survive an edge-insert failure (record_baseline txn)"
    );
    assert_eq!(
        edge_count(&conn, &cfg.pg.meta_schema, s.version.as_str()).await,
        0,
        "no edges survive"
    );

    drop_schemas(&conn, &cfg).await;
}

/// #4 — `superseded_versions` only honors edges of a GENUINE squash
/// (`kind = 'squash'`). An edge whose `squash_version` is an ORDINARY net-applied
/// migration (`kind = 'apply'`) must NOT cause supersession.
///
/// Construct an applied ordinary migration `v_ord` (kind='apply'), an applied
/// genuine squash `S` (kind='squash') with a real edge `S → v_a`, plus a FORGED
/// edge `v_ord → v_b`. `superseded_versions` must return ONLY `v_a` (covered by the
/// genuine squash) and NOT `v_b`. Without the `kind = 'squash'` filter, `v_ord`
/// (net-applied-completed) was miscounted as a squash and `v_b` was over-superseded
/// — suppressing a real migration (the C1 forgery class).
#[compio::test]
async fn superseded_versions_ignores_edges_of_non_squash_versions() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("journal");

    let meta = &cfg.pg.meta_schema;
    let v_ord = MigrationId::generate();
    let s = MigrationId::generate();
    let v_a = MigrationId::generate();
    let v_b = MigrationId::generate();

    // v_ord: an ordinary net-applied migration (kind='apply').
    conn.execute(
        &format!(
            "INSERT INTO \"{meta}\".schema_migrations
                 (event_kind, version, name, checksum, \"by\", exec_ms, phase, outcome, kind)
             VALUES ('applied', $1, 'ord', 'c', 'ci', 0, 'completed', 'success', 'apply')"
        ),
        &[&v_ord.as_str()],
    )
    .await
    .expect("insert v_ord");
    // S: a genuine net-applied squash (kind='squash').
    conn.execute(
        &format!(
            "INSERT INTO \"{meta}\".schema_migrations
                 (event_kind, version, name, checksum, \"by\", exec_ms, phase, outcome, kind)
             VALUES ('applied', $1, 'squash', 'c', 'ci', 0, 'completed', 'success', 'squash')"
        ),
        &[&s.as_str()],
    )
    .await
    .expect("insert S");

    // Real edge: S → v_a. Forged edge: v_ord → v_b (v_ord is NOT a squash).
    for (sq, sup) in [(s.as_str(), v_a.as_str()), (v_ord.as_str(), v_b.as_str())] {
        conn.execute(
            &format!(
                "INSERT INTO \"{meta}\".schema_migrations_supersedes
                     (squash_version, superseded_version) VALUES ($1, $2)"
            ),
            &[&sq, &sup],
        )
        .await
        .expect("insert edge");
    }

    let superseded = apply::journal::superseded_versions(&conn, &cfg)
        .await
        .expect("superseded_versions");

    assert!(
        superseded.contains(&v_a.as_str().to_string()),
        "v_a (covered by the genuine squash S) must be superseded; got {superseded:?}"
    );
    assert!(
        !superseded.contains(&v_b.as_str().to_string()),
        "v_b must NOT be superseded — its edge's squash_version v_ord is an ordinary \
         migration, not a squash; got {superseded:?}"
    );

    drop_schemas(&conn, &cfg).await;
}
