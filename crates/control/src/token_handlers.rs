//! Personal Access Token handlers for the creator console.

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use chrono::{DateTime, Duration, Utc};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use ntex::web;
use ntex::web::types::{Json, Path as WebPath, State};
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;
use zeroship_authz::{self as authz, AuthzContext, AuthzDecision, Effect, Policy};

use crate::authz_guard::AuthzGuard;
use crate::AppState;

const MAX_EXPIRES_IN_DAYS: u16 = 365;
const DEFAULT_EXPIRES_IN_DAYS: u16 = 365;
const MAX_GRANT_PAIRS: usize = 10_000;
const PAT_AUDIENCE: &str = "control.zeroship.ai";
const PAT_ISSUER: &str = "https://api.zeroship.ai";

#[derive(Debug, Deserialize)]
pub struct CreateTokenBody {
    name: String,
    policies: Policy,
    expires_in_days: Option<u16>,
}

#[derive(Debug, Serialize)]
struct CreateTokenResponse {
    id: String,
    token: String,
    expires_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
struct TokenListEntry {
    id: String,
    name: String,
    created_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    last_used_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
struct DeleteTokenResponse {
    id: String,
    revoked_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatClaims {
    pub iss: String,
    pub aud: String,
    pub sub: String,
    pub owner: String,
    pub tid: String,
    pub jti: String,
    pub iat: i64,
    pub exp: i64,
    pub scope: String,
    pub policy_hash: String,
    pub nonce: String,
}

pub struct PatIssuer {
    private_der: Vec<u8>,
    decoding_key: DecodingKey,
    kid: String,
}

impl std::fmt::Debug for PatIssuer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PatIssuer")
            .field("kid", &self.kid)
            .finish_non_exhaustive()
    }
}

impl PatIssuer {
    pub fn new(signing_key: &ed25519_dalek::SigningKey) -> Result<Self, String> {
        use ed25519_dalek::pkcs8::EncodePrivateKey;

        let private_der = signing_key
            .to_pkcs8_der()
            .map_err(|err| format!("PAT signing key PKCS#8 encode: {err}"))?
            .as_bytes()
            .to_vec();
        let decoding_key = DecodingKey::from_ed_der(signing_key.verifying_key().as_bytes());
        let kid = jwk_thumbprint(signing_key);
        Ok(Self {
            private_der,
            decoding_key,
            kid,
        })
    }

    pub fn dev_insecure() -> Self {
        let key = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        Self::new(&key).expect("static dev PAT key is valid")
    }

    pub fn issue(
        &self,
        token_id: Uuid,
        owner_id: Uuid,
        policy_hash: String,
        expires_at: DateTime<Utc>,
    ) -> Result<String, String> {
        let now = now_unix()?;
        let mut nonce = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(nonce);
        let token_id = token_id.to_string();
        let owner_id = owner_id.to_string();
        let claims = PatClaims {
            iss: PAT_ISSUER.to_owned(),
            aud: PAT_AUDIENCE.to_owned(),
            sub: owner_id.clone(),
            owner: owner_id,
            tid: token_id.clone(),
            jti: token_id,
            iat: now,
            exp: expires_at.timestamp(),
            scope: "pat".to_owned(),
            policy_hash,
            nonce,
        };

        let mut header = Header::new(Algorithm::EdDSA);
        header.typ = Some("pat+jwt".to_owned());
        header.kid = Some(self.kid.clone());
        let key = EncodingKey::from_ed_der(&self.private_der);
        encode(&header, &claims, &key).map_err(|err| format!("PAT JWT encode: {err}"))
    }

