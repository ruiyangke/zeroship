//! Own the database and drain this thread's gateway pool before runtime teardown.

#![allow(
    clippy::future_not_send,
    reason = "the fixture belongs to its compio runtime"
)]

use compio_postgres::{Client, NoTls};
use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use std::sync::OnceLock;
use std::time::Duration;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::{runners::SyncRunner, Container, GenericImage, ImageExt};

use super::super::{DbConfig, POOL};

mod migrations;

pub(crate) struct Database {
    postgres: Container<GenericImage>,
    pub(crate) url: url::Url,
    pub(crate) admin: Client,
    driver: compio::runtime::JoinHandle<Result<(), compio_postgres::Error>>,
}

impl Database {
    pub(crate) async fn run(test: impl AsyncFnOnce(&Self)) {
        Self::run_image(image(), test).await;
    }

    pub(crate) async fn migrated(test: impl AsyncFnOnce(&Self)) {
        static SEED: OnceLock<migrations::Seed> = OnceLock::new();
        let seed = SEED.get_or_init(migrations::Seed::build);
        let image = image()
            .with_env_var("POSTGRES_DB", "postgres")
            .with_copy_to("/docker-entrypoint-initdb.d/roles.sql", seed.roles.clone())
            .with_copy_to(
                "/docker-entrypoint-initdb.d/schema.sql",
                seed.database.clone(),
            );
        Self::run_image(image, test).await;
    }

    async fn run_image(
        image: testcontainers::ContainerRequest<GenericImage>,
        test: impl AsyncFnOnce(&Self),
    ) {
        assert!(
            POOL.with(|pool| pool.borrow().is_none()),
            "a previous case retained its gateway pool"
        );
        let postgres = image
            .start()
            .expect("gateway database tests require Docker and PostgreSQL");
        let url = database_url(&postgres);
        let mut config: compio_postgres::Config = url.as_str().parse().unwrap();
        config.connect_timeout(Duration::from_secs(10));
        let (admin, connection) = config
            .connect(NoTls)
            .await
            .expect("connect fixture observer");
        let database = Self {
            postgres,
            url,
            admin,
            driver: compio::runtime::spawn(async move { connection.run().await }),
        };
        let outcome = AssertUnwindSafe(async {
            compio::time::timeout(Duration::from_secs(45), Box::pin(test(&database)))
                .await
                .expect("gateway database case timed out");
        })
        .catch_unwind()
        .await;

        let drained = compio::time::timeout(Duration::from_secs(15), async {
            let cached = POOL.with(|slot| slot.borrow_mut().take());
            if let Some((_, pool)) = cached {
                pool.close().await;
            }
            loop {
                let empty: bool = database.admin.query_one(
                    "SELECT NOT EXISTS (SELECT FROM pg_stat_activity WHERE backend_type = 'client backend' AND pid <> pg_backend_pid())", &[]
                ).await.expect("observe pool connection cleanup").get(0);
                if empty { break; }
                compio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await;
        let Self {
            postgres,
            admin,
            driver,
            ..
        } = database;
        drop(admin);
        let closed = compio::time::timeout(Duration::from_secs(15), driver).await;
        let removed = postgres.rm();
        drained.expect("pool connections must close before the database is removed");
        closed
            .expect("observer connection timed out")
            .expect("observer task")
            .expect("observer connection");
        removed.expect("remove the owned PostgreSQL server");
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    pub(crate) fn config(&self, capacity: usize) -> DbConfig {
        DbConfig::new(self.url.as_str(), capacity)
    }

    pub(crate) fn config_as(&self, role: &str, capacity: usize) -> DbConfig {
        let mut url = self.url.clone();
        url.set_username(role).unwrap();
        url.set_password(Some(role)).unwrap();
        DbConfig::new(url.as_str(), capacity)
    }

    pub(crate) async fn wait_until_blocked(&self, pids: &[i32]) {
        assert!(!pids.is_empty(), "lock observation needs active backends");
        compio::time::timeout(Duration::from_secs(10), async {
            loop {
                let blocked: bool = self.admin.query_one(
                    "SELECT bool_and(cardinality(pg_blocking_pids(pid)) > 0) FROM unnest($1::int[]) AS requested(pid)", &[&pids]
                ).await.expect("observe blocked pool queries").get(0);
                if blocked { break; }
                compio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await.expect("all pool queries must reach the database lock");
    }
}

fn image() -> testcontainers::ContainerRequest<GenericImage> {
    GenericImage::new("postgres", "17")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stdout(
            "PostgreSQL init process complete; ready for start up.",
        ))
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_PASSWORD", "fixture")
        .with_env_var("POSTGRES_DB", "gateway_tests")
        .with_startup_timeout(Duration::from_secs(120))
}

fn database_url(postgres: &Container<GenericImage>) -> url::Url {
    let mut url = url::Url::parse("postgresql://postgres:fixture@localhost/gateway_tests").unwrap();
    url.set_host(Some(
        &postgres.get_host().expect("database host").to_string(),
    ))
    .unwrap();
    url.set_port(Some(
        postgres.get_host_port_ipv4(5432).expect("database port"),
    ))
    .unwrap();
    url
}

#[compio::test]
async fn assertion_failure_releases_the_cache_and_the_owned_server() {
    use super::super::checkout;
    use std::cell::RefCell;

    let failed_id = RefCell::new(String::new());
    let failure = AssertUnwindSafe(Database::run(async |database| {
        *failed_id.borrow_mut() = database.postgres.id().to_owned();
        let pool = checkout(&database.config(1)).await.unwrap();
        let _lease = pool.acquire().await.unwrap();
        panic!("intentional pool fixture failure");
    }))
    .catch_unwind()
    .await
    .expect_err("propagate case failure");
    assert_eq!(
        failure.downcast_ref::<&str>(),
        Some(&"intentional pool fixture failure")
    );
    assert!(POOL.with(|pool| pool.borrow().is_none()));
    let containers = std::process::Command::new("docker")
        .args(["ps", "--all", "--quiet", "--no-trunc"])
        .output()
        .expect("list fixture containers");
    assert!(containers.status.success());
    assert!(
        !String::from_utf8(containers.stdout)
            .unwrap()
            .lines()
            .any(|id| id == *failed_id.borrow()),
        "failed case leaked its server"
    );
    Database::run(async |database| {
        let pool = checkout(&database.config(1)).await.unwrap();
        assert_eq!(
            pool.query("SELECT current_database()", &[]).await.unwrap()[0].get::<_, String>(0),
            "gateway_tests"
        );
    })
    .await;
}

#[compio::test]
async fn migrated_cases_restore_service_authority_without_sharing_mutations() {
    use super::super::checkout;

    Database::migrated(async |database| {
        database
            .admin
            .batch_execute("CREATE TABLE fixture_mutation (id integer)")
            .await
            .unwrap();
    })
    .await;
    Database::migrated(async |database| {
        let absent: bool = database
            .admin
            .query_one("SELECT to_regclass('fixture_mutation') IS NULL", &[])
            .await
            .unwrap()
            .get(0);
        assert!(absent, "a previous case mutated the migration seed");
        let pool = checkout(&database.config_as("zeroship_gateway", 1))
            .await
            .unwrap();
        let connection = pool.acquire().await.unwrap();
        assert_eq!(
            connection
                .query_one("SELECT current_user::text", &[])
                .await
                .unwrap()
                .get::<_, String>(0),
            "zeroship_gateway"
        );
        connection
            .query("SELECT client_id FROM zeroship.token_revocations", &[])
            .await
            .expect("restored gateway role can read revocations");
    })
    .await;
}
