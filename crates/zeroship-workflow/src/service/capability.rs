//! Signed app and signal capabilities. Task authority is a separate protocol.

use crate::{validation, WorkflowServiceError};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::BTreeSet;
use zeroship_core::{
    app_id::AppId,
    service_assertion::{ServiceIssuer, ServiceSigningKey, ServiceTrustBundle},
    typed_id,
};

pub const WORKFLOW_AUDIENCE: &str = "spiffe://zeroship.ai/svc/workflow";
const CONTROL_ISSUER: &str = "spiffe://zeroship.ai/svc/control";
const APP_TYPE: &str = "zeroship-workflow-app+jwt";
const SIGNAL_TYPE: &str = "zeroship-workflow-signal+jwt";
pub const APP_CAPABILITY_MAX_LIFETIME_SECONDS: i64 = 300;
pub const SIGNAL_CAPABILITY_MAX_LIFETIME_SECONDS: i64 = 604_800;
const MAX_TOKEN_BYTES: usize = 16 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilityToken(String);
impl CapabilityToken {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Debug for CapabilityToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CapabilityToken([redacted])")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IssuedAppCapability {
    pub token: CapabilityToken,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AppOperation {
    Start,
    List,
    Status,
    Signal,
    Broadcast,
    Control,
    Restart,
    ReadOutput,
    IssueSignalToken,
    RevokeSignalTokens,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppGrant {
    pub app_id: AppId,
    pub operations: BTreeSet<AppOperation>,
}
impl AppGrant {
    pub fn authorize(
        &self,
        app: &AppId,
        operation: AppOperation,
    ) -> Result<(), WorkflowServiceError> {
        if &self.app_id != app {
            return Err(WorkflowServiceError::NotFound(
                "workflow app not found".into(),
            ));
        }
        if !self.operations.contains(&operation) {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        Ok(())
    }
    fn validate(&self) -> Result<(), WorkflowServiceError> {
        if self.operations.is_empty() {
            return Err(WorkflowServiceError::InvalidRequest(
                "workflow app capability has no operations".into(),
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum SignalTarget {
    Run { run_id: String },
    Topic { topic: String },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SignalGrant {
    pub app_id: AppId,
    pub target: SignalTarget,
    pub types: BTreeSet<String>,
    pub epoch: i64,
    pub app_epoch: i64,
}
impl SignalGrant {
    fn validate(&self) -> Result<(), WorkflowServiceError> {
        if self.types.is_empty() || self.types.len() > 32 || self.epoch < 0 || self.app_epoch < 0 {
            return Err(WorkflowServiceError::InvalidRequest(
                "invalid workflow signal capability scope".into(),
            ));
        }
        for name in &self.types {
            validation::signal_type(name)?;
        }
        match &self.target {
            SignalTarget::Run { run_id } => super::app::validate_run(run_id)?,
            SignalTarget::Topic { topic } => super::signals::validate_topic(topic)?,
        }
        Ok(())
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    alg: String,
    typ: String,
    kid: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Claims<G> {
    iss: String,
    aud: String,
    iat: i64,
    exp: i64,
    jti: String,
    grant: G,
}

pub fn mint_app_capability(
    key: &ServiceSigningKey,
    grant: AppGrant,
    now: i64,
    lifetime: i64,
) -> Result<CapabilityToken, WorkflowServiceError> {
    grant.validate()?;
    sign(
        key,
        CONTROL_ISSUER,
        APP_TYPE,
        grant,
        now,
        lifetime,
        APP_CAPABILITY_MAX_LIFETIME_SECONDS,
    )
}
pub fn verify_app_capability(
    token: &str,
    trust: &ServiceTrustBundle,
    now: i64,
) -> Result<AppGrant, WorkflowServiceError> {
    let grant: AppGrant = verify(
        token,
        trust,
        CONTROL_ISSUER,
        APP_TYPE,
        now,
        APP_CAPABILITY_MAX_LIFETIME_SECONDS,
    )?;
    grant
        .validate()
        .map_err(|_| WorkflowServiceError::Unauthenticated)?;
    Ok(grant)
}
pub fn mint_signal_capability(
    key: &ServiceSigningKey,
    grant: SignalGrant,
    now: i64,
    lifetime: i64,
) -> Result<CapabilityToken, WorkflowServiceError> {
    grant.validate()?;
    sign(
        key,
        WORKFLOW_AUDIENCE,
        SIGNAL_TYPE,
        grant,
        now,
        lifetime,
        SIGNAL_CAPABILITY_MAX_LIFETIME_SECONDS,
    )
}
pub fn verify_signal_capability(
    token: &str,
    trust: &ServiceTrustBundle,
    now: i64,
) -> Result<SignalGrant, WorkflowServiceError> {
    let grant: SignalGrant = verify(
        token,
        trust,
        WORKFLOW_AUDIENCE,
        SIGNAL_TYPE,
        now,
        SIGNAL_CAPABILITY_MAX_LIFETIME_SECONDS,
    )?;
    grant
        .validate()
        .map_err(|_| WorkflowServiceError::Unauthenticated)?;
    Ok(grant)
}
fn sign<G: Serialize>(
    key: &ServiceSigningKey,
    issuer: &str,
    kind: &str,
    grant: G,
    now: i64,
    lifetime: i64,
    max_lifetime: i64,
) -> Result<CapabilityToken, WorkflowServiceError> {
    if lifetime <= 0 || lifetime > max_lifetime || now < 0 {
        return Err(WorkflowServiceError::InvalidRequest(
            "workflow capability lifetime is out of range".into(),
        ));
    }
    let exp = now.checked_add(lifetime).ok_or_else(|| {
        WorkflowServiceError::InvalidRequest("workflow capability expiry overflow".into())
    })?;
    let header = Header {
        alg: "EdDSA".into(),
        typ: kind.into(),
        kid: key.key_id(),
    };
    let claims = Claims {
        iss: issuer.into(),
        aud: WORKFLOW_AUDIENCE.into(),
        iat: now,
        exp,
        jti: typed_id::generate(typed_id::WORKFLOW_CAPABILITY_PREFIX),
        grant,
    };
    let input = format!("{}.{}", encode(&header)?, encode(&claims)?);
    let signature = URL_SAFE_NO_PAD.encode(key.sign_detached(input.as_bytes()));
    let token = format!("{input}.{signature}");
    if token.len() > MAX_TOKEN_BYTES {
        return Err(WorkflowServiceError::PayloadTooLarge);
    }
    Ok(CapabilityToken(token))
}
fn encode<T: Serialize>(value: &T) -> Result<String, WorkflowServiceError> {
    Ok(URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(value)
            .map_err(|_| WorkflowServiceError::Internal("encode workflow capability".into()))?,
    ))
}
fn decode<T: DeserializeOwned>(value: &str) -> Result<T, WorkflowServiceError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| WorkflowServiceError::Unauthenticated)?;
    serde_json::from_slice(&bytes).map_err(|_| WorkflowServiceError::Unauthenticated)
}
fn verify<G: DeserializeOwned>(
    token: &str,
    trust: &ServiceTrustBundle,
    issuer: &str,
    kind: &str,
    now: i64,
    max_lifetime: i64,
) -> Result<G, WorkflowServiceError> {
    if token.is_empty() || token.len() > MAX_TOKEN_BYTES {
        return Err(WorkflowServiceError::Unauthenticated);
    }
    let mut parts = token.split('.');
    let header_text = parts.next().ok_or(WorkflowServiceError::Unauthenticated)?;
    let claims_text = parts.next().ok_or(WorkflowServiceError::Unauthenticated)?;
    let signature_text = parts.next().ok_or(WorkflowServiceError::Unauthenticated)?;
    if parts.next().is_some() {
        return Err(WorkflowServiceError::Unauthenticated);
    }
    let header: Header = decode(header_text)?;
    if header.alg != "EdDSA" || header.typ != kind {
        return Err(WorkflowServiceError::Unauthenticated);
    }
    let expected_issuer = ServiceIssuer::parse(issuer).map_err(|_| {
        WorkflowServiceError::Internal("invalid configured capability issuer".into())
    })?;
    let keys = trust.public_keys_for(&expected_issuer);
    let (_, key) = keys
        .iter()
        .find(|(kid, _)| *kid == header.kid)
        .ok_or(WorkflowServiceError::Unauthenticated)?;
    let key = VerifyingKey::from_bytes(key).map_err(|_| WorkflowServiceError::Unauthenticated)?;
    let signature = URL_SAFE_NO_PAD
        .decode(signature_text)
        .map_err(|_| WorkflowServiceError::Unauthenticated)?;
    let signature =
        Signature::from_slice(&signature).map_err(|_| WorkflowServiceError::Unauthenticated)?;
    key.verify_strict(
        format!("{header_text}.{claims_text}").as_bytes(),
        &signature,
    )
    .map_err(|_| WorkflowServiceError::Unauthenticated)?;
    let claims: Claims<G> = decode(claims_text)?;
    if claims.iss != issuer
        || claims.aud != WORKFLOW_AUDIENCE
        || claims.iat > now
        || claims.exp <= now
        || claims.iat < 0
        || claims
            .exp
            .checked_sub(claims.iat)
            .is_none_or(|lifetime| lifetime <= 0 || lifetime > max_lifetime)
    {
        return Err(WorkflowServiceError::Unauthenticated);
    }
    typed_id::parse_with_prefix(&claims.jti, typed_id::WORKFLOW_CAPABILITY_PREFIX)
        .map_err(|_| WorkflowServiceError::Unauthenticated)?;
    Ok(claims.grant)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn trust(issuer: &str, key: &ServiceSigningKey) -> ServiceTrustBundle {
        let mut bundle = ServiceTrustBundle::new();
        bundle
            .trust_signing_key(&ServiceIssuer::parse(issuer).unwrap(), key.key_id(), key)
            .unwrap();
        bundle
    }
    fn app_grant() -> AppGrant {
        AppGrant {
            app_id: AppId::mint(),
            operations: [AppOperation::Start, AppOperation::Status].into(),
        }
    }
    fn changed_claims(
        token: &CapabilityToken,
        key: &ServiceSigningKey,
        edit: impl FnOnce(&mut Value),
    ) -> String {
        let parts: Vec<_> = token.as_str().split('.').collect();
        let mut claims: Value = decode(parts[1]).unwrap();
        edit(&mut claims);
        let input = format!("{}.{}", parts[0], encode(&claims).unwrap());
        format!(
            "{input}.{}",
            URL_SAFE_NO_PAD.encode(key.sign_detached(input.as_bytes()))
        )
    }

    #[test]
    fn app_capabilities_bind_issuer_audience_app_and_operations() {
        let control = ServiceSigningKey::generate();
        let worker = ServiceSigningKey::generate();
        let bundle = trust(CONTROL_ISSUER, &control);
        let grant = app_grant();
        let token = mint_app_capability(&control, grant.clone(), 1000, 60).unwrap();
        let verified = verify_app_capability(token.as_str(), &bundle, 1001).unwrap();
        assert_eq!(verified, grant);
        verified
            .authorize(&grant.app_id, AppOperation::Start)
            .unwrap();
        assert!(matches!(
            verified.authorize(&AppId::mint(), AppOperation::Start),
            Err(WorkflowServiceError::NotFound(_))
        ));
        assert_eq!(
            verified.authorize(&grant.app_id, AppOperation::Restart),
            Err(WorkflowServiceError::PermissionDenied)
        );
        let forged = mint_app_capability(&worker, grant, 1000, 60).unwrap();
        assert_eq!(
            verify_app_capability(forged.as_str(), &bundle, 1001),
            Err(WorkflowServiceError::Unauthenticated)
        );
        let wrong_audience = changed_claims(&token, &control, |claims| {
            claims["aud"] = json!("spiffe://zeroship.ai/svc/worker")
        });
        assert_eq!(
            verify_app_capability(&wrong_audience, &bundle, 1001),
            Err(WorkflowServiceError::Unauthenticated)
        );
        let wrong_issuer = changed_claims(&token, &control, |claims| {
            claims["iss"] = json!(WORKFLOW_AUDIENCE)
        });
        assert_eq!(
            verify_app_capability(&wrong_issuer, &bundle, 1001),
            Err(WorkflowServiceError::Unauthenticated)
        );
        assert_eq!(
            verify_app_capability(token.as_str(), &ServiceTrustBundle::new(), 1001),
            Err(WorkflowServiceError::Unauthenticated)
        );
        assert!(!format!("{token:?}").contains(token.as_str()));
    }

    #[test]
    fn signed_claims_still_require_valid_time_and_closed_shapes() {
        let key = ServiceSigningKey::generate();
        let bundle = trust(CONTROL_ISSUER, &key);
        let token = mint_app_capability(&key, app_grant(), 1000, 60).unwrap();
        assert!(verify_app_capability(token.as_str(), &bundle, 999).is_err());
        assert!(verify_app_capability(token.as_str(), &bundle, 1060).is_err());
        assert!(mint_app_capability(
            &key,
            app_grant(),
            1000,
            APP_CAPABILITY_MAX_LIFETIME_SECONDS + 1
        )
        .is_err());
        for (field, value) in [
            ("exp", json!(2000)),
            ("iat", json!(1002)),
            ("jti", json!("invalid")),
            ("unexpected", json!(true)),
        ] {
            let changed = changed_claims(&token, &key, |claims| claims[field] = value);
            assert_eq!(
                verify_app_capability(&changed, &bundle, 1001),
                Err(WorkflowServiceError::Unauthenticated)
            );
        }
        let changed = changed_claims(&token, &key, |claims| {
            claims["grant"]["operations"] = json!(["poll"])
        });
        assert_eq!(
            verify_app_capability(&changed, &bundle, 1001),
            Err(WorkflowServiceError::Unauthenticated)
        );
        let parts: Vec<_> = token.as_str().split('.').collect();
        let wrong_signature = format!(
            "{}.{}.{}",
            parts[0],
            parts[1],
            URL_SAFE_NO_PAD.encode([0; 64])
        );
        assert_eq!(
            verify_app_capability(&wrong_signature, &bundle, 1001),
            Err(WorkflowServiceError::Unauthenticated)
        );
    }

    #[test]
    fn signal_capabilities_have_a_distinct_authority_and_purpose() {
        let control = ServiceSigningKey::generate();
        let workflow = ServiceSigningKey::generate();
        let mut bundle = trust(WORKFLOW_AUDIENCE, &workflow);
        bundle
            .trust_signing_key(
                &ServiceIssuer::parse(CONTROL_ISSUER).unwrap(),
                control.key_id(),
                &control,
            )
            .unwrap();
        let grant = SignalGrant {
            app_id: AppId::mint(),
            target: SignalTarget::Run {
                run_id: typed_id::new_workflow_run_id(),
            },
            types: ["approved".into()].into(),
            epoch: 4,
            app_epoch: 2,
        };
        let signal = mint_signal_capability(&workflow, grant.clone(), 1000, 60).unwrap();
        assert_eq!(
            verify_signal_capability(signal.as_str(), &bundle, 1001).unwrap(),
            grant
        );
        assert!(verify_app_capability(signal.as_str(), &bundle, 1001).is_err());
        let app = mint_app_capability(&control, app_grant(), 1000, 60).unwrap();
        assert!(verify_signal_capability(app.as_str(), &bundle, 1001).is_err());
        let wrong = mint_signal_capability(&control, grant, 1000, 60).unwrap();
        assert!(verify_signal_capability(wrong.as_str(), &bundle, 1001).is_err());
    }

    #[test]
    fn trusted_key_rotation_preserves_issued_capabilities() {
        let previous = ServiceSigningKey::generate();
        let current = ServiceSigningKey::generate();
        let mut bundle = trust(CONTROL_ISSUER, &previous);
        bundle
            .trust_signing_key(
                &ServiceIssuer::parse(CONTROL_ISSUER).unwrap(),
                current.key_id(),
                &current,
            )
            .unwrap();
        let old = mint_app_capability(&previous, app_grant(), 1000, 60).unwrap();
        let new = mint_app_capability(&current, app_grant(), 1000, 60).unwrap();
        assert!(verify_app_capability(old.as_str(), &bundle, 1001).is_ok());
        assert!(verify_app_capability(new.as_str(), &bundle, 1001).is_ok());
        assert!(
            verify_app_capability(old.as_str(), &trust(CONTROL_ISSUER, &current), 1001).is_err()
        );
    }
}
