#![recursion_limit = "256"]
#![allow(
    clippy::future_not_send,
    reason = "scheduler contracts use compio-local database fixtures"
)]

#[allow(
    dead_code,
    reason = "shared fixtures expose other manager test helpers"
)]
mod support;

use support::{Admin, Backend, Fixture};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{Assignment, WorkerId},
    workflow_jobs::{Delivery, DeploymentId, JobId, JobOperation, JobOutcome, JobSpec, Settlement},
    workflow_schedules::{ActivateSchedules, RegisterSchedules, ScheduleDescriptor, ScheduleId},
};
use zeroship_data_orm::{
    orm::{Operation, Output},
    value, Value,
};
use zeroship_workflow_calendar::{
    IntervalAnchor, ScheduleCatchUp, ScheduleOverlap, ScheduleTiming,
};
use zeroship_workflow_manager::{
    recovery::{Options as RecoveryOptions, Recovery},
    scheduling::{Options, Scheduler},
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
    sqlite_preparation_is_immutable_and_scoped,
    postgres_preparation_is_immutable_and_scoped,
    preparation
);
case!(
    sqlite_activation_keeps_old_jobs_and_exact_prerequisites,
    postgres_activation_keeps_old_jobs_and_exact_prerequisites,
    activations
);
case!(
    sqlite_unfinished_activation_does_not_hide_recovery,
    postgres_unfinished_activation_does_not_hide_recovery,
    activation_gate
);
case!(
    sqlite_calendar_production_survives_replicas_without_workers,
    postgres_calendar_production_survives_replicas_without_workers,
    replicas
);
case!(
    sqlite_catch_up_allowance_and_boundary_survive_reopen,
    postgres_catch_up_allowance_and_boundary_survive_reopen,
    catch_up
);
case!(
    sqlite_skip_emits_oldest_due_then_advances_past_now,
    postgres_skip_emits_oldest_due_then_advances_past_now,
    skip
);
case!(
    sqlite_due_pages_exclude_disabled_schedules,
    postgres_due_pages_exclude_disabled_schedules,
    due_pages
);
case!(
    sqlite_calendar_dispatch_rolls_back_as_a_unit,
    postgres_calendar_dispatch_rolls_back_as_a_unit,
    dispatch_rollback
);
case!(
    sqlite_activation_and_recovery_roll_back_as_a_unit,
    postgres_activation_and_recovery_roll_back_as_a_unit,
    activation_rollback
);
case!(
    sqlite_interpretation_changes_fence_prepared_calendars,
    postgres_interpretation_changes_fence_prepared_calendars,
    interpretation
);
case!(
    sqlite_changed_schedule_projection_cannot_generate_jobs,
    postgres_changed_schedule_projection_cannot_generate_jobs,
    immutable_schedule_projection
);
case!(
    sqlite_activation_replay_rejects_substituted_job_metadata,
    postgres_activation_replay_rejects_substituted_job_metadata,
    activation_job_identity
);
case!(
    sqlite_occurrence_replay_rejects_another_same_app_job,
    postgres_occurrence_replay_rejects_another_same_app_job,
    occurrence_job_identity
);
case!(
    sqlite_orphan_cron_cannot_bypass_activation,
    postgres_orphan_cron_cannot_bypass_activation,
    orphan_cron
);
case!(
    sqlite_disabling_schedules_retains_pending_occurrences,
    postgres_disabling_schedules_retains_pending_occurrences,
    disabled_schedules
);

async fn host(fixture: &Fixture) -> (Scheduler, Queue) {
    let queue = Queue::connect(
        fixture.binding(),
        fixture.url(),
        zeroship_workflow_manager::Options::default(),
        support::synthetic_holds(),
    )
    .await
    .unwrap();
    let scheduler = Scheduler::new(
        queue.clone(),
        Options {
            max_schedules: 8,
            max_backfill: 5,
            min_interval_ms: 1_000,
            page_size: 2,
        },
    )
    .unwrap();
    (scheduler, queue)
}

fn descriptor(name: &str, catch_up: ScheduleCatchUp) -> ScheduleDescriptor {
    ScheduleDescriptor {
        name: name.into(),
        workflow_name: "scheduled-work".into(),
        schedule: ScheduleTiming::Interval {
            interval_ms: 1_000,
            anchor: IntervalAnchor::Epoch,
        },
        overlap: ScheduleOverlap::Allow,
        catch_up,
    }
}

fn registration(app: &AppId, schedules: Vec<ScheduleDescriptor>) -> RegisterSchedules {
    RegisterSchedules {
        app_id: app.clone(),
        deployment_id: DeploymentId::mint(),
        schedules,
    }
}

