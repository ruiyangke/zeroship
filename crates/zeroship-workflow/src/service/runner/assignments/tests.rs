use super::*;
use crate::service::{
    reconciliation::ReconciliationOptions,
    runner::{
        consumer::{ConsumerOptions, JobConsumer},
        delivery::{DeliveryOptions, JobTransport},
        ExecutionBudget, TaskExecution,
    },
    schema,
    store::{OrmStore, SchemaName},
    PolicySnapshot, TaskAssignment, WorkflowService,
};
use serde_json::{json, Value};
use zeroship_core::workflow_coordination::WorkerId;

mod fixture;
mod lifecycle;
mod ready;
use fixture::{claims, peer, Fixture};

fn scope() -> AssignedScope {
    AssignedScope {
        app_id: AppId::mint(),
        assignment_revision: 1.try_into().unwrap(),
    }
}

fn unavailable<T>(result: Result<T, WorkflowServiceError>) {
    assert!(matches!(
        result.err(),
        Some(WorkflowServiceError::Unavailable(_))
    ));
}

/// The registry's own retryable refusal, distinct from a retired generation.
fn not_ready<T: std::fmt::Debug>(result: Result<T, WorkflowServiceError>) {
    match result {
        Err(WorkflowServiceError::Unavailable(message)) => {
            assert!(message.contains("not ready"), "{message}");
        }
        other => panic!("expected the not-ready refusal, got {other:?}"),
    }
}

fn run_id() -> String {
    zeroship_core::typed_id::generate(zeroship_core::typed_id::WORKFLOW_RUN_PREFIX)
}

#[compio::test]
async fn complete_pagination_precedes_creator_preparation_and_publication() {
    let fixture = Fixture::new();
    let mut scopes = [scope(), scope()];
    scopes.sort_by(|left, right| left.app_id.cmp(&right.app_id));
    let (last, observed, release) = fixture.page(Some(&scopes[1].app_id), &[]).gated();
    let mut exchanges = vec![
        fixture.page(None, &scopes[..1]),
        fixture.page(Some(&scopes[0].app_id), &scopes[1..]),
        last,
    ];
    for scope in &scopes {
        exchanges.extend(fixture.establish(scope));
    }
    peer(&fixture, exchanges, async |client| {
        let (mut consumer, probe) = fixture.consumer(2);
        let bindings = fixture.bindings(client, &consumer, 2);
        let mut reconciling = Box::pin(bindings.reconcile());
        let pending = futures::future::select(observed, reconciling.as_mut()).await;
        assert!(matches!(pending, Either::Left((Ok(()), _))));
        assert!(fixture.factory.calls().is_empty());
        assert!(claims(&mut consumer, &probe).await.is_empty());
        release.send(()).unwrap();
        reconciling.await.unwrap();
        let calls = fixture.factory.calls();
        assert_eq!(calls.len(), scopes.len());
        for expected in &scopes {
            let call = calls.iter().find(|call| call.scope == *expected).unwrap();
            assert_eq!(call.policy.app_id(), &expected.app_id);
            call.policy.authority().unwrap().check().unwrap();
            let runtime = call.runtime.borrow();
            let app = &runtime.as_ref().unwrap().app;
            assert_eq!(app.app_id(), &expected.app_id);
            assert!(app.binding.same_binding(&call.policy));
        }
        let mut published = claims(&mut consumer, &probe).await;
        published.sort_by(|left, right| left.app_id.cmp(&right.app_id));
        assert_eq!(published, scopes);
    })
    .await;
}

#[compio::test]
async fn a_delayed_complete_scan_cannot_replace_a_newer_snapshot() {
    let fixture = Fixture::new();
    let older = scope();
    let newer = scope();
    let (last, observed, release) = fixture.page(Some(&older.app_id), &[]).gated();
    let mut exchanges = vec![fixture.page(None, std::slice::from_ref(&older)), last];
    exchanges.extend(fixture.scan(std::slice::from_ref(&newer)));
    exchanges.extend(fixture.establish(&newer));
    peer(&fixture, exchanges, async |client| {
        let (mut consumer, probe) = fixture.consumer(1);
        let bindings = fixture.bindings(client, &consumer, 1);
        let mut delayed = Box::pin(bindings.reconcile());
        assert!(matches!(
            futures::future::select(observed, delayed.as_mut()).await,
            Either::Left((Ok(()), _))
        ));
        bindings.reconcile().await.unwrap();
        release.send(()).unwrap();
        unavailable(delayed.await);
        let opened = fixture.factory.calls();
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].scope, newer);
        opened[0].policy.authority().unwrap().check().unwrap();
        assert_eq!(claims(&mut consumer, &probe).await, vec![newer]);
    })
    .await;
}

