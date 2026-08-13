use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use base64::Engine as _;
use ed25519_dalek::pkcs8::DecodePrivateKey as _;
use ed25519_dalek::SigningKey;
use zeroship_core::config::{
    validate_master_key_material, validate_pairwise_salt, validate_stash_key, validate_worker_key,
};

const SECRET_FILES: [&str; 7] = [
    "auth-signing.pem",
    "broker-secret",
    "control-signing.pem",
    "gateway-signing.pem",
    "pairwise-salt",
    "refresh-hash-key",
    "refresh-idem-key",
];

// STRIPE_WEBHOOK_SECRET is NOT here: only Stripe can issue a value that
// verifies, so `dev init` no longer manufactures one. See
// `dev_init_never_generates_a_stripe_webhook_secret`.
// Sorted, and every one is a canonical `ZEROSHIP_` name that some binary
// declares. `GATEWAY_OIDC_SECRET` was dropped rather than renamed: nothing in
// the tree reads it, so generating it only made an unread slot look configured.
const ENV_KEYS: [&str; 8] = [
    "ZEROSHIP_AUTH_STASH_SIGNING_KEY",
    "ZEROSHIP_AUTH_TOTP_ENC_KEY",
    "ZEROSHIP_CONTROL_KEY",
    "ZEROSHIP_CONTROL_MASTER_KEY",
    "ZEROSHIP_GATEWAY_STASH_SIGNING_KEY",
    "ZEROSHIP_MIGRATED_POLICY_SEAL_KEY",
    "ZEROSHIP_PAIRWISE_SALT",
    "ZEROSHIP_WORKER_KEY",
];

const WEAK_LITERALS: [&str; 6] = [
    "platform-key",
    "master-key",
    "dev-worker-key-not-for-production-use",
    "dev-secret-rotate-me-too",
    "dev-stash-signing-key-not-for-production",
    "dev-pairwise-salt-never-rotate-in-prod",
];

