//! Line-2 (DB-privilege) defense — the least-privilege per-project `migrator`
//! role (design §1.3 / §1.7). Faithful tests against a REAL Postgres (no shims).
//!
//! These are **the whole point** of Plan 3: the SQL guard (line 1) denies the
//! dangerous surface at parse time, but a parser can be evaded by
//! runtime-constructed SQL. Line 2 backstops it — the migration runs under a
//! role with no grants outside its own schema, so cross-tenant / priv-esc ops
//! fail with `permission denied` **at execution** even when they slip past
//! parse.
//!
//! Requires the dedicated database `zeroship_migrate_test` on :5440 and a
//! connecting role with `CREATEROLE` (the runbook uses `postgres`). Set
//! `MIGRATE_TEST_DB` to override the DSN. Each test uses uniquely-suffixed
//! schemas + a uniquely-suffixed project id (→ a unique role name), so tests
//! are isolated and re-runnable in the shared DB.

use compio_postgres::Client;
use zeroship_migrate::{
    apply, ensure_journal, migrator_role_name, provision_migrator, role::deprovision_migrator,
    Approval, Checksum, ExecutorConfig, Migration, MigrationFlags, MigrationId,
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
    let role = migrator_role_name(&c.project_id).unwrap();
    c.with_migrator_role(role)
}

async fn ensure_project_schema(conn: &Client, cfg: &ExecutorConfig) {
    conn.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS \"{}\"",
        cfg.project_schema
    ))
    .await
    .expect("create project schema");
}

/// Full teardown: drop the role (after reassigning/dropping its owned objects)
/// and the schemas. Role drop must precede schema drop's CASCADE only loosely —
/// `deprovision_migrator` reassigns owned objects first, so order is safe either
/// way; we deprovision then drop schemas.
async fn teardown(conn: &Client, cfg: &ExecutorConfig) {
    let _ = deprovision_migrator(conn, cfg).await;
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.meta_schema
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
    }
}

/// Idempotently stand up a shared `control` stand-in schema (a real platform
/// schema name, so the guard's body heuristics treat it as cross-tenant) + a
/// sensitive table. NEVER dropped — it is shared across the parallel tests that
/// assert denial against `control`, so dropping it would race. `control.evil`
/// (the forbidden target) is never created by any test (always denied), so its
/// absence is a clean post-condition regardless of test interleaving. Tolerates
/// concurrent creation (`IF NOT EXISTS` + ignore the catalog-race error).
async fn ensure_control_standin(conn: &Client) {
    for _ in 0..8 {
        match conn
            .batch_execute(
                "CREATE SCHEMA IF NOT EXISTS control; \
                 CREATE TABLE IF NOT EXISTS control.creator_billing (id int, secret text);",
            )
            .await
        {
            Ok(()) => return,
            // Concurrent CREATE IF NOT EXISTS of the same shared schema/table can
            // raise a transient duplicate / catalog-race; retry briefly.
            Err(e) => {
                let msg = e.as_db_error().map(|d| d.message().to_string()).unwrap_or_default();
                if msg.contains("concurrently")
                    || msg.contains("already exists")
                    || msg.contains("duplicate key")
                {
                    compio::time::sleep(std::time::Duration::from_millis(20)).await;
                    continue;
                }
                panic!("seed control schema: {e}");
            }
        }
    }
}

async fn role_exists(conn: &Client, role: &str) -> bool {
    let rows = conn
        .query("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&role])
        .await
        .expect("query pg_roles");
    !rows.is_empty()
}

async fn table_exists(conn: &Client, schema: &str, table: &str) -> bool {
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.tables WHERE table_schema = $1 AND table_name = $2",
            &[&schema, &table],
        )
        .await
        .expect("query table existence");
    !rows.is_empty()
}

/// Run `sql` on `conn` AS the migrator role (SET ROLE … then RESET ROLE),
/// returning the result. The RESET ROLE always runs so a denied op does not
/// leave the test session stuck as the migrator.
async fn as_migrator(conn: &Client, role: &str, sql: &str) -> Result<(), compio_postgres::Error> {
    conn.batch_execute(&format!("SET ROLE \"{}\"", role.replace('"', "\"\"")))
        .await
        .expect("SET ROLE migrator");
    let r = conn.batch_execute(sql).await;
    conn.batch_execute("RESET ROLE")
        .await
        .expect("RESET ROLE back to admin");
    r
}

