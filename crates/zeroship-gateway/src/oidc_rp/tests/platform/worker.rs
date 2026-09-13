use super::*;
use zeroship_core::service_assertion::{
    ServiceSigningKey, ServiceTrustBundle, TransportAssertionVerifier,
};
use zeroship_core::service_peers::{
    service_issuer, ServiceAuth, ServiceKeyring, GATEWAY_SERVICE_NAME, WORKER_SERVICE_NAME,
};

pub async fn start() -> (Arc<ServiceAuth>, test::TestServer) {
    let gateway_issuer = service_issuer(GATEWAY_SERVICE_NAME).unwrap();
    let gateway_key = ServiceSigningKey::generate();
    let mut trust = ServiceTrustBundle::new();
    trust
        .trust_signing_key(&gateway_issuer, gateway_key.key_id(), &gateway_key)
        .unwrap();
    let envelope =
        zeroship_core::user_envelope::UserEnvelopeVerifier::for_issuer(&trust, &gateway_issuer)
            .unwrap();
    let gateway = Arc::new(ServiceAuth::new(
        ServiceKeyring::from_parts(gateway_issuer, gateway_key, ServiceTrustBundle::new()).unwrap(),
        Arc::new(TransportAssertionVerifier::new(ServiceTrustBundle::new())),
    ));
    let worker = Arc::new(
        ServiceAuth::new(
            ServiceKeyring::from_parts(
                service_issuer(WORKER_SERVICE_NAME).unwrap(),
                ServiceSigningKey::generate(),
                ServiceTrustBundle::new(),
            )
            .unwrap(),
            Arc::new(TransportAssertionVerifier::new(trust)),
        )
        .verifying_user_envelopes(envelope),
    );
    let server = test::server(move || {
        let worker = worker.clone();
        async move {
            web::App::new()
                .state(worker)
                .service(web::resource("/dispatch/{app_id}").route(web::post().to(echo)))
        }
    })
    .await;
    (gateway, server)
}

async fn echo(
    request: web::HttpRequest,
    auth: web::types::State<Arc<ServiceAuth>>,
) -> web::HttpResponse {
    let header = |name: &str| {
        request
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
    };
    if auth
        .verify(
            header("authorization"),
            zeroship_core::service_identity::endpoints::WORKER_DISPATCH,
        )
        .await
        .is_err()
    {
        return web::HttpResponse::Unauthorized().finish();
    }
    let Some(request_id) = header("x-request-id").and_then(|value| Uuid::parse_str(value).ok())
    else {
        return web::HttpResponse::Unauthorized().finish();
    };
    let user = header("zeroship-user").and_then(|value| {
        auth.user_envelope_verifier()
            .unwrap()
            .verify_for_request(value, request_id)
    });
    user.map_or_else(
        || web::HttpResponse::Unauthorized().finish(),
        |user| {
            web::HttpResponse::Ok()
                .header("content-type", "application/json")
                .body(user)
        },
    )
}
