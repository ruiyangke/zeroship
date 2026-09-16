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
    // by several processes is a shared secret wearing a signature.
    "svc-auth.pem",
    "svc-control.pem",
    "svc-gateway.pem",
    // The worker holds no key of its own at all: it joins with a TOKEN a
    // trusted signer minted and mints under an instance key it draws in memory
    // at boot. This is the SIGNER's key, and it never reaches a worker.
    "join-signer.json",
];

/// The one generated file that is NOT private, and must not become private.
///
/// It holds PUBLIC keys. Pinning it to 0600 alongside the private material
/// would read as "another secret", and the first operator who had to serve it
/// to a peer would loosen the whole directory instead of this one file. It is
/// named separately here so the distinction is asserted rather than assumed.
const PUBLIC_FILES: [&str; 2] = ["service-peers.json", "join-signers.json"];

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

    // The peer document publishes the three services that hold a key of their
    // own and NO worker key: no process holds a `svc/worker` role key.
    let peers: serde_json::Value = serde_json::from_slice(
        &std::fs::read(secrets_dir.join("service-peers.json")).expect("read the peer document"),
    )
    .expect("the peer document is JSON");
    let issuers = peers["keys"]
        .as_array()
        .expect("keys")
        .iter()
        .map(|key| key["iss"].as_str().expect("iss").to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        issuers,
        [
            "spiffe://zeroship.ai/svc/auth",
            "spiffe://zeroship.ai/svc/control",
            "spiffe://zeroship.ai/svc/gateway",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
    );

    // The signer credential is read by THE reader Control's minter boots with,
    // not by a parser of this test's own, and the id it yields is the one the
    // import file names.
    let (signer_id, signer_public) = join_signer(&secrets_dir);
    let (loaded_id, loaded_key) = zeroship_core::service_peers::load_join_signer_credential(
        &secrets_dir.join("join-signer.json"),
    )
    .expect("Control's minter loads the generated signer credential");
    assert_eq!(loaded_id, signer_id);
    assert_eq!(loaded_key.verifying_key_bytes(), signer_public);

    // Control's import file names exactly this deployment's signer, for the
    // default zone, parsed by the one parser Control imports it with.
    let records = zeroship_core::worker_join::parse_join_signer_import(
        &std::fs::read(secrets_dir.join("join-signers.json")).expect("read the import file"),
    )
    .expect("Control's parser accepts the generated import file");
    assert_eq!(
        records,
        vec![zeroship_core::worker_join::JoinSignerRecord {
            id: signer_id,
            zones: vec!["default".to_owned()],
            public_key: signer_public,
        }]
    );
}

/// The signer id and PUBLIC key in a generated `join-signer.json`.
fn join_signer(secrets_dir: &Path) -> (String, [u8; 32]) {
    let credential: serde_json::Value = serde_json::from_slice(
        &std::fs::read(secrets_dir.join("join-signer.json")).expect("read the credential"),
    )
    .expect("the credential is JSON");
    let key = SigningKey::from_pkcs8_pem(credential["private_key"].as_str().expect("private_key"))
        .expect("an Ed25519 PKCS#8 PEM key");
    (
        credential["signer_id"]
            .as_str()
            .expect("signer_id")
            .to_owned(),
        key.verifying_key().to_bytes(),
    )
}

/// An operator who trusts more signers keeps them: dev init adds THIS
/// deployment's signer to an existing import file and touches no other entry,
/// and a re-run leaves the file byte-for-byte alone.
#[test]
fn dev_init_adds_its_signer_to_an_operators_import_file_and_keeps_the_rest() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let secrets_dir = temp.path().join("secrets");
    let env_file = temp.path().join("dev.env");
    std::fs::create_dir(&secrets_dir).expect("create secrets directory");
    let other = zeroship_core::worker_join::JoinSignerRecord {
        id: zeroship_core::typed_id::new_join_signer_id(),
        zones: vec!["edge".to_owned()],
        public_key: SigningKey::from_bytes(&[11_u8; 32]).verifying_key().to_bytes(),
    };
    std::fs::write(
        secrets_dir.join("join-signers.json"),
        zeroship_core::worker_join::render_join_signer_import(std::slice::from_ref(&other)),
    )
    .expect("write the operator's import file");

    assert_success(&run_dev_init(&secrets_dir, &env_file), "zeroship dev init");
    let import = std::fs::read(secrets_dir.join("join-signers.json")).expect("read import");
    let (signer_id, signer_public) = join_signer(&secrets_dir);
    assert_eq!(
        zeroship_core::worker_join::parse_join_signer_import(&import).expect("parses"),
        vec![
            other,
            zeroship_core::worker_join::JoinSignerRecord {
                id: signer_id,
                zones: vec!["default".to_owned()],
                public_key: signer_public,
            },
        ]
    );

    assert_success(&run_dev_init(&secrets_dir, &env_file), "re-run");
    assert_eq!(
        std::fs::read(secrets_dir.join("join-signers.json")).expect("reread import"),
        import,
        "a re-run must not rewrite an import file that already names this signer"
    );
}

