#![cfg(feature = "standalone-cli")]
//! Track A, Phase A3 — BINARY-level dbmate e2e: drive the ACTUAL
//! `zero-migrate` binary as a SUBPROCESS through the full public dbmate
//! workflow under the DEFAULT Trusted profile, on a fresh throwaway DB.
//!
//! This is the faithful peer of `bin_e2e_pg.rs` (which exercises the platform
//! Flyway path): here we prove the GENERIC posture works end-to-end through the
//! real `clap` parse → `run_config` (Trusted default) → `command::runner::run_*`:
//!
//!   1. `new`     — creates a dbmate file in a temp `--dir` (offline, no DB).
//!   2. (test writes a real `-- migrate:up`/`down` body into that file)
//!   3. `migrate` — DEFAULT Trusted profile applies it; the table + journal exist.
//!   4. `status`  — 1 applied / 0 pending.
//!   5. `down --yes` — rolls back the one migration; the table is gone.
//!   6. `migrate` — re-applies; the table is back.
//!
//! Plus a check that `--profile platform` still runs (the internal path is
//! unaffected by the Trusted default), and that the compose/ops callers stay
//! explicit-`platform`.
//!
//! Each test uses a FRESH database `zsmig_dbmate_<token>` on the
//! `zero_migrate_test` cluster and a unique meta schema, then drops the DB.

use std::path::PathBuf;
use std::process::Command;

use compio_postgres::Client;

const ADMIN_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zero_migrate_test";

fn admin_dsn() -> String {
    std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| ADMIN_DSN.to_string())
}

fn bin_database_url(db: &str) -> String {
    format!("postgres://postgres:zeroship@localhost:5440/{db}")
}

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

/// A process-wide temp "sink" CWD for `run_bin`. Auto-dump (default ON) writes
/// `./db/schema.sql` RELATIVE to the child's CWD; running the children from a
/// throwaway temp dir keeps the crate tree clean even if a call forgets
/// `--no-dump-schema`. Defense-in-depth (this suite passes `--no-dump-schema` on its
/// apply paths) — every `--dir`/`--database-url` here is absolute, so the CWD change
/// is otherwise invisible.
fn sink_dir() -> &'static std::path::Path {
    use std::sync::OnceLock;
    static SINK: OnceLock<PathBuf> = OnceLock::new();
    SINK.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("zsmig_sink_{}", std::process::id()));
        std::fs::create_dir_all(&d).expect("create run_bin sink dir");
        d
    })
}

