//! Deploy-time DB migration — the schema-authority §8 wiring.
//!
//! P6 makes `zeroship-migrate` the **single schema authority** for creator
//! apps: a `.zship` ships its versioned migration files (carried on the
//! manifest as content-addressed blobs, see
//! [`zeroship_bundle::MigrationFileEntry`]), and the control-plane deploy step
//! provisions the app's per-app role + schema and **applies the pending
//! migrations BEFORE the go-live commit**. The runtime no longer migrates.
//!
//! # Where this slots into the deploy handler
//!
//! ```text
//! deploy::ingest (writes blobs + manifest)            [api.rs ~505]
//!   → reconstruct migration files from blobs           [api.rs, this module]
//!   → apply_bundle_migrations (THIS module)            ← migrate phase
//!       provision schema "<app_id>" + migrator role
//!       load_dir → engine.plan (Confined) → engine.apply
//!   → set_deploy_with_manifest (go-live)               [api.rs ~605]
//! ```
//!
//! Only on a **successful** migrate does the handler reach the go-live commit.
//! A migrate failure returns an error and the old bundle keeps serving its
//! already-migrated schema (schema-authority §8.3).
//!
//! # The app identity is the trusted path id
//!
//! `app_id` is the **path parameter** the deploy handler already authorized
//! (`AppsDeploy` on `Resource::App{id}`); it is never read from a request body.
//! The per-app schema is `"<app_id>"` and the project id seeding the advisory
//! lock + journal + the least-privilege `migrator_<app_id>` role is the same
//! `app_id` (the plugin-db per-app-schema model). So a creator can only ever
//! migrate the schema they were authorized to deploy.
//!
//! # Profile: Confined, no shadow dry-run (v1)
//!
//! Migrations run under the **Confined** profile (the full SQL deny-list +
//! single-schema confinement to `"<app_id>"`) and the least-privilege
//! `migrator_<app_id>` role (line-2 DB-privilege defense). We **skip the
//! shadow-DB dry-run** at deploy: the shadow path needs a `CREATEDB` admin DSN,
//! and the in-line safety (the engine re-runs the guard on every `up` + the
//! migrator role) is the defense. A future revision MAY add an optional
//! `--admin-db` (CREATEDB) config and run the shadow when present; v1 does not.
//!
//! # Destructive migrations
//!
//! The deploy path passes [`Approval::None`]. A destructive migration (DROP /
//! TRUNCATE / lossy type change) is therefore **refused** at deploy (the engine
//! gate returns [`EngineError::ApprovalRequired`]) and no go-live happens —
//! destructive schema change goes through the out-of-band `submit_migration`
//! surface / expand-contract across deploys (schema-authority §8.4), not a
//! creator's routine deploy.

use std::path::Path;

use uuid::Uuid;
use zeroship_migrate::{
    connect, load_dir, provision_migrator, Approval, ConnectError, EngineError, ExecutorConfig,
    LoaderError, MigrationEngine, RoleError,
};

/// What a successful deploy-migrate produced (for logging / the deploy log).
#[derive(Debug, Clone)]
pub struct MigrateOutcome {
    /// Migration version ids applied this deploy (empty ⇒ already up to date).
    pub applied: Vec<String>,
    /// Migration version ids skipped because already journaled.
    pub skipped: Vec<String>,
}