/// An import file naming this deployment's signer under a DIFFERENT key is
/// refused, and nothing is changed: Control would refuse it at its next boot,
/// and guessing which of the two files is right is the operator's call.
/// The control half is the untouched directory, which re-runs cleanly.
#[test]
fn dev_init_refuses_an_import_file_that_rekeys_its_signer() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let secrets_dir = temp.path().join("secrets");
    let env_file = temp.path().join("dev.env");
    assert_success(&run_dev_init(&secrets_dir, &env_file), "zeroship dev init");
    assert_success(
        &run_dev_init(&secrets_dir, &env_file),
        "the control: an untouched directory re-runs",
    );

    let (signer_id, _) = join_signer(&secrets_dir);
    std::fs::write(
        secrets_dir.join("join-signers.json"),
        zeroship_core::worker_join::render_join_signer_import(&[
            zeroship_core::worker_join::JoinSignerRecord {
                id: signer_id.clone(),
                zones: vec!["default".to_owned()],
                public_key: SigningKey::from_bytes(&[12_u8; 32]).verifying_key().to_bytes(),
            },
        ]),
    )
    .expect("re-key this deployment's entry");
    let import_before = std::fs::read(secrets_dir.join("join-signers.json")).expect("read");
    let before = snapshot(&secrets_dir, &env_file);

    let output = run_dev_init(&secrets_dir, &env_file);
    assert!(
        !output.status.success(),
        "a re-keyed signer entry was accepted\nstderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(&signer_id), "stderr={stderr}");
    assert_eq!(snapshot(&secrets_dir, &env_file), before);
    assert_eq!(
        std::fs::read(secrets_dir.join("join-signers.json")).expect("reread"),
        import_before
    );
}

/// An import file recording this deployment's signer for DIFFERENT ZONES is
/// refused too, and for a different reason than a changed key: a signer's zones
/// ARE its authority, so widening them is provisioning a new signer rather than
/// editing a line.
#[test]
fn dev_init_refuses_an_import_file_that_rezones_its_signer() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let secrets_dir = temp.path().join("secrets");
    let env_file = temp.path().join("dev.env");
    assert_success(&run_dev_init(&secrets_dir, &env_file), "zeroship dev init");

    let (signer_id, signer_public) = join_signer(&secrets_dir);
    std::fs::write(
        secrets_dir.join("join-signers.json"),
        zeroship_core::worker_join::render_join_signer_import(&[
            zeroship_core::worker_join::JoinSignerRecord {
                id: signer_id.clone(),
                zones: vec!["default".to_owned(), "edge".to_owned()],
                public_key: signer_public,
            },
        ]),
    )
    .expect("widen this deployment's zones");
    let before = snapshot(&secrets_dir, &env_file);

    let output = run_dev_init(&secrets_dir, &env_file);
    assert!(
        !output.status.success(),
        "a re-zoned signer entry was accepted\nstderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(&signer_id), "stderr={stderr}");
    assert_eq!(snapshot(&secrets_dir, &env_file), before);
}