fn activation(registration: &RegisterSchedules, revision: i64) -> ActivateSchedules {
    ActivateSchedules {
        app_id: registration.app_id.clone(),
        deployment_id: registration.deployment_id.clone(),
        revision: revision.try_into().unwrap(),
    }
}

fn assignment(app: &AppId) -> Assignment {
    Assignment {
        app_id: app.clone(),
        worker_id: WorkerId::mint(),
        revision: 1.try_into().unwrap(),
        expires_at: i64::MAX.try_into().unwrap(),
    }
}

async fn rows(fixture: &Fixture, table: &str, filter: Value) -> Vec<Value> {
    let Output::Rows { mut rows, .. } = fixture
        .database()
        .await
        .collection(table)
        .unwrap()
        .find(filter, value!({"limit":256}))
        .await
        .unwrap()
    else {
        panic!("expected rows");
    };
    rows.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    rows
}

async fn scoped_rows(fixture: &Fixture, table: &str, app: &AppId) -> Vec<Value> {
    rows(fixture, table, value!({"app_id":app.as_str()})).await
}

async fn patch(fixture: &Fixture, table: &str, filter: Value, document: Value) {
    let updated = fixture
        .database()
        .await
        .collection(table)
        .unwrap()
        .execute(Operation::Update {
            filter,
            patch: document,
            many: true,
        })
        .await
        .unwrap();
    assert!(matches!(updated, Output::Count(count) if count > 0));
}

async fn schedule(fixture: &Fixture, app: &AppId, name: &str) -> Value {
    let mut records = rows(
        fixture,
        "schedules",
        value!({"app_id":app.as_str(), "name":name}),
    )
    .await;
    assert_eq!(records.len(), 1);
    records.pop().unwrap()
}

fn schedule_id(record: &Value) -> ScheduleId {
    ScheduleId::parse(record["id"].as_str().unwrap()).unwrap()
}

async fn make_due(fixture: &Fixture, app: &AppId, name: &str) -> ScheduleId {
    patch(
        fixture,
        "schedules",
        value!({"app_id":app.as_str(), "name":name}),
        value!({"next_at":0, "anchor_at":0}),
    )
    .await;
    schedule_id(&schedule(fixture, app, name).await)
}

fn instants(jobs: &[JobSpec]) -> Vec<i64> {
    jobs.iter()
        .map(|job| match &job.operation {
            JobOperation::Cron { scheduled_at, .. } => scheduled_at.get(),
            operation => panic!("expected cron job, got {operation:?}"),
        })
        .collect()
}

async fn claim(queue: &Queue, owner: &Assignment) -> Delivery {
    queue
        .claim(owner)
        .await
        .unwrap()
        .expect("expected deliverable job")
        .delivery()
        .clone()
}

async fn settle(queue: &Queue, owner: &Assignment, delivery: &Delivery, outcome: JobOutcome) {
    queue
        .settle(
            owner,
            &Settlement {
                delivery: delivery.clone(),
                outcome,
                successors: vec![],
            },
        )
        .await
        .unwrap();
}

async fn prepare_activate(
    scheduler: &Scheduler,
    registration: &RegisterSchedules,
    revision: i64,
) -> JobSpec {
    scheduler.prepare(registration).await.unwrap();
    scheduler
        .activate(&activation(registration, revision))
        .await
        .unwrap()
}

async fn preparation(fixture: &Fixture) {
    let (scheduler, queue) = host(fixture).await;
    let app = AppId::mint();
    let metadata = registration(
        &app,
        vec![
            descriptor("later", ScheduleCatchUp::Skip),
            descriptor("earlier", ScheduleCatchUp::Backfill { max: 5 }),
        ],
    );
    scheduler.prepare(&metadata).await.unwrap();
    let prepared = scoped_rows(fixture, "schedule_deployments", &app).await;
    assert_eq!(prepared.len(), 1);
    let mut reordered = metadata.clone();
    reordered.schedules.reverse();
    scheduler.prepare(&reordered).await.unwrap();
    assert_eq!(
        scoped_rows(fixture, "schedule_deployments", &app).await,
        prepared
    );
    assert!(scoped_rows(fixture, "jobs", &app).await.is_empty());
    assert!(scheduler.due(None).await.unwrap().is_empty());
    reordered.schedules[0].overlap = ScheduleOverlap::SkipIfRunning;
    assert_eq!(scheduler.prepare(&reordered).await, Err(Error::Conflict));
    assert_eq!(
        scoped_rows(fixture, "schedule_deployments", &app).await,
        prepared
    );
    let foreign = AppId::mint();
    queue.register_scope(&foreign).await.unwrap();
    let mut wrong_scope = activation(&metadata, 1);
    wrong_scope.app_id = foreign.clone();
    assert_eq!(scheduler.activate(&wrong_scope).await, Err(Error::Denied));
    scheduler.activate(&activation(&metadata, 1)).await.unwrap();
    let id = make_due(fixture, &app, "earlier").await;
    assert_eq!(scheduler.dispatch(&foreign, &id).await, Err(Error::Denied));
    assert!(scoped_rows(fixture, "jobs", &foreign).await.is_empty());
    assert!(scoped_rows(fixture, "schedule_occurrences", &app)
        .await
        .is_empty());
}

