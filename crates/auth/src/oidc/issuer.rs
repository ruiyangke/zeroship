//! Platform JWT issuer for RFC 9068 access tokens and OIDC ID tokens.

use std::collections::HashSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use compio_postgres::Client;
use jsonwebtoken::{
    decode, decode_header, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};

use crate::advisory_lock::{with_advisory_lock, OP_SIGNING_KEY_BOOTSTRAP_LOCK};
use crate::error::{AuthError, Result};
use crate::oidc::signing;

/// RFC 9068 access-token type header.
pub const ACCESS_TOKEN_TYP: &str = "at+jwt";
/// OIDC ID-token type header. OIDC permits omitting it; zeroship stamps it.
pub const ID_TOKEN_TYP: &str = "JWT";
/// Default short-lived platform access token lifetime.
pub const ACCESS_TOKEN_TTL_SECS: i64 = 15 * 60;
/// Default ID-token lifetime; not longer than the paired access token.
pub const ID_TOKEN_TTL_SECS: i64 = 15 * 60;

/// RFC 9068 JWT access-token claims.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessTokenClaims {
    pub iss: String,
    pub sub: String,
    pub aud: String,
    pub exp: i64,
    pub iat: i64,
    pub jti: String,
    pub client_id: String,
    pub scope: String,
}

/// OIDC Core ID-token claims.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdTokenClaims {
    pub iss: String,
    pub sub: String,
    pub aud: String,
    pub exp: i64,
    pub iat: i64,
    pub nonce: String,
    pub at_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_time: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amr: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email_verified: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
}

/// Inputs for minting an access token.
#[derive(Debug, Clone)]
pub struct AccessTokenMint<'a> {
    pub user_id: &'a str,
    pub sector: &'a str,
    pub audience: &'a str,
    pub client_id: &'a str,
    pub scopes: &'a [String],
    pub ttl_secs: Option<i64>,
}

/// Inputs for minting a platform access token whose subject is the canonical
/// platform principal id instead of an app-sector pairwise subject.
#[derive(Debug, Clone)]
pub struct PrincipalAccessTokenMint<'a> {
    pub principal_id: &'a str,
    pub audience: &'a str,
    pub client_id: &'a str,
    pub scopes: &'a [String],
    pub ttl_secs: Option<i64>,
}

/// Inputs for minting an ID token whose subject is the canonical platform
/// principal id instead of an app-sector pairwise subject.
#[derive(Debug, Clone)]
pub struct PrincipalIdTokenMint<'a> {
    pub principal_id: &'a str,
    pub client_id: &'a str,
    pub nonce: &'a str,
    pub access_token: &'a str,
    pub auth_time: Option<i64>,
    pub amr: Option<&'a [String]>,
    pub acr: Option<&'a str>,
    pub email: Option<&'a str>,
    pub email_verified: Option<bool>,
    pub name: Option<&'a str>,
    pub picture: Option<&'a str>,
    pub ttl_secs: Option<i64>,
}

/// Inputs for minting an ID token paired to an access token.
#[derive(Debug, Clone)]
pub struct IdTokenMint<'a> {
    pub user_id: &'a str,
    pub sector: &'a str,
    pub client_id: &'a str,
    pub nonce: &'a str,
    pub access_token: &'a str,
    pub auth_time: Option<i64>,
    pub amr: Option<&'a [String]>,
    pub acr: Option<&'a str>,
    pub email: Option<&'a str>,
    pub email_verified: Option<bool>,
    pub name: Option<&'a str>,
    pub picture: Option<&'a str>,
    pub ttl_secs: Option<i64>,
}

/// Current + optional previous broker master secrets used to authenticate
/// gateway-brokered authorization-code exchanges.
#[derive(Clone)]
pub struct BrokerSecrets {
    current: Vec<u8>,
    previous: Option<Vec<u8>>,
}

impl std::fmt::Debug for BrokerSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrokerSecrets")
            .field("current", &"<redacted>")
            .field("previous_configured", &self.previous.is_some())
            .finish()
    }
}