/// The signer key is judged in the same no-shared-keys set as the service
/// keys: a signer credential holding a service's key would let whoever holds
/// that file present as that service, and the signer key is the one that
/// decides which processes become workers at all.
#[test]
fn dev_init_refuses_a_signer_key_equal_to_a_service_key() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let secrets_dir = temp.path().join("secrets");
    let env_file = temp.path().join("dev.env");
    assert_success(&run_dev_init(&secrets_dir, &env_file), "zeroship dev init");

    let (signer_id, _) = join_signer(&secrets_dir);
    let gateway_pem =
        std::fs::read_to_string(secrets_dir.join("svc-gateway.pem")).expect("read gateway key");
    std::fs::write(
        secrets_dir.join("join-signer.json"),
        serde_json::to_vec(&serde_json::json!({
            "signer_id": signer_id,
            "private_key": gateway_pem,
        }))
        .expect("json"),
    )
    .expect("put the gateway's key in the signer credential");
    let before = snapshot(&secrets_dir, &env_file);

    let output = run_dev_init(&secrets_dir, &env_file);
    assert!(
        !output.status.success(),
        "a signer key shared with svc-gateway was accepted"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("join-signer.json") && stderr.contains("svc-gateway.pem"),
        "stderr={stderr}"
    );
    assert_eq!(snapshot(&secrets_dir, &env_file), before);
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

/// ONE KEY AT TWO SERVICE PATHS MUST REFUSE, and this is a one-variable
/// control: the only difference between the two halves is whether one of the
/// service key files was overwritten with a copy of another.
///
/// WHAT WENT WRONG. `SERVICE_KEY_FILES`'s own rustdoc has always said one key
/// per service and not one shared file, "because a peer must be able to VERIFY
/// a service without being able to IMPERSONATE it" - and nothing enforced it.
/// `ensure_secret_file` keeps whatever exists, `validate_signing_key` asks
/// only whether it parses, and `write_service_peers` published each path under
/// its own issuer. So one key copied to every path - what a secret manager or a
/// compose override produces when it maps one secret onto every
/// `ZEROSHIP_*_SERVICE_KEY_FILE` mount - exited 0, printed "kept" for each and
/// emitted a peer document with ONE key under EVERY issuer.
///
/// WHY THAT DOCUMENT IS THE WHOLE ATTACK. The envelope and assertion wire
/// formats carry a key id derived from the public bytes and no issuer, and the
/// verifiers resolve material by issuer string alone. Under a one-key document
/// every issuer resolves to the same key, so the worker's own signer stamps
/// exactly the key id its own verifier looks up: it can mint the
/// `ZeroShip-User` envelope it then accepts, which fence F4 of
/// `docs/proposals/2026-09-05-auth-foundation-redesign.md` exists to forbid.
/// Wider still, possession of any one service key file becomes the ability to
/// present as any service to any service.
///
/// A WARNING WOULD NOT HAVE DONE. This run WRITES the credential document; a
/// message printed beside a document with that property is how the shape
/// arrived. So the assertions below are on the exit code and on the directory
/// being byte-for-byte untouched, not on the text alone.
///
/// Does NOT cover: whether any binary refuses to LOAD such a document. That is
/// the `forged` arms in `crates/zeroship-worker/tests/peer_boot.rs` and
/// `crates/zeroship-gateway/tests/peer_boot.rs`, against the real binaries.
#[test]
fn dev_init_refuses_when_two_service_key_paths_hold_the_same_key() {
    const SERVICE_KEYS: [&str; 3] = ["svc-auth.pem", "svc-control.pem", "svc-gateway.pem"];

    // The control half. Distinct keys, which is what a run generates.
    let temp = tempfile::tempdir().expect("create temp directory");
    let secrets_dir = temp.path().join("secrets");
    let env_file = temp.path().join("dev.env");
    assert_success(
        &run_dev_init(&secrets_dir, &env_file),
        "first zeroship dev init",
    );
    assert_success(
        &run_dev_init(&secrets_dir, &env_file),
        "re-run over DISTINCT service keys",
    );
    let public_keys = SERVICE_KEYS
        .iter()
        .map(|name| service_public_key(&secrets_dir.join(name)))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        public_keys.len(),
        SERVICE_KEYS.len(),
        "the control half must start from DISTINCT keys, or the refusal \
         below proves nothing"
    );

    // The case half, one variable changed: svc-auth's key copied over another
    // service's. Every other byte in the directory is the one the control just
    // accepted.
    for victim in ["svc-gateway.pem", "svc-control.pem"] {
        let temp = tempfile::tempdir().expect("create temp directory");
        let secrets_dir = temp.path().join("secrets");
        let env_file = temp.path().join("dev.env");
        assert_success(&run_dev_init(&secrets_dir, &env_file), "seed the directory");

        let source = secrets_dir.join("svc-auth.pem");
        let target = secrets_dir.join(victim);
        std::fs::copy(&source, &target).expect("copy one service key over another");
        let before = snapshot(&secrets_dir, &env_file);

        let output = run_dev_init(&secrets_dir, &env_file);
        assert!(
            !output.status.success(),
            "zeroship dev init ACCEPTED one key at both svc-auth.pem and \
             {victim}, so it can still emit a document that collapses two \
             issuers onto one key\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );

        // Both colliding paths by name: the operator's next action is deleting
        // one of two files, and this message is the only thing that says which.
        let stderr = String::from_utf8_lossy(&output.stderr);
        for named in [&source, &target] {
            assert!(
                stderr.contains(&named.display().to_string()),
                "the refusal does not name the colliding path {}\nstderr={stderr}",
                named.display()
            );
        }

        // Refused BEFORE anything was written, not after. A refusal that had
        // already republished the peer document would leave the collapsed
        // credential on disk for whatever reads it next.
        assert_eq!(
            snapshot(&secrets_dir, &env_file),
            before,
            "the refusal changed the secrets directory"
        );
    }
}

