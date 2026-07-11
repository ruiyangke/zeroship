// Also requires `native-pg`: the throwaway-DB setup helpers use the compio-concrete
// `compio_postgres` driver, which this standalone does not ship (host-pg + SQLite
// only). Permanently-off dead code here.
#![cfg(all(feature = "standalone-cli", feature = "native-pg"))]
//! Schema-lifecycle dbmate parity (PG leg) — BINARY-level e2e driving the ACTUAL
//! `zero-migrate` binary as a subprocess against a fresh throwaway DB on the
//! `zero_migrate_test` cluster.
//!
//! Covers `load` (a.k.a. `db:setup`) round-trip + refusals, and the auto-refresh
//! of `schema.sql` after `migrate`/`down`. Each test uses a FRESH database and a
//! unique meta schema, dropping the DB after.

use std::path::PathBuf;
use std::process::Command;

use compio_postgres::Client;

const ADMIN_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zero_migrate_test";

fn admin_dsn() -> String {
    std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| ADMIN_DSN.to_string())
}

fn bin_database_url(db: &str) -> String {
    throwaway_dsn(db)
}

fn dsn_for_db(dsn: &str, db: &str) -> String {
    if let Some(url) = url_dsn_for_db(dsn, db) {
        return url;
    }

    dsn.split_whitespace()
        .filter(|kv| !kv.starts_with("dbname="))
        .collect::<Vec<_>>()
        .join(" ")
        + &format!(" dbname={db}")
}

fn url_dsn_for_db(dsn: &str, db: &str) -> Option<String> {
    if !(dsn.starts_with("postgres://") || dsn.starts_with("postgresql://")) {
        return None;
    }
    let (base, query) = dsn.split_once('?').map_or((dsn, None), |(base, query)| {
        (base, Some(query))
    });
    let slash = base.rfind('/')?;
    let mut out = format!("{}{}", &base[..slash + 1], db);
    if let Some(query) = query {
        out.push('?');
        out.push_str(query);
    }
    Some(out)
}

fn default_pg_dump() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/pg_dump_docker.sh")
        .canonicalize()
        .expect("tests/pg_dump_docker.sh exists at repo root")
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
/// `./db/schema.sql` RELATIVE to the child's CWD; without this the children would
/// inherit the crate dir and litter `crates/zero-migrate/db/`. Running them from
/// a throwaway temp dir means the default `./db/schema.sql` lands in temp — still
/// FAITHFULLY exercising the default auto-dump path — and the crate tree stays clean.
/// (Every `--dir`/`--database-url`/`--schema-file` in this file is an absolute path,
/// so the CWD change does not affect any other behaviour.)
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
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zero-migrate"));
    cmd.current_dir(sink_dir()).args(args);
    if std::env::var_os("PG_DUMP").is_none() {
        cmd.env("PG_DUMP", default_pg_dump());
    }
    let out = cmd
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
    dsn_for_db(&admin_dsn(), db)
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

/// Count BASE TABLEs in `public` on the throwaway DB (C1/H1/H2 "nothing mutated").
async fn public_table_count(db: &str) -> i64 {
    let conn = connect(&throwaway_dsn(db)).await;
    let rows = conn
        .query(
            "SELECT count(*)::bigint AS n FROM information_schema.tables \
             WHERE table_schema='public' AND table_type='BASE TABLE'",
            &[],
        )
        .await
        .expect("count tables");
    rows.first().map(|r| r.get::<_, i64>("n")).unwrap_or(0)
}

