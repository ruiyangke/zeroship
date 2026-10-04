#![allow(
    clippy::future_not_send,
    reason = "fixture connections stay on their compio runtime"
)]

use std::{
    future::{ready, Future},
    pin::Pin,
    rc::Rc,
};
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};
use zeroship_core::{
    app_id::AppId,
    schema_name::SchemaName,
    workflow_coordination::WorkerId,
    workflow_deployments::{HoldGeneration, HoldReceipt, HoldScope, HoldState},
    workflow_jobs::{
        Delivery, DeploymentId, JobOutcome, JobReceipt, JournalSettlement, SettlementReceipt,
    },
    workflow_policy::AppPolicy,
};
use zeroship_data_orm::{
    binding::DbBinding, encryption::ProjectKeySource, orm::Database, ConnectOptions,
};
use zeroship_workflow_manager::{
    capacity::LocalCapacity,
    coordinator::{self, Coordinator},
    driver::{self, Driver},
    lifecycle::{AppLifecycle, Undeletable},
    retention::HoldClient,
    Claimant, DeliveryGrant, Error, Queue,
};

pub mod deployments;
pub mod policies;
pub mod retention;

use policies::LocalPolicies;

/// One worker acting on one app: the identity a direct queue call claims,
/// renews and settles as.
#[derive(Clone, Debug)]
pub struct Owner {
    pub app_id: AppId,
    pub worker_id: WorkerId,
}

impl Owner {
    #[must_use]
    pub fn new(app_id: AppId, worker_id: WorkerId) -> Self {
        Self { app_id, worker_id }
    }
}

/// Direct queue calls a contract makes as one enrolled worker holding one app.
///
/// The manager's own claim path is `claim_in_zone`; these wrap the single-app
/// authorized operations so a queue contract can drive one app without a zone
/// page, always under the same worker it names.
pub trait QueueCalls {
    /// Claim one ready job of `owner`'s app.
    ///
    /// # Errors
    /// Refuses invalid bounds and failed transactions.
    async fn claim(&self, owner: &Owner) -> Result<Option<DeliveryGrant>, Error>;

    /// Extend `owner`'s delivery lease.
    ///
    /// # Errors
    /// Refuses a stale delivery and failed transactions.
    async fn heartbeat(&self, owner: &Owner, delivery: &Delivery) -> Result<DeliveryGrant, Error>;

    /// Settle a delivery `owner` holds with the journal's settlement.
    ///
    /// # Errors
    /// Refuses a stale delivery and failed transactions.
    async fn settle(
        &self,
        owner: &Owner,
        settlement: &JournalSettlement,
    ) -> Result<SettlementReceipt, Error>;
}

impl QueueCalls for Queue {
    async fn claim(&self, owner: &Owner) -> Result<Option<DeliveryGrant>, Error> {
        let worker = owner.worker_id.clone();
        self.claim_authorized(
            &owner.app_id,
            &owner.worker_id,
            Claimant::Worker,
            Ok(AppPolicy::default().max_delivery_attempts),
            |_| ready(Ok(worker.clone())),
        )
        .await
    }

    async fn heartbeat(&self, owner: &Owner, delivery: &Delivery) -> Result<DeliveryGrant, Error> {
        let worker = owner.worker_id.clone();
        self.heartbeat_authorized(&owner.worker_id, delivery, |_| ready(Ok(worker.clone())))
            .await
    }

    async fn settle(
        &self,
        owner: &Owner,
        settlement: &JournalSettlement,
    ) -> Result<SettlementReceipt, Error> {
        let worker = owner.worker_id.clone();
        self.settle_authorized(
            &owner.worker_id,
            settlement,
            |_| ready(Ok(worker.clone())),
            |_| ready(Ok(worker.clone())),
        )
        .await
    }
}

/// The settlement the journal decided for `delivery`'s logical job.
///
/// A settlement is constructible only from a receipt, so a contract that drives
/// the queue directly builds that receipt here rather than naming an outcome a
/// caller could have chosen.
pub fn settlement(delivery: &Delivery, outcome: JobOutcome) -> JournalSettlement {
    JournalSettlement::from_receipt(
        &JobReceipt {
            job: delivery.job.clone(),
            outcome,
        },
        delivery,
    )
    .expect("a receipt built for its own delivery is a settlement")
}

