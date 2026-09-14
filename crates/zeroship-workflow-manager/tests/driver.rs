#![recursion_limit = "256"]
#![allow(
    clippy::future_not_send,
    reason = "manager driver fixtures run on their owning compio runtime"
)]

#[allow(dead_code, reason = "shared fixtures expose other manager contracts")]
#[path = "support/retention.rs"]
mod catalog_support;
#[allow(dead_code, reason = "shared fixtures expose other manager contracts")]
mod support;

use catalog_support::{Catalog, Published};
use futures::{
    channel::oneshot,
    future::{select, Either},
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};
use support::{Admin, Backend, Fixture};
use zeroship_core::{
    app_id::AppId,
    typed_id,
    workflow_deployments::{HoldGeneration, HoldState},
    workflow_jobs::DeploymentId,
    workflow_schedules::{ActivateSchedules, RegisterSchedules, ScheduleDescriptor},
};
use zeroship_data_orm::{
    orm::{Operation, Output},
    value, Value,
};
use zeroship_workflow_calendar::{
    IntervalAnchor, ScheduleCatchUp, ScheduleOverlap, ScheduleTiming,
};
use zeroship_workflow_manager::{
    driver::{Driver, LaneReport, Options},
    lifecycle::{AppLifecycle, Undeletable},
    recovery::{DutyKind, Recovery},
    retention::{HoldClient, HoldFuture},
    scheduling::Scheduler,
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
    sqlite_driver_publishes_without_workers,
    postgres_driver_publishes_without_workers,
    no_workers
);
case!(
    sqlite_driver_rotates_failed_scopes_with_finite_sweeps,
    postgres_driver_rotates_failed_scopes_with_finite_sweeps,
    finite_sweeps
);
case!(
    sqlite_driver_bounds_hot_calendar_pages,
    postgres_driver_bounds_hot_calendar_pages,
    calendar_pages
);
case!(
    sqlite_driver_continues_after_lane_scan_failure,
    postgres_driver_continues_after_lane_scan_failure,
    scan_failure
);
case!(
    sqlite_driver_reconciles_global_hold_intents,
    postgres_driver_reconciles_global_hold_intents,
    retention_replies
);
case!(
    sqlite_driver_shares_lane_deadline_and_resumes_suffix,
    postgres_driver_shares_lane_deadline_and_resumes_suffix,
    lane_timeout
);
case!(
    sqlite_driver_collects_while_reconciliation_is_damaged,
    postgres_driver_collects_while_reconciliation_is_damaged,
    independent_duties
);

case!(
    sqlite_cancelled_driver_preserves_intent_and_cursor,
    postgres_cancelled_driver_preserves_intent_and_cursor,
    cancellation
);

#[test]
fn options_reject_invalid_bounds_without_io() {
    Options::default().validate().unwrap();
    for options in [
        Options {
            page_limit: 0,
            ..Options::default()
        },
        Options {
            page_limit: u32::MAX,
            ..Options::default()
        },
        Options {
            lane_timeout: Duration::ZERO,
            ..Options::default()
        },
        Options {
            lane_timeout: Duration::MAX,
            ..Options::default()
        },
        Options {
            scheduling: zeroship_workflow_manager::scheduling::Options {
                max_backfill: 0,
                ..Default::default()
            },
            ..Default::default()
        },
        Options {
            recovery: zeroship_workflow_manager::recovery::Options {
                interval: Duration::ZERO,
                ..Default::default()
            },
            ..Default::default()
        },
    ] {
        assert_eq!(options.validate(), Err(Error::Invalid));
    }
}

/// Local hosts and these fixtures have no Control catalog to delete apps from.
fn undeletable() -> Rc<dyn AppLifecycle> {
    Rc::new(Undeletable)
}

fn options(page_limit: u32) -> Options {
    Options {
        page_limit,
        scheduling: zeroship_workflow_manager::scheduling::Options {
            page_size: 1,
            max_backfill: 5,
            ..Default::default()
        },
        recovery: zeroship_workflow_manager::recovery::Options {
            interval: Duration::from_secs(3600),
            ..Default::default()
        },
        ..Default::default()
    }
}