#[test]
fn dev_init_generates_the_complete_private_deployment_secret_set() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let secrets_dir = temp.path().join("secrets");
    let env_file = temp.path().join("dev.env");

    let output = run_dev_init(&secrets_dir, &env_file);
    assert_success(&output, "first zeroship dev init");

    assert_eq!(directory_entries(&secrets_dir), SECRET_FILES);
    assert_private_mode(&secrets_dir, 0o700);
    for name in SECRET_FILES {
        assert_private_mode(&secrets_dir.join(name), 0o600);
    }
    assert_private_mode(&env_file, 0o600);

    let mut private_keys = BTreeSet::new();
    for name in [
        "auth-signing.pem",
        "gateway-signing.pem",
        "control-signing.pem",
    ] {
        let pem = std::fs::read_to_string(secrets_dir.join(name))
            .unwrap_or_else(|error| panic!("read {name}: {error}"));
        assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----\n"), "{name}");
        assert!(pem.ends_with("-----END PRIVATE KEY-----\n"), "{name}");
        let key = SigningKey::from_pkcs8_pem(&pem)
            .unwrap_or_else(|error| panic!("parse {name} as Ed25519 PKCS#8 PEM: {error}"));
        assert!(
            private_keys.insert(key.to_bytes()),
            "{name} duplicates another generated Ed25519 key"
        );
    }
    assert_eq!(private_keys.len(), 3);

    assert_base64_file(&secrets_dir.join("broker-secret"), 48);
    assert_base64_file(&secrets_dir.join("refresh-idem-key"), 48);

    let refresh_hash = std::fs::read_to_string(secrets_dir.join("refresh-hash-key"))
        .expect("read refresh-hash-key");
    assert!(refresh_hash.ends_with('\n'));
    let (version, encoded) = refresh_hash
        .trim_end_matches('\n')
        .split_once(':')
        .expect("refresh-hash-key is version:key");
    assert_eq!(version, "1");
    assert_eq!(
        hex::decode(encoded).expect("refresh hash key is hex").len(),
        48
    );

    let overlay = parse_generated_env(&env_file);
    assert_eq!(
        overlay.keys().map(String::as_str).collect::<Vec<_>>(),
        ENV_KEYS
    );
    let mut env_values = BTreeSet::new();
    for (name, value) in &overlay {
        let decoded =
            hex::decode(value).unwrap_or_else(|error| panic!("{name} must be hex: {error}"));
        assert_eq!(decoded.len(), 32, "{name} must encode 32 random bytes");
        assert!(
            decoded.iter().any(|byte| *byte != 0),
            "{name} must not be all zero"
        );
        assert!(
            env_values.insert(value),
            "{name} duplicates another env secret"
        );
    }

    validate_master_key_material("ZEROSHIP_CONTROL_MASTER_KEY", &overlay["ZEROSHIP_CONTROL_MASTER_KEY"])
        .expect("generated master key must pass the production boot guard");
    validate_worker_key(&overlay["ZEROSHIP_WORKER_KEY"])
        .expect("generated worker key must pass the production boot guard");
    validate_stash_key(&overlay["ZEROSHIP_GATEWAY_STASH_SIGNING_KEY"])
        .expect("generated gateway stash key must pass the production boot guard");
    validate_stash_key(&overlay["ZEROSHIP_AUTH_STASH_SIGNING_KEY"])
        .expect("generated auth stash key must pass the production boot guard");
    validate_pairwise_salt(&overlay["ZEROSHIP_PAIRWISE_SALT"])
        .expect("generated pairwise salt must pass the production boot guard");
    validate_master_key_material("ZEROSHIP_AUTH_TOTP_ENC_KEY", &overlay["ZEROSHIP_AUTH_TOTP_ENC_KEY"])
        .expect("generated TOTP key must pass the production boot guard");

    let broker = std::fs::read(secrets_dir.join("broker-secret"))
        .expect("read broker-secret for the production boot guard");
    zeroship_core::auth::validate_broker_master(&broker)
        .expect("generated broker secret must pass the production boot guard");

    let pairwise = std::fs::read(secrets_dir.join("pairwise-salt")).expect("read pairwise-salt");
    assert_eq!(pairwise, overlay["ZEROSHIP_PAIRWISE_SALT"].as_bytes());
    assert!(
        !pairwise.ends_with(b"\n"),
        "pairwise-salt must have no newline"
    );
}

#[test]
fn dev_init_keeps_every_existing_secret_byte_for_byte() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let secrets_dir = temp.path().join("secrets");
    let env_file = temp.path().join("dev.env");

    let first = run_dev_init(&secrets_dir, &env_file);
    assert_success(&first, "first zeroship dev init");
    let before = snapshot(&secrets_dir, &env_file);

    let second = run_dev_init(&secrets_dir, &env_file);
    assert_success(&second, "second zeroship dev init");
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("0 created"),
        "second run did not report idempotence:\n{stderr}"
    );
    assert_eq!(snapshot(&secrets_dir, &env_file), before);
}

#[test]
fn dev_init_rejects_invalid_existing_material_without_overwriting_it() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let secrets_dir = temp.path().join("secrets");
    let env_file = temp.path().join("dev.env");

    let first = run_dev_init(&secrets_dir, &env_file);
    assert_success(&first, "first zeroship dev init");
    let corrupt_path = secrets_dir.join("auth-signing.pem");
    std::fs::write(&corrupt_path, b"not an Ed25519 private key")
        .expect("corrupt existing signing key");
    let before = snapshot(&secrets_dir, &env_file);

    let output = run_dev_init(&secrets_dir, &env_file);
    assert!(
        !output.status.success(),
        "invalid existing key unexpectedly succeeded\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid"), "stderr={stderr}");
    assert!(stderr.contains("not replaced"), "stderr={stderr}");
    assert_eq!(snapshot(&secrets_dir, &env_file), before);
}

