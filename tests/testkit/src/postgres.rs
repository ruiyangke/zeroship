//! One reaper-owned `PostgreSQL` server per test process, migrated once.
//!
//! [`platform()`] hands back the process's shared server after the canonical
//! platform migration has run, so every case in a binary connects to the same
//! migrated database and owns only the rows and names it mints. [`server()`] is
//! the same server WITHOUT the migration, which is what a lifetime child needs:
//! it starts the container, reports its id and can be killed while the container
//! is still coming up, without paying for a migration that would change what the
//! kill measures.
//!
//! The server lives in a `static`, which libtest never drops, so it is started
//! through the shared reaper ([`crate::docker`]): a reaper spawned before the
//! container exists removes it once this process has ended, however the process
//! ends.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use compio_postgres::{Client, NoTls};
use testcontainers::core::{CmdWaitFor, ExecCommand, IntoContainerPort};
use testcontainers::{Container, GenericImage, ImageExt};

use crate::docker::{start_owned, DockerCli, OwnedContainer, Ownership};

mod case;
mod image;
pub mod server;

pub use case::{run, run_fresh, Case, CaseFixture};
pub use image::build;

/// The database the shared platform migration is applied to.
const DATABASE: &str = "zeroship_testkit";

/// The pristine migrated database a fresh-database case is cloned from.
///
/// It is created once from the migrated working database before any case
/// connects, and no session ever connects to it afterwards. PostgreSQL refuses
/// `CREATE DATABASE ... TEMPLATE` while any session is attached, so keeping it
/// connection-free is what lets [`Platform::fresh_database`] clone it.
const TEMPLATE_DATABASE: &str = "zeroship_testkit_template";

/// The `PostgreSQL` server this process owns, without the platform migration.
pub struct Server {
    owned: OwnedContainer,
    base: url::Url,
}

impl Server {
    fn start() -> Self {
        let image = image::build().unwrap_or_else(|error| {
            panic!("platform database tests require Docker and PostgreSQL: {error}")
        });
        let request =
            image::await_ready(image.with_exposed_port(5432.tcp()))
                .with_env_var("POSTGRES_PASSWORD", "fixture")
                .with_env_var("POSTGRES_DB", DATABASE);
        let owned = start_owned(&DockerCli::system(), &Ownership::mint(), request)
            .unwrap_or_else(|error| panic!("platform database tests require Docker and PostgreSQL: {error}"));
        let container = owned.container();
        let host = container.get_host().expect("database host");
        let port = container.get_host_port_ipv4(5432).expect("database port");
        let mut base = url::Url::parse(&format!(
            "postgresql://postgres:fixture@localhost/{DATABASE}"
        ))
        .expect("fixture database URL");
        base.set_host(Some(&host.to_string())).unwrap();
        base.set_port(Some(port)).unwrap();
        Self { owned, base }
    }

    /// The Docker id of this server's container.
    #[must_use]
    pub fn container_id(&self) -> &str {
        self.owned.container().id()
    }

    /// The server's URL as its superuser, `postgres`.
    #[must_use]
    pub fn admin_url(&self) -> url::Url {
        self.base.clone()
    }

    /// The server's URL as `role`, which is also `role`'s password.
    #[must_use]
    pub fn url(&self, role: &str) -> url::Url {
        let mut url = self.base.clone();
        url.set_username(role).unwrap();
        url.set_password(Some(role)).unwrap();
        url
    }
}

/// The server this test binary owns, started the first time anything asks for it.
pub fn server() -> &'static Server {
    static SERVER: OnceLock<Server> = OnceLock::new();
    SERVER.get_or_init(Server::start)
}

/// The id of this binary's server, starting it WITHOUT migrating it.
///
/// This is what a lifetime child reports and what the kill-during-startup
/// measurement targets, so it must not run the platform migration first.
#[must_use]
pub fn server_container_id() -> String {
    server().container_id().to_owned()
}

/// The server's URL once the platform migration and the shared reference rows
/// are in. Applied once per test process.
fn migrated() -> &'static url::Url {
    static MIGRATED: OnceLock<url::Url> = OnceLock::new();
    MIGRATED.get_or_init(|| {
        let server = server();
        let url = server.admin_url();
        apply_migrations(url.as_str());
        seed_plan(server.owned.container());
        psql(
            server.owned.container(),
            "postgres",
            &format!(
                "CREATE DATABASE \"{TEMPLATE_DATABASE}\" TEMPLATE \"{DATABASE}\""
            ),
        );
        url
    })
}

/// The migrated platform database every case in this process shares.
pub struct Platform {
    base: url::Url,
}

impl Platform {
    /// The Docker id of the shared server.
    #[must_use]
    pub fn container_id(&self) -> &str {
        server().container_id()
    }

    /// The shared server's URL as its superuser, `postgres`.
    #[must_use]
    pub fn admin_url(&self) -> url::Url {
        self.base.clone()
    }

