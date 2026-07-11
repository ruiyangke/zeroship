#![cfg(feature = "standalone-cli")]
//! BINARY-level e2e for the `zero-migrate` subcommands that were previously
//! ONLY unit/library-tested — driven through the REAL binary as a subprocess on a
//! fresh throwaway Postgres DB, asserting real DB state (table existence) + `status`
//! counts. The Postgres peer of `cli_uncovered_e2e_sqlite.rs`. Covers:
//!
//!   * `up` (alias of `migrate`) — applies all pending, identical to `migrate`.
//!   * `rollback --steps <N> --yes` — reverses exactly the N most-recent.
//!   * `rollback --to <version> --yes` — reverses everything strictly AFTER v1.
//!   * `wait` — returns 0 promptly against a ready DB; times out non-zero (within a
//!     small budget) against an unreachable DSN.
//!
//! Each test uses a FRESH database `zsmig_unc_<token>` on the `zero_migrate_test`
//! cluster and a unique meta schema, then drops the DB.

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

/// A process-wide temp "sink" CWD for `run_bin` (see the other cli_* suites): keeps
/// the default `./db/schema.sql` auto-dump out of the crate tree. Every
/// `--dir`/`--database-url`/`--meta-schema` here is absolute / explicit.
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

async fn table_exists(conn: &Client, table: &str) -> bool {
    !conn
        .query(
            "SELECT 1 FROM information_schema.tables WHERE table_schema='public' AND table_name=$1",
            &[&table],
        )
        .await
        .expect("query table existence")
        .is_empty()
}

/// Write THREE dbmate migrations with EXPLICIT, distinct 14-digit versions (so the
/// numeric `--to <version>` is deterministic). Each creates one table (`t1`/`t2`/`t3`)
/// in `public` and drops it on `down`. Returns the migrations dir + the three numeric
/// versions.
fn scaffold_three(root: &std::path::Path) -> (String, [u64; 3]) {
    let migrations_dir = root.join("migrations");
    std::fs::create_dir_all(&migrations_dir).expect("create temp migration dir");
    let dir_s = migrations_dir.to_str().unwrap().to_string();
    let versions: [u64; 3] = [20_240_101_000_001, 20_240_101_000_002, 20_240_101_000_003];
    for (i, v) in versions.iter().enumerate() {
        let n = i + 1;
        std::fs::write(
            migrations_dir.join(format!("{v}_t{n}.sql")),
            format!(
                "-- migrate:up\nCREATE TABLE t{n} (id bigserial primary key);\n\n\
                 -- migrate:down\nDROP TABLE t{n};\n"
            ),
        )
        .expect("write migration");
    }
    (dir_s, versions)
}

/// `up` is an exact alias of `migrate`: applies ALL pending, `status` reflects them
/// (applied=3 / pending=0), every table present, and a second `up` is a no-op.
#[compio::test]
async fn cli_up_alias_applies_all_pending_like_migrate_on_pg() {
    let tok = token();
    let db = format!("zsmig_unc_up_{tok}");
    let meta = format!("unc_up_meta_{tok}");
    let admin = connect(&admin_dsn()).await;
    create_db(&admin, &db).await;

    let root = std::env::temp_dir().join(format!("zsmig_unc_up_dir_{tok}"));
    let (dir_s, _v) = scaffold_three(&root);
    let url = bin_database_url(&db);
    let base = [
        "--dir", &dir_s, "--database-url", &url, "--meta-schema", &meta, "--no-dump-schema",
    ];

    let mut up_args = vec!["up"];
    up_args.extend_from_slice(&base);
    let (ok_up, out_up, err_up) = run_bin(&up_args);
    assert!(ok_up, "`up` must exit 0\nstdout={out_up}\nstderr={err_up}");
    assert!(out_up.contains("applied 3"), "`up` reports 3 applied: {out_up}");

    let app = connect(&throwaway_dsn(&db)).await;
    for t in ["t1", "t2", "t3"] {
        assert!(table_exists(&app, t).await, "`up` created {t}");
    }

    let mut status_args = vec!["status"];
    status_args.extend_from_slice(&base);
    let (ok_st, out_st, err_st) = run_bin(&status_args);
    assert!(ok_st, "`status` must exit 0\nstdout={out_st}\nstderr={err_st}");
    assert!(
        out_st.contains("applied=3") && out_st.contains("pending=0"),
        "after `up`: 3 applied / 0 pending: {out_st}"
    );

    let (ok_up2, out_up2, _e) = run_bin(&up_args);
    assert!(ok_up2, "second `up` exits 0: {out_up2}");
    assert!(out_up2.contains("no-op"), "second `up` is a no-op: {out_up2}");

    let _ = std::fs::remove_dir_all(&root);
    drop_db(&admin, &db).await;
}

