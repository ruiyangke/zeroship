#![recursion_limit = "256"]
#![expect(
    clippy::future_not_send,
    reason = "native manager fixtures stay on their compio runtime"
)]

#[path = "support/latest.rs"]
mod latest_support;
#[allow(
    dead_code,
    reason = "shared database fixtures support other manager suites"
)]
mod support;

use futures::channel::oneshot;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    future::ready,
    rc::Rc,
};
use support::{Backend, Fixture};
use zeroship_core::{
    app_id::AppId,
    service_peers::{service_issuer, CONTROL_SERVICE_NAME},
    typed_id,
    workflow_coordination::{
        Assignment, ManageRun, ManagementOperation, ManagementOutcome, RestartDeploy,
        RestartOptions, RunId, RunOperation, WorkerId,
    },
    workflow_deployments::HoldGeneration,
    workflow_jobs::{
        DeploymentId, JobId, JobOperation, JobOutcome, JobSpec, ManagementCommand, Settlement,
    },
};
use zeroship_data_orm::{
    orm::{Database, Operation, Output},
    value, Value,
};
use zeroship_workflow_manager::{
    coordinator::{Coordinator, Options as CoordinatorOptions},
    retention::{CatalogClient, HoldClient, HoldFuture},
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

#[path = "management/acceptance.rs"]
mod acceptance;
#[path = "management/barriers.rs"]
mod barriers;
#[path = "management/status.rs"]
mod status;

#[derive(Debug)]
struct Holds {
    inner: CatalogClient,
    acquired: Cell<usize>,
    released: Cell<usize>,
    fail: Cell<bool>,
    wrong_hash: Cell<bool>,
    entered: RefCell<Option<oneshot::Sender<()>>>,
    resume: RefCell<Option<oneshot::Receiver<()>>>,
}
impl Holds {
    fn gate(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered_tx, entered_rx) = oneshot::channel();
        let (resume_tx, resume_rx) = oneshot::channel();
        self.entered.replace(Some(entered_tx));
        self.resume.replace(Some(resume_rx));
        (entered_rx, resume_tx)
    }
}
impl HoldClient for Holds {
    fn acquire<'a>(
        &'a self,
        app: &'a AppId,
        deploy: &'a DeploymentId,
        generation: HoldGeneration,
    ) -> HoldFuture<'a> {
        Box::pin(async move {
            self.acquired.set(self.acquired.get() + 1);
            let resume = self.resume.borrow_mut().take();
            if let Some(entered) = self.entered.borrow_mut().take() {
                entered.send(()).unwrap();
            }
            if let Some(resume) = resume {
                resume.await.unwrap();
            }
            if self.fail.get() {
                return Err(Error::Unavailable);
            }
            let mut receipt = self.inner.acquire(app, deploy, generation).await?;
            if self.wrong_hash.get() {
                receipt.deploy_hash = zeroship_bundle::sha256_hex(b"wrong hold hash");
            }
            Ok(receipt)
        })
    }
    fn release<'a>(
        &'a self,
        app: &'a AppId,
        deploy: &'a DeploymentId,
        generation: HoldGeneration,
    ) -> HoldFuture<'a> {
        self.released.set(self.released.get() + 1);
        self.inner.release(app, deploy, generation)
    }
}

