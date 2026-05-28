//! Wire types for hydra admin API.

use serde::{Deserialize, Serialize};

// ─── Login challenge ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub challenge: String,
    pub skip: bool,
    /// Empty string when skip = false (no subject yet known).
    pub subject: String,
    pub client: OAuth2Client,
    pub request_url: String,
    pub requested_scope: Vec<String>,
    pub requested_access_token_audience: Vec<String>,
    pub session_id: Option<String>,
    pub oidc_context: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Default)]
pub struct AcceptLoginRequest {
    pub subject: String,
    #[serde(skip_serializing_if = "Option::is_none")] pub remember: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")] pub remember_for: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")] pub acr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub amr: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")] pub context: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")] pub force_subject_identifier: Option<String>,
}

#[derive(Debug, Serialize, Default)]
pub struct RejectRequest {
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")] pub error_description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub status_code: Option<i32>,
}

// ─── Device Authorization Grant ─────────────────────────────────────────

#[derive(Debug, Serialize, Default)]
pub struct AcceptDeviceUserCodeRequest {
    #[serde(skip_serializing_if = "Option::is_none")] pub user_code: Option<String>,
}

// ─── Consent challenge ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ConsentRequest {
    pub challenge: String,
    pub skip: bool,
    pub subject: String,
    pub client: OAuth2Client,
    pub requested_scope: Vec<String>,
    pub requested_access_token_audience: Vec<String>,
    pub login_session_id: Option<String>,
    pub context: Option<serde_json::Value>,
    pub oidc_context: Option<serde_json::Value>,
    pub request_url: String,
}

#[derive(Debug, Serialize, Default)]
pub struct AcceptConsentRequest {
    pub grant_scope: Vec<String>,
    pub grant_access_token_audience: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub remember: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")] pub remember_for: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")] pub session: Option<ConsentSession>,
}

#[derive(Debug, Serialize, Default)]
pub struct ConsentSession {
    #[serde(skip_serializing_if = "Option::is_none")] pub id_token: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")] pub access_token: Option<serde_json::Value>,
}

// ─── Logout challenge ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct LogoutRequest {
    pub subject: String,
    pub sid: String,
    pub request_url: String,
    pub rp_initiated: bool,
    pub client: Option<OAuth2Client>,
}

// ─── Generic redirect envelope ───────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RedirectResponse {
    pub redirect_to: String,
}

// ─── Client (OAuth2Client) ───────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OAuth2Client {
    pub client_id: String,
    #[serde(skip_serializing_if = "Option::is_none")] pub client_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub client_secret: Option<String>,
    #[serde(default)] pub grant_types: Vec<String>,
    #[serde(default)] pub response_types: Vec<String>,
    #[serde(default)] pub redirect_uris: Vec<String>,
    #[serde(default)] pub post_logout_redirect_uris: Vec<String>,
    pub scope: String,
    pub token_endpoint_auth_method: String,
    pub subject_type: String,
    #[serde(skip_serializing_if = "Option::is_none")] pub access_token_strategy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub id_token_signed_response_alg: Option<String>,
    #[serde(default)] pub audience: Vec<String>,
    #[serde(default)] pub skip_consent: bool,
    #[serde(default)] pub require_consent: bool,
    #[serde(default)] pub require_logout_consent: bool,
    #[serde(skip_serializing_if = "Option::is_none")] pub frontchannel_logout_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub backchannel_logout_uri: Option<String>,
}

// ─── JWK admin ───────────────────────────────────────────────────────────

/// Hydra's `POST /admin/keys/{set}` body: `{ alg, use, kid }`. The `use`
/// field is a Rust keyword, so the struct field is `use_` and we provide
/// a manual `Serialize` impl that emits the wire name `"use"`.
#[derive(Debug)]
pub struct CreateJsonWebKeySetRequest {
    pub alg: String,
    pub use_: String,
    pub kid: String,
}

impl Serialize for CreateJsonWebKeySetRequest {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut o = s.serialize_struct("CreateJsonWebKeySetRequest", 3)?;
        o.serialize_field("alg", &self.alg)?;
        o.serialize_field("use", &self.use_)?;
        o.serialize_field("kid", &self.kid)?;
        o.end()
    }
}

#[derive(Debug, Deserialize)]
pub struct JsonWebKeySet {
    pub keys: Vec<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_jwk_serializes_use_field() {
        let body = CreateJsonWebKeySetRequest {
            alg: "EdDSA".into(),
            use_: "sig".into(),
            kid: "test".into(),
        };
        let json = serde_json::to_string(&body).expect("serialize");
        assert!(json.contains("\"use\":\"sig\""), "got: {json}");
        assert!(!json.contains("use_"), "got: {json}");
    }

    #[test]
    fn oauth2_client_round_trip() {
        let c = OAuth2Client {
            client_id: "test".into(),
            client_name: Some("Test".into()),
            client_secret: None,
            grant_types: vec!["authorization_code".into()],
            response_types: vec!["code".into()],
            redirect_uris: vec!["https://x.test/cb".into()],
            post_logout_redirect_uris: vec![],
            scope: "openid".into(),
            token_endpoint_auth_method: "client_secret_basic".into(),
            subject_type: "public".into(),
            access_token_strategy: Some("jwt".into()),
            id_token_signed_response_alg: Some("EdDSA".into()),
            audience: vec![],
            skip_consent: true,
            require_consent: false,
            require_logout_consent: false,
            frontchannel_logout_uri: None,
            backchannel_logout_uri: None,
        };
        let j = serde_json::to_string(&c).expect("ser");
        let d: OAuth2Client = serde_json::from_str(&j).expect("deser");
        assert_eq!(d.client_id, "test");
        assert!(d.skip_consent);
    }
}