/// The same, for a caller that already owns the delivery it settles.
pub fn settlement_from(delivery: Delivery, outcome: JobOutcome) -> JournalSettlement {
    settlement(&delivery, outcome)
}

/// A coordinator over `queue`.
pub fn coordinator(queue: &Queue, options: coordinator::Options) -> Coordinator {
    Coordinator::new(queue.clone(), options).unwrap()
}

/// A driver whose lanes run under trusted single-zone facts and the local
/// host's always-satisfied capacity. No app is ever deleted.
pub fn local_driver(queue: &Queue, options: driver::Options) -> Driver {
    driver_for(queue, options, Rc::new(Undeletable))
}

/// The same driver under a caller's deletion source, for the closing lane.
pub fn driver_for(
    queue: &Queue,
    options: driver::Options,
    lifecycle: Rc<dyn AppLifecycle>,
) -> Driver {
    try_driver(queue, options, lifecycle).unwrap()
}

/// The same construction, reporting its option validation instead of panicking.
pub fn try_driver(
    queue: &Queue,
    options: driver::Options,
    lifecycle: Rc<dyn AppLifecycle>,
) -> Result<Driver, Error> {
    Driver::new(
        coordinator(queue, coordinator::Options::default()),
        options,
        lifecycle,
        LocalPolicies::shared(),
        Rc::new(LocalCapacity),
    )
}

/// Queue state-machine tests use invented deployment identities. Retention
/// safety tests instead compose the real catalog and published app artifacts.
pub fn synthetic_holds() -> Rc<dyn HoldClient> {
    Rc::new(SyntheticHolds)
}

#[derive(Debug)]
struct SyntheticHolds;

impl SyntheticHolds {
    fn receipt(
        app: &AppId,
        deployment: &DeploymentId,
        generation: HoldGeneration,
        state: HoldState,
    ) -> HoldReceipt {
        HoldReceipt {
            app_id: app.clone(),
            deploy_id: deployment.as_str().into(),
            deploy_hash: zeroship_bundle::sha256_hex(deployment.as_str().as_bytes()),
            holder_id: HoldScope::for_queue(app.clone()).holder().into(),
            generation,
            state,
        }
    }
}

