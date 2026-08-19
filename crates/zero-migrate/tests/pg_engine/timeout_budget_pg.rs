//! A zero timeout budget can no longer disable the timeout it claims to set.
//!
//! PostgreSQL spells "no limit" as `0` for BOTH `statement_timeout` and
//! `lock_timeout`, so a per-migration `timeout_ms: 0` / `lock_timeout_ms: 0` does
//! not tighten the budget, it REMOVES it: the DDL then waits forever while
//! holding whatever it already acquired. `ExecutorConfig` documents both PG
//! timeouts as mandatory (no indefinite locks / DoS) and `MigrationFlags`
//! documents the per-migration override as raising a FINITE budget, so a zero is
//! already outside the contract the engine states for itself.
//!
//! The refusal is enforced where the effective value is computed and handed to
//! the session render, because that is the only point every path converges on:
//! the IR load gate alone is not enough, since `Migration`/`MigrationFlags` are
//! public serde structs an embedder can build and hand straight to `apply`, and a
//! zero can also arrive from CONFIG (`Duration::from_micros(500).as_millis()`
//! truncates to 0) with no IR involved. This suite drives all of it through the
//! REAL apply path against a REAL server.
//!
//! It refuses rather than clamps: `lock_timeout_ms` is folded into the migration
//! checksum, so silently substituting a different budget would run a migration
//! whose behaviour no longer matches its checksummed identity.
//!
//! GATED behind `ZERO_MIGRATE_TEST_PG_URL`; skips cleanly when unset.

use crate::support;

use std::time::Duration;

use crate::support::PgDevSession;

use zero_migrate::model::migration::Checksum;
use zero_migrate::{
    apply, Approval, ExecutorConfig, Migration, MigrationFlags, MigrationId, Phase,
};

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    format!("{pid}_{nanos}_{n}")
}

fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(
        format!("prj_{tok}"),
        format!("proj_{tok}"),
        support::no_inject(&format!("proj_{tok}")),
    );
    c.pg.meta_schema = format!("meta_{tok}");
    c
}

/// Create the project schema and hand back the guard that removes it, and the meta
/// schema apply creates later, when the test leaves scope. The DROP rides the guard
/// rather than a trailing statement so a failing assertion cannot abandon them.
#[must_use = "the guard drops the schemas when it falls out of scope"]
async fn ensure_project_schema<'a>(
    session: &'a PgDevSession,
    cfg: &ExecutorConfig,
) -> support::SchemaGuard<'a> {
    use zero_migrate::driver::SqlSession;
    let guard = support::SchemaGuard::arm(
        session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    session
        .batch(&format!(
            "CREATE SCHEMA IF NOT EXISTS \"{}\"",
            cfg.project_schema
        ))
        .await
        .expect("create project schema");
    guard
}

async fn drop_schemas(session: &PgDevSession, cfg: &ExecutorConfig) {
    use zero_migrate::driver::SqlSession;
    let _ = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.pg.meta_schema
        ))
        .await;
}

/// Whether the live catalog holds `schema.table` -- the evidence that a refused
/// migration really did not run its `up`.
async fn table_exists(session: &PgDevSession, schema: &str, table: &str) -> bool {
    use zero_migrate::driver::SqlSession;
    session
        .query(
            "SELECT to_regclass($1) IS NOT NULL AS present",
            &[format!("\"{schema}\".\"{table}\"").as_str().into()],
        )
        .await
        .expect("probe to_regclass")
        .first()
        .map(|row| row.try_get::<_, bool>("present").expect("decode present"))
        .unwrap_or(false)
}

/// A transactional migration carrying `flags`, checksummed over those same flags --
/// exactly what an embedder building `Migration` by hand would hand to `apply`.
fn mig_with_flags(name: &str, up: &str, flags: MigrationFlags) -> Migration {
    Migration {
        version: MigrationId::generate(),
        name: name.to_string(),
        up: up.to_string(),
        down: None,
        checksum: Checksum::of(&zero_migrate::ChecksumInput {
            up,
            down: None,
            flags: &flags,
            owner_app: "app_test",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        }),
        flags,
        owner_app: "app_test".to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        existence_guard: None,
        effect: None,
    }
}