/// Assert a DB error is `insufficient_privilege` (SQLSTATE 42501) — the
/// signature of line-2 confinement. The opaque `Error` Display is just
/// "db error"; the real signal is the SQLSTATE on the underlying `DbError`.
fn assert_permission_denied(err: &compio_postgres::Error, ctx: &str) {
    let code = err.code();
    let detail = err
        .as_db_error()
        .map_or_else(|| err.to_string(), ToString::to_string);
    assert_eq!(
        code,
        Some(&compio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE),
        "{ctx}: expected SQLSTATE 42501 (insufficient_privilege), got code={code:?} detail={detail}"
    );
}

// ===========================================================================
// Provisioning
// ===========================================================================

#[compio::test]
async fn provision_creates_locked_down_role_and_is_idempotent() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    let role = cfg.migrator_role.clone().unwrap();
    teardown(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("journal");

    // Run twice — idempotent.
    provision_migrator(&conn, &cfg).await.expect("provision 1");
    provision_migrator(&conn, &cfg).await.expect("provision 2 (idempotent)");

    assert!(role_exists(&conn, &role).await, "role must exist");

    // Attribute set is locked down.
    let row = conn
        .query_one(
            "SELECT rolsuper, rolcreaterole, rolcreatedb, rolcanlogin, rolbypassrls \
               FROM pg_roles WHERE rolname = $1",
            &[&role],
        )
        .await
        .expect("read role attrs");
    assert!(!row.get::<_, bool>("rolsuper"), "NOSUPERUSER");
    assert!(!row.get::<_, bool>("rolcreaterole"), "NOCREATEROLE");
    assert!(!row.get::<_, bool>("rolcreatedb"), "NOCREATEDB");
    assert!(!row.get::<_, bool>("rolcanlogin"), "NOLOGIN");
    assert!(!row.get::<_, bool>("rolbypassrls"), "NOBYPASSRLS");

    // Owns the project schema (so its DDL works) but NOT the meta schema (so it
    // can never drop the journal's immutability trigger).
    let proj_owner: String = conn
        .query_one(
            "SELECT pg_get_userbyid(nspowner) AS o FROM pg_namespace WHERE nspname = $1",
            &[&cfg.project_schema],
        )
        .await
        .expect("proj owner")
        .get("o");
    assert_eq!(proj_owner, role, "migrator owns the project schema");
    let meta_owner: String = conn
        .query_one(
            "SELECT pg_get_userbyid(nspowner) AS o FROM pg_namespace WHERE nspname = $1",
            &[&cfg.meta_schema],
        )
        .await
        .expect("meta owner")
        .get("o");
    assert_ne!(meta_owner, role, "migrator must NOT own the meta schema");

    // C1: the migrator has NO access to the meta schema at all — neither USAGE on
    // the schema nor any privilege on the journal tables. The journal is
    // unforgeable by the migration because the grant is absent (deny-by-absence).
    let has_usage: bool = conn
        .query_one(
            "SELECT has_schema_privilege($1, $2, 'USAGE') AS p",
            &[&role, &cfg.meta_schema],
        )
        .await
        .expect("has_schema_privilege")
        .get("p");
    assert!(!has_usage, "migrator must NOT have USAGE on the meta schema");

    for (verb, ok) in [("INSERT", false), ("SELECT", false), ("UPDATE", false)] {
        let priv_str = format!("{}.schema_migrations", cfg.meta_schema);
        let has: bool = conn
            .query_one(
                "SELECT has_table_privilege($1, $2, $3) AS p",
                &[&role, &priv_str, &verb],
            )
            .await
            .expect("has_table_privilege")
            .get("p");
        assert_eq!(
            has, ok,
            "migrator {verb} on schema_migrations must be {ok} (journal unforgeable)"
        );
    }
    let inflight_str = format!("{}.schema_migrations_inflight", cfg.meta_schema);
    for verb in ["INSERT", "DELETE", "SELECT"] {
        let has: bool = conn
            .query_one(
                "SELECT has_table_privilege($1, $2, $3) AS p",
                &[&role, &inflight_str, &verb],
            )
            .await
            .expect("has_table_privilege inflight")
            .get("p");
        assert!(
            !has,
            "migrator {verb} on schema_migrations_inflight must be denied (journal unforgeable)"
        );
    }

    // The migrator's search_path is the PROJECT schema only — the meta schema is
    // off the migration-time path (defense-in-depth).
    let sp: String = conn
        .query_one(
            "SELECT setconfig[1] AS sp FROM pg_db_role_setting s \
               JOIN pg_roles r ON r.oid = s.setrole \
              WHERE r.rolname = $1 \
                AND setconfig[1] LIKE 'search_path=%' LIMIT 1",
            &[&role],
        )
        .await
        .map_or_else(|_| String::new(), |row| row.get("sp"));
    assert!(
        !sp.contains(&cfg.meta_schema),
        "migrator search_path must not include the meta schema (got {sp:?})"
    );

    teardown(&conn, &cfg).await;
    assert!(!role_exists(&conn, &role).await, "deprovision drops the role");
}