fn run_bin(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_zero-migrate"))
        .current_dir(sink_dir())
        .args(args)
        .output()
        .expect("spawn the built zero-migrate binary");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

async fn create_db(admin: &Client, db: &str) {
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

/// The full dbmate workflow through the REAL binary under the DEFAULT Trusted
/// profile: new → migrate → status → down → migrate.
#[compio::test]
async fn cli_dbmate_full_workflow_under_default_trusted_profile() {
    let tok = token();
    let db = format!("zsmig_dbmate_{tok}");
    let meta = format!("dbmate_meta_{tok}");

    let admin = connect(&admin_dsn()).await;
    create_db(&admin, &db).await;

    // A temp migration dir; `new` writes the dbmate file here.
    let tmp = std::env::temp_dir().join(format!("zsmig_dbmate_dir_{tok}"));
    std::fs::create_dir_all(&tmp).expect("create temp migration dir");
    let dir_s = tmp.to_str().expect("utf-8 temp dir").to_string();
    let url = bin_database_url(&db);

    // 1. `new` — offline (no DSN needed), default Trusted profile.
    let (ok_new, out_new, err_new) = run_bin(&["new", "widgets", "--dir", &dir_s]);
    assert!(
        ok_new,
        "`new` must exit 0\nstdout={out_new}\nstderr={err_new}"
    );

    // Find the created file (the only `.sql` in the temp dir).
    let created: PathBuf = std::fs::read_dir(&tmp)
        .expect("read temp dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("sql"))
        .expect("`new` created a .sql file");
    let fname = created.file_name().unwrap().to_str().unwrap().to_string();
    assert!(
        fname.ends_with("_widgets.sql") && fname.len() == "20210101000000_widgets.sql".len(),
        "dbmate filename shape (<14-digit>_widgets.sql): {fname}"
    );

    // 2. Write a real dbmate up/down body into the generated file.
    std::fs::write(
        &created,
        "-- migrate:up\nCREATE TABLE widgets (id bigserial primary key, name text);\n\n\
         -- migrate:down\nDROP TABLE widgets;\n",
    )
    .expect("write migration body");

    // The generic dbmate posture: NO --profile flag (defaults to trusted), schema
    // `public`, meta `public` (we override meta to a unique schema to avoid
    // collisions, but it still lives generically).
    let base = [
        "--dir",
        &dir_s,
        "--database-url",
        &url,
        "--meta-schema",
        &meta,
    ];

    // 3. `migrate` — default Trusted profile applies it. `--no-dump-schema` opts
    //    out of the new auto-`schema.sql` refresh (dbmate parity, ON by default) so
    //    this apply-path e2e does not litter `./db/schema.sql` in the crate CWD.
    let mut migrate_args = vec!["migrate"];
    migrate_args.extend_from_slice(&base);
    migrate_args.push("--no-dump-schema");
    let (ok_mig, out_mig, err_mig) = run_bin(&migrate_args);
    assert!(
        ok_mig,
        "`migrate` (default trusted) must exit 0\nstdout={out_mig}\nstderr={err_mig}"
    );
    assert!(
        out_mig.contains("applied 1"),
        "migrate reports 1 applied: {out_mig}"
    );

    let app = connect(&throwaway_dsn(&db)).await;
    assert!(
        table_exists(&app, "public", "widgets").await,
        "the widgets table materialized under the Trusted (generic public) profile"
    );
    assert_eq!(
        journal_completed_count(&app, &meta).await,
        1,
        "the journal records the one applied migration"
    );

    // 4. `status` — 1 applied / 0 pending.
    let mut status_args = vec!["status"];
    status_args.extend_from_slice(&base);
    let (ok_st, out_st, err_st) = run_bin(&status_args);
    assert!(ok_st, "`status` must exit 0\nstdout={out_st}\nstderr={err_st}");
    assert!(
        out_st.contains("applied=1") && out_st.contains("pending=0"),
        "status shows 1 applied / 0 pending: {out_st}"
    );

    // 5. `down --yes` — rolls back the one migration; the table is gone.
    //    `--no-dump-schema`: rollback/down also auto-refreshes schema.sql; suppress
    //    it (apply-path e2e, run_bin inherits the crate CWD).
    let mut down_args = vec!["down"];
    down_args.extend_from_slice(&base);
    down_args.push("--yes");
    down_args.push("--no-dump-schema");
    let (ok_down, out_down, err_down) = run_bin(&down_args);
    assert!(
        ok_down,
        "`down --yes` must exit 0\nstdout={out_down}\nstderr={err_down}"
    );
    assert!(
        !table_exists(&app, "public", "widgets").await,
        "`down` dropped the widgets table"
    );

    // A `down` WITHOUT --yes must refuse (rollback is destructive).
    let mut down_noyes = vec!["down"];
    down_noyes.extend_from_slice(&base);
    let (ok_down2, _o, _e) = run_bin(&down_noyes);
    // (state is already rolled back, so this is also a no-op target; the gate is
    // what we assert — a missing --yes must NOT succeed.)
    assert!(!ok_down2, "`down` without --yes must refuse (non-zero exit)");

    // 6. `migrate` again — re-applies; the table is back.
    let (ok_re, out_re, err_re) = run_bin(&migrate_args);
    assert!(
        ok_re,
        "re-`migrate` must exit 0\nstdout={out_re}\nstderr={err_re}"
    );
    assert!(
        table_exists(&app, "public", "widgets").await,
        "re-migrate re-created the widgets table"
    );

    // Teardown.
    let _ = std::fs::remove_dir_all(&tmp);
    drop_db(&admin, &db).await;
}

/// The internal `--profile platform` path is UNAFFECTED by the Trusted default:
/// an explicit `--profile platform` can still apply a Flyway-shaped SQL fixture.
/// This guards the retained legacy SQL loader without making it the platform
/// source of truth.
#[compio::test]
async fn explicit_platform_profile_still_applies() {
    let tok = token();
    let db = format!("zsmig_plat_{tok}");
    let meta = format!("plat_meta_{tok}");

    let admin = connect(&admin_dsn()).await;
    create_db(&admin, &db).await;

    let tmp = std::env::temp_dir().join(format!("zsmig_plat_dir_{tok}"));
    std::fs::create_dir_all(&tmp).expect("create temp migration dir");
    // A Flyway-shaped file that creates the zeroship schema then a table in it —
    // exercising the widened platform guard + multi-schema search_path.
    std::fs::write(
        tmp.join("V0001__init.sql"),
        "CREATE SCHEMA IF NOT EXISTS zeroship;\nCREATE TABLE zeroship.thing (id int);\n",
    )
    .expect("write platform migration");

    let dir_s = tmp.to_str().unwrap().to_string();
    let url = bin_database_url(&db);

    let (ok, out, err) = run_bin(&[
        "migrate",
        "--dir",
        &dir_s,
        "--database-url",
        &url,
        "--profile",
        "platform",
        "--meta-schema",
        &meta,
        // Apply-path e2e (run_bin inherits the crate CWD): opt out of the auto-dump
        // so it does not write a stray `./db/schema.sql`.
        "--no-dump-schema",
    ]);
    assert!(
        ok,
        "explicit --profile platform must still apply\nstdout={out}\nstderr={err}"
    );

    let app = connect(&throwaway_dsn(&db)).await;
    assert!(
        table_exists(&app, "zeroship", "thing").await,
        "the platform profile created zeroship.thing"
    );

    let _ = std::fs::remove_dir_all(&tmp);
    drop_db(&admin, &db).await;
}

/// The Trusted default must NOT silently change the internal platform behaviour:
/// the compose `migrate` service and `ops/db-migrate.sh` must pass `--profile
/// platform` EXPLICITLY. This pins that (a regression test for the default flip).
#[test]
fn compose_and_ops_pass_profile_platform_explicitly() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");

    let compose = std::fs::read_to_string(root.join("docker-compose.yml"))
        .expect("read docker-compose.yml");
    assert!(
        compose.contains("--profile=platform") || compose.contains("--profile platform"),
        "the compose `migrate` service must pass --profile=platform explicitly \
         (so the Trusted CLI default does not change platform behaviour)"
    );

    let ops = std::fs::read_to_string(root.join("ops/db-migrate.sh"))
        .expect("read ops/db-migrate.sh");
    assert!(
        ops.contains("ZEROSHIP_MIGRATE_PROFILE:-platform"),
        "ops/db-migrate.sh must default ZEROSHIP_MIGRATE_PROFILE to platform"
    );
    assert!(
        ops.contains("--profile"),
        "ops/db-migrate.sh must pass --profile to the binary explicitly"
    );
}

/// `validate` is a CI GATE (fix #3): a CLEAN set exits 0, but a checksum-DRIFTED
/// set must exit NON-ZERO so CI can block on it — while the printed lines stay the
/// same (`validate: … drift_clean=false` + a `DRIFT` line). Pre-fix the drifted run
/// printed `drift_clean=false` yet still exited 0; this pins the non-zero exit on
/// the PG leg (the SQLite peer is in `cli_dbmate_e2e_sqlite.rs`).
///
/// Driven under `--profile platform` against a fresh throwaway DB + a unique schema
/// the migration `CREATE`s — the Platform shadow path the binary exercises today (it
/// provisions a throwaway shadow DATABASE under the admin connection, design §8).
#[compio::test]
async fn cli_validate_pg_clean_exits_zero_then_drift_exits_nonzero() {
    let tok = token();
    let db = format!("zsmig_valgate_{tok}");
    let meta = format!("valgate_meta_{tok}");
    let schema = format!("valgate_s_{tok}");

    let admin = connect(&admin_dsn()).await;
    create_db(&admin, &db).await;

    let tmp = std::env::temp_dir().join(format!("zsmig_valgate_dir_{tok}"));
    std::fs::create_dir_all(&tmp).expect("create temp migration dir");
    let dir_s = tmp.to_str().unwrap().to_string();
    let url = bin_database_url(&db);

    // A Flyway-shaped migration that creates its own schema + table (so the Platform
    // shadow apply has a clean target). The CREATE SCHEMA is the Platform path.
    let mig = tmp.join("V0001__widgets.sql");
    std::fs::write(
        &mig,
        format!(
            "CREATE SCHEMA IF NOT EXISTS \"{schema}\";\n\
             CREATE TABLE \"{schema}\".widgets (id int);\n"
        ),
    )
    .expect("write migration body");

    let base = [
        "--dir",
        &dir_s,
        "--database-url",
        &url,
        "--profile",
        "platform",
        "--schema",
        &schema,
        "--meta-schema",
        &meta,
    ];
    let mut validate_args = vec!["validate"];
    validate_args.extend_from_slice(&base);

    // (a) CLEAN — validate before any apply: dry-run ok + drift clean → EXIT 0.
    let (ok_clean, out_clean, err_clean) = run_bin(&validate_args);
    assert!(
        ok_clean,
        "clean `validate` must exit 0\nstdout={out_clean}\nstderr={err_clean}"
    );
    assert!(
        out_clean.contains("dry-run ok=true") && out_clean.contains("drift_clean=true"),
        "clean validate prints dry-run ok + drift clean: {out_clean}"
    );

    // Apply so the journal records a checksum to drift against. `--no-dump-schema`:
    // this validate/drift e2e is not the schema-dump lifecycle (avoid a stray dump).
    let mut migrate_args = vec!["migrate"];
    migrate_args.extend_from_slice(&base);
    migrate_args.push("--no-dump-schema");
    let (ok_mig, _o, e) = run_bin(&migrate_args);
    assert!(ok_mig, "`migrate`\nstderr={e}");

    // (b) DRIFT — mutate the migration body AFTER apply; the journal holds the old
    //     checksum. validate must FLAG the drift AND exit NON-ZERO (the CI gate).
    std::fs::write(
        &mig,
        format!(
            "CREATE SCHEMA IF NOT EXISTS \"{schema}\";\n\
             CREATE TABLE \"{schema}\".widgets (id int, extra text);\n"
        ),
    )
    .expect("rewrite migration body (drift)");
    let (ok_drift, out_drift, _e) = run_bin(&validate_args);
    assert!(
        out_drift.contains("drift_clean=false") && out_drift.contains("DRIFT"),
        "validate prints the drift (output unchanged): {out_drift}"
    );
    assert!(
        !ok_drift,
        "a drifted `validate` must exit NON-ZERO so CI can gate on it: {out_drift}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
    drop_db(&admin, &db).await;
}
