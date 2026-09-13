#![recursion_limit = "256"]
#![allow(
    clippy::future_not_send,
    reason = "native recovery fixtures stay on their compio runtime"
)]

#[allow(
    dead_code,
    reason = "shared queue fixtures also expose backend administration"
)]
mod support;

use std::time::Duration;
use support::{Admin, Backend, Fixture};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{Assignment, WorkerId},
    workflow_jobs::{DeploymentId, JobOperation, JobOutcome, Settlement},
};
use zeroship_data_orm::{
    orm::{Operation, Output},
    value, Value,
};
use zeroship_workflow_manager::{
    recovery::{Options, Recovery},
    Error, Queue,
};

macro_rules! case {
    ($sqlite:ident, $postgres:ident, $contract:ident) => {
        #[compio::test]
        async fn $sqlite() {
            let fixture = Fixture::new(Backend::Sqlite).await;
            Box::pin($contract(&fixture)).await;
        }
        #[compio::test]
        async fn $postgres() {
            let fixture = Fixture::new(Backend::Postgres).await;
            Box::pin($contract(&fixture)).await;
        }
    };
}

case!(
    sqlite_recovery_survives_no_workers_and_replica_restart,
    postgres_recovery_survives_no_workers_and_replica_restart,
    durable_responsibility
);
case!(
    sqlite_recovery_keeps_pending_pins_and_rejects_stale_activation,
    postgres_recovery_keeps_pending_pins_and_rejects_stale_activation,
    activation
);
case!(
    sqlite_recovery_pages_reach_later_due_scopes,
    postgres_recovery_pages_reach_later_due_scopes,
    pages
);
case!(
    sqlite_recovery_publication_and_deadline_roll_back_together,
    postgres_recovery_publication_and_deadline_roll_back_together,
    rollback
);

async fn host(fixture: &Fixture) -> (Recovery, Queue) {
    let queue = Queue::connect(
        fixture.binding(),
        fixture.url(),
        zeroship_workflow_manager::Options::default(),
    )
    .await
    .unwrap();
    (
        Recovery::new(
            queue.clone(),
            Options {
                interval: Duration::from_secs(60),
                page_size: 2,
            },
        )
        .unwrap(),
        queue,
    )
}

async fn snapshot(fixture: &Fixture, app: &AppId) -> Value {
    let db = fixture.database().await;
    let Output::Rows { mut rows, .. } = db
        .collection("recovery_scopes")
        .unwrap()
        .find(value!({"id":app.as_str()}), value!({"limit":1}))
        .await
        .unwrap()
    else {
        panic!("expected recovery rows")
    };
    assert_eq!(rows.len(), 1);
    rows.pop().unwrap()
}

async fn make_due(fixture: &Fixture, app: &AppId) {
    let db = fixture.database().await;
    db.collection("recovery_scopes")
        .unwrap()
        .update(value!({"id":app.as_str()}), value!({"next_due_at":0}))
        .await
        .unwrap();
}

async fn job_count(fixture: &Fixture, app: &AppId) -> i64 {
    let db = fixture.database().await;
    let Output::Count(count) = db
        .collection("jobs")
        .unwrap()
        .count(value!({"app_id":app.as_str()}), value!({}))
        .await
        .unwrap()
    else {
        panic!("expected job count")
    };
    count
}

fn assignment(app: &AppId) -> Assignment {
    Assignment {
        app_id: app.clone(),
        worker_id: WorkerId::mint(),
        revision: 1.try_into().unwrap(),
        expires_at: i64::MAX.try_into().unwrap(),
    }
}

async fn durable_responsibility(fixture: &Fixture) {
    let (first, first_queue) = host(fixture).await;
    let (second, second_queue) = host(fixture).await;
    let app = AppId::mint();
    let deployment = DeploymentId::mint();
    let revision = 1.try_into().unwrap();
    let (a, b) = futures::join!(
        first.ensure(&app, &deployment, revision),
        second.ensure(&app, &deployment, revision)
    );
    a.unwrap();
    b.unwrap();
    assert_eq!(first.due(None).await.unwrap(), std::slice::from_ref(&app));
    let original = snapshot(fixture, &app).await;
    second.ensure(&app, &deployment, revision).await.unwrap();
    assert_eq!(snapshot(fixture, &app).await, original);
    let (a, b) = futures::join!(first.dispatch(&app), second.dispatch(&app));
    let accepted = a.unwrap().unwrap();
    assert_eq!(b.unwrap(), Some(accepted.clone()));
    assert_eq!(accepted.operation, JobOperation::Reconcile {});
    assert_eq!(job_count(fixture, &app).await, 1);
    assert!(first.due(None).await.unwrap().is_empty());
    make_due(fixture, &app).await;
    assert_eq!(second.dispatch(&app).await.unwrap(), Some(accepted.clone()));
    assert_eq!(job_count(fixture, &app).await, 1);
    drop(first);
    drop(first_queue);
    drop(second);
    drop(second_queue);

    let (reopened, queue) = host(fixture).await;
    assert_eq!(
        reopened.due(None).await.unwrap(),
        std::slice::from_ref(&app)
    );
    assert_eq!(
        reopened.dispatch(&app).await.unwrap(),
        Some(accepted.clone())
    );
    let owner = assignment(&app);
    let delivered = queue.claim(&owner).await.unwrap().unwrap().delivery;
    let pending = snapshot(fixture, &app).await;
    queue.heartbeat(&owner, &delivered).await.unwrap();
    assert_eq!(snapshot(fixture, &app).await, pending);
    assert_eq!(
        reopened.due(None).await.unwrap(),
        std::slice::from_ref(&app)
    );
    let settlement = Settlement {
        delivery: delivered,
        outcome: JobOutcome::Completed,
        successors: vec![],
    };
    queue.settle(&owner, &settlement).await.unwrap();
    let following = reopened.dispatch(&app).await.unwrap().unwrap();
    assert_ne!(following.id, accepted.id);
    queue.settle(&owner, &settlement).await.unwrap();
    assert_eq!(reopened.dispatch(&app).await.unwrap(), Some(following));
    assert_eq!(job_count(fixture, &app).await, 2);
}

