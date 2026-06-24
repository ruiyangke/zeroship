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

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use uuid::Uuid;
use zeroship_migrate::{
    compute_manifest, connect, load_dir_migrations, provision_migrator, Approval, ConnectError,
    DriftError, EngineError, ExecutorConfig, IrAuthor, LoadAndLowerError, LoaderError,
    MigrationBackend, MigrationEngine, PostgresBackend, RoleError, SqlDialect,
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
    /// A creator `.ir.json` failed the fail-closed LOAD GATE (malformed, future
    /// `ir_version`, structural reject incl. the bare-name DropIndex, ownership
    /// violation, checksum-hint mismatch) or its lowering failed (§5.2/§8.6). A
    /// creator-fault — the deploy handler maps this to a 422; no go-live.
    #[error("deploy-migrate IR load/lower ({file}): {source}")]
    Ir {
        /// The `.ir.json` filename the gate refused.
        file: String,
        /// The fail-closed gate / lower error.
        #[source]
        source: LoadAndLowerError,
    },
    /// Reading the `.ir.json` file from the reconstructed migrations dir failed.
    #[error("deploy-migrate read IR file ({file}): {message}")]
    IrRead {
        /// The `.ir.json` filename.
        file: String,
        /// The I/O error.
        message: String,
    },
    /// Introspecting the live schema (to build the IR ownership registry + the
    /// FK-inline live-table set) failed.
    #[error("deploy-migrate live snapshot: {0}")]
    Snapshot(#[from] DriftError),
}

/// Quote a SQL identifier (double embedded quotes, wrap in `"`). Mirrors the
/// engine's `quote_ident` so the schema name is never raw-interpolated.
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Provision the app's per-app role + schema and apply the bundle's pending
/// migrations under the **Confined** profile, BEFORE go-live (§8).
///
/// `migrate_dsn` is a PRIVILEGED provisioning DSN with `CREATEROLE` + `CREATE`
/// on the database — a SEPARATE admin role, **not** the control-plane
/// `zeroship_control` role (which is BYPASSRLS but has neither privilege; see
/// V0025). It is wired from `--provision-db` / `PROVISION_DATABASE_URL`.
/// `app_id` is the trusted,
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
    // PR0 (`op.*` DSL §5.2): `load_dir` now returns `Vec<AppliedPlan>`. The
    // platform/control deploy path is the trusted `.sql` path (every file is a
    // single-step plan), and its apply runs over the FLAT `Migration` set
    // (`apply_verified` + the integrity-manifest fold), so we load the flat form
    // via `load_dir_migrations` — byte-identical to the pre-PR0 behavior. The
    // IR-path apply (PR1+) routes `Vec<AppliedPlan>` through `apply_plan`.
    let migrations = load_dir_migrations(migrations_dir)?;

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

    // (c) Plan (Confined guard) + apply PENDING via the integrity-manifest seam
    //     (`apply_verified`). Approval::None ⇒ a destructive migration is refused
    //     at deploy (no go-live); additive-forward is the routine path. The engine
    //     independently re-runs the guard + the migrator role on every up (defense
    //     in depth) — we do not skip those.
    //
    //     H2 — INTEGRITY MANIFEST: the manifest gate (`manifest.rs`) detects a
    //     creator / AI-author / build-pipeline tampering the migration SET between
    //     authoring/review and apply (reorder / edit / insert / remove). For that
    //     guarantee to hold, the EXPECTED hash MUST come from a TRUSTED, OUT-OF-BAND
    //     source — NOT from the same `.zship` the migrations arrived in (an attacker
    //     who can edit the migrations can edit a hash shipped alongside them, and the
    //     check would vacuously pass; see manifest.rs "Trust model").
    //
    //     No such build-side stamp exists yet: the `.zship` manifest carries only
    //     per-file blob hashes (which travel WITH the migrations — self-consistency,
    //     not an independent expectation), and the control DB stores no migration
    //     manifest hash. So we CANNOT honestly pass an `expected` hash here — doing
    //     so against a bundle-derived value would be a FAKE "verified". Instead we
    //     compute the manifest over the loaded set and LOG it for traceability /
    //     incident forensics, and route through `apply_verified(expected: None)` so
    //     the gate is wired and threading a trusted stamp later is a one-line change.
    //
    //     FOLLOW-UP (REQUIRED for the SEC defense to bite): the build/review side
    //     must stamp `compute_manifest(...)` at authoring time and persist it
    //     out-of-band (control DB, keyed by app + bundle), and this call must then
    //     pass `Some(&expected)` so a tampered/reordered set is REFUSED before any
    //     DDL. Until then this is traceability only, NOT tamper-prevention.
    let manifest = compute_manifest(&migrations);
    tracing::info!(
        app_id = %app_id,
        migration_count = migrations.len(),
        manifest = %manifest.as_str(),
        "deploy-migrate: computed migration-set integrity manifest (traceability only — \
         no trusted build-side stamp to verify against yet; see H2 follow-up)"
    );
    let engine = MigrationEngine::new();
    let guard_cfg = zeroship_migrate::GuardConfig::confined(schema.clone());
    // P6a genericized `MigrationEngine::apply` over `MigrationBackend`; the
    // platform/control deploy path is Postgres, so wrap the connection in the
    // PG backend (behavior-identical to the pre-seam `&Client` call).
    let backend = PostgresBackend::new(&conn);
    let outcome = engine
        .apply_verified(
            &migrations,
            &guard_cfg,
            // No trusted expectation available (see the H2 note above). NEVER pass a
            // bundle-derived hash here — it would be a vacuous self-check.
            None,
            Approval::None,
            &backend,
            &exec_cfg,
            "deploy",
        )
        .await?;

    // (d) CREATOR `.ir.json` PATH (§5.2/§6/§8.6). A `.zship` may ship `.ir.json`
    //     artifacts (the op.* DSL output) alongside / instead of `.sql`. Each is
    //     routed through the fail-closed IR LOAD GATE
    //     (`IrAuthor::load_and_lower`: deserialize → ir_version → validate_ir →
    //     server-stamped ownership → checksum-hint), with the deploy-target dialect
    //     (Postgres here) threaded in, then LOWERED + applied under the SAME
    //     Confined guard + migrator role. This is the production caller of the IR
    //     gate (previously the gate was exported but unreached).
    let ir_outcome =
        apply_bundle_ir_migrations(&backend, app_id, migrations_dir, &exec_cfg, &guard_cfg)
            .await?;

    let mut applied = outcome.applied;
    applied.extend(ir_outcome.applied);
    let mut skipped = outcome.skipped;
    skipped.extend(ir_outcome.skipped);
    Ok(MigrateOutcome { applied, skipped })
}

