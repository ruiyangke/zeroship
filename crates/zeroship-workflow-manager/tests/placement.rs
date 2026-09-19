//! Placement eligibility: Control's zone and enrollment facts, read under the
//! placement locks and again before commit, decide every placement. Spare
//! capacity is never authority.
#![recursion_limit = "256"]
#![allow(
    clippy::future_not_send,
    reason = "native placement fixtures stay on their compio runtime"
)]

#[allow(dead_code, reason = "shared fixtures expose other manager contracts")]
mod support;
#[allow(dead_code, reason = "shared placement fixtures serve two contract suites")]
#[path = "support/placement.rs"]
mod placement_support;

use futures::future::ready;
use placement_support::{blocked_manager, Facts, Host, LONG};
use std::rc::Rc;
use support::{Admin, Backend, Fixture};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{
        AssignedScope, ReleaseReason, ReleaseScope, RequestId, WorkerId,
    },
};
use zeroship_workflow_manager::{
    capacity::StaticPool,
    coordinator::{self, Coordinator, Placed},
    eligibility::{SoleWorker, ZoneId},
    Error,
};

macro_rules! case {
    ($sqlite:ident, $postgres:ident, $contract:ident) => {
        #[compio::test]
        async fn $sqlite() {
            Box::pin($contract(&Fixture::new(Backend::Sqlite).await)).await;
        }
        #[compio::test]
        async fn $postgres() {
            Box::pin($contract(&Fixture::new(Backend::Postgres).await)).await;
        }
    };
}

case!(
    sqlite_a_worker_in_another_zone_is_never_placed_even_with_spare_capacity,
    postgres_a_worker_in_another_zone_is_never_placed_even_with_spare_capacity,
    other_zone
);
case!(
    sqlite_an_instance_control_no_longer_admits_is_never_placed_or_renewed,
    postgres_an_instance_control_no_longer_admits_is_never_placed_or_renewed,
    revoked
);
case!(
    sqlite_revocation_between_the_two_reads_refuses_the_placement,
    postgres_revocation_between_the_two_reads_refuses_the_placement,
    between_reads
);
case!(
    sqlite_revocation_after_the_second_read_is_caught_by_the_next_check,
    postgres_revocation_after_the_second_read_is_caught_by_the_next_check,
    after_reads
);
case!(
    sqlite_a_refused_pair_is_never_offered_again_during_that_instance,
    postgres_a_refused_pair_is_never_offered_again_during_that_instance,
    refused
);
case!(
    sqlite_release_needs_no_peer_owner_and_replays_its_receipt,
    postgres_release_needs_no_peer_owner_and_replays_its_receipt,
    release_receipts
);
case!(
    sqlite_archived_apps_stay_placeable_while_deleted_apps_are_abandoned,
    postgres_archived_apps_stay_placeable_while_deleted_apps_are_abandoned,
    lifecycle
);
case!(
    sqlite_a_restarted_sole_worker_host_takes_its_app_back_from_its_predecessor,
    postgres_a_restarted_sole_worker_host_takes_its_app_back_from_its_predecessor,
    sole_worker_restart
);

/// A host whose capacity is one process outlives none of its registrations:
/// the catalog survives the process, so a predecessor that died without
/// draining is still stored ready and still holds the app. `SoleWorker` calls
/// only the worker this process minted live, so the app is taken back rather
/// than read as already having a ready eligible owner. The assertion that the
/// predecessor is still a ready registration is what keeps this from passing
/// for the uninteresting reason that its heartbeat lapsed.
async fn sole_worker_restart(fixture: &Fixture) {
    let host = Host::new(fixture, Rc::new(Facts::default())).await;
    let zone = ZoneId::mint();
    let app = AppId::mint();
    host.queue.register_scope(&app).await.unwrap();

    let dead = WorkerId::mint();
    let before = coordinator_for(&host, &zone, &dead);
    before
        .register(&dead, &placement_support::ready(1))
        .await
        .unwrap();
    let first = before.place(&app).await;
    assert!(
        matches!(first, Ok(Placed::Assigned(ref assignment)) if assignment.worker_id == dead),
        "{first:?}"
    );

    // The process dies. Its registration and its placement both outlive it.
    let live = WorkerId::mint();
    let after = coordinator_for(&host, &zone, &live);
    after
        .register(&live, &placement_support::ready(1))
        .await
        .unwrap();
    let ready: Vec<WorkerId> = after
        .ready_workers(None)
        .await
        .unwrap()
        .into_iter()
        .map(|worker| worker.worker_id)
        .collect();
    assert!(ready.contains(&dead) && ready.contains(&live), "{ready:?}");

    let placed = after.place(&app).await;
    assert!(
        matches!(placed, Ok(Placed::Assigned(ref assignment)) if assignment.worker_id == live),
        "{placed:?}"
    );
    assert!(after.owned(&app).await.unwrap());
}

