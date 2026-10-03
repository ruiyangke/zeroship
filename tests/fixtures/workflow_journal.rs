//! The workflow journal on `SQLite` and `PostgreSQL`, opened where every host opens
//! it, and apps registered on it.
//!
//! Both halves of the workflow engine open journals this way. Every store here
//! comes from `HostStorage`, so it is on `journal_binding()` exactly as the
//! production service's and `zeroship serve`'s are. On `PostgreSQL` the journal
//! carries the posture `db/migrations-ts/20260919000000_workflow_journal.ts`
//! leaves it in: `JOURNAL_SCHEMA` and its tables owned by the migration role,
//! and the service login holding schema usage and table DML and nothing else.
//!
//! The declaring module supplies the sibling fixtures this one composes:
//! `deployment_fixture`, `manager_queue` and `service_binding`.

#![allow(
    dead_code,
    reason = "fixture consumers open journals on different backends"
)]
#![expect(
    clippy::future_not_send,
    reason = "fixtures use their owning compio thread"
)]

use super::{
    deployment_fixture::Deployments, manager_queue::open_epoch, service_binding::ServiceFixture,
};
use compio_postgres::NoTls;
use std::{
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};
use zeroship_core::{app_id::AppId, typed_id};
use zeroship_testkit::postgres::server::Postgres;
use zeroship_data_orm::connection::ConnectionFactory;
use zeroship_workflow::service::{
    schema,
    store::{HostStorage, OrmStore, SchemaName, JOURNAL_SCHEMA},
    AppPolicy, DeployRegistration, HostPolicies, PolicySnapshot, WorkflowService,
};

/// The role that owns the journal's schema and tables, as the platform
/// migrations create it.
pub const JOURNAL_OWNER: &str = "zeroship_workflow_migrator";

/// The login the journal is read and written through: the workflow service's,
/// which the platform migration grants the journal to.
pub const JOURNAL_LOGIN: &str = "zeroship_workflow";

/// Host policy whose authority expires, as a worker's does once the manager
/// leases it rather than the configuration granting it.
pub fn leased_policy(revision: i64, policy: AppPolicy) -> PolicySnapshot {
    PolicySnapshot::lease(
        revision.try_into().unwrap(),
        policy,
        std::time::Instant::now() + std::time::Duration::from_secs(3600),
    )
    .unwrap()
    .with_ingress_epoch(Some(open_epoch()))
}

/// The file the journal binding keeps the journal in, inside `directory`.
///
/// On `SQLite` the binding's schema is the `ATTACH` alias, and the backend keeps an
/// attached alias in `zs-<alias>.sqlite` beside the session file.
pub fn journal_file(directory: &Path) -> PathBuf {
    directory.join(format!("zs-{JOURNAL_SCHEMA}.sqlite"))
}

/// The journal a host opens over a session file in `path`'s directory, installed.
///
/// Only the directory is read. The journal is kept in [`journal_file`] of it,
/// which is the path a case that installs, reads or faults the file directly
/// must name.
pub async fn sqlite_store(path: &Path) -> OrmStore {
    let directory = path.parent().expect("the path has a directory");
    let store = orm_store(&format!(
        "sqlite:{}",
        directory.join("orm.sqlite").display()
    ))
    .await;
    schema::initialize_local(&store).await.unwrap();
    store
}

/// The journal a host opens over `url`: `HostStorage`, so `journal_binding()`.
pub async fn orm_store(url: &str) -> OrmStore {
    HostStorage::new(ConnectionFactory::for_platform_url(url).unwrap())
        .open()
        .await
        .unwrap()
}

pub struct PostgresFixture {
    /// The journal opened on the case's database as [`JOURNAL_LOGIN`].
    pub store: OrmStore,
    pub admin_url: String,
    /// The server `admin_url` names, authenticated as [`JOURNAL_LOGIN`].
    pub journal_url: String,
    /// The shared bare server database this case owns. Declared after `store`
    /// so the case's connections close before the clone is removed.
    _database: Postgres,
}
impl PostgresFixture {
    pub async fn start() -> Self {
        let database = Postgres::start();
        // A role's password is its name, as the platform migrations set it, and
        // the URL carries it because the server authenticates the published
        // port, not the container's loopback.
        let admin_url = database.url();
        let admin = connect(&admin_url).await;
        ensure_roles(&admin).await;
        install_journal(&admin).await;
        grant_journal(&admin).await;
        let journal_url = admin_url.replacen(
            "postgres:fixture@",
            &format!("{JOURNAL_LOGIN}:{JOURNAL_LOGIN}@"),
            1,
        );
        Self {
            store: orm_store(&journal_url).await,
            admin_url,
            journal_url,
            _database: database,
        }
    }

    /// The Docker id of the shared server this case's database lives on.
    #[must_use]
    pub fn container_id(&self) -> &str {
        self._database.container_id()
    }
}

