//! Dev bootstrap for the first-party zeroship-builder OAuth client.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use compio_postgres::Client;
use rand::RngCore as _;
use serde::Serialize;
use uuid::Uuid;
use zeroship_authz::Scope;

// Defined in `zeroship-core` so the compiled default trusted-client set can
// reference it without a control → core dependency cycle. Re-exported here to
// keep every `bootstrap_builder::BUILDER_CLIENT_ID` call site unchanged.
pub use zeroship_core::auth::trusted_clients::BUILDER_CLIENT_ID;
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
    pub hydra_admin_url: String,
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
    Hydra(String),
    Encode(String),
}

impl std::fmt::Display for BuilderClientBootstrapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(err) => write!(f, "database: {err}"),
            Self::Io(err) => write!(f, "io: {err}"),
            Self::InvalidSecretFile { path } => {
                write!(f, "invalid builder client secret file: {}", path.display())
            }
            Self::Hydra(err) => write!(f, "hydra admin: {err}"),
            Self::Encode(err) => write!(f, "encode hydra request: {err}"),
        }
    }
}

impl std::error::Error for BuilderClientBootstrapError {}

#[derive(Debug, Serialize)]
struct HydraCreateClientRequest<'a> {
    client_id: &'a str,
    client_name: &'a str,
    client_secret: &'a str,
    redirect_uris: &'a [String],
    grant_types: &'a [&'static str],
    response_types: &'a [&'static str],
    scope: &'a str,
    token_endpoint_auth_method: &'static str,
    subject_type: &'static str,
    skip_consent: bool,
}

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
    create_hydra_client(cfg, &client_secret).await?;
    insert_oauth_client(pg, cfg).await?;

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
) -> Result<(), BuilderClientBootstrapError> {
    let redirect_uris = vec![cfg.redirect_uri.as_str()];
    let scopes = BUILDER_SCOPES
        .iter()
        .map(|scope| scope.as_str())
        .collect::<Vec<_>>();
    let created_by: Option<Uuid> = None;
    pg.execute(
        "INSERT INTO zeroship.oauth_clients \
            (client_id, client_name, client_uri, logo_uri, redirect_uris, scopes, \
             skip_consent, created_by, hydra_client_id) \
         VALUES ($1, $2, NULL, NULL, $3, $4, $5, $6, $1)",
        &[
            &BUILDER_CLIENT_ID,
            &BUILDER_CLIENT_NAME,
            &redirect_uris,
            &scopes,
            &cfg.skip_consent,
            &created_by,
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

async fn create_hydra_client(
    cfg: &BuilderClientBootstrapConfig,
    client_secret: &str,
) -> Result<(), BuilderClientBootstrapError> {
    let redirect_uris = vec![cfg.redirect_uri.clone()];
    let grant_types = ["authorization_code", "refresh_token"];
    let response_types = ["code"];
    let scope = BUILDER_SCOPES
        .iter()
        .map(|scope| scope.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let body = HydraCreateClientRequest {
        client_id: BUILDER_CLIENT_ID,
        client_name: BUILDER_CLIENT_NAME,
        client_secret,
        redirect_uris: &redirect_uris,
        grant_types: &grant_types,
        response_types: &response_types,
        scope: &scope,
        token_endpoint_auth_method: "client_secret_basic",
        subject_type: "public",
        skip_consent: cfg.skip_consent,
    };
    let body_bytes = serde_json::to_vec(&body)
        .map_err(|err| BuilderClientBootstrapError::Encode(err.to_string()))?;
    let res = cyper::Client::new()
        .request(http::Method::POST, hydra_url(&cfg.hydra_admin_url, "/admin/clients"))
        .map_err(|err| BuilderClientBootstrapError::Hydra(format!("build request: {err}")))?
        .header("content-type", "application/json")
        .map_err(|err| BuilderClientBootstrapError::Hydra(format!("build request: {err}")))?
        .body(body_bytes)
        .send()
        .await
        .map_err(|err| BuilderClientBootstrapError::Hydra(format!("transport: {err}")))?;
    let status = res.status().as_u16();
    let response_body = res
        .text()
        .await
        .map_err(|err| BuilderClientBootstrapError::Hydra(format!("read response: {err}")))?;
    if (200..300).contains(&status) || status == 409 {
        Ok(())
    } else {
        Err(BuilderClientBootstrapError::Hydra(format!(
            "POST /admin/clients returned {status}: {response_body}"
        )))
    }
}

fn hydra_url(base_url: &str, path: &str) -> String {
    format!("{}{}", base_url.trim_end_matches('/'), path)
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