/// The public half of a PKCS#8 Ed25519 private key file, as the peer document
/// spells it. Comparing PUBLIC keys and not file bytes is deliberate: the same
/// credential written once as PEM and once as DER is two files and one
/// identity.
fn service_public_key(path: &Path) -> String {
    let bytes =
        std::fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let text = std::str::from_utf8(&bytes).expect("service key file is PEM text");
    let signing = SigningKey::from_pkcs8_pem(text).expect("Ed25519 PKCS#8 PEM");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signing.verifying_key().to_bytes())
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

fn run_join_token(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zeroship"))
        .arg("join-token")
        .args(args)
        .output()
        .expect("run zeroship join-token")
}

/// `zeroship join-token` mints a token the TRUSTED SIGNER dev init recorded can
/// verify, for the zone it was asked for, with the uses it was asked for.
///
/// The verification is `zeroship_core::worker_join::verify_join_token`, the one
/// Control admits a join with, against the public key in the import file rather
/// than against the credential - so this rules on the pair an operator actually
/// deploys, not on one half of it.
#[test]
fn join_token_mints_a_token_the_recorded_signer_verifies() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let secrets_dir = temp.path().join("secrets");
    let env_file = temp.path().join("dev.env");
    assert_success(&run_dev_init(&secrets_dir, &env_file), "zeroship dev init");
    let recorded = zeroship_core::worker_join::parse_join_signer_import(
        &std::fs::read(secrets_dir.join("join-signers.json")).expect("read the import file"),
    )
    .expect("parses");
    let signer = recorded.first().expect("dev init recorded a signer");

    let credential = secrets_dir.join("join-signer.json").display().to_string();
    let output = run_join_token(&[
        &format!("--credential={credential}"),
        "--zone=default",
        "--ttl=300",
        "--uses=7",
    ]);
    assert_success(&output, "zeroship join-token");
    let token = String::from_utf8(output.stdout).expect("a UTF-8 token");
    let token = token.trim();

    let audience = zeroship_core::service_peers::service_issuer(
        zeroship_core::service_peers::CONTROL_SERVICE_NAME,
    )
    .expect("control issuer");
    let verified = zeroship_core::worker_join::verify_join_token(
        token,
        &signer.public_key,
        &audience,
        std::time::SystemTime::now(),
    )
    .expect("the recorded signer verifies the minted token");
    assert_eq!(verified.signer_id, signer.id);
    assert_eq!(verified.zone, "default");
    assert_eq!(verified.uses, 7);

    // THE ONE-VARIABLE CONTROL: another key does not verify it, so the
    // acceptance above is the recorded key rather than a verifier that accepts
    // anything shaped like a token.
    let other = SigningKey::from_bytes(&[19_u8; 32]).verifying_key().to_bytes();
    assert!(
        zeroship_core::worker_join::verify_join_token(
            token,
            &other,
            &audience,
            std::time::SystemTime::now(),
        )
        .is_err(),
        "a token must not verify under a key that did not mint it"
    );
}

