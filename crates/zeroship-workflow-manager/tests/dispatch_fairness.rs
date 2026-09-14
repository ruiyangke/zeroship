#![recursion_limit = "256"]
#![expect(
    clippy::future_not_send,
    reason = "native queue fixtures stay on their compio runtime"
)]

#[allow(dead_code, reason = "other manager suites share these fixture helpers")]
mod support;

#[allow(
    dead_code,
    reason = "the native declaration includes other manager tables"
)]
#[path = "../src/models/schema_definition.rs"]
mod native_schema;

use native_schema::schema::{jobs, queue_scopes};
use std::cell::Cell;
use support::{Backend, Fixture};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{Assignment, RunId, VerifyAssignment, WorkerId},
    workflow_jobs::{Delivery, DeploymentId, JobId, JobOperation, JobOutcome, JobSpec, Settlement},
};
use zeroship_data_orm::orm::{Database, FromRow};
use zeroship_workflow_manager::{Error, Options, Queue};

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
    sqlite_expired_delivery_rotates_behind_waiting_sibling,
    postgres_expired_delivery_rotates_behind_waiting_sibling,
    expired_delivery_rotates
);
case!(
    sqlite_lost_claim_rotation_survives_reopen_and_new_arrivals,
    postgres_lost_claim_rotation_survives_reopen_and_new_arrivals,
    lost_claim
);
case!(
    sqlite_dispatch_skips_delayed_inflight_and_foreign_jobs,
    postgres_dispatch_skips_delayed_inflight_and_foreign_jobs,
    eligibility
);
case!(
    sqlite_concurrent_claims_preserve_retry_rotation,
    postgres_concurrent_claims_preserve_retry_rotation,
    concurrent_claims
);
case!(
    sqlite_revoked_claim_rolls_back_dispatch_order_and_cursor,
    postgres_revoked_claim_rolls_back_dispatch_order_and_cursor,
    claim_rollback
);
case!(
    sqlite_replays_and_heartbeat_preserve_dispatch_order,
    postgres_replays_and_heartbeat_preserve_dispatch_order,
    stable_rotation
);
case!(
    sqlite_invalid_dispatch_state_refuses_without_mutation,
    postgres_invalid_dispatch_state_refuses_without_mutation,
    invalid_state
);

#[derive(Debug, PartialEq, Eq, FromRow)]
#[orm(entity = jobs)]
struct StoredJob {
    id: String,
    available_at: i64,
    dispatch_order: i64,
    state: String,
    attempt: i64,
    worker_id: Option<String>,
    assignment_revision: Option<i64>,
    lease_deadline: Option<i64>,
}

#[derive(Debug, PartialEq, Eq, FromRow)]
#[orm(entity = queue_scopes)]
struct Scope {
    lock_version: i64,
    dispatch_cursor: i64,
}

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    scope: Scope,
    jobs: Vec<StoredJob>,
}

#[derive(Debug, PartialEq, Eq)]
struct Rotation {
    cursor: i64,
    positions: Vec<(String, i64)>,
}

async fn snapshot(database: &Database, app: &AppId) -> Snapshot {
    let scope = database
        .entity::<queue_scopes::Entity>()
        .unwrap()
        .query()
        .filter(queue_scopes::id.eq(app.as_str()).unwrap())
        .first::<Scope>()
        .await
        .unwrap()
        .unwrap();
    let jobs = database
        .entity::<jobs::Entity>()
        .unwrap()
        .query()
        .filter(jobs::app_id.eq(app.as_str()).unwrap())
        .order_by(jobs::id.asc())
        .all::<StoredJob>()
        .await
        .unwrap();
    Snapshot { scope, jobs }
}

async fn rotation(database: &Database, app: &AppId) -> Rotation {
    let state = snapshot(database, app).await;
    Rotation {
        cursor: state.scope.dispatch_cursor,
        positions: state
            .jobs
            .into_iter()
            .map(|job| (job.id, job.dispatch_order))
            .collect(),
    }
}

async fn host(fixture: &Fixture) -> Queue {
    Queue::connect(
        fixture.binding(),
        fixture.url(),
        Options::default(),
        support::synthetic_holds(),
    )
    .await
    .unwrap()
}

