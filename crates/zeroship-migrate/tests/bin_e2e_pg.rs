//! Phase-6 BINARY-level e2e (design §9/§12): the COMPOSE `migrate` flow, proven
//! end-to-end through the ACTUAL `zeroship-migrate` binary as a SUBPROCESS — not
//! the library `run_*` functions, not a shim.
//!
//! This is the faithful peer of the compose `migrate` service: it runs
//!
//!   zeroship-migrate migrate --dir <db/migrations> --database-url <DSN> --profile platform
//!
//! exactly as the compose service does (design §9 "Compose service swap"), so the
//! real CLI arg surface (`clap` parse → `run_config` → `command::runner::run_*`)
//! is exercised in addition to the engine. The bin path is the cargo-provided
//! `CARGO_BIN_EXE_zeroship-migrate`, i.e. the freshly-built `target/<profile>/
//! zeroship-migrate`.
//!
//! To avoid clobbering the real `zeroship` schema (the ported set hardcodes the
//! `zeroship` / `oauth_hydra` schemas + the global `zeroship_*` roles), the test
//! creates a FRESH, dedicated DATABASE `zsmig_bin_e2e_<token>` on the
//! `zeroship_migrate_test` cluster, runs the bin against THAT db, asserts the full
//! schema inventory there, then drops it. The journal lives in a unique meta
//! schema inside that throwaway db. Roles are cluster-wide and idempotent
//! (`CREATE ROLE IF NOT EXISTS` / DO-block guards) so concurrent runs don't fight.
//!
//! Asserts (the compose contract): exit 0; all 58 migrations applied; a second
//! `migrate` is an idempotent no-op (exit 0); `status` shows 58 applied / 0
//! pending; the schema materialized — namespaces (`zeroship`/`oauth_hydra`), the
//! service roles, the RLS policies (the inventory checks mirrored from
//! `platform_port_pg.rs`).

use std::path::PathBuf;
use std::process::Command;

use compio_postgres::Client;
use zeroship_migrate::test_support::acquire_global_platform_resource_lock;

const ADMIN_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test";

/// Admin connection to the cluster's maintenance db (used to CREATE/DROP the
/// throwaway per-test database). Honours `MIGRATE_TEST_DB` for the host/port/creds
/// but always targets `zeroship_migrate_test` as the maintenance db.
fn admin_dsn() -> String {
    std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| ADMIN_DSN.to_string())
}

/// The libpq-style URL the BINARY connects with, pointed at the throwaway db.
/// (The bin takes `--database-url`; a keyword DSN works too, but the compose
/// service uses a URL, so we mirror that.)
fn bin_database_url(db: &str) -> String {
    format!("postgres://postgres:zeroship@localhost:5440/{db}")
}

/// A keyword DSN for the THROWAWAY db, derived from the admin DSN by swapping the
/// `dbname=` token (so `MIGRATE_TEST_DB` host/port/creds overrides are honoured).
fn throwaway_dsn(db: &str) -> String {
    admin_dsn()
        .split_whitespace()
        .filter(|kv| !kv.starts_with("dbname="))
        .collect::<Vec<_>>()
        .join(" ")
        + &format!(" dbname={db}")
}

async fn connect(dsn: &str) -> Client {
    let (client, conn) = compio_postgres::connect(dsn, compio_postgres::NoTls)
        .await
        .unwrap_or_else(|e| panic!("connect to {dsn}: {e}"));
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

fn token() -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{pid}_{nanos}")
}

/// The repo-root `db/migrations/` directory — the SAME tree the compose `migrate`
/// service mounts at `/db/migrations`.
fn migrations_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../db/migrations")
        .canonicalize()
        .expect("db/migrations exists at repo root")
}

/// A process-wide temp "sink" CWD for `run_bin`. Auto-dump (default ON) writes
/// `./db/schema.sql` RELATIVE to the child's CWD; running the children from a
/// throwaway temp dir keeps the crate tree clean even if a call forgets
/// `--no-dump-schema`. Defense-in-depth (this suite passes `--no-dump-schema`) —
/// every `--dir`/`--database-url` here is absolute, so the CWD change is invisible.
fn sink_dir() -> &'static std::path::Path {
    use std::sync::OnceLock;
    static SINK: OnceLock<PathBuf> = OnceLock::new();
    SINK.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("zsmig_sink_{}", std::process::id()));
        std::fs::create_dir_all(&d).expect("create run_bin sink dir");
        d
    })
}

