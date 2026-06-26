//! Apply-time **lock-safety envelope** — faithful e2e on REAL Postgres (`:5440`).
//!
//! The migration engine pins a SHORT, SEPARATE `lock_timeout` (default 3s) on
//! every DDL transaction, distinct from the long-running `statement_timeout`
//! (60s). This is the standard safe-migration pattern (`strong_migrations` /
//! Atlas PG101 & PG103): a blocking DDL on a populated, live multi-tenant table
//! takes an `ACCESS EXCLUSIVE` lock, and if any other transaction holds a
//! conflicting lock the DDL queues behind it — AND every subsequent query on
//! that table then queues behind the DDL. A long lock-acquisition wait is a
//! tenant-wide outage; a SHORT one makes the DDL FAIL FAST (`55P03
//! lock_not_available`), roll back cleanly (the two-phase recovery handles the
//! abort — a lock-timeout failure is retryable, never data-corrupting), and free
//! the table immediately.
//!
//! These tests drive the REAL apply path (`MigrationEngine::apply_plan` →
//! executor txn → `SET LOCAL lock_timeout` → DDL), not a render shim:
//!
//! - `blocking_ddl_fails_fast_when_conflicting_lock_held` — a SECOND connection
//!   holds a conflicting `ACCESS SHARE` lock in an open transaction; an
//!   `ALTER TABLE … ADD COLUMN` (which needs `ACCESS EXCLUSIVE`) run through the
//!   apply path FAILS FAST with `55P03` in well under the `statement_timeout` — NOT
//!   a long hang — and journals NOTHING (the txn rolled back).
//! - `same_ddl_succeeds_when_no_conflicting_lock` — the identical DDL applies
//!   cleanly once the lock is released (the failure was the lock, not the DDL).
//!
//! The fast-fail bound is asserted against a per-test `lock_timeout` set SHORT
//! (1s) with the `statement_timeout` left LONG (60s): a fold of the two together
//! (the bug this envelope prevents) would make the DDL wait the full
//! `statement_timeout` before failing — so the elapsed-time assertion is RED if the
//! lock budget were not split out.

use std::time::{Duration, Instant};

use compio_postgres::Client;
use zeroship_migrate::{
    migration::{Checksum, ChecksumInput, Migration, MigrationFlags},
    plan::PlanStep,
    provision_migrator,
    role::deprovision_migrator,
    Approval, ExecutorConfig, MigrationEngine,
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

/// A config whose `lock_timeout` is SHORT (1s) but `statement_timeout` stays
/// LONG (60s) — so a fast-fail is unambiguously the LOCK budget, not the
/// statement budget. If the two were folded, the DDL would wait 60s.
fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(format!("prj_{tok}"), format!("proj_{tok}"));
    c.pg.meta_schema = format!("meta_{tok}");
    c.pg.statement_timeout = Duration::from_secs(60);
    c.pg.lock_timeout = Duration::from_secs(1);
    let role = zeroship_migrate::migrator_role_name(&c.project_id).unwrap();
    c.with_migrator_role(role)
}

/// Like [`cfg_for`] but with a LONG executor-wide `lock_timeout` (60s). Used to
/// prove the PER-MIGRATION `lock_timeout_ms` override is what fires: with the
/// executor default long, a fast-fail can ONLY come from a migration that set
/// its own short `flags.lock_timeout_ms`.
fn cfg_long_lock(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(format!("prj_{tok}"), format!("proj_{tok}"));
    c.pg.meta_schema = format!("meta_{tok}");
    c.pg.statement_timeout = Duration::from_secs(60);
    c.pg.lock_timeout = Duration::from_secs(60);
    let role = zeroship_migrate::migrator_role_name(&c.project_id).unwrap();
    c.with_migrator_role(role)
}

async fn setup(conn: &Client, cfg: &ExecutorConfig) {
    conn.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS \"{}\"",
        cfg.project_schema
    ))
    .await
    .expect("create project schema");
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

