#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::process::{Command, Output};

#[cfg(unix)]
#[test]
fn deploy_warns_for_each_declared_secret_missing_from_the_app() {
    use std::os::unix::fs::PermissionsExt;

    let project = tempfile::tempdir().expect("create project directory");
    let curl_log = project.path().join("curl.log");
    let curl = project.path().join("curl");
    std::fs::write(
        &curl,
        r#"#!/bin/sh
method="GET"
url=""
previous=""
for argument in "$@"; do
  if [ "$previous" = "-X" ]; then method="$argument"; fi
  case "$argument" in http*) url="$argument";; esac
  previous="$argument"
done
if [ "$method" = "POST" ]; then cat >/dev/null; fi
printf '%s %s\n' "$method" "$url" >> "$ZEROSHIP_CURL_LOG"
case "$url" in
  */secrets) printf '%s\n200\n' '{"secrets":["PRESENT"]}' ;;
  */deploy) printf '%s\n200\n' '{"deploy_hash":"sha256:test"}' ;;
  *) printf '%s\n404\n' '{"error":"unexpected URL"}' ;;
esac
"#,
    )
    .expect("write curl stub");
    let mut permissions = std::fs::metadata(&curl)
        .expect("read curl stub metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&curl, permissions).expect("make curl stub executable");

    std::fs::create_dir(project.path().join("dist")).expect("create dist");
    std::fs::write(project.path().join("dist/app.zship"), b"test archive")
        .expect("write archive");
    std::fs::write(
        project.path().join("zeroship.jsonc"),
        r#"{
  "name": "secret-warning-test",
  "app": "11111111-1111-4111-8111-111111111111",
  "control": "https://control.example.test",
  "runtime_date": "2026-08-14",
  "build": { "mode": "full", "dist": "dist", "output": "dist/app.zship" },
  "migrations": { "dir": "migrations", "out": "generated/zeroship" },
  "secrets": ["PRESENT", "MISSING"]
}
"#,
    )
    .expect("write project config");

    let output = run_deploy(project.path(), &curl_log);
    assert!(
        output.status.success(),
        "deploy failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = std::fs::read_to_string(&curl_log).expect("read curl log");
    assert_eq!(
        requests.lines().collect::<Vec<_>>(),
        [
            "GET https://control.example.test/api/apps/11111111-1111-4111-8111-111111111111/secrets",
            "POST https://control.example.test/api/apps/11111111-1111-4111-8111-111111111111/deploy",
        ],
        "the secret-name check must happen before the archive upload:\n{requests}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "zeroship deploy: warning: zeroship.jsonc declares `MISSING`, but `zeroship secret list` does not show it"
        ),
        "missing secret warning absent:\n{stderr}"
    );
    assert!(
        !stderr.contains("declares `PRESENT`"),
        "a configured secret was falsely reported missing:\n{stderr}"
    );
}

#[cfg(unix)]
fn run_deploy(project: &Path, curl_log: &Path) -> Output {
    let inherited_path = zeroship_core::declared_env_os!(
        external,
        "PATH",
        zeroship_core::config::TestHarness
    )
    .unwrap_or_default();
    let mut paths = vec![project.to_path_buf()];
    paths.extend(std::env::split_paths(&inherited_path));
    let path = std::env::join_paths(paths).expect("construct PATH for curl stub");

    Command::new(env!("CARGO_BIN_EXE_zeroship"))
        .arg("deploy")
        .current_dir(project)
        .env("PATH", path)
        .env("ZEROSHIP_CURL_LOG", curl_log)
        .env("ZEROSHIP_TOKEN", "test-token")
        .env_remove("ZEROSHIP_CONFIG")
        .env_remove("ZEROSHIP_CONTROL_URL")
        .output()
        .expect("run zeroship deploy")
}
