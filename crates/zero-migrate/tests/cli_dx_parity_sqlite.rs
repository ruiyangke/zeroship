//! DX-parity (config file + env + `--engine` + `new` name validation) BINARY-level
//! e2e on SQLite: drive the ACTUAL `zero-migrate` binary as a SUBPROCESS, no
//! Postgres anywhere. The faithful peer of the unit tests in `config.rs` — these
//! shell the real binary so the clap layer, env reads, config discovery, and exit
//! codes are all exercised end-to-end.
//!
//!   1. Config precedence — a `zero-migrate.toml` sets `migrations_dir`;
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
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zero-migrate"));
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
        root.join("zero-migrate.toml"),
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

/// A malformed `zero-migrate.toml` is a clear, non-zero error — never silently
/// ignored.
#[test]
fn malformed_config_is_a_clear_error() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_dx_bad_{tok}"));
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(
        root.join("zero-migrate.toml"),
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

/// MED-1 RED: an EMPTY `DATABASE_URL` (a present-but-empty env var — the classic
/// `export DATABASE_URL=` shell footgun) must be treated as UNSET, falling back to
/// the config file's `database_url`. Pre-fix, clap's `env = "DATABASE_URL"` yields
/// `Some("")`, the config fallback is skipped, and `classify_engine("")` →
/// Unsupported, so `status` fails. Post-fix the config DSN is used. The other four
/// `ZEROSHIP_MIGRATE_*` vars already treat empty-as-unset; the DSN must match.
#[test]
fn empty_database_url_env_falls_back_to_config_dsn() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_dx_emptyenv_{tok}"));
    let migs = root.join("migrations");
    std::fs::create_dir_all(&migs).expect("migs");
    // A SQLite app file the config DSN points at (a bare-path-free sqlite: scheme so
    // there is no ambiguity).
    let app = root.join("app.db");
    let cfg_dsn = format!("sqlite:{}", app.to_str().unwrap());
    std::fs::write(
        root.join("zero-migrate.toml"),
        format!(
            "migrations_dir = \"migrations\"\ndatabase_url = \"{}\"\n",
            cfg_dsn.replace('\\', "/")
        ),
    )
    .expect("write config");

    // EMPTY DATABASE_URL in the env (run_bin_in env_removes it first, so set it back
    // to "" explicitly here to reproduce the footgun).
    let mut env = HashMap::new();
    env.insert("DATABASE_URL", String::new());

    // `status` reads the journal; it must succeed using the CONFIG dsn, NOT fail with
    // an unsupported/missing-url error from an empty DSN.
    let (ok, out, err) = run_bin_in(&root, &env, &["status"]);
    let msg = format!("{out}{err}").to_lowercase();
    assert!(
        ok,
        "empty DATABASE_URL must fall through to the config DSN\nstdout+stderr: {msg}"
    );
    assert!(
        !msg.contains("unsupported") && !msg.contains("missing database url"),
        "empty DATABASE_URL must not surface an unsupported/missing-url error: {msg}"
    );
}

/// MED-1: a NON-EMPTY `DATABASE_URL` still wins over the config `database_url`
/// (precedence: flag > env(non-empty) > config). Guards the fix from inverting
/// precedence. We point the env at a sqlite file and the config at a DIFFERENT one,
/// then observe which file `migrate` creates.
#[test]
fn nonempty_database_url_env_wins_over_config_dsn() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_dx_envwins_{tok}"));
    let migs = root.join("migrations");
    std::fs::create_dir_all(&migs).expect("migs");
    let dir_s = migs.to_str().unwrap().to_string();

    let env_app = root.join("env.db");
    let cfg_app = root.join("cfg.db");
    std::fs::write(
        root.join("zero-migrate.toml"),
        format!(
            "migrations_dir = \"migrations\"\ndatabase_url = \"sqlite:{}\"\n",
            cfg_app.to_str().unwrap().replace('\\', "/")
        ),
    )
    .expect("write config");

    // A trivial migration.
    let empty: HashMap<&str, String> = HashMap::new();
    let (ok, _o, _e) = run_bin_in(&root, &empty, &["new", "mk", "--dir", &dir_s]);
    assert!(ok, "new");
    let file = list_sql(&migs).into_iter().next().expect("a file");
    std::fs::write(
        &file,
        "-- migrate:up\nCREATE TABLE mk (id INTEGER PRIMARY KEY);\n\n-- migrate:down\nDROP TABLE mk;\n",
    )
    .expect("body");

    let mut env = HashMap::new();
    env.insert("DATABASE_URL", format!("sqlite:{}", env_app.to_str().unwrap()));
    let (ok, out, err) = run_bin_in(&root, &env, &["migrate"]);
    assert!(ok, "migrate with env DSN\n{out}\n{err}");
    assert!(env_app.exists(), "env DSN's file was created (env wins)");
    assert!(!cfg_app.exists(), "config DSN's file was NOT touched");
}