fn assignment(app: &AppId) -> Assignment {
    Assignment {
        app_id: app.clone(),
        worker_id: WorkerId::mint(),
        revision: 1.try_into().unwrap(),
        expires_at: i64::MAX.try_into().unwrap(),
    }
}

fn job(app: &AppId, available_at: i64) -> JobSpec {
    JobSpec {
        id: JobId::mint(),
        app_id: app.clone(),
        deployment_id: DeploymentId::mint(),
        operation: JobOperation::Advance {
            run_id: RunId::mint(),
            generation: 0,
            revision: 1.try_into().unwrap(),
        },
        available_at: available_at.try_into().unwrap(),
    }
}

async fn claim(queue: &Queue, authority: &Assignment) -> Delivery {
    queue
        .claim(authority)
        .await
        .unwrap()
        .expect("eligible queued work must be delivered")
        .delivery()
        .clone()
}

async fn expire(database: &Database, delivery: &Delivery) {
    assert_eq!(
        database
            .entity::<jobs::Entity>()
            .unwrap()
            .update_many(
                jobs::id
                    .eq(delivery.job.id.as_str())
                    .unwrap()
                    .and(jobs::app_id.eq(delivery.job.app_id.as_str()).unwrap())
                    .and(jobs::state.eq("leased").unwrap())
                    .and(jobs::attempt.eq(delivery.attempt.get()).unwrap()),
                jobs::lease_deadline.set(Some(0_i64)).unwrap(),
            )
            .await
            .unwrap(),
        1
    );
}

async fn expired_delivery_rotates(fixture: &Fixture) {
    let queue = host(fixture).await;
    let app = AppId::mint();
    queue.register_scope(&app).await.unwrap();
    let authority = assignment(&app);
    let oldest = job(&app, 0);
    let sibling = job(&app, 1);
    queue.submit(&oldest).await.unwrap();
    queue.submit(&sibling).await.unwrap();
    let database = fixture.database().await;
    let first = claim(&queue, &authority).await;
    assert_eq!(first.job, oldest);
    expire(&database, &first).await;

    let waiting = claim(&queue, &authority).await;
    assert_eq!(waiting.job, sibling);
    expire(&database, &waiting).await;
    let retried = claim(&queue, &authority).await;
    assert_eq!(retried.job, oldest);
    assert_eq!(retried.attempt.get(), first.attempt.get() + 1);
    expire(&database, &retried).await;
    let next = claim(&queue, &authority).await;
    assert_eq!(next.job, sibling);
    assert_eq!(next.attempt.get(), waiting.attempt.get() + 1);
}

async fn lost_claim(fixture: &Fixture) {
    let queue = host(fixture).await;
    let app = AppId::mint();
    queue.register_scope(&app).await.unwrap();
    let authority = assignment(&app);
    let oldest = job(&app, 2);
    let sibling = job(&app, 3);
    queue.submit(&oldest).await.unwrap();
    queue.submit(&sibling).await.unwrap();
    let lost_reply = claim(&queue, &authority).await;
    assert_eq!(lost_reply.job, oldest);
    drop(queue);

    let database = fixture.database().await;
    expire(&database, &lost_reply).await;
    let reopened = host(fixture).await;
    assert_eq!(reopened.submit(&oldest).await.unwrap(), oldest);
    let arrival = job(&app, 0);
    reopened.submit(&arrival).await.unwrap();
    let waiting = claim(&reopened, &authority).await;
    assert_eq!(waiting.job, sibling);
    expire(&database, &waiting).await;
    let retry = claim(&reopened, &authority).await;
    assert_eq!(retry.job, oldest);
    assert_eq!(retry.attempt.get(), lost_reply.attempt.get() + 1);
    expire(&database, &retry).await;
    assert_eq!(claim(&reopened, &authority).await.job, arrival);
}