/// A coordinator over the fixture's queue whose one live worker is `worker`.
fn coordinator_for(host: &Host, zone: &ZoneId, worker: &WorkerId) -> Coordinator {
    Coordinator::new(
        host.queue.clone(),
        coordinator::Options::default(),
        Rc::new(SoleWorker::new(zone.clone(), worker.clone())),
    )
    .unwrap()
}

/// Spare capacity in another zone is not authority. The control differs only
/// in the worker's zone: a worker enrolled in the app's zone is placed.
async fn other_zone(fixture: &Fixture) {
    let host = Host::new(fixture, Rc::new(Facts::default())).await;
    let (home, away) = (ZoneId::mint(), ZoneId::mint());
    let app = AppId::mint();
    host.due(&app, &home).await;
    let far = host.worker(&away, 8).await;
    let mut driver = host.driver(LONG, Rc::new(StaticPool));
    driver.tick().await;
    driver.tick().await;
    assert!(host.placed(&far).await.is_empty());
    assert!(!host.coordinator.owned(&app).await.unwrap());
    assert_eq!(
        driver.capacity().demands(&home).await.unwrap(),
        vec![app.clone()]
    );
    assert!(driver.capacity().demands(&away).await.unwrap().is_empty());

    let near = host.worker(&home, 8).await;
    let report = driver.tick().await;
    assert!(report.placement.failures.is_empty(), "{report:?}");
    assert_eq!(host.placed(&near).await, vec![app.clone()]);
    assert!(host.placed(&far).await.is_empty());
    assert!(host.coordinator.owned(&app).await.unwrap());
    assert!(driver.capacity().demands(&home).await.unwrap().is_empty());
}

/// An instance Control no longer admits fails the predicate and cannot renew
/// its registration. A live placement is rechecked: once the instance stops
/// being live it no longer owns the app, and the lane places it elsewhere.
async fn revoked(fixture: &Fixture) {
    let host = Host::new(fixture, Rc::new(Facts::default())).await;
    let zone = ZoneId::mint();
    let app = AppId::mint();
    host.due(&app, &zone).await;
    let first = host.worker(&zone, 4).await;
    let mut driver = host.driver(LONG, Rc::new(StaticPool));
    driver.tick().await;
    assert_eq!(host.placed(&first).await, vec![app.clone()]);
    let scope = host.coordinator.assignments(&first, None).await.unwrap()[0].clone();
    let scope = AssignedScope {
        app_id: scope.app_id,
        assignment_revision: scope.revision,
    };
    host.coordinator.renew(&first, &scope).await.unwrap();

    host.facts.revoke(&first);
    assert!(!host.coordinator.owned(&app).await.unwrap());
    assert_eq!(
        host.coordinator.renew(&first, &scope).await,
        Err(Error::Denied)
    );
    assert_eq!(
        host.coordinator
            .register(&first, &placement_support::ready(4))
            .await,
        Err(Error::Denied)
    );
    // The revoked instance is no longer an admissible candidate either.
    assert!(matches!(
        host.coordinator.place(&app).await.unwrap(),
        Placed::Unplaced(_)
    ));
    // No eligible worker remains: the app becomes demand, not a placement.
    driver.tick().await;
    assert_eq!(
        driver.capacity().demands(&zone).await.unwrap(),
        vec![app.clone()]
    );

    let second = host.worker(&zone, 4).await;
    driver.tick().await;
    assert_eq!(host.placed(&second).await, vec![app.clone()]);
    assert!(host.coordinator.owned(&app).await.unwrap());
    assert!(driver.capacity().demands(&zone).await.unwrap().is_empty());
}