/// A zone name Control would refuse is refused at the mint, and nothing is
/// printed: a minter that can produce a token nothing accepts is a fault an
/// operator finds at the worker instead of at the mint.
#[test]
fn join_token_refuses_a_zone_control_would_refuse() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let secrets_dir = temp.path().join("secrets");
    let env_file = temp.path().join("dev.env");
    assert_success(&run_dev_init(&secrets_dir, &env_file), "zeroship dev init");
    let credential = secrets_dir.join("join-signer.json").display().to_string();

    let output = run_join_token(&[&format!("--credential={credential}"), "--zone= padded"]);
    assert!(!output.status.success(), "a padded zone name was accepted");
    assert!(output.stdout.is_empty(), "a refused mint printed a token");

    // The control: the same command with a clean zone name succeeds - spelled
    // with separate values, the other form `dev init` also takes.
    let output = run_join_token(&["--credential", &credential, "--zone", "edge"]);
    assert_success(&output, "zeroship join-token with a clean zone");
    assert!(!output.stdout.is_empty());
}

/// `--confirm` binds the token to ONE joining key, and Control's verifier reads
/// the thumbprint back out.
///
/// A token minted for a key the issuer already knows admits that key and no
/// other, which removes even the "workers the captor controls" an unbound
/// captured token buys.
#[test]
fn join_token_binds_the_token_to_a_confirmed_key() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let secrets_dir = temp.path().join("secrets");
    let env_file = temp.path().join("dev.env");
    assert_success(&run_dev_init(&secrets_dir, &env_file), "zeroship dev init");
    let recorded = zeroship_core::worker_join::parse_join_signer_import(
        &std::fs::read(secrets_dir.join("join-signers.json")).expect("read the import file"),
    )
    .expect("parses");
    let signer = recorded.first().expect("dev init recorded a signer");
    let credential = secrets_dir.join("join-signer.json").display().to_string();

    let joining = SigningKey::from_bytes(&[23_u8; 32]).verifying_key().to_bytes();
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(joining);
    let output = run_join_token(&[
        &format!("--credential={credential}"),
        &format!("--confirm={encoded}"),
    ]);
    assert_success(&output, "zeroship join-token --confirm");
    let token = String::from_utf8(output.stdout).expect("a UTF-8 token");
    let verified = zeroship_core::worker_join::verify_join_token(
        token.trim(),
        &signer.public_key,
        &zeroship_core::service_peers::service_issuer(
            zeroship_core::service_peers::CONTROL_SERVICE_NAME,
        )
        .expect("control issuer"),
        std::time::SystemTime::now(),
    )
    .expect("the confirmed token verifies");
    assert_eq!(
        verified.confirmation.as_deref(),
        Some(zeroship_core::service_assertion::thumbprint_key_id(&joining).as_str()),
        "the confirmation must name the key the operator bound it to"
    );

    // THE CONTROL, one variable apart: without `--confirm` the token carries no
    // confirmation at all, so the assertion above is the flag rather than a
    // claim every token happens to have.
    let output = run_join_token(&[&format!("--credential={credential}")]);
    assert_success(&output, "zeroship join-token");
    let token = String::from_utf8(output.stdout).expect("a UTF-8 token");
    let verified = zeroship_core::worker_join::verify_join_token(
        token.trim(),
        &signer.public_key,
        &zeroship_core::service_peers::service_issuer(
            zeroship_core::service_peers::CONTROL_SERVICE_NAME,
        )
        .expect("control issuer"),
        std::time::SystemTime::now(),
    )
    .expect("the unbound token verifies");
    assert_eq!(verified.confirmation, None);

    // A confirmation that is not a key is refused at the mint rather than
    // written into a token nothing can satisfy.
    for bad in ["not base64!!", "c2hvcnQ"] {
        assert!(
            !run_join_token(&[
                &format!("--credential={credential}"),
                &format!("--confirm={bad}"),
            ])
            .status
            .success(),
            "{bad:?} must not become a confirmation"
        );
    }
}

/// A missing credential refuses rather than minting under a key it drew.
#[test]
fn join_token_refuses_without_a_credential() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let absent = temp.path().join("join-signer.json").display().to_string();
    assert!(!run_join_token(&[]).status.success());
    assert!(!run_join_token(&[&format!("--credential={absent}")])
        .status
        .success());
}