/// Run the BUILT `zeroship-migrate` binary as a subprocess. Returns
/// `(success, stdout, stderr)`. `CARGO_BIN_EXE_zeroship-migrate` is injected by
/// cargo for integration tests and points at the freshly-built binary.
fn run_bin(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_zeroship-migrate"))
        .current_dir(sink_dir())
        .args(args)
        .output()
        .expect("spawn the built zeroship-migrate binary");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

async fn create_db(admin: &Client, db: &str) {
    // FORCE-drop any leftover, then create fresh.
    admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE);"))
        .await
        .expect("drop stale throwaway db");
    admin
        .batch_execute(&format!("CREATE DATABASE \"{db}\";"))
        .await
        .expect("create throwaway db");
}

async fn drop_db(admin: &Client, db: &str) {
    let _ = admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE);"))
        .await;
}

async fn namespace_exists(conn: &Client, name: &str) -> bool {
    !conn
        .query("SELECT 1 FROM pg_namespace WHERE nspname = $1", &[&name])
        .await
        .expect("query pg_namespace")
        .is_empty()
}

async fn role_exists(conn: &Client, name: &str) -> bool {
    !conn
        .query("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&name])
        .await
        .expect("query pg_roles")
        .is_empty()
}

async fn table_exists(conn: &Client, schema: &str, table: &str) -> bool {
    !conn
        .query(
            "SELECT 1 FROM information_schema.tables WHERE table_schema = $1 AND table_name = $2",
            &[&schema, &table],
        )
        .await
        .expect("query table existence")
        .is_empty()
}

async fn policy_exists(conn: &Client, schema: &str, table: &str, policy: &str) -> bool {
    !conn
        .query(
            "SELECT 1 FROM pg_policy p \
               JOIN pg_class c ON c.oid = p.polrelid \
               JOIN pg_namespace n ON n.oid = c.relnamespace \
              WHERE n.nspname = $1 AND c.relname = $2 AND p.polname = $3",
            &[&schema, &table, &policy],
        )
        .await
        .expect("query pg_policy")
        .is_empty()
}

/// Count the `completed` rows in the engine journal (the meta-schema
/// `<meta>.schema_migrations` table — design §2.2). The journal is the source of
/// truth the compose service relies on; `phase = 'completed'` is a fully-applied
/// migration.
async fn journal_completed_count(conn: &Client, meta: &str) -> i64 {
    let rows = conn
        .query(
            &format!(
                "SELECT count(*)::bigint FROM \"{meta}\".schema_migrations \
                 WHERE phase = 'completed'"
            ),
            &[],
        )
        .await
        .expect("query the journal completed count");
    rows[0].get::<_, i64>(0)
}

// ---------------------------------------------------------------------------
// THE binary-level compose-`migrate` e2e.
// ---------------------------------------------------------------------------

#[compio::test]
async fn binary_migrate_applies_whole_set_idempotently_and_materializes_schema() {
    let _global_lock = acquire_global_platform_resource_lock(&admin_dsn()).await;
    let tok = token();
    let db = format!("zsmig_bin_e2e_{tok}");
    let meta = format!("binmeta_{tok}");

    let admin = connect(&admin_dsn()).await;
    create_db(&admin, &db).await;

    let dir = migrations_dir();
    let dir_s = dir.to_str().expect("utf-8 migrations path").to_string();
    let url = bin_database_url(&db);

    // The arg vector is the EXACT compose-service surface (plus a unique
    // --meta-schema so the journal does not collide cluster-side). Defaults
    // supply --profile platform's schema/extension allowlists. `--yes` mirrors
    // the compose service: the ported set carries reviewed in-place schema
    // evolutions (DROP CONSTRAINT/COLUMN, …) the engine flags DESTRUCTIVE, so the
    // operator's standing confirmation is required to apply them.
    let migrate_args = [
        "migrate",
        "--dir",
        &dir_s,
        "--database-url",
        &url,
        "--profile",
        "platform",
        "--meta-schema",
        &meta,
        "--yes",
        // This e2e exercises the apply path, not the schema-dump lifecycle. Opt out
        // of the new auto-`schema.sql` refresh (dbmate parity, ON by default) so the
        // test does not write a stray `./db/schema.sql` into the crate CWD.
        "--no-dump-schema",
    ];

    // 1. First apply: exit 0.
    let (ok, stdout, stderr) = run_bin(&migrate_args);
    assert!(
        ok,
        "the binary `migrate` must exit 0 on a fresh DB\nstdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains("applied 58"),
        "the binary reports 58 applied migrations: stdout={stdout}"
    );

    // 2. Inventory — verified against the THROWAWAY db (connect to it directly).
    let app = connect(&throwaway_dsn(&db)).await;

    // 2a. The journal records all 58 applied — the compose gate's source of truth.
    assert_eq!(
        journal_completed_count(&app, &meta).await,
        58,
        "the journal records 58 completed (applied) migrations"
    );

    // 2b. Namespaces.
    assert!(namespace_exists(&app, "zeroship").await, "zeroship schema");
    assert!(
        namespace_exists(&app, "oauth_hydra").await,
        "oauth_hydra schema"
    );

    // 2c. The five platform service roles + the Hydra role (cluster-wide).
    for role in [
        "zeroship_auth",
        "zeroship_control",
        "zeroship_gateway",
        "zeroship_worker",
        "zeroship_app",
        "oauth_hydra",
    ] {
        assert!(role_exists(&app, role).await, "role {role} created");
    }

    // 2d. Key tables across the auth / control / sandbox / billing domains.
    for (schema, table) in [
        ("zeroship", "users"),
        ("zeroship", "apps"),
        ("zeroship", "app_secrets"),
        ("zeroship", "gateway_sessions"),
        ("zeroship", "app_user_identities"),
        ("zeroship", "oauth_clients"),
        ("zeroship", "sandboxes"),
        ("zeroship", "plans"),
        ("zeroship", "invoices"),
    ] {
        assert!(
            table_exists(&app, schema, table).await,
            "{schema}.{table} exists"
        );
    }

    // 2e. RLS policies — the four tenant_isolation policies V0025 installs.
    for table in [
        "app_secrets",
        "gateway_sessions",
        "app_session_anchors",
        "app_user_identities",
    ] {
        assert!(
            policy_exists(&app, "zeroship", table, "tenant_isolation").await,
            "RLS policy tenant_isolation on zeroship.{table}"
        );
    }

    // 3. Second `migrate` run: idempotent no-op, still exit 0.
    let (ok2, stdout2, stderr2) = run_bin(&migrate_args);
    assert!(
        ok2,
        "a second `migrate` must exit 0\nstdout={stdout2}\nstderr={stderr2}"
    );
    assert!(
        stdout2.contains("no-op"),
        "the second run is an idempotent no-op: stdout={stdout2}"
    );
    // The journal did not grow.
    assert_eq!(
        journal_completed_count(&app, &meta).await,
        58,
        "the idempotent re-run added no journal rows"
    );

    // 4. `status` reports 58 applied / 0 pending (the binary's own view).
    let status_args = [
        "status",
        "--dir",
        &dir_s,
        "--database-url",
        &url,
        "--profile",
        "platform",
        "--meta-schema",
        &meta,
    ];
    let (ok3, stdout3, stderr3) = run_bin(&status_args);
    assert!(
        ok3,
        "`status` must exit 0\nstdout={stdout3}\nstderr={stderr3}"
    );
    assert!(
        stdout3.contains("applied=58") && stdout3.contains("pending=0"),
        "status shows 58 applied / 0 pending: stdout={stdout3}"
    );

    // Teardown: drop the throwaway db (this drops its schemas + journal with it).
    // Roles are cluster-wide and intentionally left (idempotent across runs).
    drop_db(&admin, &db).await;
    _global_lock.release().await;
}
