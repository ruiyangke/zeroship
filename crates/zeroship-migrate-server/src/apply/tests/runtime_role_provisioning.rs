#![expect(
    clippy::future_not_send,
    reason = "native provisioning fixtures remain on their compio runtime"
)]

use super::provision_runtime_app_role;
use crate::provisioning::provision_database;
use compio_postgres::{error::SqlState, Client};
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};
use zeroship_core::{app_derivation, schema_name::SchemaName};
use zeroship_id::AppId;
use zeroship_migrate_postgres::role::migrator_role_name;

struct Fixture {
    admin: Client,
    migrator: Client,
    worker: Client,
    _postgres: Container<GenericImage>,
}

impl Fixture {
    async fn new() -> Self {
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
            .expect("runtime role provisioning requires Testcontainers PostgreSQL");
        let address = format!(
            "{}:{}",
            postgres.get_host().unwrap(),
            postgres.get_host_port_ipv4(5432).unwrap()
        );
        let admin = connect(&format!("postgres://postgres@{address}/postgres")).await;
        admin
            .batch_execute(
                "CREATE ROLE zeroship_worker LOGIN NOSUPERUSER NOCREATEDB \
                    NOCREATEROLE NOINHERIT NOREPLICATION NOBYPASSRLS;
                 CREATE ROLE creator_migration_test LOGIN NOSUPERUSER NOCREATEDB \
                    NOCREATEROLE NOINHERIT NOREPLICATION NOBYPASSRLS;
                 REVOKE ALL ON SCHEMA public FROM PUBLIC;
                 REVOKE ALL ON DATABASE postgres FROM PUBLIC;
                 GRANT CONNECT ON DATABASE postgres TO zeroship_worker,creator_migration_test;",
            )
            .await
            .unwrap();
        Self {
            admin,
            migrator: connect(&format!(
                "postgres://creator_migration_test@{address}/postgres"
            ))
            .await,
            worker: connect(&format!("postgres://zeroship_worker@{address}/postgres")).await,
            _postgres: postgres,
        }
    }
}

async fn connect(url: &str) -> Client {
    let (client, connection) = compio_postgres::connect(url, compio_postgres::NoTls)
        .await
        .unwrap();
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
}

#[compio::test]
async fn repeated_runtime_provisioning_preserves_creator_migration_authority() {
    let mut fixture = Fixture::new().await;
    let app = AppId::mint();
    let schema = SchemaName::new(&app_derivation::schema_name(&app)).unwrap();
    let role = migrator_role_name(schema.as_str()).unwrap();
    provision_database(&fixture.admin, schema.as_str())
        .await
        .unwrap();
    fixture
        .admin
        .batch_execute(&format!("GRANT \"{role}\" TO creator_migration_test"))
        .await
        .unwrap();
    let sibling = AppId::mint();
    let sibling_schema = app_derivation::schema_name(&sibling);
    provision_database(&fixture.admin, &sibling_schema)
        .await
        .unwrap();
    fixture
        .admin
        .batch_execute(&format!(
            "CREATE TABLE \"{sibling_schema}\".private_data(id text PRIMARY KEY)"
        ))
        .await
        .unwrap();
    for round in 0..3 {
        assert_owner(&fixture.admin, &schema, &role).await;
        provision_runtime_app_role(&fixture.admin, &schema, &role)
            .await
            .unwrap();
        assert_owner(&fixture.admin, &schema, &role).await;
        let table = format!("migration_probe_{round}");
        migrate_table(&mut fixture, &schema, &role, &table).await;
        assert_runtime_scope(&mut fixture, &app, &schema, &sibling_schema, &table).await;
        provision_database(&fixture.admin, schema.as_str())
            .await
            .unwrap();
    }
}

async fn assert_owner(admin: &Client, schema: &SchemaName, expected: &str) {
    let row = admin
        .query_one(
            "SELECT pg_get_userbyid(nspowner) FROM pg_namespace WHERE nspname=$1",
            &[&schema.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, &str>(0), expected);
}

async fn migrate_table(fixture: &mut Fixture, schema: &SchemaName, role: &str, table: &str) {
    let schema = schema.as_str();
    let transaction = fixture.migrator.transaction().await.unwrap();
    transaction
        .batch_execute(&format!("SET LOCAL ROLE \"{role}\""))
        .await
        .unwrap();
    let identity = transaction
        .query_one("SELECT current_user, session_user", &[])
        .await
        .unwrap();
    assert_eq!(identity.get::<_, &str>(0), role);
    assert_eq!(identity.get::<_, &str>(1), "creator_migration_test");
    transaction
        .batch_execute(&format!(
            "CREATE TABLE \"{schema}\".\"{table}\"(id text PRIMARY KEY, content text NOT NULL)"
        ))
        .await
        .expect("runtime role provisioning must preserve creator migration CREATE authority");
    transaction.commit().await.unwrap();
}

async fn assert_runtime_scope(
    fixture: &mut Fixture,
    app: &AppId,
    schema: &SchemaName,
    sibling_schema: &str,
    table: &str,
) {
    let schema = schema.as_str();
    let role = app_derivation::role_name(app).unwrap();
    let transaction = fixture.worker.transaction().await.unwrap();
    transaction
        .batch_execute(&format!("SET LOCAL ROLE \"{role}\""))
        .await
        .unwrap();
    let identity = transaction
        .query_one("SELECT current_user, session_user", &[])
        .await
        .unwrap();
    assert_eq!(identity.get::<_, &str>(0), role);
    assert_eq!(identity.get::<_, &str>(1), "zeroship_worker");
    assert_eq!(
        transaction
            .execute(
                &format!("INSERT INTO \"{schema}\".\"{table}\" VALUES ($1,$2)"),
                &[&"app-row", &"creator-data"],
            )
            .await
            .unwrap(),
        1
    );
    transaction.commit().await.unwrap();
    for sql in [
        format!("CREATE TABLE \"{schema}\".worker_ddl(id text PRIMARY KEY)"),
        format!("SELECT * FROM \"{sibling_schema}\".private_data"),
        format!("CREATE TABLE \"{sibling_schema}\".worker_ddl(id text PRIMARY KEY)"),
    ] {
        let transaction = fixture.worker.transaction().await.unwrap();
        transaction
            .batch_execute(&format!("SET LOCAL ROLE \"{role}\""))
            .await
            .unwrap();
        let error = transaction.batch_execute(&sql).await.unwrap_err();
        assert_eq!(error.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE));
        transaction.rollback().await.unwrap();
    }
}