impl HoldClient for SyntheticHolds {
    fn acquire<'a>(
        &'a self,
        app: &'a AppId,
        deployment: &'a DeploymentId,
        generation: HoldGeneration,
    ) -> Pin<Box<dyn Future<Output = Result<HoldReceipt, Error>> + 'a>> {
        Box::pin(async move { Ok(Self::receipt(app, deployment, generation, HoldState::Held)) })
    }

    fn release<'a>(
        &'a self,
        app: &'a AppId,
        deployment: &'a DeploymentId,
        generation: HoldGeneration,
    ) -> Pin<Box<dyn Future<Output = Result<HoldReceipt, Error>> + 'a>> {
        Box::pin(async move {
            Ok(Self::receipt(
                app,
                deployment,
                generation,
                HoldState::Released,
            ))
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Backend {
    Postgres,
    Sqlite,
}

pub enum Admin {
    Postgres(compio_postgres::Client),
    Sqlite(rusqlite::Connection),
}

pub struct Fixture {
    pub admin: Admin,
    runtime_url: String,
    schema: SchemaName,
    _work: tempfile::TempDir,
    _postgres: Option<Container<GenericImage>>,
}

impl Fixture {
    pub fn new(backend: Backend) -> Pin<Box<dyn Future<Output = Self>>> {
        Box::pin(async move {
            match backend {
                Backend::Postgres => Self::postgres().await,
                Backend::Sqlite => Self::sqlite(),
            }
        })
    }

    async fn postgres() -> Self {
        let postgres = GenericImage::new("postgres", "18")
            .with_exposed_port(5432.tcp())
            .with_wait_for(WaitFor::message_on_stdout(
                "PostgreSQL init process complete; ready for start up.",
            ))
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_HOST_AUTH_METHOD", "trust")
            .start()
            .expect("manager tests require Testcontainers PostgreSQL");
        let address = format!(
            "{}:{}",
            postgres.get_host().unwrap(),
            postgres.get_host_port_ipv4(5432).unwrap()
        );
        let admin = connect(&format!("postgres://postgres@{address}/postgres")).await;
        admin
            .batch_execute(
                "CREATE ROLE workflow_manager_test LOGIN NOSUPERUSER NOCREATEDB \
                    NOCREATEROLE NOREPLICATION NOINHERIT NOBYPASSRLS;
                 REVOKE ALL ON DATABASE postgres FROM PUBLIC;
                 GRANT CONNECT ON DATABASE postgres TO workflow_manager_test;
                 REVOKE ALL ON SCHEMA public FROM PUBLIC;
                 CREATE SCHEMA workflow_manager;
                 CREATE SCHEMA customer;
                 CREATE TABLE customer.__zeroship_workflow_history \
                    (id text PRIMARY KEY, secret text NOT NULL);
                 INSERT INTO customer.__zeroship_workflow_history \
                    VALUES ('private-history', 'customer-private-history');
                 REVOKE ALL ON SCHEMA customer FROM PUBLIC;",
            )
            .await
            .unwrap();
        admin
            .batch_execute(include_str!("../../schema/postgres.sql"))
            .await
            .expect("generated manager PostgreSQL schema must apply");
        admin
            .batch_execute(
                "GRANT USAGE ON SCHEMA workflow_manager TO workflow_manager_test;
                 GRANT SELECT, INSERT, UPDATE, DELETE \
                    ON ALL TABLES IN SCHEMA workflow_manager TO workflow_manager_test;
                 REVOKE INSERT, UPDATE, DELETE ON workflow_manager.schema_version FROM workflow_manager_test;",
            )
            .await
            .unwrap();
        Self {
            admin: Admin::Postgres(admin),
            runtime_url: format!("postgres://workflow_manager_test@{address}/postgres"),
            schema: SchemaName::new("workflow_manager").unwrap(),
            _work: tempfile::tempdir().unwrap(),
            _postgres: Some(postgres),
        }
    }

    fn sqlite() -> Self {
        let work = tempfile::tempdir().unwrap();
        // The service binds SQLite's main schema in its configured platform file.
        let admin = rusqlite::Connection::open(work.path().join("manager.sqlite")).unwrap();
        admin
            .execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")
            .unwrap();
        admin
            .execute_batch(include_str!("../../schema/sqlite.sql"))
            .expect("generated manager SQLite schema must apply");
        Self {
            admin: Admin::Sqlite(admin),
            runtime_url: work
                .path()
                .join("manager.sqlite")
                .to_str()
                .unwrap()
                .to_owned(),
            schema: SchemaName::new("main").unwrap(),
            _work: work,
            _postgres: None,
        }
    }

    pub fn url(&self) -> &str {
        &self.runtime_url
    }

    pub const fn schema(&self) -> &SchemaName {
        &self.schema
    }

    pub fn binding(&self) -> DbBinding {
        DbBinding::platform("workflow_manager", "manager-test", self.schema().clone())
    }

    pub fn options(&self) -> ConnectOptions {
        ConnectOptions::new(self.url(), ProjectKeySource::unavailable()).connection_authority()
    }

    /// Each call opens a host with independent connections to the same queue.
    pub async fn database(&self) -> Database {
        Database::connect(
            self.binding(),
            self.options(),
            zeroship_workflow_manager::collections().unwrap(),
        )
        .await
        .unwrap()
    }
}

pub async fn connect(url: &str) -> compio_postgres::Client {
    let (client, connection) = compio_postgres::connect(url, compio_postgres::NoTls)
        .await
        .unwrap();
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
}

/// The app's configured delivery budget, for cases that exercise something
/// other than the ceiling itself.
pub fn delivery_ceiling() -> i64 {
    AppPolicy::default().max_delivery_attempts
}
