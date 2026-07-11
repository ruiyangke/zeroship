//! Schema-lifecycle dbmate parity (SQLite leg) — BINARY-level e2e driving the
//! ACTUAL `zero-migrate` binary as a subprocess. Two features:
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

/// A process-wide temp "sink" CWD for `run_bin`. Auto-dump (default ON) writes
/// `./db/schema.sql` RELATIVE to the child's CWD; without this the children would
/// inherit the crate dir and litter `crates/zero-migrate/db/`. Running them from
/// a throwaway temp dir means the default `./db/schema.sql` lands in temp — still
/// FAITHFULLY exercising the default auto-dump path — and the crate tree stays clean.
/// (Tests that pass an absolute `--schema-file` or `--no-dump-schema` are unaffected;
/// every `--dir`/`--database-url`/`--schema-file` in this file is an absolute path.)
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

/// Run the binary with an explicit working directory (L3: exercise the DEFAULT
/// `./db/schema.sql` auto-dump path relative to CWD).
fn run_bin_in(cwd: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_zero-migrate"))
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("spawn the built zero-migrate binary");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Does table `T` exist on `main` of the given SQLite app file?
fn table_exists(app_file: &str, table: &str) -> bool {
    use zero_migrate::apply::backend::sqlite::Mode;
    use zero_migrate::SqliteBackend;
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

/// Count user objects on `main` (excluding internal `sqlite_*`).
fn main_user_object_count(app_file: &str) -> i64 {
    use zero_migrate::apply::backend::sqlite::Mode;
    use zero_migrate::SqliteBackend;
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
                .query("SELECT count(*) FROM main.sqlite_master WHERE name NOT LIKE 'sqlite_%'")
                .await
                .expect("count user objects");
            rows.first()
                .and_then(|r| r.first())
                .and_then(|c| c.as_deref())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)
        })
}

