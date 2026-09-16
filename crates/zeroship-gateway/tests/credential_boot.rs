//! The gateway's refusal to start on an unconfigured service credential,
//! driven against the real binary.
//!
//! `zeroship-core`'s unit tests rule on the credential functions `main` calls.
//! They cannot rule on whether `main` calls them, or on what an operator sees
//! when it does. These tests spawn the shipped binary and read its exit status,
//! banner and `--check-config` report.
//!
//! The sentinel and the remediation command are imported from the product
//! rather than restated here, so a rename moves both together.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use zeroship_core::config::{REMEDIATION_COMMAND, SERVICE_CREDENTIAL_SENTINEL};

/// Material for the two credentials the gateway always validates. Only the
/// control key varies between these tests.
const STRONG: &str = "0123456789abcdef0123456789abcdef";

/// A temp directory that removes itself even when an assertion panics.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "zeroship_gate_credential_{tag}_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create scratch dir");
        Self(path)
    }

    /// A file the secret resolver accepts: owner-only, as it refuses any secret
    /// a second local account could read.
    fn write_secret(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, contents).expect("write fixture");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("owner-only fixture secret");
        }
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `env_clear` first: an inherited `ZEROSHIP_*` value from the caller's shell
/// reaches the same clap carrier these tests do, so ADDING variables would make
/// the assertion about the developer's shell whenever one happened to be set.
fn dry_run(control_key: &str, extra: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zeroship-gate"));
    cmd.env_clear()
        .env("ZEROSHIP_CONTROL_KEY", control_key)
        .env("ZEROSHIP_GATEWAY_STASH_SIGNING_KEY", STRONG)
        .env("ZEROSHIP_PAIRWISE_SALT", STRONG)
        .arg("--check-config")
        .arg("--check-config-format")
        .arg("json");
    cmd.args(extra);
    cmd.output().expect("spawn zeroship-gate")
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Pull one reported field out of the `--check-config` report.
///
/// A `--check-config` run also emits structured tracing lines on stdout - a
/// warning about a missing signing key file, for one - so the report is found
/// by CONTENT rather than by position. Absence panics rather than returning
/// `None`: it means the report stopped carrying a field these assertions
/// observe, and reporting that as an ordinary value mismatch would hide a moved
/// observation surface.
fn field(output: &Output, key: &str) -> serde_json::Value {
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let Ok(serde_json::Value::Object(map)) = serde_json::from_str(line.trim()) else {
            continue;
        };
        if let Some(value) = map.get(key) {
            return value.clone();
        }
    }
    panic!("no check-config JSON line carried a {key} field:\n{text}");
}