async fn queue(fixture: &Fixture, holds: Rc<dyn HoldClient>) -> Queue {
    Queue::connect(
        fixture.binding(),
        fixture.url(),
        zeroship_workflow_manager::Options::default(),
        holds,
    )
    .await
    .unwrap()
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
        panic!("expected native stored records");
    };
    rows.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    rows
}

async fn patch(fixture: &Fixture, table: &str, filter: Value, document: Value) {
    let output = fixture
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
    assert!(matches!(output, Output::Count(changed) if changed > 0));
}

fn success(report: &LaneReport, visited: usize) {
    assert_eq!(report.visited, visited);
    assert_eq!(report.completed, visited);
    assert!(report.failures.is_empty());
    assert_eq!(report.scan_error, None);
    assert!(!report.timed_out);
    assert_eq!(report.unvisited, 0);
}

fn descriptor(name: &str, catch_up: ScheduleCatchUp) -> ScheduleDescriptor {
    ScheduleDescriptor {
        name: name.into(),
        workflow_name: "scheduled-work".into(),
        schedule: ScheduleTiming::Interval {
            interval_ms: 86_400_000,
            anchor: IntervalAnchor::Epoch,
        },
        overlap: ScheduleOverlap::Allow,
        catch_up,
    }
}

async fn activate(
    fixture: &Fixture,
    queue: &Queue,
    app: &AppId,
    definitions: Vec<ScheduleDescriptor>,
) {
    let scheduler = Scheduler::new(queue.clone(), options(1).scheduling).unwrap();
    let deployment = DeploymentId::mint();
    scheduler
        .prepare(&RegisterSchedules {
            app_id: app.clone(),
            deployment_id: deployment.clone(),
            schedules: definitions,
        })
        .await
        .unwrap();
    scheduler
        .activate(&ActivateSchedules {
            app_id: app.clone(),
            deployment_id: deployment,
            revision: 1.try_into().unwrap(),
        })
        .await
        .unwrap();
    patch(
        fixture,
        "schedules",
        value!({"app_id":app.as_str()}),
        value!({"next_at":0,"anchor_at":0}),
    )
    .await;
    patch(
        fixture,
        "recovery_duties",
        value!({"app_id":app.as_str()}),
        value!({"next_due_at":0}),
    )
    .await;
}

async fn obligation(fixture: &Fixture, queue: &Queue, app: &AppId) {
    Recovery::new(queue.clone(), options(1).recovery)
        .unwrap()
        .ensure(app, &DeploymentId::mint(), 1.try_into().unwrap())
        .await
        .unwrap();
    patch(
        fixture,
        "recovery_duties",
        value!({"app_id":app.as_str()}),
        value!({"next_due_at":0}),
    )
    .await;
}

async fn no_workers(fixture: &Fixture) {
    let queue = queue(fixture, support::synthetic_holds()).await;
    let app = AppId::mint();
    activate(
        fixture,
        &queue,
        &app,
        vec![descriptor("calendar", ScheduleCatchUp::Skip)],
    )
    .await;
    let mut driver = Driver::new(queue.clone(), options(2), undeletable()).unwrap();
    let mut replica = Driver::new(queue.clone(), options(2), undeletable()).unwrap();
    let (first, second) = futures::join!(driver.tick(), replica.tick());
    assert!(first.scheduling.failures.is_empty());
    assert!(second.scheduling.failures.is_empty());
    assert!(first.reconciliation.failures.is_empty());
    assert!(second.reconciliation.failures.is_empty());
    assert!(first.scheduling.completed + second.scheduling.completed > 0);
    assert!(first.reconciliation.completed + second.reconciliation.completed > 0);
    let jobs = rows(fixture, "jobs", value!({"app_id":app.as_str()})).await;
    assert!(first.collection.failures.is_empty());
    assert!(second.collection.failures.is_empty());
    assert!(first.collection.completed + second.collection.completed > 0);
    assert_eq!(jobs.len(), 4);
    assert!(rows(fixture, "workers", value!({})).await.is_empty());
    assert!(rows(fixture, "assignments", value!({})).await.is_empty());
    let operations: Vec<_> = jobs
        .iter()
        .map(|row| {
            serde_json::from_str::<zeroship_core::workflow_jobs::JobOperation>(
                row["operation"].as_str().unwrap(),
            )
            .unwrap()
        })
        .collect();
    assert!(operations.iter().any(|op| matches!(
        op,
        zeroship_core::workflow_jobs::JobOperation::Activate { .. }
    )));
    assert!(operations
        .iter()
        .any(|op| matches!(op, zeroship_core::workflow_jobs::JobOperation::Cron { .. })));
    assert!(operations.iter().any(|op| matches!(
        op,
        zeroship_core::workflow_jobs::JobOperation::Reconcile { .. }
    )));
    assert!(operations
        .iter()
        .any(|op| matches!(op, zeroship_core::workflow_jobs::JobOperation::Collect {})));
    patch(
        fixture,
        "schedules",
        value!({"app_id":app.as_str()}),
        value!({"next_at":i64::MAX}),
    )
    .await;
    patch(
        fixture,
        "recovery_duties",
        value!({"app_id":app.as_str()}),
        value!({"next_due_at":0}),
    )
    .await;
    let mut reopened = Driver::new(queue, options(2), undeletable()).unwrap();
    success(&reopened.tick().await.reconciliation, 1);
    assert_eq!(
        rows(fixture, "jobs", value!({"app_id":app.as_str()})).await,
        jobs
    );
}