impl BrokerSecrets {
    /// Construct a validated broker-secret set from raw master-secret bytes.
    pub fn new(current: Vec<u8>, previous: Option<Vec<u8>>) -> Result<Self> {
        zeroship_core::auth::validate_broker_master(&current)
            .map_err(AuthError::Config)?;
        if let Some(previous) = previous.as_ref() {
            zeroship_core::auth::validate_broker_master(previous)
                .map_err(AuthError::Config)?;
        }
        Ok(Self { current, previous })
    }

    /// Load and validate broker master secrets from owner-only files.
    pub fn from_files(current_file: &Path, previous_file: Option<&Path>) -> Result<Self> {
        let current =
            signing::load_broker_master_secret(current_file, "AUTH_BROKER_SECRET_FILE")?;
        let previous = previous_file
            .map(|path| {
                signing::load_broker_master_secret(
                    path,
                    "AUTH_BROKER_SECRET_PREVIOUS_FILE",
                )
            })
            .transpose()?;
        Self::new(current, previous)
    }

    /// Constant-time check of a presented per-client broker secret against the
    /// current and rotation-window previous master secrets.
    #[must_use]
    pub fn verify_client_secret(&self, client_id: &str, presented: &str) -> bool {
        let current = zeroship_core::auth::derive_broker_secret(&self.current, client_id);
        let current_ok = zeroship_core::auth::validate_control_key(presented, &current);
        let previous_ok = self.previous.as_ref().is_some_and(|previous| {
            let expected = zeroship_core::auth::derive_broker_secret(previous, client_id);
            zeroship_core::auth::validate_control_key(presented, &expected)
        });
        current_ok || previous_ok
    }
}

/// Platform OP signer. Holds the PKCS#8 DER private key in memory, never in DB.
pub struct Issuer {
    private_der: Vec<u8>,
    kid: String,
    issuer: String,
    pairwise_salt: [u8; 32],
    public_jwk: serde_json::Value,
    broker_secrets: Option<BrokerSecrets>,
}

impl std::fmt::Debug for Issuer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("oidc::Issuer")
            .field("kid", &self.kid)
            .field("issuer", &self.issuer)
            .field("broker_secrets_configured", &self.broker_secrets.is_some())
            .finish_non_exhaustive()
    }
}

impl Issuer {
    /// Load the OP signing key and pairwise-salt source from files.
    pub fn from_files(
        signing_key_file: &Path,
        pairwise_salt_file: &Path,
        issuer: String,
    ) -> Result<Self> {
        let signing_key = signing::load_ed25519_from_path(signing_key_file)?;
        let salt_secret = signing::load_pairwise_salt_secret(pairwise_salt_file)?;
        let pairwise_salt = zeroship_core::auth::derive_pairwise_salt(&salt_secret);
        Self::from_signing_key(&signing_key, pairwise_salt, issuer)
    }

    /// Build an issuer from an already-loaded Ed25519 key and derived pairwise salt.
    pub fn from_signing_key(
        signing_key: &ed25519_dalek::SigningKey,
        pairwise_salt: [u8; 32],
        issuer: String,
    ) -> Result<Self> {
        use ed25519_dalek::pkcs8::EncodePrivateKey;

        let issuer = issuer.trim_end_matches('/').to_string();
        if issuer.is_empty() {
            return Err(AuthError::Config("AUTH_PUBLIC_URL / issuer is empty".into()));
        }
        let private_der = signing_key
            .to_pkcs8_der()
            .map_err(|e| AuthError::Internal(format!("PKCS#8 encode: {e}")))?
            .as_bytes()
            .to_vec();
        let kid = signing::jwk_thumbprint(signing_key);
        let public_jwk = signing::public_jwk(&signing_key.verifying_key(), &kid);
        Ok(Self {
            private_der,
            kid,
            issuer,
            pairwise_salt,
            public_jwk,
            broker_secrets: None,
        })
    }