// ===========================================================================
// Happy path: a normal migration applies under the migrator role
// ===========================================================================

#[compio::test]
async fn migrator_can_ddl_its_own_schema_and_apply_runs_under_the_role() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    teardown(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("journal");
    provision_migrator(&conn, &cfg).await.expect("provision");

    // Direct: as the migrator, CREATE TABLE in its own schema succeeds.
    as_migrator(
        &conn,
        cfg.migrator_role.as_ref().unwrap(),
        &format!("CREATE TABLE \"{}\".direct_ok (x int)", cfg.project_schema),
    )
    .await
    .expect("migrator can CREATE TABLE in its own schema");

    // Through the executor: a normal migration applies (DDL + journal under the
    // migrator via SET LOCAL ROLE), and the table lands in the project schema.
    let m = mig(
        MigrationId::generate(),
        "create_t",
        "CREATE TABLE t (id bigint PRIMARY KEY, name text NOT NULL)",
    );
    let out = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "actor")
        .await
        .expect("apply under migrator role");
    assert_eq!(out.applied.len(), 1, "one migration applied");
    assert!(
        table_exists(&conn, &cfg.project_schema, "t").await,
        "migration table created in project schema"
    );
    // Journal recorded the completed row.
    let n: i64 = conn
        .query_one(
            &format!(
                "SELECT count(*)::bigint AS c FROM \"{}\".schema_migrations",
                cfg.meta_schema
            ),
            &[],
        )
        .await
        .expect("journal count")
        .get("c");
    assert_eq!(n, 1, "completed row journaled");

    // The session must be back on the admin role after apply (H2 — no leak).
    let cur: String = conn
        .query_one("SELECT current_user AS u", &[])
        .await
        .expect("current_user")
        .get("u");
    assert_ne!(
        cur,
        *cfg.migrator_role.as_ref().unwrap(),
        "apply must RESET ROLE — the migrator role must not leak onto the session"
    );

    teardown(&conn, &cfg).await;
}

// ===========================================================================
// Cross-tenant / control-schema denial (DB-enforced)
// ===========================================================================

#[compio::test]
async fn migrator_cannot_reach_control_schema() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    let role = cfg.migrator_role.clone().unwrap();
    teardown(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("journal");
    provision_migrator(&conn, &cfg).await.expect("provision");

    // Stand up the shared stand-in `control` schema + sensitive table.
    ensure_control_standin(&conn).await;

    // CREATE TABLE in control → denied.
    let e = as_migrator(&conn, &role, "CREATE TABLE control.evil (x int)")
        .await
        .expect_err("CREATE in control must be denied");
    assert_permission_denied(&e, "CREATE TABLE control.evil");

    // SELECT from a sensitive control table → denied.
    let e = as_migrator(&conn, &role, "SELECT * FROM control.creator_billing")
        .await
        .expect_err("SELECT control.creator_billing must be denied");
    assert_permission_denied(&e, "SELECT control.creator_billing");

    // DROP SCHEMA control → denied (not the owner). Use IF EXISTS so the
    // assertion is about the privilege check (42501 / must be owner), not about
    // whether a parallel test transiently affected the shared schema.
    let e = as_migrator(&conn, &role, "DROP SCHEMA IF EXISTS control")
        .await
        .expect_err("DROP SCHEMA control must be denied");
    assert_permission_denied(&e, "DROP SCHEMA control");

    // NOTE: the shared `control` stand-in is intentionally NOT dropped (shared
    // across parallel tests; see `ensure_control_standin`).
    teardown(&conn, &cfg).await;
}