/// Create the journal's owner, login and the platform roles it must stay closed
/// to, leaving a role that already exists.
///
/// The bare server outlives one case and every case of a run shares it, so the
/// roles are cluster-global and created once. The DO block catches the duplicate
/// two cases racing to create the same role raise, rather than serializing them.
async fn ensure_roles(admin: &compio_postgres::Client) {
    let mut roles = vec![
        (JOURNAL_OWNER, "NOLOGIN".to_owned()),
        (
            JOURNAL_LOGIN,
            format!("LOGIN PASSWORD '{JOURNAL_LOGIN}'"),
        ),
    ];
    for role in [
        "zeroship_worker",
        "zeroship_gateway",
        "zeroship_app",
        "zeroship_control",
    ] {
        roles.push((role, format!("LOGIN PASSWORD '{role}'")));
    }
    let guarded = roles
        .iter()
        .map(|(name, attributes)| {
            format!(
                "IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{name}') THEN \
                 BEGIN CREATE ROLE {name} {attributes}; \
                 EXCEPTION WHEN unique_violation OR duplicate_object THEN NULL; END; \
                 END IF;"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    admin
        .batch_execute(&format!("DO $$\nBEGIN\n{guarded}\nEND $$;"))
        .await
        .unwrap();
}

/// Install the journal into `admin`'s database as the platform migration does:
/// [`JOURNAL_SCHEMA`] and every table in it owned by [`JOURNAL_OWNER`].
pub async fn install_journal(admin: &compio_postgres::Client) {
    admin
        .batch_execute(&format!(
            "CREATE SCHEMA {JOURNAL_SCHEMA} AUTHORIZATION {JOURNAL_OWNER}; \
             SET ROLE {JOURNAL_OWNER};"
        ))
        .await
        .unwrap();
    admin
        .batch_execute(&schema::postgres_sql(
            &SchemaName::new(JOURNAL_SCHEMA).unwrap(),
        ))
        .await
        .unwrap();
    admin.batch_execute("RESET ROLE;").await.unwrap();
}

/// Grant `admin`'s journal to [`JOURNAL_LOGIN`] as the platform migrations do:
/// usage on the schema and DML on its tables, and no DDL.
pub async fn grant_journal(admin: &compio_postgres::Client) {
    admin
        .batch_execute(&format!(
            "GRANT USAGE ON SCHEMA {JOURNAL_SCHEMA} TO {JOURNAL_LOGIN}; \
             GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA {JOURNAL_SCHEMA} \
             TO {JOURNAL_LOGIN};"
        ))
        .await
        .unwrap();
}

pub async fn connect(url: &str) -> compio_postgres::Client {
    let (client, connection) = compio_postgres::connect(url, NoTls).await.unwrap();
    compio::runtime::spawn(async move {
        connection.run().await.unwrap();
    })
    .detach();
    client
}

/// A host service with two registered apps, each on an active deployment.
pub async fn registered_service(
    store: Rc<OrmStore>,
) -> (WorkflowService, AppId, AppId, Deployments) {
    registered_with_deployments(store, Deployments::new().await).await
}

pub async fn registered_with_deployments(
    store: Rc<OrmStore>,
    deployments: Deployments,
) -> (WorkflowService, AppId, AppId, Deployments) {
    let a = AppId::mint();
    let b = AppId::mint();
    let service = WorkflowService::open(store, Arc::new(HostPolicies::default()))
        .await
        .unwrap()
        .with_deployments(deployments.binding(&[&a, &b]));
    for app in [&a, &b] {
        service
            .fixture_register(app, leased_policy(1, AppPolicy::default()))
            .await
            .unwrap();
        service
            .fixture_register(app, leased_policy(1, AppPolicy::default()))
            .await
            .unwrap();
        deployments
            .activate(
                &service,
                app,
                &DeployRegistration {
                    id: typed_id::generate("dep"),
                    hash: "a".repeat(64),
                    workflows: ["Example".into(), "Child".into()].into(),
                    schedules: Vec::new(),
                },
            )
            .await
            .unwrap();
    }
    (service, a, b, deployments)
}

/// Count the journal rows of one table that match a filter.
pub async fn journal_row_count(
    tx: &zeroship_workflow::service::store::Transaction,
    table: &str,
    filter: serde_json::Value,
) -> usize {
    use zeroship_data_orm::{orm::Output, value};
    let collection = tx
        .database()
        .collection(&format!("__zeroship_workflow_{table}"))
        .unwrap();
    let mut counted = 0;
    loop {
        let Output::Rows { rows: page, .. } = collection
            .find(
                filter.clone().into(),
                value!({"offset":counted, "limit":zeroship_data_orm::sql::MAX_ROW_LIMIT, "orderBy":{"id":1}}),
            )
            .await
            .unwrap()
        else {
            panic!("expected journal rows")
        };
        if page.is_empty() {
            return counted;
        }
        counted += page.len();
    }
}

/// A resolved frontier carrying the given runtime outcomes.
pub fn execution(value: serde_json::Value) -> zeroship_workflow::WorkflowExecution {
    zeroship_workflow::WorkflowExecution::from_runtime_value(serde_json::json!({"outcomes":value}))
        .unwrap()
}