/// The premise, read back from the server rather than asserted from memory: `0`
/// is PostgreSQL's spelling for "no limit" on both budgets, so a zero override
/// disables the very timeout it claims to set.
#[compio::test]
async fn postgres_reads_a_zero_timeout_as_no_limit_on_both_budgets() {
    use zero_migrate::driver::SqlSession;

    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);

    for setting in ["statement_timeout", "lock_timeout"] {
        session
            .batch(&format!("SET {setting} = 0"))
            .await
            .expect("set the budget to zero");
        let shown = session
            .query(&format!("SHOW {setting}"), &[])
            .await
            .expect("read the budget back")
            .first()
            .expect("SHOW returns a row")
            .try_get::<_, String>(0)
            .expect("decode the budget");
        assert_eq!(
            shown, "0",
            "{setting} = 0 must read back as the no-limit sentinel, not a 0ms budget"
        );

        session
            .batch(&format!("SET {setting} = 1"))
            .await
            .expect("set the budget to one millisecond");
        let shown = session
            .query(&format!("SHOW {setting}"), &[])
            .await
            .expect("read the budget back")
            .first()
            .expect("SHOW returns a row")
            .try_get::<_, String>(0)
            .expect("decode the budget");
        assert_eq!(
            shown, "1ms",
            "a genuinely tiny {setting} budget reads back in milliseconds"
        );
    }
}

/// A per-migration `lock_timeout_ms: 0` is refused, and its `up` never runs.
#[compio::test]
async fn a_zero_lock_timeout_override_is_refused_before_any_ddl_runs() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    let _schemas = ensure_project_schema(&session, &cfg).await;

    let flags = MigrationFlags {
        lock_timeout_ms: Some(0),
        ..MigrationFlags::default()
    };
    let m = mig_with_flags(
        "zero_lock_budget",
        "CREATE TABLE zero_lock (id text)",
        flags,
    );

    let err = apply(&session, &cfg, &[m], Approval::Approved, "app_test")
        .await
        .expect_err("a lock_timeout budget of 0 disables the lock timeout and must be refused")
        .to_string();
    assert!(
        err.contains("lock_timeout = 0") && err.contains("flags.lock_timeout_ms"),
        "the refusal must name the zero lock_timeout budget and the knob that set it: {err}"
    );

    assert!(
        !table_exists(&session, &cfg.project_schema, "zero_lock").await,
        "the refused migration must not have run its `up`"
    );

    drop_schemas(&session, &cfg).await;
}

/// A per-migration `timeout_ms: 0` is refused, and its `up` never runs. The
/// statement-timeout override is spelled `timeout_ms`, not `statement_timeout_ms`.
#[compio::test]
async fn a_zero_statement_timeout_override_is_refused_before_any_ddl_runs() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    let _schemas = ensure_project_schema(&session, &cfg).await;

    let flags = MigrationFlags {
        timeout_ms: Some(0),
        ..MigrationFlags::default()
    };
    let m = mig_with_flags(
        "zero_statement_budget",
        "CREATE TABLE zero_statement (id text)",
        flags,
    );

    let err = apply(&session, &cfg, &[m], Approval::Approved, "app_test")
        .await
        .expect_err("a statement_timeout budget of 0 disables the statement timeout")
        .to_string();
    assert!(
        err.contains("statement_timeout = 0") && err.contains("flags.timeout_ms"),
        "the refusal must name the zero statement_timeout budget and the knob that set it: {err}"
    );

    assert!(
        !table_exists(&session, &cfg.project_schema, "zero_statement").await,
        "the refused migration must not have run its `up`"
    );

    drop_schemas(&session, &cfg).await;
}

/// The zero can come from CONFIG with no migration flag and no IR in sight: a
/// sub-millisecond `Duration` truncates to 0 whole milliseconds, which is the
/// same no-limit sentinel. No IR-level validation can see this one.
#[compio::test]
async fn a_config_timeout_that_truncates_to_zero_milliseconds_is_refused() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let mut cfg = cfg_for(&tok);
    cfg.pg.lock_timeout = Duration::from_micros(500);
    assert_eq!(
        cfg.lock_timeout_ms(),
        0,
        "a sub-millisecond config budget truncates to the no-limit sentinel"
    );
    drop_schemas(&session, &cfg).await;
    let _schemas = ensure_project_schema(&session, &cfg).await;

    let m = mig_with_flags(
        "config_zero_budget",
        "CREATE TABLE config_zero (id text)",
        MigrationFlags::default(),
    );

    let err = apply(&session, &cfg, &[m], Approval::Approved, "app_test")
        .await
        .expect_err("a config lock_timeout that truncates to 0ms must be refused")
        .to_string();
    assert!(
        err.contains("lock_timeout = 0") && err.contains("pg.lock_timeout"),
        "the refusal must point at the config field, not at a migration flag: {err}"
    );

    assert!(
        !table_exists(&session, &cfg.project_schema, "config_zero").await,
        "the refused migration must not have run its `up`"
    );

    drop_schemas(&session, &cfg).await;
}

