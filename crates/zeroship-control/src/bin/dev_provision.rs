// The explicit-database-creation step (47ad97a28) added another await to `main`'s
// already-large async block, and rustc's layout query for it now exceeds the
// default 128 depth: "query depth increased by 130 when computing layout of
// {async block ...dev_provision.rs:92}". RELEASE ONLY - `cargo check` in debug
// compiles this file fine, which is why nothing caught it until a release build
// of the six binaries `tests/golden_path.sh` needs.
#![recursion_limit = "256"]

//! DEV/LOCAL/CI ONLY internal app provisioning tool.
//!
//! This binary must never be deployed, exposed as a service, or wired to a
//! network route. It is a local operator/CI tool that requires direct database
//! and blob-store access, and it deliberately bypasses the PAT/OAuth flow by
//! construction through internal DB writes. Production app provisioning remains
//! the PAT-gated `/api/apps` + `zeroship deploy` path; this tool does not change
//! that path at all.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use compio_postgres::{Client, NoTls};
use zeroship_bundle::{build_blob_store, StoreUrl};
use zeroship_control::plan_catalog::{free_plan_id, seed_plans};
use zeroship_control::registry::{Registry, RegistryError};
use zeroship_core::UserId;

zeroship_core::declare_env_consumer!(
    /// This one-shot has no `#[zeroship_config]` declaration, so it declares its
    /// own marker. The S3 credentials it reads are recorded against THIS target,
    /// not against `zeroship-control`, because they are different processes with
    /// different deployment surfaces.
    DevProvisionConsumer,
    target = "dev-provision",
    scope = "dev_provision"
);

#[derive(Parser, Debug)]
#[command(name = "dev-provision")]
struct Cli {
    /// PostgreSQL DSN for control-plane data.
    #[arg(long = "db", env = "DATABASE_URL", hide_env_values = true)]
    db: String,

    /// Root directory or URL for the content-addressed deploy blob store.
    #[arg(long = "blob-store")]
    blob_store: String,

    /// Application name to create or reuse.
    #[arg(long)]
    name: String,

    /// Path to the .zship artifact to ingest and commit.
    #[arg(long)]
    zship: PathBuf,

    /// Owner user id for the app. Defaults to a deterministic dev-only owner.
    #[arg(long, value_parser = parse_user_id)]
    owner: Option<UserId>,

    /// Create the app and ingest the artifact, but do NOT make the deploy live.
    ///
    /// An app whose `.zship` carries a runtime schema descriptor cannot be made
    /// live until its migrations are applied - `Registry::set_deploy_with_manifest`
    /// refuses it, the same way the deploy API refuses a creator. But the
    /// migration service needs the app row to exist before it will authorize
    /// database creation or apply anything, and this tool is what creates that
    /// row. So a schema-carrying app is provisioned in two dev-provision calls
    /// with two explicit database operations between them:
    ///
    ///   dev-provision --defer-deploy ...   # app_id + name, nothing live
    ///   POST /v1/databases/<app_id>
    ///   <apply migrations through zeroship-migrate-server>
    ///   dev-provision ...                  # same command, now activates
    ///
    /// This tool deliberately does not create the database or apply migrations.
    /// The second call reuses the existing app by name and re-ingests the same
    /// content-addressed blobs, so running it is idempotent. An app with no
    /// descriptor needs neither the flag nor the second call.
    #[arg(long = "defer-deploy")]
    defer_deploy: bool,
}

#[derive(Debug)]
struct DevProvisionError(String);

impl std::fmt::Display for DevProvisionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DevProvisionError {}

fn err(msg: impl Into<String>) -> DevProvisionError {
    DevProvisionError(msg.into())
}

