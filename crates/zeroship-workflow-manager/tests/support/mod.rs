#![allow(
    clippy::future_not_send,
    reason = "fixture connections stay on their compio runtime"
)]

use std::{future::Future, pin::Pin, rc::Rc};
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};
use zeroship_core::{
    app_id::AppId,
    schema_name::SchemaName,
    workflow_deployments::{HoldGeneration, HoldReceipt, HoldScope, HoldState},
    workflow_jobs::DeploymentId,
};
use zeroship_data_orm::{
    binding::DbBinding, encryption::ProjectKeySource, orm::Database, ConnectOptions,
};
use zeroship_workflow_manager::{
    capacity::{Contract, LocalCapacity},
    coordinator::{self, Coordinator},
    driver::{self, Driver},
    eligibility::{EligibilitySource, LocalEligibility, ZoneId},
    lifecycle::{AppLifecycle, Undeletable},
    retention::HoldClient,
    Error, Queue,
};

/// Trusted single-zone facts for contracts that do not exercise eligibility:
/// every app and worker is in the seeded zone and active.
#[allow(dead_code, reason = "placement contracts compose their own facts")]
pub fn local_eligibility() -> Rc<dyn EligibilitySource> {
    Rc::new(LocalEligibility::new(ZoneId::default_zone()))
}

/// A coordinator over `queue` under trusted single-zone facts.
#[allow(dead_code, reason = "queue-only contracts construct no coordinator")]
pub fn coordinator(queue: &Queue, options: coordinator::Options) -> Coordinator {
    Coordinator::new(queue.clone(), options, local_eligibility()).unwrap()
}

/// A driver whose placement lane runs under trusted single-zone facts and the
/// local host's always-satisfied capacity. No app is ever deleted.
#[allow(dead_code, reason = "only maintenance contracts run a driver")]
pub fn local_driver(queue: &Queue, options: driver::Options) -> Driver {
    driver_for(queue, options, Rc::new(Undeletable))
}

/// The same driver under a caller's deletion source, for the closing lane.
#[allow(dead_code, reason = "only closing contracts report deletions")]
pub fn driver_for(
    queue: &Queue,
    options: driver::Options,
    lifecycle: Rc<dyn AppLifecycle>,
) -> Driver {
    try_driver(queue, options, lifecycle).unwrap()
}

/// The same construction, reporting its option validation instead of panicking.
#[allow(dead_code, reason = "only option contracts assert a refusal")]
pub fn try_driver(
    queue: &Queue,
    options: driver::Options,
    lifecycle: Rc<dyn AppLifecycle>,
) -> Result<Driver, Error> {
    Driver::new(
        coordinator(queue, coordinator::Options::default()),
        options,
        lifecycle,
        Contract::declarative(Rc::new(LocalCapacity)),
    )
}

/// Queue state-machine tests use invented deployment identities. Retention
/// safety tests instead compose the real catalog and published app artifacts.
#[allow(
    dead_code,
    reason = "retention safety tests bind the real deployment catalog"
)]
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
        DbBinding::new("workflow_manager", "manager-test", self.schema().clone())
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