async fn activations(fixture: &Fixture) {
    let (scheduler, queue) = host(fixture).await;
    let app = AppId::mint();
    let first = registration(
        &app,
        vec![
            descriptor("kept", ScheduleCatchUp::Skip),
            descriptor("removed", ScheduleCatchUp::Skip),
        ],
    );
    let first_activation = prepare_activate(&scheduler, &first, 1).await;
    let kept = make_due(fixture, &app, "kept").await;
    let removed = make_due(fixture, &app, "removed").await;
    let old_kept = scheduler
        .dispatch(&app, &kept)
        .await
        .unwrap()
        .jobs
        .remove(0);
    let old_removed = scheduler
        .dispatch(&app, &removed)
        .await
        .unwrap()
        .jobs
        .remove(0);
    let owner = assignment(&app);
    let old_delivery = claim(&queue, &owner).await;
    assert_eq!(old_delivery.job, first_activation);

    let next = registration(&app, vec![descriptor("kept", ScheduleCatchUp::Skip)]);
    let next_activation = prepare_activate(&scheduler, &next, 3).await;
    assert_eq!(schedule_id(&schedule(fixture, &app, "kept").await), kept);
    assert_eq!(
        schedule(fixture, &app, "removed").await["next_at"],
        value!(null)
    );
    assert!(scheduler
        .dispatch(&app, &removed)
        .await
        .unwrap()
        .jobs
        .is_empty());
    let next_id = make_due(fixture, &app, "kept").await;
    let new_job = scheduler
        .dispatch(&app, &next_id)
        .await
        .unwrap()
        .jobs
        .remove(0);
    assert_ne!(new_job.id, old_kept.id);
    assert_eq!(new_job.deployment_id(), Some(&next.deployment_id));
    let current = rows(fixture, "schedule_scopes", value!({"id":app.as_str()})).await;
    assert_eq!(
        scheduler.activate(&activation(&first, 1)).await.unwrap(),
        first_activation
    );
    assert_eq!(
        rows(fixture, "schedule_scopes", value!({"id":app.as_str()})).await,
        current
    );
    assert_eq!(
        scheduler.activate(&activation(&first, 2)).await,
        Err(Error::Conflict)
    );
    assert_eq!(
        scheduler.activate(&activation(&first, 3)).await,
        Err(Error::Conflict)
    );
    assert_eq!(
        scheduler.activate(&activation(&next, 3)).await.unwrap(),
        next_activation
    );

    let next_delivery = claim(&queue, &owner).await;
    assert_eq!(next_delivery.job, next_activation);
    settle(&queue, &owner, &next_delivery, JobOutcome::Completed).await;
    let new_delivery = claim(&queue, &owner).await;
    assert_eq!(new_delivery.job, new_job);
    settle(&queue, &owner, &new_delivery, JobOutcome::Completed).await;
    assert!(queue.claim(&owner).await.unwrap().is_none());
    settle(&queue, &owner, &old_delivery, JobOutcome::Completed).await;
    let mut remaining = vec![old_kept, old_removed];
    for _ in 0..remaining.len() {
        let delivery = claim(&queue, &owner).await;
        let index = remaining
            .iter()
            .position(|job| *job == delivery.job)
            .unwrap();
        remaining.remove(index);
        assert_eq!(delivery.job.deployment_id(), Some(&first.deployment_id));
        settle(&queue, &owner, &delivery, JobOutcome::Completed).await;
    }
    assert!(remaining.is_empty());
    assert!(queue.claim(&owner).await.unwrap().is_none());
}