#[compio::test]
async fn failed_or_oversized_scan_preserves_the_installed_binding_and_consumer() {
    for overflow in [false, true] {
        let fixture = Fixture::new();
        let original = scope();
        let mut replacement = [scope(), scope()];
        replacement.sort_by(|left, right| left.app_id.cmp(&right.app_id));
        let mut exchanges = fixture.scan(std::slice::from_ref(&original));
        exchanges.extend(fixture.establish(&original));
        exchanges.push(fixture.page(None, &replacement[..1]));
        let next = fixture.page(Some(&replacement[0].app_id), &replacement[1..]);
        exchanges.push(if overflow { next } else { next.unavailable() });
        peer(&fixture, exchanges, async |client| {
            let (mut consumer, probe) = fixture.consumer(1);
            let bindings = fixture.bindings(client, &consumer, 1);
            bindings.reconcile().await.unwrap();
            let original_open = fixture.factory.calls().remove(0);
            let original_authority = original_open.policy.authority().unwrap();
            let result = bindings.reconcile().await;
            if overflow {
                assert!(matches!(
                    result,
                    Err(WorkflowServiceError::ResourceExhausted(_))
                ));
            } else {
                unavailable(result);
            }
            original_authority.check().unwrap();
            assert!(fixture
                .policies
                .current_binding(&original.app_id)
                .unwrap()
                .same_binding(&original_open.policy));
            assert_eq!(fixture.factory.calls().len(), 1);
            assert_eq!(claims(&mut consumer, &probe).await, vec![original]);
        })
        .await;
    }
}

#[compio::test]
async fn refresh_and_unchanged_scan_reuse_the_factory_runtime_and_policy_generation() {
    let fixture = Fixture::new();
    let scope = scope();
    let mut exchanges = fixture.scan(std::slice::from_ref(&scope));
    exchanges.extend(fixture.establish(&scope));
    exchanges.extend(fixture.refresh(&scope));
    exchanges.extend(fixture.scan(std::slice::from_ref(&scope)));
    exchanges.extend(fixture.refresh(&scope));
    peer(&fixture, exchanges, async |client| {
        let (mut consumer, probe) = fixture.consumer(1);
        let bindings = fixture.bindings(client, &consumer, 1);
        bindings.reconcile().await.unwrap();
        let opening = fixture.factory.calls().remove(0);
        let original = opening.policy.authority().unwrap();
        bindings.refresh().await.unwrap();
        bindings.reconcile().await.unwrap();
        original.check().unwrap();
        assert_eq!(fixture.factory.calls().len(), 1);
        assert!(fixture
            .policies
            .current_binding(&scope.app_id)
            .unwrap()
            .same_binding(&opening.policy));
        assert_eq!(claims(&mut consumer, &probe).await, vec![scope]);
    })
    .await;
}

#[compio::test]
async fn replacement_retires_old_authority_before_waiting_for_the_new_creator() {
    let fixture = Fixture::new();
    let original = scope();
    let replacement = AssignedScope {
        assignment_revision: 2.try_into().unwrap(),
        ..original.clone()
    };
    let mut exchanges = fixture.scan(std::slice::from_ref(&original));
    exchanges.extend(fixture.establish(&original));
    exchanges.extend(fixture.scan(std::slice::from_ref(&replacement)));
    exchanges.extend(fixture.establish(&replacement));
    peer(&fixture, exchanges, async |client| {
        let (mut consumer, probe) = fixture.consumer(1);
        let bindings = fixture.bindings(client, &consumer, 1);
        bindings.reconcile().await.unwrap();
        let old = fixture.factory.calls().remove(0);
        let authority = old.policy.authority().unwrap();
        let (observed, release) = fixture.factory.gate(&original.app_id);
        let mut replacing = Box::pin(bindings.reconcile());
        assert!(matches!(
            futures::future::select(observed, replacing.as_mut()).await,
            Either::Left((Ok(()), _))
        ));
        unavailable(authority.check());
        unavailable(old.policy.authority());
        assert!(claims(&mut consumer, &probe).await.is_empty());
        let fresh = fixture.factory.calls().remove(1);
        assert_eq!(fresh.scope, replacement);
        assert!(!fresh.policy.same_binding(&old.policy));
        release.send(()).unwrap();
        replacing.await.unwrap();
        assert_eq!(claims(&mut consumer, &probe).await, vec![replacement]);
        unavailable(old.policy.authority());
    })
    .await;
}