/// A revocation that commits after the read taken under the locks, but before
/// the read taken just before commit, refuses the placement.
async fn between_reads(fixture: &Fixture) {
    let host = Host::new(fixture, Rc::new(Facts::default())).await;
    let zone = ZoneId::mint();
    let app = AppId::mint();
    host.due(&app, &zone).await;
    let worker = host.worker(&zone, 4).await;
    host.facts
        .revoke_after_read(&worker, host.facts.reads(&worker) + 1);
    assert_eq!(
        host.coordinator.place(&app).await,
        Ok(Placed::Unplaced(zone.clone()))
    );
    assert!(host.placed(&worker).await.is_empty());
    assert!(!host.coordinator.owned(&app).await.unwrap());
}

/// A revocation that commits after the pre-commit read lets that placement
/// commit, but grants it nothing further: ownership, renewal, registration
/// and delivery all recheck enrollment, and the lane places the app again.
async fn after_reads(fixture: &Fixture) {
    let host = Host::new(fixture, Rc::new(Facts::default())).await;
    let zone = ZoneId::mint();
    let app = AppId::mint();
    host.due(&app, &zone).await;
    let worker = host.worker(&zone, 4).await;
    host.facts
        .revoke_after_read(&worker, host.facts.reads(&worker) + 2);
    let Ok(Placed::Assigned(assignment)) = host.coordinator.place(&app).await else {
        panic!("the placement commits before the revocation is visible");
    };
    let scope = AssignedScope {
        app_id: assignment.app_id.clone(),
        assignment_revision: assignment.revision,
    };
    assert!(!host.coordinator.owned(&app).await.unwrap());
    assert_eq!(
        host.coordinator.renew(&worker, &scope).await,
        Err(Error::Denied)
    );
    let facts = host.facts.clone();
    assert_eq!(
        host.coordinator
            .claim_job(&worker, &scope, Ok(support::delivery_ceiling()), || ready(facts.authorize(&worker)))
            .await
            .map(|grant| grant.is_some()),
        Err(Error::Denied)
    );
    let replacement = host.worker(&zone, 4).await;
    let mut driver = host.driver(LONG, Rc::new(StaticPool));
    driver.tick().await;
    assert_eq!(host.placed(&replacement).await, vec![app.clone()]);
    assert!(host.coordinator.owned(&app).await.unwrap());
}

/// A worker that cannot serve an app releases it as refused, and the lane
/// never offers that pair again. The control differs only in the reason: a
/// relinquished placement is offered to the same worker again.
async fn refused(fixture: &Fixture) {
    for reason in [ReleaseReason::Refused, ReleaseReason::Relinquished] {
        let host = Host::new(fixture, Rc::new(Facts::default())).await;
        let zone = ZoneId::mint();
        let app = AppId::mint();
        host.due(&app, &zone).await;
        let worker = host.worker(&zone, 4).await;
        let mut driver = host.driver(LONG, Rc::new(StaticPool));
        driver.tick().await;
        let assignment = host.coordinator.assignments(&worker, None).await.unwrap()[0].clone();
        host.coordinator
            .release(
                &worker,
                &ReleaseScope {
                    request_id: RequestId::mint(),
                    app_id: app.clone(),
                    assignment_revision: assignment.revision,
                    reason,
                },
            )
            .await
            .unwrap();
        assert!(host.placed(&worker).await.is_empty());
        driver.tick().await;
        driver.tick().await;
        if reason == ReleaseReason::Refused {
            assert!(host.placed(&worker).await.is_empty(), "{reason:?}");
            assert_eq!(
                driver.capacity().demands(&zone).await.unwrap(),
                vec![app.clone()]
            );
            // The refusal tombstone keeps this pair out of selection.
            assert!(matches!(
                host.coordinator.place(&app).await.unwrap(),
                Placed::Unplaced(_)
            ));
            // A different instance is offered the app.
            let other = host.worker(&zone, 4).await;
            driver.tick().await;
            assert_eq!(host.placed(&other).await, vec![app.clone()]);
            assert!(host.placed(&worker).await.is_empty());
        } else {
            assert_eq!(host.placed(&worker).await, vec![app.clone()], "{reason:?}");
        }
        assert!(driver.capacity().demands(&zone).await.unwrap().is_empty());
    }
}

