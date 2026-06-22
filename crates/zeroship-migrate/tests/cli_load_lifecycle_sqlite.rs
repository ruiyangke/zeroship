//! Schema-lifecycle dbmate parity (SQLite leg) — BINARY-level e2e driving the
//! ACTUAL `zeroship-migrate` binary as a subprocess. Two features:
//!
//! 1. `load` (a.k.a. `db:setup`): replay a dumped `schema.sql` onto a FRESH empty
//!    DB FAST (no per-migration replay), then reconstruct the journal from the
//!    dump's applied-versions trailer so `status` reports those versions applied
//!    with nothing pending and a subsequent `migrate` is a no-op.
//! 2. Auto-refresh of `schema.sql` after a successful `migrate`/`up`/`rollback`/
//!    `down` (dbmate parity), with `--no-dump-schema` to opt out, and read-only
//!    commands (`status`) NOT triggering a dump.
//!
//! No DB cluster needed — SQLite is a local temp file.

use std::path::{Path, PathBuf};
use std::process::Command;

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

/// Does table `T` exist on `main` of the given SQLite app file?
fn table_exists(app_file: &str, table: &str) -> bool {
    use zeroship_migrate::backend_sqlite::Mode;
    use zeroship_migrate::SqliteBackend;
    let app = PathBuf::from(app_file);
    let journal = {
        let mut s = app.clone().into_os_string();
        s.push(".migrations");
        PathBuf::from(s)
    };
    compio::runtime::Runtime::new()
        .expect("compio runtime")
        .block_on(async move {
            let be = SqliteBackend::open(&app, &journal).expect("re-open sqlite backend");
            be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
            let rows = be
                .actor()
                .query(&format!(
                    "SELECT name FROM main.sqlite_master WHERE type='table' AND name='{table}'"
                ))
                .await
                .expect("query sqlite_master");
            !rows.is_empty()
        })
}

/// Scaffold a migration dir with one real dbmate migration; return (dir, app_url).
fn scaffold(root: &Path, body: &str, name: &str) -> (String, String, String) {
    let migrations_dir = root.join("migrations");
    std::fs::create_dir_all(&migrations_dir).expect("create temp migration dir");
    let dir_s = migrations_dir.to_str().unwrap().to_string();
    let (ok_new, _o, e) = run_bin(&["new", name, "--dir", &dir_s]);
    assert!(ok_new, "`new` must exit 0\nstderr={e}");
    let created: PathBuf = std::fs::read_dir(&migrations_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|x| x.path())
        .find(|p| p.extension().and_then(|x| x.to_str()) == Some("sql"))
        .expect("`new` created a .sql file");
    std::fs::write(&created, body).expect("write migration body");
    let app_file = root.join("app.sqlite");
    let app_s = app_file.to_str().unwrap().to_string();
    let url = format!("sqlite:{app_s}");
    (dir_s, app_s, url)
}

/// `load` round-trip on SQLite: migrate → dump → load into a FRESH db → assert the
/// tables exist, `status` shows the versions applied with nothing pending, and a
/// subsequent `migrate` is a no-op.
#[test]
fn cli_load_round_trip_on_sqlite() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_loadsq_{tok}"));
    let (dir_s, app_s, url) = scaffold(
        &root,
        "-- migrate:up\nCREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT);\n\
         CREATE INDEX idx_widgets_name ON widgets (name);\n\n\
         -- migrate:down\nDROP TABLE widgets;\n",
        "widgets",
    );
    let base = ["--dir", &dir_s, "--database-url", &url];

    // migrate the source DB.
    let mut migrate_args = vec!["migrate"];
    migrate_args.extend_from_slice(&base);
    let (ok_mig, o, e) = run_bin(&migrate_args);
    assert!(ok_mig, "migrate must exit 0\nstdout={o}\nstderr={e}");

    // dump → schema.sql.
    let schema_file = root.join("schema.sql");
    let schema_s = schema_file.to_str().unwrap().to_string();
    let (ok_dump, o, e) = run_bin(&["dump", "--dir", &dir_s, "--database-url", &url, "--schema-file", &schema_s]);
    assert!(ok_dump, "dump must exit 0\nstdout={o}\nstderr={e}");

    // capture the applied version off `status` for later no-op assertion.
    let mut status_args = vec!["status"];
    status_args.extend_from_slice(&base);
    let (_ok, out_st, _e) = run_bin(&status_args);
    let applied_version: String = out_st
        .lines()
        .find_map(|l| l.trim().strip_prefix("applied  "))
        .map(|rest| rest.split_whitespace().next().unwrap_or("").to_string())
        .expect("status reports an applied version");

    // --- load into a FRESH empty DB ----------------------------------------
    let fresh_app = root.join("fresh.sqlite");
    let fresh_app_s = fresh_app.to_str().unwrap().to_string();
    let fresh_url = format!("sqlite:{fresh_app_s}");
    let load_base = ["--dir", &dir_s, "--database-url", &fresh_url];
    let (ok_load, o, e) = run_bin(&["load", "--dir", &dir_s, "--database-url", &fresh_url, "--schema-file", &schema_s]);
    assert!(ok_load, "`load` into a fresh sqlite must exit 0\nstdout={o}\nstderr={e}");

    // (a) the tables materialized from the dump DDL.
    assert!(table_exists(&fresh_app_s, "widgets"), "load recreated the widgets table");

    // (b) status shows the trailer version applied, nothing pending.
    let mut fresh_status = vec!["status"];
    fresh_status.extend_from_slice(&load_base);
    let (ok_st, out_fst, e) = run_bin(&fresh_status);
    assert!(ok_st, "status after load exits 0\nstderr={e}");
    assert!(
        out_fst.contains("pending=0"),
        "after load nothing is pending: {out_fst}"
    );
    assert!(
        out_fst.contains(&format!("applied  {applied_version}")) || out_fst.contains("applied=1"),
        "after load the trailer version is applied: {out_fst}"
    );

    // (c) a subsequent migrate is a no-op.
    let mut fresh_migrate = vec!["migrate"];
    fresh_migrate.extend_from_slice(&load_base);
    let (ok_re, out_re, e) = run_bin(&fresh_migrate);
    assert!(ok_re, "migrate after load exits 0\nstderr={e}");
    assert!(
        out_re.contains("no-op"),
        "migrate after load is a no-op (the trailer version is already applied): {out_re}"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = app_s; // silence unused on some paths
}