/// C1 RED: `load --profile confined`/`--profile platform` must REFUSE (non-zero)
/// and execute NO DDL — `load` pipes schema.sql straight at the admin connection
/// with no guard/migrator-role, a posture only sound under `--profile trusted`.
/// Pre-fix `load` ignored `--profile` and ran the raw DDL as admin under ANY
/// profile, bypassing the confinement model — RED (the table would materialize).
#[test]
fn cli_load_under_confined_or_platform_profile_is_refused_and_runs_no_ddl() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_loadprof_{tok}"));
    let dir_s = scaffold(
        &root,
        "-- migrate:up\nCREATE TABLE widgets (id serial PRIMARY KEY, name text);\n\n\
         -- migrate:down\nDROP TABLE widgets;\n",
        "widgets",
    );
    let src_db = format!("zsmig_lp_src_{tok}");
    block_on(async {
        let admin = connect(&admin_dsn()).await;
        create_db(&admin, &src_db).await;
    });
    let src_url = bin_database_url(&src_db);
    let schema_file = root.join("schema.sql");
    let schema_s = schema_file.to_str().unwrap().to_string();

    // Build a real dump from a trusted-migrated source.
    let (ok_mig, _o, e) = run_bin(&["migrate", "--dir", &dir_s, "--database-url", &src_url, "--no-dump-schema"]);
    assert!(ok_mig, "migrate src\nstderr={e}");
    let (ok_dump, _o, e) = run_bin(&["dump", "--dir", &dir_s, "--database-url", &src_url, "--schema-file", &schema_s]);
    assert!(ok_dump, "dump src\nstderr={e}");

    for profile in ["confined", "platform"] {
        let fresh_db = format!("zsmig_lp_fresh_{profile}_{tok}");
        block_on(async {
            let admin = connect(&admin_dsn()).await;
            create_db(&admin, &fresh_db).await;
        });
        let fresh_url = bin_database_url(&fresh_db);

        let (ok_load, o, err) = run_bin(&[
            "load", "--dir", &dir_s, "--database-url", &fresh_url,
            "--schema-file", &schema_s, "--profile", profile,
        ]);
        assert!(
            !ok_load,
            "load --profile {profile} must REFUSE (non-zero)\nstdout={o}\nstderr={err}"
        );
        assert!(
            err.to_lowercase().contains("trusted"),
            "the refusal mentions trusted-only: {err}"
        );
        // NO DDL executed: the target schema is untouched (no widgets table, in fact
        // zero user tables — no journal either since journal-create is part of load).
        assert!(
            !block_on(table_exists(&fresh_db, "widgets")),
            "load --profile {profile} must NOT create the widgets table"
        );
        assert_eq!(
            block_on(public_table_count(&fresh_db)),
            0,
            "load --profile {profile} must leave the target schema entirely untouched"
        );
        block_on(async {
            let admin = connect(&admin_dsn()).await;
            drop_db(&admin, &fresh_db).await;
        });
    }

    block_on(async {
        let admin = connect(&admin_dsn()).await;
        drop_db(&admin, &src_db).await;
    });
    let _ = std::fs::remove_dir_all(&root);
}

/// H1 RED: a `load` whose schema.sql body has a FAILING statement in the middle must
/// fully ROLL BACK — the target DB is left EMPTY, not half-restored. Pre-fix the
/// restore ran `batch_execute` with no transaction wrapper, so the statements before
/// the failure committed (partial objects survived) — RED.
#[test]
fn cli_load_failing_mid_dump_rolls_back_fully() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_loadtx_{tok}"));
    let dir_s = scaffold(
        &root,
        "-- migrate:up\nCREATE TABLE noop (id serial PRIMARY KEY);\n\n\
         -- migrate:down\nDROP TABLE noop;\n",
        "noop",
    );
    let db = format!("zsmig_loadtx_{tok}");
    block_on(async {
        let admin = connect(&admin_dsn()).await;
        create_db(&admin, &db).await;
    });
    let url = bin_database_url(&db);

    // Hand-craft a schema.sql: a valid table, then a deliberately failing statement,
    // then another table. A non-transactional restore would leave `early` behind.
    let schema_file = root.join("broken.sql");
    let schema_s = schema_file.to_str().unwrap().to_string();
    std::fs::write(
        &schema_file,
        "CREATE TABLE public.early (id integer NOT NULL);\n\
         INSERT INTO public.does_not_exist VALUES (1);\n\
         CREATE TABLE public.late (id integer NOT NULL);\n\n\
         -- zero-migrate schema_migrations\n",
    )
    .expect("write broken schema");

    let (ok_load, _o, err) = run_bin(&["load", "--dir", &dir_s, "--database-url", &url, "--schema-file", &schema_s]);
    assert!(!ok_load, "load with a failing mid-dump statement must fail (non-zero)");
    assert!(
        err.to_lowercase().contains("restore") || err.to_lowercase().contains("does_not_exist"),
        "the error surfaces the failing restore: {err}"
    );
    // Fully rolled back: NEITHER `early` NOR `late` survives.
    assert!(!block_on(table_exists(&db, "early")), "the pre-failure table must roll back");
    assert!(!block_on(table_exists(&db, "late")), "the post-failure table must not exist");
    assert_eq!(block_on(public_table_count(&db)), 0, "the DB is left EMPTY after a failed load");

    block_on(async {
        let admin = connect(&admin_dsn()).await;
        drop_db(&admin, &db).await;
    });
    let _ = std::fs::remove_dir_all(&root);
}

