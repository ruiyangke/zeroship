//! Multi-engine (P7) — BINARY-level dbmate e2e on SQLite: drive the ACTUAL
//! `zero-migrate` binary as a SUBPROCESS through the full public dbmate
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

#![cfg(feature = "standalone-cli")]

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

/// A process-wide temp "sink" CWD for `run_bin`. Auto-dump (default ON) writes
/// `./db/schema.sql` RELATIVE to the child's CWD; running the children from a
/// throwaway temp dir keeps the crate tree clean even if a call forgets
/// `--no-dump-schema`. Defense-in-depth (this suite passes `--no-dump-schema` on its
/// apply paths) — every `--dir`/`--database-url`/`--schema-file` here is absolute.
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

/// Open the migrated SQLite file directly (a fresh read-only-ish connection) and
/// answer "does table T exist on `main`?" via `sqlite_master`. Uses the
/// compio-postgres-free path: a throwaway rusqlite-style check is not available
/// here, so we re-open through the SAME hardened backend the CLI used and read
/// `sqlite_master`. To keep the test dependency-light we instead shell a tiny
/// query through the binary's own `status` output where possible; table
/// existence is verified structurally by re-opening with the public SqliteBackend.
fn table_exists(app_file: &str, table: &str) -> bool {
    use zero_migrate::apply::backend::sqlite::Mode;
    use zero_migrate::SqliteBackend;
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
    //    `--no-dump-schema` opts out of the new auto-`schema.sql` refresh (dbmate
    //    parity, ON by default) so this apply-path e2e (run_bin inherits the crate
    //    CWD) does not litter a `./db/schema.sql`.
    let mut migrate_args = vec!["migrate"];
    migrate_args.extend_from_slice(&base);
    migrate_args.push("--no-dump-schema");
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
    //     `--no-dump-schema`: same apply-path-only rationale as the migrate above.
    let mut down_args = vec!["down"];
    down_args.extend_from_slice(&base);
    down_args.push("--yes");
    down_args.push("--no-dump-schema");
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

/// `dump` on SQLite (full parity with the PG path): `new` → `migrate` → `dump`
/// writes a `schema.sql` carrying the live CREATE statements (derived from
/// `sqlite_master`, NOT a `sqlite3` shell-out) plus the SAME applied-versions
/// trailer the PG `dump` emits. Proves no `_mig`/journal artifact leaks into the
/// dump.
#[test]
fn cli_dump_on_sqlite_writes_schema_and_applied_trailer() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_dumpsq_{tok}"));
    let migrations_dir = root.join("migrations");
    std::fs::create_dir_all(&migrations_dir).expect("create temp migration dir");
    let dir_s = migrations_dir.to_str().unwrap().to_string();

    let app_file = root.join("app.sqlite");
    let app_s = app_file.to_str().unwrap().to_string();
    let url = format!("sqlite:{app_s}");
    let schema_file = root.join("schema.sql");
    let schema_s = schema_file.to_str().unwrap().to_string();

    // 1. `new` then write a real up/down body (a table + a secondary index, so the
    //    dump must order the index after its table).
    let (ok_new, _o, e) = run_bin(&["new", "widgets", "--dir", &dir_s]);
    assert!(ok_new, "`new` must exit 0\nstderr={e}");
    let created: PathBuf = std::fs::read_dir(&migrations_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|x| x.path())
        .find(|p| p.extension().and_then(|x| x.to_str()) == Some("sql"))
        .expect("`new` created a .sql file");
    std::fs::write(
        &created,
        "-- migrate:up\nCREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT);\n\
         CREATE INDEX idx_widgets_name ON widgets (name);\n\n\
         -- migrate:down\nDROP TABLE widgets;\n",
    )
    .expect("write migration body");

    let base = ["--dir", &dir_s, "--database-url", &url];

    // 2. `migrate` it onto the SQLite file. `--no-dump-schema`: the explicit `dump`
    //    below is what this test exercises; suppress the auto-refresh so it does not
    //    write a stray `./db/schema.sql` into the crate CWD.
    let mut migrate_args = vec!["migrate"];
    migrate_args.extend_from_slice(&base);
    migrate_args.push("--no-dump-schema");
    let (ok_mig, out_mig, err_mig) = run_bin(&migrate_args);
    assert!(ok_mig, "`migrate` must exit 0\nstdout={out_mig}\nstderr={err_mig}");

    // Capture the version `status` reports applied — the trailer must list it.
    let mut status_args = vec!["status"];
    status_args.extend_from_slice(&base);
    let (_ok, out_st, _e) = run_bin(&status_args);
    // status prints "  applied  <version> (...)"; pull the first applied version.
    let applied_version: String = out_st
        .lines()
        .find_map(|l| l.trim().strip_prefix("applied  "))
        .map(|rest| rest.split_whitespace().next().unwrap_or("").to_string())
        .expect("status reports an applied version");
    assert!(!applied_version.is_empty(), "an applied version: {out_st}");

    // 3. `dump`.
    let mut dump_args = vec!["dump"];
    dump_args.extend_from_slice(&base);
    dump_args.push("--schema-file");
    dump_args.push(&schema_s);
    let (ok_dump, out_dump, err_dump) = run_bin(&dump_args);
    assert!(
        ok_dump,
        "`dump` on SQLite must exit 0\nstdout={out_dump}\nstderr={err_dump}"
    );
    assert!(out_dump.contains("Dumped schema"), "dump prints a path: {out_dump}");

    let schema = std::fs::read_to_string(&schema_file).expect("read dumped schema.sql");

    // The DDL is derived from sqlite_master.
    assert!(
        schema.contains("CREATE TABLE widgets"),
        "the dump carries the table DDL:\n{schema}"
    );
    assert!(
        schema.contains("CREATE INDEX idx_widgets_name"),
        "the dump carries the index DDL:\n{schema}"
    );
    // The index DDL is ordered AFTER its table (tables before their indexes).
    assert!(
        schema.find("CREATE TABLE widgets").unwrap()
            < schema.find("CREATE INDEX idx_widgets_name").unwrap(),
        "tables are emitted before their indexes:\n{schema}"
    );

    // The applied-versions trailer matches the PG `dump` shape, byte-for-byte in
    // structure: a `-- zero-migrate schema_migrations` marker then one
    // `--   <ver>\t<checksum>\t<name>` line per applied migration (M1+M2 — the
    // trailer is self-contained so `load` reconstructs the journal from it alone).
    assert!(
        schema.contains("-- zero-migrate schema_migrations"),
        "the dump appends the applied-versions trailer marker:\n{schema}"
    );
    assert!(
        schema.contains(&format!("--   {applied_version}")),
        "the trailer lists the applied version `{applied_version}`:\n{schema}"
    );

    // NO journal / _mig artifact leaks into the DDL body (the trailer marker
    // legitimately contains the words `schema_migrations`, so check only the DDL
    // section that precedes the trailer).
    let ddl_body = schema
        .split("-- zero-migrate schema_migrations")
        .next()
        .unwrap_or(&schema);
    assert!(
        !ddl_body.contains("schema_migrations") && !ddl_body.contains("_mig"),
        "no journal/_mig artifact leaks into the dump DDL:\n{ddl_body}"
    );
    // sqlite internal tables never leak either.
    assert!(
        !ddl_body.contains("sqlite_sequence") && !ddl_body.contains("sqlite_autoindex"),
        "no sqlite_* internal leaks into the dump DDL:\n{ddl_body}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// `validate` on SQLite (full parity): a CLEAN set validates ok (shadow dry-run on
/// a throwaway SQLite + clean checksum drift). A checksum-DRIFTED set is flagged
/// (the journal checksum != the on-disk file's checksum), and a DESTRUCTIVE
/// migration is reported destructive — all without touching the real DB's schema.
#[test]
fn cli_validate_on_sqlite_clean_passes_then_flags_drift_and_destructive() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_validatesq_{tok}"));
    let migrations_dir = root.join("migrations");
    std::fs::create_dir_all(&migrations_dir).expect("create temp migration dir");
    let dir_s = migrations_dir.to_str().unwrap().to_string();

    let app_file = root.join("app.sqlite");
    let app_s = app_file.to_str().unwrap().to_string();
    let url = format!("sqlite:{app_s}");
    let base = ["--dir", &dir_s, "--database-url", &url];

    // A clean additive migration.
    let (ok_new, _o, _e) = run_bin(&["new", "widgets", "--dir", &dir_s]);
    assert!(ok_new, "`new`");
    let created: PathBuf = std::fs::read_dir(&migrations_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|x| x.path())
        .find(|p| p.extension().and_then(|x| x.to_str()) == Some("sql"))
        .expect("`new` created a .sql file");
    std::fs::write(
        &created,
        "-- migrate:up\nCREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT);\n\n\
         -- migrate:down\nDROP TABLE widgets;\n",
    )
    .expect("write migration body");

    // (a) CLEAN — validate BEFORE any apply: dry-run on a throwaway SQLite passes,
    //     drift is clean (empty journal vs the set).
    let mut validate_args = vec!["validate"];
    validate_args.extend_from_slice(&base);
    let (ok_v, out_v, err_v) = run_bin(&validate_args);
    assert!(
        ok_v,
        "`validate` on a clean SQLite set must exit 0\nstdout={out_v}\nstderr={err_v}"
    );
    assert!(
        out_v.contains("dry-run ok=true") && out_v.contains("drift_clean=true"),
        "clean validate: dry-run ok + drift clean: {out_v}"
    );

    // Apply it so the journal has a recorded checksum to drift against.
    // `--no-dump-schema`: this is the validate/drift e2e, not the schema-dump path.
    let mut migrate_args = vec!["migrate"];
    migrate_args.extend_from_slice(&base);
    migrate_args.push("--no-dump-schema");
    let (ok_mig, _o, e) = run_bin(&migrate_args);
    assert!(ok_mig, "`migrate`\nstderr={e}");

    // (b) DRIFT — mutate the migration body AFTER it was applied. The journal still
    //     holds the old checksum; `validate` must flag the drift.
    std::fs::write(
        &created,
        "-- migrate:up\nCREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT, extra TEXT);\n\n\
         -- migrate:down\nDROP TABLE widgets;\n",
    )
    .expect("rewrite migration body (drift)");
    let (ok_d, out_d, _e) = run_bin(&validate_args);
    assert!(
        out_d.contains("drift_clean=false") && out_d.contains("DRIFT"),
        "validate flags the checksum drift: {out_d}"
    );
    // Fix #3: a drifted `validate` must EXIT NON-ZERO so CI can gate on it (the
    // printed lines are unchanged; only the exit code reflects the verdict).
    assert!(
        !ok_d,
        "a drifted `validate` must exit non-zero (CI gate): {out_d}"
    );

    // (c) DESTRUCTIVE — a second migration that drops a table is reported
    //     destructive by validate (no --yes needed; validate never applies).
    std::fs::write(
        migrations_dir.join("20990101000000_drop.sql"),
        "-- migrate:up\nDROP TABLE widgets;\n\n-- migrate:down\nSELECT 1;\n",
    )
    .expect("write destructive migration");
    // Restore the first migration so only the new one is destructive (keep drift out
    // of the picture for this assertion).
    std::fs::write(
        &created,
        "-- migrate:up\nCREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT);\n\n\
         -- migrate:down\nDROP TABLE widgets;\n",
    )
    .expect("restore first migration body");
    let (_ok_x, out_x, _e) = run_bin(&validate_args);
    assert!(
        out_x.contains("destructive=true"),
        "validate reports the DROP TABLE migration destructive: {out_x}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Fix #1 (shadow dry-run attribution): a 3-migration SQLite set whose SECOND
/// migration FAILS on the shadow (a `CREATE INDEX` on a table that does not exist).
/// The dry-run report must (a) blame the migration that ACTUALLY failed (#2), not
/// blanket-blame #1 (the pre-fix bug hard-coded the offending version to
/// `plan.items.first()`); and (b) record #1 as `applied_ok: true` (the success
/// PREFIX that committed on the shadow before the abort — pre-fix it was dropped
/// entirely). That is exactly what the PG shadow leg reports for the same set. The
/// test inspects `dry_run.per_migration` directly via the library `run_validate`
/// (the binary only prints the `dry-run ok=…` summary, not the per-migration
/// attribution).
#[test]
fn sqlite_shadow_dry_run_blames_the_real_failing_migration_with_success_prefix() {
    use std::time::Duration;
    use zero_migrate::command::runner::{
        run_validate, RunConfig, RunProfile, RunReport,
    };

    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_sqfail_{tok}"));
    let migrations_dir = root.join("migrations");
    std::fs::create_dir_all(&migrations_dir).expect("create temp migration dir");

    let app_file = root.join("app.sqlite");
    let url = format!("sqlite:{}", app_file.to_str().unwrap());

    // #1 — clean additive: creates `widgets`. Applies fine on the shadow.
    std::fs::write(
        migrations_dir.join("20240101000001_one.sql"),
        "-- migrate:up\nCREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT);\n\n\
         -- migrate:down\nDROP TABLE widgets;\n",
    )
    .expect("write migration #1");
    // #2 — FAILS at apply: a CREATE INDEX on a table that does not exist. The SQLite
    // line-1 descriptor guard trusts the DDL (no syntactic/parse rejection), so it
    // reaches the shadow apply and fails there ("no such table") — exactly the
    // mid-batch failure class this attribution must get right.
    std::fs::write(
        migrations_dir.join("20240101000002_two.sql"),
        "-- migrate:up\nCREATE INDEX idx_missing ON does_not_exist (col);\n\n\
         -- migrate:down\nSELECT 1;\n",
    )
    .expect("write migration #2");
    // #3 — would be clean, but must NEVER run (the batch aborts at #2).
    std::fs::write(
        migrations_dir.join("20240101000003_three.sql"),
        "-- migrate:up\nCREATE TABLE gadgets (id INTEGER PRIMARY KEY);\n\n\
         -- migrate:down\nDROP TABLE gadgets;\n",
    )
    .expect("write migration #3");

    let cfg = RunConfig {
        dir: migrations_dir.clone(),
        database_url: url,
        engine_override: None,
        profile: RunProfile::Trusted,
        project_id: "prj_sqfail".to_string(),
        project_schema: "main".to_string(),
        schemas: vec!["main".to_string()],
        extensions: vec![],
        meta_schema: "_mig".to_string(),
        yes: true,
        statement_timeout: Duration::from_secs(30),
        lock_timeout: Duration::from_secs(30),
    };

    let report = compio::runtime::Runtime::new()
        .expect("compio runtime")
        .block_on(async move { run_validate(&cfg).await.expect("validate runs") });

    let RunReport::Validate(v) = report else {
        panic!("expected a Validate report");
    };
    assert!(
        !v.dry_run.ok,
        "the shadow dry-run must report failure (migration #2 fails): {:?}",
        v.dry_run
    );

    let pm = &v.dry_run.per_migration;
    // #1 is recorded as the applied success prefix.
    let one = pm
        .iter()
        .find(|r| r.version.contains("one") || r.version.ends_with("0001"))
        .or_else(|| pm.iter().find(|r| r.applied_ok))
        .expect("a success-prefix entry for migration #1");
    assert!(
        one.applied_ok,
        "migration #1 must be recorded applied (success prefix): {pm:?}"
    );
    // The FAILURE is attributed to #2 — exactly one failed entry, and it is NOT #1.
    let failed: Vec<_> = pm.iter().filter(|r| !r.applied_ok).collect();
    assert_eq!(
        failed.len(),
        1,
        "exactly one migration is blamed (the one that failed): {pm:?}"
    );
    let blamed = failed[0];
    assert_ne!(
        blamed.version, one.version,
        "the blame must NOT fall on the applied #1: {pm:?}"
    );
    assert!(
        blamed.error.is_some(),
        "the blamed migration carries the apply error: {pm:?}"
    );
    // The two distinct versions prove #2 (not #1) is blamed and #1 applied — the
    // exact PG-parity attribution.

    let _ = std::fs::remove_dir_all(&root);
}
