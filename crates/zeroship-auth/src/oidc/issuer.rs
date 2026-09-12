//! Platform JWT issuer for RFC 9068 access tokens and OIDC ID tokens.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Utc};
use compio_postgres::{Client, GenericClient};
use jsonwebtoken::{
    decode, decode_header, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
use zeroship_core::device_grant::PLATFORM_TOKEN_MAX_TTL_SECS;
use zeroship_core::UserId;

use crate::advisory_lock::{with_advisory_lock, OP_SIGNING_KEY_BOOTSTRAP_LOCK};
use crate::error::{AuthError, Result};
use crate::oidc::signing;
use crate::session_store::ValidatedSession;

/// RFC 9068 access-token type header.
pub const ACCESS_TOKEN_TYP: &str = "at+jwt";
/// OIDC ID-token type header. OIDC permits omitting it; zeroship stamps it.
pub const ID_TOKEN_TYP: &str = "JWT";
/// OIDC Back-Channel Logout token type header.
pub const LOGOUT_TOKEN_TYP: &str = "logout+jwt";
/// Default short-lived platform access token lifetime.
pub const ACCESS_TOKEN_TTL_SECS: i64 = 15 * 60;
/// Default ID-token lifetime; not longer than the paired access token.
pub const ID_TOKEN_TTL_SECS: i64 = 15 * 60;
/// Short logout-token lifetime. The BCL spec recommends at most two minutes.
pub const LOGOUT_TOKEN_TTL_SECS: i64 = 2 * 60;

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
    pub sid: String,
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

/// OIDC Back-Channel Logout 1.0 logout-token claims.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogoutTokenClaims {
    pub iss: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub: Option<String>,
    pub aud: String,
    pub iat: i64,
    pub exp: i64,
    pub jti: String,
    pub events: BTreeMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
}

/// Inputs for minting an access token.
#[derive(Debug, Clone)]
pub struct AccessTokenMint<'a> {
    pub user_id: &'a UserId,
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
    pub principal_id: &'a UserId,
    pub audience: &'a str,
    pub client_id: &'a str,
    pub scopes: &'a [String],
    pub ttl_secs: Option<i64>,
}

/// Inputs for minting an ID token whose subject is the canonical platform
/// principal id instead of an app-sector pairwise subject.
#[derive(Debug, Clone)]
pub struct PrincipalIdTokenMint<'a> {
    pub principal_id: &'a UserId,
    pub client_id: &'a str,
    pub sid: &'a str,
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
    pub user_id: &'a UserId,
    pub sector: &'a str,
    pub client_id: &'a str,
    pub sid: &'a str,
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