fn q(schema: &str) -> String {
    format!("\"{}\"", schema.replace('"', "\"\""))
}

fn ddl(version: u64, name: &str, up: &str, down: Option<&str>) -> Migration {
    ddl_with_flags(version, name, up, down, MigrationFlags::default())
}

/// Build a migration carrying explicit [`MigrationFlags`] (so a test can set the
/// per-migration `lock_timeout_ms` maintenance-window override). The checksum is
/// computed over the SAME flags it carries, so the migration verifies clean.
fn ddl_with_flags(
    version: u64,
    name: &str,
    up: &str,
    down: Option<&str>,
    flags: MigrationFlags,
) -> Migration {
    let checksum = Checksum::of(&ChecksumInput {
        up,
        down,
        flags: &flags,
        owner_app: "app_test",
        depends_on: &[],
        supersedes: &[],
        preconditions: &[],
    });
    Migration {
        version: zeroship_migrate::migration_id_for_version(version),
        name: name.to_string(),
        up: up.to_string(),
        down: down.map(str::to_string),
        checksum,
        flags,
        owner_app: "app_test".to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        existence_guard: None,
    }
}

async fn journaled_completed(conn: &Client, cfg: &ExecutorConfig, version: &str) -> bool {
    let rows = conn
        .query(
            &format!(
                "SELECT 1 FROM \"{}\".schema_migrations \
                 WHERE version = $1 AND phase = 'completed'",
                cfg.pg.meta_schema
            ),
            &[&version],
        )
        .await
        .expect("query journal");
    !rows.is_empty()
}

async fn column_exists(conn: &Client, schema: &str, table: &str, column: &str) -> bool {
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 AND column_name = $3",
            &[&schema, &table, &column],
        )
        .await
        .expect("query column existence");
    !rows.is_empty()
}

/// Walk an error's `source()` chain and report whether ANY link is a
/// `compio_postgres::Error` carrying SQLSTATE `55P03 lock_not_available`.
fn chain_has_lock_not_available(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        if let Some(pg) = e.downcast_ref::<compio_postgres::Error>() {
            if pg.code() == Some(&compio_postgres::error::SqlState::LOCK_NOT_AVAILABLE) {
                return true;
            }
        }
        cur = e.source();
    }
    false
}

// ---------------------------------------------------------------------------
// (1) blocking DDL behind a conflicting lock FAILS FAST with 55P03
// ---------------------------------------------------------------------------