// ===========================================================================
// Cross-tenant: project A's migrator cannot touch project B's schema
// ===========================================================================

#[compio::test]
async fn migrator_a_cannot_touch_project_b_schema() {
    let conn = pg().await;
    let tok_a = token();
    let tok_b = token();
    let cfg_a = cfg_for(&tok_a);
    let cfg_b = cfg_for(&tok_b);
    let role_a = cfg_a.migrator_role.clone().unwrap();
    teardown(&conn, &cfg_a).await;
    teardown(&conn, &cfg_b).await;

    // Provision A and B.
    for cfg in [&cfg_a, &cfg_b] {
        ensure_project_schema(&conn, cfg).await;
        ensure_journal(&conn, cfg).await.expect("journal");
        provision_migrator(&conn, cfg).await.expect("provision");
    }
    // Give B a table to attempt to read.
    conn.batch_execute(&format!(
        "CREATE TABLE \"{}\".x (id int)",
        cfg_b.project_schema
    ))
    .await
    .expect("create B.x");

    // A's migrator creating in B's schema → denied.
    let e = as_migrator(
        &conn,
        &role_a,
        &format!("CREATE TABLE \"{}\".intruder (z int)", cfg_b.project_schema),
    )
    .await
    .expect_err("A→B CREATE must be denied");
    assert_permission_denied(&e, "A creating in B's schema");

    // A's migrator reading B's table → denied.
    let e = as_migrator(
        &conn,
        &role_a,
        &format!("SELECT * FROM \"{}\".x", cfg_b.project_schema),
    )
    .await
    .expect_err("A→B SELECT must be denied");
    assert_permission_denied(&e, "A reading B's table");

    teardown(&conn, &cfg_a).await;
    teardown(&conn, &cfg_b).await;
}

// ===========================================================================
// Privilege escalation denied by role attributes
// ===========================================================================

#[compio::test]
async fn migrator_cannot_escalate_privileges() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    let role = cfg.migrator_role.clone().unwrap();
    teardown(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("journal");
    provision_migrator(&conn, &cfg).await.expect("provision");

    // CREATE ROLE → denied (NOCREATEROLE).
    let e = as_migrator(&conn, &role, "CREATE ROLE evil_role")
        .await
        .expect_err("CREATE ROLE must be denied");
    assert_permission_denied(&e, "CREATE ROLE");

    // ALTER SYSTEM → denied (NOSUPERUSER).
    let e = as_migrator(&conn, &role, "ALTER SYSTEM SET work_mem = '999MB'")
        .await
        .expect_err("ALTER SYSTEM must be denied");
    assert_permission_denied(&e, "ALTER SYSTEM");

    teardown(&conn, &cfg).await;
}

// ===========================================================================
// THE BACKSTOP TEST — runtime-constructed cross-schema SQL.
//
// The guard CANNOT statically confine this (the target schema is a runtime
// variable inside EXECUTE format(...)), so it passes line 1 — but executed as
// the migrator role it fails with permission denied at execution: line 2
// catches line 1's documented residual.
// ===========================================================================

