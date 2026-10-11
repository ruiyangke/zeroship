//! Request-path registries for cases that never reach a real manager.
//!
//! Cases that only need the `workflows` NAMESPACE registered never make a
//! creator call, so an unreachable origin is enough and an accidental call
//! fails fast instead of hanging. A case that stands up its own peer dials it
//! with the client's own default bound instead, so the peer's reply decides
//! the outcome rather than a race against a fail-fast deadline.

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

/// A registry whose client dials nothing that listens, and gives up quickly.
pub fn unreachable_workflows() -> RemoteWorkflows {
    client_workflows(
        "http://127.0.0.1:1",
        Options {
            timeout: Duration::from_millis(250),
            ..Options::default()
        },
    )
}

/// A registry whose client dials `origin`, a peer the calling case runs and
/// that answers, and whose store is rooted under a path owned by the case.
pub fn workflows_at(origin: &str) -> RemoteWorkflows {
    client_workflows(origin, Options::default())
}

fn client_workflows(origin: &str, options: Options) -> RemoteWorkflows {
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
    let client = WorkerCoordinator::new(origin, auth, options).unwrap();
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