async fn activation_gate(fixture: &Fixture) {
    let (scheduler, queue) = host(fixture).await;
    for outcome in [JobOutcome::Rejected, JobOutcome::Waiting] {
        let app = AppId::mint();
        let metadata = registration(
            &app,
            vec![descriptor("gated", ScheduleCatchUp::Backfill { max: 5 })],
        );
        let activation_job = prepare_activate(&scheduler, &metadata, 1).await;
        let id = make_due(fixture, &app, "gated").await;
        while scheduler.dispatch(&app, &id).await.unwrap().more {}
        let recovery = Recovery::new(queue.clone(), RecoveryOptions::default()).unwrap();
        let recovery_job = recovery.dispatch(&app).await.unwrap().unwrap();
        let owner = assignment(&app);
        let delivery = claim(&queue, &owner).await;
        assert_eq!(delivery.job, activation_job);
        settle(&queue, &owner, &delivery, outcome).await;
        let delivery = claim(&queue, &owner).await;
        assert_eq!(delivery.job, recovery_job);
        settle(&queue, &owner, &delivery, JobOutcome::Completed).await;
        assert!(queue.claim(&owner).await.unwrap().is_none());
        assert_eq!(
            scoped_rows(fixture, "schedule_occurrences", &app)
                .await
                .len(),
            5
        );
    }
}

async fn disabled_schedules(fixture: &Fixture) {
    let (scheduler, queue) = host(fixture).await;
    let app = AppId::mint();
    let metadata = registration(&app, vec![descriptor("pending", ScheduleCatchUp::Skip)]);
    let original_activation = prepare_activate(&scheduler, &metadata, 1).await;
    let id = make_due(fixture, &app, "pending").await;
    let pending = scheduler.dispatch(&app, &id).await.unwrap().jobs.remove(0);
    let disabled = registration(&app, vec![]);
    let disabled_activation = prepare_activate(&scheduler, &disabled, 2).await;
    assert!(scheduler.due(None).await.unwrap().is_empty());
    assert_eq!(
        schedule(fixture, &app, "pending").await["next_at"],
        value!(null)
    );
    assert!(scheduler.dispatch(&app, &id).await.unwrap().jobs.is_empty());
    assert_eq!(
        scoped_rows(fixture, "schedule_occurrences", &app)
            .await
            .len(),
        1
    );
    let owner = assignment(&app);
    let delivery = claim(&queue, &owner).await;
    assert_eq!(delivery.job, original_activation);
    settle(&queue, &owner, &delivery, JobOutcome::Completed).await;
    let delivery = claim(&queue, &owner).await;
    assert_eq!(delivery.job, pending);
    settle(&queue, &owner, &delivery, JobOutcome::Completed).await;
    assert_eq!(claim(&queue, &owner).await.job, disabled_activation);
}

async fn replicas(fixture: &Fixture) {
    let (first, first_queue) = host(fixture).await;
    let (second, second_queue) = host(fixture).await;
    let app = AppId::mint();
    let metadata = registration(
        &app,
        vec![descriptor(
            "replicated",
            ScheduleCatchUp::Backfill { max: 5 },
        )],
    );
    let (left, right) = futures::join!(first.prepare(&metadata), second.prepare(&metadata));
    left.unwrap();
    right.unwrap();
    let request = activation(&metadata, 1);
    let (left, right) = futures::join!(first.activate(&request), second.activate(&request));
    let activation_job = left.unwrap();
    assert_eq!(right.unwrap(), activation_job);
    let id = make_due(fixture, &app, "replicated").await;
    let (left, right) = futures::join!(first.dispatch(&app, &id), second.dispatch(&app, &id));
    let left = left.unwrap();
    let right = right.unwrap();
    assert!(left.more && right.more);
    let mut jobs = left.jobs;
    jobs.extend(right.jobs);
    let mut times = instants(&jobs);
    times.sort_unstable();
    assert_eq!(times, [0, 1_000, 2_000, 3_000]);
    drop(first);
    drop(first_queue);
    drop(second);
    drop(second_queue);
    let (reopened, queue) = host(fixture).await;
    let last = reopened.dispatch(&app, &id).await.unwrap();
    assert!(!last.more);
    assert_eq!(instants(&last.jobs), [4_000]);
    jobs.extend(last.jobs);
    assert_eq!(
        scoped_rows(fixture, "schedule_occurrences", &app)
            .await
            .len(),
        jobs.len()
    );
    let owner = assignment(&app);
    let delivery = claim(&queue, &owner).await;
    assert_eq!(delivery.job, activation_job);
    settle(&queue, &owner, &delivery, JobOutcome::Completed).await;
    jobs.sort_by_key(|job| job.available_at.get());
    for expected in jobs {
        let delivery = claim(&queue, &owner).await;
        assert_eq!(delivery.job, expected);
        settle(&queue, &owner, &delivery, JobOutcome::Completed).await;
    }
    assert!(queue.claim(&owner).await.unwrap().is_none());
}