/// Release keeps no wake hint and needs no peer owner: the last owner of an
/// app with due work may release it, and recovery responsibility stays with
/// the manager. A retried release replays its receipt; a changed reason under
/// the same request conflicts.
async fn release_receipts(fixture: &Fixture) {
    let host = Host::new(fixture, Rc::new(Facts::default())).await;
    let zone = ZoneId::mint();
    let app = AppId::mint();
    host.due(&app, &zone).await;
    let worker = host.worker(&zone, 4).await;
    let Ok(Placed::Assigned(assignment)) = host.coordinator.place(&app).await else {
        panic!("the only eligible worker is placed");
    };
    let request = ReleaseScope {
        request_id: RequestId::mint(),
        app_id: app.clone(),
        assignment_revision: assignment.revision,
        reason: ReleaseReason::Relinquished,
    };
    host.coordinator.release(&worker, &request).await.unwrap();
    host.coordinator.release(&worker, &request).await.unwrap();
    assert_eq!(
        host.coordinator
            .release(
                &worker,
                &ReleaseScope {
                    reason: ReleaseReason::Refused,
                    ..request.clone()
                },
            )
            .await,
        Err(Error::Conflict)
    );
    assert_eq!(
        host.coordinator
            .release(
                &worker,
                &ReleaseScope {
                    request_id: RequestId::mint(),
                    ..request.clone()
                },
            )
            .await,
        Err(Error::Conflict)
    );
    let stranger = WorkerId::mint();
    assert_eq!(
        host.coordinator
            .release(
                &stranger,
                &ReleaseScope {
                    request_id: RequestId::mint(),
                    ..request
                },
            )
            .await,
        Err(Error::Denied)
    );
    assert!(!host.coordinator.owned(&app).await.unwrap());
}

/// Archived, undeleted apps stay placeable so maintenance can drain them.
/// Deleted apps are abandoned: never placed, and their recorded demand is
/// cleared rather than left inflating the zone's target.
async fn lifecycle(fixture: &Fixture) {
    let host = Host::new(fixture, Rc::new(Facts::default())).await;
    let zone = ZoneId::mint();
    let (archived, deleted) = (AppId::mint(), AppId::mint());
    host.due(&archived, &zone).await;
    host.due(&deleted, &zone).await;
    let mut driver = host.driver(LONG, Rc::new(StaticPool));
    driver.tick().await;
    let mut unplaced = vec![archived.clone(), deleted.clone()];
    unplaced.sort();
    assert_eq!(driver.capacity().demands(&zone).await.unwrap(), unplaced);

    host.facts.delete(&deleted);
    let worker = host.worker(&zone, 4).await;
    driver.tick().await;
    assert_eq!(host.placed(&worker).await, vec![archived.clone()]);
    assert!(driver.capacity().demands(&zone).await.unwrap().is_empty());
    assert_eq!(
        host.coordinator.place(&deleted).await,
        Ok(Placed::Ineligible)
    );
    assert!(!host.coordinator.owned(&deleted).await.unwrap());
}

/// Revocation injected while the placement waits for the app lock is read
/// after the wait. The control differs only in the revocation: the same
/// blocked placement then commits.
#[compio::test]
async fn postgres_revocation_during_the_lock_wait_refuses_the_placement() {
    let fixture = Fixture::new(Backend::Postgres).await;
    let Admin::Postgres(admin) = &fixture.admin else {
        unreachable!()
    };
    for revoke in [true, false] {
        let host = Host::new(&fixture, Rc::new(Facts::default())).await;
        let zone = ZoneId::mint();
        let app = AppId::mint();
        host.due(&app, &zone).await;
        let worker = host.worker(&zone, 4).await;
        admin.batch_execute("BEGIN").await.unwrap();
        let locked = admin
            .query(
                "SELECT id FROM workflow_manager.queue_scopes WHERE id=$1 FOR UPDATE",
                &[&app.as_str()],
            )
            .await
            .unwrap();
        assert_eq!(locked.len(), 1);
        let reads = host.facts.reads(&worker);
        let release = async {
            blocked_manager(admin, 1).await;
            // Nothing has read this worker's facts while the placement waited.
            assert_eq!(host.facts.reads(&worker), reads);
            if revoke {
                host.facts.revoke(&worker);
            }
            admin.batch_execute("ROLLBACK").await.unwrap();
        };
        let (placed, ()) = futures::join!(host.coordinator.place(&app), release);
        if revoke {
            assert_eq!(placed, Ok(Placed::Unplaced(zone)));
            assert!(host.placed(&worker).await.is_empty());
        } else {
            assert!(matches!(placed, Ok(Placed::Assigned(_))), "{placed:?}");
            assert_eq!(host.placed(&worker).await, vec![app]);
        }
    }
}
