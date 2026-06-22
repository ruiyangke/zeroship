//! Schema-lifecycle dbmate parity (PG leg) — BINARY-level e2e driving the ACTUAL
//! `zeroship-migrate` binary as a subprocess against a fresh throwaway DB on the
//! `zeroship_migrate_test` cluster.
//!
//! Covers `load` (a.k.a. `db:setup`) round-trip + refusals, and the auto-refresh
//! of `schema.sql` after `migrate`/`down`. Each test uses a FRESH database and a
//! unique meta schema, dropping the DB after.

use std::path::PathBuf;
use std::process::Command;

use compio_postgres::Client;

const ADMIN_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test";

fn admin_dsn() -> String {
    std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| ADMIN_DSN.to_string())
}

fn bin_database_url(db: &str) -> String {
    format!("postgres://postgres:zeroship@localhost:5440/{db}")
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

fn run_bin(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_zeroship-migrate"))
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

async fn table_exists(db: &str, table: &str) -> bool {
    let conn = connect(&throwaway_dsn(db)).await;
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.tables WHERE table_schema='public' AND table_name=$1",
            &[&table],
        )
        .await
        .expect("query information_schema");
    !rows.is_empty()
}

fn throwaway_dsn(db: &str) -> String {
    admin_dsn()
        .split_whitespace()
        .filter(|kv| !kv.starts_with("dbname="))
        .collect::<Vec<_>>()
        .join(" ")
        + &format!(" dbname={db}")
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    compio::runtime::Runtime::new().expect("compio runtime").block_on(f)
}

/// Scaffold a migration dir with one real dbmate migration. Returns dir path.
fn scaffold(root: &std::path::Path, body: &str, name: &str) -> String {
    let migrations_dir = root.join("migrations");
    std::fs::create_dir_all(&migrations_dir).expect("create temp migration dir");
    let dir_s = migrations_dir.to_str().unwrap().to_string();
    let (ok_new, _o, e) = run_bin(&["new", name, "--dir", &dir_s]);
    assert!(ok_new, "`new`\nstderr={e}");
    let created: PathBuf = std::fs::read_dir(&migrations_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|x| x.path())
        .find(|p| p.extension().and_then(|x| x.to_str()) == Some("sql"))
        .expect("`new` created a .sql file");
    std::fs::write(&created, body).expect("write migration body");
    dir_s
}

/// `load` round-trip on PG: migrate source → dump → load into a FRESH db → assert
/// the table exists, status shows applied/nothing-pending, re-migrate is a no-op.
#[test]
fn cli_load_round_trip_on_pg() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_loadpg_{tok}"));
    let dir_s = scaffold(
        &root,
        "-- migrate:up\nCREATE TABLE widgets (id serial PRIMARY KEY, name text);\n\n\
         -- migrate:down\nDROP TABLE widgets;\n",
        "widgets",
    );

    let src_db = format!("zsmig_load_src_{tok}");
    let fresh_db = format!("zsmig_load_fresh_{tok}");
    block_on(async {
        let admin = connect(&admin_dsn()).await;
        create_db(&admin, &src_db).await;
        create_db(&admin, &fresh_db).await;
    });

    let src_url = bin_database_url(&src_db);
    let fresh_url = bin_database_url(&fresh_db);
    let schema_file = root.join("schema.sql");
    let schema_s = schema_file.to_str().unwrap().to_string();

    // migrate source. `--no-dump-schema`: we dump explicitly below to a temp path;
    // suppress the auto-refresh so the source migrate does not litter `./db/schema.sql`.
    let (ok_mig, o, e) = run_bin(&["migrate", "--dir", &dir_s, "--database-url", &src_url, "--no-dump-schema"]);
    assert!(ok_mig, "migrate src\nstdout={o}\nstderr={e}");
    // dump source.
    let (ok_dump, o, e) = run_bin(&["dump", "--dir", &dir_s, "--database-url", &src_url, "--schema-file", &schema_s]);
    assert!(ok_dump, "dump src\nstdout={o}\nstderr={e}");

    // load into the fresh DB.
    let (ok_load, o, e) = run_bin(&["load", "--dir", &dir_s, "--database-url", &fresh_url, "--schema-file", &schema_s]);
    assert!(ok_load, "load into fresh\nstdout={o}\nstderr={e}");

    assert!(block_on(table_exists(&fresh_db, "widgets")), "load recreated widgets");

    // status on the fresh DB: nothing pending.
    let (ok_st, out_st, e) = run_bin(&["status", "--dir", &dir_s, "--database-url", &fresh_url]);
    assert!(ok_st, "status\nstderr={e}");
    assert!(out_st.contains("pending=0"), "nothing pending after load: {out_st}");

    // re-migrate the fresh DB: no-op. (--no-dump-schema: avoid a stray ./db/schema.sql.)
    let (ok_re, out_re, e) = run_bin(&["migrate", "--dir", &dir_s, "--database-url", &fresh_url, "--no-dump-schema"]);
    assert!(ok_re, "re-migrate\nstderr={e}");
    assert!(out_re.contains("no-op"), "migrate after load is a no-op: {out_re}");

    block_on(async {
        let admin = connect(&admin_dsn()).await;
        drop_db(&admin, &src_db).await;
        drop_db(&admin, &fresh_db).await;
    });
    let _ = std::fs::remove_dir_all(&root);
}