async fn catch_up(fixture: &Fixture) {
    let (scheduler, queue) = host(fixture).await;
    let app = AppId::mint();
    let metadata = registration(
        &app,
        vec![descriptor("bounded", ScheduleCatchUp::Backfill { max: 5 })],
    );
    prepare_activate(&scheduler, &metadata, 1).await;
    let id = make_due(fixture, &app, "bounded").await;
    let first = scheduler.dispatch(&app, &id).await.unwrap();
    assert_eq!(instants(&first.jobs), [0, 1_000]);
    assert!(first.more);
    let saved = schedule(fixture, &app, "bounded").await;
    assert!(saved["catch_up_until"].as_i64().unwrap() > 10_000);
    assert_eq!(saved["catch_up_remaining"], value!(3));
    // Represent a page captured before a long manager outage without sleeping.
    patch(
        fixture,
        "schedules",
        value!({"id":id.as_str()}),
        value!({"catch_up_until":10_000}),
    )
    .await;
    drop(scheduler);
    drop(queue);
    let (_, queue) = host(fixture).await;
    let scheduler = Scheduler::new(
        queue,
        Options {
            max_schedules: 8,
            max_backfill: 2,
            min_interval_ms: 1_000,
            page_size: 2,
        },
    )
    .unwrap();
    let second = scheduler.dispatch(&app, &id).await.unwrap();
    assert_eq!(instants(&second.jobs), [2_000, 3_000]);
    assert!(second.more);
    let saved = schedule(fixture, &app, "bounded").await;
    assert_eq!(saved["catch_up_until"], value!(10_000));
    assert_eq!(saved["catch_up_remaining"], value!(1));
    let final_page = scheduler.dispatch(&app, &id).await.unwrap();
    assert_eq!(instants(&final_page.jobs), [4_000]);
    assert!(!final_page.more);
    let saved = schedule(fixture, &app, "bounded").await;
    assert_eq!(saved["next_at"], value!(11_000));
    assert_eq!(saved["catch_up_until"], value!(null));
    assert_eq!(saved["catch_up_remaining"], value!(null));
    let next_sweep = scheduler.dispatch(&app, &id).await.unwrap();
    assert_eq!(instants(&next_sweep.jobs), [11_000, 12_000]);
    assert!(!next_sweep.more);
    assert_eq!(
        scoped_rows(fixture, "schedule_occurrences", &app)
            .await
            .len(),
        7
    );
}

async fn skip(fixture: &Fixture) {
    let (scheduler, _) = host(fixture).await;
    let app = AppId::mint();
    let metadata = registration(&app, vec![descriptor("skip", ScheduleCatchUp::Skip)]);
    prepare_activate(&scheduler, &metadata, 1).await;
    let id = make_due(fixture, &app, "skip").await;
    let page = scheduler.dispatch(&app, &id).await.unwrap();
    assert_eq!(instants(&page.jobs), [0]);
    assert!(!page.more);
    let row = schedule(fixture, &app, "skip").await;
    let persisted = rows(fixture, "jobs", value!({"id":page.jobs[0].id.as_str()})).await;
    assert_eq!(persisted.len(), 1);
    assert!(row["next_at"].as_i64().unwrap() > persisted[0]["created_at"].as_i64().unwrap());
    assert_eq!(row["catch_up_until"], value!(null));
    assert_eq!(
        scoped_rows(fixture, "schedule_occurrences", &app)
            .await
            .len(),
        1
    );
}

async fn due_pages(fixture: &Fixture) {
    let (scheduler, _) = host(fixture).await;
    let app = AppId::mint();
    let descriptors = (0..5)
        .map(|index| descriptor(&format!("schedule-{index}"), ScheduleCatchUp::Skip))
        .collect();
    let metadata = registration(&app, descriptors);
    prepare_activate(&scheduler, &metadata, 1).await;
    let mut ids = Vec::new();
    for descriptor in &metadata.schedules {
        ids.push(make_due(fixture, &app, &descriptor.name).await);
    }
    ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    patch(
        fixture,
        "schedules",
        value!({"id":ids[1].as_str()}),
        value!({"next_at":null}),
    )
    .await;
    let expected: Vec<_> = ids
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != 1)
        .map(|(_, id)| id.clone())
        .collect();
    let mut found = Vec::new();
    loop {
        let page = scheduler.due(found.last()).await.unwrap();
        assert!(page.len() <= 2);
        if page.is_empty() {
            break;
        }
        for due in page {
            assert_eq!(due.app_id, app);
            found.push(due.schedule_id);
        }
    }
    assert_eq!(found, expected);
}