    /// The shared server's URL as `role`, which is also `role`'s password.
    #[must_use]
    pub fn url(&self, role: &str) -> url::Url {
        role_url(&self.base, role)
    }

    /// Connect a client as `role`, with the connection driver task to join when
    /// the case's runtime ends.
    ///
    /// # Panics
    /// When the server refuses the connection.
    pub async fn connect(
        &self,
        role: &str,
    ) -> (
        Client,
        compio::runtime::JoinHandle<Result<(), compio_postgres::Error>>,
    ) {
        // Boxed so the connection state machine stays out of the caller's
        // future; inlining it pushes authz's deeply nested test futures past
        // rustc's layout-query depth.
        Box::pin(connect(&self.url(role))).await
    }

    /// Connect a client as the server's superuser.
    ///
    /// # Panics
    /// When the server refuses the connection.
    pub async fn admin_connect(
        &self,
    ) -> (
        Client,
        compio::runtime::JoinHandle<Result<(), compio_postgres::Error>>,
    ) {
        Box::pin(connect(&self.admin_url())).await
    }

    /// A database cloned from the pristine migrated template, for a case whose
    /// subject is platform-global rather than scoped to the rows it mints.
    ///
    /// The clone starts from the same schema and reference rows as [`platform`],
    /// carries no other case's rows, and is removed when the returned handle
    /// drops. PostgreSQL runs the clone from the connection-free template, so
    /// no case ever waits on or perturbs another's database.
    #[must_use]
    pub fn fresh_database(&self) -> FreshDatabase {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let name = format!("zeroship_case_{}_{serial}", std::process::id());
        psql(
            server().owned.container(),
            "postgres",
            &format!("CREATE DATABASE \"{name}\" TEMPLATE \"{TEMPLATE_DATABASE}\""),
        );
        FreshDatabase {
            base: database_url(&self.base, &name),
            name,
        }
    }
}

/// A clone of the pristine migrated template, owned by one case.
///
/// Dropping it removes the database; a case's connections must already be
/// closed, which is what `WITH (FORCE)` guarantees if one was not.
pub struct FreshDatabase {
    base: url::Url,
    name: String,
}

impl FreshDatabase {
    /// The clone's URL as its superuser, `postgres`.
    #[must_use]
    pub fn admin_url(&self) -> url::Url {
        self.base.clone()
    }
}

impl Drop for FreshDatabase {
    fn drop(&mut self) {
        psql(
            server().owned.container(),
            "postgres",
            &format!("DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)", self.name),
        );
    }
}

/// The migrated platform database this test binary shares.
pub fn platform() -> &'static Platform {
    static PLATFORM: OnceLock<Platform> = OnceLock::new();
    PLATFORM.get_or_init(|| Platform {
        base: migrated().clone(),
    })
}

fn role_url(base: &url::Url, role: &str) -> url::Url {
    let mut url = base.clone();
    url.set_username(role).unwrap();
    url.set_password(Some(role)).unwrap();
    url
}

/// The URL of another database on the same server, with `base`'s credentials.
fn database_url(base: &url::Url, name: &str) -> url::Url {
    let mut url = base.clone();
    url.set_path(name);
    url
}

/// Open one connection and spawn its driver task.
async fn connect(
    url: &url::Url,
) -> (
    Client,
    compio::runtime::JoinHandle<Result<(), compio_postgres::Error>>,
) {
    let mut config: compio_postgres::Config = url.as_str().parse().expect("fixture database URL");
    config.connect_timeout(Duration::from_secs(15));
    let (client, connection) = config
        .connect(NoTls)
        .await
        .expect("connect fixture database");
    (
        client,
        compio::runtime::spawn(async move { connection.run().await }),
    )
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("testkit lives under tests/")
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
            .expect("platform database fixtures require Node; run through nix develop"),
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

/// Seed the plan row the platform schema requires an app to reference.
fn seed_plan(container: &Container<GenericImage>) {
    psql(
        container,
        DATABASE,
        "INSERT INTO zeroship.plans (id, name, runtime_limits_json) \
         VALUES ('free', 'Free', '{}') ON CONFLICT (id) DO NOTHING",
    );
}

/// Run one statement as the server's superuser through the `psql` in the container.
///
/// Used for statements PostgreSQL forbids inside a transaction or over a
/// pooled connection, such as `CREATE DATABASE ... TEMPLATE` and `DROP DATABASE`.
fn psql(container: &Container<GenericImage>, database: &str, sql: &str) {
    let arguments = [
        "psql",
        "-U",
        "postgres",
        "-d",
        database,
        "-v",
        "ON_ERROR_STOP=1",
        "-c",
        sql,
    ];
    container
        .exec(
            ExecCommand::new(arguments.iter().copied())
                .with_cmd_ready_condition(CmdWaitFor::exit_code(0)),
        )
        .expect("run a fixture statement as the database superuser");
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