/// Inputs for minting an OIDC Back-Channel Logout token.
#[derive(Debug, Clone)]
pub struct LogoutTokenMint<'a> {
    pub client_id: &'a str,
    pub sub: Option<&'a str>,
    pub sid: Option<&'a str>,
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
        zeroship_core::auth::validate_broker_master(&current).map_err(AuthError::Config)?;
        if let Some(previous) = previous.as_ref() {
            zeroship_core::auth::validate_broker_master(previous).map_err(AuthError::Config)?;
        }
        Ok(Self { current, previous })
    }

    /// Load and validate broker master secrets from owner-only files.
    pub fn from_files(current_file: &Path, previous_file: Option<&Path>) -> Result<Self> {
        let current = signing::load_broker_master_secret(current_file, "AUTH_BROKER_SECRET_FILE")?;
        let previous = previous_file
            .map(|path| {
                signing::load_broker_master_secret(path, "AUTH_BROKER_SECRET_PREVIOUS_FILE")
            })
            .transpose()?;
        Self::new(current, previous)
    }

    /// Constant-time check of a presented per-client broker secret against the
    /// current and rotation-window previous master secrets.
    #[must_use]
    pub fn verify_client_secret(&self, client_id: &str, presented: &str) -> bool {
        let current = zeroship_core::auth::derive_broker_secret(&self.current, client_id);
        let current_ok = zeroship_core::auth::constant_time_eq(presented, &current);
        let previous_ok = self.previous.as_ref().is_some_and(|previous| {
            let expected = zeroship_core::auth::derive_broker_secret(previous, client_id);
            zeroship_core::auth::constant_time_eq(presented, &expected)
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

/// A token held behind the persisted-expiry fence; never return it directly.
struct SignedJwt {
    token: String,
    expires_at: i64,
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
            return Err(AuthError::Config(
                "ZEROSHIP_AUTH_PUBLIC_URL / issuer is empty".into(),
            ));
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
    /// process from `AUTH_SIGNING_KEY_FILE`. A `retiring` or `retired` row is
    /// not reactivated, which fences a stale signer that restarts during or
    /// after rotation. An already-running retiring signer remains safe because
    /// every returned token advances the row's maximum issued expiry.
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
                db.execute("COMMIT", &[])
                    .await
                    .map_err(|e| AuthError::Db(format!("signing_keys bootstrap commit: {e}")))?;
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
        // Activation is one conditional write so terminal retirement cannot
        // win between a status read and this process reactivating the row.
        let activated = db
            .query(
                "UPDATE zeroship.signing_keys \
                 SET status = 'active', activated_at = COALESCE(activated_at, NOW()) \
                 WHERE kid = $1 \
                   AND alg = 'EdDSA' \
                   AND public_jwk = $2 \
                   AND status IN ('active', 'next') \
                 RETURNING kid",
                &[&self.kid, &self.public_jwk],
            )
            .await
            .map_err(|e| AuthError::Db(format!("activate signing key {}: {e}", self.kid)))?;

        if activated.is_empty() {
            let rows = db
                .query(
                    "SELECT alg, public_jwk, status \
                     FROM zeroship.signing_keys \
                     WHERE kid = $1",
                    &[&self.kid],
                )
                .await
                .map_err(|e| AuthError::Db(format!("select signing key {}: {e}", self.kid)))?;
            let Some(row) = rows.first() else {
                db.execute(
                    "INSERT INTO zeroship.signing_keys \
                        (kid, alg, public_jwk, status, activated_at) \
                     VALUES ($1, 'EdDSA', $2, 'active', NOW())",
                    &[&self.kid, &self.public_jwk],
                )
                .await
                .map_err(|e| AuthError::Db(format!("insert signing key {}: {e}", self.kid)))?;
                return self.retire_replaced_signing_keys(db).await;
            };
            let alg: String = row.get("alg");
            let public_jwk: serde_json::Value = row.get("public_jwk");
            let status: String = row.get("status");
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
            if status == "retired" {
                return Err(AuthError::Config(format!(
                    "signing key {} is terminally retired and cannot be reactivated",
                    self.kid
                )));
            }
            return Err(AuthError::Config(format!(
                "signing key {} has non-activatable status {status:?}",
                self.kid
            )));
        }

        self.retire_replaced_signing_keys(db).await
    }

    async fn retire_replaced_signing_keys(&self, db: &Client) -> Result<()> {
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

    /// Issue an RFC 9068 JWT access token and reserve its expiry before return.
    ///
    /// `proof` is MINT-READS-ROW as a type. A [`ValidatedSession`] cannot be
    /// constructed outside `crate::session_store`, so a caller holding one has
    /// executed the statement that enforced liveness, expiry, the grant's
    /// status and the person's credential epoch. Deleting the parameter does
    /// not weaken a check - it fails the build at every call site, which is why
    /// this is a witness and not an assertion.
    #[allow(clippy::future_not_send)]
    pub async fn issue_access_token(
        &self,
        db: &(impl GenericClient + ?Sized),
        mint: &AccessTokenMint<'_>,
        proof: &ValidatedSession,
    ) -> Result<String> {
        bind_proof_to_person(proof, mint.user_id)?;
        validate_registered_ttl(
            mint.ttl_secs.unwrap_or(ACCESS_TOKEN_TTL_SECS),
            "access token",
        )?;
        let subject = self.pairwise_subject(mint.user_id, mint.sector);
        let signed = self.build_access_token_with_subject(
            &subject,
            mint.audience,
            mint.client_id,
            mint.scopes,
            mint.ttl_secs,
        )?;
        self.register_signed_token(db, signed).await
    }

    /// Issue an RFC 9068 access token for a platform principal. This is used by
    /// first-party resource servers such as control where `sub` is the global
    /// principal id, not an end-user pairwise app subject.
    #[allow(clippy::future_not_send)]
    pub async fn issue_principal_access_token(
        &self,
        db: &(impl GenericClient + ?Sized),
        mint: &PrincipalAccessTokenMint<'_>,
        proof: &ValidatedSession,
    ) -> Result<String> {
        bind_proof_to_person(proof, mint.principal_id)?;
        validate_registered_ttl(
            mint.ttl_secs.unwrap_or(ACCESS_TOKEN_TTL_SECS),
            "principal access token",
        )?;
        let signed = self.build_access_token_with_subject(
            mint.principal_id.as_str(),
            mint.audience,
            mint.client_id,
            mint.scopes,
            mint.ttl_secs,
        )?;
        self.register_signed_token(db, signed).await
    }

    fn build_access_token_with_subject(
        &self,
        subject: &str,
        audience: &str,
        client_id: &str,
        scopes: &[String],
        ttl_secs: Option<i64>,
    ) -> Result<SignedJwt> {
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
        let expires_at = now
            .checked_add(ttl)
            .ok_or_else(|| AuthError::Internal("access-token expiry overflow".into()))?;
        let claims = AccessTokenClaims {
            iss: self.issuer.clone(),
            sub: subject.to_string(),
            aud: audience.to_string(),
            exp: expires_at,
            iat: now,
            jti: new_jti(),
            client_id: client_id.to_string(),
            scope: scopes.join(" "),
        };

        let mut header = Header::new(Algorithm::EdDSA);
        header.typ = Some(ACCESS_TOKEN_TYP.into());
        header.kid = Some(self.kid.clone());
        let key = EncodingKey::from_ed_der(&self.private_der);
        let token = encode(&header, &claims, &key)
            .map_err(|e| AuthError::Internal(format!("jwt encode: {e}")))?;
        Ok(SignedJwt { token, expires_at })
    }

    /// Issue an OIDC Core ID token and reserve its expiry before return.
    #[allow(clippy::future_not_send)]
    pub async fn issue_id_token(
        &self,
        db: &(impl GenericClient + ?Sized),
        mint: &IdTokenMint<'_>,
        proof: &ValidatedSession,
    ) -> Result<String> {
        bind_proof_to_person(proof, mint.user_id)?;
        validate_registered_ttl(mint.ttl_secs.unwrap_or(ID_TOKEN_TTL_SECS), "ID token")?;
        let subject = self.pairwise_subject(mint.user_id, mint.sector);
        let signed = self.build_id_token_with_subject(
            &subject,
            mint.client_id,
            mint.sid,
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
        )?;
        self.register_signed_token(db, signed).await
    }

    /// Issue an OIDC Core ID token for a platform principal. This is used only
    /// by gateway-brokered login after the broker secret has authenticated the
    /// code exchange; app-facing access tokens remain pairwise.
    #[allow(clippy::future_not_send)]
    pub async fn issue_principal_id_token(
        &self,
        db: &(impl GenericClient + ?Sized),
        mint: &PrincipalIdTokenMint<'_>,
        proof: &ValidatedSession,
    ) -> Result<String> {
        bind_proof_to_person(proof, mint.principal_id)?;
        validate_registered_ttl(
            mint.ttl_secs.unwrap_or(ID_TOKEN_TTL_SECS),
            "principal ID token",
        )?;
        let signed = self.build_id_token_with_subject(
            mint.principal_id.as_str(),
            mint.client_id,
            mint.sid,
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
        )?;
        self.register_signed_token(db, signed).await
    }

    // Private helper shared by `issue_id_token` / `issue_principal_id_token`,
    // both of which already destructure a `*Mint` struct before calling this.
    // The two callers disagree on which mint field maps to `subject`, so
    // collapsing this back into one mint-struct parameter is a real
    // refactor, not a mechanical lint fix - not doing that as part of a lint
    // sweep.
    #[allow(clippy::too_many_arguments)]
    fn build_id_token_with_subject(
        &self,
        subject: &str,
        client_id: &str,
        sid: &str,
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
    ) -> Result<SignedJwt> {
        if subject.trim().is_empty() {
            return Err(AuthError::Internal("missing id-token subject".into()));
        }
        if client_id.is_empty() {
            return Err(AuthError::Internal("missing id-token aud/client_id".into()));
        }
        if sid.trim().is_empty() {
            return Err(AuthError::Internal("missing id-token sid".into()));
        }
        if nonce.is_empty() {
            return Err(AuthError::Internal("missing id-token nonce".into()));
        }
        if access_token.is_empty() {
            return Err(AuthError::Internal("missing paired access token".into()));
        }

        let now = unix_timestamp()?;
        let ttl = ttl_secs.unwrap_or(ID_TOKEN_TTL_SECS);
        let expires_at = now
            .checked_add(ttl)
            .ok_or_else(|| AuthError::Internal("id-token expiry overflow".into()))?;
        let claims = IdTokenClaims {
            iss: self.issuer.clone(),
            sub: subject.to_string(),
            aud: client_id.to_string(),
            exp: expires_at,
            iat: now,
            sid: sid.to_string(),
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
        let token = encode(&header, &claims, &key)
            .map_err(|e| AuthError::Internal(format!("jwt encode: {e}")))?;
        Ok(SignedJwt { token, expires_at })
    }

    /// Issue a logout token and reserve its expiry before return.
    ///
    /// **This is the one mint on the issuer that takes no
    /// [`ValidatedSession`], and the omission is the argument rather than an
    /// oversight.** A logout token authorises nothing: it is a notification
    /// that a session ENDED, addressed to a relying party, and it is minted at
    /// revocation time when by construction no live session row remains to
    /// validate. Requiring a witness here would mean either revoking after
    /// notifying - which loses the notification when the revoke fails - or
    /// minting a proof from a row that is already dead, which is the property
    /// the witness exists to deny. Every mint that hands a SUBJECT a credential
    /// takes one.
    #[allow(clippy::future_not_send)]
    pub async fn issue_logout_token(
        &self,
        db: &(impl GenericClient + ?Sized),
        mint: &LogoutTokenMint<'_>,
    ) -> Result<String> {
        validate_registered_ttl(
            mint.ttl_secs.unwrap_or(LOGOUT_TOKEN_TTL_SECS),
            "logout token",
        )?;
        let signed = self.build_logout_token(mint)?;
        self.register_signed_token(db, signed).await
    }

    fn build_logout_token(&self, mint: &LogoutTokenMint<'_>) -> Result<SignedJwt> {
        if mint.client_id.trim().is_empty() {
            return Err(AuthError::Internal(
                "missing logout-token aud/client_id".into(),
            ));
        }
        if mint.sub.is_none() && mint.sid.is_none() {
            return Err(AuthError::Internal(
                "logout-token requires sub, sid, or both".into(),
            ));
        }
        if mint.sub.is_some_and(|sub| sub.trim().is_empty()) {
            return Err(AuthError::Internal("empty logout-token sub".into()));
        }
        if mint.sid.is_some_and(|sid| sid.trim().is_empty()) {
            return Err(AuthError::Internal("empty logout-token sid".into()));
        }

        let now = unix_timestamp()?;
        let ttl = mint.ttl_secs.unwrap_or(LOGOUT_TOKEN_TTL_SECS);
        let expires_at = now
            .checked_add(ttl)
            .ok_or_else(|| AuthError::Internal("logout-token expiry overflow".into()))?;
        let mut events = BTreeMap::new();
        events.insert(
            zeroship_core::logout_token::BCL_EVENT.to_string(),
            serde_json::json!({}),
        );
        let claims = LogoutTokenClaims {
            iss: self.issuer.clone(),
            sub: mint.sub.map(str::to_string),
            aud: mint.client_id.to_string(),
            iat: now,
            exp: expires_at,
            jti: new_jti(),
            events,
            sid: mint.sid.map(str::to_string),
        };

        let mut header = Header::new(Algorithm::EdDSA);
        header.typ = Some(LOGOUT_TOKEN_TYP.into());
        header.kid = Some(self.kid.clone());
        let key = EncodingKey::from_ed_der(&self.private_der);
        let token = encode(&header, &claims, &key)
            .map_err(|e| AuthError::Internal(format!("jwt encode: {e}")))?;
        Ok(SignedJwt { token, expires_at })
    }

    #[allow(clippy::future_not_send)]
    async fn register_signed_token(
        &self,
        db: &(impl GenericClient + ?Sized),
        signed: SignedJwt,
    ) -> Result<String> {
        let expires_at =
            DateTime::<Utc>::from_timestamp(signed.expires_at, 0).ok_or_else(|| {
                AuthError::Internal("signed-token expiry is outside PostgreSQL range".into())
            })?;
        let updated = db
            .execute(
                "UPDATE zeroship.signing_keys \
                 SET max_issued_expires_at = GREATEST( \
                     COALESCE(max_issued_expires_at, $2), $2 \
                 ) \
                 WHERE kid = $1 AND status IN ('active', 'retiring')",
                &[&self.kid, &expires_at],
            )
            .await
            .map_err(|err| {
                AuthError::Db(format!(
                    "reserve signing key {} issued expiry: {err}",
                    self.kid
                ))
            })?;
        if updated != 1 {
            return Err(AuthError::Internal(format!(
                "signing key {} is no longer trusted for issuance",
                self.kid
            )));
        }
        Ok(signed.token)
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
    pub fn pairwise_subject(&self, user_id: &UserId, sector: &str) -> String {
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

/// Refuse a mint whose subject is not the person the witness was minted for.
///
/// The witness alone says "SOME session was validated". This makes it say "THIS
/// person's session was validated", which is what MINT-READS-ROW means: a
/// caller holding a proof for one session must not be able to mint a credential
/// naming another person. It is a runtime check because the identifier crosses
/// the boundary as a string - the type says a read happened, this says what the
/// read was about, and both are needed.
///
/// Every live call site passes the session's own person id, so a failure here
/// is a programming error rather than a request-shaped one, and it is reported
/// as an internal error without naming either identifier.
fn bind_proof_to_person(proof: &ValidatedSession, minting_for: &UserId) -> Result<()> {
    if proof.person_id() == minting_for {
        return Ok(());
    }
    tracing::error!(
        session_id = proof.session_id(),
        "mint subject does not match the validated session's person"
    );
    Err(AuthError::Internal(
        "mint subject does not match the validated session".into(),
    ))
}

fn validate_registered_ttl(ttl_secs: i64, token_kind: &str) -> Result<()> {
    if ttl_secs <= 0 || ttl_secs > PLATFORM_TOKEN_MAX_TTL_SECS {
        return Err(AuthError::Internal(format!(
            "{token_kind} ttl must be between 1 and {PLATFORM_TOKEN_MAX_TTL_SECS} seconds"
        )));
    }
    Ok(())
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

#[cfg(test)]
mod ttl_tests {
    use super::{validate_registered_ttl, ACCESS_TOKEN_TTL_SECS, PLATFORM_TOKEN_MAX_TTL_SECS};

    /// The longest an access token may live, in seconds.
    ///
    /// A LITERAL, never the constant under test. Every other assertion on this
    /// lifetime in the tree compares the wire value to `ACCESS_TOKEN_TTL_SECS`
    /// itself, which proves the plumbing and passes for any value at all: the
    /// constant was set to `12 * 60 * 60` and the whole live-database auth
    /// suite stayed green, byte-identical, at 645 passed.
    ///
    /// Why a bound rather than an equality. The invariant the design rests on
    /// is "the access token is not the session" - the session is the rotating,
    /// DB-backed refresh family, and this token only has to outlive one CLI
    /// operation. A ceiling states exactly that and leaves the number tunable;
    /// an equality would re-encode today's choice, and its failure message
    /// ("expected 900, got 1200") would tell a reader nothing about why 900
    /// mattered, so the cheapest way to green would be to edit the number here
    /// - which is how the tautology comes back.
    ///
    /// Why 30 minutes. A self-contained bearer cannot be recalled; the only
    /// early recall is the `zeroship.token_revocations` marker, so this
    /// lifetime is the window a leaked token still works if that marker is
    /// never written. Thirty minutes is generous for the longest single
    /// operation (a `zeroship deploy` upload) and 24x below the 12 hours it
    /// replaced. Anything expressing "a shift" or "a working day" is above it.
    const MAX_ACCESS_TOKEN_LIFETIME_SECS: i64 = 30 * 60;

    /// The shortest it may live. Derived, not taste: `crates/zeroship-cli/src/auth.rs`
    /// treats a credential as expired at `expires_at <= now + 60`
    /// (`TOKEN_EXPIRY_SKEW_SECS`), so a lifetime at or under that skew makes
    /// every freshly minted token already stale to the CLI and turns each
    /// command into a rotation.
    const MIN_ACCESS_TOKEN_LIFETIME_SECS: i64 = 120;

    // Both assertions below compare two constants, which is exactly what
    // `clippy::assertions_on_constants` exists to flag: an assertion the
    // compiler can fold is normally either dead or a build error waiting to
    // happen. That premise does not hold for a guard test. This one is
    // deliberately true today and exists to fail the moment someone edits
    // `ACCESS_TOKEN_TTL_SECS`, and the whole value of it is the message it
    // prints when that happens - which interpolates the offending value.
    // Clippy's suggested `const { assert!(..) }` cannot format, so taking the
    // suggestion would trade the diagnostic this test exists to deliver for
    // silence from the lint.
    #[allow(clippy::assertions_on_constants)]
    #[test]
    fn the_access_token_lifetime_stays_short_enough_to_expire_as_a_backstop() {
        assert!(
            ACCESS_TOKEN_TTL_SECS <= MAX_ACCESS_TOKEN_LIFETIME_SECS,
            "access token lives {ACCESS_TOKEN_TTL_SECS}s, ceiling is \
             {MAX_ACCESS_TOKEN_LIFETIME_SECS}s. A bearer this long-lived cannot be \
             called back; put the long life in the refresh family instead."
        );
        assert!(
            ACCESS_TOKEN_TTL_SECS >= MIN_ACCESS_TOKEN_LIFETIME_SECS,
            "access token lives {ACCESS_TOKEN_TTL_SECS}s, floor is \
             {MIN_ACCESS_TOKEN_LIFETIME_SECS}s. Below the CLI's 60s expiry skew a \
             fresh token is stale on arrival."
        );
    }

    #[test]
    fn registered_token_issuer_enforces_the_platform_ttl_ceiling() {
        for invalid in [0, -1, PLATFORM_TOKEN_MAX_TTL_SECS + 1] {
            assert!(
                validate_registered_ttl(invalid, "access token").is_err(),
                "registered issuer accepted ttl_secs={invalid}"
            );
        }
        assert!(validate_registered_ttl(PLATFORM_TOKEN_MAX_TTL_SECS, "access token").is_ok());
    }
}
