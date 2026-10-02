//! One migrated platform database shared by every authn case in this process.
//!
//! The server is started and migrated the first time a case asks for it, and lives
//! in a `static`, which libtest never drops. It is started through the shared
//! [`container_reaper`]: a reaper spawned before the container exists removes it
//! once this process has ended, however the process ends.
//!
//! Cases share the server, so every case owns the rows and names it creates -
//! its ids, rate-limit keys, replay keys and per-case roles - and asserts only on
//! those. [`Database::run`] opens the case's connections against the shared
//! server and joins their driver tasks before the case's runtime ends.

#![allow(
    clippy::future_not_send,
    reason = "fixtures belong to their compio runtime"
)]

use compio_postgres::{Client, NoTls};
use futures::FutureExt;
use std::cell::RefCell;
use std::io::{Read, Seek, SeekFrom, Write};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::{Container, GenericImage, ImageExt};

/// Containers this test binary owns, removed when its process ends.
#[path = "../../../../tests/fixtures/container_reaper.rs"]
pub mod container_reaper;

use container_reaper::{start_owned, DockerCli, OwnedContainer, Ownership};

type Driver = compio::runtime::JoinHandle<Result<(), compio_postgres::Error>>;

pub struct Database {
    url: url::Url,
    drivers: RefCell<Vec<Driver>>,
}

impl Database {
    /// Run `test` against the shared migrated database.
    ///
    /// Connections the case opened are joined before its runtime ends, including
    /// when an assertion unwinds, and the case's own panic is re-raised after
    /// that.
    pub async fn run(test: impl AsyncFnOnce(&Self)) {
        let database = Self {
            url: shared_url().clone(),
            drivers: RefCell::default(),
        };
        let outcome = AssertUnwindSafe(async {
            compio::time::timeout(Duration::from_secs(90), Box::pin(test(&database)))
                .await
                .expect("authn database case timed out");
        })
        .catch_unwind()
        .await;

        // The case's clients and HTTP services drop before we wait on their
        // exact connection tasks, including when an assertion unwinds.
        let drivers = database.drivers.take();
        let closed = compio::time::timeout(
            Duration::from_secs(15),
            futures::future::join_all(drivers),
        )
        .await;
        for driver in closed.expect("fixture connections must close before their runtime") {
            driver
                .expect("fixture driver task")
                .expect("fixture PostgreSQL connection");
        }
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    pub fn url(&self) -> &str {
        self.url.as_str()
    }

    pub async fn connect(&self) -> Client {
        self.connect_to(self.url()).await
    }

    pub async fn connect_as(&self, role: &str) -> Client {
        let mut url = self.url.clone();
        url.set_username(role).unwrap();
        url.set_password(Some(role)).unwrap();
        self.connect_to(url.as_str()).await
    }

    /// Observe all requested backends waiting on locks before releasing a fixture transaction.
    #[allow(
        clippy::future_not_send,
        reason = "the database belongs to this compio runtime"
    )]
    pub async fn wait_until_blocked(&self, pids: &[i32]) -> bool {
        assert!(
            !pids.is_empty(),
            "a lock observation needs waiting backends"
        );
        let observer = self.connect().await;
        compio::time::timeout(Duration::from_secs(10), async {
            loop {
                let blocked: bool = observer
                    .query_one(
                        "SELECT bool_and(cardinality(pg_blocking_pids(pid)) > 0) \
                         FROM unnest($1::int[]) AS requested(pid)",
                        &[&pids],
                    )
                    .await
                    .expect("observe fixture lock waiters")
                    .get(0);
                if blocked {
                    return;
                }
                compio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .is_ok()
    }

    async fn connect_to(&self, url: &str) -> Client {
        let mut config: compio_postgres::Config = url.parse().expect("fixture database URL");
        config.connect_timeout(Duration::from_secs(15));
        let (client, connection) = config
            .connect(NoTls)
            .await
            .expect("connect fixture database");
        self.drivers.borrow_mut().push(compio::runtime::spawn(
            async move { connection.run().await },
        ));
        client
    }
}

/// The server this binary owns, started the first time anything asks for it.
fn server() -> &'static OwnedContainer {
    static SERVER: OnceLock<OwnedContainer> = OnceLock::new();
    SERVER.get_or_init(|| {
        start_owned(
            &DockerCli::system(),
            &Ownership::mint(),
            image().with_env_var("POSTGRES_DB", "authn_tests"),
        )
        .unwrap_or_else(|error| panic!("authn tests require Docker and PostgreSQL: {error}"))
    })
}

/// The server's URL once the platform migrations are in. Every case connects
/// here; none of them owns the server.
fn shared_url() -> &'static url::Url {
    static MIGRATED: OnceLock<url::Url> = OnceLock::new();
    MIGRATED.get_or_init(|| {
        let url = database_url(server().container());
        apply_migrations(url.as_str());
        url
    })
}