async fn finite_sweeps(fixture: &Fixture) {
    let queue = queue(fixture, support::synthetic_holds()).await;
    let mut apps = [AppId::mint(), AppId::mint()];
    apps.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    obligation(fixture, &queue, &apps[0]).await;
    let database = fixture.database().await;
    database
        .collection("queue_scopes")
        .unwrap()
        .insert(value!({"id":"!malformed"}))
        .await
        .unwrap();
    database
        .collection("recovery_scopes")
        .unwrap()
        .insert(value!({
            "id":"!malformed", "deployment_id":DeploymentId::mint().as_str(),
            "activation_revision":1, "ingress_epoch":1, "state":"open",
        }))
        .await
        .unwrap();
    database.collection("recovery_duties").unwrap().insert(value!({
        "id":typed_id::generate("wrd"), "app_id":"!malformed", "kind":"reconcile", "next_due_at":0,
    })).await.unwrap();
    let mut driver = Driver::new(queue.clone(), options(1), undeletable()).unwrap();
    let first = driver.tick().await;
    assert_eq!(first.reconciliation.visited, 1);
    assert_eq!(first.reconciliation.completed, 0);
    assert_eq!(first.reconciliation.failures[0].id, "!malformed");
    assert_eq!(first.reconciliation.failures[0].error, Error::Storage);
    assert!(!first.reconciliation.sweep_complete);
    obligation(fixture, &queue, &apps[1]).await;
    let second = driver.tick().await;
    success(&second.reconciliation, 1);
    assert!(second.reconciliation.sweep_complete);
    assert!(rows(
        fixture,
        "jobs",
        value!({"app_id":apps[1].as_str(),"operation_kind":"reconcile"})
    )
    .await
    .is_empty());
    let third = driver.tick().await;
    assert_eq!(third.reconciliation.failures[0].id, "!malformed");
    let fourth = driver.tick().await;
    success(&fourth.reconciliation, 1);
    assert_eq!(
        rows(
            fixture,
            "jobs",
            value!({"app_id":apps[1].as_str(),"operation_kind":"reconcile"})
        )
        .await
        .len(),
        1
    );
    patch(
        fixture,
        "recovery_duties",
        value!({"app_id":apps[0].as_str()}),
        value!({"next_due_at":0}),
    )
    .await;
    let fifth = driver.tick().await;
    assert_eq!(fifth.reconciliation.failures[0].id, "!malformed");
    success(&driver.tick().await.reconciliation, 1);
    assert_eq!(
        rows(
            fixture,
            "jobs",
            value!({"app_id":apps[0].as_str(),"operation_kind":"reconcile"})
        )
        .await
        .len(),
        1
    );
}

