//! The platform database every test process of a worktree shares.
//!
//! [`platform()`] joins the one migrated server a worktree boots through
//! [`crate::shared`]: the first process elects itself, runs the platform
//! migration into a pristine template and clones the working database from it,
//! and every other process joins the ready server. A process holds its lease for
//! as long as it runs; the container's watchdog removes the server once no
//! process has held it for the idle grace.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use compio_postgres::{Client, NoTls};
use zeroship_id::DatabaseId;

use crate::docker::DockerCli;
use crate::fingerprint;
use crate::shared::{self, Scope};

mod case;
mod image;
pub mod server;

pub use case::{run, run_fresh, Case, CaseFixture};
pub use image::{build, reference as image_ref};

/// The database the shared platform migration is applied to.
const DATABASE: &str = "zeroship_testkit";

/// The watchdog baked into the PostgreSQL image.
const WATCHDOG: &str = "/usr/local/bin/zeroship-watchdog";

/// The port PostgreSQL listens on in the image.
const POSTGRES_PORT: u16 = 5432;

/// The largest number of cases a run executes against the shared server at once.
///
/// The test commands run one case per process: `cargo test` up to its thread
/// count and nextest up to one per CPU. Sizing the server to the widest of
/// those keeps the budget from depending on which command a developer ran.
const PARALLEL_CASES: usize = 32;

/// The most connections one case may hold on the shared server.
///
/// A case owns its administrative connection, the pools its in-process service
/// opens, and a pool per HTTP thread for each server process it spawns. The
/// number is the ceiling the fixtures are built to, not a reading of what any
/// case happened to open.
const CONNECTIONS_PER_CASE: usize = 64;

/// The slots PostgreSQL keeps out of the general budget for superusers.
const SUPERUSER_RESERVED: usize = 3;

/// The connection budget the shared server is started with.
///
/// Every case of a run connects to this one server, so the budget is the
/// parallel case count times the per-case ceiling plus the reserved superuser
/// slots. Sizing it to a measured reading would make a run depend on which
/// cases a filter selected; sizing it to that rule does not.
const SHARED_MAX_CONNECTIONS: usize =
    PARALLEL_CASES * CONNECTIONS_PER_CASE + SUPERUSER_RESERVED;

/// The pristine migrated database a fresh-database case is cloned from.
///
/// It is created once by the elected booter, migrated, and then sealed with
/// `ALLOW_CONNECTIONS false`; no session connects to it afterwards. PostgreSQL
/// refuses `CREATE DATABASE ... TEMPLATE` while any session is attached, so
/// keeping it connection-free is what lets [`Platform::fresh_database`] clone it.
const TEMPLATE_DATABASE: &str = "zeroship_template";

/// The seed row the platform schema requires an app to reference.
const SEED_PLAN: &str = "INSERT INTO zeroship.plans (id, name, runtime_limits_json) \
     VALUES ('free', 'Free', '{}') ON CONFLICT (id) DO NOTHING";
/// The migrated platform database every case in a worktree shares.
pub struct Platform {
    base: url::Url,
    lease: shared::Lease,
}

impl Platform {
    /// Join the worktree's shared server, booting it if this process is elected.
    fn join() -> Result<Self, String> {
        let spec = spec()?;
        let scope = Scope::worktree("platform");
        let lease = shared::join(&scope, &spec, boot_platform)?;
        let mut base =
            url::Url::parse(&format!("postgresql://postgres:fixture@127.0.0.1/{DATABASE}"))
                .expect("fixture database URL");
        base.set_port(Some(lease.port)).unwrap();
        Ok(Self { base, lease })
    }