async fn fault(fixture: &Fixture, table: &str, install: bool) {
    assert!(matches!(table, "schedules" | "recovery_scopes"));
    match &fixture.admin {
        Admin::Sqlite(connection) => {
            connection.execute_batch(&if install {
                format!("CREATE TRIGGER scheduler_fault BEFORE UPDATE ON {table} BEGIN SELECT RAISE(ABORT,'scheduler fault'); END;")
            } else { "DROP TRIGGER scheduler_fault;".into() }).unwrap();
        }
        Admin::Postgres(connection) => {
            connection.batch_execute(&if install {
                format!("CREATE FUNCTION workflow_manager.scheduler_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'scheduler fault'; END $$; CREATE TRIGGER scheduler_fault BEFORE UPDATE ON workflow_manager.{table} FOR EACH ROW EXECUTE FUNCTION workflow_manager.scheduler_fault();")
            } else { format!("DROP TRIGGER scheduler_fault ON workflow_manager.{table}; DROP FUNCTION workflow_manager.scheduler_fault();") }).await.unwrap();
        }
    }
}

async fn dispatch_rollback(fixture: &Fixture) {
    let (scheduler, _) = host(fixture).await;
    let app = AppId::mint();
    let metadata = registration(
        &app,
        vec![descriptor("atomic", ScheduleCatchUp::Backfill { max: 5 })],
    );
    prepare_activate(&scheduler, &metadata, 1).await;
    let id = make_due(fixture, &app, "atomic").await;
    let original = schedule(fixture, &app, "atomic").await;
    let jobs = scoped_rows(fixture, "jobs", &app).await;
    fault(fixture, "schedules", true).await;
    assert!(scheduler.dispatch(&app, &id).await.is_err());
    assert_eq!(schedule(fixture, &app, "atomic").await, original);
    assert_eq!(scoped_rows(fixture, "jobs", &app).await, jobs);
    assert!(scoped_rows(fixture, "schedule_occurrences", &app)
        .await
        .is_empty());
    fault(fixture, "schedules", false).await;
    let page = scheduler.dispatch(&app, &id).await.unwrap();
    assert_eq!(instants(&page.jobs), [0, 1_000]);
    assert_eq!(
        scoped_rows(fixture, "schedule_occurrences", &app)
            .await
            .len(),
        page.jobs.len()
    );
}

async fn activation_rollback(fixture: &Fixture) {
    let (scheduler, _) = host(fixture).await;
    let app = AppId::mint();
    let first = registration(&app, vec![descriptor("original", ScheduleCatchUp::Skip)]);
    prepare_activate(&scheduler, &first, 1).await;
    let replacement = registration(&app, vec![descriptor("replacement", ScheduleCatchUp::Skip)]);
    scheduler.prepare(&replacement).await.unwrap();
    let prior_schedules = scoped_rows(fixture, "schedules", &app).await;
    let jobs = scoped_rows(fixture, "jobs", &app).await;
    let activations = scoped_rows(fixture, "schedule_activations", &app).await;
    let scope = rows(fixture, "schedule_scopes", value!({"id":app.as_str()})).await;
    let recovery = rows(fixture, "recovery_scopes", value!({"id":app.as_str()})).await;
    fault(fixture, "recovery_scopes", true).await;
    assert!(scheduler
        .activate(&activation(&replacement, 2))
        .await
        .is_err());
    assert_eq!(
        scoped_rows(fixture, "schedules", &app).await,
        prior_schedules
    );
    assert_eq!(scoped_rows(fixture, "jobs", &app).await, jobs);
    assert_eq!(
        scoped_rows(fixture, "schedule_activations", &app).await,
        activations
    );
    assert_eq!(
        rows(fixture, "schedule_scopes", value!({"id":app.as_str()})).await,
        scope
    );
    assert_eq!(
        rows(fixture, "recovery_scopes", value!({"id":app.as_str()})).await,
        recovery
    );
    fault(fixture, "recovery_scopes", false).await;
    let accepted = scheduler
        .activate(&activation(&replacement, 2))
        .await
        .unwrap();
    assert_eq!(accepted.deployment_id(), Some(&replacement.deployment_id));
    assert_eq!(
        schedule(fixture, &app, "original").await["next_at"],
        value!(null)
    );
    assert!(schedule(fixture, &app, "replacement").await["next_at"]
        .as_i64()
        .is_some());
    assert_eq!(
        rows(fixture, "recovery_scopes", value!({"id":app.as_str()})).await[0]["deployment_id"],
        value!(replacement.deployment_id.as_str())
    );
}