async fn activation(fixture: &Fixture) {
    let (recovery, queue) = host(fixture).await;
    let app = AppId::mint();
    let first = DeploymentId::mint();
    let second = DeploymentId::mint();
    recovery
        .ensure(&app, &first, 1.try_into().unwrap())
        .await
        .unwrap();
    let original = recovery.dispatch(&app).await.unwrap().unwrap();
    let deadline = snapshot(fixture, &app).await["next_due_at"].clone();
    assert_eq!(
        recovery.ensure(&app, &second, 1.try_into().unwrap()).await,
        Err(Error::Conflict)
    );
    recovery
        .ensure(&app, &second, 2.try_into().unwrap())
        .await
        .unwrap();
    assert_eq!(snapshot(fixture, &app).await["next_due_at"], deadline);
    assert_eq!(
        recovery.ensure(&app, &first, 1.try_into().unwrap()).await,
        Err(Error::Conflict)
    );
    assert_eq!(
        recovery.dispatch(&app).await.unwrap(),
        Some(original.clone())
    );
    let owner = assignment(&app);
    let delivery = queue.claim(&owner).await.unwrap().unwrap().delivery;
    assert_eq!(delivery.job.deployment_id, first);
    queue
        .settle(
            &owner,
            &Settlement {
                delivery,
                outcome: JobOutcome::Completed,
                successors: vec![],
            },
        )
        .await
        .unwrap();
    assert!(recovery.dispatch(&app).await.unwrap().is_none());
    make_due(fixture, &app).await;
    let next = recovery.dispatch(&app).await.unwrap().unwrap();
    assert_ne!(next.id, original.id);
    assert_eq!(next.deployment_id, second);
    let foreign = AppId::mint();
    queue.register_scope(&foreign).await.unwrap();
    assert_eq!(recovery.dispatch(&foreign).await, Err(Error::Denied));
    assert_eq!(job_count(fixture, &foreign).await, 0);
}

async fn pages(fixture: &Fixture) {
    let (recovery, _) = host(fixture).await;
    let mut apps: Vec<_> = (0..7).map(|_| AppId::mint()).collect();
    apps.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    for app in &apps {
        recovery
            .ensure(app, &DeploymentId::mint(), 1.try_into().unwrap())
            .await
            .unwrap();
    }
    for app in apps.iter().step_by(2) {
        recovery.dispatch(app).await.unwrap().unwrap();
    }
    let mut found = Vec::new();
    loop {
        let page = recovery.due(found.last()).await.unwrap();
        assert!(page.len() <= 2);
        if page.is_empty() {
            break;
        }
        found.extend(page);
    }
    assert_eq!(
        found,
        apps.iter().skip(1).step_by(2).cloned().collect::<Vec<_>>()
    );
    // A new due obligation behind an old cursor belongs to the next sweep.
    make_due(fixture, &apps[0]).await;
    assert!(recovery.due(apps.last()).await.unwrap().is_empty());
    assert_eq!(recovery.due(None).await.unwrap()[0], apps[0]);
}

async fn fault(fixture: &Fixture, install: bool) {
    match &fixture.admin {
        Admin::Sqlite(connection) => {
            connection.execute_batch(if install {
                "CREATE TRIGGER recovery_fault BEFORE UPDATE ON recovery_scopes BEGIN SELECT RAISE(ABORT,'recovery fault'); END;"
            } else { "DROP TRIGGER recovery_fault;" }).unwrap();
        }
        Admin::Postgres(connection) => {
            connection.batch_execute(if install {
                "CREATE FUNCTION workflow_manager.recovery_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'recovery fault'; END $$;
                 CREATE TRIGGER recovery_fault BEFORE UPDATE ON workflow_manager.recovery_scopes FOR EACH ROW EXECUTE FUNCTION workflow_manager.recovery_fault();"
            } else {
                "DROP TRIGGER recovery_fault ON workflow_manager.recovery_scopes; DROP FUNCTION workflow_manager.recovery_fault();"
            }).await.unwrap();
        }
    }
}

async fn rollback(fixture: &Fixture) {
    let (recovery, _) = host(fixture).await;
    let app = AppId::mint();
    recovery
        .ensure(&app, &DeploymentId::mint(), 1.try_into().unwrap())
        .await
        .unwrap();
    let original = snapshot(fixture, &app).await;
    fault(fixture, true).await;
    assert!(recovery.dispatch(&app).await.is_err());
    assert_eq!(snapshot(fixture, &app).await, original);
    assert_eq!(job_count(fixture, &app).await, 0);
    assert_eq!(
        recovery.due(None).await.unwrap(),
        std::slice::from_ref(&app)
    );
    fault(fixture, false).await;
    let accepted = recovery.dispatch(&app).await.unwrap().unwrap();
    let db = fixture.database().await;
    assert!(
        db.collection("jobs")
            .unwrap()
            .execute(Operation::Purge {
                filter: value!({"id":accepted.id.as_str()}),
                many: false,
            })
            .await
            .is_err(),
        "the pending obligation retains its referenced job"
    );
    assert_eq!(job_count(fixture, &app).await, 1);
}
