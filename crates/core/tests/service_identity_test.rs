use std::cell::Cell;
use std::collections::BTreeMap;

use serde_json::json;
use zeroship_core::service_identity::{
    verify_identity, AuthError, IdentityVerifier, MechanismTag, PeerCredentials,
    PresentedCredentials, ServiceIdentity, ServiceName, ServicePrincipal, TrustDomain,
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
    fn verify(
        &self,
        _credentials: &PresentedCredentials<'_>,
    ) -> Result<ServiceIdentity, AuthError> {
        self.calls.set(self.calls.get() + 1);
        Ok(identity("svc/control"))
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

#[test]
fn framework_rejects_missing_or_empty_credentials_before_verifier_dispatch() {
    let verifier = FailOpenVerifier {
        calls: Cell::new(0),
    };

    for bearer_assertion in [None, Some("")] {
        let observed = PeerCredentials::new(bearer_assertion, "https://control.zeroship.ai");
        assert_eq!(
            verify_identity(&verifier, &observed),
            Err(AuthError::NoCredentialPresented)
        );
    }

    assert_eq!(verifier.calls.get(), 0);
}
