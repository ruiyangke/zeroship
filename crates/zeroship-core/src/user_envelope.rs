//! The `ZeroShip-User` identity envelope: what the gateway asserts about an
//! END USER to the worker, and how the worker checks it.
//!
//! # Why this is asymmetric
//!
//! Until this module existed the envelope was an HMAC keyed by `worker_key` -
//! the SAME shared secret that bearer-authenticated the dispatch hop. Two
//! checks read as two layers, and were one: whoever could present the bearer
//! could also mint any identity, because verifying an HMAC and forging one are
//! the same capability. The worker executes creator code and is reachable from
//! every app request, so it was the worst process in the fleet to hand a
//! minting key to.
//!
//! Here the gateway SIGNS with an ed25519 private key it alone holds, and the
//! worker VERIFIES under the public half published in the peer document
//! ([`crate::service_peers`]). The worker holds no key that can produce an
//! envelope this verifier accepts - not even its own service key, because a
//! [`UserEnvelopeVerifier`] is built for exactly ONE issuer's keys and the
//! worker's own key is published under a different issuer. That asymmetry is
//! the whole point: it is what makes
//! `zeroship_gateway::oidc_rp::encode_user_header`'s long-standing claim - that
//! the signature survives a bypass of the transport credential - true rather
//! than circular.
//!
//! # The wire format
//!
//! ```text
//! <base64(user_json)>.<request_id>.<issued_at_unix_secs>.<kid>.<base64url(signature)>
//! ```
//!
//! The signature covers the first FOUR segments, `kid` included. Signing the
//! `kid` matters: without it a valid envelope could be relabelled under another
//! trusted key id, and a verifier that resolves the key by an unauthenticated
//! label is choosing its own oracle.
//!
//! `request_id` binds the envelope to one dispatch and `issued_at` bounds its
//! life to [`MAX_AGE_SECS`], so a leaked header is not a standing credential.
//! Those two checks are unchanged from the HMAC envelope; only the key model
//! moved.
//!
//! # There is no unkeyed mode
//!
//! Neither type has a constructor that produces a working-looking object
//! without key material. A verifier cannot be built for an issuer the bundle
//! carries no key for - [`UserEnvelopeVerifier::for_issuer`] returns
//! [`EnvelopeKeyError::NoKeyForIssuer`] - so a deployment whose peer document
//! omits the gateway fails at STARTUP rather than serving with the check
//! silently off. That failure mode is the one this redesign exists to remove.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::STANDARD, engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use uuid::Uuid;

use crate::service_assertion::{ServiceIssuer, ServiceSigningKey, ServiceTrustBundle};

/// How old an envelope may be before the worker refuses it.
pub const MAX_AGE_SECS: u64 = 60;

/// How far into the future an envelope's `iat` may sit, for clock skew.
pub const FUTURE_SKEW_SECS: u64 = 5;

/// The envelope's key material could not be assembled.
///
/// A PROVISIONING fault raised while a process starts, never a verdict about an
/// envelope a peer presented - those are `None` from the verify methods, which
/// deliberately say nothing about which check failed.
#[derive(Debug, thiserror::Error)]
pub enum EnvelopeKeyError {
    /// The trust bundle carries no key at all for the issuer that is supposed
    /// to sign identity envelopes.
    #[error("the peer document carries no public key for {issuer}, so identity envelopes from it could never be verified")]
    NoKeyForIssuer {
        /// The issuer that was looked up.
        issuer: String,
    },
    /// A published key is not a valid ed25519 point.
    #[error("public key {key_id} for {issuer} is not a valid ed25519 key: {reason}")]
    MalformedKey {
        /// The issuer the key was published under.
        issuer: String,
        /// The `kid` the key is indexed by.
        key_id: String,
        /// Why the key was rejected.
        reason: String,
    },
}

