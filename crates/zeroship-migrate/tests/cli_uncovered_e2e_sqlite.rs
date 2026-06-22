//! BINARY-level e2e for the `zeroship-migrate` subcommands that were previously
//! ONLY unit/library-tested — driven through the REAL binary as a subprocess on a
//! SQLite file, asserting real DB state (table existence) + `status` counts. The
//! SQLite peer of `cli_uncovered_e2e_pg.rs`. Covers:
//!
//!   * `up` (alias of `migrate`) — applies all pending, identical to `migrate`.
//!   * `rollback --steps <N> --yes` — reverses exactly the N most-recent.
//!   * `rollback --to <version> --yes` — reverses everything strictly AFTER v1.
//!   * `wait` — a SQLite file is "ready" the moment it is openable as a hardened
//!     backend (`run_wait`/`probe_once`), so `wait` returns 0 promptly. A `wait`
//!     against an unsupported engine URL times out non-zero within a small budget.
//!
//! No DB cluster needed — SQLite is a local temp file under a per-test temp dir.

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

/// A process-wide temp "sink" CWD for `run_bin` (see the other cli_* suites): the
/// auto-dump default (ON) writes `./db/schema.sql` relative to the child CWD, so we
/// run children from a throwaway temp dir to keep the crate tree clean. Every
/// `--dir`/`--database-url`/`--schema-file` here is absolute.
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
    let out = Command::new(env!("CARGO_BIN_EXE_zeroship-migrate"))
        .current_dir(sink_dir())
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

/// Write THREE dbmate migrations with EXPLICIT, distinct 14-digit versions (so the
/// numeric `--to <version>` is deterministic — `new` mints clock stamps that could
/// collide in the same second). Each creates one table (`t1`/`t2`/`t3`) and drops it
/// on `down`. Returns `(dir, app_file, url, [v1, v2, v3])` where the `v*` are the
/// 14-digit numeric versions usable with `rollback --to`.
fn scaffold_three(root: &std::path::Path) -> (String, String, String, [u64; 3]) {
    let migrations_dir = root.join("migrations");
    std::fs::create_dir_all(&migrations_dir).expect("create temp migration dir");
    let dir_s = migrations_dir.to_str().unwrap().to_string();

    let versions: [u64; 3] = [20_240_101_000_001, 20_240_101_000_002, 20_240_101_000_003];
    for (i, v) in versions.iter().enumerate() {
        let n = i + 1;
        std::fs::write(
            migrations_dir.join(format!("{v}_t{n}.sql")),
            format!(
                "-- migrate:up\nCREATE TABLE t{n} (id INTEGER PRIMARY KEY);\n\n\
                 -- migrate:down\nDROP TABLE t{n};\n"
            ),
        )
        .expect("write migration");
    }

    let app_file = root.join("app.sqlite");
    let app_s = app_file.to_str().unwrap().to_string();
    let url = format!("sqlite:{app_s}");
    (dir_s, app_s, url, versions)
}

