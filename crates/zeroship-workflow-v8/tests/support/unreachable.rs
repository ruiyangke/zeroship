//! A request-path registry that reaches no manager.
//!
//! Cases that only need the `workflows` NAMESPACE registered never make a
//! creator call, so an unreachable origin is enough and an accidental call
//! fails fast instead of hanging.

use std::{sync::Arc, time::Duration};
use zeroship_core::{
    service_assertion::{
        ServiceIssuer, ServiceSigningKey, ServiceTrustBundle, TransportAssertionVerifier,
    },
    service_peers::{ServiceAuth, ServiceKeyring},
    workflow_coordination::WorkerId,
};
use zeroship_storage::{LocalFs, StorageStore};
use zeroship_workflow_client::{Options, WorkerCoordinator};
use zeroship_workflow_runner::{remote::RemoteWorkflows, PayloadObjects};

pub fn unreachable_workflows() -> RemoteWorkflows {
    let issuer = ServiceIssuer::parse(&format!(
        "spiffe://zeroship.ai/svc/worker/{}",
        WorkerId::mint().as_str()
    ))
    .unwrap();
    let auth = Arc::new(ServiceAuth::new(
        ServiceKeyring::from_parts(
            issuer,
            ServiceSigningKey::generate(),
            ServiceTrustBundle::new(),
        )
        .unwrap(),
        Arc::new(TransportAssertionVerifier::new(ServiceTrustBundle::new())),
    ));
    client_workflows("http://127.0.0.1:1", auth)
}

/// A registry whose client dials `origin` and whose store is rooted under a
/// path owned by the calling case.
pub fn workflows_at(origin: &str) -> RemoteWorkflows {
    let issuer = ServiceIssuer::parse(&format!(
        "spiffe://zeroship.ai/svc/worker/{}",
        WorkerId::mint().as_str()
    ))
    .unwrap();
    let auth = Arc::new(ServiceAuth::new(
        ServiceKeyring::from_parts(
            issuer,
            ServiceSigningKey::generate(),
            ServiceTrustBundle::new(),
        )
        .unwrap(),
        Arc::new(TransportAssertionVerifier::new(ServiceTrustBundle::new())),
    ));
    client_workflows(origin, auth)
}

fn client_workflows(origin: &str, auth: Arc<ServiceAuth>) -> RemoteWorkflows {
    let client = WorkerCoordinator::new(
        origin,
        auth,
        Options {
            timeout: Duration::from_millis(250),
            ..Options::default()
        },
    )
    .unwrap();
    let directory = std::env::temp_dir().join(format!(
        "zs-workflow-v8-support-{}",
        WorkerId::mint().as_str()
    ));
    let objects = PayloadObjects::open(StorageStore::from_backend(Arc::new(LocalFs::new(
        directory,
    ))))
    .unwrap();
    RemoteWorkflows::new(client, objects, 64 * 1024).unwrap()
}
