//! The stateless workflow signal capability token, `wst_<claims>.<hmac>`.
//!
//! Unlike every other typed id, a `wst_` value is NOT uuid-backed: it encodes
//! signed claims, and it lives here rather than in `zeroship-id` for exactly
//! that reason. The id vocabulary is a leaf carrying `uuid` and `serde` and
//! nothing else; this needs HMAC, base64 and JSON, so putting it there would
//! pull a MAC into every crate that merely wants to name an app.
//!
//! Expiration enforcement and signal-epoch lookup happen at the control-plane
//! ingress terminus. This module defines the signed string shape and verifies
//! integrity, and does neither of those.

use zeroship_id::typed_id::WORKFLOW_SIGNAL_TOKEN_PREFIX;


/// Canonical claims signed into a stateless `wst_…` workflow signal token.
///
/// Exactly one of `run_id` or `topic` must be set. Expiration enforcement and
/// signal-epoch lookup happen at the control-plane ingress terminus; this codec
/// only defines the signed string shape and verifies integrity.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkflowSignalTokenClaims {
    pub app_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub types: Vec<String>,
    pub exp: i64,
    pub epoch: i64,
}

impl WorkflowSignalTokenClaims {
    fn validate_shape(&self) -> Result<(), String> {
        match (self.run_id.as_ref(), self.topic.as_ref()) {
            (Some(_), None) | (None, Some(_)) => {}
            _ => {
                return Err(
                    "workflow signal token claims must set exactly one of run_id or topic"
                        .to_string(),
                )
            }
        }
        if self.app_id.is_empty() {
            return Err("workflow signal token app_id must not be empty".to_string());
        }
        if self.types.is_empty() {
            return Err("workflow signal token types must not be empty".to_string());
        }
        Ok(())
    }
}

/// Sign canonical workflow signal-token claims as `wst_<claims>.<hmac>`.
///
/// The payload and HMAC are base64url-no-pad. The HMAC covers the encoded
/// payload bytes, so callers can verify without reparsing first.
pub fn sign_workflow_signal_token(
    claims: &WorkflowSignalTokenClaims,
    key: &[u8],
) -> Result<String, String> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use hmac::{Hmac, Mac};

    claims.validate_shape()?;
    let payload = serde_json::to_vec(claims)
        .map_err(|e| format!("serialize workflow signal token claims: {e}"))?;
    let payload_b64 = URL_SAFE_NO_PAD.encode(payload);

    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(key)
        .map_err(|e| format!("workflow signal token hmac key: {e}"))?;
    mac.update(payload_b64.as_bytes());
    let sig_b64 = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    Ok(format!("{WORKFLOW_SIGNAL_TOKEN_PREFIX}_{payload_b64}.{sig_b64}"))
}

/// Verify and decode a `wst_…` workflow signal token.
pub fn verify_workflow_signal_token(
    token: &str,
    key: &[u8],
) -> Result<WorkflowSignalTokenClaims, String> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use hmac::{Hmac, Mac};

    let body = token
        .strip_prefix(WORKFLOW_SIGNAL_TOKEN_PREFIX)
        .and_then(|s| s.strip_prefix('_'))
        .ok_or_else(|| format!("workflow signal token must start with {WORKFLOW_SIGNAL_TOKEN_PREFIX}_"))?;
    let (payload_b64, sig_b64) = body
        .split_once('.')
        .ok_or_else(|| "workflow signal token missing signature separator".to_string())?;
    let sig = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|e| format!("decode workflow signal token signature: {e}"))?;

    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(key)
        .map_err(|e| format!("workflow signal token hmac key: {e}"))?;
    mac.update(payload_b64.as_bytes());
    mac.verify_slice(&sig)
        .map_err(|_| "workflow signal token signature mismatch".to_string())?;

    let payload = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|e| format!("decode workflow signal token claims: {e}"))?;
    let claims: WorkflowSignalTokenClaims = serde_json::from_slice(&payload)
        .map_err(|e| format!("parse workflow signal token claims: {e}"))?;
    claims.validate_shape()?;
    Ok(claims)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_signal_token_codec_roundtrips_and_rejects_tamper() {
        let claims = WorkflowSignalTokenClaims {
            app_id: "app_0123456789abcdefghijkl000".to_string(),
            run_id: Some("run_0000000000000000000000001".to_string()),
            topic: None,
            types: vec!["approved".to_string(), "payment.succeeded".to_string()],
            exp: 1_899_999_999,
            epoch: 7,
        };
        let token = sign_workflow_signal_token(&claims, b"bearer-signing-secret")
            .expect("claims should sign");
        assert!(token.starts_with("wst_"), "got {token}");
        let decoded = verify_workflow_signal_token(&token, b"bearer-signing-secret")
            .expect("signed token should verify");
        assert_eq!(decoded, claims);

        let mut tampered = token.clone();
        tampered.push('A');
        assert!(verify_workflow_signal_token(&tampered, b"bearer-signing-secret").is_err());
        assert!(verify_workflow_signal_token(&token, b"wrong-secret").is_err());
    }
}
