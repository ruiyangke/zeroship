//! Own a migrated `PostgreSQL` server and join its clients before removing it.

#![allow(
    clippy::future_not_send,
    reason = "fixtures stay on their compio runtime"
)]

use compio_postgres::{Client, NoTls};
use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use std::sync::OnceLock;
use std::time::Duration;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::{runners::SyncRunner, Container, GenericImage, ImageExt};

mod migrations;

pub struct Database {
    postgres: Container<GenericImage>,
    pub admin: Client,
    pub service: Client,
    admin_driver: compio::runtime::JoinHandle<Result<(), compio_postgres::Error>>,
    service_driver: compio::runtime::JoinHandle<Result<(), compio_postgres::Error>>,
}

impl Database {
    pub async fn run(test: impl AsyncFnOnce(&Self)) {
        static SEED: OnceLock<migrations::Seed> = OnceLock::new();
        let seed = SEED.get_or_init(migrations::Seed::build);
        let postgres = image()
            .with_env_var("POSTGRES_DB", "postgres")
            .with_copy_to("/docker-entrypoint-initdb.d/roles.sql", seed.roles.clone())
            .with_copy_to(
                "/docker-entrypoint-initdb.d/schema.sql",
                seed.database.clone(),
            )
            .start()
            .expect("authorization database tests require Docker and PostgreSQL");
        let mut url = database_url(&postgres);
        let (admin, admin_driver) = connect(&url).await;
        url.set_username("zeroship_control").unwrap();
        url.set_password(Some("zeroship_control")).unwrap();
        let (service, service_driver) = connect(&url).await;
        let database = Self {
            postgres,
            admin,
            service,
            admin_driver,
            service_driver,
        };
        let outcome = AssertUnwindSafe(async {
            compio::time::timeout(Duration::from_secs(45), Box::pin(async {
                database.admin.execute(
                    "INSERT INTO zeroship.plans (id, name, runtime_limits_json) VALUES ('free', 'Free', '{}')",
                    &[],
                ).await.expect("seed the required plan");
                test(&database).await;
            })).await.expect("authorization database case timed out");
        }).catch_unwind().await;

        let Self {
            postgres,
            admin,
            service,
            admin_driver,
            service_driver,
        } = database;
        drop(service);
        let service_closed = compio::time::timeout(Duration::from_secs(15), service_driver).await;
        drop(admin);
        let admin_closed = compio::time::timeout(Duration::from_secs(15), admin_driver).await;
        let removed = postgres.rm();
        service_closed
            .expect("service connection timed out")
            .expect("service task")
            .expect("service connection");
        admin_closed
            .expect("observer connection timed out")
            .expect("observer task")
            .expect("observer connection");
        removed.expect("remove the owned authorization database");
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }
}

async fn connect(
    url: &url::Url,
) -> (
    Client,
    compio::runtime::JoinHandle<Result<(), compio_postgres::Error>>,
) {
    let mut config: compio_postgres::Config = url.as_str().parse().unwrap();
    config.connect_timeout(Duration::from_secs(10));
    let (client, connection) = config
        .connect(NoTls)
        .await
        .expect("connect authorization fixture");
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
        .with_env_var("POSTGRES_DB", "authz_tests")
        .with_startup_timeout(Duration::from_secs(120))
}

fn database_url(postgres: &Container<GenericImage>) -> url::Url {
    let mut url = url::Url::parse("postgresql://postgres:fixture@localhost/authz_tests").unwrap();
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
async fn a_failed_case_removes_its_database_and_preserves_the_failure() {
    use std::cell::RefCell;
    let failed_id = RefCell::new(String::new());
    let failure = AssertUnwindSafe(Database::run(async |database| {
        *failed_id.borrow_mut() = database.postgres.id().to_owned();
        database
            .service
            .query_one("SELECT current_user", &[])
            .await
            .unwrap();
        panic!("intentional authorization fixture failure");
    }))
    .catch_unwind()
    .await
    .expect_err("propagate the case failure");
    assert_eq!(
        failure.downcast_ref::<&str>(),
        Some(&"intentional authorization fixture failure")
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
    Database::run(async |database| {
        let row = database
            .service
            .query_one(
                "SELECT current_user, (SELECT count(*) FROM zeroship.users)",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<_, String>(0), "zeroship_control");
        assert_eq!(row.get::<_, i64>(1), 0, "a new case starts without users");
    })
    .await;
}