/// `rollback --steps 2 --yes` reverses EXACTLY the two most-recent (t3, t2) and keeps
/// the first (t1). Real table state + `status` (applied=1 / pending=2).
#[compio::test]
async fn cli_rollback_steps_reverses_the_n_most_recent_on_pg() {
    let tok = token();
    let db = format!("zsmig_unc_rbs_{tok}");
    let meta = format!("unc_rbs_meta_{tok}");
    let admin = connect(&admin_dsn()).await;
    create_db(&admin, &db).await;

    let root = std::env::temp_dir().join(format!("zsmig_unc_rbs_dir_{tok}"));
    let (dir_s, _v) = scaffold_three(&root);
    let url = bin_database_url(&db);
    let base = [
        "--dir", &dir_s, "--database-url", &url, "--meta-schema", &meta, "--no-dump-schema",
    ];

    let mut migrate_args = vec!["migrate"];
    migrate_args.extend_from_slice(&base);
    let (ok_mig, o, e) = run_bin(&migrate_args);
    assert!(ok_mig, "migrate (3)\nstdout={o}\nstderr={e}");

    let app = connect(&throwaway_dsn(&db)).await;
    for t in ["t1", "t2", "t3"] {
        assert!(table_exists(&app, t).await, "migrate created {t}");
    }

    let mut rb_args = vec!["rollback", "--steps", "2", "--yes"];
    rb_args.extend_from_slice(&base);
    let (ok_rb, out_rb, err_rb) = run_bin(&rb_args);
    assert!(
        ok_rb,
        "`rollback --steps 2 --yes` must exit 0\nstdout={out_rb}\nstderr={err_rb}"
    );

    assert!(table_exists(&app, "t1").await, "t1 (oldest) survives --steps 2");
    assert!(!table_exists(&app, "t2").await, "t2 reversed by --steps 2");
    assert!(!table_exists(&app, "t3").await, "t3 reversed by --steps 2");

    let mut status_args = vec!["status"];
    status_args.extend_from_slice(&base);
    let (_ok, out_st, _e) = run_bin(&status_args);
    assert!(
        out_st.contains("applied=1") && out_st.contains("pending=2"),
        "after --steps 2: 1 applied / 2 pending: {out_st}"
    );

    let _ = std::fs::remove_dir_all(&root);
    drop_db(&admin, &db).await;
}

/// `rollback --to <v1> --yes` reverses everything STRICTLY AFTER v1 (t3, t2) and
/// keeps v1 (t1). `--to` takes the 14-digit numeric file version.
#[compio::test]
async fn cli_rollback_to_version_reverses_everything_after_it_on_pg() {
    let tok = token();
    let db = format!("zsmig_unc_rbt_{tok}");
    let meta = format!("unc_rbt_meta_{tok}");
    let admin = connect(&admin_dsn()).await;
    create_db(&admin, &db).await;

    let root = std::env::temp_dir().join(format!("zsmig_unc_rbt_dir_{tok}"));
    let (dir_s, versions) = scaffold_three(&root);
    let url = bin_database_url(&db);
    let base = [
        "--dir", &dir_s, "--database-url", &url, "--meta-schema", &meta, "--no-dump-schema",
    ];

    let mut migrate_args = vec!["migrate"];
    migrate_args.extend_from_slice(&base);
    let (ok_mig, o, e) = run_bin(&migrate_args);
    assert!(ok_mig, "migrate (3)\nstdout={o}\nstderr={e}");

    let app = connect(&throwaway_dsn(&db)).await;

    let v1 = versions[0].to_string();
    let mut rb_args = vec!["rollback", "--to", &v1, "--yes"];
    rb_args.extend_from_slice(&base);
    let (ok_rb, out_rb, err_rb) = run_bin(&rb_args);
    assert!(
        ok_rb,
        "`rollback --to {v1} --yes` must exit 0\nstdout={out_rb}\nstderr={err_rb}"
    );

    assert!(table_exists(&app, "t1").await, "v1 (t1) is KEPT by --to v1");
    assert!(!table_exists(&app, "t2").await, "v2 (t2) reversed (after v1)");
    assert!(!table_exists(&app, "t3").await, "v3 (t3) reversed (after v1)");

    let mut status_args = vec!["status"];
    status_args.extend_from_slice(&base);
    let (_ok, out_st, _e) = run_bin(&status_args);
    assert!(
        out_st.contains("applied=1") && out_st.contains("pending=2"),
        "after --to v1: 1 applied / 2 pending: {out_st}"
    );

    let _ = std::fs::remove_dir_all(&root);
    drop_db(&admin, &db).await;
}

