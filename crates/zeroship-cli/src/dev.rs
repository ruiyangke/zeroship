//! Local platform provisioning commands.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use ed25519_dalek::pkcs8::{DecodePrivateKey as _, EncodePrivateKey as _};
use ed25519_dalek::SigningKey;
use rand::RngCore as _;

const DEFAULT_SECRETS_DIR: &str = "deploy/compose/secrets";
const DEFAULT_ENV_FILE: &str = "deploy/compose/.env";
const COMPOSE_FILE: &str = "deploy/compose/docker-compose.yml";

/// Secrets this deployment issues to ITSELF, and therefore can generate.
///
/// `ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET` is deliberately absent: the value is issued by
/// Stripe (`whsec_...` from the dashboard endpoint or `stripe listen
/// --print-secret`), so a locally generated one can never verify a real
/// Stripe-Signature. Generating it produced an inert placeholder that made an
/// unconfigured deployment look configured. Compose now defaults it to empty
/// and control fails every webhook closed with 500 while it stays empty.
/// EVERY name here is the canonical `ZEROSHIP_` projection of a declared
/// setting, and compose maps each one to itself. The bare middle spellings
/// this list used to write - the unprefixed master-key, stash and salt names -
/// are read by nothing since Step 5 of
/// `docs/proposals/2026-08-11-config-name-alignment.md` deleted them, so
/// writing one would leave a dev stack that looks initialised and crash-loops.
///
/// `GATEWAY_OIDC_SECRET` is gone rather than renamed: it had no Rust reader at
/// all. Generating a secret nothing consumes is the set-but-unread shape this
/// migration exists to remove, and it is not made better by a canonical name.
///
/// THE LIST IS NOT KEPT HERE. It is `zeroship_core::config::PLATFORM_SECRETS`,
/// which also carries the strength rule the product enforces on each name and
/// the validator that applies it. Keeping a second copy here is what let the
/// generator and the enforcement disagree; the services' boot-time credential
/// audit reads the same table, so a secret this command generates and a secret
/// a service is handed are judged by one rule set. What compose SHIPS is
/// judged by nothing: the gate that read the table against
/// `deploy/compose/docker-compose.yml` was deleted on 2026-08-21.
fn env_keys() -> impl Iterator<Item = &'static str> {
    zeroship_core::config::PLATFORM_SECRETS
        .iter()
        .map(|secret| secret.env)
}

const WEAK_LITERALS: &[&str] = &[
    "platform-key",
    "master-key",
    "dev-worker-key-not-for-production-use",
    "dev-secret-rotate-me-too",
    "dev-stash-signing-key-not-for-production",
    "dev-pairwise-salt-never-rotate-in-prod",
];

pub(crate) fn cmd_dev(args: &[String]) -> Result<(), String> {
    match args.get(2).map(String::as_str) {
        Some("init") => {}
        Some("enroller") => return cmd_dev_enroller(args),
        _ => return Err(format!("{}\n{}", dev_init_usage(), dev_enroller_usage())),
    }

    let options = parse_init_options(args)?;
    if options.uses_defaults && !Path::new(COMPOSE_FILE).is_file() {
        return Err(format!(
            "{} not found; run this command from the zeroship repository root, or pass both --secrets-dir and --env-file",
            COMPOSE_FILE
        ));
    }

    let outcome = init_dev_secrets(&options.secrets_dir, &options.env_file)?;
    eprintln!("zeroship dev init: {}", options.secrets_dir.display());
    for path in &outcome.created_files {
        eprintln!("  created {}", path.display());
    }
    for path in &outcome.existing_files {
        eprintln!("  kept    {}", path.display());
    }
    eprintln!(
        "  env     {} ({} added, {} kept)",
        options.env_file.display(),
        outcome.added_env,
        outcome.existing_env
    );
    eprintln!(
        "zeroship dev init: {} created, {} kept; existing secrets were not rotated",
        outcome.created_files.len(),
        outcome.existing_files.len()
    );
    Ok(())
}

fn dev_init_usage() -> &'static str {
    "Usage: zeroship dev init [--secrets-dir=PATH] [--env-file=PATH]"
}

fn dev_enroller_usage() -> &'static str {
    "Usage: zeroship dev enroller --credential=PATH --import-file=PATH [--zone=NAME]"
}

/// Provision ONE more worker deployment unit: mint its enroller, write the
/// credential its workers mount, and add its public half to Control's import
/// file.
///
/// `zeroship dev init` provisions the enroller of the host it runs on; this is
/// how an operator adds a unit - another host or pool - to the same
/// deployment. The credential is created, never replaced: a unit's key is
/// never rotated in place, because Control refuses a changed key for a
/// recorded id. The import file is extended, never rewritten: every entry
/// already in it is kept exactly as it was.
fn cmd_dev_enroller(args: &[String]) -> Result<(), String> {
    let mut credential = None;
    let mut import_file = None;
    let mut zone = None;
    // Both spellings `dev init` takes: `--name=value` and `--name value`.
    let mut index = 3;
    while index < args.len() {
        let arg = &args[index];
        let (name, inline_value) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(name, value)| (name, Some(value)));
        let slot = match name {
            "--credential" => &mut credential,
            "--import-file" => &mut import_file,
            "--zone" => &mut zone,
            "--help" => return Err(dev_enroller_usage().to_string()),
            _ => return Err(format!("unknown argument {arg:?}. {}", dev_enroller_usage())),
        };
        if slot.is_some() {
            return Err(format!("{name} was supplied more than once"));
        }
        let value = if let Some(value) = inline_value {
            value.to_owned()
        } else {
            index += 1;
            args.get(index)
                .filter(|value| !value.starts_with("--"))
                .cloned()
                .ok_or_else(|| format!("{name} requires a value"))?
        };
        if value.is_empty() {
            return Err(format!("{name} requires a non-empty value"));
        }
        *slot = Some(value);
        index += 1;
    }
    let (Some(credential), Some(import_file)) = (credential, import_file) else {
        return Err(dev_enroller_usage().to_string());
    };
    let zone = zone.unwrap_or_else(|| {
        zeroship_core::worker_enrollers::DEFAULT_EXECUTION_ZONE.to_owned()
    });
    let enroller_id = provision_enroller(Path::new(&credential), Path::new(&import_file), &zone)?;
    eprintln!("zeroship dev enroller: provisioned enroller {enroller_id} in zone {zone:?}");
    eprintln!("  created {credential} (mount it into the unit's workers as ZEROSHIP_WORKER_ENROLLER_FILE)");
    eprintln!("  added   {enroller_id} to {import_file} (restart Control to import it)");
    Ok(())
}

