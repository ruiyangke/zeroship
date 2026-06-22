//! DX-parity (config file + env + `--engine` + `new` name validation) BINARY-level
//! e2e on SQLite: drive the ACTUAL `zeroship-migrate` binary as a SUBPROCESS, no
//! Postgres anywhere. The faithful peer of the unit tests in `config.rs` — these
//! shell the real binary so the clap layer, env reads, config discovery, and exit
//! codes are all exercised end-to-end.
//!
//!   1. Config precedence — a `zeroship-migrate.toml` sets `migrations_dir`;
//!      `status`/`new` honour it; an env var overrides the config; a `--dir` flag
//!      overrides the env.
//!   2. `--engine` — a bare-path DSN + `--engine sqlite` works; `--engine pg` with
//!      a `sqlite:` DSN errors clearly (engine conflict).
//!   3. `new` validation — `new "bad name!"` exits non-zero with the allowed-chars
//!      message and writes NO file; `new good_name` still works.
//!   4. Malformed config — a broken TOML is a clear non-zero error.

use std::collections::HashMap;
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

/// Run the binary with an explicit working directory and a clean+augmented env
/// (so config discovery in CWD and the `ZEROSHIP_MIGRATE_*` vars are exercised
/// faithfully). `DATABASE_URL` is removed from the inherited env so a stray host
/// value never leaks into a config/precedence test.
fn run_bin_in(cwd: &Path, env: &HashMap<&str, String>, args: &[&str]) -> (bool, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zeroship-migrate"));
    cmd.current_dir(cwd);
    cmd.env_remove("DATABASE_URL");
    cmd.env_remove("ZEROSHIP_MIGRATE_MIGRATIONS_DIR");
    cmd.env_remove("ZEROSHIP_MIGRATE_SCHEMA_FILE");
    cmd.env_remove("ZEROSHIP_MIGRATE_ENGINE");
    cmd.env_remove("ZEROSHIP_MIGRATE_PROFILE");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.args(args).output().expect("spawn the built binary");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn list_sql(dir: &Path) -> Vec<PathBuf> {
    if !dir.exists() {
        return Vec::new();
    }
    std::fs::read_dir(dir)
        .expect("read dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("sql"))
        .collect()
}

/// `new` writes into the config-file's `migrations_dir`; an env var overrides the
/// config; a `--dir` flag overrides the env. We observe the EFFECTIVE dir by where
/// `new` drops the file at each precedence level.
#[test]
fn config_precedence_dir_flag_over_env_over_config() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_dx_prec_{tok}"));
    std::fs::create_dir_all(&root).expect("root");

    // The three candidate dirs (relative to CWD = root).
    let cfg_dir = root.join("from_config");
    let env_dir = root.join("from_env");
    let flag_dir = root.join("from_flag");

    // A config file in CWD pointing at cfg_dir.
    std::fs::write(
        root.join("zeroship-migrate.toml"),
        "migrations_dir = \"from_config\"\n",
    )
    .expect("write config");

    let empty: HashMap<&str, String> = HashMap::new();

    // Level 1: config only → `new` lands in cfg_dir.
    let (ok, out, err) = run_bin_in(&root, &empty, &["new", "alpha"]);
    assert!(ok, "config-level new must succeed\n{out}\n{err}");
    assert_eq!(list_sql(&cfg_dir).len(), 1, "config dir got the file");

    // Level 2: env overrides config → lands in env_dir, NOT cfg_dir.
    let mut env = HashMap::new();
    env.insert("ZEROSHIP_MIGRATE_MIGRATIONS_DIR", "from_env".to_string());
    let (ok, out, err) = run_bin_in(&root, &env, &["new", "beta"]);
    assert!(ok, "env-level new must succeed\n{out}\n{err}");
    assert_eq!(list_sql(&env_dir).len(), 1, "env dir got the file");
    assert_eq!(list_sql(&cfg_dir).len(), 1, "config dir unchanged");

    // Level 3: --dir flag overrides env → lands in flag_dir.
    let (ok, out, err) = run_bin_in(
        &root,
        &env,
        &["new", "gamma", "--dir", flag_dir.to_str().unwrap()],
    );
    assert!(ok, "flag-level new must succeed\n{out}\n{err}");
    assert_eq!(list_sql(&flag_dir).len(), 1, "flag dir got the file");
    assert_eq!(list_sql(&env_dir).len(), 1, "env dir unchanged");
}

/// `--engine sqlite` resolves an ambiguous bare-path DSN deterministically: the
/// migration applies onto a SQLite file even though the DSN is a bare path.
#[test]
fn engine_override_resolves_bare_path_to_sqlite() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_dx_eng_{tok}"));
    let migs = root.join("migrations");
    std::fs::create_dir_all(&migs).expect("migs");
    let dir_s = migs.to_str().unwrap().to_string();

    // A BARE path DSN (ambiguous) — pointing at a sqlite file with no scheme.
    let app = root.join("app.db");
    let bare = app.to_str().unwrap().to_string();

    let empty: HashMap<&str, String> = HashMap::new();
    let (ok, out, err) = run_bin_in(&root, &empty, &["new", "mk", "--dir", &dir_s]);
    assert!(ok, "new\n{out}\n{err}");
    let file = list_sql(&migs).into_iter().next().expect("a file");
    std::fs::write(
        &file,
        "-- migrate:up\nCREATE TABLE mk (id INTEGER PRIMARY KEY);\n\n-- migrate:down\nDROP TABLE mk;\n",
    )
    .expect("body");

    let (ok, out, err) = run_bin_in(
        &root,
        &empty,
        &[
            "migrate",
            "--dir",
            &dir_s,
            "--database-url",
            &bare,
            "--engine",
            "sqlite",
        ],
    );
    assert!(
        ok,
        "bare path + --engine sqlite must apply on SQLite\n{out}\n{err}"
    );
    assert!(app.exists(), "the SQLite app file was created");
}

