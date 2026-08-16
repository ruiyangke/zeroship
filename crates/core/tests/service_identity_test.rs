use std::collections::BTreeMap;

use serde_json::json;
use zeroship_core::service_identity::{
    MechanismTag, ServiceIdentity, ServiceName, ServicePrincipal, TrustDomain,
};

fn principal(trust_domain: &str, name: &str) -> ServicePrincipal {
    ServicePrincipal::new(TrustDomain::new(trust_domain), ServiceName::new(name))
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
