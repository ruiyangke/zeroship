//! The production binary refuses to start when lifecycle publication could
//! not run, and does so before it touches the database.
//!
//! Every case runs the real `zeroship-control` with a cleared environment and
//! a database URL nothing listens on. A run that got past the refusal under
//! test would fail differently, on that database, which is what the controls
//! beside each refusal show.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
use ed25519_dalek::pkcs8::EncodePrivateKey;
use zeroship_core::service_assertion::ServiceSigningKey;

const STRONG_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const UNREACHABLE_DATABASE: &str = "postgres://nobody@127.0.0.1:1/unreachable";

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "zeroship_control_startup_{tag}_{}_{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(path.join("deploy")).expect("create scratch dir");
        Self(path)
    }

    fn secret(&self, name: &str, contents: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, contents).expect("write secret");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("private secret");
        path
    }

    /// Control's service key and a peer document naming it.
    fn service_key(&self) -> (PathBuf, PathBuf) {
        let key = ed25519_dalek::SigningKey::from_bytes(&[41; 32]);
        let signer =
            ServiceSigningKey::from_pkcs8_der(key.to_pkcs8_der().unwrap().as_bytes()).unwrap();
        let issuer = zeroship_control::internal::control_service_issuer().unwrap();
        let peers = serde_json::json!({"keys": [{
            "kty": "OKP",
            "crv": "Ed25519",
            "x": signer.public_jwk_x(),
            "iss": issuer.as_str(),
        }]});
        (
            self.secret(
                "control.pem",
                key.to_pkcs8_pem(LineEnding::LF).unwrap().as_bytes(),
            ),
            self.secret("peers.json", &serde_json::to_vec(&peers).unwrap()),
        )
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(scratch: &Scratch, env: &[(&str, &Path)], args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zeroship-control"));
    command
        .env_clear()
        .current_dir(&scratch.0)
        .env("ZEROSHIP_CONTROL_KEY", STRONG_HEX)
        .env("ZEROSHIP_PAIRWISE_SALT", STRONG_HEX)
        .env("ZEROSHIP_CONTROL_MASTER_KEY", STRONG_HEX)
        .env("ZEROSHIP_CONTROL_DATABASE_URL", UNREACHABLE_DATABASE);
    for (name, value) in env {
        command.env(name, value);
    }
    command
        .args(["--no-config", "--auth-platform-issuer", "http://platform.test/oauth2"])
        .arg("--deploy-tmp-dir")
        .arg(scratch.0.join("deploy"))
        .args(args)
        .output()
        .expect("spawn zeroship-control")
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn unusable_publication_settings_refuse_the_check_and_the_boot() {
    let scratch = Scratch::new("settings");
    let (key, peers) = scratch.service_key();
    let signed = [
        ("ZEROSHIP_CONTROL_SERVICE_KEY_FILE", key.as_path()),
        ("ZEROSHIP_CONTROL_SERVICE_PEERS_FILE", peers.as_path()),
    ];
    for (setting, value) in [
        ("--workflow-coordinator-url", "http://coordinator.internal:9093"),
        ("--workflow-coordinator-url", "https://coordinator.internal/manager"),
        ("--catalog-max-connections", "0"),
    ] {
        let name = setting.trim_start_matches("--").replace('-', "_");
        for check in [true, false] {
            let mut args = vec![setting, value];
            if check {
                args.push("--check-config");
            }
            let output = run(&scratch, &signed, &args);
            let text = text(&output);
            assert_eq!(output.status.code(), Some(2), "{args:?}: {text}");
            assert!(
                text.contains(&format!("control.{name}")),
                "the refusal names the setting: {text}"
            );
            assert!(!text.contains("failed to connect to database"), "{text}");
        }
    }

    // The controls: usable values pass the check and reach the report.
    let output = run(
        &scratch,
        &signed,
        &[
            "--check-config",
            "--check-config-format",
            "json",
            "--workflow-coordinator-url",
            "https://coordinator.internal",
            "--catalog-max-connections",
            "3",
        ],
    );
    let text = text(&output);
    assert!(output.status.success(), "{text}");
    assert!(text.contains(r#""workflow_coordinator_url":"https://coordinator.internal""#), "{text}");
    assert!(text.contains(r#""catalog_max_connections":3"#), "{text}");
}

#[test]
fn a_control_without_its_service_key_refuses_to_boot_before_the_database() {
    let scratch = Scratch::new("signer");
    let output = run(&scratch, &[], &[]);
    let refused = text(&output);
    assert_eq!(output.status.code(), Some(1), "{refused}");
    assert!(
        refused.contains("service key material rejected")
            && refused.contains("control.service_key_file"),
        "{refused}"
    );
    assert!(!refused.contains("failed to connect to database"), "{refused}");

    // The control: with its key the boot passes the signer check and fails
    // only at the unreachable database.
    let (key, peers) = scratch.service_key();
    let output = run(
        &scratch,
        &[
            ("ZEROSHIP_CONTROL_SERVICE_KEY_FILE", key.as_path()),
            ("ZEROSHIP_CONTROL_SERVICE_PEERS_FILE", peers.as_path()),
        ],
        &[],
    );
    let signed = text(&output);
    assert!(!output.status.success(), "{signed}");
    assert!(signed.contains("failed to connect to database"), "{signed}");
    assert!(!signed.contains("service key material rejected"), "{signed}");
}
