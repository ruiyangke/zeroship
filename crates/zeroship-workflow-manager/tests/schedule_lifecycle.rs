#![recursion_limit = "256"]
#![allow(
    clippy::future_not_send,
    reason = "native fixtures share their compio runtime"
)]

#[allow(dead_code, reason = "other manager suites share these fixture helpers")]
mod support;

#[allow(
    dead_code,
    reason = "the native declaration also defines unrelated manager tables"
)]
#[path = "../src/models/schema_definition.rs"]
mod native_schema;

use native_schema::schema::{
    deployment_holds, jobs, recovery_duties, recovery_scopes, schedule_activations,
    schedule_disables, schedule_occurrences, schedule_scopes, schedules,
};
use std::{cell::RefCell, future::Future, pin::Pin, rc::Rc, time::Duration};
use support::{Admin, Backend, Fixture};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{Assignment, RequestId, WorkerId},
    workflow_deployments::{HoldGeneration, HoldReceipt},
    workflow_jobs::{DeploymentId, JobId, JobOperation, JobOutcome, JobSpec, Settlement},
    workflow_schedules::{
        ActivateSchedules, DisableSchedules, RegisterSchedules, ScheduleDescriptor, ScheduleId,
    },
};
use zeroship_data_orm::orm::{Database, FromRow, Insertable};
use zeroship_workflow_calendar::{
    IntervalAnchor, ScheduleCatchUp, ScheduleOverlap, ScheduleTiming,
};
use zeroship_workflow_manager::{
    driver::{Driver, Options as DriverOptions},
    recovery::{DutyKind, Options as RecoveryOptions, Recovery},
    retention::HoldClient,
    scheduling::{Options, Scheduler},
    Error, Queue,
};

macro_rules! case {
    ($sqlite:ident, $postgres:ident, $body:ident) => {
        #[compio::test]
        async fn $sqlite() {
            Box::pin($body(&Fixture::new(Backend::Sqlite).await)).await;
        }
        #[compio::test]
        async fn $postgres() {
            Box::pin($body(&Fixture::new(Backend::Postgres).await)).await;
        }
    };
}

case!(
    sqlite_disable_history_fences_activation,
    postgres_disable_history_fences_activation,
    history
);
case!(
    sqlite_restore_preserves_calendar_and_accepted_work,
    postgres_restore_preserves_calendar_and_accepted_work,
    restore
);
case!(
    sqlite_staged_deployment_replaces_only_future_calendar,
    postgres_staged_deployment_replaces_only_future_calendar,
    staged
);
case!(
    sqlite_disabled_apps_do_not_hide_due_work,
    postgres_disabled_apps_do_not_hide_due_work,
    eligibility
);
case!(
    sqlite_lifecycle_replicas_serialize_with_dispatch,
    postgres_lifecycle_replicas_serialize_with_dispatch,
    replicas
);
case!(
    sqlite_lifecycle_failure_rolls_back,
    postgres_lifecycle_failure_rolls_back,
    rollback
);
case!(
    sqlite_restore_refuses_corrupt_calendar,
    postgres_restore_refuses_corrupt_calendar,
    corrupt_restore
);
case!(
    sqlite_disable_fences_inflight_hold_acquisition,
    postgres_disable_fences_inflight_hold_acquisition,
    held_io
);

macro_rules! row {
    ($name:ident, $table:ident, $($field:ident: $ty:ty),+ $(,)?) => {
        #[derive(Debug, Clone, PartialEq, Eq, FromRow)]
        #[orm(entity = $table)]
        struct $name { $($field: $ty),+ }
    };
}
row!(Calendar, schedules, id:String, app_id:String, name:String, activation_id:String,
    revision:i64, definition:String, next_at:Option<i64>, anchor_at:i64,
    catch_up_until:Option<i64>, catch_up_remaining:Option<i64>);
row!(Scope, schedule_scopes, id:String, revision:i64, enabled:bool, activation_id:Option<String>);
row!(Disabled, schedule_disables, id:String, app_id:String, revision:i64, created_at:i64);
row!(Occurrence, schedule_occurrences, id:String, app_id:String, schedule_id:String,
    revision:i64, scheduled_at:i64, run_id:String, job_id:String, activation_id:String);