    /// Attach validated broker master secrets used by brokered clients on the
    /// authorization-code grant.
    #[must_use]
    pub fn with_broker_secrets(mut self, broker_secrets: BrokerSecrets) -> Self {
        self.broker_secrets = Some(broker_secrets);
        self
    }

    /// Reconcile the file-backed public key into `zeroship.signing_keys`.
    ///
    /// The table stores only public JWK metadata. The private key stays in this
    /// process from `AUTH_SIGNING_KEY_FILE`.
    pub async fn publish_active_key(&self, db: &Client) -> Result<()> {
        with_advisory_lock(db, OP_SIGNING_KEY_BOOTSTRAP_LOCK, || async {
            self.publish_active_key_locked(db).await
        })
        .await
    }

    async fn publish_active_key_locked(&self, db: &Client) -> Result<()> {
        db.execute("BEGIN", &[])
            .await
            .map_err(|e| AuthError::Db(format!("signing_keys bootstrap begin: {e}")))?;

        let result = self.reconcile_active_key(db).await;
        match result {
            Ok(()) => {
                db.execute("COMMIT", &[]).await.map_err(|e| {
                    AuthError::Db(format!("signing_keys bootstrap commit: {e}"))
                })?;
                Ok(())
            }
            Err(err) => {
                if let Err(rollback) = db.execute("ROLLBACK", &[]).await {
                    tracing::error!(error = %rollback, "signing_keys bootstrap rollback failed");
                }
                Err(err)
            }
        }
    }

    async fn reconcile_active_key(&self, db: &Client) -> Result<()> {
        let rows = db
            .query(
                "SELECT alg, public_jwk, status \
                 FROM zeroship.signing_keys \
                 WHERE kid = $1",
                &[&self.kid],
            )
            .await
            .map_err(|e| AuthError::Db(format!("select signing key {}: {e}", self.kid)))?;

        if let Some(row) = rows.first() {
            let alg: String = row.get("alg");
            let public_jwk: serde_json::Value = row.get("public_jwk");
            if alg != "EdDSA" {
                return Err(AuthError::Config(format!(
                    "signing_keys row {} has alg {alg:?}, expected EdDSA",
                    self.kid
                )));
            }
            if public_jwk != self.public_jwk {
                return Err(AuthError::Config(format!(
                    "signing_keys row {} public_jwk does not match AUTH_SIGNING_KEY_FILE",
                    self.kid
                )));
            }
            db.execute(
                "UPDATE zeroship.signing_keys \
                 SET status = 'active', activated_at = COALESCE(activated_at, NOW()) \
                 WHERE kid = $1 AND status <> 'active'",
                &[&self.kid],
            )
            .await
            .map_err(|e| AuthError::Db(format!("activate signing key {}: {e}", self.kid)))?;
        } else {
            db.execute(
                "INSERT INTO zeroship.signing_keys \
                    (kid, alg, public_jwk, status, activated_at) \
                 VALUES ($1, 'EdDSA', $2, 'active', NOW())",
                &[&self.kid, &self.public_jwk],
            )
            .await
            .map_err(|e| AuthError::Db(format!("insert signing key {}: {e}", self.kid)))?;
        }

        db.execute(
            "UPDATE zeroship.signing_keys \
             SET status = 'retiring', retiring_at = COALESCE(retiring_at, NOW()) \
             WHERE status = 'active' AND kid <> $1",
            &[&self.kid],
        )
        .await
        .map_err(|e| AuthError::Db(format!("retire replaced signing keys: {e}")))?;

        Ok(())
    }

    /// Issue an RFC 9068 JWT access token.
    pub fn issue_access_token(&self, mint: &AccessTokenMint<'_>) -> Result<String> {
        let subject = self.pairwise_subject(mint.user_id, mint.sector);
        self.issue_access_token_with_subject(
            &subject,
            mint.audience,
            mint.client_id,
            mint.scopes,
            mint.ttl_secs,
        )
    }