    /// The Docker id of the shared server.
    #[must_use]
    pub fn container_id(&self) -> &str {
        &self.lease.container_id
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
    ///
    /// The name is a minted [`DatabaseId`], so two processes that collide are
    /// impossible and a killed process cannot hand a later run a name whose old
    /// clone is still present. A clone a killed process leaves behind outlives
    /// that process until the shared server's teardown takes the whole server,
    /// which is acceptable for a throwaway worktree server.
    #[must_use]
    pub fn fresh_database(&self) -> FreshDatabase {
        let name = DatabaseId::mint().as_str().to_owned();
        shared::psql(
            &DockerCli::system(),
            &self.lease.container_id,
            "postgres",
            &format!("CREATE DATABASE \"{name}\" TEMPLATE \"{TEMPLATE_DATABASE}\""),
        )
        .expect("clone the migrated template");
        FreshDatabase {
            base: database_url(&self.base, &name),
            name,
            container_id: self.lease.container_id.clone(),
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
    container_id: String,
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
        let _ = shared::psql(
            &DockerCli::system(),
            &self.container_id,
            "postgres",
            &format!("DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)", self.name),
        );
    }
}

/// The migrated platform database every process of a worktree shares.
pub fn platform() -> &'static Platform {
    static PLATFORM: OnceLock<Platform> = OnceLock::new();
    PLATFORM.get_or_init(|| {
        Platform::join().unwrap_or_else(|error| {
            panic!("the shared platform database could not be started: {error}")
        })
    })
}

/// The recipe the shared server runs under, its identity keyed to every input
/// that changes what a ready server contains.
fn spec() -> Result<shared::Spec, String> {
    let _ = image::build().map_err(|error| {
        format!("could not build the shared PostgreSQL image (run `pnpm build` first): {error}")
    })?;
    let image = image_ref();
    let environment = vec![
        ("POSTGRES_PASSWORD".to_owned(), "fixture".to_owned()),
        ("POSTGRES_DB".to_owned(), TEMPLATE_DATABASE.to_owned()),
    ];
    let postgres_args: Vec<String> = vec![
        "-c".to_owned(),
        format!("max_connections={SHARED_MAX_CONNECTIONS}"),
        "-c".to_owned(),
        format!("superuser_reserved_connections={SUPERUSER_RESERVED}"),
        "-c".to_owned(),
        "fsync=off".to_owned(),
        "-c".to_owned(),
        "wal_level=logical".to_owned(),
        "-c".to_owned(),
        "max_replication_slots=128".to_owned(),
        "-c".to_owned(),
        "max_wal_senders=128".to_owned(),
        "-c".to_owned(),
        "max_slot_wal_keep_size=256MB".to_owned(),
    ];
    let args = server_args(&postgres_args);
    let inputs = server_inputs(&root(), &image, &environment, &args)?;
    Ok(shared::Spec {
        inputs,
        image,
        environment,
        ports: vec![port()],
        watchdog: WATCHDOG.to_owned(),
        entrypoint: shared::Entrypoint::Image,
        args,
        ready: readiness(DATABASE),
    })
}

/// The published port every PostgreSQL server from the image exposes.
fn port() -> shared::Port {
    shared::Port {
        container: POSTGRES_PORT,
        host: shared::HostPort::Assigned,
    }
}

/// The server command handed to the watchdog after its grace and nonce.
fn server_args(postgres_args: &[String]) -> Vec<String> {
    let mut args = vec!["docker-entrypoint.sh".to_owned(), "postgres".to_owned()];
    args.extend_from_slice(postgres_args);
    args
}

/// How to tell a PostgreSQL server is ready and still answers.
///
/// The ready probe only needs the server up; the answer probe connects to the
/// served database, so a database that was dropped makes a live server report
/// unreachable rather than replaced.
fn readiness(database: &str) -> shared::Readiness {
    shared::Readiness {
        log_marker: "PostgreSQL init process complete".to_owned(),
        probe: vec![
            "pg_isready".to_owned(),
            "-U".to_owned(),
            "postgres".to_owned(),
            "-h".to_owned(),
            "127.0.0.1".to_owned(),
            "-p".to_owned(),
            POSTGRES_PORT.to_string(),
        ],
        answer: vec![
            "psql".to_owned(),
            "-U".to_owned(),
            "postgres".to_owned(),
            "-d".to_owned(),
            database.to_owned(),
            "-tAc".to_owned(),
            "SELECT 1".to_owned(),
        ],
    }
}

/// The 12-hex identity of a shared platform server built from `root`: every
/// input that decides what a ready server holds.
///
/// The platform migration is applied by the CLI's compiled JavaScript over the
/// native addon and the `@zeroship/migrate` dist, so all three are hashed: a
/// changed addon or dist changes what the migration does without changing the
/// corpus. The server's own environment and command arguments decide the
/// running instance, so they are hashed too rather than described in prose.
///
/// Public so a contract test can show that changing any hashed input moves it.
///
/// # Errors
/// When the migration corpus or a compiled input cannot be read.
pub fn server_inputs(
    root: &Path,
    image: &str,
    environment: &[(String, String)],
    args: &[String],
) -> Result<String, String> {
    let mut parts: Vec<Vec<u8>> = vec![
        fingerprint::of_dir(root)?.into_bytes(),
        image.as_bytes().to_vec(),
        SEED_PLAN.as_bytes().to_vec(),
    ];

    for argument in args {
        parts.push(argument.as_bytes().to_vec());
    }
    for (key, value) in environment {
        parts.push(format!("{key}={value}").into_bytes());
    }

    parts.extend(hashed_files(
        &root.join("packages/zero-migrate-cli/dist"),
        &["js"],
    )?);
    parts.extend(hashed_files(&root.join("packages/zero-migrate/dist"), &["js"])?);
    parts.extend(hashed_files(&root.join("crates/zeroship-migrate-node"), &["node"])?);

    for policy in [
        "policies/platform-table-owners.json",
        "policies/platform.policy.toml",
    ] {
        let path = root.join(policy);
        parts.push(
            std::fs::read(&path)
                .map_err(|error| format!("could not read {}: {error}", path.display()))?,
        );
    }

    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    Ok(shared::digest(&refs))
}

/// Every file under `dir` whose extension is one of `extensions`, hashed with
/// its name so a rename moves the digest.
fn hashed_files(dir: &Path, extensions: &[&str]) -> Result<Vec<Vec<u8>>, String> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|error| {
            format!(
                "could not read {}: {error}; build the JavaScript packages with `pnpm build`",
                dir.display()
            )
        })?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|value| value.to_str())
                .is_some_and(|extension| extensions.contains(&extension))
        })
        .collect();
    files.sort();
    let mut parts = Vec::new();
    for file in files {
        let name = file
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        parts.push(name.into_bytes());
        parts.push(
            std::fs::read(&file)
                .map_err(|error| format!("could not read {}: {error}", file.display()))?,
        );
    }
    Ok(parts)
}

