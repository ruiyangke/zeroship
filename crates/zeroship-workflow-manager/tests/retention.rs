#![recursion_limit = "256"]
#![allow(
    clippy::future_not_send,
    reason = "native catalog and queue fixtures stay on their compio runtime"
)]

#[path = "support/retention.rs"]
mod catalog_support;
mod support;

use catalog_support::{Catalog, Published};
use futures::channel::oneshot;
use std::{
    cell::{Cell, RefCell},
    future::Future,
    pin::Pin,
    rc::Rc,
    time::Duration,
};
use support::{Admin, Backend, Fixture};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{Assignment, RunId, WorkerId},
    workflow_deployments::{HoldGeneration, HoldReceipt, HoldScope, HoldState},
    workflow_jobs::{DeploymentId, JobId, JobOperation, JobOutcome, JobSpec, Settlement},
    workflow_schedules::{ActivateSchedules, RegisterSchedules, ScheduleDescriptor, ScheduleId},
};
use zeroship_data_orm::{orm::Output, value, Value};
use zeroship_workflow_calendar::{
    IntervalAnchor, ScheduleCatchUp, ScheduleOverlap, ScheduleTiming,
};
use zeroship_workflow_manager::{
    recovery::{Options as RecoveryOptions, Recovery},
    retention::HoldClient,
    scheduling::{Options as SchedulerOptions, Scheduler},
    Error, Options, Queue,
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

#[path = "retention/operation_prerequisites.rs"]
mod operation_prerequisites;
#[path = "retention/outcomes.rs"]
mod outcomes;

case!(
    sqlite_queue_holds_reconcile_lost_replies,
    postgres_queue_holds_reconcile_lost_replies,
    lost_replies
);
case!(
    sqlite_queue_hold_acknowledgements_are_generation_fenced,
    postgres_queue_hold_acknowledgements_are_generation_fenced,
    stale_replies
);
case!(
    sqlite_failed_publication_preserves_recoverable_holds,
    postgres_failed_publication_preserves_recoverable_holds,
    publication_rollback
);
case!(
    sqlite_scheduling_replacement_preserves_pending_code,
    postgres_scheduling_replacement_preserves_pending_code,
    scheduling_retention
);

#[derive(Debug)]
struct Gate {
    reached: oneshot::Sender<()>,
    resumed: oneshot::Receiver<()>,
}

impl Gate {
    async fn wait(self) {
        self.reached.send(()).unwrap();
        self.resumed.await.unwrap();
    }
}

#[derive(Debug)]
struct FaultClient {
    inner: Rc<dyn HoldClient>,
    lose_acquire: Cell<bool>,
    lose_release: Cell<bool>,
    acquired: Cell<usize>,
    released: Cell<usize>,
    acquire_gate: RefCell<Option<Gate>>,
    release_gate: RefCell<Option<Gate>>,
}

impl FaultClient {
    fn new(inner: Rc<dyn HoldClient>) -> Rc<Self> {
        Rc::new(Self {
            inner,
            lose_acquire: Cell::new(false),
            lose_release: Cell::new(false),
            acquired: Cell::new(0),
            released: Cell::new(0),
            acquire_gate: RefCell::new(None),
            release_gate: RefCell::new(None),
        })
    }

    fn gate(&self, release: bool) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (reached, observed) = oneshot::channel();
        let (resume, resumed) = oneshot::channel();
        let slot = if release {
            &self.release_gate
        } else {
            &self.acquire_gate
        };
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(Gate { reached, resumed });
        (observed, resume)
    }
}

impl HoldClient for FaultClient {
    fn acquire<'a>(
        &'a self,
        app: &'a AppId,
        deployment: &'a DeploymentId,
        generation: HoldGeneration,
    ) -> Pin<Box<dyn Future<Output = Result<HoldReceipt, Error>> + 'a>> {
        Box::pin(async move {
            self.acquired.set(self.acquired.get() + 1);
            let receipt = self.inner.acquire(app, deployment, generation).await?;
            let gate = self.acquire_gate.borrow_mut().take();
            if let Some(gate) = gate {
                gate.wait().await;
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
    ) -> Pin<Box<dyn Future<Output = Result<HoldReceipt, Error>> + 'a>> {
        Box::pin(async move {
            self.released.set(self.released.get() + 1);
            let receipt = self.inner.release(app, deployment, generation).await?;
            let gate = self.release_gate.borrow_mut().take();
            if let Some(gate) = gate {
                gate.wait().await;
            }
            if self.lose_release.replace(false) {
                Err(Error::Unavailable)
            } else {
                Ok(receipt)
            }
        })
    }
}