/// The Docker id of this binary's server, starting it (without migrating it)
/// if nothing has yet.
pub fn container_id() -> String {
    server().container().id().to_owned()
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
        .with_startup_timeout(Duration::from_secs(120))
}

fn database_url(postgres: &Container<GenericImage>) -> url::Url {
    let mut url = url::Url::parse("postgresql://postgres:fixture@localhost/authn_tests").unwrap();
    url.set_host(Some(
        &postgres.get_host().expect("container host").to_string(),
    ))
    .unwrap();
    url.set_port(Some(
        postgres.get_host_port_ipv4(5432).expect("mapped port"),
    ))
    .unwrap();
    url
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("authn crate lives under crates/")
        .to_owned()
}

fn apply_migrations(url: &str) {
    let root = root();
    let cli = root.join("packages/zero-migrate-cli/dist/cli-bin.js");
    assert!(
        cli.is_file(),
        "build the migration host: cargo xtask test migrations"
    );
    let corpus = root.join("db/migrations-ts");
    let registry = root.join("policies/platform-table-owners.json");
    let policy = root.join("policies/platform.policy.toml");
    let env = toml::toml! {
        [env.platform]
        url = (url)
        dir = (corpus.to_str().unwrap())
        schema = "zeroship"
        owner_app = "zeroship_platform"
        registry = (registry.to_str().unwrap())
        policy = [(policy.to_str().unwrap())]
    };
    let mut config = tempfile::NamedTempFile::new().expect("private migration config");
    config
        .write_all(toml::to_string(&env).unwrap().as_bytes())
        .unwrap();
    let mut stdout = tempfile::tempfile().expect("migration stdout");
    let mut stderr = tempfile::tempfile().expect("migration stderr");
    let mut command = Command::new("node");
    remove_deployment_overrides(&mut command);
    let mut child = OwnedChild(
        command
            .current_dir(root)
            .arg(cli)
            .args(["apply", "--config"])
            .arg(config.path())
            .args(["--env", "platform", "--approve"])
            .stdin(Stdio::null())
            .stdout(stdout.try_clone().unwrap())
            .stderr(stderr.try_clone().unwrap())
            .spawn()
            .expect("authn database fixtures require Node; run through nix develop"),
    );
    let deadline = Instant::now() + Duration::from_secs(300);
    let status = loop {
        if let Some(status) = child.0.try_wait().expect("wait for migrations") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "platform migrations timed out: {}",
            read(&mut stderr)
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        status.success(),
        "platform migrations failed ({status}):\n{}\n{}",
        read(&mut stdout),
        read(&mut stderr)
    );
}

#[allow(
    clippy::disallowed_methods,
    reason = "remove deployment overrides from an owned fixture child"
)]
fn remove_deployment_overrides(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.starts_with("ZERO_MIGRATE_")
            || name.starts_with("PG")
            || matches!(
                name.as_ref(),
                "DATABASE_URL"
                    | "NODE_OPTIONS"
                    | "NAPI_RS_NATIVE_LIBRARY_PATH"
                    | "NAPI_RS_FORCE_WASI"
            )
        {
            command.env_remove(key);
        }
    }
}

fn read(file: &mut std::fs::File) -> String {
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut output = String::new();
    file.read_to_string(&mut output).unwrap();
    output
}

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