/// Mint one enroller into `credential` and `import_file`, returning its id.
///
/// The credential is created FIRST and removed again if the import file
/// cannot be written, so a failed run leaves neither a key Control will never
/// learn of nor an import entry whose private half nobody holds.
fn provision_enroller(credential: &Path, import_file: &Path, zone: &str) -> Result<String, String> {
    reject_symlink(import_file, "worker enroller import file")?;
    if std::fs::symlink_metadata(credential).is_ok() {
        return Err(format!(
            "{} already exists and was not replaced; a unit's enroller key is never rotated \
             in place - provision a new enroller at a new path instead",
            credential.display()
        ));
    }
    let mut records = if import_file.exists() {
        read_worker_enroller_import(import_file)?
    } else {
        Vec::new()
    };
    let bytes = generate_worker_enroller()?;
    let (enroller_id, public_key) = parse_worker_enroller(&bytes)?;
    if records
        .iter()
        .any(|record| record.id == enroller_id || record.public_key == public_key)
    {
        return Err("a freshly minted enroller collided with a recorded one; re-run".to_owned());
    }
    records.push(zeroship_core::worker_enrollers::EnrollerRecord {
        id: enroller_id.clone(),
        zone: zone.to_owned(),
        public_key,
    });
    // Validate the document we are about to write with the reader's parser, so
    // a zone name Control would refuse is refused here, before any file exists.
    let document = zeroship_core::worker_enrollers::render_enroller_import(&records);
    zeroship_core::worker_enrollers::parse_enroller_import(document.as_bytes())
        .map_err(|error| format!("the extended import file would be refused: {error}"))?;
    create_private_file(credential, &bytes)?;
    if let Err(error) = std::fs::write(import_file, document) {
        let _ = std::fs::remove_file(credential);
        return Err(format!(
            "write {}: {error}; the new credential was removed again",
            import_file.display()
        ));
    }
    Ok(enroller_id)
}

#[derive(Debug)]
struct InitOptions {
    secrets_dir: PathBuf,
    env_file: PathBuf,
    uses_defaults: bool,
}

fn parse_init_options(args: &[String]) -> Result<InitOptions, String> {
    let mut secrets_dir = None;
    let mut env_file = None;
    let mut index = 3;
    while index < args.len() {
        let arg = &args[index];
        let (name, inline_value) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(name, value)| (name, Some(value)));
        let slot = match name {
            "--secrets-dir" => &mut secrets_dir,
            "--env-file" => &mut env_file,
            "--help" => return Err(dev_init_usage().to_string()),
            _ => {
                return Err(format!("unknown argument {arg:?}. {}", dev_init_usage()));
            }
        };
        if slot.is_some() {
            return Err(format!("{name} was supplied more than once"));
        }
        let value = if let Some(value) = inline_value {
            value.to_string()
        } else {
            index += 1;
            args.get(index)
                .filter(|value| !value.starts_with("--"))
                .cloned()
                .ok_or_else(|| format!("{name} requires a path"))?
        };
        if value.is_empty() {
            return Err(format!("{name} requires a non-empty path"));
        }
        *slot = Some(PathBuf::from(value));
        index += 1;
    }

    let uses_defaults = secrets_dir.is_none() || env_file.is_none();
    Ok(InitOptions {
        secrets_dir: secrets_dir.unwrap_or_else(|| PathBuf::from(DEFAULT_SECRETS_DIR)),
        env_file: env_file.unwrap_or_else(|| PathBuf::from(DEFAULT_ENV_FILE)),
        uses_defaults,
    })
}

#[derive(Debug, Default)]
struct InitOutcome {
    created_files: Vec<PathBuf>,
    existing_files: Vec<PathBuf>,
    added_env: usize,
    existing_env: usize,
}

fn init_dev_secrets(secrets_dir: &Path, env_file: &Path) -> Result<InitOutcome, String> {
    ensure_private_directory(secrets_dir)?;
    ensure_env_parent(env_file)?;
    reject_symlink(env_file, "environment overlay")?;
    reject_env_file_alias(secrets_dir, env_file)?;

    let original_env = read_optional_file(env_file)?;
    let existing_env = parse_managed_env(&original_env)?;
    for (name, value) in &existing_env {
        validate_env_value(name, value)?;
    }

    let pairwise_path = secrets_dir.join("pairwise-salt");
    reject_symlink(&pairwise_path, "pairwise-salt")?;
    let pairwise_exists = pairwise_path.exists();
    let existing_pairwise = read_optional_file(&pairwise_path)?;
    if pairwise_exists && existing_pairwise.is_empty() {
        return Err(format!(
            "existing pairwise-salt {} is empty and was not replaced",
            pairwise_path.display()
        ));
    }
    let pairwise = resolve_pairwise(existing_env.get("ZEROSHIP_PAIRWISE_SALT"), &existing_pairwise)?;

    // Validate every existing file before creating anything. A broken partial
    // directory is an operator decision, not permission to add more state
    // around it or to rotate the invalid value.
    let secret_specs = secret_specs();
    for (name, _, validate) in &secret_specs {
        validate_existing_secret(&secrets_dir.join(name), name, *validate)?;
    }

    // A service key file that parses is not thereby a service key: several
    // paths holding ONE key is the shape that collapses several identities
    // onto one. It is a property OF THE SET, so no per-file validator above can
    // see it. Refused here, in the same before-anything-is-created phase,
    // because the run that would proceed publishes the collapsed document.
    reject_shared_service_keys(&existing_service_public_keys(secrets_dir)?)?;
    // The enroller import file is judged against the enroller credential
    // before anything is created, for the same reason: a run that would refuse
    // it afterwards has already written everything else.
    check_worker_enrollers(secrets_dir)?;

    let mut desired_env = existing_env.clone();
    for name in env_keys() {
        if !desired_env.contains_key(name) {
            desired_env.insert(name.to_string(), random_hex(32)?);
        }
    }
    desired_env.insert("ZEROSHIP_PAIRWISE_SALT".to_string(), pairwise.clone());

    let mut outcome = InitOutcome::default();
    for (name, generate, validate) in secret_specs {
        ensure_secret_file(
            &secrets_dir.join(name),
            name,
            generate,
            validate,
            &mut outcome,
        )?;
    }
    ensure_pairwise_file(&pairwise_path, pairwise.as_bytes(), &mut outcome)?;
    write_service_peers(secrets_dir)?;
    write_worker_enrollers(secrets_dir)?;

    let missing_env = env_keys()
        .filter(|name| !existing_env.contains_key(*name))
        .map(|name| (name.to_string(), desired_env[name].clone()))
        .collect::<Vec<_>>();
    append_env_values(env_file, &original_env, &missing_env)?;
    outcome.added_env = missing_env.len();
    outcome.existing_env = env_keys().count() - missing_env.len();
    Ok(outcome)
}