/// Signs identity envelopes with one service's private key.
///
/// Held by the GATEWAY and by nothing else in the fleet. It is reached through
/// [`crate::service_peers::ServiceAuth::user_envelope_signer`], which returns
/// `None` for a process that loaded no key - so a process without key material
/// cannot accidentally emit an unsigned envelope, it emits none.
pub struct UserEnvelopeSigner {
    key_id: String,
    key: ServiceSigningKey,
    /// The verifier for this signer's OWN public half.
    ///
    /// The gateway reads back the envelope it just built (to recover scopes and
    /// the pairwise subject) and must do so through exactly the check the
    /// worker will run. Building it once here rather than per call keeps that
    /// on one code path and off the request hot path.
    own: UserEnvelopeVerifier,
}

impl fmt::Debug for UserEnvelopeSigner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UserEnvelopeSigner")
            .field("kid", &self.key_id)
            .finish_non_exhaustive()
    }
}

impl UserEnvelopeSigner {
    /// Build a signer around `key`, deriving the `kid` it stamps.
    ///
    /// # Errors
    ///
    /// Returns [`EnvelopeKeyError::MalformedKey`] when the key's own public
    /// half does not decompress - which would mean the process could sign
    /// envelopes nothing could check, including itself.
    pub fn new(key: ServiceSigningKey) -> Result<Self, EnvelopeKeyError> {
        let key_id = key.key_id();
        let public = key.verifying_key_bytes();
        let own = UserEnvelopeVerifier::from_keys(
            "self",
            vec![(key_id.clone(), public)],
        )?;
        Ok(Self { key_id, key, own })
    }

    /// The `kid` this signer stamps on every envelope.
    #[must_use]
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// The verifier for this signer's own public half.
    #[must_use]
    pub const fn own_verifier(&self) -> &UserEnvelopeVerifier {
        &self.own
    }

    /// Sign `user_json` for one dispatch request, stamped at the current clock.
    #[must_use]
    pub fn sign(&self, user_json: &[u8], request_id: Uuid) -> String {
        let issued_at_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before Unix epoch")
            .as_secs();
        self.sign_at(user_json, request_id, issued_at_secs)
    }

    /// Sign `user_json` at a fixed clock. The seam every timing test drives.
    #[must_use]
    pub fn sign_at(&self, user_json: &[u8], request_id: Uuid, issued_at_secs: u64) -> String {
        let signed = signing_input(
            &STANDARD.encode(user_json),
            &request_id.to_string(),
            issued_at_secs,
            &self.key_id,
        );
        let signature = self.key.sign_detached(signed.as_bytes());
        format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature))
    }
}

/// Verifies identity envelopes issued by exactly ONE service.
///
/// Scoped to a single issuer on purpose. A verifier holding every service's key
/// in one flat pool would accept an envelope from any of them - the Storm-0558
/// shape - and in this fleet that pool would include the worker's own key,
/// handing the worker back the minting power this design removed.
pub struct UserEnvelopeVerifier {
    issuer: String,
    keys: Vec<(String, ed25519_dalek::VerifyingKey)>,
}

impl fmt::Debug for UserEnvelopeVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let key_ids: Vec<&str> = self.keys.iter().map(|(id, _)| id.as_str()).collect();
        formatter
            .debug_struct("UserEnvelopeVerifier")
            .field("issuer", &self.issuer)
            .field("kids", &key_ids)
            .finish()
    }
}

impl UserEnvelopeVerifier {
    /// Build the verifier for `issuer` from a loaded peer trust bundle.
    ///
    /// # Errors
    ///
    /// Returns [`EnvelopeKeyError::NoKeyForIssuer`] when the bundle names no
    /// key for `issuer`, and [`EnvelopeKeyError::MalformedKey`] when one does
    /// not decompress. Callers on a boot path must EXIT on either: a process
    /// that carried on would accept no identity at all while looking healthy,
    /// or - worse, in an earlier design - skip the check entirely.
    pub fn for_issuer(
        bundle: &ServiceTrustBundle,
        issuer: &ServiceIssuer,
    ) -> Result<Self, EnvelopeKeyError> {
        Self::from_keys(issuer.as_str(), bundle.public_keys_for(issuer))
    }