#[test]
fn dev_init_rejects_an_env_file_that_aliases_any_generated_secret() {
    for name in SECRET_FILES {
        let temp = tempfile::tempdir().expect("create temp directory");
        let secrets_dir = temp.path().join("secrets");
        let env_file = secrets_dir.join(name);

        let output = run_dev_init(&secrets_dir, &env_file);
        assert!(
            !output.status.success(),
            "environment overlay unexpectedly aliased {name}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("outside") || stderr.contains("collides"),
            "stderr={stderr}"
        );
        assert!(
            directory_entries(&secrets_dir).is_empty(),
            "alias failure created secret state for {name}"
        );
    }
}

#[cfg(unix)]
#[test]
fn dev_init_rejects_an_env_file_hard_linked_to_a_generated_secret() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let secrets_dir = temp.path().join("secrets");
    let env_file = temp.path().join("dev.env");
    let first = run_dev_init(&secrets_dir, &env_file);
    assert_success(&first, "first zeroship dev init");
    let before = snapshot(&secrets_dir, &env_file);

    std::fs::remove_file(&env_file).expect("remove original env file");
    std::fs::hard_link(secrets_dir.join("broker-secret"), &env_file)
        .expect("hard-link env file to broker secret");
    let output = run_dev_init(&secrets_dir, &env_file);
    assert!(!output.status.success(), "hard-linked env file succeeded");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("aliases"), "stderr={stderr}");
    assert_eq!(
        std::fs::read(secrets_dir.join("broker-secret")).expect("reread broker secret"),
        before["broker-secret"]
    );
}

#[test]
fn dev_init_rejects_interpolated_managed_values_without_creating_secrets() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let secrets_dir = temp.path().join("secrets");
    let env_file = temp.path().join("dev.env");
    let original = b"ZEROSHIP_PAIRWISE_SALT=${PAIRWISE_SALT_FROM_ANOTHER_SOURCE}\n";
    std::fs::write(&env_file, original).expect("write interpolated env value");

    let output = run_dev_init(&secrets_dir, &env_file);
    assert!(
        !output.status.success(),
        "interpolated pairwise salt unexpectedly succeeded"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("64 lowercase hex"), "stderr={stderr}");
    assert_eq!(std::fs::read(&env_file).expect("reread env file"), original);
    assert!(directory_entries(&secrets_dir).is_empty());
}

#[test]
fn dev_init_rejects_an_empty_pairwise_file_before_creating_siblings() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let secrets_dir = temp.path().join("secrets");
    let env_file = temp.path().join("dev.env");
    std::fs::create_dir(&secrets_dir).expect("create secrets directory");
    std::fs::write(secrets_dir.join("pairwise-salt"), b"").expect("write empty salt");

    let output = run_dev_init(&secrets_dir, &env_file);
    assert!(!output.status.success(), "empty pairwise salt succeeded");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("empty"), "stderr={stderr}");
    assert_eq!(directory_entries(&secrets_dir), ["pairwise-salt"]);
    assert!(!env_file.exists());
}