async fn calendar_pages(fixture: &Fixture) {
    let queue = queue(fixture, support::synthetic_holds()).await;
    let app = AppId::mint();
    activate(
        fixture,
        &queue,
        &app,
        vec![
            descriptor("hot", ScheduleCatchUp::Backfill { max: 5 }),
            descriptor("other", ScheduleCatchUp::Skip),
        ],
    )
    .await;
    let mut driver = Driver::new(queue, options(2), undeletable()).unwrap();
    let report = driver.tick().await;
    success(&report.scheduling, 2);
    success(&report.reconciliation, 1);
    success(&report.collection, 1);
    let occurrences = rows(
        fixture,
        "schedule_occurrences",
        value!({"app_id":app.as_str()}),
    )
    .await;
    assert_eq!(occurrences.len(), 2);
    let schedules = rows(fixture, "schedules", value!({"app_id":app.as_str()})).await;
    let hot = schedules
        .iter()
        .find(|row| row["name"].as_str() == Some("hot"))
        .unwrap();
    assert_eq!(hot["catch_up_remaining"].as_i64(), Some(4));
    assert!(hot["catch_up_until"].as_i64().is_some());
    success(&driver.tick().await.scheduling, 1);
    assert_eq!(
        rows(
            fixture,
            "schedule_occurrences",
            value!({"app_id":app.as_str()})
        )
        .await
        .len(),
        3
    );
}

async fn rename_schedules(fixture: &Fixture, missing: bool) {
    let (from, to) = if missing {
        ("schedules", "unavailable_schedules")
    } else {
        ("unavailable_schedules", "schedules")
    };
    match &fixture.admin {
        Admin::Postgres(admin) => admin
            .batch_execute(&format!(
                "ALTER TABLE workflow_manager.{from} RENAME TO {to}"
            ))
            .await
            .unwrap(),
        Admin::Sqlite(admin) => admin
            .execute_batch(&format!("ALTER TABLE {from} RENAME TO {to}"))
            .unwrap(),
    }
}

async fn scan_failure(fixture: &Fixture) {
    let queue = queue(fixture, support::synthetic_holds()).await;
    let app = AppId::mint();
    obligation(fixture, &queue, &app).await;
    let mut driver = Driver::new(queue, options(2), undeletable()).unwrap();
    rename_schedules(fixture, true).await;
    let report = driver.tick().await;
    rename_schedules(fixture, false).await;
    // SQLite classifies missing-table errors as transient storage failures.
    assert!(matches!(
        report.scheduling.scan_error,
        Some(Error::Storage | Error::Unavailable)
    ));
    assert!(!report.scheduling.sweep_complete);
    success(&report.reconciliation, 1);
    success(&report.collection, 1);
    success(&report.retention, 0);
    success(&driver.tick().await.scheduling, 0);
}

#[derive(Debug)]
struct Gate {
    entered: oneshot::Sender<()>,
    resumed: oneshot::Receiver<()>,
    dropped: Rc<Cell<bool>>,
}

struct Dropped(Rc<Cell<bool>>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.set(true);
    }
}

#[derive(Debug)]
struct FaultClient {
    inner: Rc<dyn HoldClient>,
    lose_acquire: Cell<bool>,
    lose_release: Cell<bool>,
    calls: Cell<usize>,
    gate: RefCell<Option<Gate>>,
}

impl FaultClient {
    fn new(inner: Rc<dyn HoldClient>) -> Rc<Self> {
        Rc::new(Self {
            inner,
            lose_acquire: Cell::new(false),
            lose_release: Cell::new(false),
            calls: Cell::new(0),
            gate: RefCell::new(None),
        })
    }

    fn gate(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>, Rc<Cell<bool>>) {
        let (entered, observed) = oneshot::channel();
        let (resume, resumed) = oneshot::channel();
        let dropped = Rc::new(Cell::new(false));
        *self.gate.borrow_mut() = Some(Gate {
            entered,
            resumed,
            dropped: dropped.clone(),
        });
        (observed, resume, dropped)
    }
}

impl HoldClient for FaultClient {
    fn acquire<'a>(
        &'a self,
        app: &'a AppId,
        deployment: &'a DeploymentId,
        generation: HoldGeneration,
    ) -> HoldFuture<'a> {
        Box::pin(async move {
            self.calls.set(self.calls.get() + 1);
            let receipt = self.inner.acquire(app, deployment, generation).await?;
            let gate = self.gate.borrow_mut().take();
            if let Some(gate) = gate {
                let _dropped = Dropped(gate.dropped);
                let _ = gate.entered.send(());
                gate.resumed.await.map_err(|_| Error::Unavailable)?;
            }
            if self.lose_acquire.replace(false) {
                Err(Error::Unavailable)
            } else {
                Ok(receipt)
            }
        })
    }

    fn release<'a>(
        &'a self,
        app: &'a AppId,
        deployment: &'a DeploymentId,
        generation: HoldGeneration,
    ) -> HoldFuture<'a> {
        Box::pin(async move {
            self.calls.set(self.calls.get() + 1);
            let receipt = self.inner.release(app, deployment, generation).await?;
            if self.lose_release.replace(false) {
                Err(Error::Unavailable)
            } else {
                Ok(receipt)
            }
        })
    }
}