async fn eligibility(fixture: &Fixture) {
    let queue = host(fixture).await;
    let app = AppId::mint();
    let foreign = AppId::mint();
    queue.register_scope(&app).await.unwrap();
    queue.register_scope(&foreign).await.unwrap();
    let authority = assignment(&app);
    let foreign_job = job(&foreign, 0);
    queue.submit(&foreign_job).await.unwrap();
    let database = fixture.database().await;
    let untouched = snapshot(&database, &foreign).await;
    let delayed = job(&app, i64::MAX);
    let active = job(&app, 1);
    queue.submit(&delayed).await.unwrap();
    queue.submit(&active).await.unwrap();
    let running = claim(&queue, &authority).await;
    assert_eq!(running.job, active);
    let sibling = job(&app, 2);
    queue.submit(&sibling).await.unwrap();
    assert_eq!(claim(&queue, &authority).await.job, sibling);
    assert!(queue.claim(&authority).await.unwrap().is_none());
    expire(&database, &running).await;
    let retry = claim(&queue, &authority).await;
    assert_eq!(retry.job, active);
    assert_eq!(retry.attempt.get(), running.attempt.get() + 1);
    assert_eq!(snapshot(&database, &foreign).await, untouched);
    assert_eq!(claim(&queue, &assignment(&foreign)).await.job, foreign_job);
}

async fn concurrent_claims(fixture: &Fixture) {
    let left = host(fixture).await;
    let right = host(fixture).await;
    let app = AppId::mint();
    left.register_scope(&app).await.unwrap();
    let authority = assignment(&app);
    let oldest = job(&app, 0);
    let second = job(&app, 1);
    let third = job(&app, 2);
    for spec in [&oldest, &second, &third] {
        left.submit(spec).await.unwrap();
    }
    let database = fixture.database().await;
    let first = claim(&left, &authority).await;
    assert_eq!(first.job, oldest);
    expire(&database, &first).await;
    let (a, b) = futures::join!(left.claim(&authority), right.claim(&authority));
    let a = a.unwrap().unwrap().delivery().clone();
    let b = b.unwrap().unwrap().delivery().clone();
    assert_ne!(a.job.id, b.job.id);
    assert!(a.job == second || a.job == third);
    assert!(b.job == second || b.job == third);
    expire(&database, &a).await;
    expire(&database, &b).await;
    let arrival = job(&app, 0);
    right.submit(&arrival).await.unwrap();
    let retry = claim(&right, &authority).await;
    assert_eq!(retry.job, oldest);
    assert_eq!(retry.attempt.get(), first.attempt.get() + 1);
    let state = snapshot(&database, &app).await;
    let mut orders: Vec<_> = state.jobs.iter().map(|job| job.dispatch_order).collect();
    orders.sort_unstable();
    orders.dedup();
    assert_eq!(orders.len(), state.jobs.len());
    assert!(orders
        .iter()
        .all(|order| *order > 0 && *order <= state.scope.dispatch_cursor));
}

async fn claim_rollback(fixture: &Fixture) {
    let queue = host(fixture).await;
    let app = AppId::mint();
    queue.register_scope(&app).await.unwrap();
    let authority = assignment(&app);
    let first = job(&app, 0);
    queue.submit(&first).await.unwrap();
    queue.submit(&job(&app, 1)).await.unwrap();
    let database = fixture.database().await;
    let before = snapshot(&database, &app).await;
    let calls = Cell::new(0);
    let identity = VerifyAssignment {
        app_id: app.clone(),
        worker_id: authority.worker_id.clone(),
        assignment_revision: authority.revision,
    };
    let result = queue
        .claim_authorized(&identity, |tx| {
            calls.set(calls.get() + 1);
            let check = calls.get();
            let observed = authority.clone();
            let id = first.id.clone();
            async move {
                if check == 2 {
                    let job = tx
                        .entity::<jobs::Entity>()?
                        .query()
                        .filter(jobs::id.eq(id.as_str())?)
                        .first::<StoredJob>()
                        .await?
                        .unwrap();
                    assert_eq!(job.state, "leased");
                    assert_eq!(job.attempt, 1);
                    return Err(Error::Denied);
                }
                Ok(observed)
            }
        })
        .await;
    assert!(matches!(result, Err(Error::Denied)));
    assert_eq!(calls.get(), 2);
    assert_eq!(snapshot(&database, &app).await, before);
    assert_eq!(claim(&queue, &authority).await.job, first);
}