/// `load` refuses when the schema file is missing/unreadable (non-zero).
#[test]
fn cli_load_refuses_missing_schema_file() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_loadmiss_{tok}"));
    let migrations_dir = root.join("migrations");
    std::fs::create_dir_all(&migrations_dir).expect("mkdir");
    let dir_s = migrations_dir.to_str().unwrap().to_string();
    let app = root.join("app.sqlite");
    let url = format!("sqlite:{}", app.to_str().unwrap());
    let missing = root.join("nope.sql");
    let missing_s = missing.to_str().unwrap().to_string();

    let (ok, _o, err) = run_bin(&["load", "--dir", &dir_s, "--database-url", &url, "--schema-file", &missing_s]);
    assert!(!ok, "load with a missing schema file must exit non-zero");
    assert!(
        err.to_lowercase().contains("schema") || err.to_lowercase().contains("read"),
        "the refusal mentions the schema file: {err}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `load` refuses to clobber an already-populated/managed DB (mirrors baseline's
/// first-entry guard): loading onto a DB that already has applied migrations is
/// a non-zero refusal.
#[test]
fn cli_load_refuses_already_managed_db() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_loadmanaged_{tok}"));
    let (dir_s, _app_s, url) = scaffold(
        &root,
        "-- migrate:up\nCREATE TABLE widgets (id INTEGER PRIMARY KEY);\n\n\
         -- migrate:down\nDROP TABLE widgets;\n",
        "widgets",
    );
    let base = ["--dir", &dir_s, "--database-url", &url];

    // migrate so the DB is now engine-managed.
    let mut migrate_args = vec!["migrate"];
    migrate_args.extend_from_slice(&base);
    let (ok_mig, _o, e) = run_bin(&migrate_args);
    assert!(ok_mig, "migrate\nstderr={e}");

    // dump it.
    let schema_file = root.join("schema.sql");
    let schema_s = schema_file.to_str().unwrap().to_string();
    let (ok_dump, _o, e) = run_bin(&["dump", "--dir", &dir_s, "--database-url", &url, "--schema-file", &schema_s]);
    assert!(ok_dump, "dump\nstderr={e}");

    // load onto the SAME already-managed DB → refuse (no clobber).
    let (ok_load, _o, err) = run_bin(&["load", "--dir", &dir_s, "--database-url", &url, "--schema-file", &schema_s]);
    assert!(!ok_load, "load onto an already-managed DB must refuse (non-zero)");
    assert!(
        err.to_lowercase().contains("manage") || err.to_lowercase().contains("already"),
        "the refusal explains the DB is already managed: {err}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Auto-dump after `migrate` (dbmate parity): `migrate` with NO explicit `dump`
/// call refreshes `schema.sql` automatically; `--no-dump-schema` suppresses it;
/// `down` also refreshes; a read-only `status` does NOT.
#[test]
fn cli_auto_dump_after_migrate_and_down_but_not_status() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_autodump_{tok}"));
    let (dir_s, _app_s, url) = scaffold(
        &root,
        "-- migrate:up\nCREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT);\n\n\
         -- migrate:down\nDROP TABLE widgets;\n",
        "widgets",
    );
    let schema_file = root.join("db").join("schema.sql");
    let schema_s = schema_file.to_str().unwrap().to_string();
    let base = ["--dir", &dir_s, "--database-url", &url, "--schema-file", &schema_s];

    // migrate (no `dump`) auto-writes schema.sql.
    let mut migrate_args = vec!["migrate"];
    migrate_args.extend_from_slice(&base);
    let (ok_mig, o, e) = run_bin(&migrate_args);
    assert!(ok_mig, "migrate\nstdout={o}\nstderr={e}");
    assert!(
        schema_file.exists(),
        "migrate auto-dumped schema.sql at {schema_s}"
    );
    let after_migrate = std::fs::read_to_string(&schema_file).expect("read auto-dumped schema");
    assert!(
        after_migrate.contains("CREATE TABLE widgets"),
        "auto-dumped schema carries the table DDL:\n{after_migrate}"
    );
    assert!(
        after_migrate.contains("-- zeroship-migrate schema_migrations"),
        "auto-dumped schema carries the applied-versions trailer:\n{after_migrate}"
    );

    // status does NOT change the schema file (read-only).
    std::fs::write(&schema_file, "SENTINEL-UNCHANGED\n").expect("overwrite sentinel");
    let mut status_args = vec!["status"];
    status_args.extend_from_slice(&base);
    let (ok_st, _o, _e) = run_bin(&status_args);
    assert!(ok_st, "status exits 0");
    let after_status = std::fs::read_to_string(&schema_file).expect("read schema after status");
    assert_eq!(
        after_status, "SENTINEL-UNCHANGED\n",
        "a read-only status must NOT auto-dump (sentinel preserved)"
    );

    // down (--yes) auto-refreshes the schema file (table now gone).
    let mut down_args = vec!["down"];
    down_args.extend_from_slice(&base);
    down_args.push("--yes");
    let (ok_down, o, e) = run_bin(&down_args);
    assert!(ok_down, "down --yes\nstdout={o}\nstderr={e}");
    let after_down = std::fs::read_to_string(&schema_file).expect("read schema after down");
    assert_ne!(after_down, "SENTINEL-UNCHANGED\n", "down auto-refreshed the schema file");
    assert!(
        !after_down.contains("CREATE TABLE widgets"),
        "after `down` the auto-dumped schema no longer has the dropped table:\n{after_down}"
    );

    // re-migrate, this time with --no-dump-schema: the schema file is NOT touched.
    std::fs::write(&schema_file, "SENTINEL-NODUMP\n").expect("overwrite sentinel 2");
    let mut nodump_args = vec!["migrate"];
    nodump_args.extend_from_slice(&base);
    nodump_args.push("--no-dump-schema");
    let (ok_nd, o, e) = run_bin(&nodump_args);
    assert!(ok_nd, "migrate --no-dump-schema\nstdout={o}\nstderr={e}");
    let after_nd = std::fs::read_to_string(&schema_file).expect("read schema after --no-dump-schema");
    assert_eq!(
        after_nd, "SENTINEL-NODUMP\n",
        "--no-dump-schema suppressed the auto-dump (sentinel preserved)"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Round-trip faithfulness: dump → load → dump yields a byte-stable schema.sql.
#[test]
fn cli_dump_load_dump_is_stable_on_sqlite() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_rtstable_{tok}"));
    let (dir_s, _app_s, url) = scaffold(
        &root,
        "-- migrate:up\nCREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT);\n\
         CREATE INDEX idx_widgets_name ON widgets (name);\n\n\
         -- migrate:down\nDROP TABLE widgets;\n",
        "widgets",
    );
    let base = ["--dir", &dir_s, "--database-url", &url];
    let mut migrate_args = vec!["migrate"];
    migrate_args.extend_from_slice(&base);
    let (ok_mig, _o, e) = run_bin(&migrate_args);
    assert!(ok_mig, "migrate\nstderr={e}");

    let schema1 = root.join("schema1.sql");
    let schema1_s = schema1.to_str().unwrap().to_string();
    let (ok_d1, _o, e) = run_bin(&["dump", "--dir", &dir_s, "--database-url", &url, "--schema-file", &schema1_s]);
    assert!(ok_d1, "dump #1\nstderr={e}");
    let dump1 = std::fs::read_to_string(&schema1).expect("read dump1");

    // load into a fresh DB, then dump that.
    let fresh = root.join("fresh.sqlite");
    let fresh_url = format!("sqlite:{}", fresh.to_str().unwrap());
    let (ok_load, _o, e) = run_bin(&["load", "--dir", &dir_s, "--database-url", &fresh_url, "--schema-file", &schema1_s]);
    assert!(ok_load, "load\nstderr={e}");
    let schema2 = root.join("schema2.sql");
    let schema2_s = schema2.to_str().unwrap().to_string();
    let (ok_d2, _o, e) = run_bin(&["dump", "--dir", &dir_s, "--database-url", &fresh_url, "--schema-file", &schema2_s]);
    assert!(ok_d2, "dump #2\nstderr={e}");
    let dump2 = std::fs::read_to_string(&schema2).expect("read dump2");

    assert_eq!(dump1, dump2, "dump→load→dump must produce an identical schema.sql");

    let _ = std::fs::remove_dir_all(&root);
}
