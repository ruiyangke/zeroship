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

/// `PUT /api/apps/:id/env/expose` REPLACES the whole list, so `expose`
/// must read-modify-write: GET the current list, add the name, PUT the
/// union. A naive PUT of just the new name silently un-exposes every
/// other secret the app has.
///
/// What this test does NOT catch: whether the control plane actually
/// persists the union (the curl stub answers the GET), whether the
/// runtime honours the list, or any ordering guarantee beyond
/// "GET precedes PUT" on this single-request pair.
#[cfg(unix)]
#[test]
fn secret_expose_preserves_names_already_on_the_list() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let request_log = temp.path().join("curl-requests");
    write_curl_stub(temp.path(), r#"{"expose":["ALPHA"]}"#);

    let output = run_secret(
        temp.path(),
        &request_log,
        &["secret", "expose", "BRAVO", "--app=app_123"],
    );
    assert!(
        output.status.success(),
        "secret expose failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = std::fs::read_to_string(&request_log).expect("read curl request log");
    let requests: Vec<&str> = log.lines().collect();
    assert_eq!(
        requests.len(),
        2,
        "expected exactly a GET then a PUT, got:\n{log}"
    );
    assert_eq!(
        requests[0],
        "GET https://control.example.test/api/apps/app_123/env/expose ",
        "expose did not read the current list first:\n{log}"
    );

    let put = requests[1];
    assert!(
        put.starts_with("PUT https://control.example.test/api/apps/app_123/env/expose "),
        "expose did not PUT the expose endpoint:\n{log}"
    );
    let body: serde_json::Value = serde_json::from_str(
        put.split_once("/env/expose ")
            .expect("PUT log line carries a body")
            .1,
    )
    .expect("PUT body is JSON");
    let keys: Vec<&str> = body["keys"]
        .as_array()
        .expect("PUT body has a `keys` array")
        .iter()
        .map(|k| k.as_str().expect("key is a string"))
        .collect();
    assert!(
        keys.contains(&"ALPHA"),
        "exposing BRAVO dropped the already-exposed ALPHA: {keys:?}"
    );
    assert!(
        keys.contains(&"BRAVO"),
        "exposing BRAVO did not add BRAVO: {keys:?}"
    );
}

/// The mirror of the above for `unexpose`: it must remove exactly one
/// name and PUT the remainder, not clear the list.
#[cfg(unix)]
#[test]
fn secret_unexpose_keeps_the_other_exposed_names() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let request_log = temp.path().join("curl-requests");
    write_curl_stub(temp.path(), r#"{"expose":["ALPHA","BRAVO"]}"#);

    let output = run_secret(
        temp.path(),
        &request_log,
        &["secret", "unexpose", "BRAVO", "--app=app_123"],
    );
    assert!(
        output.status.success(),
        "secret unexpose failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = std::fs::read_to_string(&request_log).expect("read curl request log");
    let put = log
        .lines()
        .find(|l| l.starts_with("PUT "))
        .unwrap_or_else(|| panic!("unexpose issued no PUT:\n{log}"));
    let body: serde_json::Value =
        serde_json::from_str(put.split_once("/env/expose ").expect("body").1)
            .expect("PUT body is JSON");
    assert_eq!(
        body["keys"],
        serde_json::json!(["ALPHA"]),
        "unexpose BRAVO should leave exactly [ALPHA]:\n{log}"
    );
}

/// `secret set NAME=value --expose` is the one-command common case: it
/// must both store the secret and add it to the expose list.
#[cfg(unix)]
#[test]
fn secret_set_with_expose_flag_stores_then_exposes() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let request_log = temp.path().join("curl-requests");
    write_curl_stub(temp.path(), r#"{"expose":["ALPHA"]}"#);

    let output = run_secret(
        temp.path(),
        &request_log,
        &["secret", "set", "BRAVO=sk-test", "--expose", "--app=app_123"],
    );
    assert!(
        output.status.success(),
        "secret set --expose failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = std::fs::read_to_string(&request_log).expect("read curl request log");
    let lines: Vec<&str> = log.lines().collect();
    assert!(
        lines[0].starts_with("POST https://control.example.test/api/apps/app_123/secrets "),
        "the secret must be stored before it is exposed:\n{log}"
    );
    let put = lines
        .iter()
        .find(|l| l.starts_with("PUT "))
        .unwrap_or_else(|| panic!("--expose issued no PUT:\n{log}"));
    let body: serde_json::Value =
        serde_json::from_str(put.split_once("/env/expose ").expect("body").1)
            .expect("PUT body is JSON");
    assert_eq!(
        body["keys"],
        serde_json::json!(["ALPHA", "BRAVO"]),
        "set --expose should union BRAVO onto the existing list:\n{log}"
    );
}

/// Write a `curl` stub that logs `METHOD URL BODY` one request per line
/// and answers every GET with `get_response`.
#[cfg(unix)]
fn write_curl_stub(dir: &Path, get_response: &str) {
    use std::os::unix::fs::PermissionsExt;

    let curl = dir.join("curl");
    // `-X <method>` and the URL are both in "$@"; the request body (if
    // any) arrives on stdin via `--data-binary @-`.
    let script = format!(
        r#"#!/bin/sh
method=""
url=""
prev=""
for a in "$@"; do
  if [ "$prev" = "-X" ]; then method="$a"; fi
  case "$a" in http*) url="$a";; esac
  prev="$a"
done
body=""
if [ "$method" != "GET" ] && [ "$method" != "DELETE" ]; then
  body=$(cat)
fi
printf '%s %s %s\n' "$method" "$url" "$body" >> "$ZEROSHIP_CURL_LOG"
printf '%s\n200\n' '{get_response}'
"#
    );
    std::fs::write(&curl, script).expect("write curl stub");
    let mut permissions = std::fs::metadata(&curl)
        .expect("read curl stub metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&curl, permissions).expect("make curl stub executable");
}

#[cfg(unix)]
fn run_secret(path_dir: &Path, request_log: &Path, args: &[&str]) -> Output {
    // Class `external` even though the reader is a test: `PATH` is the OS's
    // contract, not a zeroship test knob. The consumer marker is what records
    // that a test read it.
    let inherited_path = zeroship_core::declared_env_os!(
        external,
        "PATH",
        zeroship_core::config::TestHarness
    )
    .unwrap_or_default();
    let mut paths = vec![path_dir.to_path_buf()];
    paths.extend(std::env::split_paths(&inherited_path));
    let path = std::env::join_paths(paths).expect("construct PATH for curl stub");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zeroship"));
    cmd.args(args);
    cmd.args(["--control=https://control.example.test", "--token=test-token"]);
    cmd.env("PATH", path)
        .env("ZEROSHIP_CURL_LOG", request_log)
        .output()
        .expect("run zeroship secret")
}

#[cfg(unix)]
fn run_secret_rm(path_dir: &Path, request_log: &Path, key: &str) -> Output {
    // Class `external` even though the reader is a test: `PATH` is the OS's
    // contract, not a zeroship test knob. The consumer marker is what records
    // that a test read it.
    let inherited_path = zeroship_core::declared_env_os!(
        external,
        "PATH",
        zeroship_core::config::TestHarness
    )
    .unwrap_or_default();
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