/// `load` refuses to clobber an already-managed PG DB (first-entry guard).
#[test]
fn cli_load_refuses_already_managed_pg_db() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_loadpgmanaged_{tok}"));
    let dir_s = scaffold(
        &root,
        "-- migrate:up\nCREATE TABLE widgets (id serial PRIMARY KEY);\n\n\
         -- migrate:down\nDROP TABLE widgets;\n",
        "widgets",
    );
    let db = format!("zsmig_load_mgd_{tok}");
    block_on(async {
        let admin = connect(&admin_dsn()).await;
        create_db(&admin, &db).await;
    });
    let url = bin_database_url(&db);
    let schema_file = root.join("schema.sql");
    let schema_s = schema_file.to_str().unwrap().to_string();

    let (ok_mig, _o, e) = run_bin(&["migrate", "--dir", &dir_s, "--database-url", &url, "--no-dump-schema"]);
    assert!(ok_mig, "migrate\nstderr={e}");
    let (ok_dump, _o, e) = run_bin(&["dump", "--dir", &dir_s, "--database-url", &url, "--schema-file", &schema_s]);
    assert!(ok_dump, "dump\nstderr={e}");

    let (ok_load, _o, err) = run_bin(&["load", "--dir", &dir_s, "--database-url", &url, "--schema-file", &schema_s]);
    assert!(!ok_load, "load onto an already-managed DB must refuse");
    assert!(
        err.to_lowercase().contains("manage") || err.to_lowercase().contains("already"),
        "the refusal explains the DB is already managed: {err}"
    );

    block_on(async {
        let admin = connect(&admin_dsn()).await;
        drop_db(&admin, &db).await;
    });
    let _ = std::fs::remove_dir_all(&root);
}

/// Auto-dump after `migrate` on PG refreshes `schema.sql` automatically; status does not.
#[test]
fn cli_auto_dump_after_migrate_on_pg() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_autodumppg_{tok}"));
    let dir_s = scaffold(
        &root,
        "-- migrate:up\nCREATE TABLE widgets (id serial PRIMARY KEY, name text);\n\n\
         -- migrate:down\nDROP TABLE widgets;\n",
        "widgets",
    );
    let db = format!("zsmig_autodump_{tok}");
    block_on(async {
        let admin = connect(&admin_dsn()).await;
        create_db(&admin, &db).await;
    });
    let url = bin_database_url(&db);
    let schema_file = root.join("db").join("schema.sql");
    let schema_s = schema_file.to_str().unwrap().to_string();

    let (ok_mig, o, e) = run_bin(&["migrate", "--dir", &dir_s, "--database-url", &url, "--schema-file", &schema_s]);
    assert!(ok_mig, "migrate\nstdout={o}\nstderr={e}");
    assert!(schema_file.exists(), "migrate auto-dumped schema.sql");
    let dumped = std::fs::read_to_string(&schema_file).expect("read auto-dumped schema");
    assert!(dumped.contains("CREATE TABLE"), "auto-dumped schema carries DDL:\n{dumped}");
    assert!(
        dumped.contains("-- zeroship-migrate schema_migrations"),
        "auto-dumped schema carries the trailer:\n{dumped}"
    );

    // status does NOT auto-dump.
    std::fs::write(&schema_file, "SENTINEL\n").expect("sentinel");
    let (ok_st, _o, _e) = run_bin(&["status", "--dir", &dir_s, "--database-url", &url, "--schema-file", &schema_s]);
    assert!(ok_st, "status");
    assert_eq!(
        std::fs::read_to_string(&schema_file).unwrap(),
        "SENTINEL\n",
        "status must not auto-dump"
    );

    block_on(async {
        let admin = connect(&admin_dsn()).await;
        drop_db(&admin, &db).await;
    });
    let _ = std::fs::remove_dir_all(&root);
}
