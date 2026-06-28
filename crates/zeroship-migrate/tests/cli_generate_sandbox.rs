//! Binary-level regression for `zeroship-migrate-js generate --schema`: untrusted
//! schema JS must run in the sandboxed child, not in the parent process.

use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn cli_bin() -> &'static str {
    env!("CARGO_BIN_EXE_zeroship-migrate-js")
}

fn wait_with_timeout(mut child: std::process::Child, timeout: Duration) -> Output {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().expect("collect child output"),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("zeroship-migrate-js generate hung past {timeout:?}");
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => panic!("poll zeroship-migrate-js generate: {e}"),
        }
    }
}

#[test]
fn generate_schema_eval_loop_is_killed_by_child_budget_not_parent_hang() {
    let dir = tempfile::tempdir().expect("tempdir");
    let schema = dir.path().join("schema.js");
    std::fs::write(
        &schema,
        r#"
        while (true) {}
        export const schema = {};
        "#,
    )
    .expect("write schema");

    let child = Command::new(cli_bin())
        .arg("generate")
        .arg("--schema")
        .arg(&schema)
        .arg("--database-url")
        // Eval runs before DB connect. This must never be reached for the loop
        // schema; keeping the DSN dead makes the test independent of Postgres.
        .arg("postgres://postgres:zeroship@127.0.0.1:1/does_not_matter")
        .arg("--dir")
        .arg(dir.path().join("migrations"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn zeroship-migrate-js generate");

    let output = wait_with_timeout(child, Duration::from_secs(25));
    assert!(
        !output.status.success(),
        "looping schema must fail, stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("BUILD_RECORDER_BUDGET_EXCEEDED")
            || stderr.to_lowercase().contains("schema sandbox failed"),
        "generate must report a sandbox/budget error, got stderr={stderr}"
    );
    assert!(
        !stderr.contains("db connect failed"),
        "generate reached DB introspection instead of failing during sandboxed schema eval: {stderr}"
    );
}
