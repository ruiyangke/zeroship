//! Placement against Control's own rows: the migrated zone facts, the manager
//! role's column grants, the frozen-zone triggers and the real enroller
//! revocation cascade.
#![expect(
    clippy::future_not_send,
    reason = "platform fixtures stay on their compio runtime"
)]

#[path = "support/holds.rs"]
mod holds;
#[allow(
    dead_code,
    reason = "the shared platform fixture also supports process tests"
)]
#[path = "support/platform.rs"]
mod platform;

use std::{num::NonZeroU32, rc::Rc, time::Duration};
use zeroship_core::{
    app_id::AppId,
    service_assertion::ServiceSigningKey,
    typed_id,
    workflow_coordination::{RegisterWorker, WorkerId, WorkerState},
};
use zeroship_workflow_manager::{coordinator::Placed, eligibility::ZoneId, Error};
use zeroship_workflow_server::coordinator::{
    connect_eligibility, Coordinator, Error as HostError, Options,
};

async fn service(platform: &platform::Platform) -> Coordinator {
    let eligibility = Rc::new(
        connect_eligibility(&platform.runtime_url, Options::default())
            .await
            .unwrap(),
    );
    Coordinator::connect(
        &platform.runtime_url,
        Options::default(),
        holds::client(),
        eligibility,
    )
    .await
    .unwrap()
}

/// An operator-provisioned zone and an active enroller in it.
async fn zone_with_enroller(platform: &platform::Platform) -> (ZoneId, String) {
    let zone = ZoneId::mint();
    platform
        .admin
        .execute(
            "INSERT INTO zeroship.execution_zones(id,name,status) VALUES($1,$2,'active')",
            &[&zone.as_str(), &zone.as_str()],
        )
        .await
        .unwrap();
    let enroller = typed_id::generate("wen");
    platform
        .admin
        .execute(
            "INSERT INTO zeroship.worker_enrollers(id,public_key,execution_zone_id,status) \
             VALUES($1,$2,$3,'active')",
            &[
                &enroller,
                &ServiceSigningKey::generate().verifying_key_bytes().to_vec(),
                &zone.as_str(),
            ],
        )
        .await
        .unwrap();
    (zone, enroller)
}

/// An instance Control enrolled under `enroller`, registered with the manager.
async fn enrolled(platform: &platform::Platform, service: &Coordinator, enroller: &str) -> WorkerId {
    let worker = WorkerId::mint();
    platform
        .admin
        .execute(
            "INSERT INTO zeroship.worker_instances(id,ring_key,public_key,advertise_host,advertise_port,status,enroller_id) \
             VALUES($1,$2,$3,'127.0.0.1',8080,'active',$4)",
            &[
                &worker.as_str(),
                &vec![1_u8],
                &ServiceSigningKey::generate().verifying_key_bytes().to_vec(),
                &enroller,
            ],
        )
        .await
        .unwrap();
    service
        .manager
        .register(&worker, &ready())
        .await
        .unwrap();
    worker
}

const fn ready() -> RegisterWorker {
    RegisterWorker {
        capacity: NonZeroU32::new(4).unwrap(),
        state: WorkerState::Ready,
    }
}