#[compio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(app) => {
            println!("app_id={}", app.id);
            println!("name={}", app.name);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("dev-provision: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<zeroship_core::types::AppRecord, DevProvisionError> {
    let store_url = StoreUrl::parse(&cli.blob_store)
        .map_err(|e| err(format!("invalid --blob-store '{}': {e}", cli.blob_store)))?;
    let s3_runtime = match store_url.is_remote() {
        true => Some(
            zeroship_core::resolve_s3_runtime!(DevProvisionConsumer).map_err(|e| {
                err(format!(
                    "failed to resolve S3 credentials for blob store: {e}"
                ))
            })?,
        ),
        false => None,
    };
    let blob_store = build_blob_store(&store_url, s3_runtime.as_ref()).map_err(|e| {
        err(format!(
            "failed to initialize blob store '{}': {e}",
            cli.blob_store
        ))
    })?;

    let registry = Registry::new(&cli.db)
        .await
        .map_err(|e| err(format!("connect registry: {e}")))?;
    seed_plans(&registry)
        .await
        .map_err(|e| err(format!("seed built-in plans: {e}")))?;

    let owner_id = cli.owner.unwrap_or_else(default_owner_id);
    ensure_owner_exists(&cli.db, &owner_id).await?;

    // `None` puts the app in the dev owner's personal organization's default
    // project, minted on demand - the same zero-config landing place a
    // creator's first deploy gets. A dev-only shortcut here would be a second
    // answer to "where does an un-placed app go", and the rows it produced
    // would not be the rows production reads.
    let plan_id = free_plan_id();
    let app = match registry
        .create_app(&cli.name, &plan_id, &owner_id, None)
        .await
    {
        Ok(app) => app,
        Err(RegistryError::AlreadyExists(_)) => registry
            .get_app_by_name(&cli.name)
            .await
            .map_err(|e| err(format!("lookup existing app '{}': {e}", cli.name)))?
            .ok_or_else(|| {
                err(format!(
                    "app name '{}' already exists but could not be read back",
                    cli.name
                ))
            })?,
        Err(e) => return Err(err(format!("create app '{}': {e}", cli.name))),
    };

    let bytes = std::fs::read(&cli.zship)
        .map_err(|e| err(format!("read .zship {}: {e}", cli.zship.display())))?;
    let success = zeroship_bundle::ingest(&blob_store, &app.id, &bytes)
        .await
        .map_err(|e| err(format!("zship ingest: {e:?}")))?;
    if cli.defer_deploy {
        eprintln!(
            "dev-provision: --defer-deploy: app {0} created and blobs ingested; the deploy is \
             NOT live. Create its database with POST /v1/databases/{0}, apply its migrations, \
             then re-run without the flag.",
            app.id,
        );
        return Ok(app);
    }
    // Read from the SAME manifest bytes the registry is about to store, so the
    // descriptor this call presents is the descriptor that would go live.
    let descriptor_sha256 =
        serde_json::from_str::<zeroship_bundle::Manifest>(&success.manifest_json)
            .map_err(|e| err(format!("re-parse ingested manifest: {e}")))?
            .runtime_descriptor
            .map(|entry| entry.hash);
    let updated = registry
        .set_deploy_with_manifest(
            &app.id,
            &success.deploy_hash,
            &success.manifest_json,
            descriptor_sha256.as_deref(),
        )
        .await
        .map_err(|e| match e {
            // The schema precondition, restated for a tool whose caller is a
            // shell script rather than the deploy CLI. Without the second
            // sentence this reads as a bug in the artifact.
            RegistryError::SchemaNotApplied { .. } => err(format!(
                "deploy commit refused: {e}\n\
                 app {0} exists and its blobs are ingested. Create its database with POST \
                 /v1/databases/{0}, apply its migrations through zeroship-migrate-server, then \
                 re-run this command. To create the app WITHOUT this failure, pass \
                 --defer-deploy on the first call.",
                app.id,
            )),
            other => err(format!("deploy commit: {other}")),
        })?;
    if !updated {
        return Err(err(format!(
            "app {} vanished between create/reuse and deploy commit",
            app.id
        )));
    }

    Ok(app)
}

async fn ensure_owner_exists(db_url: &str, owner_id: &UserId) -> Result<(), DevProvisionError> {
    let conn = open_conn(db_url)
        .await
        .map_err(|e| err(format!("connect for dev owner seed: {e}")))?;
    let email = format!("dev-provision-{}@zeroship.localhost", owner_id.as_str());
    conn.execute(
        "INSERT INTO zeroship.users (id, email, email_verified_at, name) \
         VALUES ($1, $2, NOW(), 'Dev Provision Owner') \
         ON CONFLICT (id) DO NOTHING",
        &[&owner_id.as_str(), &email],
    )
    .await
    .map_err(|e| err(format!("seed dev owner {}: {e}", owner_id.as_str())))?;
    Ok(())
}

async fn open_conn(url: &str) -> Result<Client, compio_postgres::Error> {
    let (client, connection) = compio_postgres::connect(url, NoTls).await?;
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("dev-provision: pg connection error: {e}");
        }
    })
    .detach();
    Ok(client)
}

fn parse_user_id(raw: &str) -> Result<UserId, String> {
    UserId::parse(raw).map_err(|error| error.to_string())
}

fn default_owner_id() -> UserId {
    UserId::parse("usr_0000000000000000000001").expect("fixed dev owner id is canonical")
}