/// Discover + apply the bundle's `.ir.json` creator artifacts (§5.2/§8.6).
///
/// For each `*.ir.json` file in `migrations_dir` (version-ordered by filename),
/// the fail-closed IR LOAD GATE runs ([`IrAuthor::load_and_lower`]: deserialize →
/// `ir_version` → `validate_ir` → server-stamped ownership → advisory checksum
/// hint), with the deploy-target dialect (Postgres) threaded in (§2.4.1), then
/// the validated, owned ops are LOWERED to migrations and applied under the SAME
/// Confined guard + least-priv migrator role as the `.sql` path. The ownership
/// registry + the FK-inline live-table set are introspected from the LIVE schema
/// (the per-app schema `"<app_id>"`, all tables owned by `app_id`).
///
/// An empty / IR-free directory is a clean no-op.
///
/// # Errors
/// [`DeployMigrateError::Ir`] on a fail-closed gate refusal / lower failure (a
/// creator fault → 422); [`DeployMigrateError::Snapshot`] / [`DeployMigrateError::IrRead`]
/// / [`DeployMigrateError::Apply`] on introspection / I/O / apply failure.
async fn apply_bundle_ir_migrations(
    backend: &PostgresBackend<'_>,
    app_id: &Uuid,
    migrations_dir: &Path,
    exec_cfg: &ExecutorConfig,
    guard_cfg: &zeroship_migrate::GuardConfig,
) -> Result<MigrateOutcome, DeployMigrateError> {
    // Discover `*.ir.json` files, version-ordered by filename (deterministic).
    let mut ir_files: Vec<std::path::PathBuf> = Vec::new();
    let read = std::fs::read_dir(migrations_dir).map_err(|e| DeployMigrateError::IrRead {
        file: migrations_dir.display().to_string(),
        message: e.to_string(),
    })?;
    for entry in read {
        let entry = entry.map_err(|e| DeployMigrateError::IrRead {
            file: migrations_dir.display().to_string(),
            message: e.to_string(),
        })?;
        let path = entry.path();
        if path.is_file()
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".ir.json"))
        {
            ir_files.push(path);
        }
    }
    if ir_files.is_empty() {
        return Ok(MigrateOutcome { applied: vec![], skipped: vec![] });
    }
    ir_files.sort();

    let app = app_id.to_string();

    // Introspect the LIVE schema once: every live table in the per-app schema is
    // owned by the deploying app, so the IR ownership registry maps each live
    // table → `app_id`. The same key-set is the FK-inline live-table set.
    let live = backend.snapshot_schema(exec_cfg).await?;
    let registry: BTreeMap<String, String> =
        live.tables.keys().map(|t| (t.clone(), app.clone())).collect();
    let live_tables: BTreeSet<String> = live.tables.keys().cloned().collect();

    let engine = MigrationEngine::new();
    let mut applied: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    for path in &ir_files {
        let file = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<unknown>")
            .to_string();
        let bytes = std::fs::read_to_string(path).map_err(|e| DeployMigrateError::IrRead {
            file: file.clone(),
            message: e.to_string(),
        })?;

        // The FAIL-CLOSED gate + lower, with the deploy-target dialect (Postgres).
        let author = IrAuthor::new(app.clone(), app.clone(), SqlDialect::Postgres);
        let migrations = author
            .load_and_lower(&bytes, &app, &registry, &live_tables)
            .map_err(|source| DeployMigrateError::Ir { file: file.clone(), source })?;

        // Plan (Confined guard re-run as line-1) + apply under Approval::None — a
        // destructive op is refused at deploy, exactly like the `.sql` path.
        let plan = engine.plan(&migrations, guard_cfg);
        let outcome = engine
            .apply(&plan, Approval::None, backend, exec_cfg, "deploy-ir")
            .await?;
        applied.extend(outcome.applied);
        skipped.extend(outcome.skipped);
    }

    Ok(MigrateOutcome { applied, skipped })
}
