#![allow(
    clippy::future_not_send,
    reason = "fixture connections stay on their compio runtime"
)]

use std::{
    future::{ready, Future},
    pin::Pin,
    rc::Rc,
};
use zeroship_core::{
    app_id::AppId,
    schema_name::SchemaName,
    typed_id,
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
use zeroship_testkit::postgres::server::Postgres as BareServer;
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
    admin_url: String,
    schema: SchemaName,
    /// This case's own minted login. `PostgreSQL` roles are cluster-global
    /// (`pg_authid`), and the bare server now serves every case of the run, so
    /// a fixed name would let one case's grants - or a role-membership
    /// mutation - reach another case's session; `None` on `SQLite`, which
    /// carries no roles.
    role: Option<String>,
    /// `host:port` of the shared server and this case's own database name, for
    /// building a connection string to another role on the same clone
    /// (`Self::role_url`); `None` on `SQLite`.
    address: Option<(String, String)>,
    _work: tempfile::TempDir,
    /// The case's clone of the shared bare server. Its `Drop` force-drops the
    /// clone; `Fixture`'s own `Drop` runs that first and then drops the minted
    /// role, once no ACL entry anywhere in the cluster still names it.
    postgres: Option<BareServer>,
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
        let postgres = BareServer::start();
        let address = format!("127.0.0.1:{}", postgres.port());
        let database = postgres.name().to_owned();
        let admin_url = postgres.url();
        let admin = connect(&admin_url).await;
        // Minted per case rather than fixed: see the `role` field doc.
        let role = typed_id::generate("wmt");
        admin
            .batch_execute(&format!(
                "CREATE ROLE \"{role}\" LOGIN PASSWORD '{role}' NOSUPERUSER NOCREATEDB \
                    NOCREATEROLE NOREPLICATION NOINHERIT NOBYPASSRLS;
                 REVOKE ALL ON DATABASE \"{database}\" FROM PUBLIC;
                 GRANT CONNECT ON DATABASE \"{database}\" TO \"{role}\";
                 REVOKE ALL ON SCHEMA public FROM PUBLIC;
                 CREATE SCHEMA workflow_manager;
                 CREATE SCHEMA customer;
                 CREATE TABLE customer.__zeroship_workflow_history \
                    (id text PRIMARY KEY, secret text NOT NULL);
                 INSERT INTO customer.__zeroship_workflow_history \
                    VALUES ('private-history', 'customer-private-history');
                 REVOKE ALL ON SCHEMA customer FROM PUBLIC;"
            ))
            .await
            .unwrap();
        admin
            .batch_execute(include_str!("../../schema/postgres.sql"))
            .await
            .expect("generated manager PostgreSQL schema must apply");
        admin
            .batch_execute(&format!(
                "GRANT USAGE ON SCHEMA workflow_manager TO \"{role}\";
                 GRANT SELECT, INSERT, UPDATE, DELETE \
                    ON ALL TABLES IN SCHEMA workflow_manager TO \"{role}\";
                 REVOKE INSERT, UPDATE, DELETE ON workflow_manager.schema_version FROM \"{role}\";"
            ))
            .await
            .unwrap();
        Self {
            admin: Admin::Postgres(admin),
            runtime_url: format!("postgres://{role}:{role}@{address}/{database}"),
            admin_url,
            schema: SchemaName::new("workflow_manager").unwrap(),
            role: Some(role),
            address: Some((address, database)),
            _work: tempfile::tempdir().unwrap(),
            postgres: Some(postgres),
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
        let url = work
            .path()
            .join("manager.sqlite")
            .to_str()
            .unwrap()
            .to_owned();
        Self {
            admin: Admin::Sqlite(admin),
            runtime_url: url.clone(),
            admin_url: url,
            schema: SchemaName::new("main").unwrap(),
            role: None,
            address: None,
            _work: work,
            postgres: None,
        }
    }

    pub fn url(&self) -> &str {
        &self.runtime_url
    }

    /// The fixture's superuser connection string, for a case that provisions
    /// something the scoped runtime role was never granted (Control's
    /// deployment catalog, in its own schema).
    pub fn admin_url(&self) -> &str {
        &self.admin_url
    }

    /// A connection string for another role on this case's own database.
    /// `PostgreSQL` only; the caller mints and creates `role` itself.
    pub fn role_url(&self, role: &str) -> String {
        let (address, database) = self
            .address
            .as_ref()
            .expect("role_url is a PostgreSQL-only fixture helper");
        format!("postgres://{role}:{role}@{address}/{database}")
    }

    /// This case's own database name, for a caller that grants a role of its
    /// own `CONNECT` on it directly. `PostgreSQL` only.
    pub fn database_name(&self) -> &str {
        &self
            .address
            .as_ref()
            .expect("database_name() is a PostgreSQL-only fixture helper")
            .1
    }

    /// This case's own minted runtime login, bare - the name a
    /// `pg_stat_activity`/`pg_locks` probe filters its own session by, since
    /// the role is unique to this case rather than a fixed, shared name.
    /// `PostgreSQL` only.
    pub fn role(&self) -> &str {
        self.role
            .as_deref()
            .expect("role() is a PostgreSQL-only fixture helper")
    }

    /// The Docker id of the shared bare server this case's database lives on,
    /// so a contract can show two cases of a run share one container.
    /// `PostgreSQL` only.
    pub fn container_id(&self) -> &str {
        self.postgres
            .as_ref()
            .expect("container_id() is a PostgreSQL-only fixture helper")
            .container_id()
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

impl Drop for Fixture {
    fn drop(&mut self) {
        // Drop the per-case clone first: `pg_shdepend` tracks every ACL entry
        // granted to a role across the whole cluster, so with the clone (and
        // its grants to the minted role) still present, `DROP ROLE` would
        // refuse, naming privileges in this very case's own database.
        let server = self.postgres.take();
        let container_id = server
            .as_ref()
            .map(|server| server.container_id().to_owned());
        drop(server);
        if let (Some(role), Some(container_id)) = (self.role.take(), container_id) {
            // "postgres" is the shared server's own always-connectable
            // maintenance database, not this case's now-removed clone.
            let _ = zeroship_testkit::shared::psql(
                &container_id,
                "postgres",
                &format!("DROP ROLE IF EXISTS \"{role}\""),
            );
        }
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
