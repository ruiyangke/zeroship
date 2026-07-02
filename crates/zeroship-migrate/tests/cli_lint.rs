//! DB-free CLI coverage for `zeroship-migrate lint`.

use serde_json::Value;
use zeroship_migrate::{resolve_create_table_policy, MigrationIr, PolicyProfile};

const INDEX_IR: &str = r#"{
  "ir_version": 1,
  "name": "creator_index",
  "ops": [
    {"op":"createTable","name":"widgets","columns":[
      {"name":"name","type":"text","nullable":false}
    ]},
    {"op":"createIndex","table":"widgets","name":"widgets_name_idx",
      "columns":[{"kind":"column","name":"name"}]}
  ]
}"#;

const fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_zeroship-migrate")
}

fn run_lint(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    let cwd = tempdir_with(&[]);
    let mut cmd = std::process::Command::new(bin());
    cmd.current_dir(&cwd)
        .arg("lint")
        .arg("--dir")
        .arg(dir)
        .args(args)
        .env_remove("DATABASE_URL");
    let out = cmd.output().expect("spawn zeroship-migrate lint");
    std::fs::remove_dir_all(&cwd).ok();
    out
}

fn resolve_ir_json(ir: &str) -> String {
    let raw: MigrationIr = serde_json::from_str(ir).expect("lint fixture IR parses");
    let resolved =
        resolve_create_table_policy(&raw, &PolicyProfile::confined()).expect("lint fixture IR resolves");
    serde_json::to_string(&resolved).expect("resolved lint fixture serializes")
}

#[test]
fn lint_human_output_groups_rules_and_summarizes() {
    let index_ir = resolve_ir_json(INDEX_IR);
    let dir = tempdir_with(&[
        (
            "V0001__risky.sql",
            "CREATE TABLE parent (id bigint PRIMARY KEY);
             CREATE TABLE child (id bigint PRIMARY KEY, parent_id bigint);
             ALTER TABLE child ADD CONSTRAINT child_parent_fk
               FOREIGN KEY (parent_id) REFERENCES parent(id) NOT VALID;
             DROP TABLE legacy;",
        ),
        ("0002_creator_index.ir.json", &index_ir),
    ]);

    let out = run_lint(&dir, &["--dialect", "pg"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "lint must exit 0 by default; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("DESTRUCTIVE_DROP"), "stdout:\n{stdout}");
    assert!(stdout.contains("NON_CONCURRENT_INDEX"), "stdout:\n{stdout}");
    assert!(stdout.contains("FK_WITHOUT_INDEX"), "stdout:\n{stdout}");
    assert!(stdout.contains("suggestion:"), "stdout:\n{stdout}");
    assert!(stdout.contains("summary:"), "stdout:\n{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn lint_json_shape_is_stable() {
    let index_ir = resolve_ir_json(INDEX_IR);
    let dir = tempdir_with(&[
        ("V0001__drop.sql", "DROP TABLE legacy;"),
        ("0002_creator_index.ir.json", &index_ir),
    ]);

    let out = run_lint(&dir, &["--dialect", "pg", "--json"]);
    assert!(
        out.status.success(),
        "lint --json must exit 0 by default; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: Value = serde_json::from_slice(&out.stdout).expect("stdout is JSON");
    let arr = json.as_array().expect("top-level lint JSON is an array");
    assert!(arr.iter().any(|v| v["rule"] == "DESTRUCTIVE_DROP"), "{json:#}");
    let idx = arr
        .iter()
        .find(|v| v["rule"] == "NON_CONCURRENT_INDEX")
        .expect("IR-rendered createIndex advisory present");
    assert!(idx.get("migration").is_some(), "{idx:#}");
    assert_eq!(idx["severity"], "Warning");
    assert!(idx.get("message").and_then(Value::as_str).is_some(), "{idx:#}");
    assert!(idx.get("suggestion").is_some(), "{idx:#}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn lint_deny_warnings_fails_only_on_warning_severity() {
    let warning_dir = tempdir_with(&[("V0001__drop.sql", "DROP TABLE legacy;")]);
    let out = run_lint(&warning_dir, &["--deny-warnings"]);
    assert!(
        !out.status.success(),
        "--deny-warnings must fail when a Warning advisory is present"
    );
    std::fs::remove_dir_all(&warning_dir).ok();

    let notice_dir = tempdir_with(&[(
        "V0001__fk_notice.sql",
        "ALTER TABLE child ADD CONSTRAINT child_parent_fk
           FOREIGN KEY (parent_id) REFERENCES parent(id) NOT VALID;",
    )]);
    let out = run_lint(&notice_dir, &["--deny-warnings"]);
    assert!(
        out.status.success(),
        "--deny-warnings must not fail on Notice-only advisories; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("0 warnings, 1 notice"), "stdout:\n{stdout}");
    std::fs::remove_dir_all(&notice_dir).ok();
}

#[test]
fn lint_deny_specific_rule_can_fail_on_notice() {
    let dir = tempdir_with(&[(
        "V0001__fk_notice.sql",
        "ALTER TABLE child ADD CONSTRAINT child_parent_fk
           FOREIGN KEY (parent_id) REFERENCES parent(id) NOT VALID;",
    )]);

    let out = run_lint(&dir, &["--deny", "FK_WITHOUT_INDEX"]);
    assert!(
        !out.status.success(),
        "--deny FK_WITHOUT_INDEX must fail when that rule appears"
    );

    std::fs::remove_dir_all(&dir).ok();
}

fn tempdir_with(files: &[(&str, &str)]) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "zsm_lint_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&base).unwrap();
    for (name, body) in files {
        std::fs::write(base.join(name), body).unwrap();
    }
    base
}
