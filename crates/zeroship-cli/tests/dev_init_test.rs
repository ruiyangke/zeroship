use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::os::unix::fs::PermissionsExt as _;
use std::process::{Command, Output};

use base64::Engine as _;
use ed25519_dalek::pkcs8::DecodePrivateKey as _;
use ed25519_dalek::SigningKey;
use zeroship_core::config::{
    validate_master_key_material, validate_pairwise_salt,
    validate_stash_key,
};

// `migrate-dsn` is the one entry that is NOT generated key material: it is the
// compose `migrate` one-shot's privileged DSN, provisioned rather than randomly
// derived. It belongs in this set anyway, and the deploy path is what decides
// that, not taste:
//
//   - deploy/compose/docker-compose.yml bind-mounts
//     ${ZEROSHIP_SECRETS_DIR:-./secrets}/migrate-dsn. If nothing creates the
//     file, Docker creates a DIRECTORY at that path and the one-shot fails
//     with an error that names neither the cause nor the fix.
//   - deploy/scripts/deploy-remote.sh `secret_files()` derives the host's
//     required secret files by grepping `/etc/zeroship/secrets/<name>` out of
//     that same compose file, so the mount alone makes this a file every
//     deploy must find present.
//   - `docker compose up` already refuses without `zeroship dev init` (the
//     `${...:?run zeroship dev init}` interpolations), so dev init is the
//     provisioning step for this stack, and it is the only writer.
//
// It carries the postgres SUPERUSER password, so of everything here it is the
// entry that most needs the 0600 the loop below pins.
const SECRET_FILES: [&str; 11] = [
    "auth-signing.pem",
    "broker-secret",
    "gateway-signing.pem",
    "migrate-dsn",
    "pairwise-salt",
    "refresh-hash-key",
    "refresh-idem-key",
    // One per-service assertion key, never one shared file: a peer must be able
    // to VERIFY a service without being able to IMPERSONATE it, and a key held
    // by four processes is a shared secret wearing a signature.
    "svc-auth.pem",
    "svc-control.pem",
    "svc-gateway.pem",
    "svc-worker.pem",
];

/// The one generated file that is NOT private, and must not become private.
///
/// It holds PUBLIC keys. Pinning it to 0600 alongside the private material
/// would read as "another secret", and the first operator who had to serve it
/// to a peer would loosen the whole directory instead of this one file. It is
/// named separately here so the distinction is asserted rather than assumed.
const PUBLIC_FILES: [&str; 1] = ["service-peers.json"];

// ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET is NOT here: only Stripe can issue a value that
// verifies, so `dev init` no longer manufactures one. See
// `dev_init_never_generates_a_stripe_webhook_secret`.
// Sorted, and every one is a canonical `ZEROSHIP_` name that some binary
// declares. `GATEWAY_OIDC_SECRET` was dropped rather than renamed: nothing in
// the tree reads it, so generating it only made an unread slot look configured.
const ENV_KEYS: [&str; 7] = [
    "ZEROSHIP_AUTH_STASH_SIGNING_KEY",
    "ZEROSHIP_AUTH_TOTP_ENC_KEY",
    "ZEROSHIP_CONTROL_KEY",
    "ZEROSHIP_CONTROL_MASTER_KEY",
    "ZEROSHIP_GATEWAY_STASH_SIGNING_KEY",
    "ZEROSHIP_MIGRATE_SERVER_POLICY_SEAL_KEY",
    "ZEROSHIP_PAIRWISE_SALT",
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

    let mut expected: Vec<String> = SECRET_FILES
        .into_iter()
        .chain(PUBLIC_FILES)
        .map(str::to_owned)
        .collect();
    expected.sort();
    assert_eq!(directory_entries(&secrets_dir).to_vec(), expected);
    assert_private_mode(&secrets_dir, 0o700);
    for name in SECRET_FILES {
        assert_private_mode(&secrets_dir.join(name), 0o600);
    }
    // The peer document is deliberately NOT in that loop; see PUBLIC_FILES.
    for name in PUBLIC_FILES {
        let mode = std::fs::metadata(secrets_dir.join(name))
            .unwrap_or_else(|error| panic!("stat {name}: {error}"))
            .permissions()
            .mode()
            & 0o777;
        assert_ne!(mode, 0o600, "{name} holds public keys and must not be private");
    }
    assert_private_mode(&env_file, 0o600);

    let mut private_keys = BTreeSet::new();
    for name in [
        "auth-signing.pem",
        "gateway-signing.pem",
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
    assert_eq!(private_keys.len(), 2);

    assert_base64_file(&secrets_dir.join("broker-secret"), 48);
    assert_base64_file(&secrets_dir.join("refresh-idem-key"), 48);

    // The migrate DSN must be a DSN, and it must address the compose network.
    // `zeroship-platform-migrate` reads this file verbatim as its connection
    // string, so a file that is merely present and private still fails the
    // one-shot if the contents are not a postgres URL for the `postgres`
    // service. The host is asserted because the value it must match is
    // POSTGRES_PASSWORD/`postgres` in deploy/compose/docker-compose.yml, and
    // the two drifting apart is the failure this pins.
    //
    // Does NOT cover: that the credential is correct for any real database, or
    // that the mode survives past the moment dev init writes it. It no longer
    // has to be the only protection, though: since the owner-only policy landed
    // in crates/zeroship-core/src/config/secrets.rs `read_secret_file`, a later chmod is
    // caught at the next read rather than passing silently, and
    // `zeroship-platform-migrate` reads this exact file through that function.
    let migrate_dsn =
        std::fs::read_to_string(secrets_dir.join("migrate-dsn")).expect("read migrate-dsn");
    assert!(
        migrate_dsn.ends_with('\n'),
        "migrate-dsn must end with a newline; got {migrate_dsn:?}"
    );
    assert_eq!(
        migrate_dsn.trim_end_matches('\n'),
        "postgres://postgres:zeroship@postgres:5432/zeroship",
        "migrate-dsn must address the compose postgres service with its POSTGRES_PASSWORD"
    );


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
    validate_stash_key(
        "ZEROSHIP_GATEWAY_STASH_SIGNING_KEY",
        &overlay["ZEROSHIP_GATEWAY_STASH_SIGNING_KEY"],
    )
    .expect("generated gateway stash key must pass the production boot guard");
    validate_stash_key(
        "ZEROSHIP_AUTH_STASH_SIGNING_KEY",
        &overlay["ZEROSHIP_AUTH_STASH_SIGNING_KEY"],
    )
    .expect("generated auth stash key must pass the production boot guard");
    validate_pairwise_salt("ZEROSHIP_PAIRWISE_SALT", &overlay["ZEROSHIP_PAIRWISE_SALT"])
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

/// Both privileged readers take the superuser DSN from the ONE file dev init
/// writes, and neither inlines it.
///
/// WHAT WENT WRONG. `migrate` has read `/etc/zeroship/secrets/migrate-dsn`
/// since 2026-08-16, but `migrate-server` - the other service that needs superuser
/// rights, to `CREATE SCHEMA` and `CREATE ROLE` per app - carried its own
/// inline default, `postgres://postgres:<password>@postgres:5432/zeroship`, in
/// its `environment:` block. Two consequences, and the second is the one a
/// gate cannot see:
///
///   1. the cluster superuser's credential sat in a tracked file that people
///      copy, on the one surface check 6c of
///      `tests/config_name_alignment_gate.sh` does not read (it parses argv);
///   2. `generate_migrate_dsn` writes that file only when it is ABSENT,
///      precisely so an operator can repoint it at a real database. Doing so
///      moved the platform one-shot and left `migrate-server` provisioning against
///      the in-compose Postgres, with nothing anywhere reporting the split.
///
/// This asserts the coupling directly, which the gate cannot: the gate rules on
/// the compose file's grammar, and would stay green if `secret_specs()` renamed
/// `migrate-dsn` out from under both mounts.
///
/// Does NOT cover: that the DSN authenticates, that the container can open the
/// path (that is `deploy/scripts/deploy-remote.sh`'s per-service probe), or the
/// five least-privilege role DSNs still inlined on other services.
#[test]
fn compose_takes_the_privileged_dsn_from_the_one_file_dev_init_writes() {
    let root = workspace_root();
    let compose_path = root.join("deploy/compose/docker-compose.yml");
    let compose = std::fs::read_to_string(&compose_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", compose_path.display()));

    // The name is taken from the set dev init is asserted to write, not spelled
    // again here, so a rename that misses compose fails at this line.
    let dsn_file = SECRET_FILES
        .iter()
        .find(|name| **name == "migrate-dsn")
        .expect("dev init must still write a privileged DSN file");
    let container_path = format!("/etc/zeroship/secrets/{dsn_file}");
    let mount = format!("${{ZEROSHIP_SECRETS_DIR:-./secrets}}/{dsn_file}:{container_path}:ro");

    for service in ["migrate", "migrate-server"] {
        let block = service_block(&compose, service);
        assert!(
            block.contains(&mount),
            "{service} must bind-mount {dsn_file}; deploy-remote.sh derives the host's \
             required secret files from these mount lines"
        );
        assert!(
            block.contains(&container_path),
            "{service} must name {container_path} as the source of its privileged DSN"
        );
    }

    let migrate_server = service_block(&compose, "migrate-server");
    assert!(
        migrate_server.contains(&format!(
            "ZEROSHIP_MIGRATE_SERVER_PROVISION_DATABASE_URL: \
             ${{ZEROSHIP_MIGRATE_SERVER_PROVISION_DATABASE_URL:-urn:zeroship:file:{container_path}}}"
        )),
        "migrate_server's provisioning DSN must default to a urn:zeroship:file: reference, \
         not to material; got:\n{migrate_server}"
    );

    // The superuser's own role name, read from the postgres service rather than
    // written here, so a rename cannot leave this asserting about nobody.
    let postgres = service_block(&compose, "postgres");
    assert!(
        postgres.contains("POSTGRES_PASSWORD:"),
        "the postgres service must still declare POSTGRES_PASSWORD"
    );
    let superuser = "postgres";
    for service in ["control", "gateway", "worker", "auth", "migrate-server", "migrate"] {
        let block = service_block(&compose, service);
        assert!(
            !block.contains(&format!("://{superuser}:")),
            "{service} inlines a {superuser} superuser DSN; a privileged credential \
             belongs in the mounted file, never in a tracked deploy file"
        );
    }
}

#[test]
fn compose_preserves_shared_secret_topology_and_has_no_weak_literals() {
    let root = workspace_root();
    let compose_path = root.join("deploy/compose/docker-compose.yml");
    let compose = std::fs::read_to_string(&compose_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", compose_path.display()));

    let control = service_block(&compose, "control");
    let migrate_server = service_block(&compose, "migrate-server");
    let gateway = service_block(&compose, "gateway");
    let worker = service_block(&compose, "worker");
    let auth = service_block(&compose, "auth");

    for (name, block) in [
        ("control", control),
        ("migrate-server", migrate_server),
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
        4,
        "gateway and auth must each declare and mount the broker-secret path"
    );
    let shared_mount = "${ZEROSHIP_SECRETS_DIR:-./secrets}:/etc/zeroship/secrets:ro";
    assert!(!compose.contains(shared_mount));

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
    ] {
        assert!(
            block.contains(required_control),
            "{name} is not wired to the generated control key"
        );
    }
    assert!(
        migrate_server.contains(required_control),
        "migrate-server is not wired to the same generated control key"
    );
    assert_eq!(
        compose
            .matches("${ZEROSHIP_CONTROL_KEY:?run zeroship dev init}")
            .count(),
        4,
        "only control, gateway, worker, and migrate-server consume the control key"
    );
    assert!(
        !auth.contains(required_control),
        "auth must not receive the control key"
    );

    assert!(!worker.contains("../ops/zeroship.toml:/etc/zeroship/zeroship.toml:ro"));

    // Stripe is OPTIONAL: only Stripe can issue a webhook secret that verifies,
    // so compose must render without one rather than force a Stripe-less
    // deployment to carry a locally generated placeholder. Empty is not a
    // relaxation - control rejects every delivery with 500 when the secret is
    // empty (crates/zeroship-control/tests/stripe_webhook_test.rs).
    assert!(
        control.contains("ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET: ${ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET:-}"),
        "ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET must be optional with an empty default"
    );
    assert!(
        !compose.contains("${ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET:?"),
        "no service may REQUIRE a Stripe webhook secret to render compose"
    );
    assert!(migrate_server.contains(
        "ZEROSHIP_MIGRATE_SERVER_POLICY_SEAL_KEY: ${ZEROSHIP_MIGRATE_SERVER_POLICY_SEAL_KEY:?run zeroship dev init}"
    ));
    assert!(migrate_server.contains(
        "AUTH_PLATFORM_ISSUER: ${ZEROSHIP_ORIGIN_SCHEME:-http}://auth.${ZEROSHIP_DOMAIN:-zeroship.localhost}/oauth2"
    ));

    // The platform OP's NAME and its ADDRESS are separate settings, and the
    // rendered compose must keep them separate. The issuer follows
    // ZEROSHIP_DOMAIN because it is what a token's `iss` carries; the JWKS
    // target is a LITERAL naming the service on this network, because a
    // domain-derived value sends control out through the public edge and back,
    // which on a real single-host deployment has no route.
    for (label, service, expected) in [
        (
            "control jwks",
            control,
            "ZEROSHIP_AUTH_PLATFORM_JWKS_URL: http://auth:9092/oauth2/.well-known/jwks.json",
        ),
        (
            "migrate-server jwks",
            migrate_server,
            "ZEROSHIP_AUTH_PLATFORM_JWKS_URL: http://auth:9092/oauth2/.well-known/jwks.json",
        ),
    ] {
        assert!(
            service.contains(expected),
            "{label}: rendered compose must set {expected}"
        );
    }

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
        !overlay.contains_key("ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET"),
        "dev init generated a Stripe webhook secret it cannot possibly issue: {:?}",
        overlay.keys().collect::<Vec<_>>()
    );
    let raw = std::fs::read_to_string(&env_file).expect("read generated env file");
    assert!(
        !raw.contains("ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET"),
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
    // DIRECTORY names, not service nicknames: every crate directory carries the
    // `zeroship-` prefix. `collect_rs_files` returns SILENTLY on a missing path, so
    // a nickname here collects zero files and this test rules on nothing. The
    // assertion below and the floor after it are what make that loud; keep both.
    for crate_name in [
        "zeroship-control",
        "zeroship-gateway",
        "zeroship-worker",
        "zeroship-auth",
        "zeroship-migrate-server",
        "zeroship-cli",
    ] {
        let dir = root.join("crates").join(crate_name).join("src");
        assert!(
            dir.is_dir(),
            "{} is not a directory -- the walk would collect nothing from it and \
             `collect_rs_files` returns silently on a missing path",
            dir.display()
        );
        collect_rs_files(&dir, &mut sources);
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