/// MED-2 RED (binary level): `--engine sqlite` + an UNSUPPORTED explicit scheme
/// (`redis://h`) must REFUSE — it must NOT open a file literally named `redis://h`.
/// `--engine pg` + `mysql://h/db` must likewise refuse, NOT hand the URL to libpq.
#[test]
fn engine_override_refuses_unsupported_scheme() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_dx_unsup_{tok}"));
    let migs = root.join("migrations");
    std::fs::create_dir_all(&migs).expect("migs");
    let dir_s = migs.to_str().unwrap().to_string();
    let empty: HashMap<&str, String> = HashMap::new();

    // --engine sqlite + redis://h : must refuse, must NOT create a "redis:" file.
    let (ok, out, err) = run_bin_in(
        &root,
        &empty,
        &[
            "status",
            "--dir",
            &dir_s,
            "--database-url",
            "redis://h",
            "--engine",
            "sqlite",
        ],
    );
    assert!(!ok, "unsupported scheme under --engine must exit non-zero\n{out}\n{err}");
    let msg = format!("{out}{err}").to_lowercase();
    assert!(
        msg.contains("unsupported") || msg.contains("engine"),
        "error should name the unsupported engine, got: {err}"
    );
    // No SQLite file literally named after the redis URL was created anywhere.
    assert!(
        !root.join("redis:").exists() && !root.join("redis://h").exists(),
        "must NOT create a file from the redis:// URL"
    );

    // --engine pg + mysql://h/db : must refuse (not a libpq attempt on a mysql URL).
    let (ok, out, err) = run_bin_in(
        &root,
        &empty,
        &[
            "status",
            "--dir",
            &dir_s,
            "--database-url",
            "mysql://h/db",
            "--engine",
            "pg",
        ],
    );
    assert!(!ok, "mysql scheme under --engine pg must exit non-zero\n{out}\n{err}");
    let msg = format!("{out}{err}").to_lowercase();
    assert!(
        msg.contains("unsupported") || msg.contains("engine"),
        "error should name the unsupported engine, got: {err}"
    );
}

/// LOW-3 RED: when a `zero-migrate.toml` IS discovered+loaded, the CLI echoes a
/// one-line `using config: <abs path>` note to STDERR (so an ancestor-dir config —
/// possibly carrying a `database_url` — is never adopted invisibly). When NO config
/// exists, no such line appears.
#[test]
fn discovered_config_path_is_echoed_to_stderr() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_dx_echo_{tok}"));
    std::fs::create_dir_all(&root).expect("root");
    let cfg_path = root.join("zero-migrate.toml");
    std::fs::write(&cfg_path, "migrations_dir = \"migrations\"\n").expect("config");
    let empty: HashMap<&str, String> = HashMap::new();

    // `new` is offline; it still discovers+loads the config, so the echo must fire.
    let (ok, _out, err) = run_bin_in(&root, &empty, &["new", "echotest"]);
    assert!(ok, "new with a config must succeed\n{err}");
    assert!(
        err.contains("using config:"),
        "the discovered config path must be echoed to stderr, got: {err:?}"
    );
    assert!(
        err.contains(cfg_path.to_str().unwrap()) || err.contains("zero-migrate.toml"),
        "the echo should carry the config path, got: {err:?}"
    );

    // No config anywhere ⇒ no echo line.
    let bare = std::env::temp_dir().join(format!("zsmig_dx_noecho_{tok}"));
    std::fs::create_dir_all(&bare).expect("bare");
    let (ok, _out, err) = run_bin_in(&bare, &empty, &["new", "echotest2", "--dir", "m"]);
    assert!(ok, "new without a config must succeed\n{err}");
    assert!(
        !err.contains("using config:"),
        "no config ⇒ no echo line, got: {err:?}"
    );
}