    pub fn verify(&self, token: &str) -> Result<PatClaims, String> {
        let header = jsonwebtoken::decode_header(token)
            .map_err(|err| format!("PAT JWT header decode: {err}"))?;
        if header.typ.as_deref() != Some("pat+jwt") {
            return Err(format!("unexpected PAT JWT typ: {:?}", header.typ));
        }
        match header.kid.as_deref() {
            Some(kid) if kid == self.kid => {}
            Some(kid) => return Err(format!("unknown PAT JWT kid: {kid}")),
            None => return Err("missing PAT JWT kid".to_owned()),
        }

        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_issuer(&[PAT_ISSUER]);
        validation.set_audience(&[PAT_AUDIENCE]);
        decode::<PatClaims>(token, &self.decoding_key, &validation)
            .map(|data| data.claims)
            .map_err(|err| format!("PAT JWT verify: {err}"))
    }
}

pub fn load_signing_key_from_path(path: &Path) -> Result<ed25519_dalek::SigningKey, String> {
    let bytes = std::fs::read(path)
        .map_err(|err| format!("read PAT signing key {}: {err}", path.display()))?;
    reject_insecure_permissions(path)?;

    if let Ok(text) = std::str::from_utf8(&bytes) {
        if text.contains("-----BEGIN PRIVATE KEY-----") {
            use ed25519_dalek::pkcs8::DecodePrivateKey;
            return ed25519_dalek::SigningKey::from_pkcs8_pem(text)
                .map_err(|err| format!("Ed25519 PAT PKCS#8 PEM: {err}"));
        }
    }

    use ed25519_dalek::pkcs8::DecodePrivateKey;
    ed25519_dalek::SigningKey::from_pkcs8_der(&bytes)
        .map_err(|err| format!("Ed25519 PAT PKCS#8 DER: {err}"))
}

#[cfg(unix)]
fn reject_insecure_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;

    let mode = path
        .metadata()
        .map_err(|err| format!("stat PAT signing key {}: {err}", path.display()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(format!(
            "PAT signing key {} has insecure mode {mode:o}; group/world permissions must be zero",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn reject_insecure_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

pub async fn create_token(
    guard: AuthzGuard,
    state: State<Arc<AppState>>,
    body: Json<CreateTokenBody>,
) -> web::HttpResponse {
    if guard.token_id.is_some() {
        return web::HttpResponse::Unauthorized().json(&json!({
            "error": "session_required",
            "message": "PATs can only be minted from a console session",
        }));
    }

    let expires_in_days = body.expires_in_days.unwrap_or(DEFAULT_EXPIRES_IN_DAYS);
    if expires_in_days == 0 || expires_in_days > MAX_EXPIRES_IN_DAYS {
        return web::HttpResponse::BadRequest().json(&json!({
            "error": "invalid_expires_in_days",
            "message": "expires_in_days must be between 1 and 365",
        }));
    }

    if let Err(resp) = validate_grant_subset(&guard, &state, &body.policies).await {
        return resp;
    }

    let token_id = Uuid::new_v4();
    let policies_json = body.policies.to_json_value();
    let policy_hash = authz::policy_hash(&policies_json);
    let expires_at = Utc::now() + Duration::days(i64::from(expires_in_days));
    let token = match state.pat_issuer.issue(
        token_id,
        guard.principal_id,
        policy_hash.clone(),
        expires_at,
    ) {
        Ok(token) => token,
        Err(err) => {
            tracing::error!(error = %err, "control: PAT issue failed");
            return web::HttpResponse::InternalServerError()
                .json(&json!({"error": "pat_issue_failed"}));
        }
    };

    match state
        .auth_pg
        .execute(
            "INSERT INTO control.permission_tokens \
                (id, owner_id, kind, name, policies, policy_hash, expires_at) \
             VALUES ($1, $2, 'pat', $3, $4, $5, $6)",
            &[
                &token_id,
                &guard.principal_id,
                &body.name,
                &policies_json,
                &policy_hash,
                &expires_at,
            ],
        )
        .await
    {
        Ok(_) => web::HttpResponse::Ok().json(&CreateTokenResponse {
            id: token_id.to_string(),
            token,
            expires_at,
        }),
        Err(err) => {
            tracing::error!(error = %err, "control: PAT insert failed");
            web::HttpResponse::InternalServerError().json(&json!({"error": "pat_insert_failed"}))
        }
    }
}

pub async fn list_tokens(
    guard: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    let rows = match state
        .auth_pg
        .query(
            "SELECT id, name, created_at, expires_at, last_used_at, revoked_at \
             FROM control.permission_tokens \
             WHERE owner_id = $1 AND kind = 'pat' \
             ORDER BY created_at DESC, id DESC",
            &[&guard.principal_id],
        )
        .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(error = %err, "control: PAT list failed");
            return web::HttpResponse::InternalServerError()
                .json(&json!({"error": "pat_list_failed"}));
        }
    };

    let tokens = rows
        .into_iter()
        .map(|row| TokenListEntry {
            id: row.get::<_, Uuid>("id").to_string(),
            name: row.get("name"),
            created_at: row.get("created_at"),
            expires_at: row.get("expires_at"),
            last_used_at: row.get("last_used_at"),
            revoked_at: row.get("revoked_at"),
        })
        .collect::<Vec<_>>();

    web::HttpResponse::Ok().json(&tokens)
}

pub async fn delete_token(
    guard: AuthzGuard,
    state: State<Arc<AppState>>,
    id: WebPath<String>,
) -> web::HttpResponse {
    let token_id = match id.parse::<Uuid>() {
        Ok(id) => id,
        Err(_) => {
            return web::HttpResponse::BadRequest().json(&json!({"error": "invalid_token_id"}));
        }
    };

    let rows = match state
        .auth_pg
        .query(
            "UPDATE control.permission_tokens \
             SET revoked_at = COALESCE(revoked_at, NOW()) \
             WHERE id = $1 AND owner_id = $2 AND kind = 'pat' \
             RETURNING revoked_at",
            &[&token_id, &guard.principal_id],
        )
        .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(error = %err, "control: PAT revoke failed");
            return web::HttpResponse::InternalServerError()
                .json(&json!({"error": "pat_revoke_failed"}));
        }
    };

    let Some(row) = rows.first() else {
        return web::HttpResponse::NotFound().json(&json!({"error": "token_not_found"}));
    };
    let revoked_at: DateTime<Utc> = row.get("revoked_at");
    web::HttpResponse::Ok().json(&DeleteTokenResponse {
        id: token_id.to_string(),
        revoked_at,
    })
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/me/tokens")
            .route(web::post().to(create_token))
            .route(web::get().to(list_tokens)),
    )
    .service(web::resource("/me/tokens/{id}").route(web::delete().to(delete_token)));
}

