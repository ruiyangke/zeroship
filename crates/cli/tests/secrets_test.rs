#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::process::{Command, Output};

#[cfg(unix)]
#[test]
fn secret_rm_rejects_invalid_keys_without_request() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("create temp directory");
    let curl = temp.path().join("curl");
    let request_log = temp.path().join("curl-requests");
    std::fs::write(
        &curl,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$ZEROSHIP_CURL_LOG\"\nprintf '\\n204\\n'\n",
    )
    .expect("write curl stub");
    let mut permissions = std::fs::metadata(&curl)
        .expect("read curl stub metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&curl, permissions).expect("make curl stub executable");

    for key in ["FOO?x", "FOO#x"] {
        let output = run_secret_rm(temp.path(), &request_log, key);
        assert!(
            !output.status.success(),
            "invalid key {key:?} unexpectedly succeeded\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(
                "KEY must be 1-64 bytes, start with an ASCII uppercase letter, and contain only ASCII uppercase letters, digits, or underscores"
            ),
            "invalid key {key:?} did not produce an actionable error: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !request_log.exists(),
            "invalid key {key:?} issued an outbound request: {}",
            std::fs::read_to_string(&request_log).unwrap_or_default()
        );
    }

    let output = run_secret_rm(temp.path(), &request_log, "FOO");
    assert!(
        output.status.success(),
        "valid key failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = std::fs::read_to_string(&request_log).expect("read curl request log");
    assert!(
        requests
            .lines()
            .any(|arg| arg == "https://control.example.test/api/apps/app_123/secrets/FOO"),
        "valid key produced the wrong URL:\n{requests}"
    );
}

#[cfg(unix)]
fn run_secret_rm(path_dir: &Path, request_log: &Path, key: &str) -> Output {
    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![path_dir.to_path_buf()];
    paths.extend(std::env::split_paths(&inherited_path));
    let path = std::env::join_paths(paths).expect("construct PATH for curl stub");

    Command::new(env!("CARGO_BIN_EXE_zeroship"))
        .args([
            "secret",
            "rm",
            key,
            "--app=app_123",
            "--control=https://control.example.test",
            "--token=test-token",
        ])
        .env("PATH", path)
        .env("ZEROSHIP_CURL_LOG", request_log)
        .output()
        .expect("run zeroship secret rm")
}