type SecretSpec = (
    &'static str,
    fn() -> Result<Vec<u8>, String>,
    fn(&[u8]) -> Result<(), String>,
);

/// The compose deployment's privileged DSN, read by BOTH services that need
/// one: the `migrate` one-shot (`--database-url-file`) and `migrated`'s
/// provisioning setting (`ZEROSHIP_MIGRATE_SERVER_PROVISION_DATABASE_URL`, spelled
/// as a `urn:zeroship:file:` reference at the same container path).
///
/// COUPLED TO deploy/compose/docker-compose.yml: `postgres` there is reachable
/// on the compose network as host `postgres`, and its `POSTGRES_PASSWORD` is
/// the literal `zeroship`. Change either and this must change with it - which
/// is the point of writing it down in one place instead of leaving it inline in
/// the deploy file, where it also sat in the one-shot's ARGV until 2026-08-16
/// and in `migrated`'s `environment:` block until 2026-08-21.
///
/// Written only when absent, so an operator who repoints this file at a real
/// database keeps their value across re-runs - and, since 2026-08-21, moves
/// BOTH readers together. While `migrated` carried its own inline default the
/// repointing silently moved only the platform one-shot.
const COMPOSE_MIGRATE_DSN: &str = "postgres://postgres:zeroship@postgres:5432/zeroship\n";

fn generate_migrate_dsn() -> Result<Vec<u8>, String> {
    Ok(COMPOSE_MIGRATE_DSN.as_bytes().to_vec())
}

/// A DSN is credential material by grammar (it admits userinfo), so it gets the
/// same private-file treatment as generated key material. Validation is
/// structural only: this file is meant to be re-pointed at a real database, so
/// the contents are not pinned to the default above.
fn validate_migrate_dsn(bytes: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(bytes).map_err(|error| format!("expected UTF-8: {error}"))?;
    let text = text.trim();
    if text.is_empty() {
        return Err("is empty".to_string());
    }
    if !text.starts_with("postgres://") && !text.starts_with("postgresql://") {
        return Err("must be a postgres:// or postgresql:// DSN".to_string());
    }
    Ok(())
}

/// The services that hold a per-service assertion key, and the file each one
/// reads.
///
/// Separate keys and not one shared file, because the whole point of the
/// mechanism is that a peer can VERIFY a service without being able to
/// IMPERSONATE it. One key held by several processes would put every service's
/// identity in every service's memory and turn the assertion back into a shared
/// secret with more ceremony.
///
/// THE WORKER IS NOT HERE, and that is the enrolment design rather than an
/// omission. No worker holds a `svc/worker` role key: a worker enrols with its
/// deployment unit's ENROLLER credential ([`WORKER_ENROLLER_FILE`]) and mints
/// under an instance key it draws at boot, so the peer document publishes no
/// worker key at all.
///
/// THAT PARAGRAPH WAS A COMMENT AND NOTHING ELSE UNTIL 2026-09-07, and the
/// difference was reachable with this very command. `ensure_secret_file` keeps
/// whatever file it finds and `validate_signing_key` only asks whether it
/// parses, so one key copied to every path - the shape a secret manager or a
/// compose override produces when it maps one secret onto every
/// `ZEROSHIP_*_SERVICE_KEY_FILE` mount - exited 0, printed "kept" for each and
/// published that key under every issuer. `reject_shared_service_keys` now
/// refuses it, before anything is created and again before the document is
/// written, and it judges the worker enroller key in the same set.
///
/// Why refuse rather than warn: the run EMITS A CREDENTIAL DOCUMENT. Under a
/// one-key document every issuer resolves to the same key, so a peer holding
/// any one service key can present as any service to any service, and a worker
/// can mint the identity envelope its own verifier accepts. A warning printed
/// beside a document with that property is how this arrived.
///
/// The names match `zeroship_core::service_peers`, and
/// `tests/lib/runtime_secrets.sh` writes the same set under the same names for
/// the end-to-end harnesses.
const SERVICE_KEY_FILES: [(&str, &str); 3] = [
    ("svc-gateway.pem", "svc/gateway"),
    ("svc-control.pem", "svc/control"),
    ("svc-auth.pem", "svc/auth"),
];

/// This host's worker ENROLLER credential: the `wen_` id and the Ed25519
/// PKCS#8 PEM private key of the one deployment unit a single-host deployment
/// is, in the document `ServiceKeyring::load_worker_enroller` reads. Every
/// worker replica on the host mounts it - `--scale` replicas are one unit -
/// and spends it on its boot-time enrolment and nothing else.
///
/// Private, like every key file here. Generated once and never rotated:
/// re-keying a unit is provisioning a NEW enroller, because Control refuses a
/// changed key for a recorded id (`docs/runbooks/worker-enrollers.md`).
const WORKER_ENROLLER_FILE: &str = "worker-enroller.json";

