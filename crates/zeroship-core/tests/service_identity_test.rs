use std::cell::Cell;
use std::collections::BTreeMap;

use serde_json::json;
use zeroship_core::service_identity::{
    verify_identity, AuthError, IdentityVerifier, MechanismTag, PeerCredentials,
    PresentedCredentials, ServiceIdentity, ServiceName, ServicePrincipal, StubIdentityVerifier,
    TlsPeerInfo, TrustDomain, VerifyFuture,
};

fn principal(trust_domain: &str, name: &str) -> ServicePrincipal {
    ServicePrincipal::new(TrustDomain::new(trust_domain), ServiceName::new(name))
}

fn identity(name: &str) -> ServiceIdentity {
    ServiceIdentity::new(
        principal("zeroship.ai", name),
        MechanismTag::new("test-stub"),
        BTreeMap::new(),
    )
}

struct FailOpenVerifier {
    calls: Cell<usize>,
}

impl IdentityVerifier for FailOpenVerifier {
    fn verify<'a>(&'a self, _credentials: &'a PresentedCredentials<'a>) -> VerifyFuture<'a> {
        self.calls.set(self.calls.get() + 1);
        Box::pin(async { Ok(identity("svc/control")) })
    }
}

#[test]
fn service_identity_compares_domain_and_name_as_one_principal() {
    let expected = principal("zeroship.ai", "svc/gateway");
    let mut attributes = BTreeMap::new();
    attributes.insert("assurance".to_owned(), json!("stub"));
    let identity = ServiceIdentity::new(
        expected.clone(),
        MechanismTag::new("test-stub"),
        attributes,
    );

    assert!(identity.matches_principal(&expected));
    assert!(!identity.matches_principal(&principal("attacker.example", "svc/gateway")));
    assert!(!identity.matches_principal(&principal("zeroship.ai", "svc/control")));
    assert_eq!(identity.mechanism().as_ref(), "test-stub");
    assert_eq!(identity.attributes()["assurance"], json!("stub"));
}

#[compio::test]
async fn framework_rejects_missing_or_empty_credentials_before_verifier_dispatch() {
    let verifier = FailOpenVerifier {
        calls: Cell::new(0),
    };
    let leaf = [1, 2, 3];
    let certificate_chain: [&[u8]; 1] = [&leaf];
    let tls_peer = TlsPeerInfo::new(&certificate_chain)
        .expect("a non-empty certificate chain is a TLS peer observation");

    for (bearer_assertion, tls_observation) in [
        (None, None),
        (Some(""), None),
        (None, Some(tls_peer)),
        (Some(""), Some(tls_peer)),
    ] {
        let observed = PeerCredentials::new(
            bearer_assertion,
            tls_observation,
            "https://control.zeroship.ai",
        );
        assert_eq!(
            verify_identity(&verifier, &observed).await,
            Err(AuthError::NoCredentialPresented)
        );
    }

    assert_eq!(verifier.calls.get(), 0);
}

#[compio::test]
async fn stub_verifier_maps_presented_credentials_to_a_neutral_identity() {
    let expected = identity("svc/gateway");
    let verifier = StubIdentityVerifier::new(expected.clone());
    let observed = PeerCredentials::new(
        Some("not-cryptographically-verified"),
        None,
        "https://control.zeroship.ai",
    );

    assert_eq!(verify_identity(&verifier, &observed).await, Ok(expected));
}

struct ObservationVerifier;

impl IdentityVerifier for ObservationVerifier {
    fn verify<'a>(&'a self, credentials: &'a PresentedCredentials<'a>) -> VerifyFuture<'a> {
        Box::pin(async move {
            assert_eq!(credentials.bearer_assertion(), "observed-assertion");
            assert_eq!(
                credentials
                    .tls_peer()
                    .expect("TLS peer observation is preserved")
                    .certificate_chain_der()
                    .len(),
                1
            );
            assert_eq!(
                credentials.expected_audience(),
                "https://control.zeroship.ai"
            );
            Ok(identity("svc/gateway"))
        })
    }
}

#[compio::test]
async fn mechanism_fat_input_preserves_simultaneous_transport_observations() {
    let leaf = [1, 2, 3];
    let certificate_chain: [&[u8]; 1] = [&leaf];
    let tls_peer = TlsPeerInfo::new(&certificate_chain)
        .expect("a non-empty certificate chain is a TLS peer observation");
    let observed = PeerCredentials::new(
        Some("observed-assertion"),
        Some(tls_peer),
        "https://control.zeroship.ai",
    );

    assert_eq!(
        verify_identity(&ObservationVerifier, &observed).await,
        Ok(identity("svc/gateway"))
    );
    assert!(TlsPeerInfo::new(&[]).is_none());
}

struct DebugVerifier;

impl IdentityVerifier for DebugVerifier {
    fn verify<'a>(&'a self, credentials: &'a PresentedCredentials<'a>) -> VerifyFuture<'a> {
        Box::pin(async move {
            let diagnostic = format!("{credentials:?}");
            assert!(!diagnostic.contains("credential-must-stay-secret"));
            assert!(diagnostic.contains("[REDACTED]"));
            Ok(identity("svc/gateway"))
        })
    }
}

#[compio::test]
async fn credential_diagnostics_redact_bearer_assertions() {
    let observed = PeerCredentials::new(
        Some("credential-must-stay-secret"),
        None,
        "https://control.zeroship.ai",
    );
    let diagnostic = format!("{observed:?}");

    assert!(!diagnostic.contains("credential-must-stay-secret"));
    assert!(diagnostic.contains("[REDACTED]"));
    assert_eq!(
        verify_identity(&DebugVerifier, &observed).await,
        Ok(identity("svc/gateway"))
    );
}