#[compio::test]
async fn backstop_runtime_constructed_cross_schema_is_denied_at_execution() {
    use zeroship_migrate::{GuardConfig, SqlGuard};

    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    let role = cfg.migrator_role.clone().unwrap();
    teardown(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("journal");
    provision_migrator(&conn, &cfg).await.expect("provision");

    // `control` must EXIST so the denial is a genuine permission check (42501),
    // not a "schema does not exist" (3F000). Shared stand-in, never dropped.
    ensure_control_standin(&conn).await;

    // The runtime-constructed cross-schema payload. The target schema name is
    // assembled at EXECUTE time from fragments ('con' || 'trol'), so the literal
    // "control" never appears as a token, AND the body uses no `%I` identifier
    // template — defeating BOTH of the guard's PL/pgSQL body heuristics
    // (bare-literal platform-schema match + `%I`-template foreign-identifier
    // match). This is a genuine line-1 residual: the schema is materialized only
    // at execution, invisible to any parse-time analysis.
    let payload = "DO $$ DECLARE s text := 'con' || 'trol'; \
                   BEGIN EXECUTE 'CREATE TABLE ' || s || '.evil (x int)'; END $$";

    // 1. CONFIRM line 1 (the guard) does NOT statically deny this — it is a
    //    documented residual (runtime-constructed SQL the parser can't catch).
    let guard = SqlGuard::new(GuardConfig {
        project_schema: cfg.project_schema.clone(),
        extension_allowlist: Vec::new(),
    });
    guard.check(payload).expect(
        "the guard (line 1) must NOT statically deny runtime-constructed cross-schema SQL — \
         this is the documented residual line 2 backstops",
    );

    // 2. CONFIRM line 2 (the role) DOES deny it at execution: run as the
    //    migrator role and assert permission denied.
    let e = as_migrator(&conn, &role, payload)
        .await
        .expect_err("runtime-constructed CREATE in control must be DENIED at execution by the role");
    assert_permission_denied(&e, "backstop: runtime-constructed control.evil");

    // And prove it: control.evil was never created.
    assert!(
        !table_exists(&conn, "control", "evil").await,
        "the backstop payload must NOT have created control.evil"
    );

    teardown(&conn, &cfg).await;
}

// ===========================================================================
// Same backstop, but driven through the real executor `apply` flow — proving
// the production path (SET LOCAL ROLE inside the txn) confines it too.
// ===========================================================================

#[compio::test]
async fn backstop_through_apply_flow_is_denied() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    teardown(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("journal");
    provision_migrator(&conn, &cfg).await.expect("provision");
    // `control` must EXIST so the denial is a 42501, not a 3F000 (see above).
    ensure_control_standin(&conn).await;

    let m = mig(
        MigrationId::generate(),
        "sneaky",
        "DO $$ DECLARE s text := 'con' || 'trol'; \
         BEGIN EXECUTE 'CREATE TABLE ' || s || '.evil (x int)'; END $$",
    );
    let err = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "actor")
        .await
        .expect_err("the apply flow must surface the role's permission-denied");
    // The executor wraps the DB error as MigrationFailed; assert the wrapped DB
    // error carries the insufficient_privilege SQLSTATE (42501).
    match &err {
        zeroship_migrate::ApplyError::MigrationFailed { source, .. } => {
            assert_permission_denied(source, "backstop via apply: MigrationFailed source");
        }
        other => panic!("expected MigrationFailed(permission denied), got {other:?}"),
    }
    assert!(
        !table_exists(&conn, "control", "evil").await,
        "control.evil must not exist — the txn rolled back"
    );
    // Nothing journaled (the txn rolled back).
    let n: i64 = conn
        .query_one(
            &format!(
                "SELECT count(*)::bigint AS c FROM \"{}\".schema_migrations",
                cfg.meta_schema
            ),
            &[],
        )
        .await
        .expect("journal count")
        .get("c");
    assert_eq!(n, 0, "denied migration journals nothing");

    teardown(&conn, &cfg).await;
}

// ===========================================================================
// C1 (CRITICAL) — JOURNAL FORGERY by a migration `up`.
//
// The journal of record (`<meta>.schema_migrations`) and the inflight side-table
// MUST be unforgeable by a migration's `up`. A migration runs as the migrator
// role; the executor performs ALL journal I/O as the admin. The migrator has NO
// grants on the meta schema, so any attempt by an `up` to write the journal —
// qualified or unqualified — fails with `permission denied` at execution.
//
// These tests reproduce the forge attack end-to-end through the real `apply()`.
// Pre-fix they were RED: the migrator held `INSERT on schema_migrations` +
// `INSERT,DELETE on schema_migrations_inflight` + `USAGE on meta_schema`, and the
// migration-time search_path included the meta schema, so an unqualified
// `INSERT INTO schema_migrations(...)` resolved to the journal and SUCCEEDED.
// ===========================================================================

