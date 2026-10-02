//! Own one migrated `PostgreSQL` server for the whole test binary, and hand each
//! case its own pair of connections to it.
//!
//! The server lives in a `static`, started and migrated the first time a case asks
//! for it. It is started through the shared [`container_reaper`], so it is removed
//! once this process has ended, however the process ends.

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
use testcontainers::{Container, GenericImage, ImageExt};

mod migrations;

#[path = "../../../../../tests/fixtures/container_reaper.rs"]
mod container_reaper;

use container_reaper::{start_owned, DockerCli, OwnedContainer, Ownership};

pub struct Database {
    pub admin: Client,
    pub service: Client,
    admin_driver: compio::runtime::JoinHandle<Result<(), compio_postgres::Error>>,
    service_driver: compio::runtime::JoinHandle<Result<(), compio_postgres::Error>>,
}

/// The server this test binary owns, started the first time anything asks for it.
fn server() -> &'static OwnedContainer {
    static SERVER: OnceLock<OwnedContainer> = OnceLock::new();
    SERVER.get_or_init(|| {
        start_owned(&DockerCli::system(), &Ownership::mint(), image()).unwrap_or_else(|error| {
            panic!("authorization database tests require Docker and PostgreSQL: {error}")
        })
    })
}

/// The server's URL once the platform migrations and the shared reference rows
/// are in. Every case connects here; none of them owns the server.
fn migrated() -> &'static url::Url {
    static MIGRATED: OnceLock<url::Url> = OnceLock::new();
    MIGRATED.get_or_init(|| {
        let container = server().container();
        let url = database_url(container);
        migrations::apply(url.as_str());
        migrations::seed_plan(container);
        url
    })
}

impl Database {
    pub async fn run(test: impl AsyncFnOnce(&Self)) {
        let mut url = migrated().clone();
        let (admin, admin_driver) = connect(&url).await;
        url.set_username("zeroship_control").unwrap();
        url.set_password(Some("zeroship_control")).unwrap();
        let (service, service_driver) = connect(&url).await;
        let database = Self {
            admin,
            service,
            admin_driver,
            service_driver,
        };
        let outcome = AssertUnwindSafe(async {
            compio::time::timeout(Duration::from_secs(45), Box::pin(async {
                test(&database).await;
            }))
            .await
            .expect("authorization database case timed out");
        })
        .catch_unwind()
        .await;

        let Self {
            admin,
            service,
            admin_driver,
            service_driver,
        } = database;
        drop(service);
        let service_closed = compio::time::timeout(Duration::from_secs(15), service_driver).await;
        drop(admin);
        let admin_closed = compio::time::timeout(Duration::from_secs(15), admin_driver).await;
        service_closed
            .expect("service connection timed out")
            .expect("service task")
            .expect("service connection");
        admin_closed
            .expect("observer connection timed out")
            .expect("observer task")
            .expect("observer connection");
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

/// The child test the lifetime measurements drive, by its full path in this binary.
const CHILD_TEST: &str = "tests::database::the_shared_database_reports_its_container";

#[test]
fn the_shared_database_reports_its_container() {
    container_reaper::lifetime::report_owner();
    let id = server().container().id().to_owned();
    container_reaper::lifetime::report_container(&id);
}

#[test]
fn the_shared_database_is_removed_when_its_process_ends() {
    container_reaper::lifetime::assert_removed_after_the_child_exits(CHILD_TEST);
}

#[test]
fn the_shared_database_is_removed_when_its_process_is_killed_while_starting() {
    container_reaper::lifetime::assert_removed_after_a_kill_during_startup(CHILD_TEST);
}

#[compio::test]
async fn a_failed_case_re_raises_its_failure_and_closes_its_connections() {
    use std::cell::RefCell;
    let pids = RefCell::new(Vec::new());
    let failure = AssertUnwindSafe(Database::run(async |database| {
        let admin: i32 = database
            .admin
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        let service: i32 = database
            .service
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        *pids.borrow_mut() = vec![admin, service];
        panic!("intentional authorization fixture failure");
    }))
    .catch_unwind()
    .await
    .expect_err("propagate the case failure");
    assert_eq!(
        failure.downcast_ref::<&str>(),
        Some(&"intentional authorization fixture failure")
    );
    let failed_pids = pids.borrow().clone();
    Database::run(async |database| {
        let live: i64 = database
            .admin
            .query_one(
                "SELECT count(*) FROM pg_stat_activity WHERE pid = ANY($1)",
                &[&failed_pids],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            live, 0,
            "a failed case must close the connections it opened"
        );
    })
    .await;
}
