//! Dev bootstrap for the first-party zeroship-builder OAuth client.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use compio_postgres::Client;
use rand::RngCore as _;
use uuid::Uuid;
use zeroship_core::auth::hash_api_key;
use zeroship_authz::Scope;

/// Client ID of the first-party `zeroship-builder` OAuth client.
///
/// Owned here (the builder client's only consumer), not in `zeroship-core`:
/// core's trusted-client default is now empty (fail-closed) and references no
/// hard-coded client id, so this constant no longer needs to live there. It is
/// NOT trusted-by-default — a deployment that still uses the builder client
/// must name it explicitly in `[auth].trusted_oauth_clients`.
pub const BUILDER_CLIENT_ID: &str = "zeroship-builder";
pub const BUILDER_CLIENT_NAME: &str = "zeroship builder";
pub const DEFAULT_BUILDER_REDIRECT_URI: &str = "http://localhost:3001/auth/callback";
pub const DEFAULT_BUILDER_CLIENT_SECRET_PATH: &str = "data/builder-client-secret";

const BUILDER_SCOPES: &[Scope] = &[
    Scope::AppsRead,
    Scope::AppsWrite,
    Scope::AppsDeploy,
    Scope::EnvRead,
    Scope::EnvWrite,
    Scope::SecretsRead,
    Scope::SecretsWrite,
    Scope::DeploymentsRead,
    Scope::DeploymentsRollback,
];

#[derive(Clone, Debug)]
pub struct BuilderClientBootstrapConfig {
    pub enabled: bool,
    pub redirect_uri: String,
    pub client_secret_path: PathBuf,
    pub skip_consent: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuilderClientBootstrapStatus {
    Disabled,
    AlreadyPresent,
    Created,
}

#[derive(Clone, Debug)]
pub struct BuilderClientBootstrapResult {
    pub status: BuilderClientBootstrapStatus,
    pub client_secret_path: PathBuf,
}

#[derive(Debug)]
pub enum BuilderClientBootstrapError {
    Db(String),
    Io(String),
    InvalidSecretFile { path: PathBuf },
}

impl std::fmt::Display for BuilderClientBootstrapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(err) => write!(f, "database: {err}"),
            Self::Io(err) => write!(f, "io: {err}"),
            Self::InvalidSecretFile { path } => {
                write!(f, "invalid builder client secret file: {}", path.display())
            }
        }
    }
}

impl std::error::Error for BuilderClientBootstrapError {}

pub async fn bootstrap_builder_oauth_client(
    pg: &Client,
    cfg: &BuilderClientBootstrapConfig,
) -> Result<BuilderClientBootstrapResult, BuilderClientBootstrapError> {
    if !cfg.enabled {
        return Ok(BuilderClientBootstrapResult {
            status: BuilderClientBootstrapStatus::Disabled,
            client_secret_path: cfg.client_secret_path.clone(),
        });
    }

    if oauth_client_exists(pg).await? {
        read_existing_client_secret(&cfg.client_secret_path)?;
        tracing::info!(
            client_id = BUILDER_CLIENT_ID,
            secret_path = %cfg.client_secret_path.display(),
            "control: builder OAuth client already bootstrapped"
        );
        return Ok(BuilderClientBootstrapResult {
            status: BuilderClientBootstrapStatus::AlreadyPresent,
            client_secret_path: cfg.client_secret_path.clone(),
        });
    }

    let client_secret = ensure_client_secret(&cfg.client_secret_path)?;
    insert_oauth_client(pg, cfg, &client_secret).await?;

    tracing::info!(
        client_id = BUILDER_CLIENT_ID,
        secret_path = %cfg.client_secret_path.display(),
        "control: bootstrapped builder OAuth client"
    );
    Ok(BuilderClientBootstrapResult {
        status: BuilderClientBootstrapStatus::Created,
        client_secret_path: cfg.client_secret_path.clone(),
    })
}

async fn oauth_client_exists(pg: &Client) -> Result<bool, BuilderClientBootstrapError> {
    let rows = pg
        .query(
            "SELECT 1 FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&BUILDER_CLIENT_ID],
        )
        .await
        .map_err(|err| BuilderClientBootstrapError::Db(err.to_string()))?;
    Ok(!rows.is_empty())
}

async fn insert_oauth_client(
    pg: &Client,
    cfg: &BuilderClientBootstrapConfig,
    client_secret: &str,
) -> Result<(), BuilderClientBootstrapError> {
    let redirect_uris = vec![cfg.redirect_uri.as_str()];
    let scopes = BUILDER_SCOPES
        .iter()
        .map(|scope| scope.as_str())
        .collect::<Vec<_>>();
    let created_by: Option<Uuid> = None;
    let client_secret_hash = hash_api_key(client_secret);
    // TODO(P6): drop oauth_clients.hydra_client_id column; placeholder write until then.
    pg.execute(
        "INSERT INTO zeroship.oauth_clients \
            (client_id, client_name, client_uri, logo_uri, redirect_uris, scopes, \
             skip_consent, created_by, hydra_client_id, client_secret_hash, \
             refresh_allowed, token_endpoint_auth_method) \
         VALUES ($1, $2, NULL, NULL, $3, $4, $5, $6, $1, $7, TRUE, 'client_secret_basic')",
        &[
            &BUILDER_CLIENT_ID,
            &BUILDER_CLIENT_NAME,
            &redirect_uris,
            &scopes,
            &cfg.skip_consent,
            &created_by,
            &client_secret_hash,
        ],
    )
    .await
    .map_err(|err| BuilderClientBootstrapError::Db(err.to_string()))?;
    Ok(())
}

fn ensure_client_secret(path: &Path) -> Result<String, BuilderClientBootstrapError> {
    match read_existing_client_secret(path) {
        Ok(secret) => return Ok(secret),
        Err(BuilderClientBootstrapError::Io(_)) if !path.exists() => {}
        Err(err) => return Err(err),
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            BuilderClientBootstrapError::Io(format!("create {}: {err}", parent.display()))
        })?;
    }

    let mut bytes = [0_u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let secret = hex::encode(bytes);

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }

    match options.open(path) {
        Ok(mut file) => {
            file.write_all(secret.as_bytes()).map_err(|err| {
                BuilderClientBootstrapError::Io(format!("write {}: {err}", path.display()))
            })?;
            Ok(secret)
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            read_existing_client_secret(path)
        }
        Err(err) => Err(BuilderClientBootstrapError::Io(format!(
            "create {}: {err}",
            path.display()
        ))),
    }
}

fn read_existing_client_secret(path: &Path) -> Result<String, BuilderClientBootstrapError> {
    let raw = std::fs::read_to_string(path).map_err(|err| {
        BuilderClientBootstrapError::Io(format!("read {}: {err}", path.display()))
    })?;
    let secret = raw.trim().to_string();
    if is_valid_client_secret(&secret) {
        Ok(secret)
    } else {
        Err(BuilderClientBootstrapError::InvalidSecretFile {
            path: path.to_path_buf(),
        })
    }
}

fn is_valid_client_secret(secret: &str) -> bool {
    secret.len() == 64 && secret.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_secret_shape_is_hex_32_bytes() {
        assert!(is_valid_client_secret(&"0".repeat(64)));
        assert!(!is_valid_client_secret(&"0".repeat(63)));
        assert!(!is_valid_client_secret(&"g".repeat(64)));
    }
}