async fn interpretation(fixture: &Fixture) {
    let (scheduler, _) = host(fixture).await;
    let app = AppId::mint();
    let metadata = registration(&app, vec![descriptor("versioned", ScheduleCatchUp::Skip)]);
    scheduler.prepare(&metadata).await.unwrap();
    patch(
        fixture,
        "schedule_deployments",
        value!({"id":metadata.deployment_id.as_str()}),
        value!({"interpretation":"other-calendar"}),
    )
    .await;
    assert_eq!(
        scheduler.activate(&activation(&metadata, 1)).await,
        Err(Error::Conflict)
    );
    assert!(scoped_rows(fixture, "jobs", &app).await.is_empty());
    patch(
        fixture,
        "schedule_deployments",
        value!({"id":metadata.deployment_id.as_str()}),
        value!({"interpretation":zeroship_workflow_calendar::interpretation()}),
    )
    .await;
    scheduler.activate(&activation(&metadata, 1)).await.unwrap();
    let id = make_due(fixture, &app, "versioned").await;
    let before = schedule(fixture, &app, "versioned").await;
    patch(
        fixture,
        "schedule_deployments",
        value!({"id":metadata.deployment_id.as_str()}),
        value!({"interpretation":"other-calendar"}),
    )
    .await;
    let (reopened, _) = host(fixture).await;
    assert_eq!(reopened.dispatch(&app, &id).await, Err(Error::Conflict));
    assert_eq!(schedule(fixture, &app, "versioned").await, before);
    assert!(scoped_rows(fixture, "schedule_occurrences", &app)
        .await
        .is_empty());
}

async fn immutable_schedule_projection(fixture: &Fixture) {
    let (scheduler, _) = host(fixture).await;
    let app = AppId::mint();
    let metadata = registration(&app, vec![descriptor("immutable", ScheduleCatchUp::Skip)]);
    prepare_activate(&scheduler, &metadata, 1).await;
    let id = make_due(fixture, &app, "immutable").await;
    let jobs = scoped_rows(fixture, "jobs", &app).await;
    let mut changed = metadata.schedules[0].clone();
    changed.schedule = ScheduleTiming::Interval {
        interval_ms: 2_000,
        anchor: IntervalAnchor::Epoch,
    };
    patch(
        fixture,
        "schedules",
        value!({"id":id.as_str()}),
        value!({"definition":serde_json::to_string(&changed).unwrap()}),
    )
    .await;
    let before = schedule(fixture, &app, "immutable").await;
    assert_eq!(scheduler.dispatch(&app, &id).await, Err(Error::Storage));
    assert_eq!(schedule(fixture, &app, "immutable").await, before);
    assert_eq!(scoped_rows(fixture, "jobs", &app).await, jobs);
    assert!(scoped_rows(fixture, "schedule_occurrences", &app)
        .await
        .is_empty());
}

async fn activation_job_identity(fixture: &Fixture) {
    let (scheduler, _) = host(fixture).await;
    let app = AppId::mint();
    let metadata = registration(
        &app,
        vec![descriptor("bound-activation", ScheduleCatchUp::Skip)],
    );
    let accepted = prepare_activate(&scheduler, &metadata, 1).await;
    let request = activation(&metadata, 1);
    let before = scoped_rows(fixture, "schedules", &app).await;
    let scopes = rows(fixture, "schedule_scopes", value!({"id":app.as_str()})).await;

    patch(
        fixture,
        "jobs",
        value!({"id":accepted.id.as_str()}),
        value!({
            "operation":serde_json::to_string(&JobOperation::Reconcile {}).unwrap(),
        }),
    )
    .await;
    assert_eq!(scheduler.activate(&request).await, Err(Error::Storage));
    assert_eq!(scoped_rows(fixture, "schedules", &app).await, before);
    assert_eq!(
        rows(fixture, "schedule_scopes", value!({"id":app.as_str()})).await,
        scopes
    );

    patch(
        fixture,
        "jobs",
        value!({"id":accepted.id.as_str()}),
        value!({
            "operation":serde_json::to_string(&accepted.operation).unwrap(),
            "deployment_id":DeploymentId::mint().as_str(),
        }),
    )
    .await;
    assert_eq!(scheduler.activate(&request).await, Err(Error::Storage));
    assert_eq!(scoped_rows(fixture, "schedules", &app).await, before);
    assert_eq!(
        rows(fixture, "schedule_scopes", value!({"id":app.as_str()})).await,
        scopes
    );

    patch(
        fixture,
        "jobs",
        value!({"id":accepted.id.as_str()}),
        value!({
            "deployment_id":accepted.deployment_id().unwrap().as_str(),
        }),
    )
    .await;
    assert_eq!(scheduler.activate(&request).await.unwrap(), accepted);
}