/// `--engine pg` against an UNAMBIGUOUS `sqlite:` DSN is a clear, non-zero conflict
/// error — we never silently trust one side.
#[test]
fn engine_override_conflicts_with_unambiguous_dsn() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_dx_conf_{tok}"));
    let migs = root.join("migrations");
    std::fs::create_dir_all(&migs).expect("migs");
    let dir_s = migs.to_str().unwrap().to_string();
    let empty: HashMap<&str, String> = HashMap::new();

    let (ok, out, err) = run_bin_in(
        &root,
        &empty,
        &[
            "status",
            "--dir",
            &dir_s,
            "--database-url",
            "sqlite:/tmp/whatever.db",
            "--engine",
            "pg",
        ],
    );
    assert!(!ok, "conflict must exit non-zero\n{out}\n{err}");
    let msg = format!("{out}{err}").to_lowercase();
    assert!(
        msg.contains("engine") && (msg.contains("conflict") || msg.contains("contradict")),
        "error should name the engine conflict, got: {err}"
    );
}

/// `new "bad name!"` is rejected at scaffold time: non-zero exit, an allowed-chars
/// message, and NO file written.
#[test]
fn new_rejects_invalid_name_without_writing() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_dx_name_{tok}"));
    let migs = root.join("migrations");
    std::fs::create_dir_all(&migs).expect("migs");
    let dir_s = migs.to_str().unwrap().to_string();
    let empty: HashMap<&str, String> = HashMap::new();

    let (ok, out, err) = run_bin_in(&root, &empty, &["new", "bad name!", "--dir", &dir_s]);
    assert!(!ok, "invalid name must exit non-zero\n{out}\n{err}");
    let msg = format!("{out}{err}");
    assert!(
        msg.contains("A-Za-z0-9_") || msg.to_lowercase().contains("allowed"),
        "message must state allowed chars, got: {msg}"
    );
    // A normalized suggestion is offered.
    assert!(
        msg.contains("bad_name"),
        "message should suggest a normalized name, got: {msg}"
    );
    assert_eq!(list_sql(&migs).len(), 0, "NO file was written");

    // The good name still works.
    let (ok, out, err) = run_bin_in(&root, &empty, &["new", "good_name", "--dir", &dir_s]);
    assert!(ok, "good name must succeed\n{out}\n{err}");
    assert_eq!(list_sql(&migs).len(), 1, "good name wrote a file");
}

/// A malformed `zeroship-migrate.toml` is a clear, non-zero error — never silently
/// ignored.
#[test]
fn malformed_config_is_a_clear_error() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_dx_bad_{tok}"));
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(
        root.join("zeroship-migrate.toml"),
        "this is = = not valid toml [[[\n",
    )
    .expect("write bad config");
    let empty: HashMap<&str, String> = HashMap::new();

    let (ok, out, err) = run_bin_in(&root, &empty, &["new", "x"]);
    assert!(!ok, "malformed config must exit non-zero\n{out}\n{err}");
    let msg = format!("{out}{err}").to_lowercase();
    assert!(
        msg.contains("config") || msg.contains("toml"),
        "error should name the config/toml problem, got: {err}"
    );
}
