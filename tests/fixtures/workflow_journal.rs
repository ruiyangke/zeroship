//! Creator journals on SQLite and PostgreSQL, and apps registered on them.
//!
//! Both halves of the workflow engine open journals this way. The declaring
//! module supplies the sibling fixtures this one composes: `deployment_fixture`,
//! `manager_queue` and `service_binding`.

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
use std::{path::Path, rc::Rc, sync::Arc};
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};
use zeroship_core::{app_id::AppId, typed_id};
use zeroship_workflow::service::{
    schema,
    store::{OrmStore, SchemaName},
    AppPolicy, DeployRegistration, HostPolicies, PolicySnapshot, WorkflowService,
};

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

pub async fn sqlite_store(path: &Path) -> OrmStore {
    let store = orm_store(
        &format!(
            "sqlite:{}",
            path.parent().unwrap().join("orm.sqlite").display()
        ),
        SchemaName::new("workflow").unwrap(),
    )
    .await;
    schema::initialize_local(&store).await.unwrap();
    store
}

pub async fn orm_store(url: &str, schema: SchemaName) -> OrmStore {
    OrmStore::connect(
        zeroship_data_orm::binding::DbBinding::platform("workflow", "test-deployment", schema),
        &zeroship_data_orm::connection::ConnectionFactory::for_platform_url(url).unwrap(),
        zeroship_data_orm::encryption::ProjectKeySource::unavailable(),
    )
    .await
    .unwrap()
}

pub struct PostgresFixture {
    _container: Container<GenericImage>,
    pub store: OrmStore,
    pub admin_url: String,
}
impl PostgresFixture {
    pub async fn start() -> Self {
        let container = GenericImage::new("postgres", "18")
            .with_exposed_port(5432.tcp())
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_HOST_AUTH_METHOD", "trust")
            .start()
            .expect("workflow PostgreSQL container");
        let host = container.get_host().unwrap();
        let port = container.get_host_port_ipv4(5432).unwrap();
        let admin_url = format!("postgres://postgres@{host}:{port}/postgres");
        let admin = connect(&admin_url).await;
        admin
            .batch_execute(
                "CREATE ROLE customer_migrator NOLOGIN; \
             CREATE ROLE customer_worker LOGIN; CREATE ROLE app_customer_role NOLOGIN; \
             GRANT app_customer_role TO customer_worker; CREATE ROLE zeroship_worker LOGIN; \
             CREATE ROLE zeroship_gateway LOGIN; CREATE ROLE zeroship_app LOGIN; \
             CREATE ROLE zeroship_control LOGIN; CREATE ROLE zeroship_workflow LOGIN; \
             CREATE SCHEMA customer AUTHORIZATION customer_migrator; \
             SET ROLE customer_migrator;",
            )
            .await
            .unwrap();
        let schema_name = SchemaName::new("customer").unwrap();
        admin
            .batch_execute(&schema::postgres_sql(&schema_name))
            .await
            .unwrap();
        admin.batch_execute(
            "RESET ROLE; GRANT USAGE ON SCHEMA customer TO app_customer_role; \
             GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA customer TO app_customer_role;"
        ).await.unwrap();
        Self {
            _container: container,
            store: orm_store(
                &format!("postgres://customer_worker@{host}:{port}/postgres"),
                schema_name,
            )
            .await,
            admin_url,
        }
    }
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
