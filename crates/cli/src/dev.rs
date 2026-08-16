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
const ENV_KEYS: &[&str] = &[
    "ZEROSHIP_AUTH_PLATFORM_MINT_KEY",
    "ZEROSHIP_CONTROL_KEY",
    "ZEROSHIP_CONTROL_MASTER_KEY",
    "ZEROSHIP_WORKER_KEY",
    "ZEROSHIP_MIGRATED_POLICY_SEAL_KEY",
    "ZEROSHIP_GATEWAY_STASH_SIGNING_KEY",
    "ZEROSHIP_PAIRWISE_SALT",
    "ZEROSHIP_AUTH_STASH_SIGNING_KEY",
    "ZEROSHIP_AUTH_TOTP_ENC_KEY",
];

const WEAK_LITERALS: &[&str] = &[
    "platform-key",
    "master-key",
    "dev-worker-key-not-for-production-use",
    "dev-secret-rotate-me-too",
    "dev-stash-signing-key-not-for-production",
    "dev-pairwise-salt-never-rotate-in-prod",
];

pub(crate) fn cmd_dev(args: &[String]) -> Result<(), String> {
    if args.get(2).map(String::as_str) != Some("init") {
        return Err(dev_init_usage().to_string());
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

    let mut desired_env = existing_env.clone();
    for name in ENV_KEYS {
        if !desired_env.contains_key(*name) {
            desired_env.insert((*name).to_string(), random_hex(32)?);
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

    let missing_env = ENV_KEYS
        .iter()
        .filter(|name| !existing_env.contains_key(**name))
        .map(|name| ((*name).to_string(), desired_env[*name].clone()))
        .collect::<Vec<_>>();
    append_env_values(env_file, &original_env, &missing_env)?;
    outcome.added_env = missing_env.len();
    outcome.existing_env = ENV_KEYS.len() - missing_env.len();
    Ok(outcome)
}

type SecretSpec = (
    &'static str,
    fn() -> Result<Vec<u8>, String>,
    fn(&[u8]) -> Result<(), String>,
);

fn secret_specs() -> [SecretSpec; 6] {
    [
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
            "control-signing.pem",
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
    let managed = ENV_KEYS.iter().copied().collect::<BTreeSet<_>>();
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
    match name {
        "ZEROSHIP_CONTROL_MASTER_KEY" | "ZEROSHIP_AUTH_TOTP_ENC_KEY" => {
            zeroship_core::config::validate_master_key_material(name, value)
        }
        "ZEROSHIP_WORKER_KEY" => zeroship_core::config::validate_worker_key(name, value),
        "ZEROSHIP_GATEWAY_STASH_SIGNING_KEY" | "ZEROSHIP_AUTH_STASH_SIGNING_KEY" => {
            zeroship_core::config::validate_stash_key(name, value)
        }
        "ZEROSHIP_PAIRWISE_SALT" => zeroship_core::config::validate_pairwise_salt(name, value),
        "ZEROSHIP_AUTH_PLATFORM_MINT_KEY"
        | "ZEROSHIP_CONTROL_KEY"
        | "ZEROSHIP_MIGRATED_POLICY_SEAL_KEY" => Ok(()),
        _ => Err(format!("unknown generated environment key {name}")),
    }
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

fn create_private_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("create {} without replacing it: {error}", path.display()))?;
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
