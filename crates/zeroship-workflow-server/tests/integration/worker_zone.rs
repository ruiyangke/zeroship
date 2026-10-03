//! A verified worker carries the frozen zone Control recorded on its instance
//! row at join.
#![expect(
    clippy::future_not_send,
    reason = "the registry client and native fixtures stay on the owning compio runtime"
)]

use crate::support::{platform, zone};

use std::sync::Arc;
use zeroship_core::{
    service_assertion::{InMemoryReplayStore, ServiceAssertionVerifier, ServiceTrustBundle},
    service_identity::endpoints,
};
use zeroship_workflow_manager::eligibility::ZoneId;
use zeroship_workflow_server::{
    auth::{PostgresWorkerRegistry, WorkflowAuth},
    coordinator::Error as HostError,
};

/// The service's authentication front end over a real registry connection,
/// bound the way `server.rs` binds it.
async fn auth(platform: &platform::Platform) -> WorkflowAuth {
    let replay = Arc::new(InMemoryReplayStore::default());
    WorkflowAuth::new(
        Arc::new(ServiceAssertionVerifier::new(
            ServiceTrustBundle::new(),
            replay.clone(),
        )),
        Arc::new(PostgresWorkerRegistry::new(Arc::new(
            platform::connect(&platform.runtime_url).await,
        ))),
        replay,
    )
}

/// Each live instance verifies under its own key and resolves to the zone
/// frozen on its own row, never a sibling's.
#[ntex::test]
async fn two_instances_in_two_zones_verify_with_their_own_zones() {
    let platform = platform::Platform::new().await;
    let (away, away_signer) = zone::declare_zone(&platform).await;
    let home = zone::Enrolled::join(
        &platform,
        platform::DEFAULT_JOIN_SIGNER_ID,
        ZoneId::default_zone().as_str(),
    )
    .await;
    let far = zone::Enrolled::join(&platform, &away_signer, away.as_str()).await;
    let auth = auth(&platform).await;

    let home_worker = auth
        .worker(Some(&home.authorization()), endpoints::WORKFLOW_JOB_CLAIM)
        .await
        .unwrap();
    assert_eq!(home_worker.id(), &home.instance);
    assert_eq!(home_worker.zone(), &ZoneId::default_zone());

    let far_worker = auth
        .worker(Some(&far.authorization()), endpoints::WORKFLOW_JOB_CLAIM)
        .await
        .unwrap();
    assert_eq!(far_worker.id(), &far.instance);
    assert_eq!(far_worker.zone(), &away);
}

/// Readiness answers for exactly the columns authentication projects and
/// filters on. Losing the zone column grant must fail readiness rather than let
/// the host report ready and then refuse every worker with a non-default zone.
#[ntex::test]
async fn the_readiness_probe_fails_when_the_zone_column_grant_is_revoked() {
    let platform = platform::Platform::new().await;
    let auth = auth(&platform).await;
    auth.ready().await.unwrap();

    platform
        .admin
        .batch_execute(
            "REVOKE SELECT (execution_zone_id) ON zeroship.worker_instances FROM zeroship_workflow",
        )
        .await
        .unwrap();
    assert_eq!(auth.ready().await, Err(HostError::Unavailable));

    platform
        .admin
        .batch_execute(
            "GRANT SELECT (execution_zone_id) ON zeroship.worker_instances TO zeroship_workflow",
        )
        .await
        .unwrap();
    auth.ready().await.unwrap();
}