/// The enroller import document Control reads at startup
/// (`control.worker_enrollers_file`), naming the PUBLIC half of this host's
/// enroller in the deployment's default execution zone.
///
/// DERIVED from [`WORKER_ENROLLER_FILE`] and ADDITIVE: an entry an operator
/// added for another deployment unit is kept, and this host's entry is added
/// when it is missing. An entry that names this host's enroller id under a
/// different key is refused rather than rewritten - the file and the
/// credential disagreeing is an operator decision, not something to guess at.
/// Not private: it holds public keys, exactly like [`SERVICE_PEERS_FILE`].
const WORKER_ENROLLERS_FILE: &str = "worker-enrollers.json";

/// The JWKS-shaped document naming every service's PUBLIC key.
///
/// DERIVED from the private keys above rather than generated, so it cannot
/// drift from them: rotating a key and forgetting to republish it would leave
/// every peer refusing that service, and the failure would look like a network
/// problem. Written on every run for that reason - it is not a secret and it
/// carries no state an operator would want preserved.
const SERVICE_PEERS_FILE: &str = "service-peers.json";

fn secret_specs() -> [SecretSpec; 10] {
    [
        ("migrate-dsn", generate_migrate_dsn, validate_migrate_dsn),
        (
            SERVICE_KEY_FILES[0].0,
            generate_signing_key,
            validate_signing_key,
        ),
        (
            SERVICE_KEY_FILES[1].0,
            generate_signing_key,
            validate_signing_key,
        ),
        (
            SERVICE_KEY_FILES[2].0,
            generate_signing_key,
            validate_signing_key,
        ),
        (
            WORKER_ENROLLER_FILE,
            generate_worker_enroller,
            validate_worker_enroller,
        ),
        (
            "auth-signing.pem",
            generate_signing_key,
            validate_signing_key,
        ),
        (
            "gateway-signing.pem",
            generate_signing_key,
            validate_signing_key,
        ),
        (
            "broker-secret",
            generate_base64_secret,
            validate_broker_secret,
        ),
        (
            "refresh-hash-key",
            generate_refresh_hash_key,
            validate_refresh_hash_key,
        ),
        (
            "refresh-idem-key",
            generate_base64_secret,
            validate_base64_secret,
        ),
    ]
}

fn reject_env_file_alias(secrets_dir: &Path, env_file: &Path) -> Result<(), String> {
    let secrets_dir = std::fs::canonicalize(secrets_dir).map_err(|error| {
        format!(
            "resolve secrets directory {}: {error}",
            secrets_dir.display()
        )
    })?;
    let env_parent = env_file
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let env_parent = std::fs::canonicalize(env_parent).map_err(|error| {
        format!(
            "resolve environment overlay directory {}: {error}",
            env_parent.display()
        )
    })?;
    let env_name = env_file.file_name().ok_or_else(|| {
        format!(
            "environment overlay {} does not name a file",
            env_file.display()
        )
    })?;
    let normalized_env = env_parent.join(env_name);

    if normalized_env.starts_with(&secrets_dir) {
        return Err(format!(
            "environment overlay {} must be outside the generated secrets directory {}",
            env_file.display(),
            secrets_dir.display()
        ));
    }
    for name in secret_specs()
        .map(|(name, _, _)| name)
        .into_iter()
        .chain(std::iter::once("pairwise-salt"))
    {
        if same_existing_file(&normalized_env, &secrets_dir.join(name))? {
            return Err(format!(
                "environment overlay {} aliases generated secret file {name}",
                env_file.display()
            ));
        }
    }
    Ok(())
}

fn same_existing_file(left: &Path, right: &Path) -> Result<bool, String> {
    let left_metadata = match std::fs::metadata(left) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("inspect {}: {error}", left.display())),
    };
    let right_metadata = match std::fs::metadata(right) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("inspect {}: {error}", right.display())),
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        Ok(left_metadata.dev() == right_metadata.dev()
            && left_metadata.ino() == right_metadata.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = (left_metadata, right_metadata);
        Ok(false)
    }
}

fn validate_existing_secret(
    path: &Path,
    label: &str,
    validate: fn(&[u8]) -> Result<(), String>,
) -> Result<(), String> {
    reject_symlink(path, label)?;
    if path.exists() {
        let bytes = std::fs::read(path)
            .map_err(|error| format!("read existing {} {}: {error}", label, path.display()))?;
        validate(&bytes).map_err(|error| {
            format!(
                "existing {} {} is invalid and was not replaced: {error}",
                label,
                path.display()
            )
        })?;
    }
    Ok(())
}

fn ensure_secret_file(
    path: &Path,
    label: &str,
    generate: fn() -> Result<Vec<u8>, String>,
    validate: fn(&[u8]) -> Result<(), String>,
    outcome: &mut InitOutcome,
) -> Result<(), String> {
    if path.exists() {
        set_private_file_permissions(path)?;
        outcome.existing_files.push(path.to_path_buf());
        return Ok(());
    }

    let bytes = generate()?;
    validate(&bytes).map_err(|error| format!("generated {label} failed validation: {error}"))?;
    create_private_file(path, &bytes)?;
    outcome.created_files.push(path.to_path_buf());
    Ok(())
}

fn ensure_pairwise_file(
    path: &Path,
    desired: &[u8],
    outcome: &mut InitOutcome,
) -> Result<(), String> {
    if path.exists() {
        set_private_file_permissions(path)?;
        let actual = std::fs::read(path)
            .map_err(|error| format!("read existing pairwise-salt {}: {error}", path.display()))?;
        if actual != desired {
            return Err(format!(
                "existing pairwise-salt {} does not equal ZEROSHIP_PAIRWISE_SALT in the environment overlay; neither value was changed",
                path.display()
            ));
        }
        outcome.existing_files.push(path.to_path_buf());
    } else {
        create_private_file(path, desired)?;
        outcome.created_files.push(path.to_path_buf());
    }
    Ok(())
}