struct Host {
    source: latest_support::Source,
    holds: Rc<Holds>,
    queue: Queue,
    coordinator: Coordinator,
    database: Database,
}
impl Host {
    async fn new(fixture: &Fixture) -> Self {
        let source = latest_support::Source::new(fixture).await;
        let holds = Rc::new(Holds {
            inner: CatalogClient::new(source.ledger.clone()),
            acquired: Cell::new(0),
            released: Cell::new(0),
            fail: Cell::new(false),
            wrong_hash: Cell::new(false),
            entered: RefCell::new(None),
            resume: RefCell::new(None),
        });
        let queue = Queue::connect(
            fixture.binding(),
            fixture.url(),
            Options::default(),
            holds.clone(),
        )
        .await
        .unwrap();
        let coordinator = support::coordinator(&queue, CoordinatorOptions::default());
        Self {
            source,
            holds,
            queue,
            coordinator,
            database: fixture.database().await,
        }
    }
    async fn manage(
        &self,
        request: &ManageRun,
    ) -> Result<zeroship_core::workflow_coordination::ManagementReceipt, Error> {
        self.coordinator
            .manage(
                &service_issuer(CONTROL_SERVICE_NAME).unwrap(),
                request,
                &self.source.latest,
            )
            .await
    }
    async fn job(&self, request: &ManageRun) -> JobSpec {
        let command = single(
            &self.database,
            "management",
            value!({"request_id":request.request_id.as_str(),"app_id":request.app_id.as_str()}),
        )
        .await;
        let job = single(&self.database, "jobs", value!({"id":command["id"]})).await;
        JobSpec {
            id: JobId::parse(job["id"].as_str().unwrap()).unwrap(),
            app_id: request.app_id.clone(),
            operation: serde_json::from_str(job["operation"].as_str().unwrap()).unwrap(),
            available_at: job["available_at"].as_i64().unwrap().try_into().unwrap(),
        }
    }
}

fn command(app: &AppId, run: &RunId, operation: RunOperation) -> ManageRun {
    ManageRun {
        request_id: zeroship_core::workflow_coordination::RequestId::mint(),
        app_id: app.clone(),
        run_id: run.clone(),
        command: ManagementOperation::Transition { operation },
    }
}
fn latest(app: &AppId, run: &RunId) -> ManageRun {
    ManageRun {
        command: ManagementOperation::Restart {
            options: RestartOptions {
                from: None,
                deploy: Some(RestartDeploy::Latest),
            },
        },
        ..command(app, run, RunOperation::Pause)
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
fn ordinary(app: &AppId) -> JobSpec {
    JobSpec {
        id: JobId::mint(),
        app_id: app.clone(),
        operation: JobOperation::Reconcile {},
        available_at: 0.try_into().unwrap(),
    }
}
async fn rows(database: &Database, table: &str, filter: Value) -> Vec<Value> {
    let collection = database.collection(table).unwrap();
    let mut found = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page_filter = after.as_ref().map_or_else(
            || filter.clone(),
            |after| value!({"$and":[filter.clone(), {"id":{"$gt":after}}]}),
        );
        let Output::Rows { rows, .. } = collection
            .find(page_filter, value!({"orderBy":{"id":1},"limit":256}))
            .await
            .unwrap()
        else {
            panic!("expected manager rows");
        };
        let Some(last) = rows.last() else {
            return found;
        };
        let next = last["id"].as_str().unwrap().to_owned();
        assert!(
            after.as_ref().is_none_or(|previous| &next > previous),
            "fixture pagination must advance"
        );
        after = Some(next);
        found.extend(rows);
    }
}
async fn single(database: &Database, table: &str, filter: Value) -> Value {
    let mut found = rows(database, table, filter).await;
    assert_eq!(found.len(), 1);
    found.pop().unwrap()
}
async fn patch(database: &Database, table: &str, id: &str, changes: Value) {
    let result = database
        .collection(table)
        .unwrap()
        .execute(Operation::Update {
            filter: value!({"id":id}),
            patch: changes,
            many: true,
        })
        .await
        .unwrap();
    assert!(matches!(result, Output::Count(1)));
}
async fn snapshot(host: &Host) -> BTreeMap<&'static str, Vec<Value>> {
    let mut snapshot = BTreeMap::new();
    for table in [
        "queue_scopes",
        "jobs",
        "management",
        "management_scopes",
        "deployment_holds",
    ] {
        snapshot.insert(table, rows(&host.database, table, value!({})).await);
    }
    snapshot
}
async fn settle(
    host: &Host,
    authority: &Assignment,
    expected: &JobSpec,
    outcome: ManagementOutcome,
) -> Settlement {
    let delivery = host
        .queue
        .claim(authority)
        .await
        .unwrap()
        .unwrap()
        .delivery()
        .clone();
    assert_eq!(&delivery.job, expected);
    let settlement = Settlement {
        delivery,
        outcome: JobOutcome::Management { outcome },
        successors: vec![],
    };
    host.queue.settle(authority, &settlement).await.unwrap();
    settlement
}