row!(RecoveryRow, recovery_scopes, id:String, deployment_id:String, activation_revision:i64);
row!(DutyRow, recovery_duties, id:String, app_id:String, kind:String, next_due_at:i64, pending_job_id:Option<String>);
row!(Hold, deployment_holds, id:String, app_id:String, deployment_id:String, holder_id:String,
    deploy_hash:Option<String>, generation:i64, state:String);
row!(Job, jobs, id:String, app_id:String, deployment_id:Option<String>, operation:String,
    state:String, attempt:i64);
row!(Activation, schedule_activations, id:String, app_id:String, deployment_id:String,
    revision:i64, activated_at:i64);

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    calendars: Vec<Calendar>,
    scopes: Vec<Scope>,
    disables: Vec<Disabled>,
    occurrences: Vec<Occurrence>,
    recovery: Vec<RecoveryRow>,
    duties: Vec<DutyRow>,
    holds: Vec<Hold>,
    jobs: Vec<Job>,
    activations: Vec<Activation>,
}

async fn snapshot(db: &Database) -> Snapshot {
    macro_rules! rows {
        ($table:ident, $row:ident) => {
            db.entity::<$table::Entity>()
                .unwrap()
                .query()
                .order_by($table::id.asc())
                .all::<$row>()
                .await
                .unwrap()
        };
    }
    Snapshot {
        calendars: rows!(schedules, Calendar),
        scopes: rows!(schedule_scopes, Scope),
        disables: rows!(schedule_disables, Disabled),
        occurrences: rows!(schedule_occurrences, Occurrence),
        recovery: rows!(recovery_scopes, RecoveryRow),
        duties: rows!(recovery_duties, DutyRow),
        holds: rows!(deployment_holds, Hold),
        jobs: rows!(jobs, Job),
        activations: rows!(schedule_activations, Activation),
    }
}

const fn options() -> Options {
    Options {
        max_schedules: 8,
        max_backfill: 5,
        min_interval_ms: 1_000,
        page_size: 2,
    }
}

async fn host(fixture: &Fixture) -> (Scheduler, Queue) {
    let queue = Queue::connect(
        fixture.binding(),
        fixture.url(),
        zeroship_workflow_manager::Options::default(),
        support::synthetic_holds(),
    )
    .await
    .unwrap();
    (Scheduler::new(queue.clone(), options()).unwrap(), queue)
}

fn metadata(app: &AppId) -> RegisterSchedules {
    RegisterSchedules {
        app_id: app.clone(),
        deployment_id: DeploymentId::mint(),
        schedules: vec![ScheduleDescriptor {
            name: "scheduled".into(),
            workflow_name: "workflow".into(),
            schedule: ScheduleTiming::Interval {
                interval_ms: 1_000,
                anchor: IntervalAnchor::Deploy,
            },
            overlap: ScheduleOverlap::Allow,
            catch_up: ScheduleCatchUp::Backfill { max: 5 },
        }],
    }
}

fn activate(metadata: &RegisterSchedules, revision: i64) -> ActivateSchedules {
    ActivateSchedules {
        app_id: metadata.app_id.clone(),
        deployment_id: metadata.deployment_id.clone(),
        revision: revision.try_into().unwrap(),
    }
}

fn disable(app: &AppId, revision: i64) -> DisableSchedules {
    DisableSchedules {
        app_id: app.clone(),
        revision: revision.try_into().unwrap(),
    }
}

async fn make_due(db: &Database, app: &AppId) -> ScheduleId {
    assert_eq!(
        db.entity::<schedules::Entity>()
            .unwrap()
            .update_many(
                schedules::app_id.eq(app.as_str()).unwrap(),
                schedules::next_at
                    .set(Some(1_250_i64))
                    .unwrap()
                    .and(schedules::anchor_at.set(250_i64).unwrap())
                    .unwrap(),
            )
            .await
            .unwrap(),
        1
    );
    let row = db
        .entity::<schedules::Entity>()
        .unwrap()
        .query()
        .filter(schedules::app_id.eq(app.as_str()).unwrap())
        .first::<Calendar>()
        .await
        .unwrap()
        .unwrap();
    ScheduleId::parse(&row.id).unwrap()
}