fn resolve_pairwise(env_value: Option<&String>, file_bytes: &[u8]) -> Result<String, String> {
    let file_value = if file_bytes.is_empty() {
        None
    } else {
        let value = std::str::from_utf8(file_bytes)
            .map_err(|_| "existing pairwise-salt is not UTF-8 and was not replaced".to_string())?;
        Some(value.to_string())
    };

    let value = match (env_value, file_value) {
        (Some(env), Some(file)) if env.as_bytes() != file.as_bytes() => {
            return Err(
                "ZEROSHIP_PAIRWISE_SALT in the environment overlay differs byte-for-byte from pairwise-salt; neither value was changed"
                    .to_string(),
            );
        }
        (Some(env), _) => env.clone(),
        (None, Some(file)) => file,
        (None, None) => random_hex(32)?,
    };
    validate_env_value("ZEROSHIP_PAIRWISE_SALT", &value)?;
    if value.contains(['\r', '\n']) {
        return Err("ZEROSHIP_PAIRWISE_SALT must not contain a line ending".to_string());
    }
    Ok(value)
}

fn parse_managed_env(contents: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let text = std::str::from_utf8(contents)
        .map_err(|error| format!("environment overlay is not UTF-8: {error}"))?;
    let managed = env_keys().collect::<BTreeSet<_>>();
    let mut values = BTreeMap::new();
    for (line_index, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let assignment = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        let Some((name, raw_value)) = assignment.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if !managed.contains(name) {
            continue;
        }
        if values.contains_key(name) {
            return Err(format!(
                "environment overlay line {} assigns {} more than once",
                line_index + 1,
                name
            ));
        }
        let value = parse_env_value(raw_value.trim()).map_err(|error| {
            format!(
                "environment overlay line {} has invalid {}: {error}",
                line_index + 1,
                name
            )
        })?;
        values.insert(name.to_string(), value);
    }
    Ok(values)
}

fn parse_env_value(raw: &str) -> Result<String, String> {
    if let Some(value) = raw.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
        return Ok(value.to_string());
    }
    if let Some(value) = raw.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        if value.contains(['\\', '"']) {
            return Err("quoted escapes are not supported for generated secret values".to_string());
        }
        return Ok(value.to_string());
    }
    if raw.contains(char::is_whitespace) || raw.contains('#') {
        return Err("use an unquoted value without whitespace or comments".to_string());
    }
    Ok(raw.to_string())
}

fn validate_env_value(name: &str, value: &str) -> Result<(), String> {
    if WEAK_LITERALS.contains(&value) {
        return Err(format!("{name} still contains a shipped weak literal"));
    }
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!(
            "{name} must be exactly 64 lowercase hex characters (32 bytes)"
        ));
    }
    let decoded = hex::decode(value).map_err(|error| format!("decode {name}: {error}"))?;
    if decoded.iter().all(|byte| *byte == 0) {
        return Err(format!("{name} must not be all zero"));
    }
    // The name -> validator mapping used to be a `match` here, a second copy of
    // a fact `crates/core` already owns. It is now one lookup into
    // PLATFORM_SECRETS, so adding a secret to the table is what makes this
    // command generate AND re-validate it.
    zeroship_core::config::platform_secret(name)
        .ok_or_else(|| format!("unknown generated environment key {name}"))?
        .validate(value)
}

fn append_env_values(
    path: &Path,
    original: &[u8],
    missing: &[(String, String)],
) -> Result<(), String> {
    if missing.is_empty() {
        if path.exists() {
            set_private_file_permissions(path)?;
        }
        return Ok(());
    }

    let mut addition = Vec::new();
    if !original.is_empty() && !original.ends_with(b"\n") {
        addition.push(b'\n');
    }
    if original.is_empty() {
        addition.extend_from_slice(
            b"# Generated by zeroship dev init. Existing values are never rotated.\n",
        );
    } else {
        addition.extend_from_slice(
            b"\n# Generated by zeroship dev init. Existing values are never rotated.\n",
        );
    }
    for (name, value) in missing {
        addition.extend_from_slice(name.as_bytes());
        addition.push(b'=');
        addition.extend_from_slice(value.as_bytes());
        addition.push(b'\n');
    }

    if path.exists() {
        set_private_file_permissions(path)?;
        let mut file = OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(|error| format!("open environment overlay {}: {error}", path.display()))?;
        file.write_all(&addition)
            .map_err(|error| format!("append environment overlay {}: {error}", path.display()))?;
        file.sync_all()
            .map_err(|error| format!("sync environment overlay {}: {error}", path.display()))?;
    } else {
        create_private_file(path, &addition)?;
    }
    Ok(())
}

/// Publish every service's public key into one JWKS-shaped document.
///
/// Read back from the private files rather than remembered from generation, so
/// the document describes the keys ON DISK. A run that finds existing keys and
/// generates none still republishes them, which is what makes a hand-edited or
/// half-rotated directory converge instead of silently disagreeing.
///
/// No `kid` is written: the loader derives it as the RFC 7638 thumbprint of the
/// key, which is the same value the minter stamps. Two spellings of a derived
/// fact is one spelling too many.
fn write_service_peers(secrets_dir: &Path) -> Result<(), String> {
    use base64::Engine as _;

    let mut keys = Vec::with_capacity(SERVICE_KEY_FILES.len());
    for (file, _) in SERVICE_KEY_FILES {
        let path = secrets_dir.join(file);
        let public = read_service_public_key(&path)?;
        keys.push((path, public));
    }
    // THE LAST FENCE BEFORE THE DOCUMENT EXISTS, and it is not a duplicate of
    // the one `init_dev_secrets` runs. That one rules on the directory it
    // FOUND; this one rules on the bytes about to be published, so no future
    // caller of this function can emit a document with one key under two
    // issuers by reaching it another way. The enroller key is in the set it
    // rules on even though it is never published here.
    let enroller = secrets_dir.join(WORKER_ENROLLER_FILE);
    let mut judged = keys.clone();
    judged.push((enroller.clone(), read_worker_enroller(&enroller)?.1));
    reject_shared_service_keys(&judged)?;

    let entries = SERVICE_KEY_FILES
        .iter()
        .zip(&keys)
        .map(|((_, service), (_, public))| {
            let x = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(public);
            format!(
                r#"{{"kty":"OKP","crv":"Ed25519","iss":"spiffe://zeroship.ai/{service}","x":"{x}"}}"#
            )
        })
        .collect::<Vec<_>>();
    let document = format!("{{\"keys\":[{}]}}\n", entries.join(","));
    let path = secrets_dir.join(SERVICE_PEERS_FILE);
    std::fs::write(&path, document)
        .map_err(|error| format!("write {}: {error}", path.display()))
}