async fn validate_grant_subset(
    guard: &AuthzGuard,
    state: &AppState,
    policy: &Policy,
) -> Result<(), web::HttpResponse> {
    let mut pairs = 0usize;
    for statement in &policy.statements {
        if statement.effect != Effect::Allow {
            continue;
        }
        for action in &statement.actions {
            for resource in &statement.resources {
                pairs += 1;
                if pairs > MAX_GRANT_PAIRS {
                    return Err(web::HttpResponse::BadRequest().json(&json!({
                        "error": "too_many_policy_pairs",
                        "message": "policy expands beyond 10000 action/resource pairs",
                    })));
                }

                let ctx = AuthzContext {
                    principal_id: guard.principal_id,
                    token_id: None,
                    action: *action,
                    resource: resource.clone(),
                    request_ip: guard.request_ip,
                    mfa_verified: guard.mfa_verified,
                    mfa_age_seconds: guard.mfa_age_seconds,
                    request_id: None,
                };

                match authz::enforce(&state.auth_pg, &state.static_policies, &ctx).await {
                    Ok(AuthzDecision::Allow) => {}
                    Ok(AuthzDecision::Deny) => {
                        return Err(web::HttpResponse::BadRequest().json(&json!({
                            "error": "excess_permissions",
                            "message": "you can't grant a permission you don't have",
                            "action": action.cedar_id(),
                            "resource": resource,
                        })));
                    }
                    Err(err) => {
                        tracing::error!(error = %err, "control: PAT grant validation failed");
                        return Err(web::HttpResponse::InternalServerError()
                            .json(&json!({"error": "authz_error"})));
                    }
                }
            }
        }
    }
    Ok(())
}

fn now_unix() -> Result<i64, String> {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|err| format!("clock: {err}"))?
            .as_secs(),
    )
    .map_err(|err| format!("clock overflow: {err}"))
}

fn jwk_thumbprint(key: &ed25519_dalek::SigningKey) -> String {
    use sha2::{Digest, Sha256};

    let x = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(key.verifying_key().to_bytes());
    let canonical = format!(r#"{{"crv":"Ed25519","kty":"OKP","x":"{x}"}}"#);
    let digest = Sha256::digest(canonical.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}
