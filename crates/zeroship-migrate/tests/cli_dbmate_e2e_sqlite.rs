//! Multi-engine (P7) — BINARY-level dbmate e2e on SQLite: drive the ACTUAL
//! `zeroship-migrate` binary as a SUBPROCESS through the full public dbmate
//! workflow against a temp `sqlite:` file. The faithful SQLite peer of
//! `cli_dbmate_e2e_pg.rs`: the same `new → migrate → status → down → migrate`
//! loop, dispatched onto the hardened `SqliteBackend` by the `--database-url`
//! shape — no Postgres anywhere.
//!
//!   1. `new`     — creates a dbmate file in a temp `--dir` (offline, no DB).
//!   2. (test writes a real `-- migrate:up`/`down` body into that file)
//!   3. `migrate` — DEFAULT Trusted profile applies it onto the SQLite file.
//!   4. `status`  — 1 applied / 0 pending.
//!   5. `down --yes` — rolls back the one migration.
//!   6. `migrate` — re-applies.
//!
//! Plus: an unrecognised engine URL yields the explicit "unsupported database
//! engine" refusal (the honest boundary), and a missing-`--yes` `down` refuses.
//!
//! No DB cluster required — SQLite is a local file under a per-test temp dir.

use std::path::PathBuf;
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

/// Open the migrated SQLite file directly (a fresh read-only-ish connection) and
/// answer "does table T exist on `main`?" via `sqlite_master`. Uses the
/// compio-postgres-free path: a throwaway rusqlite-style check is not available
/// here, so we re-open through the SAME hardened backend the CLI used and read
/// `sqlite_master`. To keep the test dependency-light we instead shell a tiny
/// query through the binary's own `status` output where possible; table
/// existence is verified structurally by re-opening with the public SqliteBackend.
fn table_exists(app_file: &str, table: &str) -> bool {
    use zeroship_migrate::backend_sqlite::Mode;
    use zeroship_migrate::SqliteBackend;
    let app = PathBuf::from(app_file);
    let journal = {
        let mut s = app.clone().into_os_string();
        s.push(".migrations");
        PathBuf::from(s)
    };
    // Drive the read on the compio runtime (the actor is compio-native).
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

/// The full dbmate workflow through the REAL binary on a SQLite file under the
/// DEFAULT Trusted profile: new → migrate → status → down → migrate.
#[test]
fn cli_dbmate_full_workflow_on_sqlite() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_sqlite_{tok}"));
    std::fs::create_dir_all(&root).expect("create temp root");
    // The migration dir must hold ONLY migration files — keep the SQLite app file
    // OUT of it (else `load_dir` tries to parse the `.sqlite` as a migration).
    let migrations_dir = root.join("migrations");
    std::fs::create_dir_all(&migrations_dir).expect("create temp migration dir");
    let dir_s = migrations_dir.to_str().expect("utf-8 temp dir").to_string();

    let app_file = root.join("app.sqlite");
    let app_s = app_file.to_str().expect("utf-8 app path").to_string();
    let url = format!("sqlite:{app_s}");
    let tmp = root;

    // 1. `new` — offline, default Trusted profile (identical to the PG path).
    let (ok_new, out_new, err_new) = run_bin(&["new", "widgets", "--dir", &dir_s]);
    assert!(ok_new, "`new` must exit 0\nstdout={out_new}\nstderr={err_new}");

    let created: PathBuf = std::fs::read_dir(&migrations_dir)
        .expect("read temp dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("sql"))
        .expect("`new` created a .sql file");

    // 2. Write a real dbmate up/down body. SQLite-flavoured DDL.
    std::fs::write(
        &created,
        "-- migrate:up\nCREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT);\n\n\
         -- migrate:down\nDROP TABLE widgets;\n",
    )
    .expect("write migration body");

    let base = ["--dir", &dir_s, "--database-url", &url];

    // 3. `migrate` — default Trusted profile applies it onto the SQLite file.
    let mut migrate_args = vec!["migrate"];
    migrate_args.extend_from_slice(&base);
    let (ok_mig, out_mig, err_mig) = run_bin(&migrate_args);
    assert!(
        ok_mig,
        "`migrate` (default trusted, sqlite) must exit 0\nstdout={out_mig}\nstderr={err_mig}"
    );
    assert!(
        out_mig.contains("applied 1"),
        "migrate reports 1 applied: {out_mig}"
    );
    assert!(
        table_exists(&app_s, "widgets"),
        "the widgets table materialized on the SQLite file"
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

    // 5a. `down` WITHOUT --yes must refuse (rollback is destructive) — assert the
    //     gate BEFORE the real down, while the migration is still applied.
    let mut down_noyes = vec!["down"];
    down_noyes.extend_from_slice(&base);
    let (ok_down_noyes, _o, _e) = run_bin(&down_noyes);
    assert!(
        !ok_down_noyes,
        "`down` without --yes must refuse (non-zero exit)"
    );
    assert!(
        table_exists(&app_s, "widgets"),
        "the refused `down` left the table in place"
    );

    // 5b. `down --yes` — rolls back the one migration; the table is gone.
    let mut down_args = vec!["down"];
    down_args.extend_from_slice(&base);
    down_args.push("--yes");
    let (ok_down, out_down, err_down) = run_bin(&down_args);
    assert!(
        ok_down,
        "`down --yes` (sqlite) must exit 0\nstdout={out_down}\nstderr={err_down}"
    );
    assert!(
        !table_exists(&app_s, "widgets"),
        "`down` dropped the widgets table on the SQLite file"
    );

    // status now shows 0 applied / 1 pending (the rolled-back version re-enters).
    let (ok_st2, out_st2, _e) = run_bin(&status_args);
    assert!(ok_st2, "`status` after down must exit 0: {out_st2}");
    assert!(
        out_st2.contains("applied=0") && out_st2.contains("pending=1"),
        "after down: 0 applied / 1 pending (the rolled-back version reappears): {out_st2}"
    );

    // 6. `migrate` again — re-applies; the table is back.
    let (ok_re, out_re, err_re) = run_bin(&migrate_args);
    assert!(
        ok_re,
        "re-`migrate` (sqlite) must exit 0\nstdout={out_re}\nstderr={err_re}"
    );
    assert!(
        table_exists(&app_s, "widgets"),
        "re-migrate re-created the widgets table on the SQLite file"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// An unrecognised DB engine scheme is the explicit unsupported-engine refusal —
/// the honest boundary. Only PG + SQLite are supported, so any other DSN scheme
/// must fail clearly, never panic.
#[test]
fn unknown_engine_url_is_an_explicit_unsupported_engine_error() {
    let tok = token();
    let tmp = std::env::temp_dir().join(format!("zsmig_unknown_engine_dir_{tok}"));
    std::fs::create_dir_all(&tmp).expect("create temp migration dir");
    let dir_s = tmp.to_str().unwrap().to_string();

    let (ok, _out, err) = run_bin(&[
        "migrate",
        "--dir",
        &dir_s,
        "--database-url",
        "redis://user:pw@localhost:6379/0",
    ]);
    assert!(!ok, "an unknown engine URL must NOT succeed");
    assert!(
        err.contains("unsupported database engine"),
        "the refusal names the unsupported engine: {err}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// `dump` on a SQLite URL is the honest "not supported on SQLite yet" refusal
/// (pg_dump is Postgres-only) — never a faked schema dump.
#[test]
fn dump_on_sqlite_refuses_honestly() {
    let tok = token();
    let tmp = std::env::temp_dir().join(format!("zsmig_dumpsq_dir_{tok}"));
    std::fs::create_dir_all(&tmp).expect("create temp migration dir");
    let dir_s = tmp.to_str().unwrap().to_string();
    let app_s = tmp.join("d.sqlite").to_str().unwrap().to_string();
    let url = format!("sqlite:{app_s}");

    let (ok, _out, err) = run_bin(&[
        "dump",
        "--dir",
        &dir_s,
        "--database-url",
        &url,
        "--schema-file",
        tmp.join("schema.sql").to_str().unwrap(),
    ]);
    assert!(!ok, "`dump` on SQLite must refuse (non-zero exit)");
    assert!(
        err.to_ascii_lowercase().contains("sqlite") && err.contains("dump"),
        "the refusal explains dump is unsupported on SQLite: {err}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}