/// Two runs of the same refusal are never byte-identical as emitted because the
/// tracing lines carry a wall-clock timestamp. The identity being asserted is
/// about the MESSAGE, so drop the timestamp and compare what is left.
fn normalised(text: &str) -> String {
    text.lines()
        .map(|line| match serde_json::from_str::<serde_json::Value>(line) {
            Ok(mut value) => {
                if let Some(object) = value.as_object_mut() {
                    object.remove("timestamp");
                }
                value.to_string()
            }
            Err(_) => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The operator-facing banner: the ruled block between the first two 78-column
/// `=` rules.
fn banner_block(text: &str) -> String {
    let rule = "=".repeat(78);
    let mut block = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        if line.trim() == rule {
            block.push(line.to_string());
            if inside {
                break;
            }
            inside = true;
            continue;
        }
        if inside {
            block.push(line.to_string());
        }
    }
    block.join("\n")
}

/// A REAL boot on a placeholder, bounded. A successful boot binds a port and
/// serves, and the escape arm only needs the banner printed on the way there,
/// so the child is killed once the deadline passes.
fn boot_bounded(tag: &str, control_key: &str) -> (Option<i32>, String) {
    let log = std::env::temp_dir().join(format!(
        "zeroship_gate_boot_{tag}_{}.log",
        std::process::id()
    ));
    let sink = std::fs::File::create(&log).expect("create boot log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_zeroship-gate"))
        .env_clear()
        .env("ZEROSHIP_CONTROL_KEY", control_key)
        .env("ZEROSHIP_GATEWAY_STASH_SIGNING_KEY", STRONG)
        .env("ZEROSHIP_PAIRWISE_SALT", STRONG)
        .stdout(Stdio::from(sink.try_clone().expect("clone boot log")))
        .stderr(Stdio::from(sink))
        .spawn()
        .expect("spawn zeroship-gate");

    let deadline = Instant::now() + Duration::from_secs(20);
    let code = loop {
        match child.try_wait().expect("poll zeroship-gate") {
            Some(status) => break status.code(),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };

    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    (code, text)
}

#[test]
fn a_placeholder_control_key_is_refused_by_a_dry_run() {
    let output = dry_run(SERVICE_CREDENTIAL_SENTINEL, &[]);

    assert!(
        !output.status.success(),
        "a dry run accepted the {SERVICE_CREDENTIAL_SENTINEL} placeholder"
    );

    // The banner must name the KEY, the FILE and the exact remediation command,
    // and say which subsystem the credential belongs to.
    let banner = stderr_text(&output);
    for token in [
        SERVICE_CREDENTIAL_SENTINEL,
        REMEDIATION_COMMAND,
        "REFUSES TO START",
        "config file",
        "control-route-sync",
    ] {
        assert!(
            banner.contains(token),
            "the sentinel banner does not name {token:?}:\n{banner}"
        );
    }
}

#[test]
fn an_empty_control_key_is_refused_in_the_same_words_as_the_sentinel() {
    let sentinel = dry_run(SERVICE_CREDENTIAL_SENTINEL, &[]);
    let empty = dry_run("", &[]);

    assert!(
        !empty.status.success(),
        "a dry run accepted an EMPTY control key"
    );

    // The sharpest claim here is not "both failed" but "both failed in the same
    // words": a build that gave the empty case its own branch would satisfy a
    // both-failed check and fail this one.
    let a = normalised(&stderr_text(&sentinel));
    let b = normalised(&stderr_text(&empty));
    assert_eq!(
        a, b,
        "empty and sentinel must reach ONE branch, not two refusals that differ"
    );
}

#[test]
fn a_placeholder_on_a_real_boot_takes_the_development_escape_loudly() {
    let (code, text) = boot_bounded("sentinel", SERVICE_CREDENTIAL_SENTINEL);

    // The escape must be loud and must say what it is keyed to, so a reader of
    // the log can tell why the process did not refuse.
    for token in [
        "UNCONFIGURED SERVICE CREDENTIAL",
        "development build only",
        "debug_assertions",
        "/readyz",
    ] {
        assert!(
            text.contains(token),
            "the dev-escape banner does not say {token:?}:\n{text}"
        );
    }
    assert!(
        !text.contains("REFUSES TO START"),
        "a development build printed the production refusal (exit {code:?}):\n{text}"
    );
}

#[test]
fn an_empty_control_key_takes_the_same_escape_as_the_sentinel() {
    let (_, sentinel) = boot_bounded("esc_sentinel", SERVICE_CREDENTIAL_SENTINEL);
    let (_, empty) = boot_bounded("esc_empty", "");

    // Same branch, so the same banner - an empty credential that took its own
    // path would satisfy a "both escaped" check and fail this one.
    assert_eq!(
        banner_block(&sentinel),
        banner_block(&empty),
        "a real boot must reach ONE escape banner for empty and sentinel"
    );
}

#[test]
fn a_configured_control_key_is_accepted_and_reports_its_posture() {
    let output = dry_run("control-key-material", &[]);

    assert!(
        output.status.success(),
        "a correctly configured credential was REFUSED:\n{}",
        stderr_text(&output)
    );

    assert_eq!(
        field(&output, "service_credentials"),
        serde_json::Value::String("configured".to_string())
    );
    // The posture alone is not enough: `configured` over zero checked
    // credentials is the vacuous green this report must not be able to print.
    assert_eq!(
        field(&output, "service_credentials_checked"),
        serde_json::Value::from(3)
    );
    assert_eq!(
        field(&output, "service_credentials_skipped"),
        serde_json::Value::from(0)
    );
    assert_eq!(
        field(&output, "service_credentials_unread"),
        serde_json::Value::from(0)
    );
    assert!(
        !stderr_text(&output).contains("REFUSES TO START"),
        "a healthy configuration printed a refusal banner"
    );
}

#[test]
fn a_file_sourced_dry_run_reports_unverified_rather_than_configured() {
    let scratch = Scratch::new("files");
    let control = scratch.write_secret("control", "control-key-material");
    let stash = scratch.write_secret("stash", STRONG);
    let salt = scratch.write_secret("salt", STRONG);

    // A dry run resolved each credential's SOURCE without opening the file, so
    // it judged nothing. Reporting `configured` here would be a green built out
    // of zero readings.
    let output = dry_run(
        "",
        &[
            "--control-key-file",
            control.to_str().expect("UTF-8 path"),
            "--stash-signing-key-file",
            stash.to_str().expect("UTF-8 path"),
            "--pairwise-salt-file",
            salt.to_str().expect("UTF-8 path"),
        ],
    );

    assert!(
        output.status.success(),
        "a file-sourced dry run must not fail:\n{}",
        stderr_text(&output)
    );
    assert_eq!(
        field(&output, "service_credentials"),
        serde_json::Value::String("unverified".to_string())
    );
    assert_eq!(
        field(&output, "service_credentials_unread"),
        serde_json::Value::from(3)
    );
}