fn instants(jobs: &[JobSpec]) -> Vec<i64> {
    jobs.iter()
        .map(|job| match job.operation {
            JobOperation::Cron { scheduled_at, .. } => scheduled_at.get(),
            _ => panic!("expected cron occurrence"),
        })
        .collect()
}

const fn frontier(row: &Calendar) -> (Option<i64>, i64, Option<i64>, Option<i64>) {
    (
        row.next_at,
        row.anchor_at,
        row.catch_up_until,
        row.catch_up_remaining,
    )
}

async fn history(fixture: &Fixture) {
    let (scheduler, _) = host(fixture).await;
    let db = fixture.database().await;
    let app = AppId::mint();
    let command = disable(&app, 2);
    assert_eq!(scheduler.disable(&command).await.unwrap(), command);
    assert_eq!(scheduler.disable(&command).await.unwrap(), command);
    let stopped = snapshot(&db).await;
    assert_eq!(stopped.disables.len(), 1);
    RequestId::parse(&stopped.disables[0].id).unwrap();
    assert!(!stopped.scopes[0].enabled);
    assert_eq!(stopped.scopes[0].activation_id, None);
    assert!(stopped.jobs.is_empty() && stopped.holds.is_empty() && stopped.recovery.is_empty());
    let metadata = metadata(&app);
    scheduler.prepare(&metadata).await.unwrap();
    assert_eq!(
        scheduler.activate(&activate(&metadata, 1)).await,
        Err(Error::Conflict)
    );
    assert_eq!(
        scheduler.activate(&activate(&metadata, 2)).await,
        Err(Error::Conflict)
    );
    assert_eq!(
        scheduler.disable(&disable(&app, 1)).await,
        Err(Error::Conflict)
    );
    let job = scheduler.activate(&activate(&metadata, 3)).await.unwrap();
    let running = snapshot(&db).await;
    assert!(running.scopes[0].enabled);
    assert_eq!(scheduler.disable(&command).await.unwrap(), command);
    assert_eq!(
        scheduler.disable(&disable(&app, 3)).await,
        Err(Error::Conflict)
    );
    assert_eq!(
        scheduler.activate(&activate(&metadata, 3)).await.unwrap(),
        job
    );
    assert_eq!(snapshot(&db).await, running);
    scheduler.disable(&disable(&app, 4)).await.unwrap();
    let stopped_again = snapshot(&db).await;
    assert_eq!(
        scheduler.activate(&activate(&metadata, 3)).await.unwrap(),
        job
    );
    assert_eq!(
        scheduler.activate(&activate(&metadata, 4)).await,
        Err(Error::Conflict)
    );
    assert_eq!(snapshot(&db).await, stopped_again);
}

async fn settle_until(queue: &Queue, assignment: &Assignment, target: &JobId, blocked: &[JobSpec]) {
    for _ in 0..16 {
        let grant = queue
            .claim(assignment)
            .await
            .unwrap()
            .expect("expected retained delivery");
        let delivery = grant.delivery().clone();
        assert!(
            !blocked.iter().any(|job| job.id == delivery.job.id),
            "cron escaped its activation gate"
        );
        let found = &delivery.job.id == target;
        queue
            .settle(
                assignment,
                &Settlement {
                    delivery,
                    outcome: JobOutcome::Completed {},
                    successors: vec![],
                },
            )
            .await
            .unwrap();
        if found {
            return;
        }
    }
    panic!("retained delivery was not reachable");
}