/// Run the platform migration into the pristine template and clone the working
/// database from it.
fn boot_platform(boot: &shared::Boot) -> Result<(), String> {
    let template = admin_url(boot, TEMPLATE_DATABASE);
    apply_migrations(template.as_str())?;
    boot.psql(TEMPLATE_DATABASE, SEED_PLAN)?;
    boot.psql(
        "postgres",
        &format!(
            "ALTER DATABASE \"{TEMPLATE_DATABASE}\" IS_TEMPLATE true ALLOW_CONNECTIONS false"
        ),
    )?;
    boot.psql(
        "postgres",
        &format!("CREATE DATABASE \"{DATABASE}\" TEMPLATE \"{TEMPLATE_DATABASE}\""),
    )
}

fn role_url(base: &url::Url, role: &str) -> url::Url {
    let mut url = base.clone();
    url.set_username(role).unwrap();
    url.set_password(Some(role)).unwrap();
    url
}

/// The URL of a database on `boot`'s server as the superuser, `postgres`.
fn admin_url(boot: &shared::Boot, database: &str) -> url::Url {
    let mut url = url::Url::parse(&format!(
        "postgresql://postgres:fixture@127.0.0.1/{database}"
    ))
    .expect("fixture database URL");
    url.set_port(Some(boot.port)).expect("fixture database port");
    url
}

/// The URL of another database on the same server, with `base`'s credentials.
fn database_url(base: &url::Url, name: &str) -> url::Url {
    let mut url = base.clone();
    url.set_path(name);
    url
}

/// Open one connection and spawn its driver task.
///
/// A refused connection reports whether the shared server had no slot left for
/// this case, so an undersized budget names itself rather than surfacing as a
/// case that appears to hang on a database that never answers.
///
/// # Panics
/// When the server refuses the connection.
pub async fn connect(
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
        .unwrap_or_else(|error| panic!("{}", connection_failure(url, &error)));
    (
        client,
        compio::runtime::spawn(async move { connection.run().await }),
    )
}

/// Open one connection to a URL this crate has not parsed.
///
/// # Panics
/// When the URL is not a database URL or the server refuses the connection.
pub async fn connect_str(
    url: &str,
) -> (
    Client,
    compio::runtime::JoinHandle<Result<(), compio_postgres::Error>>,
) {
    let parsed: url::Url = url.parse().expect("fixture database URL");
    connect(&parsed).await
}

/// The message a case reports when the shared server refuses its connection.
///
/// A connection refused for want of a slot is told apart from every other
/// connect failure, because the fix is the run's budget rather than the DSN.
#[must_use]
pub fn connection_failure(url: &url::Url, error: &compio_postgres::Error) -> String {
    let database = url.path().trim_start_matches('/');
    if error
        .as_db_error()
        .is_some_and(|db_error| db_error.code().code() == "53300")
    {
        format!(
            "the shared PostgreSQL server has no connection slot left for {database}: it serves \
             every test process of a worktree under the run's parallel connection budget. A case \
             holds {CONNECTIONS_PER_CASE} at most, the run executes {PARALLEL_CASES} at once, and \
             the budget is sized for exactly that; lower a case's pools or raise the budget from \
             the rule in `spec`. {error}"
        )
    } else {
        format!("connect fixture database {url}: {error}")
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("testkit lives under tests/")
        .to_owned()
}

fn apply_migrations(url: &str) -> Result<(), String> {
    let root = root();
    let cli = root.join("packages/zero-migrate-cli/dist/cli-bin.js");
    if !cli.is_file() {
        return Err(format!(
            "{} is missing; build the JavaScript packages with `pnpm build`",
            cli.display()
        ));
    }
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
    let mut config = tempfile::NamedTempFile::new().map_err(|error| error.to_string())?;
    config
        .write_all(toml::to_string(&env).unwrap().as_bytes())
        .map_err(|error| error.to_string())?;
    let mut stdout = tempfile::tempfile().map_err(|error| error.to_string())?;
    let mut stderr = tempfile::tempfile().map_err(|error| error.to_string())?;
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
            .stdout(stdout.try_clone().map_err(|error| error.to_string())?)
            .stderr(stderr.try_clone().map_err(|error| error.to_string())?)
            .spawn()
            .map_err(|error| {
                format!("platform database fixtures require Node; run through nix develop: {error}")
            })?,
    );
    let deadline = Instant::now() + Duration::from_secs(300);
    let status = loop {
        if let Some(status) = child.0.try_wait().map_err(|error| error.to_string())? {
            break status;
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "platform migrations timed out: {}",
                read(&mut stderr)
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if !status.success() {
        return Err(format!(
            "platform migrations failed ({status}):\n{}\n{}",
            read(&mut stdout),
            read(&mut stderr)
        ));
    }
    Ok(())
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