/// C1 RED (SQLite): `load --profile confined`/`--profile platform` must REFUSE
/// (non-zero) and execute NO DDL — same Trusted-only gate as PG. Pre-fix `load`
/// ignored `--profile` and restored the DDL onto `main` regardless — RED.
#[test]
fn cli_load_under_confined_or_platform_profile_is_refused_on_sqlite() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_loadprofsq_{tok}"));
    let (dir_s, _app_s, url) = scaffold(
        &root,
        "-- migrate:up\nCREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT);\n\n\
         -- migrate:down\nDROP TABLE widgets;\n",
        "widgets",
    );
    let base = ["--dir", &dir_s, "--database-url", &url];
    let mut migrate_args = vec!["migrate"];
    migrate_args.extend_from_slice(&base);
    let (ok_mig, _o, e) = run_bin(&migrate_args);
    assert!(ok_mig, "migrate\nstderr={e}");

    let schema_file = root.join("schema.sql");
    let schema_s = schema_file.to_str().unwrap().to_string();
    let (ok_dump, _o, e) = run_bin(&["dump", "--dir", &dir_s, "--database-url", &url, "--schema-file", &schema_s]);
    assert!(ok_dump, "dump\nstderr={e}");

    for profile in ["confined", "platform"] {
        let fresh = root.join(format!("fresh_{profile}.sqlite"));
        let fresh_s = fresh.to_str().unwrap().to_string();
        let fresh_url = format!("sqlite:{fresh_s}");
        let (ok_load, o, err) = run_bin(&[
            "load", "--dir", &dir_s, "--database-url", &fresh_url,
            "--schema-file", &schema_s, "--profile", profile,
        ]);
        assert!(!ok_load, "load --profile {profile} must refuse\nstdout={o}\nstderr={err}");
        assert!(err.to_lowercase().contains("trusted"), "refusal mentions trusted-only: {err}");
        assert!(
            !table_exists(&fresh_s, "widgets"),
            "load --profile {profile} must NOT create the widgets table on main"
        );
        assert_eq!(
            main_user_object_count(&fresh_s),
            0,
            "load --profile {profile} must leave main entirely untouched"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// H2 RED (SQLite): `load` onto a DB whose `main` already has a user table (but no
/// journal) is refused with NOTHING further mutated — the guard runs BEFORE the DDL
/// restore. Pre-fix `restore_schema_sqlite` mutated `main` first and the journal-only
/// first-entry check fired afterward, so `main` was already clobbered — RED.
#[test]
fn cli_load_onto_populated_main_without_journal_is_refused_on_sqlite() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_loadpopsq_{tok}"));
    let (dir_s, _app_s, url) = scaffold(
        &root,
        "-- migrate:up\nCREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT);\n\n\
         -- migrate:down\nDROP TABLE widgets;\n",
        "widgets",
    );
    let base = ["--dir", &dir_s, "--database-url", &url];
    let mut migrate_args = vec!["migrate"];
    migrate_args.extend_from_slice(&base);
    let (ok_mig, _o, e) = run_bin(&migrate_args);
    assert!(ok_mig, "migrate\nstderr={e}");
    let schema_file = root.join("schema.sql");
    let schema_s = schema_file.to_str().unwrap().to_string();
    let (ok_dump, _o, e) = run_bin(&["dump", "--dir", &dir_s, "--database-url", &url, "--schema-file", &schema_s]);
    assert!(ok_dump, "dump\nstderr={e}");

    // Pre-populate a FRESH target's `main` with a user table, NO journal.
    let tgt = root.join("populated.sqlite");
    let tgt_s = tgt.to_str().unwrap().to_string();
    let tgt_url = format!("sqlite:{tgt_s}");
    {
        use zero_migrate::apply::backend::sqlite::Mode;
        use zero_migrate::SqliteBackend;
        let app = PathBuf::from(&tgt_s);
        let journal = {
            let mut s = app.clone().into_os_string();
            s.push(".migrations");
            PathBuf::from(s)
        };
        compio::runtime::Runtime::new().expect("rt").block_on(async move {
            let be = SqliteBackend::open(&app, &journal).expect("open");
            be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
            be.actor()
                .exec("CREATE TABLE preexisting (id INTEGER)")
                .await
                .expect("seed user table");
        });
    }

    let (ok_load, _o, err) = run_bin(&["load", "--dir", &dir_s, "--database-url", &tgt_url, "--schema-file", &schema_s]);
    assert!(!ok_load, "load onto a populated (journal-less) main must refuse");
    assert!(
        err.to_lowercase().contains("already") || err.to_lowercase().contains("user object"),
        "refusal explains main is not fresh: {err}"
    );
    assert!(table_exists(&tgt_s, "preexisting"), "the seed table survives untouched");
    assert!(!table_exists(&tgt_s, "widgets"), "no DDL ran from the dump");
    assert_eq!(main_user_object_count(&tgt_s), 1, "only the seed table exists on main");
    let _ = std::fs::remove_dir_all(&root);
}

/// M2 RED (SQLite): `load` reconstructs the journal from the dump trailer alone, NOT
/// from `--dir`. Load from the dump with `--dir` pointing at an EMPTY directory, then
/// dump the loaded DB and assert its trailer (version+checksum+name) is byte-identical
/// to the source. Pre-fix, with no file in `--dir`, the loaded journal recorded empty
/// checksum/name and the trailers differed — RED.
#[test]
fn cli_load_reconstructs_journal_from_trailer_not_dir_on_sqlite() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_loadindepsq_{tok}"));
    let (dir_s, _app_s, url) = scaffold(
        &root,
        "-- migrate:up\nCREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT);\n\n\
         -- migrate:down\nDROP TABLE widgets;\n",
        "widgets",
    );
    let empty_dir = root.join("empty_migrations");
    std::fs::create_dir_all(&empty_dir).expect("mkdir empty");
    let empty_dir_s = empty_dir.to_str().unwrap().to_string();

    let base = ["--dir", &dir_s, "--database-url", &url];
    let mut migrate_args = vec!["migrate"];
    migrate_args.extend_from_slice(&base);
    let (ok_mig, _o, e) = run_bin(&migrate_args);
    assert!(ok_mig, "migrate\nstderr={e}");
    let schema_file = root.join("schema.sql");
    let schema_s = schema_file.to_str().unwrap().to_string();
    let (ok_dump, _o, e) = run_bin(&["dump", "--dir", &dir_s, "--database-url", &url, "--schema-file", &schema_s]);
    assert!(ok_dump, "dump\nstderr={e}");
    let src_dump = std::fs::read_to_string(&schema_file).expect("read src dump");
    let src_trailer = src_dump
        .split_once("-- zero-migrate schema_migrations")
        .map(|(_, t)| t.to_string())
        .expect("src trailer");

    let fresh = root.join("fresh.sqlite");
    let fresh_url = format!("sqlite:{}", fresh.to_str().unwrap());
    let (ok_load, o, e) = run_bin(&["load", "--dir", &empty_dir_s, "--database-url", &fresh_url, "--schema-file", &schema_s]);
    assert!(ok_load, "load (empty --dir)\nstdout={o}\nstderr={e}");

    let schema2 = root.join("schema2.sql");
    let schema2_s = schema2.to_str().unwrap().to_string();
    let (ok_d2, o, e) = run_bin(&["dump", "--dir", &empty_dir_s, "--database-url", &fresh_url, "--schema-file", &schema2_s]);
    assert!(ok_d2, "dump loaded\nstdout={o}\nstderr={e}");
    let loaded_dump = std::fs::read_to_string(&schema2).expect("read loaded dump");
    let loaded_trailer = loaded_dump
        .split_once("-- zero-migrate schema_migrations")
        .map(|(_, t)| t.to_string())
        .expect("loaded trailer");
    assert_eq!(
        loaded_trailer, src_trailer,
        "the loaded journal's trailer is reconstructed from the dump trailer alone (empty --dir)"
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
        after_migrate.contains("-- zero-migrate schema_migrations"),
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

/// L3: the DEFAULT auto-dump-ON path (no `--no-dump-schema`, default
/// `./db/schema.sql` relative to CWD) — the path most users actually hit. `migrate`
/// auto-writes `db/schema.sql`; that dump then round-trips (dump→load→dump stable).
/// The other apply e2e suites all opt OUT via `--no-dump-schema`, so this is the one
/// covering the default ON behavior end-to-end.
#[test]
fn cli_default_auto_dump_on_round_trips_on_sqlite() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_defdump_{tok}"));
    let (dir_s, _app_s, url) = scaffold(
        &root,
        "-- migrate:up\nCREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT);\n\
         CREATE INDEX idx_widgets_name ON widgets (name);\n\n\
         -- migrate:down\nDROP TABLE widgets;\n",
        "widgets",
    );

    // migrate with NO --schema-file and NO --no-dump-schema: the DEFAULT auto-dump
    // ON writes `./db/schema.sql` relative to the CWD.
    let (ok_mig, o, e) = run_bin_in(&root, &["migrate", "--dir", &dir_s, "--database-url", &url]);
    assert!(ok_mig, "migrate (default auto-dump)\nstdout={o}\nstderr={e}");
    let default_schema = root.join("db").join("schema.sql");
    assert!(
        default_schema.exists(),
        "default auto-dump wrote ./db/schema.sql at {}",
        default_schema.display()
    );
    let dump1 = std::fs::read_to_string(&default_schema).expect("read default-dumped schema");
    assert!(dump1.contains("CREATE TABLE widgets"), "default dump carries DDL:\n{dump1}");
    assert!(
        dump1.contains("-- zero-migrate schema_migrations"),
        "default dump carries the trailer:\n{dump1}"
    );

    // The default-dumped schema.sql round-trips: load it into a fresh DB, dump that,
    // and the two dumps are byte-stable.
    let fresh = root.join("fresh.sqlite");
    let fresh_url = format!("sqlite:{}", fresh.to_str().unwrap());
    let schema_s = default_schema.to_str().unwrap().to_string();
    let (ok_load, o, e) = run_bin(&["load", "--dir", &dir_s, "--database-url", &fresh_url, "--schema-file", &schema_s]);
    assert!(ok_load, "load default dump\nstdout={o}\nstderr={e}");
    let schema2 = root.join("schema2.sql");
    let schema2_s = schema2.to_str().unwrap().to_string();
    let (ok_d2, o, e) = run_bin(&["dump", "--dir", &dir_s, "--database-url", &fresh_url, "--schema-file", &schema2_s]);
    assert!(ok_d2, "dump loaded\nstdout={o}\nstderr={e}");
    let dump2 = std::fs::read_to_string(&schema2).expect("read loaded dump");
    assert_eq!(dump1, dump2, "the default auto-dumped schema round-trips byte-stably");

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