async fn stable_rotation(fixture: &Fixture) {
    let queue = host(fixture).await;
    let app = AppId::mint();
    queue.register_scope(&app).await.unwrap();
    let authority = assignment(&app);
    let first = job(&app, 0);
    let second = job(&app, 1);
    queue.submit(&first).await.unwrap();
    queue.submit(&second).await.unwrap();
    let database = fixture.database().await;
    let submitted = rotation(&database, &app).await;
    queue.register_scope(&app).await.unwrap();
    assert_eq!(rotation(&database, &app).await, submitted);
    assert_eq!(queue.submit(&first).await.unwrap(), first);
    assert_eq!(rotation(&database, &app).await, submitted);

    let delivery = claim(&queue, &authority).await;
    assert_eq!(delivery.job, first);
    let claimed = rotation(&database, &app).await;
    assert_ne!(claimed, submitted);
    queue.register_scope(&app).await.unwrap();
    assert_eq!(rotation(&database, &app).await, claimed);
    let renewed = queue
        .heartbeat(&authority, &delivery)
        .await
        .unwrap()
        .delivery()
        .clone();
    assert_eq!(rotation(&database, &app).await, claimed);
    assert_eq!(queue.submit(&first).await.unwrap(), first);
    assert_eq!(rotation(&database, &app).await, claimed);
    let command = Settlement {
        delivery: renewed,
        outcome: JobOutcome::Completed,
        successors: Vec::new(),
    };
    let receipt = queue.settle(&authority, &command).await.unwrap();
    assert_eq!(rotation(&database, &app).await, claimed);
    assert_eq!(claim(&queue, &authority).await.job, second);
    let next = rotation(&database, &app).await;
    assert_eq!(queue.settle(&authority, &command).await.unwrap(), receipt);
    assert_eq!(queue.submit(&first).await.unwrap(), first);
    assert_eq!(rotation(&database, &app).await, next);
}

async fn replace_rotation(database: &Database, spec: &JobSpec, cursor: i64, order: i64) {
    assert_eq!(
        database
            .entity::<queue_scopes::Entity>()
            .unwrap()
            .update_many(
                queue_scopes::id.eq(spec.app_id.as_str()).unwrap(),
                queue_scopes::dispatch_cursor.set(cursor).unwrap(),
            )
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        database
            .entity::<jobs::Entity>()
            .unwrap()
            .update_many(
                jobs::id.eq(spec.id.as_str()).unwrap(),
                jobs::dispatch_order.set(order).unwrap(),
            )
            .await
            .unwrap(),
        1
    );
}

async fn invalid_state(fixture: &Fixture) {
    let queue = host(fixture).await;
    let app = AppId::mint();
    queue.register_scope(&app).await.unwrap();
    let authority = assignment(&app);
    let spec = job(&app, 0);
    queue.submit(&spec).await.unwrap();
    let database = fixture.database().await;
    let valid = snapshot(&database, &app).await;
    let cursor = valid.scope.dispatch_cursor;
    let order = valid.jobs[0].dispatch_order;
    assert!(order > 0 && order <= cursor);
    for (cursor, order, expected) in [
        (-1, order, Error::Storage),
        (cursor, 0, Error::Storage),
        (cursor, -1, Error::Storage),
        (cursor, cursor + 1, Error::Storage),
        (i64::MAX, order, Error::Capacity),
    ] {
        replace_rotation(&database, &spec, cursor, order).await;
        let before = snapshot(&database, &app).await;
        assert!(matches!(queue.claim(&authority).await, Err(error) if error == expected));
        assert_eq!(snapshot(&database, &app).await, before);
    }
    let mut additional = job(&app, 0);
    additional.deployment_id.clone_from(&spec.deployment_id);
    let exhausted = snapshot(&database, &app).await;
    assert_eq!(queue.submit(&additional).await, Err(Error::Capacity));
    assert_eq!(snapshot(&database, &app).await, exhausted);
    replace_rotation(&database, &spec, cursor, order).await;
    assert_eq!(claim(&queue, &authority).await.job, spec);
}