/// The public half of one service key file, as the raw Ed25519 point.
///
/// The PUBLIC key is the comparison subject, never the file bytes: the same
/// credential spelled once as PKCS#8 PEM and once as PKCS#8 DER is two
/// different files and one identity, and a byte comparison would call that
/// distinct and publish it under two issuers.
fn read_service_public_key(path: &Path) -> Result<[u8; 32], String> {
    let bytes = std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let signing = if let Ok(text) = std::str::from_utf8(&bytes) {
        SigningKey::from_pkcs8_pem(text)
            .map_err(|error| format!("{}: Ed25519 PKCS#8 PEM: {error}", path.display()))?
    } else {
        SigningKey::from_pkcs8_der(&bytes)
            .map_err(|error| format!("{}: Ed25519 PKCS#8 DER: {error}", path.display()))?
    };
    Ok(signing.verifying_key().to_bytes())
}

/// The service key files that are already on disk, paired with their public
/// halves.
///
/// Only the ones that EXIST: a path this run is about to generate cannot
/// collide with anything, because `generate_signing_key` draws from `OsRng`.
/// What the operator supplied is the whole population worth judging.
fn existing_service_public_keys(secrets_dir: &Path) -> Result<Vec<(PathBuf, [u8; 32])>, String> {
    let mut keys = Vec::new();
    for (file, _) in SERVICE_KEY_FILES {
        let path = secrets_dir.join(file);
        if path.exists() {
            let public = read_service_public_key(&path)?;
            keys.push((path, public));
        }
    }
    // The enroller key joins the set: an enroller key equal to a service key
    // would let whoever holds the worker's credential present as that service.
    let enroller = secrets_dir.join(WORKER_ENROLLER_FILE);
    if enroller.exists() {
        let (_, public) = read_worker_enroller(&enroller)?;
        keys.push((enroller, public));
    }
    Ok(keys)
}

/// Refuse a set of service key files in which two paths hold the same key.
///
/// This is the enforcement `SERVICE_KEY_FILES`'s rustdoc has always claimed.
/// The consequence it prevents is not a hygiene one: the envelope and
/// assertion wire formats carry a key id derived from the public bytes and no
/// issuer, and the verifiers resolve material by issuer string alone, so a
/// document publishing one key under several issuers makes every one of those
/// identities interchangeable - including the process's own, which is the
/// worker minting a `ZeroShip-User` envelope that its own verifier accepts.
///
/// The paths are NAMED, both of them and not just the fact of a collision,
/// because the operator's next action is deleting one of two files and the
/// message is the only thing that says which two.
fn reject_shared_service_keys(keys: &[(PathBuf, [u8; 32])]) -> Result<(), String> {
    let mut by_public: BTreeMap<[u8; 32], Vec<&Path>> = BTreeMap::new();
    for (path, public) in keys {
        by_public.entry(*public).or_default().push(path.as_path());
    }
    let collisions = by_public
        .values()
        .filter(|paths| paths.len() > 1)
        .map(|paths| {
            paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(" and ")
        })
        .collect::<Vec<_>>();
    if collisions.is_empty() {
        return Ok(());
    }
    Err(format!(
        "service key files hold the same key: {}. Every service needs its own \
         key, because a peer must be able to verify a service without being \
         able to impersonate it; one key under several issuers lets the holder \
         of any of these files present as any of these services. Give each path \
         a distinct key - deleting the duplicate and re-running generates one - \
         and check whether one secret is mounted at several \
         ZEROSHIP_*_SERVICE_KEY_FILE paths. Nothing was created or changed.",
        collisions.join("; ")
    ))
}

/// The worker enroller credential document, in the shape
/// `zeroship_core::service_peers::ServiceKeyring::load_worker_enroller` reads.
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct WorkerEnrollerCredential {
    enroller_id: String,
    private_key: String,
}

/// Mint a new enroller: a fresh `wen_` id and a fresh Ed25519 key, as one
/// credential document.
fn generate_worker_enroller() -> Result<Vec<u8>, String> {
    let private_key = String::from_utf8(generate_signing_key()?)
        .map_err(|error| format!("encode the enroller key: {error}"))?;
    let credential = WorkerEnrollerCredential {
        enroller_id: zeroship_core::typed_id::new_worker_enroller_id(),
        private_key,
    };
    let mut text = serde_json::to_string_pretty(&credential)
        .map_err(|error| format!("encode the enroller credential: {error}"))?;
    text.push('\n');
    Ok(text.into_bytes())
}

fn validate_worker_enroller(bytes: &[u8]) -> Result<(), String> {
    parse_worker_enroller(bytes).map(|_| ())
}

/// The enroller id and the PUBLIC half of its key.
fn parse_worker_enroller(bytes: &[u8]) -> Result<(String, [u8; 32]), String> {
    let credential: WorkerEnrollerCredential = serde_json::from_slice(bytes)
        .map_err(|error| format!("worker enroller credential: {error}"))?;
    zeroship_core::service_peers::worker_enroller_issuer(&credential.enroller_id).map_err(|_| {
        format!(
            "enroller_id {:?} is not a worker enroller id",
            credential.enroller_id
        )
    })?;
    let signing = SigningKey::from_pkcs8_pem(&credential.private_key)
        .map_err(|error| format!("private_key is not an Ed25519 PKCS#8 PEM key: {error}"))?;
    Ok((credential.enroller_id, signing.verifying_key().to_bytes()))
}