/// Control's zones decide placement. An app created without a zone lands in
/// the seeded zone and is placed there; an app created in a second zone is
/// placed only on that zone's enrolled instance. Instances Control never
/// enrolled cannot register.
#[ntex::test]
async fn control_zone_facts_decide_placement() {
    let platform = platform::Platform::new().await;
    let seeded = platform
        .admin
        .query(
            "SELECT id FROM zeroship.execution_zones WHERE name='default'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        seeded[0].get::<_, String>(0),
        ZoneId::default_zone().as_str()
    );
    let service = service(&platform).await;
    let (away, away_enroller) = zone_with_enroller(&platform).await;
    let home_worker = enrolled(&platform, &service, &platform.default_enroller_id).await;
    let away_worker = enrolled(&platform, &service, &away_enroller).await;
    let (home_app, away_app) = (AppId::mint(), AppId::mint());
    platform.seed_app(&home_app).await;
    platform.seed_app_in(&away_app, Some(away.as_str())).await;

    let Ok(Placed::Assigned(home)) = service.manager.place(&home_app).await else {
        panic!("the seeded zone's instance takes the app created without a zone");
    };
    assert_eq!(home.worker_id, home_worker);
    let Ok(Placed::Assigned(far)) = service.manager.place(&away_app).await else {
        panic!("the second zone's instance takes its app");
    };
    assert_eq!(far.worker_id, away_worker);
    // Each app stays with its own zone: the other zone's instance is never
    // a candidate, so a second visit reports the app already owned.
    assert_eq!(service.manager.place(&home_app).await, Ok(Placed::Owned));
    assert_eq!(service.manager.place(&away_app).await, Ok(Placed::Owned));
    assert_eq!(
        service.manager.register(&WorkerId::mint(), &ready()).await,
        Err(Error::Denied)
    );
}

/// Zone facts are frozen by Control's triggers while other columns still
/// change, and the manager's role reads nothing it was not granted.
#[ntex::test]
async fn zone_facts_are_frozen_and_the_manager_reads_only_its_grants() {
    let platform = platform::Platform::new().await;
    let (other, _) = zone_with_enroller(&platform).await;
    let app = AppId::mint();
    platform.seed_app(&app).await;
    assert!(platform
        .admin
        .execute(
            "UPDATE zeroship.apps SET execution_zone_id=$2 WHERE id=$1",
            &[&app.as_str(), &other.as_str()],
        )
        .await
        .is_err());
    assert!(platform
        .admin
        .execute(
            "UPDATE zeroship.worker_enrollers SET execution_zone_id=$2 WHERE id=$1",
            &[&platform.default_enroller_id, &other.as_str()],
        )
        .await
        .is_err());
    // Control: the same rows still take their other lifecycle changes.
    assert_eq!(
        platform
            .admin
            .execute(
                "UPDATE zeroship.apps SET archived_at=now() WHERE id=$1",
                &[&app.as_str()],
            )
            .await
            .unwrap(),
        1
    );
    // Without the manager's column grant the eligibility source is not ready.
    platform
        .admin
        .batch_execute("REVOKE SELECT (execution_zone_id) ON zeroship.worker_enrollers FROM zeroship_workflow")
        .await
        .unwrap();
    assert_eq!(
        connect_eligibility(&platform.runtime_url, Options::default())
            .await
            .map(|_| ()),
        Err(HostError::Unavailable)
    );
    platform
        .admin
        .batch_execute("GRANT SELECT (execution_zone_id) ON zeroship.worker_enrollers TO zeroship_workflow")
        .await
        .unwrap();
    connect_eligibility(&platform.runtime_url, Options::default())
        .await
        .unwrap();
}

/// Archived, undeleted apps stay placeable for maintenance; deleting an app
/// abandons it, and its live placement can no longer renew.
#[ntex::test]
async fn archived_apps_stay_placeable_and_deleted_apps_do_not() {
    let platform = platform::Platform::new().await;
    let service = service(&platform).await;
    let worker = enrolled(&platform, &service, &platform.default_enroller_id).await;
    let (archived, deleted) = (AppId::mint(), AppId::mint());
    platform.seed_app(&archived).await;
    platform.seed_app(&deleted).await;
    let Ok(Placed::Assigned(placement)) = service.manager.place(&deleted).await else {
        panic!("a live app is placeable");
    };
    platform
        .admin
        .execute(
            "UPDATE zeroship.apps SET archived_at=now() WHERE id=$1",
            &[&archived.as_str()],
        )
        .await
        .unwrap();
    platform
        .admin
        .execute(
            "UPDATE zeroship.apps SET archived_at=now(),deleted_at=now() WHERE id=$1",
            &[&deleted.as_str()],
        )
        .await
        .unwrap();
    assert!(matches!(
        service.manager.place(&archived).await,
        Ok(Placed::Assigned(assignment)) if assignment.worker_id == worker
    ));
    assert_eq!(service.manager.place(&deleted).await, Ok(Placed::Ineligible));
    assert!(!service.manager.owned(&deleted).await.unwrap());
    assert_eq!(
        service
            .manager
            .renew(
                &worker,
                &zeroship_core::workflow_coordination::AssignedScope {
                    app_id: deleted.clone(),
                    assignment_revision: placement.revision,
                },
            )
            .await,
        Err(Error::Denied)
    );
}