/// A migration whose `up` is an UNQUALIFIED `INSERT INTO schema_migrations(...)`.
/// With the meta schema dropped from the migration-time `search_path` AND the
/// grant revoked, this must fail (either `permission denied` 42501 OR
/// `relation does not exist` 42P01 — both prove the forge is impossible). The
/// migration fails and nothing is forged.
#[compio::test]
async fn migration_up_cannot_forge_journal_via_unqualified_insert() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    teardown(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("journal");
    provision_migrator(&conn, &cfg).await.expect("provision");

    // The forged row targets a fabricated victim version. UNQUALIFIED table name
    // — pre-fix this resolved to the meta journal via the pinned search_path.
    let forge = mig(
        MigrationId::generate(),
        "forge_unqualified",
        "INSERT INTO schema_migrations \
             (version, name, checksum, applied_by, exec_ms, phase, outcome) \
         VALUES ('mig_victim', 'victim', 'deadbeef', 'attacker', 0, 'completed', 'success')",
    );
    let err = apply(&conn, &cfg, std::slice::from_ref(&forge), Approval::None, "actor")
        .await
        .expect_err("forging the journal via an unqualified INSERT must fail");
    // The executor wraps the DB error as MigrationFailed.
    let zeroship_migrate::ApplyError::MigrationFailed { source, .. } = &err else {
        panic!("expected MigrationFailed, got {err:?}");
    };
    // Either permission-denied (grant revoked) or relation-not-found (meta off the
    // search_path) — both prove the forge cannot land.
    let code = source.code();
    assert!(
        code == Some(&compio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE)
            || code == Some(&compio_postgres::error::SqlState::UNDEFINED_TABLE),
        "unqualified journal forge must be denied (42501) or unresolved (42P01), got {code:?}"
    );

    // Nothing forged: the journal has no `mig_victim` row.
    let n: i64 = conn
        .query_one(
            &format!(
                "SELECT count(*)::bigint AS c FROM \"{}\".schema_migrations WHERE version = 'mig_victim'",
                cfg.meta_schema
            ),
            &[],
        )
        .await
        .expect("journal count")
        .get("c");
    assert_eq!(n, 0, "no forged row may exist in the journal");

    teardown(&conn, &cfg).await;
}

/// A migration whose `up` is a QUALIFIED `INSERT INTO <meta>.schema_migrations`.
/// The SQL guard denies cross-schema writes; even if it somehow passed, the
/// revoked grant denies it at execution. Assert the apply aborts and nothing is
/// forged.
#[compio::test]
async fn migration_up_cannot_forge_journal_via_qualified_insert() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    teardown(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("journal");
    provision_migrator(&conn, &cfg).await.expect("provision");

    let forge = mig(
        MigrationId::generate(),
        "forge_qualified",
        &format!(
            "INSERT INTO \"{}\".schema_migrations \
                 (version, name, checksum, applied_by, exec_ms, phase, outcome) \
             VALUES ('mig_victim2', 'victim', 'deadbeef', 'attacker', 0, 'completed', 'success')",
            cfg.meta_schema
        ),
    );
    let err = apply(&conn, &cfg, std::slice::from_ref(&forge), Approval::None, "actor")
        .await
        .expect_err("forging the journal via a qualified cross-schema INSERT must fail");
    // Either the guard rejects it up-front, or the role denies it at execution.
    assert!(
        matches!(
            err,
            zeroship_migrate::ApplyError::Guard { .. }
                | zeroship_migrate::ApplyError::MigrationFailed { .. }
        ),
        "expected Guard or MigrationFailed, got {err:?}"
    );

    let n: i64 = conn
        .query_one(
            &format!(
                "SELECT count(*)::bigint AS c FROM \"{}\".schema_migrations WHERE version = 'mig_victim2'",
                cfg.meta_schema
            ),
            &[],
        )
        .await
        .expect("journal count")
        .get("c");
    assert_eq!(n, 0, "no forged row may exist in the journal");

    teardown(&conn, &cfg).await;
}