async fn pending(
    fixture: &Fixture,
    catalog: &Catalog,
    queue: &Queue,
    client: &FaultClient,
    app: &AppId,
    marker: &str,
) -> Published {
    queue.register_scope(app).await.unwrap();
    let deployment = catalog.publish(app, marker, &[]).await;
    client.lose_acquire.set(true);
    assert_eq!(
        queue.ensure_deployment(app, &deployment.id).await,
        Err(Error::Unavailable)
    );
    assert_eq!(
        rows(
            fixture,
            "deployment_holds",
            value!({"app_id":app.as_str(),"deployment_id":deployment.id.as_str()})
        )
        .await[0]["state"]
            .as_str(),
        Some("acquiring")
    );
    deployment
}

async fn retention_replies(fixture: &Fixture) {
    let catalog = Catalog::new(fixture).await;
    let client = FaultClient::new(catalog.client());
    let queue = queue(fixture, client.clone()).await;
    let app = AppId::mint();
    let other_app = AppId::mint();
    let first = pending(fixture, &catalog, &queue, &client, &app, "pending first").await;
    let second = pending(
        fixture,
        &catalog,
        &queue,
        &client,
        &other_app,
        "pending second",
    )
    .await;
    let held = catalog.publish(&app, "explicit release policy", &[]).await;
    queue.ensure_deployment(&app, &held.id).await.unwrap();
    let released = catalog.publish(&app, "lost release", &[]).await;
    queue.ensure_deployment(&app, &released.id).await.unwrap();
    client.lose_release.set(true);
    assert_eq!(
        queue.release_deployment(&app, &released.id).await,
        Err(Error::Unavailable)
    );
    let before = client.calls.get();
    let mut driver = Driver::new(queue.clone(), options(8), undeletable()).unwrap();
    let report = driver.tick().await;
    success(&report.retention, 3);
    assert_eq!(client.calls.get() - before, 3);
    assert!(rows(fixture, "recovery_scopes", value!({}))
        .await
        .is_empty());
    catalog.assert_retained(&app, &first).await;
    catalog.assert_retained(&other_app, &second).await;
    catalog.assert_retained(&app, &held).await;
    catalog.reclaim(&app, &released).await;
    let before = client.calls.get();
    success(&driver.tick().await.retention, 0);
    assert_eq!(client.calls.get(), before);
}