#[compio::test]
async fn close_cancels_creator_preparation_and_prevents_late_publication() {
    let fixture = Fixture::new();
    let scope = scope();
    let (observed, release) = fixture.factory.gate(&scope.app_id);
    let mut exchanges = fixture.scan(std::slice::from_ref(&scope));
    exchanges.extend(fixture.establish(&scope));
    peer(&fixture, exchanges, async |client| {
        let (mut consumer, probe) = fixture.consumer(1);
        let bindings = fixture.bindings(client, &consumer, 1);
        let mut preparing = Box::pin(bindings.reconcile());
        assert!(matches!(
            futures::future::select(observed, preparing.as_mut()).await,
            Either::Left((Ok(()), _))
        ));
        let opening = fixture.factory.calls().remove(0);
        let authority = opening.policy.authority().unwrap();
        bindings.close().unwrap();
        unavailable(authority.check());
        unavailable(preparing.await);
        assert!(opening.dropped.get());
        assert!(opening.runtime.borrow().is_none());
        assert!(
            release.send(()).is_err(),
            "closing must drop the pending creator operation"
        );
        assert!(claims(&mut consumer, &probe).await.is_empty());
        bindings.close().unwrap();
        unavailable(bindings.reconcile().await);
        unavailable(bindings.refresh().await);
    })
    .await;
}

#[compio::test]
async fn canceled_preparation_can_retry_without_borrowing_another_generation() {
    let fixture = Fixture::new();
    let scope = scope();
    let (observed, release) = fixture.factory.gate(&scope.app_id);
    let mut exchanges = fixture.scan(std::slice::from_ref(&scope));
    exchanges.extend(fixture.establish(&scope));
    exchanges.extend(fixture.refresh(&scope));
    peer(&fixture, exchanges, async |client| {
        let (mut consumer, probe) = fixture.consumer(1);
        let bindings = fixture.bindings(client, &consumer, 1);
        let mut preparing = Box::pin(bindings.reconcile());
        assert!(matches!(
            futures::future::select(observed, preparing.as_mut()).await,
            Either::Left((Ok(()), _))
        ));
        unavailable(bindings.refresh().await);
        let interrupted = fixture.factory.calls().remove(0);
        drop(preparing);
        assert!(interrupted.dropped.get());
        assert!(interrupted.runtime.borrow().is_none());
        assert!(release.send(()).is_err());
        assert!(claims(&mut consumer, &probe).await.is_empty());
        bindings.refresh().await.unwrap();
        let opened = fixture.factory.calls();
        assert_eq!(opened.len(), 2);
        assert!(opened[1].policy.same_binding(&interrupted.policy));
        assert!(opened[1].runtime.borrow().is_some());
        assert_eq!(claims(&mut consumer, &probe).await, vec![scope]);
    })
    .await;
}

#[compio::test]
async fn factory_must_return_the_exact_app_and_policy_registry_generation() {
    for foreign_app in [false, true] {
        let fixture = Fixture::new();
        let scope = scope();
        fixture.factory.wrong_binding(if foreign_app {
            AppId::mint()
        } else {
            scope.app_id.clone()
        });
        let mut exchanges = fixture.scan(std::slice::from_ref(&scope));
        exchanges.extend(fixture.establish(&scope));
        // A generation this process may not serve is given back as refused.
        exchanges.push(fixture.release());
        peer(&fixture, exchanges, async |client| {
            let (mut consumer, probe) = fixture.consumer(1);
            let bindings = fixture.bindings(client, &consumer, 1);
            assert!(matches!(
                bindings.reconcile().await,
                Err(WorkflowServiceError::PermissionDenied)
            ));
            assert_eq!(fixture.submitted.borrow()[0]["reason"], json!("refused"));
            let opening = fixture.factory.calls().remove(0);
            // Giving the placement back revokes its generation, so the authority
            // the factory was handed is no longer live.
            assert!(opening.policy.authority().is_err());
            {
                let runtime = opening.runtime.borrow();
                let returned = &runtime.as_ref().unwrap().app;
                assert_eq!(returned.app_id() != &scope.app_id, foreign_app);
                assert!(!returned.binding.same_binding(&opening.policy));
            }
            assert!(claims(&mut consumer, &probe).await.is_empty());
        })
        .await;
    }
}