/// END-TO-END SUPPRESS ATTACK (the critic's reproduction): a malicious migration
/// forges a `completed` row for a VICTIM version. Pre-fix, the forge succeeded,
/// so a later legitimate deploy of the victim migration was silently SKIPPED
/// (pending = set − completed). With the fix, the forge FAILS, so when the victim
/// migration is later deployed it STILL RUNS — its table IS created.
#[compio::test]
async fn forged_completed_row_does_not_suppress_a_future_victim_migration() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    teardown(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("journal");
    provision_migrator(&conn, &cfg).await.expect("provision");

    // The victim version the attacker wants to suppress.
    let victim_version = MigrationId::generate();

    // Step 1: a malicious migration forges a `completed` row for the victim.
    let attack = mig(
        MigrationId::generate(),
        "suppress_attack",
        &format!(
            "INSERT INTO schema_migrations \
                 (version, name, checksum, applied_by, exec_ms, phase, outcome) \
             VALUES ('{}', 'victim', 'deadbeef', 'attacker', 0, 'completed', 'success')",
            victim_version.as_str()
        ),
    );
    let attack_err = apply(&conn, &cfg, std::slice::from_ref(&attack), Approval::None, "actor").await;
    assert!(
        attack_err.is_err(),
        "the forge attack must fail (the migrator cannot write the journal)"
    );

    // The forged completed row must NOT be in the journal.
    let forged: i64 = conn
        .query_one(
            &format!(
                "SELECT count(*)::bigint AS c FROM \"{}\".schema_migrations WHERE version = $1",
                cfg.meta_schema
            ),
            &[&victim_version.as_str()],
        )
        .await
        .expect("journal count")
        .get("c");
    assert_eq!(forged, 0, "the suppress attack must NOT plant a completed row");

    // Step 2: the LEGITIMATE victim migration is later deployed. Because nothing
    // was forged, it is still pending and RUNS — its table is created.
    let victim = mig(
        victim_version.clone(),
        "victim_creates_table",
        &format!(
            "CREATE TABLE \"{}\".victim_table (id bigint primary key)",
            cfg.project_schema
        ),
    );
    let out = apply(&conn, &cfg, std::slice::from_ref(&victim), Approval::None, "actor")
        .await
        .expect("the victim migration must still run (not suppressed)");
    assert_eq!(out.applied.len(), 1, "the victim migration applied");
    assert!(
        table_exists(&conn, &cfg.project_schema, "victim_table").await,
        "the victim migration's effect MUST be present — it was not silently suppressed"
    );

    teardown(&conn, &cfg).await;
}

/// A migration whose `up` is an UNQUALIFIED `DELETE FROM schema_migrations_inflight`.
/// The migrator no longer has DELETE on the inflight table (grant revoked) and the
/// meta schema is off its `search_path`, so this fails — it cannot tamper with the
/// inflight markers.
#[compio::test]
async fn migration_up_cannot_delete_inflight_markers() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    teardown(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("journal");
    provision_migrator(&conn, &cfg).await.expect("provision");

    let forge = mig(
        MigrationId::generate(),
        "delete_inflight",
        "DELETE FROM schema_migrations_inflight",
    );
    let err = apply(&conn, &cfg, std::slice::from_ref(&forge), Approval::None, "actor")
        .await
        .expect_err("deleting inflight markers must fail");
    let zeroship_migrate::ApplyError::MigrationFailed { source, .. } = &err else {
        panic!("expected MigrationFailed, got {err:?}");
    };
    let code = source.code();
    assert!(
        code == Some(&compio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE)
            || code == Some(&compio_postgres::error::SqlState::UNDEFINED_TABLE),
        "inflight DELETE must be denied (42501) or unresolved (42P01), got {code:?}"
    );

    teardown(&conn, &cfg).await;
}