/// `up` is an exact alias of `migrate`: it applies ALL pending migrations and
/// `status` then reflects them (applied=3 / pending=0), with every table present.
#[test]
fn cli_up_alias_applies_all_pending_like_migrate_on_sqlite() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_up_sq_{tok}"));
    let (dir_s, app_s, url, _v) = scaffold_three(&root);
    let base = ["--dir", &dir_s, "--database-url", &url, "--no-dump-schema"];

    // `up` (NOT `migrate`) applies all three.
    let mut up_args = vec!["up"];
    up_args.extend_from_slice(&base);
    let (ok_up, out_up, err_up) = run_bin(&up_args);
    assert!(ok_up, "`up` must exit 0\nstdout={out_up}\nstderr={err_up}");
    assert!(
        out_up.contains("applied 3"),
        "`up` reports 3 applied (alias of migrate): {out_up}"
    );

    // Real state: all three tables materialized.
    for t in ["t1", "t2", "t3"] {
        assert!(table_exists(&app_s, t), "`up` created {t}");
    }

    // status reflects it.
    let (ok_st, out_st, err_st) = run_bin(&["status", "--dir", &dir_s, "--database-url", &url]);
    assert!(ok_st, "`status` must exit 0\nstdout={out_st}\nstderr={err_st}");
    assert!(
        out_st.contains("applied=3") && out_st.contains("pending=0"),
        "after `up`: 3 applied / 0 pending: {out_st}"
    );

    // A SECOND `up` is a no-op (nothing pending) — proving alias parity with migrate.
    let (ok_up2, out_up2, _e) = run_bin(&up_args);
    assert!(ok_up2, "second `up` exits 0: {out_up2}");
    assert!(
        out_up2.contains("no-op"),
        "second `up` is a no-op (alias of migrate): {out_up2}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// `rollback --steps 2 --yes` reverses EXACTLY the two most-recently-applied
/// migrations (t3, t2) and leaves the first (t1) in place. Asserted via real table
/// state AND `status` (applied=1 / pending=2).
#[test]
fn cli_rollback_steps_reverses_the_n_most_recent_on_sqlite() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_rbsteps_sq_{tok}"));
    let (dir_s, app_s, url, _v) = scaffold_three(&root);
    let base = ["--dir", &dir_s, "--database-url", &url, "--no-dump-schema"];

    // Apply all three.
    let mut migrate_args = vec!["migrate"];
    migrate_args.extend_from_slice(&base);
    let (ok_mig, o, e) = run_bin(&migrate_args);
    assert!(ok_mig, "migrate (3)\nstdout={o}\nstderr={e}");
    for t in ["t1", "t2", "t3"] {
        assert!(table_exists(&app_s, t), "migrate created {t}");
    }

    // rollback --steps 2 --yes.
    let mut rb_args = vec!["rollback", "--steps", "2", "--yes"];
    rb_args.extend_from_slice(&base);
    let (ok_rb, out_rb, err_rb) = run_bin(&rb_args);
    assert!(
        ok_rb,
        "`rollback --steps 2 --yes` must exit 0\nstdout={out_rb}\nstderr={err_rb}"
    );

    // Real state: the last two are reversed, the first remains.
    assert!(table_exists(&app_s, "t1"), "t1 (oldest) survives --steps 2");
    assert!(!table_exists(&app_s, "t2"), "t2 reversed by --steps 2");
    assert!(!table_exists(&app_s, "t3"), "t3 reversed by --steps 2");

    let (_ok_st, out_st, _e) = run_bin(&["status", "--dir", &dir_s, "--database-url", &url]);
    assert!(
        out_st.contains("applied=1") && out_st.contains("pending=2"),
        "after --steps 2: 1 applied / 2 pending: {out_st}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// `rollback --to <v1> --yes` reverses everything STRICTLY AFTER v1 (t3, t2) and
/// keeps v1 (t1). `--to` takes the 14-digit numeric file version.
#[test]
fn cli_rollback_to_version_reverses_everything_after_it_on_sqlite() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_rbto_sq_{tok}"));
    let (dir_s, app_s, url, versions) = scaffold_three(&root);
    let base = ["--dir", &dir_s, "--database-url", &url, "--no-dump-schema"];

    let mut migrate_args = vec!["migrate"];
    migrate_args.extend_from_slice(&base);
    let (ok_mig, o, e) = run_bin(&migrate_args);
    assert!(ok_mig, "migrate (3)\nstdout={o}\nstderr={e}");

    // rollback --to v1 --yes: keep v1, reverse v2+v3.
    let v1 = versions[0].to_string();
    let mut rb_args = vec!["rollback", "--to", &v1, "--yes"];
    rb_args.extend_from_slice(&base);
    let (ok_rb, out_rb, err_rb) = run_bin(&rb_args);
    assert!(
        ok_rb,
        "`rollback --to {v1} --yes` must exit 0\nstdout={out_rb}\nstderr={err_rb}"
    );

    assert!(table_exists(&app_s, "t1"), "v1 (t1) is KEPT by --to v1");
    assert!(!table_exists(&app_s, "t2"), "v2 (t2) reversed (after v1)");
    assert!(!table_exists(&app_s, "t3"), "v3 (t3) reversed (after v1)");

    let (_ok_st, out_st, _e) = run_bin(&["status", "--dir", &dir_s, "--database-url", &url]);
    assert!(
        out_st.contains("applied=1") && out_st.contains("pending=2"),
        "after --to v1: 1 applied / 2 pending: {out_st}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// `rollback` is ALWAYS destructive ⇒ it must REFUSE without `--yes` (non-zero) and
/// reverse NOTHING. Pins the approval gate the runner enforces.
#[test]
fn cli_rollback_without_yes_refuses_and_reverses_nothing_on_sqlite() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_rbgate_sq_{tok}"));
    let (dir_s, app_s, url, _v) = scaffold_three(&root);
    let base = ["--dir", &dir_s, "--database-url", &url, "--no-dump-schema"];

    let mut migrate_args = vec!["migrate"];
    migrate_args.extend_from_slice(&base);
    let (ok_mig, _o, e) = run_bin(&migrate_args);
    assert!(ok_mig, "migrate\nstderr={e}");

    // No --yes ⇒ refuse.
    let mut rb_args = vec!["rollback", "--steps", "1"];
    rb_args.extend_from_slice(&base);
    let (ok_rb, _o, _e) = run_bin(&rb_args);
    assert!(!ok_rb, "`rollback` without --yes must refuse (non-zero)");

    // Everything is still applied (nothing reversed).
    for t in ["t1", "t2", "t3"] {
        assert!(table_exists(&app_s, t), "refused rollback left {t} in place");
    }

    let _ = std::fs::remove_dir_all(&root);
}

/// `wait` on SQLite: a file that is openable as a hardened backend is "ready", so
/// `wait` returns 0 PROMPTLY (`probe_once` opens the backend; no journal mutation).
#[test]
fn cli_wait_returns_zero_promptly_for_a_ready_sqlite_file() {
    let tok = token();
    let root = std::env::temp_dir().join(format!("zsmig_wait_sq_{tok}"));
    std::fs::create_dir_all(&root).expect("mkdir");
    let app_file = root.join("app.sqlite");
    let url = format!("sqlite:{}", app_file.to_str().unwrap());

    let start = std::time::Instant::now();
    let (ok, out, err) = run_bin(&["wait", "--database-url", &url, "--timeout-secs", "5"]);
    let elapsed = start.elapsed();
    assert!(ok, "`wait` on a ready SQLite file must exit 0\nstdout={out}\nstderr={err}");
    assert!(
        out.contains("ready"),
        "`wait` prints the ready message: {out}"
    );
    // "Promptly" — well under the 5s budget (the open succeeds on the first probe).
    assert!(
        elapsed < std::time::Duration::from_secs(4),
        "ready `wait` returned promptly (not after polling to the deadline): {elapsed:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