/// H2 RED: `load` onto a DB that already has a USER TABLE (but no journal) is refused
/// with nothing further mutated. Pre-fix the first-entry guard checked ONLY the
/// journal, so a journal-less but populated DB fell through to the DDL restore and
/// failed mid-batch on the first `CREATE TABLE` collision — RED.
#[test]
fn cli_load_onto_populated_db_without_journal_is_refused() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_loadpop_{tok}"));
    let dir_s = scaffold(
        &root,
        "-- migrate:up\nCREATE TABLE widgets (id serial PRIMARY KEY, name text);\n\n\
         -- migrate:down\nDROP TABLE widgets;\n",
        "widgets",
    );
    let src_db = format!("zsmig_pop_src_{tok}");
    let tgt_db = format!("zsmig_pop_tgt_{tok}");
    block_on(async {
        let admin = connect(&admin_dsn()).await;
        create_db(&admin, &src_db).await;
        create_db(&admin, &tgt_db).await;
        // Pre-populate the target with a user table, NO journal.
        let tgt = connect(&throwaway_dsn(&tgt_db)).await;
        tgt.batch_execute("CREATE TABLE public.preexisting (id integer);")
            .await
            .expect("seed a pre-existing user table");
    });
    let src_url = bin_database_url(&src_db);
    let tgt_url = bin_database_url(&tgt_db);
    let schema_file = root.join("schema.sql");
    let schema_s = schema_file.to_str().unwrap().to_string();

    let (ok_mig, _o, e) = run_bin(&["migrate", "--dir", &dir_s, "--database-url", &src_url, "--no-dump-schema"]);
    assert!(ok_mig, "migrate src\nstderr={e}");
    let (ok_dump, _o, e) = run_bin(&["dump", "--dir", &dir_s, "--database-url", &src_url, "--schema-file", &schema_s]);
    assert!(ok_dump, "dump src\nstderr={e}");

    let (ok_load, _o, err) = run_bin(&["load", "--dir", &dir_s, "--database-url", &tgt_url, "--schema-file", &schema_s]);
    assert!(!ok_load, "load onto a populated (journal-less) DB must refuse");
    assert!(
        err.to_lowercase().contains("already") || err.to_lowercase().contains("user table"),
        "the refusal explains the target is not fresh: {err}"
    );
    // Nothing further mutated: the pre-existing table is untouched and `widgets`
    // was never created.
    assert!(block_on(table_exists(&tgt_db, "preexisting")), "the seed table survives");
    assert!(!block_on(table_exists(&tgt_db, "widgets")), "no DDL ran from the dump");
    assert_eq!(block_on(public_table_count(&tgt_db)), 1, "only the seed table exists");

    block_on(async {
        let admin = connect(&admin_dsn()).await;
        drop_db(&admin, &src_db).await;
        drop_db(&admin, &tgt_db).await;
    });
    let _ = std::fs::remove_dir_all(&root);
}