/// Revoking an enroller with Control's own function while a placement waits
/// for the app lock refuses that placement: the facts are read after the
/// wait. The control differs only in the revocation.
#[ntex::test]
async fn the_revocation_cascade_during_the_lock_wait_refuses_placement() {
    let platform = platform::Platform::new().await;
    let revoker = platform::connect(
        &platform
            .runtime_url
            .replacen("zeroship_workflow@", "postgres@", 1),
    )
    .await;
    for revoke in [true, false] {
        let service = service(&platform).await;
        let (_, enroller) = zone_with_enroller(&platform).await;
        let worker = enrolled(&platform, &service, &enroller).await;
        let zone = platform
            .admin
            .query(
                "SELECT execution_zone_id FROM zeroship.worker_enrollers WHERE id=$1",
                &[&enroller],
            )
            .await
            .unwrap()[0]
            .get::<_, String>(0);
        let app = AppId::mint();
        platform.seed_app_in(&app, Some(&zone)).await;
        // Create the app's queue scope so the placement can wait on its lock.
        platform
            .admin
            .execute(
                "INSERT INTO workflow_manager.queue_scopes(id) VALUES($1)",
                &[&app.as_str()],
            )
            .await
            .unwrap();
        platform.admin.batch_execute("BEGIN").await.unwrap();
        let locked = platform
            .admin
            .query(
                "SELECT id FROM workflow_manager.queue_scopes WHERE id=$1 FOR UPDATE",
                &[&app.as_str()],
            )
            .await
            .unwrap();
        assert_eq!(locked.len(), 1);
        let release = async {
            blocked_manager(&revoker).await;
            if revoke {
                revoker
                    .execute("SELECT zeroship.revoke_worker_enroller($1)", &[&enroller])
                    .await
                    .unwrap();
            }
            platform.admin.batch_execute("ROLLBACK").await.unwrap();
        };
        let (placed, ()) = futures::join!(service.manager.place(&app), release);
        if revoke {
            assert_eq!(
                placed,
                Ok(Placed::Unplaced(ZoneId::parse(&zone).unwrap()))
            );
            assert_eq!(
                service.manager.register(&worker, &ready()).await,
                Err(Error::Denied)
            );
        } else {
            assert!(matches!(placed, Ok(Placed::Assigned(ref assignment)) if assignment.worker_id == worker), "{placed:?}");
        }
    }
}

/// Wait until a manager session queues behind the administrator's row lock.
async fn blocked_manager(observer: &compio_postgres::Client) {
    let sql = "SELECT count(DISTINCT a.pid) FROM pg_locks l \
         JOIN pg_stat_activity a ON a.pid=l.pid \
         WHERE a.usename='zeroship_workflow' AND NOT l.granted \
         AND cardinality(pg_blocking_pids(a.pid)) > 0";
    compio::time::timeout(Duration::from_secs(10), async {
        loop {
            observer
                .batch_execute("SELECT pg_stat_clear_snapshot()")
                .await
                .unwrap();
            if observer.query(sql, &[]).await.unwrap()[0].get::<_, i64>(0) >= 1 {
                return;
            }
            compio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the placement must reach the held app lock");
}