#[compio::test]
async fn blocking_ddl_fails_fast_when_conflicting_lock_held() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let s = q(&cfg.project_schema);
    let engine = MigrationEngine::new();

    // A "live" tenant table, created THROUGH a migration so the migrator role
    // OWNS it (and may later ALTER it — the apply runs under `SET LOCAL ROLE
    // migrator`, which can only alter tables it owns). Then seed it as admin so
    // it is populated, like a real tenant table.
    let create = ddl(
        1,
        "create_live",
        &format!("CREATE TABLE {s}.live (id bigint PRIMARY KEY)"),
        Some(&format!("DROP TABLE {s}.live")),
    );
    engine
        .apply_plan(
            &[PlanStep::Ddl(create)],
            Approval::None,
            &zeroship_migrate::PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("create live table via migration (migrator owns it)");
    conn.batch_execute(&format!(
        "INSERT INTO {s}.live (id) SELECT g FROM generate_series(1, 1000) g;"
    ))
    .await
    .expect("seed live table");

    // A SECOND connection holds a CONFLICTING lock in an OPEN transaction.
    // `ACCESS SHARE` conflicts with the `ACCESS EXCLUSIVE` an `ALTER TABLE …
    // ADD COLUMN` must take — so the DDL cannot acquire its lock while this is
    // held. We keep the txn open (never COMMIT) for the duration of the apply.
    let blocker = pg().await;
    blocker
        .batch_execute(&format!(
            "BEGIN; LOCK TABLE {s}.live IN ACCESS SHARE MODE;"
        ))
        .await
        .expect("blocker takes ACCESS SHARE lock");

    let m = ddl(
        2,
        "add_col_to_live",
        &format!("ALTER TABLE {s}.live ADD COLUMN note text"),
        Some(&format!("ALTER TABLE {s}.live DROP COLUMN note")),
    );
    let v = m.version.as_str().to_string();
    let steps = vec![PlanStep::Ddl(m)];

    let started = Instant::now();
    let res = engine
        .apply_plan(
            &steps,
            Approval::None,
            &zeroship_migrate::PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await;
    let elapsed = started.elapsed();

    // Release the blocker (regardless of outcome) so teardown is clean.
    let _ = blocker.batch_execute("COMMIT").await;

    // (a) It FAILED — the lock could not be acquired.
    let err = res.expect_err("blocking DDL must fail while a conflicting lock is held");

    // (b) It failed with 55P03 lock_not_available — the lock_timeout fired,
    //     NOT statement_timeout, NOT some unrelated error.
    assert!(
        chain_has_lock_not_available(&err as &(dyn std::error::Error + 'static)),
        "expected SQLSTATE 55P03 (lock_not_available) in the error chain, got: {err:?}"
    );

    // (c) It failed FAST — within a small multiple of the 1s lock_timeout, and
    //     FAR below the 60s statement_timeout. A fold of lock_timeout into
    //     statement_timeout would make this ~60s (this is the RED-pre-change
    //     assertion that pins the SPLIT).
    assert!(
        elapsed < Duration::from_secs(15),
        "blocking DDL must fail FAST (~lock_timeout=1s), not hang near \
         statement_timeout=60s; took {elapsed:?}"
    );

    // (d) Nothing was journaled and the column never landed — the txn rolled
    //     back cleanly (two-phase recovery handles the abort).
    assert!(
        !journaled_completed(&conn, &cfg, &v).await,
        "a lock-timeout-aborted migration must NOT be journaled completed"
    );
    assert!(
        !column_exists(&conn, &cfg.project_schema, "live", "note").await,
        "the aborted DDL must not have added the column"
    );

    teardown(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// (2) the SAME DDL succeeds when no conflicting lock is held
// ---------------------------------------------------------------------------

#[compio::test]
async fn same_ddl_succeeds_when_no_conflicting_lock() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let s = q(&cfg.project_schema);
    let engine = MigrationEngine::new();

    // Same setup: create `live` through a migration (migrator owns it), seed it.
    let create = ddl(
        1,
        "create_live",
        &format!("CREATE TABLE {s}.live (id bigint PRIMARY KEY)"),
        Some(&format!("DROP TABLE {s}.live")),
    );
    engine
        .apply_plan(
            &[PlanStep::Ddl(create)],
            Approval::None,
            &zeroship_migrate::PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("create live table via migration (migrator owns it)");
    conn.batch_execute(&format!(
        "INSERT INTO {s}.live (id) SELECT g FROM generate_series(1, 1000) g;"
    ))
    .await
    .expect("seed live table");

    // No blocker this time — the lock is free, so the identical DDL applies.
    let m = ddl(
        2,
        "add_col_to_live",
        &format!("ALTER TABLE {s}.live ADD COLUMN note text"),
        Some(&format!("ALTER TABLE {s}.live DROP COLUMN note")),
    );
    let v = m.version.as_str().to_string();
    let steps = vec![PlanStep::Ddl(m)];

    let out = engine
        .apply_plan(
            &steps,
            Approval::None,
            &zeroship_migrate::PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("DDL applies cleanly with no conflicting lock");

    assert_eq!(out.applied.applied, vec![v.clone()]);
    assert!(
        column_exists(&conn, &cfg.project_schema, "live", "note").await,
        "the column must land when the lock is free"
    );
    assert!(
        journaled_completed(&conn, &cfg, &v).await,
        "the successful migration must be journaled completed"
    );

    teardown(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// (3) the PER-MIGRATION lock_timeout_ms override is the budget that fires
// ---------------------------------------------------------------------------

/// The maintenance-window override seam: a single migration sets its OWN short
/// `flags.lock_timeout_ms` while the executor-wide default is LONG (60s). A
/// blocking DDL behind a conflicting lock must fail fast on the MIGRATION's
/// budget (~1s), NOT the 60s executor default.
///
/// RED pre-change: before the `lock_timeout_ms` override seam existed,
/// `set_local_session_sql` emitted `cfg.lock_timeout_ms()` (60s) unconditionally,
/// so the DDL would wait ~60s and the `< 15s` elapsed assertion would fail (and
/// the `MigrationFlags { lock_timeout_ms: .. }` literal would not even compile).
#[compio::test]
async fn per_migration_lock_timeout_override_fires_over_long_default() {
    let conn = pg().await;
    let cfg = cfg_long_lock(&token());
    setup(&conn, &cfg).await;
    let s = q(&cfg.project_schema);
    let engine = MigrationEngine::new();

    let create = ddl(
        1,
        "create_live",
        &format!("CREATE TABLE {s}.live (id bigint PRIMARY KEY)"),
        Some(&format!("DROP TABLE {s}.live")),
    );
    engine
        .apply_plan(
            &[PlanStep::Ddl(create)],
            Approval::None,
            &zeroship_migrate::PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("create live table via migration (migrator owns it)");
    conn.batch_execute(&format!(
        "INSERT INTO {s}.live (id) SELECT g FROM generate_series(1, 1000) g;"
    ))
    .await
    .expect("seed live table");

    // Conflicting ACCESS SHARE lock held open on a second connection.
    let blocker = pg().await;
    blocker
        .batch_execute(&format!(
            "BEGIN; LOCK TABLE {s}.live IN ACCESS SHARE MODE;"
        ))
        .await
        .expect("blocker takes ACCESS SHARE lock");

    // The migration sets its OWN short lock budget (1s); the executor-wide
    // default is 60s. If the override is honoured, this fails fast (~1s); if it
    // is ignored, it waits the 60s default.
    let flags = MigrationFlags { lock_timeout_ms: Some(1000), ..MigrationFlags::default() };
    let m = ddl_with_flags(
        2,
        "add_col_to_live",
        &format!("ALTER TABLE {s}.live ADD COLUMN note text"),
        Some(&format!("ALTER TABLE {s}.live DROP COLUMN note")),
        flags,
    );
    let v = m.version.as_str().to_string();
    let steps = vec![PlanStep::Ddl(m)];

    let started = Instant::now();
    let res = engine
        .apply_plan(
            &steps,
            Approval::None,
            &zeroship_migrate::PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await;
    let elapsed = started.elapsed();
    let _ = blocker.batch_execute("COMMIT").await;

    let err = res.expect_err("blocking DDL must fail while a conflicting lock is held");
    assert!(
        chain_has_lock_not_available(&err as &(dyn std::error::Error + 'static)),
        "expected SQLSTATE 55P03 (lock_not_available) in the error chain, got: {err:?}"
    );
    // The fast-fail came from the PER-MIGRATION 1s budget, NOT the 60s executor
    // default — this is the RED-pre-change assertion that pins the override seam.
    assert!(
        elapsed < Duration::from_secs(15),
        "the per-migration lock_timeout_ms override (1s) must fire, not the 60s \
         executor default; took {elapsed:?}"
    );
    assert!(
        !journaled_completed(&conn, &cfg, &v).await,
        "a lock-timeout-aborted migration must NOT be journaled completed"
    );
    assert!(
        !column_exists(&conn, &cfg.project_schema, "live", "note").await,
        "the aborted DDL must not have added the column"
    );

    teardown(&conn, &cfg).await;
}
