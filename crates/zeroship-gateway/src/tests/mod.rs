//! Fixtures shared by source-owned gateway tests.

#![allow(
    clippy::future_not_send,
    reason = "fixtures belong to their compio runtime"
)]

pub mod browser;

/// A gateway signing identity scoped to the fixture that owns it.
pub fn gateway_service_auth() -> zeroship_core::service_peers::ServiceAuth {
    use zeroship_core::service_assertion::{
        ServiceSigningKey, ServiceTrustBundle, TransportAssertionVerifier,
    };
    use zeroship_core::service_peers::{service_issuer, ServiceAuth, ServiceKeyring, GATEWAY_SERVICE_NAME};

    let issuer = service_issuer(GATEWAY_SERVICE_NAME).expect("gateway issuer");
    let keyring = ServiceKeyring::from_parts(
        issuer,
        ServiceSigningKey::generate(),
        ServiceTrustBundle::new(),
    )
    .expect("gateway keyring");
    ServiceAuth::new(
        keyring,
        std::sync::Arc::new(TransportAssertionVerifier::new(ServiceTrustBundle::new())),
    )
}