async fn restore(fixture: &Fixture) {
    let (scheduler, queue) = host(fixture).await;
    let db = fixture.database().await;
    let app = AppId::mint();
    let metadata = metadata(&app);
    scheduler.prepare(&metadata).await.unwrap();
    let original = scheduler.activate(&activate(&metadata, 1)).await.unwrap();
    let id = make_due(&db, &app).await;
    let first = scheduler.dispatch(&app, &id).await.unwrap();
    assert_eq!(instants(&first.jobs), [1_250, 2_250]);
    assert!(first.more);
    let recovery = Recovery::new(queue.clone(), RecoveryOptions::default()).unwrap();
    let recovery_job = recovery
        .dispatch(&app, DutyKind::Reconcile)
        .await
        .unwrap()
        .unwrap();
    let before = snapshot(&db).await;
    scheduler.disable(&disable(&app, 2)).await.unwrap();
    assert!(scheduler.due(None).await.unwrap().is_empty());
    assert!(scheduler.dispatch(&app, &id).await.unwrap().jobs.is_empty());
    let stopped = snapshot(&db).await;
    assert_eq!(stopped.calendars, before.calendars);
    assert_eq!(stopped.occurrences, before.occurrences);
    assert_eq!(stopped.holds, before.holds);
    assert_eq!(stopped.recovery, before.recovery);
    assert_eq!(stopped.duties, before.duties);
    assert_eq!(
        recovery
            .dispatch(&app, DutyKind::Reconcile)
            .await
            .unwrap()
            .unwrap(),
        recovery_job
    );
    assert_eq!(
        queue
            .release_deployment(&app, &metadata.deployment_id)
            .await,
        Err(Error::Conflict)
    );
    let assignment = Assignment {
        app_id: app.clone(),
        worker_id: WorkerId::mint(),
        revision: 1.try_into().unwrap(),
        expires_at: i64::MAX.try_into().unwrap(),
    };
    settle_until(&queue, &assignment, &original.id, &first.jobs).await;
    settle_until(&queue, &assignment, &first.jobs[0].id, &[]).await;

    let (reopened, reopened_queue) = host(fixture).await;
    let restored = reopened.activate(&activate(&metadata, 3)).await.unwrap();
    assert_ne!(restored.id, original.id);
    let resumed = snapshot(&db).await;
    assert_eq!(
        frontier(&resumed.calendars[0]),
        frontier(&before.calendars[0])
    );
    assert_eq!(resumed.calendars[0].id, before.calendars[0].id);
    assert_eq!(resumed.calendars[0].activation_id, restored.id.as_str());
    assert_eq!(resumed.occurrences, before.occurrences);
    assert_eq!(
        resumed
            .duties
            .iter()
            .find(|duty| duty.kind == "reconcile")
            .unwrap()
            .pending_job_id,
        Some(recovery_job.id.as_str().into())
    );
    let second = reopened.dispatch(&app, &id).await.unwrap();
    assert_eq!(instants(&second.jobs), [3_250, 4_250]);
    assert!(second.more);
    let third = reopened.dispatch(&app, &id).await.unwrap();
    assert_eq!(instants(&third.jobs), [5_250]);
    assert!(!third.more);
    let final_calendar = snapshot(&db).await.calendars.remove(0);
    assert!(final_calendar.next_at.unwrap() > before.calendars[0].catch_up_until.unwrap());
    assert_eq!(final_calendar.anchor_at, 250);
    let mut new_jobs = second.jobs.clone();
    new_jobs.extend(third.jobs);
    settle_until(&reopened_queue, &assignment, &restored.id, &new_jobs).await;
    settle_until(&reopened_queue, &assignment, &new_jobs[0].id, &[]).await;
}

async fn staged(fixture: &Fixture) {
    let (scheduler, _) = host(fixture).await;
    let db = fixture.database().await;
    let app = AppId::mint();
    let original = metadata(&app);
    scheduler.prepare(&original).await.unwrap();
    scheduler.activate(&activate(&original, 1)).await.unwrap();
    let id = make_due(&db, &app).await;
    scheduler.dispatch(&app, &id).await.unwrap();
    scheduler.disable(&disable(&app, 2)).await.unwrap();
    let before = snapshot(&db).await;
    let mut replacement = metadata(&app);
    replacement.schedules[0].schedule = ScheduleTiming::Interval {
        interval_ms: 2_000,
        anchor: IntervalAnchor::Deploy,
    };
    scheduler.prepare(&replacement).await.unwrap();
    assert_eq!(snapshot(&db).await, before);
    let job = scheduler
        .activate(&activate(&replacement, 3))
        .await
        .unwrap();
    let after = snapshot(&db).await;
    assert_eq!(after.occurrences, before.occurrences);
    assert_eq!(after.calendars[0].id, id.as_str());
    assert_eq!(after.calendars[0].anchor_at, job.available_at.get());
    assert_eq!(
        after.calendars[0].next_at,
        Some(job.available_at.get() + 2_000)
    );
    assert_eq!(after.calendars[0].catch_up_until, None);
    assert_eq!(after.calendars[0].catch_up_remaining, None);
    assert!(after
        .holds
        .iter()
        .any(|row| row.deployment_id == original.deployment_id.as_str() && row.state == "held"));
}

