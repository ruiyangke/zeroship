//! Isolated platform databases for auth behavior tests.
//!
//! The seed is generated from the actual migration corpus in this process.
//! Only its immutable dump bytes are shared; every case owns its server.

use compio_postgres::{Client, NoTls};
use futures::FutureExt;
use std::cell::RefCell;
use std::io::{Read, Seek, SeekFrom, Write};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use testcontainers::core::wait::LogWaitStrategy;
use testcontainers::core::{CmdWaitFor, ExecCommand, IntoContainerPort, WaitFor};
use testcontainers::{runners::SyncRunner, Container, GenericImage, ImageExt};

type Driver = compio::runtime::JoinHandle<Result<(), compio_postgres::Error>>;

pub struct Database {
    postgres: Container<GenericImage>,
    url: url::Url,
    drivers: RefCell<Vec<Driver>>,
}

impl Database {
    pub async fn run(test: impl AsyncFnOnce(&Self)) {
        static SEED: OnceLock<Seed> = OnceLock::new();
        let seed = SEED.get_or_init(Seed::build);
        // Restore using a bootstrap role absent from the seed. pg_dumpall
        // then creates the original roles, including postgres, without edits.
        let postgres = image()
            .with_env_var("POSTGRES_USER", "fixture_loader")
            .with_env_var("POSTGRES_DB", "postgres")
            .with_copy_to("/docker-entrypoint-initdb.d/roles.sql", seed.roles.clone())
            .with_copy_to(
                "/docker-entrypoint-initdb.d/schema.sql",
                seed.database.clone(),
            )
            .start()
            .expect("auth tests require Docker and PostgreSQL");
        let database = Self {
            url: database_url(&postgres),
            postgres,
            drivers: RefCell::default(),
        };
        let outcome = AssertUnwindSafe(async {
            compio::time::timeout(Duration::from_secs(90), test(&database))
                .await
                .expect("auth database case timed out");
        })
        .catch_unwind()
        .await;

        // The case's clients and HTTP services drop before we wait on their
        // exact connection tasks, including when an assertion unwinds.
        let drivers = database.drivers.take();
        let closed = compio::time::timeout(Duration::from_secs(15), futures::future::join_all(drivers)).await;
        let removed = database.postgres.rm();
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
        for driver in closed.expect("fixture connections must close before their runtime") {
            driver.expect("fixture driver task").expect("fixture PostgreSQL connection");
        }
        removed.expect("remove the fixture's PostgreSQL container");
    }

    pub fn url(&self) -> &str {
        self.url.as_str()
    }

    pub async fn connect(&self) -> Client {
        self.connect_to(self.url()).await
    }

    pub async fn connect_as_auth(&self) -> Client {
        let mut url = self.url.clone();
        url.set_username("zeroship_auth").unwrap();
        url.set_password(Some("zeroship_auth")).unwrap();
        self.connect_to(url.as_str()).await
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

struct Seed {
    roles: Vec<u8>,
    database: Vec<u8>,
}

impl Seed {
    fn build() -> Self {
        let postgres = image()
            .with_env_var("POSTGRES_DB", "auth_tests")
            .start()
            .expect("prepare the migrated auth database seed");
        apply_migrations(database_url(&postgres).as_str());
        let roles = dump(
            &postgres,
            &["pg_dumpall", "-U", "postgres", "--globals-only"],
        );
        let database = dump(
            &postgres,
            &[
                "pg_dump",
                "-U",
                "postgres",
                "--create",
                "--dbname",
                "auth_tests",
            ],
        );
        postgres.rm().expect("remove migration seed container");
        Self { roles, database }
    }
}

fn image() -> testcontainers::ContainerRequest<GenericImage> {
    GenericImage::new("postgres", "17")
        .with_exposed_port(5432.tcp())
        // The entrypoint starts a temporary server for initialization, then
        // starts the server that accepts client connections. Wait for both.
        .with_wait_for(WaitFor::log(
            LogWaitStrategy::stderr("database system is ready to accept connections").with_times(2),
        ))
        .with_env_var("POSTGRES_PASSWORD", "fixture")
        .with_startup_timeout(Duration::from_secs(120))
}

fn database_url(postgres: &Container<GenericImage>) -> url::Url {
    let mut url = url::Url::parse("postgresql://postgres:fixture@localhost/auth_tests").unwrap();
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

fn dump(postgres: &Container<GenericImage>, arguments: &[&str]) -> Vec<u8> {
    let mut result = postgres
        .exec(
            ExecCommand::new(arguments.iter().copied())
                .with_cmd_ready_condition(CmdWaitFor::exit_code(0)),
        )
        .expect("dump the migrated platform database");
    result.stdout_to_vec().expect("read database seed")
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("auth crate lives under crates/")
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
            .expect("auth database fixtures require Node; run through nix develop"),
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

#[ntex::test]
async fn a_failed_case_releases_its_server_and_cannot_change_the_next_database() {
    let failed_id = RefCell::new(String::new());
    let failed = AssertUnwindSafe(Database::run(async |database| {
        *failed_id.borrow_mut() = database.postgres.id().to_owned();
        assert!(container_ids().contains(&*failed_id.borrow()));
        let client = database.connect().await;
        client.batch_execute("CREATE TABLE fixture_isolation (id integer)").await.unwrap();
        panic!("intentional fixture failure");
    })).catch_unwind().await.expect_err("case must propagate its assertion failure");
    assert_eq!(failed.downcast_ref::<&str>(), Some(&"intentional fixture failure"));
    assert!(!container_ids().contains(&*failed_id.borrow()), "failed case leaked its PostgreSQL server");

    let successful_id = RefCell::new(String::new());
    Database::run(async |database| {
        *successful_id.borrow_mut() = database.postgres.id().to_owned();
        let client = database.connect().await;
        let absent: bool = client.query_one("SELECT to_regclass('fixture_isolation') IS NULL", &[]).await.unwrap().get(0);
        assert!(absent, "a failed case changed the immutable seed");
        let auth = database.connect_as_auth().await;
        let role: String = auth.query_one("SELECT current_user::text", &[]).await.unwrap().get(0);
        assert_eq!(role, "zeroship_auth");
        auth.query("SELECT id FROM zeroship.users", &[]).await.expect("restored role can read the migrated auth schema");
    }).await;
    assert!(!container_ids().contains(&*successful_id.borrow()), "successful case leaked its PostgreSQL server");
}

fn container_ids() -> Vec<String> {
    let output = Command::new("docker").args(["ps", "--all", "--quiet", "--no-trunc"])
        .output().expect("query fixture container lifecycle");
    assert!(output.status.success(), "Docker container listing failed: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap().lines().map(str::to_owned).collect()
}