/// The non-transactional two-phase path installs its budgets through a different
/// helper than the transactional one, so it is refused on its own evidence.
#[compio::test]
async fn a_zero_budget_on_the_non_transactional_path_is_refused() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    let _schemas = ensure_project_schema(&session, &cfg).await;

    // The table the concurrent index needs; applied on the ordinary path.
    let setup = mig_with_flags(
        "non_txn_setup",
        "CREATE TABLE non_txn (id text)",
        MigrationFlags::default(),
    );
    apply(&session, &cfg, &[setup], Approval::Approved, "app_test")
        .await
        .expect("the setup table applies");

    let flags = MigrationFlags {
        transactional: false,
        lock_timeout_ms: Some(0),
        ..MigrationFlags::default()
    };
    let m = mig_with_flags(
        "non_txn_zero_budget",
        "CREATE INDEX CONCURRENTLY IF NOT EXISTS ix_non_txn ON non_txn (id)",
        flags,
    );

    let err = apply(&session, &cfg, &[m], Approval::Approved, "app_test")
        .await
        .expect_err("the non-transactional path must refuse a zero lock_timeout budget too")
        .to_string();
    assert!(
        err.contains("lock_timeout = 0") && err.contains("flags.lock_timeout_ms"),
        "the refusal must name the zero lock_timeout budget and the knob that set it: {err}"
    );

    drop_schemas(&session, &cfg).await;
}

/// The positive control. A finite override on BOTH budgets still applies: the
/// migration runs, the table lands, and the journal records the completion. A
/// refusal test with no positive control proves only that something errored.
#[compio::test]
async fn finite_timeout_overrides_still_apply() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    let _schemas = ensure_project_schema(&session, &cfg).await;

    let flags = MigrationFlags {
        timeout_ms: Some(45_000),
        lock_timeout_ms: Some(7_000),
        ..MigrationFlags::default()
    };
    let m = mig_with_flags(
        "finite_budgets",
        "CREATE TABLE finite_budgets (id text)",
        flags,
    );

    let out = apply(&session, &cfg, &[m], Approval::Approved, "app_test")
        .await
        .expect("a finite maintenance-window override still applies");
    assert_eq!(
        out.applied.len(),
        1,
        "apply reports the migration applied: {out:?}"
    );

    assert!(
        table_exists(&session, &cfg.project_schema, "finite_budgets").await,
        "the applied migration really created its table"
    );

    let journal = zero_migrate::applied(&session, &cfg)
        .await
        .expect("read the journal");
    assert!(
        journal
            .iter()
            .any(|e| e.version == out.applied[0] && e.phase == Phase::Completed),
        "the journal records the completion: {journal:?}"
    );

    drop_schemas(&session, &cfg).await;
}

/// The positive control for the smallest budget PostgreSQL can still express: 1ms
/// is a real, finite limit, so it must apply rather than be swept up by the
/// zero refusal.
#[compio::test]
async fn a_one_millisecond_budget_is_finite_and_still_applies() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    let _schemas = ensure_project_schema(&session, &cfg).await;

    let flags = MigrationFlags {
        lock_timeout_ms: Some(1),
        ..MigrationFlags::default()
    };
    let m = mig_with_flags("one_ms_budget", "CREATE TABLE one_ms (id text)", flags);

    apply(&session, &cfg, &[m], Approval::Approved, "app_test")
        .await
        .expect("a 1ms lock budget is finite and applies on an uncontended table");
    assert!(
        table_exists(&session, &cfg.project_schema, "one_ms").await,
        "the applied migration really created its table"
    );

    drop_schemas(&session, &cfg).await;
}
