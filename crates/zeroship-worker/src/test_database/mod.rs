//! PostgreSQL fixtures own their server, catalog and connection drivers.

#![allow(
    clippy::future_not_send,
    reason = "fixtures stay on their compio runtime"
)]

use compio_postgres::{Client, NoTls};
use futures::FutureExt;
use std::cell::RefCell;
use std::panic::AssertUnwindSafe;
use std::sync::OnceLock;
use std::time::Duration;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::{runners::SyncRunner, Container, GenericImage, ImageExt};

mod migrations;

type Driver = compio::runtime::JoinHandle<Result<(), compio_postgres::Error>>;

pub(crate) struct Database {
    postgres: Container<GenericImage>,
    url: url::Url,
    pub(crate) admin: Client,
    driver: Driver,
    role_drivers: RefCell<Vec<Driver>>,
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
        let postgres = image
            .start()
            .expect("worker tests require Docker and PostgreSQL");
        let url = database_url(&postgres);
        let (admin, driver) = connect(&url).await;
        let database = Self {
            postgres,
            url,
            admin,
            driver,
            role_drivers: RefCell::new(Vec::new()),
        };
        let outcome = AssertUnwindSafe(async {
            compio::time::timeout(Duration::from_secs(90), Box::pin(test(&database)))
                .await
                .expect("worker database case timed out");
        })
        .catch_unwind()
        .await;

        let drained = AssertUnwindSafe(async {
            compio::time::timeout(Duration::from_secs(15), async {
                for driver in database.role_drivers.take() {
                    driver.await.expect("role driver task").expect("role connection");
                }
                loop {
                    let empty: bool = database.admin.query_one(
                        "SELECT NOT EXISTS (SELECT FROM pg_stat_activity WHERE backend_type = 'client backend' AND pid <> pg_backend_pid())",
                        &[],
                    ).await.expect("observe connection cleanup").get(0);
                    if empty { break; }
                    compio::time::sleep(Duration::from_millis(25)).await;
                }
            }).await.expect("connections must close before removing the owned server");
        }).catch_unwind().await;
        let Self {
            postgres,
            admin,
            driver,
            ..
        } = database;
        drop(admin);
        let closed = compio::time::timeout(Duration::from_secs(15), driver).await;
        let removed = postgres.rm();
        if let Err(panic) = outcome {
            if drained.is_err() || !matches!(closed, Ok(Ok(Ok(())))) || removed.is_err() {
                eprintln!("worker fixture cleanup also failed; preserving the case failure");
            }
            std::panic::resume_unwind(panic);
        }
        if let Err(panic) = drained {
            std::panic::resume_unwind(panic);
        }
        closed
            .expect("observer connection timed out")
            .expect("observer task")
            .expect("observer connection");
        removed.expect("remove the owned PostgreSQL server");
    }

    pub(crate) fn url_as(&self, role: &str) -> url::Url {
        let mut url = self.url.clone();
        url.set_username(role).unwrap();
        url.set_password(Some(role)).unwrap();
        url
    }

    pub(crate) async fn connect_as(&self, role: &str) -> Client {
        let (client, driver) = connect(&self.url_as(role)).await;
        self.role_drivers.borrow_mut().push(driver);
        client
    }
}

async fn connect(url: &url::Url) -> (Client, Driver) {
    let mut config: compio_postgres::Config = url.as_str().parse().unwrap();
    config.connect_timeout(Duration::from_secs(10));
    let (client, connection) = config.connect(NoTls).await.expect("connect worker fixture");
    (
        client,
        compio::runtime::spawn(async move { connection.run().await }),
    )
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
        .with_env_var("POSTGRES_DB", "worker_tests")
        .with_startup_timeout(Duration::from_secs(120))
}

fn database_url(postgres: &Container<GenericImage>) -> url::Url {
    let mut url = url::Url::parse("postgresql://postgres:fixture@localhost/worker_tests").unwrap();
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
async fn failed_cases_remove_their_server_without_touching_another_cases_roles() {
    Database::run(async |other| {
        other
            .admin
            .batch_execute("CREATE ROLE fixture_role LOGIN PASSWORD 'fixture_role'")
            .await
            .unwrap();
        let failed_id = RefCell::new(String::new());
        let failure = AssertUnwindSafe(Database::run(async |database| {
            *failed_id.borrow_mut() = database.postgres.id().to_owned();
            database
                .admin
                .batch_execute("CREATE ROLE fixture_role LOGIN PASSWORD 'fixture_role'")
                .await
                .unwrap();
            let client = database.connect_as("fixture_role").await;
            client.query_one("SELECT current_user", &[]).await.unwrap();
            panic!("intentional worker fixture failure");
        }))
        .catch_unwind()
        .await
        .expect_err("propagate case failure");
        assert_eq!(
            failure.downcast_ref::<&str>(),
            Some(&"intentional worker fixture failure")
        );
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
        let client = other.connect_as("fixture_role").await;
        assert_eq!(
            client
                .query_one("SELECT current_user", &[])
                .await
                .unwrap()
                .get::<_, String>(0),
            "fixture_role"
        );
    })
    .await;
}