async fn eligibility(fixture: &Fixture) {
    let (scheduler, queue) = host(fixture).await;
    let db = fixture.database().await;
    let mut disabled = Vec::new();
    for _ in 0..3 {
        let app = AppId::mint();
        let metadata = metadata(&app);
        scheduler.prepare(&metadata).await.unwrap();
        scheduler.activate(&activate(&metadata, 1)).await.unwrap();
        make_due(&db, &app).await;
        scheduler.disable(&disable(&app, 2)).await.unwrap();
        disabled.push(app);
    }
    let app = AppId::mint();
    let metadata = metadata(&app);
    scheduler.prepare(&metadata).await.unwrap();
    scheduler.activate(&activate(&metadata, 1)).await.unwrap();
    let id = make_due(&db, &app).await;
    let page = scheduler.due(None).await.unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].schedule_id, id);
    let before = snapshot(&db).await;
    let mut driver = Driver::new(
        queue,
        DriverOptions {
            scheduling: options(),
            page_limit: 1,
            ..DriverOptions::default()
        },
        std::rc::Rc::new(zeroship_workflow_manager::lifecycle::Undeletable),
    )
    .unwrap();
    let report = driver.tick().await;
    assert_eq!(report.scheduling.visited, 1);
    assert_eq!(report.scheduling.completed, 1);
    assert!(report.scheduling.failures.is_empty());
    let after = snapshot(&db).await;
    assert!(!after.occurrences.is_empty());
    assert!(after
        .occurrences
        .iter()
        .all(|row| row.app_id == app.as_str()));
    for app in disabled {
        assert_eq!(
            before
                .calendars
                .iter()
                .find(|row| row.app_id == app.as_str()),
            after
                .calendars
                .iter()
                .find(|row| row.app_id == app.as_str())
        );
    }
}

async fn replicas(fixture: &Fixture) {
    let (first, _) = host(fixture).await;
    let (second, _) = host(fixture).await;
    let db = fixture.database().await;
    let app = AppId::mint();
    let metadata = metadata(&app);
    first.prepare(&metadata).await.unwrap();
    first.activate(&activate(&metadata, 1)).await.unwrap();
    let id = make_due(&db, &app).await;
    let command = disable(&app, 2);
    let (left, right) = futures::join!(first.disable(&command), second.disable(&command));
    assert_eq!(left.unwrap(), command);
    assert_eq!(right.unwrap(), command);
    assert_eq!(snapshot(&db).await.disables.len(), 1);
    let restoring = activate(&metadata, 3);
    let stopping = disable(&app, 4);
    let (restored, stopped) = futures::join!(first.activate(&restoring), second.disable(&stopping));
    assert!(restored.is_ok() || restored == Err(Error::Conflict));
    assert_eq!(stopped.unwrap(), stopping);
    let state = snapshot(&db).await;
    assert!(!state.scopes[0].enabled);
    assert_eq!(state.scopes[0].revision, 4);
    first.activate(&activate(&metadata, 5)).await.unwrap();
    let stopping = disable(&app, 6);
    let (page, stopped) = futures::join!(first.dispatch(&app, &id), second.disable(&stopping));
    let page = page.unwrap();
    assert_eq!(stopped.unwrap(), stopping);
    let after = snapshot(&db).await;
    assert_eq!(after.occurrences.len(), page.jobs.len());
    assert!(first.dispatch(&app, &id).await.unwrap().jobs.is_empty());
    assert_eq!(snapshot(&db).await, after);
}