#[test]
fn compose_preserves_shared_secret_topology_and_has_no_weak_literals() {
    let root = workspace_root();
    let compose_path = root.join("deploy/compose/docker-compose.yml");
    let compose = std::fs::read_to_string(&compose_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", compose_path.display()));

    let control = service_block(&compose, "control");
    let migrated = service_block(&compose, "migrated");
    let gateway = service_block(&compose, "gateway");
    let worker = service_block(&compose, "worker");
    let auth = service_block(&compose, "auth");

    for (name, block) in [
        ("control", control),
        ("migrated", migrated),
        ("gateway", gateway),
        ("auth", auth),
    ] {
        assert!(
            !block.contains("--dev-insecure"),
            "{name} still enables the deleted security-relaxation flag"
        );
    }

    assert!(gateway.contains("ZEROSHIP_GATEWAY_BROKER_SECRET_FILE: /etc/zeroship/secrets/broker-secret"));
    assert!(auth.contains("ZEROSHIP_AUTH_BROKER_SECRET_FILE: /etc/zeroship/secrets/broker-secret"));
    assert_eq!(
        compose
            .matches("/etc/zeroship/secrets/broker-secret")
            .count(),
        2,
        "gateway and auth must read the same single broker-secret path"
    );
    let shared_mount = "${ZEROSHIP_SECRETS_DIR:-./secrets}:/etc/zeroship/secrets:ro";
    assert!(gateway.contains(shared_mount));
    assert!(auth.contains(shared_mount));

    let required_pairwise =
        "ZEROSHIP_PAIRWISE_SALT: ${ZEROSHIP_PAIRWISE_SALT:?run zeroship dev init}";
    assert!(control.contains(required_pairwise));
    assert!(gateway.contains(required_pairwise));
    assert!(auth.contains("ZEROSHIP_AUTH_PAIRWISE_SALT_FILE: /etc/zeroship/secrets/pairwise-salt"));
    assert_eq!(compose.matches(required_pairwise).count(), 2);

    let required_control = "ZEROSHIP_CONTROL_KEY: ${ZEROSHIP_CONTROL_KEY:?run zeroship dev init}";
    for (name, block) in [
        ("control", control),
        ("gateway", gateway),
        ("worker", worker),
        ("auth", auth),
    ] {
        assert!(
            block.contains(required_control),
            "{name} is not wired to the generated control key"
        );
    }
    assert!(
        migrated.contains(required_control),
        "migrated is not wired to the same generated control key"
    );
    assert_eq!(
        compose
            .matches("${ZEROSHIP_CONTROL_KEY:?run zeroship dev init}")
            .count(),
        5,
        "all five native services must consume one generated control key"
    );

    // Stripe is OPTIONAL: only Stripe can issue a webhook secret that verifies,
    // so compose must render without one rather than force a Stripe-less
    // deployment to carry a locally generated placeholder. Empty is not a
    // relaxation - control rejects every delivery with 500 when the secret is
    // empty (crates/control/tests/stripe_webhook_test.rs).
    assert!(
        control.contains("ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET: ${STRIPE_WEBHOOK_SECRET:-}"),
        "STRIPE_WEBHOOK_SECRET must be optional with an empty default"
    );
    assert!(
        !compose.contains("${STRIPE_WEBHOOK_SECRET:?"),
        "no service may REQUIRE a Stripe webhook secret to render compose"
    );
    assert!(migrated.contains(
        "ZEROSHIP_MIGRATED_POLICY_SEAL_KEY: ${ZEROSHIP_MIGRATED_POLICY_SEAL_KEY:?run zeroship dev init}"
    ));
    assert!(migrated.contains(
        "AUTH_PLATFORM_ISSUER: ${ZEROSHIP_ORIGIN_SCHEME:-http}://auth.${ZEROSHIP_DOMAIN:-zeroship.localhost}/oauth2"
    ));

    let active_compose = compose
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    for weak in WEAK_LITERALS {
        assert!(
            !active_compose.contains(weak),
            "active compose config still contains shipped weak literal {weak:?}"
        );
    }
}

#[test]
fn generated_default_secret_paths_are_gitignored() {
    let root = workspace_root();
    for path in ["deploy/compose/secrets/probe-secret", "deploy/compose/.env"] {
        let output = Command::new("git")
            .current_dir(&root)
            .args(["check-ignore", "--no-index", "-v", "--", path])
            .output()
            .unwrap_or_else(|error| panic!("run git check-ignore for {path}: {error}"));
        assert!(
            output.status.success(),
            "generated path {path} is not gitignored\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// `dev init` must NOT manufacture a Stripe webhook secret. Only Stripe issues a
/// value that verifies (`whsec_...`), so a generated one is inert: it makes an
/// unconfigured deployment indistinguishable from a configured one while every
/// real delivery still fails signature verification. The compose default is
/// empty and control fails those deliveries closed with 500.
///
/// This asserts ABSENCE only. It does not check that anything downstream still
/// works without the value; that is the compose render plus the control webhook
/// tests.
#[test]
fn dev_init_never_generates_a_stripe_webhook_secret() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let secrets_dir = temp.path().join("secrets");
    let env_file = temp.path().join("dev.env");

    let output = run_dev_init(&secrets_dir, &env_file);
    assert_success(&output, "zeroship dev init");

    let overlay = parse_generated_env(&env_file);
    assert!(
        !overlay.contains_key("STRIPE_WEBHOOK_SECRET"),
        "dev init generated a Stripe webhook secret it cannot possibly issue: {:?}",
        overlay.keys().collect::<Vec<_>>()
    );
    let raw = std::fs::read_to_string(&env_file).expect("read generated env file");
    assert!(
        !raw.contains("STRIPE_WEBHOOK_SECRET"),
        "the generated env file still mentions STRIPE_WEBHOOK_SECRET"
    );
}

/// The same defect as the Stripe one above, and the reason it is asserted
/// separately: `GATEWAY_OIDC_SECRET` was generated into the overlay and read by
/// NOTHING. The name asserts that the gateway OIDC path is configured, so an
/// operator auditing .env sees a plausible secret and stops looking, while the
/// value that actually matters is a FILE PATH under a different name
/// (`ZEROSHIP_GATEWAY_BROKER_SECRET_FILE`). Without this assertion nothing
/// stops it being added back, because an unread key breaks no test.
///
/// This asserts ABSENCE only. It does not prove the gateway is configured; that
/// is the compose render assertion plus the gateway boot guard.
#[test]
fn dev_init_never_generates_the_unread_gateway_oidc_secret() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let secrets_dir = temp.path().join("secrets");
    let env_file = temp.path().join("dev.env");

    let output = run_dev_init(&secrets_dir, &env_file);
    assert_success(&output, "zeroship dev init");

    let overlay = parse_generated_env(&env_file);
    assert!(
        !overlay.contains_key("GATEWAY_OIDC_SECRET"),
        "dev init generated a key nothing reads: {:?}",
        overlay.keys().collect::<Vec<_>>()
    );
    let raw = std::fs::read_to_string(&env_file).expect("read generated env file");
    assert!(
        !raw.contains("GATEWAY_OIDC_SECRET"),
        "the generated env file still mentions GATEWAY_OIDC_SECRET"
    );
    // The one-variable control: the gateway secret that IS read is a path
    // setting, so it must NOT appear in the generated scalar overlay either -
    // and the file it names must exist.
    assert!(
        !overlay.contains_key("ZEROSHIP_GATEWAY_BROKER_SECRET_FILE"),
        "a path setting must not be generated as an overlay scalar"
    );
    assert!(
        secrets_dir.join("broker-secret").exists(),
        "the broker master secret file the gateway actually reads was not created"
    );
}

fn run_dev_init(secrets_dir: &Path, env_file: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zeroship"))
        .args(["dev", "init", "--secrets-dir"])
        .arg(secrets_dir)
        .arg("--env-file")
        .arg(env_file)
        .output()
        .expect("run zeroship dev init")
}

fn assert_success(output: &Output, context: &str) {
    assert!(
        output.status.success(),
        "{context} failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn directory_entries(path: &Path) -> Vec<String> {
    let mut entries = std::fs::read_dir(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .map(|entry| {
            entry
                .expect("read directory entry")
                .file_name()
                .into_string()
                .expect("secret filename is UTF-8")
        })
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

fn assert_base64_file(path: &Path, decoded_len: usize) {
    let body = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    assert!(
        body.ends_with('\n'),
        "{} must end in a newline",
        path.display()
    );
    assert_eq!(body.matches('\n').count(), 1, "{}", path.display());
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(body.trim_end_matches('\n'))
        .unwrap_or_else(|error| panic!("decode {} as base64: {error}", path.display()));
    assert_eq!(decoded.len(), decoded_len, "{}", path.display());
    assert!(decoded.iter().any(|byte| *byte != 0), "{}", path.display());
}

fn parse_generated_env(path: &Path) -> BTreeMap<String, String> {
    let body = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let mut values = BTreeMap::new();
    for line in body.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, value) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("generated env line is not NAME=value: {line:?}"));
        assert!(values.insert(name.to_string(), value.to_string()).is_none());
    }
    values
}

fn snapshot(secrets_dir: &Path, env_file: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    for name in SECRET_FILES {
        files.insert(
            name.to_string(),
            std::fs::read(secrets_dir.join(name))
                .unwrap_or_else(|error| panic!("read snapshot file {name}: {error}")),
        );
    }
    files.insert(
        "environment-overlay".to_string(),
        std::fs::read(env_file).expect("read environment overlay snapshot"),
    );
    files
}

#[cfg(unix)]
fn assert_private_mode(path: &Path, expected: u32) {
    use std::os::unix::fs::PermissionsExt as _;

    let actual = std::fs::metadata(path)
        .unwrap_or_else(|error| panic!("stat {}: {error}", path.display()))
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(actual, expected, "wrong mode on {}", path.display());
}

#[cfg(not(unix))]
fn assert_private_mode(_path: &Path, _expected: u32) {}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// No server binary may declare the deleted security-relaxation flag again.
///
/// Each of the five binaries already has its own `try_parse_from(["...",
/// "--dev-insecure"])` rejection test. Those are per-crate and prove only that
/// TODAY'S parser rejects it; this one is cross-crate and keys on the
/// DECLARATION, so a re-added arg fails here even in a crate whose own suite
/// was not run.
///
/// WHAT THIS DOES NOT CATCH, and it is the realistic remaining hole: a
/// hand-rolled `std::env::var("ZEROSHIP_DEV_INSECURE")` read that never goes
/// through clap. Only the clap spellings are matched, because the existing
/// rejection tests legitimately contain the bare strings inside `mod tests` and
/// a bare-string scan would flag them.
#[test]
fn no_server_binary_redeclares_the_relaxation_flag() {
    let root = workspace_root();
    let mut sources = Vec::new();
    for crate_name in ["control", "gateway", "worker", "auth", "migrated", "cli"] {
        collect_rs_files(&root.join("crates").join(crate_name).join("src"), &mut sources);
    }
    assert!(
        sources.len() > 50,
        "expected to scan the five server crates plus the CLI, found only {} files -- \
         the walk is broken and this test would pass over nothing",
        sources.len()
    );

    let mut offenders = Vec::new();
    for path in &sources {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        if text.contains(r#"env = "ZEROSHIP_DEV_INSECURE""#)
            || text.contains(r#"long = "dev-insecure""#)
            || text.contains(r#"long = "insecure-dev""#)
        {
            offenders.push(path.display().to_string());
        }
    }
    assert!(
        offenders.is_empty(),
        "the deleted security-relaxation flag is declared again in: {}",
        offenders.join(", ")
    );
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

fn service_block<'a>(compose: &'a str, service: &str) -> &'a str {
    let marker = format!("\n  {service}:\n");
    let start = compose
        .find(&marker)
        .unwrap_or_else(|| panic!("compose service {service:?} is missing"));
    let body_start = start + marker.len();
    let body = &compose[body_start..];
    let end = body
        .match_indices('\n')
        .map(|(index, _)| index + 1)
        .find(|index| {
            let rest = &body[*index..];
            rest.starts_with("  ")
                && !rest.starts_with("    ")
                && rest
                    .lines()
                    .next()
                    .is_some_and(|line| line.trim_end().ends_with(':'))
        })
        .unwrap_or(body.len());
    &body[..end]
}