    /// Issue an RFC 9068 access token for a platform principal. This is used by
    /// first-party resource servers such as control where `sub` is the global
    /// principal UUID, not an end-user pairwise app subject.
    pub fn issue_principal_access_token(
        &self,
        mint: &PrincipalAccessTokenMint<'_>,
    ) -> Result<String> {
        self.issue_access_token_with_subject(
            mint.principal_id,
            mint.audience,
            mint.client_id,
            mint.scopes,
            mint.ttl_secs,
        )
    }

    fn issue_access_token_with_subject(
        &self,
        subject: &str,
        audience: &str,
        client_id: &str,
        scopes: &[String],
        ttl_secs: Option<i64>,
    ) -> Result<String> {
        if subject.trim().is_empty() {
            return Err(AuthError::Internal("missing access-token subject".into()));
        }
        if audience.is_empty() {
            return Err(AuthError::Internal("missing access-token audience".into()));
        }
        if audience == client_id {
            return Err(AuthError::Internal(
                "access-token aud must be a resource audience, not client_id".into(),
            ));
        }
        if client_id.is_empty() {
            return Err(AuthError::Internal("missing access-token client_id".into()));
        }

        let now = unix_timestamp()?;
        let ttl = ttl_secs.unwrap_or(ACCESS_TOKEN_TTL_SECS);
        let claims = AccessTokenClaims {
            iss: self.issuer.clone(),
            sub: subject.to_string(),
            aud: audience.to_string(),
            exp: now + ttl,
            iat: now,
            jti: new_jti(),
            client_id: client_id.to_string(),
            scope: scopes.join(" "),
        };

        let mut header = Header::new(Algorithm::EdDSA);
        header.typ = Some(ACCESS_TOKEN_TYP.into());
        header.kid = Some(self.kid.clone());
        let key = EncodingKey::from_ed_der(&self.private_der);
        encode(&header, &claims, &key).map_err(|e| AuthError::Internal(format!("jwt encode: {e}")))
    }

    /// Issue an OIDC Core ID token paired with an access token.
    pub fn issue_id_token(&self, mint: &IdTokenMint<'_>) -> Result<String> {
        let subject = self.pairwise_subject(mint.user_id, mint.sector);
        self.issue_id_token_with_subject(
            &subject,
            mint.client_id,
            mint.nonce,
            mint.access_token,
            mint.auth_time,
            mint.amr,
            mint.acr,
            mint.email,
            mint.email_verified,
            mint.name,
            mint.picture,
            mint.ttl_secs,
        )
    }

    /// Issue an OIDC Core ID token for a platform principal. This is used only
    /// by gateway-brokered login after the broker secret has authenticated the
    /// code exchange; app-facing access tokens remain pairwise.
    pub fn issue_principal_id_token(&self, mint: &PrincipalIdTokenMint<'_>) -> Result<String> {
        self.issue_id_token_with_subject(
            mint.principal_id,
            mint.client_id,
            mint.nonce,
            mint.access_token,
            mint.auth_time,
            mint.amr,
            mint.acr,
            mint.email,
            mint.email_verified,
            mint.name,
            mint.picture,
            mint.ttl_secs,
        )
    }

    fn issue_id_token_with_subject(
        &self,
        subject: &str,
        client_id: &str,
        nonce: &str,
        access_token: &str,
        auth_time: Option<i64>,
        amr: Option<&[String]>,
        acr: Option<&str>,
        email: Option<&str>,
        email_verified: Option<bool>,
        name: Option<&str>,
        picture: Option<&str>,
        ttl_secs: Option<i64>,
    ) -> Result<String> {
        if subject.trim().is_empty() {
            return Err(AuthError::Internal("missing id-token subject".into()));
        }
        if client_id.is_empty() {
            return Err(AuthError::Internal("missing id-token aud/client_id".into()));
        }
        if nonce.is_empty() {
            return Err(AuthError::Internal("missing id-token nonce".into()));
        }
        if access_token.is_empty() {
            return Err(AuthError::Internal("missing paired access token".into()));
        }

        let now = unix_timestamp()?;
        let ttl = ttl_secs.unwrap_or(ID_TOKEN_TTL_SECS);
        let claims = IdTokenClaims {
            iss: self.issuer.clone(),
            sub: subject.to_string(),
            aud: client_id.to_string(),
            exp: now + ttl,
            iat: now,
            nonce: nonce.to_string(),
            at_hash: oidc_at_hash(access_token),
            auth_time,
            amr: amr.map(<[String]>::to_vec),
            acr: acr.map(str::to_string),
            email: email.map(str::to_string),
            email_verified,
            name: name.map(str::to_string),
            picture: picture.map(str::to_string),
        };

        let mut header = Header::new(Algorithm::EdDSA);
        header.typ = Some(ID_TOKEN_TYP.into());
        header.kid = Some(self.kid.clone());
        let key = EncodingKey::from_ed_der(&self.private_der);
        encode(&header, &claims, &key).map_err(|e| AuthError::Internal(format!("jwt encode: {e}")))
    }