async fn fault(fixture: &Fixture, table: &str, enabled: bool) {
    match &fixture.admin {
        Admin::Postgres(admin) => {
            let sql = if enabled {
                format!("CREATE FUNCTION workflow_manager.lifecycle_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'lifecycle fault'; END $$; CREATE TRIGGER lifecycle_fault BEFORE UPDATE ON workflow_manager.{table} FOR EACH ROW EXECUTE FUNCTION workflow_manager.lifecycle_fault();")
            } else {
                format!("DROP TRIGGER lifecycle_fault ON workflow_manager.{table}; DROP FUNCTION workflow_manager.lifecycle_fault();")
            };
            admin.batch_execute(&sql).await.unwrap();
        }
        Admin::Sqlite(admin) => {
            let sql = if enabled {
                format!("CREATE TRIGGER lifecycle_fault BEFORE UPDATE ON {table} BEGIN SELECT RAISE(ABORT, 'lifecycle fault'); END;")
            } else {
                "DROP TRIGGER lifecycle_fault;".into()
            };
            admin.execute_batch(&sql).unwrap();
        }
    }
}

async fn rollback(fixture: &Fixture) {
    let (scheduler, _) = host(fixture).await;
    let db = fixture.database().await;
    let app = AppId::mint();
    let metadata = metadata(&app);
    scheduler.prepare(&metadata).await.unwrap();
    scheduler.activate(&activate(&metadata, 1)).await.unwrap();
    make_due(&db, &app).await;
    let before = snapshot(&db).await;
    fault(fixture, "schedule_scopes", true).await;
    assert!(scheduler.disable(&disable(&app, 2)).await.is_err());
    assert_eq!(snapshot(&db).await, before);
    fault(fixture, "schedule_scopes", false).await;
    scheduler.disable(&disable(&app, 2)).await.unwrap();
    let stopped = snapshot(&db).await;
    fault(fixture, "schedules", true).await;
    assert!(scheduler.activate(&activate(&metadata, 3)).await.is_err());
    assert_eq!(snapshot(&db).await, stopped);
    fault(fixture, "schedules", false).await;
    scheduler.activate(&activate(&metadata, 3)).await.unwrap();
    assert!(snapshot(&db).await.scopes[0].enabled);
}

#[derive(Insertable)]
#[orm(entity = schedules)]
struct ExtraCalendar<'a> {
    id: String,
    app_id: &'a str,
    name: &'a str,
    activation_id: &'a str,
    revision: i64,
    definition: &'a str,
    next_at: Option<i64>,
    anchor_at: i64,
    catch_up_until: Option<i64>,
    catch_up_remaining: Option<i64>,
}

async fn corrupt_restore(fixture: &Fixture) {
    let (scheduler, _) = host(fixture).await;
    let db = fixture.database().await;
    let app = AppId::mint();
    let metadata = metadata(&app);
    scheduler.prepare(&metadata).await.unwrap();
    scheduler.activate(&activate(&metadata, 1)).await.unwrap();
    let id = make_due(&db, &app).await;
    scheduler.disable(&disable(&app, 2)).await.unwrap();
    for (until, remaining) in [
        (Some(9_000_i64), None),
        (None, Some(2_i64)),
        (Some(9_000), Some(0)),
        (Some(9_000), Some(6)),
        (Some(1_000), Some(1)),
    ] {
        db.entity::<schedules::Entity>()
            .unwrap()
            .update_many(
                schedules::id.eq(id.as_str()).unwrap(),
                schedules::catch_up_until
                    .set(until)
                    .unwrap()
                    .and(schedules::catch_up_remaining.set(remaining).unwrap())
                    .unwrap(),
            )
            .await
            .unwrap();
        let before = snapshot(&db).await;
        assert_eq!(
            scheduler.activate(&activate(&metadata, 3)).await,
            Err(Error::Storage)
        );
        assert_eq!(snapshot(&db).await, before);
    }
    db.entity::<schedules::Entity>()
        .unwrap()
        .update_many(
            schedules::id.eq(id.as_str()).unwrap(),
            schedules::catch_up_until
                .set(None::<i64>)
                .unwrap()
                .and(schedules::catch_up_remaining.set(None::<i64>).unwrap())
                .unwrap(),
        )
        .await
        .unwrap();
    let before = snapshot(&db).await;
    let saved = &before.calendars[0];
    db.entity::<schedules::Entity>()
        .unwrap()
        .insert::<_, Calendar>(ExtraCalendar {
            id: ScheduleId::mint().as_str().into(),
            app_id: app.as_str(),
            name: "unexpected",
            activation_id: &saved.activation_id,
            revision: saved.revision,
            definition: &saved.definition,
            next_at: saved.next_at,
            anchor_at: saved.anchor_at,
            catch_up_until: None,
            catch_up_remaining: None,
        })
        .await
        .unwrap();
    let corrupt = snapshot(&db).await;
    assert_eq!(
        scheduler.activate(&activate(&metadata, 3)).await,
        Err(Error::Storage)
    );
    assert_eq!(snapshot(&db).await, corrupt);
    db.entity::<schedules::Entity>()
        .unwrap()
        .update_many(
            schedules::name.eq("unexpected").unwrap(),
            schedules::next_at.set(None::<i64>).unwrap(),
        )
        .await
        .unwrap();
    scheduler.activate(&activate(&metadata, 3)).await.unwrap();
    assert_eq!(scheduler.due(None).await.unwrap().len(), 1);
}