    fn from_keys(
        issuer: &str,
        published: Vec<(String, [u8; 32])>,
    ) -> Result<Self, EnvelopeKeyError> {
        if published.is_empty() {
            return Err(EnvelopeKeyError::NoKeyForIssuer {
                issuer: issuer.to_owned(),
            });
        }
        let mut keys = Vec::with_capacity(published.len());
        for (key_id, public) in published {
            let verifying = ed25519_dalek::VerifyingKey::from_bytes(&public).map_err(|error| {
                EnvelopeKeyError::MalformedKey {
                    issuer: issuer.to_owned(),
                    key_id: key_id.clone(),
                    reason: error.to_string(),
                }
            })?;
            keys.push((key_id, verifying));
        }
        Ok(Self {
            issuer: issuer.to_owned(),
            keys,
        })
    }

    /// The issuer whose envelopes this verifier accepts.
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Verify an envelope and return its user JSON, at the current clock.
    #[must_use]
    pub fn verify(&self, header: &str) -> Option<String> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        self.verify_at(header, now)
    }

    /// Verify an envelope at a fixed clock.
    #[must_use]
    pub fn verify_at(&self, header: &str, now_secs: u64) -> Option<String> {
        self.verify_parts_at(header, now_secs)
            .map(|verified| verified.user_json)
    }

    /// Verify an envelope and require it to name `expected_request_id`.
    #[must_use]
    pub fn verify_for_request(&self, header: &str, expected_request_id: Uuid) -> Option<String> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        self.verify_for_request_at(header, expected_request_id, now)
    }

    /// Verify an envelope against one dispatch request at a fixed clock.
    #[must_use]
    pub fn verify_for_request_at(
        &self,
        header: &str,
        expected_request_id: Uuid,
        now_secs: u64,
    ) -> Option<String> {
        let verified = self.verify_parts_at(header, now_secs)?;
        if verified.request_id != expected_request_id {
            return None;
        }
        Some(verified.user_json)
    }

    fn verify_parts_at(&self, header: &str, now_secs: u64) -> Option<VerifiedEnvelope> {
        let mut parts = header.split('.');
        let payload_b64 = parts.next()?;
        let request_id = parts.next()?;
        let issued_at = parts.next()?;
        let key_id = parts.next()?;
        let signature = parts.next()?;
        if parts.next().is_some()
            || payload_b64.is_empty()
            || request_id.is_empty()
            || issued_at.is_empty()
            || key_id.is_empty()
            || signature.is_empty()
        {
            return None;
        }
        let issued_at_secs = issued_at.parse::<u64>().ok()?;
        let request_id = Uuid::parse_str(request_id).ok()?;
        if now_secs.saturating_sub(issued_at_secs) > MAX_AGE_SECS {
            return None;
        }
        if issued_at_secs.saturating_sub(now_secs) > FUTURE_SKEW_SECS {
            return None;
        }

        // Resolve the key by the `kid` the envelope names, and refuse outright
        // when this issuer publishes no such key. Trying every trusted key
        // instead would make the `kid` decorative and turn a rotation mistake
        // into a silent success.
        let verifying = self
            .keys
            .iter()
            .find(|(published, _)| published == key_id)
            .map(|(_, key)| key)?;
        let raw: [u8; 64] = URL_SAFE_NO_PAD.decode(signature).ok()?.try_into().ok()?;
        let signed = signing_input(payload_b64, &request_id.to_string(), issued_at_secs, key_id);
        verifying
            .verify_strict(signed.as_bytes(), &ed25519_dalek::Signature::from_bytes(&raw))
            .ok()?;

        let json = STANDARD.decode(payload_b64).ok()?;
        Some(VerifiedEnvelope {
            user_json: String::from_utf8(json).ok()?,
            request_id,
        })
    }
}

