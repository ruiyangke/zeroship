//! Parse and validate the operator-provided OIDC clients config.
//!
//! TOML shape:
//!
//! ```toml
//! [[client]]
//! client_id = "console.zeroship.ai"
//! client_name = "zeroship Console"
//! redirect_uris = ["https://console.zeroship.ai/auth/callback"]
//! post_logout_redirect_uris = ["https://console.zeroship.ai/"]
//! scope = "openid offline_access email profile"
//! token_endpoint_auth_method = "client_secret_basic"
//! access_token_strategy = "jwt"
//! id_token_signed_response_alg = "EdDSA"
//! audience = ["https://api.zeroship.ai"]
//! first_party = true
//! ```

use serde::Deserialize;

use crate::error::{AuthError, Result};
use crate::hydra_client::types::OAuth2Client;

#[derive(Debug, Deserialize)]
pub struct ClientsConfig {
    #[serde(default, rename = "client")]
    pub clients: Vec<ClientEntry>,
}

#[derive(Debug, Deserialize)]
pub struct ClientEntry {
    pub client_id: String,
    pub client_name: Option<String>,
    /// Auto-generated if absent for confidential clients.
    pub client_secret: Option<String>,
    #[serde(default = "default_grant_types")] pub grant_types: Vec<String>,
    #[serde(default = "default_response_types")] pub response_types: Vec<String>,
    #[serde(default)] pub redirect_uris: Vec<String>,
    #[serde(default)] pub post_logout_redirect_uris: Vec<String>,
    pub scope: String,
    #[serde(default = "default_auth_method")] pub token_endpoint_auth_method: String,
    #[serde(default = "default_subject_type")] pub subject_type: String,
    pub access_token_strategy: Option<String>,
    pub id_token_signed_response_alg: Option<String>,
    #[serde(default)] pub audience: Vec<String>,
    /// First-party flag. When true: `skip_consent=true`, `require_consent=false`,
    /// `require_logout_consent=false`.
    #[serde(default)] pub first_party: bool,
    pub frontchannel_logout_uri: Option<String>,
    pub backchannel_logout_uri: Option<String>,
}

fn default_grant_types()    -> Vec<String> { vec!["authorization_code".into(), "refresh_token".into()] }
fn default_response_types() -> Vec<String> { vec!["code".into()] }
fn default_auth_method()    -> String      { "client_secret_basic".into() }
fn default_subject_type()   -> String      { "public".into() }

impl ClientsConfig {
    /// Read and parse the TOML at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Bootstrap`] if the file cannot be read or fails
    /// to parse as a valid `ClientsConfig`.
    pub fn from_path(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| AuthError::Bootstrap(format!("read {path}: {e}")))?;
        let cfg: Self = toml::from_str(&raw)
            .map_err(|e| AuthError::Bootstrap(format!("parse {path}: {e}")))?;
        Ok(cfg)
    }
}

impl ClientEntry {
    /// Map this declarative entry to the hydra-admin `OAuth2Client` wire shape.
    #[must_use]
    pub fn to_oauth2_client(&self) -> OAuth2Client {
        OAuth2Client {
            client_id: self.client_id.clone(),
            client_name: self.client_name.clone(),
            client_secret: self.client_secret.clone(),
            grant_types: self.grant_types.clone(),
            response_types: self.response_types.clone(),
            redirect_uris: self.redirect_uris.clone(),
            post_logout_redirect_uris: self.post_logout_redirect_uris.clone(),
            scope: self.scope.clone(),
            token_endpoint_auth_method: self.token_endpoint_auth_method.clone(),
            subject_type: self.subject_type.clone(),
            access_token_strategy: self.access_token_strategy.clone(),
            id_token_signed_response_alg: self.id_token_signed_response_alg.clone(),
            audience: self.audience.clone(),
            skip_consent: self.first_party,
            require_consent: !self.first_party,
            require_logout_consent: false,
            frontchannel_logout_uri: self.frontchannel_logout_uri.clone(),
            backchannel_logout_uri: self.backchannel_logout_uri.clone(),
        }
    }
}