async fn queue(fixture: &Fixture, client: Rc<dyn HoldClient>) -> Queue {
    Queue::connect(fixture.binding(), fixture.url(), Options::default(), client)
        .await
        .unwrap()
}

fn job(app: &AppId, deployment: &DeploymentId) -> JobSpec {
    JobSpec {
        id: JobId::mint(),
        app_id: app.clone(),
        operation: JobOperation::Advance {
            deployment_id: deployment.clone(),
            run_id: RunId::mint(),
            generation: 0,
            revision: 1.try_into().unwrap(),
        },
        available_at: 0.try_into().unwrap(),
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

async fn finish(queue: &Queue, authority: &Assignment, expected: &JobSpec) -> Settlement {
    let delivery = queue.claim(authority).await.unwrap().unwrap();
    assert_eq!(delivery.delivery().job, *expected);
    let settlement = Settlement {
        delivery: delivery.delivery().clone(),
        outcome: JobOutcome::Completed {},
        successors: vec![],
    };
    queue.settle(authority, &settlement).await.unwrap();
    settlement
}

async fn rows(fixture: &Fixture, collection: &str, filter: Value) -> Vec<Value> {
    let Output::Rows { rows, .. } = fixture
        .database()
        .await
        .collection(collection)
        .unwrap()
        .find(filter, value!({"limit":256}))
        .await
        .unwrap()
    else {
        panic!("expected stored metadata rows");
    };
    rows
}

async fn lost_replies(fixture: &Fixture) {
    let catalog = Catalog::new(fixture).await;
    let faults = FaultClient::new(catalog.client());
    let queue = queue(fixture, faults.clone()).await;
    let app = AppId::mint();
    queue.register_scope(&app).await.unwrap();
    assert_eq!(
        queue.submit(&job(&app, &DeploymentId::mint())).await,
        Err(Error::Denied),
        "queue publication requires a real deployment in the app catalog"
    );
    let deployment = catalog.publish(&app, "lost reply", &[]).await;
    let spec = job(&app, &deployment.id);
    faults.lose_acquire.set(true);
    assert_eq!(queue.submit(&spec).await, Err(Error::Unavailable));
    assert!(rows(fixture, "jobs", value!({"app_id":app.as_str()}))
        .await
        .is_empty());
    catalog.assert_retained(&app, &deployment).await;
    let reopened = self::queue(fixture, faults.clone()).await;
    let held = reopened
        .reconcile_deployment(&app, &deployment.id)
        .await
        .unwrap();
    assert_eq!(held.state, HoldState::Held);
    assert_eq!(held.deploy_hash, deployment.hash);
    assert_eq!(held.holder_id, HoldScope::for_queue(app.clone()).holder());
    reopened.submit(&spec).await.unwrap();
    assert_eq!(
        reopened.release_deployment(&app, &deployment.id).await,
        Err(Error::Conflict)
    );
    catalog.assert_retained(&app, &deployment).await;
    let authority = assignment(&app);
    finish(&reopened, &authority, &spec).await;
    faults.lose_release.set(true);
    assert_eq!(
        reopened.release_deployment(&app, &deployment.id).await,
        Err(Error::Unavailable)
    );
    let released = reopened
        .reconcile_deployment(&app, &deployment.id)
        .await
        .unwrap();
    assert_eq!(released.state, HoldState::Released);
    assert_eq!(released.generation, held.generation);
    catalog.reclaim(&app, &deployment).await;
}

async fn stale_replies(fixture: &Fixture) {
    let catalog = Catalog::new(fixture).await;
    let faults = FaultClient::new(catalog.client());
    let queue = queue(fixture, faults.clone()).await;
    let peer = self::queue(fixture, catalog.client()).await;
    let app = AppId::mint();
    queue.register_scope(&app).await.unwrap();
    for release in [false, true] {
        let deployment = catalog
            .publish(
                &app,
                if release {
                    "stale release"
                } else {
                    "stale acquire"
                },
                &[],
            )
            .await;
        if release {
            queue.ensure_deployment(&app, &deployment.id).await.unwrap();
        }
        let (reached, resume) = faults.gate(release);
        let running = queue.clone();
        let running_app = app.clone();
        let running_deploy = deployment.id.clone();
        let pending = compio::runtime::spawn(async move {
            if release {
                running
                    .release_deployment(&running_app, &running_deploy)
                    .await
            } else {
                running
                    .ensure_deployment(&running_app, &running_deploy)
                    .await
            }
        });
        compio::time::timeout(Duration::from_secs(10), reached)
            .await
            .unwrap()
            .unwrap();
        let prior = compio::time::timeout(
            Duration::from_secs(10),
            peer.reconcile_deployment(&app, &deployment.id),
        )
        .await
        .expect("platform response must not hold the queue transaction")
        .unwrap();
        if !release {
            peer.release_deployment(&app, &deployment.id).await.unwrap();
        }
        let held = peer.ensure_deployment(&app, &deployment.id).await.unwrap();
        assert!(held.generation.get() > prior.generation.get());
        resume.send(()).unwrap();
        assert_eq!(pending.await.unwrap(), Err(Error::Conflict));
        assert_eq!(
            peer.ensure_deployment(&app, &deployment.id).await.unwrap(),
            held
        );
        catalog.assert_retained(&app, &deployment).await;
        peer.release_deployment(&app, &deployment.id).await.unwrap();
        catalog.reclaim(&app, &deployment).await;
    }
}

async fn publication_fault(fixture: &Fixture, install: bool) {
    match &fixture.admin {
        Admin::Sqlite(connection) => connection.execute_batch(if install {
            "CREATE TRIGGER queue_retention_fault BEFORE INSERT ON jobs BEGIN SELECT RAISE(ABORT,'publication fault'); END;"
        } else { "DROP TRIGGER queue_retention_fault;" }).unwrap(),
        Admin::Postgres(connection) => connection.batch_execute(if install {
            "CREATE FUNCTION workflow_manager.queue_retention_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'publication fault'; END $$;
             CREATE TRIGGER queue_retention_fault BEFORE INSERT ON workflow_manager.jobs FOR EACH ROW EXECUTE FUNCTION workflow_manager.queue_retention_fault();"
        } else {
            "DROP TRIGGER queue_retention_fault ON workflow_manager.jobs; DROP FUNCTION workflow_manager.queue_retention_fault();"
        }).await.unwrap(),
    }
}

async fn publication_rollback(fixture: &Fixture) {
    let catalog = Catalog::new(fixture).await;
    let queue = queue(fixture, catalog.client()).await;
    let app = AppId::mint();
    queue.register_scope(&app).await.unwrap();
    let deployment = catalog.publish(&app, "failed publication", &[]).await;
    let spec = job(&app, &deployment.id);
    publication_fault(fixture, true).await;
    queue
        .submit(&spec)
        .await
        .expect_err("injected publication failure must reject the job");
    assert!(rows(fixture, "jobs", value!({"app_id":app.as_str()}))
        .await
        .is_empty());
    catalog.assert_retained(&app, &deployment).await;
    publication_fault(fixture, false).await;
    let held = queue
        .reconcile_deployment(&app, &deployment.id)
        .await
        .unwrap();
    assert_eq!(held.state, HoldState::Held);
    queue.submit(&spec).await.unwrap();
    let authority = assignment(&app);
    finish(&queue, &authority, &spec).await;
    queue
        .release_deployment(&app, &deployment.id)
        .await
        .unwrap();
    catalog.reclaim(&app, &deployment).await;
}

fn descriptor() -> ScheduleDescriptor {
    ScheduleDescriptor {
        name: "periodic".into(),
        workflow_name: "scheduled-work".into(),
        schedule: ScheduleTiming::Interval {
            interval_ms: 1_000,
            anchor: IntervalAnchor::Epoch,
        },
        overlap: ScheduleOverlap::Allow,
        catch_up: ScheduleCatchUp::Skip,
    }
}

async fn activate(
    scheduler: &Scheduler,
    app: &AppId,
    deployment: &Published,
    revision: i64,
    descriptors: Vec<ScheduleDescriptor>,
) -> JobSpec {
    scheduler
        .prepare(&RegisterSchedules {
            app_id: app.clone(),
            deployment_id: deployment.id.clone(),
            schedules: descriptors,
        })
        .await
        .unwrap();
    scheduler
        .activate(&ActivateSchedules {
            app_id: app.clone(),
            deployment_id: deployment.id.clone(),
            revision: revision.try_into().unwrap(),
        })
        .await
        .unwrap()
}

async fn scheduling_retention(fixture: &Fixture) {
    let catalog = Catalog::new(fixture).await;
    let faults = FaultClient::new(catalog.client());
    let queue = queue(fixture, faults.clone()).await;
    let scheduler = Scheduler::new(queue.clone(), SchedulerOptions::default()).unwrap();
    let recovery = Recovery::new(queue.clone(), RecoveryOptions::default()).unwrap();
    let app = AppId::mint();
    let schedule = descriptor();
    let old = catalog
        .publish(&app, "old scheduled app", std::slice::from_ref(&schedule))
        .await;
    let replacement = catalog.publish(&app, "replacement app", &[]).await;
    let activation = activate(&scheduler, &app, &old, 1, vec![schedule]).await;
    let stored = rows(fixture, "schedules", value!({"app_id":app.as_str()})).await;
    assert_eq!(stored.len(), 1);
    let schedule_id = ScheduleId::parse(stored[0]["id"].as_str().unwrap()).unwrap();
    fixture
        .database()
        .await
        .collection("schedules")
        .unwrap()
        .update(
            value!({"app_id":app.as_str()}),
            value!({"next_at":0,"anchor_at":0}),
        )
        .await
        .unwrap();
    let page = scheduler.dispatch(&app, &schedule_id).await.unwrap();
    assert_eq!(page.jobs.len(), 1);
    assert!(rows(fixture, "workers", value!({})).await.is_empty());
    catalog.assert_retained(&app, &old).await;
    assert_eq!(
        queue.release_deployment(&app, &old.id).await,
        Err(Error::Conflict)
    );

    let authority = assignment(&app);
    finish(&queue, &authority, &activation).await;
    let cron = finish(&queue, &authority, &page.jobs[0]).await;
    recovery
        .ensure(&app, &replacement.id, 2.try_into().unwrap())
        .await
        .unwrap();
    // Recovery provenance changes leave the old future schedule as a pin.
    assert_eq!(
        queue.release_deployment(&app, &old.id).await,
        Err(Error::Conflict)
    );
    let pending = job(&app, &old.id);
    queue.submit(&pending).await.unwrap();
    let _next_activation = activate(&scheduler, &app, &replacement, 2, vec![]).await;
    // Removed schedules stop future production, while accepted jobs retain code.
    assert_eq!(
        queue.release_deployment(&app, &old.id).await,
        Err(Error::Conflict)
    );
    catalog.assert_retained(&app, &old).await;
    finish(&queue, &authority, &pending).await;

    let journal = HoldScope::for_app(app.clone());
    let journal_hold = catalog
        .ledger
        .acquire(&journal, old.id.as_str(), 1.try_into().unwrap())
        .await
        .unwrap();
    let released = queue.release_deployment(&app, &old.id).await.unwrap();
    assert_eq!(released.state, HoldState::Released);
    assert_ne!(released.holder_id, journal_hold.holder_id);
    catalog.assert_retained(&app, &old).await;
    catalog
        .ledger
        .release(&journal, old.id.as_str(), journal_hold.generation)
        .await
        .unwrap();
    catalog.reclaim(&app, &old).await;
    let acquired = faults.acquired.get();
    let released = faults.released.get();
    let replay = queue.settle(&authority, &cron).await.unwrap();
    assert_eq!(replay.job_id, cron.delivery.job.id);
    assert_eq!(replay.outcome, JobOutcome::Completed {});
    assert_eq!(
        faults.acquired.get(),
        acquired,
        "settled ACK replay cannot reacquire deleted code"
    );
    assert_eq!(faults.released.get(), released);
    catalog.assert_retained(&app, &replacement).await;
}
