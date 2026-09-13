#![allow(
    clippy::future_not_send,
    reason = "fixture connections stay on their compio runtime"
)]

use std::{future::Future, pin::Pin};
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};
use zeroship_core::schema_name::SchemaName;
use zeroship_data_orm::{
    binding::DbBinding, encryption::ProjectKeySource, orm::Database, ConnectOptions,
};

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
                    ON ALL TABLES IN SCHEMA workflow_manager TO workflow_manager_test;",
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
        DbBinding::new("workflow_manager", "manager-test", self.schema.clone())
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