/// `rollback` is ALWAYS destructive ⇒ it must REFUSE without `--yes` (non-zero) and
/// reverse NOTHING.
#[compio::test]
async fn cli_rollback_without_yes_refuses_and_reverses_nothing_on_pg() {
    let tok = token();
    let db = format!("zsmig_unc_rbg_{tok}");
    let meta = format!("unc_rbg_meta_{tok}");
    let admin = connect(&admin_dsn()).await;
    create_db(&admin, &db).await;

    let root = std::env::temp_dir().join(format!("zsmig_unc_rbg_dir_{tok}"));
    let (dir_s, _v) = scaffold_three(&root);
    let url = bin_database_url(&db);
    let base = [
        "--dir", &dir_s, "--database-url", &url, "--meta-schema", &meta, "--no-dump-schema",
    ];

    let mut migrate_args = vec!["migrate"];
    migrate_args.extend_from_slice(&base);
    let (ok_mig, _o, e) = run_bin(&migrate_args);
    assert!(ok_mig, "migrate\nstderr={e}");

    let mut rb_args = vec!["rollback", "--steps", "1"];
    rb_args.extend_from_slice(&base);
    let (ok_rb, _o, _e) = run_bin(&rb_args);
    assert!(!ok_rb, "`rollback` without --yes must refuse (non-zero)");

    let app = connect(&throwaway_dsn(&db)).await;
    for t in ["t1", "t2", "t3"] {
        assert!(table_exists(&app, t).await, "refused rollback left {t} in place");
    }

    let _ = std::fs::remove_dir_all(&root);
    drop_db(&admin, &db).await;
}

/// `wait` against a READY DB returns 0 promptly; `wait --timeout-secs <small>`
/// against an UNREACHABLE DSN exits non-zero WITHIN the timeout (does not hang).
#[compio::test]
async fn cli_wait_ready_returns_zero_then_unreachable_times_out_nonzero() {
    let tok = token();
    let db = format!("zsmig_unc_wait_{tok}");
    let admin = connect(&admin_dsn()).await;
    create_db(&admin, &db).await;
    let url = bin_database_url(&db);

    // (a) READY — `wait` succeeds promptly (first probe answers `SELECT 1`).
    let start = std::time::Instant::now();
    let (ok, out, err) = run_bin(&["wait", "--database-url", &url, "--timeout-secs", "10"]);
    let elapsed = start.elapsed();
    assert!(ok, "`wait` on a ready DB must exit 0\nstdout={out}\nstderr={err}");
    assert!(out.contains("ready"), "`wait` prints the ready message: {out}");
    assert!(
        elapsed < std::time::Duration::from_secs(8),
        "ready `wait` returned promptly (not after polling to the deadline): {elapsed:?}"
    );

    // (b) UNREACHABLE — a closed port. `wait --timeout-secs 2` must exit NON-ZERO
    //     and return WITHIN a small window (timeout + slack), proving it does not
    //     hang the suite.
    let dead = "postgres://postgres:zeroship@127.0.0.1:1/nope";
    let start2 = std::time::Instant::now();
    let (ok2, _o2, err2) = run_bin(&["wait", "--database-url", dead, "--timeout-secs", "2"]);
    let elapsed2 = start2.elapsed();
    assert!(!ok2, "`wait` against an unreachable DSN must exit non-zero");
    assert!(
        elapsed2 < std::time::Duration::from_secs(20),
        "the timed-out `wait` returned within a bounded window (did not hang): {elapsed2:?}"
    );
    assert!(
        err2.to_lowercase().contains("wait")
            || err2.to_lowercase().contains("timed out")
            || err2.to_lowercase().contains("timeout"),
        "the failure names the wait timeout: {err2}"
    );

    drop_db(&admin, &db).await;
}