struct VerifiedEnvelope {
    user_json: String,
    request_id: Uuid,
}

/// The exact bytes the signature covers. ONE function, so the signer and the
/// verifier cannot disagree about what was signed - the classic way a
/// re-implemented canonicalisation admits a forgery.
fn signing_input(payload_b64: &str, request_id: &str, issued_at_secs: u64, key_id: &str) -> String {
    format!("{payload_b64}.{request_id}.{issued_at_secs}.{key_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER_JSON: &[u8] =
        br#"{"id":"pws_123","email":"a@example.com","name":"A","email_verified":true}"#;

    fn request_id() -> Uuid {
        Uuid::parse_str("018f6df3-43f7-7f68-84e0-4f1f9f5f0021").expect("fixed request id")
    }

    fn other_request_id() -> Uuid {
        Uuid::parse_str("018f6df3-43f7-7f68-84e0-4f1f9f5f0022").expect("fixed request id")
    }

    fn signer() -> UserEnvelopeSigner {
        UserEnvelopeSigner::new(ServiceSigningKey::generate()).expect("signer")
    }

    #[test]
    fn a_fresh_envelope_verifies_under_the_signer_public_half() {
        let signer = signer();
        let now = 1_900_000_000;
        let header = signer.sign_at(USER_JSON, request_id(), now);

        assert_eq!(
            signer.own_verifier().verify_at(&header, now),
            Some(String::from_utf8(USER_JSON.to_vec()).expect("utf8"))
        );
    }

    /// The property the whole module exists for: a DIFFERENT key produces an
    /// envelope that is well-formed, correctly signed, and refused.
    ///
    /// This assertion is not expressible with a shared symmetric key. Under the
    /// HMAC envelope the "wrong" key was the same key, so a test shaped like
    /// this could only have passed by testing nothing.
    #[test]
    fn an_envelope_signed_by_another_key_is_refused() {
        let gateway = signer();
        let impostor = signer();
        let now = 1_900_000_000;
        let forged = impostor.sign_at(USER_JSON, request_id(), now);

        assert_eq!(
            gateway.own_verifier().verify_at(&forged, now),
            None,
            "an envelope signed by a key this verifier does not publish must be refused"
        );
        // One-variable control: the same payload, the same clock, the same
        // request - signed by the key the verifier DOES trust - is admitted, so
        // the refusal above cannot be a verifier that refuses everything.
        assert!(gateway
            .own_verifier()
            .verify_at(&gateway.sign_at(USER_JSON, request_id(), now), now)
            .is_some());
    }

    /// A forgery is refused even when it names a `kid` the verifier trusts.
    /// The `kid` is inside the signed input, so relabelling breaks the
    /// signature rather than redirecting key resolution.
    #[test]
    fn relabelling_a_forged_envelope_with_a_trusted_kid_does_not_help() {
        let gateway = signer();
        let impostor = signer();
        let now = 1_900_000_000;
        let forged = impostor.sign_at(USER_JSON, request_id(), now);
        let mut parts: Vec<&str> = forged.split('.').collect();
        parts[3] = gateway.key_id();
        let relabelled = parts.join(".");

        assert_eq!(gateway.own_verifier().verify_at(&relabelled, now), None);
    }

    #[test]
    fn an_envelope_older_than_the_window_is_refused() {
        let signer = signer();
        let now = 1_900_000_000;
        let header = signer.sign_at(USER_JSON, request_id(), now - MAX_AGE_SECS - 1);

        assert_eq!(signer.own_verifier().verify_at(&header, now), None);
        // Control: one second inside the window is admitted.
        let fresh = signer.sign_at(USER_JSON, request_id(), now - MAX_AGE_SECS);
        assert!(signer.own_verifier().verify_at(&fresh, now).is_some());
    }

    #[test]
    fn an_envelope_from_the_future_is_refused() {
        let signer = signer();
        let now = 1_900_000_000;
        let header = signer.sign_at(USER_JSON, request_id(), now + FUTURE_SKEW_SECS + 1);

        assert_eq!(signer.own_verifier().verify_at(&header, now), None);
        let skewed = signer.sign_at(USER_JSON, request_id(), now + FUTURE_SKEW_SECS);
        assert!(signer.own_verifier().verify_at(&skewed, now).is_some());
    }

    #[test]
    fn rebinding_the_request_id_breaks_the_signature() {
        let signer = signer();
        let now = 1_900_000_000;
        let header = signer.sign_at(USER_JSON, request_id(), now);
        let mut parts: Vec<&str> = header.split('.').collect();
        let other = other_request_id().to_string();
        parts[1] = &other;
        let rebound = parts.join(".");

        assert_eq!(signer.own_verifier().verify_at(&rebound, now), None);
        // Control: an envelope legitimately signed for the other request id
        // verifies, so the refusal is about the rebinding and not the id.
        assert!(signer
            .own_verifier()
            .verify_at(&signer.sign_at(USER_JSON, other_request_id(), now), now)
            .is_some());
    }

    #[test]
    fn an_envelope_for_one_request_does_not_verify_for_another() {
        let signer = signer();
        let now = 1_900_000_000;
        let header = signer.sign_at(USER_JSON, request_id(), now);

        assert!(signer
            .own_verifier()
            .verify_for_request_at(&header, request_id(), now)
            .is_some());
        assert_eq!(
            signer
                .own_verifier()
                .verify_for_request_at(&header, other_request_id(), now),
            None
        );
    }

    /// The four-segment HMAC envelope this replaced must not verify. It cannot
    /// even be built here, so the check is that a header carrying the OLD
    /// segment count is refused rather than read as a shorter new one.
    #[test]
    fn the_four_segment_hmac_envelope_shape_is_refused() {
        let signer = signer();
        let now = 1_900_000_000;
        let header = signer.sign_at(USER_JSON, request_id(), now);
        let four: Vec<&str> = header.split('.').take(4).collect();

        assert_eq!(signer.own_verifier().verify_at(&four.join("."), now), None);
    }

    #[test]
    fn a_verifier_cannot_be_built_for_an_issuer_the_bundle_does_not_carry() {
        let issuer = crate::service_peers::service_issuer(crate::service_peers::GATEWAY_SERVICE_NAME)
            .expect("gateway issuer");
        let empty = ServiceTrustBundle::new();

        assert!(matches!(
            UserEnvelopeVerifier::for_issuer(&empty, &issuer),
            Err(EnvelopeKeyError::NoKeyForIssuer { .. })
        ));
    }

    /// A bundle carrying the WORKER's key does not let a verifier built for the
    /// GATEWAY be constructed - which is the structural reason the worker's own
    /// service key cannot mint an identity it would accept.
    #[test]
    fn a_bundle_holding_only_the_worker_key_yields_no_gateway_verifier() {
        let gateway = crate::service_peers::service_issuer(crate::service_peers::GATEWAY_SERVICE_NAME)
            .expect("gateway issuer");
        let worker_issuer = crate::service_peers::service_issuer(crate::service_peers::WORKER_SERVICE_NAME)
            .expect("worker issuer");
        let worker_key = ServiceSigningKey::generate();
        let mut bundle = ServiceTrustBundle::new();
        bundle
            .trust_signing_key(&worker_issuer, worker_key.key_id(), &worker_key)
            .expect("trust the worker");

        assert!(matches!(
            UserEnvelopeVerifier::for_issuer(&bundle, &gateway),
            Err(EnvelopeKeyError::NoKeyForIssuer { .. })
        ));
        // Control: the same bundle DOES yield a verifier for the worker, so the
        // refusal above is issuer scoping and not an unusable bundle.
        assert!(UserEnvelopeVerifier::for_issuer(&bundle, &worker_issuer).is_ok());
    }
}
