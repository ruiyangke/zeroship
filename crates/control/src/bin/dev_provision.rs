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
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroship_bundle::{build_blob_store, StoreUrl};
use zeroship_control::bootstrap_console::{free_plan_id, seed_plans};
use zeroship_control::registry::{Registry, RegistryError};

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
    #[arg(long)]
    owner: Option<Uuid>,
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
    if std::env::var("ZEROSHIP_DEV_INSECURE").ok().as_deref() != Some("1") {
        eprintln!(
            "dev-provision: refusing to run unless ZEROSHIP_DEV_INSECURE=1 is set \
             (DEV/LOCAL/CI ONLY; bypasses PAT/OAuth via direct DB writes)"
        );
        return ExitCode::FAILURE;
    }

    match run(cli).await {
        Ok(app) => {
            println!("app_id={}", app.id);
            println!("name={}", app.name);
            println!("api_key={}", app.api_key);
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
    let blob_store = build_blob_store(&store_url).map_err(|e| {
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

    let plan_id = free_plan_id();
    let app = match registry.create_app(&cli.name, &plan_id, &owner_id).await {
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
    let updated = registry
        .set_deploy_with_manifest(&app.id, &success.deploy_hash, &success.manifest_json)
        .await
        .map_err(|e| err(format!("deploy commit: {e}")))?;
    if !updated {
        return Err(err(format!(
            "app {} vanished between create/reuse and deploy commit",
            app.id
        )));
    }

    Ok(app)
}

async fn ensure_owner_exists(db_url: &str, owner_id: &Uuid) -> Result<(), DevProvisionError> {
    let conn = open_conn(db_url)
        .await
        .map_err(|e| err(format!("connect for dev owner seed: {e}")))?;
    let email = format!("dev-provision-{owner_id}@zeroship.localhost");
    conn.execute(
        "INSERT INTO zeroship.users (id, email, email_verified_at, name) \
         VALUES ($1, $2, NOW(), 'Dev Provision Owner') \
         ON CONFLICT (id) DO NOTHING",
        &[owner_id, &email],
    )
    .await
    .map_err(|e| err(format!("seed dev owner {owner_id}: {e}")))?;
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

fn default_owner_id() -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(b"zeroship:dev-provision-owner:v1");
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0F) | 0x80;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    Uuid::from_bytes(bytes)
}