#[expect(
    clippy::too_many_lines,
    reason = "the cancellation fixture observes one interrupted operation and its recovery"
)]
async fn interrupted(fixture: &Fixture, expire: bool) {
    let catalog = Catalog::new(fixture).await;
    let client = FaultClient::new(catalog.client());
    let queue = queue(fixture, client.clone()).await;
    let app = AppId::mint();
    let first = pending(
        fixture,
        &catalog,
        &queue,
        &client,
        &app,
        "interrupted first",
    )
    .await;
    let second = pending(
        fixture,
        &catalog,
        &queue,
        &client,
        &app,
        "interrupted second",
    )
    .await;
    let intents = rows(fixture, "deployment_holds", value!({"app_id":app.as_str()})).await;
    assert_eq!(intents.len(), 2);
    let first_id = intents[0]["deployment_id"].as_str().unwrap().to_owned();
    let second_id = intents[1]["deployment_id"].as_str().unwrap().to_owned();
    let (observed, resume, dropped) = client.gate();
    let mut driver = Driver::new(
        queue.clone(),
        Options {
            lane_timeout: if expire {
                Duration::from_secs(1)
            } else {
                Duration::from_secs(10)
            },
            ..options(2)
        },
        undeletable(),
    )
    .unwrap();
    let mut tick = Box::pin(driver.tick());
    match compio::time::timeout(Duration::from_secs(10), select(&mut tick, observed))
        .await
        .unwrap()
    {
        Either::Right((Ok(()), _)) => {}
        _ => panic!("hold call must be in flight before interruption"),
    }
    if expire {
        let report = tick.await;
        assert!(report.retention.timed_out);
        assert_eq!(report.retention.visited, 1);
        assert_eq!(report.retention.completed, 0);
        assert_eq!(report.retention.unvisited, 1);
        assert!(!report.retention.sweep_complete);
        assert_eq!(report.retention.failures[0].error, Error::Timeout);
    } else {
        drop(tick);
    }
    assert!(dropped.get());
    drop(resume);
    assert_eq!(
        rows(
            fixture,
            "deployment_holds",
            value!({"app_id":app.as_str(),"deployment_id":first_id})
        )
        .await[0]["state"]
            .as_str(),
        Some("acquiring")
    );
    let later = driver.tick().await;
    success(&later.retention, 1);
    assert!(later.retention.sweep_complete);
    assert_eq!(
        rows(
            fixture,
            "deployment_holds",
            value!({"app_id":app.as_str(),"deployment_id":second_id})
        )
        .await[0]["state"]
            .as_str(),
        Some("held")
    );
    success(&driver.tick().await.retention, 1);
    catalog.assert_retained(&app, &first).await;
    catalog.assert_retained(&app, &second).await;
    assert_eq!(
        queue
            .reconcile_deployment(&app, &first.id)
            .await
            .unwrap()
            .state,
        HoldState::Held
    );
    assert_eq!(
        queue
            .reconcile_deployment(&app, &second.id)
            .await
            .unwrap()
            .state,
        HoldState::Held
    );
}

async fn lane_timeout(fixture: &Fixture) {
    interrupted(fixture, true).await;
}
async fn cancellation(fixture: &Fixture) {
    interrupted(fixture, false).await;
}

async fn independent_duties(fixture: &Fixture) {
    let client = FaultClient::new(support::synthetic_holds());
    client.lose_acquire.set(true);
    client.lose_release.set(true);
    let queue = queue(fixture, client.clone()).await;
    let app = AppId::mint();
    obligation(fixture, &queue, &app).await;
    let recovery = Recovery::new(queue.clone(), options(1).recovery).unwrap();
    let pending = recovery
        .dispatch(&app, DutyKind::Reconcile)
        .await
        .unwrap()
        .unwrap();
    let original = rows(fixture, "jobs", value!({"id":pending.id.as_str()})).await;
    patch(
        fixture,
        "jobs",
        value!({"id":pending.id.as_str()}),
        value!({"spec_digest":"damaged"}),
    )
    .await;
    patch(
        fixture,
        "recovery_duties",
        value!({"app_id":app.as_str(),"kind":"reconcile"}),
        value!({"next_due_at":0}),
    )
    .await;
    let mut driver = Driver::new(queue.clone(), options(1), undeletable()).unwrap();
    let report = driver.tick().await;
    assert_eq!(report.reconciliation.failures.len(), 1);
    assert_eq!(report.reconciliation.failures[0].error, Error::Storage);
    success(&report.collection, 1);
    let collect = recovery
        .dispatch(&app, DutyKind::Collect)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        collect.operation,
        zeroship_core::workflow_jobs::JobOperation::Collect {}
    ));
    assert_eq!(collect.deployment_id(), None);
    assert!(rows(fixture, "workers", value!({})).await.is_empty());
    assert!(rows(fixture, "assignments", value!({})).await.is_empty());
    assert!(rows(fixture, "deployment_holds", value!({}))
        .await
        .is_empty());
    assert_eq!(client.calls.get(), 0);
    let mut restarted = Driver::new(queue, options(1), undeletable()).unwrap();
    assert_eq!(
        restarted.tick().await.reconciliation.failures[0].error,
        Error::Storage
    );
    assert_eq!(
        recovery.dispatch(&app, DutyKind::Collect).await.unwrap(),
        Some(collect)
    );
    assert_eq!(
        rows(fixture, "jobs", value!({"app_id":app.as_str()}))
            .await
            .len(),
        2
    );
    patch(
        fixture,
        "jobs",
        value!({"id":pending.id.as_str()}),
        value!({"spec_digest":original[0]["spec_digest"].clone()}),
    )
    .await;
    success(&restarted.tick().await.reconciliation, 1);
    assert_eq!(client.calls.get(), 0);
}
