use super::*;
use crate::service::runner::host::{HostOptions, WorkerHost};
use zeroship_core::workflow_coordination::WorkerState;

fn options() -> HostOptions {
    HostOptions {
        consumer: ConsumerOptions {
            slots: 1,
            max_scopes: 1,
            idle_poll: Duration::from_secs(3600),
            error_backoff: Duration::from_secs(3600),
            delivery: DeliveryOptions {
                execution_timeout: Duration::from_secs(5),
                operation_timeout: Duration::from_secs(5),
                retry_delay: Duration::from_secs(1),
                reconciliation: ReconciliationOptions::default(),
            },
        },
        assignments: AssignmentOptions {
            max_scopes: 1,
            operation_timeout: Duration::from_secs(5),
        },
        registration_interval: Duration::from_secs(3600),
        assignment_interval: Duration::from_secs(3600),
        policy_interval: Duration::from_secs(3600),
    }
}

#[compio::test]
async fn cancelled_host_retires_in_progress_creator_authority_before_explicit_drain() {
    let fixture = Fixture::new();
    let scope = scope();
    let (opened, release) = fixture.factory.gate(&scope.app_id);
    let mut exchanges = vec![fixture.registration(WorkerState::Ready, 1)];
    exchanges.extend(fixture.scan(std::slice::from_ref(&scope)));
    exchanges.extend(fixture.refresh(&scope));
    exchanges.push(fixture.registration(WorkerState::Draining, 1));
    peer(&fixture, exchanges, async |client| {
        let mut host = WorkerHost::new(
            client,
            fixture.policies.clone(),
            fixture.factory.clone(),
            options(),
        )
        .unwrap();
        let mut running = Box::pin(host.run_until(std::future::pending()));
        assert!(matches!(
            futures::future::select(opened, running.as_mut()).await,
            Either::Left((Ok(()), _))
        ));
        let opening = fixture.factory.calls().remove(0);
        let authority = opening.policy.authority().unwrap();
        authority.check().unwrap();
        drop(running);
        unavailable(authority.check());
        unavailable(opening.policy.authority());
        assert!(opening.dropped.get());
        assert!(release.send(()).is_err());
        assert!(matches!(
            host.run_until(async {}).await,
            Err(WorkflowServiceError::Conflict(_))
        ));
        host.drain().await.unwrap();
        assert!(opening.runtime.borrow().is_none());
    })
    .await;
}

#[compio::test]
async fn fatal_registration_retires_ready_creator_before_drain_network_wait() {
    let fixture = Fixture::new();
    let scope = scope();
    let opened = fixture.factory.completed(&scope.app_id);
    let (refusal, refused, refuse) = fixture
        .registration(WorkerState::Ready, 1)
        .conflict()
        .gated();
    let (draining, drain_started, finish_drain) =
        fixture.registration(WorkerState::Draining, 1).gated();
    let mut exchanges = vec![fixture.registration(WorkerState::Ready, 1)];
    exchanges.extend(fixture.scan(std::slice::from_ref(&scope)));
    exchanges.extend(fixture.refresh(&scope));
    exchanges.extend([refusal, draining]);
    peer(&fixture, exchanges, async |client| {
        let mut options = options();
        options.registration_interval = Duration::from_millis(10);
        let mut host = WorkerHost::new(
            client,
            fixture.policies.clone(),
            fixture.factory.clone(),
            options,
        )
        .unwrap();
        let mut running = Box::pin(host.run_until(std::future::pending()));
        assert!(matches!(
            futures::future::select(opened, running.as_mut()).await,
            Either::Left((Ok(()), _))
        ));
        assert!(matches!(
            futures::future::select(refused, running.as_mut()).await,
            Either::Left((Ok(()), _))
        ));
        let opening = fixture.factory.calls().remove(0);
        let authority = opening.policy.authority().unwrap();
        authority.check().unwrap();
        assert!(opening.runtime.borrow().is_some());
        refuse.send(()).unwrap();
        assert!(matches!(
            futures::future::select(drain_started, running.as_mut()).await,
            Either::Left((Ok(()), _))
        ));
        unavailable(authority.check());
        unavailable(opening.policy.authority());
        finish_drain.send(()).unwrap();
        assert!(matches!(
            running.await,
            Err(WorkflowServiceError::Conflict(_))
        ));
    })
    .await;
}