/// M2 RED: `load` reconstructs the journal from the DUMP TRAILER alone — NOT from
/// `--dir`. We load from the dump while pointing `--dir` at an EMPTY directory (the
/// migration files are absent), then dump the LOADED DB and assert its trailer is
/// byte-identical to the source dump's trailer — i.e. the loaded journal carries the
/// DUMP's version+checksum+name, sourced with zero help from `--dir`. Pre-fix `load`
/// re-sourced checksum/name from the file in `--dir`, so with the file ABSENT the
/// loaded journal recorded empty checksum/name and the round-trip trailer differed
/// (or drift lied) — RED.
#[test]
fn cli_load_reconstructs_journal_from_trailer_not_dir() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_loadindep_{tok}"));
    let dir_s = scaffold(
        &root,
        "-- migrate:up\nCREATE TABLE widgets (id serial PRIMARY KEY, name text);\n\n\
         -- migrate:down\nDROP TABLE widgets;\n",
        "widgets",
    );
    // An EMPTY migration dir — `load` must not need ANY file here for journal fidelity.
    let empty_dir = root.join("empty_migrations");
    std::fs::create_dir_all(&empty_dir).expect("mkdir empty");
    let empty_dir_s = empty_dir.to_str().unwrap().to_string();

    let src_db = format!("zsmig_indep_src_{tok}");
    let fresh_db = format!("zsmig_indep_fresh_{tok}");
    block_on(async {
        let admin = connect(&admin_dsn()).await;
        create_db(&admin, &src_db).await;
        create_db(&admin, &fresh_db).await;
    });
    let src_url = bin_database_url(&src_db);
    let fresh_url = bin_database_url(&fresh_db);
    let schema_file = root.join("schema.sql");
    let schema_s = schema_file.to_str().unwrap().to_string();

    let (ok_mig, _o, e) = run_bin(&["migrate", "--dir", &dir_s, "--database-url", &src_url, "--no-dump-schema"]);
    assert!(ok_mig, "migrate src\nstderr={e}");
    let (ok_dump, _o, e) = run_bin(&["dump", "--dir", &dir_s, "--database-url", &src_url, "--schema-file", &schema_s]);
    assert!(ok_dump, "dump src\nstderr={e}");
    let src_dump = std::fs::read_to_string(&schema_file).expect("read src dump");
    let src_trailer = src_dump
        .split_once("-- zero-migrate schema_migrations")
        .map(|(_, t)| t.to_string())
        .expect("src dump has a trailer");

    // load from the dump into a fresh DB, with `--dir` pointing at the EMPTY dir.
    let (ok_load, o, e) = run_bin(&["load", "--dir", &empty_dir_s, "--database-url", &fresh_url, "--schema-file", &schema_s]);
    assert!(ok_load, "load into fresh (empty --dir)\nstdout={o}\nstderr={e}");

    // dump the LOADED DB; its trailer must equal the source dump's trailer exactly —
    // proving the journal was reconstructed from the trailer, not from `--dir`.
    let schema2 = root.join("schema2.sql");
    let schema2_s = schema2.to_str().unwrap().to_string();
    let (ok_d2, o, e) = run_bin(&["dump", "--dir", &empty_dir_s, "--database-url", &fresh_url, "--schema-file", &schema2_s]);
    assert!(ok_d2, "dump loaded\nstdout={o}\nstderr={e}");
    let loaded_dump = std::fs::read_to_string(&schema2).expect("read loaded dump");
    let loaded_trailer = loaded_dump
        .split_once("-- zero-migrate schema_migrations")
        .map(|(_, t)| t.to_string())
        .expect("loaded dump has a trailer");
    assert_eq!(
        loaded_trailer, src_trailer,
        "the loaded journal's trailer (version+checksum+name) is reconstructed from the \
         dump trailer alone — identical to the source, with an EMPTY --dir"
    );

    block_on(async {
        let admin = connect(&admin_dsn()).await;
        drop_db(&admin, &src_db).await;
        drop_db(&admin, &fresh_db).await;
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
        dumped.contains("-- zero-migrate schema_migrations"),
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
