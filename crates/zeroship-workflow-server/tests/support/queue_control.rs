//! Signed retention peer for server composition tests; receipts are deterministic.

use ntex::web::{
    self, test,
    types::{Json, State},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use zeroship_core::{
    service_assertion::{
        InMemoryReplayStore, ServiceAssertionVerifier, ServiceSigningKey, ServiceTrustBundle,
    },
    service_identity::{endpoints, verify_service_call, ServiceEndpoint},
    service_peers::{service_issuer, CONTROL_SERVICE_NAME, WORKFLOW_SERVICE_NAME},
    workflow_coordination::{Failure, FailureCode},
    workflow_deployments::{HoldReceipt, HoldScope, HoldState, QueueHoldRequest},
};

pub struct Control {
    server: test::TestServer,
    pub key_file: PathBuf,
}
impl Control {
    pub async fn start(directory: &Path, name: &str) -> Self {
        let mut der = vec![
            0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22,
            0x04, 0x20,
        ];
        der.extend([17; 32]);
        let key = ServiceSigningKey::from_pkcs8_der(&der).unwrap();
        let key_file = directory.join(format!("{name}-workflow-key.der"));
        super::super::platform::write_private(&key_file, der);
        let workflow = service_issuer(WORKFLOW_SERVICE_NAME).unwrap();
        let mut bundle = ServiceTrustBundle::new();
        bundle
            .trust_signing_key(&workflow, key.key_id(), &key)
            .unwrap();
        let peer = Arc::new(Peer {
            verifier: ServiceAssertionVerifier::new(
                bundle,
                Arc::new(InMemoryReplayStore::default()),
            ),
        });
        let server = test::server(move || {
            let peer = peer.clone();
            async move {
                web::App::new()
                    .state(peer)
                    .service(
                        web::resource(
                            endpoints::CONTROL_QUEUE_DEPLOYMENT_HOLD_ACQUIRE.path_template(),
                        )
                        .route(web::post().to(acquire)),
                    )
                    .service(
                        web::resource(
                            endpoints::CONTROL_QUEUE_DEPLOYMENT_HOLD_RELEASE.path_template(),
                        )
                        .route(web::post().to(release)),
                    )
            }
        })
        .await;
        Self { server, key_file }
    }
    pub fn url(&self) -> String {
        format!("http://{}/", self.server.addr())
    }
}

struct Peer {
    verifier: ServiceAssertionVerifier,
}
async fn acquire(
    request: web::HttpRequest,
    body: Json<QueueHoldRequest>,
    peer: State<Arc<Peer>>,
) -> web::HttpResponse {
    respond(
        request,
        body.into_inner(),
        &peer,
        HoldState::Held,
        endpoints::CONTROL_QUEUE_DEPLOYMENT_HOLD_ACQUIRE,
    )
    .await
}
async fn release(
    request: web::HttpRequest,
    body: Json<QueueHoldRequest>,
    peer: State<Arc<Peer>>,
) -> web::HttpResponse {
    respond(
        request,
        body.into_inner(),
        &peer,
        HoldState::Released,
        endpoints::CONTROL_QUEUE_DEPLOYMENT_HOLD_RELEASE,
    )
    .await
}
async fn respond(
    request: web::HttpRequest,
    body: QueueHoldRequest,
    peer: &Peer,
    state: HoldState,
    endpoint: ServiceEndpoint,
) -> web::HttpResponse {
    let control = service_issuer(CONTROL_SERVICE_NAME).unwrap();
    let authorization = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok());
    let verified =
        verify_service_call(&peer.verifier, authorization, control.as_str(), endpoint).await;
    if verified.is_err() {
        return web::HttpResponse::Unauthorized().json(&Failure {
            code: FailureCode::Unauthenticated,
        });
    }
    let holder = HoldScope::for_queue(body.app_id.clone());
    web::HttpResponse::Ok().json(&HoldReceipt {
        app_id: body.app_id,
        deploy_id: body.deploy_id.as_str().to_owned(),
        holder_id: holder.holder().to_owned(),
        generation: body.generation,
        state,
        deploy_hash: "a".repeat(64),
    })
}