#[derive(Debug)]
struct GatedHolds {
    delegate: Rc<dyn HoldClient>,
    entered: RefCell<Option<futures::channel::oneshot::Sender<()>>>,
    resume: RefCell<Option<futures::channel::oneshot::Receiver<()>>>,
}
impl HoldClient for GatedHolds {
    fn acquire<'a>(
        &'a self,
        app: &'a AppId,
        deployment: &'a DeploymentId,
        generation: HoldGeneration,
    ) -> Pin<Box<dyn Future<Output = Result<HoldReceipt, Error>> + 'a>> {
        Box::pin(async move {
            let entered = self.entered.borrow_mut().take();
            let resume = self.resume.borrow_mut().take();
            if let Some(entered) = entered {
                entered.send(()).unwrap();
            }
            if let Some(resume) = resume {
                resume.await.unwrap();
            }
            self.delegate.acquire(app, deployment, generation).await
        })
    }
    fn release<'a>(
        &'a self,
        app: &'a AppId,
        deployment: &'a DeploymentId,
        generation: HoldGeneration,
    ) -> Pin<Box<dyn Future<Output = Result<HoldReceipt, Error>> + 'a>> {
        self.delegate.release(app, deployment, generation)
    }
}

async fn held_io(fixture: &Fixture) {
    let (entered, reached) = futures::channel::oneshot::channel();
    let (release, resume) = futures::channel::oneshot::channel();
    let holds = Rc::new(GatedHolds {
        delegate: support::synthetic_holds(),
        entered: RefCell::new(Some(entered)),
        resume: RefCell::new(Some(resume)),
    });
    let queue = Queue::connect(
        fixture.binding(),
        fixture.url(),
        zeroship_workflow_manager::Options::default(),
        holds,
    )
    .await
    .unwrap();
    let scheduler = Scheduler::new(queue.clone(), options()).unwrap();
    let (other, _) = host(fixture).await;
    let app = AppId::mint();
    let metadata = metadata(&app);
    scheduler.prepare(&metadata).await.unwrap();
    let activation = activate(&metadata, 1);
    let attempt = scheduler.activate(&activation);
    let stop = async {
        reached.await.unwrap();
        other.disable(&disable(&app, 2)).await.unwrap();
        release.send(()).unwrap();
    };
    let (result, ()) = compio::time::timeout(Duration::from_secs(10), async {
        futures::join!(attempt, stop)
    })
    .await
    .unwrap();
    assert_eq!(result, Err(Error::Conflict));
    let snapshot = snapshot(&fixture.database().await).await;
    assert!(
        snapshot.jobs.is_empty()
            && snapshot.calendars.is_empty()
            && snapshot.activations.is_empty()
    );
    assert!(!snapshot.scopes[0].enabled);
    assert_eq!(snapshot.holds[0].state, "held");
    queue
        .release_deployment(&app, &metadata.deployment_id)
        .await
        .unwrap();
}