fn read_worker_enroller(path: &Path) -> Result<(String, [u8; 32]), String> {
    let bytes = std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    parse_worker_enroller(&bytes).map_err(|error| format!("{}: {error}", path.display()))
}

fn read_worker_enroller_import(
    path: &Path,
) -> Result<Vec<zeroship_core::worker_enrollers::EnrollerRecord>, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    zeroship_core::worker_enrollers::parse_enroller_import(&bytes).map_err(|error| {
        format!(
            "existing worker enroller import file {} is invalid and was not replaced: {error}",
            path.display()
        )
    })
}

/// The import records with this host's enroller present, and whether they
/// changed.
///
/// ADDITIVE: every other entry is kept as it is. This host's entry is appended
/// when absent; when present it must name the credential's own key, and a
/// record naming that key under ANOTHER id is refused too - Control would
/// refuse either at its next boot, so this refuses it first and names the file.
fn with_this_hosts_enroller(
    mut records: Vec<zeroship_core::worker_enrollers::EnrollerRecord>,
    enroller_id: &str,
    public_key: [u8; 32],
) -> Result<(Vec<zeroship_core::worker_enrollers::EnrollerRecord>, bool), String> {
    if let Some(existing) = records.iter().find(|record| record.id == enroller_id) {
        if existing.public_key != public_key {
            return Err(format!(
                "{WORKER_ENROLLERS_FILE} names enroller {enroller_id} under a different public key \
                 than {WORKER_ENROLLER_FILE} holds; neither file was changed. A changed key is a \
                 new enroller: see docs/runbooks/worker-enrollers.md"
            ));
        }
        return Ok((records, false));
    }
    if let Some(existing) = records.iter().find(|record| record.public_key == public_key) {
        return Err(format!(
            "{WORKER_ENROLLERS_FILE} names the key in {WORKER_ENROLLER_FILE} under enroller {}, \
             not {enroller_id}; neither file was changed",
            existing.id
        ));
    }
    records.push(zeroship_core::worker_enrollers::EnrollerRecord {
        id: enroller_id.to_owned(),
        zone: zeroship_core::worker_enrollers::DEFAULT_EXECUTION_ZONE.to_owned(),
        public_key,
    });
    Ok((records, true))
}

/// Judge an existing import file against an existing credential BEFORE
/// anything is created. A missing credential cannot conflict: the one this run
/// generates is fresh.
fn check_worker_enrollers(secrets_dir: &Path) -> Result<(), String> {
    let import = secrets_dir.join(WORKER_ENROLLERS_FILE);
    reject_symlink(&import, "worker enroller import file")?;
    let credential = secrets_dir.join(WORKER_ENROLLER_FILE);
    if !import.exists() || !credential.exists() {
        return Ok(());
    }
    let (enroller_id, public_key) = read_worker_enroller(&credential)?;
    with_this_hosts_enroller(read_worker_enroller_import(&import)?, &enroller_id, public_key)
        .map(|_| ())
}

/// Write the import file Control reads, with this host's enroller in it.
///
/// Written only when it changes, so a re-run leaves an operator's file
/// byte-for-byte alone.
fn write_worker_enrollers(secrets_dir: &Path) -> Result<(), String> {
    let (enroller_id, public_key) = read_worker_enroller(&secrets_dir.join(WORKER_ENROLLER_FILE))?;
    let import = secrets_dir.join(WORKER_ENROLLERS_FILE);
    let existing = if import.exists() {
        read_worker_enroller_import(&import)?
    } else {
        Vec::new()
    };
    let (records, changed) = with_this_hosts_enroller(existing, &enroller_id, public_key)?;
    if !changed {
        return Ok(());
    }
    std::fs::write(
        &import,
        zeroship_core::worker_enrollers::render_enroller_import(&records),
    )
    .map_err(|error| format!("write {}: {error}", import.display()))
}

fn generate_signing_key() -> Result<Vec<u8>, String> {
    let mut rng = rand::rngs::OsRng;
    let signing = SigningKey::generate(&mut rng);
    let pem = signing
        .to_pkcs8_pem(ed25519_dalek::pkcs8::spki::der::pem::LineEnding::LF)
        .map_err(|error| format!("encode Ed25519 PKCS#8 PEM: {error}"))?;
    Ok(pem.as_bytes().to_vec())
}

fn validate_signing_key(bytes: &[u8]) -> Result<(), String> {
    if let Ok(text) = std::str::from_utf8(bytes) {
        if text.contains("-----BEGIN PRIVATE KEY-----") {
            return SigningKey::from_pkcs8_pem(text)
                .map(|_| ())
                .map_err(|error| format!("Ed25519 PKCS#8 PEM: {error}"));
        }
    }
    SigningKey::from_pkcs8_der(bytes)
        .map(|_| ())
        .map_err(|error| format!("Ed25519 PKCS#8 DER: {error}"))
}

fn generate_base64_secret() -> Result<Vec<u8>, String> {
    let bytes = random_bytes::<48>()?;
    let mut encoded = base64::engine::general_purpose::STANDARD
        .encode(bytes)
        .into_bytes();
    encoded.push(b'\n');
    Ok(encoded)
}

fn validate_broker_secret(bytes: &[u8]) -> Result<(), String> {
    validate_base64_secret(bytes)?;
    zeroship_core::auth::validate_broker_master(bytes)
}

fn validate_base64_secret(bytes: &[u8]) -> Result<(), String> {
    let text =
        std::str::from_utf8(bytes).map_err(|error| format!("expected base64 text: {error}"))?;
    let encoded = text.strip_suffix('\n').unwrap_or(text);
    if encoded.contains('\n') || encoded.contains('\r') {
        return Err("expected one base64 line".to_string());
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| format!("invalid base64: {error}"))?;
    if decoded.len() < 32 {
        Err(format!(
            "secret decodes to {} bytes; require at least 32",
            decoded.len()
        ))
    } else if decoded.iter().all(|byte| *byte == 0) {
        Err("secret is all zero".to_string())
    } else {
        Ok(())
    }
}