/// A deploy-time migration failure. The deploy handler maps this to an HTTP
/// error and **does not commit go-live** — the old bundle keeps serving.
#[derive(Debug, thiserror::Error)]
pub enum DeployMigrateError {
    /// Opening the admin connection failed.
    #[error("deploy-migrate connect: {0}")]
    Connect(#[from] ConnectError),
    /// Creating the per-app schema `"<app_id>"` failed (admin DDL).
    #[error("deploy-migrate provision schema: {0}")]
    ProvisionSchema(compio_postgres::Error),
    /// Provisioning the least-privilege `migrator_<app_id>` role failed.
    #[error("deploy-migrate provision role: {0}")]
    ProvisionRole(#[from] RoleError),
    /// Loading / parsing the reconstructed migration directory failed (bad
    /// filename grammar, duplicate version, orphan down, unparseable body, …).
    #[error("deploy-migrate load migrations: {0}")]
    Load(#[from] LoaderError),
    /// The engine refused or failed the apply: a guard denial, a destructive
    /// migration without approval (refused at deploy), checksum drift, or a
    /// mid-apply DB error.
    #[error("deploy-migrate apply: {0}")]
    Apply(#[from] EngineError),
}

/// Quote a SQL identifier (double embedded quotes, wrap in `"`). Mirrors the
/// engine's `quote_ident` so the schema name is never raw-interpolated.
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Provision the app's per-app role + schema and apply the bundle's pending
/// migrations under the **Confined** profile, BEFORE go-live (§8).
///
/// `migrate_dsn` is an admin DSN with `CREATEROLE` + the ability to
/// `CREATE SCHEMA` (the control-plane DB role). `app_id` is the trusted,
/// already-authorized path id; the per-app schema + project id + migrator role
/// are all derived from it. `migrations_dir` holds the migration files the
/// deploy handler reconstructed from the bundle's blobs (Flyway `V<NNNN>__…` or
/// dbmate-shaped). An **empty** directory is a clean no-op (the app ships no
/// schema).
///
/// On success the schema + journal are committed; the caller then commits
/// go-live. On ANY error the caller MUST NOT commit go-live.
///
/// # Errors
/// [`DeployMigrateError`] on connect / schema-provision / role-provision /
/// load / apply failure.
pub async fn apply_bundle_migrations(
    migrate_dsn: &str,
    app_id: &Uuid,
    migrations_dir: &Path,
) -> Result<MigrateOutcome, DeployMigrateError> {
    // The per-app schema + project id are the trusted path id. The plugin-db
    // model maps app_id → schema "<app_id>"; the engine uses the same id to seed
    // the advisory lock, journal, and the migrator_<app_id> role name.
    let schema = app_id.to_string();

    // Load the migration set FIRST — a malformed directory is a deploy error we
    // surface before touching the DB (no schema/role provisioned for a bundle
    // that can't load).
    let migrations = load_dir(migrations_dir)?;

    // Open the admin connection (CREATEROLE + CREATE SCHEMA). Detaches its
    // driver loop onto the compio runtime.
    let conn = connect(migrate_dsn).await?;

    // (a) Provision the per-app schema. Idempotent: IF NOT EXISTS. The migrator
    //     role provisioning (below) reassigns ownership to the migrator, so the
    //     migrator's DDL + ALTER DEFAULT PRIVILEGES resolve to an owner it
    //     controls.
    conn.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS {}",
        quote_ident(&schema)
    ))
    .await
    .map_err(DeployMigrateError::ProvisionSchema)?;

    // Build the Confined executor config (full deny-list + single-schema
    // confinement to "<app_id>"), running migrations under the least-privilege
    // migrator_<app_id> role (line-2 DB defense).
    let role = zeroship_migrate::migrator_role_name(&schema)?;
    let exec_cfg =
        ExecutorConfig::new(schema.clone(), schema.clone()).with_migrator_role(role.clone());

    // (b) Provision the migrator role (idempotent). Owns the project schema,
    //     no access to the meta schema (unforgeable journal), no reach into
    //     control/auth/other schemas.
    provision_migrator(&conn, &exec_cfg).await?;

    // (c) Plan (Confined guard) + apply PENDING. Approval::None ⇒ a destructive
    //     migration is refused at deploy (no go-live); additive-forward is the
    //     routine path. The engine independently re-runs the guard + the
    //     migrator role on every up (defense in depth) — we do not skip those.
    let engine = MigrationEngine::new();
    let guard_cfg = zeroship_migrate::GuardConfig::confined(schema.clone());
    let plan = engine.plan(&migrations, &guard_cfg);
    let outcome = engine
        .apply(&plan, Approval::None, &conn, &exec_cfg, "deploy")
        .await?;

    Ok(MigrateOutcome {
        applied: outcome.applied,
        skipped: outcome.skipped,
    })
}
