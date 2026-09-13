use std::sync::{Arc, OnceLock};

// Keys and verifier trust are stable; credentials are minted at request time.
pub struct WorkerTestIdentity {
    pub service_auth: Arc<zeroship_core::service_peers::ServiceAuth>,
    control: zeroship_core::service_peers::ServiceKeyring,
    /// The gateway signs identity envelopes accepted by the worker.
    pub gateway: zeroship_core::service_peers::ServiceKeyring,
    /// An untrusted keyring signs well-formed envelopes that must be refused.
    pub impostor: zeroship_core::service_peers::ServiceKeyring,
}

pub fn identity() -> &'static WorkerTestIdentity {
    use zeroship_core::service_assertion::{
        ServiceSigningKey, ServiceTrustBundle, TransportAssertionVerifier,
    };
    use zeroship_core::service_peers::{
        service_issuer, ServiceAuth, ServiceKeyring, CONTROL_SERVICE_NAME, GATEWAY_SERVICE_NAME,
        WORKER_SERVICE_NAME,
    };
    use zeroship_core::user_envelope::UserEnvelopeVerifier;

    static IDENTITY: OnceLock<WorkerTestIdentity> = OnceLock::new();
    IDENTITY.get_or_init(|| {
        let worker_issuer = service_issuer(WORKER_SERVICE_NAME).expect("worker issuer");
        let gateway_issuer = service_issuer(GATEWAY_SERVICE_NAME).expect("gateway issuer");
        let control_issuer = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");
        let worker_key = ServiceSigningKey::generate();
        let gateway_key = ServiceSigningKey::generate();
        let control_key = ServiceSigningKey::generate();

        // Gateway dispatches; control reads logs. The worker trusts both peers.
        let mut trusted = ServiceTrustBundle::new();
        let mut held = ServiceTrustBundle::new();
        for bundle in [&mut trusted, &mut held] {
            bundle
                .trust_signing_key(&gateway_issuer, gateway_key.key_id(), &gateway_key)
                .expect("trust the gateway");
            bundle
                .trust_signing_key(&control_issuer, control_key.key_id(), &control_key)
                .expect("trust control");
        }

        // End-user envelopes are accepted only from the gateway's key.
        let user_envelope = UserEnvelopeVerifier::for_issuer(&trusted, &gateway_issuer)
            .expect("the bundle publishes the gateway key");

        let keyring =
            ServiceKeyring::from_parts(worker_issuer, worker_key, held).expect("worker keyring");
        let gateway = ServiceKeyring::from_parts(
            gateway_issuer.clone(),
            gateway_key,
            ServiceTrustBundle::new(),
        )
        .expect("gateway keyring");
        let control =
            ServiceKeyring::from_parts(control_issuer, control_key, ServiceTrustBundle::new())
                .expect("control keyring");
        // Keep the issuer identical so an impostor is refused for its key.
        let impostor = ServiceKeyring::from_parts(
            gateway_issuer.clone(),
            ServiceSigningKey::generate(),
            ServiceTrustBundle::new(),
        )
        .expect("impostor keyring");
        WorkerTestIdentity {
            service_auth: Arc::new(
                ServiceAuth::new(keyring, Arc::new(TransportAssertionVerifier::new(trusted)))
                    .verifying_user_envelopes(user_envelope),
            ),
            control,
            gateway,
            impostor,
        }
    })
}

/// The gateway's credential for the worker, for tests that drive dispatch.
pub fn gateway_authorization() -> String {
    authorization(&identity().gateway)
}

pub fn control_authorization() -> String {
    authorization(&identity().control)
}

fn authorization(keyring: &zeroship_core::service_peers::ServiceKeyring) -> String {
    let worker = zeroship_core::service_peers::service_issuer(
        zeroship_core::service_peers::WORKER_SERVICE_NAME,
    )
    .expect("worker issuer");
    format!(
        "Bearer {}",
        keyring
            .mint_for(&worker)
            .expect("mint current worker credential")
    )
}

pub fn service_auth() -> Arc<zeroship_core::service_peers::ServiceAuth> {
    Arc::clone(&identity().service_auth)
}

#[compio::test]
async fn credentials_are_minted_for_each_request_under_the_same_trust() {
    use zeroship_core::service_identity::endpoints;
    let auth = service_auth();
    let first = gateway_authorization();
    let next = gateway_authorization();
    assert_ne!(
        first, next,
        "request credentials must not be cached for the test process"
    );
    for header in [&first, &next, &first] {
        auth.verify(Some(header), endpoints::WORKER_WORKFLOW_ADVANCE)
            .await
            .unwrap();
    }
    let first = control_authorization();
    let next = control_authorization();
    assert_ne!(
        first, next,
        "control requests also mint current credentials"
    );
    for header in [&first, &next] {
        auth.verify(Some(header), endpoints::WORKER_APP_LOGS)
            .await
            .unwrap();
        assert!(auth
            .verify(Some(header), endpoints::WORKER_DISPATCH)
            .await
            .is_err());
    }
}