fn generate_refresh_hash_key() -> Result<Vec<u8>, String> {
    Ok(format!("1:{}\n", random_hex(48)?).into_bytes())
}



fn validate_refresh_hash_key(bytes: &[u8]) -> Result<(), String> {
    let text =
        std::str::from_utf8(bytes).map_err(|error| format!("keyring must be UTF-8: {error}"))?;
    let mut versions = BTreeSet::new();
    let mut count = 0;
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let (version, encoded) = line
            .split_once(':')
            .ok_or_else(|| "each keyring line must be version:hex-or-base64url-key".to_string())?;
        let version = version
            .trim()
            .parse::<i16>()
            .map_err(|_| format!("invalid key version {version:?}"))?;
        if !versions.insert(version) {
            return Err(format!("duplicate key version {version}"));
        }
        let encoded = encoded.trim();
        let decoded = hex::decode(encoded).ok().or_else(|| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(encoded)
                .ok()
        });
        let decoded = decoded.ok_or_else(|| "unparseable key material".to_string())?;
        if decoded.len() < 32 {
            return Err(format!(
                "key version {version} decodes to {} bytes; require at least 32",
                decoded.len()
            ));
        }
        count += 1;
    }
    if count == 0 {
        Err("keyring contains no keys".to_string())
    } else {
        Ok(())
    }
}

fn random_hex(bytes: usize) -> Result<String, String> {
    let mut value = vec![0u8; bytes];
    rand::rngs::OsRng
        .try_fill_bytes(&mut value)
        .map_err(|error| format!("read operating-system randomness: {error}"))?;
    Ok(hex::encode(value))
}

fn random_bytes<const N: usize>() -> Result<[u8; N], String> {
    let mut value = [0u8; N];
    rand::rngs::OsRng
        .try_fill_bytes(&mut value)
        .map_err(|error| format!("read operating-system randomness: {error}"))?;
    Ok(value)
}

fn ensure_private_directory(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!("secrets directory {} is a symlink", path.display()));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(format!(
                "secrets path {} is not a directory",
                path.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)
                .map_err(|error| format!("create secrets directory {}: {error}", path.display()))?;
        }
        Err(error) => {
            return Err(format!(
                "inspect secrets directory {}: {error}",
                path.display()
            ));
        }
    }
    set_directory_permissions(path)
}

fn ensure_env_parent(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|error| {
        format!(
            "create environment overlay directory {}: {error}",
            parent.display()
        )
    })
}

fn reject_symlink(path: &Path, label: &str) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(format!("{label} {} is a symlink", path.display()))
        }
        Ok(metadata) if !metadata.is_file() => {
            Err(format!("{label} {} is not a regular file", path.display()))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("inspect {label} {}: {error}", path.display())),
    }
}

fn read_optional_file(path: &Path) -> Result<Vec<u8>, String> {
    match File::open(path) {
        Ok(mut file) => {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .map_err(|error| format!("read {}: {error}", path.display()))?;
            Ok(bytes)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(format!("open {}: {error}", path.display())),
    }
}

/// Create a new file that is owner-only FROM THE MOMENT IT EXISTS.
///
/// The `O_CREAT` mode is not redundant with the `set_permissions` that
/// [`create_private_file`] runs afterwards, and deleting it would be a silent
/// regression: between `open` and `set_permissions` the file exists with
/// whatever mode the create used, and a local attacker who opens it inside that
/// window keeps a readable descriptor even after the chmod lands. Only the
/// creation mode closes it.
///
/// This is a separate function because the window is not observable from the
/// finished file: `create_private_file` ends at 0600 whichever mode it created
/// with, so a test that stats the path afterwards cannot tell the two apart.
/// Its caller returns the open handle, and the test fstats THAT.
fn open_new_private(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|error| format!("create {} without replacing it: {error}", path.display()))
}

fn create_private_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = open_new_private(path)?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(format!("write {}: {error}", path.display()));
    }
    set_private_file_permissions(path)
}

#[cfg(unix)]
fn set_directory_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("chmod 0700 {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn set_directory_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("chmod 0600 {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::open_new_private;

    /// THE UNTESTED HALF of the write side. `create_private_file` set the mode
    /// twice - once as the `O_CREAT` mode and once with `set_permissions` after
    /// the write - and only the second was covered: mutating the `O_CREAT` mode
    /// to 0o644 left the whole `dev_init` suite green, because the chmod reset
    /// it before anything looked. So the creation mode could be widened
    /// silently, and with it the window in which the secret exists world-
    /// readable, which is the window a local attacker actually races.
    ///
    /// This asserts the mode on the DESCRIPTOR `open` returned, before any
    /// chmod can run, which is the only way to tell the two apart.
    ///
    /// It does NOT cover a run under a umask of 0o077 or tighter: the kernel
    /// masks the `O_CREAT` mode, so a widened constant would come out 0600
    /// anyway - and in that environment the widening is also not exploitable.
    /// Measured under the 022 umask this repo's suites run with.
    #[test]
    fn a_generated_secret_is_owner_only_at_the_instant_it_is_created() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("generated-secret");

        let file = open_new_private(&path).expect("create the secret file");
        let mode = file
            .metadata()
            .expect("fstat the handle that created it")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(
            mode, 0o600,
            "a generated secret must never exist at a wider mode, not even for \
             the instant before set_private_file_permissions runs"
        );

        // The one-variable partner: same directory, same umask, same process -
        // only the create mode differs. `File::create` uses 0o666, so this shows
        // the assertion above is measuring the requested creation mode and not
        // some property of the filesystem or a umask that would force 0600
        // regardless.
        let control = dir.path().join("default-create");
        let mode = std::fs::File::create(&control)
            .expect("create the control")
            .metadata()
            .expect("fstat the control")
            .permissions()
            .mode()
            & 0o777;
        assert_ne!(
            mode, 0o600,
            "the control came out 0600 too, so this run's umask hides the \
             difference and the assertion above proves nothing"
        );
    }
}