    /// Verify this OP's own RFC 9068 JWT access token.
    pub fn verify_access_token(&self, token: &str) -> Result<AccessTokenClaims> {
        let header = decode_header(token)
            .map_err(|e| AuthError::Internal(format!("access-token header decode: {e}")))?;
        if header.typ.as_deref() != Some(ACCESS_TOKEN_TYP) {
            return Err(AuthError::Internal("access-token typ mismatch".into()));
        }
        if header.alg != Algorithm::EdDSA {
            return Err(AuthError::Internal("access-token alg mismatch".into()));
        }

        let x = self
            .public_jwk
            .get("x")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| AuthError::Internal("OP public JWK missing x".into()))?;
        let decoding_key = DecodingKey::from_ed_components(x)
            .map_err(|e| AuthError::Internal(format!("access-token public key: {e}")))?;
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.algorithms = vec![Algorithm::EdDSA];
        validation.set_issuer(&[self.issuer.as_str()]);
        validation.validate_aud = false;
        validation.validate_nbf = true;
        validation.leeway = 0;
        validation.required_spec_claims = [
            "exp",
            "iss",
            "aud",
            "sub",
            "iat",
            "jti",
            "client_id",
            "scope",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<HashSet<_>>();

        let data = decode::<AccessTokenClaims>(token, &decoding_key, &validation)
            .map_err(|e| AuthError::Internal(format!("access-token verify: {e}")))?;
        if data.claims.iss != self.issuer {
            return Err(AuthError::Internal("access-token issuer mismatch".into()));
        }
        Ok(data.claims)
    }

    /// Derive the app/sector pairwise subject with the issuer's loaded salt.
    #[must_use]
    pub fn pairwise_subject(&self, user_id: &str, sector: &str) -> String {
        zeroship_core::auth::derive_pairwise(&self.pairwise_salt, user_id, sector)
    }

    /// Verify a brokered client's presented derived broker secret.
    pub fn verify_broker_secret(&self, client_id: &str, presented: &str) -> Result<bool> {
        let Some(secrets) = self.broker_secrets.as_ref() else {
            return Err(AuthError::Config(
                "AUTH_BROKER_SECRET_FILE is required for brokered clients".into(),
            ));
        };
        Ok(secrets.verify_client_secret(client_id, presented))
    }

    #[must_use]
    pub fn kid(&self) -> &str {
        &self.kid
    }

    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    #[must_use]
    pub fn public_jwk(&self) -> &serde_json::Value {
        &self.public_jwk
    }
}

/// OIDC `at_hash` for EdDSA: SHA-512, leftmost 256 bits, base64url no padding.
#[must_use]
pub fn oidc_at_hash(access_token: &str) -> String {
    let digest = Sha512::digest(access_token.as_bytes());
    URL_SAFE_NO_PAD.encode(&digest[..32])
}

fn unix_timestamp() -> Result<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| AuthError::Internal(format!("system clock before Unix epoch: {e}")))?;
    i64::try_from(duration.as_secs())
        .map_err(|_| AuthError::Internal("system clock timestamp overflow".into()))
}

fn new_jti() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}