#[compio::test]
async fn complete_removal_retires_retained_handles_and_stops_claiming() {
    let fixture = Fixture::new();
    let scope = scope();
    let mut exchanges = fixture.scan(std::slice::from_ref(&scope));
    exchanges.extend(fixture.establish(&scope));
    exchanges.push(fixture.page(None, &[]));
    peer(&fixture, exchanges, async |client| {
        let (mut consumer, probe) = fixture.consumer(1);
        let bindings = fixture.bindings(client, &consumer, 1);
        bindings.reconcile().await.unwrap();
        assert_eq!(claims(&mut consumer, &probe).await, vec![scope.clone()]);
        let retained = fixture.factory.calls().remove(0);
        bindings.reconcile().await.unwrap();
        unavailable(retained.policy.authority());
        assert!(claims(&mut consumer, &probe).await.is_empty());
        assert_eq!(fixture.factory.calls().len(), 1);
    })
    .await;
}

#[compio::test]
async fn slow_creator_preparation_does_not_block_another_apps_disabled_policy_refresh() {
    use zeroship_core::workflow_policy::AppPolicy;

    let fixture = Fixture::new();
    let mut scopes = [scope(), scope()];
    scopes.sort_by(|left, right| left.app_id.cmp(&right.app_id));
    let (observed, release) = fixture.factory.gate(&scopes[0].app_id);
    let completed = fixture.factory.completed(&scopes[1].app_id);
    let mut exchanges = fixture.scan(&scopes);
    for scope in &scopes {
        exchanges.extend(fixture.establish(scope));
    }
    let disabled = AppPolicy {
        admission: false,
        dispatch: false,
        ingress: false,
        ..AppPolicy::default()
    };
    let mut refresh = fixture.refresh(&scopes[1]);
    refresh[1] = fixture.policy(&scopes[1], disabled.clone(), 2, 60_000);
    exchanges.extend(refresh);
    peer(&fixture, exchanges, async |client| {
        let (mut consumer, probe) = fixture.consumer(2);
        let bindings = fixture.bindings(client, &consumer, 2);
        let mut preparing = Box::pin(bindings.reconcile());
        assert!(matches!(
            futures::future::select(observed, preparing.as_mut()).await,
            Either::Left((Ok(()), _))
        ));
        assert!(matches!(
            futures::future::select(completed, preparing.as_mut()).await,
            Either::Left((Ok(()), _))
        ));
        let calls = fixture.factory.calls();
        let ready = calls.iter().find(|call| call.scope == scopes[1]).unwrap();
        let original = ready.policy.authority().unwrap();
        assert_eq!(claims(&mut consumer, &probe).await, vec![scopes[1].clone()]);
        unavailable(bindings.refresh().await);
        unavailable(original.check());
        let refreshed = ready.policy.authority().unwrap();
        refreshed.check().unwrap();
        assert_eq!(refreshed.policy, disabled);
        assert_eq!(fixture.factory.calls().len(), scopes.len());
        release.send(()).unwrap();
        preparing.await.unwrap();
        let mut published = claims(&mut consumer, &probe).await;
        published.sort_by(|left, right| left.app_id.cmp(&right.app_id));
        assert_eq!(published, scopes);
    })
    .await;
}

/// A placement whose policy refuses establishment, as archive does, or whose
/// app has no responsibility yet, still prepares under a plain lease so that
/// delivered work such as the app's own closure can run.
#[compio::test]
async fn refused_establishment_prepares_the_app_under_a_plain_lease() {
    for denied in [true, false] {
        let fixture = Fixture::new();
        let scope = scope();
        let mut exchanges = fixture.scan(std::slice::from_ref(&scope));
        let mut established = fixture.establish(&scope);
        let refusal = established.pop().unwrap();
        exchanges.extend(established);
        exchanges.push(if denied {
            refusal.denied()
        } else {
            refusal.conflict()
        });
        exchanges.push(fixture.policy(
            &scope,
            zeroship_core::workflow_policy::AppPolicy::default(),
            1,
            60_000,
        ));
        peer(&fixture, exchanges, async |client| {
            let (mut consumer, probe) = fixture.consumer(1);
            let bindings = fixture.bindings(client, &consumer, 1);
            bindings.reconcile().await.unwrap();
            let opened = fixture.factory.calls();
            assert_eq!(opened.len(), 1);
            opened[0].policy.authority().unwrap().check().unwrap();
            assert_eq!(claims(&mut consumer, &probe).await, vec![scope.clone()]);
        })
        .await;
    }
}