async fn occurrence_job_identity(fixture: &Fixture) {
    let (scheduler, queue) = host(fixture).await;
    let app = AppId::mint();
    let metadata = registration(
        &app,
        vec![descriptor("bound-occurrence", ScheduleCatchUp::Skip)],
    );
    prepare_activate(&scheduler, &metadata, 1).await;
    let id = make_due(fixture, &app, "bound-occurrence").await;
    let original = scheduler.dispatch(&app, &id).await.unwrap().jobs.remove(0);
    let unrelated = JobSpec {
        id: JobId::mint(),
        operation: JobOperation::Collect {},
        ..original.clone()
    };
    queue.submit(&unrelated).await.unwrap();
    patch(
        fixture,
        "schedule_occurrences",
        value!({"job_id":original.id.as_str()}),
        value!({
            "job_id":unrelated.id.as_str(),
        }),
    )
    .await;
    make_due(fixture, &app, "bound-occurrence").await;
    let before = schedule(fixture, &app, "bound-occurrence").await;
    let jobs = scoped_rows(fixture, "jobs", &app).await;
    let occurrences = scoped_rows(fixture, "schedule_occurrences", &app).await;
    assert_eq!(scheduler.dispatch(&app, &id).await, Err(Error::Storage));
    assert_eq!(schedule(fixture, &app, "bound-occurrence").await, before);
    assert_eq!(scoped_rows(fixture, "jobs", &app).await, jobs);
    assert_eq!(
        scoped_rows(fixture, "schedule_occurrences", &app).await,
        occurrences
    );

    patch(
        fixture,
        "schedule_occurrences",
        value!({"job_id":unrelated.id.as_str()}),
        value!({
            "job_id":original.id.as_str(),
        }),
    )
    .await;
    let replay = scheduler.dispatch(&app, &id).await.unwrap();
    assert_eq!(replay.jobs, vec![original]);
    assert!(!replay.more);
}

async fn orphan_cron(fixture: &Fixture) {
    let (scheduler, queue) = host(fixture).await;
    let app = AppId::mint();
    let metadata = registration(&app, vec![descriptor("linked", ScheduleCatchUp::Skip)]);
    let activation_job = prepare_activate(&scheduler, &metadata, 1).await;
    let id = make_due(fixture, &app, "linked").await;
    let cron = scheduler.dispatch(&app, &id).await.unwrap().jobs.remove(0);
    let mut linkage = rows(
        fixture,
        "schedule_occurrences",
        value!({"job_id":cron.id.as_str()}),
    )
    .await;
    assert_eq!(linkage.len(), 1);
    let db = fixture.database().await;
    let deleted = db
        .collection("schedule_occurrences")
        .unwrap()
        .execute(Operation::Purge {
            filter: value!({"job_id":cron.id.as_str()}),
            many: true,
        })
        .await
        .unwrap();
    assert!(matches!(deleted, Output::Count(1)));
    let owner = assignment(&app);
    let activation_delivery = match queue.claim(&owner).await {
        Err(Error::Storage) => None,
        Ok(Some(grant)) => {
            assert_eq!(grant.delivery().job, activation_job);
            match queue.claim(&owner).await {
                Err(Error::Storage) | Ok(None) => {}
                other => panic!("unactivated orphan cron was deliverable: {other:?}"),
            }
            Some(grant.delivery().clone())
        }
        other => panic!("unexpected orphan queue result: {other:?}"),
    };
    assert_eq!(
        rows(fixture, "jobs", value!({"id":cron.id.as_str()})).await[0]["state"],
        value!("ready")
    );
    db.collection("schedule_occurrences")
        .unwrap()
        .insert(linkage.pop().unwrap())
        .await
        .unwrap();
    let activation_delivery = if let Some(delivery) = activation_delivery {
        delivery
    } else {
        claim(&queue, &owner).await
    };
    assert_eq!(activation_delivery.job, activation_job);
    settle(&queue, &owner, &activation_delivery, JobOutcome::Completed).await;
    assert_eq!(claim(&queue, &owner).await.job, cron);
}
