//! Support for running a case that shares process-wide state (here, the one
//! PostgreSQL instance a test binary owns) alone in a child copy of this binary.
//!
//! A fleet-wide sweep run by a sibling case can transition an app this case is
//! asserting on, or mutate a singleton it reads, so the case is `#[ignore]`d in
//! the shared run and its spawner runs it in a child whose own migrated database
//! no sibling observes.

/// Run `test_name` (e.g. `spend::some_case`) alone in a child copy of this test
/// binary, with the child's ignored case enabled. Panics if the child fails or
/// runs no test.
pub(crate) fn run_alone(test_name: &str) {
    let out = std::process::Command::new(std::env::current_exe().expect("current_exe"))
        .args([
            "--exact",
            test_name,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .output()
        .expect("spawn a child copy of this test binary");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{test_name} failed alone:\n{text}");
    assert!(
        text.contains("1 passed"),
        "the child ran no test, so it proved nothing:\n{text}"
    );
}
